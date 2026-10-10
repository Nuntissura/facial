#!/usr/bin/env python3
"""Collect and analyze bounded WP-087 non-render Match measurements.

This tool reports caller-observed CLI-to-terminal-receipt latency separately
from endpoint-reported UI time. It does not measure physical presentation
latency. Settings Manage people and cached autocomplete are invoked only through
their exact new receipt-backed routes; if the packaged executable does not
provide those commands, collection records an invalid run. It never substitutes
MediaSearch. Concurrency/resource acceptance needs instrumented runtime
telemetry; unavailable fields never become zero.
"""

from __future__ import annotations

import argparse
import functools
import hashlib
import json
import os
import re
import stat
import subprocess
import sys
import time
import uuid
from pathlib import Path
from typing import Any

SCHEMA_VERSION = 1
MAX_CONFIG_BYTES = 64 * 1024
MAX_LINE_BYTES = 16 * 1024
# Pretty diagnostics contain three 256-sample rings (four u64 values each),
# 200 redacted recent jobs, and fixed catalog/governor/receipt metadata. These
# conservative per-row allowances bound input before allocating or parsing it.
# Pretty JSON: three latency rings, two larger raw activity rings, 200 jobs,
# and bounded fixed metadata. Raw events are extracted into separate JSONL lines.
MAX_DIAGNOSTICS_RECEIPT_BYTES = 3 * 256 * 512 + 2 * 256 * 2048 + 200 * 2048 + 128 * 1024
MAX_FILE_BYTES = 20 * 1024 * 1024
MAX_RECORDS = 100_000
RUN_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$")
SHA256 = re.compile(r"^[0-9a-f]{64}$")
ACTION_ID = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$")
REQUIRED_INTERACTION_ENDPOINTS = {
    "settings_manage_people_acknowledgement",
    "match_people_10000_open",
    "cached_autocomplete",
    "pause_route_feedback_and_safe_unit",
    "operator_pause_feedback",
}
SUPPORTED_ACTIONS = {
    "match_people_10000_open": ("match_intent", ["match_intent", "--action", "open_people", "--offset", "0"], "open_people"),
    "settings_manage_people_acknowledgement": ("match_settings_manage_people", ["match_settings_manage_people"], "settings_manage_people_acknowledgement"),
    "settings_context_reset": ("match_intent", ["match_intent", "--action", "open_settings", "--offset", "0"], "open_settings"),
    "pause_context_reset": ("match_intent", ["match_intent", "--action", "resume_all"], "resume_all"),
    "operator_pause_feedback": ("match_intent", ["match_intent", "--action", "pause_all"], "pause_all"),
}
CONCURRENCY_FIELDS = (
    "indexing_progress", "indexing_progress_scope", "index_stage", "index_stage_counts", "index_stage_scope",
    "active_holds", "admitted_resources", "lease_deltas",
)
VISIBLE_WORK_SCOPES = {
    "thumbnail": "visible_priority_request_to_texture_painted_excluding_backend_and_vsync",
    "navigation": "grid_navigation_to_target_tile_painted_excluding_backend_and_vsync",
    "playback_seek": "native_seek_request_to_raw_libvlc_clock_confirmation_excluding_presentation",
}
VISIBLE_SERIES_FIELDS = {
    "lifetime_id", "endpoint_scope", "captured_at_us", "sequence", "dropped_records",
    "abandoned", "overflow", "pending", "samples",
}
VISIBLE_SAMPLE_FIELDS = {"sequence", "start_us", "end_us", "duration_us"}
VISIBLE_CHECKPOINT_FIELDS = {
    "record_type", "phase", "timestamp_us", "endpoint", "lifetime_id", "endpoint_scope",
    "captured_at_us", "sequence", "dropped_records", "abandoned", "overflow", "pending",
}
VISIBLE_EVENT_FIELDS = {
    "record_type", "phase", "observed_at_us", "endpoint", "lifetime_id",
    "sequence", "start_us", "end_us", "duration_us",
}
RESOURCE_AXES = (
    "admitted_items", "aggregate_queue_items", "aggregate_queue_bytes",
    "cpu_inference_concurrency", "decoded_image_bytes", "gpu_vram_bytes_if_enabled",
    "surrealdb_write_concurrency", "vector_index_build_concurrency",
)
TELEMETRY_AXES = (
    "admitted_items", "queued_items", "queued_bytes", "cpu_inference", "decoded_bytes",
    "gpu_vram_bytes", "worker_memory_bytes", "surreal_writes", "vector_index_builds",
)
TELEMETRY_COUNTERS = ("acquisitions", "replacements", "releases", "preparation_releases", "pressure_events")
TELEMETRY_FIELDS = {"lifetime_id", "scope", "current_usage", "peak_usage", *TELEMETRY_COUNTERS, "overflow"}
TELEMETRY_BUDGET_FIELDS = set(TELEMETRY_AXES)
RUNTIME_TIMESTAMP_SCOPE = "monotonic_us_since_process_runtime_epoch"
RUNTIME_RING_SCOPES = {
    "lease_activity": "admitted_resource_leases_excluding_kernel_execution",
    "native_playback": "raw_libvlc_state_and_clock_excluding_presentation",
}
RUNTIME_RING_SCOPES_V2 = {**RUNTIME_RING_SCOPES,
    "worker_control": "parent_fenced_transport_external_playback_fullscreen_CAS_and_owned_exit_observer_calls_excluding_exact_kernel_timing_operator_pause_and_unobserved_raw_job_reaper"}
def runtime_scopes(version):
    if type(version) is not int or version not in (1, 2):
        raise Invalid("runtime evidence version must be 1 or 2")
    return RUNTIME_RING_SCOPES if version == 1 else RUNTIME_RING_SCOPES_V2

RUNTIME_RING_FIELDS = {"runtime_id", "lifetime_id", "endpoint_scope", "captured_at_us",
                       "sequence", "dropped_records", "overflow", "samples"}
RUNTIME_INTERVAL_FIELDS = {"scope", "runtime_id", "lifetime_id", "sequence", "start_us", "end_us",
                           "opening_usage", "closing_usage", "peak_usage", *TELEMETRY_COUNTERS, "overflow",
                           "opening_live_stage_leases", "closing_live_stage_leases",
                           "opening_unclassified_leases", "closing_unclassified_leases"}
RUNTIME_SAMPLE_FIELDS = {
    "worker_control": {"sequence", "timestamp_us", "event", "worker_id", "operation_id", "operation",
                       "fence_sha256", "admission_epoch", "previous_epoch", "transition_start_us", "parent_request_start_us"},
    "lease_activity": {"sequence", "timestamp_us", "event", "usage", "live_stage_leases", "unclassified_leases"},
    "native_playback": {"sequence", "timestamp_us", "poll_start_us", "poll_end_us", "status",
                        "native_player_present", "native_playing", "clock_available", "time_ms",
                        "player_generation", "generation_overflow"},
}
GOVERNOR_EVIDENCE_FIELDS = {
    "record_type", "measurement_start_us", "measurement_end_us", "resource_budget",
    "baseline_resource_telemetry", "terminal_resource_telemetry",
}
INTERACTION_FIELDS = {
    "record_type", "phase", "ordinal", "endpoint", "call_start_timestamp_us",
    "call_end_timestamp_us", "duration_us", "endpoint_outcome", "receipt_action_id",
    "context_setup_action_id", "context_setup_timestamp_us",
    "reported_endpoint_duration_us", "reported_endpoint_duration_scope",
    "reported_endpoint_current_state_confirmed", "reported_endpoint_rendered",
    "reported_endpoint_result_count", "reported_endpoint_query_present", "reported_endpoint_desired_mode",
}
CATALOG_EVIDENCE_FIELDS = {
    "total_people", "count_scope", "catalog_revision", "schema_generation", "store_session_id",
}
CATALOG_OBSERVATION_FIELDS = {
    "receipt_action_id", "call_start_timestamp_us", "call_end_timestamp_us",
    "fixture_manifest_sha256", "catalog_evidence",
}
CONTEXT_SETUP_FIELDS = {
    "record_type", "phase", "ordinal", "endpoint", "reset_endpoint", "timestamp_us",
    "setup_outcome", "receipt_action_id", "reported_endpoint_duration_scope",
    "reported_endpoint_current_state_confirmed", "reported_endpoint_rendered",
    "reported_endpoint_result_count", "reported_endpoint_query_present", "reported_endpoint_desired_mode",
    "requested_offset", "applied_offset", "page_limit",
}
INITIAL_SETTINGS_SETUP_FIELDS = CONTEXT_SETUP_FIELDS | {
    "call_start_timestamp_us", "call_end_timestamp_us", "duration_us", "reported_endpoint_duration_us",
}
CONCURRENCY_RECORD_FIELDS = {"record_type", "phase", "timestamp_us", *CONCURRENCY_FIELDS}


class Invalid(ValueError):
    pass


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def canonical_json_line(value: dict[str, Any]) -> bytes:
    data = json.dumps(value, sort_keys=True, separators=(",", ":"), allow_nan=False).encode("utf-8") + b"\n"
    if len(data) > MAX_LINE_BYTES:
        raise Invalid("record exceeds 16 KiB")
    return data


def reject_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise Invalid(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def strict_json(data: bytes) -> Any:
    try:
        return json.loads(data, object_pairs_hook=reject_duplicate_keys,
                          parse_constant=lambda value: (_ for _ in ()).throw(Invalid(f"invalid number {value}")))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise Invalid(f"invalid JSON: {error}") from error


def check_hash(value: Any, label: str) -> None:
    if not isinstance(value, str) or not SHA256.fullmatch(value):
        raise Invalid(f"{label} must be a lowercase SHA-256 hex digest")


def is_reparse(path: Path) -> bool:
    try:
        info = path.lstat()
    except FileNotFoundError:
        return False
    attributes = getattr(info, "st_file_attributes", 0)
    reparse_flag = getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0x400)
    return stat.S_ISLNK(info.st_mode) or bool(attributes & reparse_flag)


def ensure_output(workspace: Path, run_id: str) -> Path:
    if not RUN_ID.fullmatch(run_id):
        raise Invalid("run_id must be 1-64 safe ASCII letters, digits, underscore, or hyphen")
    workspace = workspace.resolve(strict=True)
    current = workspace
    for part in (".facial", "benchmarks"):
        current = current / part
        if is_reparse(current):
            raise Invalid(f"refusing reparse-point output parent: {current}")
        current.mkdir(exist_ok=True)
    current = current.resolve(strict=True)
    if not current.is_relative_to(workspace):
        raise Invalid("benchmark output escapes workspace")
    output = current / f"{run_id}.jsonl"
    if is_reparse(output):
        raise Invalid("refusing reparse-point output")
    return output


def write_new(path: Path, records: list[dict[str, Any]]) -> None:
    size = 0
    lines: list[bytes] = []
    if len(records) > MAX_RECORDS:
        raise Invalid("record count exceeds 100,000")
    for record in records:
        line = canonical_json_line(record)
        size += len(line)
        if size > MAX_FILE_BYTES:
            raise Invalid("capture exceeds 20 MiB")
        lines.append(line)
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    if hasattr(os, "O_BINARY"):
        flags |= os.O_BINARY
    fd = os.open(path, flags, 0o600)
    with os.fdopen(fd, "wb") as stream:
        for line in lines:
            stream.write(line)
        stream.flush()
        os.fsync(stream.fileno())


class CaptureWriter:
    """Create-new bounded streaming writer; a crash leaves an invalid prefix."""

    def __init__(self, path: Path):
        flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
        if hasattr(os, "O_BINARY"):
            flags |= os.O_BINARY
        self.stream = os.fdopen(os.open(path, flags, 0o600), "wb")
        self.size = 0
        self.count = 0

    def append(self, record: dict[str, Any]) -> None:
        line = canonical_json_line(record)
        if self.count >= MAX_RECORDS or self.size + len(line) > MAX_FILE_BYTES:
            raise Invalid("capture record/byte bound exceeded; run is invalid")
        self.stream.write(line)
        self.stream.flush()
        self.size += len(line)
        self.count += 1

    def close(self) -> None:
        self.stream.flush()
        os.fsync(self.stream.fileno())
        self.stream.close()


def load_records(path: Path) -> list[dict[str, Any]]:
    if is_reparse(path):
        raise Invalid("input must not be a reparse point")
    size = path.stat().st_size
    if size > MAX_FILE_BYTES:
        raise Invalid("input exceeds 20 MiB")
    records = []
    with path.open("rb") as stream:
        for number, line in enumerate(stream, 1):
            if len(line) > MAX_LINE_BYTES:
                raise Invalid(f"line {number} exceeds 16 KiB")
            if not line.endswith(b"\n"):
                raise Invalid(f"line {number} is incomplete")
            value = strict_json(line)
            if not isinstance(value, dict):
                raise Invalid(f"line {number} must be a JSON object")
            records.append(value)
            if len(records) > MAX_RECORDS:
                raise Invalid("record count exceeds 100,000")
    if not records:
        raise Invalid("empty capture")
    return records


def run_cli(cli: Path, argv: list[str], workspace: Path, timeout_s: float) -> tuple[int, dict[str, Any] | None, int, int]:
    start_ns = time.monotonic_ns()
    try:
        completed = subprocess.run(
            [str(cli), *argv], cwd=workspace,
            env={**os.environ, "FACIAL_WORKSPACE_ROOT": str(workspace)},
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            timeout=timeout_s, check=False, shell=False,
        )
    except subprocess.TimeoutExpired:
        end_ns = time.monotonic_ns()
        return 124, None, start_ns, end_ns
    end_ns = time.monotonic_ns()
    try:
        receipt = strict_json(completed.stdout)
    except Invalid:
        receipt = None
    return completed.returncode, receipt if isinstance(receipt, dict) else None, start_ns, end_ns


@functools.lru_cache(maxsize=1)
def windows_receipt_api() -> tuple[Any, Any]:
    import ctypes
    from ctypes import wintypes
    class FileInformation(ctypes.Structure):
        _fields_ = [("attributes", wintypes.DWORD), ("created", wintypes.FILETIME),
                    ("accessed", wintypes.FILETIME), ("written", wintypes.FILETIME),
                    ("volume", wintypes.DWORD), ("size_high", wintypes.DWORD), ("size_low", wintypes.DWORD),
                    ("links", wintypes.DWORD), ("index_high", wintypes.DWORD), ("index_low", wintypes.DWORD)]
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.CreateFileW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD, ctypes.c_void_p,
                                   wintypes.DWORD, wintypes.DWORD, wintypes.HANDLE]
    kernel.CreateFileW.restype = wintypes.HANDLE
    kernel.GetFileInformationByHandle.argtypes = [wintypes.HANDLE, ctypes.POINTER(FileInformation)]
    kernel.GetFileInformationByHandle.restype = wintypes.BOOL
    kernel.GetFileInformationByHandleEx.argtypes = [wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD]
    kernel.GetFileInformationByHandleEx.restype = wintypes.BOOL
    kernel.GetFinalPathNameByHandleW.argtypes = [wintypes.HANDLE, wintypes.LPWSTR, wintypes.DWORD, wintypes.DWORD]
    kernel.GetFinalPathNameByHandleW.restype = wintypes.DWORD
    kernel.GetFileType.argtypes = [wintypes.HANDLE]
    kernel.GetFileType.restype = wintypes.DWORD
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel.CloseHandle.restype = wintypes.BOOL
    return kernel, FileInformation


class ReceiptPublicationRace(OSError):
    """An owned deleted receipt handle must be closed and reopened, never read."""


def windows_receipt_handle_deleted(handle: Any) -> bool:
    import ctypes
    from ctypes import wintypes
    class StandardInfo(ctypes.Structure):
        _fields_ = [("allocation_size", ctypes.c_longlong), ("end_of_file", ctypes.c_longlong),
                    ("number_of_links", wintypes.DWORD), ("delete_pending", ctypes.c_ubyte),
                    ("directory", ctypes.c_ubyte)]
    kernel, _ = windows_receipt_api()
    standard = StandardInfo()
    # FileStandardInfo(1) reports deletion on this exact owned handle; a path
    # spelling alone never establishes a retryable publication race.
    # https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_standard_info
    if not kernel.GetFileInformationByHandleEx(handle, 1, ctypes.byref(standard), ctypes.sizeof(standard)):
        raise ctypes.WinError(ctypes.get_last_error())
    return bool(standard.delete_pending) and standard.number_of_links == 0 and not standard.directory


def normalized_windows_receipt_path(value: str) -> str:
    if value.startswith("\\\\?\\UNC\\"):
        value = "\\\\" + value[8:]
    elif value.startswith("\\\\?\\"):
        value = value[4:]
    return os.path.normcase(os.path.normpath(value))


def open_receipt_stream(path: Path) -> Any:
    if os.name != "nt":
        return path.open("rb")
    import ctypes
    import msvcrt
    kernel, Information = windows_receipt_api()
    expected_path = path.parent.resolve(strict=True) / path.name
    # CreateFileW sharing permits the receipt publisher's delete/rename. Opening
    # the reparse point itself allows handle-based rejection before any read.
    # https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew
    handle = kernel.CreateFileW(str(expected_path), 0x80000000, 0x1 | 0x2 | 0x4,
                                None, 3, 0x00200000 | 0x08000000, None)
    if handle == ctypes.c_void_p(-1).value:
        raise ctypes.WinError(ctypes.get_last_error())
    descriptor = None
    try:
        information = Information()
        if not kernel.GetFileInformationByHandle(handle, ctypes.byref(information)):
            raise ctypes.WinError(ctypes.get_last_error())
        if information.attributes & 0x400:
            raise Invalid("terminal receipt handle is a reparse point")
        if information.attributes & 0x10 or kernel.GetFileType(handle) != 1:
            raise Invalid("terminal receipt handle is not a regular disk file")
        final_path = ctypes.create_unicode_buffer(32768)
        length = kernel.GetFinalPathNameByHandleW(handle, final_path, len(final_path), 0)
        if length == 0:
            path_error = ctypes.get_last_error()
            if path_error == 1168 and windows_receipt_handle_deleted(handle):
                raise ReceiptPublicationRace("terminal receipt publication replaced a deleted handle")
            raise ctypes.WinError(path_error)
        if length >= len(final_path):
            raise Invalid("terminal receipt handle path exceeds Windows path bound")
        if normalized_windows_receipt_path(final_path.value) != normalized_windows_receipt_path(str(expected_path)):
            if windows_receipt_handle_deleted(handle):
                raise ReceiptPublicationRace("terminal receipt publication replaced a deleted handle")
            raise Invalid("terminal receipt handle escapes its canonical receipt path")
        # _open_osfhandle transfers ownership; closing the CRT descriptor closes
        # the Windows handle. Never CloseHandle after a successful transfer.
        # https://learn.microsoft.com/en-us/cpp/c-runtime-library/reference/open-osfhandle
        descriptor = msvcrt.open_osfhandle(handle, os.O_RDONLY | os.O_BINARY | os.O_NOINHERIT)
        handle = None
        stream = os.fdopen(descriptor, "rb")
        descriptor = None
        return stream
    finally:
        if descriptor is not None:
            os.close(descriptor)
        if handle is not None and not kernel.CloseHandle(handle):
            raise ctypes.WinError(ctypes.get_last_error())


def terminal_receipt(api_root: Path, action_id: str, deadline_ns: int, expected_kind: str,
                     max_bytes: int = MAX_LINE_BYTES) -> dict[str, Any] | None:
    if not isinstance(action_id, str) or not ACTION_ID.fullmatch(action_id):
        raise Invalid("terminal action_id must be a canonical lowercase UUID")
    receipt_path = api_root / "receipts" / f"{action_id}.json"
    while time.monotonic_ns() < deadline_ns:
        try:
            if is_reparse(receipt_path):
                raise Invalid("terminal receipt path is a reparse point")
            with open_receipt_stream(receipt_path) as stream:
                data = stream.read(max_bytes + 1)
        except OSError as error:
            if not isinstance(error, (FileNotFoundError, PermissionError, ReceiptPublicationRace)) and getattr(error, "winerror", None) not in {32, 33}:
                raise
            time.sleep(0.01)
            continue
        if len(data) > max_bytes:
            raise Invalid(f"terminal receipt exceeds {max_bytes}-byte bound")
        receipt = strict_json(data)
        if not isinstance(receipt, dict):
            raise Invalid("terminal receipt is not an object")
        if receipt.get("status") in {"applied", "rejected", "error", "ok"}:
            if receipt.get("action_id") != action_id:
                raise Invalid("terminal receipt action_id does not match the requested action")
            if receipt.get("kind") != expected_kind:
                raise Invalid("terminal receipt kind does not match the requested action")
            if time.monotonic_ns() >= deadline_ns:
                return None
            return receipt
        time.sleep(0.01)
    return None


def interaction_outcome(cli: Path, api_root: Path, workspace: Path, endpoint: str, timeout_s: float,
                        query: str | None = None, media_key: str | None = None,
                        catalog_revision: int | None = None, face_id: str | None = None) -> tuple[str, int, int, str | None, dict[str, Any] | None]:
    route = SUPPORTED_ACTIONS.get(endpoint)
    if endpoint == "cached_autocomplete":
        if not query or not media_key or catalog_revision is None:
            raise Invalid("cached_autocomplete requires exact query, media key, and expected catalog revision")
        if (query.strip() != query or len(query) > 4096 or any(ord(ch) < 32 for ch in query)
                or media_key.strip() != media_key or len(media_key) > 4096 or any(ord(ch) < 32 for ch in media_key)
                or catalog_revision < 1
                or (face_id is not None and (face_id.strip() != face_id or not face_id or len(face_id) > 4096 or any(ord(ch) < 32 for ch in face_id)))):
            raise Invalid("autocomplete query/media/revision/face fields must be canonical and bounded")
        route = ("match_editor_autocomplete", ["match_editor_autocomplete", "--query", query, "--id", media_key,
                 "--expected-revision", str(catalog_revision)] + (["--target-id", face_id] if face_id else []),
                 "cached_person_autocomplete")
    if endpoint == "pause_route_feedback_and_safe_unit":
        raise Invalid("pause/route/safe-unit endpoint lacks an implemented combined receipt and bounded checkpoint source")
    if route is None:
        raise Invalid(f"endpoint {endpoint!r} has no implemented receipt-backed route; refusing substitute")
    expected_kind, argv, expected_result_endpoint = route
    code, receipt, start_ns, end_ns = run_cli(cli, argv, workspace, timeout_s)
    if receipt is None:
        return ("cli_timeout" if code == 124 else "invalid_cli_receipt", start_ns, end_ns, None, None)
    action_id = receipt.get("action_id")
    if not isinstance(action_id, str) or not ACTION_ID.fullmatch(action_id):
        return "missing_action_id", start_ns, end_ns, None, None
    if receipt.get("kind") != expected_kind:
        return "cli_receipt_kind_mismatch", start_ns, end_ns, action_id, None
    if receipt.get("status") not in {"accepted", "applied"}:
        return str(receipt.get("status", "unknown")), start_ns, end_ns, action_id, None
    if receipt.get("status") == "accepted":
        final = terminal_receipt(api_root, action_id, start_ns + int(timeout_s * 1_000_000_000), expected_kind)
        end_ns = time.monotonic_ns()
        if final is None:
            return "terminal_receipt_timeout", start_ns, end_ns, action_id, None
        if final.get("action_id") != action_id or final.get("kind") != expected_kind:
            return "terminal_receipt_mismatch", start_ns, end_ns, action_id, None
        receipt = final
    if receipt.get("status") != "applied":
        return str(receipt.get("status", "unknown")), start_ns, end_ns, action_id, None
    result = receipt.get("result")
    if not isinstance(result, dict):
        return "endpoint_receipt_mismatch", start_ns, end_ns, action_id, None
    result_endpoint = result.get("endpoint") or result.get("endpoint_id") or result.get("action")
    if result_endpoint != expected_result_endpoint:
        return "endpoint_receipt_mismatch", start_ns, end_ns, action_id, None
    scope = result.get("duration_scope")
    duration = result.get("duration_us")
    if type(duration) is not int or duration < 0 or not isinstance(scope, str):
        result = {"duration_us": None, "duration_scope": None,
                  "current_state_confirmed": None, "rendered": None,
                  "result_count": None, "query_present": None,
                  "endpoint": result_endpoint, "desired_mode": None, "navigation": None}
    else:
        catalog_evidence = safe_catalog_evidence(result.get("catalog_evidence"))
        navigation = result.get("navigation")
        safe_navigation = ({key: navigation.get(key) for key in
                            ("action", "requested_offset", "applied_offset", "page_limit")}
                           if isinstance(navigation, dict) else None)
        result = {"duration_us": duration, "duration_scope": scope,
                  "current_state_confirmed": result.get("current_state_confirmed"),
                  "rendered": result.get("rendered"), "result_count": result.get("result_count"),
                  "query_present": result.get("query_present"), "endpoint": result_endpoint,
                  "desired_mode": result.get("desired_mode"), "navigation": safe_navigation}
        if endpoint == "match_people_10000_open":
            result["catalog_evidence"] = catalog_evidence
    return "applied", start_ns, end_ns, action_id, result


def context_reset_spec(endpoint: str) -> tuple[str, str, str, str | None] | None:
    if endpoint == "settings_manage_people_acknowledgement":
        return ("settings_context_reset", "open_settings",
                "match_settings_open_render_ui_excluding_backend_and_vsync", None)
    if endpoint == "operator_pause_feedback":
        return ("pause_context_reset", "resume_all",
                "operator_pause_feedback_render_ui_excluding_backend_and_vsync", "running")
    return None


def context_reset_applied(endpoint: str, outcome: str, result: dict[str, Any] | None) -> bool:
    spec = context_reset_spec(endpoint)
    if spec is None or outcome != "applied" or result is None:
        return False
    _, reset_endpoint, scope, desired_mode = spec
    if (result.get("endpoint") != reset_endpoint or result.get("duration_scope") != scope
            or result.get("current_state_confirmed") is not True or result.get("rendered") is not True
            or type(result.get("result_count")) is not int or result.get("result_count") != 0
            or result.get("query_present") is not False
            or result.get("desired_mode") != desired_mode):
        return False
    if reset_endpoint == "open_settings":
        navigation = result.get("navigation")
        return (isinstance(navigation, dict) and navigation.get("action") == "open_settings"
                and navigation.get("requested_offset") == 0 and navigation.get("applied_offset") == 0
                and navigation.get("page_limit") == 200)
    return True


def initial_settings_setup_record(outcome: str, start_ns: int, end_ns: int, action_id: str | None,
                                  result: dict[str, Any] | None, origin_ns: int) -> dict[str, Any]:
    result = result or {}
    navigation = result.get("navigation")
    navigation = navigation if isinstance(navigation, dict) else {}
    def typed(key: str, expected_type: type, source: dict[str, Any] = result) -> Any:
        value = source.get(key)
        return value if type(value) is expected_type else None
    scope = "match_settings_open_render_ui_excluding_backend_and_vsync"
    start, finish = (start_ns - origin_ns) // 1000, (end_ns - origin_ns) // 1000
    return {"record_type": "initial_settings_setup", "phase": "initial", "ordinal": 0,
            "endpoint": "operator_pause_feedback",
            "reset_endpoint": "open_settings" if result.get("endpoint") == "open_settings" and navigation.get("action") == "open_settings" else "unconfirmed",
            "timestamp_us": finish, "setup_outcome": "applied" if outcome == "applied" else "not_applied",
            "receipt_action_id": action_id if isinstance(action_id, str) and ACTION_ID.fullmatch(action_id) else None,
            "call_start_timestamp_us": start, "call_end_timestamp_us": finish, "duration_us": finish - start,
            "reported_endpoint_duration_us": typed("duration_us", int),
            "reported_endpoint_duration_scope": scope if result.get("duration_scope") == scope else None,
            "reported_endpoint_current_state_confirmed": typed("current_state_confirmed", bool),
            "reported_endpoint_rendered": typed("rendered", bool), "reported_endpoint_result_count": typed("result_count", int),
            "reported_endpoint_query_present": typed("query_present", bool),
            "reported_endpoint_desired_mode": None if result.get("desired_mode") is None else "unexpected",
            "requested_offset": typed("requested_offset", int, navigation),
            "applied_offset": typed("applied_offset", int, navigation), "page_limit": typed("page_limit", int, navigation)}


def validate_initial_settings_setup(row: dict[str, Any]) -> None:
    if (set(row) != INITIAL_SETTINGS_SETUP_FIELDS or row.get("record_type") != "initial_settings_setup"
            or row.get("phase") != "initial" or type(row.get("ordinal")) is not int or row["ordinal"] != 0
            or row.get("endpoint") != "operator_pause_feedback" or row.get("reset_endpoint") != "open_settings"):
        raise Invalid("pause initial Settings setup fields/route mismatch")
    evidence_uuid(row["receipt_action_id"], "initial Settings setup action_id")
    if any(type(row[key]) is not int for key in ("requested_offset", "applied_offset", "page_limit")):
        raise Invalid("initial Settings setup navigation bounds must be integers")
    for key in ("call_start_timestamp_us", "call_end_timestamp_us", "timestamp_us", "duration_us", "reported_endpoint_duration_us"):
        u64(row[key], f"initial Settings setup {key}")
    if (row["timestamp_us"] != row["call_end_timestamp_us"]
            or row["call_end_timestamp_us"] < row["call_start_timestamp_us"]
            or row["duration_us"] != row["call_end_timestamp_us"] - row["call_start_timestamp_us"]
            or row["reported_endpoint_duration_us"] > row["duration_us"]):
        raise Invalid("initial Settings setup causal duration mismatch")
    result = {"endpoint": row["reset_endpoint"], "duration_scope": row["reported_endpoint_duration_scope"],
              "current_state_confirmed": row["reported_endpoint_current_state_confirmed"],
              "rendered": row["reported_endpoint_rendered"], "result_count": row["reported_endpoint_result_count"],
              "query_present": row["reported_endpoint_query_present"], "desired_mode": row["reported_endpoint_desired_mode"],
              "navigation": {"action": row["reset_endpoint"], "requested_offset": row["requested_offset"],
                             "applied_offset": row["applied_offset"], "page_limit": row["page_limit"]}}
    if not context_reset_applied("settings_manage_people_acknowledgement", row["setup_outcome"], result):
        raise Invalid("pause initial Settings setup lacks exact confirmed rendered Match Settings context")


def runtime_artifact_metadata(args: argparse.Namespace, artifact: Path) -> dict[str, Any]:
    kind = getattr(args, "runtime_artifact_kind", "packaged_portable")
    if not isinstance(kind, str) or kind not in {"packaged_portable", "unpackaged_component"}:
        raise Invalid("runtime artifact kind is unsupported")
    digest = sha256_file(artifact)
    return {"artifact_kind": kind, "runtime_binary_sha256": digest,
            "packaged_binary_sha256": digest if kind == "packaged_portable" else None}


def validate_artifact_metadata(header: dict[str, Any]) -> dict[str, Any]:
    # Earlier captures have only a declared package hash; neither shape proves packaging.
    kind = header.get("artifact_kind", "packaged_portable")
    if not isinstance(kind, str) or kind not in {"packaged_portable", "unpackaged_component"}:
        raise Invalid("runtime artifact kind is unsupported")
    if "artifact_kind" in header and "runtime_binary_sha256" not in header:
        raise Invalid("explicit artifact kind requires runtime binary hash")
    if "artifact_kind" not in header and "runtime_binary_sha256" in header:
        raise Invalid("runtime binary hash requires explicit artifact kind")
    runtime_hash = header.get("runtime_binary_sha256", header.get("packaged_binary_sha256"))
    check_hash(runtime_hash, "runtime_binary_sha256")
    if kind == "unpackaged_component":
        if header.get("packaged_binary_sha256") is not None or "packaged_binary_sha256" not in header:
            raise Invalid("unpackaged component cannot declare a packaged binary hash")
    else:
        check_hash(header.get("packaged_binary_sha256"), "packaged_binary_sha256")
        if runtime_hash != header["packaged_binary_sha256"]:
            raise Invalid("packaged and runtime binary hashes disagree")
    return {"artifact_kind": kind, "runtime_binary_sha256": runtime_hash,
            "packaged_binary_sha256": header.get("packaged_binary_sha256"),
            "artifact_kind_scope": "caller_declared_artifact_kind_not_package_proof",
            "canonical_package_proven": False,
            "canonical_package_verdict": "pending_independent_package_proof"}


def collect_people(args: argparse.Namespace) -> int:
    workspace = Path(args.workspace_root)
    api_root = Path(args.api_root)
    cli = Path(args.facial_cli)
    portable = Path(args.packaged_portable)
    hardware_manifest = Path(args.hardware_manifest)
    if not workspace.is_dir() or not api_root.is_dir() or not cli.is_file() or not portable.is_file() or not hardware_manifest.is_file():
        raise Invalid("workspace, API root, facial-cli, runtime artifact executable, and hardware manifest must exist")
    fixture = Path(args.fixture_manifest)
    if not fixture.is_file():
        raise Invalid("fixture manifest must exist; fixture size/content is not inferred")
    if args.endpoint not in REQUIRED_INTERACTION_ENDPOINTS:
        raise Invalid("endpoint is outside the WP-087 interaction vocabulary")
    if args.endpoint == "match_people_10000_open" and args.fixture_people_count != 10_000:
        raise Invalid("WP-087 requires the 10,000-People fixture declaration")
    header = {
        "record_type": "header", "schema_version": SCHEMA_VERSION,
        "run_id": args.run_id, "workload": "interaction",
        "endpoint": args.endpoint,
        "metric_scope": "facial_cli_invocation_to_terminal_applied_receipt",
        "clock": "python.time.monotonic_ns", "receipt_status_required": "applied",
        "warmup_calls": 20, "measured_calls": 200,
        "facial_cli_sha256": sha256_file(cli),
        **runtime_artifact_metadata(args, portable),
        "hardware_manifest_sha256": sha256_file(hardware_manifest),
        "fixture_manifest_sha256": sha256_file(fixture),
        "input_script_sha256": sha256_file(Path(__file__)),
        "fixture_people_count_declared": args.fixture_people_count if args.fixture_people_count is not None else None,
        "fixture_count_independent_proof": None,
    }
    output = ensure_output(workspace, args.run_id)
    writer = CaptureWriter(output)
    records_written = 1
    writer.append(header)
    origin_ns = time.monotonic_ns()
    failure = None
    catalog_before = None
    catalog_after = None
    if args.endpoint == "match_people_10000_open":
        catalog_before = collect_catalog_observation(cli, api_root, workspace, args.timeout_s,
                                                     origin_ns, header["fixture_manifest_sha256"])
    interaction_count = 0
    if args.endpoint == "operator_pause_feedback":
        initial_outcome, initial_start, initial_end, initial_id, initial_result = interaction_outcome(
            cli, api_root, workspace, "settings_context_reset", args.timeout_s)
        initial_record = initial_settings_setup_record(initial_outcome, initial_start, initial_end,
                                                       initial_id, initial_result, origin_ns)
        writer.append(initial_record)
        records_written += 1
        try:
            validate_initial_settings_setup(initial_record)
            if not context_reset_applied("settings_manage_people_acknowledgement", initial_outcome, initial_result):
                raise Invalid("initial Settings route was not confirmed")
        except Invalid:
            failure = "initial_settings_context_not_confirmed:operator_pause_feedback"
    phases = (("warmup", 20), ("measure", 200)) if failure is None else ()
    for phase, count in phases:
        for ordinal in range(count):
            setup_action_id = None
            setup_timestamp_us = None
            reset = context_reset_spec(args.endpoint)
            if reset is not None:
                reset_endpoint, reset_result_endpoint, _, _ = reset
                reset_outcome, _, reset_end_ns, setup_action_id, reset_result = interaction_outcome(
                    cli, api_root, workspace, reset_endpoint, args.timeout_s)
                setup_timestamp_us = (reset_end_ns - origin_ns) // 1000
                setup_record = {
                    "record_type": "context_setup", "phase": phase, "ordinal": ordinal,
                    "endpoint": args.endpoint, "reset_endpoint": reset_result_endpoint,
                    "timestamp_us": setup_timestamp_us, "setup_outcome": reset_outcome,
                    "receipt_action_id": setup_action_id,
                    "reported_endpoint_duration_scope": reset_result.get("duration_scope") if reset_result else None,
                    "reported_endpoint_current_state_confirmed": reset_result.get("current_state_confirmed") if reset_result else None,
                    "reported_endpoint_rendered": reset_result.get("rendered") if reset_result else None,
                    "reported_endpoint_result_count": reset_result.get("result_count") if reset_result else None,
                    "reported_endpoint_query_present": reset_result.get("query_present") if reset_result else None,
                    "reported_endpoint_desired_mode": reset_result.get("desired_mode") if reset_result else None,
                    "requested_offset": (reset_result.get("navigation") or {}).get("requested_offset") if reset_result else None,
                    "applied_offset": (reset_result.get("navigation") or {}).get("applied_offset") if reset_result else None,
                    "page_limit": (reset_result.get("navigation") or {}).get("page_limit") if reset_result else None,
                }
                writer.append(setup_record)
                records_written += 1
                if not context_reset_applied(args.endpoint, reset_outcome, reset_result):
                    failure = f"context_reset_not_confirmed:{args.endpoint}"
                    break
            outcome, start_ns, end_ns, action_id, endpoint_result = interaction_outcome(
                cli, api_root, workspace, args.endpoint, args.timeout_s,
                args.query, args.media_key, args.catalog_revision, args.face_id)
            start_us = (start_ns - origin_ns) // 1000
            end_us = (end_ns - origin_ns) // 1000
            record = {
                "record_type": "interaction", "phase": phase, "ordinal": ordinal,
                "endpoint": args.endpoint,
                "call_start_timestamp_us": start_us,
                "call_end_timestamp_us": end_us,
                "duration_us": end_us - start_us,
                "endpoint_outcome": outcome,
                "receipt_action_id": action_id,
                "context_setup_action_id": setup_action_id,
                "context_setup_timestamp_us": setup_timestamp_us,
                "reported_endpoint_duration_us": endpoint_result.get("duration_us") if endpoint_result else None,
                "reported_endpoint_duration_scope": endpoint_result.get("duration_scope") if endpoint_result else None,
                "reported_endpoint_current_state_confirmed": endpoint_result.get("current_state_confirmed") if endpoint_result else None,
                "reported_endpoint_rendered": endpoint_result.get("rendered") if endpoint_result else None,
                "reported_endpoint_result_count": endpoint_result.get("result_count") if endpoint_result else None,
                "reported_endpoint_query_present": endpoint_result.get("query_present") if endpoint_result else None,
                "reported_endpoint_desired_mode": endpoint_result.get("desired_mode") if endpoint_result else None,
            }
            if args.endpoint == "match_people_10000_open":
                record["reported_catalog_evidence"] = endpoint_result.get("catalog_evidence") if endpoint_result else None
            writer.append(record)
            records_written += 1
            interaction_count += 1
            if outcome != "applied":
                failure = outcome
                break
        if failure is not None:
            break
    if failure is None:
        if args.endpoint == "match_people_10000_open":
            catalog_after = collect_catalog_observation(cli, api_root, workspace, args.timeout_s,
                                                        origin_ns, sha256_file(fixture))
        writer.append({"record_type": "end", "outcome": "completed", "observed_at_us": (time.monotonic_ns() - origin_ns) // 1000,
                       "catalog_before": catalog_before, "catalog_after": catalog_after,
                       "sample_count": 220})
        records_written += 1
    else:
        writer.append({"record_type": "end", "outcome": "invalid", "reason": failure,
                       "observed_at_us": (time.monotonic_ns() - origin_ns) // 1000,
                       "sample_count": interaction_count})
        records_written += 1
    writer.close()
    print(json.dumps({"output": str(output), "records": records_written, "scope": header["metric_scope"],
                      "independent_fixture_count_proof": None}, separators=(",", ":")))
    return 0 if failure is None else 2


def runtime_diagnostics(cli: Path, api_root: Path, workspace: Path, timeout_s: float,
                        receipt_identity: dict[str, str] | None = None) -> tuple[dict[str, Any] | None, str]:
    code, receipt, start_ns, _ = run_cli(cli, ["match_runtime_diagnostics"], workspace, timeout_s)
    if receipt is None:
        return None, "runtime_diagnostics_cli_unavailable" if code != 124 else "runtime_diagnostics_timeout"
    action_id = receipt.get("action_id")
    if (not isinstance(action_id, str) or not ACTION_ID.fullmatch(action_id)
            or receipt.get("kind") != "match_runtime_diagnostics"):
        return None, "runtime_diagnostics_receipt_mismatch"
    if receipt.get("status") == "accepted":
        receipt = terminal_receipt(api_root, action_id, start_ns + int(timeout_s * 1_000_000_000),
                                   "match_runtime_diagnostics", MAX_DIAGNOSTICS_RECEIPT_BYTES)
    if (not isinstance(receipt, dict) or receipt.get("action_id") != action_id
            or receipt.get("kind") != "match_runtime_diagnostics" or receipt.get("status") != "applied"):
        return None, "runtime_diagnostics_not_applied"
    result = receipt.get("result")
    if not isinstance(result, dict) or result.get("endpoint_scope") != "running_gui_governor_lifetime":
        return None, "runtime_diagnostics_scope_mismatch"
    snapshot = result.get("snapshot")
    if not isinstance(snapshot, dict):
        return None, "runtime_diagnostics_snapshot_unavailable"
    if receipt_identity is not None:
        receipt_identity["action_id"] = action_id
    return snapshot, "applied"


def collect_catalog_observation(cli: Path, api_root: Path, workspace: Path, timeout_s: float,
                                origin_ns: int, fixture_sha256: str) -> dict[str, Any] | None:
    identity: dict[str, str] = {}
    start = (time.monotonic_ns() - origin_ns) // 1000
    snapshot, outcome = runtime_diagnostics(cli, api_root, workspace, timeout_s, identity)
    finish = (time.monotonic_ns() - origin_ns) // 1000
    if outcome != "applied" or not snapshot:
        return None
    return {"receipt_action_id": identity.get("action_id"),
            "call_start_timestamp_us": start, "call_end_timestamp_us": finish,
            "fixture_manifest_sha256": fixture_sha256,
            "catalog_evidence": safe_catalog_evidence((snapshot.get("catalog") or {}).get("evidence"))}


def safe_catalog_evidence(evidence: Any) -> dict[str, Any] | None:
    if (not isinstance(evidence, dict) or set(evidence) != CATALOG_EVIDENCE_FIELDS
            or any(type(evidence.get(field)) is not int or not 0 <= evidence[field] <= 2**64 - 1
                   for field in ("total_people", "catalog_revision"))
            or evidence.get("count_scope") not in ("canonical_nonhidden_people", "canonical_all_people")
            or evidence.get("schema_generation") != "match-schema-v2"
            or not isinstance(evidence.get("store_session_id"), str)
            or not re.fullmatch(r"[0-9a-f]{32}", evidence["store_session_id"])):
        return None
    return dict(evidence)


def validate_catalog_evidence(evidence: Any) -> None:
    if (safe_catalog_evidence(evidence) is None or evidence["total_people"] != 10_000
            or evidence["count_scope"] != "canonical_nonhidden_people"):
        raise Invalid("People count evidence must bind canonical nonhidden 10000 rows, revision, schema, and store session")


def analyze_catalog_observations(header: dict[str, Any], rows: list[dict[str, Any]],
                                 end: dict[str, Any], action_ids: set[str]) -> bool:
    before, after = end.get("catalog_before"), end.get("catalog_after")
    if before is None or after is None or any(row.get("reported_catalog_evidence") is None for row in rows):
        return False
    for observation in (before, after):
        if not isinstance(observation, dict) or set(observation) != CATALOG_OBSERVATION_FIELDS:
            raise Invalid("People canonical observation fields do not match schema")
        action_id = observation.get("receipt_action_id")
        if not isinstance(action_id, str) or not ACTION_ID.fullmatch(action_id) or action_id in action_ids:
            raise Invalid("People canonical observation requires a distinct diagnostic receipt UUID")
        action_ids.add(action_id)
        if observation.get("fixture_manifest_sha256") != header.get("fixture_manifest_sha256"):
            raise Invalid("People canonical observation fixture manifest binding changed")
        start, finish = observation.get("call_start_timestamp_us"), observation.get("call_end_timestamp_us")
        if type(start) is not int or type(finish) is not int or start < 0 or finish < start:
            raise Invalid("People canonical observation interval is invalid")
        validate_catalog_evidence(observation.get("catalog_evidence"))
    if (before["call_end_timestamp_us"] >= rows[0]["call_start_timestamp_us"]
            or after["call_start_timestamp_us"] <= rows[-1]["call_end_timestamp_us"]
            or after["call_end_timestamp_us"] > end["observed_at_us"]):
        raise Invalid("People canonical observations must surround the measured calls causally")
    expected = before["catalog_evidence"]
    for evidence in [after["catalog_evidence"], *(row["reported_catalog_evidence"] for row in rows)]:
        validate_catalog_evidence(evidence)
        if evidence != expected:
            raise Invalid("People canonical count, revision, schema, or store session changed between observed samples")
    return True


def concurrency_sample(snapshot: dict[str, Any], phase: str, timestamp_us: int,
                       previous_counters: dict[str, int] | None,
                       expected_lifetime_id: str | None = None) -> tuple[dict[str, Any], dict[str, int] | None, str]:
    execution = snapshot.get("execution")
    jobs = snapshot.get("jobs")
    if not isinstance(execution, dict) or not isinstance(jobs, list):
        raise Invalid("live runtime snapshot lacks execution/jobs state")
    telemetry = execution.get("resource_telemetry")
    usage = execution.get("resource_usage")
    if not isinstance(telemetry, dict) or set(telemetry) != TELEMETRY_FIELDS:
        raise Invalid("live runtime snapshot lacks current governor telemetry")
    if not isinstance(usage, dict) or set(usage) != set(TELEMETRY_AXES):
        raise Invalid("live runtime snapshot lacks current admitted resource usage")
    lifetime_id = telemetry.get("lifetime_id")
    if not isinstance(lifetime_id, str) or telemetry.get("scope") != "governor_lifetime_including_warmup":
        raise Invalid("live runtime snapshot governor identity/scope is invalid")
    try:
        uuid.UUID(lifetime_id)
    except (ValueError, TypeError, AttributeError) as error:
        raise Invalid("live runtime snapshot governor lifetime_id is not a UUID") from error
    if telemetry.get("overflow") is not False:
        raise Invalid("live runtime governor telemetry overflow invalidates the run")
    if expected_lifetime_id is not None and lifetime_id != expected_lifetime_id:
        raise Invalid("live runtime governor lifetime changed during collection")
    for key in ("current_usage", "peak_usage"):
        values = telemetry.get(key)
        if not isinstance(values, dict) or set(values) != set(TELEMETRY_AXES):
            raise Invalid(f"live runtime governor {key} axes are invalid")
        if any(type(value) is not int or value < 0 or value > 2**64 - 1 for value in values.values()):
            raise Invalid(f"live runtime governor {key} contains invalid values")
    if not isinstance(execution.get("resource_budget"), dict) or set(execution["resource_budget"]) != TELEMETRY_BUDGET_FIELDS:
        raise Invalid("live runtime snapshot lacks configured resource ceilings")
    index_stage = execution.get("index_stage")
    index_stage_counts = execution.get("index_stage_counts")
    if (not isinstance(index_stage, str)
            or execution.get("index_stage_scope") != "persisted_asset_next_stage_counts"
            or not isinstance(index_stage_counts, dict)
            or any(not isinstance(stage, str) or type(value) is not int or value < 0 or value > 2**64 - 1
                   for stage, value in index_stage_counts.items())):
        raise Invalid("live runtime snapshot lacks canonical persisted-asset index-stage counts")
    progress = snapshot.get("job_progress")
    progress_fields = {"discovered", "completed", "failed", "skipped"}
    if (snapshot.get("job_progress_scope") != "canonical_all_index_jobs"
            or not isinstance(progress, dict) or set(progress) != progress_fields
            or any(type(value) is not int or value < 0 or value > 2**64 - 1
                   for value in progress.values())):
        raise Invalid("live runtime snapshot lacks canonical all-index-job progress counts")
    counters = {key: telemetry.get(key) for key in TELEMETRY_COUNTERS}
    if any(type(value) is not int or value < 0 for value in counters.values()):
        raise Invalid("runtime telemetry counter is unavailable")
    if previous_counters is None:
        deltas = None
    else:
        if any(counters[key] < previous_counters[key] for key in counters):
            raise Invalid("runtime telemetry counter reset during collection")
        deltas = {key: counters[key] - previous_counters[key] for key in counters}
    row = {
        "record_type": "concurrency", "phase": phase, "timestamp_us": timestamp_us,
        "indexing_progress": progress, "indexing_progress_scope": "canonical_all_index_jobs",
        "index_stage": index_stage,
        "index_stage_counts": index_stage_counts,
        "index_stage_scope": "persisted_asset_next_stage_counts",
        "active_holds": execution.get("holds"),
        "admitted_resources": usage, "lease_deltas": deltas,
    }
    return row, counters, lifetime_id


def u64(value: Any, label: str) -> int:
    if type(value) is not int or not 0 <= value <= 2**64 - 1:
        raise Invalid(f"{label} must be u64")
    return value


def resource_axes(value: Any, label: str) -> dict[str, int]:
    if not isinstance(value, dict) or set(value) != set(TELEMETRY_AXES):
        raise Invalid(f"{label} must contain exact resource axes")
    for item in value.values():
        u64(item, label)
    return value


def evidence_uuid(value: Any, label: str) -> str:
    if not isinstance(value, str) or not ACTION_ID.fullmatch(value):
        raise Invalid(f"{label} must be canonical UUID")
    return value


def runtime_evidence_snapshot(snapshot: dict[str, Any]) -> dict[str, Any]:
    evidence = snapshot.get("runtime_evidence")
    scopes = runtime_scopes(evidence.get("schema_version") if isinstance(evidence, dict) else None)
    fields = {"schema_version", "runtime_id", "timestamp_scope", "captured_at_us",
              "governor_interval", *scopes}
    if (not isinstance(evidence, dict) or set(evidence) != fields
            or type(evidence.get("schema_version")) is not int or evidence["schema_version"] not in (1, 2)):
        raise Invalid("runtime_evidence must match the bounded producer schema")
    runtime_id = evidence_uuid(evidence["runtime_id"], "runtime_id")
    if evidence["timestamp_scope"] != RUNTIME_TIMESTAMP_SCOPE:
        raise Invalid("runtime_evidence clock scope mismatch")
    captured = u64(evidence["captured_at_us"], "runtime captured_at_us")
    interval = evidence["governor_interval"]
    if not isinstance(interval, dict) or set(interval) != RUNTIME_INTERVAL_FIELDS:
        raise Invalid("governor interval fields mismatch")
    evidence_uuid(interval["lifetime_id"], "governor interval lifetime_id")
    if interval["runtime_id"] != runtime_id or interval["overflow"] is not False:
        raise Invalid("governor interval identity/overflow invalid")
    if interval["scope"] != "governor_interval_between_dedicated_runtime_diagnostics":
        raise Invalid("governor interval scope mismatch")
    for key in ("sequence", "start_us", "end_us", *TELEMETRY_COUNTERS):
        u64(interval[key], f"governor interval {key}")
    if interval["sequence"] == 0 or not interval["start_us"] <= interval["end_us"] <= captured:
        raise Invalid("governor interval clock bounds invalid")
    for key in ("opening_usage", "closing_usage", "peak_usage"):
        resource_axes(interval[key], f"governor interval {key}")
    for prefix in ("opening", "closing"):
        stages = interval[f"{prefix}_live_stage_leases"]
        if not isinstance(stages, list) or len(stages) != 7:
            raise Invalid("interval live stages must retain seven producer stages")
        for count in (*stages, interval[f"{prefix}_unclassified_leases"]):
            u64(count, "interval live lease count")
    for axis in TELEMETRY_AXES:
        if interval["peak_usage"][axis] < max(interval["opening_usage"][axis], interval["closing_usage"][axis]):
            raise Invalid("governor interval peak excludes boundary usage")
    for endpoint, scope in scopes.items():
        ring = evidence[endpoint]
        if not isinstance(ring, dict) or set(ring) != RUNTIME_RING_FIELDS:
            raise Invalid(f"{endpoint} ring fields mismatch")
        evidence_uuid(ring["lifetime_id"], f"{endpoint} lifetime_id")
        if ring["runtime_id"] != runtime_id or ring["endpoint_scope"] != scope or ring["overflow"] is not False:
            raise Invalid(f"{endpoint} identity/scope/overflow invalid")
        for key in ("captured_at_us", "sequence", "dropped_records"):
            u64(ring[key], f"{endpoint} {key}")
        samples = ring["samples"]
        if (not isinstance(samples, list) or len(samples) > 256 or len(samples) != min(ring["sequence"], 256)
                or ring["dropped_records"] != ring["sequence"] - len(samples)
                or ring["captured_at_us"] > captured):
            raise Invalid(f"{endpoint} bounded ring count/clock mismatch")
        previous_sequence, previous_time = ring["dropped_records"], -1
        previous_generation, previous_poll_end = -1, -1
        for sample in samples:
            if not isinstance(sample, dict) or set(sample) != RUNTIME_SAMPLE_FIELDS[endpoint]:
                raise Invalid(f"{endpoint} raw sample fields mismatch")
            stamp = u64(sample["timestamp_us"], f"{endpoint} timestamp_us")
            if u64(sample["sequence"], f"{endpoint} sequence") != previous_sequence + 1:
                raise Invalid(f"{endpoint} raw sequence gap")
            if not previous_time <= stamp <= ring["captured_at_us"]:
                raise Invalid(f"{endpoint} raw clock reset/outside capture")
            previous_sequence, previous_time = sample["sequence"], stamp
            if endpoint == "lease_activity":
                if sample["event"] not in {"acquire", "replace", "release", "preparation_release", "pressure", "stage_tagged"}:
                    raise Invalid("lease activity event invalid")
                resource_axes(sample["usage"], "lease activity usage")
                stages = sample["live_stage_leases"]
                if not isinstance(stages, list) or len(stages) != 7:
                    raise Invalid("lease stage array must retain seven producer stages")
                for count in (*stages, sample["unclassified_leases"]):
                    u64(count, "live stage lease count")
            elif endpoint == "worker_control":
                if sample["event"] not in {"hold_transition", "parent_request_begin", "transport_dispatch", "fenced_preparation_progress",
                        "fenced_checkpoint", "fenced_response", "fenced_child_error", "late_fenced_response",
                        "quarantined", "owned_exit_confirmed"}:
                    raise Invalid("worker control event invalid")
                u64(sample["admission_epoch"], "worker admission epoch")
                if sample["event"] == "parent_request_begin":
                    if u64(sample["parent_request_start_us"], "parent request start") > stamp:
                        raise Invalid("parent request causal clock invalid")
                elif sample["parent_request_start_us"] is not None:
                    raise Invalid("request start supplied outside parent request begin")
                if sample["operation"] not in {"none", "detect", "embed", "preparation_step", "decode", "fingerprint_step", "discovery_next", "other"}:
                    raise Invalid("worker operation invalid")
                if sample["event"] == "hold_transition":
                    if any(sample[key] is not None for key in ("worker_id", "operation_id", "fence_sha256")):
                        raise Invalid("hold transition contains worker identifiers")
                    start = u64(sample["transition_start_us"], "hold CAS bracket start")
                    u64(sample["previous_epoch"], "previous hold epoch")
                    if start > stamp or sample["previous_epoch"] == sample["admission_epoch"]:
                        raise Invalid("hold transition bracket invalid")
                else:
                    evidence_uuid(sample["worker_id"], "worker id")
                    if sample["operation_id"] is not None:
                        evidence_uuid(sample["operation_id"], "operation id")
                    if sample["fence_sha256"] is not None and not SHA256.fullmatch(sample["fence_sha256"]):
                        raise Invalid("worker fence digest invalid")
                    if sample["event"] not in {"owned_exit_confirmed", "quarantined"} and (sample["operation_id"] is None or sample["fence_sha256"] is None):
                        raise Invalid("worker operation lacks exact fence/operation identity")
                    if sample["transition_start_us"] is not None or sample["previous_epoch"] is not None:
                        raise Invalid("worker event contains hold transition metadata")
            else:
                u64(sample["player_generation"], "native player generation")
                if sample["player_generation"] < previous_generation:
                    raise Invalid("native player generation reset")
                previous_generation = sample["player_generation"]
                if sample["generation_overflow"] is not False:
                    raise Invalid("native player generation overflow invalidates capture")
                if sample["status"] not in {"pending", "opening", "buffering", "playing", "paused", "stopped", "ended", "error"}:
                    raise Invalid("native playback status invalid")
                if any(type(sample[key]) is not bool for key in ("native_player_present", "native_playing", "clock_available")):
                    raise Invalid("native playback observation flags must be bool")
                start = u64(sample["poll_start_us"], "native poll start")
                finish = u64(sample["poll_end_us"], "native poll end")
                if not start <= finish <= stamp:
                    raise Invalid("native poll causal clock mismatch")
                if start < previous_poll_end:
                    raise Invalid("native raw polls overlap or reset")
                previous_poll_end = finish
                clock = sample["time_ms"]
                if clock is not None and (type(clock) is not int or not 0 <= clock < 2**63):
                    raise Invalid("native available clock must be nonnegative i64 or null")
                if sample["clock_available"] != (clock is not None):
                    raise Invalid("native clock availability mismatch")
    return evidence


def runtime_evidence_records(snapshot: dict[str, Any], phase: str, timestamp_us: int,
                             previous: dict[str, Any] | None) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    current = runtime_evidence_snapshot(snapshot)
    scopes = runtime_scopes(current["schema_version"])
    if previous is not None and current["schema_version"] != previous["schema_version"]:
        raise Invalid("runtime producer schema changed during capture")
    if phase not in {"baseline", "measure", "terminal"} or (phase == "baseline") != (previous is None):
        raise Invalid("runtime evidence baseline/phase invalid")
    rows = []
    if previous is not None:
        old, interval = previous["governor_interval"], current["governor_interval"]
        if (current["runtime_id"] != previous["runtime_id"] or interval["lifetime_id"] != old["lifetime_id"]
                or interval["sequence"] != old["sequence"] + 1 or interval["start_us"] != old["end_us"]
                or interval["opening_usage"] != old["closing_usage"]
                or interval["opening_live_stage_leases"] != old["closing_live_stage_leases"]
                or interval["opening_unclassified_leases"] != old["closing_unclassified_leases"]
                or current["captured_at_us"] <= previous["captured_at_us"]):
            raise Invalid("runtime/governor interval reset, gap, or diagnostics interference")
    rings = {}
    for endpoint in scopes:
        ring = current[endpoint]
        rings[endpoint] = {key: value for key, value in ring.items() if key != "samples"}
        if previous is None:
            for sample in ring["samples"]:
                rows.append({"record_type": "runtime_sample", "phase": phase, "timestamp_us": timestamp_us,
                             "endpoint": endpoint, "runtime_id": current["runtime_id"],
                             "lifetime_id": ring["lifetime_id"], "sample": sample})
            continue
        old = previous[endpoint]
        if (ring["lifetime_id"] != old["lifetime_id"] or ring["sequence"] < old["sequence"]
                or ring["captured_at_us"] < old["captured_at_us"] or ring["dropped_records"] < old["dropped_records"]):
            raise Invalid(f"{endpoint} series reset")
        samples = {sample["sequence"]: sample for sample in ring["samples"]}
        if ring["sequence"] - old["sequence"] > 256:
            raise Invalid(f"{endpoint} unretained measured ring loss")
        for sequence in range(old["sequence"] + 1, ring["sequence"] + 1):
            if sequence not in samples:
                raise Invalid(f"{endpoint} measured sequence gap/loss")
            rows.append({"record_type": "runtime_sample", "phase": phase, "timestamp_us": timestamp_us,
                         "endpoint": endpoint, "runtime_id": current["runtime_id"],
                         "lifetime_id": ring["lifetime_id"], "sample": samples[sequence]})
    rows.append({"record_type": "runtime_checkpoint", "phase": phase, "timestamp_us": timestamp_us,
                 "runtime_id": current["runtime_id"], "timestamp_scope": current["timestamp_scope"],
                 "captured_at_us": current["captured_at_us"], "governor_interval": current["governor_interval"],
                 "rings": rings})
    return rows, current


def visible_work_snapshot(snapshot: dict[str, Any]) -> dict[str, dict[str, Any]]:
    visible = snapshot.get("visible_work")
    if not isinstance(visible, dict) or set(visible) != {"schema_version", *VISIBLE_WORK_SCOPES}:
        raise Invalid("live runtime snapshot lacks the exact WP-087 visible_work schema")
    version = visible.get("schema_version")
    if type(version) is not int or version not in {1, 2}:
        raise Invalid("visible_work schema_version must be integer 1 or 2")
    result = {}
    for endpoint, expected_scope in VISIBLE_WORK_SCOPES.items():
        series = visible.get(endpoint)
        fields = VISIBLE_SERIES_FIELDS | ({"runtime_id", "timestamp_scope"} if version == 2 else set())
        if not isinstance(series, dict) or set(series) != fields:
            raise Invalid(f"visible_work {endpoint} fields do not match the producer schema")
        if version == 2:
            evidence_uuid(series["runtime_id"], "visible runtime_id")
            if series["timestamp_scope"] != RUNTIME_TIMESTAMP_SCOPE:
                raise Invalid("visible common clock scope mismatch")
        try:
            uuid.UUID(series["lifetime_id"])
        except (ValueError, TypeError, AttributeError) as error:
            raise Invalid(f"visible_work {endpoint} lifetime_id must be UUID") from error
        if series["endpoint_scope"] != expected_scope:
            raise Invalid(f"visible_work {endpoint} endpoint scope mismatch")
        for key in ("captured_at_us", "sequence", "dropped_records", "abandoned", "pending"):
            value = series[key]
            if type(value) is not int or value < 0 or value > 2**64 - 1:
                raise Invalid(f"visible_work {endpoint} {key} must be a u64")
        if type(series["overflow"]) is not bool or series["overflow"]:
            raise Invalid(f"visible_work {endpoint} overflow invalidates the capture")
        samples = series["samples"]
        if not isinstance(samples, list) or len(samples) > 256:
            raise Invalid(f"visible_work {endpoint} sample ring exceeds its 256-record bound")
        if len(samples) != min(series["sequence"], 256) or series["dropped_records"] != series["sequence"] - len(samples):
            raise Invalid(f"visible_work {endpoint} ring count disagrees with sequence/drop counters")
        previous_sequence = series["sequence"] - len(samples)
        for sample in samples:
            if not isinstance(sample, dict) or set(sample) != VISIBLE_SAMPLE_FIELDS:
                raise Invalid(f"visible_work {endpoint} sample fields do not match the producer schema")
            if any(type(sample[key]) is not int or sample[key] < 0 or sample[key] > 2**64 - 1
                   for key in VISIBLE_SAMPLE_FIELDS):
                raise Invalid(f"visible_work {endpoint} sample values must be u64")
            if sample["sequence"] != previous_sequence + 1:
                raise Invalid(f"visible_work {endpoint} retained sample ring has a sequence gap")
            if sample["end_us"] < sample["start_us"] or sample["duration_us"] != sample["end_us"] - sample["start_us"]:
                raise Invalid(f"visible_work {endpoint} sample duration is inconsistent")
            if sample["end_us"] > series["captured_at_us"]:
                raise Invalid(f"visible_work {endpoint} sample ends after its captured_at_us")
            previous_sequence = sample["sequence"]
        if samples and samples[-1]["sequence"] != series["sequence"]:
            raise Invalid(f"visible_work {endpoint} final retained sample misses the current sequence")
        result[endpoint] = series
    return result


def visible_work_records(snapshot: dict[str, Any], phase: str, timestamp_us: int,
                         previous: dict[str, dict[str, Any]] | None) -> tuple[list[dict[str, Any]], dict[str, dict[str, Any]]]:
    current = visible_work_snapshot(snapshot)
    if phase not in {"baseline", "measure", "terminal"}:
        raise Invalid("visible_work checkpoint phase is invalid")
    if (phase == "baseline") != (previous is None):
        raise Invalid("visible_work baseline must be captured exactly once before polling")
    records = []
    for endpoint, series in current.items():
        old = previous.get(endpoint) if previous is not None else None
        new_samples = []
        if old is not None:
            if series.get("runtime_id") != old.get("runtime_id") or series.get("timestamp_scope") != old.get("timestamp_scope"):
                raise Invalid("visible common clock identity changed")
            if series["lifetime_id"] != old["lifetime_id"]:
                raise Invalid(f"visible_work {endpoint} lifetime changed during capture")
            if series["captured_at_us"] <= old["captured_at_us"]:
                raise Invalid(f"visible_work {endpoint} capture time did not increase")
            for counter in ("sequence", "dropped_records", "abandoned"):
                if series[counter] < old[counter]:
                    raise Invalid(f"visible_work {endpoint} {counter} reset")
            if series["sequence"] - old["sequence"] > 256:
                raise Invalid(f"visible_work {endpoint} new sequence gap exceeds retained ring capacity")
            by_sequence = {sample["sequence"]: sample for sample in series["samples"]}
            first_new = old["sequence"] + 1
            expected = list(range(first_new, series["sequence"] + 1))
            if any(sequence not in by_sequence for sequence in expected):
                raise Invalid(f"visible_work {endpoint} sequence gap is not covered by retained ring")
            new_samples = [by_sequence[sequence] for sequence in expected]
        for sample in new_samples:
            records.append({
                "record_type": "visible_work_sample", "phase": phase,
                "observed_at_us": timestamp_us, "endpoint": endpoint,
                "lifetime_id": series["lifetime_id"], **sample,
                **{key: series[key] for key in ("runtime_id", "timestamp_scope") if key in series},
            })
        records.append({
            "record_type": "visible_work_checkpoint", "phase": phase,
            "timestamp_us": timestamp_us, "endpoint": endpoint,
            "lifetime_id": series["lifetime_id"], "endpoint_scope": series["endpoint_scope"],
            "captured_at_us": series["captured_at_us"], "sequence": series["sequence"],
            "dropped_records": series["dropped_records"], "abandoned": series["abandoned"],
            "overflow": series["overflow"], "pending": series["pending"],
            **{key: series[key] for key in ("runtime_id", "timestamp_scope") if key in series},
        })
    return records, current


def collect_concurrency(args: argparse.Namespace) -> int:
    workspace = Path(args.workspace_root)
    api_root = Path(args.api_root)
    cli = Path(args.facial_cli)
    portable = Path(args.packaged_portable)
    hardware_manifest = Path(args.hardware_manifest)
    fixture = Path(args.fixture_manifest)
    if not workspace.is_dir() or not api_root.is_dir() or not all(path.is_file() for path in (cli, portable, hardware_manifest, fixture)):
        raise Invalid("workspace, API root, CLI, runtime artifact, hardware manifest, and fixture manifest must exist")
    output = ensure_output(workspace, args.run_id)
    writer = CaptureWriter(output)
    header = {
        "record_type": "header", "schema_version": SCHEMA_VERSION, "run_id": args.run_id,
        "workload": getattr(args, "workload", "concurrency"), "metric_scope": "live_gui_runtime_diagnostics_plus_external_visible_work_inputs",
        "diagnostics_route": "match_runtime_diagnostics", "target_interval_us": 1_000_000,
        "warmup_seconds": 60, "measure_seconds": 600,
        "facial_cli_sha256": sha256_file(cli), **runtime_artifact_metadata(args, portable),
        "hardware_manifest_sha256": sha256_file(hardware_manifest),
        "fixture_manifest_sha256": sha256_file(fixture), "input_script_sha256": sha256_file(Path(__file__)),
        "visible_work_schema_version": 2, "runtime_evidence_schema_version": 2,
        "visible_work_sample_semantics": "per_endpoint_measurement_baseline_then_each_new_success_sequence_once",
        "status_poll_scope": "read_only_same_running_gui_service; polling latency excluded from visible-work metrics",
    }
    origin_ns = time.monotonic_ns()
    writer.append(header)
    count = 0
    previous_counters = None
    governor_lifetime_id = None
    previous_visible_work = None
    previous_runtime_evidence = None
    last_snapshot = None
    last_timestamp = 0
    failure = None
    deadline_ns = origin_ns + 60_000_000_000
    while time.monotonic_ns() < deadline_ns:
        snapshot, outcome = runtime_diagnostics(cli, api_root, workspace, args.timeout_s)
        if outcome != "applied" or snapshot is None:
            failure = outcome
            break
        observed = time.monotonic_ns()
        ts = (observed - origin_ns) // 1000
        row, previous_counters, governor_lifetime_id = concurrency_sample(
            snapshot, "warmup", ts, previous_counters, governor_lifetime_id)
        writer.append(row)
        count += 1
        last_snapshot, last_timestamp = snapshot, ts
        remaining = deadline_ns - time.monotonic_ns()
        if remaining > 0:
            time.sleep(min(1.0, remaining / 1_000_000_000))
    if failure is None and count == 0:
        failure = "warmup produced no valid runtime samples"
    if failure is None:
        baseline, outcome = runtime_diagnostics(cli, api_root, workspace, args.timeout_s)
        if outcome != "applied" or baseline is None:
            failure = outcome
        else:
            measurement_start_ns = time.monotonic_ns()
            measurement_start_us = (measurement_start_ns - origin_ns) // 1000
            baseline_row, previous_counters, governor_lifetime_id = concurrency_sample(
                baseline, "measure", measurement_start_us, previous_counters, governor_lifetime_id)
            writer.append(baseline_row)
            count += 1
            visible_records, previous_visible_work = visible_work_records(
                baseline, "baseline", measurement_start_us, None)
            for record in visible_records:
                writer.append(record)
            runtime_records, previous_runtime_evidence = runtime_evidence_records(
                baseline, "baseline", measurement_start_us, None)
            for record in runtime_records:
                writer.append(record)
            last_snapshot, last_timestamp = baseline, measurement_start_us
            measure_deadline_ns = measurement_start_ns + 600_000_000_000
    if failure is None:
        while time.monotonic_ns() < measure_deadline_ns:
            snapshot, outcome = runtime_diagnostics(cli, api_root, workspace, args.timeout_s)
            if outcome != "applied" or snapshot is None:
                failure = outcome
                break
            observed = time.monotonic_ns()
            ts = (observed - origin_ns) // 1000
            row, previous_counters, governor_lifetime_id = concurrency_sample(
                snapshot, "measure", ts, previous_counters, governor_lifetime_id)
            writer.append(row)
            count += 1
            visible_records, previous_visible_work = visible_work_records(
                snapshot, "measure", ts, previous_visible_work)
            for record in visible_records:
                writer.append(record)
            runtime_records, previous_runtime_evidence = runtime_evidence_records(
                snapshot, "measure", ts, previous_runtime_evidence)
            for record in runtime_records:
                writer.append(record)
            last_snapshot, last_timestamp = snapshot, ts
            remaining = measure_deadline_ns - time.monotonic_ns()
            if remaining > 0:
                time.sleep(min(1.0, remaining / 1_000_000_000))
        terminal, outcome = runtime_diagnostics(cli, api_root, workspace, args.timeout_s)
        if outcome != "applied" or terminal is None:
            failure = outcome
        measurement_end_us = (time.monotonic_ns() - origin_ns) // 1000
        if failure is None and isinstance(terminal, dict):
            terminal_observed_us = (time.monotonic_ns() - origin_ns) // 1000
            terminal_row, previous_counters, governor_lifetime_id = concurrency_sample(
                terminal, "measure", terminal_observed_us, previous_counters, governor_lifetime_id)
            writer.append(terminal_row)
            count += 1
            visible_records, previous_visible_work = visible_work_records(
                terminal, "terminal", terminal_observed_us, previous_visible_work)
            for record in visible_records:
                writer.append(record)
            runtime_records, previous_runtime_evidence = runtime_evidence_records(
                terminal, "terminal", terminal_observed_us, previous_runtime_evidence)
            for record in runtime_records:
                writer.append(record)
            measurement_end_us = terminal_observed_us
        if failure is None and isinstance(baseline, dict) and isinstance(terminal, dict):
            writer.append({"record_type": "governor_evidence", "measurement_start_us": measurement_start_us,
                           "measurement_end_us": measurement_end_us,
                           "resource_budget": baseline.get("execution", {}).get("resource_budget"),
                           "baseline_resource_telemetry": baseline.get("execution", {}).get("resource_telemetry"),
                           "terminal_resource_telemetry": terminal.get("execution", {}).get("resource_telemetry")})
    if failure is None:
        writer.append({"record_type": "end", "outcome": "completed", "observed_at_us": measurement_end_us,
                       "sample_count": count})
    else:
        writer.append({"record_type": "end", "outcome": "invalid", "reason": failure,
                       "observed_at_us": (time.monotonic_ns() - origin_ns) // 1000, "sample_count": count})
    writer.close()
    print(json.dumps({"output": str(output), "samples": count, "outcome": "completed" if failure is None else "invalid",
                      "visible_work_endpoints": sorted(VISIBLE_WORK_SCOPES),
                      "independent_visible_work_budgets": "not_collected"}, separators=(",", ":")))
    return 0 if failure is None else 2


def percentile(values: list[int], p: float) -> int:
    ordered = sorted(values)
    return ordered[max(0, (len(ordered) * int(p * 100) + 99) // 100 - 1)]


def analyze(path: Path) -> dict[str, Any]:
    records = load_records(path)
    header = records[0]
    if header.get("record_type") != "header" or header.get("schema_version") != SCHEMA_VERSION:
        raise Invalid("missing supported header")
    if type(header.get("schema_version")) is not int:
        raise Invalid("schema_version must be an integer")
    if header.get("workload") not in {"interaction", "concurrency", "saturation"}:
        raise Invalid("unsupported workload")
    ends = [row for row in records[1:] if row.get("record_type") == "end"]
    if len(ends) != 1 or ends[0].get("outcome") != "completed":
        raise Invalid("capture needs exactly one completed end record")
    samples = records[1:-1]
    end = ends[0]
    expected_type = {"interaction": "interaction", "concurrency": "concurrency", "saturation": "saturation"}[header["workload"]]
    if header["workload"] == "saturation" and header.get("runtime_evidence_schema_version") in (1, 2):
        expected_type = "concurrency"
    if type(end.get("sample_count")) is not int or end["sample_count"] != len([r for r in samples if r.get("record_type") == expected_type]):
        raise Invalid("end sample_count does not match raw records")
    if not isinstance(header.get("run_id"), str) or not RUN_ID.fullmatch(header["run_id"]):
        raise Invalid("header run_id is invalid")
    check_hash(header.get("facial_cli_sha256"), "facial_cli_sha256")
    artifact = validate_artifact_metadata(header)
    check_hash(header.get("hardware_manifest_sha256"), "hardware_manifest_sha256")
    check_hash(header.get("fixture_manifest_sha256"), "fixture_manifest_sha256")
    check_hash(header.get("input_script_sha256"), "input_script_sha256")
    if header["workload"] == "interaction":
        result = analyze_interaction(header, samples, end)
    elif header["workload"] == "concurrency":
        result = analyze_concurrency(header, samples, end)
    else:
        result = analyze_saturation(header, samples, end)
    if header["workload"] == "interaction":
        result["endpoint_component_verdict"] = result["endpoint_gate_verdict"]
    result.update(artifact)
    if artifact["artifact_kind"] == "unpackaged_component":
        result["gate_eligible"] = False
    result["raw_file_sha256"] = sha256_file(path)
    return result


def analyze_interaction(header: dict[str, Any], records: list[dict[str, Any]], end: dict[str, Any]) -> dict[str, Any]:
    raw_records = records
    endpoint = header.get("endpoint")
    if endpoint not in REQUIRED_INTERACTION_ENDPOINTS:
        raise Invalid("interaction endpoint is outside WP-087 vocabulary")
    measured = []
    endpoint_measured = []
    warmup = 0
    previous_start = -1
    previous_finish = -1
    expected_scope = {
        "settings_manage_people_acknowledgement": "ui_state_rendered_by_render_ui_excluding_backend_and_vsync",
        "cached_autocomplete": "ui_state_rendered_by_render_ui_excluding_backend_and_vsync",
        "match_people_10000_open": "match_people_open_render_ui_excluding_backend_and_vsync",
        "operator_pause_feedback": "operator_pause_feedback_render_ui_excluding_backend_and_vsync",
    }.get(endpoint)
    if endpoint == "match_people_10000_open" and header.get("fixture_people_count_declared") != 10_000:
        raise Invalid("Match People endpoint requires the declared 10,000-People fixture")
    reset_spec = context_reset_spec(endpoint)
    initial_settings = None
    if endpoint == "operator_pause_feedback":
        if not records or records[0].get("record_type") != "initial_settings_setup":
            raise Invalid("pause capture requires initial rendered Settings setup before every warmup/reset")
        initial_settings = records[0]
        validate_initial_settings_setup(initial_settings)
        previous_finish = initial_settings["call_end_timestamp_us"]
        records = records[1:]
    setup_by_key: dict[tuple[str, int], dict[str, Any]] = {}
    interaction_rows = []
    for row in records:
        if row.get("record_type") == "context_setup":
            if reset_spec is None or set(row) != CONTEXT_SETUP_FIELDS:
                raise Invalid("unexpected or malformed interaction context setup record")
            key = (row.get("phase"), row.get("ordinal"))
            if key in setup_by_key:
                raise Invalid("interaction context setup is duplicated")
            _, expected_reset_endpoint, expected_reset_scope, expected_mode = reset_spec
            if (row.get("endpoint") != endpoint or row.get("reset_endpoint") != expected_reset_endpoint
                    or row.get("setup_outcome") != "applied"
                    or row.get("reported_endpoint_duration_scope") != expected_reset_scope
                    or row.get("reported_endpoint_current_state_confirmed") is not True
                    or row.get("reported_endpoint_rendered") is not True
                    or type(row.get("reported_endpoint_result_count")) is not int
                    or row.get("reported_endpoint_result_count") != 0
                    or row.get("reported_endpoint_query_present") is not False
                    or row.get("reported_endpoint_desired_mode") != expected_mode):
                raise Invalid("interaction context reset lacks the exact applied rendered-state receipt")
            if (not isinstance(row.get("receipt_action_id"), str)
                    or not ACTION_ID.fullmatch(row["receipt_action_id"])):
                raise Invalid("interaction context reset action_id must be a canonical UUID")
            timestamp = row.get("timestamp_us")
            ordinal = row.get("ordinal")
            if (row.get("phase") not in {"warmup", "measure"} or type(ordinal) is not int
                    or ordinal < 0 or type(timestamp) is not int or timestamp < 0):
                raise Invalid("interaction context reset phase, ordinal, or timestamp is invalid")
            if expected_reset_endpoint == "open_settings":
                if (type(row.get("requested_offset")) is not int or row.get("requested_offset") != 0
                        or type(row.get("applied_offset")) is not int or row.get("applied_offset") != 0
                        or type(row.get("page_limit")) is not int or row.get("page_limit") != 200):
                    raise Invalid("Settings context reset did not render the required first Match Settings page")
            elif any(row.get(field) is not None for field in ("requested_offset", "applied_offset", "page_limit")):
                raise Invalid("pause context reset must not carry navigation fields")
            setup_by_key[key] = row
        elif row.get("record_type") == "interaction":
            interaction_rows.append(row)
        else:
            raise Invalid("interaction capture contains an unknown record type")
    if reset_spec is not None and len(setup_by_key) != 220:
        raise Invalid("Settings/pause interaction requires one rendered context reset per call")
    if reset_spec is None and setup_by_key:
        raise Invalid("interaction endpoint must not include a context reset")
    if len(interaction_rows) != 220:
        raise Invalid("interaction requires exactly 20 warmup and 200 measured calls")
    expected_keys = ({("warmup", index) for index in range(20)}
                     | {("measure", index) for index in range(200)})
    interaction_keys = {(row.get("phase"), row.get("ordinal")) for row in interaction_rows}
    if interaction_keys != expected_keys:
        raise Invalid("interaction phase/ordinals must cover each warmup and measured call exactly once")
    if reset_spec is not None and set(setup_by_key) != expected_keys:
        raise Invalid("interaction context reset phase/ordinals must cover every call exactly once")
    expected_order = ([("warmup", index) for index in range(20)]
                      + [("measure", index) for index in range(200)])
    if [(row.get("phase"), row.get("ordinal")) for row in interaction_rows] != expected_order:
        raise Invalid("interaction order must be 20 warmup calls followed by 200 measured calls in ordinal order")
    if reset_spec is not None:
        expected_record_order = [(kind, phase, ordinal) for phase, ordinal in expected_order
                                 for kind in ("context_setup", "interaction")]
        if [(row.get("record_type"), row.get("phase"), row.get("ordinal"))
                for row in records] != expected_record_order:
            raise Invalid("each interaction must immediately follow its matching context setup in phase/ordinal order")
    seen_action_ids: set[str] = {initial_settings["receipt_action_id"]} if initial_settings is not None else set()
    for row in records:
        if row.get("record_type") != "interaction":
            continue
        allowed_fields = [INTERACTION_FIELDS]
        if endpoint == "match_people_10000_open":
            allowed_fields.append(INTERACTION_FIELDS | {"reported_catalog_evidence"})
        if set(row) not in allowed_fields:
            raise Invalid("interaction record fields do not match schema")
        if row.get("record_type") != "interaction" or row.get("endpoint") != endpoint:
            raise Invalid("interaction record type or endpoint mismatch")
        start = row.get("call_start_timestamp_us")
        finish = row.get("call_end_timestamp_us")
        duration = row.get("duration_us")
        if any(type(value) is not int for value in (start, finish, duration)) or start < 0 or finish <= start or duration != finish - start or start <= previous_start:
            raise Invalid("interaction timestamps/duration must be strictly increasing and consistent")
        if start < previous_finish:
            raise Invalid("interaction calls must not overlap the previous call interval")
        previous_start = start
        phase = row.get("phase")
        ordinal = row.get("ordinal")
        if phase not in {"warmup", "measure"} or type(ordinal) is not int or ordinal < 0:
            raise Invalid("interaction phase or ordinal is invalid")
        action_id = row.get("receipt_action_id")
        if not isinstance(action_id, str) or not ACTION_ID.fullmatch(action_id):
            raise Invalid("interaction action_id must be a canonical UUID")
        if action_id in seen_action_ids:
            raise Invalid("interaction action_id is duplicated")
        seen_action_ids.add(action_id)
        if reset_spec is not None:
            setup = setup_by_key.get((phase, ordinal))
            if (setup is None or row.get("context_setup_action_id") != setup.get("receipt_action_id")
                    or row.get("context_setup_timestamp_us") != setup.get("timestamp_us")
                    or setup["timestamp_us"] >= start
                    or setup["timestamp_us"] < previous_finish):
                raise Invalid("interaction is not preceded by its exact rendered context reset")
            if setup["receipt_action_id"] in seen_action_ids:
                raise Invalid("context setup and measured action IDs must be distinct")
            seen_action_ids.add(setup["receipt_action_id"])
        elif row.get("context_setup_action_id") is not None or row.get("context_setup_timestamp_us") is not None:
            raise Invalid("interaction without a context reset contains context setup fields")
        previous_finish = finish
        if row.get("endpoint_outcome") != "applied":
            raise Invalid("endpoint did not produce an applied terminal receipt")
        reported_duration = row.get("reported_endpoint_duration_us")
        reported_scope = row.get("reported_endpoint_duration_scope")
        if reported_duration is not None and (type(reported_duration) is not int or reported_duration < 0):
            raise Invalid("reported endpoint duration must be a nonnegative integer or null")
        if reported_duration is not None and reported_duration > duration:
            raise Invalid("reported UI endpoint duration exceeds its containing CLI-to-terminal interval")
        current_confirmed = row.get("reported_endpoint_current_state_confirmed")
        rendered = row.get("reported_endpoint_rendered")
        result_count = row.get("reported_endpoint_result_count")
        query_present = row.get("reported_endpoint_query_present")
        desired_mode = row.get("reported_endpoint_desired_mode")
        expected_query = endpoint == "cached_autocomplete"
        if endpoint in {"settings_manage_people_acknowledgement", "operator_pause_feedback"}:
            expected_mode = "operator_paused" if endpoint == "operator_pause_feedback" else None
            state_ok = (current_confirmed is True and rendered is True and query_present is False
                        and type(result_count) is int and result_count == 0
                        and desired_mode == expected_mode)
        else:
            state_ok = (current_confirmed is True and rendered is True and query_present is expected_query
                        and desired_mode is None and type(result_count) is int and result_count >= 0
                        and ((endpoint == "match_people_10000_open" and result_count <= 256)
                             or (endpoint == "cached_autocomplete" and result_count <= 32)))
        if phase == "warmup":
            warmup += 1
        elif phase == "measure":
            measured.append(duration)
            if expected_scope and reported_scope == expected_scope and type(reported_duration) is int and state_ok:
                endpoint_measured.append(reported_duration)
        else:
            raise Invalid("interaction phase must be warmup or measure")
    if warmup != 20 or len(measured) != 200:
        raise Invalid("interaction requires exactly 20 warmup and 200 measured calls")
    if header.get("metric_scope") != "facial_cli_invocation_to_terminal_applied_receipt":
        raise Invalid("interaction metric scope must identify CLI-to-receipt end-to-end timing")
    if type(end.get("observed_at_us")) is not int or end["observed_at_us"] < records[-1]["call_end_timestamp_us"]:
        raise Invalid("end observation timestamp must follow the final interaction call")
    result = {"endpoint": endpoint, "warmup_calls": warmup, "sample_count": len(measured),
              "cli_to_terminal_receipt_p50_us": percentile(measured, .50),
              "cli_to_terminal_receipt_p95_us": percentile(measured, .95),
              "cli_to_terminal_receipt_p99_us": percentile(measured, .99),
              "cli_to_terminal_receipt_max_us": max(measured),
              "raw_records_sha256": hashlib.sha256(b"".join(canonical_json_line(r) for r in raw_records)).hexdigest(),
              "metric_scope": header["metric_scope"], "verdict": "pending"}
    result["endpoint_duration_scope"] = expected_scope
    if initial_settings is not None:
        result["initial_settings_context_confirmed"] = True
        result["initial_settings_setup_scope"] = "rendered_match_settings_context_before_warmup_excluded_from_measured_endpoint_duration"
    result["endpoint_duration_sample_count"] = len(endpoint_measured)
    result["endpoint_duration_p95_us"] = percentile(endpoint_measured, .95) if len(endpoint_measured) == 200 else None
    result["endpoint_duration_max_us"] = max(endpoint_measured) if len(endpoint_measured) == 200 else None
    limits = {"match_people_10000_open": (200_000, None),
              "settings_manage_people_acknowledgement": (100_000, None),
              "cached_autocomplete": (100_000, 200_000),
              "operator_pause_feedback": (100_000, None)}
    if endpoint in limits:
        p95_limit, max_limit = limits[endpoint]
        result["performance_threshold_observed"] = (
            result["endpoint_duration_p95_us"] <= p95_limit
            and (max_limit is None or result["endpoint_duration_max_us"] <= max_limit)
            if result["endpoint_duration_p95_us"] is not None else None
        )
    else:
        result["performance_threshold_observed"] = None
    if expected_scope is None:
        result["endpoint_gate_verdict"] = "pending_combined_pause_route_and_safe_unit_proofs"
        result["gate_eligible"] = False
    elif result["endpoint_duration_sample_count"] != 200:
        result["endpoint_gate_verdict"] = "pending_missing_exact_ui_measurements"
        result["gate_eligible"] = False
    elif endpoint == "match_people_10000_open" and not analyze_catalog_observations(header, interaction_rows, end, seen_action_ids):
        result["endpoint_gate_verdict"] = "pending_independent_fixture_count_proof"
        result["gate_eligible"] = False
    else:
        result["endpoint_gate_verdict"] = "pass" if result["performance_threshold_observed"] else "fail"
        result["gate_eligible"] = True
    if endpoint == "match_people_10000_open" and result["gate_eligible"]:
        result["fixture_count_proof_scope"] = "canonical_nonhidden_count_at_each_rendered_snapshot_and_surrounding_diagnostics"
        result["fixture_manifest_sha256"] = header["fixture_manifest_sha256"]
        result["fixture_manifest_binding_scope"] = "supplied_manifest_artifact_not_catalog_membership_attestation"
        result["whole_interval_immutability_proven"] = False
    return result


def analyze_runtime_evidence(records: list[dict[str, Any]], budget: dict[str, int], governor_id: str,
                             measurement_start: int, measurement_end: int) -> dict[str, Any]:
    raw = [row for row in records if row.get("record_type") in {"runtime_sample", "runtime_checkpoint"}]
    if not raw:
        return {"verdict": "pending", "missing_fields": ["measured_common_clock_runtime_evidence"],
                "duration_contract_met": False}
    checkpoint_fields = {"record_type", "phase", "timestamp_us", "runtime_id", "timestamp_scope",
                         "captured_at_us", "governor_interval", "rings"}
    sample_fields = {"record_type", "phase", "timestamp_us", "endpoint", "runtime_id", "lifetime_id", "sample"}
    previous = None
    pending = []
    checkpoints = []
    measured_samples = {endpoint: [] for endpoint in RUNTIME_RING_SCOPES_V2}
    terminal_seen = False
    last_collector = -1
    for row in raw:
        if terminal_seen:
            raise Invalid("runtime evidence follows terminal checkpoint")
        if row["record_type"] == "runtime_sample":
            if set(row) != sample_fields or row.get("endpoint") not in RUNTIME_RING_SCOPES_V2:
                raise Invalid("runtime raw record fields mismatch")
            pending.append(row)
            if len(pending) > 256 * len(RUNTIME_RING_SCOPES_V2):
                raise Invalid("runtime raw checkpoint group exceeds bounded rings")
            continue
        if set(row) != checkpoint_fields or not isinstance(row["rings"], dict) or set(row["rings"]) not in (set(RUNTIME_RING_SCOPES), set(RUNTIME_RING_SCOPES_V2)):
            raise Invalid("runtime checkpoint fields mismatch")
        phase, stamp = row["phase"], u64(row["timestamp_us"], "runtime collector timestamp")
        if stamp <= last_collector or (previous is None and (phase != "baseline" or stamp != measurement_start)):
            raise Invalid("runtime collector checkpoint order/baseline mismatch")
        if phase == "terminal" and stamp != measurement_end:
            raise Invalid("runtime terminal does not bind measured collector end")
        last_collector = stamp
        snapshot = {key: row[key] for key in ("runtime_id", "timestamp_scope", "captured_at_us", "governor_interval")}
        snapshot["schema_version"] = 2 if "worker_control" in row["rings"] else 1
        scopes = runtime_scopes(snapshot["schema_version"])
        if any(item["endpoint"] not in scopes for item in pending):
            raise Invalid("sample endpoint contradicts checkpoint producer schema")
        for endpoint in scopes:
            meta = row["rings"][endpoint]
            if not isinstance(meta, dict) or set(meta) != RUNTIME_RING_FIELDS - {"samples"}:
                raise Invalid("runtime ring checkpoint fields mismatch")
            new = [item["sample"] for item in pending if item["endpoint"] == endpoint]
            retained = [] if previous is None else previous[endpoint]["samples"]
            snapshot[endpoint] = {**meta, "samples": (retained + new)[-256:]}
        expected, current = runtime_evidence_records({"runtime_evidence": snapshot}, phase, stamp, previous)
        if expected != [*pending, row]:
            raise Invalid("runtime raw samples do not match exact checkpoint identity/sequence/order")
        if current["governor_interval"]["lifetime_id"] != governor_id:
            raise Invalid("runtime interval governor identity differs from canonical live governor")
        if previous is not None:
            interval = current["governor_interval"]
            lease_samples = [item["sample"] for item in pending if item["endpoint"] == "lease_activity"]
            names = {"acquisitions": "acquire", "replacements": "replace", "releases": "release",
                     "preparation_releases": "preparation_release", "pressure_events": "pressure"}
            if any(interval[key] != sum(sample["event"] == name for sample in lease_samples)
                   for key, name in names.items()):
                raise Invalid("governor interval counters disagree with retained raw lease activity")
            for sample in lease_samples:
                if (sample["timestamp_us"] > interval["end_us"]
                        or any(sample["usage"][axis] > interval["peak_usage"][axis] for axis in TELEMETRY_AXES)):
                    raise Invalid("raw lease activity lies outside interval clock/peaks")
            closing = lease_samples[-1] if lease_samples else None
            for suffix, sample_key in (("usage", "usage"), ("live_stage_leases", "live_stage_leases"),
                                       ("unclassified_leases", "unclassified_leases")):
                expected_closing = closing[sample_key] if closing else interval[f"opening_{suffix}"]
                if interval[f"closing_{suffix}"] != expected_closing:
                    raise Invalid("raw lease activity does not reconcile interval closing state")
            for item in pending:
                if item["sample"]["timestamp_us"] < checkpoints[0]["governor_interval"]["end_us"]:
                    raise Invalid("measured runtime sample precedes root measurement boundary")
                measured_samples[item["endpoint"]].append(item["sample"])
        checkpoints.append(current)
        previous, pending = current, []
        terminal_seen = phase == "terminal"
    if pending or not terminal_seen or len(checkpoints) < 2:
        raise Invalid("runtime evidence lacks closed terminal checkpoint")
    start = checkpoints[0]["governor_interval"]["end_us"]
    stop = checkpoints[-1]["governor_interval"]["end_us"]
    intervals = [checkpoint["governor_interval"] for checkpoint in checkpoints[1:]]
    peaks = {axis: max(interval["peak_usage"][axis] for interval in intervals) for axis in TELEMETRY_AXES}
    exceeded = [axis for axis in TELEMETRY_AXES if peaks[axis] > budget[axis]]
    counters = {key: sum(interval[key] for interval in intervals) for key in TELEMETRY_COUNTERS}
    missing = []
    if stop - start < 600_000_000:
        missing.append("canonical_600s_root_clock_measured_interval")
    if counters["pressure_events"] == 0:
        missing.append("genuine_measured_governor_pressure")
    unexercised = [axis for axis in TELEMETRY_AXES if budget[axis] > 0 and peaks[axis] == 0]
    native = measured_samples["native_playback"]
    advancing = []
    for before, after in zip(native, native[1:]):
        if (before["player_generation"] == after["player_generation"]
                and all(sample["status"] == "playing" and sample["native_player_present"]
                        and sample["native_playing"] and sample["clock_available"] for sample in (before, after))
                and start <= before["poll_start_us"] <= after["poll_end_us"] <= stop
                and after["time_ms"] > before["time_ms"]):
            advancing.append((before["poll_end_us"], after["poll_start_us"]))
    if not advancing:
        missing.append("raw_native_playing_and_same_generation_advancing_clock")
    leases = measured_samples["lease_activity"]
    lease_spans = []
    cursor = start
    stages = checkpoints[0]["governor_interval"]["closing_live_stage_leases"]
    for sample in leases:
        if any(stages[index] for index in (1, 2, 3)) and sample["timestamp_us"] > cursor:
            lease_spans.append((cursor, sample["timestamp_us"]))
        cursor, stages = sample["timestamp_us"], sample["live_stage_leases"]
    if any(stages[index] for index in (1, 2, 3)) and stop > cursor:
        lease_spans.append((cursor, stop))
    overlap_us = 0
    lease_index = native_index = 0
    while lease_index < len(lease_spans) and native_index < len(advancing):
        lease_start, lease_end = lease_spans[lease_index]
        native_start, native_end = advancing[native_index]
        overlap_us += max(0, min(lease_end, native_end) - max(lease_start, native_start))
        if lease_end <= native_end:
            lease_index += 1
        else:
            native_index += 1
    if overlap_us == 0:
        missing.append("admitted_index_stage_lease_and_native_observation_span_overlap")
    final = intervals[-1]
    drain = (all(value == 0 for value in final["closing_usage"].values())
             and not any(final["closing_live_stage_leases"]) and final["closing_unclassified_leases"] == 0)
    native_stopped = bool(native) and native[-1]["native_playing"] is False and native[-1]["status"] in {"stopped", "ended"}
    if not drain:
        missing.append("observed_terminal_all_resources_and_stage_leases_zero")
    if not native_stopped:
        missing.append("observed_terminal_native_stopped")
    # Stop provenance and execution are absent from these admitted-lease/raw-poll producers.
    missing.extend(["actual_workload_stop_and_final_drain", "actual_indexing_kernel_execution_overlap",
                    "whole_interval_simultaneous_visible_workload"])
    ceiling_missing = [name for name in missing if name in {
        "canonical_600s_root_clock_measured_interval", "genuine_measured_governor_pressure"}]
    return {"verdict": "fail" if exceeded else ("pending" if ceiling_missing else "pass"),
            "scope": "measured_governor_intervals_excluding_warmup",
            "producer_schema_version": previous["schema_version"],
            "runtime_id": checkpoints[0]["runtime_id"], "governor_lifetime_id": governor_id,
            "measurement_start_us": start, "measurement_end_us": stop, "interval_count": len(intervals),
            "duration_contract_met": stop - start >= 600_000_000,
            "configured_ceilings": budget, "observed_measured_peaks": peaks, "exceeded_axes": exceeded,
            "measured_counter_totals": counters, "unexercised_enabled_axes": unexercised,
            "saturation_axis_coverage_verdict": "pending" if unexercised or counters["pressure_events"] == 0 else "pass",
            "lease_evidence_scope": RUNTIME_RING_SCOPES["lease_activity"],
            "native_evidence_scope": RUNTIME_RING_SCOPES["native_playback"],
            "lease_native_observation_span_overlap_us": overlap_us,
            "lease_native_overlap_scope": "observed_playing_clock_pair_span_intersected_with_admitted_detect_align_embed_leases_excluding_kernel_execution_and_unobserved_playback_state",
            "raw_native_advancing_pairs": len(advancing), "observed_terminal_resources_zero": drain,
            "observed_terminal_native_stopped": native_stopped, "missing_fields": sorted(missing)}


def common_clock_visible_records(records: list[dict[str, Any]], runtime: dict[str, Any]) -> list[dict[str, Any]]:
    visible = []
    for row in records:
        if row.get("record_type") not in {"visible_work_sample", "visible_work_checkpoint"}:
            continue
        if (row.get("runtime_id") != runtime.get("runtime_id") or row.get("timestamp_scope") != RUNTIME_TIMESTAMP_SCOPE):
            raise Invalid("visible sample runtime/common-clock identity mismatch")
        fields = VISIBLE_EVENT_FIELDS if row["record_type"] == "visible_work_sample" else VISIBLE_CHECKPOINT_FIELDS
        if set(row) != fields | {"runtime_id", "timestamp_scope"}:
            raise Invalid("common-clock visible record fields mismatch")
        stamp_key = "timestamp_us" if row["record_type"] == "visible_work_checkpoint" else "observed_at_us"
        checkpoints = [item for item in records if item.get("record_type") == "runtime_checkpoint"
                       and item.get("timestamp_us") == row[stamp_key]]
        if len(checkpoints) != 1:
            raise Invalid("visible record lacks same-poll runtime checkpoint")
        if row["record_type"] == "visible_work_checkpoint" and row["captured_at_us"] > checkpoints[0]["captured_at_us"]:
            raise Invalid("visible capture exceeds root capture clock")
        visible.append({key: value for key, value in row.items() if key in fields})
    return visible


def analyze_concurrency(header: dict[str, Any], records: list[dict[str, Any]], end: dict[str, Any]) -> dict[str, Any]:
    allowed_record_types = {"concurrency", "governor_evidence", "visible_work_checkpoint", "visible_work_sample",
                            "runtime_checkpoint", "runtime_sample"}
    if any(row.get("record_type") not in allowed_record_types for row in records):
        raise Invalid("concurrency capture contains an unknown record type")
    if (header.get("visible_work_schema_version") not in {1, 2}
            or header.get("visible_work_sample_semantics") != "per_endpoint_measurement_baseline_then_each_new_success_sequence_once"):
        raise Invalid("concurrency header does not declare the supported visible_work sample semantics")
    rows = [row for row in records if row.get("record_type") == "concurrency"]
    evidence = [row for row in records if row.get("record_type") == "governor_evidence"]
    if len(evidence) != 1 or set(evidence[0]) != GOVERNOR_EVIDENCE_FIELDS:
        raise Invalid("concurrency requires one bounded governor baseline/end evidence record")
    evidence = evidence[0]
    warm = 0
    measured_rows = []
    last_ts = -1
    seen_measure = False
    for row in rows:
        if set(row) != CONCURRENCY_RECORD_FIELDS:
            raise Invalid("concurrency record fields do not match schema")
        ts = row.get("timestamp_us")
        if type(ts) is not int or ts <= last_ts:
            raise Invalid("concurrency timestamps must be strictly increasing")
        last_ts = ts
        if row.get("phase") == "warmup":
            if seen_measure:
                raise Invalid("warmup rows cannot follow measured concurrency rows")
            warm += 1
        elif row.get("phase") == "measure":
            seen_measure = True
            measured_rows.append(row)
        else:
            raise Invalid("concurrency phase must be warmup or measure")
        for field in CONCURRENCY_FIELDS:
            if field not in row:
                raise Invalid(f"concurrency record missing {field}; unavailable data must be explicit null")
        progress = row["indexing_progress"]
        if row["indexing_progress_scope"] != "canonical_all_index_jobs":
            raise Invalid("indexing_progress_scope must be canonical_all_index_jobs")
        if progress is not None and (not isinstance(progress, dict)
                or set(progress) != {"discovered", "completed", "failed", "skipped"}
                or any(type(value) is not int or value < 0 or value > 2**64 - 1 for value in progress.values())):
            raise Invalid("indexing_progress must contain exact nonnegative runtime counts or null")
        resources = row["admitted_resources"]
        if resources is not None and (not isinstance(resources, dict) or set(resources) != set(TELEMETRY_AXES)
                or any(type(value) is not int or value < 0 or value > 2**64 - 1 for value in resources.values())):
            raise Invalid("admitted_resources must contain exact runtime axes or null")
        holds = row["active_holds"]
        if holds is not None and (not isinstance(holds, list) or any(not isinstance(value, str) for value in holds)):
            raise Invalid("active_holds must be a list of current reason names or null")
        deltas = row["lease_deltas"]
        if deltas is not None and (not isinstance(deltas, dict) or set(deltas) != set(TELEMETRY_COUNTERS)
                or any(type(value) is not int or value < 0 or value > 2**64 - 1 for value in deltas.values())):
            raise Invalid("lease_deltas must contain exact nonnegative runtime counters or null")
        if (not isinstance(row["index_stage"], str)
                or row["index_stage_scope"] != "persisted_asset_next_stage_counts"
                or not isinstance(row["index_stage_counts"], dict)
                or any(not isinstance(stage, str) or type(value) is not int or value < 0 or value > 2**64 - 1
                       for stage, value in row["index_stage_counts"].items())):
            raise Invalid("index-stage fields must use persisted-asset next-stage counts scope")
    measured = measured_rows
    if not measured or warm < 1:
        raise Invalid("concurrency capture is missing warmup or measured rows")
    missing = sorted({field for row in measured for field in CONCURRENCY_FIELDS if row.get(field) is None})
    measured_start = measured[0]["timestamp_us"]
    measured_end = measured[-1]["timestamp_us"]
    end_us = end.get("observed_at_us")
    if type(end_us) is not int or end_us < measured_end:
        raise Invalid("concurrency end timestamp must follow the final sample")
    complete_window = measured_start >= 60_000_000 and end_us - measured_start >= 600_000_000
    governor = verify_governor_evidence(evidence["resource_budget"],
                                       evidence["baseline_resource_telemetry"],
                                       evidence["terminal_resource_telemetry"])
    if (type(evidence["measurement_start_us"]) is not int
            or type(evidence["measurement_end_us"]) is not int
            or evidence["measurement_start_us"] != measured_start
            or evidence["measurement_end_us"] < end_us):
        raise Invalid("governor telemetry interval does not bind to the measured concurrency interval")
    runtime = analyze_runtime_evidence(records, evidence["resource_budget"], governor["lifetime_id"], measured_start, measured_end)
    if header.get("visible_work_schema_version") == 2:
        if header.get("runtime_evidence_schema_version") not in (1, 2) or header.get("runtime_evidence_schema_version") != runtime.get("producer_schema_version") or not runtime.get("runtime_id"):
            raise Invalid("common-clock visible capture lacks declared actual runtime interval evidence")
        visible_records = common_clock_visible_records(records, runtime)
    else:
        if any(row.get("record_type") in {"runtime_checkpoint", "runtime_sample"} for row in records):
            raise Invalid("runtime evidence requires visible common-clock schema version 2")
        visible_records = records
    visible = analyze_visible_work(visible_records, measured_start, end_us,
                                  (runtime["measurement_start_us"], runtime["measurement_end_us"])
                                  if header.get("visible_work_schema_version") == 2 else None)
    first_progress = measured[0].get("indexing_progress")
    last_progress = measured[-1].get("indexing_progress")
    progress_delta = None
    throughput = None
    if isinstance(first_progress, dict) and isinstance(last_progress, dict):
        previous_progress = first_progress
        for row in measured[1:]:
            progress = row.get("indexing_progress")
            if not isinstance(progress, dict) or any(progress[key] < previous_progress[key] for key in previous_progress):
                raise Invalid("canonical indexing progress reset or unavailable within measured interval")
            previous_progress = progress
        progress_delta = {}
        for key in ("discovered", "completed", "failed", "skipped"):
            before, after = first_progress.get(key), last_progress.get(key)
            if type(before) is int and type(after) is int and after >= before:
                progress_delta[key] = after - before
            else:
                progress_delta = None
                break
        if progress_delta is not None and end_us > measured_start:
            throughput = progress_delta["completed"] * 1_000_000 / (end_us - measured_start)
    known_failure = bool(governor["exceeded_axes"]) or not governor["terminal_lease_balance"] or runtime["verdict"] == "fail"
    return {"warmup_samples": warm, "measured_samples": len(measured),
            "measurement_start_us": measured_start, "measurement_end_us": measured_end,
            "indexing_progress_delta": progress_delta,
            "indexing_progress_scope": "canonical_all_index_jobs",
            "completed_assets_per_second": throughput,
            "failed_or_skipped_assets_delta": (progress_delta["failed"] + progress_delta["skipped"]
                                                if progress_delta is not None else None),
            "missing_fields": sorted(set(missing + visible["missing_fields"] + runtime["missing_fields"] + [
                "thumbnail_budget_verdict", "playback_budget_verdict", "navigation_budget_verdict",
                "independent_simultaneous_workload_evidence",
            ])),
            "visible_work": visible["series"],
            "simultaneous_workload_evidence": "pending_independent_runtime_review",
            "visible_work_budget_verdicts": {name: "pending" for name in VISIBLE_WORK_SCOPES},
            "governor_telemetry": governor,
            "measured_governor_component_verdict": runtime["verdict"], "measured_runtime_evidence": runtime,
            "terminal_lease_balance": governor["terminal_lease_balance"],
            "duration_contract_met": complete_window and visible["duration_contract_met"] and (
                runtime["duration_contract_met"] if header.get("visible_work_schema_version") == 2 else True),
            "verdict": ("fail" if known_failure else "pending"),
            "gate_eligible": False,
            "reason": "WP-087 requires the real simultaneous indexing/playback/thumbnail/navigation workload and its existing visible-work budget verdicts; endpoint samples and status polling alone do not prove them."}


def analyze_visible_work(records: list[dict[str, Any]], measurement_start_us: int,
                         measurement_end_us: int, root_window: tuple[int, int] | None = None) -> dict[str, Any]:
    checkpoints = [row for row in records if row.get("record_type") == "visible_work_checkpoint"]
    events = [row for row in records if row.get("record_type") == "visible_work_sample"]
    if any(set(row) != VISIBLE_CHECKPOINT_FIELDS for row in checkpoints):
        raise Invalid("visible_work checkpoint fields do not match the collector schema")
    if any(set(row) != VISIBLE_EVENT_FIELDS for row in events):
        raise Invalid("visible_work sample fields do not match the collector schema")
    expected_endpoints = set(VISIBLE_WORK_SCOPES)
    groups: dict[tuple[str, str, int], list[dict[str, Any]]] = {}
    for row in checkpoints:
        endpoint = row.get("endpoint")
        phase = row.get("phase")
        timestamp = row.get("timestamp_us")
        if endpoint not in expected_endpoints or phase not in {"baseline", "measure", "terminal"}:
            raise Invalid("visible_work checkpoint endpoint/phase is invalid")
        if type(timestamp) is not int or timestamp < 0 or row.get("overflow") is not False:
            raise Invalid("visible_work checkpoint timestamp/overflow is invalid")
        if row.get("endpoint_scope") != VISIBLE_WORK_SCOPES[endpoint]:
            raise Invalid(f"visible_work {endpoint} checkpoint scope mismatch")
        try:
            uuid.UUID(row["lifetime_id"])
        except (ValueError, TypeError, AttributeError) as error:
            raise Invalid(f"visible_work {endpoint} checkpoint lifetime_id is invalid") from error
        for key in ("captured_at_us", "sequence", "dropped_records", "abandoned", "pending"):
            value = row.get(key)
            if type(value) is not int or value < 0 or value > 2**64 - 1:
                raise Invalid(f"visible_work {endpoint} checkpoint {key} must be a u64")
        groups.setdefault((phase, "", timestamp), []).append(row)
    if not checkpoints:
        raise Invalid("visible_work baseline, polls, and terminal checkpoint are required")
    for (_, _, timestamp), group in groups.items():
        if len(group) != len(expected_endpoints) or {row["endpoint"] for row in group} != expected_endpoints:
            raise Invalid(f"visible_work checkpoint at {timestamp} is missing or duplicates a series")
    phase_order = {"baseline": 0, "measure": 1, "terminal": 2}
    checkpoints.sort(key=lambda row: (row["timestamp_us"], phase_order[row["phase"]], row["endpoint"]))
    events_by_group: dict[tuple[str, str, int], list[dict[str, Any]]] = {}
    seen_events: set[tuple[str, str, int]] = set()
    for event in events:
        endpoint = event.get("endpoint")
        phase = event.get("phase")
        observed = event.get("observed_at_us")
        if endpoint not in expected_endpoints or phase not in {"measure", "terminal"}:
            raise Invalid("visible_work sample endpoint/phase is invalid")
        try:
            uuid.UUID(event["lifetime_id"])
        except (ValueError, TypeError, AttributeError) as error:
            raise Invalid("visible_work sample lifetime_id is invalid") from error
        if type(observed) is not int or observed < 0:
            raise Invalid("visible_work sample observation timestamp is invalid")
        for key in ("sequence", "start_us", "end_us", "duration_us"):
            value = event.get(key)
            if type(value) is not int or value < 0 or value > 2**64 - 1:
                raise Invalid(f"visible_work sample {key} must be a u64")
        if event["end_us"] < event["start_us"] or event["duration_us"] != event["end_us"] - event["start_us"]:
            raise Invalid("visible_work sample duration is inconsistent")
        key = (endpoint, event["lifetime_id"], event["sequence"])
        if key in seen_events:
            raise Invalid("visible_work sample sequence was captured more than once")
        seen_events.add(key)
        events_by_group.setdefault((phase, endpoint, observed), []).append(event)
    state: dict[str, dict[str, Any]] = {}
    endpoint_checkpoints: dict[str, list[dict[str, Any]]] = {name: [] for name in expected_endpoints}
    for checkpoint in checkpoints:
        endpoint = checkpoint["endpoint"]
        phase = checkpoint["phase"]
        timestamp = checkpoint["timestamp_us"]
        key = (phase, endpoint, timestamp)
        group_events = events_by_group.pop(key, [])
        group_events.sort(key=lambda row: row["sequence"])
        previous = state.get(endpoint)
        if previous is None:
            if phase != "baseline" or group_events:
                raise Invalid("visible_work must begin at an empty per-series baseline")
        else:
            if phase == "baseline":
                raise Invalid("visible_work baseline may occur only once per endpoint")
            if phase == "measure" and endpoint_checkpoints[endpoint][-1]["phase"] == "terminal":
                raise Invalid("visible_work measurement checkpoint follows terminal")
            if checkpoint["lifetime_id"] != previous["lifetime_id"]:
                raise Invalid(f"visible_work {endpoint} lifetime changed")
            if checkpoint["captured_at_us"] <= previous["captured_at_us"] or timestamp <= previous["timestamp_us"]:
                raise Invalid(f"visible_work {endpoint} checkpoint clocks must strictly increase")
            for counter in ("sequence", "dropped_records", "abandoned"):
                if checkpoint[counter] < previous[counter]:
                    raise Invalid(f"visible_work {endpoint} checkpoint {counter} reset")
            delta = checkpoint["sequence"] - previous["sequence"]
            if delta > 256:
                raise Invalid(f"visible_work {endpoint} sequence gap exceeds retained-ring capacity")
            expected_sequences = list(range(previous["sequence"] + 1, checkpoint["sequence"] + 1))
            if [row["sequence"] for row in group_events] != expected_sequences:
                raise Invalid(f"visible_work {endpoint} sequence gap is not completely covered by samples")
            if any(row["lifetime_id"] != checkpoint["lifetime_id"] or row["end_us"] > checkpoint["captured_at_us"]
                   for row in group_events):
                raise Invalid(f"visible_work {endpoint} sample is outside its checkpoint lifetime/time")
        if phase == "terminal" and any(row["endpoint"] == endpoint and row["phase"] != "terminal"
                                        and row["observed_at_us"] == timestamp for row in events):
            raise Invalid("visible_work terminal checkpoint has samples from another phase")
        state[endpoint] = checkpoint
        endpoint_checkpoints[endpoint].append(checkpoint)
    if events_by_group:
        raise Invalid("visible_work sample has no matching checkpoint")
    summaries = {}
    missing = []
    all_durations_met = True
    for endpoint in sorted(expected_endpoints):
        series_rows = endpoint_checkpoints[endpoint]
        if (not series_rows or series_rows[0]["phase"] != "baseline"
                or series_rows[-1]["phase"] != "terminal"
                or sum(row["phase"] == "baseline" for row in series_rows) != 1
                or sum(row["phase"] == "terminal" for row in series_rows) != 1):
            raise Invalid(f"visible_work {endpoint} requires exactly one baseline and terminal checkpoint")
        baseline, terminal = series_rows[0], series_rows[-1]
        if baseline["timestamp_us"] != measurement_start_us or terminal["timestamp_us"] > measurement_end_us:
            raise Invalid(f"visible_work {endpoint} checkpoints do not bind to the measured run boundaries")
        interval_end = baseline["captured_at_us"] + 600_000_000
        duration_met = terminal["captured_at_us"] >= interval_end
        all_durations_met = all_durations_met and duration_met
        measured_values = []
        preexisting_completions = 0
        boundary_crossings = 0
        outside_root_interval = 0
        for row in events:
            if row["endpoint"] != endpoint or row["lifetime_id"] != baseline["lifetime_id"]:
                continue
            if root_window is not None and not root_window[0] <= row["start_us"] <= row["end_us"] <= root_window[1]:
                outside_root_interval += 1
                continue
            if row["start_us"] < baseline["captured_at_us"]:
                preexisting_completions += 1
            elif baseline["captured_at_us"] <= row["start_us"] < interval_end:
                if row["end_us"] <= interval_end:
                    measured_values.append(row["duration_us"])
                else:
                    boundary_crossings += 1
        abandoned_delta = terminal["abandoned"] - baseline["abandoned"]
        incomplete = terminal["pending"] > 0 or abandoned_delta > 0 or boundary_crossings > 0
        if not measured_values:
            missing.append(f"visible_work.{endpoint}.measured_samples")
        if not duration_met:
            missing.append(f"visible_work.{endpoint}.600_second_interval")
        if incomplete:
            missing.append(f"visible_work.{endpoint}.pending_or_abandoned")
        summaries[endpoint] = {
            "lifetime_id": baseline["lifetime_id"], "endpoint_scope": baseline["endpoint_scope"],
            "measurement_start_us": baseline["captured_at_us"], "measurement_end_us": interval_end,
            "observed_end_us": terminal["captured_at_us"], "duration_contract_met": duration_met,
            "sample_count": len(measured_values),
            "p50_us": percentile(measured_values, .50) if measured_values else None,
            "p95_us": percentile(measured_values, .95) if measured_values else None,
            "p99_us": percentile(measured_values, .99) if measured_values else None,
            "max_us": max(measured_values) if measured_values else None,
            "preexisting_completions_excluded": preexisting_completions,
            "outside_root_interval_excluded": outside_root_interval,
            "boundary_crossings": boundary_crossings,
            "dropped_records_delta": terminal["dropped_records"] - baseline["dropped_records"],
            "abandoned_delta": abandoned_delta, "terminal_pending": terminal["pending"],
            "complete": not incomplete and duration_met and bool(measured_values),
        }
    return {"series": summaries, "missing_fields": missing, "duration_contract_met": all_durations_met}


def analyze_saturation(header: dict[str, Any], records: list[dict[str, Any]], end: dict[str, Any] | None = None) -> dict[str, Any]:
    if header.get("runtime_evidence_schema_version") in (1, 2):
        if end is None:
            raise Invalid("measured saturation interval requires actual terminal capture")
        result = analyze_concurrency(header, records, end)
        runtime = result["measured_runtime_evidence"]
        result["governor_component_verdict"] = runtime["verdict"]
        result["saturation_axis_coverage_verdict"] = runtime.get("saturation_axis_coverage_verdict", "pending")
        result["missing_fields"] = sorted(set(result["missing_fields"] + [
            f"saturation.enabled_axis_not_exercised.{axis}" for axis in runtime.get("unexercised_enabled_axes", [])]))
        result["workload_verdict"] = "pending"
        return result
    required = {"record_type", "resource_budget", "baseline_resource_telemetry",
                "terminal_resource_telemetry", "thumbnail_budget_verdict", "playback_budget_verdict",
                "navigation_budget_verdict"}
    if len(records) != 1 or set(records[0]) != required or records[0].get("record_type") != "saturation":
        raise Invalid("saturation requires one closed terminal resource summary record")
    row = records[0]
    governor = verify_governor_evidence(row["resource_budget"], row["baseline_resource_telemetry"],
                                       row["terminal_resource_telemetry"])
    missing = governor["missing_fields"]
    budget_keys = ("thumbnail_budget_verdict", "playback_budget_verdict", "navigation_budget_verdict")
    allowed_verdicts = {"pass", "fail", "pending"}
    if any(not isinstance(row[key], str) or row[key] not in allowed_verdicts for key in budget_keys):
        missing.extend(key for key in budget_keys if not isinstance(row[key], str) or row[key] not in allowed_verdicts)
    exceeded = governor["exceeded_axes"]
    known_failure = bool(exceeded) or not governor["terminal_lease_balance"] or any(row[key] == "fail" for key in budget_keys)
    governor_verdict = "fail" if exceeded or not governor["terminal_lease_balance"] else ("pending" if missing else "pass")
    # A closed summary and caller-entered budget labels cannot establish an
    # independently verified, simultaneous 600-second visible workload.
    missing.append("independent_simultaneous_workload_binding")
    verdict = "fail" if known_failure else "pending"
    return {"configured_ceilings": governor["configured_ceilings"],
            "observed_peaks": governor["observed_lifetime_peaks"], "missing_fields": sorted(set(missing)),
            "exceeded_axes": exceeded, "backpressure_events": governor["backpressure_events_delta"],
            "terminal_lease_balance": governor["terminal_lease_balance"],
            "visible_work_budgets": {key: row[key] for key in budget_keys},
            "governor_component_verdict": governor_verdict, "workload_verdict": "pending",
            "verdict": verdict, "gate_eligible": False}


def verify_governor_evidence(budget: Any, baseline: Any, terminal: Any) -> dict[str, Any]:
    if not isinstance(budget, dict) or set(budget) != TELEMETRY_BUDGET_FIELDS:
        raise Invalid("governor resource budget does not match runtime axes")
    if any(type(value) is not int or value < 0 or value > 2**64 - 1 for value in budget.values()):
        raise Invalid("governor resource ceilings must contain u64 values")
    if not isinstance(baseline, dict) or not isinstance(terminal, dict):
        raise Invalid("governor baseline and terminal telemetry are required")
    for label, telemetry in (("baseline", baseline), ("terminal", terminal)):
        if set(telemetry) != TELEMETRY_FIELDS:
            raise Invalid(f"{label} telemetry fields do not match the runtime schema")
        try:
            uuid.UUID(telemetry["lifetime_id"])
        except (ValueError, TypeError, AttributeError) as error:
            raise Invalid(f"{label} lifetime_id must be a UUID") from error
        if telemetry["scope"] != "governor_lifetime_including_warmup":
            raise Invalid("governor peaks must retain their lifetime-including-warmup scope")
        if telemetry["overflow"] is not False:
            raise Invalid("governor telemetry overflow invalidates the run")
        for field in ("current_usage", "peak_usage"):
            values = telemetry[field]
            if not isinstance(values, dict) or set(values) != set(TELEMETRY_AXES):
                raise Invalid(f"{label} {field} axes do not match runtime schema")
            if any(type(value) is not int or value < 0 or value > 2**64 - 1 for value in values.values()):
                raise Invalid(f"{label} {field} must contain u64 values")
        if any(type(telemetry[key]) is not int or telemetry[key] < 0 or telemetry[key] > 2**64 - 1 for key in TELEMETRY_COUNTERS):
            raise Invalid(f"{label} counters must contain u64 values")
    if baseline["lifetime_id"] != terminal["lifetime_id"]:
        raise Invalid("governor lifetime changed during the measured run")
    deltas = {}
    for counter in TELEMETRY_COUNTERS:
        if terminal[counter] < baseline[counter]:
            raise Invalid(f"governor counter reset: {counter}")
        deltas[counter] = terminal[counter] - baseline[counter]
    for axis in TELEMETRY_AXES:
        if terminal["peak_usage"][axis] < baseline["peak_usage"][axis]:
            raise Invalid(f"governor peak reset: {axis}")
    wpruntime = {
        "admitted_items": "admitted_items", "aggregate_queue_items": "queued_items",
        "aggregate_queue_bytes": "queued_bytes", "cpu_inference_concurrency": "cpu_inference",
        "decoded_image_bytes": "decoded_bytes", "gpu_vram_bytes_if_enabled": "gpu_vram_bytes",
        "surrealdb_write_concurrency": "surreal_writes", "vector_index_build_concurrency": "vector_index_builds",
    }
    ceilings = {name: budget[runtime] for name, runtime in wpruntime.items()}
    peaks = {name: terminal["peak_usage"][runtime] for name, runtime in wpruntime.items()}
    exceeded = [name for name in wpruntime if peaks[name] > ceilings[name]]
    balance = all(value == 0 for value in terminal["current_usage"].values()) and terminal["acquisitions"] == terminal["releases"]
    return {"lifetime_id": terminal["lifetime_id"], "scope": terminal["scope"],
            "counter_deltas": deltas, "configured_ceilings": ceilings, "observed_lifetime_peaks": peaks,
            "worker_memory_peak_bytes": terminal["peak_usage"]["worker_memory_bytes"],
            "worker_memory_ceiling_bytes": budget["worker_memory_bytes"],
            "exceeded_axes": exceeded, "backpressure_events_delta": deltas["pressure_events"],
            "terminal_current_usage": terminal["current_usage"], "terminal_lease_balance": balance,
            "missing_fields": []}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, epilog=(
        "No-context sequence: (1) use only generated media/fictitious people and prepare the "
        "fixture, hardware manifest, and packaged release; (2) keep that packaged GUI running "
        "with the same workspace/API root; (3) run `python product/scripts/match-nonrender-benchmark.py "
        "collect-interaction --run-id RUN --workspace-root WORKSPACE --api-root API_ROOT "
        "--facial-cli CLI --packaged-portable PORTABLE --hardware-manifest HARDWARE.json "
        "--fixture-manifest FIXTURE.json --endpoint ENDPOINT`; autocomplete additionally needs "
        "--query QUERY --media-key KEY --catalog-revision REV and optional --face-id ID; (4) for "
        "concurrency, start the contract's real indexing, Viewer playback, visible thumbnails, and "
        "navigation load first, then run `collect-concurrency` with the same paths plus a unique "
        "run ID; it only polls the same GUI service and observes for 60+600 seconds; (5) run "
        "`python product/scripts/match-nonrender-benchmark.py analyze --input WORKSPACE/.facial/benchmarks/RUN.jsonl`. "
        "Each capture uses create-new output. Preserve interrupted/invalid evidence and retry with a "
        "fresh run ID; do not analyze incomplete runs. Exact unavailable endpoints fail closed; "
        "autocomplete never substitutes MediaSearch. A pending result needs the missing real runtime "
        "inputs; this tool does not fabricate saturation or visible-work outcomes."
    ))
    sub = parser.add_subparsers(dest="command", required=True)
    collect = sub.add_parser("collect-interaction", help="collect one exact receipt-backed WP-087 interaction endpoint")
    collect.add_argument("--run-id", required=True)
    collect.add_argument("--workspace-root", required=True)
    collect.add_argument("--api-root", required=True)
    collect.add_argument("--facial-cli", required=True)
    collect.add_argument("--packaged-portable", required=True, help="runtime executable artifact; declare its kind explicitly for an unpackaged component")
    collect.add_argument("--runtime-artifact-kind", choices=("packaged_portable", "unpackaged_component"), default="packaged_portable")
    collect.add_argument("--hardware-manifest", required=True)
    collect.add_argument("--fixture-manifest", required=True)
    collect.add_argument("--endpoint", choices=sorted(REQUIRED_INTERACTION_ENDPOINTS), required=True)
    collect.add_argument("--fixture-people-count", type=int)
    collect.add_argument("--query")
    collect.add_argument("--media-key")
    collect.add_argument("--catalog-revision", type=int)
    collect.add_argument("--face-id")
    collect.add_argument("--timeout-s", type=float, default=30.0)
    concurrency = sub.add_parser("collect-concurrency", help="capture bounded 60-second warmup and 600-second live Match runtime telemetry")
    concurrency.add_argument("--run-id", required=True)
    concurrency.add_argument("--workspace-root", required=True)
    concurrency.add_argument("--api-root", required=True)
    concurrency.add_argument("--facial-cli", required=True)
    concurrency.add_argument("--packaged-portable", required=True, help="runtime executable artifact; declare its kind explicitly for an unpackaged component")
    concurrency.add_argument("--runtime-artifact-kind", choices=("packaged_portable", "unpackaged_component"), default="packaged_portable")
    concurrency.add_argument("--hardware-manifest", required=True)
    concurrency.add_argument("--fixture-manifest", required=True)
    concurrency.add_argument("--timeout-s", type=float, default=2.0)
    concurrency.add_argument("--workload", choices=("concurrency", "saturation"), default="concurrency",
                             help="label the actual operator-driven workload; neither label supplies execution or budget proof")
    analyze_parser = sub.add_parser("analyze", help="validate a non-render JSONL capture and summarize its measured records")
    analyze_parser.add_argument("--input", required=True)
    args = parser.parse_args()
    try:
        if args.command == "collect-interaction":
            if not 1 <= args.timeout_s <= 300:
                raise Invalid("timeout-s must be within 1..300 seconds")
            if args.endpoint == "match_people_10000_open" and args.fixture_people_count != 10_000:
                raise Invalid("WP-087 requires --fixture-people-count 10000")
            return collect_people(args)
        if args.command == "collect-concurrency":
            if not 1 <= args.timeout_s <= 30:
                raise Invalid("timeout-s must be within 1..30 seconds")
            return collect_concurrency(args)
        result = analyze(Path(args.input))
        print(json.dumps(result, sort_keys=True, separators=(",", ":"), allow_nan=False))
        return 0
    except (Invalid, OSError) as error:
        print(f"match-nonrender-benchmark: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
