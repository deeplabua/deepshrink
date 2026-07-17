//! Human-readable size parsing and platform presets.
//!
//! Sizes (`8MB`, `500KB`, `1.5GB`) → bytes. Decimal units (`KB/MB/GB`) use 1000,
//! binary units (`KiB/MiB/GiB`) use 1024. Decimal is the deliberate default:
//! for the same number it is smaller than binary, so a "≤ 8 MB" goal stays
//! conservative even on platforms that mean 8 MiB when they say "8 MB".

use thiserror::Error;

/// Errors from parsing a size or a percent.
#[derive(Debug, Error, PartialEq)]
pub enum SizeError {
    #[error("empty size string")]
    Empty,
    #[error("invalid number in size: {0:?}")]
    InvalidNumber(String),
    #[error("unknown size unit: {0:?}")]
    UnknownUnit(String),
    #[error("percent must be in (0, 100), got {0}")]
    PercentOutOfRange(f64),
}

/// Parse a size like `8MB`, `500KB`, `1.5GB`, `1024KiB`, `900000` into bytes.
///
/// Units are case-insensitive. A space between number and unit is allowed.
pub fn parse_size(input: &str) -> Result<u64, SizeError> {
    let s = input.trim();
    if s.is_empty() {
        return Err(SizeError::Empty);
    }

    // Split the numeric prefix from the unit suffix.
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num_part, unit_part) = s.split_at(split);
    if num_part.is_empty() {
        return Err(SizeError::InvalidNumber(s.to_string()));
    }

    let value: f64 = num_part
        .parse()
        .map_err(|_| SizeError::InvalidNumber(num_part.to_string()))?;
    if !value.is_finite() || value < 0.0 {
        return Err(SizeError::InvalidNumber(num_part.to_string()));
    }

    let unit = unit_part.trim();
    let multiplier: f64 = match unit.to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1_000.0,
        "m" | "mb" => 1_000_000.0,
        "g" | "gb" => 1_000_000_000.0,
        "kib" => 1_024.0,
        "mib" => (1_024u64 * 1_024) as f64,
        "gib" => (1_024u64 * 1_024 * 1_024) as f64,
        _ => return Err(SizeError::UnknownUnit(unit.to_string())),
    };

    Ok((value * multiplier).round() as u64)
}

/// Parse a percent like `70%` or `70` into the fraction `0.70`. Range is strictly (0, 100).
pub fn parse_percent(input: &str) -> Result<f64, SizeError> {
    let s = input.trim().trim_end_matches('%').trim();
    if s.is_empty() {
        return Err(SizeError::Empty);
    }
    let value: f64 = s
        .parse()
        .map_err(|_| SizeError::InvalidNumber(input.to_string()))?;
    if !value.is_finite() || value <= 0.0 || value >= 100.0 {
        return Err(SizeError::PercentOutOfRange(value));
    }
    Ok(value / 100.0)
}

/// A platform preset: a name and a hard limit in bytes (if any).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub name: &'static str,
    /// Target size in bytes; `None` means optimize without a hard limit (`web`).
    pub limit_bytes: Option<u64>,
}

/// Look up a preset by name. Limit values are TO BE VERIFIED AT RELEASE: platforms change them.
pub fn preset(name: &str) -> Option<Preset> {
    let (name, limit_bytes) = match name.trim().to_ascii_lowercase().as_str() {
        "discord" => ("discord", Some(8_000_000)),
        "discord-nitro" => ("discord-nitro", Some(500_000_000)),
        "email" => ("email", Some(20_000_000)),
        "telegram" => ("telegram", Some(2_000_000_000)),
        "whatsapp" => ("whatsapp", Some(16_000_000)),
        "web" => ("web", None),
        _ => return None,
    };
    Some(Preset { name, limit_bytes })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_decimal_units() {
        assert_eq!(parse_size("8MB").unwrap(), 8_000_000);
        assert_eq!(parse_size("500KB").unwrap(), 500_000);
        assert_eq!(parse_size("1.5GB").unwrap(), 1_500_000_000);
        assert_eq!(parse_size("2GB").unwrap(), 2_000_000_000);
    }

    #[test]
    fn parses_binary_units() {
        assert_eq!(parse_size("1KiB").unwrap(), 1_024);
        assert_eq!(parse_size("1MiB").unwrap(), 1_048_576);
        assert_eq!(parse_size("1GiB").unwrap(), 1_073_741_824);
    }

    #[test]
    fn parses_bare_bytes_and_case_and_spaces() {
        assert_eq!(parse_size("900000").unwrap(), 900_000);
        assert_eq!(parse_size("10b").unwrap(), 10);
        assert_eq!(parse_size("8mb").unwrap(), 8_000_000);
        assert_eq!(parse_size("  8 MB ").unwrap(), 8_000_000);
    }

    #[test]
    fn rounds_fractional_bytes() {
        // 0.0000005 MB = 0.5 bytes → rounds to 1 (round-half-away).
        assert_eq!(parse_size("0.0000005MB").unwrap(), 1);
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_size(""), Err(SizeError::Empty));
        assert_eq!(parse_size("   "), Err(SizeError::Empty));
        assert!(matches!(parse_size("MB"), Err(SizeError::InvalidNumber(_))));
        assert!(matches!(parse_size("8TB"), Err(SizeError::UnknownUnit(_))));
        assert!(matches!(
            parse_size("1.2.3MB"),
            Err(SizeError::InvalidNumber(_))
        ));
    }

    #[test]
    fn parses_percent() {
        assert_eq!(parse_percent("70%").unwrap(), 0.70);
        assert_eq!(parse_percent("70").unwrap(), 0.70);
        assert_eq!(parse_percent(" 12.5 % ").unwrap(), 0.125);
    }

    #[test]
    fn rejects_bad_percent() {
        assert!(matches!(
            parse_percent("0%"),
            Err(SizeError::PercentOutOfRange(_))
        ));
        assert!(matches!(
            parse_percent("100%"),
            Err(SizeError::PercentOutOfRange(_))
        ));
        assert!(matches!(
            parse_percent("150"),
            Err(SizeError::PercentOutOfRange(_))
        ));
        assert!(matches!(
            parse_percent("abc"),
            Err(SizeError::InvalidNumber(_))
        ));
    }

    #[test]
    fn known_presets_resolve() {
        assert_eq!(preset("discord").unwrap().limit_bytes, Some(8_000_000));
        assert_eq!(preset("DISCORD").unwrap().limit_bytes, Some(8_000_000));
        assert_eq!(
            preset("discord-nitro").unwrap().limit_bytes,
            Some(500_000_000)
        );
        assert_eq!(preset("email").unwrap().limit_bytes, Some(20_000_000));
        assert_eq!(preset("telegram").unwrap().limit_bytes, Some(2_000_000_000));
        assert_eq!(preset("whatsapp").unwrap().limit_bytes, Some(16_000_000));
        // web — optimization without a hard limit.
        assert_eq!(preset("web").unwrap().limit_bytes, None);
    }

    #[test]
    fn unknown_preset_is_none() {
        assert!(preset("myspace").is_none());
    }
}
