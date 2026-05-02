use std::path::PathBuf;
use std::sync::mpsc::Sender;

use crate::bulkstat;

pub type ScanId = u64;

/// Messages from the scanner thread back to the UI.
pub enum ScanMsg {
    DirSize {
        scan_id: ScanId,
        path: PathBuf,
        size: u64,
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

    /// Scan each directory in `dirs` for its TOTAL recursive size on disk.
    /// Must NOT block the UI thread.
    pub fn scan_all(&self, scan_id: ScanId, dirs: Vec<PathBuf>) {
        let tx = self.tx.clone();
        let _ = std::thread::Builder::new()
            .name(String::from("diskr-scan"))
            .spawn(move || {
                for dir in dirs {
                    let size = bulkstat::size_of_dir(&dir);
                    let _ = tx.send(ScanMsg::DirSize {
                        scan_id,
                        path: dir,
                        size,
                    });
                }
                let _ = tx.send(ScanMsg::AllDone { scan_id });
            });
    }
}
