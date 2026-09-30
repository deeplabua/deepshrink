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
    AudioSpec, CaptureMeta, EncodePlan, EncodeSpec, Engine, EngineError, Hdr, MediaInfo, Outcome,
    ShrinkOpts, SizeGoal, VideoSpec,
};
use crate::budget;
use crate::detect::{detect_kind, MediaKind};
use crate::options::{AudioChoice, AudioCodec, FpsOpt, QualityPreset, ResolutionOpt, VideoCodec};

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
#[derive(Debug, Default, Clone)]
pub struct MediaEngine {
    /// Stops this engine's runs (see [`MediaEngine::with_cancel`]).
    cancel: Option<deepshrink_ffmpeg::CancelToken>,
}

impl MediaEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// An engine whose `run` / `estimate` stop when `cancel` is set: the running
    /// ffmpeg is killed, the partial output removed, and the call returns an
    /// error for which [`EngineError::is_cancelled`] is true.
    pub fn with_cancel(cancel: deepshrink_ffmpeg::CancelToken) -> Self {
        Self {
            cancel: Some(cancel),
        }
    }

    /// The located ffmpeg / ffprobe, carrying this engine's cancel token.
    fn tools(&self) -> Result<deepshrink_ffmpeg::Tools, EngineError> {
        let tools = deepshrink_ffmpeg::locate()?;
        Ok(match &self.cancel {
            Some(c) => tools.with_cancel(c.clone()),
            None => tools,
        })
    }

    /// Like [`Engine::run`] but reports progress: `on_progress(pass, fraction)`
    /// is called with `fraction` in 0.0..=1.0 as each pass proceeds.
    pub fn run_with_progress(
        &self,
        plan: &EncodePlan,
        on_progress: &mut dyn FnMut(PassKind, f64),
    ) -> Result<Outcome, EngineError> {
        let outcome = match self.run_inner(plan, on_progress) {
            Ok(o) => o,
            Err(e) => {
                // A stopped encode leaves nothing behind: no half-written file,
                // no two-pass log.
                if e.is_cancelled() && plan.output != plan.input {
                    let _ = fs::remove_file(&plan.output);
                    cleanup_passlog(&passlog_base(plan));
                }
                return Err(e);
            }
        };
        // Keep the source's modification time too, so the result sorts next to
        // the original (Finder, Photos imports) instead of "today".
        if plan.spec.keep_metadata {
            copy_mtime(&plan.input, &outcome.output);
        }
        Ok(outcome)
    }

    fn run_inner(
        &self,
        plan: &EncodePlan,
        on_progress: &mut dyn FnMut(PassKind, f64),
    ) -> Result<Outcome, EngineError> {
        let tools = self.tools()?;
        let encoder = resolve_encoder(&tools, plan)?;
        let zscale = wants_zscale(&tools, plan);

        // VMAF-targeted quality search: applies to CRF-mode video only. Size /
        // audio / passthrough encodes keep their existing single path.
        if let Some(target_vmaf) = plan.target_vmaf {
            if plan.spec.video.crf.is_some() && !plan.spec.audio_only && !plan.spec.passthrough {
                return self.run_crf_search(
                    &tools,
                    plan,
                    encoder,
                    zscale,
                    target_vmaf,
                    on_progress,
                );
            }
        }

        // "Never make it bigger" in quality mode (sizes are guaranteed by the
        // target path already): predict a CRF video from samples and skip the
        // encode when it won't save at least `MIN_SAVING`; after any guarded
        // encode, keep the original if the result doesn't after all.
        let source = if plan.guard_larger && !plan.spec.passthrough {
            fs::metadata(&plan.input).map(|m| m.len()).unwrap_or(0)
        } else {
            0
        };
        if source > 0
            && plan.target_vmaf.is_none()
            && !plan.spec.audio_only
            && plan.spec.video.crf.is_some()
        {
            if let Some(predicted) = predict_crf_bytes(&tools, plan, encoder, zscale) {
                if super::not_worth_it(predicted, source) {
                    return self.keep_original(&tools, plan, on_progress);
                }
            }
        }

        // A size target is a ceiling, not a quota: when the quality preset's
        // CRF comfortably fits, encode at it instead of filling the budget.
        let ceiling = match ceiling_fit(&tools, plan, encoder, zscale) {
            Some((ceiling, _)) => {
                let o = self.run_plain(&tools, &ceiling, encoder, zscale, on_progress)?;
                // Predictions are ±5%; a miss falls back to the budgeted encode.
                plan.target_bytes
                    .is_some_and(|t| o.final_bytes <= t)
                    .then_some(o)
            }
            None => None,
        };
        let mut outcome = match ceiling {
            Some(o) => o,
            None => self.run_plain(&tools, plan, encoder, zscale, on_progress)?,
        };
        if source > 0 && super::not_worth_it(outcome.final_bytes, source) {
            let _ = fs::remove_file(&outcome.output);
            return self.keep_original(&tools, plan, on_progress);
        }

        // Size-targeted video with `--vmaf`: encode to budget, then report the
        // VMAF actually achieved (best effort — a failed measurement is silent).
        if plan.target_vmaf.is_some() && !plan.spec.audio_only && !plan.spec.passthrough {
            outcome.vmaf = self.measure_output(&tools, plan, &plan.output);
        }
        Ok(outcome)
    }

    /// The plain encode: two-pass (with one correction retry) or single-pass,
    /// no VMAF handling. Returns an [`Outcome`] with `vmaf = None`.
    fn run_plain(
        &self,
        tools: &deepshrink_ffmpeg::Tools,
        plan: &EncodePlan,
        encoder: &str,
        zscale: bool,
        on_progress: &mut dyn FnMut(PassKind, f64),
    ) -> Result<Outcome, EngineError> {
        let passlog = passlog_base(plan);
        let total = plan.source_duration_sec;

        if plan.spec.passthrough {
            return self.run_passthrough(tools, plan, on_progress);
        }

        if plan.spec.two_pass {
            let args1 = build_pass_args(plan, PassKind::First, &passlog, encoder, zscale);
            tools.run_pass(&args1, total, &mut |f| on_progress(PassKind::First, f))?;
            let args2 = build_pass_args(plan, PassKind::Second, &passlog, encoder, zscale);
            tools.run_pass(&args2, total, &mut |f| on_progress(PassKind::Second, f))?;
        } else {
            let args = build_pass_args(plan, PassKind::Single, &passlog, encoder, zscale);
            tools.run_pass(&args, total, &mut |f| on_progress(PassKind::Single, f))?;
        }

        let mut size = fs::metadata(&plan.output)?.len();

        // Correction retry: if the encode overshot the target (VBV slack),
        // scale the video bitrate down proportionally and re-run the final
        // pass. Two-pass needs one; Apple's one-pass encoder is looser, so it
        // gets up to three.
        if let (Some(target), Some(mut vbps)) = (plan.target_bytes, plan.spec.video.bitrate_bps) {
            let (tries, pass) = if plan.spec.two_pass {
                (1, PassKind::Second)
            } else if plan.spec.video.hardware {
                (3, PassKind::Single)
            } else {
                (0, PassKind::Single)
            };
            for _ in 0..tries {
                if size <= target {
                    break;
                }
                let corrected = (vbps as f64 * (target as f64 / size as f64) * 0.97) as u64;
                if corrected < budget::ABSOLUTE_MIN_VIDEO_BPS {
                    break;
                }
                vbps = corrected;
                let mut retry = plan.clone();
                retry.spec.video.bitrate_bps = Some(corrected);
                let args = build_pass_args(&retry, pass, &passlog, encoder, zscale);
                tools.run_pass(&args, total, &mut |f| on_progress(pass, f))?;
                size = fs::metadata(&plan.output)?.len();
            }
        }

        cleanup_passlog(&passlog);
        Ok(Outcome {
            output: plan.output.clone(),
            final_bytes: size,
            vmaf: None,
            already_compact: false,
        })
    }

    /// The expected output size of `plan`, without running it — the honest
    /// preview for a UI. Size targets and audio come straight from the plan
    /// (pure); a quality-mode (CRF) video is predicted from short sample
    /// encodes, like the "never bigger" guard does (~2–3 s for any length).
    /// Compare the result with the source: at or above it, a guarded run keeps
    /// the original (`Outcome::already_compact`). `None` if it can't be told.
    pub fn estimate(&self, plan: &EncodePlan) -> Result<Option<u64>, EngineError> {
        if plan.spec.passthrough {
            return Ok(fs::metadata(&plan.input).ok().map(|m| m.len()));
        }
        if ceiling_plan(plan).is_some() {
            // A size target: the budget, or less when the quality CRF fits.
            let tools = self.tools()?;
            let encoder = resolve_encoder(&tools, plan)?;
            let zscale = wants_zscale(&tools, plan);
            let fit = ceiling_fit(&tools, plan, encoder, zscale).map(|(_, bytes)| bytes);
            return Ok(fit.or(plan.expected_bytes));
        }
        if let Some(bytes) = plan.expected_bytes {
            return Ok(Some(bytes));
        }
        if plan.spec.audio_only || plan.spec.video.crf.is_none() {
            return Ok(None);
        }
        let tools = self.tools()?;
        let encoder = resolve_encoder(&tools, plan)?;
        let zscale = wants_zscale(&tools, plan);
        Ok(predict_crf_bytes(&tools, plan, encoder, zscale))
    }

    /// The guard fired: deliver the source as-is (a byte copy, in its own
    /// container/extension) instead of a re-encode that would not be smaller.
    fn keep_original(
        &self,
        tools: &deepshrink_ffmpeg::Tools,
        plan: &EncodePlan,
        on_progress: &mut dyn FnMut(PassKind, f64),
    ) -> Result<Outcome, EngineError> {
        let _ = tools; // no ffmpeg needed: the source is delivered byte-for-byte
        let output = match plan.input.extension() {
            Some(ext) => plan.output.with_extension(ext),
            None => plan.output.clone(),
        };
        // A plain copy, not a remux: "kept as-is" must mean identical bytes (a
        // +faststart remux came out a few KB larger than the source).
        fs::copy(&plan.input, &output)?;
        on_progress(PassKind::Single, 1.0);
        Ok(Outcome {
            final_bytes: fs::metadata(&output)?.len(),
            output,
            vmaf: None,
            already_compact: true,
        })
    }

    /// Passthrough: the source already fits, so its streams are copied as-is.
    ///
    /// A stream copy is normally the cheapest and safest path, but it is not
    /// infallible — some codecs simply cannot be muxed by the container's muxer
    /// (ffmpeg needs a parser it may not have). Since nothing is being
    /// re-encoded here, a failed remux falls back to copying the file verbatim:
    /// the promise of this branch is "you get your file, unchanged and within
    /// target", and that must hold for every input.
    fn run_passthrough(
        &self,
        tools: &deepshrink_ffmpeg::Tools,
        plan: &EncodePlan,
        on_progress: &mut dyn FnMut(PassKind, f64),
    ) -> Result<Outcome, EngineError> {
        // Stream copy — the video encoder is never reached.
        let args = build_pass_args(plan, PassKind::Single, "", "copy", false);
        let remuxed = tools.run_pass(&args, plan.source_duration_sec, &mut |f| {
            on_progress(PassKind::Single, f)
        });
        if remuxed.is_err() {
            fs::copy(&plan.input, &plan.output)?;
            on_progress(PassKind::Single, 1.0);
        }
        let size = fs::metadata(&plan.output)?.len();
        Ok(Outcome {
            output: plan.output.clone(),
            final_bytes: size,
            vmaf: None,
            already_compact: true,
        })
    }

    /// Search CRF for the smallest output that still meets `target_vmaf`.
    ///
    /// Each trial is a single-pass CRF encode into `plan.output` followed by a
    /// VMAF measurement against the source. Drives [`budget::search_crf`], so
    /// the search algorithm itself is unit-tested separately. Falls back to a
    /// plain encode if the source resolution is unknown (nothing to measure).
    fn run_crf_search(
        &self,
        tools: &deepshrink_ffmpeg::Tools,
        plan: &EncodePlan,
        encoder: &str,
        zscale: bool,
        target_vmaf: f64,
        on_progress: &mut dyn FnMut(PassKind, f64),
    ) -> Result<Outcome, EngineError> {
        let (ref_w, ref_h) = match (plan.source_width, plan.source_height) {
            (Some(w), Some(h)) => (w, h),
            _ => return self.run_plain(tools, plan, encoder, zscale, on_progress),
        };
        let ref_fps = plan.source_fps.unwrap_or(0.0);
        let total = plan.source_duration_sec;
        let (lo, hi) = plan.spec.video.codec.crf_search_bounds();
        let n_threads = thread_count();

        let mut err: Option<EngineError> = None;
        let mut last_crf: Option<u8> = None;

        let (chosen_crf, chosen_vmaf) = budget::search_crf(target_vmaf, lo, hi, |crf| {
            if err.is_some() {
                return f64::NEG_INFINITY;
            }
            match encode_at_crf(tools, plan, encoder, zscale, crf, total, on_progress).and_then(
                |()| {
                    last_crf = Some(crf);
                    deepshrink_ffmpeg::measure_vmaf(
                        &tools.ffmpeg,
                        &plan.output,
                        &plan.input,
                        ref_w,
                        ref_h,
                        ref_fps,
                        n_threads,
                    )
                    .map_err(EngineError::from)
                },
            ) {
                Ok(v) => v,
                Err(e) => {
                    err = Some(e);
                    f64::NEG_INFINITY
                }
            }
        });
        if let Some(e) = err {
            return Err(e);
        }

        // Leave the chosen CRF on disk (the search may have ended elsewhere).
        if last_crf != Some(chosen_crf) {
            encode_at_crf(tools, plan, encoder, zscale, chosen_crf, total, on_progress)?;
        }
        let size = fs::metadata(&plan.output)?.len();
        Ok(Outcome {
            output: plan.output.clone(),
            final_bytes: size,
            vmaf: Some(chosen_vmaf),
            already_compact: false,
        })
    }

    /// Measure the VMAF of an encoded `output` against the plan's source.
    /// Returns `None` on any failure or when the source dimensions are unknown.
    fn measure_output(
        &self,
        tools: &deepshrink_ffmpeg::Tools,
        plan: &EncodePlan,
        output: &Path,
    ) -> Option<f64> {
        let (w, h) = (plan.source_width?, plan.source_height?);
        let fps = plan.source_fps.unwrap_or(0.0);
        deepshrink_ffmpeg::measure_vmaf(
            &tools.ffmpeg,
            output,
            &plan.input,
            w,
            h,
            fps,
            thread_count(),
        )
        .ok()
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
                let predicted = (bps as f64 * duration / 8.0 * (1.0 + budget::CONTAINER_OVERHEAD))
                    .round() as u64;
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
                let predicted = (bps as f64 * duration / 8.0 * (1.0 + budget::CONTAINER_OVERHEAD))
                    .round() as u64;
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
            audio_bitrate_bps: p.audio_bitrate_bps(),
            capture: capture_meta(&p),
            hdr: video
                .and_then(|v| v.color_transfer.as_deref())
                .and_then(Hdr::from_transfer),
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
            .filter(|_| {
                opts.hardware && opts.target_vmaf.is_none() && hardware_encoding_available()
            });
        let hardware = hw_quality.is_some();
        // The quality value for this encoder: CRF, or VideoToolbox's `-q:v`.
        let quality_value =
            hw_quality.unwrap_or_else(|| opts.quality.default_crf(opts.video_codec));

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
        to_sdr: None,
        hardware: false,
    }
}

/// Whether this Mac can encode with Apple's hardware (VideoToolbox) with a
/// constant-quality target: Apple Silicon and an ffmpeg with the encoders.
/// Asked once per process (it spawns `ffmpeg -encoders`).
pub fn hardware_encoding_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        // VideoToolbox's constant quality (`-q:v`) is Apple Silicon only.
        cfg!(all(target_os = "macos", target_arch = "aarch64"))
            && deepshrink_ffmpeg::locate().is_ok_and(|t| {
                deepshrink_ffmpeg::has_encoder(&t.ffmpeg, "h264_videotoolbox")
                    && deepshrink_ffmpeg::has_encoder(&t.ffmpeg, "hevc_videotoolbox")
            })
    })
}

/// A stream-copy remux plan for when the source already fits the target.
/// `faststart` is only meaningful for MP4/MOV; pass `false` for pure audio.
fn passthrough_plan(
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
fn quality_audio_bps(quality: QualityPreset, codec: AudioCodec, mono: bool) -> u64 {
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
fn already_compact_audio(bps: u64, info: &MediaInfo) -> bool {
    if info.duration_sec <= 0.0 || info.size_bytes == 0 {
        return false;
    }
    let source_bps = info.size_bytes as f64 * 8.0 / info.duration_sec;
    bps as f64 >= source_bps * 0.9
}

/// Best-effort: give `output` the modification time of `input`.
fn copy_mtime(input: &Path, output: &Path) {
    let Ok(mtime) = fs::metadata(input).and_then(|m| m.modified()) else {
        return;
    };
    if let Ok(f) = fs::File::options().write(true).open(output) {
        let _ = f.set_modified(mtime);
    }
}

/// Output container for a video. A QuickTime source (an iPhone `.MOV`) stays
/// QuickTime in quality mode: only a MOV carries its location / camera tags in
/// a form Apple's apps read (the MP4 muxer drops them). Size targets and
/// platform presets get MP4 — the most compatible for sharing. AV1 is always
/// MP4 (QuickTime has no AV1 mapping).
fn video_container(info: &MediaInfo, target: Option<u64>, opts: &ShrinkOpts) -> &'static str {
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

/// Read the capture metadata from the probe's container tags.
fn capture_meta(p: &deepshrink_ffmpeg::Ffprobe) -> CaptureMeta {
    let tag = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| p.format_tag(k))
            .map(str::to_string)
    };
    let created_local = tag(&["com.apple.quicktime.creationdate"]);
    let created_utc = created_local
        .as_deref()
        .and_then(to_utc)
        .or_else(|| tag(&["creation_time"]));
    CaptureMeta {
        created_utc,
        created_local,
        location: tag(&["com.apple.quicktime.location.ISO6709", "location"]),
        make: tag(&["com.apple.quicktime.make", "make"]),
        model: tag(&["com.apple.quicktime.model", "model"]),
    }
}

/// The explicit output tags for `info`'s capture metadata (empty when
/// metadata is stripped).
fn capture_tags(info: &MediaInfo, keep: bool) -> Vec<(String, String)> {
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
fn to_utc(s: &str) -> Option<String> {
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

/// Metadata flags for the output. Keep = map the source's global metadata,
/// then re-state the capture tags explicitly (`-metadata k=v`): ffmpeg's own
/// copy of an iPhone's `com.apple.quicktime.*` keys (`use_metadata_tags`) is
/// not readable by Apple's frameworks, whereas `location` / `make` / `model` /
/// `date` land in QuickTime user data (©xyz, ©mak, …) that Photos and Finder
/// read, and `creation_time` sets the movie header. Strip = drop it all.
fn metadata_args(plan: &EncodePlan) -> (Vec<OsString>, Option<&'static str>) {
    if !plan.spec.keep_metadata {
        return (vec!["-map_metadata".into(), "-1".into()], None);
    }
    let mut a: Vec<OsString> = vec!["-map_metadata".into(), "0".into()];
    for (k, v) in &plan.spec.tags {
        a.push("-metadata".into());
        a.push(format!("{k}={v}").into());
    }
    (a, None)
}

/// `-movflags` value combining faststart and metadata tags (None = no flag).
fn movflags(faststart: bool, meta: Option<&'static str>) -> Option<String> {
    let mut v = String::new();
    if faststart {
        v.push_str("+faststart");
    }
    if let Some(m) = meta {
        v.push_str(m);
    }
    (!v.is_empty()).then_some(v)
}

/// How far under the target a predicted CRF encode must land to be used
/// instead of the budget (predictions are within ~5%).
const CEILING_MARGIN: f64 = 0.9;

/// A size-target plan re-cast as a single-pass CRF encode at the quality
/// preset's CRF ([`EncodePlan::ceiling_crf`]). `None` for anything else.
fn ceiling_plan(plan: &EncodePlan) -> Option<EncodePlan> {
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

/// [`ceiling_plan`] and its predicted size, if sample encodes say it lands
/// comfortably under the target (the same ~2–3 s of samples as the
/// quality-mode preview).
fn ceiling_fit(
    tools: &deepshrink_ffmpeg::Tools,
    plan: &EncodePlan,
    encoder: &str,
    zscale: bool,
) -> Option<(EncodePlan, u64)> {
    let target = plan.target_bytes?;
    let ceiling = ceiling_plan(plan)?;
    let predicted = predict_crf_bytes(tools, &ceiling, encoder, zscale)?;
    ((predicted as f64) < target as f64 * CEILING_MARGIN).then_some((ceiling, predicted))
}

/// Whether to tone-map with `zscale` — asked of ffmpeg only for a PQ source.
fn wants_zscale(tools: &deepshrink_ffmpeg::Tools, plan: &EncodePlan) -> bool {
    plan.spec.video.to_sdr == Some(Hdr::Pq)
        && deepshrink_ffmpeg::has_filter(&tools.ffmpeg, "zscale")
}

/// Sample windows for [`predict_crf_bytes`]: three 3-second clips at 20/50/80%.
const SAMPLE_SECS: f64 = 3.0;
/// The shortest window for heavy video (4K, 60 fps): measured on a 60 s 4K60
/// iPhone clip, 1.5 s windows predicted as well as 3 s (+3.3 % vs +3.8 %) in
/// half the time; 1 s drifted to +7 %.
const MIN_SAMPLE_SECS: f64 = 1.5;

/// Sample window length: 3 s up to 1080p30, shorter as the pixel rate grows
/// (4K60 → 1.5 s), so a preview of heavy video doesn't take a minute.
fn sample_secs(plan: &EncodePlan) -> f64 {
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
const SAMPLE_BIAS: f64 = 0.92;

/// Predict a CRF video encode's final size from short sample encodes (same
/// encoder, CRF, preset, scaling, fps; audio at the planned bitrate): three
/// 3 s windows, or the whole clip when it's under 12 s. `None` if a sample fails.
fn predict_crf_bytes(
    tools: &deepshrink_ffmpeg::Tools,
    plan: &EncodePlan,
    encoder: &str,
    zscale: bool,
) -> Option<u64> {
    let duration = plan.source_duration_sec;
    if !duration.is_finite() || duration <= 0.0 {
        return None;
    }
    // Long clips: three 3 s windows. Short ones (< 12 s): the whole clip once —
    // exact, and still cheap — so no keyframe bias to correct either.
    let win = sample_secs(plan);
    let (windows, bias): (Vec<(f64, f64)>, f64) = if duration >= win * 4.0 {
        (
            [0.2, 0.5, 0.8]
                .iter()
                .map(|at| ((duration * at - win / 2.0).max(0.0), win))
                .collect(),
            SAMPLE_BIAS,
        )
    } else {
        (vec![(0.0, duration)], 1.0)
    };
    let mut sample = plan.clone();
    sample.spec.audio = None;
    sample.spec.faststart = false;
    let mut video_bytes = 0u64;
    let mut sampled = 0.0;
    for (i, &(start, len)) in windows.iter().enumerate() {
        sample.output =
            std::env::temp_dir().join(format!("deepshrink-sample-{}-{i}.mp4", std::process::id()));
        let mut args = build_pass_args(&sample, PassKind::Single, "", encoder, zscale);
        let at_input = args.iter().position(|a| a == "-i")?;
        args.splice(
            at_input..at_input,
            [
                OsString::from("-ss"),
                OsString::from(format!("{start:.2}")),
                OsString::from("-t"),
                OsString::from(format!("{len:.2}")),
            ],
        );
        let ran = tools.run_pass(&args, len, &mut |_| {});
        let bytes = fs::metadata(&sample.output).map(|m| m.len()).ok();
        let _ = fs::remove_file(&sample.output);
        video_bytes += bytes.filter(|_| ran.is_ok())?;
        sampled += len;
    }
    let video_bps = video_bytes as f64 * 8.0 / sampled * bias;
    let audio_bps = plan.spec.audio.as_ref().map(|a| a.bitrate_bps).unwrap_or(0) as f64;
    Some(((video_bps + audio_bps) * duration / 8.0 * (1.0 + budget::CONTAINER_OVERHEAD)) as u64)
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
const MIN_TRACK_BPS: u64 = 32_000;

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

/// The `-vf` chain: downscale first (fewer pixels to convert), then HDR → SDR.
///
/// HLG (phones) was designed to stay watchable as SDR: `colorspace` re-maps
/// BT.2020 → BT.709 reading the HLG curve as the BT.2020 gamma — side by side
/// with an iPhone clip it's the closest match to what macOS itself shows, and
/// it works in every ffmpeg build. PQ (HDR10) needs a real tone-map, which
/// takes `zscale` (libzimg — in the app's bundled ffmpeg, not in every build);
/// without it PQ falls back to `colorspace` too: flatter, but 8-bit SDR that plays.
fn video_filters(video: &VideoSpec, zscale: bool) -> Option<String> {
    const COLORSPACE: &str = "colorspace=all=bt709:iall=bt2020:itrc=bt2020-10:format=yuv420p";
    let mut chain = Vec::new();
    if let Some(h) = video.height {
        chain.push(format!("scale=-2:{h}"));
    }
    match video.to_sdr {
        Some(Hdr::Pq) if zscale => chain.push(
            "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,\
             tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,format=yuv420p"
                .to_string(),
        ),
        Some(_) => chain.push(COLORSPACE.to_string()),
        None => {}
    }
    (!chain.is_empty()).then(|| chain.join(","))
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

/// Encode a single-pass CRF trial into `plan.output` at the given CRF.
fn encode_at_crf(
    tools: &deepshrink_ffmpeg::Tools,
    plan: &EncodePlan,
    encoder: &str,
    zscale: bool,
    crf: u8,
    total: f64,
    on_progress: &mut dyn FnMut(PassKind, f64),
) -> Result<(), EngineError> {
    let mut trial = plan.clone();
    trial.spec.video.crf = Some(crf);
    trial.spec.video.bitrate_bps = None;
    trial.spec.two_pass = false;
    let args = build_pass_args(&trial, PassKind::Single, "", encoder, zscale);
    tools.run_pass(&args, total, &mut |f| on_progress(PassKind::Single, f))?;
    Ok(())
}

/// Threads to hand libvmaf (bounded by available parallelism).
fn thread_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Platform null sink for the discard output of pass 1.
fn null_sink() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

/// Pick the ffmpeg encoder to drive this plan with.
///
/// x264/x265 are in every build worth supporting, so they're taken on faith —
/// asking ffmpeg costs a process spawn per run. AV1 is the exception: builds
/// disagree on which (if any) AV1 encoder they carry, so it's probed, with
/// libaom as the fallback and a plain-English error when neither is present
/// (better than handing the user ffmpeg's "Unknown encoder" dump).
fn resolve_encoder(
    tools: &deepshrink_ffmpeg::Tools,
    plan: &EncodePlan,
) -> Result<&'static str, EngineError> {
    let codec = plan.spec.video.codec;
    // Apple's hardware encoder (availability was checked when planning).
    if plan.spec.video.hardware && !plan.spec.passthrough && !plan.spec.audio_only {
        if let Some(hw) = codec.hardware_encoder() {
            return Ok(hw);
        }
    }
    let primary = codec.encoder();
    let Some(fallback) = codec.fallback_encoder() else {
        return Ok(primary);
    };
    // Passthrough/audio-only encodes never touch the video encoder.
    if plan.spec.passthrough || plan.spec.audio_only {
        return Ok(primary);
    }
    if deepshrink_ffmpeg::has_encoder(&tools.ffmpeg, primary) {
        return Ok(primary);
    }
    if deepshrink_ffmpeg::has_encoder(&tools.ffmpeg, fallback) {
        return Ok(fallback);
    }
    Err(EngineError::Unsupported(format!(
        "this ffmpeg build has no {} encoder (looked for {primary} and {fallback})",
        codec.label()
    )))
}

/// Build the ffmpeg argv for one pass. Video-processing options (codec, filters,
/// bitrate) are shared across passes; audio/output differ per pass. `encoder` is
/// the resolved `-c:v` name (see [`resolve_encoder`]) — it can differ from the
/// codec's default for AV1.
fn build_pass_args(
    plan: &EncodePlan,
    pass: PassKind,
    passlog: &str,
    encoder: &str,
    zscale: bool,
) -> Vec<OsString> {
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

    let (meta, meta_flag) = metadata_args(plan);

    // Passthrough: stream copy, no re-encode. Output only (single pass).
    if s.passthrough {
        push!("-c");
        push!("copy");
        a.extend(meta.iter().cloned());
        if let Some(flags) = movflags(s.faststart, meta_flag) {
            push!("-movflags");
            push!(flags);
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
        a.extend(meta.iter().cloned());
        if let Some(flags) = movflags(false, meta_flag) {
            push!("-movflags");
            push!(flags);
        }
        a.push(plan.output.clone().into_os_string());
        return a;
    }

    // Video codec + filters.
    push!("-c:v");
    push!(encoder);
    if let Some(vf) = video_filters(&s.video, zscale) {
        push!("-vf");
        push!(vf);
    }
    if let Some(f) = s.video.fps {
        push!("-r");
        push!(f.to_string());
    }
    // The speed knob is per-encoder: `-preset medium` is meaningless (and fatal)
    // to SVT-AV1, which wants a number.
    // VideoToolbox has no speed preset — it's fast by construction.
    let videotoolbox = encoder.ends_with("_videotoolbox");
    if !videotoolbox {
        let (speed_flag, speed_value) = s.video.preset.speed_flags(encoder);
        push!(speed_flag);
        push!(speed_value);
    }
    if let Some(tag) = s.video.codec.mp4_tag() {
        push!("-tag:v");
        push!(tag);
    }
    // Tone-mapped to SDR: 8-bit, and labelled BT.709 so players don't treat
    // it as HDR (the source's BT.2020/HLG tags would otherwise carry over).
    if s.video.to_sdr.is_some() {
        for (flag, value) in [
            ("-pix_fmt", "yuv420p"),
            ("-color_primaries", "bt709"),
            ("-color_trc", "bt709"),
            ("-colorspace", "bt709"),
        ] {
            push!(flag);
            push!(value);
        }
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
            // Apple's encoder takes a constant quality (1–100), not a CRF.
            push!(if videotoolbox { "-q:v" } else { "-crf" });
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
                    // A mono downmix has to reach ffmpeg here too — the audio
                    // track of a video is muxed in this pass, not the audio-only
                    // branch above.
                    if au.mono {
                        push!("-ac");
                        push!("1");
                    }
                    push!("-b:a");
                    push!(au.bitrate_bps.to_string());
                }
                None => push!("-an"),
            }
            a.extend(meta.iter().cloned());
            if let Some(flags) = movflags(s.faststart, meta_flag) {
                push!("-movflags");
                push!(flags);
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

    /// The encoder `run` would resolve for a plan without probing ffmpeg (every
    /// codec these tests use has its primary encoder everywhere).
    fn enc(plan: &EncodePlan) -> &'static str {
        plan.spec.video.codec.encoder()
    }

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
            audio_bitrate_bps: None,
            capture: CaptureMeta::default(),
            hdr: None,
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
    fn plan_video_mono_downmixes_the_audio_track() {
        let info = video_info(60.0, 100_000_000, 1280, 720, true);
        let opts = ShrinkOpts {
            mono: true,
            ..opts_target(8_000_000)
        };
        let plan = MediaEngine::new().plan(&info, &opts).unwrap();
        let audio = plan.spec.audio.as_ref().expect("kept audio track");
        assert!(audio.mono, "opts.mono downmixes the video's audio track");
        // A stereo request stays stereo.
        let stereo = MediaEngine::new()
            .plan(&info, &opts_target(8_000_000))
            .unwrap();
        assert!(!stereo.spec.audio.as_ref().unwrap().mono);
    }

    #[test]
    fn video_mono_reaches_ffmpeg_as_ac_1() {
        // The plan carrying `mono` is only half the job — the muxing pass of a
        // video encode has to actually emit `-ac 1`, or the output stays stereo.
        let info = video_info(60.0, 100_000_000, 1280, 720, true);
        let plan = MediaEngine::new()
            .plan(
                &info,
                &ShrinkOpts {
                    mono: true,
                    ..opts_target(8_000_000)
                },
            )
            .unwrap();
        let args = build_pass_args(&plan, PassKind::Second, "/tmp/passlog", enc(&plan), false);
        let joined: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let ac = joined.iter().position(|a| a == "-ac").expect("-ac emitted");
        assert_eq!(joined[ac + 1], "1");

        // Stereo request → no downmix flag at all.
        let stereo = MediaEngine::new()
            .plan(&info, &opts_target(8_000_000))
            .unwrap();
        let stereo_args: Vec<String> = build_pass_args(
            &stereo,
            PassKind::Second,
            "/tmp/passlog",
            enc(&stereo),
            false,
        )
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
        assert!(!stereo_args.iter().any(|a| a == "-ac"));
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
        let args = build_pass_args(&plan, PassKind::Single, "/tmp/passlog", enc(&plan), false);
        let joined: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(joined.contains(&"copy".to_string()));
        // Same container as the source: a stream copy must land somewhere its
        // codecs are muxable.
        assert_eq!(plan.output, PathBuf::from("/tmp/clip.shrink.mp4"));
    }

    #[test]
    fn plan_passthrough_keeps_the_source_container() {
        // A .3gp may carry codecs (AMR-NB) that no .mp4 muxer accepts — copying
        // its streams into an .mp4 would fail on a file we aren't re-encoding.
        let info = MediaInfo {
            path: PathBuf::from("/tmp/voice.3gp"),
            ..video_info(10.0, 200_000, 320, 240, true)
        };
        let plan = MediaEngine::new()
            .plan(&info, &opts_target(1_000_000))
            .unwrap();
        assert!(plan.spec.passthrough);
        assert_eq!(plan.output, PathBuf::from("/tmp/voice.shrink.3gp"));
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
            audio_bitrate_bps: None,
            capture: CaptureMeta::default(),
            hdr: None,
        }
    }

    #[test]
    fn quality_audio_bitrate_follows_tier_codec_and_channels() {
        use QualityPreset::*;
        assert_eq!(quality_audio_bps(Balanced, AudioCodec::Aac, false), 128_000);
        assert_eq!(quality_audio_bps(Balanced, AudioCodec::Aac, true), 64_000);
        assert_eq!(quality_audio_bps(Fast, AudioCodec::Opus, true), 32_000);
        assert_eq!(quality_audio_bps(Max, AudioCodec::Mp3, false), 256_000);
        // Every tier is strictly smaller → larger, per codec.
        for c in [AudioCodec::Aac, AudioCodec::Opus, AudioCodec::Mp3] {
            let t: Vec<_> = [Fast, Balanced, Max]
                .map(|q| quality_audio_bps(q, c, false))
                .into();
            assert!(t[0] < t[1] && t[1] < t[2], "{c:?}: {t:?}");
        }
    }

    #[test]
    fn a_compact_audiobook_is_kept_in_quality_mode() {
        // 1 h mono at 64 kbps (the review case): balanced AAC mono is 64 kbps too.
        let mut info = audio_info(3600.0, 64_000 / 8 * 3600, 1);
        info.path = PathBuf::from("/tmp/book.mp3");
        let plan = MediaEngine::new()
            .plan(&info, &ShrinkOpts::default())
            .unwrap();
        assert!(plan.spec.passthrough, "{}", plan.summary);
        assert!(plan.output.to_string_lossy().ends_with(".mp3"));
        assert!(plan.summary.contains("already compact"));

        // A genuinely smaller recipe still encodes (Opus fast mono = 32 kbps).
        let smaller = ShrinkOpts {
            audio_codec: AudioCodec::Opus,
            quality: QualityPreset::Fast,
            ..ShrinkOpts::default()
        };
        let plan = MediaEngine::new().plan(&info, &smaller).unwrap();
        assert!(!plan.spec.passthrough);
        assert_eq!(plan.spec.audio.as_ref().unwrap().bitrate_bps, 32_000);
        assert!(
            plan.guard_larger,
            "quality mode keeps the post-encode check"
        );

        // Opting out re-encodes at the tier bitrate.
        let allow = ShrinkOpts {
            allow_larger: true,
            ..ShrinkOpts::default()
        };
        let plan = MediaEngine::new().plan(&info, &allow).unwrap();
        assert!(!plan.spec.passthrough && !plan.guard_larger);
    }

    #[test]
    fn the_guard_is_for_quality_mode_only() {
        let info = video_info(60.0, 50_000_000, 1920, 1080, true);
        let quality = MediaEngine::new()
            .plan(&info, &ShrinkOpts::default())
            .unwrap();
        assert!(quality.guard_larger);
        let target = MediaEngine::new()
            .plan(&info, &opts_target(10_000_000))
            .unwrap();
        assert!(!target.guard_larger, "a size target is its own guarantee");
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
        let args = build_pass_args(&plan, PassKind::Single, "/tmp/passlog", enc(&plan), false);
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
        let args = build_pass_args(&plan, PassKind::First, "/tmp/passlog", enc(&plan), false);
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
        let args = build_pass_args(&plan, PassKind::Second, "/tmp/passlog", enc(&plan), false);
        let joined: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(joined.iter().any(|a| a.contains("clip.shrink.mp4")));
        assert!(joined.contains(&"-c:a".to_string()));
        assert!(joined.iter().any(|a| a.contains("+faststart")));
        assert!(joined.iter().any(|a| a == "2")); // -pass 2
    }

    fn joined(plan: &EncodePlan, pass: PassKind) -> Vec<String> {
        joined_with(plan, pass, false)
    }

    fn joined_with(plan: &EncodePlan, pass: PassKind, zscale: bool) -> Vec<String> {
        build_pass_args(plan, pass, "/tmp/passlog", enc(plan), zscale)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn hdr_transfer_is_recognised() {
        assert_eq!(Hdr::from_transfer("arib-std-b67"), Some(Hdr::Hlg));
        assert_eq!(Hdr::from_transfer("smpte2084"), Some(Hdr::Pq));
        assert_eq!(Hdr::from_transfer("bt709"), None);
    }

    #[test]
    fn hdr_is_tone_mapped_to_sdr_for_size_targets_only() {
        let mut info = iphone_info();
        info.hdr = Some(Hdr::Hlg);
        let engine = MediaEngine::new();

        // Discord: must play everywhere → 8-bit SDR BT.709.
        let target = engine.plan(&info, &opts_target(10_000_000)).unwrap();
        assert_eq!(target.spec.video.to_sdr, Some(Hdr::Hlg));
        assert!(target.summary.contains("HDR → SDR"));
        let vf_of =
            |args: &[String]| args[args.iter().position(|a| a == "-vf").unwrap() + 1].clone();
        // HLG: the colorspace re-map (closest to what macOS shows), any build.
        let hlg = joined_with(&target, PassKind::Second, true);
        let vf = vf_of(&hlg);
        assert!(
            vf.contains("colorspace=all=bt709") && !vf.contains("zscale"),
            "{vf}"
        );
        // Downscale first, then convert the fewer pixels.
        assert!(
            vf.find("scale=-2:").unwrap() < vf.find("colorspace").unwrap(),
            "{vf}"
        );
        for pair in [
            ["-pix_fmt", "yuv420p"],
            ["-color_trc", "bt709"],
            ["-colorspace", "bt709"],
        ] {
            assert!(hlg.windows(2).any(|w| w == pair), "{pair:?}");
        }
        // PQ: a real tone-map with zscale, the colorspace re-map without it.
        let mut pq = target.clone();
        pq.spec.video.to_sdr = Some(Hdr::Pq);
        let vf = vf_of(&joined_with(&pq, PassKind::Second, true));
        assert!(
            vf.contains("tonemap=hable") && vf.contains("npl=100"),
            "{vf}"
        );
        let vf = vf_of(&joined_with(&pq, PassKind::Second, false));
        assert!(
            vf.contains("colorspace=all=bt709") && !vf.contains("zscale"),
            "{vf}"
        );

        // Quality mode keeps HDR and 10-bit as shot.
        let quality = engine.plan(&info, &ShrinkOpts::default()).unwrap();
        assert_eq!(quality.spec.video.to_sdr, None);
        let args = joined(&quality, PassKind::Single);
        assert!(!args
            .iter()
            .any(|a| a == "-pix_fmt" || a.contains("colorspace")));

        // An SDR source is left alone even with a target.
        let sdr = engine
            .plan(&iphone_info(), &opts_target(10_000_000))
            .unwrap();
        assert_eq!(sdr.spec.video.to_sdr, None);
        assert!(!joined(&sdr, PassKind::Second)
            .iter()
            .any(|a| a == "-pix_fmt"));
    }

    #[test]
    fn a_size_target_is_a_ceiling_at_the_quality_crf() {
        let info = video_info(60.0, 200_000_000, 1920, 1080, true);
        let engine = MediaEngine::new();
        let opts = opts_target(50_000_000);
        let plan = engine.plan(&info, &opts).unwrap();
        let crf = opts.quality.default_crf(opts.video_codec);
        assert_eq!(plan.ceiling_crf, Some(crf));

        let ceiling = ceiling_plan(&plan).unwrap();
        assert_eq!(ceiling.spec.video.crf, Some(crf));
        assert_eq!(ceiling.spec.video.bitrate_bps, None);
        assert!(!ceiling.spec.two_pass);
        // Same everything else: resolution, audio, output, the target itself.
        assert_eq!(ceiling.spec.video.height, plan.spec.video.height);
        assert_eq!(ceiling.spec.audio, plan.spec.audio);
        assert_eq!(ceiling.output, plan.output);
        assert_eq!(ceiling.target_bytes, plan.target_bytes);

        // Quality mode and passthrough have no ceiling to try.
        let quality = engine.plan(&info, &ShrinkOpts::default()).unwrap();
        assert!(quality.ceiling_crf.is_none() && ceiling_plan(&quality).is_none());
        let fits = engine.plan(&info, &opts_target(300_000_000)).unwrap();
        assert!(fits.spec.passthrough && ceiling_plan(&fits).is_none());
    }

    #[test]
    fn heavy_video_samples_shorter_windows() {
        let engine = MediaEngine::new();
        let mut info = video_info(120.0, 500_000_000, 1920, 1080, true);
        info.fps = Some(30.0);
        let plan = engine.plan(&info, &ShrinkOpts::default()).unwrap();
        assert_eq!(sample_secs(&plan), 3.0);
        info = video_info(120.0, 500_000_000, 3840, 2160, true);
        info.fps = Some(60.0);
        let plan = engine.plan(&info, &ShrinkOpts::default()).unwrap();
        assert_eq!(sample_secs(&plan), MIN_SAMPLE_SECS);
    }

    #[test]
    fn apple_hardware_uses_quality_one_pass_and_no_preset() {
        let engine = MediaEngine::new();
        let info = video_info(60.0, 200_000_000, 1920, 1080, true);
        let mut plan = engine.plan(&info, &ShrinkOpts::default()).unwrap();
        // As a plan would be on an Apple Silicon Mac with `hardware: true`.
        plan.spec.video.hardware = true;
        plan.spec.video.crf = QualityPreset::Balanced.default_hw_quality(VideoCodec::H264);
        let args: Vec<String> =
            build_pass_args(&plan, PassKind::Single, "", "h264_videotoolbox", false)
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
        assert!(args.windows(2).any(|w| w == ["-c:v", "h264_videotoolbox"]));
        assert!(args.windows(2).any(|w| w == ["-q:v", "66"]), "{args:?}");
        assert!(
            !args.iter().any(|a| a == "-crf" || a == "-preset"),
            "{args:?}"
        );
        let mut hw = plan.clone();
        hw.spec.passthrough = false;
        assert_eq!(
            resolve_encoder(
                &deepshrink_ffmpeg::Tools {
                    ffmpeg: "ffmpeg".into(),
                    ffprobe: "ffprobe".into(),
                    cancel: Default::default(),
                },
                &hw
            )
            .unwrap(),
            "h264_videotoolbox"
        );
        // AV1 has no Apple encoder: the quality map says so.
        assert_eq!(
            QualityPreset::Balanced.default_hw_quality(VideoCodec::Av1),
            None
        );
    }

    fn iphone_info() -> MediaInfo {
        let mut info = video_info(60.0, 50_000_000, 2160, 3840, true);
        info.path = PathBuf::from("/tmp/IMG_3325.MOV");
        info.capture = CaptureMeta {
            created_utc: to_utc("2026-09-26T20:01:54+0300"),
            created_local: Some("2026-09-26T20:01:54+0300".into()),
            location: Some("+50.4160+030.2796+155.635/".into()),
            make: Some("Apple".into()),
            model: Some("iPhone 12 Pro Max".into()),
        };
        info
    }

    #[test]
    fn metadata_is_kept_by_default_and_strippable() {
        let info = iphone_info();
        let plan = MediaEngine::new()
            .plan(&info, &ShrinkOpts::default())
            .unwrap();
        let a = joined(&plan, PassKind::Single);
        let at = a.iter().position(|x| x == "-map_metadata").unwrap();
        assert_eq!(a[at + 1], "0");
        // The capture tags are re-stated explicitly — the shooting date (not
        // the file's export time) in UTC, and location / make / model.
        for tag in [
            "creation_time=2026-09-26T17:01:54Z",
            "location=+50.4160+030.2796+155.635/",
            "make=Apple",
            "model=iPhone 12 Pro Max",
            "date=2026-09-26T20:01:54+0300",
        ] {
            assert!(a.contains(&tag.to_string()), "{tag} in {a:?}");
        }
        assert!(a.contains(&"+faststart".to_string()));

        let strip = ShrinkOpts {
            keep_metadata: false,
            ..ShrinkOpts::default()
        };
        let plan = MediaEngine::new().plan(&info, &strip).unwrap();
        let a = joined(&plan, PassKind::Single);
        let at = a.iter().position(|x| x == "-map_metadata").unwrap();
        assert_eq!(a[at + 1], "-1");
        assert!(!a.iter().any(|x| x.starts_with("location=")));
        assert!(a.contains(&"+faststart".to_string()));
    }

    #[test]
    fn apple_local_time_converts_to_utc() {
        let utc = |s: &str| to_utc(s);
        assert_eq!(
            utc("2026-09-26T20:01:54+0300").as_deref(),
            Some("2026-09-26T17:01:54Z")
        );
        assert_eq!(
            utc("2026-09-26T20:01:54+03:00").as_deref(),
            Some("2026-09-26T17:01:54Z")
        );
        assert_eq!(
            utc("2026-01-01T01:30:00+0300").as_deref(),
            Some("2025-12-31T22:30:00Z")
        );
        assert_eq!(
            utc("2026-03-01T23:00:00-0500").as_deref(),
            Some("2026-03-02T04:00:00Z")
        );
        assert_eq!(
            utc("2024-02-29T12:00:00.123Z").as_deref(),
            Some("2024-02-29T12:00:00Z")
        );
        assert_eq!(utc("yesterday"), None);
    }

    #[test]
    fn an_iphone_mov_stays_mov_in_quality_mode_only() {
        let info = iphone_info();
        let out = |opts: &ShrinkOpts| {
            let plan = MediaEngine::new().plan(&info, opts).unwrap();
            plan.output.to_string_lossy().into_owned()
        };
        assert!(out(&ShrinkOpts::default()).ends_with(".shrink.mov"));
        // Platform presets / size targets are for sharing → MP4.
        assert!(out(&opts_target(8_000_000)).ends_with(".shrink.mp4"));
        // AV1 has no QuickTime mapping → MP4.
        let av1 = ShrinkOpts {
            video_codec: VideoCodec::Av1,
            ..ShrinkOpts::default()
        };
        assert!(out(&av1).ends_with(".shrink.mp4"));
        // Non-MOV sources are unaffected.
        let mp4 = video_info(60.0, 50_000_000, 1920, 1080, true);
        let p = MediaEngine::new()
            .plan(&mp4, &ShrinkOpts::default())
            .unwrap();
        assert!(p.output.to_string_lossy().ends_with(".shrink.mp4"));
    }

    #[test]
    fn a_video_audio_track_is_never_upsampled() {
        let mut info = video_info(60.0, 50_000_000, 1920, 1080, true);
        info.audio_bitrate_bps = Some(64_000);
        let bps_of = |info: &MediaInfo, opts: &ShrinkOpts| {
            let plan = MediaEngine::new().plan(info, opts).unwrap();
            plan.spec.audio.unwrap().bitrate_bps
        };
        // Quality mode default is 128 kbps — capped at the source's 64 kbps.
        assert_eq!(bps_of(&info, &ShrinkOpts::default()), 64_000);
        // A size target too: never above the source (the rest goes to video).
        assert!(bps_of(&info, &opts_target(20_000_000)) <= 64_000);
        // An explicit `--audio 128k` is still honoured as asked.
        let explicit = ShrinkOpts {
            audio: AudioChoice::Bitrate(128_000),
            ..ShrinkOpts::default()
        };
        assert_eq!(bps_of(&info, &explicit), 128_000);
        // Unknown source rate → the default.
        info.audio_bitrate_bps = None;
        assert_eq!(
            bps_of(&info, &ShrinkOpts::default()),
            budget::DEFAULT_AUDIO_BPS
        );
    }

    #[test]
    fn h265_adds_hvc1_tag() {
        let info = video_info(60.0, 100_000_000, 1280, 720, false);
        let opts = ShrinkOpts {
            video_codec: VideoCodec::H265,
            ..opts_target(8_000_000)
        };
        let plan = MediaEngine::new().plan(&info, &opts).unwrap();
        let args = build_pass_args(&plan, PassKind::Second, "/tmp/passlog", enc(&plan), false);
        let joined: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(joined.contains(&"hvc1".to_string()));
        assert!(joined.contains(&"libx265".to_string()));
    }
}
