//! End-to-end test: encode a generated sample and assert the result fits the
//! target. Requires ffmpeg/ffprobe; skips (passes) gracefully when they are
//! absent so local `cargo test` works without them. CI installs ffmpeg.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Locate a binary in PATH, returning its name if runnable.
fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn unique_dir() -> PathBuf {
    let mut d = std::env::temp_dir();
    d.push(format!("deepshrink-it-{}", std::process::id()));
    std::fs::create_dir_all(&d).expect("create temp dir");
    d
}

fn generate_sample(dir: &Path) -> PathBuf {
    let sample = dir.join("sample.mp4");
    // ~5s 640x480 with a tone; -b:v 2M keeps it comfortably above our target.
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=duration=5:size=640x480:rate=30",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=5",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-b:v",
            "2M",
            "-c:a",
            "aac",
            "-shortest",
        ])
        .arg(&sample)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg failed to generate the sample");
    sample
}

#[test]
fn encodes_video_under_target() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("skipping: ffmpeg/ffprobe not found in PATH");
        return;
    }

    let dir = unique_dir();
    let sample = generate_sample(&dir);
    let output = dir.join("sample.shrink.mp4");
    let target: u64 = 500_000;

    let status = Command::new(env!("CARGO_BIN_EXE_deepshrink"))
        .arg(&sample)
        .args(["--target", "500KB", "--quiet"])
        .status()
        .expect("run deepshrink");
    assert!(status.success(), "deepshrink exited with failure");

    let size = std::fs::metadata(&output)
        .expect("output file exists")
        .len();
    assert!(
        size <= target,
        "output {size} bytes exceeds target {target} bytes"
    );

    // The original must be untouched.
    assert!(sample.exists(), "original was removed");

    // Output must be a valid, playable video (ffprobe parses a video stream).
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "csv=p=0",
        ])
        .arg(&output)
        .output()
        .expect("run ffprobe");
    assert!(probe.status.success(), "ffprobe rejected the output");
    let codec = String::from_utf8_lossy(&probe.stdout);
    assert!(
        codec.trim().starts_with("h264"),
        "unexpected codec: {codec:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
