//! The compression engine contract and shared types.
//!
//! Key architectural decision: `core` is designed around the [`Engine`]
//! interface even while there is exactly one engine (media/ffmpeg). This lets us
//! plug in images/PDF/office later without rewriting the skeleton.
//!
//! Principle: [`Engine::plan`] is a pure, testable function (bitrate math);
//! side effects are isolated in [`Engine::run`].

pub mod media;

use std::path::{Path, PathBuf};
use thiserror::Error;

use crate::detect::MediaKind;
use crate::options::{AudioChoice, AudioCodec, FpsOpt, QualityPreset, ResolutionOpt, VideoCodec};
use crate::size::Preset;

/// Source file metadata — the result of [`Engine::probe`].
#[derive(Debug, Clone, PartialEq)]
pub struct MediaInfo {
    pub path: PathBuf,
    pub kind: MediaKind,
    pub duration_sec: f64,
    pub size_bytes: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub audio_channels: Option<u32>,
}

impl MediaInfo {
    /// Whether the source carries an audio track.
    pub fn has_audio(&self) -> bool {
        self.audio_codec.is_some()
    }
}

/// What the user wants in terms of size.
#[derive(Debug, Clone, PartialEq)]
pub enum SizeGoal {
    /// Fit into an absolute size (bytes).
    Target(u64),
    /// Reduce by a fraction of the original (0.70 = "by 70%").
    Reduce(f64),
    /// Platform preset (sets the target size).
    Preset(Preset),
    /// Smart, quality-preserving shrink without a hard limit.
    Quality,
}

/// Compression options passed to the engine — the decoded form of the flags.
#[derive(Debug, Clone, PartialEq)]
pub struct ShrinkOpts {
    pub goal: SizeGoal,
    pub video_codec: VideoCodec,
    /// What to do with the audio track *inside a video*.
    pub audio: AudioChoice,
    pub resolution: ResolutionOpt,
    pub fps: FpsOpt,
    pub quality: QualityPreset,
    /// Codec for a *pure-audio* input.
    pub audio_codec: AudioCodec,
    /// Downmix pure audio to mono.
    pub mono: bool,
    /// Force a sample rate (Hz) for pure audio; `None` keeps the source rate.
    pub sample_rate: Option<u32>,
    /// Prefer VBR where the codec supports it.
    pub vbr: bool,
    /// Target VMAF: in quality mode, search CRF for the smallest output that
    /// still scores at least this. `None` disables VMAF-aware encoding.
    pub target_vmaf: Option<f64>,
    /// Explicit output path; when `None` the engine derives one.
    pub output: Option<PathBuf>,
}

impl Default for ShrinkOpts {
    fn default() -> Self {
        Self {
            goal: SizeGoal::Quality,
            video_codec: VideoCodec::H264,
            audio: AudioChoice::Keep,
            resolution: ResolutionOpt::Auto,
            fps: FpsOpt::Auto,
            quality: QualityPreset::Balanced,
            audio_codec: AudioCodec::Aac,
            mono: false,
            sample_rate: None,
            vbr: false,
            target_vmaf: None,
            output: None,
        }
    }
}

/// Video encoding parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoSpec {
    pub codec: VideoCodec,
    /// Target average bitrate (bits/s) for two-pass; `None` in CRF/quality mode.
    pub bitrate_bps: Option<u64>,
    /// CRF value for quality mode; `None` in bitrate mode.
    pub crf: Option<u8>,
    /// Downscale target height; `None` keeps the source resolution.
    pub height: Option<u32>,
    /// Frame-rate cap; `None` keeps the source rate.
    pub fps: Option<u32>,
    pub preset: QualityPreset,
}

/// Audio encoding parameters (absent means drop the track).
#[derive(Debug, Clone, PartialEq)]
pub struct AudioSpec {
    pub codec: AudioCodec,
    pub bitrate_bps: u64,
    /// Downmix to a single channel.
    pub mono: bool,
    /// Resample to this rate (Hz); `None` keeps the source rate.
    pub sample_rate: Option<u32>,
    /// Prefer VBR where the codec supports it.
    pub vbr: bool,
}

impl AudioSpec {
    /// A plain CBR/ABR track (used for the audio track inside a video).
    pub fn cbr(codec: AudioCodec, bitrate_bps: u64) -> Self {
        Self {
            codec,
            bitrate_bps,
            mono: false,
            sample_rate: None,
            vbr: false,
        }
    }
}

/// The full encode recipe.
#[derive(Debug, Clone, PartialEq)]
pub struct EncodeSpec {
    pub video: VideoSpec,
    /// `None` means `-an` (no audio).
    pub audio: Option<AudioSpec>,
    pub faststart: bool,
    pub two_pass: bool,
    /// Stream-copy remux only: the source already fits, so never re-encode
    /// (and never inflate). `video`/`audio` are ignored when set.
    pub passthrough: bool,
    /// Pure-audio encode: emit `-vn` and use `audio` only (`video` is ignored).
    pub audio_only: bool,
}

/// The encode plan — the result of [`Engine::plan`]. Usable for `--dry-run`.
#[derive(Debug, Clone, PartialEq)]
pub struct EncodePlan {
    pub input: PathBuf,
    pub output: PathBuf,
    /// Human-readable description of the plan (codec, bitrates, passes).
    pub summary: String,
    /// Expected final size in bytes, if predictable.
    pub expected_bytes: Option<u64>,
    /// The hard size cap, if any (drives the post-encode correction retry).
    pub target_bytes: Option<u64>,
    /// Target VMAF for a CRF search, if requested (quality mode only).
    pub target_vmaf: Option<f64>,
    /// Source duration in seconds (for progress reporting).
    pub source_duration_sec: f64,
    /// Source resolution and frame rate — the reference for VMAF measurement.
    pub source_width: Option<u32>,
    pub source_height: Option<u32>,
    pub source_fps: Option<f64>,
    pub spec: EncodeSpec,
}

/// The execution result — the result of [`Engine::run`].
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub output: PathBuf,
    pub final_bytes: u64,
    /// Measured VMAF of the result vs the source, if a measurement was taken.
    pub vmaf: Option<f64>,
}

/// Engine errors.
#[derive(Debug, Error)]
pub enum EngineError {
    #[error("input not supported by this engine: {0}")]
    Unsupported(String),
    #[error("cannot reach target size at a reasonable quality")]
    Infeasible,
    #[error("not yet implemented: {0}")]
    NotImplemented(&'static str),
    #[error(transparent)]
    Ffmpeg(#[from] deepshrink_ffmpeg::FfmpegError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The compression engine contract. The one v0.1 implementation is [`media::MediaEngine`].
pub trait Engine {
    /// Whether this engine handles the given file.
    fn supports(&self, input: &Path) -> bool;
    /// Read metadata (side effect: run a probe, e.g. ffprobe).
    fn probe(&self, input: &Path) -> Result<MediaInfo, EngineError>;
    /// Build the encode plan. Pure function — tested without encoding.
    fn plan(&self, info: &MediaInfo, opts: &ShrinkOpts) -> Result<EncodePlan, EngineError>;
    /// Execute the plan (side effect: run the encoder binary).
    fn run(&self, plan: &EncodePlan) -> Result<Outcome, EngineError>;
}
