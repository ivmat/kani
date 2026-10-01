#!/usr/bin/env python3
# Copyright Kani Contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT
"""
Checks scripts/validate_json_export.py against the fixtures in `fixture-*.json`.

Every fixture, and every twin (a fixture with a valid edit applied), must validate. Each case
below breaks one rule in a copy of a fixture or a twin; the validator must reject it with a
diagnostic naming the broken rule, and must not crash. The predicates and `id_class` are
checked directly. Each extension adds a field RFC 0015 does not define to a copy; the validator
must accept it.

Usage: check_fixtures.py <path to validate_json_export.py>
"""

import copy
import importlib.util
import json
import subprocess
import sys
from pathlib import Path

sys.dont_write_bytecode = True

FIXTURES = Path(__file__).parent
COMPLETED = "fixture-completed-failure"
SUCCESS = "fixture-completed-success"
NON_COMPLETED = "fixture-non-completed-outcomes"
NO_HARNESSES = "fixture-no-harnesses-selected"
MARKER = "fixture-incomplete-marker"


def parts(path):
    return [int(p) if p.isdigit() else p for p in path.split(".")]


def locate(doc, path):
    *parents, last = parts(path)
    for key in parents:
        doc = doc[key]
    return doc, last


def set_at(path, value):
    def mutate(doc):
        target, key = locate(doc, path)
        target[key] = value
    return mutate


def delete_at(path):
    def mutate(doc):
        target, key = locate(doc, path)
        del target[key]
    return mutate


def all_of(*mutations):
    def mutate(doc):
        for mutation in mutations:
            mutation(doc)
    return mutate


def bump_at(path, delta=1):
    def mutate(doc):
        target, key = locate(doc, path)
        target[key] += delta
    return mutate


def reverse_at(path):
    def mutate(doc):
        target, key = locate(doc, path)
        target[key].reverse()
    return mutate


def append_copy(path, **changes):
    """Append to the list at `path` a copy of its first element with `changes`."""
    def mutate(doc):
        target, key = locate(doc, path)
        target[key].append({**target[key][0], **changes})
    return mutate


def add_property(domain, bucket, property_id, status=None):
    """Account for one more property of harness 0 everywhere a COMPLETED export counts it."""
    def mutate(doc):
        harness = doc["harnesses"][0]
        entry = {"id": property_id, "status": status} if bucket == "other" else property_id
        harness[domain][bucket].append(entry)
        harness[domain]["total"] += 1
        harness["n_properties"] += 1
        doc["summary"][f"{domain}_total"] += 1
        if bucket == "satisfied":
            doc["summary"]["covers_satisfied"] += 1
        if bucket == "failure":
            harness["n_failed"] += 1
            harness["failed_properties"].append({
                "id": property_id, "description": "d", "class": "assertion", "file": None,
                "line": None, "trace_available": False, "status": "FAILURE"})
    return mutate


# Valid variations of a fixture (or of another twin). A case may use one as its base: the case
# then differs from a valid export by exactly its own edit.
TWINS = {
    "expected-panic": (SUCCESS, all_of(
        add_property("checks", "failure", "check_ok.assertion.9"),
        set_at("harnesses.0.attributes.should_panic", True),
        set_at("harnesses.0.failure_kind", "PANICS_ONLY"))),
    # An ignored quantifier makes failure_kind ERROR without any ERROR property.
    "ignored-quantifier": (SUCCESS, all_of(
        set_at("harnesses.0.failure_kind", "ERROR"),
        set_at("harnesses.0.outcome.verdict", "FAILURE"),
        set_at("summary.successful", 0),
        set_at("summary.failed", 1))),
    "errored-check": ("ignored-quantifier", add_property("checks", "error", "check_ok.assertion.8")),
    "bounded-autoharness": (SUCCESS, all_of(
        set_at("harnesses.0.is_automatically_generated", True), set_at("harnesses.0.is_bounded", True))),
    "exact-selection": (COMPLETED, all_of(
        set_at("harness_selection.requested_filters", ["check_two"]),
        set_at("harness_selection.unmatched_filters", []), set_at("harness_selection.exact", True))),
    "smallest-values": (SUCCESS, all_of(
        set_at("wall_time_s", 0), set_at("harnesses.0.line", 1), set_at("harnesses.0.resources.verification_time_s", 0),
        set_at("harnesses.0.attributes.unwind_value", 0), set_at("harnesses.0.resolved_unwind", 0))),
}


def load_base(name):
    if name in TWINS:
        base, mutate = TWINS[name]
        doc = load_base(base)
        mutate(doc)
        return doc
    with open(FIXTURES / f"{name}.json") as f:
        return json.load(f)


def replace_root(value):
    return lambda doc: value


H0 = "harnesses.0"
# (case name, fixture or twin, mutation, text the diagnostic must contain)
CASES = [
    # Scalar leaf types.
    ("integer warning message", COMPLETED, set_at(f"{H0}.warnings.0.message", 7),
     "harnesses[0].warnings[0].message"),
    ("integer property id", COMPLETED, set_at(f"{H0}.failed_properties.0.id", 7),
     "harnesses[0].failed_properties[0].id"),
    ("integer line in a property", COMPLETED, set_at(f"{H0}.failed_properties.0.line", 10),
     "harnesses[0].failed_properties[0].line"),
    ("string trace_available", COMPLETED, set_at(f"{H0}.failed_properties.0.trace_available", "yes"),
     "harnesses[0].failed_properties[0].trace_available"),
    ("boolean harness line", COMPLETED, set_at(f"{H0}.line", True), "harnesses[0].line"),
    ("string wall_time_s", COMPLETED, set_at("wall_time_s", "fast"), "wall_time_s"),
    ("malformed started_at", COMPLETED, set_at("started_at", "yesterday"), "started_at"),
    ("lowercase status", COMPLETED, set_at(f"{H0}.failed_properties.0.status", "Failure"),
     "harnesses[0].failed_properties[0].status"),
    ("unknown failure_kind", COMPLETED, set_at(f"{H0}.failure_kind", "BROKEN"),
     "harnesses[0].failure_kind"),
    ("unknown outcome kind", NON_COMPLETED, set_at(f"{H0}.outcome.kind", "ABORTED"),
     "harnesses[0].outcome.kind"),
    ("unsupported schema version", COMPLETED, set_at("schema_version", "9.9.9"), "schema_version"),
    ("null where not allowed", COMPLETED, set_at("tools.kani", None), "tools.kani"),
    ("null covers.satisfied", COMPLETED, set_at(f"{H0}.covers.satisfied", None),
     "harnesses[0].covers.satisfied"),
    ("null checks.unreachable", COMPLETED, set_at(f"{H0}.checks.unreachable", None),
     "harnesses[0].checks.unreachable"),
    ("scalar checks.other", COMPLETED, set_at(f"{H0}.checks.other", 9), "harnesses[0].checks.other"),
    ("solver of the wrong shape", COMPLETED, set_at(f"{H0}.attributes.solver", {"Path": "x"}),
     "harnesses[0].attributes.solver"),
    ("attribute kind of the wrong shape", COMPLETED, set_at(f"{H0}.attributes.kind", "Kani"),
     "harnesses[0].attributes.kind"),
    ("missing required field", COMPLETED, delete_at(f"{H0}.resources.verification_time_s"),
     "harnesses[0].resources.verification_time_s"),
    # Elements of arrays that are empty in some fixtures.
    ("integer in cbmc_args", COMPLETED, set_at("configuration.cbmc_args", [5]),
     "configuration.cbmc_args[0]"),
    ("integer in requested_filters", COMPLETED, set_at("harness_selection.requested_filters", [1]),
     "harness_selection.requested_filters[0]"),
    ("integer in enabled_unstable_features", COMPLETED, set_at("enabled_unstable_features", [3]),
     "enabled_unstable_features[0]"),
    ("malformed stub", COMPLETED, set_at(f"{H0}.attributes.stubs", [{"original": 1, "replacement": "f"}]),
     "harnesses[0].attributes.stubs[0].original"),
    ("integer verified stub", COMPLETED, set_at(f"{H0}.attributes.verified_stubs", [1]),
     "harnesses[0].attributes.verified_stubs[0]"),
    ("integer in checks.failure", COMPLETED, set_at(f"{H0}.checks.failure", [4]),
     "harnesses[0].checks.failure[0]"),
    ("malformed checks.other entry", COMPLETED, set_at(f"{H0}.checks.other", [{"id": 5, "status": "SUCCESS"}]),
     "harnesses[0].checks.other[0].id"),
    ("malformed covers.other entry", COMPLETED, set_at(f"{H0}.covers.other", [{"id": "c"}]),
     "harnesses[0].covers.other[0].status"),
    ("incomplete unsupported construct", COMPLETED, set_at(f"{H0}.unsupported_constructs", [{}]),
     "harnesses[0].unsupported_constructs[0].id"),
    ("malformed warning", COMPLETED, set_at(f"{H0}.warnings", [{"message": "m"}]),
     "harnesses[0].warnings[0].truncated"),
    ("integer harness in an otherwise valid list", NON_COMPLETED, set_at("harnesses.2", 7),
     "harnesses[2]"),
    # Presence by outcome.
    ("COMPLETED without a verdict", COMPLETED, delete_at(f"{H0}.outcome.verdict"),
     "outcome.verdict must be present exactly on COMPLETED"),
    ("COMPLETED without failure_kind", COMPLETED, delete_at(f"{H0}.failure_kind"),
     "failure_kind must be present exactly on COMPLETED"),
    ("COMPLETED with a null n_properties", COMPLETED, set_at(f"{H0}.n_properties", None),
     "n_properties must be an integer on COMPLETED"),
    ("COMPLETED with a null checks.total", COMPLETED, set_at(f"{H0}.checks.total", None),
     "checks.total must be an integer on COMPLETED"),
    ("COMPLETED with a CRASHED code", COMPLETED, set_at(f"{H0}.outcome.code", 1),
     "outcome.code must be present exactly on CRASHED"),
    ("verdict contradicting failure_kind", COMPLETED, set_at(f"{H0}.outcome.verdict", "SUCCESS"),
     "does not follow from"),
    ("TIMEOUT carrying a verdict", NON_COMPLETED, set_at(f"{H0}.outcome.verdict", "SUCCESS"),
     "outcome.verdict must be present exactly on COMPLETED"),
    ("TIMEOUT carrying failure_kind", NON_COMPLETED, set_at(f"{H0}.failure_kind", "NONE"),
     "failure_kind must be present exactly on COMPLETED"),
    ("TIMEOUT with an integer checks.total", NON_COMPLETED, set_at(f"{H0}.checks.total", 0),
     "checks.total must be null on TIMEOUT"),
    ("TIMEOUT with an integer n_failed", NON_COMPLETED, set_at(f"{H0}.n_failed", 0),
     "n_failed must be null on TIMEOUT"),
    ("TIMEOUT with a non-empty checks.failure", NON_COMPLETED, set_at(f"{H0}.checks.failure", ["a.b.1"]),
     "checks.failure must be [] on TIMEOUT"),
    ("OUT_OF_MEMORY with a satisfied cover", NON_COMPLETED, set_at("harnesses.1.covers.satisfied", ["c"]),
     "covers.satisfied must be [] on OUT_OF_MEMORY"),
    ("OUT_OF_MEMORY with a failed property", NON_COMPLETED,
     set_at("harnesses.1.failed_properties", [{"id": "a"}]),
     "failed_properties"),
    ("CRASHED without a code", NON_COMPLETED, delete_at("harnesses.2.outcome.code"),
     "outcome.code must be present exactly on CRASHED"),
    ("CRASHED without a message", NON_COMPLETED, delete_at("harnesses.2.outcome.message"),
     "outcome.message must be present exactly on CRASHED"),
    ("CRASHED with a verdict", NON_COMPLETED, set_at("harnesses.2.outcome.verdict", "FAILURE"),
     "outcome.verdict must be present exactly on COMPLETED"),
    ("run-level outcome with a verdict", COMPLETED, set_at("outcome", {"kind": "COMPLETED", "verdict": "SUCCESS"}),
     "run-level outcome"),
    ("run-level outcome that is not COMPLETED", NON_COMPLETED, set_at("outcome.kind", "TIMEOUT"),
     "outcome.kind"),
    ("root that is not an object", COMPLETED, replace_root([]), "<root>: expected object"),
    # Run-level rules.
    ("commit without a dirty flag", NO_HARNESSES, set_at("kani_commit_dirty", None),
     "kani_commit_dirty must be null exactly when"),
    ("dirty flag without a commit", COMPLETED, set_at("kani_commit_dirty", False),
     "kani_commit_dirty must be null exactly when"),
    ("unsorted unstable features", COMPLETED, set_at("enabled_unstable_features", ["export-json", "a"]),
     "enabled_unstable_features must be sorted"),
    ("started_at that is no date", COMPLETED, set_at("started_at", "2026-02-30T00:00:00Z"),
     "started_at is not a real date"),
    ("negative wall time", COMPLETED, set_at("wall_time_s", -1), "wall_time_s: must be >= 0 and finite"),
    ("NaN wall time", COMPLETED, set_at("wall_time_s", float("nan")), "wall_time_s: must be >= 0 and finite"),
    ("infinite wall time", COMPLETED, set_at("wall_time_s", float("inf")),
     "wall_time_s: must be >= 0 and finite"),
    ("negative harness timeout", NON_COMPLETED, set_at("harness_timeout_s", -5),
     "harness_timeout_s: must be >= 0 and finite"),
    ("TIMEOUT without harness_timeout_s", NON_COMPLETED, set_at("harness_timeout_s", None),
     "harness_timeout_s is null"),
    # Harness selection, on the marker so that no other rule applies.
    ("unmatched filter under --exact", MARKER, set_at("harness_selection.exact", True),
     "unmatched_filters ['typo'] must be empty when exact is true"),
    ("unmatched filter that was not requested", MARKER,
     set_at("harness_selection.unmatched_filters", ["typo", "other"]),
     "unmatched_filters ['other'] are not in requested_filters"),
    ("every requested filter unmatched", MARKER,
     set_at("harness_selection.unmatched_filters", ["check_two", "typo"]),
     "every requested filter is unmatched"),
    ("filters requested but nothing matched", MARKER, set_at("harness_selection.matched_count", 0),
     "filters were requested but matched_count is 0"),
    ("negative matched_count on the marker", MARKER, set_at("harness_selection.matched_count", -1),
     "harness_selection.matched_count: must be >= 0 and finite"),
    ("negative matched_count in a terminal document", COMPLETED, set_at("harness_selection.matched_count", -1),
     "harness_selection.matched_count: must be >= 0 and finite"),
    # INCOMPLETE marker.
    ("marker without target", MARKER, delete_at("target"), "Missing required field: target"),
    *[(f"marker carrying {field}", MARKER, set_at(field, []), f"{field}: an INCOMPLETE marker does not carry it")
      for field in ("outcome", "wall_time_s", "summary", "harnesses")],
    # Harness order and identity.
    ("harnesses out of order", NON_COMPLETED, reverse_at("harnesses"), "harnesses must be sorted by"),
    ("repeated crate_name and name", NON_COMPLETED, set_at("harnesses.1.name", "timed_out"),
     "harnesses: (crate_name, name) ('test', 'timed_out') occurs 2 times"),
    # Per-harness values.
    ("is_bounded on a manual harness", COMPLETED, set_at(f"{H0}.is_bounded", True), "is_bounded is true but"),
    ("line 0", COMPLETED, set_at(f"{H0}.line", 0), "line: must be >= 1"),
    ("negative verification time", COMPLETED, set_at(f"{H0}.resources.verification_time_s", -1),
     "verification_time_s: must be >= 0 and finite"),
    ("negative warnings_truncated", COMPLETED, set_at(f"{H0}.warnings_truncated", -1),
     "warnings_truncated: must be >= 0 and finite"),
    ("negative original_chars", COMPLETED, set_at(f"{H0}.warnings.1.original_chars", -1),
     "original_chars: must be >= 0 and finite"),
    ("negative unwind_value", COMPLETED, set_at(f"{H0}.attributes.unwind_value", -3),
     "unwind_value: must be >= 0 and finite"),
    ("negative resolved_unwind", COMPLETED, set_at(f"{H0}.resolved_unwind", -1),
     "resolved_unwind: must be >= 0 and finite"),
    ("negative checks.success", SUCCESS, all_of(
        set_at(f"{H0}.checks.success", -1), set_at(f"{H0}.checks.total", 0), set_at(f"{H0}.n_properties", 1),
        set_at("summary.checks_total", 0), set_at("summary.checks_success", -1)),
     "checks.success: must be >= 0 and finite"),
    ("truncated warning without original_chars", COMPLETED, set_at(f"{H0}.warnings.1.original_chars", None),
     "truncated is True but original_chars is None"),
    ("untruncated warning with original_chars", COMPLETED, set_at(f"{H0}.warnings.0.original_chars", 5),
     "truncated is False but original_chars is 5"),
    # Summary and run_state.
    *[(f"summary.{field} off by one", SUCCESS, bump_at(f"summary.{field}"), f"summary.{field} (")
      for field in ("total", "successful", "failed", "checks_total", "checks_success", "covers_total",
                    "covers_satisfied")],
    ("summary counts a non-COMPLETED harness", NON_COMPLETED, bump_at("summary.successful"),
     "summary.successful (1) != 0, recomputed over the harnesses"),
    ("NO_HARNESSES_SELECTED with a requested filter", NO_HARNESSES,
     set_at("harness_selection.requested_filters", ["x"]), "NO_HARNESSES_SELECTED needs no requested filters"),
    ("NO_HARNESSES_SELECTED with a matched harness", NO_HARNESSES, set_at("harness_selection.matched_count", 1),
     "NO_HARNESSES_SELECTED needs no requested filters"),
    ("NO_HARNESSES_SELECTED with a harness entry", COMPLETED, all_of(
        set_at("run_state", "NO_HARNESSES_SELECTED"), set_at("harness_selection.requested_filters", []),
        set_at("harness_selection.unmatched_filters", []), set_at("harness_selection.matched_count", 0)),
     "NO_HARNESSES_SELECTED needs no requested filters"),
    ("COMPLETE although nothing matched", COMPLETED, all_of(
        set_at("harness_selection.requested_filters", []), set_at("harness_selection.unmatched_filters", []),
        set_at("harness_selection.matched_count", 0)),
     "matched_count is 0, which is NO_HARNESSES_SELECTED"),
    ("COMPLETE with fewer entries than matches", COMPLETED, set_at("harness_selection.matched_count", 2),
     "COMPLETE but summary.total (1) != matched_count (2)"),
    ("PARTIAL with every match present", COMPLETED, set_at("run_state", "PARTIAL"),
     "PARTIAL but summary.total (1) is not below matched_count (1)"),
    # Buckets.
    ("checks.total not the sum of its parts", COMPLETED, set_at(f"{H0}.checks.total", 5),
     "checks.total (5) != the 3 properties"),
    ("covers.total not the sum of its parts", COMPLETED, set_at(f"{H0}.covers.total", 2),
     "covers.total (2) != the 1 properties"),
    ("n_properties not the two totals", COMPLETED, set_at(f"{H0}.n_properties", 99),
     "n_properties (99) != checks.total + covers.total (4)"),
    ("n_failed not len(checks.failure)", COMPLETED, set_at(f"{H0}.n_failed", 0),
     "n_failed (0) != len(checks.failure) (2)"),
    ("failed_properties id not in checks.failure", COMPLETED,
     set_at(f"{H0}.failed_properties.0.id", "check_two.assertion.7"), "failed_properties ids"),
    ("failed property with status SUCCESS", COMPLETED, set_at(f"{H0}.failed_properties.0.status", "SUCCESS"),
     "status SUCCESS, must be FAILURE"),
    ("failed property of class cover", COMPLETED, set_at(f"{H0}.failed_properties.0.class", "cover"),
     "class cover never appears there"),
    ("cover id in checks", SUCCESS, add_property("checks", "unknown", "check_ok.cover.7"),
     "id 'check_ok.cover.7' (class cover) does not belong in checks"),
    ("cover id in checks.other", COMPLETED, set_at(f"{H0}.checks.other.0.id", "check_two.cover.2"),
     "checks.other: id 'check_two.cover.2' (class cover) does not belong in checks"),
    ("code_coverage id in checks", SUCCESS, add_property("checks", "unknown", "check_ok.code_coverage.1"),
     "(class code_coverage) does not belong in checks"),
    ("assertion id in covers", SUCCESS, add_property("covers", "unknown", "check_ok.assertion.7"),
     "(class assertion) does not belong in covers"),
    ("FAILURE in checks.other", COMPLETED, set_at(f"{H0}.checks.other.0.status", "FAILURE"),
     "status FAILURE, which has a list of its own"),
    ("SATISFIED in covers.other", SUCCESS, add_property("covers", "other", "check_ok.cover.8", "SATISFIED"),
     "status SATISFIED, which has a list of its own"),
    # Unsupported constructs.
    ("unsupported construct of another class", COMPLETED,
     set_at(f"{H0}.unsupported_constructs.0.class", "assertion"), "class assertion, expected 'unsupported_construct'"),
    ("unsupported failure unlike its failed property", COMPLETED,
     set_at(f"{H0}.unsupported_constructs.0.description", "other text"),
     "differs from its record in failed_properties"),
    ("unsupported failure with no failed property", COMPLETED,
     set_at(f"{H0}.unsupported_constructs.0.id", "check_two.unsupported_construct.9"),
     "the checks record 'check_two.unsupported_construct.9' as SUCCESS"),
    ("unsupported status against its bucket", COMPLETED,
     set_at(f"{H0}.unsupported_constructs.0.status", "UNREACHABLE"),
     "status UNREACHABLE, but the checks record 'check_two.unsupported_construct.1' as FAILURE"),
    ("one unsupported id with two statuses", COMPLETED,
     append_copy(f"{H0}.unsupported_constructs", status="SUCCESS"),
     "status SUCCESS, but the checks record 'check_two.unsupported_construct.1' as FAILURE"),
    ("unsupported id in two buckets with consistent counts", COMPLETED, all_of(
        add_property("checks", "unreachable", "check_two.unsupported_construct.1"),
        set_at(f"{H0}.unsupported_constructs.0.status", "UNREACHABLE")),
     "'check_two.unsupported_construct.1' has contradictory statuses FAILURE and UNREACHABLE"),
    ("more successful unsupported constructs than checks.success", SUCCESS, all_of(
        append_copy(f"{H0}.unsupported_constructs", id="check_ok.unsupported_construct.2"),
        append_copy(f"{H0}.unsupported_constructs", id="check_ok.unsupported_construct.3")),
     "3 unsupported_constructs succeeded but checks.success is 2"),
    # failure_kind against the properties.
    ("NONE with a failed check", SUCCESS, add_property("checks", "failure", "check_ok.assertion.9"),
     "failure_kind is NONE but a property failed or errored"),
    ("NONE with an errored check", SUCCESS, add_property("checks", "error", "check_ok.assertion.9"),
     "failure_kind is NONE but a property failed or errored"),
    ("NONE with an errored cover", SUCCESS, add_property("covers", "error", "check_ok.cover.9"),
     "failure_kind is NONE but a property failed or errored"),
    ("NONE with a failed cover", SUCCESS, add_property("covers", "other", "check_ok.cover.9", "FAILURE"),
     "failure_kind is NONE but a property failed or errored"),
    ("PANICS_ONLY with an errored check", "expected-panic", add_property("checks", "error", "check_ok.assertion.8"),
     "failure_kind is PANICS_ONLY but a property errored"),
    ("PANICS_ONLY with an errored cover", "expected-panic", add_property("covers", "error", "check_ok.cover.9"),
     "failure_kind is PANICS_ONLY but a property errored"),
]

# (case name, fixture or twin, mutation): unknown fields that consumers must ignore.
EXTENSIONS = [
    ("unknown top-level field", COMPLETED, set_at("future_field", {"a": 1})),
    ("unknown run-level outcome member", COMPLETED, set_at("outcome.extra", True)),
    ("unknown harness outcome member", COMPLETED, set_at(f"{H0}.outcome.extra", True)),
    ("unknown member of a COMPLETED checks object", COMPLETED, set_at(f"{H0}.checks.extra", ["future"])),
    ("unknown member of a TIMEOUT checks object", NON_COMPLETED, set_at(f"{H0}.checks.extra", ["future"])),
    ("unknown member of an OUT_OF_MEMORY covers object", NON_COMPLETED,
     set_at("harnesses.1.covers.extra", ["future"])),
    ("unknown field beside a Binary solver", COMPLETED,
     set_at(f"{H0}.attributes.solver", {"Binary": "my-solver", "extra": 1})),
    ("unknown field beside a ProofForContract", COMPLETED,
     set_at(f"{H0}.attributes.kind", {"ProofForContract": {"target_fn": "f"}, "extra": 1})),
    ("unknown covers.success, integer", COMPLETED, set_at(f"{H0}.covers.success", 1)),
    ("unknown covers.success, string", COMPLETED, set_at(f"{H0}.covers.success", "future")),
    ("unknown field inside a ProofForContract", COMPLETED,
     set_at(f"{H0}.attributes.kind", {"ProofForContract": {"target_fn": "f", "extra": 1}})),
]


def checks_all_unreachable(doc):
    set_at(f"{H0}.checks.success", 0)(doc)
    set_at(f"{H0}.checks.unreachable", [f"check_ok.assertion.{n}" for n in (1, 2, 3)])(doc)


def covers_all_unreachable(doc):
    set_at(f"{H0}.covers.satisfied", [])(doc)
    set_at(f"{H0}.covers.unreachable", ["check_ok.cover.1"])(doc)


REACH_CHECKS_OFF = set_at("configuration.checks.assertion_reach_checks", False)
FAILING = set_at(f"{H0}.outcome.verdict", "FAILURE")


def predicate(name):
    def call(validator, doc):
        harness = doc["harnesses"][0]
        if name == "cover_only_vacuous":
            return validator.cover_only_vacuous(harness)
        return getattr(validator, name)(harness, doc["configuration"])
    return call


# (case name, fixture, mutation or None, predicate, expected result). The predicates read only
# the verdict, the unreachable lists, the totals and the reach-checks flag.
PREDICATES = [
    ("vacuous_pass: one of three checks unreachable", SUCCESS, None, predicate("vacuous_pass"), False),
    ("vacuous_pass: every check unreachable", SUCCESS, checks_all_unreachable, predicate("vacuous_pass"), True),
    ("vacuous_pass: failing verdict", SUCCESS, all_of(checks_all_unreachable, FAILING),
     predicate("vacuous_pass"), False),
    ("vacuous_pass: no checks", SUCCESS,
     all_of(set_at(f"{H0}.checks.total", 0), set_at(f"{H0}.checks.unreachable", [])),
     predicate("vacuous_pass"), False),
    ("vacuous_pass: reach checks off", SUCCESS, all_of(checks_all_unreachable, REACH_CHECKS_OFF),
     predicate("vacuous_pass"), None),
    ("vacuous_pass: every cover unreachable", SUCCESS, covers_all_unreachable, predicate("vacuous_pass"), False),
    ("vacuity_suspect: one unreachable check", SUCCESS, None, predicate("vacuity_suspect"), True),
    ("vacuity_suspect: no unreachable check", SUCCESS, set_at(f"{H0}.checks.unreachable", []),
     predicate("vacuity_suspect"), False),
    ("vacuity_suspect: failing verdict", SUCCESS, FAILING, predicate("vacuity_suspect"), False),
    ("vacuity_suspect: reach checks off", SUCCESS, REACH_CHECKS_OFF, predicate("vacuity_suspect"), None),
    ("cover_only_vacuous: a satisfied cover", SUCCESS, None, predicate("cover_only_vacuous"), False),
    ("cover_only_vacuous: every cover unreachable", SUCCESS, covers_all_unreachable,
     predicate("cover_only_vacuous"), True),
    ("cover_only_vacuous: one of two covers unreachable", SUCCESS, all_of(
        set_at(f"{H0}.covers.total", 2), set_at(f"{H0}.covers.unreachable", ["check_ok.cover.2"])),
     predicate("cover_only_vacuous"), False),
    ("cover_only_vacuous: no covers", SUCCESS,
     all_of(set_at(f"{H0}.covers.total", 0), set_at(f"{H0}.covers.satisfied", [])),
     predicate("cover_only_vacuous"), False),
    ("cover_only_vacuous: every check unreachable", SUCCESS, checks_all_unreachable,
     predicate("cover_only_vacuous"), False),
]

ID_CLASSES = {
    "assertion.1": "assertion",
    "main.cover.2": "cover",
    "a.b.code_coverage.3": "code_coverage",
    "f.NaN.1": "NaN",
    "alloc::raw_vec::RawVec::<u8>::allocate_in.1": "missing_definition",
    "alloc.RawVec.1": "missing_definition",
    "alloc::foo.recursion": "recursion",
    ".recursion": "recursion",
    "no_counter": None,
    "123": None,
    "f.cover.\u00b2": None,
    "f.assertion.x": None,
}


def load_validator(path):
    spec = importlib.util.spec_from_file_location("validate_json_export", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def check_join_key_and_warnings(validator, failures):
    """`join_key` tells same-named harnesses of two crates apart, which warnings_shape_equivalent relies on."""
    doc = load_base(SUCCESS)
    other_crate = copy.deepcopy(doc["harnesses"][0])
    other_crate["crate_name"] = "other"
    doc["harnesses"].append(other_crate)
    if validator.join_key(doc["harnesses"][0]) == validator.join_key(doc["harnesses"][1]):
        failures.append("join_key does not tell same-named harnesses of two crates apart")

    def with_warnings(warnings, dropped=0, index=0):
        changed = copy.deepcopy(doc)
        changed["harnesses"][index]["warnings"] = warnings
        changed["harnesses"][index]["warnings_truncated"] = dropped
        return changed

    plain = {"message": "warning: a", "truncated": False, "original_chars": None}
    cut = {"message": "warning: a", "truncated": True, "original_chars": 90}
    with_plain = with_warnings([plain])
    cases = [
        ("the same document", doc, doc, True),
        ("a different message", with_plain, with_warnings([{**plain, "message": "warning: b"}]), True),
        ("an added warning", doc, with_plain, False),
        ("a truncated warning", with_plain, with_warnings([cut]), False),
        ("a dropped warning count", doc, with_warnings([], 1), False),
        ("a warning on the first of two same-named harnesses", doc, with_warnings([plain], 0, 0), False),
        ("a warning on the second of two same-named harnesses", doc, with_warnings([plain], 0, 1), False),
        ("a missing harness", doc, {"harnesses": doc["harnesses"][:1]}, False),
    ]
    for name, left, right, expected in cases:
        if validator.warnings_shape_equivalent(left, right) is not expected:
            failures.append(f"warnings_shape_equivalent should be {expected} for {name}")
        else:
            print(f"  warnings_shape_equivalent for {name}: {expected}")


def run_validator(validator, path):
    result = subprocess.run(
        [sys.executable, validator, str(path)], capture_output=True, text=True
    )
    return result.returncode, result.stdout + result.stderr


def main():
    validator = sys.argv[1]
    failures = []

    scratch = Path("fixture_under_test.json")

    def run_document(doc):
        scratch.write_text(json.dumps(doc))
        return run_validator(validator, scratch)

    try:
        for fixture in sorted(FIXTURES.glob("fixture-*.json")):
            code, output = run_validator(validator, fixture)
            if code != 0:
                failures.append(f"valid fixture {fixture.name} was rejected:\n{output}")
            else:
                print(f"  {fixture.name}: accepted")

        for twin in TWINS:
            code, output = run_document(load_base(twin))
            if code != 0:
                failures.append(f"valid twin {twin} was rejected:\n{output}")
            else:
                print(f"  twin {twin}: accepted")

        for name, base, mutate, expected in CASES:
            doc = load_base(base)
            replaced = mutate(doc)
            doc = doc if replaced is None else replaced
            code, output = run_document(doc)
            if code == 0:
                failures.append(f"{name}: the validator accepted a malformed export")
            elif "Traceback" in output:
                failures.append(f"{name}: the validator crashed:\n{output}")
            elif expected not in output:
                failures.append(f"{name}: diagnostic does not mention {expected!r}:\n{output}")
            else:
                print(f"  {name}: correctly rejected")
        for name, base, mutate in EXTENSIONS:
            doc = load_base(base)
            mutate(doc)
            code, output = run_document(doc)
            if code != 0:
                failures.append(f"{name}: the validator rejected an unknown field:\n{output}")
            else:
                print(f"  {name}: accepted")
    finally:
        scratch.unlink(missing_ok=True)

    module = load_validator(validator)
    for name, base, mutate, call, expected in PREDICATES:
        doc = load_base(base)
        if mutate:
            mutate(doc)
        result = call(module, doc)
        if result is not expected:
            failures.append(f"{name}: expected {expected}, got {result}")
        else:
            print(f"  {name}: {expected}")
    for property_id, expected in ID_CLASSES.items():
        if module.id_class(property_id) != expected:
            failures.append(f"id_class({property_id!r}) should be {expected!r}, got {module.id_class(property_id)!r}")
    check_join_key_and_warnings(module, failures)

    for failure in failures:
        print(f"ERROR: {failure}")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
