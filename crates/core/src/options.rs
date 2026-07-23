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
    Av1,
}

impl VideoCodec {
    /// The libav encoder name passed to `ffmpeg -c:v`.
    pub fn encoder(self) -> &'static str {
        match self {
            VideoCodec::H264 => "libx264",
            VideoCodec::H265 => "libx265",
            // SVT-AV1 is the practical default: far faster than libaom at
            // comparable quality, and present in mainstream ffmpeg builds.
            VideoCodec::Av1 => "libsvtav1",
        }
    }

    /// A second encoder to try when [`encoder`](Self::encoder) is missing from
    /// the local ffmpeg build. Only AV1 has one — x264/x265 are universal.
    pub fn fallback_encoder(self) -> Option<&'static str> {
        match self {
            VideoCodec::Av1 => Some("libaom-av1"),
            _ => None,
        }
    }

    /// The `-tag:v` value needed for MP4 compatibility, if any.
    pub fn mp4_tag(self) -> Option<&'static str> {
        match self {
            VideoCodec::H264 => None,
            // Without hvc1, HEVC in MP4 won't play in QuickTime/Safari.
            VideoCodec::H265 => Some("hvc1"),
            VideoCodec::Av1 => Some("av01"),
        }
    }

    /// Human-readable label for output.
    pub fn label(self) -> &'static str {
        match self {
            VideoCodec::H264 => "H.264",
            VideoCodec::H265 => "H.265",
            VideoCodec::Av1 => "AV1",
        }
    }

    /// Inclusive CRF range to search when targeting a VMAF score, best quality
    /// (lowest CRF) first. x265's CRF scale is shifted ~+6 vs x264 for the same
    /// perceptual quality, and AV1's runs 0..63 — so the bounds differ per codec.
    pub fn crf_search_bounds(self) -> (u8, u8) {
        match self {
            VideoCodec::H264 => (18, 32),
            VideoCodec::H265 => (22, 36),
            VideoCodec::Av1 => (25, 50),
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

    /// The speed/quality knob for a specific *encoder*, as `(flag, value)`.
    ///
    /// Encoders disagree on both the flag and its scale: x264/x265 take named
    /// `-preset`s, SVT-AV1 takes a numeric `-preset` (0 slowest … 13 fastest),
    /// and libaom takes `-cpu-used`. Passing an x264 preset name to SVT-AV1 is
    /// a hard ffmpeg error, so the argv builder must ask per encoder.
    pub fn speed_flags(self, encoder: &str) -> (&'static str, &'static str) {
        match encoder {
            "libsvtav1" => (
                "-preset",
                match self {
                    QualityPreset::Fast => "9",
                    QualityPreset::Balanced => "7",
                    QualityPreset::Max => "5",
                },
            ),
            "libaom-av1" => (
                "-cpu-used",
                match self {
                    QualityPreset::Fast => "8",
                    QualityPreset::Balanced => "5",
                    QualityPreset::Max => "3",
                },
            ),
            _ => ("-preset", self.encoder_preset()),
        }
    }

    /// Default quality-mode CRF for a codec. x265 needs a higher CRF than x264
    /// for comparable quality, and AV1 higher still, so the numbers are
    /// codec-specific. These defaults aim for roughly VMAF ~93 (visually
    /// near-transparent) on typical content.
    pub fn default_crf(self, codec: VideoCodec) -> u8 {
        match codec {
            VideoCodec::H264 => match self {
                QualityPreset::Fast => 25,
                QualityPreset::Balanced => 23,
                QualityPreset::Max => 20,
            },
            VideoCodec::H265 => match self {
                QualityPreset::Fast => 30,
                QualityPreset::Balanced => 28,
                QualityPreset::Max => 24,
            },
            VideoCodec::Av1 => match self {
                QualityPreset::Fast => 38,
                QualityPreset::Balanced => 35,
                QualityPreset::Max => 30,
            },
        }
    }
}
