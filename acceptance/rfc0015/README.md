# Acceptance package for RFC 0015 (`--export-json`)

This directory holds an acceptance package for the three commits that implement
[RFC 0015](../../rfc/src/rfcs/0015-export-json.md) (`--export-json`):

| commit | change |
|---|---|
| `b90379c7d` | PR 1: the writer produces the RFC 0015 document |
| `dd914b1ea` | PR 2: the reference consumer (`scripts/validate_json_export.py`) checks the cross-field rules |
| `0e71661f1` | PR 3: end-to-end tests |

The package is written in the [acceptance format](https://github.com/ivmat/acceptance-format)
version 0.3.2. It records, in machine-checkable form, what the contract asks for, which claims the
evidence supports, and which parts are not delivered. It is not part of the Kani build and no
Kani test reads it.

## Current status

- The package describes the code at commit `0e71661f12396b2df509b5397975f130854832c6`
  (the top of the stack). It is in this branch only because the branch adds this directory on top of that commit.
- **Requirement coverage is 12/15, and `acceptable=False`.** Always read the two together.
  R0 to R9, R11 and R13 are satisfied. R10 (completeness) and R12 (reference consumer) are
  **partial**. R14 (tracking issues) is a declared deviation.
- The contract is **not ratified**. The Kani maintainers have not reviewed it. For this reason
  `check-package` fails with one error: "the bound contract is not BINDING". This is expected.
- R10 is partial because the INCOMPLETE marker is not written by any of the three commits. R12 is partial
  because the reference consumer does not implement every rule that the contract names.
  The claims that are evidenced for R10 and R12 are marked `unweighted` on purpose, so that
  the tool reports "partial" and not "satisfied".
- The clause ledger (`ledger/acceptance.toml`) covers all 259 clauses of the RFC. Of the 210
  applicable clauses, 111 are evidenced and 99 are gaps. 51 of the gaps are clauses that the tests
  only partly assert. The 12/15 above counts requirements. It says nothing about clauses.
- The evidence is dynamic: unit tests, end-to-end test suites, and recorded runs checked by a script.
  For each evidenced claim, a control (a small patch that breaks the code) was shown to turn the check red.
  21 runs and 129 controls were executed, all at commit `0e71661f1`.
- The RFC text that the contract pins (`spec/`) has an older wording of `resolved_solver` and
  `resolved_unwind` than the one PR 1 implements. See "Open points".

## Check the package yourself

Run these commands from this directory. You need Python 3.11 or later and git. You do not need a Kani build.

```sh
cd acceptance/rfc0015
export PYTHONDONTWRITEBYTECODE=1

# 1. Get the checker. Use exactly this commit (the 0.3.2 release commit of the public repository).
git clone https://github.com/ivmat/acceptance-format.git vendor/acceptance-format
git -C vendor/acceptance-format checkout 455ca4f84ada3fc51eed30942696f34d118e1a59
V=vendor/acceptance-format

# 2. Check the contract, the package and the requirement coverage.
python3 $V/protocol_acceptance/tools/acceptance_protocol.py check-contract acceptance-contract.toml
python3 $V/format_acceptance/tools/check_acceptance.py --strict --strict-weight acceptance.toml
python3 $V/protocol_acceptance/tools/acceptance_protocol.py coverage acceptance.toml --contract acceptance-contract.toml
```

Expected result:

- `check-contract`: `PASS` with 3 notes.
- `check_acceptance`: `PASS` with 29 warnings (`dynamic-only evidence at band A1`). 29 claims are weighted and 11 are unweighted.
- `coverage`: the last line is `summary: mandatory 12/15, optional 0/0, acceptable=False`.

Two more checks:

```sh
# The clause ledger is a valid conformance manifest (expected: PASS, then a COVERAGE line with gap: 99).
python3 $V/format_acceptance/tools/check_core.py ledger/acceptance.toml --strict --strict-weight

# The delivery gate. Expected: FAIL with exactly one error, "the bound contract is not BINDING".
python3 $V/protocol_acceptance/tools/acceptance_protocol.py check-package acceptance.toml --contract acceptance-contract.toml
```

The `vendor/` directory is ignored by git.

## What is in this directory

| path | content |
|---|---|
| `acceptance-contract.toml` | the contract: requirements R0 to R14 for the whole RFC |
| `acceptance.toml` | the package: 40 claims (`RFC15-xx`) under those requirements, each with its evidence |
| `applicability.toml` | for each clause of the RFC: applicable, not applicable or excluded, with the reason |
| `standards/rfc0015.clauses.toml` | the clause inventory: one row for each normative unit of the RFC |
| `spec/0015-export-json.txt` | a byte copy of the RFC text that the contract pins (its digest is in the contract) |
| `ledger/acceptance.toml` | the conformance manifest: one claim for each clause. It is generated. Do not edit it |
| `ledger/gen_ledger.py` | generates `ledger/acceptance.toml` |
| `ledger/clause_requirements.toml` | for each applicable clause, the requirement whose claims may evidence it |
| `ledger/test_clauses.toml` | for each test, the clauses that it asserts (see below) |
| `ledger/test_citations.py` | helper of `gen_ledger.py` |
| `evidence/recipes.toml` | the registry of runs and controls, and which claim cites which one |
| `evidence/gen/` | the generated records: the output of every run and control |
| `evidence/patches/` | the patches that the controls apply |
| `gen_evidence.py` | runs the recipes and writes `evidence/gen/` |
| `restamp.py` | rewrites the header of `acceptance.toml` (contract hash, Kani commit, counts) |

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
and a separate, clean checkout of the stack top. The generator refuses to start if that checkout is not clean,
so do not use the checkout that holds this directory. The commands below put the new checkout next to the Kani clone.

```sh
git worktree add --detach ../../../kani-rfc0015-stack 0e71661f12396b2df509b5397975f130854832c6
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

- **Contract R10.** The text demands the INCOMPLETE marker, which none of the three commits writes. Either keep R10 as it is
  (the deviation then stands until the marker exists) or split it into R10a (terminal `run_state`, run-level presence of a
  terminal document, no change to files after rejected arguments or filters; delivered) and R10b (the marker, a crash after it,
  the case that no harness completed; not delivered).
- **Contract R12.** It names the selection guidance, the `output_dir` rule and the no-inference rule. The reference consumer keeps only
  the version, vacuity, join, structure and warnings rules. Either narrow R12 to those rules (and drop claim `RFC15-D2` and the R12 deviation)
  or restore the dropped rules with tests.
- **Contract R14.** The delivery path of the tracking issues has changed. The deviation text is out of date.
- **Pinned RFC text.** PR 1 corrects the RFC: `resolved_solver` and `resolved_unwind` are what CBMC runs. The copy in `spec/`
  and the clauses `RFC0015-S-7.2-13` and `RFC0015-S-7.2-14` keep the older wording. After the corrected RFC is merged, pin the new
  text and update both clauses. Then `S-7.2-13` can be mapped to the effective-solver tests.
- **Applicability record.** 29 rows are marked "applicable, PR 2". That text is from before the work was split in three commits. Some of those rows are now delivered.
  The consumer-guidance rows `S-3.3-2`, `-22`, `-23`, `-24` and `S-5-15`, `-16` are applicable but no test evidences them.
- **Compound clauses.** The 51 partly evidenced clauses state more than one obligation. If the inventory splits them, the evidenced half can be counted.
- **Format limit.** The coverage tool calls a requirement satisfied as soon as one claim meets the floor. A requirement that is delivered
  in part can therefore read "satisfied". This package marks the partial claims of R10 and R12 as unweighted to avoid that.
