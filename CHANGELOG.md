# Changelog

All notable changes to DeepShrink. The release pipeline uses the matching
`## [x.y.z]` section as the GitHub release notes.

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
