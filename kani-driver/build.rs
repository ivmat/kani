// Copyright Kani Contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use std::env::var;
use std::path::Path;
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

        // Ask cargo to rerun this script (and thus refresh the SHA/dirty flag) when the git
        // state that produced them changes. See `watch_git_state_for_rerun`'s doc comment for
        // exactly what this does and does not cover -- it is a best-effort freshness measure,
        // not a guarantee, and the doc comments on `KANI_GIT_SHA`/`KANI_GIT_DIRTY` below say so.
        watch_git_state_for_rerun();
    }
    // NOTE: unlike the `TARGET` line above (which has no `rerun-if-changed` because it's a
    // build-environment fact, not a moving target), the git-provenance block above DOES need
    // one: cargo's documented default with no `rerun-if-changed` directive at all is to rerun
    // a build script only when a file *in this package* (kani-driver) changes -- not "every
    // build" as an earlier version of this comment incorrectly claimed. Without an explicit
    // watch on the actual git state, committing a change in a sibling crate (e.g.
    // kani-compiler) and rebuilding would leave this script un-run and `KANI_GIT_SHA` stale:
    // exactly the kind of untrustworthy provenance field this session has spent itself
    // hunting down elsewhere.
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

/// Tells cargo to rerun this build script when the on-disk git state that produced
/// `KANI_GIT_SHA`/`KANI_GIT_DIRTY` changes, so a rebuild after `git commit`/`checkout`/etc.
/// picks up the new state instead of keeping the previous build's stale values baked in.
///
/// Delegates path resolution entirely to `git rev-parse --git-path <name>` rather than
/// hand-parsing `.git`: that correctly handles a plain clone, a linked worktree (where `HEAD`
/// and `index` live under `.git/worktrees/<name>/` but refs are still shared), and other
/// layouts we don't want to reimplement and get subtly wrong.
///
/// **This is a best-effort freshness measure, not a guarantee** -- say so plainly, because a
/// false assurance here is exactly the defect this whole mechanism exists to avoid repeating:
///  * It covers "HEAD now points at a different commit" (checkout, commit, merge, rebase...)
///    via `HEAD` itself and the ref it points to (plus `packed-refs`, for a packed branch).
///  * For `KANI_GIT_DIRTY` specifically, it only covers *staged* changes (`index` changing).
///    A file edited but never `git add`ed touches none of these paths, so cargo has no
///    trigger to rerun this script, and the recorded dirty flag can go stale in exactly that
///    case. Watching the entire working tree would close that gap, but would also mean this
///    script (and therefore a full rebuild trigger) reruns on every single file edit,
///    defeating incremental builds -- so this is a deliberate, bounded gap, not an oversight.
///    In short: `kani_commit`/`kani_commit_dirty` reflect git's state as of the *last rebuild
///    of this crate*, which can lag the true live state between rebuilds.
///  * If none of these paths can be resolved (no git checkout, `git` unavailable), this
///    prints nothing, which is exactly cargo's own default behavior for a build script with
///    no `rerun-if-changed` at all -- a safe fallback, not a special case to handle.
fn watch_git_state_for_rerun() {
    for name in ["HEAD", "packed-refs", "index"] {
        if let Some(path) = git_path(name) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    // HEAD only changes on a branch switch or detached checkout; an ordinary commit on the
    // current branch instead updates the ref HEAD points at (e.g. `refs/heads/main`), which
    // also needs watching. Best-effort: if this can't be read/parsed, HEAD itself is still
    // watched above, which covers at least the "switched branches/commits" case.
    if let Some(head_path) = git_path("HEAD")
        && let Ok(contents) = std::fs::read_to_string(&head_path)
        && let Some(ref_name) = contents.strip_prefix("ref: ").map(str::trim)
        && let Some(ref_path) = git_path(ref_name)
    {
        println!("cargo:rerun-if-changed={ref_path}");
    }
}

/// Resolves `git rev-parse --git-path <name>` for the current checkout. Returns `None`
/// silently on any failure (not a git checkout, `git` unavailable, or the resolved path
/// doesn't currently exist) -- this is a freshness hint, not something the build can depend
/// on succeeding.
fn git_path(name: &str) -> Option<String> {
    let output = Command::new("git").args(["rev-parse", "--git-path", name]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if path.is_empty() || !Path::new(&path).exists() { None } else { Some(path) }
}
