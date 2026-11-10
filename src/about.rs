//! Facts shown in the About window. Plain data, so they are tested without a UI.

pub const REPO_URL: &str = "https://github.com/rapeeza1598/twinsync";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildInfo {
    pub version: &'static str,
    /// Short git commit the binary was built from, or "unknown" outside a checkout.
    pub commit: &'static str,
    pub profile: &'static str,
    pub arch: &'static str,
    pub schema_version: i64,
}

pub fn build_info() -> BuildInfo {
    BuildInfo {
        version: env!("CARGO_PKG_VERSION"),
        commit: env!("TWINSYNC_GIT_COMMIT"),
        profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        arch: std::env::consts::ARCH,
        schema_version: crate::storage::latest_version(),
    }
}

impl BuildInfo {
    pub fn version_line(&self) -> String {
        format!(
            "Version {} (commit {}, {} build, {})",
            self.version, self.commit, self.profile, self.arch
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_info_matches_the_crate_and_names_a_commit() {
        let b = build_info();
        assert_eq!(b.version, env!("CARGO_PKG_VERSION"));
        assert!(
            b.commit == "unknown" || b.commit.chars().all(|c| c.is_ascii_hexdigit()),
            "{}",
            b.commit
        );
        assert!(b.schema_version >= 1);
        let line = b.version_line();
        assert!(line.contains(b.version) && line.contains(b.commit) && line.contains(b.profile));
    }
}
