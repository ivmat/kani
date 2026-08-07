// Copyright Kani Contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `--export-json`: one JSON file per verification run, for automated consumption instead of
//! grepping terse text output. Answers Kani issue #942.
//!
//! Contract: the target file is deleted at the start of the run (`write_export_json_file`),
//! before anything is written -- so a file that *exists* at this path was written by this
//! run's completion path, never a stale leftover from an earlier one. A *missing* file means
//! this run did not complete its export: that covers "verification never began" (compilation
//! error, a rejected `--harness` filter) but also an export failure (unwritable path, disk
//! full) and abnormal termination (OOM-kill, Ctrl-C) -- none of those leave a file behind, and
//! none of them are distinguishable from each other by absence alone. An export failure is
//! reported like other write errors but never changes the run's verdict.

use crate::call_cbmc::{ExitStatus, FailedProperties, VerificationStatus, resolve_unwind_value};
use crate::cbmc_output_parser::{CheckStatus, Property};
use crate::harness_runner::HarnessResult;
use crate::sarif::relativize_path;
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

/// `"0.1.0"` while gated behind `-Z export-json`; becomes `"1.0.0"` the moment the gate
/// lifts (a real SemVer promise starts then, not before).
///
/// Field/key naming is snake_case, not kebab-case: this schema embeds `kani_metadata`/
/// `cbmc_output_parser` types verbatim (`HarnessAttributes`, `AssignsContract`, `CheckStatus`)
/// to avoid a third-copy duplication debt, and every *machine-consumed* Kani artifact is
/// snake_case -- only `kani list`'s hand-written JSON is kebab-case. We adopt the substance
/// of its two-field version idiom (`kani_version` + `schema_version`), not its casing.
const SCHEMA_VERSION: &str = "0.1.0";

// One casing convention for every enum-like *value* in this schema (not object/field names,
// which stay snake_case): SCREAMING_SNAKE_CASE, matching `CheckStatus`'s pre-existing
// `#[serde(rename_all = "UPPERCASE")]`, already pervasive via `PropertyExport.status`.
// `enabled_unstable_features` stays kebab-case deliberately: it mirrors `-Z` flag spelling, a
// different namespace from "internal status enum".

/// Git commit this binary was built from (`build.rs`). `None` outside a git checkout, never a
/// guess; may lag the live tree by "changes since the last `cargo build`" -- see `build.rs`.
const KANI_GIT_SHA: Option<&str> = option_env!("KANI_GIT_SHA");
/// Whether the tree was dirty at build time. `None` exactly when `KANI_GIT_SHA` is `None`.
const KANI_GIT_DIRTY: Option<&str> = option_env!("KANI_GIT_DIRTY");

/// Everything `ExportedRun::from_harness_results` needs besides the harness results, bundled
/// to keep that function's signature small.
struct RunContext {
    cbmc_version: Option<String>,
    kani_commit: Option<&'static str>,
    kani_commit_dirty: Option<bool>,
    enabled_unstable_features: Vec<String>,
    harness_selection: HarnessSelectionExport,
    harness_timeout_s: Option<f64>,
    configuration: ConfigurationExport,
    outcome: Outcome,
    started_at: OffsetDateTime,
    wall_time: Duration,
}

impl KaniSession {
    /// Write `--export-json` for a run that ran to completion (harness failures included --
    /// "completed" means "didn't crash", see `outcome`). No-op if `--export-json` unset.
    ///
    /// `matched_harnesses` is the post-`determine_targets`, pre-verification list, not
    /// derived from `results`: `--fail-fast` can truncate `results` to one harness, which
    /// would wrongly report other matched-but-unreached harnesses' filters as unmatched.
    pub fn write_export_json(
        &self,
        matched_harnesses: &[&HarnessMetadata],
        results: &[HarnessResult<'_>],
        started_at: OffsetDateTime,
        wall_time: Duration,
    ) -> Result<()> {
        let Some(path) = &self.args.export_json else { return Ok(()) };
        let ctx = self.build_run_context(
            matched_harnesses,
            started_at,
            wall_time,
            Outcome::Completed { verdict: None },
        );
        let resolved_unwinds: Vec<Option<u32>> =
            results.iter().map(|hr| resolve_unwind_value(&self.args, hr.harness)).collect();
        let export = ExportedRun::from_harness_results(results, &resolved_unwinds, ctx);
        write_export_json_file(path, &export)
    }

    /// Write `--export-json` for a run that **aborted** before producing any results at all
    /// (`check_all_harnesses` returning a genuine `Err`, not `--fail-fast`, which already
    /// yields `Ok` with a partial result). No-op if `--export-json` unset. This is the case a
    /// missing file cannot express: "Kani crashed mid-run" vs. "never got this far".
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
            Outcome::Crashed { code: None, message: Some(format!("{error:#}")) },
        );
        let export = ExportedRun::from_harness_results(&[], &[], ctx);
        write_export_json_file(path, &export)
    }

    fn build_run_context(
        &self,
        matched_harnesses: &[&HarnessMetadata],
        started_at: OffsetDateTime,
        wall_time: Duration,
        outcome: Outcome,
    ) -> RunContext {
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
            configuration: ConfigurationExport {
                checks: ChecksFlags {
                    memory_safety: self.args.checks.memory_safety_on(),
                    overflow: self.args.checks.overflow_on(),
                    unwinding: self.args.checks.unwinding_on(),
                    undefined_function: self.args.checks.undefined_function_on(),
                    assertion_reach_checks: self.args.assertion_reach_checks(),
                },
                // The honest comparability limit: we cannot see what this smuggles (e.g. a
                // different solver, per `cbmc_args_may_override_solver`), so we record it
                // verbatim rather than pretend two runs with different values are comparable.
                cbmc_args: self
                    .args
                    .cbmc_args
                    .iter()
                    .map(|s| s.to_string_lossy().into_owned())
                    .collect(),
            },
            outcome,
            started_at,
            wall_time,
        }
    }
}

/// Filters that matched *zero* harnesses among `matched_harnesses`, via Kani's own matching
/// predicate (`find_proof_harnesses`, the same one `determine_targets` uses) so this cannot
/// drift from real filtering behavior. Only ever non-empty without `--exact` (with it,
/// `determine_targets` already bails before verification runs) -- a substring filter that
/// matches nothing is otherwise completely silent.
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

    // Delete any pre-existing file at this path before writing: without this, a run that dies
    // mid-way (export failure, OOM-kill, Ctrl-C -- everything short of the `File::create` +
    // `to_writer_pretty` + final write below all succeeding) leaves a stale file from an
    // *earlier* run sitting at the target path, indistinguishable from this run's real output.
    // Deleting first makes "a file exists at this path" mean "this run's completion path wrote
    // it" -- see the module doc comment. `NotFound` is the expected common case (no prior run);
    // any other removal failure is reported the same way as other export failures below,
    // without touching the verdict.
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| {
                format!("Failed to remove stale --export-json output file `{}`", path.display())
            });
        }
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

/// Invokes CBMC once for its version (the driver otherwise doesn't know which build ran --
/// the pin is per-tree). `None` -- never a guess -- on any failure.
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
    kani_commit: Option<&'static str>,
    kani_commit_dirty: Option<bool>,
    cbmc_version: Option<String>,
    /// Coarse and non-identifying: never hostname, username, absolute paths, or environment
    /// -- this artifact gets archived and attached to issues.
    machine: MachineExport,
    /// `-Z` unstable features enabled, sorted. Results under different feature sets are not
    /// comparable (e.g. a quantifier proof verified without `-Z quantifiers`).
    enabled_unstable_features: Vec<String>,
    harness_selection: HarnessSelectionExport,
    harness_timeout_s: Option<f64>,
    configuration: ConfigurationExport,
    /// `COMPLETED` unless the run itself crashed before producing any results -- independent
    /// of whether individual harnesses passed (see `summary` for that).
    outcome: Outcome,
    /// Whether a result was obtained for every harness in `harness_selection.matched_count`.
    /// `false` under `--fail-fast`, or whenever `outcome.kind != "COMPLETED"`.
    run_complete: bool,
    target: &'static str,
    started_at: String,
    wall_time_s: f64,
    /// Sorted by `(crate_name, file, line, name)`, not runner order (nondeterministic under
    /// `--jobs`): two identical runs must produce byte-identical files.
    harnesses: Vec<HarnessExport>,
    summary: Summary,
}

#[derive(Serialize)]
struct MachineExport {
    cpu_count: Option<usize>,
    total_memory_bytes: Option<u64>,
    /// The ceiling actually in force (cgroup/ulimit), not installed RAM -- a duration or a
    /// memory figure is not interpretable without it.
    memory_limit_bytes: Option<u64>,
    os: &'static str,
    arch: &'static str,
}

fn probe_machine() -> MachineExport {
    MachineExport {
        cpu_count: std::thread::available_parallelism().ok().map(|n| n.get()),
        total_memory_bytes: linux_total_memory_bytes(),
        memory_limit_bytes: linux_cgroup_memory_limit_bytes(),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
    }
}

#[cfg(target_os = "linux")]
fn linux_total_memory_bytes() -> Option<u64> {
    let contents = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = contents.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}
#[cfg(not(target_os = "linux"))]
fn linux_total_memory_bytes() -> Option<u64> {
    None
}

/// cgroup v2 first (`memory.max`, `"max"` means unlimited), else cgroup v1
/// (`memory.limit_in_bytes`, a near-`i64::MAX` sentinel means unlimited). `None` if neither
/// is readable/parseable, or the limit is unlimited -- never a fabricated ceiling.
#[cfg(target_os = "linux")]
fn linux_cgroup_memory_limit_bytes() -> Option<u64> {
    if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/memory.max") {
        let s = s.trim();
        return if s == "max" { None } else { s.parse().ok() };
    }
    if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/memory/memory.limit_in_bytes") {
        const CGROUP_V1_UNLIMITED_THRESHOLD: u64 = 1 << 62;
        if let Ok(n) = s.trim().parse::<u64>()
            && n < CGROUP_V1_UNLIMITED_THRESHOLD
        {
            return Some(n);
        }
    }
    None
}
#[cfg(not(target_os = "linux"))]
fn linux_cgroup_memory_limit_bytes() -> Option<u64> {
    None
}

#[derive(Serialize)]
struct HarnessSelectionExport {
    /// Raw `--harness` values. Empty means no filter (every harness ran).
    requested_filters: Vec<String>,
    /// Without `--exact`, `requested_filters` are substring matches.
    exact: bool,
    /// Requested filters that matched zero harnesses -- see `compute_unmatched_filters`.
    unmatched_filters: Vec<String>,
    /// Pre-verification match count; compare against `run_complete`/`summary.total`.
    matched_count: usize,
}

#[derive(Serialize)]
struct ConfigurationExport {
    checks: ChecksFlags,
    /// Verbatim `--cbmc-args`: the honest comparability limit (see `build_run_context`).
    cbmc_args: Vec<String>,
}

#[derive(Serialize)]
struct ChecksFlags {
    memory_safety: bool,
    overflow: bool,
    unwinding: bool,
    undefined_function: bool,
    /// Whether Kani inserted reachability checks ahead of ordinary assertions
    /// (`--no-assertion-reach-checks` flips this to `false`). This is the flag the schema's
    /// vacuity signal depends on: with reach-checks off, an assertion made unreachable by a
    /// contradictory `kani::assume` reports as `checks.success` instead of
    /// `checks.unreachable`, silently defeating `ChecksExport`'s vacuity story. Record it so a
    /// consumer can tell the two situations apart.
    assertion_reach_checks: bool,
}

/// Same tagged shape at both harness and run level. `Completed.verdict` is
/// `Some("SUCCESS"|"FAILURE")` for a harness (matching `CheckStatus`'s vocabulary and
/// `VerificationStatus`, not the text renderer's `"SUCCESSFUL"/"FAILED"` -- the consumer of
/// this file never reads the text output), `None` for a run (no single verdict; see
/// `summary`). Always present, never a `null` standing in for the normal case.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
enum Outcome {
    Completed {
        #[serde(skip_serializing_if = "Option::is_none")]
        verdict: Option<&'static str>,
    },
    Timeout,
    OutOfMemory,
    Crashed {
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<i32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

impl From<&ExitStatus> for Outcome {
    fn from(status: &ExitStatus) -> Self {
        match status {
            ExitStatus::Timeout => Outcome::Timeout,
            ExitStatus::OutOfMemory => Outcome::OutOfMemory,
            ExitStatus::Other(code) => Outcome::Crashed { code: Some(*code), message: None },
        }
    }
}

fn is_successful(outcome: &Outcome) -> bool {
    matches!(outcome, Outcome::Completed { verdict: Some(v) } if *v == "SUCCESS")
}

#[derive(Serialize)]
struct HarnessExport {
    name: String,
    /// Distinguishes same-named harnesses in different crates of one workspace.
    crate_name: String,
    /// Repo-relative, matching `sarif.rs` (`relativize_path`) -- we used to leak absolute
    /// paths (usernames, directory layout) into an archived artifact; SARIF already didn't.
    file: String,
    line: usize,
    contract: Option<AssignsContract>,
    is_automatically_generated: bool,
    has_loop_contracts: bool,
    /// Requested attributes (kind, panic expectation, *requested* solver, unwind, stubs).
    /// Compare against `resolved_solver`/`resolved_unwind`, which are what actually ran.
    attributes: HarnessAttributes,
    outcome: Outcome,
    /// Actual solver (CLI `--solver` > harness attribute > default). `None` when
    /// `--cbmc-args` may have smuggled in a different one -- see `resolved_solver` on
    /// `VerificationResult`.
    resolved_solver: Option<String>,
    /// Actual unwind bound (CLI `--unwind` > `#[kani::unwind(N)]` > `--default-unwind`),
    /// reusing `resolve_unwind_value` so this can't drift from what CBMC was actually told.
    resolved_unwind: Option<u32>,
    generated_concrete_test: bool,
    resources: ResourcesExport,
    /// Count of properties this schema accounts for: `checks.total + covers.total`. Excludes
    /// `code_coverage` properties (`--coverage`'s COVERED/UNCOVERED), which this schema does
    /// not export -- see the exclusion in `from_harness_result`.
    n_properties: usize,
    n_failed: usize,
    /// `NONE`/`PANICS_ONLY`/`OTHER`/`ERROR` verbatim from `determine_failed_properties`.
    /// Does not further split `OTHER` into "raise --unwind" vs. "undefined function": those
    /// signals are discarded before reaching `VerificationResult` today (future work).
    failure_kind: FailedProperties,
    failed_properties: Vec<PropertyExport>,
    /// Failures caused by a Rust/MIR construct Kani cannot model -- separate from
    /// `failed_properties` (though a reached one appears in both) because it demands a
    /// different action: fix the tool, not the harness.
    unsupported_constructs: Vec<PropertyExport>,
    /// Raw CBMC `WARNING` messages, free text, non-contractual -- a dropped `forall` means
    /// the obligation was never checked, invisible without this.
    warnings: Vec<WarningExport>,
    checks: ChecksExport,
    covers: CoversExport,
}

#[derive(Serialize)]
struct ResourcesExport {
    verification_time_s: f64,
}

#[derive(Serialize)]
struct WarningExport {
    message: String,
}

#[derive(Serialize)]
struct PropertyExport {
    id: String,
    description: String,
    class: String,
    file: Option<String>,
    line: Option<String>,
    trace_available: bool,
    status: CheckStatus,
}

impl PropertyExport {
    fn from_property(p: &Property) -> Self {
        PropertyExport {
            // Real CBMC id, not `property_name()`'s display rendering -- see
            // `PropertyId::to_cbmc_id`.
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

/// A property whose status didn't fit a named bucket. See `bucket_by_status`.
#[derive(Serialize)]
struct OtherPropertyExport {
    id: String,
    status: CheckStatus,
}

/// Non-vacuity signal for ordinary (non-cover, non-`code_coverage`) checks: an
/// over-constrained `kani::assume` makes every check UNREACHABLE while the harness still
/// reports SUCCESS, and most harnesses have no `kani::cover!` at all, so `covers` alone is
/// empty in exactly the runs where this matters most.
///
/// `success` is a count, not a list: checks are auto-generated and can number in the
/// thousands. Compare `covers.satisfied` below, which is a list -- covers are user-authored
/// and few. Deliberate, principled asymmetry, not an oversight.
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
            success: b.good.len(),
            failure: b.bad,
            unreachable: b.unreachable,
            undetermined: b.undetermined,
            error: b.error,
            unknown: b.unknown,
            other: b.other,
        }
    }
}

/// Non-vacuity signal for `kani::cover!` properties. SARIF drops these entirely; an
/// unsatisfiable cover still reports `VERIFICATION: SUCCESSFUL` with exit 0.
///
/// `satisfied` is an identity list, not a count (see `ChecksExport` for the reverse choice
/// and why): covers are user-authored and few, so naming which ones passed costs nothing and
/// lets a consumer cross-check intent.
#[derive(Serialize)]
struct CoversExport {
    total: usize,
    satisfied: Vec<String>,
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

/// Output of `bucket_by_status`: every property lands in exactly one field, so
/// `good.len() + bad.len() + unreachable.len() + undetermined.len() + error.len() +
/// unknown.len() + other.len() == total` unconditionally.
struct StatusBuckets {
    total: usize,
    good: Vec<String>,
    bad: Vec<String>,
    unreachable: Vec<String>,
    undetermined: Vec<String>,
    error: Vec<String>,
    unknown: Vec<String>,
    other: Vec<OtherPropertyExport>,
}

/// Shared partitioning behind `CoversExport`/`ChecksExport`, so the exhaustiveness invariant
/// is implemented (and tested) once. `good_status`/`bad_status` are the domain-specific
/// "as expected"/"opposite" statuses (`Satisfied`/`Unsatisfiable` for covers,
/// `Success`/`Failure` for checks); `Unreachable`/`Undetermined`/`Error`/`Unknown` get their
/// own buckets regardless of domain; anything else falls into `other` (id + actual status).
fn bucket_by_status(
    properties: &[&Property],
    good_status: CheckStatus,
    bad_status: CheckStatus,
) -> StatusBuckets {
    let total = properties.len();
    let mut good = Vec::new();
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
            good.push(id);
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
        ctx: RunContext,
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
        // Deterministic order: runner order follows `--jobs` scheduling.
        harnesses.sort_by(|a, b| {
            (&a.crate_name, &a.file, a.line, &a.name).cmp(&(
                &b.crate_name,
                &b.file,
                b.line,
                &b.name,
            ))
        });
        let mut enabled_unstable_features = ctx.enabled_unstable_features;
        enabled_unstable_features.sort();

        let successful = harnesses.iter().filter(|h| is_successful(&h.outcome)).count();
        let failed = harnesses.len() - successful;
        let checks_total: usize = harnesses.iter().map(|h| h.checks.total).sum();
        let checks_success: usize = harnesses.iter().map(|h| h.checks.success).sum();
        let covers_total: usize = harnesses.iter().map(|h| h.covers.total).sum();
        let covers_satisfied: usize = harnesses.iter().map(|h| h.covers.satisfied.len()).sum();

        let run_complete = results.len() == ctx.harness_selection.matched_count;

        ExportedRun {
            schema_version: SCHEMA_VERSION,
            kani_version: KANI_VERSION,
            kani_commit: ctx.kani_commit,
            kani_commit_dirty: ctx.kani_commit_dirty,
            cbmc_version: ctx.cbmc_version,
            machine: probe_machine(),
            enabled_unstable_features,
            harness_selection: ctx.harness_selection,
            harness_timeout_s: ctx.harness_timeout_s,
            configuration: ctx.configuration,
            outcome: ctx.outcome,
            run_complete,
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
    // Not `well_known::Rfc3339`: matches the hand-rolled format description idiom already
    // used for the `kanicov_<date>` timestamp in `main.rs`, rather than a new dependency edge.
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
            outcome,
            n_properties,
            n_failed,
            failed_properties,
            unsupported_constructs,
            checks,
            covers,
        ) = match &result.results {
            Ok(properties) => {
                // Excludes `code_coverage` properties (COVERED/UNCOVERED, present only under
                // `--coverage`): they land in neither `checks` (excluded, see
                // `ChecksExport::from_properties`) nor `covers` (a different property class
                // entirely, see `CoversExport::from_properties`), and this schema does not
                // export them at all today. Counting them here without a place to bucket them
                // would break the exhaustive-partition invariant
                // `n_properties == checks.total + covers.total`, so we hold the invariant by
                // scoping `n_properties` to only the properties this schema actually accounts
                // for.
                let n_properties =
                    properties.iter().filter(|p| !p.is_code_coverage_property()).count();
                // Mirrors `call_cbmc::determine_failed_properties`: `Error` fails a
                // harness even with zero `Failure`-status properties.
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
                let verdict = if result.status == VerificationStatus::Success {
                    "SUCCESS"
                } else {
                    "FAILURE"
                };
                (
                    Outcome::Completed { verdict: Some(verdict) },
                    n_properties,
                    n_failed,
                    failed_properties,
                    unsupported_constructs,
                    checks,
                    covers,
                )
            }
            Err(exit_status) => (
                Outcome::from(exit_status),
                0,
                0,
                Vec::new(),
                Vec::new(),
                ChecksExport::from_properties(&[]),
                CoversExport::from_properties(&[]),
            ),
        };

        HarnessExport {
            name: harness.pretty_name.clone(),
            crate_name: harness.crate_name.clone(),
            file: relativize_path(&harness.original_file),
            line: harness.original_start_line,
            contract: harness.contract.clone(),
            is_automatically_generated: harness.is_automatically_generated,
            has_loop_contracts: harness.has_loop_contracts,
            attributes: harness.attributes.clone(),
            outcome,
            resolved_solver: result.resolved_solver.clone(),
            resolved_unwind,
            generated_concrete_test: result.generated_concrete_test,
            resources: ResourcesExport { verification_time_s: result.runtime.as_secs_f64() },
            n_properties,
            n_failed,
            failure_kind: result.failed_properties,
            failed_properties,
            unsupported_constructs,
            warnings: result
                .warnings
                .iter()
                .map(|m| WarningExport { message: m.clone() })
                .collect(),
            checks,
            covers,
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
            warnings: Vec::new(),
        }
    }

    fn started() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_754_500_902).unwrap()
    }

    /// Minimal `RunContext`; override fields with struct-update syntax as needed.
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
            configuration: ConfigurationExport {
                checks: ChecksFlags {
                    memory_safety: true,
                    overflow: true,
                    unwinding: true,
                    undefined_function: true,
                    assertion_reach_checks: true,
                },
                cbmc_args: Vec::new(),
            },
            outcome: Outcome::Completed { verdict: None },
            started_at: started(),
            wall_time: Duration::from_millis(1),
        }
    }

    fn export_one(hr: HarnessResult<'_>) -> ExportedRun {
        ExportedRun::from_harness_results(&[hr], &[None], test_context())
    }

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
        assert_eq!(v["outcome"]["kind"], "COMPLETED");
        assert!(v["outcome"].get("verdict").is_none());
        assert_eq!(v["run_complete"], true);
        assert_eq!(v["cbmc_version"], "CBMC 6.8.0");
        assert!(v.get("solver").is_none(), "top-level solver must be cut entirely");
        assert_eq!(v["summary"]["total"], 1);
        assert_eq!(v["summary"]["successful"], 1);
        assert_eq!(v["summary"]["failed"], 0);
        assert_eq!(v["summary"]["covers_total"], 1);
        assert_eq!(v["summary"]["covers_satisfied"], 1);
        assert_eq!(v["summary"]["checks_total"], 1);
        assert_eq!(v["summary"]["checks_success"], 1);
        assert!(v["machine"]["cpu_count"].as_u64().unwrap() > 0);
        assert!(v["machine"]["os"].is_string());
        assert_eq!(v["configuration"]["checks"]["memory_safety"], true);
        assert!(v["configuration"]["cbmc_args"].as_array().unwrap().is_empty());

        let hj = &v["harnesses"][0];
        assert_eq!(hj["name"], "my_harness");
        assert_eq!(hj["crate_name"], "krate");
        assert_eq!(hj["outcome"]["kind"], "COMPLETED");
        assert_eq!(hj["outcome"]["verdict"], "SUCCESS");
        assert_eq!(hj["resolved_solver"], "cadical");
        assert_eq!(hj["failure_kind"], "NONE");
        assert_eq!(hj["n_properties"], 2);
        assert_eq!(hj["n_failed"], 0);
        assert!(hj["failed_properties"].as_array().unwrap().is_empty());
        assert_eq!(hj["covers"]["total"], 1);
        assert_eq!(
            hj["covers"]["satisfied"].as_array().unwrap(),
            &vec![serde_json::Value::String("harness.cover.1".to_string())]
        );
        assert_eq!(hj["checks"]["total"], 1);
        assert_eq!(hj["checks"]["success"], 1);
        assert_eq!(hj["resources"]["verification_time_s"].as_f64().unwrap(), 0.329);
        assert!(hj["warnings"].as_array().unwrap().is_empty());
    }

    #[test]
    fn export_with_failed_property() {
        let h = harness("my_harness");
        let properties = vec![property("assertion", 1, CheckStatus::Failure)];
        let mut result = success_result(properties, Some("cadical"));
        result.status = VerificationStatus::Failure;
        result.failed_properties = FailedProperties::PanicsOnly;
        let hr = HarnessResult { harness: &h, result };

        let v = serde_json::to_value(export_one(hr)).unwrap();

        assert!(v["cbmc_version"].is_null());
        let hj = &v["harnesses"][0];
        assert_eq!(hj["outcome"]["verdict"], "FAILURE");
        assert_eq!(hj["failure_kind"], "PANICS_ONLY");
        assert_eq!(hj["n_failed"], 1);
        assert_eq!(hj["failed_properties"][0]["id"], "harness.assertion.1");
        assert_eq!(
            hj["checks"]["failure"].as_array().unwrap(),
            &vec![serde_json::Value::String("harness.assertion.1".to_string())]
        );
    }

    /// The headline reproduction: a contradictory `kani::assume` makes every ordinary check
    /// UNREACHABLE while the harness still reports SUCCESS, with no covers involved at all.
    #[test]
    fn export_checks_unreachable_under_contradictory_assume() {
        let h = harness("check_contradictory_assume");
        let properties = vec![
            property("assertion", 1, CheckStatus::Unreachable),
            property("assertion", 2, CheckStatus::Unreachable),
        ];
        let result = success_result(properties, Some("cadical"));
        let hr = HarnessResult { harness: &h, result };

        let v = serde_json::to_value(export_one(hr)).unwrap();

        let hj = &v["harnesses"][0];
        assert_eq!(hj["outcome"]["verdict"], "SUCCESS");
        assert_eq!(hj["n_failed"], 0);
        assert_eq!(hj["covers"]["total"], 0);
        let checks = &hj["checks"];
        assert_eq!(checks["total"], 2);
        assert_eq!(checks["success"], 0);
        assert_eq!(checks["unreachable"].as_array().unwrap().len(), 2);
    }

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

        let v = serde_json::to_value(export_one(hr)).unwrap();
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

        let v = serde_json::to_value(export_one(hr)).unwrap();
        assert_eq!(v["harnesses"][0]["checks"]["total"], 1);
        assert_eq!(v["harnesses"][0]["covers"]["total"], 1);
    }

    /// `--coverage` interleaves `code_coverage` properties (COVERED/UNCOVERED) with ordinary
    /// checks and covers. This schema exports neither in a dedicated bucket, so `n_properties`
    /// must exclude them -- otherwise `n_properties > checks.total + covers.total`, breaking
    /// the exhaustive-partition invariant the two buckets are supposed to hold together.
    #[test]
    fn export_n_properties_excludes_code_coverage_under_coverage() {
        let h = harness("coverage_harness");
        let properties = vec![
            property("assertion", 1, CheckStatus::Success),
            property("cover", 1, CheckStatus::Satisfied),
            property("code_coverage", 1, CheckStatus::Covered),
            property("code_coverage", 2, CheckStatus::Uncovered),
        ];
        let result = success_result(properties, Some("cadical"));
        let hr = HarnessResult { harness: &h, result };

        let v = serde_json::to_value(export_one(hr)).unwrap();
        let hj = &v["harnesses"][0];
        let checks_total = hj["checks"]["total"].as_u64().unwrap();
        let covers_total = hj["covers"]["total"].as_u64().unwrap();
        assert_eq!(hj["n_properties"], 2, "excludes the two code_coverage properties");
        assert_eq!(hj["n_properties"].as_u64().unwrap(), checks_total + covers_total);
    }

    #[test]
    fn export_with_exit_status_outcomes() {
        let h = harness("crashed_harness");
        let mk = |exit: ExitStatus| VerificationResult {
            status: VerificationStatus::Failure,
            failed_properties: FailedProperties::None,
            results: Err(exit),
            runtime: Duration::from_secs(1),
            generated_concrete_test: false,
            coverage_results: None,
            resolved_solver: Some("cadical".to_string()),
            warnings: Vec::new(),
        };

        let timeout_v = serde_json::to_value(export_one(HarnessResult {
            harness: &h,
            result: mk(ExitStatus::Timeout),
        }))
        .unwrap();
        assert_eq!(timeout_v["harnesses"][0]["outcome"]["kind"], "TIMEOUT");
        assert_eq!(timeout_v["harnesses"][0]["covers"]["total"], 0);

        let oom_v = serde_json::to_value(export_one(HarnessResult {
            harness: &h,
            result: mk(ExitStatus::OutOfMemory),
        }))
        .unwrap();
        assert_eq!(oom_v["harnesses"][0]["outcome"]["kind"], "OUT_OF_MEMORY");

        let crashed_v = serde_json::to_value(export_one(HarnessResult {
            harness: &h,
            result: mk(ExitStatus::Other(101)),
        }))
        .unwrap();
        assert_eq!(crashed_v["harnesses"][0]["outcome"]["kind"], "CRASHED");
        assert_eq!(crashed_v["harnesses"][0]["outcome"]["code"], 101);
    }

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

        let v = serde_json::to_value(export_one(hr)).unwrap();
        let hj = &v["harnesses"][0];
        assert_eq!(hj["outcome"]["verdict"], "FAILURE");
        assert_eq!(hj["failure_kind"], "ERROR");
        assert_eq!(hj["n_failed"], 1);
        assert_eq!(hj["failed_properties"][0]["id"], "harness.assertion.2");
        assert_eq!(hj["failed_properties"][0]["status"], "ERROR");
    }

    fn sum_cover_buckets(covers: &serde_json::Value) -> u64 {
        covers["satisfied"].as_array().unwrap().len() as u64
            + covers["unsatisfiable"].as_array().unwrap().len() as u64
            + covers["unreachable"].as_array().unwrap().len() as u64
            + covers["undetermined"].as_array().unwrap().len() as u64
            + covers["error"].as_array().unwrap().len() as u64
            + covers["unknown"].as_array().unwrap().len() as u64
            + covers["other"].as_array().unwrap().len() as u64
    }

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

        let v = serde_json::to_value(export_one(hr)).unwrap();
        let covers = &v["harnesses"][0]["covers"];
        assert_eq!(covers["total"], 4);
        assert_eq!(
            covers["satisfied"].as_array().unwrap(),
            &vec![serde_json::Value::String("harness.cover.1".to_string())]
        );
        assert_eq!(
            covers["unsatisfiable"].as_array().unwrap(),
            &vec![serde_json::Value::String("harness.cover.2".to_string())]
        );
        assert_eq!(sum_cover_buckets(covers), covers["total"].as_u64().unwrap());
    }

    #[test]
    fn export_covers_unexpected_status_goes_to_other() {
        let h = harness("unexpected_cover_status_harness");
        // `Success` never appears on a cover property after Kani's own postprocessing.
        let properties = vec![property("cover", 1, CheckStatus::Success)];
        let result = success_result(properties, Some("cadical"));
        let hr = HarnessResult { harness: &h, result };

        let v = serde_json::to_value(export_one(hr)).unwrap();
        let covers = &v["harnesses"][0]["covers"];
        assert_eq!(covers["other"].as_array().unwrap().len(), 1);
        assert_eq!(covers["other"][0]["status"], "SUCCESS");
        assert_eq!(sum_cover_buckets(covers), covers["total"].as_u64().unwrap());
    }

    #[test]
    fn export_unknown_resolved_solver_is_null() {
        let h = harness("cbmc_args_solver_harness");
        let result = success_result(vec![], None);
        let hr = HarnessResult { harness: &h, result };

        let v = serde_json::to_value(export_one(hr)).unwrap();
        assert!(v["harnesses"][0]["resolved_solver"].is_null());
    }

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

        let v = serde_json::to_value(export_one(hr)).unwrap();
        let hj = &v["harnesses"][0];
        assert_eq!(hj["n_failed"], 2);
        let unsupported = hj["unsupported_constructs"].as_array().unwrap();
        assert_eq!(unsupported.len(), 1);
        assert_eq!(unsupported[0]["id"], "harness.unsupported_construct.1");
        assert!(!unsupported.iter().any(|p| p["id"] == "harness.assertion.1"));
    }

    #[test]
    fn export_kani_commit_and_dirty_flag() {
        let h = harness("h");
        let hr = HarnessResult { harness: &h, result: success_result(vec![], Some("cadical")) };
        let ctx = RunContext {
            kani_commit: Some("d4df833c8f8f18e632e7b0a7945bb2161f708990"),
            kani_commit_dirty: Some(true),
            ..test_context()
        };
        let v =
            serde_json::to_value(ExportedRun::from_harness_results(&[hr], &[None], ctx)).unwrap();
        assert_eq!(v["kani_commit"], "d4df833c8f8f18e632e7b0a7945bb2161f708990");
        assert_eq!(v["kani_commit_dirty"], true);
    }

    #[test]
    fn export_kani_commit_null_when_unavailable() {
        let h = harness("h");
        let hr = HarnessResult { harness: &h, result: success_result(vec![], Some("cadical")) };
        let v = serde_json::to_value(export_one(hr)).unwrap();
        assert!(v["kani_commit"].is_null());
        assert!(v["kani_commit_dirty"].is_null());
    }

    #[test]
    fn export_enabled_unstable_features_sorted() {
        let h = harness("h");
        let hr = HarnessResult { harness: &h, result: success_result(vec![], Some("cadical")) };
        let ctx = RunContext {
            enabled_unstable_features: vec![
                "quantifiers".to_string(),
                "function-contracts".to_string(),
            ],
            ..test_context()
        };
        let v =
            serde_json::to_value(ExportedRun::from_harness_results(&[hr], &[None], ctx)).unwrap();
        assert_eq!(
            v["enabled_unstable_features"].as_array().unwrap(),
            &vec![
                serde_json::Value::String("function-contracts".to_string()),
                serde_json::Value::String("quantifiers".to_string())
            ]
        );
    }

    #[test]
    fn export_harness_selection_and_configuration() {
        let h = harness("check_volatile_load_wrapper_contract");
        let hr = HarnessResult { harness: &h, result: success_result(vec![], Some("cadical")) };
        let ctx = RunContext {
            harness_selection: HarnessSelectionExport {
                requested_filters: vec!["check_volatile_load_wrapper_contract".to_string()],
                exact: true,
                unmatched_filters: Vec::new(),
                matched_count: 1,
            },
            configuration: ConfigurationExport {
                checks: ChecksFlags {
                    memory_safety: false,
                    overflow: true,
                    unwinding: true,
                    undefined_function: true,
                    assertion_reach_checks: false,
                },
                cbmc_args: vec!["--object-bits".to_string(), "16".to_string()],
            },
            ..test_context()
        };
        let v =
            serde_json::to_value(ExportedRun::from_harness_results(&[hr], &[None], ctx)).unwrap();
        assert_eq!(v["harness_selection"]["exact"], true);
        assert_eq!(v["harness_selection"]["matched_count"], 1);
        assert_eq!(v["configuration"]["checks"]["memory_safety"], false);
        assert_eq!(v["configuration"]["checks"]["assertion_reach_checks"], false);
        assert_eq!(
            v["configuration"]["cbmc_args"].as_array().unwrap(),
            &vec![
                serde_json::Value::String("--object-bits".to_string()),
                serde_json::Value::String("16".to_string())
            ]
        );
    }

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

    #[test]
    fn compute_unmatched_filters_respects_substring_matching() {
        let h = harness("mymod::check_volatile_load_wrapper_contract");
        let matched: Vec<&HarnessMetadata> = vec![&h];
        let requested = vec!["volatile_load".to_string()];
        assert!(compute_unmatched_filters(&requested, &matched, false).is_empty());
        assert_eq!(
            compute_unmatched_filters(&requested, &matched, true),
            vec!["volatile_load".to_string()]
        );
    }

    #[test]
    fn export_aborted_run_reports_outcome_and_reason() {
        let ctx = RunContext {
            harness_selection: HarnessSelectionExport {
                requested_filters: Vec::new(),
                exact: false,
                unmatched_filters: Vec::new(),
                matched_count: 3,
            },
            outcome: Outcome::Crashed {
                code: None,
                message: Some("goto-instrument crashed on harness 2".to_string()),
            },
            ..test_context()
        };
        let v = serde_json::to_value(ExportedRun::from_harness_results(&[], &[], ctx)).unwrap();
        assert_eq!(v["outcome"]["kind"], "CRASHED");
        assert_eq!(v["outcome"]["message"], "goto-instrument crashed on harness 2");
        assert_eq!(v["run_complete"], false);
        assert!(v["harnesses"].as_array().unwrap().is_empty());
    }

    #[test]
    fn export_run_incomplete_when_fewer_results_than_matched() {
        let h = harness("h");
        let hr = HarnessResult { harness: &h, result: success_result(vec![], Some("cadical")) };
        let ctx = RunContext {
            harness_selection: HarnessSelectionExport {
                requested_filters: Vec::new(),
                exact: false,
                unmatched_filters: Vec::new(),
                matched_count: 50,
            },
            ..test_context()
        };
        let v =
            serde_json::to_value(ExportedRun::from_harness_results(&[hr], &[None], ctx)).unwrap();
        assert_eq!(v["outcome"]["kind"], "COMPLETED");
        assert_eq!(v["run_complete"], false);
        assert_eq!(v["summary"]["total"], 1);
    }

    #[test]
    fn export_harnesses_sorted_by_crate_file_line_name() {
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
        let v = serde_json::to_value(ExportedRun::from_harness_results(
            &[hr1, hr2, hr3],
            &[None, None, None],
            ctx,
        ))
        .unwrap();
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

    #[test]
    fn export_resolved_unwind_and_has_loop_contracts_and_generated_test() {
        let mut h = harness("h");
        h.has_loop_contracts = true;
        let mut result = success_result(vec![], Some("cadical"));
        result.generated_concrete_test = true;
        let hr = HarnessResult { harness: &h, result };

        let v = serde_json::to_value(ExportedRun::from_harness_results(
            &[hr],
            &[Some(7)],
            test_context(),
        ))
        .unwrap();
        let hj = &v["harnesses"][0];
        assert_eq!(hj["resolved_unwind"], 7);
        assert_eq!(hj["has_loop_contracts"], true);
        assert_eq!(hj["generated_concrete_test"], true);
    }

    #[test]
    fn export_resolved_unwind_null_when_unset() {
        let h = harness("h");
        let hr = HarnessResult { harness: &h, result: success_result(vec![], Some("cadical")) };
        let v = serde_json::to_value(export_one(hr)).unwrap();
        assert!(v["harnesses"][0]["resolved_unwind"].is_null());
    }

    #[test]
    fn export_harness_timeout_s() {
        let h = harness("h");
        let hr = HarnessResult { harness: &h, result: success_result(vec![], Some("cadical")) };
        let ctx = RunContext { harness_timeout_s: Some(30.0), ..test_context() };
        let v =
            serde_json::to_value(ExportedRun::from_harness_results(&[hr], &[None], ctx)).unwrap();
        assert_eq!(v["harness_timeout_s"], 30.0);
    }

    #[test]
    fn export_warnings_passthrough() {
        let h = harness("h");
        let mut result = success_result(vec![], Some("cadical"));
        result.warnings = vec!["ignoring forall".to_string()];
        let hr = HarnessResult { harness: &h, result };
        let v = serde_json::to_value(export_one(hr)).unwrap();
        assert_eq!(v["harnesses"][0]["warnings"][0]["message"], "ignoring forall");
    }

    /// A stale file from an earlier run must not survive: `write_export_json_file` deletes
    /// whatever is at the target path before writing, so its content is always this call's,
    /// never a leftover (the vacuous case a "missing file" contract cannot express).
    #[test]
    fn write_export_json_file_overwrites_stale_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("export.json");
        std::fs::write(&path, b"stale content from an earlier, crashed run").unwrap();

        let h = harness("h");
        let hr = HarnessResult { harness: &h, result: success_result(vec![], Some("cadical")) };
        let export = export_one(hr);
        write_export_json_file(&path, &export).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(!contents.contains("stale content"));
        assert!(contents.contains("\"schema_version\""));
    }

    /// Removal of a genuinely absent file (the common case: no prior run at this path) must
    /// not be treated as an error.
    #[test]
    fn write_export_json_file_succeeds_when_no_stale_file_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fresh").join("export.json");

        let h = harness("h");
        let hr = HarnessResult { harness: &h, result: success_result(vec![], Some("cadical")) };
        let export = export_one(hr);
        write_export_json_file(&path, &export).unwrap();

        assert!(path.exists());
    }

    #[test]
    fn export_file_is_repo_relative() {
        let mut h = harness("h");
        h.original_file =
            std::env::current_dir().unwrap().join("src/foo.rs").to_string_lossy().into_owned();
        let hr = HarnessResult { harness: &h, result: success_result(vec![], Some("cadical")) };
        let v = serde_json::to_value(export_one(hr)).unwrap();
        assert_eq!(v["harnesses"][0]["file"], "src/foo.rs");
    }
}
