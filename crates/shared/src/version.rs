//! The release version stamped into every shipped binary (ST-M006-014).
//!
//! semantic-release owns versions (REQ-N-018), so every Cargo manifest stays at
//! `0.0.0`. The release workflow determines the next version before it builds
//! and passes it as the `CRETLINK_VERSION` environment variable; it is read here
//! at compile time. Local and CI builds without it report `0.0.0-dev`, so a
//! development build can never be mistaken for a release.

/// The version this build belongs to: the release version for official
/// artifacts, `0.0.0-dev` otherwise.
pub const RELEASE_VERSION: &str = match option_env!("CRETLINK_VERSION") {
    Some(version) if !version.is_empty() => version,
    _ => "0.0.0-dev",
};

#[cfg(test)]
mod tests {
    use super::RELEASE_VERSION;

    #[test]
    fn release_version_is_never_empty() {
        assert!(!RELEASE_VERSION.trim().is_empty());
    }
}
