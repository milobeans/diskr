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
        class: Reclaimability::Risky,
        note: "Device sandboxes, test data and installed apps may be irreplaceable. Review devices in Xcode; use simctl delete unavailable only after checking needed state.",
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
        class: Reclaimability::Risky,
        note: "Review discarded files before emptying; they are not regenerated and permanent removal cannot be undone.",
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
        "Python environment marker found; export dependencies and preserve local changes before recreating.",
    ),
    (
        "venv",
        Reclaimability::Regenerable,
        "Python environment marker found; export dependencies and preserve local changes before recreating.",
    ),
    (
        "__pycache__",
        Reclaimability::Safe,
        "Python bytecode cache; regenerated on import.",
    ),
    (
        ".next",
        Reclaimability::Risky,
        "Next.js output may be the deployed application. Rebuild and redeploy from retained source before removal.",
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
    /// paths. Roll-up findings are shown as contextual subtotals; `report.total`
    /// is independently calculated from the union of every finding path.
    pub rollup: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReclaimReport {
    pub root: PathBuf,
    pub findings: Vec<Finding>,
    pub total: SizeInfo,
    pub inaccessible: u32,
    pub skipped_mounts: u32,
    pub scope: bulkstat::DiscoveryCoverage,
}

struct FindingBuild {
    finding: Finding,
    contributions: Vec<PathContribution>,
}

#[derive(Clone)]
struct PathContribution {
    path: PathBuf,
    size: SizeInfo,
    inaccessible: u32,
    skipped_mounts: u32,
    hard_links: Vec<(u64, u64, SizeInfo)>,
}

/// Scan for reclaimable space under `root`, resolving fixed caches against `$HOME`.
pub fn report(root: &Path) -> ReclaimReport {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    report_with_home(root, home.as_deref())
}

/// Classify a file-browser row purely by its name and path, reusing the same
/// artifact-name and fixed-cache rules the reclaim report uses, so the engine's
/// knowledge surfaces on the main surface and not only inside the Reclaim
/// overlay. Name/path comparison only — no I/O on the render path. Returns the
/// reclaimability class so the caller can render a colored chip.
pub fn classify_by_name_path(
    name: &str,
    path: &Path,
    home: Option<&Path>,
) -> Option<Reclaimability> {
    let _ = name;
    if let Some(home) = home {
        for category in FIXED_CATEGORIES {
            for rel in category.paths {
                if path == home.join(rel) {
                    return Some(category.class);
                }
            }
        }
    }
    None
}

pub(crate) fn classify_path(path: &Path, home: Option<&Path>) -> Option<Reclaimability> {
    let name = path.file_name()?.to_str()?;
    classify_by_name_path(name, path, home).or_else(|| {
        ARTIFACTS
            .iter()
            .find(|(n, _, _)| *n == name && artifact_evidence(path, name))
            .map(|(_, class, _)| *class)
    })
}

fn artifact_evidence(path: &Path, name: &str) -> bool {
    let parent = path.parent().unwrap_or(path);
    match name {
        "target" => parent.join("Cargo.toml").is_file(),
        "node_modules" => parent.join("package.json").is_file(),
        "venv" | ".venv" => path.join("pyvenv.cfg").is_file(),
        "__pycache__" => std::fs::read_dir(path).ok().is_some_and(|entries| {
            entries
                .flatten()
                .any(|e| e.path().extension().is_some_and(|x| x == "pyc"))
        }),
        ".next" => std::fs::read_to_string(parent.join("package.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .is_some_and(|v| {
                ["dependencies", "devDependencies"]
                    .iter()
                    .any(|k| v.get(k).and_then(|d| d.get("next")).is_some())
            }),
        ".gradle" => [
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "settings.gradle.kts",
        ]
        .iter()
        .any(|m| parent.join(m).is_file()),
        _ => false,
    }
}

/// Same as [`report`], but with an explicit home directory (used in tests).
pub fn report_with_home(root: &Path, home: Option<&Path>) -> ReclaimReport {
    report_with_home_cancellation(root, home, &bulkstat::ScanCancellation::never())
        .expect("uncancelled reclaim report")
}

pub fn report_with_cancellation(
    root: &Path,
    cancellation: &bulkstat::ScanCancellation,
) -> Option<ReclaimReport> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    report_with_home_cancellation(root, home.as_deref(), cancellation)
}

fn report_with_home_cancellation(
    root: &Path,
    home: Option<&Path>,
    cancellation: &bulkstat::ScanCancellation,
) -> Option<ReclaimReport> {
    if cancellation.is_cancelled() {
        return None;
    }
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let home = home.map(|home| home.canonicalize().unwrap_or_else(|_| home.to_path_buf()));
    let mut builds = Vec::new();

    if let Some(home) = home.as_deref() {
        builds.extend(fixed_findings(&root, home, cancellation));
    }
    if cancellation.is_cancelled() {
        return None;
    }
    let (artifact_builds, artifact_search_errors, scope) = artifact_findings(&root, cancellation);
    if cancellation.is_cancelled() {
        return None;
    }
    builds.extend(artifact_builds);

    for build in &mut builds {
        let (size, inaccessible, skipped_mounts) = union_totals(build.contributions.iter());
        build.finding.size = size;
        build.finding.inaccessible = inaccessible;
        build.finding.skipped_mounts = skipped_mounts;
    }

    builds.retain(|build| {
        build.finding.size.allocated > 0
            || build.finding.size.logical > 0
            || build.finding.inaccessible > 0
            || build.finding.skipped_mounts > 0
    });
    let (total, mut inaccessible, mut skipped_mounts) =
        union_totals(builds.iter().flat_map(|build| build.contributions.iter()));
    inaccessible = inaccessible.saturating_add(artifact_search_errors);
    skipped_mounts = skipped_mounts.saturating_add(scope.skipped_mounts);
    let mut findings: Vec<Finding> = builds.into_iter().map(|build| build.finding).collect();

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

    Some(ReclaimReport {
        root,
        findings,
        total,
        inaccessible,
        skipped_mounts,
        scope,
    })
}

/// Sum the minimal set of non-overlapping roots. Per-path contributions retain
/// bytes that belong to a multi-path category outside an overlapping parent,
/// while ancestors cover their descendants exactly once.
fn union_totals<'a>(
    contributions: impl Iterator<Item = &'a PathContribution>,
) -> (SizeInfo, u32, u32) {
    let mut ordered: Vec<PathContribution> = contributions.cloned().collect();
    ordered.sort_by(|a, b| {
        a.path
            .components()
            .count()
            .cmp(&b.path.components().count())
            .then_with(|| a.path.cmp(&b.path))
    });

    let mut roots: Vec<PathBuf> = Vec::new();
    let mut total = SizeInfo::default();
    let mut inaccessible = 0u32;
    let mut skipped_mounts = 0u32;
    let mut hard_links = std::collections::HashSet::new();
    for contribution in ordered {
        if roots.iter().any(|root| contribution.path.starts_with(root)) {
            continue;
        }
        total.logical = total.logical.saturating_add(contribution.size.logical);
        total.allocated = total.allocated.saturating_add(contribution.size.allocated);
        for (dev, ino, size) in &contribution.hard_links {
            if !hard_links.insert((*dev, *ino)) {
                total.logical = total.logical.saturating_sub(size.logical);
                total.allocated = total.allocated.saturating_sub(size.allocated);
            }
        }
        inaccessible = inaccessible.saturating_add(contribution.inaccessible);
        skipped_mounts = skipped_mounts.saturating_add(contribution.skipped_mounts);
        roots.push(contribution.path);
    }
    (total, inaccessible, skipped_mounts)
}

/// For every finding whose paths are a strict prefix of another finding's
/// paths, set `rollup = true`. Roll-ups remain visible as category subtotals;
/// report totals are calculated separately from the union of their paths.
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

fn fixed_findings(
    root: &Path,
    home: &Path,
    cancellation: &bulkstat::ScanCancellation,
) -> Vec<FindingBuild> {
    FIXED_CATEGORIES
        .iter()
        .filter_map(|category| {
            let mut size = SizeInfo::default();
            let mut inaccessible = 0u32;
            let mut skipped_mounts = 0u32;
            let mut paths = Vec::new();
            let mut contributions = Vec::new();
            for rel in category.paths {
                if cancellation.is_cancelled() {
                    return None;
                }
                let known = home.join(rel);
                let candidate = if root.starts_with(&known) {
                    root.to_path_buf()
                } else {
                    known
                };
                if !candidate.starts_with(root) || !candidate.is_dir() {
                    continue;
                }
                let scope = bulkstat::ScanScope::new(root).ok().map(|s| s.fresh());
                let scanned =
                    bulkstat::scan_dir_in_scope(&candidate, 0, cancellation, scope.as_ref())
                        .unwrap_or_default();
                size.logical = size.logical.saturating_add(scanned.size.logical);
                size.allocated = size.allocated.saturating_add(scanned.size.allocated);
                inaccessible = inaccessible.saturating_add(scanned.inaccessible);
                skipped_mounts = skipped_mounts.saturating_add(scanned.skipped_mounts);
                paths.push(candidate.clone());
                contributions.push(PathContribution {
                    path: candidate,
                    size: scanned.size,
                    inaccessible: scanned.inaccessible,
                    skipped_mounts: scanned.skipped_mounts,
                    hard_links: scanned.hard_links,
                });
            }
            if paths.is_empty() {
                return None;
            }
            Some(FindingBuild {
                finding: Finding {
                    label: category.label.to_string(),
                    class: category.class,
                    note: category.note.to_string(),
                    size,
                    inaccessible,
                    skipped_mounts,
                    count: paths.len(),
                    paths,
                    rollup: false,
                },
                contributions,
            })
        })
        .collect()
}

fn artifact_findings(
    root: &Path,
    cancellation: &bulkstat::ScanCancellation,
) -> (Vec<FindingBuild>, u32, bulkstat::DiscoveryCoverage) {
    let search = find_artifacts_cancellable(root, cancellation);
    // Preserve the declared ARTIFACTS order so output is stable before sorting.
    let findings = ARTIFACTS
        .iter()
        .filter_map(|(name, class, note)| {
            let mut size = SizeInfo::default();
            let mut inaccessible = 0u32;
            let mut skipped_mounts = 0u32;
            let mut paths = Vec::new();
            let mut contributions = Vec::new();
            for path in search.hits.iter().filter(|(hit_name, _)| hit_name == name) {
                if cancellation.is_cancelled() {
                    return None;
                }
                let scope = bulkstat::ScanScope::new(root).ok().map(|s| s.fresh());
                let scanned = bulkstat::scan_dir_in_scope(&path.1, 0, cancellation, scope.as_ref())
                    .unwrap_or_default();
                size.logical = size.logical.saturating_add(scanned.size.logical);
                size.allocated = size.allocated.saturating_add(scanned.size.allocated);
                inaccessible = inaccessible.saturating_add(scanned.inaccessible);
                skipped_mounts = skipped_mounts.saturating_add(scanned.skipped_mounts);
                paths.push(path.1.clone());
                contributions.push(PathContribution {
                    path: path.1.clone(),
                    size: scanned.size,
                    inaccessible: scanned.inaccessible,
                    skipped_mounts: scanned.skipped_mounts,
                    hard_links: scanned.hard_links,
                });
            }
            if paths.is_empty() {
                return None;
            }
            Some(FindingBuild {
                finding: Finding {
                    label: (*name).to_string(),
                    class: *class,
                    note: (*note).to_string(),
                    size,
                    inaccessible,
                    skipped_mounts,
                    count: paths.len(),
                    paths,
                    rollup: false,
                },
                contributions,
            })
        })
        .collect();
    (findings, search.inaccessible, search.scope)
}

struct ArtifactSearch {
    hits: Vec<(&'static str, PathBuf)>,
    inaccessible: u32,
    scope: bulkstat::DiscoveryCoverage,
}

/// Walk `root` for artifact directories by name. Matched directories are recorded
/// but not descended into, symlinks are never followed, and the `Library` tree and
/// unrelated hidden directories are skipped to keep the walk fast and focused on
/// project trees.
#[cfg(test)]
fn find_artifacts(root: &Path) -> ArtifactSearch {
    find_artifacts_cancellable(root, &bulkstat::ScanCancellation::never())
}

fn find_artifacts_cancellable(
    root: &Path,
    cancellation: &bulkstat::ScanCancellation,
) -> ArtifactSearch {
    let mut hits = Vec::new();
    let mut inaccessible = 0u32;
    let scan_scope = bulkstat::ScanScope::new(root).ok();
    let mut scope = bulkstat::DiscoveryCoverage {
        depth_limit: MAX_ARTIFACT_DEPTH,
        ..Default::default()
    };
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];

    if let Some(name) = root.file_name().and_then(|n| n.to_str()) {
        if let Some((matched, _, _)) = ARTIFACTS
            .iter()
            .find(|(n, _, _)| *n == name && artifact_evidence(root, name))
        {
            hits.push((*matched, root.to_path_buf()));
            return ArtifactSearch {
                hits,
                inaccessible,
                scope,
            };
        }
    }

    while let Some((dir, depth)) = stack.pop() {
        if cancellation.is_cancelled() {
            break;
        }
        if let (Some(policy), Ok(meta)) = (&scan_scope, std::fs::symlink_metadata(&dir)) {
            if !policy.accepts(&dir, &meta) {
                scope.skipped_mounts = scope.skipped_mounts.saturating_add(1);
                continue;
            }
        }
        let read = match std::fs::read_dir(&dir) {
            Ok(read) => read,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                inaccessible = inaccessible.saturating_add(1);
                continue;
            }
        };
        for entry in read {
            if cancellation.is_cancelled() {
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => {
                    inaccessible = inaccessible.saturating_add(1);
                    continue;
                }
            };
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => {
                    inaccessible = inaccessible.saturating_add(1);
                    continue;
                }
            };
            if !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if let Some((matched, _, _)) = ARTIFACTS
                .iter()
                .find(|(n, _, _)| *n == name && artifact_evidence(&entry.path(), name))
            {
                hits.push((*matched, entry.path()));
                continue; // do not descend into a matched artifact directory
            }
            if depth < MAX_ARTIFACT_DEPTH && should_descend(name) {
                stack.push((entry.path(), depth + 1));
            } else if !should_descend(name) {
                scope.excluded_policy = scope.excluded_policy.saturating_add(1);
            } else {
                scope.depth_limited = scope.depth_limited.saturating_add(1);
            }
        }
    }
    ArtifactSearch {
        hits,
        inaccessible,
        scope,
    }
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

    #[test]
    fn cancelled_reclaim_never_becomes_a_confident_empty_report() {
        let root = test_root("cancelled_reclaim");
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("Cargo.toml"), b"[package]\nname='fixture'\n").unwrap();
        std::fs::write(root.join("target/file"), b"fixture").unwrap();
        let generation = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
        let cancellation = bulkstat::ScanCancellation::new(generation, 0);
        assert!(report_with_home_cancellation(&root, None, &cancellation).is_none());
        assert!(!report_with_home(&root, None).findings.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
    use std::fs;

    #[test]
    fn artifact_requires_evidence_and_selected_root_is_classified() {
        let root = test_root("artifact_evidence");
        let target = root.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("personal"), b"data").unwrap();
        assert!(report_with_home(&root, None).findings.is_empty());
        assert_eq!(classify_path(&target, None), None);
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='fixture'\nversion='0.0.0'\n",
        )
        .unwrap();
        let parent = report_with_home(&root, None);
        let selected = report_with_home(&target, None);
        assert_eq!(parent.total, selected.total);
        assert_eq!(
            selected.findings[0].paths,
            vec![target.canonicalize().unwrap()]
        );
        assert_eq!(
            FIXED_CATEGORIES
                .iter()
                .find(|c| c.label == "Simulator devices")
                .unwrap()
                .class,
            Reclaimability::Risky
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fully_unreadable_candidate_is_retained_with_zero_known_bytes() {
        use std::os::unix::fs::PermissionsExt;
        let root = test_root("unknown_candidate");
        let target = root.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(root.join("Cargo.toml"), "").unwrap();
        fs::write(target.join("data"), b"data").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o000)).unwrap();
        let report = report_with_home(&root, None);
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.total.logical, 0);
        assert!(report.inaccessible > 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sibling_artifacts_count_shared_hardlinks_once_in_category_and_total() {
        let root = test_root("candidate_hardlinks");
        for p in ["a", "b"] {
            fs::create_dir_all(root.join(p).join("node_modules")).unwrap();
            fs::write(root.join(p).join("package.json"), "{}").unwrap();
        }
        fs::write(root.join("a/node_modules/data"), vec![0u8; 8192]).unwrap();
        fs::hard_link(
            root.join("a/node_modules/data"),
            root.join("b/node_modules/data"),
        )
        .unwrap();
        let report = report_with_home(&root, None);
        assert_eq!(report.total.logical, 8192);
        assert_eq!(report.findings[0].size.logical, 8192);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn discovery_reports_hidden_and_depth_exclusions_even_with_no_findings() {
        let root = test_root("discovery_scope");
        fs::create_dir_all(root.join(".hidden")).unwrap();
        fs::create_dir_all(root.join("Library")).unwrap();
        let mut deep = root.clone();
        for _ in 0..13 {
            deep = deep.join("level");
            fs::create_dir_all(&deep).unwrap();
        }
        let report = report_with_home(&root, None);
        assert!(report.findings.is_empty());
        assert!(report.scope.depth_limited > 0);
        assert!(report.scope.excluded_policy >= 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn classify_matches_artifacts_and_fixed_caches() {
        // A name alone is not sufficient evidence for a cleanup recipe.
        assert_eq!(
            classify_by_name_path("node_modules", Path::new("/x/node_modules"), None),
            None
        );
        assert_eq!(
            classify_by_name_path("__pycache__", Path::new("/x/__pycache__"), None),
            None
        );
        // A fixed cache only matches when the path equals a home-relative root.
        let home = Path::new("/Users/example");
        assert_eq!(
            classify_by_name_path("Caches", &home.join("Library/Caches"), Some(home)),
            Some(Reclaimability::Safe)
        );
        assert_eq!(
            classify_by_name_path(".Trash", &home.join(".Trash"), Some(home)),
            Some(Reclaimability::Risky)
        );
        assert_eq!(
            classify_by_name_path("Caches", Path::new("/elsewhere/Caches"), Some(home)),
            None
        );
        // Ordinary directories are not classified.
        assert_eq!(
            classify_by_name_path("src", Path::new("/x/src"), Some(home)),
            None
        );
    }

    fn class_for(report: &ReclaimReport, label: &str) -> Option<Reclaimability> {
        report
            .findings
            .iter()
            .find(|finding| finding.label == label)
            .map(|finding| finding.class)
    }

    #[test]
    fn trash_requires_review_and_is_not_a_safe_cache_finding() {
        let root = test_root("trash_classification");
        fs::create_dir_all(root.join(".Trash")).unwrap();
        fs::create_dir_all(root.join("Library/Caches")).unwrap();
        fs::write(root.join(".Trash/discarded-document"), b"unique contents").unwrap();
        fs::write(root.join("Library/Caches/cache"), b"cached contents").unwrap();

        let report = report_with_home(&root, Some(&root));
        assert_eq!(class_for(&report, "Trash"), Some(Reclaimability::Risky));
        assert_eq!(
            class_for(&report, "User caches"),
            Some(Reclaimability::Safe)
        );
        let trash = report
            .findings
            .iter()
            .find(|finding| finding.label == "Trash")
            .unwrap();
        assert!(trash.note.contains("Review discarded files"));
        assert!(trash.note.contains("permanent removal cannot be undone"));
        assert!(!report
            .findings
            .iter()
            .filter(|finding| finding.class == Reclaimability::Safe)
            .any(|finding| finding.label == "Trash"));
        fs::remove_dir_all(root).unwrap();
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
        fs::write(root.join("proj/package.json"), "{}").unwrap();
        fs::write(root.join("proj/node_modules/dep/index.js"), vec![0u8; 4096]).unwrap();
        fs::create_dir_all(root.join("work/app/node_modules")).unwrap();
        fs::write(root.join("work/app/package.json"), "{}").unwrap();
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
        fs::write(root.join("proj/package.json"), "{}").unwrap();
        fs::write(
            root.join("proj/node_modules/dep/node_modules/inner.js"),
            vec![0u8; 1024],
        )
        .unwrap();

        let search = find_artifacts(&root);
        let node_hits = search
            .hits
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

        let search = find_artifacts(&root);
        assert!(
            search.hits.is_empty(),
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

    /// Parent + child both present: the parent is marked rollup, while the
    /// path union keeps parent-only residual bytes and counts the child once.
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
        fs::create_dir_all(root.join("Library/Caches/OtherApp")).unwrap();
        fs::write(
            root.join("Library/Caches/OtherApp/cache.bin"),
            vec![0u8; 4096],
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

        assert_eq!(
            report.total.logical, user_caches.size.logical,
            "the union should equal the parent subtree without adding the child twice"
        );
        assert!(
            report.total.logical > homebrew.size.logical,
            "parent-only residual bytes must remain in the total"
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
