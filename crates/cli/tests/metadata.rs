//! End-to-end: an iPhone-style .MOV keeps its capture metadata by default —
//! the shooting date (from Apple's `creationdate`, not the export-time
//! `creation_time`), location, make, model — and its mtime; the output stays a
//! MOV so Apple's frameworks read it (checked through AVFoundation on macOS,
//! the way Photos / Finder see it). `--strip-metadata` removes it, and a folder
//! `--dry-run` ends with a predicted total. Requires ffmpeg/ffprobe.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("deepshrink-it-meta-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// An iPhone-like .mov: QuickTime location/make/model keys + creation time,
/// and an mtime a year in the past.
fn iphone_like(path: &Path) -> SystemTime {
    let status = Command::new("ffmpeg")
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360:rate=30:duration=4",
        ])
        .args(["-f", "lavfi", "-i", "sine=duration=4"])
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-crf",
            "8",
            "-c:a",
            "aac",
            "-shortest",
        ])
        // Export time differs from the shooting date — like an AirDropped clip.
        .args(["-metadata", "creation_time=2025-06-03T09:00:00Z"])
        .args([
            "-metadata",
            "com.apple.quicktime.creationdate=2025-06-01T13:20:30+0300",
        ])
        .args([
            "-metadata",
            "com.apple.quicktime.location.ISO6709=+50.4501+030.5234+170.000/",
        ])
        .args(["-metadata", "com.apple.quicktime.make=Apple"])
        .args(["-metadata", "com.apple.quicktime.model=iPhone 14 Pro Max"])
        .args(["-movflags", "use_metadata_tags"])
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
    let past = SystemTime::now() - Duration::from_secs(365 * 24 * 3600);
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(past)
        .unwrap();
    past
}

fn tags(path: &Path) -> String {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format_tags",
            "-of",
            "default=nw=1",
        ])
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn shrink(args: &[&str]) {
    let st = Command::new(env!("CARGO_BIN_EXE_deepshrink"))
        .args(args)
        .args(["--quiet", "--allow-larger"])
        .status()
        .unwrap();
    assert!(st.success());
}

#[test]
fn metadata_and_mtime_survive_unless_stripped() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("skipping: ffmpeg/ffprobe not found in PATH");
        return;
    }
    let d = dir("keep");
    let src = d.join("IMG_0001.MOV");
    let past = iphone_like(&src);
    let s = src.to_str().unwrap();

    shrink(&[s]);
    let out = d.join("IMG_0001.shrink.mov");
    let t = tags(&out);
    assert!(t.contains("+50.4501+030.5234"), "location kept:\n{t}");
    assert!(t.contains("iPhone 14 Pro Max"), "model kept:\n{t}");
    // The shooting date (13:20 +03:00 → 10:20 UTC), not the export time.
    assert!(
        t.contains("creation_time=2025-06-01T10:20:30"),
        "shooting date kept:\n{t}"
    );
    #[cfg(target_os = "macos")]
    assert_apple_reads(&out);
    let mtime = std::fs::metadata(&out).unwrap().modified().unwrap();
    let drift = mtime.duration_since(past).unwrap_or_else(|e| e.duration());
    assert!(
        drift < Duration::from_secs(2),
        "mtime copied (drift {drift:?})"
    );

    std::fs::remove_file(&out).unwrap();
    shrink(&[s, "--strip-metadata"]);
    let t = tags(&out);
    assert!(
        !t.contains("030.5234") && !t.contains("iPhone"),
        "stripped:\n{t}"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_folder_dry_run_ends_with_a_predicted_total() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("skipping: ffmpeg/ffprobe not found in PATH");
        return;
    }
    let d = dir("dry");
    iphone_like(&d.join("a.MOV"));
    iphone_like(&d.join("b.MOV"));
    let out = Command::new(env!("CARGO_BIN_EXE_deepshrink"))
        .arg(&d)
        .args(["--dry-run", "--recursive"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Dry run. 2 file(s)"), "{stdout}");
    assert!(stdout.contains(" → ~"), "{stdout}");
    let _ = std::fs::remove_dir_all(&d);
}

/// What Photos / Finder see: AVFoundation's common metadata of the output.
/// Skips quietly if `swift` isn't available.
#[cfg(target_os = "macos")]
fn assert_apple_reads(path: &Path) {
    let script = path.with_file_name("read.swift");
    std::fs::write(
        &script,
        r#"import AVFoundation
let a = AVURLAsset(url: URL(fileURLWithPath: CommandLine.arguments[1]))
let sem = DispatchSemaphore(value: 0)
Task {
  for i in (try? await a.load(.commonMetadata)) ?? [] {
    print("\(i.commonKey?.rawValue ?? "?")=\((try? await i.load(.stringValue)) ?? "-")")
  }
  sem.signal()
}
sem.wait()
"#,
    )
    .unwrap();
    let Ok(out) = Command::new("swift").arg(&script).arg(path).output() else {
        eprintln!("skipping AVFoundation check: swift not available");
        return;
    };
    let seen = String::from_utf8_lossy(&out.stdout);
    assert!(
        seen.contains("location=+50.4501+030.5234"),
        "AVFoundation location:\n{seen}"
    );
    assert!(
        seen.contains("model=iPhone 14 Pro Max"),
        "AVFoundation model:\n{seen}"
    );
    assert!(
        seen.contains("creationDate=2025-06-01T13:20:30+0300"),
        "AVFoundation date:\n{seen}"
    );
}
