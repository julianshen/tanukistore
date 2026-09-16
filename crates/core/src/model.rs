use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use semver::Version;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("unknown platform: {0}")]
    Platform(String),
    #[error("unknown arch: {0}")]
    Arch(String),
    #[error("rollout_pct must be 0..=100, got {0}")]
    RolloutPct(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Darwin,
    Win32,
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Platform::Darwin => "darwin",
            Platform::Win32 => "win32",
        })
    }
}

impl FromStr for Platform {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "darwin" => Ok(Platform::Darwin),
            "win32" => Ok(Platform::Win32),
            other => Err(ParseError::Platform(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Arch {
    X64,
    Arm64,
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Arch::X64 => "x64",
            Arch::Arm64 => "arm64",
        })
    }
}

impl FromStr for Arch {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "x64" => Ok(Arch::X64),
            "arm64" => Ok(Arch::Arm64),
            other => Err(ParseError::Arch(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetKind {
    Zip,
    Nupkg,
    Dmg,
    Exe,
}

impl fmt::Display for AssetKind {
    /// Tokens must match the `rename_all = "lowercase"` serde representation
    /// above: these are the same wire tokens seen from two directions.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AssetKind::Zip => "zip",
            AssetKind::Nupkg => "nupkg",
            AssetKind::Dmg => "dmg",
            AssetKind::Exe => "exe",
        })
    }
}

/// Percentage of the fleet eligible for a release. Constrained at the type
/// level so an out-of-range value cannot make every client eligible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u16", into = "u16")]
pub struct RolloutPct(u8);

impl RolloutPct {
    pub const FULL: RolloutPct = RolloutPct(100);

    pub fn new(value: u8) -> Result<Self, ParseError> {
        if value > 100 {
            return Err(ParseError::RolloutPct(u16::from(value)));
        }
        Ok(RolloutPct(value))
    }

    pub fn get(&self) -> u8 {
        self.0
    }
}

impl TryFrom<u16> for RolloutPct {
    type Error = ParseError;
    fn try_from(value: u16) -> Result<Self, Self::Error> {
        if value > 100 {
            return Err(ParseError::RolloutPct(value));
        }
        Ok(RolloutPct(value as u8))
    }
}

impl From<RolloutPct> for u16 {
    fn from(value: RolloutPct) -> u16 {
        u16::from(value.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub kind: AssetKind,
    pub filename: String,
    /// Hex SHA-1. Required by the Squirrel.Windows `RELEASES` format.
    pub sha1: String,
    /// Hex SHA-512, for integrity verification.
    pub sha512: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Release {
    pub version: Version,
    #[serde(default)]
    pub notes: String,
    pub pub_date: DateTime<Utc>,
    pub rollout_pct: RolloutPct,
    pub assets: Vec<Asset>,
}

/// The append-only system of record for one app/channel/platform/arch.
/// Serializes as a bare JSON array to match the on-disk `index.json` shape.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Index {
    pub releases: Vec<Release>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_and_arch_render_as_wire_tokens() {
        assert_eq!(Platform::Darwin.to_string(), "darwin");
        assert_eq!(Platform::Win32.to_string(), "win32");
        assert_eq!(Arch::X64.to_string(), "x64");
        assert_eq!(Arch::Arm64.to_string(), "arm64");
        assert_eq!("darwin".parse::<Platform>().unwrap(), Platform::Darwin);
        assert_eq!("arm64".parse::<Arch>().unwrap(), Arch::Arm64);
        assert!("linux".parse::<Platform>().is_err());
    }

    #[test]
    fn rollout_pct_rejects_out_of_range() {
        assert_eq!(RolloutPct::new(0).unwrap().get(), 0);
        assert_eq!(RolloutPct::new(100).unwrap().get(), 100);
        assert!(RolloutPct::new(101).is_err());
        assert_eq!(RolloutPct::FULL.get(), 100);
    }

    #[test]
    fn index_round_trips_as_a_json_array() {
        let json = r#"[
          {
            "version": "1.5.0",
            "notes": "fixes",
            "pub_date": "2026-09-16T10:00:00Z",
            "rollout_pct": 10,
            "assets": [
              { "kind": "zip", "filename": "app-1.5.0-darwin-arm64.zip",
                "sha1": "aa", "sha512": "bb", "size_bytes": 1024 }
            ]
          }
        ]"#;
        let index: Index = serde_json::from_str(json).unwrap();
        assert_eq!(index.releases.len(), 1);
        let r = &index.releases[0];
        assert_eq!(r.version, semver::Version::new(1, 5, 0));
        assert_eq!(r.rollout_pct.get(), 10);
        assert_eq!(r.assets[0].kind, AssetKind::Zip);

        let back: Index = serde_json::from_str(&serde_json::to_string(&index).unwrap()).unwrap();
        assert_eq!(back.releases[0].version, r.version);
    }

    #[test]
    fn rollout_pct_out_of_range_fails_deserialization() {
        let json = r#"[{"version":"1.0.0","notes":"","pub_date":"2026-09-16T10:00:00Z",
                        "rollout_pct":150,"assets":[]}]"#;
        assert!(serde_json::from_str::<Index>(json).is_err());
    }
}
