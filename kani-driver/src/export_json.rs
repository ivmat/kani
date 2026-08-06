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

/// Represents the version of the `--export-json` schema.
/// Increment this (following semantic versioning rules) whenever the JSON output shape
/// changes -- copies the idiom used for `FILE_VERSION` in `list/output.rs`.
const SCHEMA_VERSION: &str = "0.1.0";

const STATUS_SUCCESSFUL: &str = "SUCCESSFUL";
const STATUS_FAILED: &str = "FAILED";

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
        let cbmc_version = probe_cbmc_version();
        let export =
            ExportedRun::from_harness_results(results, cbmc_version, started_at, wall_time);
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
    /// `None` only if the `cbmc --version` probe failed; never a guess.
    cbmc_version: Option<String>,
    /// The solver actually used, IF uniform across every harness in this run.
    /// `None` when no harnesses ran, or when harnesses resolved to different solvers
    /// (a harness-level `solver` attribute can override the run-wide default) -- see each
    /// harness's own `resolved_solver` for the per-harness ground truth in that case.
    solver: Option<String>,
    target: &'static str,
    /// RFC3339-ish UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`) for when harness checking began.
    started_at: String,
    /// Wall-clock duration of the whole run (all harnesses). Do not confuse with any
    /// single harness's `verification_time_s`.
    wall_time_s: f64,
    harnesses: Vec<HarnessExport>,
    summary: Summary,
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
    failed_properties: Vec<FailedPropertyExport>,
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

#[derive(Serialize)]
struct FailedPropertyExport {
    id: String,
    description: String,
    class: String,
    file: Option<String>,
    line: Option<String>,
    trace_available: bool,
    /// `FAILURE` or `ERROR`. A harness can be `FAILED` because of `ERROR` properties alone
    /// (CBMC returns `ERROR` when an SMT solver itself errors out; Kani's own
    /// `determine_failed_properties` treats *any* `ERROR` property as failing the whole
    /// harness), with zero `FAILURE`-status properties -- so this field lets a consumer
    /// tell the two apart instead of silently dropping the `ERROR` ones.
    status: CheckStatus,
}

/// Non-vacuity signal, grouped by CBMC's own cover-property vocabulary (see
/// `cbmc_property_renderer::format_result`, which tracks exactly these four buckets:
/// `number_covers_satisfied`, `number_covers_unsatisfiable`, `number_covers_unreachable`,
/// `number_covers_undetermined`). Kept as separate lists rather than one merged
/// "not satisfied" bucket because "dead code" (`unreachable`), "logically impossible"
/// (`unsatisfiable`), and "the solver couldn't determine it" (`undetermined`) are different
/// diagnoses -- a consumer should not have to guess which one they got.
#[derive(Serialize)]
struct CoversExport {
    total: usize,
    satisfied: usize,
    unsatisfiable: Vec<String>,
    unreachable: Vec<String>,
    undetermined: Vec<String>,
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
    fn from_harness_results(
        results: &[HarnessResult<'_>],
        cbmc_version: Option<String>,
        started_at: OffsetDateTime,
        wall_time: Duration,
    ) -> Self {
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
            cbmc_version,
            solver,
            target: env!("TARGET"),
            started_at: format_started_at(started_at),
            wall_time_s: wall_time.as_secs_f64(),
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

        let (status, n_properties, n_failed, failed_properties, covers, exit_status) =
            match &result.results {
                Ok(properties) => {
                    let n_properties = properties.len();
                    // Mirrors `call_cbmc::determine_failed_properties`, which keys on
                    // exactly these two statuses (an `Error` property fails the harness
                    // even with zero `Failure`-status properties -- see the `status` field
                    // doc comment on `FailedPropertyExport`). `Undetermined`/`Unknown` do
                    // NOT fail a harness in Kani's own determination, so they are
                    // deliberately excluded here too.
                    let failed_properties: Vec<FailedPropertyExport> = properties
                        .iter()
                        .filter(|p| matches!(p.status, CheckStatus::Failure | CheckStatus::Error))
                        .map(FailedPropertyExport::from_property)
                        .collect();
                    let n_failed = failed_properties.len();
                    let covers = CoversExport::from_properties(properties);
                    let status = if result.status == VerificationStatus::Success {
                        STATUS_SUCCESSFUL
                    } else {
                        STATUS_FAILED
                    };
                    (status, n_properties, n_failed, failed_properties, covers, None)
                }
                Err(exit_status) => (
                    STATUS_FAILED,
                    0,
                    0,
                    Vec::new(),
                    CoversExport {
                        total: 0,
                        satisfied: 0,
                        unsatisfiable: Vec::new(),
                        unreachable: Vec::new(),
                        undetermined: Vec::new(),
                    },
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
            covers,
            exit_status,
        }
    }
}

impl FailedPropertyExport {
    fn from_property(p: &Property) -> Self {
        FailedPropertyExport {
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
    fn from_properties(properties: &[Property]) -> Self {
        let covers: Vec<&Property> = properties.iter().filter(|p| p.is_cover_property()).collect();
        let total = covers.len();
        let satisfied = covers.iter().filter(|p| p.status == CheckStatus::Satisfied).count();
        let ids_with_status = |status: CheckStatus| -> Vec<String> {
            covers
                .iter()
                .filter(|p| p.status == status)
                .map(|p| p.property_id.to_cbmc_id())
                .collect()
        };
        let unsatisfiable = ids_with_status(CheckStatus::Unsatisfiable);
        let unreachable = ids_with_status(CheckStatus::Unreachable);
        let undetermined = ids_with_status(CheckStatus::Undetermined);
        CoversExport { total, satisfied, unsatisfiable, unreachable, undetermined }
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

        let export = ExportedRun::from_harness_results(
            &[hr],
            Some("CBMC 6.8.0".to_string()),
            started(),
            Duration::from_millis(500),
        );
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

        let export =
            ExportedRun::from_harness_results(&[hr], None, started(), Duration::from_millis(1));
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

        let export =
            ExportedRun::from_harness_results(&[hr], None, started(), Duration::from_millis(1));
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

        let export =
            ExportedRun::from_harness_results(&[hr], None, started(), Duration::from_secs(300));
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

        let export = ExportedRun::from_harness_results(
            &[hr1, hr2],
            None,
            started(),
            Duration::from_millis(1),
        );
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

        let export =
            ExportedRun::from_harness_results(&[hr], None, started(), Duration::from_millis(1));
        let v = serde_json::to_value(&export).unwrap();

        let harness_json = &v["harnesses"][0];
        assert_eq!(harness_json["status"], "FAILED");
        // The real bug: with only the (buggy) `Failure`-only filter this would be empty.
        assert_eq!(harness_json["n_failed"], 1);
        assert!(!harness_json["failed_properties"].as_array().unwrap().is_empty());
        assert_eq!(harness_json["failed_properties"][0]["id"], "harness.assertion.2");
        assert_eq!(harness_json["failed_properties"][0]["status"], "ERROR");
    }

    /// Covers can land in any of CBMC's four terminal states, not just satisfied/
    /// unsatisfiable. `unreachable`/`undetermined` must be visible too, each in their own
    /// list (not merged into `unsatisfiable`, since "dead code" and "logically impossible"
    /// are different diagnoses) -- and the four buckets must exactly partition `total`.
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

        let export =
            ExportedRun::from_harness_results(&[hr], None, started(), Duration::from_millis(1));
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

        // The invariant: every cover property is accounted for by exactly one bucket.
        let satisfied = covers["satisfied"].as_u64().unwrap();
        let unsatisfiable = covers["unsatisfiable"].as_array().unwrap().len() as u64;
        let unreachable = covers["unreachable"].as_array().unwrap().len() as u64;
        let undetermined = covers["undetermined"].as_array().unwrap().len() as u64;
        assert_eq!(
            satisfied + unsatisfiable + unreachable + undetermined,
            covers["total"].as_u64().unwrap()
        );
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

        let export =
            ExportedRun::from_harness_results(&[hr], None, started(), Duration::from_millis(1));
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

        let export =
            ExportedRun::from_harness_results(&[hr], None, started(), Duration::from_millis(1));
        let v = serde_json::to_value(&export).unwrap();

        assert!(v["harnesses"][0]["resolved_solver"].is_null());
        assert!(v["solver"].is_null());
    }
}
