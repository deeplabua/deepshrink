//! `MediaEngine::estimate` against a real encode: the preview a UI shows must be
//! close to what `run` produces. Needs ffmpeg/ffprobe; skips without them.

use std::path::{Path, PathBuf};
use std::process::Command;

use deepshrink_core::MediaEngine;
use deepshrink_core::{Engine, ShrinkOpts};

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn fixture(dir: &Path, name: &str, secs: u32) -> PathBuf {
    let out = dir.join(name);
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
        ])
        .arg(format!("testsrc2=size=1280x720:rate=30:duration={secs}"))
        .args(["-f", "lavfi", "-i"])
        .arg(format!("sine=frequency=440:duration={secs}"))
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-crf",
            "12",
            "-c:a",
            "aac",
            "-shortest",
        ])
        .arg(&out)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success());
    out
}

fn check(src: &Path) {
    let engine = MediaEngine::new();
    let info = engine.probe(src).unwrap();
    let opts = ShrinkOpts {
        allow_larger: true, // measure the real encode, not a kept original
        ..ShrinkOpts::default()
    };
    let mut plan = engine.plan(&info, &opts).unwrap();
    assert!(
        plan.expected_bytes.is_none(),
        "quality-mode video has no plan size"
    );
    let predicted = engine.estimate(&plan).unwrap().expect("a prediction");
    plan.output = src.with_file_name(format!(
        "{}.out.mp4",
        src.file_stem().unwrap().to_string_lossy()
    ));
    let actual = engine.run(&plan).unwrap().final_bytes;
    let err = (predicted as f64 - actual as f64).abs() / actual as f64;
    println!(
        "{}: predicted {predicted} vs actual {actual} ({:.0}%)",
        src.display(),
        err * 100.0
    );
    assert!(err < 0.25, "prediction off by {:.0}%", err * 100.0);
}

#[test]
fn estimate_tracks_the_real_encode() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("skipping: ffmpeg/ffprobe not found in PATH");
        return;
    }
    let dir = std::env::temp_dir().join(format!("deepshrink-estimate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    check(&fixture(&dir, "long.mp4", 16)); // three 3 s samples
    check(&fixture(&dir, "short.mp4", 5)); // the whole clip once
    let _ = std::fs::remove_dir_all(&dir);
}
