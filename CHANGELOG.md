# Changelog

All notable changes to DeepShrink. The release pipeline uses the matching
`## [x.y.z]` section as the GitHub release notes.

## [0.3.15] - 2026-10-03

### Fixed

- **A kept-as-is result never overwrites another file.** When a re-encode
  wouldn't be smaller, the original is delivered under the output name with
  its own extension (`book.mp3` → `book.shrink.mp3`) — that name skipped the
  "output already exists" check, so an existing `book.shrink.mp3` was
  replaced even without `--overwrite`. It's now refused like any other
  collision; `--overwrite` replaces it as before.

### Library (`deepshrink-core`)

- The kept-as-is copy is created fresh (`create_new`): the engine fails with an
  I/O error instead of overwriting a file at that path.

## [0.3.14] - 2026-10-01

### Changed

- **`--for discord` now targets 10 MB** (Discord's current free upload limit;
  was 8 MB). Pass `--target 8MB` for the old size.

### Added

- Presets **`slack`** (1 GB) and **`imessage`** (100 MB — Apple doesn't
  publish a limit; larger videos go as links).

## [0.3.13] - 2026-10-01

### Changed

- **More accurate previews with `--fast`.** A quality-mode size estimate for
  Apple's hardware encoder no longer applies the software encoder's
  keyframe correction (it under-predicted by ~6 % on average): measured on
  6 clips × H.264/HEVC, 11 of 12 previews now land within ±10 %.

### Library (`deepshrink-core`)

- `engine::plan::SAMPLE_BIAS_HW`; `sample_windows` picks the bias by encoder.

## [0.3.12] - 2026-10-01

### Library (`deepshrink-core`)

- **Size prediction from samples is pure** — `engine::plan::sample_windows`
  (which windows to sample-encode, and the keyframe bias) and
  `engine::plan::predicted_bytes` (the final size from the samples), so an
  encoder other than ffmpeg (the iOS app's VideoToolbox) predicts the same way.
  Behaviour unchanged.

## [0.3.11] - 2026-10-01

### Library (`deepshrink-core`)

- **The ffmpeg engine is an optional feature** (`ffmpeg`, on by default — the
  CLI and existing users see no change). With `default-features = false` the
  crate is the pure planning logic only: size budgets, presets, quality tiers,
  Apple-encoder calibration, the "worth it" threshold and
  `engine::plan::plan(info, opts, hw_available)` — e.g. for an iOS app that
  encodes with AVFoundation.
- Planning moved out of `engine::media` into `engine::plan` (behaviour
  unchanged); `plan::ceiling_plan`, `plan::corrected_bitrate`, `plan::to_utc`
  are public.

## [0.3.10] - 2026-09-30

### Fixed

- **Portrait phone video reads as portrait.** Phones store a portrait clip as a
  landscape frame plus a rotation; the probe now applies it, so a clip reports
  576 × 1024 (9:16), not 1024 × 576 — in `--dry-run`, JSON and for apps.
- **Resolution caps mean the short side.** `--resolution 1080p` on a portrait
  4K iPhone clip gives 1080 × 1920 (it gave a narrow 608 × 1080).

### Library

- `Stream::rotation()`, `Stream::display_size()` (`deepshrink-ffmpeg`);
  `MediaInfo` width / height are as shown; `VideoSpec::portrait`.

## [0.3.9] - 2026-09-30

### Added

- **`--fast`: Apple's hardware encoder** (VideoToolbox, macOS on Apple Silicon).
  3–5× faster and a fifth of the memory for the same visual quality (VMAF-
  calibrated per quality tier), for a somewhat larger file — about the same
  as x264 for H.264, ~50% larger than x265 for H.265. One pass; size targets
  are hit with up to three corrections. HDR through Apple's H.264 (8-bit only)
  becomes SDR. macOS on Apple Silicon only: on Windows, Linux, Intel Macs and
  for AV1 it prints a note and encodes in software.

### Changed

- **Faster previews of heavy video.** `--dry-run` / `estimate` sample shorter
  windows as the pixel rate grows (4K60: 1.5 s instead of 3 s) — half the time,
  same accuracy (+3.3% vs +3.8% on a 60 s 4K60 clip).

### Library (`deepshrink-core`)

- `ShrinkOpts::hardware`, `VideoSpec::hardware`, `QualityPreset::default_hw_quality`,
  `VideoCodec::hardware_encoder`, `media::hardware_encoding_available()`.

## [0.3.8] - 2026-09-30

### Changed

- **"Never bigger" means worth it.** In quality mode a re-encode now has to save
  at least 5% of the source; otherwise the original is kept as-is. An iPhone
  HEVC clip that H.264 would shrink by 0.2% is no longer encoded for minutes
  for nothing. `--dry-run` reports those files as "would be kept as-is".

### Library (`deepshrink-core`)

- `MIN_SAVING` and `not_worth_it(expected, source)` — the same threshold for
  the guard, dry runs and UI previews.

## [0.3.7] - 2026-09-30

### Library (`deepshrink-core`, `deepshrink-ffmpeg`)

- **Encodes can be stopped.** `MediaEngine::with_cancel(token)` — setting the
  `CancelToken` from any thread kills the running ffmpeg within ~0.1 s,
  removes the half-written output and the two-pass log, and returns an error
  for which `EngineError::is_cancelled()` is true. Before, an app could only
  stop listening while ffmpeg kept encoding in the background.
- `deepshrink_ffmpeg::{CancelToken, run_pass_cancellable}`, `Tools::run_pass`,
  `Tools::with_cancel`, `FfmpegError::Cancelled` (new); `Tools` gained a
  `cancel` field and `MediaEngine` is no longer `Copy`.

## [0.3.6] - 2026-09-30

### Added

- **Metadata is kept.** The shooting date, location and camera (make / model)
  are carried over in a form Apple's apps read — Photos and Finder show the
  same place and date as the original — and the output gets the source's
  modification time, so it sorts next to the original instead of "today". The
  date comes from the iPhone's own capture time, not the moment the file was
  exported or AirDropped. `--strip-metadata` removes it instead.
- **Folder dry runs end with a total:** `Dry run. 21 file(s) · 328.2 MB →
  ~290.0 MB (−12%) · 9 would be kept as-is`.
- **Realistic size preview.** `--dry-run` now shows a real expected size in
  quality mode too: video is predicted from three short sample encodes (the
  whole clip when it's under 12 s) — within a few percent of the real encode,
  in about 3 seconds. Quality-mode audio reports bitrate × duration.

### Changed

- **Size targets play everywhere.** An HDR video (iPhone HLG, HDR10) sent
  through a size target or platform preset comes out as 8-bit SDR (BT.709):
  10-bit H.264 and HDR don't play on most phones, browsers and in chat apps.
  iPhone HLG is converted close to how macOS shows it, in any ffmpeg build;
  HDR10 (PQ) gets a proper tone-map when ffmpeg has `zscale`. Quality mode
  keeps HDR and 10-bit as shot.
- **A size target is a ceiling, not a quota.** When the quality preset fits
  well under the target, it's used instead of filling the budget: a 2 s
  iPhone clip for Discord is 611 KB, not 7.9 MB. The target itself still holds
  (a miss falls back to the budgeted two-pass encode), and `--dry-run` /
  `estimate` report the smaller size.
- **An iPhone `.MOV` stays `.mov` in quality mode.** Only QuickTime carries its
  location / camera tags where Apple's apps look for them. Size targets and
  platform presets (`--target`, `--for discord`, …) still produce `.mp4` for
  maximum compatibility (those keep the shooting date).
- **A video's audio track is never upsampled.** It's re-encoded at most at its
  own bitrate (a 64 kbps phone recording no longer becomes 128 kbps AAC);
  under a size target the saved bits go to the picture. An explicit
  `--audio <rate>` is still honoured.

### Library (`deepshrink-core`)

- `MediaInfo::audio_bitrate_bps`, `MediaInfo::capture` (`CaptureMeta`),
  `MediaInfo::hdr` (`Hdr`), `VideoSpec::to_sdr`, `EncodePlan::ceiling_crf`,
  `ShrinkOpts::keep_metadata`, `EncodeSpec::keep_metadata`, `EncodeSpec::tags`
  (new public items); `deepshrink_ffmpeg::has_filter`.
- `MediaEngine::estimate(&plan)` — the expected output size without running
  the encode; compare it with the source to know whether a guarded run would
  keep the original.

## [0.3.5] - 2026-09-30

### Added

- **Never bigger (quality mode).** Without a size target, DeepShrink no longer
  hands back a file that isn't smaller than the source — re-encoding an
  already-compact file only loses quality. Such files are kept as-is and
  reported as `already compact — kept as-is`.
  - **Video:** before the full encode, three short sample encodes (at 20 / 50 /
    80 % of the clip) predict the final size, so an already-optimal clip is
    recognised in seconds instead of after a full encode.
  - **Audio:** a file whose own bitrate is at or under the one the preset would
    encode at is kept, not re-encoded (a 64 kbps MP3 audiobook no longer
    doubles).
  - Every result is checked at the end as well; if it didn't shrink, the
    original is kept instead.
- `--allow-larger` — opt out and re-encode anyway.
- `--json` output gains `already_compact`.

### Changed

- **Audio quality presets now differ in bitrate.** Quality mode used a fixed
  160 kbps (stereo) / 96 kbps (mono) for every preset; it now follows the
  quality tier and codec — AAC 96 / 128 / 192, Opus 64 / 96 / 128, MP3
  128 / 160 / 256 kbps stereo for fast / balanced / max (mono: half).

### Library (`deepshrink-core`)

- `ShrinkOpts::allow_larger`, `EncodePlan::guard_larger`,
  `Outcome::already_compact` (new public fields).

## [0.3.4] - 2026-07

- AV1 codec (`--codec av1`, SVT-AV1 with a libaom fallback).
- `--mono` also applies to a video's audio track.
- Safe passthrough: a source that already fits is stream-copied in its own
  container.

## [0.3.3] - 2026-07-18

- Refreshed README (status, Support section). No code changes.

## [0.3.2] - 2026-07-17

- Fix: the Homebrew update nudge now fires for genuinely outdated versions.

## [0.3.1] - 2026-07-17

- The update nudge highlights the new version and the upgrade command.

## [0.3.0] - 2026-07-17

- Homebrew "new version available" nudge (local `brew outdated`, no network
  from the binary; `DEEPSHRINK_NO_UPDATE_CHECK=1` to opt out).

## [0.2.1] - 2026-07-17

- `--output <dir>` for single files and batches; the result path is shown in
  the log.

## [0.2.0] - 2026-07-17

- First public release: target size / `--reduce` / platform presets, VMAF
  quality mode (`--vmaf`), H.264 / H.265, audio targets, batch folders.
