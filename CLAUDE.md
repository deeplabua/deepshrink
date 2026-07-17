# CLAUDE.md — DeepShrink

Context for contributors / Claude. Read at the start of a session.

## What this is

DeepShrink is a command-line tool that shrinks media to a target size in one command
("fit this under 8 MB for Discord / email / Telegram"). Free and open-source; part of the
DeepLab line of tools.

Engine: a thin, fast **Rust** CLI over **ffmpeg** (called as an external process, not via
libav bindings).

## Status

Pre-alpha / early development. v0.1 targets **video and audio**.

## Product boundaries — IMPORTANT

"Compress" spans different engines, so to stay focused:

- **v0.1 = video + audio only** (ffmpeg engine).
- **Images / PDF / office** (mozjpeg·pngquant / ghostscript / zip+downsample) are a planned
  "universal compressor" path via pluggable engines — **not** v0.1.
- **GIF is a separate tool (DeepGif)** — don't add it here.

The core is designed around an `Engine` trait (`supports` / `probe` / `plan` / `run`) so new
backends plug in without rewriting the CLI.

## Structure

```
crates/
  cli/     # clap, arg parsing, routing (bin: deepshrink)
  core/    # pure, testable library
    detect # file type -> which Engine
    size   # parse 8MB/500KB + platform presets
    engine # trait Engine + media (ffmpeg: video + audio)
  ffmpeg/  # locate ffmpeg/ffprobe, parse ffprobe + progress
```

Testing principle: the math (bitrate budget, size parsing, preset selection, `detect`
dispatch) lives as **pure functions in `core`**, unit-tested without real media.
`--dry-run` is a test hook (plan without encoding).

## Commands

```sh
cargo build
cargo run -- <args>          # e.g. cargo run -- clip.mp4 --target 8MB
cargo test
cargo clippy -- -D warnings
cargo fmt
```

## Requirements

`ffmpeg` and `ffprobe` on PATH (for development and for users). On absence -> exit code 3
plus a hint (`brew install ffmpeg`). The Homebrew formula installs ffmpeg automatically.

## Version control

This project uses **jj (Jujutsu)**. Pre-alpha: commit directly to `main`; commit messages
in English.

```sh
jj git fetch
jj new main@origin -m "feat: ..."   # only when the working copy is empty
# ...edits...
jj describe -m "feat: message"      # English only
jj bookmark set main -r @
jj git push --bookmark main
```

## Part of DeepLab

DeepShrink is part of [DeepLab](https://deeplab.tools). Multi-repo: this is the standalone
public CLI repository. Product roadmap and planning live in the private DeepLab workspace,
not in this repo.
