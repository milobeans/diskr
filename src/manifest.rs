use std::collections::HashSet;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DependencyCount {
    pub known: usize,
    pub complete: bool,
    pub warnings: Vec<String>,
}

impl From<usize> for DependencyCount {
    fn from(known: usize) -> Self {
        Self {
            known,
            complete: true,
            warnings: Vec::new(),
        }
    }
}

impl fmt::Display for DependencyCount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.complete {
            write!(f, "{}", self.known)
        } else if self.known > 0 {
            write!(f, "≥{}", self.known)
        } else {
            f.write_str("unknown")
        }
    }
}

#[derive(Default)]
struct Declarations {
    names: HashSet<String>,
    warnings: Vec<String>,
}

impl Declarations {
    fn warn(&mut self, warning: impl Into<String>) {
        self.warnings.push(warning.into());
    }

    fn python_name(&mut self, requirement: &str) {
        let requirement = requirement.trim();
        let end = requirement
            .find(|ch: char| !ch.is_ascii_alphanumeric() && !matches!(ch, '-' | '_' | '.'))
            .unwrap_or(requirement.len());
        let name = &requirement[..end];
        let tail = requirement[end..].trim_start();
        if name.is_empty()
            || !name.starts_with(|ch: char| ch.is_ascii_alphanumeric())
            || (!tail.is_empty() && !tail.starts_with(['[', '<', '>', '=', '!', '~', '@', ';']))
        {
            self.warn("unsupported Python requirement expression");
            return;
        }
        let mut normalized = String::new();
        for ch in name.chars() {
            if matches!(ch, '-' | '_' | '.') {
                if !normalized.ends_with('-') {
                    normalized.push('-');
                }
            } else {
                normalized.push(ch.to_ascii_lowercase());
            }
        }
        self.names.insert(normalized);
    }
}

pub(crate) fn bounded_text(path: &Path) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let mut text = String::new();
    file.take(MAX_MANIFEST_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|error| error.to_string())?;
    if text.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(String::from("file exceeds 1 MiB parser limit"));
    }
    Ok(text)
}

pub fn count(dir: &Path, manifests: &[&str]) -> DependencyCount {
    let mut declarations = Declarations::default();
    for manifest in manifests {
        let path = dir.join(manifest);
        if *manifest == "requirements.txt" {
            requirements_file(&path, &mut HashSet::new(), &mut 0, 0, &mut declarations);
            continue;
        }
        let content = match bounded_text(&path) {
            Ok(content) => content,
            Err(error) => {
                declarations.warn(format!("{manifest}: {error}"));
                continue;
            }
        };
        let before = declarations.warnings.len();
        match *manifest {
            "package.json" => json_dependencies(
                &content,
                &[
                    "dependencies",
                    "devDependencies",
                    "optionalDependencies",
                    "peerDependencies",
                ],
                &mut declarations,
            ),
            "composer.json" => {
                json_dependencies(&content, &["require", "require-dev"], &mut declarations)
            }
            "Cargo.toml" => cargo_dependencies(&content, &mut declarations),
            "pyproject.toml" => python_dependencies(&content, &mut declarations),
            "go.mod" => go_dependencies(&content, &mut declarations),
            "Gemfile" => declarations
                .warn("Gemfile is executable Ruby; exact declaration counting is unsupported"),
            _ => declarations.warn("unsupported manifest format"),
        }
        for warning in &mut declarations.warnings[before..] {
            *warning = format!("{manifest}: {warning}");
        }
    }
    declarations.warnings.sort();
    declarations.warnings.dedup();
    DependencyCount {
        known: declarations.names.len(),
        complete: declarations.warnings.is_empty(),
        warnings: declarations.warnings,
    }
}

fn json_dependencies(content: &str, keys: &[&str], declarations: &mut Declarations) {
    let Ok(serde_json::Value::Object(root)) = serde_json::from_str(content) else {
        declarations.warn("malformed JSON manifest");
        return;
    };
    for key in keys {
        if let Some(value) = root.get(*key) {
            if let Some(table) = value.as_object() {
                for (name, constraint) in table {
                    if constraint.is_string() {
                        declarations.names.insert(name.clone());
                    } else {
                        declarations.warn(format!("invalid dependency constraint for {name}"));
                    }
                }
            } else {
                declarations.warn(format!("{key} must be an object"));
            }
        }
    }
}

fn cargo_tables(table: &toml::Table, declarations: &mut Declarations) {
    for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(value) = table.get(key) {
            if let Some(dependencies) = value.as_table() {
                for (name, value) in dependencies {
                    if value.is_str() || value.is_table() {
                        declarations.names.insert(name.clone());
                    } else {
                        declarations.warn(format!("invalid dependency declaration for {name}"));
                    }
                }
            } else {
                declarations.warn(format!("{key} must be a table"));
            }
        }
    }
}

fn cargo_dependencies(content: &str, declarations: &mut Declarations) {
    let Ok(root) = content.parse::<toml::Table>() else {
        declarations.warn("malformed TOML manifest");
        return;
    };
    if !root.contains_key("package") && !root.contains_key("workspace") {
        declarations.warn("missing package or workspace declaration");
    }
    cargo_tables(&root, declarations);
    if let Some(targets) = root.get("target") {
        if let Some(targets) = targets.as_table() {
            for target in targets.values() {
                if let Some(target) = target.as_table() {
                    cargo_tables(target, declarations);
                } else {
                    declarations.warn("invalid Cargo target table");
                }
            }
        } else {
            declarations.warn("target must be a table");
        }
    }
}

fn python_array(value: &toml::Value, declarations: &mut Declarations) {
    if let Some(requirements) = value.as_array() {
        for requirement in requirements {
            if let Some(requirement) = requirement.as_str() {
                declarations.python_name(requirement);
            } else {
                declarations.warn("dependency array contains a non-string expression");
            }
        }
    } else {
        declarations.warn("dependencies must be an array");
    }
}

fn poetry_table(value: &toml::Value, declarations: &mut Declarations) {
    if let Some(table) = value.as_table() {
        for name in table.keys().filter(|name| name.as_str() != "python") {
            declarations.python_name(name);
        }
    } else {
        declarations.warn("Poetry dependencies must be a table");
    }
}

fn dependency_group(
    name: &str,
    groups: &toml::Table,
    visiting: &mut HashSet<String>,
    expanded: &mut HashSet<String>,
    declarations: &mut Declarations,
) {
    if expanded.contains(name) {
        return;
    }
    if visiting.len() > 32 || !visiting.insert(name.to_string()) {
        declarations.warn("cyclic or excessively nested dependency groups");
        return;
    }
    if let Some(entries) = groups.get(name).and_then(toml::Value::as_array) {
        for entry in entries {
            if let Some(requirement) = entry.as_str() {
                declarations.python_name(requirement);
            } else if let Some(include) = entry.get("include-group").and_then(toml::Value::as_str) {
                dependency_group(include, groups, visiting, expanded, declarations);
            } else {
                declarations.warn("unsupported dependency-group entry");
            }
        }
    } else {
        declarations.warn(format!("missing or invalid dependency group {name}"));
    }
    visiting.remove(name);
    expanded.insert(name.to_string());
}

fn python_dependencies(content: &str, declarations: &mut Declarations) {
    let Ok(root) = content.parse::<toml::Table>() else {
        declarations.warn("malformed TOML manifest");
        return;
    };
    if let Some(project) = root.get("project").and_then(toml::Value::as_table) {
        if let Some(dependencies) = project.get("dependencies") {
            python_array(dependencies, declarations);
        }
        if let Some(optional) = project.get("optional-dependencies") {
            if let Some(optional) = optional.as_table() {
                for group in optional.values() {
                    python_array(group, declarations);
                }
            } else {
                declarations.warn("optional-dependencies must be a table");
            }
        }
        if project
            .get("dynamic")
            .and_then(toml::Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    matches!(
                        item.as_str(),
                        Some("dependencies" | "optional-dependencies")
                    )
                })
            })
        {
            declarations.warn("dynamic Python dependencies cannot be counted statically");
        }
    } else if root.contains_key("project") {
        declarations.warn("project must be a table");
    }
    if let Some(groups) = root.get("dependency-groups") {
        if let Some(groups) = groups.as_table() {
            let mut expanded = HashSet::new();
            for name in groups.keys() {
                dependency_group(
                    name,
                    groups,
                    &mut HashSet::new(),
                    &mut expanded,
                    declarations,
                );
            }
        } else {
            declarations.warn("dependency-groups must be a table");
        }
    }
    if let Some(poetry) = root.get("tool").and_then(|tool| tool.get("poetry")) {
        if !poetry.is_table() {
            declarations.warn("Poetry configuration must be a table");
        }
        for key in ["dependencies", "dev-dependencies"] {
            if let Some(value) = poetry.get(key) {
                poetry_table(value, declarations);
            }
        }
        if let Some(groups) = poetry.get("group").and_then(toml::Value::as_table) {
            for group in groups.values() {
                if let Some(value) = group.get("dependencies") {
                    poetry_table(value, declarations);
                } else if !group.is_table() {
                    declarations.warn("Poetry group must be a table");
                }
            }
        } else if poetry.get("group").is_some() {
            declarations.warn("Poetry groups must be a table");
        }
    }
    if let Some(requires) = root
        .get("build-system")
        .and_then(|build| build.get("requires"))
    {
        python_array(requires, declarations);
    }
}

fn requirements_file(
    path: &Path,
    visiting: &mut HashSet<PathBuf>,
    files: &mut usize,
    depth: usize,
    declarations: &mut Declarations,
) {
    if depth > 8 || *files >= 128 {
        declarations.warn("requirements include limit reached");
        return;
    }
    let identity = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !visiting.insert(identity.clone()) {
        declarations.warn("cyclic requirements include");
        return;
    }
    *files += 1;
    let content = match bounded_text(path) {
        Ok(content) => content,
        Err(error) => {
            declarations.warn(format!("requirements file unavailable: {error}"));
            visiting.remove(&identity);
            return;
        }
    };
    let mut logical = String::new();
    for raw in content.lines() {
        let raw = raw.trim();
        if raw.starts_with('#') || raw.is_empty() {
            continue;
        }
        let continued = raw.ends_with('\\');
        logical.push_str(raw.trim_end_matches('\\'));
        if continued {
            logical.push(' ');
            continue;
        }
        let line = logical
            .split_once(" #")
            .map(|(line, _)| line)
            .unwrap_or(&logical)
            .trim();
        let include = line
            .strip_prefix("--requirement=")
            .or_else(|| line.strip_prefix("--requirement "))
            .or_else(|| line.strip_prefix("-r"));
        if let Some(include) = include {
            let include = include.trim().trim_matches(['\'', '"']);
            if include.is_empty() || include.contains("://") || include.contains('$') {
                declarations.warn("unsupported requirements include path");
            } else {
                requirements_file(
                    &path.parent().unwrap_or(Path::new(".")).join(include),
                    visiting,
                    files,
                    depth + 1,
                    declarations,
                );
            }
        } else if line.starts_with("-c")
            || line.starts_with("--constraint")
            || line.starts_with("--hash=")
            || [
                "--index-url",
                "--extra-index-url",
                "--trusted-host",
                "--find-links",
                "--no-index",
                "--pre",
                "--require-hashes",
                "--only-binary",
                "--no-binary",
                "--prefer-binary",
            ]
            .iter()
            .any(|option| line.starts_with(option))
        {
        } else if line.starts_with("-e") || line.starts_with("--editable") {
            if let Some((_, name)) = line.split_once("#egg=") {
                declarations.python_name(name);
            } else {
                declarations.warn("editable requirement has no declared package name");
            }
        } else if line.starts_with('-') {
            declarations.warn("unsupported requirements option");
        } else {
            declarations.python_name(line.split(" --hash=").next().unwrap_or(line));
        }
        logical.clear();
    }
    if !logical.is_empty() {
        declarations.warn("unterminated requirements continuation");
    }
    visiting.remove(&identity);
}

fn go_dependencies(content: &str, declarations: &mut Declarations) {
    let mut in_require = false;
    let mut module = false;
    for raw in content.lines() {
        let line = raw
            .split_once("//")
            .map(|(line, _)| line)
            .unwrap_or(raw)
            .trim();
        if line.starts_with("module ") {
            module = true;
        }
        if line.is_empty() {
            continue;
        }
        if in_require && line == ")" {
            in_require = false;
            continue;
        }
        let requirement = if in_require {
            Some(line)
        } else {
            line.strip_prefix("require ").map(str::trim)
        };
        if let Some(requirement) = requirement {
            if requirement == "(" {
                in_require = true;
                continue;
            }
            if raw
                .split_once("//")
                .is_some_and(|(_, comment)| comment.trim() == "indirect")
            {
                continue;
            }
            let mut parts = requirement.split_whitespace();
            match (parts.next(), parts.next(), parts.next()) {
                (Some(name), Some(version), None) if version.starts_with('v') => {
                    declarations
                        .names
                        .insert(name.trim_matches('"').to_string());
                }
                _ => declarations.warn("unsupported or malformed Go requirement"),
            }
        }
    }
    if !module || in_require {
        declarations.warn("missing module declaration or unterminated Go require block");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let id = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "diskr_manifest_{}_{id}_{sequence}",
                std::process::id()
            ));
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }

        fn write(&self, name: &str, content: &str) {
            std::fs::write(self.0.join(name), content).unwrap();
        }

        fn count(&self, names: &[&str]) -> DependencyCount {
            count(&self.0, names)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn json_counts_unique_declared_names_and_validates_shapes() {
        let fixture = Fixture::new();
        fixture.write("package.json", r#"{"dependencies":{"a":"1"},"devDependencies":{"a":"2","b":"3"},"peerDependencies":{"c":"*"},"optionalDependencies":{"b":"*"}}"#);
        assert_eq!(fixture.count(&["package.json"]), 3usize.into());
        fixture.write(
            "package.json",
            r#"{"dependencies":{"a":123},"devDependencies":[]}"#,
        );
        assert!(!fixture.count(&["package.json"]).complete);
        fixture.write("package.json", "{}");
        assert_eq!(fixture.count(&["package.json"]), 0usize.into());
    }

    #[test]
    fn cargo_includes_table_target_build_and_shared_direct_declarations() {
        let fixture = Fixture::new();
        fixture.write(
            "Cargo.toml",
            r#"
[package]
name = "fixture"
[dependencies.serde]
version = "1"
[dependencies.shared]
workspace = true
[dev-dependencies]
serde = "1"
[build-dependencies]
cc = "1"
[target.'cfg(unix)'.dependencies]
libc = "0.2"
[workspace.dependencies]
unused = "1"
shared = "1"
"#,
        );
        assert_eq!(fixture.count(&["Cargo.toml"]), 4usize.into());
    }

    #[test]
    fn python_unions_modern_groups_poetry_and_build_dependencies() {
        let fixture = Fixture::new();
        fixture.write(
            "pyproject.toml",
            r#"
[project]
dependencies = ["Requests>=2", "Click[extra]; python_version > '3.8'"]
[project.optional-dependencies]
test = ["pytest>=8"]
[dependency-groups]
lint = ["ruff", {include-group="test"}]
test = ["pytest"]
[tool.poetry.dependencies]
python = "^3.10"
numpy = "2"
requests = {version="2", optional=true}
[tool.poetry.group.docs.dependencies]
sphinx = "8"
[build-system]
requires = ["setuptools>=61"]
"#,
        );
        fixture.write("requirements.txt", "requests\nCLICK>=8\n");
        assert_eq!(
            fixture.count(&["pyproject.toml", "requirements.txt"]),
            7usize.into()
        );
    }

    #[test]
    fn requirements_follow_local_includes_without_counting_constraints_or_hashes() {
        let fixture = Fixture::new();
        fixture.write(
            "requirements.txt",
            concat!(
                "-r extra.txt\nFoo__bar>=1 \\",
                "\n --hash=sha256:abcd\n-c constraints.txt\n"
            ),
        );
        fixture.write("extra.txt", "foo.bar==1\nrequests>=2 # note\n");
        assert_eq!(fixture.count(&["requirements.txt"]), 2usize.into());
        fixture.write(
            "extra.txt",
            "requests\n-r requirements.txt\n-r missing.txt\n-r https://example.test/deps\n",
        );
        let result = fixture.count(&["requirements.txt"]);
        assert_eq!(result.known, 2);
        assert!(!result.complete);
        assert_eq!(result.to_string(), "≥2");
        assert!(result
            .warnings
            .iter()
            .any(|warning| warning.contains("cyclic")));
    }

    #[test]
    fn malformed_missing_oversized_and_executable_manifests_are_unknown() {
        let fixture = Fixture::new();
        fixture.write("Cargo.toml", "[");
        fixture.write("package.json", "not json");
        fixture.write("Gemfile", "gem ENV.fetch('DYNAMIC_GEM')");
        fixture.write(
            "pyproject.toml",
            &" ".repeat(MAX_MANIFEST_BYTES as usize + 1),
        );
        for manifest in [
            "Cargo.toml",
            "package.json",
            "Gemfile",
            "pyproject.toml",
            "requirements.txt",
        ] {
            let result = fixture.count(&[manifest]);
            assert!(!result.complete, "{manifest}");
            assert_eq!(result.to_string(), "unknown");
            assert!(!result.warnings.is_empty());
        }
    }

    #[test]
    fn dynamic_and_cyclic_python_groups_preserve_known_lower_bounds() {
        let fixture = Fixture::new();
        fixture.write("pyproject.toml", "[project]\ndependencies=['requests']\ndynamic=['optional-dependencies']\n[dependency-groups]\na=['pytest', {include-group='b'}]\nb=[{include-group='a'}]\n");
        let result = fixture.count(&["pyproject.toml"]);
        assert_eq!(result.known, 2);
        assert!(!result.complete);
        fixture.write("pyproject.toml", "project = 42");
        assert!(!fixture.count(&["pyproject.toml"]).complete);
    }

    #[test]
    fn go_counts_unique_direct_requirements_and_rejects_unterminated_blocks() {
        let fixture = Fixture::new();
        fixture.write("go.mod", "module example.test/app\nrequire (\n example.test/a v1.0.0\n example.test/b v1.0.0 // indirect\n)\nrequire example.test/a v1.0.0\n");
        assert_eq!(fixture.count(&["go.mod"]), 1usize.into());
        fixture.write(
            "go.mod",
            "module example.test/app\nrequire (\n example.test/a v1.0.0\n",
        );
        let result = fixture.count(&["go.mod"]);
        assert_eq!(result.known, 1);
        assert!(!result.complete);
    }
}
