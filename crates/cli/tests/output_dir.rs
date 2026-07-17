//! End-to-end test: a directory `--output` writes the result into that folder
//! under the derived name (rather than treating the dir as a literal file, which
//! used to fail with "output already exists"). Skips without ffmpeg/ffprobe.

use std::path::{Path, PathBuf};
use std::process::Command;

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn unique_dir(label: &str) -> PathBuf {
    let mut d = std::env::temp_dir();
    // Include the test label so parallel tests never share a directory.
    d.push(format!(
        "deepshrink-outdir-it-{}-{label}",
        std::process::id()
    ));
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
            "testsrc2=duration=4:size=640x480:rate=30",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-b:v",
            "2M",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&sample)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "ffmpeg failed to generate the sample");
    sample
}

#[test]
fn output_directory_receives_derived_name() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("skipping: ffmpeg/ffprobe not found in PATH");
        return;
    }

    let dir = unique_dir("into-dir");
    let sample = generate_sample(&dir);
    let out_dir = dir.join("out");
    std::fs::create_dir_all(&out_dir).expect("create out dir");

    // `--output <dir>` → write <dir>/sample.shrink.mp4, not a file literally
    // named after the directory.
    let status = Command::new(env!("CARGO_BIN_EXE_deepshrink"))
        .arg(&sample)
        .args(["--target", "500KB", "--quiet", "--output"])
        .arg(&out_dir)
        .status()
        .expect("run deepshrink");
    assert!(status.success(), "deepshrink exited with failure");

    let expected = out_dir.join("sample.shrink.mp4");
    assert!(expected.exists(), "output not placed inside the directory");
    let size = std::fs::metadata(&expected).unwrap().len();
    assert!(size <= 500_000, "output {size} exceeds target");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn output_dot_writes_into_cwd() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("skipping: ffmpeg/ffprobe not found in PATH");
        return;
    }

    let dir = unique_dir("dot-cwd");
    generate_sample(&dir);

    // Run with cwd = dir, input relative, `--output .` → dir/sample.shrink.mp4.
    let status = Command::new(env!("CARGO_BIN_EXE_deepshrink"))
        .current_dir(&dir)
        .args([
            "sample.mp4",
            "--target",
            "500KB",
            "--quiet",
            "--output",
            ".",
        ])
        .status()
        .expect("run deepshrink");
    assert!(status.success(), "--output . should succeed");
    assert!(
        dir.join("sample.shrink.mp4").exists(),
        "--output . did not write into the current directory"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
