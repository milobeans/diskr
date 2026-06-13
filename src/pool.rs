//! Std-only replacement for the rayon machinery diskr actually used:
//! a fixed worker pool that runs the non-blocking bulkstat walk tasks, plus
//! scoped parallel helpers for package scanning.
//!
//! Deadlock rule: pool workers must only run tasks that never block on a
//! `TaskGroup`. Blocking callers (the sync `scan_dir` wrappers, `par_map`
//! and `par_drain` closures) always run on threads external to the pool,
//! so the pool can always drain.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};

type Job = Box<dyn FnOnce() + Send + 'static>;

pub fn worker_count() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(4)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Work-stealing pool, structured like rayon's: each worker owns a deque,
/// pushes and pops its newest tasks (depth-first, cache-warm subtree), and
/// when empty takes the oldest task from the shared injector or steals the
/// oldest from another worker (a large, distant subtree). This locality is
/// load-bearing: a single shared queue measurably loses ~10% on real
/// directory trees because workers scatter across sibling directories.
struct PoolInner {
    injector: Mutex<std::collections::VecDeque<Job>>,
    workers: Vec<Mutex<std::collections::VecDeque<Job>>>,
    /// Pairs with `available`; held while a worker announces itself idle,
    /// re-checks every queue, and parks, and while a pusher notifies. This
    /// ordering makes a lost wakeup impossible: a parking worker has seen
    /// every queue empty after raising `idle`, so any later push reads
    /// `idle > 0` and notifies under the same lock.
    sleep: Mutex<()>,
    available: Condvar,
    idle: AtomicUsize,
}

thread_local! {
    /// Index of the current pool worker, None on external threads.
    static WORKER_INDEX: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

fn pool() -> &'static Arc<PoolInner> {
    static POOL: OnceLock<Arc<PoolInner>> = OnceLock::new();
    POOL.get_or_init(|| {
        let inner = Arc::new(PoolInner {
            injector: Mutex::new(std::collections::VecDeque::new()),
            workers: (0..worker_count())
                .map(|_| Mutex::new(std::collections::VecDeque::new()))
                .collect(),
            sleep: Mutex::new(()),
            available: Condvar::new(),
            idle: AtomicUsize::new(0),
        });
        // Detached workers live for the process lifetime, like rayon's
        // global pool did.
        for index in 0..inner.workers.len() {
            let pool = Arc::clone(&inner);
            let _ = std::thread::Builder::new()
                .name(format!("diskr-pool-{index}"))
                .spawn(move || {
                    WORKER_INDEX.with(|cell| cell.set(Some(index)));
                    worker_loop(&pool, index);
                });
        }
        inner
    })
}

/// Own newest first, then the injector's oldest, then steal the oldest
/// task from another worker, scanning victims from a per-caller offset so
/// thieves do not all converge on worker 0.
fn find_job(pool: &PoolInner, me: Option<usize>) -> Option<Job> {
    if let Some(index) = me {
        if let Some(job) = lock(&pool.workers[index]).pop_back() {
            return Some(job);
        }
    }
    if let Some(job) = lock(&pool.injector).pop_front() {
        return Some(job);
    }
    let count = pool.workers.len();
    let start = me.map(|index| index + 1).unwrap_or(0);
    for offset in 0..count {
        let victim = (start + offset) % count;
        if Some(victim) == me {
            continue;
        }
        if let Some(job) = lock(&pool.workers[victim]).pop_front() {
            return Some(job);
        }
    }
    None
}

fn worker_loop(pool: &PoolInner, index: usize) {
    loop {
        if let Some(job) = find_job(pool, Some(index)) {
            run_job(job);
            continue;
        }
        let guard = lock(&pool.sleep);
        pool.idle.fetch_add(1, Ordering::SeqCst);
        // Re-check after raising `idle`: any job pushed before our final
        // sweep is found here; any job pushed after it sees `idle > 0`.
        if let Some(job) = find_job(pool, Some(index)) {
            pool.idle.fetch_sub(1, Ordering::SeqCst);
            drop(guard);
            run_job(job);
            continue;
        }
        let guard = pool
            .available
            .wait(guard)
            .unwrap_or_else(PoisonError::into_inner);
        pool.idle.fetch_sub(1, Ordering::SeqCst);
        drop(guard);
    }
}

fn run_job(job: Job) {
    // A panicking task must not kill the worker; the FinishGuard still
    // marks it finished. panic = "abort" makes this moot in release.
    let _ = catch_unwind(AssertUnwindSafe(job));
}

fn try_pop_job() -> Option<Job> {
    let pool = pool();
    find_job(pool, WORKER_INDEX.with(|cell| cell.get()))
}

/// Pushes a batch under one queue lock: a pool worker keeps its tasks on
/// its own deque, external threads go through the shared injector.
fn push_jobs(jobs: impl IntoIterator<Item = Job>) {
    let pool = pool();
    let me = WORKER_INDEX.with(|cell| cell.get());
    let pushed = {
        let mut queue = match me {
            Some(index) => lock(&pool.workers[index]),
            None => lock(&pool.injector),
        };
        let before = queue.len();
        queue.extend(jobs);
        queue.len() - before
    };
    if pushed == 0 {
        return;
    }
    // Wake one worker per available job, never a stampede. Skipping the
    // notify when `idle` reads 0 is safe: see the `sleep` field invariant.
    let idle = pool.idle.load(Ordering::SeqCst);
    if idle > 0 {
        let _guard = lock(&pool.sleep);
        for _ in 0..pushed.min(idle) {
            pool.available.notify_one();
        }
    }
}

/// Tracks a set of pool tasks (including tasks they spawn) and reports
/// completion either by unblocking `wait()` or by running an `on_finish`
/// callback on the worker that finishes last.
///
/// The group is created holding a token so completion cannot fire while
/// the creator is still spawning initial tasks. Call `arm()` (or `wait()`,
/// which arms implicitly) exactly once after the initial spawns; tasks may
/// keep spawning siblings from inside the group afterwards.
pub struct TaskGroup {
    pending: AtomicUsize,
    state: Mutex<GroupState>,
    finished_cv: Condvar,
}

struct GroupState {
    finished: bool,
    on_finish: Option<Job>,
}

impl TaskGroup {
    pub fn new() -> Arc<Self> {
        Self::build(None)
    }

    pub fn with_finish(on_finish: impl FnOnce() + Send + 'static) -> Arc<Self> {
        Self::build(Some(Box::new(on_finish)))
    }

    fn build(on_finish: Option<Job>) -> Arc<Self> {
        Arc::new(Self {
            // One creation token, released by arm()/wait().
            pending: AtomicUsize::new(1),
            state: Mutex::new(GroupState {
                finished: false,
                on_finish,
            }),
            finished_cv: Condvar::new(),
        })
    }

    pub fn spawn(self: &Arc<Self>, job: impl FnOnce() + Send + 'static) {
        self.pending.fetch_add(1, Ordering::SeqCst);
        let guard = FinishGuard(Arc::clone(self));
        push_jobs([Box::new(move || {
            let _guard = guard;
            job();
        }) as Job]);
    }

    /// Spawns a batch of tasks with a single queue lock acquisition.
    pub fn spawn_all<F>(self: &Arc<Self>, jobs: Vec<F>)
    where
        F: FnOnce() + Send + 'static,
    {
        if jobs.is_empty() {
            return;
        }
        self.pending.fetch_add(jobs.len(), Ordering::SeqCst);
        push_jobs(jobs.into_iter().map(|job| {
            let guard = FinishGuard(Arc::clone(self));
            Box::new(move || {
                let _guard = guard;
                job();
            }) as Job
        }));
    }

    /// Releases the creation token; once all spawned tasks finish, the
    /// group completes (running `on_finish` on the last worker, or on this
    /// thread if everything already finished). Call exactly once.
    pub fn arm(&self) {
        self.task_finished();
    }

    /// Arms the group and blocks until every task has finished. Must never
    /// be called from a pool worker.
    ///
    /// While waiting, the caller helps drain the pool queue (like a blocked
    /// `rayon::scope` caller work-stealing), adding one effective worker to
    /// every sync scan. It may briefly run a task from an unrelated group
    /// after its own finishes; pool tasks are short and never block.
    pub fn wait(&self) {
        self.arm();
        loop {
            if lock(&self.state).finished {
                return;
            }
            match try_pop_job() {
                Some(job) => run_job(job),
                None => break,
            }
        }
        let mut state = lock(&self.state);
        while !state.finished {
            state = self
                .finished_cv
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn task_finished(&self) {
        if self.pending.fetch_sub(1, Ordering::SeqCst) != 1 {
            return;
        }
        let mut state = lock(&self.state);
        state.finished = true;
        let on_finish = state.on_finish.take();
        drop(state);
        self.finished_cv.notify_all();
        if let Some(on_finish) = on_finish {
            on_finish();
        }
    }
}

/// Decrements the group counter even if the task panics.
struct FinishGuard(Arc<TaskGroup>);

impl Drop for FinishGuard {
    fn drop(&mut self) {
        self.0.task_finished();
    }
}

/// Parallel map over owned items preserving input order, on transient
/// scoped threads. Closures may block (these threads are not pool workers).
pub fn par_map<T, R, F>(items: Vec<T>, f: F) -> Vec<R>
where
    T: Send,
    R: Send,
    F: Fn(T) -> R + Sync,
{
    if items.len() <= 1 {
        return items.into_iter().map(f).collect();
    }
    let threads = worker_count().min(items.len());
    let work = Mutex::new(items.into_iter().enumerate());
    let mut indexed: Vec<(usize, R)> = Vec::new();
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        let next = lock(&work).next();
                        let Some((index, item)) = next else {
                            break;
                        };
                        out.push((index, f(item)));
                    }
                    out
                })
            })
            .collect();
        for handle in handles {
            indexed.extend(handle.join().expect("par_map worker panicked"));
        }
    });
    indexed.sort_unstable_by_key(|&(index, _)| index);
    indexed.into_iter().map(|(_, result)| result).collect()
}

/// Work queue handle passed to `par_drain` closures for enqueueing more
/// items while draining.
pub struct WorkQueue<T> {
    state: Mutex<DrainState<T>>,
    work_available: Condvar,
}

struct DrainState<T> {
    items: Vec<T>,
    active: usize,
}

impl<T> WorkQueue<T> {
    pub fn push(&self, item: T) {
        lock(&self.state).items.push(item);
        self.work_available.notify_one();
    }
}

/// Drains `seed` and everything pushed during processing across transient
/// scoped threads; returns when the queue is empty and no item is still
/// being processed. Closures may block (these threads are not pool workers).
pub fn par_drain<T, F>(seed: Vec<T>, f: F)
where
    T: Send,
    F: Fn(T, &WorkQueue<T>) + Sync,
{
    if seed.is_empty() {
        return;
    }
    let queue = WorkQueue {
        state: Mutex::new(DrainState {
            items: seed,
            active: 0,
        }),
        work_available: Condvar::new(),
    };
    std::thread::scope(|scope| {
        for _ in 0..worker_count() {
            scope.spawn(|| loop {
                let item = {
                    let mut state = lock(&queue.state);
                    loop {
                        if let Some(item) = state.items.pop() {
                            state.active += 1;
                            break item;
                        }
                        if state.active == 0 {
                            return;
                        }
                        state = queue
                            .work_available
                            .wait(state)
                            .unwrap_or_else(PoisonError::into_inner);
                    }
                };
                let _guard = DrainGuard { queue: &queue };
                f(item, &queue);
            });
        }
    });
}

/// Marks the in-flight item done even if the closure panics, so idle
/// workers can observe termination instead of waiting forever.
struct DrainGuard<'a, T> {
    queue: &'a WorkQueue<T>,
}

impl<T> Drop for DrainGuard<'_, T> {
    fn drop(&mut self) {
        let mut state = lock(&self.queue.state);
        state.active -= 1;
        if state.active == 0 && state.items.is_empty() {
            self.queue.work_available.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn par_map_preserves_order() {
        let items: Vec<usize> = (0..100).collect();
        let doubled = par_map(items, |n| n * 2);
        assert_eq!(doubled, (0..100).map(|n| n * 2).collect::<Vec<_>>());
    }

    #[test]
    fn par_drain_processes_pushed_items() {
        let count = AtomicUsize::new(0);
        par_drain(vec![0u32], |depth, queue| {
            count.fetch_add(1, Ordering::SeqCst);
            if depth < 3 {
                queue.push(depth + 1);
                queue.push(depth + 1);
            }
        });
        // 1 + 2 + 4 + 8 nodes in a depth-3 binary tree.
        assert_eq!(count.load(Ordering::SeqCst), 15);
    }

    #[test]
    fn task_group_wait_blocks_until_nested_tasks_finish() {
        let count = Arc::new(AtomicUsize::new(0));
        let group = TaskGroup::new();
        for _ in 0..8 {
            let count = Arc::clone(&count);
            let nested = Arc::clone(&group);
            group.spawn(move || {
                count.fetch_add(1, Ordering::SeqCst);
                let count = Arc::clone(&count);
                nested.clone().spawn(move || {
                    count.fetch_add(1, Ordering::SeqCst);
                });
            });
        }
        group.wait();
        assert_eq!(count.load(Ordering::SeqCst), 16);
    }

    #[test]
    fn task_group_on_finish_fires_once_after_all_tasks() {
        let (tx, rx) = mpsc::channel();
        let count = Arc::new(AtomicUsize::new(0));
        let finish_count = Arc::clone(&count);
        let group = TaskGroup::with_finish(move || {
            let _ = tx.send(finish_count.load(Ordering::SeqCst));
        });
        for _ in 0..16 {
            let count = Arc::clone(&count);
            group.spawn(move || {
                count.fetch_add(1, Ordering::SeqCst);
            });
        }
        group.arm();
        let seen = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(seen, 16);
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    }
}
