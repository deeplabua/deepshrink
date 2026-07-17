//! Parse ffmpeg's `-progress pipe:1` key=value stream.
//!
//! With `-progress pipe:1 -nostats`, ffmpeg writes blocks of `key=value` lines
//! to stdout, ending each block with `progress=continue` or `progress=end`.
//! We only care about the elapsed output time and the terminal marker.

/// One parsed progress line of interest.
#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    /// Elapsed output time in microseconds.
    OutTimeUs(u64),
    /// `progress=continue`.
    Continue,
    /// `progress=end` — encoding finished.
    End,
}

/// Parse a single line. Returns `None` for keys we ignore.
pub fn parse_line(line: &str) -> Option<Progress> {
    let (key, value) = line.split_once('=')?;
    match key.trim() {
        "out_time_us" | "out_time_ms" => {
            // `out_time_us` is microseconds; the older `out_time_ms` alias is
            // *also* microseconds in ffmpeg (a long-standing naming quirk).
            value.trim().parse().ok().map(Progress::OutTimeUs)
        }
        "progress" => match value.trim() {
            "end" => Some(Progress::End),
            "continue" => Some(Progress::Continue),
            _ => None,
        },
        _ => None,
    }
}

/// Convert elapsed microseconds and total seconds into a clamped 0.0..=1.0 fraction.
pub fn fraction(out_time_us: u64, total_secs: f64) -> f64 {
    if total_secs <= 0.0 {
        return 0.0;
    }
    let elapsed_secs = out_time_us as f64 / 1_000_000.0;
    (elapsed_secs / total_secs).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_out_time() {
        assert_eq!(
            parse_line("out_time_us=1500000"),
            Some(Progress::OutTimeUs(1_500_000))
        );
        assert_eq!(
            parse_line("out_time_ms=2000000"),
            Some(Progress::OutTimeUs(2_000_000))
        );
    }

    #[test]
    fn parses_progress_markers() {
        assert_eq!(parse_line("progress=continue"), Some(Progress::Continue));
        assert_eq!(parse_line("progress=end"), Some(Progress::End));
    }

    #[test]
    fn ignores_other_keys() {
        assert_eq!(parse_line("frame=42"), None);
        assert_eq!(parse_line("bitrate=  456.7kbits/s"), None);
        assert_eq!(parse_line("no-equals-sign"), None);
    }

    #[test]
    fn fraction_clamps() {
        assert_eq!(fraction(0, 10.0), 0.0);
        assert_eq!(fraction(5_000_000, 10.0), 0.5);
        assert_eq!(fraction(20_000_000, 10.0), 1.0);
        assert_eq!(fraction(1_000_000, 0.0), 0.0);
    }
}
