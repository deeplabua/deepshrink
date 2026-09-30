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

/// Whether this ffmpeg has the video filter `name` (e.g. `zscale`, which needs
/// a build with libzimg). Spawns `ffmpeg -filters`; callers ask only when needed.
pub fn has_filter(ffmpeg: &Path, name: &str) -> bool {
    Command::new(ffmpeg)
        .args(["-hide_banner", "-filters"])
        .output()
        .map(|o| filter_listed(&String::from_utf8_lossy(&o.stdout), name))
        .unwrap_or(false)
}

/// Parse the `ffmpeg -filters` table (" TS colorspace   V->V   Convert…").
pub(crate) fn filter_listed(listing: &str, name: &str) -> bool {
    listing.lines().any(|line| {
        let mut cols = line.split_whitespace();
        let flags = cols.next().unwrap_or("");
        flags.len() <= 3 && flags.chars().all(|c| "TSC.".contains(c)) && cols.next() == Some(name)
    })
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

    #[test]
    fn filters_table_is_parsed_by_exact_name() {
        let listing = " TS colorspace        V->V       Convert between colorspaces.\n \
                        .S tonemap           V->V       Conversion to/from different dynamic ranges.\n";
        assert!(filter_listed(listing, "tonemap"));
        assert!(filter_listed(listing, "colorspace"));
        assert!(!filter_listed(listing, "zscale"));
        assert!(!filter_listed(listing, "V->V"));
    }

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
