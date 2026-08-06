// Copyright Kani Contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use std::env::var;
use std::process::Command;

fn main() {
    // We want to know what target triple we were built with, but this isn't normally provided to us.
    // Note the difference between:
    // https://doc.rust-lang.org/cargo/reference/environment-variables.html#environment-variables-cargo-sets-for-crates
    // https://doc.rust-lang.org/cargo/reference/environment-variables.html#environment-variables-cargo-sets-for-build-scripts
    // So "repeat" the info from build script (here) to our crate's build environment.
    println!("cargo:rustc-env=TARGET={}", var("TARGET").unwrap());

    // Record the git commit this build was made from, for `--export-json` provenance (see
    // `export_json.rs`'s `kani_commit`/`kani_commit_dirty` fields). "Kani Rust Verifier
    // 0.67.0" alone is not enough to attribute a result to a build: a release build and a
    // dev build have been observed printing the identical version string while differing in
    // what they actually support (e.g. volatile-intrinsic support). `option_env!` at the use
    // site turns a missing/failed probe into `None` -- this must be `null` for a build from
    // a source tarball with no `.git` (e.g. a published release), never a guessed SHA.
    if let Some(sha) = git_head_sha() {
        println!("cargo:rustc-env=KANI_GIT_SHA={sha}");
        // A build from a dirty tree is not the commit it claims to be, so this must be
        // visible alongside the SHA rather than silently reported as that exact commit.
        // Resolve failure-to-determine toward `dirty=true` (the cautious claim), matching
        // this session's established "when in doubt, resolve toward the answer that costs
        // the consumer nothing" rule -- a spurious `dirty` costs nothing, a false `clean`
        // costs provenance.
        println!("cargo:rustc-env=KANI_GIT_DIRTY={}", git_tree_is_dirty());
    }
    // Deliberately no `cargo:rerun-if-changed` directive here (matching the pre-existing
    // `TARGET` line above): the absence means cargo reruns this build script on every build,
    // so the recorded SHA/dirty flag can never go stale relative to what's actually compiled.
}

/// The current commit SHA, or `None` if this isn't a git checkout (e.g. a release source
/// tarball) or the `git` binary isn't available.
fn git_head_sha() -> Option<String> {
    let output = Command::new("git").args(["rev-parse", "HEAD"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if sha.is_empty() { None } else { Some(sha) }
}

/// Whether the working tree has uncommitted changes (staged, unstaged, or untracked) relative
/// to `git_head_sha()`. Only called once a git checkout is already confirmed present, so a
/// failure here is unexpected; it resolves to `true` (the cautious claim) rather than `false`.
fn git_tree_is_dirty() -> bool {
    Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .map(|output| !output.status.success() || !output.stdout.is_empty())
        .unwrap_or(true)
}
