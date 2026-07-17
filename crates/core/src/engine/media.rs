//! Media engine v0.1: video + audio via ffmpeg (external process).
//!
//! - `probe` shells out to ffprobe and maps the result into [`MediaInfo`].
//! - `plan` is pure bitrate budgeting → an [`EncodePlan`] (tested without ffmpeg).
//!   `plan` dispatches on media kind: two-pass video vs single-pass audio.
//! - `run` executes the plan: encode, size verification and (for video) a single
//!   correction retry on overshoot.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use super::{
    AudioSpec, EncodePlan, EncodeSpec, Engine, EngineError, MediaInfo, Outcome, ShrinkOpts,
    SizeGoal, VideoSpec,
};
use crate::budget;
use crate::detect::{detect_kind, MediaKind};
use crate::options::{AudioChoice, AudioCodec, FpsOpt, ResolutionOpt};

/// Audio bitrate ladder (bits/s, descending) tried when keeping a track under
/// a tight size budget.
const AUDIO_LADDER: &[u64] = &[128_000, 96_000, 64_000, 48_000];

/// Which pass of the encode a progress update belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassKind {
    Single,
    First,
    Second,
}

/// The ffmpeg engine for video and audio.
#[derive(Debug, Default, Clone, Copy)]
pub struct MediaEngine;

impl MediaEngine {
    pub fn new() -> Self {
        Self
    }

    /// Like [`Engine::run`] but reports progress: `on_progress(pass, fraction)`
    /// is called with `fraction` in 0.0..=1.0 as each pass proceeds.
    pub fn run_with_progress(
        &self,
        plan: &EncodePlan,
        on_progress: &mut dyn FnMut(PassKind, f64),
    ) -> Result<Outcome, EngineError> {
        let tools = deepshrink_ffmpeg::locate()?;
        let passlog = passlog_base(plan);
        let total = plan.source_duration_sec;

        if plan.spec.two_pass {
            let args1 = build_pass_args(plan, PassKind::First, &passlog);
            deepshrink_ffmpeg::run_pass(&tools.ffmpeg, &args1, total, &mut |f| {
                on_progress(PassKind::First, f)
            })?;
            let args2 = build_pass_args(plan, PassKind::Second, &passlog);
            deepshrink_ffmpeg::run_pass(&tools.ffmpeg, &args2, total, &mut |f| {
                on_progress(PassKind::Second, f)
            })?;
        } else {
            let args = build_pass_args(plan, PassKind::Single, &passlog);
            deepshrink_ffmpeg::run_pass(&tools.ffmpeg, &args, total, &mut |f| {
                on_progress(PassKind::Single, f)
            })?;
        }

        let mut size = fs::metadata(&plan.output)?.len();

        // Single correction retry: if two-pass overshot the target (VBV slack),
        // scale the video bitrate down proportionally and re-run pass 2.
        if let (Some(target), Some(vbps)) = (plan.target_bytes, plan.spec.video.bitrate_bps) {
            if size > target && plan.spec.two_pass {
                let corrected = (vbps as f64 * (target as f64 / size as f64) * 0.97) as u64;
                if corrected >= budget::ABSOLUTE_MIN_VIDEO_BPS {
                    let mut retry = plan.clone();
                    retry.spec.video.bitrate_bps = Some(corrected);
                    let args = build_pass_args(&retry, PassKind::Second, &passlog);
                    deepshrink_ffmpeg::run_pass(&tools.ffmpeg, &args, total, &mut |f| {
                        on_progress(PassKind::Second, f)
                    })?;
                    size = fs::metadata(&plan.output)?.len();
                }
            }
        }

        cleanup_passlog(&passlog);
        Ok(Outcome {
            output: plan.output.clone(),
            final_bytes: size,
        })
    }

    /// Plan a pure-audio encode (single pass, codec + fitted bitrate).
    fn plan_audio(&self, info: &MediaInfo, opts: &ShrinkOpts) -> Result<EncodePlan, EngineError> {
        let duration = info.duration_sec;
        if !duration.is_finite() || duration <= 0.0 {
            return Err(EngineError::Unsupported(format!(
                "could not determine duration of {}",
                info.path.display()
            )));
        }
        let codec = opts.audio_codec;
        let target = target_bytes(&opts.goal, info.size_bytes);

        // "Never make it bigger": stream-copy remux when the source already fits.
        if let Some(tb) = target {
            if info.size_bytes > 0 && info.size_bytes <= tb {
                let src_ext = info
                    .path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("audio");
                let output = opts
                    .output
                    .clone()
                    .unwrap_or_else(|| output_with_ext(&info.path, src_ext));
                return Ok(passthrough_plan(info, output, tb, false));
            }
        }

        // Mono for speech: explicit flag, or a single-channel source.
        let mono = opts.mono || info.audio_channels == Some(1);

        let (bitrate_bps, expected_bytes) = match target {
            Some(tb) => {
                let raw = budget::audio_bitrate_bps(tb, duration).ok_or(EngineError::Infeasible)?;
                if raw < budget::ABSOLUTE_MIN_AUDIO_BPS {
                    return Err(EngineError::Infeasible);
                }
                let bps = budget::snap_audio_bitrate(raw);
                let predicted = (bps as f64 * duration / 8.0 * (1.0 + budget::CONTAINER_OVERHEAD))
                    .round() as u64;
                (bps, Some(predicted))
            }
            None => {
                // Quality mode: a transparent-ish default, lower for speech.
                let bps = if mono { 96_000 } else { 160_000 };
                (bps, None)
            }
        };

        let audio = AudioSpec {
            codec,
            bitrate_bps,
            mono,
            sample_rate: opts.sample_rate,
            vbr: opts.vbr,
        };
        let output = opts
            .output
            .clone()
            .unwrap_or_else(|| output_with_ext(&info.path, codec.extension()));
        let summary = build_audio_summary(&audio, info.audio_channels);

        Ok(EncodePlan {
            input: info.path.clone(),
            output,
            summary,
            expected_bytes,
            target_bytes: target,
            source_duration_sec: duration,
            spec: EncodeSpec {
                video: placeholder_video_spec(),
                audio: Some(audio),
                faststart: false,
                two_pass: false,
                passthrough: false,
                audio_only: true,
            },
        })
    }
}

impl Engine for MediaEngine {
    fn supports(&self, input: &Path) -> bool {
        matches!(detect_kind(input), MediaKind::Video | MediaKind::Audio)
    }

    fn probe(&self, input: &Path) -> Result<MediaInfo, EngineError> {
        let tools = deepshrink_ffmpeg::locate()?;
        let p = deepshrink_ffmpeg::probe(&tools.ffprobe, input)?;

        let video = p.video_stream();
        let audio = p.audio_stream();
        // Prefer ffprobe's reported size; fall back to the filesystem.
        let size_bytes = p
            .size_bytes()
            .or_else(|| fs::metadata(input).ok().map(|m| m.len()))
            .unwrap_or(0);

        Ok(MediaInfo {
            path: input.to_path_buf(),
            kind: detect_kind(input),
            duration_sec: p.duration_sec().unwrap_or(0.0),
            size_bytes,
            width: video.and_then(|v| v.width),
            height: video.and_then(|v| v.height),
            fps: p.fps(),
            video_codec: video.and_then(|v| v.codec_name.clone()),
            audio_codec: audio.and_then(|a| a.codec_name.clone()),
            audio_channels: audio.and_then(|a| a.channels),
        })
    }

    fn plan(&self, info: &MediaInfo, opts: &ShrinkOpts) -> Result<EncodePlan, EngineError> {
        match info.kind {
            MediaKind::Audio => return self.plan_audio(info, opts),
            MediaKind::Unsupported => {
                return Err(EngineError::Unsupported(format!(
                    "{} is not a supported media file",
                    info.path.display()
                )))
            }
            MediaKind::Video => {}
        }
        let duration = info.duration_sec;
        if !duration.is_finite() || duration <= 0.0 {
            return Err(EngineError::Unsupported(format!(
                "could not determine duration of {}",
                info.path.display()
            )));
        }
        let src_height = info.height.unwrap_or(0);

        let target = target_bytes(&opts.goal, info.size_bytes);
        let output = opts
            .output
            .clone()
            .unwrap_or_else(|| output_with_ext(&info.path, "mp4"));

        // "Never make it bigger": if the source already fits the target, just
        // remux (stream copy) instead of re-encoding it up to the target.
        if let Some(tb) = target {
            if info.size_bytes > 0 && info.size_bytes <= tb {
                return Ok(passthrough_plan(info, output, tb, true));
            }
        }

        let audio = decide_audio(opts, info.has_audio(), target, duration)?;
        let audio_bps = audio.as_ref().map(|a| a.bitrate_bps).unwrap_or(0);

        let (video, expected_bytes) = if let Some(tb) = target {
            let vbps = budget::video_bitrate_bps(tb, duration, audio_bps)
                .filter(|&b| b >= budget::ABSOLUTE_MIN_VIDEO_BPS)
                .ok_or(EngineError::Infeasible)?;
            let height = pick_height(opts.resolution, src_height, vbps);
            let predicted = ((vbps + audio_bps) as f64 * duration / 8.0
                * (1.0 + budget::CONTAINER_OVERHEAD))
                .round() as u64;
            (
                VideoSpec {
                    codec: opts.video_codec,
                    bitrate_bps: Some(vbps),
                    crf: None,
                    height,
                    fps: pick_fps(opts.fps, info.fps),
                    preset: opts.quality,
                },
                Some(predicted),
            )
        } else {
            // Quality mode: CRF, no hard size guarantee.
            let crf = match opts.quality {
                crate::options::QualityPreset::Fast => 25,
                crate::options::QualityPreset::Balanced => 23,
                crate::options::QualityPreset::Max => 20,
            };
            let height = match opts.resolution {
                ResolutionOpt::Height(h) => clamp_height(h, src_height),
                ResolutionOpt::Auto => None,
            };
            (
                VideoSpec {
                    codec: opts.video_codec,
                    bitrate_bps: None,
                    crf: Some(crf),
                    height,
                    fps: pick_fps(opts.fps, info.fps),
                    preset: opts.quality,
                },
                None,
            )
        };

        let two_pass = video.bitrate_bps.is_some();
        let summary = build_summary(&video, audio.as_ref(), two_pass);

        Ok(EncodePlan {
            input: info.path.clone(),
            output,
            summary,
            expected_bytes,
            target_bytes: target,
            source_duration_sec: duration,
            spec: EncodeSpec {
                video,
                audio,
                faststart: true,
                two_pass,
                passthrough: false,
                audio_only: false,
            },
        })
    }

    fn run(&self, plan: &EncodePlan) -> Result<Outcome, EngineError> {
        self.run_with_progress(plan, &mut |_, _| {})
    }
}

/// A placeholder video spec — ignored while `passthrough`/`audio_only` is set.
fn placeholder_video_spec() -> VideoSpec {
    VideoSpec {
        codec: crate::options::VideoCodec::H264,
        bitrate_bps: None,
        crf: None,
        height: None,
        fps: None,
        preset: crate::options::QualityPreset::Balanced,
    }
}

/// A stream-copy remux plan for when the source already fits the target.
/// `faststart` is only meaningful for MP4/MOV; pass `false` for pure audio.
fn passthrough_plan(info: &MediaInfo, output: PathBuf, target: u64, faststart: bool) -> EncodePlan {
    EncodePlan {
        input: info.path.clone(),
        output,
        summary: "stream copy (already within target)".to_string(),
        expected_bytes: Some(info.size_bytes),
        target_bytes: Some(target),
        source_duration_sec: info.duration_sec,
        spec: EncodeSpec {
            video: placeholder_video_spec(),
            audio: None,
            faststart,
            two_pass: false,
            passthrough: true,
            audio_only: false,
        },
    }
}

/// Human-readable summary for a pure-audio plan, e.g.
/// "Opus · 22 kbps · mono (speech)".
fn build_audio_summary(audio: &AudioSpec, src_channels: Option<u32>) -> String {
    let mut parts = vec![
        audio.codec.label().to_string(),
        format!("{} kbps", audio.bitrate_bps / 1000),
    ];
    if audio.mono {
        // A single-channel source (or --mono) reads as speech.
        let note = if src_channels == Some(1) {
            "mono"
        } else {
            "mono (downmix)"
        };
        parts.push(note.to_string());
    }
    if let Some(sr) = audio.sample_rate {
        parts.push(format!("{} Hz", sr));
    }
    parts.join(" · ")
}

/// Resolve the absolute target size (bytes) for a goal, if it imposes one.
fn target_bytes(goal: &SizeGoal, original: u64) -> Option<u64> {
    match goal {
        SizeGoal::Target(b) => Some(*b),
        SizeGoal::Reduce(f) => Some(budget::reduce_target_bytes(original, *f)),
        SizeGoal::Preset(p) => p.limit_bytes,
        SizeGoal::Quality => None,
    }
}

/// Decide the audio track for a video encode.
fn decide_audio(
    opts: &ShrinkOpts,
    has_audio: bool,
    target: Option<u64>,
    duration: f64,
) -> Result<Option<AudioSpec>, EngineError> {
    if !has_audio {
        return Ok(None);
    }
    match opts.audio {
        AudioChoice::Drop => Ok(None),
        AudioChoice::Bitrate(b) => Ok(Some(AudioSpec::cbr(AudioCodec::Aac, b))),
        AudioChoice::Keep => {
            let bps = match target {
                Some(tb) => budget::fit_audio_bps(tb, duration, AUDIO_LADDER)
                    .ok_or(EngineError::Infeasible)?,
                None => budget::DEFAULT_AUDIO_BPS,
            };
            Ok(Some(AudioSpec::cbr(AudioCodec::Aac, bps)))
        }
    }
}

/// Choose the encode height in auto/explicit mode.
fn pick_height(res: ResolutionOpt, src_height: u32, vbps: u64) -> Option<u32> {
    match res {
        ResolutionOpt::Height(h) => clamp_height(h, src_height),
        ResolutionOpt::Auto => {
            let chosen = budget::choose_height(src_height, vbps);
            if src_height > 0 && chosen < src_height {
                Some(chosen)
            } else {
                None
            }
        }
    }
}

/// Clamp an explicit height to the source (never upscale); `None` if it equals
/// the source (no scaling needed).
fn clamp_height(requested: u32, src_height: u32) -> Option<u32> {
    if src_height == 0 {
        return Some(requested);
    }
    let h = requested.min(src_height);
    if h == src_height {
        None
    } else {
        Some(h)
    }
}

/// Choose an fps cap; `None` if uncapped or the cap is ≥ the source rate.
fn pick_fps(fps: FpsOpt, src_fps: Option<f64>) -> Option<u32> {
    match fps {
        FpsOpt::Auto => None,
        FpsOpt::Cap(f) => match src_fps {
            Some(src) if (f as f64) >= src => None,
            _ => Some(f),
        },
    }
}

/// Default output path: `<stem>.shrink.<ext>` next to the input.
fn output_with_ext(input: &Path, ext: &str) -> PathBuf {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".to_string());
    let mut out = input.parent().map(Path::to_path_buf).unwrap_or_default();
    out.push(format!("{stem}.shrink.{ext}"));
    out
}

fn build_summary(video: &VideoSpec, audio: Option<&AudioSpec>, two_pass: bool) -> String {
    let mut parts = vec![video.codec.label().to_string()];
    match (video.bitrate_bps, video.crf) {
        (Some(bps), _) => parts.push(format!("{} kbps video", bps / 1000)),
        (_, Some(crf)) => parts.push(format!("CRF {crf}")),
        _ => {}
    }
    if let Some(a) = audio {
        parts.push(format!("{} kbps audio", a.bitrate_bps / 1000));
    } else {
        parts.push("no audio".to_string());
    }
    if let Some(h) = video.height {
        parts.push(format!("{h}p"));
    }
    if let Some(f) = video.fps {
        parts.push(format!("{f} fps"));
    }
    parts.push(if two_pass { "two-pass" } else { "CRF" }.to_string());
    parts.join(" · ")
}

/// Base path for ffmpeg's two-pass log, unique per process + input stem.
fn passlog_base(plan: &EncodePlan) -> String {
    let stem = plan
        .input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "ds".to_string());
    let dir = std::env::temp_dir();
    dir.join(format!("deepshrink-{}-{}", std::process::id(), stem))
        .to_string_lossy()
        .into_owned()
}

/// Remove the files ffmpeg leaves behind for `-passlogfile <base>`.
fn cleanup_passlog(base: &str) {
    for suffix in ["-0.log", "-0.log.mbtree"] {
        let _ = fs::remove_file(format!("{base}{suffix}"));
    }
}

/// Platform null sink for the discard output of pass 1.
fn null_sink() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

/// Build the ffmpeg argv for one pass. Video-processing options (codec, filters,
/// bitrate) are shared across passes; audio/output differ per pass.
fn build_pass_args(plan: &EncodePlan, pass: PassKind, passlog: &str) -> Vec<OsString> {
    let s = &plan.spec;
    let mut a: Vec<OsString> = Vec::new();
    // Local helper — a macro (not a closure) so it doesn't hold a borrow of `a`
    // across the direct `a.push(..)` calls used for OsString paths.
    macro_rules! push {
        ($arg:expr) => {
            a.push(OsString::from($arg))
        };
    }

    push!("-hide_banner");
    push!("-y");
    push!("-loglevel");
    push!("error");
    push!("-progress");
    push!("pipe:1");
    push!("-nostats");
    push!("-i");
    a.push(plan.input.clone().into_os_string());

    // Passthrough: stream copy, no re-encode. Output only (single pass).
    if s.passthrough {
        push!("-c");
        push!("copy");
        if s.faststart {
            push!("-movflags");
            push!("+faststart");
        }
        a.push(plan.output.clone().into_os_string());
        return a;
    }

    // Pure audio: drop video, encode the audio track only (single pass).
    if s.audio_only {
        push!("-vn");
        if let Some(au) = &s.audio {
            push!("-c:a");
            push!(au.codec.encoder());
            if au.mono {
                push!("-ac");
                push!("1");
            }
            if let Some(sr) = au.sample_rate {
                push!("-ar");
                push!(sr.to_string());
            }
            push!("-b:a");
            push!(au.bitrate_bps.to_string());
            // Opus supports VBR; use constrained VBR by default for a tighter
            // fit to the target, or full VBR when requested.
            if matches!(au.codec, AudioCodec::Opus) {
                push!("-vbr");
                push!(if au.vbr { "on" } else { "constrained" });
            }
        }
        a.push(plan.output.clone().into_os_string());
        return a;
    }

    // Video codec + filters.
    push!("-c:v");
    push!(s.video.codec.encoder());
    if let Some(h) = s.video.height {
        push!("-vf");
        push!(format!("scale=-2:{h}"));
    }
    if let Some(f) = s.video.fps {
        push!("-r");
        push!(f.to_string());
    }
    push!("-preset");
    push!(s.video.preset.encoder_preset());
    if let Some(tag) = s.video.codec.mp4_tag() {
        push!("-tag:v");
        push!(tag);
    }

    // Rate control.
    match (s.video.bitrate_bps, s.video.crf) {
        (Some(bps), _) => {
            push!("-b:v");
            push!(bps.to_string());
            if s.two_pass {
                push!("-pass");
                push!(match pass {
                    PassKind::First => "1",
                    _ => "2",
                });
                push!("-passlogfile");
                push!(passlog);
            }
        }
        (_, Some(crf)) => {
            push!("-crf");
            push!(crf.to_string());
        }
        _ => {}
    }

    // Audio + output.
    match pass {
        PassKind::First => {
            // Analysis pass: no audio, discard the muxed output.
            push!("-an");
            push!("-f");
            push!("null");
            push!(null_sink());
        }
        PassKind::Second | PassKind::Single => {
            match &s.audio {
                Some(au) => {
                    push!("-c:a");
                    push!(au.codec.encoder());
                    push!("-b:a");
                    push!(au.bitrate_bps.to_string());
                }
                None => push!("-an"),
            }
            if s.faststart {
                push!("-movflags");
                push!("+faststart");
            }
            a.push(plan.output.clone().into_os_string());
        }
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::{AudioCodec, QualityPreset, VideoCodec};
    use crate::size::preset;

    fn video_info(duration: f64, size: u64, w: u32, h: u32, audio: bool) -> MediaInfo {
        MediaInfo {
            path: PathBuf::from("/tmp/clip.mp4"),
            kind: MediaKind::Video,
            duration_sec: duration,
            size_bytes: size,
            width: Some(w),
            height: Some(h),
            fps: Some(30.0),
            video_codec: Some("h264".into()),
            audio_codec: if audio { Some("aac".into()) } else { None },
            audio_channels: if audio { Some(2) } else { None },
        }
    }

    fn opts_target(bytes: u64) -> ShrinkOpts {
        ShrinkOpts {
            goal: SizeGoal::Target(bytes),
            ..Default::default()
        }
    }

    #[test]
    fn supports_video_and_audio() {
        let e = MediaEngine::new();
        assert!(e.supports(&PathBuf::from("clip.mp4")));
        assert!(e.supports(&PathBuf::from("lecture.wav")));
        assert!(!e.supports(&PathBuf::from("photo.jpg")));
    }

    #[test]
    fn plan_target_builds_two_pass_with_budget() {
        let info = video_info(120.0, 300_000_000, 1920, 1080, true);
        let plan = MediaEngine::new()
            .plan(&info, &opts_target(8_000_000))
            .unwrap();

        assert!(plan.spec.two_pass);
        assert_eq!(plan.target_bytes, Some(8_000_000));
        assert_eq!(plan.output, PathBuf::from("/tmp/clip.shrink.mp4"));
        let vbps = plan.spec.video.bitrate_bps.unwrap();
        assert!(vbps >= budget::ABSOLUTE_MIN_VIDEO_BPS);
        // 8 MB over 120 s is a low budget → downscale from 1080p.
        assert!(plan.spec.video.height.is_some());
        assert!(plan.spec.audio.is_some());
        // Predicted size should not exceed the target.
        assert!(plan.expected_bytes.unwrap() <= 8_000_000 + 8_000_000 / 20);
    }

    #[test]
    fn plan_preset_discord_sets_target() {
        let info = video_info(30.0, 50_000_000, 1280, 720, true);
        let opts = ShrinkOpts {
            goal: SizeGoal::Preset(preset("discord").unwrap()),
            ..Default::default()
        };
        let plan = MediaEngine::new().plan(&info, &opts).unwrap();
        assert_eq!(plan.target_bytes, Some(8_000_000));
    }

    #[test]
    fn plan_reduce_targets_complement_of_original() {
        let info = video_info(60.0, 100_000_000, 1920, 1080, true);
        let opts = ShrinkOpts {
            goal: SizeGoal::Reduce(0.70),
            ..Default::default()
        };
        let plan = MediaEngine::new().plan(&info, &opts).unwrap();
        assert_eq!(plan.target_bytes, Some(30_000_000));
    }

    #[test]
    fn plan_passthrough_when_source_already_fits() {
        // Source is 200 KB, target 1 MB → never inflate; stream-copy remux.
        let info = video_info(10.0, 200_000, 1280, 720, true);
        let plan = MediaEngine::new()
            .plan(&info, &opts_target(1_000_000))
            .unwrap();
        assert!(plan.spec.passthrough);
        assert!(!plan.spec.two_pass);
        assert_eq!(plan.expected_bytes, Some(200_000));
        let args = build_pass_args(&plan, PassKind::Single, "/tmp/passlog");
        let joined: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(joined.contains(&"copy".to_string()));
    }

    #[test]
    fn plan_infeasible_when_target_too_small() {
        let info = video_info(600.0, 500_000_000, 1920, 1080, true);
        let err = MediaEngine::new().plan(&info, &opts_target(50_000));
        assert!(matches!(err, Err(EngineError::Infeasible)));
    }

    #[test]
    fn plan_quality_mode_uses_crf_single_pass() {
        let info = video_info(60.0, 100_000_000, 1920, 1080, true);
        let opts = ShrinkOpts {
            goal: SizeGoal::Quality,
            quality: QualityPreset::Balanced,
            ..Default::default()
        };
        let plan = MediaEngine::new().plan(&info, &opts).unwrap();
        assert!(!plan.spec.two_pass);
        assert_eq!(plan.spec.video.crf, Some(23));
        assert!(plan.spec.video.bitrate_bps.is_none());
        assert!(plan.expected_bytes.is_none());
    }

    #[test]
    fn plan_drops_audio_when_requested() {
        let info = video_info(30.0, 50_000_000, 1280, 720, true);
        let opts = ShrinkOpts {
            audio: AudioChoice::Drop,
            ..opts_target(8_000_000)
        };
        let plan = MediaEngine::new().plan(&info, &opts).unwrap();
        assert!(plan.spec.audio.is_none());
    }

    fn audio_info(duration: f64, size: u64, channels: u32) -> MediaInfo {
        MediaInfo {
            path: PathBuf::from("/tmp/lecture.wav"),
            kind: MediaKind::Audio,
            duration_sec: duration,
            size_bytes: size,
            width: None,
            height: None,
            fps: None,
            video_codec: None,
            audio_codec: Some("pcm_s16le".into()),
            audio_channels: Some(channels),
        }
    }

    #[test]
    fn plan_audio_single_pass_with_fitted_bitrate() {
        // 58 min stereo lecture, target 10 MB.
        let info = audio_info(3480.0, 600_000_000, 2);
        let plan = MediaEngine::new()
            .plan(&info, &opts_target(10_000_000))
            .unwrap();
        assert!(plan.spec.audio_only);
        assert!(!plan.spec.two_pass);
        assert_eq!(plan.output, PathBuf::from("/tmp/lecture.shrink.m4a"));
        let au = plan.spec.audio.as_ref().unwrap();
        // Snapped down to a standard step, never above the raw budget.
        assert!(budget::AUDIO_STEPS.contains(&au.bitrate_bps));
        assert!(plan.expected_bytes.unwrap() <= 10_000_000 + 10_000_000 / 20);
    }

    #[test]
    fn plan_audio_mono_source_marked_speech() {
        let info = audio_info(600.0, 100_000_000, 1);
        let plan = MediaEngine::new()
            .plan(&info, &opts_target(5_000_000))
            .unwrap();
        assert!(plan.spec.audio.as_ref().unwrap().mono);
    }

    #[test]
    fn plan_audio_opus_extension_and_vbr_args() {
        let info = audio_info(600.0, 100_000_000, 2);
        let opts = ShrinkOpts {
            audio_codec: AudioCodec::Opus,
            mono: true,
            ..opts_target(3_000_000)
        };
        let plan = MediaEngine::new().plan(&info, &opts).unwrap();
        assert_eq!(plan.output, PathBuf::from("/tmp/lecture.shrink.opus"));
        let args = build_pass_args(&plan, PassKind::Single, "/tmp/passlog");
        let j: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(j.contains(&"-vn".to_string()));
        assert!(j.contains(&"libopus".to_string()));
        assert!(j.contains(&"-ac".to_string())); // mono downmix
        assert!(j.contains(&"-vbr".to_string()));
    }

    #[test]
    fn plan_audio_infeasible_when_target_tiny() {
        let info = audio_info(3600.0, 500_000_000, 2);
        assert!(matches!(
            MediaEngine::new().plan(&info, &opts_target(1_000)),
            Err(EngineError::Infeasible)
        ));
    }

    #[test]
    fn plan_audio_passthrough_when_source_fits() {
        let info = audio_info(600.0, 2_000_000, 2);
        let plan = MediaEngine::new()
            .plan(&info, &opts_target(10_000_000))
            .unwrap();
        assert!(plan.spec.passthrough);
        // Passthrough keeps the source container/extension.
        assert_eq!(plan.output, PathBuf::from("/tmp/lecture.shrink.wav"));
    }

    #[test]
    fn pass1_args_have_no_audio_and_null_sink() {
        let info = video_info(120.0, 300_000_000, 1920, 1080, true);
        let plan = MediaEngine::new()
            .plan(&info, &opts_target(8_000_000))
            .unwrap();
        let args = build_pass_args(&plan, PassKind::First, "/tmp/passlog");
        let joined: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(joined.contains(&"-an".to_string()));
        assert!(joined.contains(&"null".to_string()));
        assert!(joined.iter().any(|a| a == "1")); // -pass 1
        assert!(!joined.iter().any(|a| a.contains("shrink.mp4")));
    }

    #[test]
    fn pass2_args_write_output_with_audio() {
        let info = video_info(120.0, 300_000_000, 1920, 1080, true);
        let plan = MediaEngine::new()
            .plan(&info, &opts_target(8_000_000))
            .unwrap();
        let args = build_pass_args(&plan, PassKind::Second, "/tmp/passlog");
        let joined: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(joined.iter().any(|a| a.contains("clip.shrink.mp4")));
        assert!(joined.contains(&"-c:a".to_string()));
        assert!(joined.contains(&"+faststart".to_string()));
        assert!(joined.iter().any(|a| a == "2")); // -pass 2
    }

    #[test]
    fn h265_adds_hvc1_tag() {
        let info = video_info(60.0, 100_000_000, 1280, 720, false);
        let opts = ShrinkOpts {
            video_codec: VideoCodec::H265,
            ..opts_target(8_000_000)
        };
        let plan = MediaEngine::new().plan(&info, &opts).unwrap();
        let args = build_pass_args(&plan, PassKind::Second, "/tmp/passlog");
        let joined: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(joined.contains(&"hvc1".to_string()));
        assert!(joined.contains(&"libx265".to_string()));
    }
}
