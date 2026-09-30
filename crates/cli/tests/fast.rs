//! `--fast` (Apple's hardware encoder) is macOS-on-Apple-Silicon only: other
//! platforms, and AV1 anywhere, get a note and a software encode — never a
//! silent no-op. Requires ffmpeg/ffprobe; skips without them.

use std::path::{Path, PathBuf};
use std::process::Command;

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn sample(dir: &Path) -> PathBuf {
    let out = dir.join("clip.mp4");
    let ok = Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i"])
        .arg("testsrc2=size=320x240:rate=30:duration=2")
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-crf", "10"])
        .arg(&out)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok);
    out
}

fn dry_run_stderr(input: &Path, extra: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_deepshrink"))
        .arg(input)
        .args(["--fast", "--dry-run"])
        .args(extra)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn fast_says_when_it_cant_apply() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("skipping: ffmpeg/ffprobe not found in PATH");
        return;
    }
    let dir = std::env::temp_dir().join(format!("deepshrink-it-fast-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let clip = sample(&dir);

    // AV1: no Apple hardware encoder anywhere.
    let err = dry_run_stderr(&clip, &["--codec", "av1"]);
    assert!(err.contains("no AV1 hardware encoder"), "{err}");

    // Not a Mac with Apple Silicon: H.264 falls back too, and says so.
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        let err = dry_run_stderr(&clip, &[]);
        assert!(err.contains("needs macOS on Apple Silicon"), "{err}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
