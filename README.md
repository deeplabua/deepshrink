# DeepShrink

> Fit any video or audio under a size limit — one command, local, no watermarks.

[![CI](https://github.com/deeplabua/deepshrink/actions/workflows/ci.yml/badge.svg)](https://github.com/deeplabua/deepshrink/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/deepshrink.svg)](https://crates.io/crates/deepshrink)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Stars](https://img.shields.io/github/stars/deeplabua/deepshrink?style=flat&logo=github&label=Star)](https://github.com/deeplabua/deepshrink/stargazers)

`deepshrink` compresses a video or audio file to a target size in a single command.
Need a clip under Discord's 8 MB limit, an email's 25 MB cap, or a Telegram / WhatsApp
size? Point DeepShrink at the file — it does the bitrate math, runs a two-pass encode,
and lands **under the limit**. Locally, without uploading to anyone's server, and
without watermarks.

A thin, fast Rust layer over ffmpeg.

> **Status:** released and actively developed (v0.3). Handles **video and audio**, with
> target-size and VMAF-quality modes, H.264/H.265, and batch processing. Install via
> Homebrew or crates.io.
>
> If DeepShrink is useful to you, please **[⭐ star the repo](https://github.com/deeplabua/deepshrink)** — it genuinely helps. See [Support](#support) to chip in.

## Why

Everyone hits upload limits — Discord (8 / 25 / 50 MB), email (~25 MB), Telegram,
WhatsApp, bug trackers, CI artifacts. The usual options are web converters (upload your
file to a stranger's server, watermarks, queues), HandBrake (powerful, but hitting
*exactly* N MB is manual trial and error), or hand-written ffmpeg commands (recompute the
bitrate every time). DeepShrink answers the one question directly: **make it ≤ N MB,
don't make me think.**

## Install

Homebrew (macOS / Linux) — pulls in ffmpeg for you:

```sh
brew install deeplabua/tap/deepshrink
```

From crates.io (expects `ffmpeg` on your PATH):

```sh
cargo install deepshrink
```

Or download a prebuilt binary from the [releases page](https://github.com/deeplabua/deepshrink/releases).

DeepShrink shells out to `ffmpeg` / `ffprobe`. The Homebrew formula installs ffmpeg
automatically; other methods expect it on your PATH (`brew install ffmpeg`, or your
platform's package manager).

## Usage

Fit a video under Discord's 8 MB limit:

```sh
deepshrink gameplay.mp4 --for discord
```

Target an explicit size, or shrink by a percentage:

```sh
deepshrink big.mp4 --target 8MB
deepshrink clip.mp4 --reduce 70%
```

Compress audio (podcast, voice memo, music):

```sh
deepshrink lecture.wav --target 10MB
```

Preview the plan without encoding, or batch a whole folder:

```sh
deepshrink big.mp4 --target 8MB --dry-run
deepshrink ./clips --recursive --for telegram
```

Target a perceptual quality instead of a size, or pick a more efficient codec:

```sh
deepshrink clip.mp4 --vmaf 93            # smallest file that still scores VMAF ≥ 93
deepshrink clip.mp4 --codec h265         # HEVC — smaller at the same quality
deepshrink clip.mp4 --codec av1          # AV1 — smallest of all, slowest to encode
```

`--vmaf` searches the encoder's quality setting (CRF) for the smallest output that meets
your target VMAF, and reports the score it achieved. It needs an ffmpeg built with the
`libvmaf` filter; without it, DeepShrink skips the measurement and encodes at a sensible
default. When a size target is set, `--vmaf` reports the VMAF actually achieved.

`--codec av1` uses SVT-AV1 (falling back to libaom-av1); it needs an ffmpeg built with one
of those encoders, and DeepShrink says so plainly if neither is present. `--mono` downmixes
the audio to one channel — useful for speech — and applies to a video's audio track as well
as to a pure-audio input.

The original file is never modified — DeepShrink writes a new `*.shrink.*` file unless
you pass `--overwrite`.

### Platform presets

| Preset | Limit |
| --- | --- |
| `discord` | 8 MB |
| `discord-nitro` | 500 MB |
| `email` | 20 MB |
| `telegram` | 2 GB |
| `whatsapp` | 16 MB |

Platform limits change over time — verify against the current service.

## How it works

`ffprobe` reads the file's duration and streams. DeepShrink computes the video / audio
bitrate that fits the target (minus container overhead), then runs a **two-pass** encode
for video, or picks a codec bitrate for audio. If the result overshoots the target, it
corrects once. Run with `--dry-run` to see the plan first.

## Update checks

For **Homebrew** installs, DeepShrink prints a one-line hint when a newer version is
available. It does this **without any network call of its own** — it asks your local
Homebrew (`brew outdated`, at most once a day, in a detached background process that never
delays your command). Nothing about your files is ever uploaded. Disable it entirely with
`DEEPSHRINK_NO_UPDATE_CHECK=1`. (Other install methods rely on `cargo install` / the
installer as usual.)

## Scope

DeepShrink handles **video and audio** (the ffmpeg engine). Images, PDF, and office files
are a planned "universal compressor" path built on pluggable engines. GIFs are handled by a
sibling tool, DeepGif.

## Support

DeepShrink is free and open-source, built and maintained by one developer.

- **[⭐ Star the repo](https://github.com/deeplabua/deepshrink)** — the cheapest way to help;
  it boosts visibility so more people find the tool.
- **Chip in a tip** via the **Sponsor** button at the top of the repo, or directly through the
  [monobank jar](https://send.monobank.ua/jar/9sb4WzNQwj). It supports a Ukrainian developer
  and keeps the project moving. Thank you 💙💛

## Part of DeepLab

DeepShrink is part of [DeepLab](https://deeplab.tools) — a line of tools for developers
and product teams.

## License

Licensed under either of

- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in this work by you, as defined in the Apache-2.0 license, shall be dual
licensed as above, without any additional terms or conditions.
