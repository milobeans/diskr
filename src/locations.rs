use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, TryRecvError};

use anyhow::{bail, Context, Result};

use crate::history;
use crate::packages::InventoryScope;
use crate::terminal_text::{InputEdit, TextCursor};

#[derive(Clone)]
pub struct LocationEntry {
    pub label: String,
    pub path: PathBuf,
    pub profile: Option<String>,
    pub origin: String,
    pub foreign: bool,
    pub scope: Option<InventoryScope>,
}

impl LocationEntry {
    fn root(label: String, path: PathBuf) -> Self {
        Self {
            label,
            path,
            profile: None,
            origin: String::from("local folder"),
            foreign: false,
            scope: None,
        }
    }

    fn profile(name: String, profile: history::ScanProfile, foreign: bool) -> Self {
        Self {
            label: format!("profile: {name}"),
            path: profile.root,
            profile: Some(name),
            origin: if foreign {
                format!("imported from {} (cannot run here)", profile.machine)
            } else {
                String::from("local package profile")
            },
            foreign,
            scope: Some(profile.scope),
        }
    }
}

pub struct ResolvedLocation {
    pub path: PathBuf,
    pub profile: Option<String>,
    pub scope: InventoryScope,
}

#[derive(Default)]
pub struct InventorySelection {
    pub profile: Option<String>,
    pub root: Option<PathBuf>,
    pub scope: InventoryScope,
}

#[derive(Default)]
pub struct LocationChooser {
    pub open: bool,
    pub loading: bool,
    pub selected: usize,
    pub entries: Vec<LocationEntry>,
    pub error: Option<String>,
    pub path_input: Option<String>,
    pub cursor: TextCursor,
    catalog_rx: Option<Receiver<Result<Vec<LocationEntry>, String>>>,
    resolve_rx: Option<Receiver<Result<ResolvedLocation, String>>>,
}

impl LocationChooser {
    pub fn open(&mut self, home: Option<&Path>, latest: impl Iterator<Item = PathBuf>) {
        *self = Self {
            open: true,
            loading: true,
            entries: common_roots(home, latest),
            ..Default::default()
        };
        let (tx, rx) = channel();
        self.catalog_rx = Some(rx);
        std::thread::spawn(move || {
            let result = history::scan_profiles()
                .map(|profiles| {
                    profiles
                        .into_iter()
                        .map(|(name, profile, foreign)| {
                            LocationEntry::profile(name, profile, foreign)
                        })
                        .collect()
                })
                .map_err(|error| format!("saved profiles unavailable: {error}"));
            let _ = tx.send(result);
        });
    }

    pub fn close(&mut self) {
        *self = Self::default();
    }

    pub fn cancel(&mut self) {
        if self.path_input.take().is_some() {
            self.resolve_rx = None;
            self.error = None;
            self.loading = self.catalog_rx.is_some();
        } else {
            self.close();
        }
    }

    pub fn move_selection(&mut self, amount: isize) {
        self.set_selection(self.selected.saturating_add_signed(amount));
    }

    pub fn set_selection(&mut self, selected: usize) {
        self.cancel_resolution();
        self.selected = selected.min(self.entries.len().saturating_sub(1));
    }

    fn cancel_resolution(&mut self) {
        self.resolve_rx = None;
        self.loading = self.catalog_rx.is_some();
    }

    pub fn enter_path(&mut self, cwd: &Path) {
        self.cancel_resolution();
        let input = cwd.to_string_lossy().into_owned();
        self.cursor.end(&input);
        self.path_input = Some(input);
        self.error = None;
    }

    pub fn edit_path(&mut self, edit: InputEdit) {
        self.cancel_resolution();
        if let Some(input) = self.path_input.as_mut() {
            self.cursor.edit(input, edit);
            self.error = None;
        }
    }

    pub fn activate(&mut self, cwd: &Path, home: Option<&Path>) {
        if self.resolve_rx.is_some() {
            return;
        }
        let request = if let Some(input) = &self.path_input {
            match entered_path(input, cwd, home) {
                Ok(path) => LocationRequest::Path(path),
                Err(error) => {
                    self.error = Some(error.to_string());
                    return;
                }
            }
        } else if let Some(entry) = self.entries.get(self.selected) {
            if entry.foreign {
                self.error = Some(String::from(
                    "Imported profiles cannot run here. Save a new local profile with an explicit root and scope.",
                ));
                return;
            }
            match &entry.profile {
                Some(name) => LocationRequest::Profile(name.clone()),
                None => LocationRequest::Path(entry.path.clone()),
            }
        } else {
            return;
        };
        self.error = None;
        self.loading = true;
        let (tx, rx) = channel();
        self.resolve_rx = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(resolve(request).map_err(|error| error.to_string()));
        });
    }

    pub fn poll(&mut self) -> (bool, Option<ResolvedLocation>) {
        let mut changed = false;
        if let Some(rx) = &self.catalog_rx {
            match rx.try_recv() {
                Ok(result) => {
                    match result {
                        Ok(entries) => self.entries.extend(entries),
                        Err(error) => self.error = Some(error),
                    }
                    self.catalog_rx = None;
                    changed = true;
                }
                Err(TryRecvError::Disconnected) => {
                    self.catalog_rx = None;
                    self.error = Some(String::from("saved profile loader stopped"));
                    changed = true;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        let mut resolved = None;
        if let Some(rx) = &self.resolve_rx {
            match rx.try_recv() {
                Ok(result) => {
                    match result {
                        Ok(location) => resolved = Some(location),
                        Err(error) => self.error = Some(error),
                    }
                    self.resolve_rx = None;
                    changed = true;
                }
                Err(TryRecvError::Disconnected) => {
                    self.resolve_rx = None;
                    self.error = Some(String::from("folder lookup stopped"));
                    changed = true;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        self.loading = self.catalog_rx.is_some() || self.resolve_rx.is_some();
        (changed, resolved)
    }
}

enum LocationRequest {
    Path(PathBuf),
    Profile(String),
}

fn resolve(request: LocationRequest) -> Result<ResolvedLocation> {
    let (path, profile, scope) = match request {
        LocationRequest::Path(path) => (path, None, InventoryScope::default()),
        LocationRequest::Profile(name) => {
            let profile = history::scan_profile(&name)?;
            (profile.root, Some(name), profile.scope)
        }
    };
    let path = path.canonicalize().context("cannot open folder")?;
    if !path.is_dir() {
        bail!("selected path is not a folder");
    }
    Ok(ResolvedLocation {
        path,
        profile,
        scope,
    })
}

fn entered_path(input: &str, cwd: &Path, home: Option<&Path>) -> Result<PathBuf> {
    if input.is_empty() || input.contains('\0') {
        bail!("enter a folder path");
    }
    if input == "~" || input.starts_with("~/") {
        let home = home.context("home folder is unavailable")?;
        return Ok(if input == "~" {
            home.to_path_buf()
        } else {
            home.join(&input[2..])
        });
    }
    let path = PathBuf::from(input);
    Ok(if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    })
}

fn common_roots(home: Option<&Path>, latest: impl Iterator<Item = PathBuf>) -> Vec<LocationEntry> {
    let mut entries = Vec::new();
    if let Some(home) = home {
        entries.push(LocationEntry::root(
            String::from("Home"),
            home.to_path_buf(),
        ));
        for name in ["Downloads", "Documents"] {
            entries.push(LocationEntry::root(name.to_owned(), home.join(name)));
        }
    }
    entries.push(LocationEntry::root(
        String::from("Applications"),
        PathBuf::from("/Applications"),
    ));
    entries.push(LocationEntry::root(
        String::from("Root"),
        PathBuf::from("/"),
    ));
    let mut latest: Vec<_> = latest.collect();
    latest.sort();
    for path in latest {
        if !entries.iter().any(|entry| entry.path == path) {
            entries.push(LocationEntry::root(String::from("Saved root"), path));
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entered_paths_keep_spaces_and_resolve_relative_to_the_current_folder() {
        let cwd = Path::new("/work");
        let home = Some(Path::new("/home/demo"));
        assert_eq!(
            entered_path("~/code", cwd, home).unwrap(),
            Path::new("/home/demo/code")
        );
        assert_eq!(
            entered_path(" folder ", cwd, home).unwrap(),
            Path::new("/work/ folder ")
        );
        assert!(entered_path("", cwd, home).is_err());
    }

    #[test]
    fn imported_profile_cannot_start_a_resolution_job() {
        let profile = history::ScanProfile {
            machine: String::from("other-machine"),
            root: PathBuf::from("/untrusted-root"),
            scope: InventoryScope {
                managers: Vec::new(),
                projects: true,
                depth: 2,
                excludes: vec![PathBuf::from("archive")],
            },
        };
        let mut chooser = LocationChooser {
            open: true,
            entries: vec![LocationEntry::profile(String::from("work"), profile, true)],
            ..Default::default()
        };
        chooser.activate(Path::new("/"), None);
        assert!(chooser.resolve_rx.is_none());
        assert!(chooser.error.unwrap().contains("cannot run here"));
        assert_eq!(chooser.entries[0].scope.as_ref().unwrap().depth, 2);
        assert!(chooser.entries[0].origin.contains("other-machine"));
    }

    #[test]
    fn closing_chooser_discards_pending_results() {
        let (tx, rx) = channel();
        let mut chooser = LocationChooser {
            open: true,
            loading: true,
            catalog_rx: Some(rx),
            ..Default::default()
        };
        chooser.close();
        assert!(tx.send(Ok(Vec::new())).is_err());
        assert!(!chooser.poll().0);
        assert!(!chooser.loading);
    }

    #[test]
    fn saved_roots_are_deduplicated_and_sorted_after_common_targets() {
        let entries = common_roots(
            Some(Path::new("/home/demo")),
            vec![
                PathBuf::from("/z"),
                PathBuf::from("/home/demo"),
                PathBuf::from("/a"),
            ]
            .into_iter(),
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.path == Path::new("/home/demo"))
                .count(),
            1
        );
        assert_eq!(entries[entries.len() - 2].path, Path::new("/a"));
        assert_eq!(entries.last().unwrap().path, Path::new("/z"));
    }

    #[test]
    fn changing_selection_discards_an_older_folder_resolution() {
        let (tx, rx) = channel();
        let mut chooser = LocationChooser {
            open: true,
            loading: true,
            resolve_rx: Some(rx),
            entries: common_roots(None, std::iter::empty()),
            ..Default::default()
        };
        chooser.move_selection(1);
        assert!(tx
            .send(Ok(ResolvedLocation {
                path: PathBuf::from("/old-selection"),
                profile: None,
                scope: Default::default(),
            }))
            .is_err());
        assert!(chooser.poll().1.is_none());
        assert!(!chooser.loading);
    }
}
