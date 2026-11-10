# Acceptance package for RFC 0015 (`--export-json`)

This directory holds an acceptance package for the Kani branch `export-json-rfc0015-reference-consumer`
at commit `df3e37771`. The branch implements [RFC 0015](../../rfc/src/rfcs/0015-export-json.md)
(`--export-json`). It has three parts:

| commit | change |
|---|---|
| `bf9c58e66` | upstream `main`. It already contains the writer (#4941) and the INCOMPLETE marker (#5062), both merged |
| `fb3ef0e76` | PR 3: end-to-end tests (head of the tests branch, on top of `main`) |
| `df3e37771` | PR 4, optional: the reference consumer (`scripts/validate_json_export.py`) checks the cross-field rules. It is merged with PR 3 in this branch |

The package is written in the [acceptance format](https://github.com/ivmat/acceptance-format)
version 0.4.0 (protocol `acceptance-protocol/1`). It records, in machine-checkable form, what the specification asks for, which claims the
evidence supports, and which parts are not delivered. It is not part of the Kani build and no
Kani test reads it.

The package measures semantic coverage of the RFC (which RFC clauses are asserted by a test that passed at this commit; each evidenced claim has at least one specific mutation that was observed to turn a test red) rather than code coverage.

## How to read the ids in this document

This document uses short ids. Each id family has one home file, where you can read the full text.
`R0` to `R14` are the requirements of the specification. They are in `acceptance-specification.toml`, in the
`[[requirement]]` tables, in the field `statement`. The table below gives every statement in full,
and each later mention of an id in this document repeats its statement in brackets.
`RFC15-xx` (for example `RFC15-D2`) are the claims of the package. They are in `acceptance.toml`, in the `[[claim]]` tables.
`RFC0015-S-...` (for example `RFC0015-S-7.2-13`) are the clauses of the RFC clause inventory in
`standards/rfc0015.clauses.toml`. This document sometimes writes a clause id without the `RFC0015-` prefix (for example `S-7.2-13`).
`A0` to `A4` are the assurance bands of the acceptance format, from lowest to highest. `A0` means that the claim was run or asserted
without a control that was watched to fail. `A1` means that the check is not vacuous (a test with a control that turns it red) but does not cover every state.
The higher bands (`A2` to `A4`) need stronger evidence, for example Kani proofs. The claims of this package are at `A0` and `A1`.

| id | statement (verbatim from `acceptance-specification.toml`) | mandatory |
|---|---|---|
| R0 | The RFC 0015 clause ledger (standards/rfc0015.clauses.toml, conformance manifest) validates under the conformance profile, every clause of the pinned RFC text is listed, and every clause carries an applicability declared before any evidence was read and, if applicable, a claim. | yes |
| R1 | Every export is one document carrying schema_version, a harnesses[] array sorted by (crate_name, file, line, name) independent of completion order, and per-harness fields present or null exactly as the RFC's harness-level presence matrix says, including the harness_selection block. | yes |
| R2 | Every closed enum (outcome.kind, verdict, failure_kind, status, attributes.kind) is a Rust type serialized in the RFC's casing, so an unmodeled variant cannot be silently absorbed, and property ids are rebuilt from the parsed id in the RFC's \<function>.\<class>.\<counter> forms. | yes |
| R3 | On a COMPLETED harness the check/cover buckets partition every property by class and sum to their totals, failed_properties[] is exactly checks.failure with n_failed and n_properties consistent, unsupported constructs are reported with full records, and summary.* is computed over COMPLETED harnesses only, never counting null as zero. | yes |
| R4 | outcome.verdict and failure_kind follow the RFC's truth table over should_panic x FailedProperties, and a harness that #4719 fails because the solver dropped quantifiers is exported as ERROR/FAILURE, never recomputed to SUCCESS from its properties. | yes |
| R5 | With assertion reach checks on, a harness whose assertions are all unreachable exports verdict SUCCESS with checks.total > 0 and checks.unreachable listing every check, so a consumer can apply the RFC's vacuous-pass predicate. | yes |
| R6 | --export-json requires its own -Z export-json unstable feature, and combinations that cannot produce real results (--output-format=old, --only-codegen) are rejected before verification starts. | yes |
| R7 | Every write goes to a temporary file in the target's own directory and is renamed onto the target, a missing parent directory is created first, and an interrupted write never leaves a partially written target. | yes |
| R8 | The configuration block reports the solver and unwind bound CBMC actually runs with, resolved from Kani's own flags followed by --cbmc-args, the nine mandatory check flags and coverage_enabled as effective for the run, cbmc_args verbatim, and run provenance (kani_commit, dirty flag, tool versions) as null rather than guessed when unknown. | yes |
| R9 | Timeout, out-of-memory and CBMC crash are recorded as that harness's outcome while the file is still written, warnings are capped as the RFC says, is_bounded is reported for every harness, and a multi-crate run writes one file covering every crate. | yes |
| R10 | A run that has begun verification atomically replaces any earlier file with an INCOMPLETE marker, a finished run writes the complete document with its run_state, run-level fields are present exactly as the RFC's run-level presence matrix says, and compile errors or rejected filters before the marker leave existing files untouched. | yes |
| R11 | An --export-json path that names an existing directory is rejected when arguments are parsed, before any build or verification starts. | yes |
| R12 | Kani's reference consumer, scripts/validate_json_export.py, implements the RFC's consumer-side rules (versioning acceptance, vacuity predicates, selection and join guidance, no assumed output_dir, no inference of unrestricted proofs from an omitted disclosure) and rejects documents that break them. | yes |
| R13 | --export-json only adds a file: rendered output and --sarif output are unchanged, --output-into-files stays independent, and a path of '-' is a literal filename, not stdout. | yes |
| R14 | On adoption of RFC 0015 (merged 2026-09-24), tracking issues are filed upstream for the effective --object-bits provenance representation and for full tool/solver provenance and host machine metadata. | yes |

## Current status

- The package describes the code at commit `df3e37771821750a6e888c01b2d88611485cd991`
  (the top of the branch). It is in this branch only because the branch adds this directory on top of that commit.
- **The evidence records were regenerated at this commit on 2026-11-10.** Every record that a claim cites was captured at
  `df3e37771`. 22 runs passed, and 144 controls were executed: 142 turned red and 2 did not. The two that did not are
  `rfc15-30-oom-report-not-checked-before-trusting-array` and `rfc15-30-status-6-trusted-without-error-property`. No evidence
  entry cites them, and claim RFC15-30 is `partial` for that reason.
- **Requirement coverage is 12/15, and `acceptable=False`.** Always read the two together.
  These 12 requirements are satisfied:
  - R0 ("The RFC 0015 clause ledger (standards/rfc0015.clauses.toml, conformance manifest) validates under the conformance profile, every clause of the pinned RFC text is listed, and every clause carries an applicability declared before any evidence was read and, if applicable, a claim.")
  - R1 ("Every export is one document carrying schema_version, a harnesses[] array sorted by (crate_name, file, line, name) independent of completion order, and per-harness fields present or null exactly as the RFC's harness-level presence matrix says, including the harness_selection block.")
  - R2 ("Every closed enum (outcome.kind, verdict, failure_kind, status, attributes.kind) is a Rust type serialized in the RFC's casing, so an unmodeled variant cannot be silently absorbed, and property ids are rebuilt from the parsed id in the RFC's \<function>.\<class>.\<counter> forms.")
  - R3 ("On a COMPLETED harness the check/cover buckets partition every property by class and sum to their totals, failed_properties[] is exactly checks.failure with n_failed and n_properties consistent, unsupported constructs are reported with full records, and summary.* is computed over COMPLETED harnesses only, never counting null as zero.")
  - R4 ("outcome.verdict and failure_kind follow the RFC's truth table over should_panic x FailedProperties, and a harness that #4719 fails because the solver dropped quantifiers is exported as ERROR/FAILURE, never recomputed to SUCCESS from its properties.")
  - R5 ("With assertion reach checks on, a harness whose assertions are all unreachable exports verdict SUCCESS with checks.total > 0 and checks.unreachable listing every check, so a consumer can apply the RFC's vacuous-pass predicate.")
  - R6 ("--export-json requires its own -Z export-json unstable feature, and combinations that cannot produce real results (--output-format=old, --only-codegen) are rejected before verification starts.")
  - R7 ("Every write goes to a temporary file in the target's own directory and is renamed onto the target, a missing parent directory is created first, and an interrupted write never leaves a partially written target.")
  - R8 ("The configuration block reports the solver and unwind bound CBMC actually runs with, resolved from Kani's own flags followed by --cbmc-args, the nine mandatory check flags and coverage_enabled as effective for the run, cbmc_args verbatim, and run provenance (kani_commit, dirty flag, tool versions) as null rather than guessed when unknown.")
  - R9 ("Timeout, out-of-memory and CBMC crash are recorded as that harness's outcome while the file is still written, warnings are capped as the RFC says, is_bounded is reported for every harness, and a multi-crate run writes one file covering every crate.")
  - R11 ("An --export-json path that names an existing directory is rejected when arguments are parsed, before any build or verification starts.")
  - R13 ("--export-json only adds a file: rendered output and --sarif output are unchanged, --output-into-files stays independent, and a path of '-' is a literal filename, not stdout.")
  These 2 requirements are **partial**:
  - R10 ("A run that has begun verification atomically replaces any earlier file with an INCOMPLETE marker, a finished run writes the complete document with its run_state, run-level fields are present exactly as the RFC's run-level presence matrix says, and compile errors or rejected filters before the marker leave existing files untouched.") (completeness)
  - R12 ("Kani's reference consumer, scripts/validate_json_export.py, implements the RFC's consumer-side rules (versioning acceptance, vacuity predicates, selection and join guidance, no assumed output_dir, no inference of unrestricted proofs from an omitted disclosure) and rejects documents that break them.") (reference consumer)
  This requirement is a declared deviation:
  - R14 ("On adoption of RFC 0015 (merged 2026-09-24), tracking issues are filed upstream for the effective --object-bits provenance representation and for full tool/solver provenance and host machine metadata.") (tracking issues)
- The specification is **not ratified**. The Kani maintainers have not reviewed it. For this reason
  `check-package` reports one error, "the bound specification is not BINDING", and ends with `FAIL acceptance.toml (1 error)`.
  This is expected. Nothing in this package is binding until the specification is ratified and a decision is made.
- R10 ("A run that has begun verification atomically replaces any earlier file with an INCOMPLETE marker, a finished run writes the complete document with its run_state, run-level fields are present exactly as the RFC's run-level presence matrix says, and compile errors or rejected filters before the marker leave existing files untouched.") is partial. The INCOMPLETE marker is merged upstream (#5062) and its end-to-end suite is in this branch. Its
  claims RFC15-06, RFC15-35 and RFC15-36 are `partial`: the atomicity of the marker write is not observed, crash kinds other
  than SIGKILL and a failing terminal write are not shown, and no control mutates the content of the marker. The claim for the
  case that no harness completed (RFC15-38) is a gap. R12 ("Kani's reference consumer, scripts/validate_json_export.py, implements the RFC's consumer-side rules (versioning acceptance, vacuity predicates, selection and join guidance, no assumed output_dir, no inference of unrestricted proofs from an omitted disclosure) and rejects documents that break them.") is partial
  because the reference consumer does not implement every rule that the specification names.
  The claims that are evidenced for R10 and R12 are marked `unweighted` on purpose, so that
  the tool reports "partial" and not "satisfied".
- Of the 40 claims, 33 are evidenced, 4 are `partial` (RFC15-06, -30, -35, -36) and 3 are gaps (RFC15-38, RFC15-D2, RFC15-P1).
  Here `partial` (at band A1 or A0) means that part of a statement is not shown. The coverage tool counts such a claim as not
  evidenced.
- The clause ledger (`ledger/acceptance.toml`) covers all 259 clauses of the RFC. Of the 210
  applicable clauses, 112 are evidenced and 98 are gaps. 60 of the gaps are clauses that the tests
  only partly assert. The 12/15 above counts requirements. It says nothing about clauses.
- The evidence is dynamic: unit tests, end-to-end test suites, and recorded runs checked by a script.
  For each evidenced claim, at least one control (a small patch that breaks the code) was observed to turn a check red.
  22 runs and 144 controls were executed at commit `df3e37771`: 142 controls are red, and 2 are not red and are cited by no evidence entry (RFC15-30 is partial).
- The RFC text that the specification pins (`spec/`) has an older wording of `resolved_solver` and
  `resolved_unwind` than the one the writer implements. See "Open points".

## Check the package yourself

Run these commands from this directory. You need Python 3.11 or later and git. You do not need a Kani build.

```sh
cd acceptance/rfc0015
export PYTHONDONTWRITEBYTECODE=1

# 1. Get the checker. Use exactly this commit (the 0.4.0 release commit of the public repository).
git clone https://github.com/ivmat/acceptance-format.git vendor/acceptance-format
git -C vendor/acceptance-format checkout 3dbd403d0929a5398c43f93bd41a31e3365e82cd
V=vendor/acceptance-format

# 2. Check the specification, the package and the requirement coverage.
python3 $V/protocol_acceptance/tools/acceptance_protocol.py check-spec acceptance-specification.toml
python3 $V/format_acceptance/tools/check_acceptance.py --strict --strict-weight acceptance.toml
python3 $V/protocol_acceptance/tools/acceptance_protocol.py coverage acceptance.toml --spec acceptance-specification.toml
```

Expected result:

- `check-spec`: `PASS acceptance-specification.toml (3 warnings)`. The 3 warnings are spec-clarity notes.
- `check_acceptance`: `PASS acceptance.toml (29 warnings) [weighted: 29, unweighted: 11]`. The warnings say `dynamic-only evidence at band A1`.
- `coverage`: the last line is `summary: mandatory 12/15, optional 0/0, acceptable=False`. R10 and R12 are `partial` and R14 is `deviation-declared`; the other 12 requirements are satisfied.

Two more checks:

```sh
# The clause ledger is a valid conformance manifest (expected: `PASS ledger/acceptance.toml [weighted: 0, unweighted: 259]`, then `COVERAGE ledger/acceptance.toml [not-applicable: 21, excluded: 28, gap: 98]`).
python3 $V/format_acceptance/tools/check_core.py ledger/acceptance.toml --strict --strict-weight

# The delivery gate. Expected: `FAIL acceptance.toml (1 error)`. The one error is "the bound specification is not BINDING",
# because the specification is not ratified. The command prints the same requirement coverage as `coverage` above.
python3 $V/protocol_acceptance/tools/acceptance_protocol.py check-package acceptance.toml --spec acceptance-specification.toml
```

The `vendor/` directory is ignored by git.

## What is in this directory

| path | content |
|---|---|
| `acceptance-specification.toml` | the specification: the 15 requirements for the whole RFC (listed in the first table of this file) |
| `acceptance.toml` | the package: 40 claims (`RFC15-xx`) under those 15 requirements, each with its evidence |
| `applicability.toml` | for each clause of the RFC: applicable, not applicable or excluded, with the reason |
| `standards/rfc0015.clauses.toml` | the clause inventory: one row for each normative unit of the RFC |
| `spec/0015-export-json.txt` | a byte copy of the RFC text that the specification pins (its digest is in the specification) |
| `ledger/acceptance.toml` | the conformance manifest: one claim for each clause. It is generated. Do not edit it |
| `ledger/gen_ledger.py` | generates `ledger/acceptance.toml` |
| `ledger/clause_requirements.toml` | for each applicable clause, the requirement whose claims may evidence it |
| `ledger/test_clauses.toml` | for each test, the clauses that it asserts (see below) |
| `ledger/test_citations.py` | helper of `gen_ledger.py` |
| `evidence/recipes.toml` | the registry of runs and controls, and which claim cites which one |
| `evidence/gen/` | the generated records: the output of every run and control |
| `evidence/patches/` | the patches that the controls apply |
| `gen_evidence.py` | runs the recipes and writes `evidence/gen/` |
| `restamp.py` | rewrites the header of `acceptance.toml` (specification hash, Kani commit, counts) |

### Test-to-clause mapping

`ledger/test_clauses.toml` is written by hand. A test is listed against a clause only if the test
asserts every obligation of that clause on the production path. A clause that the tests only partly
assert is not evidenced; it is listed under `[[partial]]` with what is and is not asserted.
This includes a clause that says a field is a real quantity (the Kani release, the build target, CBMC's
version output) when the test checks only that the field is present or has the right shape, and a clause
with an "only", "never" or "every" obligation (an exact set of keys, an absent field) when no test
asserts the absence or the full set.
The validators check that a cited record exists, is fresh and has the right hash. They do not
read the test. A wrong mapping is therefore found only by a person who reads the test.

## Optional: run the evidence again

You need a Kani build, CBMC 6.11.0, Linux with `systemd --user` (the Kani runs happen in a memory-capped service, one at a time),
and a separate, clean checkout of the branch top. The generator refuses to start if that checkout is not clean,
so do not use the checkout that holds this directory. The commands below put the new checkout next to the Kani clone.

```sh
git worktree add --detach ../../../kani-rfc0015-stack df3e37771821750a6e888c01b2d88611485cd991
export RFC0015_KANI_WORKTREE=$(cd ../../../kani-rfc0015-stack && pwd)   # the checkout above
export RFC0015_CBMC_DIR=/path/to/your/cbmc/install                      # its usr/bin has cbmc 6.11.0
python3 -B ledger/gen_ledger.py --check      # the ledger still matches the package (no Kani build needed)
python3 -B gen_evidence.py --dry-run         # list every run and control without running anything
python3 -B gen_evidence.py --only <id> ...   # run some of them
python3 -B gen_evidence.py                   # run all of them (long), rewrite acceptance.toml, restamp, validate
```

Regeneration rewrites `evidence/gen/` and the evidence entries of `acceptance.toml`. After a full run, regenerate the
ledger last (`python3 -B ledger/gen_ledger.py`), because it embeds the hash of `acceptance.toml`,
and run the checks above again. Never edit a record, a `record_hash` or a `captured_at_commit` by hand.

A control applies its patch with `git apply`, runs the check, and reverts the patch. The generator stops with exit code 97 if the
checkout is not clean again afterwards.

### Paths

No file here contains a machine-specific path. The two paths that the tools need are given on the command line
(`--kani-worktree`, `--cbmc-dir`) or in the environment variables above. There is no default for either one.

### Transcript normalization

Every record in `evidence/gen/` is normalized before it is stored and hashed. The steps apply in this order:

1. the absolute path of the Kani checkout becomes `<kani>`
2. the directory of this package becomes `<pkg>`
3. the CBMC install directory becomes `<cbmc>`
4. any remaining `$HOME` prefix becomes `~`
5. any other `/home/<user>/` prefix becomes `<home>/`
6. a systemd invocation id becomes `<invocation-id>`
7. a `/tmp` directory name made by `mktemp` becomes `<tmp>`

The `record_hash` of a record is computed over these normalized bytes, as stored. Each record states this in its header.

## Open points for the consumer

These are proposals. None of them has been applied.

- **Specification R10** ("A run that has begun verification atomically replaces any earlier file with an INCOMPLETE marker, a finished run writes the complete document with its run_state, run-level fields are present exactly as the RFC's run-level presence matrix says, and compile errors or rejected filters before the marker leave existing files untouched."). The text demands the INCOMPLETE marker, which the writer now writes (#5062); the claims for it (RFC15-06, -35, -36) are `partial`, and the all-non-COMPLETED accounting case (RFC15-38) is a gap. Either keep R10 ("A run that has begun verification atomically replaces any earlier file with an INCOMPLETE marker, a finished run writes the complete document with its run_state, run-level fields are present exactly as the RFC's run-level presence matrix says, and compile errors or rejected filters before the marker leave existing files untouched.") as it is
  (the deviation then stands for the parts that are not evidenced) or split it into R10a (a proposed new requirement, not in the specification: terminal `run_state`, run-level presence of a
  terminal document, no change to files after rejected arguments or filters; delivered) and R10b (a proposed new requirement, not in the specification: the marker, a crash after it,
  the case that no harness completed; the marker is merged upstream (#5062) and its claims are partial).
- **Specification R12** ("Kani's reference consumer, scripts/validate_json_export.py, implements the RFC's consumer-side rules (versioning acceptance, vacuity predicates, selection and join guidance, no assumed output_dir, no inference of unrestricted proofs from an omitted disclosure) and rejects documents that break them."). It names the selection guidance, the `output_dir` rule and the no-inference rule. The reference consumer keeps only
  the version, vacuity, join, structure and warnings rules. Either narrow R12 ("Kani's reference consumer, scripts/validate_json_export.py, implements the RFC's consumer-side rules (versioning acceptance, vacuity predicates, selection and join guidance, no assumed output_dir, no inference of unrestricted proofs from an omitted disclosure) and rejects documents that break them.") to those rules (and drop claim `RFC15-D2` and the deviation that is recorded for R12 ("Kani's reference consumer, scripts/validate_json_export.py, implements the RFC's consumer-side rules (versioning acceptance, vacuity predicates, selection and join guidance, no assumed output_dir, no inference of unrestricted proofs from an omitted disclosure) and rejects documents that break them."))
  or restore the dropped rules with tests.
- **Specification R14** ("On adoption of RFC 0015 (merged 2026-09-24), tracking issues are filed upstream for the effective --object-bits provenance representation and for full tool/solver provenance and host machine metadata."). The delivery path of the tracking issues has changed. The deviation text is out of date.
- **Pinned RFC text.** The writer corrects the RFC: `resolved_solver` and `resolved_unwind` are what CBMC runs. The copy in `spec/`
  and the clauses `RFC0015-S-7.2-13` and `RFC0015-S-7.2-14` keep the older wording. The corrected RFC is merged upstream at `bf9c58e66`; re-pinning `spec/` and updating both clause titles is a pending consumer act. Then `S-7.2-13` can be mapped to the effective-solver tests.
- **OUT_OF_MEMORY clause.** `RFC0015-S-7.5-10` and the pinned RFC text keep the earlier wording: OUT_OF_MEMORY is inferred from a
  CBMC-child status of 137 when no property array exists. The writer corrects the RFC sentence: it is also inferred from CBMC's own
  `Out of memory` report, even when a property array exists, and a property array counts only when CBMC's exit status shows it
  finished reporting (0, 10, or 6 with an `ERROR` property); otherwise the harness is `CRASHED` with that status as `code`. The corrected
  RFC is merged upstream at `bf9c58e66`; re-pinning `spec/` and the clause title is a pending consumer act. Until then the clause is credited only for its stated 137 case, and the
  unit test of the corrected behaviour (`export_outcome_follows_cbmc_exit_status_and_results`) is listed under the partly evidenced
  clauses `S-7.1-40` and `S-7.1-41`.
- **Applicability record.** 29 rows are marked "applicable, PR 2". That text is from before the work was split into the writer, the tests and the optional consumer. Some of those rows are now delivered.
  The consumer-guidance rows `S-3.3-2`, `-22`, `-23`, `-24` and `S-5-15`, `-16` are applicable but no test evidences them.
- **Compound clauses.** The 60 partly evidenced clauses state more than one obligation. If the inventory splits them, the evidenced half can be counted.
- **Format limit.** The coverage tool calls a requirement satisfied as soon as one claim meets the floor. A requirement that is delivered
  in part can therefore read "satisfied". This package marks the partial claims of R10 ("A run that has begun verification atomically replaces any earlier file with an INCOMPLETE marker, a finished run writes the complete document with its run_state, run-level fields are present exactly as the RFC's run-level presence matrix says, and compile errors or rejected filters before the marker leave existing files untouched.") and R12 ("Kani's reference consumer, scripts/validate_json_export.py, implements the RFC's consumer-side rules (versioning acceptance, vacuity predicates, selection and join guidance, no assumed output_dir, no inference of unrestricted proofs from an omitted disclosure) and rejects documents that break them.") as unweighted to avoid that.
