//! Ecosystem detection and standard check commands.
//!
//! Detects the workspace's package ecosystem from manifest files (not from
//! untrusted text) and proposes the canonical read-only-ish check commands:
//! build, test, lint, format-check, and dependency manifests. Every proposed
//! command is still passed through [`classify_command`] by the runner — an
//! adapter never bypasses the safety gate.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::core::command_safety::{classify_command, CommandClass};

/// Detected package ecosystem for a workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ecosystem {
    Cargo,
    Npm,
    Pnpm,
    Yarn,
    Bun,
    Python,
    Go,
    Maven,
    Gradle,
    /// No recognized manifest.
    Unknown,
}

impl Ecosystem {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
            Self::Yarn => "yarn",
            Self::Bun => "bun",
            Self::Python => "python",
            Self::Go => "go",
            Self::Maven => "maven",
            Self::Gradle => "gradle",
            Self::Unknown => "unknown",
        }
    }
}

/// A standard check the ecosystem knows how to express in argv form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterCheck {
    /// Stable check name recorded in the validation graph, e.g. `cargo test`.
    pub name: String,
    pub argv: Vec<String>,
    /// `required` checks must pass for admission; `optional` failures are
    /// recorded but non-blocking.
    pub required: bool,
}

/// Manifest filenames mapped to ecosystems, in detection precedence order.
/// Lockfiles decide between JS package managers. `has` answers whether a
/// top-level manifest name is present — a directory probe on disk for
/// [`detect`], or a path-set membership test for [`detect_paths`].
fn detect_ecosystem_with(has: impl Fn(&str) -> bool) -> Vec<Ecosystem> {
    let mut found = Vec::new();

    if has("Cargo.toml") {
        found.push(Ecosystem::Cargo);
    }
    if has("package.json") {
        // Lockfile precedence determines the package manager.
        if has("pnpm-lock.yaml") {
            found.push(Ecosystem::Pnpm);
        } else if has("yarn.lock") {
            found.push(Ecosystem::Yarn);
        } else if has("bun.lockb") || has("bun.lock") {
            found.push(Ecosystem::Bun);
        } else {
            found.push(Ecosystem::Npm);
        }
    }
    if has("pyproject.toml") || has("setup.py") || has("requirements.txt") {
        found.push(Ecosystem::Python);
    }
    if has("go.mod") {
        found.push(Ecosystem::Go);
    }
    if has("pom.xml") {
        found.push(Ecosystem::Maven);
    }
    if has("build.gradle") || has("build.gradle.kts") || has("settings.gradle") {
        found.push(Ecosystem::Gradle);
    }
    if found.is_empty() {
        found.push(Ecosystem::Unknown);
    }
    found
}

/// Detect the workspace ecosystems. Monorepos can legitimately contain more
/// than one (e.g., Cargo + npm); checks are emitted per ecosystem.
pub fn detect(root: &Path) -> Vec<Ecosystem> {
    detect_ecosystem_with(|name| root.join(name).is_file())
}

/// Detect ecosystems from a repo-relative path list (e.g. a bounded
/// workspace listing) instead of touching the filesystem.
pub fn detect_paths(paths: &[String]) -> Vec<Ecosystem> {
    detect_ecosystem_with(|name| paths.iter().any(|p| p == name))
}

/// Standard build/test/lint/fmt check argvs for an ecosystem. Only commands
/// that classify as `Safe` (or `RequiresApproval` for install steps) are
/// returned — the adapter never proposes a forbidden command.
pub fn standard_checks(ecosystem: Ecosystem) -> Vec<AdapterCheck> {
    let raw: Vec<(&str, Vec<&str>, bool)> = match ecosystem {
        Ecosystem::Cargo => vec![
            ("cargo build", vec!["cargo", "build"], true),
            ("cargo test", vec!["cargo", "test"], true),
            (
                "cargo clippy",
                vec!["cargo", "clippy", "--", "-D", "warnings"],
                false,
            ),
            (
                "cargo fmt --check",
                vec!["cargo", "fmt", "--all", "--", "--check"],
                false,
            ),
        ],
        Ecosystem::Npm => vec![
            ("npm test", vec!["npm", "test"], true),
            ("npm run build", vec!["npm", "run", "build"], false),
            ("npm run lint", vec!["npm", "run", "lint"], false),
        ],
        Ecosystem::Pnpm => vec![
            ("pnpm test", vec!["pnpm", "test"], true),
            ("pnpm run build", vec!["pnpm", "run", "build"], false),
            ("pnpm run lint", vec!["pnpm", "run", "lint"], false),
        ],
        Ecosystem::Yarn => vec![
            ("yarn test", vec!["yarn", "test"], true),
            ("yarn build", vec!["yarn", "build"], false),
            ("yarn lint", vec!["yarn", "lint"], false),
        ],
        Ecosystem::Bun => vec![
            ("bun test", vec!["bun", "test"], true),
            ("bun run build", vec!["bun", "run", "build"], false),
        ],
        Ecosystem::Python => vec![
            ("python -m pytest", vec!["python", "-m", "pytest"], true),
            (
                "python -m compileall",
                vec!["python", "-m", "compileall", "."],
                false,
            ),
        ],
        Ecosystem::Go => vec![
            ("go build ./...", vec!["go", "build", "./..."], true),
            ("go test ./...", vec!["go", "test", "./..."], true),
            ("go vet ./...", vec!["go", "vet", "./..."], false),
            ("gofmt -l .", vec!["gofmt", "-l", "."], false),
        ],
        Ecosystem::Maven => vec![
            ("mvn -q compile", vec!["mvn", "-q", "compile"], true),
            ("mvn -q test", vec!["mvn", "-q", "test"], true),
        ],
        Ecosystem::Gradle => vec![
            ("gradle build", vec!["gradle", "build"], true),
            ("gradle test", vec!["gradle", "test"], true),
        ],
        Ecosystem::Unknown => Vec::new(),
    };

    raw.into_iter()
        .map(|(name, parts, required)| AdapterCheck {
            name: name.to_string(),
            argv: parts.into_iter().map(String::from).collect(),
            required,
        })
        // Never propose a command the safety gate would forbid — keeps the
        // adapter output honest by construction.
        .filter(|check| classify_command(&check.argv).class != CommandClass::Forbidden)
        .collect()
}

/// Detected ecosystems plus their proposed checks for a workspace.
pub fn plan_checks(root: &Path) -> Vec<(Ecosystem, Vec<AdapterCheck>)> {
    detect(root)
        .into_iter()
        .map(|eco| (eco, standard_checks(eco)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with(files: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for f in files {
            std::fs::write(dir.path().join(f), b"x").unwrap();
        }
        dir
    }

    #[test]
    fn detects_cargo() {
        let dir = dir_with(&["Cargo.toml"]);
        assert_eq!(detect(dir.path()), vec![Ecosystem::Cargo]);
    }

    #[test]
    fn lockfile_selects_js_package_manager() {
        assert_eq!(
            detect(dir_with(&["package.json", "pnpm-lock.yaml"]).path()),
            vec![Ecosystem::Pnpm]
        );
        assert_eq!(
            detect(dir_with(&["package.json", "yarn.lock"]).path()),
            vec![Ecosystem::Yarn]
        );
        assert_eq!(
            detect(dir_with(&["package.json", "bun.lockb"]).path()),
            vec![Ecosystem::Bun]
        );
        assert_eq!(
            detect(dir_with(&["package.json"]).path()),
            vec![Ecosystem::Npm]
        );
    }

    #[test]
    fn monorepo_detects_multiple_ecosystems() {
        let dir = dir_with(&["Cargo.toml", "package.json", "go.mod"]);
        let found = detect(dir.path());
        assert!(found.contains(&Ecosystem::Cargo));
        assert!(found.contains(&Ecosystem::Npm));
        assert!(found.contains(&Ecosystem::Go));
    }

    #[test]
    fn unknown_when_no_manifest() {
        let dir = dir_with(&[]);
        assert_eq!(detect(dir.path()), vec![Ecosystem::Unknown]);
        assert!(plan_checks(dir.path())
            .iter()
            .all(|(_, checks)| checks.is_empty()));
    }

    #[test]
    fn no_forbidden_command_is_ever_proposed() {
        for eco in [
            Ecosystem::Cargo,
            Ecosystem::Npm,
            Ecosystem::Pnpm,
            Ecosystem::Yarn,
            Ecosystem::Bun,
            Ecosystem::Python,
            Ecosystem::Go,
            Ecosystem::Maven,
            Ecosystem::Gradle,
        ] {
            for check in standard_checks(eco) {
                assert_ne!(
                    classify_command(&check.argv).class,
                    CommandClass::Forbidden,
                    "{} proposed forbidden argv {:?}",
                    check.name,
                    check.argv
                );
            }
        }
    }

    #[test]
    fn go_and_python_checks_cover_build_and_test() {
        let go: Vec<String> = standard_checks(Ecosystem::Go)
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert!(go.iter().any(|n| n == "go build ./..."));
        assert!(go.iter().any(|n| n == "go test ./..."));
        let py: Vec<String> = standard_checks(Ecosystem::Python)
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert!(py.iter().any(|n| n == "python -m pytest"));
    }
}
