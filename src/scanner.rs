use std::path::PathBuf;
use std::sync::mpsc::Sender;

use crate::bulkstat;
use crate::bulkstat::SizeInfo;
use rayon::prelude::*;

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
                dirs.into_par_iter().for_each(|dir| {
                    let scan = bulkstat::scan_dir(&dir, 0);
                    let _ = tx.send(ScanMsg::DirSize {
                        scan_id,
                        path: dir,
                        size: scan.size,
                        inaccessible: scan.inaccessible,
                    });
                });

                let _ = tx.send(ScanMsg::AllDone { scan_id });
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
        loop {
            match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                ScanMsg::DirSize {
                    scan_id,
                    path,
                    size,
                    inaccessible,
                } => {
                    assert_eq!(scan_id, 42);
                    assert_eq!(inaccessible, 0);
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
        assert!(seen.contains(&first));
        assert!(seen.contains(&second));
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
