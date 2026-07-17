# DeepShrink

> Fit any video or audio under a size limit — one command, local, no watermarks.

[![CI](https://github.com/deeplabua/deepshrink/actions/workflows/ci.yml/badge.svg)](https://github.com/deeplabua/deepshrink/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/deepshrink.svg)](https://crates.io/crates/deepshrink)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

`deepshrink` compresses a video or audio file to a target size in a single command.
Need a clip under Discord's 8 MB limit, an email's 25 MB cap, or a Telegram / WhatsApp
size? Point DeepShrink at the file — it does the bitrate math, runs a two-pass encode,
and lands **under the limit**. Locally, without uploading to anyone's server, and
without watermarks.

A thin, fast Rust layer over ffmpeg.

> **Status:** early development (pre-alpha). v0.1 targets **video and audio**.

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

## Scope

v0.1 handles **video and audio** (the ffmpeg engine). Images, PDF, and office files are a
planned "universal compressor" path built on pluggable engines. GIFs are handled by a
sibling tool, DeepGif.

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
