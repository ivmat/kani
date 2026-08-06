// Copyright Kani Contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `--export-json`: write a single JSON file describing an entire verification run, for
//! consumption by LLM-driven automation instead of grepping terse text output.
//!
//! Modeled on `sarif.rs`, but answers a different question. SARIF is a defect-reporting
//! format and deliberately drops cover properties (`sarif_level` only maps `Failure`,
//! `Undetermined`, and `Unknown` to a SARIF level; every successful property, including
//! cover satisfaction, is discarded). A fully green Kani run therefore produces a SARIF
//! file with an empty `results` array -- structurally incapable of saying "5/5 verified,
//! 6/6 covers satisfied". This format instead records what was proven, including cover
//! satisfaction: the signal that distinguishes a real proof from a vacuous one. An
//! unsatisfiable cover still reports `VERIFICATION: SUCCESSFUL` with exit code 0, so a
//! consumer that only reads top-level status cannot detect it without this.
//!
//! **Contract for consumers:** a *missing* `--export-json` output file means verification
//! never began at all (e.g. a compilation error, or an invalid `--harness` filter rejected by
//! `determine_targets` before any harness runs). Once verification begins, a file is always
//! written, even if it aborts partway through (`run_status: "ABORTED"`, see
//! `KaniSession::write_export_json_aborted`) -- so "no file" and "a file with `run_status:
//! ABORTED`" are deliberately different, distinguishable states.

use crate::call_cbmc::{ExitStatus, FailedProperties, VerificationStatus, resolve_unwind_value};
use crate::cbmc_output_parser::{CheckStatus, Property};
use crate::harness_runner::HarnessResult;
use crate::session::KaniSession;
use crate::version::KANI_VERSION;
use anyhow::{Context, Result};
use kani_metadata::{AssignsContract, HarnessAttributes, HarnessMetadata, find_proof_harnesses};
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Duration;
use time::OffsetDateTime;
use time::format_description;

/// Represents the version of the `--export-json` schema -- copies the versioned-field idiom
/// used for `FILE_VERSION` in `list/output.rs`.
///
/// `0.1.0` is the shape as first released (i.e. the shape at the point `--export-json`
/// stabilizes, loses its `-Z export-json` gate, and a consumer could actually depend on it).
/// Until then, this branch is unreleased and behind an unstable flag, so no consumer can
/// have observed an earlier shape; iterating the schema pre-release does not warrant bumping
/// this constant. Once released, the semantic-versioning obligation is real and starts here:
/// any shape change made *after* release must increment this (major for a breaking change to
/// an existing field, minor for an additive one like a new field), so that a consumer reading
/// `schema_version` can rely on it having changed whenever the shape did.
const SCHEMA_VERSION: &str = "0.1.0";

// Casing convention for every enum-like string value in this schema (as opposed to object/
// field *names*, which stay snake_case): SCREAMING_SNAKE_CASE, matching the one convention
// this module cannot change -- `CheckStatus`'s pre-existing `#[serde(rename_all =
// "UPPERCASE")]`, which is already embedded pervasively (`PropertyExport.status`,
// `OtherPropertyExport.status`). The one deliberate exception is
// `enabled_unstable_features`, which is kebab-case because it mirrors `-Z` flag spelling, a
// different namespace (CLI syntax) from "internal status enum", not a competing convention
// for the same kind of data.
const STATUS_SUCCESSFUL: &str = "SUCCESSFUL";
const STATUS_FAILED: &str = "FAILED";
const RUN_STATUS_COMPLETED: &str = "COMPLETED";
const RUN_STATUS_ABORTED: &str = "ABORTED";

/// The git commit this `kani-driver` binary was compiled from, set by `build.rs`. `None` for
/// a build outside a git checkout (e.g. a published release source tarball) -- never a
/// guessed SHA. "Kani Rust Verifier 0.67.0" alone cannot attribute a result to a build: a
/// release build and a dev build have been observed printing that identical string while
/// differing in what they actually support.
///
/// This is the git state as of the *last rebuild of this crate*, not necessarily the live
/// working-tree state at the moment `--export-json` runs: `build.rs`'s
/// `watch_git_state_for_rerun` asks cargo to rebuild on a HEAD/branch/staged-index change,
/// but does not (and, short of watching every file in the tree and defeating incremental
/// builds, cannot) watch arbitrary unstaged edits. In practice this means the SHA can lag by
/// at most "changes made since the last `cargo build`", which is the honest limit of what a
/// build-time probe can promise -- see `build.rs` for the exact coverage.
const KANI_GIT_SHA: Option<&str> = option_env!("KANI_GIT_SHA");

/// Whether the working tree had uncommitted changes at build time, set by `build.rs`
/// alongside `KANI_GIT_SHA`. `None` exactly when `KANI_GIT_SHA` is `None` -- there is nothing
/// to be dirty relative to. A build from a dirty tree is not the commit it claims to be.
/// Subject to the same "as of the last rebuild" staleness limit as `KANI_GIT_SHA` above.
const KANI_GIT_DIRTY: Option<&str> = option_env!("KANI_GIT_DIRTY");

/// Everything `ExportedRun::from_harness_results` needs besides the harness results
/// themselves, bundled into one struct to keep that function's signature small (this is
/// already past the point where clippy's `too_many_arguments` would start complaining about a
/// flat parameter list).
struct RunContext {
    cbmc_version: Option<String>,
    kani_commit: Option<&'static str>,
    kani_commit_dirty: Option<bool>,
    /// The `-Z` unstable features enabled for this run, sorted (see `RunOutcome` -- sorting
    /// everything order-dependent is what makes two identical runs produce byte-identical
    /// files, so "did anything change?" is a diff, not a parse). Results produced under
    /// different feature sets are not comparable -- e.g. a quantifier-bearing proof verified
    /// without `-Z quantifiers` is a different claim than one verified with it.
    enabled_unstable_features: Vec<String>,
    harness_selection: HarnessSelectionExport,
    /// The global `--harness-timeout` bound in force for this run, if any. Without this, an
    /// `exit_status` of `TIMEOUT` is uninterpretable: a consumer cannot tell whether 30s or
    /// 30min was exceeded.
    harness_timeout_s: Option<f64>,
    outcome: RunOutcome,
    started_at: OffsetDateTime,
    wall_time: Duration,
}

/// Whether this run completed (in the sense of "verification ran to completion without an
/// unrelated internal crash" -- NOT "every harness passed") or aborted before producing any
/// harness results at all. See `ExportedRun::run_status`/`abort_reason`.
enum RunOutcome {
    Completed,
    Aborted(String),
}

impl KaniSession {
    /// Write the `--export-json` output for a run that ran to completion (successfully or
    /// with harness failures -- "completed" means "verification finished", not "everything
    /// passed"; see `run_status`/`status`). Early-returns (writes nothing) when
    /// `--export-json` was not passed.
    ///
    /// `matched_harnesses` is the post-`determine_targets`, pre-verification harness list
    /// (i.e. what `main.rs` gets back from `determine_targets`, *not* derived from `results`):
    /// with `--fail-fast`, `results` can be truncated to a single harness on the first
    /// failure, which would make deriving the matched set from `results` alone wrongly
    /// report other, unreached-but-genuinely-matched harnesses' filters as unmatched (and
    /// `run_complete` would have nothing correct to compare against).
    ///
    /// `started_at`/`wall_time` describe the whole verification run (all harnesses),
    /// not any single harness -- callers should measure them around
    /// `HarnessRunner::check_all_harnesses`.
    pub fn write_export_json(
        &self,
        matched_harnesses: &[&HarnessMetadata],
        results: &[HarnessResult<'_>],
        started_at: OffsetDateTime,
        wall_time: Duration,
    ) -> Result<()> {
        let Some(path) = &self.args.export_json else { return Ok(()) };
        let ctx =
            self.build_run_context(matched_harnesses, started_at, wall_time, RunOutcome::Completed);
        let resolved_unwinds: Vec<Option<u32>> =
            results.iter().map(|hr| resolve_unwind_value(&self.args, hr.harness)).collect();
        let export = ExportedRun::from_harness_results(results, &resolved_unwinds, ctx);
        write_export_json_file(path, &export)
    }

    /// Write the `--export-json` output for a run that **aborted** before producing any
    /// harness results at all -- e.g. an internal crash during verification unrelated to any
    /// single harness's outcome (`check_all_harnesses` returning `Err` for a reason other
    /// than `--fail-fast`, which is not this path: that already turns into `Ok` with a
    /// partial result). Early-returns (writes nothing) when `--export-json` was not passed.
    ///
    /// This exists so an autonomous consumer waiting on `--export-json` can tell "Kani itself
    /// crashed mid-run" (`run_status: "ABORTED"`, `abort_reason` set) apart from "no file
    /// because we never got this far" (see the module doc comment's contract) -- a *missing*
    /// file is otherwise ambiguous between a compilation failure and a verification crash.
    pub fn write_export_json_aborted(
        &self,
        matched_harnesses: &[&HarnessMetadata],
        started_at: OffsetDateTime,
        wall_time: Duration,
        error: &anyhow::Error,
    ) -> Result<()> {
        let Some(path) = &self.args.export_json else { return Ok(()) };
        let ctx = self.build_run_context(
            matched_harnesses,
            started_at,
            wall_time,
            RunOutcome::Aborted(format!("{error:#}")),
        );
        let export = ExportedRun::from_harness_results(&[], &[], ctx);
        write_export_json_file(path, &export)
    }

    fn build_run_context(
        &self,
        matched_harnesses: &[&HarnessMetadata],
        started_at: OffsetDateTime,
        wall_time: Duration,
        outcome: RunOutcome,
    ) -> RunContext {
        // Sorting happens in `ExportedRun::from_harness_results` (alongside the harness
        // sort), not here, so the diff-stability guarantee holds at the one true
        // construction point regardless of caller discipline.
        let enabled_unstable_features: Vec<String> = self
            .args
            .common_args
            .unstable_features
            .iter()
            .map(|feature| feature.as_ref().to_string())
            .collect();

        RunContext {
            cbmc_version: probe_cbmc_version(),
            kani_commit: KANI_GIT_SHA,
            kani_commit_dirty: KANI_GIT_DIRTY.map(|dirty| dirty == "true"),
            enabled_unstable_features,
            harness_selection: HarnessSelectionExport {
                requested_filters: self.args.harnesses.clone(),
                exact: self.args.exact,
                unmatched_filters: compute_unmatched_filters(
                    &self.args.harnesses,
                    matched_harnesses,
                    self.args.exact,
                ),
                matched_count: matched_harnesses.len(),
            },
            harness_timeout_s: self.args.harness_timeout.map(|t| Duration::from(t).as_secs_f64()),
            outcome,
            started_at,
            wall_time,
        }
    }
}

/// Requested `--harness` filters that matched *zero* harnesses among `matched_harnesses`
/// (the actual post-filtering set for this run), computed via Kani's own matching predicate
/// (`find_proof_harnesses`, the same function `determine_targets` uses to double-check the
/// compiler's filtering) rather than reimplementing the exact-name/unqualified-name/substring
/// rules here, so this cannot silently drift from real filtering behavior.
///
/// With `--exact`, `determine_targets` already bails out with a hard error before
/// verification ever runs if any filter matches nothing -- so in practice this only ever
/// returns non-empty when `--exact` was *not* passed. That asymmetry is deliberate upstream
/// behavior, not a bug in this function: without `--exact`, a filter that matches nothing is
/// otherwise completely silent (`kani --harness A --harness TYPO` verifies only `A`, reports
/// success, and nothing else indicates `TYPO` matched zero harnesses). This is that signal.
///
/// `matched_harnesses` being the union of every filter's matches (not the full, pre-filter
/// crate harness list) does not weaken this: a single filter's match set is always a subset
/// of that union, so re-testing each filter against the union gives the identical answer as
/// testing against the full harness list would.
fn compute_unmatched_filters(
    requested: &[String],
    matched_harnesses: &[&HarnessMetadata],
    exact: bool,
) -> Vec<String> {
    requested
        .iter()
        .filter(|filter| {
            let targets: BTreeSet<&String> = BTreeSet::from([*filter]);
            find_proof_harnesses(&targets, matched_harnesses.iter().copied(), exact).is_empty()
        })
        .cloned()
        .collect()
}

fn write_export_json_file(path: &Path, export: &ExportedRun) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("Failed to create --export-json output directory `{}`", parent.display())
        })?;
    }

    let file = File::create(path).with_context(|| {
        format!("Failed to create --export-json output file `{}`", path.display())
    })?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, export)
        .with_context(|| format!("Failed to write --export-json output to `{}`", path.display()))?;
    writer.write_all(b"\n")?;
    Ok(())
}

/// Invoke the CBMC binary once to record the version that actually ran this session.
/// The driver does not otherwise know this today (the CBMC pin is per-tree, and nothing
/// records which build actually executed). Returns `None` -- never a guess -- if the
/// probe fails for any reason (binary missing, non-zero exit, unparseable output).
fn probe_cbmc_version() -> Option<String> {
    let output = std::process::Command::new("cbmc").arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if version.is_empty() { None } else { Some(version) }
}

#[derive(Serialize)]
struct ExportedRun {
    schema_version: &'static str,
    kani_version: &'static str,
    /// The git commit this build was compiled from. `None` for a build outside a git
    /// checkout (e.g. a published release) -- never a guess. See `KANI_GIT_SHA`.
    kani_commit: Option<&'static str>,
    /// Whether the working tree had uncommitted changes at build time. `None` exactly when
    /// `kani_commit` is `None`. See `KANI_GIT_DIRTY`.
    kani_commit_dirty: Option<bool>,
    /// `"COMPLETED"` or `"ABORTED"` -- see the module doc comment's contract. This is
    /// independent of whether individual harnesses passed (`status`/`summary` cover that);
    /// it answers "did verification itself run to completion".
    run_status: &'static str,
    /// Whether a result was obtained for every harness in `harness_selection.matched_count`.
    /// `false` under `--fail-fast` after the first failure (some matched harnesses were never
    /// attempted) or whenever `run_status` is `"ABORTED"`. A 50-harness run that fails at
    /// harness 3 must not export `summary.total: 3` with nothing indicating 47 were never
    /// attempted -- this is that signal.
    run_complete: bool,
    /// Set only when `run_status` is `"ABORTED"`: the error that aborted verification.
    abort_reason: Option<String>,
    /// `None` only if the `cbmc --version` probe failed; never a guess.
    cbmc_version: Option<String>,
    /// The solver used across this run. See `SolverExport`: unlike a plain `Option<String>`,
    /// this cannot conflate "no harnesses ran" with "solver unknown for some harness" with
    /// "harnesses genuinely used different solvers" into a single ambiguous `null`.
    solver: SolverExport,
    /// The `-Z` unstable features enabled for this run (kebab-case, as passed on the command
    /// line, e.g. `"quantifiers"`), sorted. See `RunContext::enabled_unstable_features`.
    enabled_unstable_features: Vec<String>,
    /// The `--harness` filters requested, whether `--exact` was set, how many harnesses
    /// actually matched, and which requested filters matched zero harnesses -- so a consumer
    /// can directly see under-matching rather than a smaller-than-expected run silently
    /// reporting success for what it did run and saying nothing about what it skipped.
    harness_selection: HarnessSelectionExport,
    /// The `--harness-timeout` bound in force for this run, if any -- without this, an
    /// `exit_status` of `TIMEOUT` on some harness is uninterpretable.
    harness_timeout_s: Option<f64>,
    target: &'static str,
    /// RFC3339-ish UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`) for when harness checking began.
    started_at: String,
    /// Wall-clock duration of the whole run (all harnesses). Do not confuse with any
    /// single harness's `verification_time_s`.
    wall_time_s: f64,
    /// Sorted by `(crate_name, name)`, not runner order (which is nondeterministic under
    /// `--jobs`): two identical runs must produce byte-identical files, so "did anything
    /// change?" is a diff, not a parse.
    harnesses: Vec<HarnessExport>,
    summary: Summary,
}

/// See `ExportedRun::solver`. Every state that could otherwise collapse into a bare `null` is
/// named explicitly: a schema whose entire pitch is removing ambiguity should not itself
/// have an ambiguous `null`.
#[derive(Serialize)]
#[serde(tag = "state", rename_all = "SCREAMING_SNAKE_CASE")]
enum SolverExport {
    /// Every harness in this run resolved to this same, known solver.
    Uniform { solver: String },
    /// No harnesses ran, so there is nothing to be uniform about.
    NoHarnesses,
    /// At least one harness's resolved solver is unknown (e.g. `--cbmc-args` may have
    /// overridden it) -- see that harness's own `resolved_solver`.
    UnknownForSomeHarness,
    /// Harnesses in this run resolved to genuinely different solvers -- see each harness's
    /// own `resolved_solver` for which.
    Mixed,
}

impl SolverExport {
    fn from_harnesses(harnesses: &[HarnessExport]) -> Self {
        if harnesses.is_empty() {
            return SolverExport::NoHarnesses;
        }
        if harnesses.iter().any(|h| h.resolved_solver.is_none()) {
            return SolverExport::UnknownForSomeHarness;
        }
        let mut resolved = harnesses.iter().map(|h| h.resolved_solver.as_deref().unwrap());
        // Safe: `harnesses` is non-empty (checked above), so there is a first element.
        let first = resolved.next().unwrap();
        if resolved.all(|solver| solver == first) {
            SolverExport::Uniform { solver: first.to_string() }
        } else {
            SolverExport::Mixed
        }
    }
}

/// See `ExportedRun::harness_selection`.
#[derive(Serialize)]
struct HarnessSelectionExport {
    /// The raw `--harness` values requested. Empty means no filter was given (every harness
    /// in the crate ran).
    requested_filters: Vec<String>,
    /// Whether `--exact` was set: without it, `requested_filters` are substring matches, so
    /// more harnesses can match than a consumer expects.
    exact: bool,
    /// Requested filters that matched zero harnesses in this run -- see
    /// `compute_unmatched_filters`'s doc comment for why this is necessary (in short: without
    /// `--exact`, nothing else in Kani's output says so, and a consumer would otherwise have
    /// to reimplement Kani's own matching rules to work it out).
    unmatched_filters: Vec<String>,
    /// How many harnesses actually matched (before verification, so unaffected by
    /// `--fail-fast` truncation) -- compare against `ExportedRun::run_complete` /
    /// `summary.total` to know whether every matched harness was actually attempted.
    matched_count: usize,
}

#[derive(Serialize)]
struct HarnessExport {
    name: String,
    /// The crate this harness belongs to. Without this, two harnesses with the same
    /// `pretty_name` in different crates of the same workspace (e.g. `tests::check_foo` in
    /// two separate crates) are indistinguishable to a consumer keying on name alone --
    /// silently merging two distinct harnesses' results.
    crate_name: String,
    file: String,
    line: usize,
    contract: Option<AssignsContract>,
    is_automatically_generated: bool,
    /// Whether this harness actually had a loop contract in force during verification
    /// (already on `HarnessMetadata`, previously omitted here) -- lets a consumer confirm the
    /// contract was used rather than the loop being fully unrolled instead.
    has_loop_contracts: bool,
    /// The harness's `#[kani::*]` attributes as requested by the user (kind, whether it
    /// should panic, the *requested* solver, unwind value, stubs, verified stubs). Compare
    /// against `resolved_solver`/`resolved_unwind` below, which are what actually ran, not
    /// what was asked for.
    attributes: HarnessAttributes,
    status: &'static str,
    verification_time_s: f64,
    /// The solver CBMC actually ran this harness with (CLI `--solver` overrides the
    /// harness `solver` attribute, else the driver default). NOT the same thing as
    /// `attributes.solver`, which is only the request. `None` when `--cbmc-args` may have
    /// smuggled in a different solver flag -- see `VerificationResult::resolved_solver`.
    resolved_solver: Option<String>,
    /// The effective unwind bound CBMC actually ran this harness with, resolved the same way
    /// `handle_solver_args` resolves the solver: `--unwind` (CLI, per-harness) overrides the
    /// harness's own `#[kani::unwind(N)]` (`attributes.unwind_value`), which overrides
    /// `--default-unwind`. Without this, an agent that hits an unwinding-assertion failure
    /// has no way to know the bound it needs to raise: `attributes.unwind_value` alone omits
    /// both CLI overrides entirely.
    resolved_unwind: Option<u32>,
    /// Whether concrete playback generated a replayable unit test for this harness's failure
    /// (already on `VerificationResult`, previously omitted here) -- tells a consumer whether
    /// a test exists to reproduce the failure, instead of regexing stdout for it.
    generated_concrete_test: bool,
    n_properties: usize,
    n_failed: usize,
    /// `result.failed_properties` verbatim: `NONE` / `PANICS_ONLY` / `OTHER` / `ERROR`.
    /// Distinguishes a panic-only failure (investigate the code) from an `ERROR` failure (an
    /// SMT solver itself errored out -- investigate the tool, not the proof). `NONE` for a
    /// harness that aborted before producing any properties (`exit_status` set) too: it is
    /// literally true that no *property*-level failure was identified in that case, even
    /// though something else clearly went wrong -- `exit_status` is what flags that.
    ///
    /// This does NOT further distinguish "insufficient `--unwind`" or "reachable undefined
    /// function" within `OTHER`/`PANICS_ONLY`, even though those call for different actions
    /// (raise `--unwind` and retry, vs. stub the missing function) -- Kani computes that
    /// distinction (`has_failed_unwinding_asserts`/`has_reachable_undefined_functions` in
    /// `cbmc_property_renderer::postprocess_result`) and then discards it; it exists only as
    /// local variables inside that function, never reaching `VerificationResult`. Recovering
    /// it here would mean either duplicating CBMC-message-substring matching outside the one
    /// place that already does it (drift risk, and the exact architecture objection that
    /// sank the previous attempt at this feature), or new plumbing through
    /// `VerificationResult` that does not exist today. Left out rather than approximated;
    /// see also the RFC's `warnings` field, cut for the same underlying reason.
    failure_kind: FailedProperties,
    failed_properties: Vec<PropertyExport>,
    /// Properties that exist because Kani hit a Rust/MIR construct it does not currently
    /// support (`Property::is_unsupported_construct_property`), listed separately from
    /// `failed_properties` even though (when reached) they also appear there with
    /// `status: "FAILURE"`. Without this, an automated consumer cannot distinguish "this
    /// harness found a real bug" (investigate the code) from "Kani cannot model this"
    /// (investigate the tool instead; no amount of harness work fixes it) -- both otherwise
    /// look like an ordinary failed property.
    unsupported_constructs: Vec<PropertyExport>,
    /// Non-vacuity signal for ORDINARY checks -- see `ChecksExport`'s doc comment for why
    /// this, not `covers`, is the field that actually matters most of the time.
    checks: ChecksExport,
    /// Non-vacuity signal for `kani::cover!` properties. SARIF drops cover properties
    /// entirely; an unsatisfiable cover still reports `VERIFICATION: SUCCESSFUL` with exit
    /// code 0, so `status` alone cannot tell a consumer whether a proof was vacuous.
    /// `unsatisfiable` lists the property identities, not just a count, so a consumer can
    /// tell *which* cover(s) went vacuous.
    covers: CoversExport,
    /// Present (non-null) only when CBMC produced no parsed properties at all for this
    /// harness (crash, timeout, out-of-memory) -- see `VerificationResult::results`. When
    /// this is set, `n_properties`/`n_failed`/`checks`/`covers` are all zero because there is
    /// nothing to report, not because verification passed.
    exit_status: Option<ExitStatusExport>,
}

/// `ExitStatus`, reshaped for JSON: a single tagged-object shape (`{"kind": "OTHER", "code":
/// 101}`) instead of the derive's two incompatible shapes for the same field (`"TIMEOUT"` as
/// a bare string, `{"Other": 101}` as an object -- one field, two JSON types depending on the
/// variant, plus Rust-PascalCase values sitting beside SCREAMING_SNAKE_CASE ones elsewhere in
/// this schema). `code` is present only where there is one.
#[derive(Serialize)]
struct ExitStatusExport {
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<i32>,
}

impl From<&ExitStatus> for ExitStatusExport {
    fn from(status: &ExitStatus) -> Self {
        match status {
            ExitStatus::Timeout => ExitStatusExport { kind: "TIMEOUT", code: None },
            ExitStatus::OutOfMemory => ExitStatusExport { kind: "OUT_OF_MEMORY", code: None },
            ExitStatus::Other(code) => ExitStatusExport { kind: "OTHER", code: Some(*code) },
        }
    }
}

/// A single CBMC property, projected down to what an automated consumer needs to triage it.
/// Used both for `failed_properties` (status is always `FAILURE`/`ERROR` there, see
/// `HarnessExport::failed_properties`) and `unsupported_constructs` (any status: an
/// unsupported-construct check that was never reached shows up as `SUCCESS`/`UNREACHABLE`,
/// not `FAILURE` -- a consumer wanting only the ones that actually blocked this harness
/// should filter on `status`).
#[derive(Serialize)]
struct PropertyExport {
    id: String,
    description: String,
    class: String,
    file: Option<String>,
    line: Option<String>,
    trace_available: bool,
    /// In `failed_properties`, always `FAILURE` or `ERROR` (see that field's doc comment for
    /// why both are included, and why distinguishing them matters). In
    /// `unsupported_constructs`, any status: a construct Kani flagged as unsupported but that
    /// this harness never actually reached shows up as `SUCCESS`/`UNREACHABLE`, not
    /// `FAILURE` -- this field is how a consumer tells "blocked this proof" apart from
    /// "present in the code, but not on any path this harness explored".
    status: CheckStatus,
}

impl PropertyExport {
    fn from_property(p: &Property) -> Self {
        PropertyExport {
            // The real CBMC property id (not `property_name()`'s display rendering --
            // see `PropertyId::to_cbmc_id`'s doc comment for why they can differ), so a
            // consumer can correlate this against CBMC's own output.
            id: p.property_id.to_cbmc_id(),
            description: p.description.clone(),
            class: p.property_class(),
            file: p.source_location.file.clone(),
            line: p.source_location.line.clone(),
            trace_available: p.trace.is_some(),
            status: p.status,
        }
    }
}

/// A property whose status didn't fit any of `CoversExport`/`ChecksExport`'s named buckets.
/// See `bucket_by_status`.
#[derive(Serialize)]
struct OtherPropertyExport {
    id: String,
    status: CheckStatus,
}

/// Non-vacuity signal for ORDINARY (non-cover, non-`code_coverage`) checks -- the same
/// exhaustive-partition machinery as `CoversExport` (see `bucket_by_status`), because an
/// over-constrained `kani::assume` is the single most common way a Kani proof passes while
/// proving nothing, and it is invisible without this: a harness whose every ordinary check is
/// `UNREACHABLE` (contradictory assumptions upstream, e.g. `kani::assume(x > 200);
/// kani::assume(x < 100)`) reports `status: "SUCCESSFUL"` and `n_failed: 0` today, identically
/// to a harness that actually proved something. Most harnesses have no `kani::cover!` at all,
/// so `covers` alone (the original headline field) is empty in exactly the runs where this
/// distinction matters most -- this field is the one that is populated there instead.
///
/// `success + failure.len() + unreachable.len() + undetermined.len() + error.len() +
/// unknown.len() + other.len() == total` by construction, exactly as for `CoversExport`.
#[derive(Serialize)]
struct ChecksExport {
    total: usize,
    success: usize,
    failure: Vec<String>,
    unreachable: Vec<String>,
    undetermined: Vec<String>,
    error: Vec<String>,
    unknown: Vec<String>,
    other: Vec<OtherPropertyExport>,
}

impl ChecksExport {
    fn from_properties(properties: &[Property]) -> Self {
        let checks: Vec<&Property> = properties
            .iter()
            .filter(|p| !p.is_cover_property() && !p.is_code_coverage_property())
            .collect();
        let b = bucket_by_status(&checks, CheckStatus::Success, CheckStatus::Failure);
        ChecksExport {
            total: b.total,
            success: b.good,
            failure: b.bad,
            unreachable: b.unreachable,
            undetermined: b.undetermined,
            error: b.error,
            unknown: b.unknown,
            other: b.other,
        }
    }
}

/// Non-vacuity signal, grouped by CBMC's own cover-property vocabulary (see
/// `cbmc_property_renderer::format_result`, which tracks four named buckets:
/// `number_covers_satisfied`, `number_covers_unsatisfiable`, `number_covers_unreachable`,
/// `number_covers_undetermined`) plus two more this module adds for completeness. Kept as
/// separate lists rather than one merged "not satisfied" bucket because "dead code"
/// (`unreachable`), "logically impossible" (`unsatisfiable`), "the solver couldn't determine
/// it" (`undetermined`), and "inconclusive because a *different* check failed"
/// (`unknown` -- see below) are different diagnoses -- a consumer should not have to guess
/// which one they got.
///
/// `satisfied + unsatisfiable.len() + unreachable.len() + undetermined.len() + error.len() +
/// unknown.len() + other.len() == total` by construction, via the same `bucket_by_status`
/// machinery `ChecksExport` uses.
#[derive(Serialize)]
struct CoversExport {
    total: usize,
    satisfied: usize,
    unsatisfiable: Vec<String>,
    unreachable: Vec<String>,
    undetermined: Vec<String>,
    error: Vec<String>,
    unknown: Vec<String>,
    other: Vec<OtherPropertyExport>,
}

impl CoversExport {
    fn from_properties(properties: &[Property]) -> Self {
        let covers: Vec<&Property> = properties.iter().filter(|p| p.is_cover_property()).collect();
        let b = bucket_by_status(&covers, CheckStatus::Satisfied, CheckStatus::Unsatisfiable);
        CoversExport {
            total: b.total,
            satisfied: b.good,
            unsatisfiable: b.bad,
            unreachable: b.unreachable,
            undetermined: b.undetermined,
            error: b.error,
            unknown: b.unknown,
            other: b.other,
        }
    }
}

/// The output of `bucket_by_status`: every property lands in exactly one field, which is what
/// makes `good + bad.len() + unreachable.len() + undetermined.len() + error.len() +
/// unknown.len() + other.len() == total` hold unconditionally, rather than merely for the
/// statuses anticipated when this was written.
struct StatusBuckets {
    total: usize,
    good: usize,
    bad: Vec<String>,
    unreachable: Vec<String>,
    undetermined: Vec<String>,
    error: Vec<String>,
    unknown: Vec<String>,
    other: Vec<OtherPropertyExport>,
}

/// Shared partitioning behind `CoversExport` and `ChecksExport`, so the exhaustiveness
/// invariant only has to be implemented -- and tested -- once. `good_status` is the status
/// that means "as expected" (`Satisfied` for covers, `Success` for ordinary checks);
/// `bad_status` is its opposite (`Unsatisfiable`, `Failure`). `Unreachable`/`Undetermined`/
/// `Error`/`Unknown` get their own buckets regardless of domain, since they mean the same
/// thing either way; anything else -- including a status that happens to coincide with
/// neither `good_status` nor `bad_status` nor one of those four -- falls into `other` (id +
/// its actual status), never silently inflating `total` while landing nowhere.
fn bucket_by_status(
    properties: &[&Property],
    good_status: CheckStatus,
    bad_status: CheckStatus,
) -> StatusBuckets {
    let total = properties.len();
    let mut good = 0;
    let mut bad = Vec::new();
    let mut unreachable = Vec::new();
    let mut undetermined = Vec::new();
    let mut error = Vec::new();
    let mut unknown = Vec::new();
    let mut other = Vec::new();

    for p in properties {
        let id = p.property_id.to_cbmc_id();
        let status = p.status;
        if status == good_status {
            good += 1;
        } else if status == bad_status {
            bad.push(id);
        } else {
            match status {
                CheckStatus::Unreachable => unreachable.push(id),
                CheckStatus::Undetermined => undetermined.push(id),
                CheckStatus::Error => error.push(id),
                CheckStatus::Unknown => unknown.push(id),
                _ => other.push(OtherPropertyExport { id, status }),
            }
        }
    }

    StatusBuckets { total, good, bad, unreachable, undetermined, error, unknown, other }
}

#[derive(Serialize)]
struct Summary {
    total: usize,
    successful: usize,
    failed: usize,
    checks_total: usize,
    checks_success: usize,
    covers_total: usize,
    covers_satisfied: usize,
}

impl ExportedRun {
    fn from_harness_results(
        results: &[HarnessResult<'_>],
        resolved_unwinds: &[Option<u32>],
        mut ctx: RunContext,
    ) -> Self {
        assert_eq!(
            results.len(),
            resolved_unwinds.len(),
            "resolved_unwinds must be computed 1:1 with results"
        );
        let mut harnesses: Vec<HarnessExport> = results
            .iter()
            .zip(resolved_unwinds.iter().copied())
            .map(|(hr, resolved_unwind)| HarnessExport::from_harness_result(hr, resolved_unwind))
            .collect();
        // Deterministic order: runner order follows `--jobs` scheduling (nondeterministic
        // under parallelism), but two identical runs must produce byte-identical files. Same
        // reasoning for the feature list below: sorted here, at the one true construction
        // point, rather than relying on every caller to have pre-sorted its input.
        harnesses.sort_by(|a, b| (&a.crate_name, &a.name).cmp(&(&b.crate_name, &b.name)));
        ctx.enabled_unstable_features.sort();

        let solver = SolverExport::from_harnesses(&harnesses);

        let successful = harnesses.iter().filter(|h| h.status == STATUS_SUCCESSFUL).count();
        let failed = harnesses.len() - successful;
        let checks_total: usize = harnesses.iter().map(|h| h.checks.total).sum();
        let checks_success: usize = harnesses.iter().map(|h| h.checks.success).sum();
        let covers_total: usize = harnesses.iter().map(|h| h.covers.total).sum();
        let covers_satisfied: usize = harnesses.iter().map(|h| h.covers.satisfied).sum();

        let (run_status, abort_reason) = match ctx.outcome {
            RunOutcome::Completed => (RUN_STATUS_COMPLETED, None),
            RunOutcome::Aborted(reason) => (RUN_STATUS_ABORTED, Some(reason)),
        };
        let run_complete = results.len() == ctx.harness_selection.matched_count;

        ExportedRun {
            schema_version: SCHEMA_VERSION,
            kani_version: KANI_VERSION,
            kani_commit: ctx.kani_commit,
            kani_commit_dirty: ctx.kani_commit_dirty,
            run_status,
            run_complete,
            abort_reason,
            cbmc_version: ctx.cbmc_version,
            solver,
            enabled_unstable_features: ctx.enabled_unstable_features,
            harness_selection: ctx.harness_selection,
            harness_timeout_s: ctx.harness_timeout_s,
            target: env!("TARGET"),
            started_at: format_started_at(ctx.started_at),
            wall_time_s: ctx.wall_time.as_secs_f64(),
            harnesses,
            summary: Summary {
                total: results.len(),
                successful,
                failed,
                checks_total,
                checks_success,
                covers_total,
                covers_satisfied,
            },
        }
    }
}

fn format_started_at(dt: OffsetDateTime) -> String {
    // Deliberately not `time::format_description::well_known::Rfc3339`: this crate is
    // already pulling in a hand-rolled format description elsewhere (see the
    // `kanicov_<date>` timestamp in `main.rs`), so this matches that idiom instead of
    // introducing a new feature-gated dependency surface.
    let format =
        format_description::parse_borrowed::<2>("[year]-[month]-[day]T[hour]:[minute]:[second]Z")
            .unwrap();
    dt.format(&format).unwrap()
}

impl HarnessExport {
    fn from_harness_result(hr: &HarnessResult<'_>, resolved_unwind: Option<u32>) -> Self {
        let harness = hr.harness;
        let result = &hr.result;

        let (
            status,
            n_properties,
            n_failed,
            failed_properties,
            unsupported_constructs,
            checks,
            covers,
            exit_status,
        ) = match &result.results {
            Ok(properties) => {
                let n_properties = properties.len();
                // Mirrors `call_cbmc::determine_failed_properties`, which keys on
                // exactly these two statuses (an `Error` property fails the harness
                // even with zero `Failure`-status properties -- see the `status` field
                // doc comment on `PropertyExport`). `Undetermined`/`Unknown` do
                // NOT fail a harness in Kani's own determination, so they are
                // deliberately excluded here too.
                let failed_properties: Vec<PropertyExport> = properties
                    .iter()
                    .filter(|p| matches!(p.status, CheckStatus::Failure | CheckStatus::Error))
                    .map(PropertyExport::from_property)
                    .collect();
                let n_failed = failed_properties.len();
                let unsupported_constructs: Vec<PropertyExport> = properties
                    .iter()
                    .filter(|p| p.is_unsupported_construct_property())
                    .map(PropertyExport::from_property)
                    .collect();
                let checks = ChecksExport::from_properties(properties);
                let covers = CoversExport::from_properties(properties);
                let status = if result.status == VerificationStatus::Success {
                    STATUS_SUCCESSFUL
                } else {
                    STATUS_FAILED
                };
                (
                    status,
                    n_properties,
                    n_failed,
                    failed_properties,
                    unsupported_constructs,
                    checks,
                    covers,
                    None,
                )
            }
            Err(exit_status) => (
                STATUS_FAILED,
                0,
                0,
                Vec::new(),
                Vec::new(),
                // Empty by construction, via the same path as the real case, rather
                // than a hand-written empty literal that could drift as fields are
                // added.
                ChecksExport::from_properties(&[]),
                CoversExport::from_properties(&[]),
                Some(ExitStatusExport::from(exit_status)),
            ),
        };

        HarnessExport {
            name: harness.pretty_name.clone(),
            crate_name: harness.crate_name.clone(),
            file: harness.original_file.clone(),
            line: harness.original_start_line,
            contract: harness.contract.clone(),
            is_automatically_generated: harness.is_automatically_generated,
            has_loop_contracts: harness.has_loop_contracts,
            attributes: harness.attributes.clone(),
            status,
            verification_time_s: result.runtime.as_secs_f64(),
            resolved_solver: result.resolved_solver.clone(),
            resolved_unwind,
            generated_concrete_test: result.generated_concrete_test,
            n_properties,
            n_failed,
            failure_kind: result.failed_properties,
            failed_properties,
            unsupported_constructs,
            checks,
            covers,
            exit_status,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call_cbmc::VerificationResult;
    use crate::cbmc_output_parser::{PropertyId, SourceLocation};
    use kani_metadata::{HarnessKind, HarnessMetadata};

    fn harness(pretty: &str) -> HarnessMetadata {
        harness_in_crate(pretty, "krate")
    }

    fn harness_in_crate(pretty: &str, crate_name: &str) -> HarnessMetadata {
        HarnessMetadata {
            pretty_name: pretty.to_string(),
            mangled_name: "mangled".to_string(),
            crate_name: crate_name.to_string(),
            original_file: "src/lib.rs".to_string(),
            original_start_line: 10,
            original_end_line: 20,
            goto_file: None,
            attributes: HarnessAttributes::new(HarnessKind::Proof),
            contract: None,
            has_loop_contracts: false,
            is_automatically_generated: false,
        }
    }

    fn property(class: &str, id: u32, status: CheckStatus) -> Property {
        Property {
            description: format!("{class} check"),
            property_id: PropertyId {
                fn_name: Some("harness".to_string()),
                class: class.to_string(),
                id,
            },
            source_location: SourceLocation {
                file: Some("src/lib.rs".to_string()),
                line: Some("12".to_string()),
                column: Some("3".to_string()),
                function: Some("harness".to_string()),
            },
            status,
            reach: None,
            trace: None,
        }
    }

    fn success_result(
        properties: Vec<Property>,
        resolved_solver: Option<&str>,
    ) -> VerificationResult {
        VerificationResult {
            status: VerificationStatus::Success,
            failed_properties: FailedProperties::None,
            results: Ok(properties),
            runtime: Duration::from_millis(329),
            generated_concrete_test: false,
            coverage_results: None,
            resolved_solver: resolved_solver.map(str::to_string),
        }
    }

    fn started() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_754_500_902).unwrap()
    }

    /// A minimal `RunContext` for tests that don't care about provenance/selection fields.
    /// Override individual fields with struct-update syntax where a test does care.
    /// `matched_count` defaults to 1, matching the common single-harness-result test shape.
    fn test_context() -> RunContext {
        RunContext {
            cbmc_version: None,
            kani_commit: None,
            kani_commit_dirty: None,
            enabled_unstable_features: Vec::new(),
            harness_selection: HarnessSelectionExport {
                requested_filters: Vec::new(),
                exact: false,
                unmatched_filters: Vec::new(),
                matched_count: 1,
            },
            harness_timeout_s: None,
            outcome: RunOutcome::Completed,
            started_at: started(),
            wall_time: Duration::from_millis(1),
        }
    }

    /// Build an `ExportedRun` from a single harness result with the default test context and
    /// no CLI-resolved unwind override -- the common case for tests that don't care about
    /// `resolved_unwind`/`matched_count`/etc.
    fn export_one(hr: HarnessResult<'_>) -> ExportedRun {
        ExportedRun::from_harness_results(&[hr], &[None], test_context())
    }

    /// An all-successful run: no failed properties, and every cover satisfied.
    #[test]
    fn export_all_successful() {
        let h = harness("my_harness");
        let properties = vec![
            property("assertion", 1, CheckStatus::Success),
            property("cover", 1, CheckStatus::Satisfied),
        ];
        let result = success_result(properties, Some("cadical"));
        let hr = HarnessResult { harness: &h, result };

        let ctx = RunContext {
            cbmc_version: Some("CBMC 6.8.0".to_string()),
            wall_time: Duration::from_millis(500),
            ..test_context()
        };
        let export = ExportedRun::from_harness_results(&[hr], &[None], ctx);
        let v = serde_json::to_value(&export).unwrap();

        assert_eq!(v["schema_version"], "0.1.0");
        assert_eq!(v["run_status"], "COMPLETED");
        assert_eq!(v["run_complete"], true);
        assert!(v["abort_reason"].is_null());
        assert_eq!(v["cbmc_version"], "CBMC 6.8.0");
        assert_eq!(v["solver"]["state"], "UNIFORM");
        assert_eq!(v["solver"]["solver"], "cadical");
        assert_eq!(v["summary"]["total"], 1);
        assert_eq!(v["summary"]["successful"], 1);
        assert_eq!(v["summary"]["failed"], 0);
        assert_eq!(v["summary"]["covers_total"], 1);
        assert_eq!(v["summary"]["covers_satisfied"], 1);
        assert_eq!(v["summary"]["checks_total"], 1);
        assert_eq!(v["summary"]["checks_success"], 1);

        let harness_json = &v["harnesses"][0];
        assert_eq!(harness_json["name"], "my_harness");
        assert_eq!(harness_json["crate_name"], "krate");
        assert_eq!(harness_json["status"], "SUCCESSFUL");
        assert_eq!(harness_json["resolved_solver"], "cadical");
        assert_eq!(harness_json["failure_kind"], "NONE");
        assert_eq!(harness_json["n_properties"], 2);
        assert_eq!(harness_json["n_failed"], 0);
        assert!(harness_json["failed_properties"].as_array().unwrap().is_empty());
        assert_eq!(harness_json["covers"]["total"], 1);
        assert_eq!(harness_json["covers"]["satisfied"], 1);
        assert!(harness_json["covers"]["unsatisfiable"].as_array().unwrap().is_empty());
        assert_eq!(harness_json["checks"]["total"], 1);
        assert_eq!(harness_json["checks"]["success"], 1);
        assert!(harness_json["exit_status"].is_null());
    }

    /// A run with a failed (non-cover) property: `failed_properties` must name it, and it
    /// must also show up in `checks.failure`.
    #[test]
    fn export_with_failed_property() {
        let h = harness("my_harness");
        let properties = vec![property("assertion", 1, CheckStatus::Failure)];
        let mut result = success_result(properties, Some("cadical"));
        result.status = VerificationStatus::Failure;
        result.failed_properties = FailedProperties::PanicsOnly;
        let hr = HarnessResult { harness: &h, result };

        let export = export_one(hr);
        let v = serde_json::to_value(&export).unwrap();

        assert!(v["cbmc_version"].is_null());
        let harness_json = &v["harnesses"][0];
        assert_eq!(harness_json["status"], "FAILED");
        assert_eq!(harness_json["failure_kind"], "PANICS_ONLY");
        assert_eq!(harness_json["n_failed"], 1);
        assert_eq!(harness_json["failed_properties"][0]["id"], "harness.assertion.1");
        assert_eq!(harness_json["failed_properties"][0]["class"], "assertion");
        assert_eq!(harness_json["failed_properties"][0]["trace_available"], false);
        assert_eq!(
            harness_json["checks"]["failure"].as_array().unwrap(),
            &vec![serde_json::Value::String("harness.assertion.1".to_string())]
        );
    }

    /// The vacuity case: an unsatisfiable cover in an otherwise-SUCCESSFUL run.
    /// This is the entire point of the `covers` field -- assert it's visible in the JSON
    /// *and* that `status` is still SUCCESSFUL, exactly matching what real CBMC output does.
    #[test]
    fn export_with_unsatisfiable_cover() {
        let h = harness("vacuous_harness");
        let properties = vec![
            property("assertion", 1, CheckStatus::Success),
            property("cover", 1, CheckStatus::Satisfied),
            property("cover", 2, CheckStatus::Unsatisfiable),
        ];
        let result = success_result(properties, Some("cadical"));
        let hr = HarnessResult { harness: &h, result };

        let export = export_one(hr);
        let v = serde_json::to_value(&export).unwrap();

        let harness_json = &v["harnesses"][0];
        // The whole point: still SUCCESSFUL, exactly like CBMC's own exit-0 verdict --
        // but the vacuity is now visible without parsing prose.
        assert_eq!(harness_json["status"], "SUCCESSFUL");
        assert_eq!(harness_json["covers"]["total"], 2);
        assert_eq!(harness_json["covers"]["satisfied"], 1);
        assert_eq!(
            harness_json["covers"]["unsatisfiable"].as_array().unwrap(),
            &vec![serde_json::Value::String("harness.cover.2".to_string())]
        );
        assert_eq!(v["summary"]["successful"], 1);
        assert_eq!(v["summary"]["covers_total"], 2);
        assert_eq!(v["summary"]["covers_satisfied"], 1);
    }

    /// FIX 10, the headline reproduction: an over-constrained `kani::assume` makes every
    /// ordinary check UNREACHABLE while the harness still reports SUCCESSFUL and has no
    /// covers at all. `covers` alone (empty) says nothing; `checks.unreachable` must show it.
    /// This pairing -- SUCCESSFUL status alongside a populated `checks.unreachable` -- is the
    /// feature's entire thesis.
    #[test]
    fn export_checks_unreachable_under_contradictory_assume() {
        let h = harness("check_contradictory_assume");
        // Mirrors: kani::assume(x > 200); kani::assume(x < 100); assert!(x == 42); assert!(x != x);
        let properties = vec![
            property("assertion", 1, CheckStatus::Unreachable),
            property("assertion", 2, CheckStatus::Unreachable),
        ];
        let result = success_result(properties, Some("cadical"));
        let hr = HarnessResult { harness: &h, result };

        let export = export_one(hr);
        let v = serde_json::to_value(&export).unwrap();

        let harness_json = &v["harnesses"][0];
        // The pairing that has no test today, per the adversarial review: SUCCESSFUL status
        // co-existing with a fully vacuous set of checks.
        assert_eq!(harness_json["status"], "SUCCESSFUL");
        assert_eq!(harness_json["n_failed"], 0);
        assert!(harness_json["covers"]["total"].as_u64().unwrap() == 0);
        assert!(harness_json["unsupported_constructs"].as_array().unwrap().is_empty());

        let checks = &harness_json["checks"];
        assert_eq!(checks["total"], 2);
        assert_eq!(checks["success"], 0);
        assert_eq!(checks["unreachable"].as_array().unwrap().len(), 2);
        assert_eq!(v["summary"]["checks_total"], 2);
        assert_eq!(v["summary"]["checks_success"], 0);
    }

    /// A check can carry `Error` status -- must be its own named `checks.error` bucket.
    #[test]
    fn export_checks_error_status_bucketed() {
        let h = harness("solver_error_harness");
        let properties = vec![
            property("assertion", 1, CheckStatus::Success),
            property("assertion", 2, CheckStatus::Error),
        ];
        let result = success_result(properties, Some("cadical"));
        let hr = HarnessResult { harness: &h, result };

        let export = export_one(hr);
        let v = serde_json::to_value(&export).unwrap();

        let checks = &v["harnesses"][0]["checks"];
        assert_eq!(checks["total"], 2);
        assert_eq!(checks["success"], 1);
        assert_eq!(
            checks["error"].as_array().unwrap(),
            &vec![serde_json::Value::String("harness.assertion.2".to_string())]
        );
    }

    /// The invariant, structurally: every bucket in `checks` sums to `total`.
    #[test]
    fn export_checks_invariant_holds() {
        let h = harness("mixed_checks_harness");
        let properties = vec![
            property("assertion", 1, CheckStatus::Success),
            property("assertion", 2, CheckStatus::Failure),
            property("assertion", 3, CheckStatus::Unreachable),
            property("assertion", 4, CheckStatus::Undetermined),
            property("assertion", 5, CheckStatus::Error),
            property("assertion", 6, CheckStatus::Unknown),
        ];
        let result = success_result(properties, Some("cadical"));
        let hr = HarnessResult { harness: &h, result };

        let export = export_one(hr);
        let v = serde_json::to_value(&export).unwrap();
        let checks = &v["harnesses"][0]["checks"];

        let sum = checks["success"].as_u64().unwrap()
            + checks["failure"].as_array().unwrap().len() as u64
            + checks["unreachable"].as_array().unwrap().len() as u64
            + checks["undetermined"].as_array().unwrap().len() as u64
            + checks["error"].as_array().unwrap().len() as u64
            + checks["unknown"].as_array().unwrap().len() as u64
            + checks["other"].as_array().unwrap().len() as u64;
        assert_eq!(sum, checks["total"].as_u64().unwrap());
        assert_eq!(checks["total"], 6);
    }

    /// `checks` must not include cover or code_coverage properties (they have their own
    /// dedicated fields).
    #[test]
    fn export_checks_excludes_covers_and_code_coverage() {
        let h = harness("mixed_property_kinds_harness");
        let properties = vec![
            property("assertion", 1, CheckStatus::Success),
            property("cover", 1, CheckStatus::Satisfied),
            property("code_coverage", 1, CheckStatus::Covered),
        ];
        let result = success_result(properties, Some("cadical"));
        let hr = HarnessResult { harness: &h, result };

        let export = export_one(hr);
        let v = serde_json::to_value(&export).unwrap();

        assert_eq!(v["harnesses"][0]["checks"]["total"], 1);
        assert_eq!(v["harnesses"][0]["covers"]["total"], 1);
    }

    /// A harness that produced no properties at all (e.g. CBMC crashed/timed out) must
    /// still be visibly FAILED with a reason, not silently reported as "0/0 covers", and its
    /// `exit_status` must use the tagged shape.
    #[test]
    fn export_with_exit_status_failure() {
        let h = harness("crashed_harness");
        let result = VerificationResult {
            status: VerificationStatus::Failure,
            failed_properties: FailedProperties::None,
            results: Err(ExitStatus::Timeout),
            runtime: Duration::from_secs(300),
            generated_concrete_test: false,
            coverage_results: None,
            resolved_solver: Some("cadical".to_string()),
        };
        let hr = HarnessResult { harness: &h, result };

        let ctx = RunContext { wall_time: Duration::from_secs(300), ..test_context() };
        let export = ExportedRun::from_harness_results(&[hr], &[None], ctx);
        let v = serde_json::to_value(&export).unwrap();

        let harness_json = &v["harnesses"][0];
        assert_eq!(harness_json["status"], "FAILED");
        assert_eq!(harness_json["n_properties"], 0);
        assert_eq!(harness_json["covers"]["total"], 0);
        assert_eq!(harness_json["checks"]["total"], 0);
        assert!(!harness_json["exit_status"].is_null());
        assert_eq!(harness_json["exit_status"]["kind"], "TIMEOUT");
        assert!(harness_json["exit_status"]["code"].is_null());
    }

    /// `ExitStatus::Other`'s tagged shape carries a `code`.
    #[test]
    fn export_exit_status_other_carries_code() {
        let h = harness("crashed_harness");
        let result = VerificationResult {
            status: VerificationStatus::Failure,
            failed_properties: FailedProperties::None,
            results: Err(ExitStatus::Other(101)),
            runtime: Duration::from_secs(1),
            generated_concrete_test: false,
            coverage_results: None,
            resolved_solver: Some("cadical".to_string()),
        };
        let hr = HarnessResult { harness: &h, result };

        let export = export_one(hr);
        let v = serde_json::to_value(&export).unwrap();

        let exit_status = &v["harnesses"][0]["exit_status"];
        assert_eq!(exit_status["kind"], "OTHER");
        assert_eq!(exit_status["code"], 101);
    }

    /// Mixed solvers across harnesses must be reported as the explicit `MIXED` state, not a
    /// bare `null` that could equally mean "no harnesses" or "unknown".
    #[test]
    fn export_solver_mixed_is_explicit() {
        let h1 = harness("h1");
        let h2 = harness("h2");
        let r1 = success_result(vec![], Some("cadical"));
        let r2 = success_result(vec![], Some("kissat"));
        let hr1 = HarnessResult { harness: &h1, result: r1 };
        let hr2 = HarnessResult { harness: &h2, result: r2 };

        let ctx = RunContext {
            harness_selection: HarnessSelectionExport {
                requested_filters: Vec::new(),
                exact: false,
                unmatched_filters: Vec::new(),
                matched_count: 2,
            },
            ..test_context()
        };
        let export = ExportedRun::from_harness_results(&[hr1, hr2], &[None, None], ctx);
        let v = serde_json::to_value(&export).unwrap();
        assert_eq!(v["solver"]["state"], "MIXED");
        assert!(v["solver"].get("solver").is_none());
    }

    /// A run with zero harnesses must report the explicit `NO_HARNESSES` solver state, not a
    /// `null` indistinguishable from "mixed" or "unknown".
    #[test]
    fn export_solver_no_harnesses_is_explicit() {
        let ctx = RunContext {
            harness_selection: HarnessSelectionExport {
                requested_filters: Vec::new(),
                exact: false,
                unmatched_filters: Vec::new(),
                matched_count: 0,
            },
            ..test_context()
        };
        let export = ExportedRun::from_harness_results(&[], &[], ctx);
        let v = serde_json::to_value(&export).unwrap();
        assert_eq!(v["solver"]["state"], "NO_HARNESSES");
        assert_eq!(v["run_complete"], true);
    }

    /// A harness can be FAILED purely because of `Error`-status properties (CBMC returns
    /// `ERROR` when an SMT solver itself errors out), with zero `Failure`-status properties
    /// -- `call_cbmc::determine_failed_properties` treats any `Error` property as failing
    /// the whole harness. `failed_properties` must not come back empty in that case; that
    /// would be `status: "FAILED", failed_properties: []`, the exact silent-lie shape this
    /// feature exists to prevent.
    #[test]
    fn export_with_error_property_is_listed_as_failed() {
        let h = harness("solver_error_harness");
        let properties = vec![
            property("assertion", 1, CheckStatus::Success),
            property("assertion", 2, CheckStatus::Error),
        ];
        let mut result = success_result(properties, Some("cadical"));
        result.status = VerificationStatus::Failure;
        result.failed_properties = FailedProperties::Error;
        let hr = HarnessResult { harness: &h, result };

        let export = export_one(hr);
        let v = serde_json::to_value(&export).unwrap();

        let harness_json = &v["harnesses"][0];
        assert_eq!(harness_json["status"], "FAILED");
        assert_eq!(harness_json["failure_kind"], "ERROR");
        // The real bug: with only the (buggy) `Failure`-only filter this would be empty.
        assert_eq!(harness_json["n_failed"], 1);
        assert!(!harness_json["failed_properties"].as_array().unwrap().is_empty());
        assert_eq!(harness_json["failed_properties"][0]["id"], "harness.assertion.2");
        assert_eq!(harness_json["failed_properties"][0]["status"], "ERROR");
    }

    /// Sums every bucket in a `covers` JSON object -- the structural form of the invariant
    /// "every cover property lands in exactly one bucket", usable regardless of which
    /// statuses actually showed up in a given test.
    fn sum_cover_buckets(covers: &serde_json::Value) -> u64 {
        covers["satisfied"].as_u64().unwrap()
            + covers["unsatisfiable"].as_array().unwrap().len() as u64
            + covers["unreachable"].as_array().unwrap().len() as u64
            + covers["undetermined"].as_array().unwrap().len() as u64
            + covers["error"].as_array().unwrap().len() as u64
            + covers["unknown"].as_array().unwrap().len() as u64
            + covers["other"].as_array().unwrap().len() as u64
    }

    /// Covers can land in any of CBMC's four terminal states, not just satisfied/
    /// unsatisfiable. `unreachable`/`undetermined` must be visible too, each in their own
    /// list (not merged into `unsatisfiable`, since "dead code" and "logically impossible"
    /// are different diagnoses) -- and every bucket together must exactly partition `total`.
    #[test]
    fn export_covers_all_four_states_and_invariant() {
        let h = harness("mixed_covers_harness");
        let properties = vec![
            property("cover", 1, CheckStatus::Satisfied),
            property("cover", 2, CheckStatus::Unsatisfiable),
            property("cover", 3, CheckStatus::Unreachable),
            property("cover", 4, CheckStatus::Undetermined),
        ];
        let result = success_result(properties, Some("cadical"));
        let hr = HarnessResult { harness: &h, result };

        let export = export_one(hr);
        let v = serde_json::to_value(&export).unwrap();

        let covers = &v["harnesses"][0]["covers"];
        assert_eq!(covers["total"], 4);
        assert_eq!(covers["satisfied"], 1);
        assert_eq!(
            covers["unsatisfiable"].as_array().unwrap(),
            &vec![serde_json::Value::String("harness.cover.2".to_string())]
        );
        assert_eq!(
            covers["unreachable"].as_array().unwrap(),
            &vec![serde_json::Value::String("harness.cover.3".to_string())]
        );
        assert_eq!(
            covers["undetermined"].as_array().unwrap(),
            &vec![serde_json::Value::String("harness.cover.4".to_string())]
        );
        assert!(covers["error"].as_array().unwrap().is_empty());
        assert!(covers["unknown"].as_array().unwrap().is_empty());
        assert!(covers["other"].as_array().unwrap().is_empty());

        assert_eq!(sum_cover_buckets(covers), covers["total"].as_u64().unwrap());
    }

    /// A cover property can carry `Error` status (an SMT solver erroring out on that
    /// specific property) -- it must appear in its own named `error` bucket, not vanish or
    /// get merged into `unsatisfiable`.
    #[test]
    fn export_covers_error_status_bucketed() {
        let h = harness("solver_error_cover_harness");
        let properties = vec![
            property("cover", 1, CheckStatus::Satisfied),
            property("cover", 2, CheckStatus::Error),
        ];
        let result = success_result(properties, Some("cadical"));
        let hr = HarnessResult { harness: &h, result };

        let export = export_one(hr);
        let v = serde_json::to_value(&export).unwrap();

        let covers = &v["harnesses"][0]["covers"];
        assert_eq!(covers["total"], 2);
        assert_eq!(
            covers["error"].as_array().unwrap(),
            &vec![serde_json::Value::String("harness.cover.2".to_string())]
        );
        assert_eq!(sum_cover_buckets(covers), covers["total"].as_u64().unwrap());
    }

    /// A cover property can carry `Unknown` status -- and unlike `Error`, this is not an
    /// exotic path: `cbmc_property_renderer::format_result`'s own cover-bucketing match only
    /// recognizes `Undetermined`, not `Unknown`, so any run with a genuine undefined-behavior
    /// failure elsewhere in the harness (an entirely ordinary outcome, not a crash) can come
    /// back with covers left `Unknown`. It must appear in its own named `unknown` bucket, not
    /// the generic `other` catch-all, so a consumer can read "inconclusive because something
    /// else failed" directly.
    #[test]
    fn export_covers_unknown_status_bucketed() {
        let h = harness("undefined_behavior_elsewhere_harness");
        let properties = vec![
            property("cover", 1, CheckStatus::Satisfied),
            property("cover", 2, CheckStatus::Unknown),
        ];
        let result = success_result(properties, Some("cadical"));
        let hr = HarnessResult { harness: &h, result };

        let export = export_one(hr);
        let v = serde_json::to_value(&export).unwrap();

        let covers = &v["harnesses"][0]["covers"];
        assert_eq!(covers["total"], 2);
        assert_eq!(
            covers["unknown"].as_array().unwrap(),
            &vec![serde_json::Value::String("harness.cover.2".to_string())]
        );
        assert!(covers["other"].as_array().unwrap().is_empty());
        assert_eq!(sum_cover_buckets(covers), covers["total"].as_u64().unwrap());
    }

    /// The catch-all: a cover status neither this module nor its `unknown`/`error` buckets
    /// name explicitly must still be accounted for in `other`, carrying both its id and its
    /// actual status, so the invariant holds even for a status nobody anticipated -- rather
    /// than that cover silently inflating `total` while appearing in no list, which is
    /// exactly how the invariant broke before this fix.
    #[test]
    fn export_covers_unexpected_status_goes_to_other() {
        let h = harness("unexpected_cover_status_harness");
        // `Success` never appears on a cover property after Kani's own postprocessing (see
        // `cbmc_property_renderer::update_results_of_cover_checks`), which is exactly why
        // it's a good stand-in here for "a status this module doesn't have a named bucket
        // for".
        let properties = vec![property("cover", 1, CheckStatus::Success)];
        let result = success_result(properties, Some("cadical"));
        let hr = HarnessResult { harness: &h, result };

        let export = export_one(hr);
        let v = serde_json::to_value(&export).unwrap();

        let covers = &v["harnesses"][0]["covers"];
        assert_eq!(covers["total"], 1);
        assert_eq!(covers["other"].as_array().unwrap().len(), 1);
        assert_eq!(covers["other"][0]["id"], "harness.cover.1");
        assert_eq!(covers["other"][0]["status"], "SUCCESS");
        assert_eq!(sum_cover_buckets(covers), covers["total"].as_u64().unwrap());
    }

    /// `is_cover_property()` must not mistake `code_coverage` properties for `cover`
    /// properties (they're deliberately distinct CBMC property classes, produced by
    /// `--coverage`, not `kani::cover!`) -- a `code_coverage` property must not appear in
    /// `covers` or inflate `total`.
    #[test]
    fn export_covers_excludes_code_coverage_properties() {
        let h = harness("code_coverage_harness");
        let properties = vec![
            property("cover", 1, CheckStatus::Satisfied),
            property("code_coverage", 1, CheckStatus::Covered),
        ];
        let result = success_result(properties, Some("cadical"));
        let hr = HarnessResult { harness: &h, result };

        let export = export_one(hr);
        let v = serde_json::to_value(&export).unwrap();

        let covers = &v["harnesses"][0]["covers"];
        assert_eq!(covers["total"], 1);
        assert_eq!(covers["satisfied"], 1);
    }

    /// When `--cbmc-args` may have smuggled in a solver-selecting flag,
    /// `VerificationResult::resolved_solver` is `None` -- confirm that surfaces as
    /// `UNKNOWN_FOR_SOME_HARNESS` (never a guessed value) at the run-wide `solver` field, and
    /// `null` per-harness.
    #[test]
    fn export_unknown_resolved_solver_is_explicit() {
        let h = harness("cbmc_args_solver_harness");
        let result = success_result(vec![], None);
        let hr = HarnessResult { harness: &h, result };

        let export = export_one(hr);
        let v = serde_json::to_value(&export).unwrap();

        assert!(v["harnesses"][0]["resolved_solver"].is_null());
        assert_eq!(v["solver"]["state"], "UNKNOWN_FOR_SOME_HARNESS");
    }

    /// ADD A: an unsupported-construct property must be listed in `unsupported_constructs`
    /// -- separately from an ordinary failed assertion -- so a consumer can tell "Kani cannot
    /// model this" (stop; no harness work fixes it) apart from "this harness found a bug"
    /// (investigate the code), which otherwise look identical (`status: "FAILURE"`).
    #[test]
    fn export_unsupported_construct_listed_separately() {
        let h = harness("volatile_probe_harness");
        let properties = vec![
            property("assertion", 1, CheckStatus::Failure),
            property("unsupported_construct", 1, CheckStatus::Failure),
        ];
        let mut result = success_result(properties, Some("cadical"));
        result.status = VerificationStatus::Failure;
        let hr = HarnessResult { harness: &h, result };

        let export = export_one(hr);
        let v = serde_json::to_value(&export).unwrap();

        let harness_json = &v["harnesses"][0];
        // Reached, so it's a real failure: still counted (and listed) as a failed property...
        assert_eq!(harness_json["n_failed"], 2);
        let failed_ids: Vec<&str> = harness_json["failed_properties"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap())
            .collect();
        assert!(failed_ids.contains(&"harness.assertion.1"));
        assert!(failed_ids.contains(&"harness.unsupported_construct.1"));
        // ... AND separately identifiable as a tool gap, not a proof bug.
        let unsupported = harness_json["unsupported_constructs"].as_array().unwrap();
        assert_eq!(unsupported.len(), 1);
        assert_eq!(unsupported[0]["id"], "harness.unsupported_construct.1");
        assert_eq!(unsupported[0]["status"], "FAILURE");
        // The ordinary assertion must not be misclassified as a tool gap.
        assert!(!unsupported.iter().any(|p| p["id"] == "harness.assertion.1"));
    }

    /// An unsupported-construct check that exists in the harness's search space but was never
    /// actually reached on any explored path shows up as `SUCCESS`, not `FAILURE` -- it must
    /// still be listed (a consumer may want to know the construct is present at all), with
    /// its real status visible so "present but not reached" isn't confused with "blocked
    /// this proof".
    #[test]
    fn export_unreached_unsupported_construct_has_its_real_status() {
        let h = harness("volatile_probe_harness");
        let properties = vec![property("unsupported_construct", 1, CheckStatus::Success)];
        let result = success_result(properties, Some("cadical"));
        let hr = HarnessResult { harness: &h, result };

        let export = export_one(hr);
        let v = serde_json::to_value(&export).unwrap();

        let harness_json = &v["harnesses"][0];
        assert_eq!(harness_json["status"], "SUCCESSFUL");
        assert!(harness_json["failed_properties"].as_array().unwrap().is_empty());
        let unsupported = harness_json["unsupported_constructs"].as_array().unwrap();
        assert_eq!(unsupported.len(), 1);
        assert_eq!(unsupported[0]["status"], "SUCCESS");
    }

    /// ADD B: `kani_commit`/`kani_commit_dirty` must pass through from `RunContext` into the
    /// exported JSON verbatim -- this covers the wiring within this module; the actual
    /// `git rev-parse`/`git status` probing lives in `build.rs` and is covered by a real
    /// end-to-end run, not a unit test.
    #[test]
    fn export_kani_commit_and_dirty_flag() {
        let h = harness("h");
        let hr = HarnessResult { harness: &h, result: success_result(vec![], Some("cadical")) };

        let ctx = RunContext {
            kani_commit: Some("d4df833c8f8f18e632e7b0a7945bb2161f708990"),
            kani_commit_dirty: Some(true),
            ..test_context()
        };
        let export = ExportedRun::from_harness_results(&[hr], &[None], ctx);
        let v = serde_json::to_value(&export).unwrap();

        assert_eq!(v["kani_commit"], "d4df833c8f8f18e632e7b0a7945bb2161f708990");
        assert_eq!(v["kani_commit_dirty"], true);
    }

    /// `kani_commit_dirty` must be `null`, not a guessed `false`, when there's no commit to
    /// be dirty relative to (e.g. a build outside a git checkout).
    #[test]
    fn export_kani_commit_null_when_unavailable() {
        let h = harness("h");
        let hr = HarnessResult { harness: &h, result: success_result(vec![], Some("cadical")) };

        let export = export_one(hr);
        let v = serde_json::to_value(&export).unwrap();

        assert!(v["kani_commit"].is_null());
        assert!(v["kani_commit_dirty"].is_null());
    }

    /// ADD D: the enabled `-Z` unstable features must be recorded, and sorted (diff-
    /// stability), since results produced under different feature sets are not comparable --
    /// e.g. a quantifier-bearing proof verified without `-Z quantifiers` is a different claim
    /// than one verified with it.
    #[test]
    fn export_enabled_unstable_features_sorted() {
        let h = harness("h");
        let hr = HarnessResult { harness: &h, result: success_result(vec![], Some("cadical")) };

        let ctx = RunContext {
            // Deliberately out of alphabetical (and CLI) order.
            enabled_unstable_features: vec![
                "quantifiers".to_string(),
                "function-contracts".to_string(),
            ],
            ..test_context()
        };
        let export = ExportedRun::from_harness_results(&[hr], &[None], ctx);
        let v = serde_json::to_value(&export).unwrap();

        assert_eq!(
            v["enabled_unstable_features"].as_array().unwrap(),
            &vec![
                serde_json::Value::String("function-contracts".to_string()),
                serde_json::Value::String("quantifiers".to_string())
            ]
        );
    }

    /// ADD E: the requested `--harness` filters and whether `--exact` was set must be
    /// recorded, so a consumer can notice a filter that matched fewer harnesses than
    /// intended by comparing this against `harnesses`/`summary.total` -- rather than a
    /// smaller-than-expected run silently reporting success for what it did run and saying
    /// nothing about what it skipped.
    #[test]
    fn export_harness_selection_requested_filters() {
        let h = harness("check_volatile_load_wrapper_contract");
        let hr = HarnessResult { harness: &h, result: success_result(vec![], Some("cadical")) };

        let ctx = RunContext {
            harness_selection: HarnessSelectionExport {
                requested_filters: vec!["check_volatile_load_wrapper_contract".to_string()],
                exact: true,
                unmatched_filters: Vec::new(),
                matched_count: 1,
            },
            ..test_context()
        };
        let export = ExportedRun::from_harness_results(&[hr], &[None], ctx);
        let v = serde_json::to_value(&export).unwrap();

        assert_eq!(
            v["harness_selection"]["requested_filters"].as_array().unwrap(),
            &vec![serde_json::Value::String("check_volatile_load_wrapper_contract".to_string())]
        );
        assert_eq!(v["harness_selection"]["exact"], true);
        assert_eq!(v["harness_selection"]["matched_count"], 1);
    }

    /// No `--harness` filter given (`requested_filters` empty) must not be confused with a
    /// filter that matched nothing -- both `harness_selection.exact` default and empty
    /// `requested_filters` mean "no filter, every harness ran".
    #[test]
    fn export_harness_selection_defaults_to_no_filter() {
        let h = harness("h");
        let hr = HarnessResult { harness: &h, result: success_result(vec![], Some("cadical")) };

        let export = export_one(hr);
        let v = serde_json::to_value(&export).unwrap();

        assert!(v["harness_selection"]["requested_filters"].as_array().unwrap().is_empty());
        assert_eq!(v["harness_selection"]["exact"], false);
        assert!(v["harness_selection"]["unmatched_filters"].as_array().unwrap().is_empty());
    }

    /// The precise scenario this field exists for: a non-exact run with one filter that
    /// matches a real harness and one that matches nothing (a typo). Kani itself is silent
    /// about the typo -- it just verifies the one harness that matched and reports success --
    /// so `unmatched_filters` must name exactly the typo, and the run must still be reported
    /// as successful (this field is informational, not a failure signal).
    #[test]
    fn compute_unmatched_filters_flags_the_nonmatching_one() {
        let h = harness("check_volatile_load_wrapper_contract");
        let matched: Vec<&HarnessMetadata> = vec![&h];
        let requested = vec![
            "check_volatile_load_wrapper_contract".to_string(),
            "check_totally_bogus_typo".to_string(),
        ];

        let unmatched = compute_unmatched_filters(&requested, &matched, false);

        assert_eq!(unmatched, vec!["check_totally_bogus_typo".to_string()]);
    }

    /// A filter that matches via Kani's substring rule (not just an exact name) must not be
    /// flagged as unmatched -- this exercises the actual matching predicate
    /// (`find_proof_harnesses`), not a simplified reimplementation of it.
    #[test]
    fn compute_unmatched_filters_respects_substring_matching() {
        let h = harness("mymod::check_volatile_load_wrapper_contract");
        let matched: Vec<&HarnessMetadata> = vec![&h];
        let requested = vec!["volatile_load".to_string()];

        let unmatched = compute_unmatched_filters(&requested, &matched, false);

        assert!(unmatched.is_empty());
    }

    /// The same substring filter, under `--exact`, must NOT match (it isn't the fully
    /// qualified name) -- and must therefore be reported as unmatched (in real usage,
    /// `determine_targets` would have already bailed with a hard error before this point is
    /// ever reached, but the predicate itself must still be exact-aware).
    #[test]
    fn compute_unmatched_filters_exact_mode_rejects_substring() {
        let h = harness("mymod::check_volatile_load_wrapper_contract");
        let matched: Vec<&HarnessMetadata> = vec![&h];
        let requested = vec!["volatile_load".to_string()];

        let unmatched = compute_unmatched_filters(&requested, &matched, true);

        assert_eq!(unmatched, vec!["volatile_load".to_string()]);
    }

    /// An all-matching request (every filter matches at least one harness) must come back
    /// with an empty `unmatched_filters`.
    #[test]
    fn compute_unmatched_filters_empty_when_everything_matched() {
        let h1 = harness("check_a");
        let h2 = harness("check_b");
        let matched: Vec<&HarnessMetadata> = vec![&h1, &h2];
        let requested = vec!["check_a".to_string(), "check_b".to_string()];

        let unmatched = compute_unmatched_filters(&requested, &matched, false);

        assert!(unmatched.is_empty());
    }

    /// No filter requested at all must never produce any unmatched filters (there is nothing
    /// to have failed to match).
    #[test]
    fn compute_unmatched_filters_empty_when_no_filter_requested() {
        let h = harness("check_a");
        let matched: Vec<&HarnessMetadata> = vec![&h];

        let unmatched = compute_unmatched_filters(&[], &matched, false);

        assert!(unmatched.is_empty());
    }

    /// FIX 9: a run that aborted before producing any results must still write a file, with
    /// `run_status: "ABORTED"` and the error message -- never a bare missing file
    /// indistinguishable from "verification never began".
    #[test]
    fn export_aborted_run_reports_status_and_reason() {
        let ctx = RunContext {
            harness_selection: HarnessSelectionExport {
                requested_filters: Vec::new(),
                exact: false,
                unmatched_filters: Vec::new(),
                matched_count: 3,
            },
            outcome: RunOutcome::Aborted("goto-instrument crashed on harness 2".to_string()),
            ..test_context()
        };
        let export = ExportedRun::from_harness_results(&[], &[], ctx);
        let v = serde_json::to_value(&export).unwrap();

        assert_eq!(v["run_status"], "ABORTED");
        assert_eq!(v["abort_reason"], "goto-instrument crashed on harness 2");
        // 3 harnesses were matched, but 0 results exist: the run is NOT complete.
        assert_eq!(v["run_complete"], false);
        assert!(v["harnesses"].as_array().unwrap().is_empty());
    }

    /// FIX 12: `--fail-fast` truncating `results` to fewer than `matched_count` must be
    /// visible via `run_complete`, not silently absorbed into a `summary.total` that looks
    /// like the whole request.
    #[test]
    fn export_run_incomplete_when_fewer_results_than_matched() {
        let h = harness("h");
        let hr = HarnessResult { harness: &h, result: success_result(vec![], Some("cadical")) };

        let ctx = RunContext {
            harness_selection: HarnessSelectionExport {
                requested_filters: Vec::new(),
                exact: false,
                unmatched_filters: Vec::new(),
                // Pretend 50 harnesses matched, but only 1 result exists (fail-fast stopped
                // after the first failure).
                matched_count: 50,
            },
            ..test_context()
        };
        let export = ExportedRun::from_harness_results(&[hr], &[None], ctx);
        let v = serde_json::to_value(&export).unwrap();

        assert_eq!(v["run_status"], "COMPLETED");
        assert_eq!(v["run_complete"], false);
        assert_eq!(v["summary"]["total"], 1);
        assert_eq!(v["harness_selection"]["matched_count"], 50);
    }

    /// FIX 15/16: harnesses must sort by `(crate_name, name)`, not the order they were
    /// passed in -- runner order is nondeterministic under `--jobs`.
    #[test]
    fn export_harnesses_sorted_by_crate_and_name() {
        let h_z_in_a = harness_in_crate("z_harness", "crate_a");
        let h_a_in_b = harness_in_crate("a_harness", "crate_b");
        let h_a_in_a = harness_in_crate("a_harness", "crate_a");
        let hr1 = HarnessResult { harness: &h_z_in_a, result: success_result(vec![], None) };
        let hr2 = HarnessResult { harness: &h_a_in_b, result: success_result(vec![], None) };
        let hr3 = HarnessResult { harness: &h_a_in_a, result: success_result(vec![], None) };

        let ctx = RunContext {
            harness_selection: HarnessSelectionExport {
                requested_filters: Vec::new(),
                exact: false,
                unmatched_filters: Vec::new(),
                matched_count: 3,
            },
            ..test_context()
        };
        // Deliberately out of sorted order.
        let export = ExportedRun::from_harness_results(&[hr1, hr2, hr3], &[None, None, None], ctx);
        let v = serde_json::to_value(&export).unwrap();

        let names: Vec<(&str, &str)> = v["harnesses"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| (h["crate_name"].as_str().unwrap(), h["name"].as_str().unwrap()))
            .collect();
        assert_eq!(
            names,
            vec![("crate_a", "a_harness"), ("crate_a", "z_harness"), ("crate_b", "a_harness")]
        );
    }

    /// `resolved_unwind` must reflect the CLI-resolved value passed in (computed by
    /// `resolve_unwind_value`, reused rather than reimplemented at the call site in
    /// `write_export_json` -- this test only covers that the value passed through the
    /// pipeline lands in the right field).
    #[test]
    fn export_resolved_unwind_passthrough() {
        let h = harness("h");
        let hr = HarnessResult { harness: &h, result: success_result(vec![], Some("cadical")) };

        let export = ExportedRun::from_harness_results(&[hr], &[Some(7)], test_context());
        let v = serde_json::to_value(&export).unwrap();

        assert_eq!(v["harnesses"][0]["resolved_unwind"], 7);
    }

    /// `resolved_unwind` must be `null`, not a guessed `0`, when no unwind bound was resolved
    /// from any source.
    #[test]
    fn export_resolved_unwind_null_when_unset() {
        let h = harness("h");
        let hr = HarnessResult { harness: &h, result: success_result(vec![], Some("cadical")) };

        let export = export_one(hr);
        let v = serde_json::to_value(&export).unwrap();

        assert!(v["harnesses"][0]["resolved_unwind"].is_null());
    }

    /// `has_loop_contracts`/`generated_concrete_test` must pass through from
    /// `HarnessMetadata`/`VerificationResult` verbatim.
    #[test]
    fn export_has_loop_contracts_and_generated_concrete_test() {
        let mut h = harness("h");
        h.has_loop_contracts = true;
        let mut result = success_result(vec![], Some("cadical"));
        result.generated_concrete_test = true;
        let hr = HarnessResult { harness: &h, result };

        let export = export_one(hr);
        let v = serde_json::to_value(&export).unwrap();

        assert_eq!(v["harnesses"][0]["has_loop_contracts"], true);
        assert_eq!(v["harnesses"][0]["generated_concrete_test"], true);
    }

    /// `harness_timeout_s` must pass through from `RunContext` verbatim, and be `null` when
    /// no `--harness-timeout` was given.
    #[test]
    fn export_harness_timeout_s() {
        let h = harness("h");
        let hr = HarnessResult { harness: &h, result: success_result(vec![], Some("cadical")) };

        let ctx = RunContext { harness_timeout_s: Some(30.0), ..test_context() };
        let export = ExportedRun::from_harness_results(&[hr], &[None], ctx);
        let v = serde_json::to_value(&export).unwrap();
        assert_eq!(v["harness_timeout_s"], 30.0);

        let h2 = harness("h2");
        let hr2 = HarnessResult { harness: &h2, result: success_result(vec![], Some("cadical")) };
        let v2 = serde_json::to_value(export_one(hr2)).unwrap();
        assert!(v2["harness_timeout_s"].is_null());
    }
}
