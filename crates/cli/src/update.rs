//! Best-effort "a new version is available" nudge for Homebrew installs.
//!
//! Design constraints (see the README's privacy note):
//! - **No network from our binary.** We ask the local `brew` what it already
//!   knows (`HOMEBREW_NO_AUTO_UPDATE=1 brew outdated`), which reads Homebrew's
//!   on-disk index and never touches the network — so no hang offline.
//! - **No new dependencies.** Just the standard library + `serde_json`.
//! - **Never blocks the work.** The refresh runs as a detached child; the notice
//!   is always printed from the last cached result (npm `update-notifier` model).
//! - **Homebrew installs only**, throttled to once per day, opt-out via
//!   `DEEPSHRINK_NO_UPDATE_CHECK=1`, silent on any error.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

/// The tapped formula name used for the check and the upgrade command.
const TAP_FORMULA: &str = "deeplabua/tap/deepshrink";

/// Re-check at most this often.
const THROTTLE: Duration = Duration::from_secs(24 * 60 * 60);

/// If a newer Homebrew release is known, return a one-line upgrade hint; also
/// trigger a detached background refresh when the cached result is stale.
///
/// Returns `None` (silently) whenever the nudge does not apply: quiet/JSON
/// output, a non-terminal stderr, CI, an explicit opt-out, a non-Homebrew
/// install, or any missing/malformed cache.
pub fn upgrade_hint(quiet: bool, json: bool) -> Option<String> {
    if quiet
        || json
        || std::env::var_os("DEEPSHRINK_NO_UPDATE_CHECK").is_some()
        || std::env::var_os("CI").is_some()
        || !std::io::stderr().is_terminal()
        || !is_homebrew_install()
    {
        return None;
    }

    let cache = cache_path()?;
    if is_stale(&cache) {
        spawn_refresh(&cache);
    }
    parse_hint(&std::fs::read_to_string(&cache).ok()?)
}

/// Whether the running binary lives inside a Homebrew Cellar.
fn is_homebrew_install() -> bool {
    std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map(|p| is_cellar_path(&p.to_string_lossy()))
        .unwrap_or(false)
}

/// Pure Cellar-path test (unit-tested).
fn is_cellar_path(path: &str) -> bool {
    path.contains("/Cellar/deepshrink/")
}

/// `$XDG_CACHE_HOME/deepshrink/brew-outdated.json`, else `~/.cache/…`.
fn cache_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => PathBuf::from(std::env::var_os("HOME").filter(|v| !v.is_empty())?).join(".cache"),
    };
    Some(base.join("deepshrink").join("brew-outdated.json"))
}

/// Whether the cache is missing or older than [`THROTTLE`].
fn is_stale(cache: &Path) -> bool {
    match std::fs::metadata(cache).and_then(|m| m.modified()) {
        Ok(mtime) => SystemTime::now()
            .duration_since(mtime)
            .map(|age| age >= THROTTLE)
            .unwrap_or(true),
        Err(_) => true,
    }
}

/// Fire off a detached `brew outdated` that writes the cache atomically, then
/// return immediately. Dropping the child does not kill it: once we exit it is
/// reparented and finishes on its own. Any failure just leaves the cache as-is.
fn spawn_refresh(cache: &Path) {
    let Some(dir) = cache.parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    // A per-process temp keeps concurrent runs from clobbering each other's write.
    let tmp = dir.join(format!("brew-outdated.{}.tmp", std::process::id()));
    // `HOMEBREW_NO_AUTO_UPDATE=1` keeps this local-only (no network, no hang).
    let script = format!(
        "HOMEBREW_NO_AUTO_UPDATE=1 brew outdated --json=v2 {formula} > {tmp} 2>/dev/null \
         && mv -f {tmp} {cache} || rm -f {tmp}",
        formula = TAP_FORMULA,
        tmp = shell_single_quote(&tmp),
        cache = shell_single_quote(cache),
    );
    let _ = Command::new("sh")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Single-quote a path for safe interpolation into an `sh -c` script.
fn shell_single_quote(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', r"'\''"))
}

/// Parse `brew outdated --json=v2` output into an upgrade hint, if our formula
/// is listed as outdated. Pure — unit-tested with captured brew output.
fn parse_hint(brew_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(brew_json).ok()?;
    let formula = v.get("formulae")?.as_array()?.iter().find(|f| {
        f.get("name")
            .and_then(|n| n.as_str())
            .is_some_and(|n| n.ends_with("deepshrink"))
    })?;
    let current = formula.get("current_version")?.as_str()?;
    let installed = formula
        .get("installed_versions")?
        .as_array()?
        .last()?
        .as_str()?;
    if installed == current {
        return None;
    }
    Some(format!(
        "A new version of deepshrink is available: {installed} → {current}\n\
         Update: brew upgrade {TAP_FORMULA}",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cellar_path_detection() {
        assert!(is_cellar_path(
            "/opt/homebrew/Cellar/deepshrink/0.3.0/bin/deepshrink"
        ));
        assert!(is_cellar_path(
            "/usr/local/Cellar/deepshrink/0.3.0/bin/deepshrink"
        ));
        assert!(!is_cellar_path("/usr/local/bin/deepshrink"));
        assert!(!is_cellar_path("/home/u/.cargo/bin/deepshrink"));
    }

    #[test]
    fn parse_hint_when_outdated() {
        let json = r#"{"formulae":[{"name":"deeplabua/tap/deepshrink",
            "installed_versions":["0.2.1"],"current_version":"0.3.0"}],"casks":[]}"#;
        let hint = parse_hint(json).expect("expected a hint");
        assert!(hint.contains("0.2.1 → 0.3.0"), "got: {hint}");
        assert!(hint.contains("brew upgrade deeplabua/tap/deepshrink"));
    }

    #[test]
    fn no_hint_when_up_to_date() {
        assert_eq!(parse_hint(r#"{"formulae":[],"casks":[]}"#), None);
        // Defensive: installed already equals current.
        let json = r#"{"formulae":[{"name":"deepshrink",
            "installed_versions":["0.3.0"],"current_version":"0.3.0"}]}"#;
        assert_eq!(parse_hint(json), None);
    }

    #[test]
    fn no_hint_on_garbage() {
        assert_eq!(parse_hint("not json"), None);
        assert_eq!(parse_hint("{}"), None);
        assert_eq!(parse_hint(r#"{"formulae":[]}"#), None);
    }
}
