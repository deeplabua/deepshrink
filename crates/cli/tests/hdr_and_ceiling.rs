//! End-to-end: a size target is for sending, so an HDR (HLG, 10-bit) source
//! comes out as 8-bit SDR BT.709 that phones and browsers decode, while
//! quality mode keeps HDR as shot. And a size target is a ceiling, not a
//! quota: a clip the quality preset shrinks far below it isn't padded up to
//! the budget. Requires ffmpeg/ffprobe (with libx265); skips when absent.

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
    let d = std::env::temp_dir().join(format!("deepshrink-it-hdr-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn ffmpeg(args: &[&str], out: &Path) -> bool {
    Command::new("ffmpeg")
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args(args)
        .arg(out)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// A phone-like HLG clip: HEVC Main 10, BT.2020 / arib-std-b67, in a .mov.
fn hlg_clip(path: &Path) -> bool {
    ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360:rate=30:duration=3",
            "-f",
            "lavfi",
            "-i",
            "sine=duration=3",
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-crf",
            "8",
            "-pix_fmt",
            "yuv420p10le",
            "-x265-params",
            "log-level=error:colorprim=bt2020:transfer=arib-std-b67:colormatrix=bt2020nc",
            "-color_primaries",
            "bt2020",
            "-color_trc",
            "arib-std-b67",
            "-colorspace",
            "bt2020nc",
            "-tag:v",
            "hvc1",
            "-c:a",
            "aac",
            "-shortest",
        ],
        path,
    )
}

/// `pix_fmt,color_transfer` of the first video stream.
fn video_format(path: &Path) -> String {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=pix_fmt,color_transfer",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn shrink(input: &Path, out_dir: &Path, extra: &[&str]) -> PathBuf {
    let st = Command::new(env!("CARGO_BIN_EXE_deepshrink"))
        .arg(input)
        .args(extra)
        .arg("--output")
        .arg(out_dir)
        .arg("--quiet")
        .status()
        .unwrap();
    assert!(st.success(), "deepshrink failed for {extra:?}");
    std::fs::read_dir(out_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.to_string_lossy().contains(".shrink."))
        .expect("an output")
}

#[test]
fn hdr_becomes_sdr_for_a_size_target_and_stays_hdr_in_quality_mode() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("skipping: ffmpeg/ffprobe not found in PATH");
        return;
    }
    let d = dir("sdr");
    let src = d.join("IMG_0001.MOV");
    if !hlg_clip(&src) {
        eprintln!("skipping: this ffmpeg can't make a 10-bit HEVC sample (libx265)");
        return;
    }
    assert_eq!(video_format(&src), "yuv420p10le,arib-std-b67");

    let sized = d.join("sized");
    std::fs::create_dir_all(&sized).unwrap();
    let out = shrink(&src, &sized, &["--reduce", "50%"]);
    assert_eq!(video_format(&out), "yuv420p,bt709", "{}", out.display());

    let quality = d.join("quality");
    std::fs::create_dir_all(&quality).unwrap();
    let out = shrink(&src, &quality, &["--codec", "h265"]);
    assert_eq!(
        video_format(&out),
        "yuv420p10le,arib-std-b67",
        "{}",
        out.display()
    );

    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_generous_target_is_not_filled_up() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("skipping: ffmpeg/ffprobe not found in PATH");
        return;
    }
    let d = dir("ceiling");
    let src = d.join("clip.mp4");
    // Near-lossless: big on disk, easy for the quality preset.
    assert!(ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360:rate=30:duration=4",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-crf",
            "0",
        ],
        &src,
    ));
    let source = std::fs::metadata(&src).unwrap().len();
    let target = source * 9 / 10;

    let out_dir = d.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let out = shrink(&src, &out_dir, &["--reduce", "10%"]);
    let size = std::fs::metadata(&out).unwrap().len();
    // The old behaviour spent the whole budget (~target); the quality CRF
    // lands at a small fraction of it.
    assert!(
        size < target / 2,
        "{size} bytes for a {target}-byte target — the budget was filled"
    );

    let _ = std::fs::remove_dir_all(&d);
}
