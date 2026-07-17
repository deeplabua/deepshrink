//! Human-friendly formatting for the terminal report.

use std::path::{Path, PathBuf};

use deepshrink_core::EncodePlan;

/// Format a byte count with decimal units (matching the size-parsing convention).
pub fn size(bytes: u64) -> String {
    let f = bytes as f64;
    if f >= 1e9 {
        format!("{:.1} GB", f / 1e9)
    } else if f >= 1e6 {
        format!("{:.1} MB", f / 1e6)
    } else if f >= 1e3 {
        format!("{:.1} KB", f / 1e3)
    } else {
        format!("{bytes} B")
    }
}

/// Format a duration in seconds as `2m14s` / `1h03m07s`.
pub fn duration(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "—".to_string();
    }
    let total = secs.round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else {
        format!("{m}m{s:02}s")
    }
}

/// The `target` line value: the hard size cap, or a quality-mode note.
pub fn target_label(plan: &EncodePlan) -> String {
    match plan.target_bytes {
        Some(b) => size(b),
        None => "quality (no size limit)".to_string(),
    }
}

/// Format an output path for display: relative to the current directory when
/// the file sits under it, else with the home directory collapsed to `~`, else
/// the path as-is. Helps users find results written next to a distant source.
pub fn display_path(path: &Path) -> String {
    let cwd = std::env::current_dir().ok();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    display_path_rel(path, cwd.as_deref(), home.as_deref())
}

/// Core of [`display_path`], with the current dir and home passed explicitly so
/// it can be unit-tested deterministically.
fn display_path_rel(path: &Path, cwd: Option<&Path>, home: Option<&Path>) -> String {
    if let Some(cwd) = cwd {
        if let Ok(rel) = path.strip_prefix(cwd) {
            let s = rel.display().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    if let Some(home) = home {
        if let Ok(rest) = path.strip_prefix(home) {
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(size(500), "500 B");
        assert_eq!(size(500_000), "500.0 KB");
        assert_eq!(size(7_600_000), "7.6 MB");
        assert_eq!(size(1_500_000_000), "1.5 GB");
    }

    #[test]
    fn durations() {
        assert_eq!(duration(134.0), "2m14s");
        assert_eq!(duration(3_607.0), "1h00m07s");
        assert_eq!(duration(5.0), "0m05s");
        assert_eq!(duration(f64::NAN), "—");
    }

    #[test]
    fn display_path_prefers_cwd_relative() {
        let cwd = Path::new("/home/u/work");
        let home = Path::new("/home/u");
        // Under cwd → relative.
        assert_eq!(
            display_path_rel(
                Path::new("/home/u/work/out/clip.mp4"),
                Some(cwd),
                Some(home)
            ),
            "out/clip.mp4"
        );
        // Under home but not cwd → ~-collapsed.
        assert_eq!(
            display_path_rel(
                Path::new("/home/u/Downloads/clip.mp4"),
                Some(cwd),
                Some(home)
            ),
            "~/Downloads/clip.mp4"
        );
        // Elsewhere → absolute.
        assert_eq!(
            display_path_rel(Path::new("/var/tmp/clip.mp4"), Some(cwd), Some(home)),
            "/var/tmp/clip.mp4"
        );
        // Relative input (no prefixes match) → unchanged.
        assert_eq!(
            display_path_rel(Path::new("clip.shrink.mp4"), Some(cwd), Some(home)),
            "clip.shrink.mp4"
        );
    }
}
