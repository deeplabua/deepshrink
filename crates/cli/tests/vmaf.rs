//! End-to-end test for `--vmaf`: a quality-mode encode targeting a VMAF score
//! must produce a valid video and report a measured VMAF in a sane corridor.
//!
//! Requires ffmpeg/ffprobe **with the libvmaf filter**. Skips (passes) when any
//! of those is missing so local `cargo test` works without a full ffmpeg build.

use std::path::{Path, PathBuf};
use std::process::Command;

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Whether the ffmpeg on PATH exposes the libvmaf filter.
fn have_libvmaf() -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-filters"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("libvmaf"))
        .unwrap_or(false)
}

fn unique_dir() -> PathBuf {
    let mut d = std::env::temp_dir();
    d.push(format!("deepshrink-vmaf-it-{}", std::process::id()));
    std::fs::create_dir_all(&d).expect("create temp dir");
    d
}

fn generate_sample(dir: &Path) -> PathBuf {
    let sample = dir.join("sample.mp4");
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=duration=3:size=640x480:rate=30",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-b:v",
            "3M",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&sample)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg failed to generate the sample");
    sample
}

/// Pull the `"vmaf": <n>` value out of the JSON result line.
fn parse_json_vmaf(stdout: &str) -> Option<f64> {
    let key = "\"vmaf\":";
    let idx = stdout.rfind(key)? + key.len();
    let tail = stdout[idx..].trim_start();
    let end = tail.find([',', '}']).unwrap_or(tail.len());
    tail[..end].trim().parse::<f64>().ok()
}

#[test]
fn vmaf_quality_mode_reports_score() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("skipping: ffmpeg/ffprobe not found in PATH");
        return;
    }
    if !have_libvmaf() {
        eprintln!("skipping: this ffmpeg build has no libvmaf filter");
        return;
    }

    let dir = unique_dir();
    let sample = generate_sample(&dir);
    let output = dir.join("sample.shrink.mp4");

    // Quality mode (no size target) + a VMAF target → CRF search.
    let out = Command::new(env!("CARGO_BIN_EXE_deepshrink"))
        .arg(&sample)
        .args(["--vmaf", "90", "--json"])
        .output()
        .expect("run deepshrink");
    assert!(
        out.status.success(),
        "deepshrink failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(output.exists(), "expected output was not written");
    assert!(sample.exists(), "original was removed");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let vmaf = parse_json_vmaf(&stdout)
        .unwrap_or_else(|| panic!("no vmaf in JSON output: {stdout:?}"));
    // A measured score must land in a sane corridor; the synthetic pattern
    // compresses very well, so it should sit comfortably high.
    assert!(
        (60.0..=100.0).contains(&vmaf),
        "VMAF {vmaf} outside the expected corridor"
    );

    // The result must be a valid, playable H.264 video.
    let probe = Command::new("ffprobe")
        .args([
            "-v", "error", "-select_streams", "v:0", "-show_entries",
            "stream=codec_name", "-of", "csv=p=0",
        ])
        .arg(&output)
        .output()
        .expect("run ffprobe");
    assert!(probe.status.success(), "ffprobe rejected the output");
    let codec = String::from_utf8_lossy(&probe.stdout);
    assert!(codec.trim().starts_with("h264"), "unexpected codec: {codec:?}");

    let _ = std::fs::remove_dir_all(&dir);
}
