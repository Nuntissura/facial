"""WP-087 analyzer regressions; synthetic inputs prove the validator only."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


spec = importlib.util.spec_from_file_location(
    "analyzer", Path(__file__).with_name("analyze-match-render-samples.py")
)
analyzer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(analyzer)


class RenderSamplesTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="wp087-analyzer-")
        self.root = Path(self.temp.name)

    def tearDown(self):
        self.temp.cleanup()

    @staticmethod
    def end(frames):
        return {"record_type": "end", "outcome": "completed", "sample_count": len(frames), "observed_at_us": 150_000_000}

    def write_run(self, name="run", state="active_quiet", duration=1000, change=None):
        header = {
            "schema_version": 1, "record_type": "run", "run_id": name, "state": state,
            "app_version": "0.1.0", "git_commit": "e" * 40, "cargo_lock_sha256": "f" * 64,
            "model_generation": "unconfigured", "schema_generation": "19",
            "timestamp_scope": "previous_frame_cpu_usage_observed_at_next_update_monotonic_since_capture_start",
            "admission_evidence": {"path": ".facial/benchmarks/synthetic-evidence.json", "sha256": "1" * 64},
            "measurement_start_us": 30_000_000, "measurement_end_us": 150_000_000,
            "warmup_seconds": 30, "package_sha256": "a" * 64,
            "fixture_generation": "synthetic-validator-only", "cache_state": "fixed",
            "input_script_sha256": "b" * 64, "hardware_manifest_sha256": "c" * 64,
            "display_profile_sha256": "d" * 64, "power_mode": "fixed",
            "metric_scope": "eframe_update_render_cpu_time_excluding_vsync",
            "admission_counts": {"match_workers": 0, "model_loads": 0, "match_index_queries": 0},
        }
        if change:
            header.update(change)
        frames = [{"record_type": "frame", "frame_end_timestamp_us": 30_000_000 + i * 120_000_000 // 7200,
                   "frame_duration_us": duration} for i in range(7200)]
        path = self.root / (name + ".jsonl")
        path.write_text("\n".join(json.dumps(row) for row in [header, *frames, self.end(frames)]) + "\n", encoding="utf-8")
        return path, header, frames

    def test_exact_windows_and_nearest_rank(self):
        path, _, _ = self.write_run()
        result = analyzer.analyze(path, 100000, "normal_typical")
        self.assertEqual(result["rolling_verdict"], "pass")
        self.assertEqual(result["required_window_count"], 473)
        self.assertEqual(result["sample_count"], 7200)
        self.assertEqual(analyzer.nearest_rank([1, 2, 3, 4], .5), 2)
        self.assertEqual(analyzer.nearest_rank([1, 2, 3, 4], .99), 4)

    def test_short_local_spike_fails_worst_window(self):
        path, header, frames = self.write_run()
        for row in frames[120:130]:
            row["frame_duration_us"] = 50000
        path.write_text("\n".join(json.dumps(row) for row in [header, *frames, self.end(frames)]) + "\n", encoding="utf-8")
        result = analyzer.analyze(path, 100000, "normal_typical")
        self.assertEqual(result["rolling_verdict"], "fail")
        self.assertEqual(result["worst_window_p99_us"], 50000)
        self.assertEqual(result["p99_us"], 1000)

    def test_disabled_cannot_admit_work(self):
        path, _, _ = self.write_run(state="match_disabled", change={"admission_counts": {
            "match_workers": 1, "model_loads": 0, "match_index_queries": 0}})
        with self.assertRaises(analyzer.InputError):
            analyzer.analyze(path, 100000, "normal_typical")

    def test_gap_fails_even_with_enough_total_samples(self):
        path, header, frames = self.write_run()
        for i in range(150):
            frames[i]["frame_end_timestamp_us"] = 33_000_000 + i
        frames.sort(key=lambda row: row["frame_end_timestamp_us"])
        path.write_text("\n".join(json.dumps(row) for row in [header, *frames, self.end(frames)]) + "\n", encoding="utf-8")
        with self.assertRaises(analyzer.InputError):
            analyzer.analyze(path, 100000, "normal_typical")

    def test_ab_invariant_mismatch_rejects(self):
        runs = []
        for i, state in enumerate(["match_disabled", "active_quiet", "active_quiet", "match_disabled"]):
            path, _, _ = self.write_run(str(i), state, change={"power_mode": "changed"} if i == 2 else None)
            runs.append({"state": state, "path": str(path)})
        manifest = self.root / "ab.json"
        manifest.write_text(json.dumps({"schema_version": 1, "runs": runs}), encoding="utf-8")
        with self.assertRaises(analyzer.InputError):
            analyzer.analyze_ab_manifest(manifest, 100000)

    def test_normal_state_cannot_select_pathological_budget(self):
        path, _, _ = self.write_run(duration=25000)
        with self.assertRaises(analyzer.InputError):
            analyzer.analyze(path, 100000, "pathological_1000")

    def test_duplicate_timestamps_rejected(self):
        path, header, frames = self.write_run()
        frames[1]["frame_end_timestamp_us"] = frames[0]["frame_end_timestamp_us"]
        path.write_text("\n".join(json.dumps(row) for row in [header, *frames, self.end(frames)]) + "\n", encoding="utf-8")
        with self.assertRaises(analyzer.InputError):
            analyzer.analyze(path, 100000, "normal_typical")

    def test_missing_or_early_terminal_record_rejected(self):
        path, header, frames = self.write_run()
        raw = "\n".join(json.dumps(row) for row in [header, *frames]) + "\n"
        path.write_text(raw, encoding="utf-8")
        with self.assertRaises(analyzer.InputError):
            analyzer.analyze(path, 100000, "normal_typical")
        end = self.end(frames)
        end["observed_at_us"] -= 1
        path.write_text(raw + json.dumps(end) + "\n", encoding="utf-8")
        with self.assertRaises(analyzer.InputError):
            analyzer.analyze(path, 100000, "normal_typical")

    def test_false_schema_version_and_duplicate_fields_rejected(self):
        path, _, _ = self.write_run(change={"schema_version": True})
        with self.assertRaises(analyzer.InputError):
            analyzer.analyze(path, 100000, "normal_typical")
        with self.assertRaises(analyzer.InputError):
            json.loads('{"state":"match_disabled","state":"active_quiet"}', object_pairs_hook=analyzer.unique_object)

    def test_git_identity_shape_matches_runtime_producer(self):
        for identity in ["A" * 40, "B" * 64]:
            path, _, _ = self.write_run(change={"git_commit": identity})
            result = analyzer.analyze(path, 100000, "normal_typical")
            self.assertEqual(result["run_header"]["git_commit"], identity.lower())

    def test_declared_overflow_is_never_green(self):
        path, header, frames = self.write_run()
        end = self.end(frames)
        end["outcome"] = "overflow"
        path.write_text("\n".join(json.dumps(row) for row in [header, *frames, end]) + "\n", encoding="utf-8")
        with self.assertRaises(analyzer.InputError):
            analyzer.analyze(path, 100000, "normal_typical")
        with self.assertRaises(analyzer.InputError):
            analyzer.analyze(path, 100, "normal_typical")

    def write_media_ab(self, candidate_duration=1000, change_runtime=None):
        artifacts = {
            "portable_executable_path": self.root / "synthetic-package.exe",
            "hardware_manifest_path": self.root / "hardware.json",
            "display_profile_path": self.root / "display.json",
            "input_script_path": self.root / "input.json",
        }
        artifacts["portable_executable_path"].write_bytes(b"synthetic-validator-input-only")
        hardware = {field: "synthetic-validator-only" for field in ("cpu_model", "gpu_model", "gpu_driver", "inference_backend", "os_edition", "os_build", "power_mode", "storage_kind", "media_root_kind", "network_link")}
        hardware.update(physical_cores=2, logical_cores=4, ram_bytes=1024, power_mode="fixed")
        artifacts["hardware_manifest_path"].write_text(json.dumps(hardware), encoding="utf-8")
        artifacts["display_profile_path"].write_text(json.dumps({"viewport_physical_px": [1920, 1080], "dpi_scale_percent": 100, "egui_pixels_per_point": 1.0, "font_family": "Inter", "font_size_pt": 19}), encoding="utf-8")
        artifacts["input_script_path"].write_text("{}", encoding="utf-8")
        digest_fields = {"portable_executable_path": "package_sha256", "hardware_manifest_path": "hardware_manifest_sha256", "display_profile_path": "display_profile_sha256", "input_script_path": "input_script_sha256"}
        changes = {digest_fields[field]: analyzer.sha256_file(artifact) for field, artifact in artifacts.items()}
        runs = []
        for index, state in enumerate(["media_labels_baseline", "media_labels_candidate", "media_labels_candidate", "media_labels_baseline"]):
            fixture = {"fixture_sha256": analyzer.MEDIA_FIXTURE_SHA256, "rows": 50_000, "assignment": "empty" if state.endswith("baseline") else "five_ordered", "build_ui_sha256": "3" * 64, "build_lib_sha256": "4" * 64, "build_collector_sha256": "5" * 64}
            run_path, header, frames = self.write_run("media-" + str(index), state, candidate_duration if state.endswith("candidate") else 1000, {**changes, "media_labels_fixture": fixture})
            end = self.end(frames)
            runtime = {"match_workers": 0, "model_loads": 0, "match_index_queries": 0, "match_database_requests": 0, "visible_tile_lookups": 14_400, "visible_tile_lookups_min": 2, "visible_tile_lookups_max": 2, "visible_work_frames": 7200, "display_observations": 7200, "display_valid": True, "input_valid": True, "viewport_physical_px": [1920, 1080], "native_pixels_per_point": 1.0, "egui_pixels_per_point": 1.0, "font_size_pt": 19.0, "font_family": "Inter", "fixture_sha256": analyzer.MEDIA_FIXTURE_SHA256}
            if change_runtime and index == 1:
                runtime.update(change_runtime)
            end["runtime_evidence"] = runtime
            run_path.write_text("\n".join(json.dumps(row) for row in [header, *frames, end]) + "\n", encoding="utf-8")
            runs.append({"state": state, "path": run_path.name})
        manifest = self.root / "media-ab.json"
        manifest.write_text(json.dumps({"schema_version": 1, "runs": runs, **{field: artifact.name for field, artifact in artifacts.items()}}), encoding="utf-8")
        return manifest

    def test_media_strict_relative_delta_without_absolute_exemption(self):
        result = analyzer.analyze_media_label_ab_manifest(self.write_media_ab(1101), 100000)
        self.assertFalse(result["passes_delta_budget"])
        self.assertTrue(result["passes_absolute_budget"])
        self.assertEqual(result["overall_verdict"], "fail")
        result = analyzer.analyze_media_label_ab_manifest(self.write_media_ab(1100), 100000)
        self.assertEqual(result["overall_verdict"], "pass")
        self.assertEqual(result["release_verdict"], "pending_independent_package_and_runtime_evidence_review")

    def test_media_runtime_evidence_cannot_be_config_or_headless_claim(self):
        for mutation in ({"match_index_queries": 1}, {"match_database_requests": 1}, {"display_valid": False}, {"input_valid": False}, {"display_observations": 7199}, {"viewport_physical_px": [1280, 800]}, {"native_pixels_per_point": 2.0}, {"font_family": "Other"}, {"visible_tile_lookups": 0}, {"visible_tile_lookups_max": 3}, {"visible_work_frames": 7199}, {"visible_tile_lookups": 14_399}, {"fixture_sha256": "6" * 64}):
            with self.subTest(mutation=mutation), self.assertRaises(analyzer.InputError):
                analyzer.analyze_media_label_ab_manifest(self.write_media_ab(change_runtime=mutation), 100000)

    def test_media_manifest_hashes_actual_artifacts(self):
        manifest = self.write_media_ab()
        (self.root / "synthetic-package.exe").write_bytes(b"different-binary")
        with self.assertRaises(analyzer.InputError):
            analyzer.analyze_media_label_ab_manifest(manifest, 100000)

    def test_media_equal_sample_work_and_strict_absolute_boundary(self):
        result = analyzer.analyze_media_label_ab_manifest(self.write_media_ab(change_runtime={"visible_tile_lookups_min": 3, "visible_tile_lookups_max": 3, "visible_tile_lookups": 21_600}), 100000)
        self.assertFalse(result["passes_comparable_visible_work"])
        self.assertEqual(result["overall_verdict"], "fail")
        result = analyzer.analyze_media_label_ab_manifest(self.write_media_ab(16_700), 100000)
        self.assertFalse(result["passes_absolute_budget"])

    def test_media_worst_window_spike_is_not_hidden_by_pooled_percentile(self):
        manifest = self.write_media_ab()
        stream = self.root / "media-1.jsonl"
        records = [json.loads(line) for line in stream.read_text(encoding="utf-8").splitlines()]
        for row in records[121:131]:
            row["frame_duration_us"] = 50_000
        stream.write_text("\n".join(json.dumps(row) for row in records) + "\n", encoding="utf-8")
        result = analyzer.analyze_media_label_ab_manifest(manifest, 100000)
        self.assertTrue(result["passes_delta_budget"])
        self.assertEqual(result["rolling_verdict"], "fail")
        self.assertEqual(result["overall_verdict"], "fail")

    def test_media_requires_native_completed_terminal_evidence(self):
        manifest = self.write_media_ab()
        stream = self.root / "media-0.jsonl"
        records = [json.loads(line) for line in stream.read_text(encoding="utf-8").splitlines()]
        del records[-1]["runtime_evidence"]
        stream.write_text("\n".join(json.dumps(row) for row in records) + "\n", encoding="utf-8")
        with self.assertRaises(analyzer.InputError):
            analyzer.analyze_media_label_ab_manifest(manifest, 100000)


if __name__ == "__main__":
    unittest.main()
