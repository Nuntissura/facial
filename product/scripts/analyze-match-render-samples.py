#!/usr/bin/env python3
"""Validate one bounded WP-087 raw render sample stream and compute its gates.

Input is UTF-8 JSONL: one `record_type: run` header, `record_type: frame`
objects, and a completed terminal `end` record. Hashing covers exact bytes, including
line endings. The producer must stop at the declared measurement end and must
fail rather than drop records when its configured bounded sink fills.
"""

from __future__ import annotations

import argparse
import bisect
import hashlib
import json
import math
import re
import sys
from pathlib import Path


SCHEMA_VERSION = 1
WINDOW_US = 2_000_000
CADENCE_US = 250_000
MIN_WINDOW_SAMPLES = 60
MIN_TOTAL_SAMPLES = 7_200
MIN_DURATION_US = 120_000_000
MAX_DURATION_US = 120_000_000
DEFAULT_MAX_SAMPLES = 100_000
HARD_MAX_SAMPLES = 100_000
MAX_INPUT_BYTES = 20 * 1024 * 1024
MAX_LINE_BYTES = 16 * 1024

HEADER_FIELDS = (
    "schema_version",
    "record_type",
    "run_id",
    "state",
    "measurement_start_us",
    "measurement_end_us",
    "warmup_seconds",
    "package_sha256",
    "fixture_generation",
    "cache_state",
    "input_script_sha256",
    "hardware_manifest_sha256",
    "display_profile_sha256",
    "power_mode",
    "metric_scope",
    "admission_counts",
    "app_version", "git_commit", "cargo_lock_sha256", "model_generation",
    "schema_generation", "timestamp_scope", "admission_evidence",
)
MEDIA_STATES = ("media_labels_baseline", "media_labels_candidate")
MEDIA_FIXTURE_FIELDS = {"fixture_sha256", "rows", "assignment", "build_ui_sha256", "build_lib_sha256", "build_collector_sha256"}
MEDIA_RUNTIME_FIELDS = {"match_workers", "model_loads", "match_index_queries", "match_database_requests", "visible_tile_lookups", "visible_tile_lookups_min", "visible_tile_lookups_max", "visible_work_frames", "display_observations", "display_valid", "input_valid", "viewport_physical_px", "native_pixels_per_point", "egui_pixels_per_point", "font_size_pt", "font_family", "fixture_sha256"}
MEDIA_FIXTURE_SHA256 = hashlib.sha256(json.dumps([f"label-pool-{index:05}.png" for index in range(50_000)], separators=(",", ":")).encode("utf-8")).hexdigest()


class InputError(Exception):
    pass


def fail(message: str) -> None:
    raise InputError(message)


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            fail(f"duplicate JSON field: {key}")
        result[key] = value
    return result


def integer(value: object, field: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        fail(f"{field} must be an integer")
    return value


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while block := source.read(1024 * 1024):
            digest.update(block)
    return digest.hexdigest()


def nearest_rank(values: list[int], percentile: float) -> int:
    if not values:
        fail("cannot compute a percentile from an empty sample set")
    ordered = sorted(values)
    index = math.ceil(percentile * len(ordered)) - 1
    return ordered[max(0, index)]


def summarize(values: list[int]) -> dict[str, int]:
    return {
        "sample_count": len(values),
        "p50_us": nearest_rank(values, 0.50),
        "p95_us": nearest_rank(values, 0.95),
        "p99_us": nearest_rank(values, 0.99),
        "max_us": max(values),
    }


def read_stream(path: Path, max_samples: int) -> tuple[dict[str, object], list[int], list[int], str, dict | None]:
    with path.open("rb") as source:
        raw_stream = source.read(MAX_INPUT_BYTES + 1)
    if len(raw_stream) > MAX_INPUT_BYTES:
        fail(f"input exceeds bounded {MAX_INPUT_BYTES}-byte limit")

    frames_at: list[int] = []
    durations: list[int] = []
    header: dict[str, object] | None = None
    last_timestamp: int | None = None
    ended = False
    runtime_evidence = None
    for line_number, raw in enumerate(raw_stream.splitlines(keepends=True), start=1):
        if len(raw) > MAX_LINE_BYTES:
            fail(f"line {line_number} exceeds {MAX_LINE_BYTES}-byte limit")
        if not raw.strip():
            fail(f"blank line at {line_number} is not allowed")
        try:
            record = json.loads(raw.decode("utf-8"), object_pairs_hook=unique_object)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            fail(f"invalid UTF-8 JSON at line {line_number}: {error}")
        if not isinstance(record, dict):
            fail(f"line {line_number} must be a JSON object")

        if header is None:
            missing = [field for field in HEADER_FIELDS if field not in record]
            if missing:
                fail(f"run header missing fields: {', '.join(missing)}")
            if record["record_type"] != "run" or integer(record["schema_version"], "schema_version") != SCHEMA_VERSION:
                fail("first record must be a schema-version-1 run header")
            if record["state"] not in {"match_disabled", "active_quiet", "typical_face_edit", "pathological_1000_face_edit", *MEDIA_STATES}:
                fail("run header has an unsupported render state")
            for field in ("run_id", "package_sha256", "fixture_generation", "cache_state", "input_script_sha256", "hardware_manifest_sha256", "display_profile_sha256", "power_mode", "app_version", "git_commit", "cargo_lock_sha256", "model_generation", "schema_generation", "timestamp_scope"):
                if not isinstance(record[field], str) or not record[field].strip():
                    fail(f"run header {field} must be a non-empty string")
            for field in ("package_sha256", "input_script_sha256", "hardware_manifest_sha256", "display_profile_sha256", "cargo_lock_sha256"):
                if not re.fullmatch(r"[0-9a-f]{64}", record[field]):
                    fail(f"run header {field} must be lowercase SHA-256 hex")
            if not re.fullmatch(r"(?:[0-9a-fA-F]{40}|[0-9a-fA-F]{64})", record["git_commit"]):
                fail("git_commit must be a full hexadecimal Git object ID")
            if not re.fullmatch(r"[A-Za-z0-9_-]{1,96}", record["run_id"]):
                fail("run_id must be bounded ASCII without path separators")
            if record["timestamp_scope"] != "previous_frame_cpu_usage_observed_at_next_update_monotonic_since_capture_start":
                fail("timestamp_scope does not match the versioned producer contract")
            evidence = record["admission_evidence"]
            if evidence is not None:
                if not isinstance(evidence, dict) or set(evidence) != {"path", "sha256"}:
                    fail("admission_evidence must contain path and sha256")
                if not isinstance(evidence["path"], str) or not evidence["path"] or len(evidence["path"]) > 4096:
                    fail("admission_evidence path must be bounded nonempty text")
                if not isinstance(evidence["sha256"], str) or not re.fullmatch(r"[0-9a-f]{64}", evidence["sha256"]):
                    fail("admission_evidence sha256 is invalid")
            expected_header_fields = set(HEADER_FIELDS)
            if record["state"] in MEDIA_STATES:
                expected_header_fields.add("media_labels_fixture")
                fixture = record.get("media_labels_fixture")
                if not isinstance(fixture, dict) or set(fixture) != MEDIA_FIXTURE_FIELDS:
                    fail("Media labels fixture has missing or unknown fields")
                for field in MEDIA_FIXTURE_FIELDS - {"rows", "assignment"}:
                    if not isinstance(fixture[field], str) or not re.fullmatch(r"[0-9a-f]{64}", fixture[field]):
                        fail(f"Media labels fixture {field} must be lowercase SHA-256")
                if integer(fixture["rows"], "fixture rows") != 50_000:
                    fail("Media labels fixture requires exactly 50000 rows")
                if fixture["fixture_sha256"] != MEDIA_FIXTURE_SHA256:
                    fail("Media labels fixture hash differs from the exact relative-name manifest")
                assignment = "empty" if record["state"] == MEDIA_STATES[0] else "five_ordered"
                if fixture["assignment"] != assignment:
                    fail("Media labels assignment contradicts run state")
                if record["metric_scope"] != "eframe_update_render_cpu_time_excluding_vsync":
                    fail("Media labels requires native eframe backend render samples")
            if set(record) != expected_header_fields:
                fail("run header has unknown fields; update the versioned producer contract before extending it")
            admission = record["admission_counts"]
            if admission is not None:
                if not isinstance(admission, dict) or set(admission) != {"match_workers", "model_loads", "match_index_queries"}:
                    fail("admission_counts must name match_workers, model_loads, and match_index_queries")
                for field, value in admission.items():
                    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
                        fail(f"admission_counts.{field} must be a non-negative integer")
            if record["state"] in {"match_disabled", *MEDIA_STATES} and admission is not None and any(admission.values()):
                fail("match_disabled baseline admitted a Match worker, model load, or index query")
            if record["metric_scope"] not in {"presentation_frame_wall_clock", "eframe_update_render_cpu_time_excluding_vsync"}:
                fail("metric_scope must identify presentation_frame_wall_clock or eframe_update_render_cpu_time_excluding_vsync")
            start = integer(record["measurement_start_us"], "measurement_start_us")
            end = integer(record["measurement_end_us"], "measurement_end_us")
            duration = end - start
            if start < 0 or duration != MAX_DURATION_US:
                fail("measurement bounds must describe exactly 120 seconds on the monotonic clock")
            if integer(record["warmup_seconds"], "warmup_seconds") != 30:
                fail("render warmup must be exactly 30 seconds")
            record["git_commit"] = record["git_commit"].lower()
            header = record
            continue

        if ended:
            fail("records after the terminal end record are forbidden")
        if record.get("record_type") == "end":
            expected_end_fields = {"record_type", "outcome", "sample_count", "observed_at_us"}
            if header["state"] in MEDIA_STATES:
                expected_end_fields.add("runtime_evidence")
            if set(record) != expected_end_fields:
                fail("terminal end record has missing or unknown fields")
            if record["outcome"] != "completed":
                fail("capture did not complete: " + str(record["outcome"]))
            if integer(record["sample_count"], "end sample_count") != len(durations):
                fail("terminal sample count does not reconcile")
            if integer(record["observed_at_us"], "end observed_at_us") < header["measurement_end_us"]:
                fail("capture terminated before the declared measurement ended")
            if header["state"] in MEDIA_STATES:
                runtime_evidence = record["runtime_evidence"]
                validate_media_runtime(runtime_evidence, header, len(durations))
            ended = True
            continue
        if record.get("record_type") != "frame":
            fail(f"line {line_number} must be a frame record")
        if set(record) != {"record_type", "frame_end_timestamp_us", "frame_duration_us"}:
            fail(f"line {line_number} frame record has missing or unknown fields")
        timestamp = integer(record["frame_end_timestamp_us"], f"line {line_number} frame_end_timestamp_us")
        frame_duration = integer(record["frame_duration_us"], f"line {line_number} frame_duration_us")
        if frame_duration <= 0:
            fail(f"line {line_number} frame_duration_us must be positive")
        if last_timestamp is not None and timestamp <= last_timestamp:
            fail(f"frame timestamps are not monotonic at line {line_number}")
        if not header["measurement_start_us"] <= timestamp < header["measurement_end_us"]:
            fail(f"frame timestamp at line {line_number} is outside the half-open measurement interval")
        if len(frames_at) >= max_samples:
            fail(f"sample sink exceeded explicit bound of {max_samples}; run is invalid and samples may not be dropped")
        frames_at.append(timestamp)
        durations.append(frame_duration)
        last_timestamp = timestamp
    if header is None:
        fail("input contains no run header")
    if not ended:
        fail("capture has no completed terminal end record")
    return header, frames_at, durations, hashlib.sha256(raw_stream).hexdigest(), runtime_evidence


def validate_media_runtime(evidence, header, sample_count):
    if not isinstance(evidence, dict) or set(evidence) != MEDIA_RUNTIME_FIELDS:
        fail("Media labels terminal runtime evidence has missing or unknown fields")
    for field in ("match_workers", "model_loads", "match_index_queries", "match_database_requests"):
        if integer(evidence[field], field) != 0:
            fail(f"Media labels runtime admitted {field}")
    if integer(evidence["visible_tile_lookups"], "visible_tile_lookups") <= 0:
        fail("Media labels runtime painted no visible label tiles")
    minimum = integer(evidence["visible_tile_lookups_min"], "visible_tile_lookups_min")
    maximum = integer(evidence["visible_tile_lookups_max"], "visible_tile_lookups_max")
    work_frames = integer(evidence["visible_work_frames"], "visible_work_frames")
    if minimum <= 0 or maximum != minimum or work_frames != sample_count or evidence["visible_tile_lookups"] != work_frames * minimum:
        fail("Media labels measured-frame work does not reconcile exactly")
    if integer(evidence["display_observations"], "display_observations") < sample_count:
        fail("Media labels display evidence does not cover every sample")
    if evidence["display_valid"] is not True or evidence["viewport_physical_px"] != [1920, 1080]:
        fail("Media labels runtime display is not the reference viewport")
    if evidence["input_valid"] is not True:
        fail("Media labels runtime input changed the fixed no-pointer workload")
    for field, expected in (("native_pixels_per_point", 1.0), ("egui_pixels_per_point", 1.0), ("font_size_pt", 19.0)):
        if isinstance(evidence[field], bool) or evidence[field] != expected:
            fail(f"Media labels runtime {field} differs from reference")
    if evidence["font_family"] != "Inter":
        fail("Media labels runtime font family differs from reference")
    if evidence["fixture_sha256"] != header["media_labels_fixture"]["fixture_sha256"]:
        fail("Media labels terminal fixture differs from captured fixture")


def analyze(path: Path, max_samples: int, gate_profile: str) -> dict[str, object]:
    header, timestamps, durations, raw_sha256, runtime_evidence = read_stream(path, max_samples)
    required_profile = "pathological_1000" if header["state"] == "pathological_1000_face_edit" else "normal_typical"
    if gate_profile != required_profile:
        fail("gate profile does not match the declared render state")
    if len(durations) < MIN_TOTAL_SAMPLES:
        fail(f"render run has {len(durations)} samples; at least {MIN_TOTAL_SAMPLES} are required")

    start = int(header["measurement_start_us"])
    end = int(header["measurement_end_us"])
    last_start = end - WINDOW_US
    starts = list(range(start, last_start + 1, CADENCE_US))
    windows: list[dict[str, int]] = []
    for window_start in starts:
        left = bisect.bisect_left(timestamps, window_start)
        right = bisect.bisect_left(timestamps, window_start + WINDOW_US)
        window_values = durations[left:right]
        if len(window_values) < MIN_WINDOW_SAMPLES:
            fail(f"required window [{window_start},{window_start + WINDOW_US}) has only {len(window_values)} samples")
        windows.append({
            "start_us": window_start,
            "end_us": window_start + WINDOW_US,
            **summarize(window_values),
        })

    worst_p95 = max(windows, key=lambda row: (row["p95_us"], row["start_us"]))
    worst_p99 = max(windows, key=lambda row: (row["p99_us"], row["start_us"]))
    worst_max = max(windows, key=lambda row: (row["max_us"], row["start_us"]))
    whole_run = summarize(durations)

    p95_gate = 16_700 if gate_profile == "normal_typical" else 33_300
    numeric_verdict = "pass" if worst_p95["p95_us"] <= p95_gate and worst_p99["p99_us"] <= 33_300 else "fail"
    metric_scope = header["metric_scope"]
    verdict = numeric_verdict
    return {
        "schema_version": 1,
        "analyzer": "wp087-match-render-samples-v1",
        "source_path": str(path),
        "source_sha256": raw_sha256,
        "run_header": header,
        "runtime_evidence": runtime_evidence,
        "gate_profile": gate_profile,
        "admission_verdict": "unverified_requires_independent_post_run_full_interval_evidence",
        "admission_note": "header counts and optional preexisting evidence are declared inputs, not observed runtime admission telemetry; render analysis alone does not pass the disabled-baseline or release gates",
        "metric_scope": metric_scope,
        "metric_scope_note": "records App::update plus backend rendering when using eframe_update_render_cpu_time_excluding_vsync; does not include vsync wait or claim physical display latency",
        "numeric_verdict": numeric_verdict,
        "measurement_start_us": start,
        "measurement_end_us": end,
        **whole_run,
        "raw_sample_records_sha256": raw_sha256,
        "window_ms": 2000,
        "window_cadence_ms": 250,
        "window_min_samples": MIN_WINDOW_SAMPLES,
        "required_window_count": len(starts),
        "evaluated_window_count": len(windows),
        "worst_window_p50_us": max(row["p50_us"] for row in windows),
        "worst_window_p95_us": worst_p95["p95_us"],
        "worst_window_p99_us": worst_p99["p99_us"],
        "worst_window_max_us": worst_max["max_us"],
        "worst_window_start_us": worst_p95["start_us"],
        "worst_window_end_us": worst_p95["end_us"],
        "worst_p99_window_start_us": worst_p99["start_us"],
        "worst_max_window_start_us": worst_max["start_us"],
        "p95_limit_us": p95_gate,
        "p99_limit_us": 33_300,
        "rolling_verdict": verdict,
        "window_worst_p95": worst_p95,
        "window_worst_p99": worst_p99,
        "window_worst_max": worst_max,
    }


def analyze_ab_manifest(path: Path, max_samples: int) -> dict[str, object]:
    try:
        with path.open("rb") as source:
            manifest_bytes = source.read(65537)
        if len(manifest_bytes) > 65536:
            fail("A/B manifest exceeds 64 KiB")
        manifest = json.loads(manifest_bytes.decode("utf-8"), object_pairs_hook=unique_object)
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        fail(f"cannot read A/B manifest: {error}")
    if not isinstance(manifest, dict) or set(manifest) != {"schema_version", "runs"} or integer(manifest["schema_version"], "schema_version") != 1:
        fail("A/B manifest must contain only schema_version=1 and runs")
    runs = manifest["runs"]
    expected_states = ["match_disabled", "active_quiet", "active_quiet", "match_disabled"]
    if not isinstance(runs, list) or len(runs) != len(expected_states):
        fail("A/B manifest must list exactly four ordered runs")
    results: list[dict[str, object]] = []
    for index, (run, expected_state) in enumerate(zip(runs, expected_states), start=1):
        if not isinstance(run, dict) or set(run) != {"state", "path"} or run["state"] != expected_state or not isinstance(run["path"], str):
            fail(f"A/B run {index} must be state={expected_state} with a path")
        sample_path = (path.parent / run["path"]).resolve()
        result = analyze(sample_path, max_samples, "normal_typical")
        if result["run_header"]["state"] != expected_state:
            fail(f"A/B run {index} header state does not match the manifest")
        if expected_state == "match_disabled" and result["run_header"]["admission_counts"] is None:
            fail("disabled A/B eligibility is unverified: missing full-interval admission counts")
        results.append(result)

    if len({result["run_header"]["run_id"] for result in results}) != 4:
        fail("A/B runs require four distinct run IDs")
    if len({result["source_path"] for result in results}) != 4:
        fail("A/B runs require four independently captured streams")
    invariant_fields = (
        "app_version", "git_commit", "cargo_lock_sha256", "model_generation", "schema_generation", "timestamp_scope",
        "package_sha256",
        "fixture_generation",
        "cache_state",
        "input_script_sha256",
        "hardware_manifest_sha256",
        "display_profile_sha256",
        "power_mode",
        "metric_scope",
    )
    headers = [result["run_header"] for result in results]
    baseline = headers[0]
    mismatches = {
        field: [header[field] for header in headers]
        for field in invariant_fields
        if any(header[field] != baseline[field] for header in headers[1:])
    }
    if mismatches:
        fail(f"A/B invariant mismatch invalidates comparison: {json.dumps(mismatches, sort_keys=True)}")
    return {
        "schema_version": 1,
        "analyzer": "wp087-match-render-samples-v1",
        "manifest_path": str(path),
        "manifest_sha256": hashlib.sha256(manifest_bytes).hexdigest(),
        "render_ab_order": expected_states,
        "invariant_fields": list(invariant_fields),
        "invariant_verdict": "pass",
        "runs": results,
        "render_verdict": "pass" if all(result["rolling_verdict"] == "pass" for result in results) else "fail",
        "overall_verdict": "pending_independent_admission_proof" if all(result["rolling_verdict"] == "pass" for result in results) else "fail",
        "admission_verdict": "unverified_requires_independent_post_run_full_interval_evidence",
    }


def bounded_json(path: Path, label: str):
    with path.open("rb") as source:
        raw = source.read(65_537)
    if len(raw) > 65_536:
        fail(f"{label} exceeds 64 KiB")
    try:
        return json.loads(raw.decode("utf-8"), object_pairs_hook=unique_object)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        fail(f"invalid {label}: {error}")


def analyze_media_label_ab_manifest(path: Path, max_samples: int) -> dict:
    manifest = bounded_json(path, "Media labels A/B manifest")
    artifact_fields = {"portable_executable_path": "package_sha256", "hardware_manifest_path": "hardware_manifest_sha256", "display_profile_path": "display_profile_sha256", "input_script_path": "input_script_sha256"}
    if not isinstance(manifest, dict) or set(manifest) != {"schema_version", "runs", *artifact_fields} or integer(manifest["schema_version"], "schema_version") != 1:
        fail("Media labels manifest requires schema_version, runs and four evidence artifact paths")
    artifacts = {}
    for field in artifact_fields:
        value = manifest[field]
        if not isinstance(value, str) or not value.strip():
            fail(f"{field} must be a nonempty path")
        artifact = (path.parent / value).resolve()
        if not artifact.is_file():
            fail(f"{field} must name an existing regular file")
        artifacts[field] = artifact
    display = bounded_json(artifacts["display_profile_path"], "reference display profile")
    expected_display = {"viewport_physical_px": [1920, 1080], "dpi_scale_percent": 100, "egui_pixels_per_point": 1.0, "font_family": "Inter", "font_size_pt": 19}
    if display != expected_display:
        fail("display profile must equal the WP087 reference display")
    hardware = bounded_json(artifacts["hardware_manifest_path"], "reference hardware manifest")
    hardware_fields = {"cpu_model", "physical_cores", "logical_cores", "ram_bytes", "gpu_model", "gpu_driver", "inference_backend", "os_edition", "os_build", "power_mode", "storage_kind", "media_root_kind", "network_link"}
    if not isinstance(hardware, dict) or set(hardware) != hardware_fields:
        fail("hardware manifest has missing or unknown reference fields")
    for field in hardware_fields:
        if field in {"physical_cores", "logical_cores", "ram_bytes"}:
            if integer(hardware[field], field) <= 0:
                fail(f"hardware {field} must be positive")
        elif not isinstance(hardware[field], str) or not hardware[field].strip():
            fail(f"hardware {field} must be observed nonempty text")
    expected_states = [MEDIA_STATES[0], MEDIA_STATES[1], MEDIA_STATES[1], MEDIA_STATES[0]]
    if not isinstance(manifest["runs"], list) or len(manifest["runs"]) != 4:
        fail("Media labels A/B requires four ordered runs")
    results, lane_values, headers = [], {MEDIA_STATES[0]: [], MEDIA_STATES[1]: []}, []
    for index, (run, state) in enumerate(zip(manifest["runs"], expected_states)):
        if not isinstance(run, dict) or set(run) != {"state", "path"} or run["state"] != state or not isinstance(run["path"], str) or not run["path"]:
            fail(f"Media labels A/B run {index} must declare {state} and a path")
        sample_path = (path.parent / run["path"]).resolve()
        result = analyze(sample_path, max_samples, "normal_typical")
        header = result["run_header"]
        if header["state"] != state:
            fail("Media labels stream state contradicts ABBA manifest")
        for field, header_field in artifact_fields.items():
            if sha256_file(artifacts[field]) != header[header_field]:
                fail(f"Media labels {header_field} does not match the actual evidence file")
        if hardware["power_mode"] != header["power_mode"]:
            fail("Media labels power mode differs from reference hardware manifest")
        if header["model_generation"] != "unconfigured":
            fail("Media labels baseline must have unconfigured Match models")
        results.append(result)
        headers.append(header)
        reread = read_stream(sample_path, max_samples)
        if reread[3] != result["source_sha256"]:
            fail("Media labels raw stream changed during analysis")
        lane_values[state].extend(reread[2])
    if len({row["source_path"] for row in results}) != 4 or len({row["run_id"] for row in headers}) != 4:
        fail("Media labels ABBA requires four distinct run IDs and captured streams")
    invariants = set(HEADER_FIELDS) - {"run_id", "state", "measurement_start_us", "measurement_end_us", "admission_counts", "admission_evidence"}
    for field in invariants:
        if any(row[field] != headers[0][field] for row in headers[1:]):
            fail(f"Media labels ABBA invariant mismatch: {field}")
    for field in MEDIA_FIXTURE_FIELDS - {"assignment"}:
        if any(row["media_labels_fixture"][field] != headers[0]["media_labels_fixture"][field] for row in headers[1:]):
            fail(f"Media labels fixture invariant mismatch: {field}")
    baseline = summarize(lane_values[MEDIA_STATES[0]])
    candidate = summarize(lane_values[MEDIA_STATES[1]])
    relative_pass = all(candidate[field] * 100 <= baseline[field] * 110 for field in ("p50_us", "p95_us"))
    absolute_pass = candidate["p95_us"] < 16_700
    visible_lookups = [row["runtime_evidence"]["visible_tile_lookups_min"] for row in results]
    visible_work_pass = len(set(visible_lookups)) == 1
    rolling_pass = all(row["rolling_verdict"] == "pass" for row in results)
    passed = relative_pass and absolute_pass and visible_work_pass and rolling_pass
    return {"schema_version": 1, "analyzer": "wp087-native-media-label-ab-v1", "manifest_path": str(path), "manifest_sha256": sha256_file(path), "measurement_order": expected_states, "runs": results, "baseline": baseline, "candidate": candidate, "delta_budget_percent": 10, "candidate_p95_budget_us": 16_700, "passes_delta_budget": relative_pass, "passes_absolute_budget": absolute_pass, "passes_comparable_visible_work": visible_work_pass, "rolling_verdict": "pass" if rolling_pass else "fail", "overall_verdict": "pass" if passed else "fail", "runtime_admission_verdict": "observed_zero_at_process_boundaries", "release_verdict": "pending_independent_package_and_runtime_evidence_review"}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--input", type=Path, help="one bounded raw render JSONL stream")
    source.add_argument("--ab-manifest", type=Path, help="JSON manifest listing disabled/quiet/quiet/disabled JSONL streams")
    source.add_argument("--media-label-ab-manifest", type=Path, help="native Media empty/five/five/empty captures with evidence artifact paths")
    parser.add_argument("--output", required=True, type=Path, help="result JSON path")
    parser.add_argument("--gate-profile", choices=("normal_typical", "pathological_1000"))
    parser.add_argument("--max-samples", type=int, default=DEFAULT_MAX_SAMPLES)
    args = parser.parse_args()
    if not 1 <= args.max_samples <= HARD_MAX_SAMPLES:
        parser.error(f"--max-samples must be between 1 and {HARD_MAX_SAMPLES}")
    try:
        if args.input is not None:
            if args.gate_profile is None:
                parser.error("--gate-profile is required with --input")
            if not args.input.is_file():
                fail("input path must be an existing regular file")
            result = analyze(args.input, args.max_samples, args.gate_profile)
            verdict = result["rolling_verdict"]
        elif args.media_label_ab_manifest is not None:
            if args.gate_profile is not None:
                parser.error("--gate-profile applies only to --input")
            result = analyze_media_label_ab_manifest(args.media_label_ab_manifest, args.max_samples)
            verdict = result["overall_verdict"]
        else:
            if args.gate_profile is not None:
                parser.error("--gate-profile applies only to --input")
            if not args.ab_manifest.is_file():
                fail("A/B manifest path must be an existing regular file")
            result = analyze_ab_manifest(args.ab_manifest, args.max_samples)
            verdict = result["overall_verdict"]
        args.output.parent.mkdir(parents=True, exist_ok=True)
        temporary = args.output.with_suffix(args.output.suffix + ".tmp")
        temporary.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
        temporary.replace(args.output)
        print(json.dumps({"output": str(args.output), "verdict": verdict}))
        return 0 if verdict == "pass" else 1
    except (InputError, OSError) as error:
        print(f"wp087 benchmark input invalid: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
