//! What the local ffmpeg build can actually do.
//!
//! ffmpeg is an external dependency we don't control: the same version can ship
//! with or without a given encoder (AV1 in particular). Callers gate on these
//! probes and either fall back or fail with a clear message, instead of letting
//! ffmpeg reject the argv with a wall of text.

use std::path::Path;
use std::process::Command;

/// Whether this ffmpeg build exposes an encoder by name (e.g. `libsvtav1`).
///
/// Matches on the encoder-name column of `ffmpeg -encoders`, so a name that only
/// appears inside a description doesn't count as a match.
pub fn has_encoder(ffmpeg: &Path, name: &str) -> bool {
    Command::new(ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .output()
        .map(|o| {
            let text = String::from_utf8_lossy(&o.stdout);
            encoder_listed(&text, name)
        })
        .unwrap_or(false)
}

/// Parse the `ffmpeg -encoders` table for an exact encoder name. Split out from
/// the process call so the parsing is testable without ffmpeg.
pub(crate) fn encoder_listed(listing: &str, name: &str) -> bool {
    listing.lines().any(|line| {
        // " V..... libsvtav1            SVT-AV1 … encoder (codec av1)"
        let mut cols = line.split_whitespace();
        let flags = cols.next().unwrap_or("");
        // The flag column is a fixed-width capability mask, never a word.
        flags.len() == 6 && cols.next() == Some(name)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING: &str = "Encoders:
 V..... libx264              libx264 H.264 / AVC (codec h264)
 V..... libsvtav1            SVT-AV1 encoder (codec av1)
 A....D aac                  AAC (Advanced Audio Coding)
";

    #[test]
    fn finds_a_listed_encoder() {
        assert!(encoder_listed(LISTING, "libsvtav1"));
        assert!(encoder_listed(LISTING, "libx264"));
        assert!(encoder_listed(LISTING, "aac"));
    }

    #[test]
    fn rejects_absent_and_description_only_matches() {
        assert!(!encoder_listed(LISTING, "libaom-av1"));
        // "AV1" and "codec av1" appear in the description column only.
        assert!(!encoder_listed(LISTING, "av1"));
        assert!(!encoder_listed(LISTING, "Encoders:"));
    }
}
