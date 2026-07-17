//! Run `ffprobe` and parse its JSON output.
//!
//! We deserialize only the fields DeepShrink needs and expose typed accessors.
//! Mapping into the core `MediaInfo` type happens in `deepshrink-core` so this
//! crate stays free of a dependency on core.

use std::path::Path;
use std::process::Command;

use serde::Deserialize;

use crate::FfmpegError;

/// Top-level `ffprobe -show_format -show_streams -of json` output (subset).
#[derive(Debug, Clone, Deserialize)]
pub struct Ffprobe {
    #[serde(default)]
    pub streams: Vec<Stream>,
    #[serde(default)]
    pub format: Format,
}

/// The `format` object: container-level metadata.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Format {
    /// Duration in seconds, as a string like "58.023000".
    pub duration: Option<String>,
    /// File size in bytes, as a string.
    pub size: Option<String>,
    /// Overall bit rate in bits/s, as a string.
    pub bit_rate: Option<String>,
}

/// A single stream (video or audio).
#[derive(Debug, Clone, Deserialize)]
pub struct Stream {
    /// "video", "audio", "subtitle", ...
    pub codec_type: Option<String>,
    pub codec_name: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub channels: Option<u32>,
    /// Average frame rate as "num/den", e.g. "30000/1001".
    pub r_frame_rate: Option<String>,
}

impl Ffprobe {
    /// Container duration in seconds, if reported.
    pub fn duration_sec(&self) -> Option<f64> {
        self.format.duration.as_deref().and_then(parse_f64)
    }

    /// Container size in bytes, if reported.
    pub fn size_bytes(&self) -> Option<u64> {
        self.format
            .size
            .as_deref()
            .and_then(|s| s.trim().parse().ok())
    }

    /// First video stream, if any.
    pub fn video_stream(&self) -> Option<&Stream> {
        self.streams
            .iter()
            .find(|s| s.codec_type.as_deref() == Some("video"))
    }

    /// First audio stream, if any.
    pub fn audio_stream(&self) -> Option<&Stream> {
        self.streams
            .iter()
            .find(|s| s.codec_type.as_deref() == Some("audio"))
    }

    /// Frame rate of the first video stream, if parseable.
    pub fn fps(&self) -> Option<f64> {
        self.video_stream()
            .and_then(|s| s.r_frame_rate.as_deref())
            .and_then(parse_ratio)
    }
}

/// Parse "num/den" (e.g. "30000/1001") into a float, guarding against `/0`.
fn parse_ratio(s: &str) -> Option<f64> {
    let (num, den) = s.split_once('/')?;
    let num: f64 = num.trim().parse().ok()?;
    let den: f64 = den.trim().parse().ok()?;
    if den == 0.0 {
        return None;
    }
    Some(num / den)
}

fn parse_f64(s: &str) -> Option<f64> {
    let v: f64 = s.trim().parse().ok()?;
    if v.is_finite() {
        Some(v)
    } else {
        None
    }
}

/// Probe `input` with `ffprobe`, returning parsed metadata.
pub fn probe(ffprobe: &Path, input: &Path) -> Result<Ffprobe, FfmpegError> {
    let output = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_format",
            "-show_streams",
            "-of",
            "json",
        ])
        .arg(input)
        .output()
        .map_err(|source| FfmpegError::Spawn {
            tool: "ffprobe",
            source,
        })?;

    if !output.status.success() {
        return Err(FfmpegError::CommandFailed {
            tool: "ffprobe",
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }

    serde_json::from_slice(&output.stdout).map_err(|e| FfmpegError::Parse(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "streams": [
            {"codec_type":"video","codec_name":"h264","width":1920,"height":1080,"r_frame_rate":"30000/1001"},
            {"codec_type":"audio","codec_name":"aac","channels":2,"r_frame_rate":"0/0"}
        ],
        "format": {"duration":"134.20","size":"327553024","bit_rate":"19500000"}
    }"#;

    #[test]
    fn parses_sample() {
        let p: Ffprobe = serde_json::from_str(SAMPLE).unwrap();
        assert_eq!(p.duration_sec(), Some(134.20));
        assert_eq!(p.size_bytes(), Some(327_553_024));
        let v = p.video_stream().unwrap();
        assert_eq!(v.width, Some(1920));
        assert_eq!(v.height, Some(1080));
        assert_eq!(p.audio_stream().unwrap().channels, Some(2));
        assert!((p.fps().unwrap() - 29.97).abs() < 0.01);
    }

    #[test]
    fn tolerates_missing_fields() {
        let p: Ffprobe = serde_json::from_str(r#"{"format":{}}"#).unwrap();
        assert_eq!(p.duration_sec(), None);
        assert!(p.video_stream().is_none());
        assert!(p.audio_stream().is_none());
    }

    #[test]
    fn ratio_guards_zero_denominator() {
        assert_eq!(parse_ratio("0/0"), None);
        assert_eq!(parse_ratio("30/1"), Some(30.0));
    }
}
