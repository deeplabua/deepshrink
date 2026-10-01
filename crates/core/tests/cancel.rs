#![cfg(feature = "ffmpeg")]

//! Stopping a running encode: `MediaEngine::with_cancel` kills ffmpeg, removes
//! the half-written output and returns a "cancelled" error — nothing keeps
//! encoding in the background. Needs ffmpeg/ffprobe; skips without them.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use deepshrink_core::{Engine, MediaEngine, ShrinkOpts, SizeGoal};
use deepshrink_ffmpeg::CancelToken;

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// ~30 s of 1080p: minutes of `veryslow` work, so the cancel lands mid-encode.
fn fixture(dir: &Path) -> PathBuf {
    let out = dir.join("long.mp4");
    let ok = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
        ])
        .arg("testsrc2=size=1920x1080:rate=30:duration=30")
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-crf", "10"])
        .arg(&out)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "fixture");
    out
}

fn ffmpeg_writing(output: &Path) -> bool {
    Command::new("pgrep")
        .args(["-f", &output.to_string_lossy()])
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false)
}

#[test]
fn a_cancelled_encode_leaves_no_process_and_no_file() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("skipping: ffmpeg/ffprobe not found in PATH");
        return;
    }
    let dir = std::env::temp_dir().join(format!("deepshrink-it-cancel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let src = fixture(&dir);

    // Size target → a two-pass encode (plus the ceiling's sample encodes first).
    let token = CancelToken::new();
    let engine = MediaEngine::with_cancel(token.clone());
    let info = engine.probe(&src).unwrap();
    let opts = ShrinkOpts {
        goal: SizeGoal::Target(info.size_bytes / 20),
        ..ShrinkOpts::default()
    };
    let mut plan = engine.plan(&info, &opts).unwrap();
    plan.ceiling_crf = None; // straight into the long two-pass encode
    let output = plan.output.clone();

    let stopper = token.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1500));
        stopper.cancel();
    });
    let started = Instant::now();
    let err = engine
        .run_with_progress(&plan, &mut |_, _| {})
        .expect_err("stopped");
    assert!(err.is_cancelled(), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert!(!output.exists(), "the half-written output is removed");
    assert!(!ffmpeg_writing(&output), "no ffmpeg keeps encoding");

    let _ = std::fs::remove_dir_all(&dir);
}
