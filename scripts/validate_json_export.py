#!/usr/bin/env python3
# Copyright Kani Contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT
#
"""
JSON Export Validation Script for Kani Integration Tests

Validates an `--export-json` document (RFC 0015) in three steps:

1. Structure, against the schema template `kani_json_schema.json`.
2. Presence by outcome: the fields that depend on a harness's `outcome.kind`.
3. The cross-field rules the RFC states in prose: bucket shape and counts, `failure_kind`,
   `summary` and `run_state`, ordering, and value ranges.

An `INCOMPLETE` marker is checked against the template without the fields only a terminal
document has, and against the run-level rules of step 3. Unknown fields are ignored, as the
RFC requires of consumers.

The functions at the end are the RFC's reading rules, for tests that import this file.

Template syntax:
    "string" | "integer" | "number" | "boolean"   a scalar of that type
    "<type>?"                                     the same, or null
    "enum:A|B"                                    one of the listed strings
    "pattern:<regex>"                             a string matching the whole regex
    "named:<name>"                                a check in NAMED_CHECKS
    "ref:<name>"                                  the template in `_defs`
    {...}                                         an object; `_optional` lists keys that
                                                  may be absent, `_nullable` keys that
                                                  may be null
    [<template>]                                  an array whose elements all match
"""

import json
import math
import re
import sys
from collections import Counter, defaultdict
from datetime import datetime
from pathlib import Path


INCOMPLETE_ABSENT = ("outcome", "wall_time_s", "summary", "harnesses")
CHECK_LISTS = ("failure", "unreachable", "undetermined", "error", "unknown")
COVER_LISTS = ("satisfied", "unsatisfiable", "unreachable", "undetermined", "error", "unknown")
# The statuses that have a list of their own in each domain. Any other status goes to `other[]`.
NAMED_STATUSES = {
    "checks": {"SUCCESS", "FAILURE", "UNREACHABLE", "UNDETERMINED", "ERROR", "UNKNOWN"},
    "covers": {"SATISFIED", "UNSATISFIABLE", "UNREACHABLE", "UNDETERMINED", "ERROR", "UNKNOWN"},
}
# Mirrors `Property::has_property_class_format` in kani-driver/src/cbmc_output_parser.rs.
CLASS_FORMAT = re.compile(r"NaN|[a-z_-]*")


def load_schema_template():
    """Load the JSON schema template"""
    # Find schema template in tests/json-handler/schema-validation directory
    script_dir = Path(__file__).parent
    schema_path = (
        script_dir.parent
        / "tests"
        / "json-handler"
        / "schema-validation"
        / "kani_json_schema.json"
    )

    if not schema_path.exists():
        print(f"ERROR: Schema template not found at {schema_path}")
        return None

    with open(schema_path, "r") as f:
        return json.load(f)


def type_name(value):
    return "null" if value is None else type(value).__name__


def check_solver(value):
    """`attributes.solver`: a solver name, or {"Binary": "<path>"}. Names are an open set."""
    if isinstance(value, str):
        return None
    if isinstance(value, dict) and isinstance(value.get("Binary"), str):
        return None
    return f'expected a solver name or {{"Binary": <string>}}, got {value!r}'


def check_attribute_kind(value):
    """`attributes.kind`: "Proof", "Test" or {"ProofForContract": {"target_fn": <string>}}."""
    if value in ("Proof", "Test"):
        return None
    if (
        isinstance(value, dict)
        and isinstance(value.get("ProofForContract"), dict)
        and isinstance(value["ProofForContract"].get("target_fn"), str)
    ):
        return None
    return f"expected Proof, Test or a ProofForContract object, got {value!r}"


NAMED_CHECKS = {"solver": check_solver, "attribute_kind": check_attribute_kind}


def check_scalar(value, spec, path, defs, errors):
    if spec.startswith("ref:"):
        check_value(value, defs[spec[len("ref:"):]], path, defs, errors)
        return
    nullable = spec.endswith("?")
    spec = spec[:-1] if nullable else spec
    if value is None:
        if not nullable:
            errors.append(f"{path}: null is not allowed (expected {spec})")
        return
    kind, _, argument = spec.partition(":")
    problem = None
    if kind == "string":
        ok = isinstance(value, str)
    elif kind == "integer":
        ok = isinstance(value, int) and not isinstance(value, bool)
    elif kind == "number":
        ok = isinstance(value, (int, float)) and not isinstance(value, bool)
    elif kind == "boolean":
        ok = isinstance(value, bool)
    elif kind == "enum":
        ok = isinstance(value, str) and value in argument.split("|")
    elif kind == "pattern":
        ok = isinstance(value, str) and re.fullmatch(argument, value) is not None
    elif kind == "named":
        problem = NAMED_CHECKS[argument](value)
        ok = problem is None
    else:
        raise ValueError(f"unknown template type {spec!r} at {path}")
    if not ok:
        problem = problem or f"expected {spec}, got {type_name(value)} {value!r}"
        errors.append(f"{path}: {problem}")


def check_value(value, spec, path, defs, errors):
    """Append every mismatch between `value` and the template `spec` to `errors`."""
    if isinstance(spec, str):
        check_scalar(value, spec, path, defs, errors)
    elif isinstance(spec, dict):
        if not isinstance(value, dict):
            errors.append(
                f"{path or '<root>'}: expected object, got {type_name(value)}")
            return
        optional = spec.get("_optional", [])
        nullable = spec.get("_nullable", [])
        for key, sub_spec in spec.items():
            if key.startswith("_"):
                continue
            sub_path = f"{path}.{key}" if path else key
            if key not in value:
                if key not in optional:
                    errors.append(f"Missing required field: {sub_path}")
            elif value[key] is not None or key not in nullable:
                check_value(value[key], sub_spec, sub_path, defs, errors)
    elif isinstance(spec, list):
        if not isinstance(value, list):
            errors.append(f"{path}: expected array, got {type_name(value)}")
            return
        for index, item in enumerate(value):
            check_value(item, spec[0], f"{path}[{index}]", defs, errors)


def check_harness_presence(harness, template, path, errors):
    """The RFC 0015 presence matrix for one harness, by `outcome.kind`."""
    outcome = harness["outcome"]
    kind = outcome["kind"]
    completed = kind == "COMPLETED"
    crashed = kind == "CRASHED"

    def require(condition, message):
        if not condition:
            errors.append(f"{path}: {message}")

    require(("verdict" in outcome) == completed,
            f"outcome.verdict must be present exactly on COMPLETED (kind {kind})")
    require(("failure_kind" in harness) == completed,
            f"failure_kind must be present exactly on COMPLETED (kind {kind})")
    require(("code" in outcome) == crashed,
            f"outcome.code must be present exactly on CRASHED (kind {kind})")
    require(("message" in outcome) == crashed,
            f"outcome.message must be present exactly on CRASHED (kind {kind})")

    totals = {
        "n_properties": harness["n_properties"],
        "n_failed": harness["n_failed"],
        "checks.total": harness["checks"]["total"],
        "checks.success": harness["checks"]["success"],
        "covers.total": harness["covers"]["total"],
    }
    for name, value in totals.items():
        if completed:
            require(value is not None, f"{name} must be an integer on COMPLETED")
        else:
            require(value is None, f"{name} must be null on {kind}, got {value!r}")

    if not completed:
        lists = {
            "failed_properties": harness["failed_properties"],
            "unsupported_constructs": harness["unsupported_constructs"],
        }
        for bucket in ("checks", "covers"):
            for name, spec in template[bucket].items():
                if isinstance(spec, list):
                    lists[f"{bucket}.{name}"] = harness[bucket][name]
        for name, value in lists.items():
            require(value == [], f"{name} must be [] on {kind}, got {value!r}")
    elif "verdict" in outcome and "failure_kind" in harness:
        # RFC 0015, "Value domains": the verdict follows from `should_panic` and
        # `failure_kind`.
        should_panic = harness["attributes"]["should_panic"]
        passing_kind = "PANICS_ONLY" if should_panic else "NONE"
        expected = "SUCCESS" if harness["failure_kind"] == passing_kind else "FAILURE"
        require(
            outcome["verdict"] == expected,
            f"outcome.verdict {outcome['verdict']} does not follow from "
            f"should_panic={should_panic} and failure_kind={harness['failure_kind']}",
        )


def check_number(value, path, errors):
    """A count or duration is not negative and not infinite or NaN. A null is not checked."""
    if value is None:
        return
    if not 0 <= value < math.inf:
        errors.append(f"{path}: must be >= 0 and finite, got {value!r}")


def check_selection(data, errors):
    """The `harness_selection` rules of RFC 0015 "Completeness"."""
    selection = data["harness_selection"]
    requested, unmatched = selection["requested_filters"], selection["unmatched_filters"]
    if selection["exact"] and unmatched:
        errors.append(f"harness_selection: unmatched_filters {unmatched} must be empty when exact is true")
    stray = [name for name in unmatched if name not in requested]
    if stray:
        errors.append(f"harness_selection: unmatched_filters {stray} are not in requested_filters")
    if requested and all(name in unmatched for name in requested):
        errors.append("harness_selection: every requested filter is unmatched")
    check_number(selection["matched_count"], "harness_selection.matched_count", errors)
    if requested and selection["matched_count"] == 0:
        errors.append("harness_selection: filters were requested but matched_count is 0")


def check_run_rules(data, errors):
    """The run-level rules shared by an INCOMPLETE marker and a terminal document."""
    if (data["kani_commit"] is None) != (data["kani_commit_dirty"] is None):
        errors.append("kani_commit_dirty must be null exactly when kani_commit is null")
    features = data["enabled_unstable_features"]
    if features != sorted(features):
        errors.append(f"enabled_unstable_features must be sorted, got {features}")
    try:
        datetime.strptime(data["started_at"], "%Y-%m-%dT%H:%M:%SZ")
    except ValueError:
        errors.append(f"started_at is not a real date and time: {data['started_at']!r}")
    check_number(data["harness_timeout_s"], "harness_timeout_s", errors)
    check_selection(data, errors)


def id_class(property_id):
    """
    The property class CBMC's id encodes, or None if the id has no counter segment.

    An id whose second-to-last segment is not in class format is a function name: the
    class is then `missing_definition`, as the Rust parser decides.
    """
    if property_id.endswith(".recursion"):
        return "recursion"
    parts = property_id.rsplit(".", 2)
    if len(parts) < 2 or not (parts[-1].isascii() and parts[-1].isdigit()):
        return None
    return parts[-2] if CLASS_FORMAT.fullmatch(parts[-2]) else "missing_definition"


def recorded_statuses(bucket, lists):
    """The statuses a `checks` or `covers` object gives each id, across its lists and `other[]`."""
    statuses = defaultdict(set)
    for field in lists:
        for property_id in bucket[field]:
            statuses[property_id].add(field.upper())
    for entry in bucket["other"]:
        statuses[entry["id"]].add(entry["status"])
    return statuses


def check_buckets(harness, path, errors):
    """The `checks`/`covers` bucket rules of a COMPLETED harness."""
    for name, lists in (("checks", CHECK_LISTS), ("covers", COVER_LISTS)):
        bucket, where = harness[name], f"{path}.{name}"
        ids = [(field, i) for field in lists for i in bucket[field]]
        ids += [("other", entry["id"]) for entry in bucket["other"]]
        for field, property_id in ids:
            property_class = id_class(property_id)
            if name == "covers":
                belongs = property_class == "cover"
            else:
                belongs = property_class not in ("cover", "code_coverage")
            if not belongs:
                errors.append(f"{where}.{field}: id {property_id!r} (class {property_class}) does not belong in {name}")
        for property_id, statuses in recorded_statuses(bucket, lists).items():
            if len(statuses) > 1:
                errors.append(f"{where}: {property_id!r} has contradictory statuses {' and '.join(sorted(statuses))}")
        for entry in bucket["other"]:
            if entry["status"] in NAMED_STATUSES[name]:
                errors.append(f"{where}.other: {entry['id']!r} has status {entry['status']}, "
                              "which has a list of its own")
        success = bucket["success"] if name == "checks" else 0
        accounted = success + sum(len(bucket[field]) for field in lists) + len(bucket["other"])
        if accounted != bucket["total"]:
            errors.append(f"{where}.total ({bucket['total']}) != the {accounted} properties its parts account for")
    check_number(harness["checks"]["success"], f"{path}.checks.success", errors)
    expected = harness["checks"]["total"] + harness["covers"]["total"]
    if harness["n_properties"] != expected:
        errors.append(f"{path}.n_properties ({harness['n_properties']}) != checks.total + covers.total ({expected})")


def check_failed_properties(harness, path, errors):
    """`failed_properties[]` is `checks.failure` with full records."""
    failed = harness["failed_properties"]
    failure_ids = harness["checks"]["failure"]
    if Counter(p["id"] for p in failed) != Counter(failure_ids):
        errors.append(f"{path}.failed_properties ids {sorted(p['id'] for p in failed)} != "
                      f"checks.failure {sorted(failure_ids)}")
    if harness["n_failed"] != len(failure_ids):
        errors.append(f"{path}.n_failed ({harness['n_failed']}) != len(checks.failure) ({len(failure_ids)})")
    for i, record in enumerate(failed):
        if record["status"] != "FAILURE":
            errors.append(f"{path}.failed_properties[{i}]: status {record['status']}, must be FAILURE")
        if record["class"] in ("cover", "code_coverage"):
            errors.append(f"{path}.failed_properties[{i}]: class {record['class']} never appears there")


def check_unsupported_constructs(harness, path, errors):
    """`unsupported_constructs[]` agrees with the buckets and with `failed_properties[]`."""
    checks = harness["checks"]
    listed = recorded_statuses(checks, CHECK_LISTS)
    records = harness["unsupported_constructs"]
    for i, record in enumerate(records):
        where = f"{path}.unsupported_constructs[{i}]"
        if record["class"] != "unsupported_construct":
            errors.append(f"{where}: class {record['class']}, expected 'unsupported_construct'")
        # A successful check is only counted, so its id is in no list.
        recorded = listed.get(record["id"], {"SUCCESS"})
        if record["status"] not in recorded:
            errors.append(f"{where}: status {record['status']}, but the checks record {record['id']!r} "
                          f"as {' and '.join(sorted(recorded))}")
        elif record["status"] == "FAILURE" and record not in harness["failed_properties"]:
            errors.append(f"{where}: differs from its record in failed_properties")
    successes = sum(1 for record in records if record["status"] == "SUCCESS")
    if successes > checks["success"]:
        errors.append(f"{path}: {successes} unsupported_constructs succeeded but checks.success is {checks['success']}")


def check_failure_kind(harness, path, errors):
    """`failure_kind` against the properties. ERROR may stand without an ERROR property (ignored quantifiers)."""
    checks, covers = harness["checks"], harness["covers"]
    errored = bool(checks["error"] or covers["error"])
    failed = bool(checks["failure"]) or any(entry["status"] == "FAILURE" for entry in covers["other"])
    kind = harness["failure_kind"]
    if kind == "NONE" and (failed or errored):
        errors.append(f"{path}: failure_kind is NONE but a property failed or errored")
    if kind == "PANICS_ONLY" and errored:
        errors.append(f"{path}: failure_kind is PANICS_ONLY but a property errored")


def check_harness_rules(harness, path, errors):
    if harness["is_bounded"] and not harness["is_automatically_generated"]:
        errors.append(f"{path}: is_bounded is true but is_automatically_generated is false")
    if harness["line"] < 1:
        errors.append(f"{path}.line: must be >= 1, got {harness['line']}")
    check_number(harness["resources"]["verification_time_s"], f"{path}.resources.verification_time_s", errors)
    check_number(harness["warnings_truncated"], f"{path}.warnings_truncated", errors)
    check_number(harness["attributes"]["unwind_value"], f"{path}.attributes.unwind_value", errors)
    check_number(harness["resolved_unwind"], f"{path}.resolved_unwind", errors)
    for i, warning in enumerate(harness["warnings"]):
        if warning["truncated"] != (warning["original_chars"] is not None):
            errors.append(f"{path}.warnings[{i}]: truncated is {warning['truncated']} but "
                          f"original_chars is {warning['original_chars']}")
        check_number(warning["original_chars"], f"{path}.warnings[{i}].original_chars", errors)
    if harness["outcome"]["kind"] == "COMPLETED":
        check_buckets(harness, path, errors)
        check_failed_properties(harness, path, errors)
        check_unsupported_constructs(harness, path, errors)
        check_failure_kind(harness, path, errors)


def check_summary(data, errors):
    """`summary.*` is recomputed over the COMPLETED harnesses (RFC "Summary")."""
    harnesses = data["harnesses"]
    completed = [h for h in harnesses if h["outcome"]["kind"] == "COMPLETED"]
    expected = {
        "total": len(harnesses),
        "successful": sum(1 for h in completed if h["outcome"]["verdict"] == "SUCCESS"),
        "failed": sum(1 for h in completed if h["outcome"]["verdict"] == "FAILURE"),
        "checks_total": sum(h["checks"]["total"] for h in completed),
        "checks_success": sum(h["checks"]["success"] for h in completed),
        "covers_total": sum(h["covers"]["total"] for h in completed),
        "covers_satisfied": sum(len(h["covers"]["satisfied"]) for h in completed),
    }
    for field, value in expected.items():
        if data["summary"][field] != value:
            errors.append(f"summary.{field} ({data['summary'][field]}) != {value}, recomputed over the harnesses")


def check_run_state(data, errors):
    """`run_state` against `harness_selection` and `summary.total` (RFC "Summary")."""
    state, total = data["run_state"], data["summary"]["total"]
    selection = data["harness_selection"]
    matched = selection["matched_count"]
    if state == "NO_HARNESSES_SELECTED":
        if selection["requested_filters"] or matched or total:
            errors.append("run_state NO_HARNESSES_SELECTED needs no requested filters and "
                          f"matched_count == summary.total == 0, got {matched} and {total}")
    elif matched == 0:
        errors.append(f"run_state {state} but matched_count is 0, which is NO_HARNESSES_SELECTED")
    elif state == "COMPLETE" and total != matched:
        errors.append(f"run_state COMPLETE but summary.total ({total}) != matched_count ({matched})")
    elif state == "PARTIAL" and total >= matched:
        errors.append(f"run_state PARTIAL but summary.total ({total}) is not below matched_count ({matched})")


def check_terminal_rules(data, errors):
    check_run_rules(data, errors)
    check_number(data["wall_time_s"], "wall_time_s", errors)
    harnesses = data["harnesses"]
    order = [(h["crate_name"], h["file"], h["line"], h["name"]) for h in harnesses]
    if order != sorted(order):
        errors.append("harnesses must be sorted by (crate_name, file, line, name)")
    for key, count in Counter(join_key(h) for h in harnesses).items():
        if count > 1:
            errors.append(f"harnesses: (crate_name, name) {key} occurs {count} times")
    if data["harness_timeout_s"] is None and any(h["outcome"]["kind"] == "TIMEOUT" for h in harnesses):
        errors.append("a harness timed out but harness_timeout_s is null")
    for index, harness in enumerate(harnesses):
        check_harness_rules(harness, f"harnesses[{index}]", errors)
    check_summary(data, errors)
    check_run_state(data, errors)


def validate_marker(data, schema):
    template = {key: spec for key, spec in schema.items() if key not in INCOMPLETE_ABSENT}
    template["run_state"] = "enum:INCOMPLETE"
    errors = []
    check_value(data, template, "", schema.get("_defs", {}), errors)
    errors += [f"{field}: an INCOMPLETE marker does not carry it" for field in INCOMPLETE_ABSENT if field in data]
    if not errors:
        check_run_rules(data, errors)
    return errors


def validate_document(data, schema):
    """Return the list of problems with an export document (empty if valid)."""
    if isinstance(data, dict) and data.get("run_state") == "INCOMPLETE":
        return validate_marker(data, schema)
    errors = []
    check_value(data, schema, "", schema.get("_defs", {}), errors)
    if errors:
        return errors
    for key in ("verdict", "code", "message"):
        if key in data["outcome"]:
            errors.append(f"outcome.{key}: the run-level outcome carries only a kind")
    for index, harness in enumerate(data["harnesses"]):
        check_harness_presence(
            harness, schema["harnesses"][0], f"harnesses[{index}]", errors)
    if not errors:
        check_terminal_rules(data, errors)
    return errors


def validate_json_structure(json_file, schema=None):
    """
    Validate that JSON export matches the schema template and the presence rules.
    """
    try:
        with open(json_file, "r") as f:
            data = json.load(f)
    except FileNotFoundError:
        print(f"ERROR: JSON file {json_file} not found")
        return False
    except json.JSONDecodeError as e:
        print(f"ERROR: Invalid JSON in {json_file}: {e}")
        return False

    # Load schema if not provided
    if schema is None:
        schema = load_schema_template()
        if schema is None:
            return False

    all_errors = validate_document(data, schema)
    if all_errors:
        print(f"ERROR: Validation failed for {json_file}:")
        for error in all_errors:
            print(f"  - {error}")
        return False

    print(f"JSON structure validation passed for {json_file}")
    return True


def validate_field_path(json_file, field_path, schema=None):
    """
    Validate the structure of the fields at a given path.

    Args:
        json_file: Path to JSON file
        field_path: Dot-separated path (e.g., 'tools', 'summary')
        schema: Optional pre-loaded schema
    """
    try:
        with open(json_file, "r") as f:
            data = json.load(f)
    except Exception as e:
        print(f"ERROR: Failed to load {json_file}: {e}")
        return False

    # Load schema if not provided
    if schema is None:
        schema = load_schema_template()
        if schema is None:
            return False

    # Navigate to the field in both data and schema
    parts = field_path.split(".")
    current_data = data
    current_schema = schema

    for part in parts:
        if part not in current_data:
            print(
                f"ERROR: Field path '{field_path}' not found in data. Missing part: '{part}'"
            )
            return False
        current_data = current_data[part]

        if part not in current_schema:
            print(
                f"ERROR: Field path '{field_path}' not found in schema template. Missing part: '{part}'"
            )
            return False
        current_schema = current_schema[part]

        # Handle arrays - check first item
        if isinstance(current_schema, list) and len(current_schema) > 0:
            current_schema = current_schema[0]
            if isinstance(current_data, list) and len(current_data) > 0:
                current_data = current_data[0]

    errors = []
    defs = schema.get("_defs", {})
    check_value(current_data, current_schema, field_path, defs, errors)
    if errors:
        print(f"ERROR: Validation failed for {field_path}:")
        for error in errors:
            print(f"  - {error}")
        return False

    print(f"Field validation passed for {field_path}")
    return True


def join_key(harness):
    """Harness names are unique only within a crate (RFC "Selection")."""
    return (harness["crate_name"], harness["name"])


def _all_unreachable(bucket):
    return bool(bucket["total"]) and len(bucket["unreachable"]) == bucket["total"]


def vacuous_pass(harness, configuration):
    """
    RFC "Reading the results": a SUCCESS in which every check is unreachable.
    None when `assertion_reach_checks` is off, because the checks then cannot say.
    """
    if not configuration["checks"]["assertion_reach_checks"]:
        return None
    return harness["outcome"].get("verdict") == "SUCCESS" and _all_unreachable(harness["checks"])


def vacuity_suspect(harness, configuration):
    """Advisory: a SUCCESS with any unreachable check. None when `assertion_reach_checks` is off."""
    if not configuration["checks"]["assertion_reach_checks"]:
        return None
    return harness["outcome"].get("verdict") == "SUCCESS" and bool(harness["checks"]["unreachable"])


def cover_only_vacuous(harness):
    """Every cover of the harness is unreachable, which `checks` cannot show."""
    return _all_unreachable(harness["covers"])


def warnings_shape_equivalent(doc_a, doc_b):
    """True if two exports have the same warning records per harness, ignoring the message text."""

    def shapes(doc):
        return {
            join_key(h): ([(w["truncated"], w["original_chars"]) for w in h["warnings"]], h["warnings_truncated"])
            for h in doc["harnesses"]
        }

    return shapes(doc_a) == shapes(doc_b)


def main():
    if len(sys.argv) < 2:
        print(
            "Usage: python3 validate_json_export.py <json_file> [--field-path <path>]"
        )
        sys.exit(1)

    json_file = sys.argv[1]

    # Check if specific field validation requested
    if len(sys.argv) > 2 and sys.argv[2] == "--field-path":
        if len(sys.argv) < 4:
            print("ERROR: --field-path requires a path argument")
            sys.exit(1)

        field_path = sys.argv[3]
        if validate_field_path(json_file, field_path):
            sys.exit(0)
        else:
            sys.exit(1)

    # Load schema once
    schema = load_schema_template()
    if schema is None:
        print("ERROR: Could not load schema template")
        sys.exit(1)

    # Run full validation
    if validate_json_structure(json_file, schema):
        print(f"\nAll validations passed for {json_file}")
        sys.exit(0)
    else:
        print(f"\nValidation failed for {json_file}")
        sys.exit(1)


if __name__ == "__main__":
    main()
