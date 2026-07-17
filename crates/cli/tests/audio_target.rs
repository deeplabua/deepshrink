//! End-to-end test for the pure-audio branch: encode a generated tone and
//! assert the result fits the target. Skips (passes) without ffmpeg; CI has it.

use std::path::{Path, PathBuf};
use std::process::Command;

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn unique_dir() -> PathBuf {
    let mut d = std::env::temp_dir();
    d.push(format!("deepshrink-audio-it-{}", std::process::id()));
    std::fs::create_dir_all(&d).expect("create temp dir");
    d
}

fn generate_wav(dir: &Path) -> PathBuf {
    let sample = dir.join("tone.wav");
    // 30s stereo PCM ≈ 5 MB, comfortably above our 500KB target.
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=30:sample_rate=44100",
            "-ac",
            "2",
        ])
        .arg(&sample)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg failed to generate the sample");
    sample
}

#[test]
fn encodes_audio_under_target() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("skipping: ffmpeg/ffprobe not found in PATH");
        return;
    }

    let dir = unique_dir();
    let sample = generate_wav(&dir);
    let output = dir.join("tone.shrink.opus");
    let target: u64 = 500_000;

    let status = Command::new(env!("CARGO_BIN_EXE_deepshrink"))
        .arg(&sample)
        .args([
            "--target",
            "500KB",
            "--audio-codec",
            "opus",
            "--mono",
            "--quiet",
        ])
        .status()
        .expect("run deepshrink");
    assert!(status.success(), "deepshrink exited with failure");

    let size = std::fs::metadata(&output).expect("output exists").len();
    assert!(size <= target, "output {size} exceeds target {target}");
    assert!(sample.exists(), "original was removed");

    // Output must be a valid Opus stream, downmixed to mono.
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=codec_name,channels",
            "-of",
            "csv=p=0",
        ])
        .arg(&output)
        .output()
        .expect("run ffprobe");
    assert!(probe.status.success(), "ffprobe rejected the output");
    let info = String::from_utf8_lossy(&probe.stdout);
    assert!(
        info.trim().starts_with("opus"),
        "unexpected codec: {info:?}"
    );
    assert!(info.contains(",1"), "expected mono, got: {info:?}");

    let _ = std::fs::remove_dir_all(&dir);
}
