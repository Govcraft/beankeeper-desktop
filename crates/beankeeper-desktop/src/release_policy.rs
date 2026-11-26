//! The release policy shared by the build script and Desktop.

use std::{fmt, io, num::NonZeroU32};

/// The positive major version covered by a Desktop license.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MajorVersion(NonZeroU32);

impl MajorVersion {
    pub(crate) const fn new(value: NonZeroU32) -> Self {
        Self(value)
    }
}

impl TryFrom<u64> for MajorVersion {
    type Error = InvalidMajorVersion;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        u32::try_from(value)
            .ok()
            .and_then(NonZeroU32::new)
            .map(Self::new)
            .ok_or(InvalidMajorVersion)
    }
}

impl From<MajorVersion> for u32 {
    fn from(value: MajorVersion) -> Self {
        value.0.get()
    }
}

impl fmt::Display for MajorVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// A major version outside the range supported by Desktop licenses.
#[derive(Debug)]
pub struct InvalidMajorVersion;

impl fmt::Display for InvalidMajorVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Desktop licenses require a positive major version that fits in a u32")
    }
}

impl std::error::Error for InvalidMajorVersion {}

#[derive(Clone, Copy)]
pub(crate) enum EnforcementPolicy {
    Free,
    Enforced(MajorVersion),
}

pub(crate) struct ReleaseMetadata<'a> {
    pub(crate) official: bool,
    pub(crate) major: &'a str,
    pub(crate) prerelease: &'a str,
}

impl TryFrom<ReleaseMetadata<'_>> for EnforcementPolicy {
    type Error = io::Error;

    fn try_from(metadata: ReleaseMetadata<'_>) -> Result<Self, Self::Error> {
        // Cargo already parsed the full semver. Use its canonical major and prerelease.
        let major = metadata.major.parse::<u64>().map_err(io::Error::other)?;
        if !metadata.official || !metadata.prerelease.is_empty() || major == 0 {
            return Ok(Self::Free);
        }
        MajorVersion::try_from(major)
            .map(Self::Enforced)
            .map_err(io::Error::other)
    }
}
