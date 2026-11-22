//! The Cargo project a command works on, found the way cargo finds it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::code::Code;
use crate::report::Failure;

/// A Cargo project: the package or workspace that contains the current
/// directory.
#[derive(Debug, Clone)]
pub struct Project {
    /// The directory of the outermost Cargo.toml (the workspace root, or the package).
    pub root: PathBuf,
    /// Where cargo writes builds.
    pub target_dir: PathBuf,
    /// Where cargo keeps intermediate build files (`build.build-dir`): the
    /// target directory unless that is configured.
    pub build_dir: PathBuf,
    /// `cargo metadata` (without dependencies resolved twice).
    pub metadata: Value,
}

/// The nearest directory at or above `start` that holds a Cargo.toml.
pub fn find_manifest_dir(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|d| d.join("Cargo.toml").is_file())
        .map(Path::to_path_buf)
}

impl Project {
    /// The project around the current directory, read with `cargo metadata`.
    pub fn here() -> Result<Project, Failure> {
        let cwd = std::env::current_dir().map_err(|e| {
            Failure::new(
                Code::NOT_A_PROJECT,
                format!("the current directory cannot be read: {e}"),
                "run giong from inside a project",
            )
        })?;
        Project::at(&cwd)
    }

    pub fn at(dir: &Path) -> Result<Project, Failure> {
        if find_manifest_dir(dir).is_none() {
            return Err(Failure::new(
                Code::NOT_A_PROJECT,
                format!("no Cargo.toml in {} or above it", dir.display()),
                "run giong inside a project, or start one with giong new <name>",
            ));
        }
        let out = cargo()
            .args(["metadata", "--format-version", "1", "--no-deps"])
            .current_dir(dir)
            .output()
            .map_err(cargo_missing)?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(Failure::new(
                Code::CARGO_FAILED,
                format!("cargo metadata failed: {}", first_error(&err)),
                "fix Cargo.toml (cargo metadata says what is wrong)",
            ));
        }
        let metadata: Value = serde_json::from_slice(&out.stdout).map_err(|e| {
            Failure::new(
                Code::CARGO_FAILED,
                format!("cargo metadata printed something that is not JSON: {e}"),
                "update cargo: rustup update stable",
            )
        })?;
        let root = PathBuf::from(metadata["workspace_root"].as_str().unwrap_or("."));
        let target_dir = PathBuf::from(
            metadata["target_directory"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| root.join("target").display().to_string()),
        );
        let build_dir = metadata["build_directory"]
            .as_str()
            .map(PathBuf::from)
            .unwrap_or_else(|| target_dir.clone());
        Ok(Project {
            root,
            target_dir,
            build_dir,
            metadata,
        })
    }

    /// The packages of the workspace (not the dependencies).
    pub fn packages(&self) -> impl Iterator<Item = &Value> {
        self.metadata["packages"].as_array().into_iter().flatten()
    }

    /// The text of the root Cargo.toml.
    pub fn manifest(&self) -> String {
        fs::read_to_string(self.root.join("Cargo.toml")).unwrap_or_default()
    }
}

/// `cargo`, as the environment names it (`CARGO` when run through cargo).
pub fn cargo() -> Command {
    Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
}

pub fn cargo_missing(e: std::io::Error) -> Failure {
    Failure::new(
        Code::CARGO_FAILED,
        format!("cargo could not be started: {e}"),
        "install Rust with rustup (https://rustup.rs) and open a new terminal",
    )
}

/// The first `error:` line of cargo's output, or its first line.
pub fn first_error(stderr: &str) -> String {
    // CARGO_TERM_COLOR=always (as CI sets) colours the output even when it
    // is captured: `\x1b[1m\x1b[91merror`.
    let stderr = crate::build::strip_ansi(stderr);
    stderr
        .lines()
        .find(|l| l.trim_start().starts_with("error"))
        .or_else(|| stderr.lines().find(|l| !l.trim().is_empty()))
        .unwrap_or("no output")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_dir_is_found_above() {
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert_eq!(find_manifest_dir(&here.join("src")).as_deref(), Some(here));
    }

    #[test]
    fn first_error_line() {
        assert_eq!(
            first_error("warning: x\nerror: bad thing\n  more"),
            "error: bad thing"
        );
        assert_eq!(first_error("\nsomething\n"), "something");
        assert_eq!(first_error(""), "no output");
        assert_eq!(
            first_error(
                "\x1b[1m\x1b[92m   Compiling\x1b[0m x v0.1.0\n\x1b[1m\x1b[91merror\x1b[0m\x1b[1m: bad thing\x1b[0m\n"
            ),
            "error: bad thing"
        );
    }
}
