use std::fmt;
use thiserror::Error;

pub const PURGERY_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeryVersion {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

#[derive(Error, Debug)]
pub enum VersionError {
    #[error("invalid version string: {0}")]
    InvalidFormat(String),
    #[error("incompatible Purgery version in {context}: producer {producer}, current {current}; major/minor versions must match")]
    Incompatible {
        context: String,
        producer: String,
        current: String,
    },
}

impl fmt::Display for PurgeryVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Returns the current package version string (e.g. `"0.1.0"`).
pub fn current_purgery_version() -> &'static str {
    PURGERY_VERSION
}

/// Parse a semver version string. Handles standard `X.Y.Z` format.
/// Prerelease and build metadata suffixes are stripped for parsing
/// but do not cause rejection. Returns an error if the input is empty,
/// malformed, or does not contain at least major.minor.patch.
pub fn parse_purgery_version(input: &str) -> Result<PurgeryVersion, VersionError> {
    if input.is_empty() {
        return Err(VersionError::InvalidFormat("empty string".into()));
    }
    // Strip build metadata after '+'
    let without_build = input.split('+').next().unwrap_or(input);
    // Strip prerelease after '-'
    let without_prerelease = without_build.split('-').next().unwrap_or(without_build);

    let mut parts = without_prerelease.splitn(3, '.');
    let major = parts
        .next()
        .ok_or_else(|| VersionError::InvalidFormat(input.to_owned()))?;
    let minor = parts
        .next()
        .ok_or_else(|| VersionError::InvalidFormat(input.to_owned()))?;
    let patch = parts
        .next()
        .ok_or_else(|| VersionError::InvalidFormat(input.to_owned()))?;

    let major: u64 = major
        .parse()
        .map_err(|_| VersionError::InvalidFormat(input.to_owned()))?;
    let minor: u64 = minor
        .parse()
        .map_err(|_| VersionError::InvalidFormat(input.to_owned()))?;
    let patch: u64 = patch
        .parse()
        .map_err(|_| VersionError::InvalidFormat(input.to_owned()))?;

    Ok(PurgeryVersion { major, minor, patch })
}

/// Returns `true` if the producer and consumer have the same major
/// and minor versions. Patch versions may differ.
pub fn versions_compatible(producer: &str, consumer: &str) -> Result<bool, VersionError> {
    let p = parse_purgery_version(producer)?;
    let c = parse_purgery_version(consumer)?;
    Ok(p.major == c.major && p.minor == c.minor)
}

/// Checks that `producer` is major/minor-compatible with the current
/// package version. Returns an error with context if incompatible.
pub fn require_compatible_purgery_version(
    producer: &str,
    context: impl fmt::Display,
) -> Result<(), VersionError> {
    let current = current_purgery_version();
    if !versions_compatible(producer, current)? {
        return Err(VersionError::Incompatible {
            context: context.to_string(),
            producer: producer.to_owned(),
            current: current.to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_version_is_valid() {
        let v = parse_purgery_version(current_purgery_version()).unwrap();
        assert!(v.major == 0 || v.major >= 1);
    }

    #[test]
    fn same_major_minor_different_patch_is_compatible() {
        assert!(versions_compatible("0.1.0", "0.1.7").unwrap());
        assert!(versions_compatible("0.1.7", "0.1.0").unwrap());
        assert!(versions_compatible("1.2.0", "1.2.99").unwrap());
    }

    #[test]
    fn same_version_is_compatible() {
        assert!(versions_compatible("0.1.0", "0.1.0").unwrap());
        assert!(versions_compatible("1.0.0", "1.0.0").unwrap());
    }

    #[test]
    fn different_minor_is_incompatible() {
        assert!(!versions_compatible("0.1.0", "0.2.0").unwrap());
        assert!(!versions_compatible("1.2.0", "1.3.0").unwrap());
    }

    #[test]
    fn different_major_is_incompatible() {
        assert!(!versions_compatible("0.1.0", "1.0.0").unwrap());
        assert!(!versions_compatible("1.0.0", "2.0.0").unwrap());
    }

    #[test]
    fn parse_malformed_rejected() {
        assert!(parse_purgery_version("").is_err());
        assert!(parse_purgery_version("abc").is_err());
        assert!(parse_purgery_version("1.2").is_err());
        assert!(parse_purgery_version("1.2.x").is_err());
    }

    #[test]
    fn parse_with_prerelease() {
        let v = parse_purgery_version("0.1.0-alpha.1").unwrap();
        assert_eq!(v, PurgeryVersion { major: 0, minor: 1, patch: 0 });
    }

    #[test]
    fn parse_with_build_metadata() {
        let v = parse_purgery_version("0.1.0+build.42").unwrap();
        assert_eq!(v, PurgeryVersion { major: 0, minor: 1, patch: 0 });
    }

    #[test]
    fn require_compatible_ok() {
        require_compatible_purgery_version(current_purgery_version(), "test").unwrap();
    }

    #[test]
    fn require_compatible_fails() {
        let err = require_compatible_purgery_version("99.99.0", "test context").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("incompatible Purgery version"));
        assert!(msg.contains("test context"));
        assert!(msg.contains("99.99.0"));
        assert!(msg.contains(current_purgery_version()));
    }

    #[test]
    fn version_display() {
        let v = PurgeryVersion { major: 0, minor: 1, patch: 7 };
        assert_eq!(v.to_string(), "0.1.7");
    }
}
