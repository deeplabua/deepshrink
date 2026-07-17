//! `deepshrink-core` — the pure, testable core of DeepShrink.
//!
//! No I/O side effects: size/preset parsing, file-type detection and the engine
//! contract (`Engine`). The heavy lifting (invoking ffmpeg) happens in a
//! concrete engine's `run` layer; here we keep the math and types, unit-tested.
#![forbid(unsafe_code)]

pub mod budget;
pub mod detect;
pub mod engine;
pub mod options;
pub mod size;

pub use detect::{detect_kind, MediaKind};
pub use engine::{
    media::MediaEngine, AudioSpec, EncodePlan, EncodeSpec, Engine, EngineError, MediaInfo, Outcome,
    ShrinkOpts, SizeGoal, VideoSpec,
};
pub use options::{AudioChoice, AudioCodec, FpsOpt, QualityPreset, ResolutionOpt, VideoCodec};
pub use size::{parse_percent, parse_size, preset, Preset, SizeError};
