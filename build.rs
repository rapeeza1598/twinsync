//! Embeds the git commit in the binary so the About window can say exactly which
//! build is running. Falls back to "unknown" outside a git checkout.
use std::process::Command;

fn main() {
    let commit = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=TWINSYNC_GIT_COMMIT={commit}");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=.git/HEAD");
    // A commit moves the branch ref (and this log), not HEAD itself.
    println!("cargo:rerun-if-changed=.git/logs/HEAD");
}
