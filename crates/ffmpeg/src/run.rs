//! Run a single ffmpeg pass, streaming progress and surfacing failures.

use std::ffi::OsStr;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::progress::{self, Progress};
use crate::FfmpegError;

/// Run `ffmpeg` with `args`, invoking `on_progress` with a 0.0..=1.0 fraction as
/// the pass proceeds. `total_secs` is the source duration (for the fraction).
///
/// The caller is expected to have included `-progress pipe:1 -nostats` in `args`
/// so progress is emitted on stdout.
pub fn run_pass<S: AsRef<OsStr>>(
    ffmpeg: &Path,
    args: &[S],
    total_secs: f64,
    on_progress: &mut dyn FnMut(f64),
) -> Result<(), FfmpegError> {
    let mut child = Command::new(ffmpeg)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| FfmpegError::Spawn {
            tool: "ffmpeg",
            source,
        })?;

    // Drain stderr on a separate thread so a chatty encoder can't deadlock us
    // while we read stdout for progress.
    let stderr = child.stderr.take();
    let stderr_handle = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(stderr) = stderr {
            use std::io::Read;
            let _ = BufReader::new(stderr).read_to_string(&mut buf);
        }
        buf
    });

    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            match progress::parse_line(&line) {
                Some(Progress::OutTimeUs(us)) => on_progress(progress::fraction(us, total_secs)),
                Some(Progress::End) => on_progress(1.0),
                _ => {}
            }
        }
    }

    let status = child.wait().map_err(|source| FfmpegError::Spawn {
        tool: "ffmpeg",
        source,
    })?;
    let stderr = stderr_handle.join().unwrap_or_default();

    if !status.success() {
        return Err(FfmpegError::CommandFailed {
            tool: "ffmpeg",
            status: status.to_string(),
            stderr: stderr.trim().to_string(),
        });
    }
    Ok(())
}
