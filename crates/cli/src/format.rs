//! Human-friendly formatting for the terminal report.

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
}
