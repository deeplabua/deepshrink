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

use owo_colors::{OwoColorize, Stream::Stderr};

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
    let (installed, current) = parse_versions(&std::fs::read_to_string(&cache).ok()?)?;
    Some(styled_hint(&installed, &current))
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
    //
    // IMPORTANT: `brew outdated` exits *non-zero* when something IS outdated —
    // exactly the case we care about. So we must not gate the move on its exit
    // code (`&& mv`), or the notice would never be written. Instead: always run
    // the query, then move iff it produced output (`[ -s tmp ]`).
    let script = format!(
        "HOMEBREW_NO_AUTO_UPDATE=1 brew outdated --json=v2 {formula} > {tmp} 2>/dev/null; \
         [ -s {tmp} ] && mv -f {tmp} {cache} || rm -f {tmp}",
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

/// Extract `(installed, current)` from `brew outdated --json=v2` output when our
/// formula is listed as outdated. Pure — unit-tested with captured brew output.
fn parse_versions(brew_json: &str) -> Option<(String, String)> {
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
    Some((installed.to_string(), current.to_string()))
}

/// Render the two-line notice: the intro is dimmed, versions and "Update:" stay
/// in the normal foreground, and the two things that matter are highlighted —
/// the new version (bold green) and the upgrade command (bold cyan).
fn styled_hint(installed: &str, current: &str) -> String {
    let cmd = format!("brew upgrade {TAP_FORMULA}");
    format!(
        "{} {installed} → {}\nUpdate: {}",
        "A new version of deepshrink is available:"
            .if_supports_color(Stderr, |t| t.dimmed().to_string()),
        current.if_supports_color(Stderr, |t| t.green().bold().to_string()),
        cmd.if_supports_color(Stderr, |t| t.cyan().bold().to_string()),
    )
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
    fn parse_versions_when_outdated() {
        let json = r#"{"formulae":[{"name":"deeplabua/tap/deepshrink",
            "installed_versions":["0.2.1"],"current_version":"0.3.0"}],"casks":[]}"#;
        assert_eq!(
            parse_versions(json),
            Some(("0.2.1".to_string(), "0.3.0".to_string()))
        );
    }

    #[test]
    fn no_versions_when_up_to_date() {
        assert_eq!(parse_versions(r#"{"formulae":[],"casks":[]}"#), None);
        // Defensive: installed already equals current.
        let json = r#"{"formulae":[{"name":"deepshrink",
            "installed_versions":["0.3.0"],"current_version":"0.3.0"}]}"#;
        assert_eq!(parse_versions(json), None);
    }

    #[test]
    fn no_versions_on_garbage() {
        assert_eq!(parse_versions("not json"), None);
        assert_eq!(parse_versions("{}"), None);
        assert_eq!(parse_versions(r#"{"formulae":[]}"#), None);
    }

    #[test]
    fn styled_hint_carries_versions_and_command() {
        let hint = styled_hint("0.2.1", "0.3.0");
        assert!(hint.contains("0.2.1"), "got: {hint}");
        assert!(hint.contains("0.3.0"));
        assert!(hint.contains("brew upgrade deeplabua/tap/deepshrink"));
    }
}
