# Copyright: internal (RFC 0015 PR-1 acceptance package)
"""test_citations.py -- resolves each test named in `../ledger/test_clauses.toml` (the
HAND-MAINTAINED mapping: for each test, the clause ids it truly asserts -- see README.md "Test-to-
clause mapping") to the evidence/recipes.toml recipe(s) that actually execute it. Imported by
gen_ledger.py (see its own module docstring, "Mapping method" step 4): a clause is only cited via
a package claim if that claim has an evidence entry bound to a recipe whose test(s), per
test_clauses.toml, assert the clause -- grounding the ledger in a hand-verified, hand-maintained
mapping, not in a live scan of source comments (the Kani source itself carries no RFC0015-S-...
ids: they were removed from it, so the tracking lives only in the mapping file).

Three source kinds, matching the actual granularity `evidence/recipes.toml` addresses tests at
(same three `kind` values `test_clauses.toml` uses):

  - "rust-test": a `#[test] fn <name>` in a kani-driver/kani-compiler source file, identified by
    its BARE function name only (no module-path reconstruction): every `[[control]]`/`[[run]]`
    recipe in recipes.toml that targets a specific rust test already names it as
    `cargo test -p <crate> <dotted::path>::<name>` -- taking just the trailing `<name>` is enough
    to resolve it back to the same source function (cargo's own test filter is a substring match,
    so recipes.toml never needed the full path either).
  - "json-suite": a `tests/json-handler/<dir>/` exec suite, at suite granularity (matching
    `--force-rerun <dir>` recipes exactly).
  - "validator": scripts/validate_json_export.py, as ONE bucket (its predicates and rules are exercised by
    the fixture checker tests/json-handler/schema-validation/check_fixtures.py, which the pure-python
    `validator-fixtures` recipe runs on its own -- see `FIXTURE_CHECKER_RE` below).

`scan_*_citations` below (an RFC0015-S-... regex scan of the kani worktree's own source) is kept
ONLY as an optional cross-check (`gen_ledger.py --cross-check`), not the primary mapping source:
it can no longer find anything (the source carries no ids), so its only remaining use
is a historical/regression check that nothing reintroduces bare ids into source comments.
"""
from __future__ import annotations

import re
import tomllib
from pathlib import Path

CLAUSE_RE = re.compile(r"RFC0015-S-[0-9]+(?:\.[0-9]+)?-[0-9]+")

TEST_ATTR_RE = re.compile(r"^\s*#\[\s*(test|should_panic)")
ATTR_RE = re.compile(r"^\s*#\[")
COMMENT_RE = re.compile(r"^\s*//")
FN_RE = re.compile(r"^\s*(?:pub\s+)?(?:async\s+)?fn\s+(\w+)\s*(?:<[^>]*>)?\s*\(")


def clause_ids_in(text: str) -> set[str]:
    return set(CLAUSE_RE.findall(text))


def scan_rust_test_citations(rust_files: list[Path]) -> dict[str, set[str]]:
    """bare test function name -> the set of clause ids cited in its doc preamble or body, across
    every file in `rust_files`. A name that appears (implausibly) in more than one file has its
    citations unioned -- recipes.toml resolves by bare name only, so this scanner must too."""
    out: dict[str, set[str]] = {}
    for path in rust_files:
        lines = path.read_text().splitlines()
        i = 0
        n = len(lines)
        while i < n:
            if TEST_ATTR_RE.match(lines[i]):
                # Walk forward over any further attributes (#[should_panic], #[allow(...)], ...)
                # to the `fn` line itself.
                j = i + 1
                while j < n and (ATTR_RE.match(lines[j]) or lines[j].strip() == ""):
                    j += 1
                m = FN_RE.match(lines[j]) if j < n else None
                if not m:
                    i += 1
                    continue
                name = m.group(1)
                # Preamble: contiguous comment lines directly above the #[test] attribute.
                k = i - 1
                preamble = []
                while k >= 0 and (COMMENT_RE.match(lines[k]) or lines[k].strip() == ""):
                    preamble.append(lines[k])
                    k -= 1
                # Body: brace-match from the `fn` line's opening `{` to the matching close.
                body_lines = []
                depth = 0
                started = False
                b = j
                while b < n:
                    line = lines[b]
                    body_lines.append(line)
                    depth += line.count("{") - line.count("}")
                    if "{" in line:
                        started = True
                    if started and depth <= 0:
                        break
                    b += 1
                cited = clause_ids_in("\n".join(preamble)) | clause_ids_in("\n".join(body_lines))
                if cited:
                    out.setdefault(name, set()).update(cited)
                i = b + 1
                continue
            i += 1
    return out


def scan_json_suite_citations(json_handler_dir: Path) -> dict[str, set[str]]:
    """suite dirname (immediate child of tests/json-handler/) -> clause ids cited anywhere in its
    own files (test.sh, test.rs, any other *.rs helper)."""
    out: dict[str, set[str]] = {}
    if not json_handler_dir.is_dir():
        return out
    for suite_dir in sorted(p for p in json_handler_dir.iterdir() if p.is_dir()):
        cited: set[str] = set()
        for pattern in ("test.sh", "test.rs", "*.rs"):
            for f in suite_dir.glob(pattern):
                cited |= clause_ids_in(f.read_text())
        if cited:
            out[suite_dir.name] = cited
    return out


def scan_validator_citations(validator_path: Path) -> set[str]:
    if not validator_path.is_file():
        return set()
    return clause_ids_in(validator_path.read_text())


# --- recipe -> covered clause ids -----------------------------------------------------------

RUST_TEST_CMD_RE = re.compile(r"cargo test -p kani-(driver|compiler)(?:\s+(\S+))?")
FORCE_RERUN_RE = re.compile(r"--force-rerun\s+(\S+)")
BASH_SUITE_RE = re.compile(r"bash tests/json-handler/(\S+)/test\.sh")

# A recipe that runs the reference consumer's fixture checker directly (the `validator-fixtures` recipe) covers the
# clauses of the `validator` bucket. The schema-validation e2e suite also ends by running that checker, but a suite
# recipe is mapped by its own `json-suite` entry only -- disclosed, conservative: every json-handler suite also
# invokes the validator on its own real output, which checks accept/reject only, not a named rule.
FIXTURE_CHECKER_RE = re.compile(r"python3 check_fixtures\.py")

# Retained for recipes of the older shape (`bash tests/json-handler/<suite>/test.sh`); no recipe uses it now.
VALIDATOR_EXERCISER_SUITES: set[str] = set()


TEST_CLAUSES_PATH = Path(__file__).resolve().parent / "test_clauses.toml"


def require_unique(rows, keyfn, where: str) -> None:
    """Raise ValueError naming every key that occurs more than once in `rows`. Called on a raw TOML
    table BEFORE it is turned into a dict, where a repeated key would silently overwrite (or, for the
    `validator` bucket, union into) the earlier row."""
    seen: set = set()
    repeated: list = []
    for row in rows:
        key = keyfn(row)
        if key in seen and key not in repeated:
            repeated.append(key)
        seen.add(key)
    if repeated:
        raise ValueError(f"{where}: duplicate key(s) {repeated}")


def load_test_clauses(path: Path = TEST_CLAUSES_PATH) -> dict[str, dict]:
    """Parses `test_clauses.toml` into three dicts, keyed the same way the (retired) live
    scanner's own dicts were: `rust_test_citations`/`json_suite_citations` (name -> set of clause
    ids) and `validator_citations` (a bare set of clause ids). Every `[[test]]` entry's `kind`
    must be one of "rust-test"/"json-suite"/"validator"; a "rust-test" entry's `name` must be
    unique across the whole file (recipes.toml resolves by bare name only, same rule the old
    scanner's own docstring already stated), and a "json-suite" entry's `name` likewise; a
    "validator" entry's `clauses` are unioned into one set (there is exactly one bucket, but the
    format does not special-case that -- a second "validator" entry, if ever added, would simply
    union in too).
    """
    with open(path, "rb") as f:
        data = tomllib.load(f)
    # Reject repeated keys before any dict is built: a repeated [[test]] (kind, name) or [[partial]]
    # clause, a clause id listed twice in one [[test]], and a repeated `validator` entry.
    require_unique(data.get("test", []), lambda e: (e["kind"], e["name"]), "test_clauses.toml [[test]] (kind, name)")
    require_unique(data.get("partial", []), lambda e: e["clause"], "test_clauses.toml [[partial]] clause")
    for entry in data.get("test", []):
        require_unique(entry["clauses"], lambda c: c,
                       f"test_clauses.toml [[test]] {entry['name']!r} clauses")
    rust_test_citations: dict[str, set[str]] = {}
    json_suite_citations: dict[str, set[str]] = {}
    validator_citations: set[str] = set()
    for entry in data.get("test", []):
        kind = entry["kind"]
        clauses = set(entry["clauses"])
        if kind == "rust-test":
            name = entry["name"]
            if name in rust_test_citations:
                raise ValueError(f"test_clauses.toml: duplicate rust-test name {name!r}")
            rust_test_citations[name] = clauses
        elif kind == "json-suite":
            name = entry["name"]
            if name in json_suite_citations:
                raise ValueError(f"test_clauses.toml: duplicate json-suite name {name!r}")
            json_suite_citations[name] = clauses
        elif kind == "validator":
            validator_citations |= clauses
        else:
            raise ValueError(f"test_clauses.toml: unknown kind {kind!r} (entry: {entry})")
    partial: dict[str, dict] = {}
    for entry in data.get("partial", []):
        clause = entry["clause"]
        if clause in partial:
            raise ValueError(f"test_clauses.toml: duplicate [[partial]] clause {clause!r}")
        for field in ("class", "supported", "missing", "tests"):
            if not entry.get(field):
                raise ValueError(f"test_clauses.toml: [[partial]] {clause!r} has no {field!r}")
        partial[clause] = entry
    listed = rust_test_citations.keys() | json_suite_citations.keys()
    for name, clauses in list(rust_test_citations.items()) + list(json_suite_citations.items()):
        both = clauses & partial.keys()
        if both:
            raise ValueError(f"test_clauses.toml: {name!r} is listed against {sorted(both)}, "
                             "which are [[partial]] (a partly evidenced clause has no [[test]] line)")
    both = validator_citations & partial.keys()
    if both:
        raise ValueError(f"test_clauses.toml: the validator bucket lists {sorted(both)}, which are [[partial]]")
    del listed
    return {
        "rust_test_citations": rust_test_citations,
        "json_suite_citations": json_suite_citations,
        "validator_citations": validator_citations,
        "partial": partial,
    }


class Sources:
    """Every test-to-clause citation, loaded once from `test_clauses.toml` and reused for every
    recipe lookup. The hand-maintained mapping file is the SOLE source (module docstring); no
    kani worktree is read here at all."""

    def __init__(self, test_clauses_path: Path = TEST_CLAUSES_PATH):
        loaded = load_test_clauses(test_clauses_path)
        self.rust_test_citations: dict[str, set[str]] = loaded["rust_test_citations"]
        self.json_suite_citations: dict[str, set[str]] = loaded["json_suite_citations"]
        self.validator_citations: set[str] = loaded["validator_citations"]
        self.partial: dict[str, dict] = loaded["partial"]
        self.all_rust_test_clauses: set[str] = set().union(
            *self.rust_test_citations.values()) if self.rust_test_citations else set()
        self.all_json_suite_clauses: set[str] = set().union(
            *self.json_suite_citations.values()) if self.json_suite_citations else set()

    def recipe_covered_clauses(self, command: str) -> set[str]:
        """The union of clause ids named by every test this recipe's `command` string executes."""
        covered: set[str] = set()

        m = RUST_TEST_CMD_RE.search(command)
        if m:
            path = m.group(2)
            if path:
                bare_name = path.split("::")[-1]
                covered |= self.rust_test_citations.get(bare_name, set())
            else:
                covered |= self.all_rust_test_clauses

        m = FORCE_RERUN_RE.search(command)
        if m:
            covered |= self.json_suite_citations.get(m.group(1), set())
        elif "--suite json-handler" in command and "--mode exec" in command:
            covered |= self.all_json_suite_clauses

        if FIXTURE_CHECKER_RE.search(command):
            covered |= self.validator_citations

        m = BASH_SUITE_RE.search(command)
        if m:
            suite = m.group(1)
            covered |= self.json_suite_citations.get(suite, set())
            if suite in VALIDATOR_EXERCISER_SUITES:
                covered |= self.validator_citations

        return covered


def cross_check_against_source(kani_root: Path, test_clauses_path: Path = TEST_CLAUSES_PATH) -> list[str]:
    """Optional regression check (`gen_ledger.py --cross-check`): re-scans the kani worktree's
    own source for any RFC0015-S-... id (the retired live-scan mechanism) and reports every hit
    -- this should always be empty, since ids were removed from source comments entirely (module
    docstring). A nonzero result means either a comment reintroduced an
    id (fix the source) or `test_clauses.toml` is silently stale against a real citation (fix the
    mapping) -- this check only detects the former; it does not attempt to reconcile the two.
    """
    driver_files = sorted((kani_root / "kani-driver" / "src").rglob("*.rs"))
    compiler_files = sorted((kani_root / "kani-compiler" / "src").rglob("*.rs"))
    findings: list[str] = []
    rust_hits = scan_rust_test_citations(driver_files + compiler_files)
    for name, clauses in sorted(rust_hits.items()):
        findings.append(f"rust-test {name!r} still cites {sorted(clauses)} in source comments")
    suite_hits = scan_json_suite_citations(kani_root / "tests" / "json-handler")
    for name, clauses in sorted(suite_hits.items()):
        findings.append(f"json-suite {name!r} still cites {sorted(clauses)} in source comments")
    validator_hits = scan_validator_citations(kani_root / "scripts" / "validate_json_export.py")
    if validator_hits:
        findings.append(f"validator still cites {sorted(validator_hits)} in source comments")
    return findings
