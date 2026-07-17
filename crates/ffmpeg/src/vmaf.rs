//! VMAF quality measurement via ffmpeg's `libvmaf` filter.
//!
//! `libvmaf` is not present in every ffmpeg build, so callers should gate on
//! [`has_libvmaf`] and degrade gracefully (skip the measurement) when it is
//! absent — see the CLI's `--vmaf` handling.

use std::path::Path;
use std::process::Command;

use crate::FfmpegError;

/// Whether this ffmpeg build exposes the `libvmaf` filter.
pub fn has_libvmaf(ffmpeg: &Path) -> bool {
    Command::new(ffmpeg)
        .args(["-hide_banner", "-filters"])
        .output()
        .map(|o| {
            let text = String::from_utf8_lossy(&o.stdout);
            text.contains("libvmaf")
        })
        .unwrap_or(false)
}

/// Measure the VMAF of `distorted` against the `reference` (original) video.
///
/// The distorted stream is scaled up to the reference resolution and both
/// streams are normalized to `fps` so their frame counts align (a downscale or
/// fps cap would otherwise desynchronize the comparison). Returns the pooled
/// mean VMAF score in 0..=100.
pub fn measure_vmaf(
    ffmpeg: &Path,
    distorted: &Path,
    reference: &Path,
    ref_width: u32,
    ref_height: u32,
    fps: f64,
    n_threads: usize,
) -> Result<f64, FfmpegError> {
    // Input 0 = distorted, input 1 = reference. libvmaf consumes `[main][ref]`.
    let fps_term = if fps.is_finite() && fps > 0.0 {
        format!(",fps={fps}")
    } else {
        String::new()
    };
    let threads = n_threads.max(1);
    let filter = format!(
        "[0:v]scale={ref_width}:{ref_height}:flags=bicubic,setsar=1{fps_term},\
         settb=AVTB,setpts=PTS-STARTPTS[dist];\
         [1:v]setsar=1{fps_term},settb=AVTB,setpts=PTS-STARTPTS[ref];\
         [dist][ref]libvmaf=n_threads={threads}"
    );

    let output = Command::new(ffmpeg)
        .args(["-hide_banner", "-nostdin", "-i"])
        .arg(distorted)
        .arg("-i")
        .arg(reference)
        .args(["-lavfi", &filter, "-f", "null", "-"])
        .output()
        .map_err(|source| FfmpegError::Spawn {
            tool: "ffmpeg",
            source,
        })?;

    if !output.status.success() {
        return Err(FfmpegError::CommandFailed {
            tool: "ffmpeg",
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }

    // libvmaf logs `VMAF score: NN.NN` to stderr.
    let stderr = String::from_utf8_lossy(&output.stderr);
    parse_vmaf_score(&stderr)
        .ok_or_else(|| FfmpegError::Parse("could not find a VMAF score in ffmpeg output".into()))
}

/// Extract the `VMAF score: <n>` value from ffmpeg's stderr.
fn parse_vmaf_score(stderr: &str) -> Option<f64> {
    const MARKER: &str = "VMAF score:";
    for line in stderr.lines() {
        if let Some(idx) = line.find(MARKER) {
            let tail = line[idx + MARKER.len()..].trim();
            if let Some(score) = tail
                .split_whitespace()
                .next()
                .and_then(|t| t.parse::<f64>().ok())
            {
                return Some(score);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_score_line() {
        let s = "frame=  150 fps=...\n[libvmaf @ 0x7f] VMAF score: 91.234567\n";
        assert_eq!(parse_vmaf_score(s), Some(91.234567));
    }

    #[test]
    fn parses_bare_score_line() {
        assert_eq!(parse_vmaf_score("VMAF score: 100.000000"), Some(100.0));
    }

    #[test]
    fn none_when_absent() {
        assert_eq!(parse_vmaf_score("frame=1 fps=2\nEncoding done\n"), None);
    }
}
