//! Bake build metadata (date, git commit) into compile-time env vars
//! so the About dialog can show them without runtime lookups.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // ISO-8601 UTC timestamp. Falls back to "unknown" if `date` is
    // somehow missing — never fails the build.
    let date = Command::new("date")
        .args(["-u", "+%Y-%m-%d %H:%M UTC"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GLASS_BUILD_DATE={date}");

    // Short git commit hash. Falls back to "unknown" outside a git
    // checkout (release tarballs, sdist, etc.).
    let commit = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GLASS_GIT_COMMIT={commit}");

    // Tag (if HEAD is exactly at a tag) or describe output. Falls
    // back to the empty string.
    let tag = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    println!("cargo:rustc-env=GLASS_GIT_DESCRIBE={tag}");

    // Bust the cache if HEAD or the index changes — keeps the
    // baked-in commit fresh without needing `cargo clean`.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
}
