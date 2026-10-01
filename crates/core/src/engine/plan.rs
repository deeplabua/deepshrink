//! The pure half of the media engine: planning an encode (video bitrate under
//! a size target, quality mode, resolution / fps caps, the audio decision,
//! output container, metadata tags) and the small calculations the run loop
//! uses (the size-target ceiling, overshoot corrections). No ffmpeg — this is
//! what builds without the `ffmpeg` feature (e.g. for iOS).

use std::path::{Path, PathBuf};

use super::{
    AudioSpec, EncodePlan, EncodeSpec, EngineError, MediaInfo, ShrinkOpts, SizeGoal, VideoSpec,
};
use crate::budget;
use crate::detect::MediaKind;
use crate::options::{AudioChoice, AudioCodec, FpsOpt, QualityPreset, ResolutionOpt, VideoCodec};

/// Plan an encode — the pure half of [`super::Engine::plan`], with no ffmpeg:
/// `hw_available` says whether Apple's hardware encoder may be used (the
/// media engine asks ffmpeg; another platform asks its own encoder).
pub fn plan(
    info: &MediaInfo,
    opts: &ShrinkOpts,
    hw_available: bool,
) -> Result<EncodePlan, EngineError> {
    match info.kind {
        MediaKind::Audio => return plan_audio(info, opts),
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
    // Resolution caps apply to the short side (portrait video included).
    let (w, h) = (info.width.unwrap_or(0), info.height.unwrap_or(0));
    let portrait = h > w;
    let src_height = w.min(h);

    let target = target_bytes(&opts.goal, info.size_bytes);
    let output = opts
        .output
        .clone()
        .unwrap_or_else(|| output_with_ext(&info.path, video_container(info, target, opts)));

    // "Never make it bigger": if the source already fits the target, just
    // remux (stream copy) instead of re-encoding it up to the target. The
    // copy stays in the *source* container — an .mp4 cannot hold every codec
    // a source may carry (an AMR-NB track from a .3gp, say), and a stream
    // copy must not be the thing that breaks a file we aren't even re-encoding.
    if let Some(tb) = target {
        if info.size_bytes > 0 && info.size_bytes <= tb {
            let src_ext = info
                .path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("mp4");
            let output = opts
                .output
                .clone()
                .unwrap_or_else(|| output_with_ext(&info.path, src_ext));
            return Ok(passthrough_plan(info, output, tb, true, opts.keep_metadata));
        }
    }

    let audio = decide_audio(
        opts,
        info.has_audio(),
        info.audio_bitrate_bps,
        target,
        duration,
    )?;
    let audio_bps = audio.as_ref().map(|a| a.bitrate_bps).unwrap_or(0);

    // Apple's hardware encoder, when asked for and present. Not for a VMAF
    // search (its CRF bounds are the software encoder's).
    let hw_quality = opts
        .quality
        .default_hw_quality(opts.video_codec)
        .filter(|_| opts.hardware && opts.target_vmaf.is_none() && hw_available);
    let hardware = hw_quality.is_some();
    // The quality value for this encoder: CRF, or VideoToolbox's `-q:v`.
    let quality_value = hw_quality.unwrap_or_else(|| opts.quality.default_crf(opts.video_codec));

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
                // A size target is for sending: make it play everywhere.
                to_sdr: info.hdr,
                hardware,
                portrait,
            },
            Some(predicted),
        )
    } else {
        // Quality mode: CRF, no hard size guarantee. The CRF default is
        // codec-aware; a `--vmaf` target refines it via a search in `run`.
        let crf = quality_value;
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
                // Quality mode keeps HDR (and 10-bit) as shot — except
                // Apple's H.264, which is 8-bit only: SDR it is.
                to_sdr: info
                    .hdr
                    .filter(|_| hardware && opts.video_codec == VideoCodec::H264),
                hardware,
                portrait,
            },
            None,
        )
    };

    // Two-pass is how a bitrate budget is actually hit; the caller can force
    // it off (faster, looser) but can't force it on in CRF mode, where there
    // is no budget for a first pass to measure.
    // Apple's encoder has no two-pass: it hits a budget in one (with the
    // overshoot retry in `run`).
    let two_pass = video.bitrate_bps.is_some() && opts.two_pass.unwrap_or(true) && !hardware;
    let summary = build_summary(&video, audio.as_ref(), two_pass);

    Ok(EncodePlan {
        input: info.path.clone(),
        output,
        summary,
        expected_bytes,
        target_bytes: target,
        target_vmaf: opts.target_vmaf,
        source_duration_sec: duration,
        source_width: info.width,
        source_height: info.height,
        source_fps: info.fps,
        spec: EncodeSpec {
            video,
            audio,
            faststart: true,
            two_pass,
            passthrough: false,
            audio_only: false,
            dpi: None,
            keep_metadata: opts.keep_metadata,
            tags: capture_tags(info, opts.keep_metadata),
        },
        // A size target is its own guarantee; quality mode gets the guard.
        guard_larger: target.is_none() && !opts.allow_larger,
        ceiling_crf: target.map(|_| quality_value),
    })
}

/// Plan a pure-audio encode (single pass, codec + fitted bitrate).
pub(crate) fn plan_audio(info: &MediaInfo, opts: &ShrinkOpts) -> Result<EncodePlan, EngineError> {
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
            return Ok(passthrough_plan(
                info,
                output,
                tb,
                false,
                opts.keep_metadata,
            ));
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
            let predicted =
                (bps as f64 * duration / 8.0 * (1.0 + budget::CONTAINER_OVERHEAD)).round() as u64;
            (bps, Some(predicted))
        }
        None => {
            // Quality mode: per tier, codec and channel count.
            let bps = quality_audio_bps(opts.quality, codec, mono);
            // Never re-encode lossy audio at (nearly) its own bitrate or
            // above: that is only generation loss, often a bigger file (a
            // 64 kbps MP3 audiobook → 160 kbps AAC doubled it). Keep it.
            if !opts.allow_larger && already_compact_audio(bps, info) {
                let src_ext = info
                    .path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("audio");
                let output = opts
                    .output
                    .clone()
                    .unwrap_or_else(|| output_with_ext(&info.path, src_ext));
                let mut plan =
                    passthrough_plan(info, output, info.size_bytes, false, opts.keep_metadata);
                plan.summary =
                    "stream copy (already compact — a re-encode would not be smaller)".into();
                return Ok(plan);
            }
            // Constant-bitrate estimate (VBR lands close enough for a preview).
            let predicted =
                (bps as f64 * duration / 8.0 * (1.0 + budget::CONTAINER_OVERHEAD)).round() as u64;
            (bps, Some(predicted))
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
        target_vmaf: None,
        source_duration_sec: duration,
        source_width: info.width,
        source_height: info.height,
        source_fps: info.fps,
        spec: EncodeSpec {
            video: placeholder_video_spec(),
            audio: Some(audio),
            faststart: false,
            two_pass: false,
            passthrough: false,
            audio_only: true,
            dpi: None,
            keep_metadata: opts.keep_metadata,
            tags: capture_tags(info, opts.keep_metadata),
        },
        // A size target is its own guarantee; quality mode gets the guard.
        guard_larger: target.is_none() && !opts.allow_larger,
        ceiling_crf: None,
    })
}

/// Audio bitrate ladder (bits/s, descending) tried when keeping a track under
/// a tight size budget.
pub(crate) const AUDIO_LADDER: &[u64] = &[128_000, 96_000, 64_000, 48_000];

/// A placeholder video spec — ignored while `passthrough`/`audio_only` is set.
pub(crate) fn placeholder_video_spec() -> VideoSpec {
    VideoSpec {
        codec: crate::options::VideoCodec::H264,
        bitrate_bps: None,
        crf: None,
        height: None,
        fps: None,
        preset: crate::options::QualityPreset::Balanced,
        to_sdr: None,
        hardware: false,
        portrait: false,
    }
}

/// A stream-copy remux plan for when the source already fits the target.
/// `faststart` is only meaningful for MP4/MOV; pass `false` for pure audio.
pub(crate) fn passthrough_plan(
    info: &MediaInfo,
    output: PathBuf,
    target: u64,
    faststart: bool,
    keep_metadata: bool,
) -> EncodePlan {
    EncodePlan {
        input: info.path.clone(),
        output,
        summary: "stream copy (already within target)".to_string(),
        expected_bytes: Some(info.size_bytes),
        target_bytes: Some(target),
        target_vmaf: None,
        source_duration_sec: info.duration_sec,
        source_width: info.width,
        source_height: info.height,
        source_fps: info.fps,
        spec: EncodeSpec {
            video: placeholder_video_spec(),
            audio: None,
            faststart,
            two_pass: false,
            passthrough: true,
            audio_only: false,
            dpi: None,
            keep_metadata,
            tags: capture_tags(info, keep_metadata),
        },
        guard_larger: false,
        ceiling_crf: None,
    }
}

/// Quality-mode audio bitrate by tier, codec and channel count (mono = half).
/// Opus needs the least for the same quality, MP3 the most.
pub(crate) fn quality_audio_bps(quality: QualityPreset, codec: AudioCodec, mono: bool) -> u64 {
    let stereo = match (codec, quality) {
        (AudioCodec::Opus, QualityPreset::Fast) => 64_000,
        (AudioCodec::Opus, QualityPreset::Balanced) => 96_000,
        (AudioCodec::Opus, QualityPreset::Max) => 128_000,
        (AudioCodec::Mp3, QualityPreset::Fast) => 128_000,
        (AudioCodec::Mp3, QualityPreset::Balanced) => 160_000,
        (AudioCodec::Mp3, QualityPreset::Max) => 256_000,
        (AudioCodec::Aac, QualityPreset::Fast) => 96_000,
        (AudioCodec::Aac, QualityPreset::Balanced) => 128_000,
        (AudioCodec::Aac, QualityPreset::Max) => 192_000,
    };
    if mono {
        stereo / 2
    } else {
        stereo
    }
}

/// A pure-audio source whose own bitrate is at or under ~110% of what we'd
/// encode at: a re-encode can't meaningfully shrink it. The source rate comes
/// from size / duration (embedded cover art only raises it — the safe side).
pub(crate) fn already_compact_audio(bps: u64, info: &MediaInfo) -> bool {
    if info.duration_sec <= 0.0 || info.size_bytes == 0 {
        return false;
    }
    let source_bps = info.size_bytes as f64 * 8.0 / info.duration_sec;
    bps as f64 >= source_bps * 0.9
}

/// Output container for a video. A QuickTime source (an iPhone `.MOV`) stays
/// QuickTime in quality mode: only a MOV carries its location / camera tags in
/// a form Apple's apps read (the MP4 muxer drops them). Size targets and
/// platform presets get MP4 — the most compatible for sharing. AV1 is always
/// MP4 (QuickTime has no AV1 mapping).
pub(crate) fn video_container(
    info: &MediaInfo,
    target: Option<u64>,
    opts: &ShrinkOpts,
) -> &'static str {
    let mov_source = info
        .path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("mov"));
    if mov_source && target.is_none() && opts.video_codec != VideoCodec::Av1 {
        "mov"
    } else {
        "mp4"
    }
}

/// The explicit output tags for `info`'s capture metadata (empty when
/// metadata is stripped).
pub(crate) fn capture_tags(info: &MediaInfo, keep: bool) -> Vec<(String, String)> {
    if !keep {
        return Vec::new();
    }
    let c = &info.capture;
    [
        ("creation_time", c.created_utc.as_ref()),
        ("date", c.created_local.as_ref()),
        ("location", c.location.as_ref()),
        ("make", c.make.as_ref()),
        ("model", c.model.as_ref()),
    ]
    .into_iter()
    .filter_map(|(k, v)| v.map(|v| (k.to_string(), v.clone())))
    .collect()
}

/// `2026-09-26T20:01:54+0300` (also `+03:00`, `Z`, fractional seconds) →
/// `2026-09-26T17:01:54Z`. `None` if it doesn't parse.
pub fn to_utc(s: &str) -> Option<String> {
    let s = s.trim();
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, se) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if s.get(4..5)? != "-" || s.get(10..11).map(|c| c == "T" || c == " ") != Some(true) {
        return None;
    }
    // Offset: skip any fraction, then Z / ±HH[:]MM.
    let rest = s
        .get(19..)?
        .trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    let offset_min = match rest {
        "" | "Z" | "z" => 0,
        r if r.starts_with('+') || r.starts_with('-') => {
            let digits: String = r[1..].chars().filter(char::is_ascii_digit).collect();
            let (oh, om) = (
                digits.get(0..2)?.parse::<i64>().ok()?,
                digits.get(2..4).unwrap_or("00").parse::<i64>().ok()?,
            );
            let m = oh * 60 + om;
            if r.starts_with('-') {
                -m
            } else {
                m
            }
        }
        _ => return None,
    };
    let secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se - offset_min * 60;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, mo, d) = civil_from_days(days);
    Some(format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    ))
}

/// Days since 1970-01-01 for a proleptic Gregorian date (H. Hinnant).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`].
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// How far under the target a predicted CRF encode must land to be used
/// instead of the budget (predictions are within ~5%).
pub const CEILING_MARGIN: f64 = 0.9;

/// A size-target plan re-cast as a single-pass CRF encode at the quality
/// preset's CRF ([`EncodePlan::ceiling_crf`]). `None` for anything else.
pub fn ceiling_plan(plan: &EncodePlan) -> Option<EncodePlan> {
    let crf = plan.ceiling_crf?;
    if plan.target_bytes.is_none()
        || plan.spec.passthrough
        || plan.spec.audio_only
        || plan.spec.video.bitrate_bps.is_none()
    {
        return None;
    }
    let mut c = plan.clone();
    c.spec.video.bitrate_bps = None;
    c.spec.video.crf = Some(crf);
    c.spec.two_pass = false;
    c.summary = build_summary(&c.spec.video, c.spec.audio.as_ref(), false);
    Some(c)
}

/// Human-readable summary for a pure-audio plan, e.g.
/// "Opus · 22 kbps · mono (speech)".
pub(crate) fn build_audio_summary(audio: &AudioSpec, src_channels: Option<u32>) -> String {
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
pub(crate) fn target_bytes(goal: &SizeGoal, original: u64) -> Option<u64> {
    match goal {
        SizeGoal::Target(b) => Some(*b),
        SizeGoal::Reduce(f) => Some(budget::reduce_target_bytes(original, *f)),
        SizeGoal::Preset(p) => p.limit_bytes,
        SizeGoal::Quality => None,
    }
}

/// Decide the audio track for a video encode.
pub(crate) fn decide_audio(
    opts: &ShrinkOpts,
    has_audio: bool,
    source_bps: Option<u64>,
    target: Option<u64>,
    duration: f64,
) -> Result<Option<AudioSpec>, EngineError> {
    if !has_audio {
        return Ok(None);
    }
    // A `--mono` request downmixes the kept audio track (speech clips / smaller
    // files). A single-channel source stays mono regardless.
    let mono = opts.mono;
    match opts.audio {
        AudioChoice::Drop => Ok(None),
        AudioChoice::Bitrate(b) => Ok(Some(AudioSpec {
            mono,
            ..AudioSpec::cbr(AudioCodec::Aac, b)
        })),
        AudioChoice::Keep => {
            let bps = match target {
                Some(tb) => budget::fit_audio_bps(tb, duration, AUDIO_LADDER)
                    .ok_or(EngineError::Infeasible)?,
                None => budget::DEFAULT_AUDIO_BPS,
            };
            // Never re-encode the track above its own bitrate: that only adds
            // bytes (a 64 kbps phone recording doesn't need 128 kbps AAC). Any
            // budget saved here goes to the video.
            let bps = match source_bps {
                Some(src) => bps.min(src.max(MIN_TRACK_BPS)),
                None => bps,
            };
            Ok(Some(AudioSpec {
                mono,
                ..AudioSpec::cbr(AudioCodec::Aac, bps)
            }))
        }
    }
}

/// Floor for a capped audio track (a mis-reported tiny source rate must not
/// starve the audio).
pub(crate) const MIN_TRACK_BPS: u64 = 32_000;

/// Choose the encode height in auto/explicit mode.
pub(crate) fn pick_height(res: ResolutionOpt, src_height: u32, vbps: u64) -> Option<u32> {
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
pub(crate) fn clamp_height(requested: u32, src_height: u32) -> Option<u32> {
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
pub(crate) fn pick_fps(fps: FpsOpt, src_fps: Option<f64>) -> Option<u32> {
    match fps {
        FpsOpt::Auto => None,
        FpsOpt::Cap(f) => match src_fps {
            Some(src) if (f as f64) >= src => None,
            _ => Some(f),
        },
    }
}

/// Default output path: `<stem>.shrink.<ext>` next to the input.
pub(crate) fn output_with_ext(input: &Path, ext: &str) -> PathBuf {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".to_string());
    let mut out = input.parent().map(Path::to_path_buf).unwrap_or_default();
    out.push(format!("{stem}.shrink.{ext}"));
    out
}

pub(crate) fn build_summary(
    video: &VideoSpec,
    audio: Option<&AudioSpec>,
    two_pass: bool,
) -> String {
    let mut parts = vec![if video.hardware {
        format!("{} (Apple hardware)", video.codec.label())
    } else {
        video.codec.label().to_string()
    }];
    match (video.bitrate_bps, video.crf) {
        (Some(bps), _) => parts.push(format!("up to {} kbps video", bps / 1000)),
        (_, Some(q)) if video.hardware => parts.push(format!("quality {q}")),
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
    if video.to_sdr.is_some() {
        parts.push("HDR → SDR".to_string());
    }
    parts.push(
        if two_pass {
            "two-pass"
        } else if video.hardware {
            "one pass"
        } else {
            "CRF"
        }
        .to_string(),
    );
    parts.join(" · ")
}

/// The video bitrate for a re-run after an encode of `size` bytes overshot
/// `target`: scaled down in proportion, with 3 % headroom. `None` below the
/// encoder's floor (no point re-running).
pub fn corrected_bitrate(vbps: u64, target: u64, size: u64) -> Option<u64> {
    let corrected = (vbps as f64 * (target as f64 / size as f64) * 0.97) as u64;
    (corrected >= budget::ABSOLUTE_MIN_VIDEO_BPS).then_some(corrected)
}

/// Sample window length for predicting a CRF encode (see [`sample_windows`]).
pub const SAMPLE_SECS: f64 = 3.0;
/// The shortest window for heavy video (4K, 60 fps): measured on a 60 s 4K60
/// iPhone clip, 1.5 s windows predicted as well as 3 s (+3.3 % vs +3.8 %) in
/// half the time; 1 s drifted to +7 %.
pub const MIN_SAMPLE_SECS: f64 = 1.5;

/// Sample window length: 3 s up to 1080p30, shorter as the pixel rate grows
/// (4K60 → 1.5 s), so a preview of heavy video doesn't take a minute.
pub fn sample_secs(plan: &EncodePlan) -> f64 {
    const REFERENCE: f64 = 1920.0 * 1080.0 * 30.0;
    let (w, h) = match (plan.source_width, plan.source_height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => (w as f64, h as f64),
        _ => return SAMPLE_SECS,
    };
    let fps = plan
        .source_fps
        .filter(|f| f.is_finite() && *f > 0.0)
        .unwrap_or(30.0);
    (SAMPLE_SECS * REFERENCE / (w * h * fps)).clamp(MIN_SAMPLE_SECS, SAMPLE_SECS)
}
/// Each sample starts on a keyframe, so samples over-predict by ~8–10% (a
/// 30 s phone clip: 20.4 MB predicted vs 18.7 MB real) — scale that back.
pub const SAMPLE_BIAS: f64 = 0.92;

/// The windows `(start, length)` in seconds to sample-encode for predicting a
/// quality-mode (CRF) encode's size, and the bias to apply to their bit rate.
/// Long clips: three windows at 20/50/80 %; short ones (under four windows):
/// the whole clip once — exact, so no keyframe bias to correct.
pub fn sample_windows(plan: &EncodePlan) -> (Vec<(f64, f64)>, f64) {
    let duration = plan.source_duration_sec;
    let win = sample_secs(plan);
    if duration >= win * 4.0 {
        (
            [0.2, 0.5, 0.8]
                .iter()
                .map(|at| ((duration * at - win / 2.0).max(0.0), win))
                .collect(),
            SAMPLE_BIAS,
        )
    } else {
        (vec![(0.0, duration)], 1.0)
    }
}

/// The predicted final size of `plan` from its sample encodes: `video_bytes`
/// of video-only output over `sampled_secs`, scaled by `bias`, plus the planned
/// audio and container overhead.
pub fn predicted_bytes(plan: &EncodePlan, video_bytes: u64, sampled_secs: f64, bias: f64) -> u64 {
    let duration = plan.source_duration_sec;
    if sampled_secs <= 0.0 {
        return 0;
    }
    let video_bps = video_bytes as f64 * 8.0 / sampled_secs * bias;
    let audio_bps = plan.spec.audio.as_ref().map(|a| a.bitrate_bps).unwrap_or(0) as f64;
    ((video_bps + audio_bps) * duration / 8.0 * (1.0 + budget::CONTAINER_OVERHEAD)) as u64
}

#[cfg(test)]
mod tests {
    //! The planning logic on its own — no ffmpeg (runs with
    //! `--no-default-features`, as on iOS).
    use super::*;
    use crate::engine::CaptureMeta;

    fn video(w: u32, h: u32, secs: f64, size: u64) -> MediaInfo {
        MediaInfo {
            path: PathBuf::from("/tmp/clip.mp4"),
            kind: MediaKind::Video,
            duration_sec: secs,
            size_bytes: size,
            width: Some(w),
            height: Some(h),
            fps: Some(30.0),
            video_codec: Some("h264".into()),
            audio_codec: Some("aac".into()),
            audio_channels: Some(2),
            audio_bitrate_bps: Some(128_000),
            capture: CaptureMeta::default(),
            hdr: None,
        }
    }

    #[test]
    fn a_size_target_plans_a_bitrate_under_budget() {
        let opts = ShrinkOpts {
            goal: SizeGoal::Target(10_000_000),
            ..ShrinkOpts::default()
        };
        let plan = plan(&video(1920, 1080, 60.0, 200_000_000), &opts, false).unwrap();
        let vbps = plan.spec.video.bitrate_bps.unwrap();
        assert_eq!(
            vbps,
            budget::video_bitrate_bps(10_000_000, 60.0, 128_000).unwrap()
        );
        assert!(plan.spec.two_pass);
        assert!(plan.expected_bytes.unwrap() <= 10_000_000);
    }

    #[test]
    fn apple_hardware_quality_is_used_only_when_available() {
        let opts = ShrinkOpts {
            hardware: true,
            ..ShrinkOpts::default()
        };
        let info = video(1920, 1080, 60.0, 200_000_000);
        let hw = plan(&info, &opts, true).unwrap();
        assert!(hw.spec.video.hardware);
        assert_eq!(
            hw.spec.video.crf,
            QualityPreset::Balanced.default_hw_quality(VideoCodec::H264)
        );
        let sw = plan(&info, &opts, false).unwrap();
        assert!(!sw.spec.video.hardware);
        assert_eq!(
            sw.spec.video.crf,
            Some(QualityPreset::Balanced.default_crf(VideoCodec::H264))
        );
    }

    #[test]
    fn a_portrait_cap_is_on_the_short_side() {
        let opts = ShrinkOpts {
            resolution: ResolutionOpt::Height(1080),
            ..ShrinkOpts::default()
        };
        let p = plan(&video(2160, 3840, 30.0, 100_000_000), &opts, false).unwrap();
        assert_eq!(p.spec.video.height, Some(1080));
        assert!(p.spec.video.portrait);
    }

    #[test]
    fn an_overshoot_is_corrected_in_proportion_down_to_a_floor() {
        // 12 MB for a 10 MB target at 4 Mbit/s → ~3.23 Mbit/s.
        assert_eq!(
            corrected_bitrate(4_000_000, 10_000_000, 12_000_000),
            Some(3_233_333)
        );
        assert_eq!(corrected_bitrate(10_000, 1_000, 1_000_000), None);
        // The ceiling: the same plan, at the quality CRF instead of the budget.
        let opts = ShrinkOpts {
            goal: SizeGoal::Target(50_000_000),
            ..ShrinkOpts::default()
        };
        let p = plan(&video(1920, 1080, 60.0, 200_000_000), &opts, false).unwrap();
        let c = ceiling_plan(&p).unwrap();
        assert_eq!(c.spec.video.bitrate_bps, None);
        assert!(c.spec.video.crf.is_some() && !c.spec.two_pass);
    }

    #[test]
    fn long_clips_sample_three_windows_short_ones_the_whole_clip() {
        let opts = ShrinkOpts::default();
        let long = plan(&video(1920, 1080, 60.0, 200_000_000), &opts, false).unwrap();
        let (w, bias) = sample_windows(&long);
        assert_eq!(w.len(), 3);
        assert_eq!(w[1], (28.5, 3.0));
        assert_eq!(bias, SAMPLE_BIAS);
        // 1 MB of video over 9 s, 128 kbps audio, 60 s, 1 % overhead.
        let bytes = predicted_bytes(&long, 1_000_000, 9.0, 1.0);
        assert_eq!(
            bytes,
            ((8_000_000.0 / 9.0 + 128_000.0) * 60.0 / 8.0 * (1.0 + budget::CONTAINER_OVERHEAD))
                as u64
        );
        let short = plan(&video(1920, 1080, 8.0, 20_000_000), &opts, false).unwrap();
        assert_eq!(sample_windows(&short), (vec![(0.0, 8.0)], 1.0));
    }

    #[test]
    fn local_capture_time_converts_to_utc() {
        assert_eq!(
            to_utc("2026-09-26T20:01:54+0300").as_deref(),
            Some("2026-09-26T17:01:54Z")
        );
    }
}
