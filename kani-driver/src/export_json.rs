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

use crate::call_cbmc::{ExitStatus, VerificationStatus};
use crate::cbmc_output_parser::{CheckStatus, Property};
use crate::harness_runner::HarnessResult;
use crate::session::KaniSession;
use crate::version::KANI_VERSION;
use anyhow::{Context, Result};
use kani_metadata::{AssignsContract, HarnessAttributes};
use serde::Serialize;
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

const STATUS_SUCCESSFUL: &str = "SUCCESSFUL";
const STATUS_FAILED: &str = "FAILED";

/// The git commit this `kani-driver` binary was compiled from, set by `build.rs`. `None` for
/// a build outside a git checkout (e.g. a published release source tarball) -- never a
/// guessed SHA. "Kani Rust Verifier 0.67.0" alone cannot attribute a result to a build: a
/// release build and a dev build have been observed printing that identical string while
/// differing in what they actually support.
const KANI_GIT_SHA: Option<&str> = option_env!("KANI_GIT_SHA");

/// Whether the working tree had uncommitted changes at build time, set by `build.rs`
/// alongside `KANI_GIT_SHA`. `None` exactly when `KANI_GIT_SHA` is `None` -- there is nothing
/// to be dirty relative to. A build from a dirty tree is not the commit it claims to be.
const KANI_GIT_DIRTY: Option<&str> = option_env!("KANI_GIT_DIRTY");

/// Everything `ExportedRun::from_harness_results` needs besides the harness results
/// themselves, bundled into one struct to keep that function's signature small (this is
/// already past the point where clippy's `too_many_arguments` would start complaining about a
/// flat parameter list).
struct RunContext {
    cbmc_version: Option<String>,
    kani_commit: Option<&'static str>,
    kani_commit_dirty: Option<bool>,
    /// The `-Z` unstable features enabled for this run. Results produced under different
    /// feature sets are not comparable -- e.g. a quantifier-bearing proof verified without
    /// `-Z quantifiers` is a different claim than one verified with it.
    enabled_unstable_features: Vec<String>,
    harness_selection: HarnessSelectionExport,
    started_at: OffsetDateTime,
    wall_time: Duration,
}

impl KaniSession {
    /// Write the `--export-json` output for this run.
    /// Early-returns (writes nothing) when `--export-json` was not passed.
    ///
    /// `started_at`/`wall_time` describe the whole verification run (all harnesses),
    /// not any single harness -- callers should measure them around
    /// `HarnessRunner::check_all_harnesses`.
    pub fn write_export_json(
        &self,
        results: &[HarnessResult<'_>],
        started_at: OffsetDateTime,
        wall_time: Duration,
    ) -> Result<()> {
        let Some(path) = &self.args.export_json else { return Ok(()) };
        let ctx = RunContext {
            cbmc_version: probe_cbmc_version(),
            kani_commit: KANI_GIT_SHA,
            kani_commit_dirty: KANI_GIT_DIRTY.map(|dirty| dirty == "true"),
            enabled_unstable_features: self
                .args
                .common_args
                .unstable_features
                .iter()
                .map(|feature| feature.as_ref().to_string())
                .collect(),
            harness_selection: HarnessSelectionExport {
                requested_filters: self.args.harnesses.clone(),
                exact: self.args.exact,
            },
            started_at,
            wall_time,
        };
        let export = ExportedRun::from_harness_results(results, ctx);
        write_export_json_file(path, &export)
    }
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
    /// `None` only if the `cbmc --version` probe failed; never a guess.
    cbmc_version: Option<String>,
    /// The solver actually used, IF uniform across every harness in this run.
    /// `None` when no harnesses ran, or when harnesses resolved to different solvers
    /// (a harness-level `solver` attribute can override the run-wide default) -- see each
    /// harness's own `resolved_solver` for the per-harness ground truth in that case.
    solver: Option<String>,
    /// The `-Z` unstable features enabled for this run (kebab-case, as passed on the command
    /// line, e.g. `"quantifiers"`). See `RunContext::enabled_unstable_features`.
    enabled_unstable_features: Vec<String>,
    /// The `--harness` filters requested and whether `--exact` was set, so a consumer can
    /// notice under-matching (a filter that matched fewer harnesses than intended) by
    /// comparing this against `harnesses`/`summary.total`, rather than a smaller-than-
    /// expected run silently reporting success for what it did run and saying nothing about
    /// what it skipped.
    harness_selection: HarnessSelectionExport,
    target: &'static str,
    /// RFC3339-ish UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`) for when harness checking began.
    started_at: String,
    /// Wall-clock duration of the whole run (all harnesses). Do not confuse with any
    /// single harness's `verification_time_s`.
    wall_time_s: f64,
    harnesses: Vec<HarnessExport>,
    summary: Summary,
}

/// See `ExportedRun::harness_selection`.
#[derive(Serialize)]
struct HarnessSelectionExport {
    /// The raw `--harness` values requested. Empty means no filter was given (every harness
    /// in the crate ran).
    requested_filters: Vec<String>,
    /// Whether `--exact` was set: without it, `requested_filters` are substring matches, so
    /// more harnesses can match than a consumer expects; with it, a filter that matches
    /// nothing is already a hard error (`kani-driver` refuses to proceed) rather than a
    /// silent under-match.
    exact: bool,
}

#[derive(Serialize)]
struct HarnessExport {
    name: String,
    file: String,
    line: usize,
    contract: Option<AssignsContract>,
    is_automatically_generated: bool,
    /// The harness's `#[kani::*]` attributes as requested by the user (kind, whether it
    /// should panic, the *requested* solver, unwind value, stubs, verified stubs). Compare
    /// against `resolved_solver` below, which is what actually ran, not what was asked for.
    attributes: HarnessAttributes,
    status: &'static str,
    /// Wall-clock duration of this harness's CBMC subprocess invocation (spawn through
    /// parsed output) -- the same measurement the "Verification Time:" line in regular
    /// text output uses. This is not a lower-level solver-internal timing breakdown (CBMC
    /// does not expose one in structured form; see the design doc's cut `cbmc_stats`).
    verification_time_s: f64,
    /// The solver CBMC actually ran this harness with (CLI `--solver` overrides the
    /// harness `solver` attribute, else the driver default). NOT the same thing as
    /// `attributes.solver`, which is only the request. `None` when `--cbmc-args` may have
    /// smuggled in a different solver flag -- see `VerificationResult::resolved_solver`.
    resolved_solver: Option<String>,
    n_properties: usize,
    n_failed: usize,
    failed_properties: Vec<PropertyExport>,
    /// Properties that exist because Kani hit a Rust/MIR construct it does not currently
    /// support (`Property::is_unsupported_construct_property`), listed separately from
    /// `failed_properties` even though (when reached) they also appear there with
    /// `status: "FAILURE"`. Without this, an automated consumer cannot distinguish "this
    /// harness found a real bug" (investigate the code) from "Kani cannot model this"
    /// (investigate the tool instead; no amount of harness work fixes it) -- both otherwise
    /// look like an ordinary failed property.
    unsupported_constructs: Vec<PropertyExport>,
    /// Non-vacuity signal. SARIF drops cover properties entirely; an unsatisfiable cover
    /// still reports `VERIFICATION: SUCCESSFUL` with exit code 0, so `status` alone cannot
    /// tell a consumer whether a proof was vacuous. `unsatisfiable` lists the property
    /// identities, not just a count, so a consumer can tell *which* cover(s) went vacuous.
    covers: CoversExport,
    /// Present (non-null) only when CBMC produced no parsed properties at all for this
    /// harness (crash, timeout, out-of-memory) -- see `VerificationResult::results`. When
    /// this is set, `n_properties`/`n_failed`/`covers` are all zero because there is
    /// nothing to report, not because verification passed.
    exit_status: Option<ExitStatus>,
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
/// The sum of every bucket below (satisfied's count plus every other list's length) equals
/// `total` **by construction**: `CoversExport::from_properties` puts every cover property
/// into exactly one of these seven buckets, so there is no status a cover property could have
/// that silently escapes the count.
///  * `error` (`CheckStatus::Error`, an SMT solver erroring out on that specific property)
///    and `unknown` (`CheckStatus::Unknown`) are both realistic enough to name explicitly.
///    `unknown` in particular is not exotic: `cbmc_property_renderer::format_result`'s own
///    cover-bucketing match only recognizes `Undetermined`, not `Unknown`, so any run with a
///    genuine undefined-behavior failure elsewhere in the harness -- an entirely ordinary,
///    non-crash outcome -- can come back with covers left `Unknown`, and Kani's own text
///    summary already silently drops those from its cover counts today.
///  * `other` is the true catch-all, carrying both the id and the actual status string, so
///    that even a status nobody anticipated (e.g. if CBMC or Kani's postprocessing ever adds
///    one) still shows up somewhere rather than vanishing from the total.
#[derive(Serialize)]
struct CoversExport {
    total: usize,
    satisfied: usize,
    unsatisfiable: Vec<String>,
    unreachable: Vec<String>,
    undetermined: Vec<String>,
    error: Vec<String>,
    unknown: Vec<String>,
    other: Vec<OtherCoverExport>,
}

/// A cover property whose status didn't fit any of `CoversExport`'s named buckets.
#[derive(Serialize)]
struct OtherCoverExport {
    id: String,
    status: CheckStatus,
}

#[derive(Serialize)]
struct Summary {
    total: usize,
    successful: usize,
    failed: usize,
    covers_total: usize,
    covers_satisfied: usize,
}

impl ExportedRun {
    fn from_harness_results(results: &[HarnessResult<'_>], ctx: RunContext) -> Self {
        let harnesses: Vec<HarnessExport> =
            results.iter().map(HarnessExport::from_harness_result).collect();

        let solver = uniform_solver(&harnesses);

        let successful = harnesses.iter().filter(|h| h.status == STATUS_SUCCESSFUL).count();
        let failed = harnesses.len() - successful;
        let covers_total: usize = harnesses.iter().map(|h| h.covers.total).sum();
        let covers_satisfied: usize = harnesses.iter().map(|h| h.covers.satisfied).sum();

        ExportedRun {
            schema_version: SCHEMA_VERSION,
            kani_version: KANI_VERSION,
            kani_commit: ctx.kani_commit,
            kani_commit_dirty: ctx.kani_commit_dirty,
            cbmc_version: ctx.cbmc_version,
            solver,
            enabled_unstable_features: ctx.enabled_unstable_features,
            harness_selection: ctx.harness_selection,
            target: env!("TARGET"),
            started_at: format_started_at(ctx.started_at),
            wall_time_s: ctx.wall_time.as_secs_f64(),
            harnesses,
            summary: Summary {
                total: results.len(),
                successful,
                failed,
                covers_total,
                covers_satisfied,
            },
        }
    }
}

/// `Some(solver)` only if every harness in this run resolved to a *known* solver, and it's
/// the same one for all of them. `None` (never a guess) if the run was empty, if any
/// harness's own `resolved_solver` is unknown (e.g. `--cbmc-args` may have overridden it),
/// or if harnesses genuinely used different solvers.
fn uniform_solver(harnesses: &[HarnessExport]) -> Option<String> {
    let mut iter = harnesses.iter().map(|h| h.resolved_solver.as_ref());
    let first = iter.next()??;
    if iter.all(|s| s == Some(first)) { Some(first.clone()) } else { None }
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
    fn from_harness_result(hr: &HarnessResult<'_>) -> Self {
        let harness = hr.harness;
        let result = &hr.result;

        let (
            status,
            n_properties,
            n_failed,
            failed_properties,
            unsupported_constructs,
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
                CoversExport::from_properties(&[]),
                Some(*exit_status),
            ),
        };

        HarnessExport {
            name: harness.pretty_name.clone(),
            file: harness.original_file.clone(),
            line: harness.original_start_line,
            contract: harness.contract.clone(),
            is_automatically_generated: harness.is_automatically_generated,
            attributes: harness.attributes.clone(),
            status,
            verification_time_s: result.runtime.as_secs_f64(),
            resolved_solver: result.resolved_solver.clone(),
            n_properties,
            n_failed,
            failed_properties,
            unsupported_constructs,
            covers,
            exit_status,
        }
    }
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

impl CoversExport {
    /// Every cover property is placed into *exactly one* of the six buckets below, which is
    /// what makes the sum of all six (satisfied's count plus every other list's length) equal
    /// `total` unconditionally, rather than merely for the statuses this code happened to
    /// anticipate. The `other` arm is not reachable for any `CheckStatus` variant that exists
    /// today (Kani's postprocessing settles cover properties into
    /// `Satisfied`/`Unsatisfiable`/`Unreachable`/`Undetermined`, and CBMC can additionally
    /// report `Error`) -- it exists for whatever status shows up next, not one we've already
    /// named.
    fn from_properties(properties: &[Property]) -> Self {
        let covers: Vec<&Property> = properties.iter().filter(|p| p.is_cover_property()).collect();
        let total = covers.len();

        let mut satisfied = 0;
        let mut unsatisfiable = Vec::new();
        let mut unreachable = Vec::new();
        let mut undetermined = Vec::new();
        let mut error = Vec::new();
        let mut unknown = Vec::new();
        let mut other = Vec::new();

        for p in &covers {
            let id = p.property_id.to_cbmc_id();
            match p.status {
                CheckStatus::Satisfied => satisfied += 1,
                CheckStatus::Unsatisfiable => unsatisfiable.push(id),
                CheckStatus::Unreachable => unreachable.push(id),
                CheckStatus::Undetermined => undetermined.push(id),
                CheckStatus::Error => error.push(id),
                CheckStatus::Unknown => unknown.push(id),
                status => other.push(OtherCoverExport { id, status }),
            }
        }

        CoversExport {
            total,
            satisfied,
            unsatisfiable,
            unreachable,
            undetermined,
            error,
            unknown,
            other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call_cbmc::{FailedProperties, VerificationResult};
    use crate::cbmc_output_parser::{PropertyId, SourceLocation};
    use kani_metadata::{HarnessKind, HarnessMetadata};

    fn harness(pretty: &str) -> HarnessMetadata {
        HarnessMetadata {
            pretty_name: pretty.to_string(),
            mangled_name: "mangled".to_string(),
            crate_name: "krate".to_string(),
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
    fn test_context() -> RunContext {
        RunContext {
            cbmc_version: None,
            kani_commit: None,
            kani_commit_dirty: None,
            enabled_unstable_features: Vec::new(),
            harness_selection: HarnessSelectionExport {
                requested_filters: Vec::new(),
                exact: false,
            },
            started_at: started(),
            wall_time: Duration::from_millis(1),
        }
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
        let export = ExportedRun::from_harness_results(&[hr], ctx);
        let v = serde_json::to_value(&export).unwrap();

        assert_eq!(v["schema_version"], "0.1.0");
        assert_eq!(v["cbmc_version"], "CBMC 6.8.0");
        assert_eq!(v["solver"], "cadical");
        assert_eq!(v["summary"]["total"], 1);
        assert_eq!(v["summary"]["successful"], 1);
        assert_eq!(v["summary"]["failed"], 0);
        assert_eq!(v["summary"]["covers_total"], 1);
        assert_eq!(v["summary"]["covers_satisfied"], 1);

        let harness_json = &v["harnesses"][0];
        assert_eq!(harness_json["name"], "my_harness");
        assert_eq!(harness_json["status"], "SUCCESSFUL");
        assert_eq!(harness_json["resolved_solver"], "cadical");
        assert_eq!(harness_json["n_properties"], 2);
        assert_eq!(harness_json["n_failed"], 0);
        assert!(harness_json["failed_properties"].as_array().unwrap().is_empty());
        assert_eq!(harness_json["covers"]["total"], 1);
        assert_eq!(harness_json["covers"]["satisfied"], 1);
        assert!(harness_json["covers"]["unsatisfiable"].as_array().unwrap().is_empty());
        assert!(harness_json["exit_status"].is_null());
    }

    /// A run with a failed (non-cover) property: `failed_properties` must name it.
    #[test]
    fn export_with_failed_property() {
        let h = harness("my_harness");
        let properties = vec![property("assertion", 1, CheckStatus::Failure)];
        let mut result = success_result(properties, Some("cadical"));
        result.status = VerificationStatus::Failure;
        result.failed_properties = FailedProperties::PanicsOnly;
        let hr = HarnessResult { harness: &h, result };

        let export = ExportedRun::from_harness_results(&[hr], test_context());
        let v = serde_json::to_value(&export).unwrap();

        assert!(v["cbmc_version"].is_null());
        let harness_json = &v["harnesses"][0];
        assert_eq!(harness_json["status"], "FAILED");
        assert_eq!(harness_json["n_failed"], 1);
        assert_eq!(harness_json["failed_properties"][0]["id"], "harness.assertion.1");
        assert_eq!(harness_json["failed_properties"][0]["class"], "assertion");
        assert_eq!(harness_json["failed_properties"][0]["trace_available"], false);
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

        let export = ExportedRun::from_harness_results(&[hr], test_context());
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

    /// A harness that produced no properties at all (e.g. CBMC crashed/timed out) must
    /// still be visibly FAILED with a reason, not silently reported as "0/0 covers".
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
        let export = ExportedRun::from_harness_results(&[hr], ctx);
        let v = serde_json::to_value(&export).unwrap();

        let harness_json = &v["harnesses"][0];
        assert_eq!(harness_json["status"], "FAILED");
        assert_eq!(harness_json["n_properties"], 0);
        assert_eq!(harness_json["covers"]["total"], 0);
        assert!(!harness_json["exit_status"].is_null());
        assert_eq!(harness_json["exit_status"], "Timeout");
    }

    /// Mixed solvers across harnesses must not be reported as a false uniform value.
    #[test]
    fn export_solver_not_uniform_is_null() {
        let h1 = harness("h1");
        let h2 = harness("h2");
        let r1 = success_result(vec![], Some("cadical"));
        let r2 = success_result(vec![], Some("kissat"));
        let hr1 = HarnessResult { harness: &h1, result: r1 };
        let hr2 = HarnessResult { harness: &h2, result: r2 };

        let export = ExportedRun::from_harness_results(&[hr1, hr2], test_context());
        let v = serde_json::to_value(&export).unwrap();
        assert!(v["solver"].is_null());
        assert_eq!(v["harnesses"][0]["resolved_solver"], "cadical");
        assert_eq!(v["harnesses"][1]["resolved_solver"], "kissat");
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

        let export = ExportedRun::from_harness_results(&[hr], test_context());
        let v = serde_json::to_value(&export).unwrap();

        let harness_json = &v["harnesses"][0];
        assert_eq!(harness_json["status"], "FAILED");
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

        let export = ExportedRun::from_harness_results(&[hr], test_context());
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

        let export = ExportedRun::from_harness_results(&[hr], test_context());
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

        let export = ExportedRun::from_harness_results(&[hr], test_context());
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

        let export = ExportedRun::from_harness_results(&[hr], test_context());
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

        let export = ExportedRun::from_harness_results(&[hr], test_context());
        let v = serde_json::to_value(&export).unwrap();

        let covers = &v["harnesses"][0]["covers"];
        assert_eq!(covers["total"], 1);
        assert_eq!(covers["satisfied"], 1);
    }

    /// When `--cbmc-args` may have smuggled in a solver-selecting flag,
    /// `VerificationResult::resolved_solver` is `None` -- confirm that surfaces as `null`
    /// (never a guessed value) both per-harness and at the run-wide `solver` field.
    #[test]
    fn export_unknown_resolved_solver_is_null() {
        let h = harness("cbmc_args_solver_harness");
        let result = success_result(vec![], None);
        let hr = HarnessResult { harness: &h, result };

        let export = ExportedRun::from_harness_results(&[hr], test_context());
        let v = serde_json::to_value(&export).unwrap();

        assert!(v["harnesses"][0]["resolved_solver"].is_null());
        assert!(v["solver"].is_null());
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

        let export = ExportedRun::from_harness_results(&[hr], test_context());
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

        let export = ExportedRun::from_harness_results(&[hr], test_context());
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
        let export = ExportedRun::from_harness_results(&[hr], ctx);
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

        let export = ExportedRun::from_harness_results(&[hr], test_context());
        let v = serde_json::to_value(&export).unwrap();

        assert!(v["kani_commit"].is_null());
        assert!(v["kani_commit_dirty"].is_null());
    }

    /// ADD D: the enabled `-Z` unstable features must be recorded, since results produced
    /// under different feature sets are not comparable -- e.g. a quantifier-bearing proof
    /// verified without `-Z quantifiers` is a different claim than one verified with it.
    #[test]
    fn export_enabled_unstable_features() {
        let h = harness("h");
        let hr = HarnessResult { harness: &h, result: success_result(vec![], Some("cadical")) };

        let ctx = RunContext {
            enabled_unstable_features: vec![
                "quantifiers".to_string(),
                "function-contracts".to_string(),
            ],
            ..test_context()
        };
        let export = ExportedRun::from_harness_results(&[hr], ctx);
        let v = serde_json::to_value(&export).unwrap();

        assert_eq!(
            v["enabled_unstable_features"].as_array().unwrap(),
            &vec![
                serde_json::Value::String("quantifiers".to_string()),
                serde_json::Value::String("function-contracts".to_string())
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
            },
            ..test_context()
        };
        let export = ExportedRun::from_harness_results(&[hr], ctx);
        let v = serde_json::to_value(&export).unwrap();

        assert_eq!(
            v["harness_selection"]["requested_filters"].as_array().unwrap(),
            &vec![serde_json::Value::String("check_volatile_load_wrapper_contract".to_string())]
        );
        assert_eq!(v["harness_selection"]["exact"], true);
    }

    /// No `--harness` filter given (`requested_filters` empty) must not be confused with a
    /// filter that matched nothing -- both `harness_selection.exact` default and empty
    /// `requested_filters` mean "no filter, every harness ran".
    #[test]
    fn export_harness_selection_defaults_to_no_filter() {
        let h = harness("h");
        let hr = HarnessResult { harness: &h, result: success_result(vec![], Some("cadical")) };

        let export = ExportedRun::from_harness_results(&[hr], test_context());
        let v = serde_json::to_value(&export).unwrap();

        assert!(v["harness_selection"]["requested_filters"].as_array().unwrap().is_empty());
        assert_eq!(v["harness_selection"]["exact"], false);
    }
}
