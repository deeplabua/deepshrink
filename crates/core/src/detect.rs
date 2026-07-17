//! File-type detection → which engine to use.
//!
//! In v0.1 dispatch is by extension: video and audio go to the ffmpeg engine,
//! everything else (images/PDF/office) is `Unsupported` (out of scope for v0.1).
//! A more precise container-based check can be added later.

use std::path::Path;

/// Media class used to pick the processing branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Video,
    Audio,
    /// Out of scope for v0.1 (image, PDF, document, unknown extension).
    Unsupported,
}

/// Determine the media class from the path's extension.
pub fn detect_kind(path: &Path) -> MediaKind {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => classify_extension(ext),
        None => MediaKind::Unsupported,
    }
}

fn classify_extension(ext: &str) -> MediaKind {
    match ext.to_ascii_lowercase().as_str() {
        "mp4" | "mov" | "mkv" | "webm" | "avi" | "m4v" | "flv" | "wmv" | "mpeg" | "mpg" | "ts"
        | "m2ts" | "3gp" => MediaKind::Video,
        "mp3" | "aac" | "m4a" | "opus" | "ogg" | "oga" | "wav" | "flac" | "wma" | "aiff"
        | "aif" | "alac" => MediaKind::Audio,
        _ => MediaKind::Unsupported,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn kind(name: &str) -> MediaKind {
        detect_kind(&PathBuf::from(name))
    }

    #[test]
    fn detects_video() {
        assert_eq!(kind("clip.mp4"), MediaKind::Video);
        assert_eq!(kind("movie.MKV"), MediaKind::Video);
        assert_eq!(kind("/a/b/gameplay.mov"), MediaKind::Video);
    }

    #[test]
    fn detects_audio() {
        assert_eq!(kind("lecture.wav"), MediaKind::Audio);
        assert_eq!(kind("song.MP3"), MediaKind::Audio);
        assert_eq!(kind("voice.opus"), MediaKind::Audio);
    }

    #[test]
    fn rejects_out_of_scope() {
        assert_eq!(kind("photo.jpg"), MediaKind::Unsupported);
        assert_eq!(kind("doc.pdf"), MediaKind::Unsupported);
        assert_eq!(kind("anim.gif"), MediaKind::Unsupported);
        assert_eq!(kind("README"), MediaKind::Unsupported);
    }
}
