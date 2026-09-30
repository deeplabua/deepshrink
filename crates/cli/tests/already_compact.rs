//! End-to-end: quality mode never hands back a result that isn't smaller. A
//! source already squeezed hard (x264 CRF 38 on noise) can only grow when
//! re-encoded at the balanced CRF, so the default keeps it as-is (and says so);
//! `--allow-larger` re-encodes anyway. Same for a low-bitrate audiobook-style
//! MP3. Requires ffmpeg/ffprobe; skips gracefully without them.

use std::path::{Path, PathBuf};
use std::process::Command;

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "deepshrink-it-compact-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("create temp dir");
    d
}

fn ffmpeg(args: &[&str], out: &Path) {
    let status = Command::new("ffmpeg")
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args(args)
        .arg(out)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg fixture failed");
}

fn shrink(input: &Path, extra: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_deepshrink"))
        .arg(input)
        .args(extra)
        .output()
        .expect("run deepshrink");
    assert!(
        out.status.success(),
        "deepshrink failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn a_compact_video_is_kept_instead_of_growing() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("skipping: ffmpeg/ffprobe not found in PATH");
        return;
    }
    let d = dir("video");
    let src = d.join("clip.mp4");
    // 14 s (long enough for the sample prediction) of noise, crushed to CRF 38.
    ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "nullsrc=size=640x360:rate=30:duration=14,geq=random(1)*255:128:128",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-crf",
            "38",
        ],
        &src,
    );
    let before = std::fs::metadata(&src).unwrap().len();

    let stdout = shrink(&src, &["--json"]);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json");
    assert_eq!(v["already_compact"], true, "{stdout}");
    assert_eq!(
        v["final_bytes"].as_u64().unwrap(),
        before,
        "a byte-for-byte copy"
    );

    // Opting out re-encodes — and it really does come out larger.
    std::fs::remove_file(d.join("clip.shrink.mp4")).unwrap();
    let stdout = shrink(&src, &["--json", "--allow-larger"]);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json");
    assert_eq!(v["already_compact"], false);
    assert!(v["final_bytes"].as_u64().unwrap() > before, "{stdout}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_low_bitrate_mp3_is_kept_not_upsampled() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("skipping: ffmpeg/ffprobe not found in PATH");
        return;
    }
    let d = dir("audio");
    let src = d.join("book.mp3");
    ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=300:duration=20",
            "-ac",
            "1",
            "-b:a",
            "48k",
        ],
        &src,
    );
    let before = std::fs::metadata(&src).unwrap().len();

    // Balanced AAC mono = 64 kbps ≥ 48 kbps source → kept as-is (still .mp3).
    let stdout = shrink(&src, &["--json"]);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json");
    assert_eq!(v["already_compact"], true, "{stdout}");
    assert!(v["output"].as_str().unwrap().ends_with(".mp3"));
    assert!(v["final_bytes"].as_u64().unwrap() <= before + 1024);

    // A genuinely smaller recipe still encodes: Opus "fast" mono = 32 kbps.
    let stdout = shrink(
        &src,
        &["--json", "--audio-codec", "opus", "--quality", "fast"],
    );
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json");
    assert_eq!(v["already_compact"], false);
    assert!(v["final_bytes"].as_u64().unwrap() < before, "{stdout}");
    let _ = std::fs::remove_dir_all(&d);
}
