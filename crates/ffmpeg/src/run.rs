//! Run a single ffmpeg pass, streaming progress and surfacing failures — and
//! stop it on request ([`CancelToken`]).

use std::ffi::OsStr;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use crate::progress::{self, Progress};
use crate::FfmpegError;

/// How often a running pass looks at its cancel token.
const CANCEL_POLL: Duration = Duration::from_millis(100);

/// A shared "stop" flag: [`CancelToken::cancel`] from any thread stops every
/// pass run with (a clone of) this token — the ffmpeg process is killed within
/// ~0.1 s and the pass returns [`FfmpegError::Cancelled`].
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Run `ffmpeg` with `args`, invoking `on_progress` with a 0.0..=1.0 fraction as
/// the pass proceeds. `total_secs` is the source duration (for the fraction).
///
/// The caller is expected to have included `-progress pipe:1 -nostats` in `args`
/// so progress is emitted on stdout. Not cancellable — see
/// [`run_pass_cancellable`].
pub fn run_pass<S: AsRef<OsStr>>(
    ffmpeg: &Path,
    args: &[S],
    total_secs: f64,
    on_progress: &mut dyn FnMut(f64),
) -> Result<(), FfmpegError> {
    run_pass_cancellable(ffmpeg, args, total_secs, on_progress, &CancelToken::new())
}

/// [`run_pass`] that stops when `cancel` is set: before ffmpeg starts, or while
/// it runs (the process is killed and reaped — no orphan keeps encoding). The
/// partial output is the caller's to remove.
pub fn run_pass_cancellable<S: AsRef<OsStr>>(
    ffmpeg: &Path,
    args: &[S],
    total_secs: f64,
    on_progress: &mut dyn FnMut(f64),
    cancel: &CancelToken,
) -> Result<(), FfmpegError> {
    if cancel.is_cancelled() {
        return Err(FfmpegError::Cancelled);
    }
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

    // Progress is read on its own thread too, so this one can wake up every
    // `CANCEL_POLL` to check the token even while ffmpeg prints nothing.
    let (tx, rx) = mpsc::channel::<f64>();
    let stdout = child.stdout.take();
    let stdout_handle = std::thread::spawn(move || {
        if let Some(stdout) = stdout {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let f = match progress::parse_line(&line) {
                    Some(Progress::OutTimeUs(us)) => progress::fraction(us, total_secs),
                    Some(Progress::End) => 1.0,
                    _ => continue,
                };
                if tx.send(f).is_err() {
                    break;
                }
            }
        }
    });

    loop {
        match rx.recv_timeout(CANCEL_POLL) {
            Ok(f) => on_progress(f),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break, // stdout closed: ffmpeg is done
        }
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_handle.join();
            let _ = stderr_handle.join();
            return Err(FfmpegError::Cancelled);
        }
    }
    let _ = stdout_handle.join();

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn ffmpeg() -> Option<std::path::PathBuf> {
        crate::locate().ok().map(|t| t.ffmpeg)
    }

    /// A long synthetic encode to stop half-way (~20 s of work if left alone).
    fn long_args(out: &Path) -> Vec<std::ffi::OsString> {
        let mut a: Vec<std::ffi::OsString> = [
            "-hide_banner",
            "-y",
            "-loglevel",
            "error",
            "-progress",
            "pipe:1",
            "-nostats",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=1920x1080:rate=30:duration=600",
            "-c:v",
            "libx264",
            "-preset",
            "veryslow",
        ]
        .iter()
        .map(Into::into)
        .collect();
        a.push(out.as_os_str().to_owned());
        a
    }

    #[test]
    fn a_cancelled_pass_kills_ffmpeg_promptly() {
        let Some(ffmpeg) = ffmpeg() else {
            eprintln!("skipping: ffmpeg not found");
            return;
        };
        let out =
            std::env::temp_dir().join(format!("deepshrink-cancel-{}.mp4", std::process::id()));
        let token = CancelToken::new();
        let stopper = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(2500));
            stopper.cancel();
        });
        let started = Instant::now();
        let r = run_pass_cancellable(&ffmpeg, &long_args(&out), 600.0, &mut |_| {}, &token);
        assert!(matches!(r, Err(FfmpegError::Cancelled)), "{r:?}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn an_already_cancelled_token_never_starts_ffmpeg() {
        let token = CancelToken::new();
        token.cancel();
        let r = run_pass_cancellable(
            Path::new("/nonexistent/ffmpeg"),
            &["-version"],
            1.0,
            &mut |_| {},
            &token,
        );
        assert!(matches!(r, Err(FfmpegError::Cancelled)));
    }
}
