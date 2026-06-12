use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use crate::bulkstat;
use crate::bulkstat::SizeInfo;

pub type ScanId = u64;

/// Messages from the scanner thread back to the UI.
pub enum ScanMsg {
    DirSize {
        scan_id: ScanId,
        path: PathBuf,
        size: SizeInfo,
        /// Permission-denied directories under `path`; size is a lower bound when > 0.
        inaccessible: u32,
    },
    AllDone {
        scan_id: ScanId,
    },
}

pub struct Scanner {
    tx: Sender<ScanMsg>,
}

impl Scanner {
    pub fn new(tx: Sender<ScanMsg>) -> Self {
        Self { tx }
    }

    /// Scan each directory in `dirs` for its recursive logical and allocated size.
    /// Must NOT block the UI thread.
    pub fn scan_all(&self, scan_id: ScanId, dirs: Vec<PathBuf>) -> std::io::Result<()> {
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name(String::from("diskr-scan"))
            .spawn(move || {
                let worker_count = worker_count(dirs.len());
                let dirs = Arc::new(dirs);
                let next_index = AtomicUsize::new(0);

                std::thread::scope(|scope| {
                    for _ in 0..worker_count {
                        let tx = tx.clone();
                        let dirs = Arc::clone(&dirs);
                        let next_index = &next_index;
                        scope.spawn(move || loop {
                            let index = next_index.fetch_add(1, Ordering::Relaxed);
                            let Some(dir) = dirs.get(index).cloned() else {
                                break;
                            };
                            let scan = bulkstat::scan_dir(&dir, 0);
                            let _ = tx.send(ScanMsg::DirSize {
                                scan_id,
                                path: dir,
                                size: scan.size,
                                inaccessible: scan.inaccessible,
                            });
                        });
                    }
                });

                let _ = tx.send(ScanMsg::AllDone { scan_id });
            })
            .map(|_| ())
    }
}

fn worker_count(dir_count: usize) -> usize {
    if dir_count <= 1 {
        return dir_count;
    }

    let available = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    dir_count.min(available.clamp(1, 8))
}

#[cfg(test)]
mod tests {
    use super::worker_count;

    #[test]
    fn worker_count_respects_bounds() {
        assert_eq!(worker_count(0), 0);
        assert_eq!(worker_count(1), 1);

        let workers = worker_count(64);
        assert!((1..=8).contains(&workers));
    }
}
