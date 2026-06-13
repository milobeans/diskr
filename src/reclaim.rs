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
    /// Permission-denied directories encountered while sizing this finding.
    pub inaccessible: u32,
    /// Mounted volumes skipped below /Volumes while sizing this finding.
    pub skipped_mounts: u32,
    /// Number of directories rolled into this finding (1 for fixed locations).
    pub count: usize,
    pub paths: Vec<PathBuf>,
    /// True when this finding's paths are ancestors of one or more other findings'
    /// paths, meaning its bytes are already counted inside those child findings.
    /// Roll-up findings are shown for context but excluded from `report.total`.
    pub rollup: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReclaimReport {
    pub root: PathBuf,
    pub findings: Vec<Finding>,
    pub total: SizeInfo,
    pub inaccessible: u32,
    pub skipped_mounts: u32,
}

/// Scan for reclaimable space under `root`, resolving fixed caches against `$HOME`.
pub fn report(root: &Path) -> ReclaimReport {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    report_with_home(root, home.as_deref())
}

/// Same as [`report`], but with an explicit home directory (used in tests).
pub fn report_with_home(root: &Path, home: Option<&Path>) -> ReclaimReport {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let home = home.map(|home| home.canonicalize().unwrap_or_else(|_| home.to_path_buf()));
    let mut findings = Vec::new();

    if let Some(home) = home.as_deref() {
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

    // Mark a finding as a roll-up when at least one of its paths is a strict
    // prefix of a path in another finding.  This detects parent/child overlaps
    // (e.g. "User caches" ⊇ "Homebrew cache") regardless of which categories
    // are active — no hardcoding required.
    mark_rollups(&mut findings);

    let mut total = SizeInfo::default();
    let mut inaccessible = 0u32;
    let mut skipped_mounts = 0u32;
    for finding in &findings {
        if finding.rollup {
            // Bytes already counted inside child findings; exclude from total.
            continue;
        }
        total.logical = total.logical.saturating_add(finding.size.logical);
        total.allocated = total.allocated.saturating_add(finding.size.allocated);
        inaccessible = inaccessible.saturating_add(finding.inaccessible);
        skipped_mounts = skipped_mounts.saturating_add(finding.skipped_mounts);
    }

    ReclaimReport {
        root,
        findings,
        total,
        inaccessible,
        skipped_mounts,
    }
}

/// For every finding whose paths are a strict prefix of another finding's
/// paths, set `rollup = true`.  A roll-up's bytes are already counted inside
/// its child findings, so the total must exclude it to avoid double-counting.
///
/// Detection is purely structural: path A is a parent of path B when B starts
/// with A as a complete component prefix (i.e. `B.starts_with(A)` and `A != B`
/// in canonical `Path` terms).  No category names are hardcoded.
fn mark_rollups(findings: &mut [Finding]) {
    // Collect a flat list of all child paths for the containment check.
    // Using indices avoids borrow conflicts when we mutate `findings`.
    let path_sets: Vec<Vec<PathBuf>> = findings.iter().map(|f| f.paths.clone()).collect();

    for i in 0..findings.len() {
        // A finding is a roll-up when at least one of its paths is a strict
        // ancestor of at least one path in a *different* finding.
        let is_rollup = path_sets[i].iter().any(|parent_path| {
            path_sets.iter().enumerate().any(|(j, other_paths)| {
                j != i
                    && other_paths
                        .iter()
                        .any(|child_path| is_strict_prefix(parent_path, child_path))
            })
        });
        if is_rollup {
            findings[i].rollup = true;
        }
    }
}

/// Returns true when `ancestor` is a strict path prefix of `descendant`
/// (i.e. `descendant` is inside `ancestor`, not the same path).
fn is_strict_prefix(ancestor: &Path, descendant: &Path) -> bool {
    if ancestor == descendant {
        return false;
    }
    descendant.starts_with(ancestor)
}

fn fixed_findings(root: &Path, home: &Path) -> Vec<Finding> {
    FIXED_CATEGORIES
        .iter()
        .filter_map(|category| {
            let mut size = SizeInfo::default();
            let mut inaccessible = 0u32;
            let mut skipped_mounts = 0u32;
            let mut paths = Vec::new();
            for rel in category.paths {
                let candidate = home.join(rel);
                if !candidate.starts_with(root) || !candidate.is_dir() {
                    continue;
                }
                let scanned = bulkstat::scan_dir(&candidate, 0);
                size.logical = size.logical.saturating_add(scanned.size.logical);
                size.allocated = size.allocated.saturating_add(scanned.size.allocated);
                inaccessible = inaccessible.saturating_add(scanned.inaccessible);
                skipped_mounts = skipped_mounts.saturating_add(scanned.skipped_mounts);
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
                inaccessible,
                skipped_mounts,
                count: paths.len(),
                paths,
                rollup: false,
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
            let mut inaccessible = 0u32;
            let mut skipped_mounts = 0u32;
            let mut paths = Vec::new();
            for path in hits.iter().filter(|(hit_name, _)| hit_name == name) {
                let scanned = bulkstat::scan_dir(&path.1, 0);
                size.logical = size.logical.saturating_add(scanned.size.logical);
                size.allocated = size.allocated.saturating_add(scanned.size.allocated);
                inaccessible = inaccessible.saturating_add(scanned.inaccessible);
                skipped_mounts = skipped_mounts.saturating_add(scanned.skipped_mounts);
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
                inaccessible,
                skipped_mounts,
                count: paths.len(),
                paths,
                rollup: false,
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

    #[test]
    fn fixed_findings_accept_dotted_home_root() {
        let home = test_root("reclaim_dotted_home");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join("Library/Caches")).unwrap();
        fs::write(home.join("Library/Caches/c.bin"), vec![0u8; 4096]).unwrap();

        let report = report_with_home(&home.join("."), Some(&home));

        assert_eq!(
            class_for(&report, "User caches"),
            Some(Reclaimability::Safe)
        );
        assert_eq!(report.root, home.canonicalize().unwrap());
        fs::remove_dir_all(home).unwrap();
    }

    // --- rollup / double-counting tests ---

    /// Parent + child both present: the parent is marked rollup, and the
    /// total equals only the child's bytes, not parent + child.
    #[test]
    fn parent_child_overlap_counts_bytes_once() {
        let root = test_root("reclaim_overlap");
        let _ = fs::remove_dir_all(&root);

        // A file that lives under Library/Caches/Homebrew — counted by
        // both "User caches" (parent) and "Homebrew cache" (child).
        fs::create_dir_all(root.join("Library/Caches/Homebrew")).unwrap();
        fs::write(
            root.join("Library/Caches/Homebrew/bottle.bin"),
            vec![0u8; 8192],
        )
        .unwrap();

        let report = report_with_home(&root, Some(&root));

        let user_caches = report
            .findings
            .iter()
            .find(|f| f.label == "User caches")
            .expect("User caches finding should be present");
        let homebrew = report
            .findings
            .iter()
            .find(|f| f.label == "Homebrew cache")
            .expect("Homebrew cache finding should be present");

        assert!(user_caches.rollup, "User caches should be marked rollup");
        assert!(!homebrew.rollup, "Homebrew cache should NOT be rollup");

        // The total must not exceed the child's size (it shouldn't count the
        // parent's overlapping bytes on top).
        assert!(
            report.total.logical <= homebrew.size.logical,
            "total ({}) must not exceed the disjoint child size ({})",
            report.total.logical,
            homebrew.size.logical,
        );

        fs::remove_dir_all(root).unwrap();
    }

    /// Only a non-overlapping cache is present; total equals its size and it
    /// is NOT marked as a rollup.
    #[test]
    fn non_overlapping_finding_not_marked_rollup() {
        let root = test_root("reclaim_no_overlap");
        let _ = fs::remove_dir_all(&root);

        // A file under Library/Caches/some-app — only "User caches" covers it.
        fs::create_dir_all(root.join("Library/Caches/my-app")).unwrap();
        fs::write(root.join("Library/Caches/my-app/data.bin"), vec![0u8; 4096]).unwrap();

        let report = report_with_home(&root, Some(&root));

        let user_caches = report
            .findings
            .iter()
            .find(|f| f.label == "User caches")
            .expect("User caches finding should be present");

        // No child category matches, so it must not be a rollup.
        assert!(
            !user_caches.rollup,
            "User caches with no child overlap should NOT be rollup"
        );
        assert!(
            report.total.logical >= 4096,
            "total should include the non-overlapping cache bytes"
        );

        fs::remove_dir_all(root).unwrap();
    }

    /// is_strict_prefix: sanity checks for the helper.
    #[test]
    fn is_strict_prefix_basic() {
        use std::path::PathBuf;
        let parent = PathBuf::from("/a/b");
        let child = PathBuf::from("/a/b/c");
        let sibling = PathBuf::from("/a/d");
        let same = PathBuf::from("/a/b");

        assert!(is_strict_prefix(&parent, &child));
        assert!(!is_strict_prefix(&parent, &sibling));
        assert!(!is_strict_prefix(&parent, &same), "same path is not strict");
        assert!(
            !is_strict_prefix(&child, &parent),
            "child not prefix of parent"
        );
    }

    fn test_root(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("diskr_{name}_{}_{}", std::process::id(), nanos))
    }
}
