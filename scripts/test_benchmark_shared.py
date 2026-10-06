"""python -B -m unittest discover -s scripts -p test_benchmark_shared.py -v"""

import copy
import contextlib
import gzip
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

import benchmark_shared as bench


class MetricsTests(unittest.TestCase):
    def test_optional_missing_tool_is_unavailable(self):
        result = bench.tool_version("tgrep-benchmark-nonexistent-tool-8b01870d")
        self.assertIsNone(result["value"])
        self.assertIn("not executable on harness PATH", result["reason"])

    def test_optional_nonzero_tool_is_unavailable(self):
        command = bench.command
        with patch.object(bench, "command", side_effect=lambda *args, **kwargs: command(
                [sys.executable, "-c", "import sys; sys.stderr.write('no toolchain'); sys.exit(2)"])):
            result = bench.tool_version(sys.executable)
        self.assertIsNone(result["value"])
        self.assertIn("exit 2", result["reason"])
        self.assertIn("no toolchain", result["reason"])

    def test_optional_timeout_reaps_before_returning_unavailable(self):
        command = bench.command
        with patch.object(bench, "command", side_effect=lambda *args, **kwargs: command(
                [sys.executable, "-c", "import time; time.sleep(30)"], timeout=.05)):
            result = bench.tool_version(sys.executable)
        self.assertIsNone(result["value"])
        self.assertIn("timed out", result["reason"])

    def test_optional_probe_does_not_hide_containment_or_cleanup_errors(self):
        for error in (OSError("job assignment failed"), RuntimeError("job still active"),
                      FileNotFoundError("cleanup failed"), subprocess.TimeoutExpired("reap", 10)):
            with self.subTest(error=type(error).__name__), \
                    patch.object(bench, "command", side_effect=error):
                with self.assertRaises(type(error)):
                    bench.tool_version(sys.executable)

    def test_optional_cleanup_failure_after_child_exit_is_fatal(self):
        stop = bench.OwnedProcess.stop

        def failed_stop(owned):
            stop(owned)
            raise subprocess.TimeoutExpired("owned cleanup verification", 10)

        with patch.object(bench.OwnedProcess, "stop", failed_stop):
            with self.assertRaises(subprocess.TimeoutExpired):
                bench.tool_version(sys.executable)

    def test_safety_limits_and_output_reuse(self):
        with tempfile.TemporaryDirectory(prefix="tgrep-bench-test-") as directory:
            output = Path(directory) / "result.json"
            base = ["--binary", sys.executable, "--binary-commit", "a" * 40,
                    "--output", str(output)]
            for extra in (["--worktrees", "33"], ["--files", "1000000"],
                          ["--scenarios", "unknown"], ["--conditions", "restart"],
                          ["--threads", "64"], ["--worktrees", "1,1"],
                          ["--timeout", "nan"]):
                with self.subTest(extra=extra), contextlib.redirect_stderr(io.StringIO()):
                    with self.assertRaises(SystemExit):
                        bench.parse_args(base + extra)
            output.write_text("existing")
            with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                bench.parse_args(base)

    def test_report_requires_every_workload_parameter(self):
        for key in ("scenarios", "worktrees", "conditions", "samples_per_view", "churn_rounds",
                    "churn_interval", "idle_seconds", "files", "file_bytes", "seed", "threads",
                    "timeout", "binary_commit"):
            report = self.report()
            del report["parameters"][key]
            with self.subTest(missing=key), self.assertRaisesRegex(ValueError, key):
                bench.validate(report)

    def test_parser_and_report_share_parameter_bounds(self):
        invalid = {
            "worktrees": [[], [0], [33], [1, 1]],
            "scenarios": [[], ["unknown"], ["lf", "lf"]],
            "conditions": [["restart"], ["fresh", "fresh"]],
            "files": [7, 4097, 131072],
            "file_bytes": [255, 32 * 1024 * 1024],
            "threads": [0, 9],
            "samples_per_view": [0, 1001],
            "churn_rounds": [0, 101],
            "churn_interval": [-.1, 10.1, float("nan"), float("inf")],
            "idle_seconds": [-1, 601, float("nan"), float("inf")],
            "timeout": [0, 601, float("nan"), float("inf")],
            "binary_commit": ["", "A" * 40],
        }
        with tempfile.TemporaryDirectory(prefix="tgrep-bench-test-") as directory:
            base = ["--binary", sys.executable, "--binary-commit", "a" * 40,
                    "--output", str(Path(directory) / "result.json")]
            for key, values in invalid.items():
                for value in values:
                    with self.subTest(key=key, value=value):
                        report = self.report()
                        report["parameters"][key] = value
                        extra = []
                        if key == "files":
                            report["parameters"]["file_bytes"] = 256
                            extra = ["--file-bytes", "256"]
                        with self.assertRaises(ValueError):
                            bench.validate(report)
                        argument = ",".join(map(str, value)) if isinstance(value, list) else str(value)
                        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                            bench.parse_args(base + ["--" + key.replace("_", "-"), argument] + extra)
            args = bench.parse_args(base + ["--files", "4096", "--file-bytes", "8192",
                                           "--worktrees", "1,4,16,32", "--threads", "8",
                                           "--churn-interval", "10", "--idle-seconds", "600",
                                           "--timeout", "600", "--samples-per-view", "1000",
                                           "--churn-rounds", "100"])
            bench.validate_parameters(vars(args))
        for key, value in (("files", True), ("threads", 1.5), ("seed", False),
                           ("churn_interval", "0.1"), ("worktrees", [True])):
            report = self.report()
            report["parameters"][key] = value
            with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                bench.validate(report)

    def test_fixture_reserve_accounts_for_allocation_and_metadata(self):
        for files, size, count, unit in ((131072, 256, 32, 4096), (4096, 8192, 32, 4096),
                                         (4096, 256, 32, 65536), (8, 256, 1, 512)):
            block = max(4096, unit)
            payload = ((2 * size + block - 1) // block) * block
            reserve = bench.fixture_disk_reserve(files, size, count, unit)
            with self.subTest(files=files, size=size, unit=unit):
                self.assertEqual(reserve, (files + 8) * (payload + block) * (count + 1) * 4
                                 + 256 * 1024 * 1024)
                self.assertGreater(reserve, files * ((size + block - 1) // block) * block * (count + 1))
        for unit in (0, -1, True, 4096.0):
            with self.subTest(unit=unit), self.assertRaises(ValueError):
                bench.fixture_disk_reserve(8, 256, 1, unit)

    def test_disk_guard_rejects_before_git_or_file_population(self):
        args = bench.argparse.Namespace(**self.report()["parameters"])
        args.files, args.file_bytes = 4096, 256
        reserve = bench.fixture_disk_reserve(args.files, args.file_bytes, 32, 4096)
        with tempfile.TemporaryDirectory(prefix="tgrep-bench-test-") as directory:
            fixture = bench.Fixture(directory, args, 32, "lf")
            try:
                with patch.object(bench, "allocation_unit", return_value=4096), \
                        patch.object(bench.shutil, "disk_usage") as usage, \
                        patch.object(fixture, "git") as git:
                    usage.return_value.free = reserve - 1
                    git.side_effect = AssertionError("population started before disk guard")
                    with self.assertRaisesRegex(RuntimeError, "Insufficient free disk"):
                        fixture.populate(32)
                    git.assert_not_called()
                    self.assertFalse((fixture.repo / "src").exists())
                    self.assertEqual(fixture.trees, [])
                    usage.return_value.free = reserve
                    git.side_effect = RuntimeError("guard passed")
                    with self.assertRaisesRegex(RuntimeError, "guard passed"):
                        fixture.populate(32)
                    git.assert_called_once()
                with patch.object(bench, "allocation_unit", side_effect=OSError("geometry failed")), \
                        patch.object(fixture, "git") as git:
                    with self.assertRaisesRegex(OSError, "geometry failed"):
                        fixture.populate(32)
                    git.assert_not_called()
            finally:
                bench.remove_fixture(fixture.root)

    def test_allocation_unit_uses_fixture_filesystem(self):
        with tempfile.TemporaryDirectory(prefix="tgrep-bench-test-") as directory:
            unit = bench.allocation_unit(Path(directory))
            self.assertIs(type(unit), int)
            self.assertGreater(unit, 0)

    def test_nearest_rank_and_empty(self):
        self.assertEqual(bench.distribution(list(range(1, 21))),
                         {"count": 20, "p50_ms": 10, "p95_ms": 19,
                          "max_ms": 20, "reason": None})
        self.assertEqual(bench.distribution([])["reason"], "no samples")

    def test_missing_metric_is_not_zero(self):
        for value, reason in ((None, None), (0, "unknown"), (None, ""), (-1, None),
                              (float("nan"), None), (True, None)):
            with self.assertRaises(ValueError):
                bench.metric(value, reason)
        snapshots = [{k: bench.metric(1) for k in bench.RESOURCE_KEYS} for _ in range(2)]
        self.assertEqual(bench.aggregate(snapshots)["cpu_seconds"]["value"], 2)
        snapshots[0]["pss_bytes"] = bench.metric(reason="unsupported")
        self.assertEqual(bench.aggregate(snapshots)["pss_bytes"],
                         bench.metric(reason="unsupported"))
        delta = bench.resource_delta(snapshots[0], snapshots[1])
        self.assertEqual(delta["cpu_seconds"], bench.metric(0))

    def test_match_canonicalization_preserves_offsets_spans_context(self):
        row = {"type": "match", "data": {
            "path": {"text": ".\\src\\a.txt"}, "line_number": 3, "absolute_offset": 12,
            "lines": {"text": "hello\r\n"},
            "submatches": [{"start": 0, "end": 5, "match": {"text": "hello"}}]}}
        context = {"type": "context", "data": {
            "path": {"text": "src/a.txt"}, "line_number": 2, "absolute_offset": 5,
            "lines": {"text": "world\r\n"}, "submatches": []}}
        data = (json.dumps(row) + "\n" + json.dumps(context) +
                '\n{"type":"summary","data":{"elapsed":99}}\n').encode()
        result = bench.canonical_matches(data)
        self.assertEqual(len(result), 2)
        match = next(r for r in result if r["type"] == "match")
        self.assertEqual(match["data"]["path"]["text"], "src/a.txt")
        self.assertEqual(match["data"]["absolute_offset"], 12)
        self.assertEqual(match["data"]["lines"], {"text": "hello\r\n"})
        self.assertEqual(match["data"]["submatches"], row["data"]["submatches"])

    def test_output_validation(self):
        report = self.report()
        bench.validate(report)
        for key in ("ok", "error", "finished_utc"):
            del report[key]
        bench.validate(report, finalized=False)
        with self.assertRaisesRegex(ValueError, "ok"):
            bench.validate(report)

    @staticmethod
    def report():
        # The smallest committed raw artifact is a complete, real v1 schema fixture.
        path = Path(__file__).parent / "benchmark-results" / "2026-10-06-linux-scale-crlf.json.gz"
        with gzip.open(path, "rt", encoding="utf-8") as stream:
            return json.load(stream)

    def test_output_validation_requires_each_mode_section(self):
        original = self.report()
        for index in range(2):
            for key in original["cases"][0]["modes"][index]:
                with self.subTest(mode=index, missing=key):
                    report = copy.deepcopy(original)
                    del report["cases"][0]["modes"][index][key]
                    with self.assertRaises(ValueError):
                        bench.validate(report)

    def test_output_validation_rejects_inconsistent_evidence(self):
        mutations = [
            (("resources_final", "aggregate"), {}),
            (("resources_final", "aggregate", "cpu_seconds", "value"), 999),
            (("resources_final", "processes"), []),
            (("queries", "resource_delta", "cpu_seconds", "value"), 999),
            (("queries", "samples", 0, "ms"), -1),
            (("queries", "samples", 0, "view"), 999),
            (("queries", "samples", 0, "query"), "wrong"),
            (("queries", "samples"), []),
            (("startup", "total_ready_ms"), 0),
            (("startup", "per_view"), []),
            (("startup", "statuses"), []),
            (("startup", "listener_ms"), {"value": None, "reason": None}),
            (("storage_final", "total", "files"), "1"),
            (("storage_ready", "total", "logical_bytes"), -1),
            (("status_final",), []),
            (("churn", "reason"), None),
            (("idle", "reason"), ""),
            (("cleanup", "processes"), []),
            (("cleanup", "processes", 0, "exited"), False),
            (("cleanup", "processes", 0, "pid"), 0),
            (("cleanup", "processes", 0, "returncode"), None),
            (("cleanup", "stop_errors"), ["still running"]),
            (("cleanup", "logs"), []),
            (("backend_gate", "files_checked"), False),
        ]
        for path, value in mutations:
            with self.subTest(path=path):
                report = self.report()
                target = report["cases"][0]["modes"][0]
                for key in path[:-1]:
                    target = target[key]
                target[path[-1]] = value
                with self.assertRaises(ValueError):
                    bench.validate(report)
        for key in ("attach_build", "ready"):
            report = self.report()
            shared = next(m for m in report["cases"][0]["modes"] if m["mode"] == "shared")
            del shared["startup"]["attachments"][0][key]
            with self.subTest(missing=key), self.assertRaises(ValueError):
                bench.validate(report)
        report = self.report()
        report["cleanup"]["fixtures"][0]["removed"] = False
        with self.assertRaisesRegex(ValueError, "cleanup"):
            bench.validate(report)
        report = self.report()
        report["parameters"]["worktrees"].append(16)
        with self.assertRaisesRegex(ValueError, "matrix"):
            bench.validate(report)

    def test_output_validation_requires_active_churn_and_idle_evidence(self):
        for scenario in ("churn", "lf"):
            report = self.report()
            report["parameters"].update(scenarios=[scenario], churn_rounds=1, idle_seconds=1)
            case = report["cases"][0]
            case["scenario"] = scenario
            for mode in case["modes"]:
                phase = {"reason": None, "samples": [],
                         "resources_before": copy.deepcopy(mode["resources_final"]),
                         "resources_after": copy.deepcopy(mode["resources_final"]),
                         "resource_delta": bench.resource_delta(
                             mode["resources_final"]["aggregate"], mode["resources_final"]["aggregate"])}
                if scenario == "churn":
                    for view in range(case["worktrees"]):
                        sample = copy.deepcopy(mode["queries"]["samples"][view])
                        sample.update(round=0, equal=True, polls=1, diagnostics=[])
                        phase["samples"].append(sample)
                    phase["latency"] = bench.distribution([s["ms"] for s in phase["samples"]])
                    phase["statuses"] = [copy.deepcopy(mode["status_final"])]
                    mode["churn"] = phase
                    mode["equality_after_churn"] = copy.deepcopy(mode["equality"])
                else:
                    keys = (("ready", "reconcile_running", "reconcile_attempts", "total_reads",
                             "total_extractions", "last_success") if mode["mode"] == "shared" else
                            ("reconcile_running", "last_reconcile_at",
                             "last_reconcile_duration_ms", "reconcile_overdue"))
                    views = [{k: s[k] for k in keys} for s in mode["status_final"]]
                    phase["samples"] = [{"elapsed_seconds": t, "views": copy.deepcopy(views)}
                                        for t in (0, 1)]
                    phase["changed_statuses"] = [
                        {"elapsed_seconds": 0, "view": i, "status": copy.deepcopy(s)}
                        for i, s in enumerate(mode["status_final"])]
                    if mode["mode"] == "shared":
                        # A real idle full pass can temporarily make the view unready.
                        observation = phase["samples"][-1]["views"][0]
                        observation.update(ready=False, reconcile_running=True)
                        observation["reconcile_attempts"] += 1
                        phase["changed_statuses"].append(
                            {"elapsed_seconds": 1, "view": 0,
                             "status": {**mode["status_final"][0], **observation}})
                    mode["idle"] = phase
            bench.validate(report)
            name = "churn" if scenario == "churn" else "idle"
            for key in case["modes"][0][name]:
                with self.subTest(phase=name, missing=key):
                    broken = copy.deepcopy(report)
                    del broken["cases"][0]["modes"][0][name][key]
                    with self.assertRaises(ValueError):
                        bench.validate(broken)

    def test_offset_only_difference_rejects_parity(self):
        fixture = type("Fixture", (), {"trees": [Path(".")]})()
        with patch.object(bench, "query", side_effect=[
                ([{"absolute_offset": 0}], "1 matches (1 matched lines) in 1.0ms (via server)", 1),
                ([{"absolute_offset": 256}],
                 "Brute-force search completed in 1.0ms (1 file): 1 matches (1 matched lines)", 1)]):
            with self.assertRaisesRegex(RuntimeError, "Parity failure"):
                bench.equality(fixture, "ordinary")

    def test_identical_output_wrong_backend_is_rejected(self):
        fixture = type("Fixture", (), {"trees": [Path(".")]})()
        shared = "1 matches (1 matched lines) in 1.0ms (via shared daemon v1)"
        scan = "Brute-force search completed in 1.0ms (1 file): 1 matches (1 matched lines)"
        for diagnostic in ("", scan, shared, "warning: fallback\n" + shared):
            with patch.object(bench, "query", return_value=([], diagnostic, 1)):
                with self.assertRaisesRegex(RuntimeError, "Expected ordinary backend"):
                    bench.equality(fixture, "ordinary")
        bench.backend_proof(scan, "scan")
        bench.backend_proof(shared, "shared")
        with self.assertRaisesRegex(RuntimeError, "Expected scan backend"):
            bench.backend_proof(shared, "scan")
        with self.assertRaises(RuntimeError):
            bench.backend_proof("Filename search completed (via local index)", "ordinary", files=True)


class ProcessTests(unittest.TestCase):
    @unittest.skipUnless(os.name == "nt", "Windows suspended process containment")
    def test_windows_assignment_resume_failures_never_execute_child(self):
        original_popen = bench.subprocess.Popen
        original_assign = bench.WindowsJob.assign_and_resume
        children = []

        def record(*args, **kwargs):
            child = original_popen(*args, **kwargs)
            children.append(child)
            return child

        with tempfile.TemporaryDirectory(prefix="tgrep-bench-test-") as directory:
            marker = Path(directory) / "must-not-run"
            for failure in ("assign", "resume"):
                def fail(job, process):
                    time.sleep(.05)
                    self.assertFalse(marker.exists())
                    if failure == "assign":
                        raise OSError("injected assignment failure")
                    with patch.object(bench.kernel, "ResumeThread", return_value=0xFFFFFFFF):
                        original_assign(job, process)

                with self.subTest(failure=failure), \
                        patch.object(bench.subprocess, "Popen", side_effect=record), \
                        patch.object(bench.WindowsJob, "assign_and_resume", fail):
                    with self.assertRaises(OSError):
                        bench.OwnedProcess([sys.executable, "-c",
                                            f"from pathlib import Path; Path({str(marker)!r}).touch()"])
                self.assertIsNotNone(children[-1].poll())
                self.assertFalse(marker.exists())

    def test_cleanup_reaps_descendant_after_parent_exit(self):
        with tempfile.TemporaryDirectory(prefix="tgrep-bench-test-") as directory:
            root = Path(directory)
            marker, release = root / "pid", root / "release"
            script = (
                "import subprocess,sys,time; from pathlib import Path; "
                "p=subprocess.Popen([sys.executable,'-c','import time; time.sleep(30)'], "
                "stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL); "
                "Path(sys.argv[1]).write_text(str(p.pid)); "
                "\nwhile not Path(sys.argv[2]).exists(): time.sleep(.01)\n")
            owned = bench.OwnedProcess([sys.executable, "-c", script, str(marker), str(release)],
                                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            try:
                deadline = time.monotonic() + 5
                while not marker.exists():
                    if time.monotonic() >= deadline:
                        self.fail("child PID handshake timed out")
                    time.sleep(.01)
                release.touch()
                owned.process.wait(timeout=5)
            finally:
                owned.stop()
            self.assert_dead(int(marker.read_text()))

    def assert_dead(self, pid):
        if os.name == "nt":
            bench.kernel.OpenProcess.argtypes = [bench.wt.DWORD, bench.wt.BOOL, bench.wt.DWORD]
            bench.kernel.OpenProcess.restype = bench.wt.HANDLE
            handle = bench.kernel.OpenProcess(0x100000, False, pid)
            if handle:
                try:
                    bench.kernel.WaitForSingleObject.argtypes = [bench.wt.HANDLE, bench.wt.DWORD]
                    self.assertEqual(bench.kernel.WaitForSingleObject(handle, 3000), 0)
                finally:
                    bench.kernel.CloseHandle(handle)
        else:
            deadline = time.monotonic() + 3
            while True:
                status = Path(f"/proc/{pid}/stat")
                try:
                    state = status.read_text().rsplit(")", 1)[1].split()[0]
                except FileNotFoundError:
                    return
                if state == "Z":
                    return
                self.assertLess(time.monotonic(), deadline, "owned descendant survived")
                time.sleep(.01)

    def test_malformed_detach_still_reaps_all_servers(self):
        for error in (json.JSONDecodeError("injected", "", 0), KeyError("detached")):
            with self.subTest(error=type(error).__name__), \
                    tempfile.TemporaryDirectory(prefix="tgrep-bench-test-") as directory:
                fixture = type("Fixture", (), {"root": Path(directory)})()
                servers = bench.Servers(fixture, "shared")
                servers.attachments.append((Path(directory), "lease"))
                owned = bench.OwnedProcess([sys.executable, "-c", "import time; time.sleep(30)"])
                servers.owned.append(owned)
                with patch.object(servers, "lifecycle", side_effect=error):
                    servers.close()
                self.assertFalse(servers.cleanup["ok"])
                self.assertIn(type(error).__name__, servers.cleanup["detach_errors"][0])
                self.assertIsNotNone(owned.process.poll())

    def test_readonly_fixture_cleanup(self):
        root = Path(tempfile.mkdtemp(prefix="tgrep-bench-test-"))
        path = root / "object"
        path.write_bytes(b"read only")
        path.chmod(0o400)
        bench.remove_fixture(root)
        self.assertFalse(root.exists())

    def test_failed_start_still_cleans_owned_server(self):
        with tempfile.TemporaryDirectory(prefix="tgrep-bench-test-") as directory:
            fixture = type("Fixture", (), {"root": Path(directory)})()
            servers = bench.Servers(fixture, "ordinary")
            owned = bench.OwnedProcess([sys.executable, "-c", "import time; time.sleep(30)"])
            servers.owned.append(owned)
            with patch.object(bench, "Servers", return_value=servers), \
                    patch.object(servers, "start", side_effect=RuntimeError("injected")), \
                    patch.object(servers, "sizes", return_value={}):
                result = {}
                with self.assertRaisesRegex(RuntimeError, "injected"):
                    bench.run_mode(fixture, "ordinary", "fresh", result, 0)
            self.assertTrue(result["cleanup"]["ok"])
            self.assertIsNotNone(owned.process.poll())

    def test_owned_process_cleanup_and_metrics(self):
        owned = bench.OwnedProcess([sys.executable, "-c", "import time; time.sleep(30)"])
        try:
            resources = bench.process_resources(owned.process)
            self.assertEqual(set(resources), set(bench.RESOURCE_KEYS))
            for item in resources.values():
                bench.metric(item["value"], item["reason"])
        finally:
            owned.stop()
        self.assertIsNotNone(owned.process.poll())

    def test_command_timeout_kills_descendant(self):
        with tempfile.TemporaryDirectory(prefix="tgrep-bench-test-") as directory:
            marker = Path(directory) / "pid"
            script = ("import subprocess,sys,time; "
                      "p=subprocess.Popen([sys.executable,'-c','import time; time.sleep(30)']); "
                      "open(sys.argv[1],'w').write(str(p.pid)); time.sleep(30)")
            with self.assertRaisesRegex(bench.CommandError, "timed out"):
                bench.command([sys.executable, "-c", script, str(marker)], timeout=1)
            self.assert_dead(int(marker.read_text()))


if __name__ == "__main__":
    unittest.main()
