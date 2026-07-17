//! `deepshrink` binary entrypoint: parse flags, route to the media engine, map
//! errors to the exit codes from `docs/PRD/02-cli-spec.md`.
//!
//! Flow: collect inputs (expand folders with `--recursive`) → for each,
//! probe → plan → (dry-run | encode). Encoding writes to a temp sibling and
//! renames into place, so a crash never leaves a half-written output and
//! `--overwrite` can safely replace an existing file. A batch prints an
//! aggregate summary and keeps going past a single file's failure.

mod cli;
mod format;

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use deepshrink_ffmpeg::FfmpegError;
use indicatif::{ProgressBar, ProgressStyle};
use owo_colors::{OwoColorize, Stream::Stdout};

use deepshrink_core::engine::{media::PassKind, EncodePlan, Outcome};
use deepshrink_core::{
    detect_kind, parse_percent, parse_size, preset, AudioChoice, AudioCodec, Engine, EngineError,
    FpsOpt, MediaEngine, MediaInfo, MediaKind, QualityPreset, ResolutionOpt, ShrinkOpts, SizeGoal,
    VideoCodec,
};

use crate::cli::Cli;

/// Exit codes (see `docs/PRD/02-cli-spec.md`). Code 2 (bad args) is also emitted
/// by clap directly when parsing fails.
mod exit {
    pub const GENERAL_ERROR: u8 = 1;
    pub const INVALID_ARGS: u8 = 2;
    pub const FFMPEG_NOT_FOUND: u8 = 3;
    pub const INFEASIBLE: u8 = 4;
    pub const UNSUPPORTED: u8 = 5;
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!(
                "{} {}",
                "deepshrink:".if_supports_color(Stdout, |t| t.red().bold().to_string()),
                err.message()
            );
            ExitCode::from(err.code())
        }
    }
}

/// Application-level error carrying the process exit code.
#[derive(Debug)]
enum AppError {
    InvalidArgs(String),
    FfmpegNotFound(FfmpegError),
    Infeasible,
    Unsupported(String),
    Runtime(String),
    /// One or more files failed during a batch; carries the worst exit code.
    Batch {
        code: u8,
        message: String,
    },
}

impl AppError {
    fn code(&self) -> u8 {
        match self {
            AppError::InvalidArgs(_) => exit::INVALID_ARGS,
            AppError::FfmpegNotFound(_) => exit::FFMPEG_NOT_FOUND,
            AppError::Infeasible => exit::INFEASIBLE,
            AppError::Unsupported(_) => exit::UNSUPPORTED,
            AppError::Runtime(_) => exit::GENERAL_ERROR,
            AppError::Batch { code, .. } => *code,
        }
    }

    fn message(&self) -> String {
        match self {
            AppError::InvalidArgs(m) => m.clone(),
            AppError::FfmpegNotFound(e) => e.to_string(),
            AppError::Infeasible => {
                "cannot reach the requested size at a reasonable quality".to_string()
            }
            AppError::Unsupported(m) => m.clone(),
            AppError::Runtime(m) => m.clone(),
            AppError::Batch { message, .. } => message.clone(),
        }
    }
}

/// Map an engine error to the right application error / exit code.
fn map_engine_error(err: EngineError) -> AppError {
    match err {
        EngineError::Ffmpeg(f) => {
            if matches!(f, FfmpegError::NotFound { .. }) {
                AppError::FfmpegNotFound(f)
            } else {
                AppError::Runtime(f.to_string())
            }
        }
        EngineError::Infeasible => AppError::Infeasible,
        EngineError::Unsupported(s) => AppError::Unsupported(s),
        EngineError::NotImplemented(s) => AppError::Runtime(format!("not implemented: {s}")),
        EngineError::Io(e) => AppError::Runtime(e.to_string()),
    }
}

/// Outcome of processing a single file, for the batch summary.
enum FileResult {
    Encoded { original: u64, final_bytes: u64 },
    DryRun,
    Skipped,
}

/// Running totals across a batch.
#[derive(Default)]
struct BatchStats {
    encoded: usize,
    skipped: usize,
    total_original: u64,
    total_final: u64,
}

fn run(cli: &Cli) -> Result<(), AppError> {
    let goal = resolve_goal(cli)?;
    let files = collect_inputs(cli)?;

    if cli.output.is_some() && files.len() > 1 {
        return Err(AppError::InvalidArgs(
            "--output can only be used with a single input file".to_string(),
        ));
    }

    let mut opts = build_opts(cli, goal)?;

    // `--vmaf` needs ffmpeg's libvmaf filter. If this build lacks it, degrade
    // gracefully: warn once and drop the target so encoding still proceeds.
    if opts.target_vmaf.is_some() {
        let available = deepshrink_ffmpeg::locate()
            .map(|t| deepshrink_ffmpeg::has_libvmaf(&t.ffmpeg))
            .unwrap_or(true); // ffmpeg missing entirely surfaces later as code 3
        if !available {
            if !cli.quiet && !cli.json {
                eprintln!(
                    "  {}",
                    "note: this ffmpeg build has no libvmaf filter — skipping VMAF \
                     measurement (install a full ffmpeg build to enable --vmaf)"
                        .if_supports_color(Stdout, |t| t.dimmed())
                );
            }
            opts.target_vmaf = None;
        }
    }

    let engine = MediaEngine::new();
    let interactive = std::io::stdout().is_terminal();

    // Single file: propagate its error verbatim (clean exit code).
    if files.len() == 1 {
        process_one(&engine, cli, &opts, &files[0], interactive)?;
        return Ok(());
    }

    // Batch: keep going past a failing file, then summarize.
    let mut stats = BatchStats::default();
    let mut failed = 0usize;
    let mut worst_code = 0u8;
    for input in &files {
        match process_one(&engine, cli, &opts, input, interactive) {
            Ok(FileResult::Encoded {
                original,
                final_bytes,
            }) => {
                stats.encoded += 1;
                stats.total_original += original;
                stats.total_final += final_bytes;
            }
            Ok(FileResult::Skipped) => stats.skipped += 1,
            Ok(FileResult::DryRun) => {}
            Err(err) => {
                failed += 1;
                worst_code = worst_code.max(err.code());
                eprintln!(
                    "{} {}: {}",
                    "error".if_supports_color(Stdout, |t| t.red().bold().to_string()),
                    input.display(),
                    err.message()
                );
            }
        }
    }

    if !cli.quiet && !cli.json {
        print_summary(&stats, failed);
    }

    if failed > 0 {
        return Err(AppError::Batch {
            code: worst_code,
            message: format!("{failed} file(s) failed"),
        });
    }
    Ok(())
}

/// Expand the CLI inputs into a concrete list of media files.
///
/// A directory is expanded (recursively with `--recursive`, else an error).
/// An explicitly named file keeps strict behavior: an unsupported type is a
/// hard error (code 5). Non-media files found while walking a folder are
/// silently skipped.
fn collect_inputs(cli: &Cli) -> Result<Vec<PathBuf>, AppError> {
    let mut out = Vec::new();
    for path in &cli.inputs {
        if path.is_dir() {
            if !cli.recursive {
                return Err(AppError::InvalidArgs(format!(
                    "{} is a directory; pass --recursive to process folders",
                    path.display()
                )));
            }
            collect_dir(path, &mut out)?;
        } else if path.is_file() {
            if detect_kind(path) == MediaKind::Unsupported {
                return Err(AppError::Unsupported(format!(
                    "unsupported file type: {} (images/PDF/office are out of scope in v0.1)",
                    path.display()
                )));
            }
            out.push(path.clone());
        } else {
            return Err(AppError::InvalidArgs(format!(
                "no such file or directory: {}",
                path.display()
            )));
        }
    }
    if out.is_empty() {
        return Err(AppError::InvalidArgs(
            "no media files found in the given inputs".to_string(),
        ));
    }
    Ok(out)
}

/// Recursively collect media files under `dir` (sorted for determinism).
fn collect_dir(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), AppError> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| AppError::Runtime(format!("cannot read {}: {e}", dir.display())))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_dir(&path, out)?;
        } else if detect_kind(&path) != MediaKind::Unsupported {
            out.push(path);
        }
    }
    Ok(())
}

fn process_one(
    engine: &MediaEngine,
    cli: &Cli,
    opts: &ShrinkOpts,
    input: &Path,
    interactive: bool,
) -> Result<FileResult, AppError> {
    let info = engine.probe(input).map_err(map_engine_error)?;
    let mut plan = engine.plan(&info, opts).map_err(map_engine_error)?;

    if cli.dry_run {
        if cli.json {
            print_json(cli, &info, &plan, None);
        } else if !cli.quiet {
            print_report(&info, &plan);
            println!(
                "  {}\n",
                "(dry run — no encoding)".if_supports_color(Stdout, |t| t.dimmed())
            );
        }
        return Ok(FileResult::DryRun);
    }

    if !cli.quiet && !cli.json {
        print_report(&info, &plan);
    }

    // Collision handling on the final destination.
    let final_dest = plan.output.clone();
    if final_dest.exists() {
        if !cli.overwrite {
            return Err(AppError::InvalidArgs(format!(
                "output already exists: {} (use --overwrite to replace it)",
                final_dest.display()
            )));
        }
        if interactive && !cli.quiet && !cli.json {
            let q = format!("Overwrite {}?", final_dest.display());
            if !confirm(&q) {
                if !cli.quiet {
                    println!(
                        "  {}\n",
                        "(skipped)".if_supports_color(Stdout, |t| t.dimmed())
                    );
                }
                return Ok(FileResult::Skipped);
            }
        }
    }

    // Encode to a temp sibling, then move into place (atomic on the same fs).
    let temp = temp_sibling(&final_dest);
    plan.output = temp.clone();
    let outcome = encode(engine, &plan, cli).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })?;
    std::fs::rename(&temp, &final_dest).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        AppError::Runtime(format!("failed to move output into place: {e}"))
    })?;

    let outcome = Outcome {
        output: final_dest,
        final_bytes: outcome.final_bytes,
        vmaf: outcome.vmaf,
    };

    if cli.json {
        print_json_result(&info, &outcome);
    } else if !cli.quiet {
        print_outcome(&info, &outcome);
    }
    Ok(FileResult::Encoded {
        original: info.size_bytes,
        final_bytes: outcome.final_bytes,
    })
}

/// Run the encode with a progress bar (unless quiet/json/non-tty).
fn encode(engine: &MediaEngine, plan: &EncodePlan, cli: &Cli) -> Result<Outcome, AppError> {
    let bar = if cli.quiet || cli.json {
        ProgressBar::hidden()
    } else {
        let b = ProgressBar::new(100);
        b.set_style(
            ProgressStyle::with_template("  {bar:24.cyan/blue} {percent:>3}%  {msg}")
                .unwrap()
                .progress_chars("##-"),
        );
        b
    };

    let outcome = engine
        .run_with_progress(plan, &mut |pass, fraction| {
            let (base, span, label) = match pass {
                PassKind::Single => (0.0, 1.0, ""),
                PassKind::First => (0.0, 0.5, "pass 1/2"),
                PassKind::Second => (0.5, 0.5, "pass 2/2"),
            };
            bar.set_position(((base + fraction * span) * 100.0) as u64);
            bar.set_message(label);
        })
        .map_err(map_engine_error)?;

    bar.finish_and_clear();
    Ok(outcome)
}

/// A hidden temp sibling of `dest`, keeping its extension so ffmpeg picks the
/// right muxer. Same directory → the later rename stays on one filesystem.
fn temp_sibling(dest: &Path) -> PathBuf {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let stem = dest
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "out".to_string());
    let ext = dest.extension().and_then(|e| e.to_str()).unwrap_or("tmp");
    parent.join(format!(".{stem}.deepshrink-{}.{ext}", std::process::id()))
}

/// Prompt on stdin for a yes/no answer (default no).
fn confirm(prompt: &str) -> bool {
    print!("  {prompt} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Turn the mutually exclusive size flags into a [`SizeGoal`].
fn resolve_goal(cli: &Cli) -> Result<SizeGoal, AppError> {
    if let Some(t) = &cli.target {
        let bytes = parse_size(t).map_err(|e| AppError::InvalidArgs(format!("--target: {e}")))?;
        Ok(SizeGoal::Target(bytes))
    } else if let Some(r) = &cli.reduce {
        let fraction =
            parse_percent(r).map_err(|e| AppError::InvalidArgs(format!("--reduce: {e}")))?;
        Ok(SizeGoal::Reduce(fraction))
    } else if let Some(name) = &cli.for_preset {
        let p = preset(name)
            .ok_or_else(|| AppError::InvalidArgs(format!("--for: unknown preset {name:?}")))?;
        Ok(SizeGoal::Preset(p))
    } else {
        Ok(SizeGoal::Quality)
    }
}

/// Build engine options from the decoded flags.
fn build_opts(cli: &Cli, goal: SizeGoal) -> Result<ShrinkOpts, AppError> {
    Ok(ShrinkOpts {
        goal,
        video_codec: match cli.codec {
            Some(cli::VideoCodec::H264) | None => VideoCodec::H264,
            Some(cli::VideoCodec::H265) => VideoCodec::H265,
        },
        audio: parse_audio(&cli.audio)?,
        resolution: parse_resolution(&cli.resolution)?,
        fps: parse_fps(&cli.fps)?,
        quality: match cli.quality {
            cli::Quality::Fast => QualityPreset::Fast,
            cli::Quality::Balanced => QualityPreset::Balanced,
            cli::Quality::Max => QualityPreset::Max,
        },
        audio_codec: match cli.audio_codec {
            Some(cli::AudioCodec::Aac) | None => AudioCodec::Aac,
            Some(cli::AudioCodec::Opus) => AudioCodec::Opus,
            Some(cli::AudioCodec::Mp3) => AudioCodec::Mp3,
        },
        mono: cli.mono,
        sample_rate: parse_sample_rate(&cli.sample_rate)?,
        vbr: cli.vbr,
        target_vmaf: cli.vmaf,
        output: cli.output.clone(),
    })
}

fn parse_sample_rate(s: &str) -> Result<Option<u32>, AppError> {
    let t = s.trim().to_ascii_lowercase();
    if t == "auto" {
        return Ok(None);
    }
    t.parse::<u32>()
        .map(Some)
        .map_err(|_| AppError::InvalidArgs(format!("--sample-rate: invalid value {s:?}")))
}

fn parse_audio(s: &str) -> Result<AudioChoice, AppError> {
    match s.trim().to_ascii_lowercase().as_str() {
        "keep" => Ok(AudioChoice::Keep),
        "none" | "drop" => Ok(AudioChoice::Drop),
        other => parse_size(other)
            .map(AudioChoice::Bitrate)
            .map_err(|e| AppError::InvalidArgs(format!("--audio: {e} (use keep|none|<bitrate>)"))),
    }
}

fn parse_resolution(s: &str) -> Result<ResolutionOpt, AppError> {
    let t = s.trim().to_ascii_lowercase();
    if t == "auto" {
        return Ok(ResolutionOpt::Auto);
    }
    t.trim_end_matches('p')
        .parse::<u32>()
        .map(ResolutionOpt::Height)
        .map_err(|_| AppError::InvalidArgs(format!("--resolution: invalid value {s:?}")))
}

fn parse_fps(s: &str) -> Result<FpsOpt, AppError> {
    let t = s.trim().to_ascii_lowercase();
    if t == "auto" {
        return Ok(FpsOpt::Auto);
    }
    t.parse::<u32>()
        .map(FpsOpt::Cap)
        .map_err(|_| AppError::InvalidArgs(format!("--fps: invalid value {s:?}")))
}

// --- Output rendering ---

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn print_report(info: &MediaInfo, plan: &EncodePlan) {
    // Video shows WxH; pure audio shows its channel layout.
    let dims = match (info.width, info.height) {
        (Some(w), Some(h)) => format!("{w}x{h}"),
        _ => match info.audio_channels {
            Some(1) => "mono".to_string(),
            Some(2) => "stereo".to_string(),
            Some(n) => format!("{n}ch"),
            None => "—".to_string(),
        },
    };
    println!();
    println!(
        "  {}   {}  {}   {}",
        file_label(&info.path).if_supports_color(Stdout, |t| t.bold()),
        dims,
        format::duration(info.duration_sec),
        format::size(info.size_bytes),
    );
    println!(
        "  {}     {}",
        "target".if_supports_color(Stdout, |t| t.dimmed()),
        format::target_label(plan)
    );
    println!(
        "  {}       {}",
        "plan".if_supports_color(Stdout, |t| t.dimmed()),
        plan.summary
    );
}

fn print_outcome(info: &MediaInfo, outcome: &Outcome) {
    let ratio = if info.size_bytes > 0 {
        (1.0 - outcome.final_bytes as f64 / info.size_bytes as f64) * 100.0
    } else {
        0.0
    };
    // Negative reduction means the file grew — show it as a "+" delta.
    let delta = if ratio >= 0.0 {
        format!("−{ratio:.1}%")
    } else {
        format!("+{:.1}%", -ratio)
    };
    // Optional VMAF readout, e.g. "   VMAF 91.2".
    let vmaf = match outcome.vmaf {
        Some(v) => format!(
            "   {}",
            format!("VMAF {v:.1}").if_supports_color(Stdout, |t| t.cyan().to_string())
        ),
        None => String::new(),
    };
    println!(
        "  {} {}   {}   {}{}\n",
        "✓".if_supports_color(Stdout, |t| t.green().bold().to_string()),
        file_label(&outcome.output).if_supports_color(Stdout, |t| t.bold()),
        format::size(outcome.final_bytes),
        delta.if_supports_color(Stdout, |t| if ratio >= 0.0 {
            t.green().to_string()
        } else {
            t.yellow().to_string()
        }),
        vmaf,
    );
}

fn print_summary(stats: &BatchStats, failed: usize) {
    let saved = stats.total_original.saturating_sub(stats.total_final);
    let pct = if stats.total_original > 0 {
        (1.0 - stats.total_final as f64 / stats.total_original as f64) * 100.0
    } else {
        0.0
    };
    let mut line = format!(
        "Done. {} file(s) · {} → {} · saved {} (−{:.1}%)",
        stats.encoded,
        format::size(stats.total_original),
        format::size(stats.total_final),
        format::size(saved),
        pct,
    );
    if stats.skipped > 0 {
        line.push_str(&format!(" · {} skipped", stats.skipped));
    }
    if failed > 0 {
        line.push_str(&format!(" · {failed} failed"));
    }
    println!("  {}", line.if_supports_color(Stdout, |t| t.bold()));
}

fn print_json(cli: &Cli, info: &MediaInfo, plan: &EncodePlan, outcome: Option<&Outcome>) {
    let value = serde_json::json!({
        "input": info.path.to_string_lossy(),
        "output": plan.output.to_string_lossy(),
        "original_bytes": info.size_bytes,
        "target_bytes": plan.target_bytes,
        "expected_bytes": plan.expected_bytes,
        "plan": plan.summary,
        "two_pass": plan.spec.two_pass,
        "dry_run": cli.dry_run,
        "final_bytes": outcome.map(|o| o.final_bytes),
    });
    println!("{value}");
}

fn print_json_result(info: &MediaInfo, outcome: &Outcome) {
    let value = serde_json::json!({
        "input": info.path.to_string_lossy(),
        "output": outcome.output.to_string_lossy(),
        "original_bytes": info.size_bytes,
        "final_bytes": outcome.final_bytes,
        "vmaf": outcome.vmaf,
        "dry_run": false,
    });
    println!("{value}");
}
