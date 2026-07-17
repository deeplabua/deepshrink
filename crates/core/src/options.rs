//! Encoding option enums shared between the CLI and the engine.
//!
//! These are the decoded, engine-facing forms of the user's flags (the CLI maps
//! its clap types onto these). Keeping them here lets `plan` stay a pure
//! function over well-typed inputs.

/// Video codec choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    H265,
}

impl VideoCodec {
    /// The libav encoder name passed to `ffmpeg -c:v`.
    pub fn encoder(self) -> &'static str {
        match self {
            VideoCodec::H264 => "libx264",
            VideoCodec::H265 => "libx265",
        }
    }

    /// The `-tag:v` value needed for MP4 compatibility, if any.
    pub fn mp4_tag(self) -> Option<&'static str> {
        match self {
            VideoCodec::H264 => None,
            // Without hvc1, HEVC in MP4 won't play in QuickTime/Safari.
            VideoCodec::H265 => Some("hvc1"),
        }
    }

    /// Human-readable label for output.
    pub fn label(self) -> &'static str {
        match self {
            VideoCodec::H264 => "H.264",
            VideoCodec::H265 => "H.265",
        }
    }
}

/// Audio codec choice (also used for pure-audio in session 003).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    Aac,
    Opus,
    Mp3,
}

impl AudioCodec {
    pub fn encoder(self) -> &'static str {
        match self {
            AudioCodec::Aac => "aac",
            AudioCodec::Opus => "libopus",
            AudioCodec::Mp3 => "libmp3lame",
        }
    }

    /// Output file extension for a pure-audio result.
    pub fn extension(self) -> &'static str {
        match self {
            AudioCodec::Aac => "m4a",
            AudioCodec::Opus => "opus",
            AudioCodec::Mp3 => "mp3",
        }
    }

    /// Human-readable label for output.
    pub fn label(self) -> &'static str {
        match self {
            AudioCodec::Aac => "AAC",
            AudioCodec::Opus => "Opus",
            AudioCodec::Mp3 => "MP3",
        }
    }
}

/// What to do with the audio track inside a video.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioChoice {
    /// Keep the track, re-encoding at a sensible (budget-aware) bitrate.
    Keep,
    /// Keep the track at an explicit bitrate in bits/s.
    Bitrate(u64),
    /// Drop the audio entirely.
    Drop,
}

/// Target resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionOpt {
    /// Let the engine pick (downscale only if bitrate is too low).
    Auto,
    /// Force a target height (width follows aspect ratio).
    Height(u32),
}

/// Frame-rate cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FpsOpt {
    Auto,
    Cap(u32),
}

/// Speed/quality trade-off, mapped to the encoder preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityPreset {
    Fast,
    Balanced,
    Max,
}

impl QualityPreset {
    /// x264/x265 `-preset` value.
    pub fn encoder_preset(self) -> &'static str {
        match self {
            QualityPreset::Fast => "veryfast",
            QualityPreset::Balanced => "medium",
            QualityPreset::Max => "slow",
        }
    }
}
