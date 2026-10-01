#!/usr/bin/env python3
"""gen_evidence.py -- the evidence GENERATOR for acceptance.toml (registry: evidence/recipes.toml;
style modeled on restamp.py). Every relied-on evidence record this package cites is (re)produced by
actually RUNNING its recipe against the kani worktree, all at ONE kani commit, so acceptance/0.3.2's
freshness rule (protocol_acceptance acceptance-protocol/0 sec 6.2 cond 7: every relied-on record's
captured_at_commit equals the package's own subject identity, in full) holds for every claim graded
`evidenced`. A record under evidence/gen/ is never hand-written -- see README.md "Evidence
generation".

Usage (from this directory):
  python3 -B gen_evidence.py [--only ID ...] [--dry-run]
    [--kani-worktree PATH] [--cbmc-dir PATH]

Both paths are machine-local and are never hard-coded: `--kani-worktree` (or the environment
variable `RFC0015_KANI_WORKTREE`) names the local clone/worktree of the kani branch this package
certifies; `--cbmc-dir` (or `RFC0015_CBMC_DIR`) names the local CBMC install directory (the one
whose `usr/bin` this run's PATH needs). Neither has a default -- an unset or missing path is a
hard, explicit failure, never a silent guess.

Refuses to run unless the kani worktree is git-clean. `kani=true` recipes run ONE AT A TIME through
a memory-capped, swap-off, detached `systemd --user` service -- never in parallel and never
without the memory cap. A control's mutation is applied via `git apply`, run, and unconditionally reverted via
`git checkout --` in a `finally`; if the worktree is not byte-clean afterward this script aborts
hard (exit 97) rather than continue with a dirty subject.

A run/control MAY declare `root = "package"` (default: "kani") when its real subject is this
ACCEPTANCE PACKAGE's own tooling, not the kani branch -- R0's evidence (the generated conformance
ledger, ledger/acceptance.toml) is the first such case. Such a recipe's command runs with this
script's own directory as cwd (never a hard-coded path -- `HERE`, resolved from `__file__`, same
portability discipline as `--kani-worktree`/`--cbmc-dir`); its control's git apply/checkout and
post-revert cleanliness check run against the this package's own repository too, but the cleanliness check is
SCOPED to the patch's own target files -- that repository legitimately carries other, unrelated
in-flight work the whole time this script runs, so a repo-wide check would fail for reasons having
nothing to do with this recipe.

Every transcript this script persists is NORMALIZED first, as a disclosed transform (see
`normalize_transcript` below and README.md "Transcript normalization"): the kani worktree path
becomes `<kani>`, this package's own directory becomes `<pkg>`, the CBMC dir becomes `<cbmc>`, a
remaining `$HOME` prefix becomes `~`, any other `/home/<user>/` becomes `<home>/`, and a systemd
invocation id becomes `<invocation-id>`. The record hash is computed over exactly these normalized
bytes, never over the raw machine-local transcript.
"""
import argparse
import datetime
import os
import re
import shlex
import subprocess
import sys
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE / "vendor/acceptance-format/format_acceptance/tools"))
import hashdomains  # noqa: E402  (vendored, closure-verified)

RECIPES_PATH = HERE / "evidence" / "recipes.toml"
GEN_DIR = HERE / "evidence" / "gen"
ACCEPTANCE_PATH = HERE / "acceptance.toml"
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
UNIT_PREFIX = "kani-gen"
INVOCATION_ID_RE = re.compile(r"(invocation ID: )[0-9a-f]{16,}")
MKTEMP_DIR_RE = re.compile(r"/tmp/(?:tmp\.[A-Za-z0-9]+|[A-Za-z0-9_.-]*\.tmp[A-Za-z0-9]+)")
HOME_USER_RE = re.compile(r"/home/[A-Za-z0-9_-]+/")


def resolve_path_arg(cli_value: str | None, env_name: str, label: str) -> Path:
    """A required local path: `--<flag>` first, then the named env var, else a hard failure.
    Never a hard-coded fallback (README.md "Portable paths")."""
    raw = cli_value or os.environ.get(env_name)
    if not raw:
        sys.exit(
            f"FATAL: {label} is not set -- pass --{label.replace('_', '-')} PATH or export "
            f"{env_name}=PATH (no built-in default)"
        )
    path = Path(raw).expanduser()
    if not path.is_dir():
        sys.exit(f"FATAL: {label} {str(path)!r} is not a directory (from --{label.replace('_', '-')} or {env_name})")
    return path.resolve()


def normalize_transcript(text: str, kani_worktree: str, pkg_dir: str, cbmc_dir: str) -> str:
    """Disclosed transform applied to every transcript BEFORE it is persisted and hashed (README.md
    "Transcript normalization"). Order matters: the most specific, deepest local roots are replaced
    first so a kani-worktree or package path never collapses merely to a bare home path.
    - the kani worktree absolute path    -> <kani>
    - this package's own directory       -> <pkg>
    - the local CBMC install directory   -> <cbmc>
    - a remaining $HOME prefix           -> ~
    - any other /home/<user>/ prefix     -> <home>/
    - a systemd invocation id            -> <invocation-id>
    - an mktemp-style /tmp directory     -> <tmp>
    """
    text = text.replace(kani_worktree, "<kani>")
    text = text.replace(pkg_dir, "<pkg>")
    text = text.replace(cbmc_dir, "<cbmc>")
    home = str(Path.home())
    text = re.sub(re.escape(home) + r"(?=/|$)", "~", text)
    text = HOME_USER_RE.sub("<home>/", text)
    text = INVOCATION_ID_RE.sub(r"\1<invocation-id>", text)
    text = MKTEMP_DIR_RE.sub("<tmp>", text)
    return text


def now_utc() -> str:
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def git(args: list[str], cwd: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["git", "-C", cwd, *args], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True
    )


def worktree_head(cwd: str) -> str:
    head = git(["rev-parse", "HEAD"], cwd).stdout.strip()
    if not COMMIT_RE.match(head):
        sys.exit(f"FATAL: could not read a full 40-hex HEAD from {cwd!r} (got {head!r})")
    return head


def worktree_clean(cwd: str, paths: list[str] | None = None) -> bool:
    """Empty `git status --porcelain`, optionally SCOPED to `paths` (repo-ROOT-relative, e.g. a
    patch's own `+++ b/<path>` targets -- see the `:/` pathspec note in `do_control`). Scoping
    matters for `root = "package"` recipes: the check must confirm only that THIS recipe's own
    target files are clean, not that the whole repository is -- that repo legitimately
    carries other, unrelated in-flight work the whole time this script runs."""
    args = ["status", "--porcelain"]
    if paths:
        args += ["--", *(f":/{p}" for p in paths)]
    return git(args, cwd).stdout.strip() == ""


def run_shell(cmd: str, cwd: str) -> tuple[int, str]:
    proc = subprocess.run(
        ["bash", "-lc", cmd], cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True
    )
    return proc.returncode, proc.stdout


def plain_env_cmd(cmd: str, cfg: dict) -> str:
    # NOTE: path_prefix is embedded inside double quotes ON PURPOSE, not shlex.quote()'d -- it
    # ends in `:$PATH`, a shell variable reference meant to expand against the CURRENT PATH, not
    # a literal string (single-quoting or shlex.quote would defeat that expansion).
    return (
        f'export PATH="{cfg["path_prefix"]}"; '
        f'export RUST_TEST_THREADS={shlex.quote(cfg["rust_test_threads"])}; '
        f"{cmd}"
    )


def kani_cmd(cmd: str, cwd: str, unit: str, cfg: dict) -> tuple[int, str]:
    """kani/cbmc run ONLY through a
    memory-capped, swap-off, detached systemd --user service, one at a time (this script never
    launches two of these concurrently -- it is a plain, single-threaded, sequential loop)."""
    inner = f"cd {shlex.quote(cwd)} && {plain_env_cmd(cmd, cfg)}"
    full = [
        "systemd-run", "--user", "--wait", "--pipe", "--collect",
        "-p", f'MemoryMax={cfg["systemd_memory_max"]}',
        "-p", f'MemorySwapMax={cfg["systemd_memory_swap_max"]}',
        f"--unit={unit}",
        "bash", "-lc", inner,
    ]
    proc = subprocess.run(full, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    return proc.returncode, proc.stdout


def plain_cmd(cmd: str, cwd: str, cfg: dict) -> tuple[int, str]:
    return run_shell(plain_env_cmd(cmd, cfg), cwd)


def record_hash(path: Path) -> str:
    return hashdomains.digest("record:", path.read_bytes())


NORMALIZATION_NOTE = (
    "normalized (disclosed transform, see README.md \"Transcript normalization\"): kani worktree "
    "-> <kani>, this package's directory -> <pkg>, CBMC dir -> <cbmc>, $HOME -> ~, other "
    "/home/<user>/ -> <home>/, systemd invocation id -> <invocation-id>, mktemp /tmp dir -> <tmp>"
)


def write_header(head: str, command: str, start: str, end: str, code: int, note: str) -> str:
    lines = [
        f"kani commit: {head}",
        f"command: {command}",
        f"start (UTC): {start}",
        f"end (UTC): {end}",
        f"exit: {code}",
        NORMALIZATION_NOTE,
        note,
    ]
    return "\n".join(lines) + "\n" + ("-" * 78) + "\n\n"


class Result:
    __slots__ = ("ok", "record_path", "note", "coverage")

    def __init__(self, ok: bool, record_path: Path | None, note: str, coverage: dict | None = None):
        self.ok = ok
        self.record_path = record_path
        self.note = note
        # {"metric": ..., "of": ..., "value": ...} when the run declares `coverage` +
        # `coverage_value_regex` (README "Evidence generation" / a structural/proof-coverage
        # metric, format B21) -- None for every other run/control.
        self.coverage = coverage


def extract_coverage(run: dict, out: str) -> tuple[dict | None, str | None]:
    """A run MAY declare `coverage = {metric, of}` plus a `coverage_value_regex` with one capture
    group (a fraction in [0, 1]) matched against its own combined stdout+stderr. Returns
    (coverage_dict_or_None, error_note_or_None) -- never guesses a value: no match or an
    out-of-range number is a hard failure for this run, not a silent skip."""
    spec = run.get("coverage")
    if spec is None:
        return None, None
    value_re = run.get("coverage_value_regex")
    if not value_re:
        sys.exit(f"FATAL: run {run['id']!r} declares coverage but no coverage_value_regex")
    m = re.search(value_re, out)
    if not m:
        return None, "coverage_value_regex did NOT match the run's output"
    try:
        value = float(m.group(1))
    except (IndexError, ValueError):
        sys.exit(f"FATAL: run {run['id']!r}: coverage_value_regex has no usable capture group")
    if not (0.0 <= value <= 1.0):
        sys.exit(f"FATAL: run {run['id']!r}: computed coverage.value {value!r} is not a fraction in [0, 1]")
    return {"metric": spec["metric"], "of": spec["of"], "value": value}, None


def recipe_cwd(recipe: dict, worktree: str) -> str:
    """`root = "package"` (opt-in, default "kani"): this recipe's real subject is the ACCEPTANCE
    PACKAGE itself (this repository, not the kani branch this package
    certifies -- R0 (the clause ledger) is the first such case: its evidence is about this
    package's OWN generated conformance manifest, which does not live in the kani worktree at
    all. `HERE` (this script's own directory, resolved from `__file__`) is already how every
    other path in this script stays portable/non-hard-coded -- reusing it here, rather than a
    literal path, is the same discipline, not a new absolute-path leak (README.md "Portable
    paths")."""
    return str(HERE) if recipe.get("root") == "package" else worktree


def do_run(run: dict, cfg: dict, worktree: str, norm: tuple[str, str, str]) -> Result:
    cwd = recipe_cwd(run, worktree)
    start = now_utc()
    unit = f"{UNIT_PREFIX}-run-{run['id']}"
    if run["kani"]:
        code, out = kani_cmd(run["command"], cwd, unit, cfg)
    else:
        code, out = plain_cmd(run["command"], cwd, cfg)
    end = now_utc()
    expect_ok = bool(re.search(run["expect"], out))
    exit_ok = code == run["exit"]
    coverage, coverage_err = extract_coverage(run, out)
    ok = expect_ok and exit_ok and coverage_err is None
    note = f"expect matched: {'yes' if expect_ok else 'no'}; exit as expected: {'yes' if exit_ok else 'no'}"
    if coverage is not None:
        note += f"; coverage.value = {coverage['value']:.6f}"
    elif coverage_err is not None:
        note += f"; {coverage_err}"
    path = GEN_DIR / f"run-{run['id']}.txt"
    head = worktree_head(cwd)
    head_label = head if run.get("root") != "package" else f"{head} (this repo's own HEAD, root=package)"
    body = write_header(head_label, run["command"], start, end, code, note) + out
    path.write_text(normalize_transcript(body, *norm))
    return Result(ok, path, note, coverage)


def patch_target_files(patch_text: str) -> list[str]:
    return re.findall(r"^\+\+\+ b/(.+)$", patch_text, re.MULTILINE)


def do_control(ctrl: dict, cfg: dict, worktree: str, norm: tuple[str, str, str]) -> Result:
    cwd = recipe_cwd(ctrl, worktree)
    is_package_root = ctrl.get("root") == "package"
    patch_path = HERE / ctrl["patch"]
    patch_text = patch_path.read_text()
    files = patch_target_files(patch_text)
    if not files:
        sys.exit(f"FATAL: control {ctrl['id']!r}: patch {patch_path} names no target files")
    # A package-root patch's own paths (see evidence/patches/*.patch, git-diff'd from the
    # the package repo ROOT) are relative to that repo root, not to `cwd` (this package's
    # subdirectory) -- `git -C cwd apply` still resolves them correctly because git discovers
    # the repo root from `cwd`'s ancestry and applies paths relative to THAT, exactly as it
    # would from any other subdirectory.
    clean_scope = files if is_package_root else None
    apply_args = ["apply"]
    if is_package_root:
        # The patch may have been written when this package sat at a different path inside its
        # repository (a patch's paths always start at the repository root). Locate each target
        # inside THIS package, strip the leading components that do not belong to it, and re-root
        # the rest at the package's present location, so the control works wherever the package
        # lives. A patch already written for the present layout maps to itself.
        top = Path(git(["rev-parse", "--show-toplevel"], cwd).stdout.strip()).resolve()
        pkg_rel = HERE.relative_to(top).as_posix()
        strip, clean_scope = None, []
        for f in files:
            parts = f.split("/")
            for i in range(len(parts)):
                if (HERE / "/".join(parts[i:])).exists():
                    strip = i + 1 if strip is None else min(strip, i + 1)
                    clean_scope.append("/".join(filter(None, [pkg_rel if pkg_rel != "." else "", *parts[i:]])))
                    break
            else:
                sys.exit(f"FATAL: control {ctrl['id']!r}: patch target {f!r} is not inside this package")
        files = clean_scope
        apply_args += [f"-p{strip}"]
        if pkg_rel != ".":
            apply_args += [f"--directory={pkg_rel}"]

    apply_proc = git([*apply_args, str(patch_path)], cwd)
    if apply_proc.returncode != 0:
        note = "git apply FAILED -- patch no longer applies cleanly at HEAD"
        path = GEN_DIR / f"control-{ctrl['id']}.txt"
        body = (
            write_header(worktree_head(cwd), ctrl["command"], now_utc(), now_utc(), apply_proc.returncode, note)
            + apply_proc.stdout
            + "\n\n--- patch (did not apply) ---\n"
            + patch_text
        )
        path.write_text(normalize_transcript(body, *norm))
        return Result(False, path, note)

    start = now_utc()
    try:
        unit = f"{UNIT_PREFIX}-ctrl-{ctrl['id']}"
        if ctrl["kani"]:
            code, out = kani_cmd(ctrl["command"], cwd, unit, cfg)
        else:
            code, out = plain_cmd(ctrl["command"], cwd, cfg)
        end = now_utc()
        red = bool(re.search(ctrl["expect_red"], out))
    finally:
        # `:/<path>` is git's own "repo top-level, not cwd" pathspec magic -- needed because a
        # patch's `+++ b/<path>` is always REPO-ROOT-relative (that's what `git diff`/`git apply`
        # write and read), but `cwd` here may be a subdirectory (a package-root recipe's cwd is
        # this script's own directory, not the containing repo's root) -- a plain relative
        # pathspec would resolve against `cwd`, not the root, and silently miss the file.
        co = git(["checkout", "--", *(f":/{f}" for f in files)], cwd)
        if co.returncode != 0 or not worktree_clean(cwd, clean_scope):
            print(
                f"FATAL: control {ctrl['id']!r} did not revert cleanly "
                f"(git checkout exit {co.returncode}; clean={worktree_clean(cwd, clean_scope)}) "
                "-- refusing to continue with a dirty subject worktree",
                file=sys.stderr,
            )
            sys.exit(97)

    note = f"observed red: {'yes' if red else 'no'}"
    path = GEN_DIR / f"control-{ctrl['id']}.txt"
    head_label = worktree_head(cwd) if not is_package_root else f"{worktree_head(cwd)} (this repo's own HEAD, root=package)"
    body = (
        write_header(head_label, ctrl["command"], start, end, code, note)
        + out
        + "\n\n--- patch ---\n"
        + patch_text
    )
    path.write_text(normalize_transcript(body, *norm))
    return Result(red, path, note)


def split_claims(raw: str) -> tuple[str, list[str]]:
    """Split acceptance.toml's raw text at every top-level `[[claim]]` line. Returns
    (preamble, [claim_chunk, ...]) where each claim_chunk starts with its own `[[claim]]` line and
    runs up to (not including) the next one (the last chunk also swallows the trailing
    [[deviation]] sections, which is harmless -- nothing here searches past its own claim's id)."""
    parts = re.split(r"(?=^\[\[claim\]\]$)", raw, flags=re.MULTILINE)
    return parts[0], parts[1:]


def claim_id_of(chunk: str) -> str | None:
    m = re.search(r'^id\s*=\s*"([^"]+)"', chunk, re.MULTILINE)
    return m.group(1) if m else None


EVIDENCE_SPLIT_RE = re.compile(r"(?=^  \[\[claim\.evidence\]\]$)", re.MULTILINE)


def split_evidence_blocks(chunk: str) -> tuple[str, list[str]]:
    """Split one claim's chunk into (preamble, [evidence_block, ...]): preamble is everything
    before the claim's first `  [[claim.evidence]]` line (id/status/bounds/[claim.self_verify]/
    etc, untouched here); each evidence_block starts with its own `  [[claim.evidence]]` line and
    runs up to (not including) the next one (so it also swallows its own nested
    `[claim.evidence.control]` / `[claim.evidence.coverage]` sub-tables, which is the point --
    each evidence entry is now rewritten in total isolation, never by a claim-wide leftmost
    search)."""
    parts = EVIDENCE_SPLIT_RE.split(chunk)
    return parts[0], parts[1:]


def build_recipe_index(cfg_doc: dict) -> dict[str, str]:
    """recipe id -> kind ("run"/"control"), FATAL on any id collision (within or across the two
    tables) -- a recipe id must resolve to exactly one recipe, never zero, never several."""
    index: dict[str, str] = {}
    for kind in ("run", "control"):
        for r in cfg_doc.get(kind, []):
            rid = r["id"]
            if rid in index:
                sys.exit(f"FATAL: recipe id {rid!r} is defined more than once in recipes.toml "
                          f"(as both {index[rid]!r} and {kind!r}, or twice as {kind!r})")
            index[rid] = kind
    return index


def update_acceptance_toml(recipe_index: dict[str, str], results: dict[str, Result], worktree: str) -> list[str]:
    """Rewrite acceptance.toml's evidence pointers (record/record_hash/captured_at_commit) for
    every [[claim.evidence]] entry, bound to its recipe by that entry's OWN `recipe = "<id>"`
    field (never by a claim-wide leftmost text search -- see README.md "Evidence generation").
    Textual, minimal, deterministic (restamp.py's own approach -- no TOML writer in stdlib): the
    claim/status/band/weight/statement/bounds text is never touched. Returns a list of
    human-readable notes.

    Every `[[claim.evidence]]` entry in the whole file MUST carry a `recipe` field naming a known
    run/control id -- this is checked for EVERY claim chunk, not only ones this run happened to
    touch, so a missing/typo'd binding is caught even on a `--only` partial run. A binding whose
    record/record_hash/captured_at_commit block does not appear EXACTLY once inside its own
    (already-isolated) evidence entry is a hard failure, never a silent guess."""
    head = worktree_head(worktree)
    raw = ACCEPTANCE_PATH.read_text()
    preamble, chunks = split_claims(raw)

    notes: list[str] = []
    for idx, chunk in enumerate(chunks):
        claim = claim_id_of(chunk) or f"<unidentified claim at chunk {idx}>"
        ev_preamble, blocks = split_evidence_blocks(chunk)
        if not blocks:
            continue  # no [[claim.evidence]] entries on this claim (gap/A0 claims etc.)

        all_ok = True
        for i, block in enumerate(blocks):
            rm = re.search(r'^\s*recipe\s*=\s*"([^"]+)"\s*$', block, re.MULTILINE)
            if rm is None:
                sys.exit(
                    f"FATAL: {claim}: [[claim.evidence]] entry #{i} carries no `recipe` field -- "
                    "every evidence entry must be explicitly bound to the recipe that produced "
                    "it (README.md \"Evidence generation\"); do not guess, add the binding"
                )
            recipe_id = rm.group(1)
            if recipe_id not in recipe_index:
                sys.exit(
                    f"FATAL: {claim}: [[claim.evidence]] entry #{i} is bound to recipe "
                    f"{recipe_id!r}, which does not exist in recipes.toml"
                )

            res = results.get(recipe_id)
            if res is None or not res.ok:
                all_ok = False
                notes.append(
                    f"SKIP {claim}[{i}] <- recipe {recipe_id!r}: did not generate cleanly this "
                    "run (left pointing at the old record)"
                )
                continue

            new_record = str(res.record_path.relative_to(HERE))
            new_hash = record_hash(res.record_path)
            pattern = re.compile(
                r'(record\s*=\s*)"[^"]*"\n'
                r'(\s*record_hash\s*=\s*)"[^"]*"\n'
                r'(\s*captured_at_commit\s*=\s*)"[0-9a-f]{40}"'
            )
            new_block, n = pattern.subn(
                lambda mo: f'{mo.group(1)}"{new_record}"\n{mo.group(2)}"{new_hash}"\n{mo.group(3)}"{head}"',
                block,
            )
            if n != 1:
                sys.exit(
                    f"FATAL: {claim}[{i}] (recipe {recipe_id!r}): the record/record_hash/"
                    f"captured_at_commit block matched {n} times inside its own evidence entry "
                    "(expected exactly 1) -- fix the entry, do not guess"
                )
            block = new_block
            notes.append(f"OK   {claim}[{i}] <- recipe {recipe_id!r} -> {new_record}")

            if res.coverage is not None:
                # The recipe also measured a structural/proof-coverage metric (format B21):
                # rewrite ONLY the `value` of the `[claim.evidence.coverage]` table that sits
                # under THIS SAME (already-isolated) evidence entry -- never guessed, never
                # hand-edited.
                metric = res.coverage["metric"]
                value = res.coverage["value"]
                cov_pattern = re.compile(
                    r'(\[claim\.evidence\.coverage\]\s*\n'
                    r'\s*metric\s*=\s*"' + re.escape(metric) + r'"\s*\n'
                    r'\s*value\s*=\s*)[0-9]+(?:\.[0-9]+)?'
                )
                new_block2, ncov = cov_pattern.subn(
                    lambda mo, v=value: f"{mo.group(1)}{v:.6f}", block
                )
                if ncov != 1:
                    sys.exit(
                        f"FATAL: {claim}[{i}] (recipe {recipe_id!r}): declares coverage (metric="
                        f"{metric!r}) but its [claim.evidence.coverage] table matched {ncov} "
                        "times (expected exactly 1) -- add/fix the skeleton table, do not guess"
                    )
                block = new_block2
                notes.append(f"OK   {claim}[{i}]: coverage.value -> {value:.6f} (recipe {recipe_id!r})")

            blocks[i] = block

        if all_ok:
            # The claim-level `captured_at_commit` (unindented, distinct from the 2-space-indented
            # evidence-level field of the same name) only moves once every one of this claim's
            # evidence entries is fresh.
            new_ev_preamble, n = re.subn(
                r'^captured_at_commit(\s*=\s*)"[0-9a-f]{40}"',
                f'captured_at_commit\\1"{head}"',
                ev_preamble,
                count=1,
                flags=re.MULTILINE,
            )
            if n == 1:
                ev_preamble = new_ev_preamble
                notes.append(f"OK   {claim}: claim-level captured_at_commit -> {head[:9]}")

        chunks[idx] = ev_preamble + "".join(blocks)

    ACCEPTANCE_PATH.write_text(preamble + "".join(chunks))
    return notes


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--only", nargs="*", default=None, help="only these run/control ids")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--kani-worktree", default=None,
                     help="local kani worktree/clone (or env RFC0015_KANI_WORKTREE) -- required, no default")
    ap.add_argument("--cbmc-dir", default=None,
                     help="local CBMC install dir (or env RFC0015_CBMC_DIR) -- required, no default")
    args = ap.parse_args()

    kani_worktree_path = resolve_path_arg(args.kani_worktree, "RFC0015_KANI_WORKTREE", "kani_worktree")
    cbmc_dir_path = resolve_path_arg(args.cbmc_dir, "RFC0015_CBMC_DIR", "cbmc_dir")
    worktree = str(kani_worktree_path)
    if not (kani_worktree_path / ".git").exists():
        sys.exit(f"FATAL: kani_worktree {worktree!r} has no .git -- not a git worktree/clone")
    norm = (worktree, str(HERE), str(cbmc_dir_path))

    cfg_doc = tomllib.loads(RECIPES_PATH.read_text())
    cfg = dict(cfg_doc["defaults"])
    # recipes.toml's own text carries no absolute user paths (README.md "Portable paths"): its
    # path_prefix template names <cbmc> as a placeholder, substituted here with the resolved
    # --cbmc-dir/RFC0015_CBMC_DIR; the rest of the template ($HOME, $PWD, $PATH) is left for the
    # shell that actually runs each recipe to expand.
    cfg["path_prefix"] = cfg["path_prefix"].replace("<cbmc>", str(cbmc_dir_path))

    if not worktree_clean(worktree):
        sys.exit(f"FATAL: kani worktree {worktree!r} is not git-clean -- refusing to run")

    runs = cfg_doc.get("run", [])
    controls = cfg_doc.get("control", [])
    recipe_index = build_recipe_index(cfg_doc)

    selected = set(args.only) if args.only else None

    GEN_DIR.mkdir(parents=True, exist_ok=True)

    if args.dry_run:
        print(f"DRY RUN -- worktree {worktree} @ {worktree_head(worktree)}")
        for r in runs:
            if selected is None or r["id"] in selected:
                print(f"  would run:    {r['id']:55s} kani={r['kani']!s:5} {r['command']}")
        for c in controls:
            if selected is None or c["id"] in selected:
                print(f"  would control:{c['id']:55s} kani={c['kani']!s:5} {c['command']}")
        return 0

    results: dict[str, Result] = {}
    any_bad = False

    for r in runs:
        if selected is not None and r["id"] not in selected:
            continue
        print(f"[run]     {r['id']} ...", flush=True)
        res = do_run(r, cfg, worktree, norm)
        results[r["id"]] = res
        any_bad = any_bad or not res.ok
        print(f"          -> {res.note}", flush=True)

    for c in controls:
        if selected is not None and c["id"] not in selected:
            continue
        print(f"[control] {c['id']} ...", flush=True)
        res = do_control(c, cfg, worktree, norm)
        results[c["id"]] = res
        any_bad = any_bad or not res.ok
        print(f"          -> {res.note}", flush=True)

    notes = update_acceptance_toml(recipe_index, results, worktree)
    print("\n".join(notes))

    print("\n--- restamp ---")
    subprocess.run([sys.executable, "-B", str(HERE / "restamp.py"), "--kani-worktree", worktree], cwd=HERE)

    print("\n--- check_acceptance --strict --strict-weight ---")
    subprocess.run(
        [sys.executable, "-B",
         str(HERE / "vendor/acceptance-format/format_acceptance/tools/check_acceptance.py"),
         "--strict", "--strict-weight", "acceptance.toml"],
        cwd=HERE,
    )

    print("\n--- coverage ---")
    subprocess.run(
        [sys.executable, "-B",
         str(HERE / "vendor/acceptance-format/protocol_acceptance/tools/acceptance_protocol.py"),
         "coverage", "acceptance.toml", "--contract", "acceptance-contract.toml"],
        cwd=HERE,
    )

    return 1 if any_bad else 0


if __name__ == "__main__":
    raise SystemExit(main())
