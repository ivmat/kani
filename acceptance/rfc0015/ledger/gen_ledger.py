#!/usr/bin/env python3
# Copyright: internal (RFC 0015 PR-1 acceptance package)
"""gen_ledger.py -- generates ledger/acceptance.toml, the RFC 0015 clause CONFORMANCE manifest
(profile acceptance/conformance/code/rust, requirement R0 of contract AC-2026-RFC0015).

Never hand-edit ledger/acceptance.toml -- run this script instead (README.md "Evidence
generation" states the same rule for evidence/gen/; this is the same discipline for the ledger).

Inputs (all read-only, stdlib tomllib):
  ../standards/rfc0015.clauses.toml   the frozen clause inventory (259 rows, one per normative unit)
  ../applicability.toml               the consumer's applicability record (per-clause applicable /
                                       not-applicable / excluded + reason), declared before evidence
  clause_requirements.toml            per (applicable) clause: which contract REQUIREMENT (R0..R14)
                                       it was originally planned under
  ../acceptance.toml                  the PACKAGE (protocol S4): the RFC15-xx claims whose
                                       evidence this ledger borrows via class B8 acceptance-claim
                                       reference rows (core.md B8; PROFILE.md "Evidence borrowed
                                       from a verification manifest")

Mapping method (clause -> covering package claim(s)), in order:
  1. every clause's clause_requirements.toml `requirement` field names the package claim(s) whose
     `clause` field equals that requirement -- the "requirement bucket" (e.g. requirement R1 ->
     the four claims RFC15-01/-02/-20/-27);
  2. BD_CLAUSES below (20 clauses, the consumer-side reading rules) are a DECLARED OVERRIDE:
     their evidence was folded into ONE claim, RFC15-D1 (contract clause R12), regardless of their
     own individual `requirement` tag (7 of the 20 were already tagged R12; the other 13 keep their
     original R-number tag in clause_requirements.toml but are evidenced under R12/RFC15-D1). RFC15-D1 is
     ADDED to the requirement bucket, never a replacement;
  3. NO_CLAUSE_REQUIREMENT below excludes requirement "R0" from the requirement-bucket lookup: one
     clause_requirements.toml row (RFC0015-S-1-2) is tagged requirement="R0" (the RFC's own "this is the
     complete normative reference" sentence), but R0's own package claim is RFC15-L0 -- THIS
     LEDGER's own bridge claim. Citing RFC15-L0 as evidence for a content clause would be
     self-referential (the ledger citing itself as evidence about itself); excluded, gap instead
     (conservative: gap rather than a circular citation);
  4. within the (possibly BD-extended) requirement bucket, a candidate claim is KEPT only if it is
     `status = "evidenced"` AND the clause id is listed against some recipe that claim's own
     `[[claim.evidence]]` entries cite (`recipe = "<id>"`, per README.md "Evidence generation") in
     `../ledger/test_clauses.toml` -- the HAND-MAINTAINED, hand-verified mapping of test -> clause
     ids it truly asserts (see `test_citations.py` and README.md "Test-to-clause mapping"; each
     recipe's own `command` string is resolved to the test(s) it runs). This grounds the ledger in
     a mapping a human verified by reading each test, not in a claim's own hand-written prose (the
     original heuristic -- a literal substring match against the claim's statement/bounds/item/
     evidence-ref text) nor in a live scan of source comments (an intermediate mechanism, retired
     when the RFC0015-S-... ids were removed from Kani source entirely --
     `test_citations.py`'s scanner survives only as an optional `--cross-check`). A clause no test
     in the mapping file asserts falls to gap: see reason "no test names it" below. A test is
     listed against a clause only if it asserts an obligation of the clause on the production path
     (the rule is stated at the top of test_clauses.toml and in README.md), and a clause listed
     under `[[partial]]` there -- some obligation unsupported -- is a gap whatever tests cover the
     rest (step 4a);
  4a. (applied before step 4) a clause with a `[[partial]]` entry in test_clauses.toml is a
     gap, reason "partly evidenced": the entry records what a test asserts and what no test does,
     and the gap row carries both in its `bounds`;
  5. if that step finds one or more claims, ALL of them are cited (one B8 row each -- PROFILE.md's
     own multi-source shape, "one reference row PER contributing claim");
  6. otherwise: GAP, categorized in --report as one of: "partly evidenced" (step 4a), "R10 marker" (the clause's requirement is R10/R11
     and no evidenced claim covers it: the INCOMPLETE marker is not implemented in the stack, per
     acceptance.toml's own R10 [[deviation]] block), "process" (R14 --
     a tracking-issue fact no test could ever assert), "no requirement mapping" (no clause_requirements.toml
     row, or tagged R0 and excluded by NO_CLAUSE_REQUIREMENT), or "no test names it" (the clause has
     a requirement and an evidenced-claim bucket, but test_clauses.toml names no recipe any of those
     claims cite as asserting this specific clause id) -- never a guessed citation.

Run: `python3 -B gen_ledger.py --kani-worktree PATH` (writes ledger/acceptance.toml; PATH/the
`RFC0015_KANI_WORKTREE` env var name the local kani worktree this ledger's `[subject].commit`
stamp reads -- same portable-paths rule as gen_evidence.py/restamp.py, no built-in default; the
test-to-clause mapping itself comes only from `test_clauses.toml`, never the worktree). `--report`
additionally prints, to stderr, the full evidenced/gap/not-applicable/excluded tally and every gap's
reason -- `--selftest` runs a handful of fixed self-checks (BD_CLAUSES membership, the R0
self-reference exclusion, clause-id/requirement count sanity, and that test_clauses.toml names a
nonzero number of clause ids) and exits nonzero on any failure -- `--cross-check` additionally
re-scans the kani worktree's own source for any RFC0015-S-... id (the retired live-scan mechanism)
and reports each hit: normally empty, a nonzero result flags either a reintroduced source id or a
possibly-stale mapping entry (see `test_citations.cross_check_against_source`'s own docstring).
"""
from __future__ import annotations

import argparse
import os
import subprocess
import sys
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent
PKG = HERE.parent

sys.path.insert(0, str(PKG / "vendor/acceptance-format/format_acceptance/tools"))
import hashdomains  # noqa: E402  (vendored, closure-verified)

sys.path.insert(0, str(HERE))
import test_citations  # noqa: E402  (this directory; see its own module docstring)

STANDARDS_PATH = PKG / "standards" / "rfc0015.clauses.toml"
APPLICABILITY_PATH = PKG / "applicability.toml"
REQUIREMENTS_PATH = HERE / "clause_requirements.toml"
PACKAGE_PATH = PKG / "acceptance.toml"
RECIPES_PATH = PKG / "evidence" / "recipes.toml"
LEDGER_PATH = HERE / "acceptance.toml"

# 20 clauses (the consumer-side reading rules), all folded
# into ONE package claim (RFC15-D1, contract clause R12) -- see module docstring step 2.
BD_CLAUSES = {
    "RFC0015-S-3.3-1", "RFC0015-S-3.3-2", "RFC0015-S-3.3-4", "RFC0015-S-3.3-5",
    "RFC0015-S-3.3-6", "RFC0015-S-3.3-7", "RFC0015-S-3.3-10", "RFC0015-S-3.3-21",
    "RFC0015-S-3.3-22", "RFC0015-S-3.3-23", "RFC0015-S-3.3-24", "RFC0015-S-3.3-25",
    "RFC0015-S-4.3-3", "RFC0015-S-4.3-4", "RFC0015-S-4.3-7", "RFC0015-S-4.5-2",
    "RFC0015-S-5-15", "RFC0015-S-5-16", "RFC0015-S-7.5-13", "RFC0015-S-7.5-20",
}
BD_CLAIM_ID = "RFC15-D1"

# requirement tags that do not name a real per-clause evidence bucket -- see module docstring
# step 3 (self-reference avoidance: R0's own claim is this ledger's own bridge claim, RFC15-L0).
NO_CLAUSE_REQUIREMENT = {"R0"}

# requirement tags whose gap clauses get a specific --report reason instead of the generic
# "no test names it" (module docstring step 6).
PR2_REQUIREMENTS = {"R10", "R11"}
PROCESS_REQUIREMENTS = {"R14"}

GAP_GRADE = "not-covered"
EXCLUDED_GRADE = "out-of-scope"
BAND_ORDER = ("A0", "A1", "A2", "A3", "A4")
BAND_RANK = {b: i for i, b in enumerate(BAND_ORDER)}


def load_toml(path: Path) -> dict:
    with open(path, "rb") as f:
        return tomllib.load(f)


def claim_recipe_ids(claim: dict) -> set[str]:
    """Every `recipe = "<id>"` an evidence entry of this claim is bound to (README.md "Evidence
    generation": every `[[claim.evidence]]` entry carries its own recipe binding)."""
    out = set()
    for ev in claim.get("evidence", []) or []:
        if isinstance(ev, dict) and isinstance(ev.get("recipe"), str):
            out.add(ev["recipe"])
    return out


def check_unique_inputs(evidence_plan: dict, applicability: dict, package: dict) -> None:
    """Reject a repeated key in any input table BEFORE it is turned into a dict (a repeated row would
    silently overwrite the earlier one). test_clauses.toml is checked the same way inside
    test_citations.load_test_clauses."""
    test_citations.require_unique(evidence_plan["clause"], lambda r: r["id"],
                                  "ledger/clause_requirements.toml [[clause]] id")
    test_citations.require_unique(applicability["row"], lambda r: r["clause"],
                                  "applicability.toml [[row]] clause")
    test_citations.require_unique([c for c in package["claim"] if isinstance(c, dict) and "id" in c],
                                  lambda c: c["id"], "acceptance.toml [[claim]] id")


def build_claim_covered_clauses(package: dict, recipes: dict, sources: "test_citations.Sources") -> dict[str, set[str]]:
    """claim id -> the union of clause ids named by the TEST SOURCE of every recipe that claim's
    own evidence entries cite (module docstring step 4)."""
    commands_by_id = {}
    for kind in ("run", "control"):
        for r in recipes.get(kind, []):
            commands_by_id[r["id"]] = r.get("command", "")
    recipe_clauses_cache: dict[str, set[str]] = {}

    def recipe_clauses(recipe_id: str) -> set[str]:
        if recipe_id not in recipe_clauses_cache:
            command = commands_by_id.get(recipe_id)
            recipe_clauses_cache[recipe_id] = (
                sources.recipe_covered_clauses(command) if command is not None else set()
            )
        return recipe_clauses_cache[recipe_id]

    out: dict[str, set[str]] = {}
    for c in package["claim"]:
        if not (isinstance(c, dict) and "id" in c):
            continue
        covered: set[str] = set()
        for recipe_id in claim_recipe_ids(c):
            if recipe_id not in commands_by_id:
                sys.exit(f"FATAL: claim {c['id']!r} cites unknown recipe {recipe_id!r} "
                          "(not found in evidence/recipes.toml)")
            covered |= recipe_clauses(recipe_id)
        out[c["id"]] = covered
    return out


def native_pass_record_hashes(claim: dict) -> list[str]:
    """RFC0015-S-... B8 `records`: every NATIVE (kind != acceptance-claim), non-control, passing
    evidence entry's own record_hash from the SOURCE claim -- exact membership, `record:` domain
    only (check_core.check_reference_evidence steps 4-5)."""
    out = []
    for ev in claim.get("evidence", []) or []:
        if not isinstance(ev, dict):
            continue
        if ev.get("kind") == "acceptance-claim":
            continue
        if "control" in ev:
            continue
        if ev.get("result") != "pass":
            continue
        rh = ev.get("record_hash", "")
        if isinstance(rh, str) and rh.startswith("record:"):
            out.append(rh)
    return out


def build(*, report: bool, kani_worktree: Path) -> str:
    standards = load_toml(STANDARDS_PATH)
    applicability = load_toml(APPLICABILITY_PATH)
    evidence_plan = load_toml(REQUIREMENTS_PATH)
    package = load_toml(PACKAGE_PATH)
    recipes = load_toml(RECIPES_PATH)
    check_unique_inputs(evidence_plan, applicability, package)

    sources = test_citations.Sources()
    claim_covered_clauses = build_claim_covered_clauses(package, recipes, sources)

    kani_head = subprocess.check_output(
        ["git", "-C", str(kani_worktree), "rev-parse", "HEAD"], text=True
    ).strip()
    kani_dirty = bool(subprocess.check_output(
        ["git", "-C", str(kani_worktree), "status", "--porcelain"], text=True
    ).strip())
    if kani_dirty:
        sys.exit("FATAL: kani worktree is dirty -- the ledger's test-citation scan and its "
                 "[subject] commit stamp both need a clean, checkpointed kani HEAD (same rule as "
                 "restamp.py's --mode checkpoint)")

    standard_id = standards["standard"]["id"]
    clauses = standards["clause"]
    clause_ids = [c["id"] for c in clauses]
    if len(set(clause_ids)) != len(clause_ids):
        sys.exit("FATAL: duplicate clause id in standards/rfc0015.clauses.toml")

    by_row = {r["clause"]: r for r in applicability["row"]}
    missing_rows = [cid for cid in clause_ids if cid not in by_row]
    if missing_rows:
        sys.exit(f"FATAL: applicability.toml has no row for: {missing_rows[:5]} (+{len(missing_rows) - 5} more)"
                  if len(missing_rows) > 5 else f"FATAL: applicability.toml has no row for: {missing_rows}")
    if applicability["standard"] != standard_id:
        sys.exit("FATAL: applicability.toml [standard] does not match the inventory's [standard].id")

    plan_by_clause = {row["id"]: row for row in evidence_plan["clause"]}

    claims_by_id = {c["id"]: c for c in package["claim"] if isinstance(c, dict) and "id" in c}
    claims_by_requirement: dict[str, list[dict]] = {}
    for c in claims_by_id.values():
        claims_by_requirement.setdefault(c.get("clause"), []).append(c)

    package_raw = PACKAGE_PATH.read_bytes()
    manifest_pointer = "../acceptance.toml"
    manifest_hash = hashdomains.digest("manifest:", package_raw)

    applicability_raw = APPLICABILITY_PATH.read_bytes()
    applicability_hash = hashdomains.digest("applicability-record:", applicability_raw)
    declared_at = applicability["declared_at"]

    n_applicable = n_not_applicable = n_excluded = n_evidenced = n_gap = 0
    gap_reasons: dict[str, list[str]] = {"no test names it": [], "partly evidenced": [], "R10 marker": [],
                                          "process": [], "no requirement mapping": []}
    generated_claims: list[str] = []  # rendered TOML text, one block per clause, in inventory order

    for clause in clauses:
        cid = clause["id"]
        row = by_row[cid]
        applicability_token = row["applicability"]
        reason = row.get("reason", "")
        statement = clause["title"]
        item = clause.get("ref", cid)
        gen_id = "CONF/" + cid

        if applicability_token == "not-applicable":
            n_not_applicable += 1
            generated_claims.append(render_not_applicable(gen_id, cid, item, statement, reason))
            continue
        if applicability_token == "excluded":
            n_excluded += 1
            generated_claims.append(render_excluded(gen_id, cid, item, statement, reason))
            continue

        n_applicable += 1
        plan_row = plan_by_clause.get(cid)
        requirement = plan_row.get("requirement") if plan_row else None

        if requirement is None or requirement in NO_CLAUSE_REQUIREMENT:
            n_gap += 1
            gap_reasons["no requirement mapping"].append(cid)
            generated_claims.append(render_gap(gen_id, cid, item, statement, reason))
            continue

        if cid in sources.partial:
            n_gap += 1
            gap_reasons["partly evidenced"].append(cid)
            generated_claims.append(render_gap(gen_id, cid, item, statement, reason,
                                               partial_bounds(sources.partial[cid])))
            continue

        bucket: list[dict] = list(claims_by_requirement.get(requirement, []))
        if cid in BD_CLAUSES:
            bd_claim = claims_by_id.get(BD_CLAIM_ID)
            if bd_claim is not None and bd_claim not in bucket:
                bucket.append(bd_claim)

        evidenced_bucket = [c for c in bucket if c.get("status") == "evidenced"]
        contributors = sorted(
            (c for c in evidenced_bucket if cid in claim_covered_clauses.get(c["id"], set())),
            key=lambda c: c["id"],
        )

        if contributors:
            n_evidenced += 1
            generated_claims.append(render_evidenced(
                gen_id, cid, item, statement, reason, contributors, manifest_pointer, manifest_hash,
            ))
        else:
            n_gap += 1
            if requirement in PR2_REQUIREMENTS:
                gap_reasons["R10 marker"].append(cid)
            elif requirement in PROCESS_REQUIREMENTS:
                gap_reasons["process"].append(cid)
            else:
                gap_reasons["no test names it"].append(cid)
            generated_claims.append(render_gap(gen_id, cid, item, statement, reason))

    header = render_header(
        standard_id=standard_id,
        standards_raw=STANDARDS_PATH.read_bytes(),
        declared_at=declared_at,
        applicability_hash=applicability_hash,
        clauses_total=len(clause_ids),
        kani_head=kani_head,
    )
    body = "\n".join(generated_claims)
    out = header + body + "\n"

    if report:
        print(
            f"clauses_total={len(clause_ids)} applicable={n_applicable} "
            f"not_applicable={n_not_applicable} excluded={n_excluded} "
            f"evidenced={n_evidenced} gap={n_gap}",
            file=sys.stderr,
        )
        for reason, ids in gap_reasons.items():
            if ids:
                print(f"gap reason {reason!r} ({len(ids)}): {ids}", file=sys.stderr)
    return out


def render_header(*, standard_id, standards_raw, declared_at, applicability_hash, clauses_total, kani_head) -> str:
    inventory_hash = hashdomains.digest("normative-reference:", standards_raw)
    return f'''# ledger/acceptance.toml -- GENERATED by gen_ledger.py. DO NOT HAND-EDIT (see gen_ledger.py's
# own module docstring: "Never hand-edit ledger/acceptance.toml -- run this script instead").
#
# The RFC 0015 clause CONFORMANCE manifest (protocol S4), profile acceptance/conformance/code/rust
# -- contract AC-2026-RFC0015's requirement R0 ("the clause ledger validates and every clause is
# accounted for"). One claim per clause of the frozen inventory (../standards/rfc0015.clauses.toml,
# 259 rows); applicability comes from ../applicability.toml, declared before any evidence was read
# (2026-09-28). An APPLICABLE clause with evidence borrows it from ../acceptance.toml (the PACKAGE)
# via a class B8 acceptance-claim reference row per contributing package claim -- never freshly
# re-run, never copied verbatim (core.md B8; PROFILE.md "Evidence borrowed from a verification
# manifest"); a borrowed claim is unweighted by construction (B8 step 6).
#
# VALIDATE (from this directory) with check_core.py, the conformance-meaning checker:
#   python3 -B ../vendor/acceptance-format/format_acceptance/tools/check_core.py --strict --strict-weight acceptance.toml
# (check_acceptance.py is NOT the checker for this manifest: its entry point forces the 'verification' meaning
# and answers INDETERMINATE for a conformance ledger.)

[format]
id       = "acceptance/0"
protocol = "acceptance-protocol/0"
profile  = "acceptance/conformance/code/rust"
profile_version = "0.2.0"
generated_by = "gen_ledger.py"
closure_commit = "455ca4f84ada3fc51eed30942696f34d118e1a59"

[subject]
name   = "model-checking/kani -- RFC 0015 (export-json) clause conformance ledger"
kind   = "rust-workspace"
mode   = "retrospective"
commit = "{kani_head}"
dirty  = false
repo   = "github.com/model-checking/kani (branch export-json-rfc0015, not yet pushed)"

[spec]
path       = "../standards/rfc0015.clauses.toml"
provenance = "external"
version    = "{inventory_hash}"
axis       = "one claim per clause of the declared slice of {standard_id} (whole RFC, {clauses_total} clauses)"

[coverage]
clauses_total = {clauses_total}
claims_total  = {clauses_total}

[conformance]
standard                  = "{standard_id}"
applicability_record      = "../applicability.toml"
applicability_hash        = "{applicability_hash}"
applicability_declared_at = "{declared_at}"

'''


def _toml_str(s: str) -> str:
    escaped = s.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n")
    return f'"{escaped}"'


def render_not_applicable(gen_id, clause_id, item, statement, reason) -> str:
    return (
        "[[claim]]\n"
        f'id                 = {_toml_str(gen_id)}\n'
        f'clause             = {_toml_str(clause_id)}\n'
        f'item               = {_toml_str(item)}\n'
        f'statement          = {_toml_str(statement)}\n'
        'status             = "not-applicable"\n'
        'applicability      = "not-applicable"\n'
        f'applicability_reason = {_toml_str(reason)}\n'
        'clause_source      = "spec-document"\n\n'
    )


def render_excluded(gen_id, clause_id, item, statement, reason) -> str:
    return (
        "[[claim]]\n"
        f'id                 = {_toml_str(gen_id)}\n'
        f'clause             = {_toml_str(clause_id)}\n'
        f'item               = {_toml_str(item)}\n'
        f'statement          = {_toml_str(statement)}\n'
        'status             = "gap"\n'
        'applicability      = "excluded"\n'
        f'applicability_reason = {_toml_str(reason)}\n'
        f'scope_ref          = {_toml_str("../applicability.toml#" + clause_id)}\n'
        f'weight             = "unweighted"\n'
        f'grade              = "{EXCLUDED_GRADE}"\n'
        'band               = "A0"\n'
        'clause_source      = "spec-document"\n\n'
    )


def partial_bounds(entry: dict) -> str:
    tests = ", ".join(entry["tests"])
    return (f"PARTLY EVIDENCED, so a gap (class: {entry['class']}). Asserted: {entry['supported']} "
            f"(by {tests}). NOT evidenced: {entry['missing']}.")


def render_gap(gen_id, clause_id, item, statement, reason, bounds=None) -> str:
    bounds_line = f'bounds        = {_toml_str(bounds)}\n' if bounds else ""
    return (
        "[[claim]]\n"
        f'id            = {_toml_str(gen_id)}\n'
        f'clause        = {_toml_str(clause_id)}\n'
        f'item          = {_toml_str(item)}\n'
        f'statement     = {_toml_str(statement)}\n'
        'status        = "gap"\n'
        'applicability = "applicable"\n'
        f'applicability_reason = {_toml_str(reason)}\n'
        'weight        = "unweighted"\n'
        f'grade         = "{GAP_GRADE}"\n'
        'band          = "A0"\n'
        'clause_source = "spec-document"\n'
        + bounds_line + "\n"
    )


def render_evidenced(gen_id, clause_id, item, statement, reason, contributors, manifest_pointer, manifest_hash) -> str:
    # B8 step 7 / check_core's control_free_ceiling: the "reference" family has no entry in the
    # verification ladder's control_free_ceiling table, so it falls to "default" = A0 -- a
    # reference-only (control-free, "controls do not transfer") claim can never band higher than
    # A0, REGARDLESS of the contributing claims' own (control-backed) band. Not a downgrade this
    # generator invents: it is what the format itself enforces for borrowed evidence.
    band = "A0"
    grade = contributors[0].get("grade", "ungraded")
    contributor_ids = ", ".join(c["id"] for c in contributors)
    out = (
        "[[claim]]\n"
        f'id            = {_toml_str(gen_id)}\n'
        f'clause        = {_toml_str(clause_id)}\n'
        f'item          = {_toml_str(item)}\n'
        f'statement     = {_toml_str(statement)}\n'
        'status        = "evidenced"\n'
        'applicability = "applicable"\n'
        f'applicability_reason = {_toml_str(reason)}\n'
        'weight        = "unweighted"\n'
        f'grade         = "{grade}"\n'
        f'band          = "{band}"\n'
        'clause_source = "spec-document"\n'
        f'bounds        = {_toml_str(f"borrowed, unweighted (B8): discharged by {contributor_ids} in the PR-1 package (../acceptance.toml); band is the minimum of every contributing claim's own band; the actual evidence, controls and freshness live there, not duplicated here.")}\n'
    )
    for c in contributors:
        records = native_pass_record_hashes(c)
        if not records:
            sys.exit(f"FATAL: contributing claim {c['id']!r} (for clause {clause_id!r}) has no "
                      "native passing evidence record to borrow -- B8 requires a nonempty records list")
        records_toml = ", ".join(_toml_str(r) for r in records)
        out += (
            "\n  [[claim.evidence]]\n"
            '  kind          = "acceptance-claim"\n'
            '  family        = "reference"\n'
            f'  ref           = {_toml_str(manifest_pointer + "#" + c["id"])}\n'
            '  result        = "pass"\n'
            '  tool          = "gen_ledger.py (class B8 reference; no tool independently run here -- see the cited claim\'s own evidence for the actual tool)"\n'
            f'  manifest      = {_toml_str(manifest_pointer)}\n'
            f'  manifest_hash = {_toml_str(manifest_hash)}\n'
            f'  claim         = {_toml_str(c["id"])}\n'
            f'  record        = {_toml_str(manifest_pointer)}\n'
            f'  record_hash   = {_toml_str(manifest_hash)}\n'
            f'  records       = [{records_toml}]\n'
        )
    return out + "\n"


def selftest(kani_worktree: Path) -> int:
    failures = []
    standards = load_toml(STANDARDS_PATH)
    applicability = load_toml(APPLICABILITY_PATH)
    evidence_plan = load_toml(REQUIREMENTS_PATH)
    package = load_toml(PACKAGE_PATH)
    recipes = load_toml(RECIPES_PATH)

    try:
        check_unique_inputs(evidence_plan, applicability, package)
    except ValueError as exc:
        failures.append(str(exc))

    clause_ids = {c["id"] for c in standards["clause"]}
    if not BD_CLAUSES <= clause_ids:
        failures.append(f"BD_CLAUSES contains ids outside the inventory: {BD_CLAUSES - clause_ids}")

    plan_ids = {r["id"] for r in evidence_plan["clause"]}
    row_ids = {r["clause"] for r in applicability["row"]}
    applicable_ids = {r["clause"] for r in applicability["row"] if r["applicability"] == "applicable"}
    if plan_ids != applicable_ids:
        failures.append(
            f"clause_requirements.toml rows ({len(plan_ids)}) do not exactly match applicability.toml's "
            f"'applicable' rows ({len(applicable_ids)}); symmetric diff: {plan_ids ^ applicable_ids}"
        )
    if clause_ids != row_ids:
        failures.append(f"inventory clause ids and applicability.toml row ids differ: {clause_ids ^ row_ids}")

    # test_clauses.toml must name a real, nonzero signal (not a vacuous no-op), and RFC15-23's
    # own recipe(s) must actually name RFC0015-S-3.3-5 (its real subject, per test_clauses.toml's
    # own "vacuity-check" / RFC15-23-citing entries).
    sources = test_citations.Sources()
    if not sources.all_rust_test_clauses:
        failures.append("test_clauses.toml names no clause ids for any rust-test entry "
                         "(selftest fixture assumption broken, or the mapping file is empty)")
    if not sources.all_json_suite_clauses:
        failures.append("test_clauses.toml names no clause ids for any json-suite entry")
    not_applicable = sorted(set(sources.partial) - applicable_ids)
    if not_applicable:
        failures.append(f"[[partial]] entries for clauses that are not applicable: {not_applicable}")
    claims_by_id = {c["id"]: c for c in package["claim"] if isinstance(c, dict) and "id" in c}
    claim_covered_clauses = build_claim_covered_clauses(package, recipes, sources)
    rfc15_23 = claims_by_id.get("RFC15-23")
    if rfc15_23 is None:
        failures.append("package claim RFC15-23 not found (selftest fixture assumption broken)")
    elif "RFC0015-S-3.3-5" not in claim_covered_clauses.get("RFC15-23", set()):
        failures.append("RFC15-23's own cited recipe(s) should name RFC0015-S-3.3-5 in their test "
                         "source (it is RFC15-23's real subject, per validator-fixtures/test.sh)")

    # R0 self-reference exclusion: RFC0015-S-1-2 is tagged requirement=R0 in clause_requirements.toml;
    # NO_CLAUSE_REQUIREMENT must exclude it from ever citing RFC15-L0.
    plan_by_clause = {r["id"]: r for r in evidence_plan["clause"]}
    s12 = plan_by_clause.get("RFC0015-S-1-2")
    if s12 is None or s12.get("requirement") != "R0":
        failures.append("selftest assumption broken: RFC0015-S-1-2 is no longer tagged requirement=R0")
    if "R0" not in NO_CLAUSE_REQUIREMENT:
        failures.append("NO_CLAUSE_REQUIREMENT must contain R0 (self-reference guard)")

    # Every clause id the mapping file names exists in the frozen inventory (a typo would otherwise
    # silently map nothing).
    named = (sources.all_rust_test_clauses | sources.all_json_suite_clauses | sources.validator_citations
             | set(sources.partial))
    unknown = sorted(named - clause_ids)
    if unknown:
        failures.append(f"test_clauses.toml names clause ids that are not in the inventory: {unknown}")

    # Duplicate-row rejection (hardening): each synthetic duplicate must be refused, and the unique
    # control must not be.
    failures.extend(selftest_duplicate_rejection())

    # The generated header must recommend check_core.py (check_acceptance.py is INDETERMINATE for a ledger).
    header = render_header(standard_id="x", standards_raw=b"", declared_at="x", applicability_hash="x",
                           clauses_total=0, kani_head="x")
    recommended = [line for line in header.splitlines() if line.startswith("#   python3")]
    if len(recommended) != 1 or "check_core.py" not in recommended[0] or "check_acceptance.py" in recommended[0]:
        failures.append(f"the generated header must recommend check_core.py, got {recommended}")

    if failures:
        for f in failures:
            print(f"SELFTEST FAIL: {f}", file=sys.stderr)
        return 1
    print(f"SELFTEST OK ({SELFTEST_CHECKS} checks)", file=sys.stderr)
    return 0


SELFTEST_CHECKS = 13


def selftest_duplicate_rejection() -> list[str]:
    """Synthetic inputs: a duplicate requirement row, a duplicate [[test]] key, a duplicate [[partial]]
    key, a clause listed twice in one [[test]], and a repeated validator entry must each raise
    ValueError; the same inputs without the duplicate must load."""
    import tempfile

    failures: list[str] = []

    def must_raise(label: str, thunk) -> None:
        try:
            thunk()
        except ValueError:
            return
        failures.append(f"duplicate not rejected: {label}")

    def must_pass(label: str, thunk) -> None:
        try:
            thunk()
        except ValueError as exc:
            failures.append(f"unique control wrongly rejected ({label}): {exc}")

    rows_dup = [{"id": "RFC0015-S-1-2"}, {"id": "RFC0015-S-3-2"}, {"id": "RFC0015-S-1-2"}]
    rows_ok = rows_dup[:2]
    must_raise("clause_requirements row", lambda: test_citations.require_unique(rows_dup, lambda r: r["id"], "synthetic"))
    must_pass("clause_requirements row", lambda: test_citations.require_unique(rows_ok, lambda r: r["id"], "synthetic"))

    base = (
        '[[test]]\nkind = "rust-test"\ncrate = "kani-driver"\nname = "t1"\nclauses = ["RFC0015-S-3-2"]\n\n'
        '[[test]]\nkind = "validator"\nname = "validate_json_export.py"\nclauses = ["RFC0015-S-3.3-21"]\n\n'
        '[[partial]]\nclause = "RFC0015-S-3-3"\nclass = "other"\nsupported = "s"\nmissing = "m"\ntests = ["t1"]\n'
    )
    variants = {
        "[[test]] key": base + '\n[[test]]\nkind = "rust-test"\ncrate = "kani-driver"\nname = "t1"\nclauses = ["RFC0015-S-3.1-1"]\n',
        "validator entry": base + '\n[[test]]\nkind = "validator"\nname = "validate_json_export.py"\nclauses = ["RFC0015-S-3.3-4"]\n',
        "[[partial]] key": base + '\n[[partial]]\nclause = "RFC0015-S-3-3"\nclass = "other"\nsupported = "s"\nmissing = "m"\ntests = ["t1"]\n',
        "clause twice in one [[test]]": base.replace('clauses = ["RFC0015-S-3-2"]', 'clauses = ["RFC0015-S-3-2", "RFC0015-S-3-2"]'),
    }
    with tempfile.TemporaryDirectory() as tmp:
        def load(label: str, text: str):
            path = Path(tmp) / (label.replace(" ", "_").replace("[", "").replace("]", "") + ".toml")
            path.write_text(text)
            return test_citations.load_test_clauses(path)

        must_pass("test_clauses.toml", lambda: load("control", base))
        for label, text in variants.items():
            must_raise(label, lambda label=label, text=text: load(label, text))
    return failures


def resolve_kani_worktree(arg: str | None) -> Path:
    kani_worktree = arg or os.environ.get("RFC0015_KANI_WORKTREE")
    if not kani_worktree:
        sys.exit("gen_ledger: --kani-worktree PATH or RFC0015_KANI_WORKTREE is required (no built-in default)")
    path = Path(kani_worktree).expanduser().resolve()
    if not (path / ".git").exists():
        sys.exit(f"gen_ledger: kani_worktree {kani_worktree!r} has no .git -- not a git worktree/clone")
    return path


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--kani-worktree", default=None,
                         help="local kani worktree/clone (or env RFC0015_KANI_WORKTREE) -- required, "
                              "no default; the [subject] commit stamp reads it (the test-to-clause "
                              "mapping itself comes only from test_clauses.toml)")
    parser.add_argument("--report", action="store_true", help="print the clause tally + notes to stderr")
    parser.add_argument("--selftest", action="store_true", help="run the fixed self-checks and exit")
    parser.add_argument("--check", action="store_true",
                         help="build in memory and diff against the file on disk; nonzero exit on drift")
    parser.add_argument("--cross-check", action="store_true",
                         help="re-scan the kani worktree's own source for any RFC0015-S-... id "
                              "(the retired live-scan mechanism) and report each hit to stderr; "
                              "exits nonzero if any are found")
    args = parser.parse_args()
    kani_worktree = resolve_kani_worktree(args.kani_worktree)

    if args.cross_check:
        findings = test_citations.cross_check_against_source(kani_worktree)
        if findings:
            for f in findings:
                print(f"CROSS-CHECK FINDING: {f}", file=sys.stderr)
            return 1
        print("cross-check OK: no RFC0015-S-... id found in kani source", file=sys.stderr)
        return 0

    if args.selftest:
        return selftest(kani_worktree)

    try:
        out = build(report=args.report, kani_worktree=kani_worktree)
    except ValueError as exc:
        print(f"FATAL: {exc}", file=sys.stderr)
        return 1
    if args.check:
        current = LEDGER_PATH.read_text() if LEDGER_PATH.exists() else None
        if current != out:
            print("DRIFT: ledger/acceptance.toml does not match what gen_ledger.py would generate "
                  "-- run `python3 -B gen_ledger.py` and commit the result", file=sys.stderr)
            return 1
        print("ledger/acceptance.toml is up to date", file=sys.stderr)
        return 0

    LEDGER_PATH.write_text(out)
    print(f"wrote {LEDGER_PATH}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
