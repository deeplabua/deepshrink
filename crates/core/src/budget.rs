//! Pure bitrate-budgeting math for fitting media into a target size.
//!
//! All functions here are side-effect free and unit-tested. The classic
//! two-pass budgeting model (see `docs/PRD/02-cli-spec.md`):
//!
//! ```text
//! usable_bits   = target_bytes * 8 * (1 - overhead)
//! video_bits    = usable_bits - audio_bitrate * duration
//! video_bitrate = video_bits / duration
//! ```

/// Container/muxing overhead reserved from the target budget.
pub const CONTAINER_OVERHEAD: f64 = 0.02;

/// Default audio bitrate (bits/s) when keeping a track without a hard number.
pub const DEFAULT_AUDIO_BPS: u64 = 128_000;

/// Below this video bitrate we consider the result not worth encoding → the
/// engine reports `Infeasible` (exit code 4) rather than producing mush.
pub const ABSOLUTE_MIN_VIDEO_BPS: u64 = 60_000;

/// A rung of the resolution ladder: a standard height and the minimum video
/// bitrate (bits/s) below which that height stops being worthwhile for H.264.
///
/// Values are heuristics — TO BE TUNED with real VMAF measurements before release.
#[derive(Debug, Clone, Copy)]
pub struct Rung {
    pub height: u32,
    pub min_video_bps: u64,
}

/// Standard resolution ladder, highest first.
pub const LADDER: &[Rung] = &[
    Rung {
        height: 1080,
        min_video_bps: 2_500_000,
    },
    Rung {
        height: 720,
        min_video_bps: 1_200_000,
    },
    Rung {
        height: 480,
        min_video_bps: 600_000,
    },
    Rung {
        height: 360,
        min_video_bps: 350_000,
    },
    Rung {
        height: 240,
        min_video_bps: 200_000,
    },
    Rung {
        height: 144,
        min_video_bps: 100_000,
    },
];

/// Video bitrate (bits/s) that fits a target size, after reserving audio and
/// container overhead. Returns `None` if the budget can't hold any video.
pub fn video_bitrate_bps(target_bytes: u64, duration_sec: f64, audio_bps: u64) -> Option<u64> {
    if duration_sec <= 0.0 {
        return None;
    }
    let usable_bits = target_bytes as f64 * 8.0 * (1.0 - CONTAINER_OVERHEAD);
    let audio_bits = audio_bps as f64 * duration_sec;
    let video_bits = usable_bits - audio_bits;
    if video_bits <= 0.0 {
        return None;
    }
    Some((video_bits / duration_sec) as u64)
}

/// Absolute target bytes for a `--reduce <pct>` request: keep `(1 - fraction)`
/// of the original. `reduce_fraction` is the parsed fraction (0.70 for "70%").
pub fn reduce_target_bytes(original_bytes: u64, reduce_fraction: f64) -> u64 {
    let keep = (1.0 - reduce_fraction).clamp(0.0, 1.0);
    (original_bytes as f64 * keep).round() as u64
}

/// Choose a target height (≤ source) for the given video bitrate.
///
/// Picks the highest ladder rung that (a) is no taller than the source and
/// (b) has enough bitrate. If the bitrate is below every applicable rung's
/// minimum, falls back to the smallest rung ≤ source; if the source is smaller
/// than the whole ladder, keeps the source height.
pub fn choose_height(src_height: u32, video_bps: u64) -> u32 {
    for rung in LADDER {
        if rung.height <= src_height && video_bps >= rung.min_video_bps {
            return rung.height;
        }
    }
    LADDER
        .iter()
        .filter(|r| r.height <= src_height)
        .map(|r| r.height)
        .min()
        .unwrap_or(src_height)
}

/// Pick the highest audio bitrate from `candidates` (bits/s, descending) that
/// still leaves room for at least `ABSOLUTE_MIN_VIDEO_BPS` of video. Returns
/// `None` if even the lowest candidate leaves no viable video budget.
pub fn fit_audio_bps(target_bytes: u64, duration_sec: f64, candidates: &[u64]) -> Option<u64> {
    candidates.iter().copied().find(|&audio_bps| {
        video_bitrate_bps(target_bytes, duration_sec, audio_bps)
            .is_some_and(|v| v >= ABSOLUTE_MIN_VIDEO_BPS)
    })
}

// --- VMAF-targeted CRF search (session 005) ---

/// Find the highest CRF in `lo..=hi` (i.e. the smallest output) whose measured
/// VMAF still meets `target`. `measure` maps a CRF to its VMAF score and is
/// assumed monotonically non-increasing in CRF (higher CRF → lower quality).
///
/// Returns the chosen CRF and the VMAF it scored. If even `lo` (the best
/// quality) can't reach `target`, returns `lo` and its score as a best effort.
/// Runs at most `1 + ceil(log2(hi - lo + 1))` measurements via binary search.
///
/// This is the pure, unit-tested form of the search the engine drives with real
/// encodes (see `engine::media::MediaEngine::run_crf_search`).
pub fn search_crf(target: f64, lo: u8, hi: u8, mut measure: impl FnMut(u8) -> f64) -> (u8, f64) {
    let base = measure(lo);
    // Monotonic: if the best quality misses the target, nothing will.
    if base < target {
        return (lo, base);
    }
    let mut best = (lo, base);
    let (mut a, mut b) = (lo as i32 + 1, hi as i32);
    while a <= b {
        let mid = a + (b - a) / 2;
        let v = measure(mid as u8);
        if v >= target {
            best = (mid as u8, v);
            a = mid + 1;
        } else {
            b = mid - 1;
        }
    }
    best
}

// --- Pure-audio budgeting (session 003) ---

/// Standard audio bitrate steps (bits/s, descending). We snap the raw budget
/// *down* to one of these so the encoded track never exceeds the target.
pub const AUDIO_STEPS: &[u64] = &[
    320_000, 256_000, 192_000, 160_000, 128_000, 96_000, 80_000, 64_000, 48_000, 32_000, 24_000,
    16_000, 12_000,
];

/// Below this audio bitrate even speech stops being intelligible → the engine
/// reports `Infeasible` (exit code 4) rather than producing noise.
pub const ABSOLUTE_MIN_AUDIO_BPS: u64 = 12_000;

/// Raw audio bitrate (bits/s) that fits a target size for a pure-audio file,
/// after reserving container overhead. `None` if the budget holds nothing.
pub fn audio_bitrate_bps(target_bytes: u64, duration_sec: f64) -> Option<u64> {
    if duration_sec <= 0.0 {
        return None;
    }
    let usable_bits = target_bytes as f64 * 8.0 * (1.0 - CONTAINER_OVERHEAD);
    let bps = usable_bits / duration_sec;
    if bps <= 0.0 {
        return None;
    }
    Some(bps as u64)
}

/// Snap a raw bitrate down to the nearest standard step (never above it). The
/// caller must ensure `bps >= ABSOLUTE_MIN_AUDIO_BPS`, so a step always fits.
pub fn snap_audio_bitrate(bps: u64) -> u64 {
    AUDIO_STEPS
        .iter()
        .copied()
        .find(|&step| step <= bps)
        .unwrap_or_else(|| *AUDIO_STEPS.last().expect("non-empty ladder"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_bitrate_subtracts_audio_and_overhead() {
        // 8 MB, 60 s, 128 kbps audio.
        // usable = 8_000_000*8*0.98 = 62_720_000 bits
        // audio  = 128_000*60       =  7_680_000 bits
        // video  = 55_040_000 / 60  ≈ 917_333 bps
        let v = video_bitrate_bps(8_000_000, 60.0, 128_000).unwrap();
        assert!((916_000..=918_000).contains(&v), "got {v}");
    }

    #[test]
    fn video_bitrate_none_when_audio_eats_budget() {
        // Tiny target, long duration, fat audio → nothing left for video.
        assert_eq!(video_bitrate_bps(100_000, 600.0, 128_000), None);
        assert_eq!(video_bitrate_bps(1_000_000, 0.0, 0), None);
    }

    #[test]
    fn reduce_keeps_complement() {
        assert_eq!(reduce_target_bytes(1_000_000, 0.70), 300_000);
        assert_eq!(reduce_target_bytes(1_000_000, 0.0), 1_000_000);
        assert_eq!(reduce_target_bytes(1_000_000, 1.0), 0);
    }

    #[test]
    fn choose_height_picks_best_feasible_rung() {
        // Plenty of bitrate → keep 1080.
        assert_eq!(choose_height(1080, 4_000_000), 1080);
        // 1080 needs 2.5M; 800k only clears the 480 rung.
        assert_eq!(choose_height(1080, 800_000), 480);
        // 720 source, high bitrate → stays 720 (never upscales).
        assert_eq!(choose_height(720, 5_000_000), 720);
        // Very low bitrate → smallest rung ≤ source.
        assert_eq!(choose_height(1080, 10_000), 144);
        // Source below the ladder → unchanged.
        assert_eq!(choose_height(100, 500_000), 100);
    }

    #[test]
    fn audio_bitrate_fits_target() {
        // 10 MB, 3480 s (58 min): usable = 10e6*8*0.98 = 78_400_000 bits
        // bps = 78_400_000 / 3480 ≈ 22_528
        let bps = audio_bitrate_bps(10_000_000, 3480.0).unwrap();
        assert!((22_000..=23_000).contains(&bps), "got {bps}");
        assert_eq!(audio_bitrate_bps(1_000_000, 0.0), None);
    }

    #[test]
    fn snap_audio_floors_to_a_step() {
        assert_eq!(snap_audio_bitrate(22_528), 16_000);
        assert_eq!(snap_audio_bitrate(128_000), 128_000);
        assert_eq!(snap_audio_bitrate(130_000), 128_000);
        assert_eq!(snap_audio_bitrate(1_000_000), 320_000); // capped
        assert_eq!(snap_audio_bitrate(12_000), 12_000);
    }

    #[test]
    fn search_crf_picks_highest_meeting_target() {
        // Synthetic monotonic model: VMAF falls 1.5 points per CRF step from a
        // reference of 100 at CRF 18.
        let vmaf = |crf: u8| 100.0 - 1.5 * (crf as f64 - 18.0);
        // Target 91 → highest CRF with vmaf >= 91 is 24 (vmaf 91.0).
        let (crf, v) = search_crf(91.0, 18, 32, vmaf);
        assert_eq!(crf, 24);
        assert!(v >= 91.0);
        // The next CRF up must dip below the target (we picked the smallest file).
        assert!(vmaf(crf + 1) < 91.0);
    }

    #[test]
    fn search_crf_best_effort_when_unreachable() {
        // Even the best quality (lo) can't reach the target → return lo.
        let vmaf = |crf: u8| 80.0 - (crf as f64 - 18.0);
        let (crf, v) = search_crf(95.0, 18, 32, vmaf);
        assert_eq!(crf, 18);
        assert_eq!(v, 80.0);
    }

    #[test]
    fn search_crf_keeps_top_quality_when_all_pass() {
        // Everything clears a very low target → the largest CRF (smallest file).
        let vmaf = |crf: u8| 100.0 - 0.1 * (crf as f64 - 18.0);
        let (crf, _) = search_crf(50.0, 18, 32, vmaf);
        assert_eq!(crf, 32);
    }

    #[test]
    fn fit_audio_steps_down() {
        let candidates = [128_000, 96_000, 64_000, 48_000];
        // Roomy target → top candidate.
        assert_eq!(fit_audio_bps(8_000_000, 60.0, &candidates), Some(128_000));
        // Tight target → 128k leaves too little video, so step down to 96k.
        assert_eq!(fit_audio_bps(1_300_000, 60.0, &candidates), Some(96_000));
        // Impossible → None (even 48k audio leaves no viable video budget).
        assert_eq!(fit_audio_bps(50_000, 600.0, &candidates), None);
    }
}
