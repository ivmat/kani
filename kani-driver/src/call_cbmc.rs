// Copyright Kani Contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use anyhow::{Result, bail};
use kani_metadata::{CbmcSolver, HarnessMetadata};
use regex::Regex;
use rustc_demangle::demangle;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::ffi::OsString;
use std::fmt::Write;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use strum_macros::Display;
use tokio::process::Command as TokioCommand;

use crate::args::common::Verbosity;
use crate::args::{OutputFormat, VerificationArgs};
use crate::cbmc_output_parser::{
    CheckStatus, ParserItem, Property, VerificationOutput, extract_results, process_cbmc_output,
};
use crate::cbmc_property_renderer::{format_coverage, format_result, kani_cbmc_output_filter};
use crate::coverage::cov_results::{CoverageCheck, CoverageResults};
use crate::coverage::cov_results::{CoverageRegion, CoverageTerm};
use crate::session::KaniSession;
use crate::util::render_command;

/// We will use Cadical by default since it performed better than MiniSAT in our analysis.
/// Note: Kissat was marginally better, but it is an external solver which could be more unstable.
static DEFAULT_SOLVER: CbmcSolver = CbmcSolver::Cadical;

#[derive(Clone, Copy, Debug, Display, PartialEq, Eq)]
pub enum VerificationStatus {
    Success,
    Failure,
}

/// Represents failed properties in three different categories.
/// This simplifies the process to determine and format verification results.
///
/// `Serialize` (`SCREAMING_SNAKE_CASE`, matching this crate's export-json convention) is
/// genuinely used: `export_json.rs`'s `HarnessExport::failure_kind` embeds this directly, so
/// an automated consumer can tell a panic-only failure (investigate the code) apart from an
/// `Error` failure (an SMT solver itself errored out -- investigate the tool, not the proof).
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FailedProperties {
    // No failures
    None,
    // One or more panic-related failures
    PanicsOnly,
    // One or more failures that aren't panic-related
    Other,
    // One or more properties resulted in an ERROR rather than a failing/successful verification
    Error,
}

/// The possible CBMC exit statuses
#[derive(Clone, Copy, Debug)]
pub enum ExitStatus {
    Timeout,
    OutOfMemory,
    /// the integer is the process exit status
    Other(i32),
}

/// Our (kani-driver) notions of CBMC results.
#[derive(Debug)]
pub struct VerificationResult {
    /// Whether verification should be considered to have succeeded, or have failed.
    pub status: VerificationStatus,
    /// The compact representation for failed properties
    pub failed_properties: FailedProperties,
    /// The `Result` properties in detail or the exit_status of CBMC.
    /// Note: CBMC process exit status is only potentially useful if `status` is `Failure`.
    /// Kani will see CBMC report "failure" that's actually success (interpreting "failed"
    /// checks like coverage as expected and desirable.)
    pub results: Result<Vec<Property>, ExitStatus>,
    /// The runtime duration of this CBMC invocation.
    pub runtime: Duration,
    /// Whether concrete playback generated a test
    pub generated_concrete_test: bool,
    /// The coverage results
    pub coverage_results: Option<CoverageResults>,
    /// The solver CBMC actually ran this harness with, resolved (by
    /// [`KaniSession::handle_solver_args`]) from `--solver` / the harness's `solver`
    /// attribute / the driver's default, in that priority order.
    ///
    /// This is provenance for `--export-json`. It is deliberately *not* the same thing
    /// as `HarnessAttributes::solver`: that field is only the harness's *request*, and
    /// recording a request as if it were the fact actually invoked is the defect that had
    /// months of results claiming `kissat` ran while CaDiCaL did.
    ///
    /// `None` when `--cbmc-args` contains a solver-selecting flag (see
    /// `cbmc_args_may_override_solver` below): those flags are appended to CBMC's argv
    /// *after* the flags this resolution pushes, so CBMC's own last-flag-wins argument
    /// parsing can silently pick a different solver than the one resolved here (this is a
    /// real, exercised path -- see `tests/cargo-kani/simple-kissat`, which selects kissat
    /// purely via `--cbmc-args`). Recording a value we cannot stand behind would repeat the
    /// exact "recorded kissat, CaDiCaL ran" defect this field exists to prevent, so we
    /// record "unknown" instead of guessing.
    pub resolved_solver: Option<String>,
    /// Raw CBMC `WARNING`-type messages for this run (e.g. "ignoring forall" when a
    /// symbolic-bound quantifier falls back to a solver that can't handle it -- a dropped
    /// obligation that a green result otherwise hides completely). No taxonomy: free text,
    /// non-contractual, for `--export-json` only.
    pub warnings: Vec<String>,
    /// Peak RSS in bytes across CBMC and its descendants (e.g. an external SAT solver),
    /// via `getrusage(RUSAGE_CHILDREN)`. `None` -- never a guess -- whenever it cannot be
    /// honestly attributed to this specific harness; see `peak_memory_delta_bytes`.
    pub peak_memory_bytes: Option<u64>,
}

impl KaniSession {
    /// Verify a goto binary that's been prepared with goto-instrument
    pub fn run_cbmc(&self, file: &Path, harness: &HarnessMetadata) -> Result<VerificationResult> {
        let (args, resolved_solver): (Vec<OsString>, CbmcSolver) =
            self.cbmc_flags(file, harness)?;
        let resolved_solver = if cbmc_args_may_override_solver(&self.args.cbmc_args) {
            // `--cbmc-args` is appended after our own solver flags (see `cbmc_flags`), so if
            // it contains a solver-selecting flag we cannot know which one CBMC actually
            // honors -- see the doc comment on `VerificationResult::resolved_solver`.
            None
        } else {
            Some(solver_identity(&resolved_solver))
        };

        // TODO get cbmc path from self
        let mut cmd = TokioCommand::new("cbmc");
        cmd.args(args);

        let verification_results = if self.args.output_format == crate::args::OutputFormat::Old {
            if self.run_terminal_timeout(cmd).is_err() {
                VerificationResult::mock_failure(resolved_solver)
            } else {
                VerificationResult::mock_success(resolved_solver)
            }
        } else {
            // Add extra argument to receive the output in JSON format.
            // Done here because now removed `--visualize` used the XML format instead.
            // TODO: move this now that we don't use --visualize
            cmd.arg("--json-ui");

            self.runtime.block_on(self.run_cbmc_piped(cmd, harness, resolved_solver))?
        };

        Ok(verification_results)
    }

    async fn run_cbmc_piped(
        &self,
        mut cmd: TokioCommand,
        harness: &HarnessMetadata,
        resolved_solver: Option<String>,
    ) -> Result<VerificationResult> {
        if self.args.common_args.verbose() {
            println!("[Kani] Running: `{}`", render_command(cmd.as_std()).to_string_lossy());
        }
        // See `peak_memory_delta_bytes`: only meaningful single-threaded.
        let peak_memory_before =
            if self.args.jobs().will_multithread() { None } else { getrusage_children_maxrss_kb() };

        // Spawn the CBMC process and process its output below
        let mut cbmc_process = cmd
            .stdout(std::process::Stdio::piped())
            .spawn()
            .map_err(|_| anyhow::Error::msg("Failed to run cbmc"))?;

        let start_time = Instant::now();

        let res = if let Some(timeout) = self.args.harness_timeout {
            tokio::time::timeout(
                timeout.into(),
                process_cbmc_output(&mut cbmc_process, |i| {
                    kani_cbmc_output_filter(
                        i,
                        self.args.extra_pointer_checks,
                        self.args.common_args.quiet,
                        &self.args.output_format,
                    )
                }),
            )
            .await
        } else {
            Ok(process_cbmc_output(&mut cbmc_process, |i| {
                kani_cbmc_output_filter(
                    i,
                    self.args.extra_pointer_checks,
                    self.args.common_args.quiet,
                    &self.args.output_format,
                )
            })
            .await)
        };

        if let Ok(output) = res {
            // The timeout wasn't reached
            let peak_memory_bytes = peak_memory_before.and_then(peak_memory_delta_bytes);
            Ok(VerificationResult::from(
                output?,
                harness.attributes.should_panic,
                start_time,
                resolved_solver,
                peak_memory_bytes,
            ))
        } else {
            // An error occurs if the timeout was reached

            // Kill the process
            cbmc_process.kill().await?;

            Ok(VerificationResult {
                status: VerificationStatus::Failure,
                failed_properties: FailedProperties::None,
                results: Err(ExitStatus::Timeout),
                runtime: start_time.elapsed(),
                generated_concrete_test: false,
                coverage_results: None,
                resolved_solver,
                warnings: Vec::new(),
                peak_memory_bytes: None,
            })
        }
    }

    /// "Internal," but also used by call_cbmc_viewer.
    /// Also returns the solver actually resolved for this harness (see
    /// [`Self::handle_solver_args`]), for `--export-json` provenance.
    pub fn cbmc_flags(
        &self,
        file: &Path,
        harness_metadata: &HarnessMetadata,
    ) -> Result<(Vec<OsString>, CbmcSolver)> {
        let mut args = self.cbmc_check_flags();

        if let Some(object_bits) = self.args.cbmc_object_bits() {
            args.push("--object-bits".into());
            args.push(object_bits.to_string().into());
        }

        if let Some(unwind_value) = resolve_unwind_value(&self.args, harness_metadata) {
            args.push("--unwind".into());
            args.push(unwind_value.to_string().into());
        }

        let resolved_solver =
            self.handle_solver_args(&harness_metadata.attributes.solver, &mut args)?;

        if self.args.run_sanity_checks {
            args.push("--validate-goto-model".into());
            args.push("--validate-ssa-equation".into());
        }

        if self.args.concrete_playback.is_none() && !self.args.no_slice_formula {
            args.push("--slice-formula".into());
        }

        if self.args.concrete_playback.is_some() {
            args.push("--trace".into());
            // Concrete playback only consumes the values of `kani::any_raw_*`
            // return-value assignments from the trace. CBMC's compact trace
            // retains those (they are regular, non-hidden assignments) while
            // dropping hidden instrumentation steps whose values can dominate
            // the trace by orders of magnitude on contract-heavy harnesses
            // (e.g. 427 MB -> 3 MB of JSON). Requires CBMC with
            // https://github.com/diffblue/cbmc/pull/9135 to have an effect;
            // CBMC versions that do not yet honor `--compact-trace` with
            // `--json-ui` accept but ignore the option, so this is
            // compatible either way.
            args.push("--compact-trace".into());
        }

        args.extend(self.args.cbmc_args.iter().cloned());

        args.push(file.to_owned().into_os_string());

        // Make CBMC verbose by default to tell users about unwinding progress. This should be
        // reviewed as CBMC's verbosity defaults evolve.
        args.push("--verbosity".into());
        args.push("9".into());

        Ok((args, resolved_solver))
    }

    /// Just the flags to CBMC that enable property checking of any sort.
    pub fn cbmc_check_flags(&self) -> Vec<OsString> {
        let mut args = Vec::new();

        // We assume that malloc cannot fail, see https://github.com/model-checking/kani/issues/891
        args.push("--no-malloc-may-fail".into());

        // With PR #2630 we generate the appropriate checks directly rather than relying on CBMC's
        // checks (which are for C semantics).
        args.push("--no-undefined-shift-check".into());
        // With PR #647 we use Rust's `-C overflow-checks=on` instead of:
        // --unsigned-overflow-check
        // --signed-overflow-check
        // So these options are deliberately skipped to avoid erroneously re-checking operations.
        args.push("--no-signed-overflow-check".into());

        if !self.args.checks.memory_safety_on() {
            args.push("--no-bounds-check".into());
            args.push("--no-pointer-check".into());
        }
        if self.args.checks.overflow_on() {
            args.push("--nan-check".into());

            // TODO: Implement conversion checks as an optional check.
            // They are a well defined operation in rust, but they may yield unexpected results to
            // many users. https://github.com/model-checking/kani/issues/840
            // We might want to create a transformation pass instead of enabling CBMC since Kani
            // compiler sometimes rely on the bitwise conversion of signed <-> unsigned.
            // args.push("--conversion-check".into());
        } else {
            args.push("--no-div-by-zero-check".into());
        }

        if !self.args.checks.unwinding_on() {
            args.push("--no-unwinding-assertions".into());
        } else {
            args.push("--no-self-loops-to-assumptions".into());
        }

        if self.args.extra_pointer_checks {
            // This was adding a lot of false positives with std dangling pointer. We should
            // still catch any invalid dereference with --pointer-check. Thus, only enable them
            // if the user explicitly request them.
            args.push("--pointer-overflow-check".into());
        } else {
            args.push("--no-pointer-primitive-check".into());
        }

        args
    }

    /// Resolves which solver CBMC will actually run with (CLI `--solver` overrides the
    /// harness `solver` attribute, else the driver default) and pushes the corresponding
    /// CBMC flags onto `args`. Returns the resolved solver, so callers that need to
    /// *record* what actually ran (as opposed to just requesting it) don't have to
    /// duplicate this resolution logic -- see `--export-json`'s `resolved_solver`.
    pub fn handle_solver_args(
        &self,
        harness_solver: &Option<CbmcSolver>,
        args: &mut Vec<OsString>,
    ) -> Result<CbmcSolver> {
        let solver = if let Some(solver) = &self.args.solver {
            // `--solver` option takes precedence over attributes
            solver
        } else if let Some(solver) = harness_solver {
            solver
        } else {
            &DEFAULT_SOLVER
        };

        match solver {
            CbmcSolver::Bitwuzla => {
                args.push("--bitwuzla".into());
            }
            CbmcSolver::Cadical => {
                args.push("--sat-solver".into());
                args.push("cadical".into());
            }
            CbmcSolver::Cvc5 => {
                args.push("--cvc5".into());
            }
            CbmcSolver::Kissat => {
                args.push("--external-sat-solver".into());
                args.push("kissat".into());
            }
            CbmcSolver::Minisat => {
                // Minisat is currently CBMC's default solver, so no need to
                // pass any arguments
            }
            CbmcSolver::Z3 => {
                args.push("--z3".into());
            }
            CbmcSolver::Binary(solver_binary) => {
                // Check if the specified binary exists in path
                if which::which(solver_binary).is_err() {
                    bail!("the specified solver \"{solver_binary}\" was not found in path")
                }
                args.push("--external-sat-solver".into());
                args.push(solver_binary.into());
            }
        }
        Ok(solver.clone())
    }
}

/// A human-readable identity for the solver CBMC actually ran with, suitable for
/// `--export-json` provenance. Kept in a 1:1 match with `KaniSession::handle_solver_args`
/// above so the recorded identity can never drift from the flags actually passed to CBMC.
pub fn solver_identity(solver: &CbmcSolver) -> String {
    match solver {
        CbmcSolver::Bitwuzla => "bitwuzla".to_string(),
        CbmcSolver::Cadical => "cadical".to_string(),
        CbmcSolver::Cvc5 => "cvc5".to_string(),
        CbmcSolver::Kissat => "kissat".to_string(),
        CbmcSolver::Minisat => "minisat".to_string(),
        CbmcSolver::Z3 => "z3".to_string(),
        CbmcSolver::Binary(solver_binary) => solver_binary.clone(),
    }
}

/// CBMC flags that select a solver backend by themselves, independent of
/// `KaniSession::handle_solver_args`'s own resolution. `--cbmc-args` (an unstable
/// passthrough escape hatch, see `VerificationArgs::cbmc_args`) appends its contents to
/// CBMC's argv *after* the flags `cbmc_flags` pushes for the resolved solver, so if any of
/// these are present there, CBMC's own last-flag-wins argument parsing can silently honor a
/// different solver than the one `handle_solver_args` resolved.
///
/// This list is deliberately broader than the solvers `handle_solver_args` itself knows how
/// to select (it also includes e.g. `--boolector`/`--yices`, which Kani has no dedicated
/// `CbmcSolver` variant for): the goal here is only to detect "a solver flag might have been
/// smuggled in", not to identify which solver it selects. See `tests/cargo-kani/simple-kissat`
/// for a real, exercised instance of exactly this pattern (`cbmc-args = ["--external-sat-solver", "kissat"]`).
const CBMC_SOLVER_SELECTING_ARGS: &[&str] = &[
    "--sat-solver",
    "--external-sat-solver",
    "--smt1",
    "--smt2",
    "--bitwuzla",
    "--boolector",
    "--cprover-smt2",
    "--cvc3",
    "--cvc4",
    "--cvc5",
    "--mathsat",
    "--yices",
    "--z3",
    "--external-smt2-solver",
    "--incremental-smt2-solver",
];

/// Whether `cbmc_args` (the raw `--cbmc-args` passthrough) contains a flag that could
/// override the solver `handle_solver_args` resolved. See `CBMC_SOLVER_SELECTING_ARGS`.
///
/// Matches both the space-separated form (`--sat-solver kissat`, two tokens) and the
/// `=`-joined form (`--sat-solver=kissat`, one token). Missing the second form would
/// silently record the resolved default with confidence while a different solver actually
/// ran, which is the exact defect this field exists to prevent (months of results recorded
/// `kissat` while CaDiCaL ran) -- so it is matched defensively even though, as verified
/// directly against the pinned CBMC 6.8.0 binary, CBMC's own argument parser currently
/// *rejects* the `=`-joined form outright for every flag tried (`cbmc --sat-solver=kissat`
/// errors `Usage error!`, and so does e.g. `--object-bits=16`) rather than silently ignoring
/// it or honoring it: CBMC's parser is not getopt-style in this respect. That makes this
/// specific branch currently unreachable in practice for `--cbmc-args` values CBMC will
/// actually accept, but it costs nothing to keep: a spurious `null` costs a consumer
/// nothing, while a future CBMC version (or a different invocation path) accepting `=`-joined
/// flags and this code not noticing would reintroduce exactly the defect this exists to
/// prevent. An argument that isn't valid UTF-8 can't be inspected at all either, so it is
/// treated as a possible override too -- when in doubt, this resolves to `true`.
fn cbmc_args_may_override_solver(cbmc_args: &[OsString]) -> bool {
    cbmc_args.iter().any(|arg| {
        let Some(arg) = arg.to_str() else {
            // Can't inspect it; don't rule out an override.
            return true;
        };
        CBMC_SOLVER_SELECTING_ARGS.iter().any(|flag| {
            arg == *flag || arg.strip_prefix(flag).is_some_and(|rest| rest.starts_with('='))
        })
    })
}

/// Raw CBMC `WARNING`-type messages, for `VerificationResult::warnings`. See its doc comment.
fn extract_warnings(items: &[ParserItem]) -> Vec<String> {
    items
        .iter()
        .filter_map(|item| match item {
            ParserItem::Message { message_text, message_type }
                if message_type.eq_ignore_ascii_case("warning") =>
            {
                Some(message_text.clone())
            }
            _ => None,
        })
        .collect()
}

/// `getrusage(RUSAGE_CHILDREN).ru_maxrss` in KiB (Linux only -- other platforms use different
/// units or don't have this field). `None` if unavailable.
#[cfg(target_os = "linux")]
fn getrusage_children_maxrss_kb() -> Option<i64> {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `usage` is a correctly-sized out-param for a standard, precondition-free read
    // of kernel-maintained accounting.
    if unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) } == 0 {
        Some(usage.ru_maxrss)
    } else {
        None
    }
}

#[cfg(not(target_os = "linux"))]
fn getrusage_children_maxrss_kb() -> Option<i64> {
    None
}

/// `RUSAGE_CHILDREN` is a running MAXIMUM across every child this process has ever reaped
/// (not a per-child figure, and not summable), and it propagates through a child's own
/// `wait()` calls on its descendants (so an external solver spawned by CBMC is included). A
/// value read after one specific child is only honestly attributable to it if it is a NEW
/// record versus `before_kb` (measured immediately before spawning) -- otherwise some earlier
/// process already set a higher mark and we cannot tell how much of it is this child's.
/// Callers only attempt this single-threaded: under `--jobs`, the counter is shared across
/// threads, so a "new record" could belong to a concurrently-running sibling instead.
fn peak_memory_delta_bytes(before_kb: i64) -> Option<u64> {
    let after_kb = getrusage_children_maxrss_kb()?;
    if after_kb > before_kb { Some(after_kb as u64 * 1024) } else { None }
}

impl VerificationResult {
    /// Computes a `VerificationResult` (kani-driver's notion of the result of a CBMC call) from a
    /// `VerificationOutput` (cbmc_output_parser's idea of CBMC results).
    ///
    /// NOTE: We actually ignore the CBMC exit status, in favor of two checks:
    ///   1. Examining the actual results of CBMC properties.
    ///      (CBMC will regularly report "failure" but that's just our cover checks.)
    ///   2. Positively checking for the presence of results.
    ///      (Do not mistake lack of results for success: report it as failure.)
    fn from(
        output: VerificationOutput,
        should_panic: bool,
        start_time: Instant,
        resolved_solver: Option<String>,
        peak_memory_bytes: Option<u64>,
    ) -> VerificationResult {
        let runtime = start_time.elapsed();
        let (other_items, results) = extract_results(output.processed_items);
        let warnings = extract_warnings(&other_items);

        if let Some(results) = results {
            let (status, failed_properties) =
                verification_outcome_from_properties(&results, should_panic);
            let coverage_results = coverage_results_from_properties(&results);
            VerificationResult {
                status,
                failed_properties,
                results: Ok(results),
                runtime,
                generated_concrete_test: false,
                coverage_results,
                resolved_solver,
                warnings,
                peak_memory_bytes,
            }
        } else {
            // We never got results from CBMC - something went wrong (e.g. crash) so it's failure
            let exit_status = if output.process_status == 137 {
                ExitStatus::OutOfMemory
            } else {
                ExitStatus::Other(output.process_status)
            };
            VerificationResult {
                status: VerificationStatus::Failure,
                failed_properties: FailedProperties::Other,
                results: Err(exit_status),
                runtime,
                generated_concrete_test: false,
                coverage_results: None,
                resolved_solver,
                warnings,
                peak_memory_bytes,
            }
        }
    }

    pub fn mock_success(resolved_solver: Option<String>) -> VerificationResult {
        VerificationResult {
            status: VerificationStatus::Success,
            failed_properties: FailedProperties::None,
            results: Ok(vec![]),
            runtime: Duration::from_secs(0),
            generated_concrete_test: false,
            coverage_results: None,
            resolved_solver,
            warnings: Vec::new(),
            peak_memory_bytes: None,
        }
    }

    fn mock_failure(resolved_solver: Option<String>) -> VerificationResult {
        VerificationResult {
            status: VerificationStatus::Failure,
            failed_properties: FailedProperties::Other,
            // on failure, exit codes in theory might be used,
            // but `mock_failure` should never be used in a context where they will,
            // so again use something weird:
            results: Err(ExitStatus::Other(42)),
            runtime: Duration::from_secs(0),
            generated_concrete_test: false,
            coverage_results: None,
            resolved_solver,
            warnings: Vec::new(),
            peak_memory_bytes: None,
        }
    }

    pub fn render(&self, output_format: &OutputFormat, should_panic: bool) -> String {
        match &self.results {
            Ok(results) => {
                let status = self.status;
                let failed_properties = self.failed_properties;
                let show_checks = matches!(output_format, OutputFormat::Regular);

                let mut result = if let Some(cov_results) = &self.coverage_results {
                    format_coverage(
                        results,
                        cov_results,
                        status,
                        should_panic,
                        failed_properties,
                        show_checks,
                    )
                } else {
                    format_result(results, status, should_panic, failed_properties, show_checks)
                };
                writeln!(result, "Verification Time: {}s", self.runtime.as_secs_f32()).unwrap();
                result
            }
            Err(exit_status) => {
                let verification_result = console::style("FAILED").red();
                let (header, explanation) = match exit_status {
                    ExitStatus::OutOfMemory => (
                        String::from("CBMC failed"),
                        "CBMC appears to have run out of memory. You may want to rerun your proof in \
                    an environment with additional memory or use stubbing to reduce the size of the \
                    code the verifier reasons about.\n",
                    ),
                    ExitStatus::Timeout => (
                        String::from("CBMC failed"),
                        "CBMC timed out. You may want to rerun your proof with a larger timeout \
                    or use stubbing to reduce the size of the code the verifier reasons about.\n",
                    ),
                    ExitStatus::Other(exit_status) => {
                        (format!("CBMC failed with status {exit_status}"), "")
                    }
                };
                format!(
                    "\n{header}\n\
                    VERIFICATION:- {verification_result}\n\
                    {explanation}",
                )
            }
        }
    }
}

/// We decide if verification succeeded based on properties, not (typically) on exit code
fn verification_outcome_from_properties(
    properties: &[Property],
    should_panic: bool,
) -> (VerificationStatus, FailedProperties) {
    let failed_properties = determine_failed_properties(properties);
    let status = if should_panic {
        match failed_properties {
            FailedProperties::Error => VerificationStatus::Failure,
            FailedProperties::None | FailedProperties::Other => VerificationStatus::Failure,
            FailedProperties::PanicsOnly => VerificationStatus::Success,
        }
    } else {
        match failed_properties {
            FailedProperties::Error => VerificationStatus::Failure,
            FailedProperties::None => VerificationStatus::Success,
            FailedProperties::PanicsOnly | FailedProperties::Other => VerificationStatus::Failure,
        }
    };
    (status, failed_properties)
}

/// Determines the `FailedProperties` variant that corresponds to an array of properties
fn determine_failed_properties(properties: &[Property]) -> FailedProperties {
    if properties.iter().any(|prop| prop.status == CheckStatus::Error) {
        return FailedProperties::Error;
    };
    let failed_properties: Vec<&Property> =
        properties.iter().filter(|prop| prop.status == CheckStatus::Failure).collect();
    // Return `FAILURE` if there isn't at least one failed property
    if failed_properties.is_empty() {
        FailedProperties::None
    } else {
        // Check if all failed properties correspond to the `assertion` class.
        // Note: Panics caused by `panic!` and `assert!` fall into this class.
        let all_failed_checks_are_panics =
            failed_properties.iter().all(|prop| prop.property_class() == "assertion");
        if all_failed_checks_are_panics {
            FailedProperties::PanicsOnly
        } else {
            FailedProperties::Other
        }
    }
}

fn coverage_results_from_properties(properties: &[Property]) -> Option<CoverageResults> {
    let cov_properties: Vec<&Property> =
        properties.iter().filter(|p| p.is_code_coverage_property()).collect();

    if cov_properties.is_empty() {
        return None;
    }

    // Postprocessing the coverage results involves matching on the descriptions
    // of code coverage properties with the `counter_re` regex. These are two
    // real examples of such descriptions:
    //
    // ```
    // CounterIncrement(0) $test_cov$ - src/main.rs:5:1 - 6:15
    // ExpressionUsed(0) $test_cov$ - src/main.rs:6:19 - 6:28
    // ```
    //
    // The span is further processed to extract the code region attributes.
    // Ideally, we should have coverage mappings (i.e., the relation between
    // counters and code regions) available in the coverage metadata:
    // <https://github.com/model-checking/kani/issues/3445>. If that were the
    // case, we would not need the spans in these descriptions.
    let counter_re = {
        static COUNTER_RE: OnceLock<Regex> = OnceLock::new();
        COUNTER_RE.get_or_init(|| {
            Regex::new(
                r#"^(?<kind>VirtualCounter\(bcb)(?<counter_num>[0-9]+)\) \$(?<func_name>[^\$]+)\$ - (?<span>.+)"#,
            )
            .unwrap()
        })
    };

    let mut coverage_results: BTreeMap<String, Vec<CoverageCheck>> = BTreeMap::default();

    for prop in cov_properties {
        let mut prop_processed = false;
        if let Some(captures) = counter_re.captures(&prop.description) {
            let counter_num = &captures["counter_num"];
            let function = demangle(&captures["func_name"]).to_string();
            let status = prop.status;
            let span = captures["span"].to_string();

            let counter_id = counter_num.parse().unwrap();
            let term = CoverageTerm::Counter(counter_id);
            let region = CoverageRegion::from_str(span);

            let cov_check = CoverageCheck::new(function, term, region, status);
            let file = cov_check.region.file.clone();

            if let Entry::Vacant(e) = coverage_results.entry(file.clone()) {
                e.insert(vec![cov_check]);
            } else {
                coverage_results.entry(file).and_modify(|checks| checks.push(cov_check));
            }
            prop_processed = true;
        }

        assert!(prop_processed, "error: coverage property not processed\n{prop:?}");
    }

    Some(CoverageResults::new(coverage_results))
}
/// Solve Unwind Value from conflicting inputs of unwind values. (--default-unwind, annotation-unwind, --unwind)
pub fn resolve_unwind_value(
    args: &VerificationArgs,
    harness_metadata: &HarnessMetadata,
) -> Option<u32> {
    // Check for which flag is being passed and prioritize extracting unwind from the
    // respective flag/annotation.
    args.unwind.or(harness_metadata.attributes.unwind_value).or(args.default_unwind)
}

#[cfg(test)]
mod tests {
    use crate::args;
    use crate::metadata::tests::mock_proof_harness;
    use clap::Parser;

    use super::*;

    #[test]
    fn check_resolve_unwind_value() {
        // Command line unwind value for specific harnesses take precedence over default annotation value
        let args_empty = ["kani", "x.rs"];
        let args_only_default = ["kani", "x.rs", "--default-unwind", "2"];
        let args_only_harness = ["kani", "x.rs", "--unwind", "1", "--harness", "check_one"];
        let args_both =
            ["kani", "x.rs", "--default-unwind", "2", "--unwind", "1", "--harness", "check_one"];

        let harness_none = mock_proof_harness("check_one", None, None, None);
        let harness_some = mock_proof_harness("check_one", Some(3), None, None);

        fn resolve(args: &[&str], harness: &HarnessMetadata) -> Option<u32> {
            resolve_unwind_value(
                &args::StandaloneArgs::try_parse_from(args).unwrap().verify_opts,
                harness,
            )
        }

        // test against no unwind annotation
        assert_eq!(resolve(&args_empty, &harness_none), None);
        assert_eq!(resolve(&args_only_default, &harness_none), Some(2));
        assert_eq!(resolve(&args_only_harness, &harness_none), Some(1));
        assert_eq!(resolve(&args_both, &harness_none), Some(1));

        // test against unwind annotation
        assert_eq!(resolve(&args_empty, &harness_some), Some(3));
        assert_eq!(resolve(&args_only_default, &harness_some), Some(3));
        assert_eq!(resolve(&args_only_harness, &harness_some), Some(1));
        assert_eq!(resolve(&args_both, &harness_some), Some(1));
    }

    fn os_strings(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    /// A solver flag smuggled in via `--cbmc-args` (the real pattern used by
    /// `tests/cargo-kani/simple-kissat`) must be detected, so `resolved_solver` can be
    /// recorded as unknown rather than the (possibly wrong) resolved default.
    #[test]
    fn check_cbmc_args_solver_override_detected() {
        assert!(cbmc_args_may_override_solver(&os_strings(&["--external-sat-solver", "kissat"])));
        assert!(cbmc_args_may_override_solver(&os_strings(&["--cvc5"])));
        assert!(cbmc_args_may_override_solver(&os_strings(&["--sat-solver", "minisat"])));
    }

    /// The `=`-joined form (`--sat-solver=kissat`, a single token) must be detected exactly
    /// like the space-separated form, as a defensive measure -- see the doc comment on
    /// `cbmc_args_may_override_solver` for what was actually verified about CBMC's own
    /// parser here (it currently rejects this form outright rather than honoring it).
    #[test]
    fn check_cbmc_args_solver_override_detected_equals_form() {
        assert!(cbmc_args_may_override_solver(&os_strings(&["--sat-solver=kissat"])));
        assert!(cbmc_args_may_override_solver(&os_strings(&["--external-sat-solver=kissat"])));
    }

    /// Unrelated `--cbmc-args` flags must not be mistaken for a solver override.
    #[test]
    fn check_cbmc_args_without_solver_override_not_detected() {
        assert!(!cbmc_args_may_override_solver(&os_strings(&[])));
        assert!(!cbmc_args_may_override_solver(&os_strings(&["--object-bits", "16"])));
        assert!(!cbmc_args_may_override_solver(&os_strings(&["--json-ui"])));
    }

    /// A flag name that merely shares a prefix with a solver flag (but isn't followed by
    /// end-of-token or `=`) must not be a false positive.
    #[test]
    fn check_cbmc_args_solver_flag_prefix_lookalike_not_detected() {
        assert!(!cbmc_args_may_override_solver(&os_strings(&["--z3-something-else"])));
    }

    /// An argument that isn't valid UTF-8 can't be inspected, so it must resolve toward
    /// "may override" (`true`), not toward a false "definitely not" (`false`).
    #[test]
    fn check_cbmc_args_non_utf8_resolves_to_may_override() {
        use std::os::unix::ffi::OsStrExt;
        let non_utf8 = std::ffi::OsStr::from_bytes(&[0xff, 0xfe]).to_owned();
        assert!(cbmc_args_may_override_solver(&[non_utf8]));
    }

    /// `message_type` matching is case-insensitive and ignores non-WARNING messages (e.g.
    /// the "ERROR"-typed messages `cbmc_property_renderer.rs` already matches exactly this
    /// way). Matches the convention already proven in this codebase; a live CBMC "ignoring
    /// forall" repro was not obtained in this session, so this is unit-level evidence only.
    #[test]
    fn check_extract_warnings_filters_by_message_type() {
        let items = vec![
            ParserItem::Message {
                message_text: "ignoring forall".to_string(),
                message_type: "WARNING".to_string(),
            },
            ParserItem::Message {
                message_text: "lowercase warning too".to_string(),
                message_type: "warning".to_string(),
            },
            ParserItem::Message {
                message_text: "not a warning".to_string(),
                message_type: "STATUS-MESSAGE".to_string(),
            },
            ParserItem::Program { program: "CBMC 6.8.0".to_string() },
        ];

        let warnings = extract_warnings(&items);

        assert_eq!(
            warnings,
            vec!["ignoring forall".to_string(), "lowercase warning too".to_string()]
        );
    }
}
