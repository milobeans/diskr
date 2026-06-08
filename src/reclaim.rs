//! Reclaimable-space detector for macOS.
//!
//! Two complementary passes find space that is usually safe to free:
//!   1. Fixed, well-known cache locations (Xcode DerivedData, Homebrew, package
//!      manager caches, browser caches, Trash, …) resolved relative to `$HOME`.
//!   2. A bounded recursive walk for repeated build-artifact directories
//!      (`node_modules`, `target`, `.venv`, …) under the scan root.
//!
//! Every reported item carries a reclaimability class so the report can explain
//! *why* something is safe to delete, not just how big it is. Sizing is delegated
//! to the fast `getattrlistbulk(2)` walker in [`crate::bulkstat`].

use std::path::{Path, PathBuf};

use crate::bulkstat::{self, SizeInfo};

/// How safe a category is to delete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reclaimability {
    /// Pure cache; the system or tool regenerates it automatically with no real loss.
    Safe,
    /// Rebuildable, but reclaiming costs time or bandwidth (recompile, re-download).
    Regenerable,
    /// May contain data you cannot easily recreate, or deleting it can break things.
    Risky,
}

impl Reclaimability {
    pub fn label(self) -> &'static str {
        match self {
            Reclaimability::Safe => "safe",
            Reclaimability::Regenerable => "regenerable",
            Reclaimability::Risky => "risky",
        }
    }
}

/// A fixed location keyed off `$HOME`. Multiple candidate paths can map to one
/// category (e.g. pip caches living under either `~/.cache` or `~/Library/Caches`).
struct FixedCategory {
    label: &'static str,
    class: Reclaimability,
    note: &'static str,
    /// Paths relative to `$HOME` (none begin with `/`).
    paths: &'static [&'static str],
}

const FIXED_CATEGORIES: &[FixedCategory] = &[
    FixedCategory {
        label: "User caches",
        class: Reclaimability::Safe,
        note: "App and system caches; rebuilt on demand.",
        paths: &["Library/Caches"],
    },
    FixedCategory {
        label: "Xcode DerivedData",
        class: Reclaimability::Safe,
        note: "Build intermediates; Xcode regenerates them on the next build.",
        paths: &["Library/Developer/Xcode/DerivedData"],
    },
    FixedCategory {
        label: "Xcode Archives",
        class: Reclaimability::Risky,
        note: "Shipped app archives and dSYMs; these are not regenerable.",
        paths: &["Library/Developer/Xcode/Archives"],
    },
    FixedCategory {
        label: "iOS DeviceSupport",
        class: Reclaimability::Regenerable,
        note: "Symbol caches re-created when you reconnect a device.",
        paths: &["Library/Developer/Xcode/iOS DeviceSupport"],
    },
    FixedCategory {
        label: "Simulator devices",
        class: Reclaimability::Regenerable,
        note: "Simulator state and installed apps; Xcode can recreate them.",
        paths: &["Library/Developer/CoreSimulator/Devices"],
    },
    FixedCategory {
        label: "Homebrew cache",
        class: Reclaimability::Safe,
        note: "Downloaded bottles; `brew cleanup` removes these.",
        paths: &["Library/Caches/Homebrew"],
    },
    FixedCategory {
        label: "Cargo cache",
        class: Reclaimability::Regenerable,
        note: "Downloaded crates and git checkouts; re-fetched on build.",
        paths: &[".cargo/registry", ".cargo/git"],
    },
    FixedCategory {
        label: "npm cache",
        class: Reclaimability::Safe,
        note: "npm package cache; rebuilt on the next install.",
        paths: &[".npm"],
    },
    FixedCategory {
        label: "Yarn cache",
        class: Reclaimability::Safe,
        note: "Yarn package cache; rebuilt on the next install.",
        paths: &["Library/Caches/Yarn", ".cache/yarn"],
    },
    FixedCategory {
        label: "pnpm store",
        class: Reclaimability::Regenerable,
        note: "Shared package store; re-fetched on install (breaks linked installs).",
        paths: &["Library/pnpm/store", ".local/share/pnpm/store"],
    },
    FixedCategory {
        label: "pip cache",
        class: Reclaimability::Safe,
        note: "Downloaded wheels; re-fetched on the next install.",
        paths: &["Library/Caches/pip", ".cache/pip"],
    },
    FixedCategory {
        label: "uv cache",
        class: Reclaimability::Safe,
        note: "uv package cache; re-fetched on the next install.",
        paths: &["Library/Caches/uv", ".cache/uv"],
    },
    FixedCategory {
        label: "Go module/build cache",
        class: Reclaimability::Regenerable,
        note: "Go module and build caches; rebuilt by the toolchain.",
        paths: &["Library/Caches/go-build", "go/pkg/mod"],
    },
    FixedCategory {
        label: "Trash",
        class: Reclaimability::Safe,
        note: "Already discarded items; emptying is permanent.",
        paths: &[".Trash"],
    },
    FixedCategory {
        label: "Chrome cache",
        class: Reclaimability::Safe,
        note: "Browser cache; rebuilt as you browse.",
        paths: &["Library/Caches/Google/Chrome"],
    },
    FixedCategory {
        label: "Safari cache",
        class: Reclaimability::Safe,
        note: "Browser cache; rebuilt as you browse.",
        paths: &["Library/Caches/com.apple.Safari"],
    },
    FixedCategory {
        label: "Docker data",
        class: Reclaimability::Risky,
        note: "Docker images and volumes; may hold data you have not pushed.",
        paths: &["Library/Containers/com.docker.docker/Data/vms"],
    },
];

/// Build-artifact directory names found by the recursive pass. Each entry is
/// deliberately unambiguous — generic names like `build`/`dist` are excluded to
/// avoid flagging real user data.
const ARTIFACTS: &[(&str, Reclaimability, &str)] = &[
    (
        "node_modules",
        Reclaimability::Regenerable,
        "Node dependencies; `npm install` restores them.",
    ),
    (
        "target",
        Reclaimability::Regenerable,
        "Rust/Cargo build output; `cargo build` restores it.",
    ),
    (
        ".venv",
        Reclaimability::Regenerable,
        "Python virtualenv; recreate from requirements.",
    ),
    (
        "venv",
        Reclaimability::Regenerable,
        "Python virtualenv; recreate from requirements.",
    ),
    (
        "__pycache__",
        Reclaimability::Safe,
        "Python bytecode cache; regenerated on import.",
    ),
    (
        ".next",
        Reclaimability::Safe,
        "Next.js build cache; rebuilt on the next build.",
    ),
    (
        ".gradle",
        Reclaimability::Regenerable,
        "Gradle caches and build state; rebuilt on demand.",
    ),
];

/// How deep the recursive artifact walk descends before giving up.
const MAX_ARTIFACT_DEPTH: usize = 10;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub label: String,
    pub class: Reclaimability,
    pub note: String,
    pub size: SizeInfo,
    /// Number of directories rolled into this finding (1 for fixed locations).
    pub count: usize,
    pub paths: Vec<PathBuf>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReclaimReport {
    pub root: PathBuf,
    pub findings: Vec<Finding>,
    pub total: SizeInfo,
}

/// Scan for reclaimable space under `root`, resolving fixed caches against `$HOME`.
pub fn report(root: &Path) -> ReclaimReport {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    report_with_home(root, home.as_deref())
}

/// Same as [`report`], but with an explicit home directory (used in tests).
pub fn report_with_home(root: &Path, home: Option<&Path>) -> ReclaimReport {
    let root = root.to_path_buf();
    let mut findings = Vec::new();

    if let Some(home) = home {
        findings.extend(fixed_findings(&root, home));
    }
    findings.extend(artifact_findings(&root));

    findings.retain(|finding| finding.size.allocated > 0 || finding.size.logical > 0);
    findings.sort_by(|a, b| {
        b.size
            .allocated
            .cmp(&a.size.allocated)
            .then(b.size.logical.cmp(&a.size.logical))
            .then(a.label.cmp(&b.label))
    });

    let mut total = SizeInfo::default();
    for finding in &findings {
        total.logical = total.logical.saturating_add(finding.size.logical);
        total.allocated = total.allocated.saturating_add(finding.size.allocated);
    }

    ReclaimReport {
        root,
        findings,
        total,
    }
}

fn fixed_findings(root: &Path, home: &Path) -> Vec<Finding> {
    FIXED_CATEGORIES
        .iter()
        .filter_map(|category| {
            let mut size = SizeInfo::default();
            let mut paths = Vec::new();
            for rel in category.paths {
                let candidate = home.join(rel);
                if !candidate.starts_with(root) || !candidate.is_dir() {
                    continue;
                }
                let scanned = bulkstat::scan_dir(&candidate, 0).size;
                size.logical = size.logical.saturating_add(scanned.logical);
                size.allocated = size.allocated.saturating_add(scanned.allocated);
                paths.push(candidate);
            }
            if paths.is_empty() {
                return None;
            }
            Some(Finding {
                label: category.label.to_string(),
                class: category.class,
                note: category.note.to_string(),
                size,
                count: paths.len(),
                paths,
            })
        })
        .collect()
}

fn artifact_findings(root: &Path) -> Vec<Finding> {
    let hits = find_artifacts(root);
    // Preserve the declared ARTIFACTS order so output is stable before sorting.
    ARTIFACTS
        .iter()
        .filter_map(|(name, class, note)| {
            let mut size = SizeInfo::default();
            let mut paths = Vec::new();
            for path in hits.iter().filter(|(hit_name, _)| hit_name == name) {
                let scanned = bulkstat::scan_dir(&path.1, 0).size;
                size.logical = size.logical.saturating_add(scanned.logical);
                size.allocated = size.allocated.saturating_add(scanned.allocated);
                paths.push(path.1.clone());
            }
            if paths.is_empty() {
                return None;
            }
            Some(Finding {
                label: (*name).to_string(),
                class: *class,
                note: (*note).to_string(),
                size,
                count: paths.len(),
                paths,
            })
        })
        .collect()
}

/// Walk `root` for artifact directories by name. Matched directories are recorded
/// but not descended into, symlinks are never followed, and the `Library` tree and
/// unrelated hidden directories are skipped to keep the walk fast and focused on
/// project trees.
fn find_artifacts(root: &Path) -> Vec<(&'static str, PathBuf)> {
    let mut hits = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];

    while let Some((dir, depth)) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if let Some((matched, _, _)) = ARTIFACTS.iter().find(|(n, _, _)| *n == name) {
                hits.push((*matched, entry.path()));
                continue; // do not descend into a matched artifact directory
            }
            if depth < MAX_ARTIFACT_DEPTH && should_descend(name) {
                stack.push((entry.path(), depth + 1));
            }
        }
    }
    hits
}

/// Skip the `Library` tree (its caches are covered by fixed categories) and any
/// hidden directory that is not itself an artifact name.
fn should_descend(name: &str) -> bool {
    if name == "Library" {
        return false;
    }
    !name.starts_with('.')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn class_for(report: &ReclaimReport, label: &str) -> Option<Reclaimability> {
        report
            .findings
            .iter()
            .find(|finding| finding.label == label)
            .map(|finding| finding.class)
    }

    #[test]
    fn finds_fixed_caches_and_project_artifacts() {
        let root = test_root("reclaim");
        let _ = fs::remove_dir_all(&root);
        // Fixed cache under the resolved home.
        fs::create_dir_all(root.join("Library/Caches")).unwrap();
        fs::write(root.join("Library/Caches/big.bin"), vec![0u8; 8192]).unwrap();
        // Two separate project artifact directories.
        fs::create_dir_all(root.join("proj/node_modules/dep")).unwrap();
        fs::write(root.join("proj/node_modules/dep/index.js"), vec![0u8; 4096]).unwrap();
        fs::create_dir_all(root.join("work/app/node_modules")).unwrap();
        fs::write(root.join("work/app/node_modules/lib.js"), vec![0u8; 2048]).unwrap();

        let report = report_with_home(&root, Some(&root));

        let caches = report
            .findings
            .iter()
            .find(|f| f.label == "User caches")
            .expect("user caches finding");
        assert_eq!(caches.count, 1);
        assert!(caches.size.logical >= 8192);

        let node = report
            .findings
            .iter()
            .find(|f| f.label == "node_modules")
            .expect("node_modules finding");
        assert_eq!(node.count, 2, "both node_modules trees should roll up");
        assert_eq!(node.class, Reclaimability::Regenerable);

        assert!(report.total.allocated >= report.total.logical.min(1));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn does_not_descend_into_matched_artifacts() {
        let root = test_root("reclaim_nested");
        let _ = fs::remove_dir_all(&root);
        // A node_modules that itself contains a nested node_modules.
        fs::create_dir_all(root.join("proj/node_modules/dep/node_modules")).unwrap();
        fs::write(
            root.join("proj/node_modules/dep/node_modules/inner.js"),
            vec![0u8; 1024],
        )
        .unwrap();

        let hits = find_artifacts(&root);
        let node_hits = hits
            .iter()
            .filter(|(name, _)| *name == "node_modules")
            .count();
        assert_eq!(
            node_hits, 1,
            "nested node_modules must not be double counted"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn skips_library_tree_in_recursive_pass() {
        let root = test_root("reclaim_library");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("Library/weird/node_modules")).unwrap();
        fs::write(root.join("Library/weird/node_modules/x.js"), vec![0u8; 512]).unwrap();

        let hits = find_artifacts(&root);
        assert!(
            hits.is_empty(),
            "recursive pass should not walk into the Library tree"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn excludes_paths_outside_the_root() {
        let home = test_root("reclaim_home");
        let root = home.join("scoped");
        let _ = fs::remove_dir_all(&home);
        // A cache that exists in home but is outside the requested root.
        fs::create_dir_all(home.join("Library/Caches")).unwrap();
        fs::write(home.join("Library/Caches/c.bin"), vec![0u8; 4096]).unwrap();
        fs::create_dir_all(&root).unwrap();

        let report = report_with_home(&root, Some(&home));
        assert!(
            class_for(&report, "User caches").is_none(),
            "caches outside root must be excluded"
        );
        fs::remove_dir_all(home).unwrap();
    }

    fn test_root(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("diskr_{name}_{}_{}", std::process::id(), nanos))
    }
}
