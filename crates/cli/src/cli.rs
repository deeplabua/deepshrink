//! Command-line surface (clap derive). Flags mirror `docs/PRD/02-cli-spec.md`.
//!
//! This module only declares the interface; routing and behaviour live in
//! `main.rs`. In this scaffold most flags are parsed but not yet acted upon.

use std::path::PathBuf;

use clap::{ArgGroup, Parser, ValueEnum};

/// Shrink media (video/audio) to a target size with one command.
#[derive(Debug, Parser)]
#[command(
    name = "deepshrink",
    version,
    about = "Shrink media to a target size with one command — local, private, no watermarks.",
    long_about = None,
)]
// The three size flags are mutually exclusive; none is required (default = smart quality mode).
#[command(group(ArgGroup::new("size").args(["target", "reduce", "for_preset"])))]
pub struct Cli {
    /// Input files (video/audio), or a folder with --recursive.
    #[arg(required = true, value_name = "INPUT")]
    pub inputs: Vec<PathBuf>,

    // --- Size options (mutually exclusive) ---
    /// Fit into an absolute size, e.g. 8MB, 500KB, 1.5GB.
    #[arg(long, value_name = "SIZE")]
    pub target: Option<String>,

    /// Reduce by N percent of the original, e.g. 70%.
    #[arg(long, value_name = "PCT")]
    pub reduce: Option<String>,

    /// Platform preset that sets the target: discord, email, telegram, whatsapp, web, ...
    #[arg(long = "for", value_name = "PRESET")]
    pub for_preset: Option<String>,

    // --- Video ---
    /// Video codec (default: h264 for compatibility).
    #[arg(long, value_enum)]
    pub codec: Option<VideoCodec>,

    /// Lower the resolution if bitrate is insufficient.
    #[arg(long, value_name = "RES", default_value = "auto")]
    pub resolution: String,

    /// Cap the frame rate.
    #[arg(long, value_name = "FPS", default_value = "auto")]
    pub fps: String,

    /// Audio track inside video: keep | <bitrate> | none.
    #[arg(long, value_name = "SPEC", default_value = "keep")]
    pub audio: String,

    /// Target VMAF; do not re-encode below this quality.
    #[arg(long, value_name = "N")]
    pub vmaf: Option<f64>,

    // --- Audio ---
    /// Audio codec (default: aac; opus is best at low bitrates).
    #[arg(long = "audio-codec", value_enum)]
    pub audio_codec: Option<AudioCodec>,

    /// Downmix to mono (cheaper bitrate for speech).
    #[arg(long)]
    pub mono: bool,

    /// Lower the sample rate when needed.
    #[arg(long = "sample-rate", value_name = "HZ", default_value = "auto")]
    pub sample_rate: String,

    /// Use VBR instead of CBR where the codec supports it.
    #[arg(long)]
    pub vbr: bool,

    // --- Misc ---
    /// Speed/quality trade-off (encoder preset).
    #[arg(long, value_enum, default_value_t = Quality::Balanced)]
    pub quality: Quality,

    /// Where to write the result.
    #[arg(long, value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// Overwrite the original (asks for confirmation).
    #[arg(long)]
    pub overwrite: bool,

    /// Recurse into folders.
    #[arg(long)]
    pub recursive: bool,

    /// Show the plan (bitrate, expected size) without encoding.
    #[arg(long = "dry-run")]
    pub dry_run: bool,

    /// Machine-readable output (for integrations / desktop).
    #[arg(long)]
    pub json: bool,

    /// Quiet output.
    #[arg(short, long)]
    pub quiet: bool,

    /// Verbose output.
    #[arg(short, long)]
    pub verbose: bool,
}

/// Video codec choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum VideoCodec {
    #[value(name = "h264")]
    H264,
    #[value(name = "h265")]
    H265,
}

/// Audio codec choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AudioCodec {
    Aac,
    Opus,
    Mp3,
}

/// Speed/quality trade-off preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Quality {
    Fast,
    Balanced,
    Max,
}
