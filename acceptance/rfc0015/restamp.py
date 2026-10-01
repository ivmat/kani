#!/usr/bin/env python3
"""restamp.py -- regenerate the mechanical header of acceptance.toml; never touches the claims.

The claims below the END marker are the working document: the producer fills them by hand as PR 1
is implemented (status/band/weight/evidence). Everything above the marker is derived and must never
be hand-edited: the contract hash (M11 over the contract bytes), the subject read-locator (the kani
branch HEAD), the claim count, and the pinned closure commit.

Usage (from this directory):  python3 -B restamp.py [--kani-worktree PATH]

`--kani-worktree` (or the environment variable `RFC0015_KANI_WORKTREE`) names the local kani
worktree/clone this package certifies. It is machine-local and has no built-in default (README.md
"Portable paths") -- an unset or missing path is a hard, explicit failure, never a silent guess.
"""
import argparse, os, subprocess, sys, tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE / "vendor/acceptance-format/protocol_acceptance/tools"))
import m11  # noqa: E402  (vendored, closure-verified)

MARK = "# ===== END OF GENERATED HEADER (restamp.py) -- claims below are the working document ====="

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--kani-worktree", default=None,
                     help="local kani worktree/clone (or env RFC0015_KANI_WORKTREE) -- required, no default")
    ap.add_argument("--mode", choices=["checkpoint", "prospective"], default="checkpoint",
                    help="checkpoint: retrospective at the kani HEAD (requires a clean kani worktree) -- the package "
                         "certifies exactly what its evidence shows at that commit; prospective: no certified identity")
    a = ap.parse_args()
    kani_worktree = a.kani_worktree or os.environ.get("RFC0015_KANI_WORKTREE")
    if not kani_worktree:
        sys.exit("restamp: --kani-worktree PATH or RFC0015_KANI_WORKTREE is required (no built-in default)")
    if not (Path(kani_worktree).expanduser() / ".git").exists():
        sys.exit(f"restamp: kani_worktree {kani_worktree!r} has no .git -- not a git worktree/clone")
    a.kani_worktree = str(Path(kani_worktree).expanduser().resolve())
    contract_path = HERE / "acceptance-contract.toml"
    contract = tomllib.loads(contract_path.read_text())
    tool_dir = HERE / "vendor/acceptance-format"
    if (tool_dir / "CLOSURE.json").exists():  # a vendored closure
        closure = __import__("json").loads((tool_dir / "CLOSURE.json").read_text())
    else:  # a plain git clone of the public acceptance-format repository
        closure = {"source_commit": subprocess.check_output(
            ["git", "-C", str(tool_dir), "rev-parse", "HEAD"], text=True).strip()}
    head = subprocess.check_output(["git", "-C", a.kani_worktree, "rev-parse", "HEAD"], text=True).strip()
    if a.mode == "checkpoint":
        dirty = subprocess.check_output(["git", "-C", a.kani_worktree, "status", "--porcelain"], text=True).strip()
        if dirty:
            sys.exit("restamp: checkpoint mode needs a clean kani worktree (commit first); use --mode prospective")
        identity = f'mode           = "retrospective"\ncommit         = "{head}"\ndirty          = false'
    else:
        identity = f'mode           = "prospective"\nread_at_commit = "{head}"'
    pkg = HERE / "acceptance.toml"
    body = pkg.read_text().split(MARK, 1)[1]
    n = sum(1 for line in body.splitlines() if line.strip() == "[[claim]]")
    doc = contract["document"]
    header = f'''# acceptance.toml -- the PRODUCER's package (acceptance-protocol/0 §4) for Kani RFC 0015, delivery 1 (PR 1),
# bound to contract {doc["id"]} v{doc["version"]} ({doc["status"]}). A WORKING DOCUMENT: it starts
# prospective with every claim at gap/A0 and is filled row by row while PR 1 is implemented; at the
# delivered revision it is re-stamped retrospective and handed to the consumer for `decide`.
#
# Claim ids RFC15-xx keep the 2026-09-20 seed's numbering (protocol
# format_acceptance/examples/export-json-rfc0015/pr1-migrate/), re-keyed under the contract's
# requirements (clause = R0..R11); the RFC section each came from is the comment above the claim.
# The PR 2 seed's claims sit under R10/R11 as visible gaps, declared by [[deviation]] at the end.
# A gap row is graded not-covered (spec §7.1); a row moves to test-only (driver tests, not Kani
# proofs) when its evidence lands -- the seed's contract/probe labels would overclaim here.
#
# VALIDATE (from this directory):
#   python3 -B vendor/acceptance-format/format_acceptance/tools/check_acceptance.py --strict --strict-weight acceptance.toml
#   python3 -B vendor/acceptance-format/protocol_acceptance/tools/acceptance_protocol.py check-package acceptance.toml --contract acceptance-contract.toml

[format]
id       = "acceptance/0"
protocol = "acceptance-protocol/0"
profile  = "acceptance/verification/code/rust"
generated_by = "restamp.py (header only); claims hand-filled"
closure_commit = "{closure.get("source_commit", "unknown")}"

[subject]
name           = "model-checking/kani — --export-json writer (RFC 0015), delivery PR 1"
kind           = "rust-workspace"
{identity}
repo           = "github.com/model-checking/kani (branch export-json-rfc0015, not yet pushed)"

[contract]
id                 = "{doc["id"]}"
hash               = "{m11.digest_file("contract", contract_path)}"
requirements_total = {len(contract["requirement"])}

[spec]
path    = "acceptance-contract.toml"
version = "{doc["id"]}@v{doc["version"]}"
axis    = "the requirements of contract {doc["id"]} (RFC 0015 text pinned in the contract, copy at spec/0015-export-json.txt)"

[coverage]
clauses_total = {len(contract["requirement"])}
claims_total  = {n}

'''
    pkg.write_text(header + MARK + body)
    print(f"restamped ({a.mode}): contract {doc['id']} v{doc['version']} ({doc['status']}), kani {head[:9]}, {n} claims")

if __name__ == "__main__":
    main()
