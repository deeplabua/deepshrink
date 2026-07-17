//! Discovery of the external `ffmpeg`/`ffprobe` binaries.
//!
//! Search order (see `docs/PRD/03-tech-stack.md`):
//! 1. Explicit path from an env var (`DEEPSHRINK_FFMPEG` / `DEEPSHRINK_FFPROBE`).
//! 2. A binary in `PATH`.
//! 3. (later) a bundled binary next to the app — for the desktop layer.
//!
//! If not found, [`FfmpegError::NotFound`]; the CLI maps this to exit code 3
//! with an install hint. ffprobe/progress parsing arrives in sessions 002/003.
#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::path::PathBuf;

use thiserror::Error;

pub mod probe;
pub mod progress;
pub mod run;

pub use probe::{probe, Ffprobe};
pub use run::run_pass;

/// Errors from the ffmpeg layer.
#[derive(Debug, Error)]
pub enum FfmpegError {
    #[error(
        "`{tool}` not found in PATH. Install ffmpeg (macOS: `brew install ffmpeg`) \
         or point {env} at the binary."
    )]
    NotFound {
        tool: &'static str,
        env: &'static str,
    },
    #[error("failed to spawn `{tool}`: {source}")]
    Spawn {
        tool: &'static str,
        source: std::io::Error,
    },
    #[error("`{tool}` exited with {status}:\n{stderr}")]
    CommandFailed {
        tool: &'static str,
        status: String,
        stderr: String,
    },
    #[error("failed to parse ffprobe output: {0}")]
    Parse(String),
}

/// The located binaries.
#[derive(Debug, Clone)]
pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

/// Locate `ffmpeg` and `ffprobe`. Returns the first missing tool as an error.
pub fn locate() -> Result<Tools, FfmpegError> {
    Ok(Tools {
        ffmpeg: locate_one("ffmpeg", "DEEPSHRINK_FFMPEG")?,
        ffprobe: locate_one("ffprobe", "DEEPSHRINK_FFPROBE")?,
    })
}

/// Locate a single tool: first via its env var, then in `PATH`.
pub fn locate_one(tool: &'static str, env: &'static str) -> Result<PathBuf, FfmpegError> {
    if let Some(explicit) = std::env::var_os(env) {
        if !explicit.is_empty() {
            return Ok(PathBuf::from(explicit));
        }
    }
    which_in_path(tool).ok_or(FfmpegError::NotFound { tool, env })
}

/// A simple executable lookup in `PATH` (no external dependencies).
fn which_in_path(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let names = candidate_names(tool);
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for name in &names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(windows)]
fn candidate_names(tool: &str) -> Vec<OsString> {
    // On Windows, account for the common executable extensions.
    vec![
        OsString::from(format!("{tool}.exe")),
        OsString::from(format!("{tool}.bat")),
        OsString::from(format!("{tool}.cmd")),
        OsString::from(tool),
    ]
}

#[cfg(not(windows))]
fn candidate_names(tool: &str) -> Vec<OsString> {
    vec![OsString::from(tool)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_binary_is_none() {
        assert!(which_in_path("deepshrink-definitely-not-a-real-binary-xyz").is_none());
    }

    #[test]
    fn candidate_names_include_bare_tool() {
        let names = candidate_names("ffmpeg");
        assert!(names.contains(&OsString::from("ffmpeg")));
    }
}
