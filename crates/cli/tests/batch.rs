//! End-to-end test for batch/folder processing and collision handling.
//! Skips (passes) without ffmpeg; CI has it.

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
    d.push(format!("deepshrink-batch-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("sub")).expect("create temp dirs");
    d
}

fn make_video(path: &Path) {
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=duration=4:size=320x240:rate=30",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-b:v",
            "2M",
        ])
        .arg(path)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg failed to generate {path:?}");
}

fn deepshrink() -> Command {
    Command::new(env!("CARGO_BIN_EXE_deepshrink"))
}

#[test]
fn processes_a_folder_recursively() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("skipping: ffmpeg not found");
        return;
    }
    let dir = unique_dir();
    make_video(&dir.join("a.mp4"));
    make_video(&dir.join("sub/b.mp4"));
    std::fs::write(dir.join("notes.txt"), b"not media").unwrap();

    // A directory without --recursive is an error (exit code 2).
    let no_rec = deepshrink()
        .arg(&dir)
        .args(["--target", "300KB", "--quiet"])
        .status()
        .expect("run");
    assert_eq!(
        no_rec.code(),
        Some(2),
        "directory should require --recursive"
    );

    // With --recursive both videos are shrunk.
    let status = deepshrink()
        .arg(&dir)
        .args(["--recursive", "--target", "300KB", "--quiet"])
        .status()
        .expect("run");
    assert!(status.success(), "batch run failed");

    for out in [dir.join("a.shrink.mp4"), dir.join("sub/b.shrink.mp4")] {
        let size = std::fs::metadata(&out)
            .unwrap_or_else(|_| panic!("missing {out:?}"))
            .len();
        assert!(size <= 300_000, "{out:?} is {size} bytes, over target");
    }

    // Re-running now collides with the existing outputs → error without --overwrite.
    let collide = deepshrink()
        .arg(dir.join("a.mp4"))
        .args(["--target", "300KB", "--quiet"])
        .status()
        .expect("run");
    assert_eq!(
        collide.code(),
        Some(2),
        "collision should require --overwrite"
    );

    // --overwrite replaces it.
    let overwrite = deepshrink()
        .arg(dir.join("a.mp4"))
        .args(["--target", "300KB", "--overwrite", "--quiet"])
        .status()
        .expect("run");
    assert!(overwrite.success(), "--overwrite run failed");

    let _ = std::fs::remove_dir_all(&dir);
}
