//! Unit conversions for Valkvork.
//!
//! This module provides types and functions for converting between human-readable
//! units (like "512MiB") and machine-friendly integers (like bytes or millicores).

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use thiserror::Error;

/// Error type for unit parsing failures.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum UnitParseError {
    #[error("invalid format: {0}")]
    InvalidFormat(String),
    #[error("unknown unit: {0}")]
    UnknownUnit(String),
    #[error("invalid value: {0}")]
    InvalidValue(String),
}

/// CPU amount in millicores (1/1000 of a core).
/// This is the machine-friendly representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Millicores(pub u32);

impl Millicores {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn as_u32(&self) -> u32 {
        self.0
    }

    /// Convert from cores (float) to millicores, rounding up.
    pub fn from_cores(cores: f64) -> Self {
        // Round up to nearest millicore
        let millicores = (cores * 1000.0).ceil() as u32;
        Self(millicores)
    }

    pub fn to_cores(&self) -> f64 {
        self.0 as f64 / 1000.0
    }
}

impl fmt::Display for Millicores {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}m", self.0)
    }
}

/// Memory size in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Bytes(pub u64);

impl Bytes {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn as_u64(&self) -> u64 {
        self.0
    }

    pub fn to_kib(&self) -> u64 {
        self.0 / 1024
    }

    pub fn to_mib(&self) -> u64 {
        self.0 / (1024 * 1024)
    }

    pub fn to_gib(&self) -> u64 {
        self.0 / (1024 * 1024 * 1024)
    }
}

impl fmt::Display for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 >= 1024 * 1024 * 1024 {
            write!(f, "{}GiB", self.to_gib())
        } else if self.0 >= 1024 * 1024 {
            write!(f, "{}MiB", self.to_mib())
        } else if self.0 >= 1024 {
            write!(f, "{}KiB", self.to_kib())
        } else {
            write!(f, "{}B", self.0)
        }
    }
}

/// Size unit prefixes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeUnit {
    Bytes,
    KiB,
    MiB,
    GiB,
    TiB,
}

impl FromStr for SizeUnit {
    type Err = UnitParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "" | "b" | "bytes" => Ok(SizeUnit::Bytes),
            "kib" | "k" => Ok(SizeUnit::KiB),
            "mib" | "m" => Ok(SizeUnit::MiB),
            "gib" | "g" => Ok(SizeUnit::GiB),
            "tib" | "t" => Ok(SizeUnit::TiB),
            _ => Err(UnitParseError::UnknownUnit(s.to_string())),
        }
    }
}

/// Parse a size string like "512MiB" or "1GiB" into Bytes. A bare number is
/// bytes.
pub fn parse_size(s: &str) -> Result<Bytes, UnitParseError> {
    let s = s.trim();
    let unit_start = s.find(|c: char| c.is_alphabetic()).unwrap_or(s.len());
    let (num_str, unit_str) = s.split_at(unit_start);
    let num: u64 = num_str
        .trim()
        .parse()
        .map_err(|_| UnitParseError::InvalidValue(format!("{s:?} (expected e.g. \"512MiB\")")))?;
    let multiplier: u64 = match unit_str.parse::<SizeUnit>()? {
        SizeUnit::Bytes => 1,
        SizeUnit::KiB => 1 << 10,
        SizeUnit::MiB => 1 << 20,
        SizeUnit::GiB => 1 << 30,
        SizeUnit::TiB => 1 << 40,
    };
    num.checked_mul(multiplier)
        .map(Bytes)
        .ok_or_else(|| UnitParseError::InvalidValue(s.to_string()))
}

/// Parse a CPU specification (cores as float) into Millicores.
pub fn parse_cpu(cores: f64) -> Millicores {
    Millicores::from_cores(cores)
}

/// Parse duration string like "10s", "5m", "1h" into seconds.
pub fn parse_duration(s: &str) -> Result<u64, UnitParseError> {
    let s = s.trim().to_lowercase();

    if s.ends_with('s') {
        let num: u64 = s
            .trim_end_matches('s')
            .parse()
            .map_err(|_| UnitParseError::InvalidValue(s))?;
        Ok(num)
    } else if s.ends_with('m') {
        let num: u64 = s
            .trim_end_matches('m')
            .parse()
            .map_err(|_| UnitParseError::InvalidValue(s))?;
        Ok(num * 60)
    } else if s.ends_with('h') {
        let num: u64 = s
            .trim_end_matches('h')
            .parse()
            .map_err(|_| UnitParseError::InvalidValue(s))?;
        Ok(num * 3600)
    } else {
        // Assume seconds if no unit
        let num: u64 = s.parse().map_err(|_| UnitParseError::InvalidValue(s))?;
        Ok(num)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_size() {
        assert_eq!(parse_size("512MiB").unwrap(), Bytes(512 * 1024 * 1024));
        assert_eq!(parse_size("1GiB").unwrap(), Bytes(1024 * 1024 * 1024));
        assert_eq!(parse_size("100KiB").unwrap(), Bytes(100 * 1024));
        assert_eq!(parse_size("1024").unwrap(), Bytes(1024));
        assert_eq!(parse_size("1024B").unwrap(), Bytes(1024));
    }

    #[test]
    fn test_parse_cpu() {
        assert_eq!(parse_cpu(0.5).as_u32(), 500);
        assert_eq!(parse_cpu(1.0).as_u32(), 1000);
        assert_eq!(parse_cpu(2.5).as_u32(), 2500);
    }

    #[test]
    fn test_parse_duration() {
        assert_eq!(parse_duration("10s").unwrap(), 10);
        assert_eq!(parse_duration("5m").unwrap(), 300);
        assert_eq!(parse_duration("1h").unwrap(), 3600);
        assert_eq!(parse_duration("60").unwrap(), 60);
    }
}
