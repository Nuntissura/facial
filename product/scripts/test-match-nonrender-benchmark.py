import importlib.util
import json
import tempfile
import threading
import time
import unittest
import uuid
from pathlib import Path
from unittest import mock

SCRIPT = Path(__file__).with_name("match-nonrender-benchmark.py")
SPEC = importlib.util.spec_from_file_location("match_nonrender", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(MODULE)


def interaction_fixture(endpoint="match_people_10000_open"):
    rows = [{
        "record_type": "header", "schema_version": 1, "run_id": "test-run",
        "workload": "interaction", "endpoint": endpoint,
        "metric_scope": "facial_cli_invocation_to_terminal_applied_receipt",
        "facial_cli_sha256": "a" * 64, "fixture_manifest_sha256": "b" * 64,
        "packaged_binary_sha256": "c" * 64, "hardware_manifest_sha256": "d" * 64,
        "input_script_sha256": "e" * 64,
        "fixture_people_count_declared": 10_000,
    }]
    if endpoint == "operator_pause_feedback":
        initial = MODULE.initial_settings_setup_record(
            "applied", 0, 40_000, "88888888-8888-4888-8888-888888888888",
            {"endpoint": "open_settings", "duration_us": 20,
             "duration_scope": "match_settings_open_render_ui_excluding_backend_and_vsync",
             "current_state_confirmed": True, "rendered": True, "result_count": 0,
             "query_present": False, "desired_mode": None,
             "navigation": {"action": "open_settings", "requested_offset": 0, "applied_offset": 0, "page_limit": 200}}, 0)
        rows.append(initial)
    timestamp = 0
    for phase, count in (("warmup", 20), ("measure", 200)):
        for i in range(count):
            start = timestamp + 100
            setup_action_id = None
            setup_timestamp_us = None
            if endpoint in {"settings_manage_people_acknowledgement", "operator_pause_feedback"}:
                reset_endpoint = "open_settings" if endpoint == "settings_manage_people_acknowledgement" else "resume_all"
                scope = ("match_settings_open_render_ui_excluding_backend_and_vsync"
                         if reset_endpoint == "open_settings"
                         else "operator_pause_feedback_render_ui_excluding_backend_and_vsync")
                setup_action_id = str(uuid.UUID(int=100_000 + len(rows), version=4))
                setup_timestamp_us = timestamp + 50
                start = setup_timestamp_us + 50
                reset_row = {"record_type": "context_setup", "phase": phase, "ordinal": i,
                             "endpoint": endpoint, "reset_endpoint": reset_endpoint,
                             "timestamp_us": setup_timestamp_us, "setup_outcome": "applied",
                             "receipt_action_id": setup_action_id,
                             "reported_endpoint_duration_scope": scope,
                             "reported_endpoint_current_state_confirmed": True,
                             "reported_endpoint_rendered": True, "reported_endpoint_result_count": 0,
                             "reported_endpoint_query_present": False,
                             "reported_endpoint_desired_mode": "running" if reset_endpoint == "resume_all" else None,
                             "requested_offset": 0 if reset_endpoint == "open_settings" else None,
                             "applied_offset": 0 if reset_endpoint == "open_settings" else None,
                             "page_limit": 200 if reset_endpoint == "open_settings" else None}
                rows.append(reset_row)
            rows.append({"record_type": "interaction", "phase": phase, "ordinal": i,
                         "endpoint": endpoint, "call_start_timestamp_us": start,
                         "call_end_timestamp_us": start + 100_000, "duration_us": 100_000,
                         "endpoint_outcome": "applied", "receipt_action_id": str(uuid.UUID(int=200_000 + len(rows), version=4)),
                         "context_setup_action_id": setup_action_id,
                         "context_setup_timestamp_us": setup_timestamp_us,
                         "reported_endpoint_duration_us": None, "reported_endpoint_duration_scope": None,
                         "reported_endpoint_current_state_confirmed": None, "reported_endpoint_rendered": None,
                         "reported_endpoint_result_count": None, "reported_endpoint_query_present": None,
                         "reported_endpoint_desired_mode": None})
            timestamp = start + 100_000
    rows.append({"record_type": "end", "outcome": "completed", "sample_count": 220,
                 "observed_at_us": timestamp + 1})
    return rows


def governor_evidence():
    budget = {axis: 10 for axis in MODULE.TELEMETRY_AXES}
    budget["gpu_vram_bytes"] = 0
    empty = {axis: 0 for axis in MODULE.TELEMETRY_AXES}
    used = dict(empty)
    used["admitted_items"] = 1
    base = {"lifetime_id": "b2c8bd15-9601-40bc-86ac-2d0a4d7fa2e1",
            "scope": "governor_lifetime_including_warmup", "current_usage": empty,
            "peak_usage": empty, "acquisitions": 0, "replacements": 0, "releases": 0,
            "preparation_releases": 0, "pressure_events": 0, "overflow": False}
    end = {**base, "current_usage": empty, "peak_usage": used,
           "acquisitions": 1, "releases": 1, "pressure_events": 2}
    return budget, base, end


VISIBLE_IDS = {
    "thumbnail": "11111111-1111-4111-8111-111111111111",
    "navigation": "22222222-2222-4222-8222-222222222222",
    "playback_seek": "33333333-3333-4333-8333-333333333333",
}


def visible_checkpoint(endpoint, phase, timestamp, captured, sequence=0, dropped=0, abandoned=0, pending=0):
    return {"record_type": "visible_work_checkpoint", "phase": phase, "timestamp_us": timestamp,
            "endpoint": endpoint, "lifetime_id": VISIBLE_IDS[endpoint],
            "endpoint_scope": MODULE.VISIBLE_WORK_SCOPES[endpoint], "captured_at_us": captured,
            "sequence": sequence, "dropped_records": dropped, "abandoned": abandoned,
            "overflow": False, "pending": pending}


def visible_work_records(run_start=60_000_000, run_end=660_000_001, include_sample=True):
    rows = []
    base_clock = {"thumbnail": 10_000_000, "navigation": 20_000_000, "playback_seek": 30_000_000}
    sample_time = run_start + 1_000_000
    for endpoint, start in base_clock.items():
        rows.append(visible_checkpoint(endpoint, "baseline", run_start, start))
    for endpoint, start in base_clock.items():
        if include_sample:
            sample = {"record_type": "visible_work_sample", "phase": "measure",
                      "observed_at_us": sample_time, "endpoint": endpoint,
                      "lifetime_id": VISIBLE_IDS[endpoint], "sequence": 1,
                      "start_us": start + 1_000, "end_us": start + 1_800,
                      "duration_us": 800}
            rows.append(sample)
        rows.append(visible_checkpoint(endpoint, "measure", sample_time,
                                       start + 2_000_000, sequence=1 if include_sample else 0))
    for endpoint, start in base_clock.items():
        rows.append(visible_checkpoint(endpoint, "terminal", run_end, start + 600_000_000,
                                       sequence=1 if include_sample else 0))
    return rows


def concurrency_rows(timestamps=(0, 60_000_000, 660_000_000)):
    rows = [{"record_type": "header", "schema_version": 1, "run_id": "test-run",
             "workload": "concurrency", "facial_cli_sha256": "a" * 64,
             "fixture_manifest_sha256": "b" * 64, "packaged_binary_sha256": "c" * 64,
             "hardware_manifest_sha256": "d" * 64, "input_script_sha256": "e" * 64,
             "visible_work_schema_version": 1,
             "visible_work_sample_semantics": "per_endpoint_measurement_baseline_then_each_new_success_sequence_once"}]
    for index, timestamp in enumerate(timestamps):
        rows.append({"record_type": "concurrency", "phase": "warmup" if index == 0 else "measure",
                     "timestamp_us": timestamp, "indexing_progress": None,
                     "indexing_progress_scope": "canonical_all_index_jobs",
                     "index_stage": "", "index_stage_counts": {},
                     "index_stage_scope": "persisted_asset_next_stage_counts",
                     "active_holds": None, "admitted_resources": None, "lease_deltas": None})
    return rows


def interaction_rows(rows):
    return [row for row in rows if row.get("record_type") == "interaction"]


RUNTIME_ID = "55555555-5555-4555-8555-555555555555"
RUNTIME_RING_IDS = {"lease_activity": "66666666-6666-4666-8666-666666666666",
                    "native_playback": "77777777-7777-4777-8777-777777777777"}


def runtime_snapshot(start, stop, sequence, previous=None, pressure=True, playing=True):
    empty = {axis: 0 for axis in MODULE.TELEMETRY_AXES}
    used = {axis: 1 if axis != "gpu_vram_bytes" else 0 for axis in MODULE.TELEMETRY_AXES}
    measured = previous is not None
    interval = {"scope": "governor_interval_between_dedicated_runtime_diagnostics", "runtime_id": RUNTIME_ID,
                "lifetime_id": governor_evidence()[1]["lifetime_id"], "sequence": sequence,
                "start_us": start, "end_us": stop, "opening_usage": empty, "closing_usage": empty,
                "peak_usage": used if measured else empty, "opening_live_stage_leases": [0] * 7,
                "closing_live_stage_leases": [0] * 7, "opening_unclassified_leases": 0,
                "closing_unclassified_leases": 0, "acquisitions": int(measured), "replacements": 0,
                "releases": int(measured), "preparation_releases": 0,
                "pressure_events": int(measured and pressure), "overflow": False}
    evidence = {"schema_version": 1, "runtime_id": RUNTIME_ID, "timestamp_scope": MODULE.RUNTIME_TIMESTAMP_SCOPE,
                "captured_at_us": stop + 10, "governor_interval": interval}
    for endpoint, scope in MODULE.RUNTIME_RING_SCOPES.items():
        samples = [] if previous is None else list(previous["runtime_evidence"][endpoint]["samples"])
        def push(stamp, **data):
            samples.append({"sequence": len(samples) + 1, "timestamp_us": stamp, **data})
        if measured and endpoint == "lease_activity":
            push(start + 10, event="acquire", usage=used, live_stage_leases=[0] * 7, unclassified_leases=1)
            push(start + 20, event="stage_tagged", usage=used, live_stage_leases=[0, 0, 0, 1, 0, 0, 0], unclassified_leases=0)
            if pressure:
                push(start + 30, event="pressure", usage=used, live_stage_leases=[0, 0, 0, 1, 0, 0, 0], unclassified_leases=0)
            push(stop - 10, event="release", usage=empty, live_stage_leases=[0] * 7, unclassified_leases=0)
        if measured and endpoint == "native_playback":
            for stamp, clock in ((start + 1000, 10), (stop - 1000, 100)):
                push(stamp, poll_start_us=stamp - 2, poll_end_us=stamp - 1,
                     status="playing" if playing else "paused", native_player_present=True,
                     native_playing=playing, clock_available=True, time_ms=clock,
                     player_generation=1, generation_overflow=False)
            if sequence == 3:
                push(stop + 2, poll_start_us=stop, poll_end_us=stop + 1, status="stopped",
                     native_player_present=True, native_playing=False, clock_available=True, time_ms=100,
                     player_generation=1, generation_overflow=False)
        evidence[endpoint] = {"runtime_id": RUNTIME_ID, "lifetime_id": RUNTIME_RING_IDS[endpoint],
                              "endpoint_scope": scope, "captured_at_us": stop + 3,
                              "sequence": len(samples), "dropped_records": 0, "overflow": False, "samples": samples}
    return {"runtime_evidence": evidence}


def interval_capture(pressure=True, playing=True):
    rows = concurrency_rows((0, 60_000_100, 360_000_100))
    rows[0].update(visible_work_schema_version=2, runtime_evidence_schema_version=1)
    rows.append({**rows[3], "timestamp_us": 660_000_100})
    for index, row in enumerate(rows[1:]):
        row["indexing_progress"] = {"discovered": 10, "completed": max(0, index - 1), "failed": 0, "skipped": 0}
    previous = None
    for start, stop, sequence, phase, stamp in ((0, 60_000_000, 1, "baseline", 60_000_100),
                                               (60_000_000, 360_000_000, 2, "measure", 360_000_100),
                                               (360_000_000, 660_000_000, 3, "terminal", 660_000_100)):
        snapshot = runtime_snapshot(start, stop, sequence, previous, pressure, playing)
        records, previous_evidence = MODULE.runtime_evidence_records(snapshot, phase, stamp,
                                                                     previous["runtime_evidence"] if previous else None)
        rows.extend(records)
        previous = {"runtime_evidence": previous_evidence}
        for endpoint in MODULE.VISIBLE_WORK_SCOPES:
            count = 0 if phase == "baseline" else 1
            if phase == "measure":
                rows.append({"record_type": "visible_work_sample", "phase": phase, "observed_at_us": stamp,
                             "endpoint": endpoint, "lifetime_id": VISIBLE_IDS[endpoint], "runtime_id": RUNTIME_ID,
                             "timestamp_scope": MODULE.RUNTIME_TIMESTAMP_SCOPE, "sequence": 1,
                             "start_us": 60_000_200, "end_us": 60_001_000, "duration_us": 800})
            checkpoint = visible_checkpoint(endpoint, phase, stamp, stop + 3, count)
            checkpoint.update(runtime_id=RUNTIME_ID, timestamp_scope=MODULE.RUNTIME_TIMESTAMP_SCOPE)
            rows.append(checkpoint)
    budget, baseline, terminal = governor_evidence()
    terminal.update(acquisitions=2, releases=2, pressure_events=2 if pressure else 0)
    rows.append({"record_type": "governor_evidence", "measurement_start_us": 60_000_100,
                 "measurement_end_us": 660_000_100, "resource_budget": budget,
                 "baseline_resource_telemetry": baseline, "terminal_resource_telemetry": terminal})
    rows.append({"record_type": "end", "outcome": "completed", "observed_at_us": 660_000_100, "sample_count": 4})
    return rows


class NonRenderBenchmarkTests(unittest.TestCase):
    @unittest.skipUnless(MODULE.os.name == "nt", "requires real Windows file sharing")
    def test_windows_receipt_reader_permits_atomic_replacement_while_open(self):
        import msvcrt
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "receipt.json"
            replacement = Path(directory) / "receipt-new.json"
            old = b'{"status":"accepted"}'
            new = b'{"status":"applied"}'
            target.write_bytes(old)
            replacement.write_bytes(new)
            with target.open("rb"):
                with self.assertRaises(PermissionError):
                    MODULE.os.unlink(target)
            with MODULE.open_receipt_stream(target) as reader:
                descriptor = reader.fileno()
                self.assertFalse(MODULE.os.get_inheritable(descriptor))
                self.assertGreater(msvcrt.get_osfhandle(descriptor), 0)
                # api.rs atomic_write removes the old receipt before renaming
                # its completed temporary file; MoveFileEx replace differs.
                MODULE.os.unlink(target)
                MODULE.os.rename(replacement, target)
                self.assertEqual(reader.read(), old)
                self.assertEqual(target.read_bytes(), new)
            with self.assertRaises(OSError):
                MODULE.os.fstat(descriptor)

    @unittest.skipUnless(MODULE.os.name == "nt", "requires real Windows handle ownership")
    def test_windows_receipt_handle_and_descriptor_failure_paths_close_owned_handle(self):
        import msvcrt
        kernel, _ = MODULE.windows_receipt_api()
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "receipt.json"
            target.write_bytes(b'{}')
            with mock.patch.object(msvcrt, "open_osfhandle", side_effect=OSError("controlled transfer failure")), \
                 mock.patch.object(kernel, "CloseHandle", wraps=kernel.CloseHandle) as close:
                with self.assertRaisesRegex(OSError, "controlled transfer failure"):
                    MODULE.open_receipt_stream(target)
                self.assertEqual(close.call_count, 1)
            descriptor = None
            original_transfer = msvcrt.open_osfhandle
            def remember_transfer(*args):
                nonlocal descriptor
                descriptor = original_transfer(*args)
                return descriptor
            with mock.patch.object(msvcrt, "open_osfhandle", side_effect=remember_transfer), \
                 mock.patch.object(MODULE.os, "fdopen", side_effect=OSError("controlled stream failure")), \
                 mock.patch.object(kernel, "CloseHandle", wraps=kernel.CloseHandle) as close:
                with self.assertRaisesRegex(OSError, "controlled stream failure"):
                    MODULE.open_receipt_stream(target)
                self.assertEqual(close.call_count, 0)
            with self.assertRaises(OSError):
                MODULE.os.fstat(descriptor)

    @unittest.skipUnless(MODULE.os.name == "nt", "requires real Windows handle confinement")
    def test_windows_receipt_handle_final_path_mismatch_is_rejected_and_closed(self):
        kernel, _ = MODULE.windows_receipt_api()
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "receipt.json"
            target.write_bytes(b'{}')
            with mock.patch.object(MODULE, "normalized_windows_receipt_path", side_effect=["outside", "expected"]), \
                 mock.patch.object(kernel, "CloseHandle", wraps=kernel.CloseHandle) as close:
                with self.assertRaisesRegex(MODULE.Invalid, "canonical receipt path"):
                    MODULE.open_receipt_stream(target)
                self.assertEqual(close.call_count, 1)

    @unittest.skipUnless(MODULE.os.name == "nt", "requires real Windows deleted-handle state")
    def test_windows_concurrent_publisher_reopens_deleted_handle_under_original_action_deadline(self):
        import ctypes
        for force_deleted_path in (False, True):
            with self.subTest(deleted_path_branch=force_deleted_path):
                identity = str(uuid.uuid4())
                terminal = {"action_id": identity, "kind": "match_settings_manage_people", "status": "applied",
                            "result": {"endpoint": "settings_manage_people_acknowledgement", "duration_us": 10,
                                       "duration_scope": "ui_state_rendered_by_render_ui_excluding_backend_and_vsync",
                                       "current_state_confirmed": True, "rendered": True, "result_count": 0,
                                       "query_present": False}}
                api_root, target = self.diagnostics_file(terminal)
                replacement = target.with_suffix(".tmp")
                replacement.write_bytes(target.read_bytes())
                target.write_bytes(json.dumps({"action_id": identity, "kind": terminal["kind"], "status": "accepted"}).encode())
                requested, published = threading.Event(), threading.Event()
                publisher_errors = []
                def publish():
                    try:
                        if not requested.wait(2):
                            raise AssertionError("publisher was not requested")
                        MODULE.os.unlink(target)
                        MODULE.os.rename(replacement, target)
                    except BaseException as error:
                        publisher_errors.append(error)
                    finally:
                        published.set()
                publisher = threading.Thread(target=publish)
                kernel, _ = MODULE.windows_receipt_api()
                original_path_query = kernel.GetFinalPathNameByHandleW
                original_close = kernel.CloseHandle
                query_handles, closed = [], []
                def query_after_publication(handle, buffer, capacity, flags):
                    query_handles.append(handle)
                    if len(query_handles) == 1:
                        requested.set()
                        self.assertTrue(published.wait(2))
                        self.assertFalse(publisher_errors)
                        # Both branches retain actual native DeletePending/links0
                        # evidence. This spelling injects the observed live NTFS
                        # tombstone branch; it never supplies receipt data.
                        if force_deleted_path:
                            buffer.value = "\\\\?\\C:\\$Extend\\$Deleted\\controlled-test"
                            return len(buffer.value)
                    return original_path_query(handle, buffer, capacity, flags)
                def close_owned(handle):
                    result = original_close(handle)
                    closed.append(bool(result))
                    return result
                start = time.monotonic_ns()
                publisher.start()
                try:
                    with mock.patch.object(kernel, "GetFinalPathNameByHandleW", side_effect=query_after_publication), \
                         mock.patch.object(kernel, "CloseHandle", side_effect=close_owned), \
                         mock.patch.object(MODULE.os, "fdopen", wraps=MODULE.os.fdopen) as transfer, \
                         mock.patch.object(MODULE, "run_cli", return_value=(0, {"action_id": identity,
                             "kind": terminal["kind"], "status": "accepted"}, start, start + 1_000)) as submit, \
                         mock.patch.object(MODULE, "terminal_receipt", wraps=MODULE.terminal_receipt) as poll:
                        result = MODULE.interaction_outcome(Path("cli"), api_root, api_root,
                                                            "settings_manage_people_acknowledgement", 2.0)
                    self.assertEqual(result[0], "applied")
                    self.assertEqual(result[3], identity)
                    self.assertEqual(submit.call_count, 1)
                    self.assertEqual(poll.call_args.args[2], start + 2_000_000_000)
                    self.assertEqual(len(query_handles), 2)
                    self.assertEqual(closed, [True])
                    self.assertEqual(transfer.call_count, 1)
                finally:
                    requested.set()
                    publisher.join(2)
                self.assertFalse(publisher.is_alive())
                self.assertFalse(publisher_errors)

    @unittest.skipUnless(MODULE.os.name == "nt", "requires real Windows handle state")
    def test_windows_live_handle_path_error_is_not_a_publication_race(self):
        import ctypes
        kernel, _ = MODULE.windows_receipt_api()
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "receipt.json"
            target.write_bytes(b'{}')
            def missing_path(*args):
                ctypes.set_last_error(1168)
                return 0
            with mock.patch.object(kernel, "GetFinalPathNameByHandleW", side_effect=missing_path), \
                 mock.patch.object(kernel, "CloseHandle", wraps=kernel.CloseHandle) as close, \
                 mock.patch.object(MODULE.os, "fdopen", wraps=MODULE.os.fdopen) as transfer:
                with self.assertRaises(OSError) as raised:
                    MODULE.open_receipt_stream(target)
                self.assertEqual(raised.exception.winerror, 1168)
                self.assertNotIsInstance(raised.exception, MODULE.ReceiptPublicationRace)
                self.assertEqual(close.call_count, 1)
                transfer.assert_not_called()

    @unittest.skipUnless(MODULE.os.name == "nt", "requires real Windows handle validation")
    def test_windows_reparse_handle_is_rejected_before_publication_retry_or_read(self):
        kernel, _ = MODULE.windows_receipt_api()
        original_info = kernel.GetFileInformationByHandle
        def reparse_information(handle, information):
            result = original_info(handle, information)
            information._obj.attributes |= 0x400
            return result
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "receipt.json"
            target.write_bytes(b'{}')
            with mock.patch.object(kernel, "GetFileInformationByHandle", side_effect=reparse_information), \
                 mock.patch.object(MODULE, "windows_receipt_handle_deleted") as deleted, \
                 mock.patch.object(kernel, "CloseHandle", wraps=kernel.CloseHandle) as close, \
                 mock.patch.object(MODULE.os, "fdopen", wraps=MODULE.os.fdopen) as transfer:
                with self.assertRaisesRegex(MODULE.Invalid, "reparse point"):
                    MODULE.open_receipt_stream(target)
                deleted.assert_not_called()
                transfer.assert_not_called()
                self.assertEqual(close.call_count, 1)

    def test_deleted_handle_retry_expires_without_extending_deadline_or_resubmitting(self):
        identity = "99999999-9999-4999-8999-999999999999"
        start = 1_000_000_000
        with mock.patch.object(MODULE, "open_receipt_stream", side_effect=MODULE.ReceiptPublicationRace("deleted")) as read, \
             mock.patch.object(MODULE, "run_cli", return_value=(0, {"action_id": identity,
                 "kind": "match_settings_manage_people", "status": "accepted"}, start, start + 1_900_000_000)) as submit, \
             mock.patch.object(MODULE, "terminal_receipt", wraps=MODULE.terminal_receipt) as poll, \
             mock.patch.object(MODULE.time, "monotonic_ns", side_effect=[start + 1_990_000_000, start + 2_000_000_000, start + 2_000_000_000]), \
             mock.patch.object(MODULE.time, "sleep"):
            result = MODULE.interaction_outcome(Path("cli"), Path("unused"), Path("unused"),
                                               "settings_manage_people_acknowledgement", 2.0)
        self.assertEqual(result[0], "terminal_receipt_timeout")
        self.assertEqual(result[3], identity)
        self.assertEqual(read.call_count, 1)
        self.assertEqual(submit.call_count, 1)
        self.assertEqual(poll.call_args.args[2], start + 2_000_000_000)

    def pause_collector_args(self, run_id):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name)
        workspace, api_root = root / "workspace", root / "api"
        workspace.mkdir()
        api_root.mkdir()
        inputs = []
        for name in ("cli.exe", "component.exe", "hardware.json", "fixture.json"):
            path = root / name
            path.write_bytes(b"bounded-fixture")
            inputs.append(path)
        return MODULE.argparse.Namespace(workspace_root=str(workspace), api_root=str(api_root),
            facial_cli=str(inputs[0]), packaged_portable=str(inputs[1]), hardware_manifest=str(inputs[2]),
            fixture_manifest=str(inputs[3]), endpoint="operator_pause_feedback", fixture_people_count=None,
            query=None, media_key=None, catalog_revision=None, face_id=None, timeout_s=5.0,
            runtime_artifact_kind="unpackaged_component", run_id=run_id)

    def pause_collector_model(self, args, initial_confirmed=True, interfere=False):
        origin = 1_000_000_000
        calls = []
        settings_visible = False
        def interaction(_cli, _api, _workspace, endpoint, timeout, *_args):
            nonlocal settings_visible
            calls.append(endpoint)
            self.assertEqual(timeout, 5.0)
            start = origin + (len(calls) - 1) * 2_000_000
            finish = start + 1_000_000
            identity = str(uuid.UUID(int=30_000 + len(calls), version=4))
            if endpoint == "settings_context_reset":
                settings_visible = initial_confirmed
                result = {"endpoint": "open_settings", "duration_us": 400,
                          "duration_scope": "match_settings_open_render_ui_excluding_backend_and_vsync",
                          "current_state_confirmed": initial_confirmed, "rendered": initial_confirmed,
                          "result_count": 0, "query_present": False, "desired_mode": None,
                          "navigation": {"action": "open_settings", "requested_offset": 0,
                                         "applied_offset": 0, "page_limit": 200}}
                return "applied", start, finish, identity, result
            if interfere:
                settings_visible = False
            # Actual UI route emits timing only while Settings > Match is visible.
            if not settings_visible:
                return "endpoint_receipt_mismatch", start, finish, identity, None
            self.assertIn(endpoint, {"pause_context_reset", "operator_pause_feedback"})
            result = {"endpoint": "resume_all" if endpoint == "pause_context_reset" else "pause_all",
                      "duration_us": 400, "duration_scope": "operator_pause_feedback_render_ui_excluding_backend_and_vsync",
                      "current_state_confirmed": True, "rendered": True, "result_count": 0, "query_present": False,
                      "desired_mode": "running" if endpoint == "pause_context_reset" else "operator_paused", "navigation": None}
            return "applied", start, finish, identity, result
        with mock.patch.object(MODULE, "interaction_outcome", side_effect=interaction), \
             mock.patch.object(MODULE.time, "monotonic_ns", side_effect=[origin, origin + 2_000_000_000]):
            code = MODULE.collect_people(args)
        path = Path(args.workspace_root) / ".facial" / "benchmarks" / f"{args.run_id}.jsonl"
        return code, calls, path

    def test_pause_collector_opens_initially_closed_settings_before_warmup(self):
        args = self.pause_collector_args("pause-initial-context")
        code, calls, path = self.pause_collector_model(args)
        self.assertEqual(code, 0)
        self.assertEqual(calls[0], "settings_context_reset")
        self.assertEqual(calls[1::2], ["pause_context_reset"] * 220)
        self.assertEqual(calls[2::2], ["operator_pause_feedback"] * 220)
        rows = MODULE.load_records(path)
        self.assertEqual(rows[1]["record_type"], "initial_settings_setup")
        self.assertEqual(len(interaction_rows(rows)), 220)
        self.assertLess(rows[1]["call_end_timestamp_us"], rows[2]["timestamp_us"])
        result = MODULE.analyze(path)
        self.assertEqual(result["endpoint_duration_sample_count"], 200)
        self.assertEqual(result["endpoint_duration_p95_us"], 400)
        self.assertTrue(result["initial_settings_context_confirmed"])
        self.assertFalse(result["gate_eligible"])

    def test_pause_collector_unconfirmed_initial_context_and_interference_fail_closed(self):
        for initial, interference, expected in ((False, False, ["settings_context_reset"]),
                                                (True, True, ["settings_context_reset", "pause_context_reset"])):
            with self.subTest(initial=initial, interference=interference):
                args = self.pause_collector_args(f"pause-invalid-{initial}-{interference}")
                code, calls, path = self.pause_collector_model(args, initial, interference)
                self.assertEqual(code, 2)
                self.assertEqual(calls, expected)
                self.assertEqual(len(interaction_rows(MODULE.load_records(path))), 0)
                with self.assertRaisesRegex(MODULE.Invalid, "completed end"):
                    MODULE.analyze(path)

    def test_pause_analyzer_requires_unique_causal_rendered_initial_settings_record(self):
        for mutation in ("missing", "rendered", "clock", "identity", "duration", "private"):
            rows = interaction_fixture("operator_pause_feedback")
            if mutation == "missing":
                rows.pop(1)
            elif mutation == "rendered":
                rows[1]["reported_endpoint_rendered"] = False
            elif mutation == "clock":
                rows[1].update(timestamp_us=60, call_end_timestamp_us=60, duration_us=60)
            elif mutation == "identity":
                rows[1]["receipt_action_id"] = rows[2]["receipt_action_id"]
            elif mutation == "duration":
                rows[1]["reported_endpoint_duration_us"] = 41
            else:
                rows[1]["person_name"] = "must-not-accept-private-fields"
            with self.subTest(mutation=mutation), self.assertRaises(MODULE.Invalid):
                MODULE.analyze(self.capture(rows))

    def test_terminal_permission_retry_keeps_original_deadline_action_and_single_submission(self):
        identity = "99999999-9999-4999-8999-999999999999"
        start = 1_000_000_000
        receipt = {"action_id": identity, "kind": "match_settings_manage_people", "status": "applied",
                   "result": {"endpoint": "settings_manage_people_acknowledgement", "duration_us": 10,
                              "duration_scope": "ui_state_rendered_by_render_ui_excluding_backend_and_vsync",
                              "current_state_confirmed": True, "rendered": True, "result_count": 0, "query_present": False}}
        api_root, path = self.diagnostics_file(receipt)
        original_open = MODULE.open_receipt_stream
        opened = []
        def transient_open(candidate, *args, **kwargs):
            opened.append(candidate)
            if len(opened) == 1:
                raise PermissionError(13, "controlled Windows replacement race")
            return original_open(candidate, *args, **kwargs)
        with mock.patch.object(MODULE, "open_receipt_stream", side_effect=transient_open), \
             mock.patch.object(MODULE, "run_cli", return_value=(0, {"action_id": identity,
                 "kind": receipt["kind"], "status": "accepted"}, start, start + 1_900_000_000)) as submit, \
             mock.patch.object(MODULE, "terminal_receipt", wraps=MODULE.terminal_receipt) as poll, \
             mock.patch.object(MODULE.time, "monotonic_ns", return_value=start + 1_950_000_000), \
             mock.patch.object(MODULE.time, "sleep") as sleep:
            result = MODULE.interaction_outcome(Path("cli"), api_root, api_root, "settings_manage_people_acknowledgement", 2.0)
        self.assertEqual(result[0], "applied")
        self.assertEqual(result[3], identity)
        self.assertEqual(opened, [path, path])
        self.assertEqual(submit.call_count, 1)
        self.assertEqual(poll.call_args.args[2], start + 2_000_000_000)
        sleep.assert_called_once_with(0.01)

    def test_terminal_permission_retry_expires_without_extending_deadline(self):
        identity = "99999999-9999-4999-8999-999999999999"
        start = 1_000_000_000
        with mock.patch.object(MODULE, "open_receipt_stream", side_effect=PermissionError(13, "controlled replacement race")) as read, \
             mock.patch.object(MODULE, "run_cli", return_value=(0, {"action_id": identity,
                 "kind": "match_settings_manage_people", "status": "accepted"}, start, start + 1_900_000_000)) as submit, \
             mock.patch.object(MODULE.time, "monotonic_ns", side_effect=[start + 1_990_000_000, start + 2_000_000_000, start + 2_000_000_000]), \
             mock.patch.object(MODULE.time, "sleep"):
            result = MODULE.interaction_outcome(Path("cli"), Path("unused"), Path("unused"), "settings_manage_people_acknowledgement", 2.0)
        self.assertEqual(result[0], "terminal_receipt_timeout")
        self.assertEqual(result[3], identity)
        self.assertEqual(read.call_count, 1)
        self.assertEqual(submit.call_count, 1)

    def test_unpacked_component_preserves_endpoint_measurement_without_canonical_gate(self):
        rows = self.people_count_fixture()
        rows[0].update(artifact_kind="unpackaged_component", runtime_binary_sha256="f" * 64,
                       packaged_binary_sha256=None)
        result = MODULE.analyze(self.capture(rows))
        self.assertEqual(result["artifact_kind"], "unpackaged_component")
        self.assertEqual(result["runtime_binary_sha256"], "f" * 64)
        self.assertIsNone(result["packaged_binary_sha256"])
        self.assertTrue(result["performance_threshold_observed"])
        self.assertEqual(result["endpoint_duration_sample_count"], 200)
        self.assertEqual(result["endpoint_component_verdict"], "pass")
        self.assertIn("fixture_count_proof_scope", result)
        self.assertFalse(result["gate_eligible"])
        self.assertFalse(result["canonical_package_proven"])
        self.assertEqual(result["canonical_package_verdict"], "pending_independent_package_proof")

    def test_package_label_and_hash_do_not_prove_canonical_packaging(self):
        rows = self.people_count_fixture()
        for header in (dict(rows[0]), {**rows[0], "artifact_kind": "packaged_portable",
                                      "runtime_binary_sha256": rows[0]["packaged_binary_sha256"],
                                      "canonical_package_proven": True}):
            rows[0] = header
            result = MODULE.analyze(self.capture(rows))
            self.assertEqual(result["endpoint_component_verdict"], "pass")
            self.assertTrue(result["gate_eligible"])
            self.assertFalse(result["canonical_package_proven"])
            self.assertEqual(result["artifact_kind_scope"], "caller_declared_artifact_kind_not_package_proof")

    def test_artifact_metadata_rejects_mislabelled_hashes_and_partial_kind(self):
        for metadata in (
            {"artifact_kind": "unpackaged_component", "runtime_binary_sha256": "f" * 64},
            {"artifact_kind": "unpackaged_component", "runtime_binary_sha256": "f" * 64, "packaged_binary_sha256": "c" * 64},
            {"artifact_kind": "packaged_portable", "runtime_binary_sha256": "f" * 64},
            {"artifact_kind": "packaged_portable"},
            {"artifact_kind": []},
            {"runtime_binary_sha256": "f" * 64},
            {"artifact_kind": "unpackaged_component", "runtime_binary_sha256": None, "packaged_binary_sha256": None},
        ):
            rows = self.people_count_fixture()
            rows[0].update(metadata)
            with self.subTest(metadata=metadata), self.assertRaises(MODULE.Invalid):
                MODULE.analyze(self.capture(rows))

    def test_component_capture_header_hashes_actual_supplied_artifact_and_concurrency_stays_scoped(self):
        with tempfile.TemporaryDirectory() as directory:
            artifact = Path(directory) / "runtime.exe"
            artifact.write_bytes(b"optimized-unpackaged-component-fixture")
            args = MODULE.argparse.Namespace(runtime_artifact_kind="unpackaged_component")
            metadata = MODULE.runtime_artifact_metadata(args, artifact)
            self.assertEqual(metadata["runtime_binary_sha256"], MODULE.sha256_file(artifact))
            self.assertIsNone(metadata["packaged_binary_sha256"])
            default = MODULE.runtime_artifact_metadata(MODULE.argparse.Namespace(), artifact)
            self.assertEqual(default["artifact_kind"], "packaged_portable")
            self.assertEqual(default["packaged_binary_sha256"], metadata["runtime_binary_sha256"])
        rows = interval_capture()
        rows[0].update(metadata)
        result = MODULE.analyze(self.capture(rows))
        self.assertEqual(result["measured_governor_component_verdict"], "pass")
        self.assertFalse(result["gate_eligible"])
        self.assertFalse(result["canonical_package_proven"])
        self.assertIsNone(result["packaged_binary_sha256"])

    def test_people_page_result_count_cannot_materialize_full_catalog(self):
        for count in (257, 10_000):
            with self.subTest(count=count):
                rows = self.people_count_fixture()
                interaction_rows(rows)[20]["reported_endpoint_result_count"] = count
                result = MODULE.analyze(self.capture(rows))
                self.assertFalse(result["gate_eligible"])
                self.assertNotEqual(result["verdict"], "pass")

    def test_actual_root_intervals_advance_only_measured_governor_component(self):
        result = MODULE.analyze(self.capture(interval_capture()))
        runtime = result["measured_runtime_evidence"]
        self.assertEqual(result["measured_governor_component_verdict"], "pass")
        self.assertEqual(runtime["scope"], "measured_governor_intervals_excluding_warmup")
        self.assertEqual(runtime["interval_count"], 2)
        self.assertEqual(runtime["observed_measured_peaks"]["cpu_inference"], 1)
        self.assertEqual(runtime["measured_counter_totals"]["pressure_events"], 2)
        self.assertGreater(runtime["raw_native_advancing_pairs"], 0)
        self.assertGreater(runtime["lease_native_observation_span_overlap_us"], 0)
        self.assertIn("excluding_kernel_execution", runtime["lease_native_overlap_scope"])
        self.assertTrue(runtime["observed_terminal_resources_zero"])
        self.assertTrue(runtime["observed_terminal_native_stopped"])
        self.assertEqual(result["indexing_progress_delta"]["completed"], 2)
        self.assertEqual(result["verdict"], "pending")
        self.assertFalse(result["gate_eligible"])
        for missing in ("actual_indexing_kernel_execution_overlap", "actual_workload_stop_and_final_drain",
                        "whole_interval_simultaneous_visible_workload", "thumbnail_budget_verdict"):
            self.assertIn(missing, result["missing_fields"])

    def test_measured_interval_does_not_use_warmup_lifetime_peaks(self):
        rows = interval_capture()
        evidence = next(row for row in rows if row["record_type"] == "governor_evidence")
        evidence["baseline_resource_telemetry"]["peak_usage"]["cpu_inference"] = 10
        evidence["terminal_resource_telemetry"]["peak_usage"]["cpu_inference"] = 10
        result = MODULE.analyze(self.capture(rows))
        self.assertEqual(result["measured_runtime_evidence"]["observed_measured_peaks"]["cpu_inference"], 1)
        self.assertEqual(result["governor_telemetry"]["observed_lifetime_peaks"]["cpu_inference_concurrency"], 10)

    def test_interval_gaps_identity_resets_overflow_and_counter_tampering_reject(self):
        for mutation in ("gap", "clock_gap", "runtime_reset", "overflow", "counter"):
            rows = interval_capture()
            row = next(row for row in rows if row["record_type"] == "runtime_checkpoint" and row["phase"] == "measure")
            interval = row["governor_interval"]
            if mutation == "gap":
                interval["sequence"] += 1
            elif mutation == "clock_gap":
                interval["start_us"] += 1
            elif mutation == "runtime_reset":
                row["runtime_id"] = str(uuid.uuid4())
            elif mutation == "overflow":
                interval["overflow"] = True
            else:
                interval["pressure_events"] += 1
            with self.subTest(mutation=mutation), self.assertRaises(MODULE.Invalid):
                MODULE.analyze(self.capture(rows))

    def test_raw_ring_loss_private_fields_and_player_generation_overflow_reject(self):
        for mutation in ("lost", "private", "generation_overflow"):
            rows = interval_capture()
            row = next(row for row in rows if row["record_type"] == "runtime_sample" and row["phase"] == "measure"
                       and row["endpoint"] == "native_playback")
            if mutation == "lost":
                rows.remove(row)
            elif mutation == "private":
                row["sample"]["path"] = "private-media-path"
            else:
                row["sample"]["generation_overflow"] = True
            with self.subTest(mutation=mutation), self.assertRaises(MODULE.Invalid):
                MODULE.analyze(self.capture(rows))

    def test_idle_pressure_and_raw_playing_missing_remain_explicit_pending(self):
        result = MODULE.analyze(self.capture(interval_capture(pressure=False, playing=False)))
        self.assertEqual(result["measured_governor_component_verdict"], "pending")
        self.assertIn("genuine_measured_governor_pressure", result["missing_fields"])
        self.assertIn("raw_native_playing_and_same_generation_advancing_clock", result["missing_fields"])
        rows = interval_capture()
        samples = [row["sample"] for row in rows if row["record_type"] == "runtime_sample"
                   and row["endpoint"] == "native_playback"]
        for index, sample in enumerate(samples):
            sample["player_generation"] = index + 1
        result = MODULE.analyze(self.capture(rows))
        self.assertEqual(result["measured_runtime_evidence"]["raw_native_advancing_pairs"], 0)

    def test_root_interval_short_ceiling_exceeded_and_saturation_axis_coverage(self):
        rows = interval_capture()
        for row in rows:
            if row["record_type"] == "runtime_checkpoint" and row["phase"] == "terminal":
                row["governor_interval"]["end_us"] -= 1
        result = MODULE.analyze(self.capture(rows))
        self.assertEqual(result["measured_governor_component_verdict"], "pending")
        self.assertIn("canonical_600s_root_clock_measured_interval", result["missing_fields"])
        rows = interval_capture()
        next(row for row in rows if row["record_type"] == "governor_evidence")["resource_budget"]["cpu_inference"] = 0
        result = MODULE.analyze(self.capture(rows))
        self.assertEqual(result["measured_governor_component_verdict"], "fail")
        rows = interval_capture()
        rows[0]["workload"] = "saturation"
        result = MODULE.analyze(self.capture(rows))
        self.assertEqual(result["governor_component_verdict"], "pass")
        self.assertEqual(result["saturation_axis_coverage_verdict"], "pass")
        self.assertEqual(result["workload_verdict"], "pending")
        self.assertFalse(result["gate_eligible"])
        rows = interval_capture()
        rows[0]["workload"] = "saturation"
        next(row for row in rows if row["record_type"] == "governor_evidence")["resource_budget"]["gpu_vram_bytes"] = 10
        result = MODULE.analyze(self.capture(rows))
        self.assertEqual(result["saturation_axis_coverage_verdict"], "pending")
        self.assertIn("saturation.enabled_axis_not_exercised.gpu_vram_bytes", result["missing_fields"])

    def test_visible_root_clock_mismatch_and_canonical_progress_reset_reject(self):
        rows = interval_capture()
        next(row for row in rows if row["record_type"] == "visible_work_sample")["runtime_id"] = str(uuid.uuid4())
        with self.assertRaisesRegex(MODULE.Invalid, "common-clock"):
            MODULE.analyze(self.capture(rows))
        rows = interval_capture()
        measured = [row for row in rows if row["record_type"] == "concurrency" and row["phase"] == "measure"]
        measured[1]["indexing_progress"]["completed"] = 3
        with self.assertRaisesRegex(MODULE.Invalid, "progress reset"):
            MODULE.analyze(self.capture(rows))

    def test_post_governor_capture_visible_completion_is_retained_but_excluded(self):
        rows = interval_capture()
        final = next(row for row in rows if row["record_type"] == "visible_work_checkpoint"
                     and row["phase"] == "terminal" and row["endpoint"] == "thumbnail")
        final["sequence"] = 2
        event = {"record_type": "visible_work_sample", "phase": "terminal", "observed_at_us": final["timestamp_us"],
                 "endpoint": "thumbnail", "lifetime_id": VISIBLE_IDS["thumbnail"], "runtime_id": RUNTIME_ID,
                 "timestamp_scope": MODULE.RUNTIME_TIMESTAMP_SCOPE, "sequence": 2,
                 "start_us": 660_000_001, "end_us": 660_000_002, "duration_us": 1}
        rows.insert(rows.index(final), event)
        result = MODULE.analyze(self.capture(rows))
        self.assertEqual(result["visible_work"]["thumbnail"]["sample_count"], 1)
        self.assertEqual(result["visible_work"]["thumbnail"]["outside_root_interval_excluded"], 1)

    def test_five_full_pretty_rings_fit_cap_and_extract_only_bounded_lines(self):
        receipt = self.full_diagnostics_receipt()
        snapshot = receipt["result"]["snapshot"]
        evidence = runtime_snapshot(0, 600_000_000, 1)["runtime_evidence"]
        maximum = 2**64 - 1
        usage = {axis: maximum for axis in MODULE.TELEMETRY_AXES}
        for endpoint in MODULE.RUNTIME_RING_SCOPES:
            ring = evidence[endpoint]
            ring.update(sequence=256, dropped_records=0, samples=[])
            for index in range(1, 257):
                stamp = 600_000_000 - 1000 + index
                if endpoint == "lease_activity":
                    data = {"event": "stage_tagged", "usage": usage,
                            "live_stage_leases": [maximum] * 7, "unclassified_leases": maximum}
                else:
                    data = {"poll_start_us": stamp - 2, "poll_end_us": stamp - 1, "status": "playing",
                            "native_player_present": True, "native_playing": True, "clock_available": True,
                            "time_ms": 2**63 - 1, "player_generation": maximum, "generation_overflow": False}
                ring["samples"].append({"sequence": index, "timestamp_us": stamp, **data})
        snapshot["runtime_evidence"] = evidence
        snapshot["visible_work"]["schema_version"] = 2
        for endpoint in MODULE.VISIBLE_WORK_SCOPES:
            snapshot["visible_work"][endpoint].update(runtime_id=RUNTIME_ID, timestamp_scope=MODULE.RUNTIME_TIMESTAMP_SCOPE)
        api_root, path = self.diagnostics_file(receipt)
        self.assertLess(path.stat().st_size, MODULE.MAX_DIAGNOSTICS_RECEIPT_BYTES)
        loaded, outcome = self.read_live_diagnostics_file(api_root, receipt)
        self.assertEqual(outcome, "applied")
        records, _ = MODULE.runtime_evidence_records(loaded, "baseline", 60_000_000, None)
        self.assertEqual(len(records), 513)
        self.assertTrue(all(len(json.dumps(row).encode("utf-8")) < MODULE.MAX_LINE_BYTES for row in records))
        self.assertEqual(MODULE.MAX_DIAGNOSTICS_RECEIPT_BYTES, 1_982_464)

    def test_collector_raw_ring_gap_fails_before_writing_misleading_evidence(self):
        base = runtime_snapshot(0, 60_000_000, 1)
        _, previous = MODULE.runtime_evidence_records(base, "baseline", 60_000_100, None)
        sample = runtime_snapshot(60_000_000, 360_000_000, 2, base)
        sample["runtime_evidence"]["lease_activity"]["samples"].pop(0)
        with self.assertRaisesRegex(MODULE.Invalid, "ring count|sequence"):
            MODULE.runtime_evidence_records(sample, "measure", 360_000_100, previous)

    def people_count_fixture(self):
        rows = interaction_fixture()
        evidence = {"total_people": 10_000, "count_scope": "canonical_nonhidden_people",
                    "catalog_revision": 7, "schema_generation": "match-schema-v2",
                    "store_session_id": "0123456789abcdef0123456789abcdef"}
        for row in rows[1:-1]:
            row.update({"reported_endpoint_duration_us": 80_000,
                        "reported_endpoint_duration_scope": "match_people_open_render_ui_excluding_backend_and_vsync",
                        "reported_endpoint_current_state_confirmed": True, "reported_endpoint_rendered": True,
                        "reported_endpoint_result_count": 256, "reported_endpoint_query_present": False,
                        "reported_catalog_evidence": dict(evidence)})
        final_call = rows[-2]["call_end_timestamp_us"]
        for label, start, finish in (("catalog_before", 0, 50),
                                     ("catalog_after", final_call + 1, final_call + 50)):
            rows[-1][label] = {"receipt_action_id": str(uuid.uuid4()),
                               "call_start_timestamp_us": start, "call_end_timestamp_us": finish,
                               "fixture_manifest_sha256": rows[0]["fixture_manifest_sha256"],
                               "catalog_evidence": dict(evidence)}
        rows[-1]["observed_at_us"] = final_call + 100
        return rows

    def test_people_gate_requires_canonical_count_for_each_rendered_sample(self):
        rows = self.people_count_fixture()
        result = MODULE.analyze(self.capture(rows))
        self.assertTrue(result["gate_eligible"])
        self.assertEqual(result["endpoint_gate_verdict"], "pass")
        self.assertFalse(result["whole_interval_immutability_proven"])
        self.assertEqual(result["fixture_manifest_binding_scope"], "supplied_manifest_artifact_not_catalog_membership_attestation")
        rows[80].pop("reported_catalog_evidence")
        result = MODULE.analyze(self.capture(rows))
        self.assertFalse(result["gate_eligible"])
        self.assertEqual(result["endpoint_gate_verdict"], "pending_independent_fixture_count_proof")

    def test_people_observed_revision_count_schema_and_session_changes_are_invalid(self):
        for field, value in (("catalog_revision", 8), ("total_people", 9999),
                             ("total_people", True), ("count_scope", "canonical_all_people"),
                             ("schema_generation", "other"),
                             ("store_session_id", "f" * 32)):
            with self.subTest(field=field, value=value):
                rows = self.people_count_fixture()
                rows[80]["reported_catalog_evidence"][field] = value
                with self.assertRaises(MODULE.Invalid):
                    MODULE.analyze(self.capture(rows))

    def test_people_diagnostic_observations_are_uuid_causal_and_fixture_bound(self):
        for label, field, value in (
            ("catalog_before", "receipt_action_id", "not-uuid"),
            ("catalog_before", "call_end_timestamp_us", 101),
            ("catalog_after", "call_start_timestamp_us", 0),
            ("catalog_after", "fixture_manifest_sha256", "f" * 64),
        ):
            with self.subTest(label=label, field=field):
                rows = self.people_count_fixture()
                rows[-1][label][field] = value
                with self.assertRaises(MODULE.Invalid):
                    MODULE.analyze(self.capture(rows))
        rows = self.people_count_fixture()
        rows[-1]["catalog_before"]["receipt_action_id"] = rows[1]["receipt_action_id"]
        with self.assertRaises(MODULE.Invalid):
            MODULE.analyze(self.capture(rows))

    def test_catalog_observation_copies_only_same_gui_canonical_evidence(self):
        evidence = self.people_count_fixture()[1]["reported_catalog_evidence"]
        def diagnostics(cli, api_root, workspace, timeout, identity):
            identity["action_id"] = "12345678-1234-4123-8123-123456789012"
            return {"catalog": {"evidence": evidence}, "private": "must-not-copy"}, "applied"
        with mock.patch.object(MODULE, "runtime_diagnostics", side_effect=diagnostics), \
             mock.patch.object(MODULE.time, "monotonic_ns", side_effect=[100_000, 200_000]):
            observation = MODULE.collect_catalog_observation(Path("cli"), Path("api"), Path("work"), 1, 0, "b" * 64)
        self.assertEqual(observation["catalog_evidence"], evidence)
        self.assertEqual(set(observation), MODULE.CATALOG_OBSERVATION_FIELDS)
        self.assertNotIn("private", observation)

    def test_people_transport_preserves_safe_catalog_binding_and_rejects_private_fields(self):
        evidence = self.people_count_fixture()[1]["reported_catalog_evidence"]
        receipt = {"action_id": str(uuid.uuid4()), "kind": "match_intent", "status": "applied",
                   "result": {"endpoint": "open_people", "duration_us": 5,
                              "duration_scope": "match_people_open_render_ui_excluding_backend_and_vsync",
                              "catalog_evidence": evidence, "rendered": True,
                              "current_state_confirmed": True, "result_count": 256,
                              "query_present": False}}
        with mock.patch.object(MODULE, "run_cli", return_value=(0, receipt, 100, 100_100)):
            result = MODULE.interaction_outcome(Path("cli"), Path("api"), Path("work"), "match_people_10000_open", 1)
        self.assertEqual(result[4]["catalog_evidence"], evidence)
        for invalid in ({**evidence, "names": ["private"]}, {**evidence, "total_people": "private"}):
            self.assertIsNone(MODULE.safe_catalog_evidence(invalid))

    def capture(self, rows):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        path = Path(temp.name) / "run.jsonl"
        path.write_bytes(b"".join(MODULE.canonical_json_line(row) for row in rows))
        return path

    def runtime_snapshot(self, progress=None, scope="canonical_all_index_jobs", jobs=None):
        empty = {axis: 0 for axis in MODULE.TELEMETRY_AXES}
        telemetry = {"lifetime_id": "b2c8bd15-9601-40bc-86ac-2d0a4d7fa2e1",
                     "scope": "governor_lifetime_including_warmup", "current_usage": empty,
                     "peak_usage": empty, "acquisitions": 0, "replacements": 0, "releases": 0,
                     "preparation_releases": 0, "pressure_events": 0, "overflow": False}
        return {"execution": {"resource_telemetry": telemetry, "resource_usage": empty,
                               "resource_budget": {axis: 0 for axis in MODULE.TELEMETRY_AXES},
                               "holds": [], "index_stage": "", "index_stage_counts": {},
                               "index_stage_scope": "persisted_asset_next_stage_counts"},
                "jobs": [] if jobs is None else jobs,
                "job_progress": progress, "job_progress_scope": scope}

    def full_diagnostics_receipt(self):
        jobs = [{"job_id": f"00000000-0000-4000-8000-{index:012d}", "lifecycle": "completed",
                 "discovered": 2**64 - 1, "completed": 2**64 - 1, "failed": 0,
                 "skipped": 0, "failure_code": None} for index in range(200)]
        snapshot = self.runtime_snapshot({key: 0 for key in ("discovered", "completed", "failed", "skipped")}, jobs=jobs)
        snapshot["visible_work"] = {"schema_version": 1}
        for endpoint, scope in MODULE.VISIBLE_WORK_SCOPES.items():
            snapshot["visible_work"][endpoint] = {
                "lifetime_id": VISIBLE_IDS[endpoint], "endpoint_scope": scope,
                "captured_at_us": 600_000_000, "sequence": 256, "dropped_records": 0,
                "abandoned": 0, "overflow": False, "pending": 0,
                "samples": [{"sequence": index, "start_us": index * 1000,
                             "end_us": index * 1000 + 800, "duration_us": 800}
                            for index in range(1, 257)]}
        return {"action_id": "44444444-4444-4444-8444-444444444444",
                "kind": "match_runtime_diagnostics", "status": "applied",
                "result": {"endpoint_scope": "running_gui_governor_lifetime", "snapshot": snapshot}}

    def diagnostics_file(self, receipt, extra_bytes=0):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        api_root = Path(temp.name)
        (api_root / "receipts").mkdir()
        data = json.dumps(receipt, indent=2).encode("utf-8")
        if extra_bytes:
            data += b" " * (MODULE.MAX_DIAGNOSTICS_RECEIPT_BYTES - len(data) + extra_bytes)
        path = api_root / "receipts" / f'{receipt["action_id"]}.json'
        path.write_bytes(data)
        return api_root, path

    def read_live_diagnostics_file(self, api_root, receipt):
        initial = {"action_id": receipt["action_id"], "kind": receipt["kind"], "status": "accepted"}
        started = time.monotonic_ns()
        with mock.patch.object(MODULE, "run_cli", return_value=(0, initial, started, started + 1)):
            return MODULE.runtime_diagnostics(Path("unused-cli"), api_root, api_root, 1)

    def test_pretty_full_diagnostics_rings_and_200_jobs_fit_separate_bounded_receipt(self):
        receipt = self.full_diagnostics_receipt()
        api_root, path = self.diagnostics_file(receipt)
        self.assertGreater(path.stat().st_size, MODULE.MAX_LINE_BYTES)
        self.assertLess(path.stat().st_size, MODULE.MAX_DIAGNOSTICS_RECEIPT_BYTES)
        snapshot, outcome = self.read_live_diagnostics_file(api_root, receipt)
        self.assertEqual(outcome, "applied")
        self.assertEqual(snapshot, receipt["result"]["snapshot"])
        series = MODULE.visible_work_snapshot(snapshot)
        self.assertTrue(all(len(row["samples"]) == 256 for row in series.values()))
        self.assertEqual(MODULE.MAX_LINE_BYTES, 16 * 1024)
        with self.assertRaisesRegex(MODULE.Invalid, "record exceeds 16 KiB"):
            MODULE.canonical_json_line(receipt)
        with self.assertRaisesRegex(MODULE.Invalid, "terminal receipt exceeds"):
            MODULE.terminal_receipt(api_root, receipt["action_id"], time.monotonic_ns() + 1_000_000_000,
                                    "match_runtime_diagnostics")

    def test_terminal_receipt_rejects_path_escape_and_mismatched_identity(self):
        with self.assertRaisesRegex(MODULE.Invalid, "canonical lowercase UUID"):
            MODULE.terminal_receipt(Path("unused"), "../outside", time.monotonic_ns() + 1_000_000_000,
                                    "match_runtime_diagnostics")
        requested_id = "11111111-1111-4111-8111-111111111111"
        api_root, path = self.diagnostics_file({"action_id": requested_id,
                                                "kind": "match_runtime_diagnostics",
                                                "status": "applied"})
        wrong_id = {"action_id": "22222222-2222-4222-8222-222222222222",
                    "kind": "match_runtime_diagnostics", "status": "applied"}
        path.write_bytes(json.dumps(wrong_id).encode())
        with self.assertRaisesRegex(MODULE.Invalid, "action_id does not match"):
            MODULE.terminal_receipt(api_root, requested_id, time.monotonic_ns() + 1_000_000_000,
                                    "match_runtime_diagnostics")
        wrong_kind = {"action_id": requested_id, "kind": "unrelated", "status": "applied"}
        path.write_bytes(json.dumps(wrong_kind).encode())
        with self.assertRaisesRegex(MODULE.Invalid, "kind does not match"):
            MODULE.terminal_receipt(api_root, requested_id, time.monotonic_ns() + 1_000_000_000,
                                    "match_runtime_diagnostics")

    def test_interaction_terminal_deadline_uses_original_cli_start(self):
        action_id = "11111111-1111-4111-8111-111111111111"
        start_ns = 1_000_000_000
        cli_end_ns = start_ns + 1_900_000_000
        terminal = {"action_id": action_id, "kind": "match_settings_manage_people", "status": "applied",
                    "result": {"endpoint": "settings_manage_people_acknowledgement", "duration_us": 10,
                               "duration_scope": "ui_state_rendered_by_render_ui_excluding_backend_and_vsync",
                               "current_state_confirmed": True, "rendered": True,
                               "result_count": 0, "query_present": False}}
        with mock.patch.object(MODULE, "run_cli", return_value=(0, {
                "action_id": action_id, "kind": "match_settings_manage_people", "status": "accepted"},
                start_ns, cli_end_ns)), mock.patch.object(MODULE, "terminal_receipt", return_value=terminal) as poll:
            with mock.patch.object(MODULE.time, "monotonic_ns", return_value=cli_end_ns + 50_000_000):
                result = MODULE.interaction_outcome(Path("unused"), Path("unused"), Path("unused"),
                                                    "settings_manage_people_acknowledgement", 2.0)
        self.assertEqual(poll.call_args.args[2], start_ns + 2_000_000_000)
        self.assertEqual(result[0], "applied")

    def test_runtime_diagnostics_terminal_deadline_uses_original_cli_start(self):
        action_id = "11111111-1111-4111-8111-111111111111"
        start_ns = 1_000_000_000
        cli_end_ns = start_ns + 1_900_000_000
        snapshot = self.full_diagnostics_receipt()["result"]["snapshot"]
        terminal = {"action_id": action_id, "kind": "match_runtime_diagnostics", "status": "applied",
                    "result": {"endpoint_scope": "running_gui_governor_lifetime", "snapshot": snapshot}}
        with mock.patch.object(MODULE, "run_cli", return_value=(0, {
                "action_id": action_id, "kind": "match_runtime_diagnostics", "status": "accepted"},
                start_ns, cli_end_ns)), mock.patch.object(MODULE, "terminal_receipt", return_value=terminal) as poll:
            snapshot, outcome = MODULE.runtime_diagnostics(Path("unused"), Path("unused"), Path("unused"), 2.0)
        self.assertEqual(poll.call_args.args[2], start_ns + 2_000_000_000)
        self.assertEqual(outcome, "applied")
        self.assertIsInstance(snapshot, dict)

    def test_callers_reject_terminal_receipts_not_bound_to_the_request(self):
        action_id = "11111111-1111-4111-8111-111111111111"
        start_ns = 1_000_000_000
        for terminal, expected_outcome in (
                ({"action_id": "22222222-2222-4222-8222-222222222222",
                  "kind": "match_settings_manage_people", "status": "applied"}, "terminal_receipt_mismatch"),
                ({"action_id": action_id, "kind": "unrelated", "status": "applied"}, "terminal_receipt_mismatch")):
            with self.subTest(terminal=terminal):
                with mock.patch.object(MODULE, "run_cli", return_value=(0, {
                        "action_id": action_id, "kind": "match_settings_manage_people", "status": "accepted"},
                        start_ns, start_ns + 100)), mock.patch.object(MODULE, "terminal_receipt", return_value=terminal):
                    result = MODULE.interaction_outcome(Path("unused"), Path("unused"), Path("unused"),
                                                        "settings_manage_people_acknowledgement", 2.0)
                self.assertEqual(result[0], expected_outcome)

                diagnostic_terminal = {**terminal, "result": {
                    "endpoint_scope": "running_gui_governor_lifetime",
                    "snapshot": self.full_diagnostics_receipt()["result"]["snapshot"]}}
                with mock.patch.object(MODULE, "run_cli", return_value=(0, {
                        "action_id": action_id, "kind": "match_runtime_diagnostics", "status": "accepted"},
                        start_ns, start_ns + 100)), mock.patch.object(MODULE, "terminal_receipt",
                                                                       return_value=diagnostic_terminal):
                    snapshot, outcome = MODULE.runtime_diagnostics(
                        Path("unused"), Path("unused"), Path("unused"), 2.0)
                self.assertIsNone(snapshot)
                self.assertEqual(outcome, "runtime_diagnostics_not_applied")

    def test_diagnostics_exact_cap_accepted_and_cap_plus_one_rejected_before_parse(self):
        receipt = self.full_diagnostics_receipt()
        api_root, path = self.diagnostics_file(receipt, extra_bytes=1)
        self.assertEqual(path.stat().st_size, MODULE.MAX_DIAGNOSTICS_RECEIPT_BYTES + 1)
        with self.assertRaisesRegex(MODULE.Invalid, "terminal receipt exceeds"):
            self.read_live_diagnostics_file(api_root, receipt)
        with path.open("r+b") as stream:
            stream.truncate(MODULE.MAX_DIAGNOSTICS_RECEIPT_BYTES)
        self.assertEqual(self.read_live_diagnostics_file(api_root, receipt)[1], "applied")

    def test_full_diagnostics_receipt_does_not_relax_closed_visible_schema(self):
        receipt = self.full_diagnostics_receipt()
        receipt["result"]["snapshot"]["visible_work"]["thumbnail"]["unknown"] = True
        api_root, _ = self.diagnostics_file(receipt)
        snapshot, outcome = self.read_live_diagnostics_file(api_root, receipt)
        self.assertEqual(outcome, "applied")
        with self.assertRaisesRegex(MODULE.Invalid, "fields do not match"):
            MODULE.visible_work_snapshot(snapshot)

    def test_live_progress_uses_canonical_all_jobs_not_recent_200_projection(self):
        projection = [{"discovered": 1, "completed": 1, "failed": 0, "skipped": 0}
                      for _ in range(200)]
        canonical = {"discovered": 9123, "completed": 8456, "failed": 17, "skipped": 650}
        row, _, _ = MODULE.concurrency_sample(
            self.runtime_snapshot(canonical, jobs=projection), "measure", 1, None)
        self.assertEqual(row["indexing_progress"], canonical)
        self.assertEqual(row["indexing_progress_scope"], "canonical_all_index_jobs")

    def test_live_progress_rejects_missing_malformed_or_wrong_scope(self):
        valid = {"discovered": 0, "completed": 0, "failed": 0, "skipped": 0}
        for progress, scope in ((None, "canonical_all_index_jobs"),
                                ({"discovered": 0}, "canonical_all_index_jobs"),
                                (valid, "recent_jobs_200"),
                                ({**valid, "completed": True}, "canonical_all_index_jobs")):
            with self.subTest(progress=progress, scope=scope):
                with self.assertRaisesRegex(MODULE.Invalid, "canonical all-index-job progress"):
                    MODULE.concurrency_sample(self.runtime_snapshot(progress, scope), "measure", 1, None)

    def test_live_progress_accepts_canonical_empty_library_as_zero_counts(self):
        zeros = {key: 0 for key in ("discovered", "completed", "failed", "skipped")}
        row, _, _ = MODULE.concurrency_sample(self.runtime_snapshot(zeros), "measure", 1, None)
        self.assertEqual(row["indexing_progress"], zeros)
        self.assertEqual(row["indexing_progress_scope"], "canonical_all_index_jobs")

    def test_interaction_nearest_rank_and_scope_pending_without_independent_fixture_proof(self):
        path = self.capture(interaction_fixture())
        result = MODULE.analyze(path)
        self.assertEqual(result["sample_count"], 200)
        self.assertEqual(result["cli_to_terminal_receipt_p95_us"], 100_000)
        self.assertIsNone(result["endpoint_duration_p95_us"])
        self.assertEqual(result["metric_scope"], "facial_cli_invocation_to_terminal_applied_receipt")
        self.assertEqual(result["verdict"], "pending")
        self.assertFalse(result["gate_eligible"])

    def test_endpoint_budget_uses_ui_measurements_not_cli_transport_time(self):
        rows = interaction_fixture("settings_manage_people_acknowledgement")
        rows[0]["fixture_people_count_declared"] = None
        for row in interaction_rows(rows):
            row["reported_endpoint_duration_us"] = 80_000
            row["reported_endpoint_duration_scope"] = "ui_state_rendered_by_render_ui_excluding_backend_and_vsync"
            row["reported_endpoint_current_state_confirmed"] = True
            row["reported_endpoint_rendered"] = True
            row["reported_endpoint_result_count"] = 0
            row["reported_endpoint_query_present"] = False
            row["duration_us"] = 900_000
            row["call_end_timestamp_us"] = row["call_start_timestamp_us"] + 900_000
        timestamp = 0
        for setup, call in zip(rows[1:-1:2], rows[2:-1:2]):
            setup["timestamp_us"] = timestamp + 50
            call["context_setup_timestamp_us"] = setup["timestamp_us"]
            call["call_start_timestamp_us"] = timestamp + 100
            call["call_end_timestamp_us"] = call["call_start_timestamp_us"] + call["duration_us"]
            timestamp = call["call_end_timestamp_us"]
        rows[-1]["observed_at_us"] = 10_000_000_000
        result = MODULE.analyze(self.capture(rows))
        self.assertEqual(result["cli_to_terminal_receipt_p95_us"], 900_000)
        self.assertEqual(result["endpoint_duration_p95_us"], 80_000)
        self.assertTrue(result["performance_threshold_observed"])
        self.assertTrue(result["gate_eligible"])
        self.assertEqual(result["endpoint_gate_verdict"], "pass")

    def test_endpoint_metric_requires_exact_render_and_state_receipt(self):
        rows = interaction_fixture("settings_manage_people_acknowledgement")
        rows[0]["fixture_people_count_declared"] = None
        for row in interaction_rows(rows):
            row.update({"reported_endpoint_duration_us": 40_000,
                        "reported_endpoint_duration_scope": "ui_state_rendered_by_render_ui_excluding_backend_and_vsync",
                        "reported_endpoint_current_state_confirmed": True,
                        "reported_endpoint_rendered": False,
                        "reported_endpoint_result_count": 0,
                        "reported_endpoint_query_present": False})
        result = MODULE.analyze(self.capture(rows))
        self.assertEqual(result["endpoint_duration_sample_count"], 0)
        self.assertIsNone(result["performance_threshold_observed"])

    def test_settings_route_requires_a_fresh_rendered_context_before_every_call(self):
        rows = interaction_fixture("settings_manage_people_acknowledgement")
        rows[0]["fixture_people_count_declared"] = None
        for row in interaction_rows(rows):
            row.update({"reported_endpoint_duration_us": 50_000,
                        "reported_endpoint_duration_scope": "ui_state_rendered_by_render_ui_excluding_backend_and_vsync",
                        "reported_endpoint_current_state_confirmed": True,
                        "reported_endpoint_rendered": True,
                        "reported_endpoint_result_count": 0,
                        "reported_endpoint_query_present": False,
                        "reported_endpoint_desired_mode": None})
        result = MODULE.analyze(self.capture(rows))
        self.assertEqual(len([row for row in rows if row.get("record_type") == "context_setup"]), 220)
        self.assertEqual(result["endpoint_duration_sample_count"], 200)
        self.assertTrue(result["gate_eligible"])

    def test_settings_route_rejects_missing_or_late_context_setup(self):
        rows = interaction_fixture("settings_manage_people_acknowledgement")
        rows[0]["fixture_people_count_declared"] = None
        rows = [row for row in rows if row.get("record_type") != "context_setup"]
        with self.assertRaisesRegex(MODULE.Invalid, "one rendered context reset per call"):
            MODULE.analyze(self.capture(rows))

        rows = interaction_fixture("settings_manage_people_acknowledgement")
        rows[0]["fixture_people_count_declared"] = None
        first_setup = next(row for row in rows if row.get("record_type") == "context_setup")
        first_interaction = next(row for row in rows if row.get("record_type") == "interaction")
        first_setup["timestamp_us"] = first_interaction["call_start_timestamp_us"]
        first_interaction["context_setup_timestamp_us"] = first_setup["timestamp_us"]
        with self.assertRaisesRegex(MODULE.Invalid, "not preceded by its exact rendered context reset"):
            MODULE.analyze(self.capture(rows))

    def test_interaction_rejects_measure_before_warmup_with_increasing_timestamps(self):
        for endpoint in ("cached_autocomplete", "settings_manage_people_acknowledgement"):
            with self.subTest(endpoint=endpoint):
                rows = interaction_fixture(endpoint)
                rows[0]["fixture_people_count_declared"] = None
                body = rows[1:-1]
                body = ([row for row in body if row["phase"] == "measure"]
                        + [row for row in body if row["phase"] == "warmup"])
                timestamp = 0
                setup_timestamp = None
                for row in body:
                    if row["record_type"] == "context_setup":
                        setup_timestamp = timestamp + 50
                        row["timestamp_us"] = setup_timestamp
                    else:
                        row.update({"call_start_timestamp_us": timestamp + 100,
                                    "call_end_timestamp_us": timestamp + 1100,
                                    "reported_endpoint_duration_us": 400,
                                    "reported_endpoint_duration_scope": "ui_state_rendered_by_render_ui_excluding_backend_and_vsync",
                                    "reported_endpoint_current_state_confirmed": True,
                                    "reported_endpoint_rendered": True,
                                    "reported_endpoint_result_count": 0,
                                    "reported_endpoint_query_present": endpoint == "cached_autocomplete"})
                        if setup_timestamp is not None:
                            row["context_setup_timestamp_us"] = setup_timestamp
                        timestamp += 1100
                rows = [rows[0], *body, {**rows[-1], "observed_at_us": timestamp + 1}]
                with self.assertRaisesRegex(MODULE.Invalid, "20 warmup calls followed by 200 measured calls"):
                    MODULE.analyze(self.capture(rows))

    def test_interaction_rejects_reordered_ordinals_and_detached_context_setup(self):
        rows = interaction_fixture("cached_autocomplete")
        rows[1]["ordinal"], rows[2]["ordinal"] = rows[2]["ordinal"], rows[1]["ordinal"]
        with self.assertRaisesRegex(MODULE.Invalid, "in ordinal order"):
            MODULE.analyze(self.capture(rows))
        rows = interaction_fixture("settings_manage_people_acknowledgement")
        # Move the second reset before the first call, preserving its exact
        # action ID and timestamp correlation with the later second call.
        second_setup = rows.pop(3)
        rows.insert(2, second_setup)
        with self.assertRaisesRegex(MODULE.Invalid, "immediately follow its matching context setup"):
            MODULE.analyze(self.capture(rows))
        rows = interaction_fixture("settings_manage_people_acknowledgement")
        rows[3]["timestamp_us"] = rows[2]["call_start_timestamp_us"] + 1
        rows[4]["context_setup_timestamp_us"] = rows[3]["timestamp_us"]
        with self.assertRaisesRegex(MODULE.Invalid, "not preceded by its exact rendered context reset"):
            MODULE.analyze(self.capture(rows))

    def test_pause_feedback_is_measured_separately_from_safe_unit_proof(self):
        rows = interaction_fixture("operator_pause_feedback")
        rows[0]["fixture_people_count_declared"] = None
        for row in interaction_rows(rows):
            row.update({"reported_endpoint_duration_us": 90_000,
                        "reported_endpoint_duration_scope": "operator_pause_feedback_render_ui_excluding_backend_and_vsync",
                        "reported_endpoint_current_state_confirmed": True,
                        "reported_endpoint_rendered": True,
                        "reported_endpoint_result_count": 0,
                        "reported_endpoint_query_present": False,
                        "reported_endpoint_desired_mode": "operator_paused"})
        result = MODULE.analyze(self.capture(rows))
        self.assertEqual(result["endpoint_duration_sample_count"], 200)
        self.assertEqual(result["endpoint_duration_p95_us"], 90_000)
        self.assertTrue(result["gate_eligible"])
        self.assertEqual(result["endpoint_gate_verdict"], "pass")

    def test_reported_ui_duration_cannot_exceed_containing_cli_interval(self):
        rows = interaction_fixture("settings_manage_people_acknowledgement")
        rows[0]["fixture_people_count_declared"] = None
        for row in interaction_rows(rows):
            row.update({"reported_endpoint_duration_us": 50_000,
                        "reported_endpoint_duration_scope": "ui_state_rendered_by_render_ui_excluding_backend_and_vsync",
                        "reported_endpoint_current_state_confirmed": True,
                        "reported_endpoint_rendered": True,
                        "reported_endpoint_result_count": 0,
                        "reported_endpoint_query_present": False})
        result = MODULE.analyze(self.capture(rows))
        self.assertTrue(result["gate_eligible"])
        self.assertEqual(result["endpoint_gate_verdict"], "pass")
        first_measured = next(row for row in interaction_rows(rows) if row["phase"] == "measure")
        first_measured["reported_endpoint_duration_us"] = first_measured["duration_us"] + 1
        with self.assertRaisesRegex(MODULE.Invalid, "exceeds its containing CLI-to-terminal interval"):
            MODULE.analyze(self.capture(rows))

    def test_cached_interaction_rejects_overlapping_serial_calls(self):
        rows = interaction_fixture("cached_autocomplete")
        first, second = interaction_rows(rows)[:2]
        second["call_start_timestamp_us"] = first["call_start_timestamp_us"] + 1
        second["call_end_timestamp_us"] = second["call_start_timestamp_us"] + second["duration_us"]
        with self.assertRaisesRegex(MODULE.Invalid, "must not overlap the previous call interval"):
            MODULE.analyze(self.capture(rows))

    def test_combined_pause_route_safe_unit_endpoint_stays_pending(self):
        result = MODULE.analyze(self.capture(interaction_fixture("pause_route_feedback_and_safe_unit")))
        self.assertFalse(result["gate_eligible"])
        self.assertIsNone(result["performance_threshold_observed"])
        self.assertEqual(result["endpoint_gate_verdict"], "pending_combined_pause_route_and_safe_unit_proofs")

    def test_collector_resets_settings_outside_each_measured_call(self):
        with tempfile.TemporaryDirectory() as root_name:
            root = Path(root_name)
            workspace = root / "workspace"
            api_root = root / "api"
            workspace.mkdir()
            api_root.mkdir()
            inputs = []
            for filename in ("facial-cli", "portable.exe", "hardware.json", "fixture.json"):
                path = root / filename
                path.write_bytes(b"fixture")
                inputs.append(path)
            origin = 1_000_000_000
            args = MODULE.argparse.Namespace(
                workspace_root=str(workspace), api_root=str(api_root), facial_cli=str(inputs[0]),
                packaged_portable=str(inputs[1]), hardware_manifest=str(inputs[2]),
                fixture_manifest=str(inputs[3]), endpoint="settings_manage_people_acknowledgement",
                fixture_people_count=None, query=None, media_key=None, catalog_revision=None,
                face_id=None, timeout_s=5.0, run_id="settings-context-test")
            calls = []

            def fake_interaction(_cli, _api, _workspace, endpoint, _timeout, *_args):
                calls.append(endpoint)
                ordinal = (len(calls) - 1) // 2
                base = origin + ordinal * 10_000_000
                if endpoint == "settings_context_reset":
                    return ("applied", base, base + 100_000, str(uuid.UUID(int=10_000 + ordinal, version=4)),
                            {"endpoint": "open_settings", "duration_us": 40,
                             "duration_scope": "match_settings_open_render_ui_excluding_backend_and_vsync",
                             "current_state_confirmed": True, "rendered": True, "result_count": 0,
                             "query_present": False, "desired_mode": None,
                             "navigation": {"action": "open_settings", "requested_offset": 0,
                                            "applied_offset": 0, "page_limit": 200}})
                self.assertEqual(endpoint, "settings_manage_people_acknowledgement")
                return ("applied", base + 200_000, base + 1_200_000,
                        str(uuid.UUID(int=20_000 + ordinal, version=4)),
                        {"endpoint": "settings_manage_people_acknowledgement", "duration_us": 800,
                         "duration_scope": "ui_state_rendered_by_render_ui_excluding_backend_and_vsync",
                         "current_state_confirmed": True, "rendered": True, "result_count": 0,
                         "query_present": False, "desired_mode": None, "navigation": None})

            with mock.patch.object(MODULE, "interaction_outcome", side_effect=fake_interaction), \
                    mock.patch.object(MODULE.time, "monotonic_ns", side_effect=[origin, origin + 3_000_000_000]):
                self.assertEqual(MODULE.collect_people(args), 0)
            self.assertEqual(len(calls), 440)
            self.assertEqual(calls[::2], ["settings_context_reset"] * 220)
            self.assertEqual(calls[1::2], ["settings_manage_people_acknowledgement"] * 220)
            capture = workspace / ".facial" / "benchmarks" / "settings-context-test.jsonl"
            raw = MODULE.load_records(capture)
            setups = [row for row in raw if row.get("record_type") == "context_setup"]
            measured = interaction_rows(raw)
            self.assertEqual(len(setups), 220)
            self.assertTrue(all(row["timestamp_us"] < sample["call_start_timestamp_us"]
                                for row, sample in zip(setups, measured)))
            self.assertTrue(all(sample["duration_us"] == 1000 for sample in measured))
            summary = MODULE.analyze(capture)
            self.assertEqual(summary["endpoint_duration_p95_us"], 800)

    def test_exact_unsupported_endpoint_rejected(self):
        with self.assertRaisesRegex(MODULE.Invalid, "requires exact query"):
            MODULE.interaction_outcome(Path("unused"), Path("unused"), Path("unused"), "cached_autocomplete", 1)

    def test_settings_context_consumer_reads_nested_navigation_fields_from_intent_receipt(self):
        action_id = "11111111-1111-4111-8111-111111111111"
        receipt = {"action_id": action_id, "kind": "match_intent", "status": "applied",
                   "result": {"endpoint": "open_settings", "duration_us": 50,
                              "duration_scope": "match_settings_open_render_ui_excluding_backend_and_vsync",
                              "rendered": True, "current_state_confirmed": True,
                              "result_count": 0, "query_present": False,
                              "navigation": {"action": "open_settings", "requested_offset": 0,
                                             "applied_offset": 0, "page_limit": 200}}}
        with mock.patch.object(MODULE, "run_cli", return_value=(0, receipt, 100, 150)):
            outcome = MODULE.interaction_outcome(Path("unused"), Path("unused"), Path("unused"),
                                                 "settings_context_reset", 2.0)
        self.assertEqual(outcome[0], "applied")
        self.assertTrue(MODULE.context_reset_applied(
            "settings_manage_people_acknowledgement", outcome[0], outcome[4]))

    def test_duplicate_json_key_rejected(self):
        with self.assertRaisesRegex(MODULE.Invalid, "duplicate JSON key"):
            MODULE.strict_json(b'{"record_type":"header","record_type":"end"}')

    def test_duplicate_or_out_of_order_timestamps_rejected(self):
        rows = interaction_fixture()
        rows[2]["call_start_timestamp_us"] = rows[1]["call_start_timestamp_us"]
        rows[2]["call_end_timestamp_us"] = rows[2]["call_start_timestamp_us"] + 1000
        with self.assertRaisesRegex(MODULE.Invalid, "strictly increasing"):
            MODULE.analyze(self.capture(rows))

    def test_incomplete_run_rejected(self):
        rows = interaction_fixture()
        rows[-1]["outcome"] = "invalid"
        with self.assertRaisesRegex(MODULE.Invalid, "completed end record"):
            MODULE.analyze(self.capture(rows))

    def test_incorrect_sample_count_rejected(self):
        rows = interaction_fixture()
        rows[-1]["sample_count"] = 219
        with self.assertRaisesRegex(MODULE.Invalid, "sample_count"):
            MODULE.analyze(self.capture(rows))

    def test_missing_concurrency_metrics_stay_pending(self):
        budget, baseline, terminal = governor_evidence()
        rows = concurrency_rows()
        rows.extend(visible_work_records(include_sample=False))
        rows.insert(-1, {"record_type": "governor_evidence", "measurement_start_us": 60_000_000,
                         "measurement_end_us": 660_000_001, "resource_budget": budget,
                         "baseline_resource_telemetry": baseline, "terminal_resource_telemetry": terminal})
        rows.append({"record_type": "end", "outcome": "completed", "sample_count": 3,
                     "observed_at_us": 660_000_001})
        result = MODULE.analyze(self.capture(rows))
        self.assertIn("visible_work.thumbnail.measured_samples", result["missing_fields"])
        self.assertTrue(result["terminal_lease_balance"])
        self.assertFalse(result["gate_eligible"])
        self.assertTrue(result["duration_contract_met"])
        self.assertEqual(result["visible_work_budget_verdicts"]["thumbnail"], "pending")

    def test_terminal_snapshot_completion_is_included_at_the_measured_end(self):
        budget, baseline, terminal = governor_evidence()
        start_counts = {"discovered": 1000, "completed": 500, "failed": 2, "skipped": 8}
        poll_counts = {"discovered": 1100, "completed": 550, "failed": 2, "skipped": 8}
        terminal_counts = {"discovered": 1110, "completed": 560, "failed": 2, "skipped": 8}
        end_us = 660_000_001
        rows = concurrency_rows((0, 60_000_000, 659_000_000))
        rows[1]["indexing_progress"] = {"discovered": 0, "completed": 0, "failed": 0, "skipped": 0}
        rows[2]["indexing_progress"] = start_counts
        rows[3]["indexing_progress"] = poll_counts
        terminal_row = {**rows[3], "timestamp_us": end_us, "indexing_progress": terminal_counts}
        rows.append(terminal_row)
        rows.extend(visible_work_records(run_end=end_us, include_sample=False))
        rows.insert(-1, {"record_type": "governor_evidence", "measurement_start_us": 60_000_000,
                         "measurement_end_us": end_us, "resource_budget": budget,
                         "baseline_resource_telemetry": baseline, "terminal_resource_telemetry": terminal})
        rows.append({"record_type": "end", "outcome": "completed", "sample_count": 4,
                     "observed_at_us": end_us})

        result = MODULE.analyze(self.capture(rows))

        self.assertEqual(result["indexing_progress_delta"], {
            "discovered": 110, "completed": 60, "failed": 0, "skipped": 0})
        self.assertEqual(result["measurement_end_us"], end_us)
        self.assertAlmostEqual(result["completed_assets_per_second"], 60 * 1_000_000 / (end_us - 60_000_000))

    def test_concurrency_window_rejects_short_measurement(self):
        budget, baseline, terminal = governor_evidence()
        rows = concurrency_rows((0, 60_000_000, 659_999_999))
        rows.extend(visible_work_records(run_end=659_999_999))
        rows.insert(-1, {"record_type": "governor_evidence", "measurement_start_us": 60_000_000,
                         "measurement_end_us": 659_999_999, "resource_budget": budget,
                         "baseline_resource_telemetry": baseline, "terminal_resource_telemetry": terminal})
        rows.append({"record_type": "end", "outcome": "completed", "sample_count": 3,
                     "observed_at_us": 659_999_999})
        result = MODULE.analyze(self.capture(rows))
        self.assertFalse(result["duration_contract_met"])
        self.assertEqual(result["verdict"], "pending")

    def test_visible_work_sequences_are_retained_once_and_percentiled(self):
        budget, baseline, terminal = governor_evidence()
        rows = concurrency_rows()
        rows.extend(visible_work_records())
        rows.insert(-1, {"record_type": "governor_evidence", "measurement_start_us": 60_000_000,
                         "measurement_end_us": 660_000_001, "resource_budget": budget,
                         "baseline_resource_telemetry": baseline, "terminal_resource_telemetry": terminal})
        rows.append({"record_type": "end", "outcome": "completed", "sample_count": 3,
                     "observed_at_us": 660_000_001})
        result = MODULE.analyze(self.capture(rows))
        for endpoint in MODULE.VISIBLE_WORK_SCOPES:
            self.assertEqual(result["visible_work"][endpoint]["sample_count"], 1)
            self.assertEqual(result["visible_work"][endpoint]["p95_us"], 800)
        self.assertEqual(result["verdict"], "pending")
        self.assertFalse(result["gate_eligible"])

    def test_visible_work_sequence_gap_or_duplicate_is_rejected(self):
        budget, baseline, terminal = governor_evidence()
        rows = concurrency_rows()
        rows.extend(visible_work_records())
        sample = next(row for row in rows if row.get("record_type") == "visible_work_sample")
        sample["sequence"] = 2
        rows.insert(-1, {"record_type": "governor_evidence", "measurement_start_us": 60_000_000,
                         "measurement_end_us": 660_000_001, "resource_budget": budget,
                         "baseline_resource_telemetry": baseline, "terminal_resource_telemetry": terminal})
        rows.append({"record_type": "end", "outcome": "completed", "sample_count": 3,
                     "observed_at_us": 660_000_001})
        with self.assertRaisesRegex(MODULE.Invalid, "sequence gap"):
            MODULE.analyze(self.capture(rows))

    def test_visible_work_overflow_and_scope_mismatch_fail_closed(self):
        snapshot = {"visible_work": {"schema_version": 1}}
        series = {}
        for endpoint, scope in MODULE.VISIBLE_WORK_SCOPES.items():
            series[endpoint] = {"lifetime_id": VISIBLE_IDS[endpoint], "endpoint_scope": scope,
                                "captured_at_us": 1, "sequence": 0, "dropped_records": 0,
                                "abandoned": 0, "overflow": False, "pending": 0, "samples": []}
        snapshot["visible_work"].update(series)
        series["thumbnail"]["overflow"] = True
        with self.assertRaisesRegex(MODULE.Invalid, "overflow"):
            MODULE.visible_work_snapshot(snapshot)
        series["thumbnail"]["overflow"] = False
        series["thumbnail"]["endpoint_scope"] = "wrong_scope"
        with self.assertRaisesRegex(MODULE.Invalid, "scope mismatch"):
            MODULE.visible_work_snapshot(snapshot)

    def test_live_sample_fails_on_lifetime_change_or_overflow(self):
        empty = {axis: 0 for axis in MODULE.TELEMETRY_AXES}
        telemetry = {"lifetime_id": "b2c8bd15-9601-40bc-86ac-2d0a4d7fa2e1",
                     "scope": "governor_lifetime_including_warmup", "current_usage": empty,
                     "peak_usage": empty, "acquisitions": 0, "replacements": 0, "releases": 0,
                     "preparation_releases": 0, "pressure_events": 0, "overflow": False}
        snapshot = self.runtime_snapshot({"discovered": 0, "completed": 0, "failed": 0, "skipped": 0})
        snapshot["execution"]["resource_telemetry"] = telemetry
        with self.assertRaisesRegex(MODULE.Invalid, "lifetime changed"):
            MODULE.concurrency_sample(snapshot, "measure", 1, None, "c2c8bd15-9601-40bc-86ac-2d0a4d7fa2e1")
        telemetry["overflow"] = True
        with self.assertRaisesRegex(MODULE.Invalid, "overflow"):
            MODULE.concurrency_sample(snapshot, "measure", 1, None)

    def test_saturation_cannot_pass_from_self_declared_visible_budgets(self):
        budget, baseline, terminal = governor_evidence()
        rows = [{"record_type": "header", "schema_version": 1, "run_id": "test-run",
                 "workload": "saturation", "facial_cli_sha256": "a" * 64,
                 "fixture_manifest_sha256": "b" * 64, "packaged_binary_sha256": "c" * 64,
                 "hardware_manifest_sha256": "d" * 64, "input_script_sha256": "e" * 64},
                {"record_type": "saturation", "resource_budget": budget,
                 "baseline_resource_telemetry": baseline, "terminal_resource_telemetry": terminal,
                 "thumbnail_budget_verdict": "pass", "playback_budget_verdict": "pass",
                 "navigation_budget_verdict": "pass"},
                {"record_type": "end", "outcome": "completed", "sample_count": 1}]
        result = MODULE.analyze(self.capture(rows))
        self.assertEqual(result["verdict"], "pending")
        self.assertEqual(result["governor_component_verdict"], "pass")
        self.assertEqual(result["workload_verdict"], "pending")
        self.assertIn("independent_simultaneous_workload_binding", result["missing_fields"])
        self.assertFalse(result["gate_eligible"])
        rows[1]["terminal_resource_telemetry"]["current_usage"]["admitted_items"] = 1
        result = MODULE.analyze(self.capture(rows))
        self.assertEqual(result["verdict"], "fail")
        self.assertEqual(result["governor_component_verdict"], "fail")
        self.assertFalse(result["gate_eligible"])


if __name__ == "__main__":
    unittest.main()
