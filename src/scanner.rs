use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use crate::bulkstat;
use crate::bulkstat::SizeInfo;
use rayon::prelude::*;

pub type ScanId = u64;

/// Messages from the scanner thread back to the UI.
pub enum ScanMsg {
    DirStarted {
        scan_id: ScanId,
        path: PathBuf,
    },
    DirSize {
        scan_id: ScanId,
        path: PathBuf,
        size: SizeInfo,
        /// Permission-denied directories under `path`; size is a lower bound when > 0.
        inaccessible: u32,
        /// Mounted volumes skipped below /Volumes to avoid duplicate or external volume walks.
        skipped_mounts: u32,
    },
    AllDone {
        scan_id: ScanId,
    },
}

pub struct Scanner {
    tx: Sender<ScanMsg>,
    generation: Arc<AtomicU64>,
}

impl Scanner {
    pub fn new(tx: Sender<ScanMsg>) -> Self {
        Self {
            tx,
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn cancel_current(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Scan each directory in `dirs` for its recursive logical and allocated size.
    /// Must NOT block the UI thread.
    pub fn scan_all(&self, scan_id: ScanId, dirs: Vec<PathBuf>) -> std::io::Result<()> {
        let tx = self.tx.clone();
        let generation = self
            .generation
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let cancellation =
            bulkstat::ScanCancellation::new(Arc::clone(&self.generation), generation);
        std::thread::Builder::new()
            .name(String::from("diskr-scan"))
            .spawn(move || {
                dirs.into_par_iter().for_each(|dir| {
                    if cancellation.is_cancelled() {
                        return;
                    }
                    let _ = tx.send(ScanMsg::DirStarted {
                        scan_id,
                        path: dir.clone(),
                    });
                    let Some(scan) = bulkstat::scan_dir_with_cancellation(&dir, 0, &cancellation)
                    else {
                        return;
                    };
                    if cancellation.is_cancelled() {
                        return;
                    }
                    let _ = tx.send(ScanMsg::DirSize {
                        scan_id,
                        path: dir,
                        size: scan.size,
                        inaccessible: scan.inaccessible,
                        skipped_mounts: scan.skipped_mounts,
                    });
                });

                if !cancellation.is_cancelled() {
                    let _ = tx.send(ScanMsg::AllDone { scan_id });
                }
            })
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::{ScanMsg, Scanner};
    use std::collections::HashSet;
    use std::fs;
    use std::sync::mpsc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn scan_all_streams_each_dir_and_completion() {
        let root = test_root("scan_all_streams");
        let first = root.join("first");
        let second = root.join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::write(first.join("a.txt"), b"a").unwrap();
        fs::write(second.join("b.txt"), b"bb").unwrap();

        let (tx, rx) = mpsc::channel();
        Scanner::new(tx)
            .scan_all(42, vec![first.clone(), second.clone()])
            .unwrap();

        let mut seen = HashSet::new();
        let mut started = HashSet::new();
        loop {
            match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                ScanMsg::DirStarted { scan_id, path } => {
                    assert_eq!(scan_id, 42);
                    assert!(path == first || path == second);
                    started.insert(path);
                }
                ScanMsg::DirSize {
                    scan_id,
                    path,
                    size,
                    inaccessible,
                    skipped_mounts,
                } => {
                    assert_eq!(scan_id, 42);
                    assert_eq!(inaccessible, 0);
                    assert_eq!(skipped_mounts, 0);
                    assert!(size.logical > 0);
                    assert!(path == first || path == second);
                    seen.insert(path);
                }
                ScanMsg::AllDone { scan_id } => {
                    assert_eq!(scan_id, 42);
                    break;
                }
            }
        }

        assert_eq!(seen.len(), 2);
        assert_eq!(started.len(), 2);
        assert!(seen.contains(&first));
        assert!(seen.contains(&second));
        assert!(started.contains(&first));
        assert!(started.contains(&second));
        fs::remove_dir_all(root).unwrap();
    }

    fn test_root(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "diskr_scanner_{name}_{}_{}",
            std::process::id(),
            nanos
        ))
    }
}
