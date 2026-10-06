"""Run: python -B -m unittest discover -s scripts/qualification -p 'test_*.py' -v"""

import copy
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

import qualify


class QualificationTests(unittest.TestCase):
    def test_dependency_graph_requires_single_exact_path_dependency(self):
        checkout = Path(__file__).resolve().parents[2]
        ignore = {
            "name": "ignore", "version": "0.4.25", "source": None, "id": "vendored",
            "manifest_path": str(checkout / "vendor/ignore/Cargo.toml"),
        }
        metadata = {
            "packages": [ignore, {"name": "tgrep-core", "id": "core"}],
            "resolve": {"nodes": [{"id": "core", "deps": [{"name": "ignore", "pkg": "vendored"}]}]},
        }
        qualify.check_graph(metadata, checkout)
        for field, value in (("version", "0.4.26"), ("source", "registry"),
                             ("manifest_path", str(checkout / "other/Cargo.toml"))):
            with self.subTest(field=field):
                broken = copy.deepcopy(metadata)
                broken["packages"][0][field] = value
                with self.assertRaisesRegex(RuntimeError, "locked 0.4.25 path dependency"):
                    qualify.check_graph(broken, checkout)
        broken = copy.deepcopy(metadata)
        broken["packages"].append(dict(ignore, id="registry", source="registry"))
        with self.assertRaisesRegex(RuntimeError, "only one vendored ignore"):
            qualify.check_graph(broken, checkout)
        broken = copy.deepcopy(metadata)
        broken["resolve"]["nodes"][0]["deps"][0]["pkg"] = "other"
        with self.assertRaisesRegex(RuntimeError, "directly resolve"):
            qualify.check_graph(broken, checkout)

    def test_checks_both_graphs_and_every_fuzz_target_with_locked_commands(self):
        with patch.object(qualify, "run") as run, patch.object(qualify, "check_graph") as graph:
            run.return_value.stdout = "{}"
            qualify.dependencies(Path("."))
        self.assertEqual(graph.call_count, 2)
        commands = [call.args[0] for call in run.call_args_list]
        self.assertEqual(len(commands), 3)
        self.assertTrue(all("--locked" in command for command in commands))
        self.assertEqual(commands[-1][-8:], [
            "--bin", "fuzz_trigram", "--bin", "fuzz_query",
            "--bin", "fuzz_ondisk", "--bin", "fuzz_reader",
        ])

    def test_lock_mutation_detected_even_after_command_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in qualify.LOCKFILES:
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"locked\n")
            with qualify.unchanged_locks(root):
                pass
            with self.assertRaisesRegex(RuntimeError, "changed lockfiles"):
                with qualify.unchanged_locks(root):
                    (root / "fuzz/Cargo.lock").write_bytes(b"changed\n")
                    raise ValueError("command failed")

    def test_same_results_from_wrong_backend_are_not_success(self):
        indexed = subprocess.CompletedProcess([], 0, "hit\n", "via local index")
        scanned = subprocess.CompletedProcess([], 0, "hit\n", "")
        with self.assertRaisesRegex(RuntimeError, "expected backend"):
            qualify.assert_parity(indexed, scanned, "via shared daemon v1")
        indexed.stderr = "via shared daemon v1"
        qualify.assert_parity(indexed, scanned, "via shared daemon v1")
        scanned.stdout = "other\n"
        with self.assertRaisesRegex(RuntimeError, "output mismatch"):
            qualify.assert_parity(indexed, scanned, "via shared daemon v1")
        scanned.returncode = 2
        with self.assertRaisesRegex(RuntimeError, "exit-code mismatch"):
            qualify.assert_parity(indexed, scanned, "via shared daemon v1")

    def test_command_failure_is_not_suppressed(self):
        with self.assertRaisesRegex(RuntimeError, "expected exit 0, got 2"):
            qualify.run([sys.executable, "-c", "raise SystemExit(2)"], cwd=Path.cwd())

    def test_service_is_reaped_after_exception_and_deadline(self):
        for fail in ("exception", "deadline"):
            with self.subTest(fail=fail):
                started = time.monotonic()
                with self.assertRaisesRegex(RuntimeError, "injected|timed out waiting"):
                    with qualify.process([sys.executable, "-c", "import time; time.sleep(60)"]) as child:
                        if fail == "exception":
                            raise RuntimeError("injected failure")
                        qualify.wait_for(child, lambda _: False, "test readiness", timeout=0.1)
                self.assertIsNotNone(child.poll())
                self.assertLess(time.monotonic() - started, 15)

    def test_dead_server_fails_before_readiness_deadline(self):
        with qualify.process([sys.executable, "-c", "raise SystemExit(7)"]) as child:
            child.wait(timeout=10)
            with self.assertRaisesRegex(RuntimeError, "server exited"):
                qualify.wait_for(child, lambda _: False, "test readiness")

    def test_cleanup_stops_descendants_not_only_the_direct_child(self):
        with tempfile.TemporaryDirectory() as directory:
            marker = Path(directory) / "port"
            descendant = (
                "import socket, pathlib, time; "
                "s = socket.socket(); s.bind(('127.0.0.1', 0)); s.listen(); "
                f"pathlib.Path({str(marker)!r}).write_text(str(s.getsockname()[1])); "
                "time.sleep(60)"
            )
            parent = (
                "import subprocess, sys, time; "
                f"subprocess.Popen([sys.executable, '-c', {descendant!r}]); "
                "time.sleep(60)"
            )
            with qualify.process([sys.executable, "-c", parent]) as child:
                qualify.wait_for(child, lambda _: marker.exists() and bool(marker.read_text()),
                                 "descendant listener", timeout=10)
                port = int(marker.read_text())
                with socket.create_connection(("127.0.0.1", port), timeout=1):
                    pass
            deadline = time.monotonic() + 5
            while True:
                try:
                    with socket.create_connection(("127.0.0.1", port), timeout=1):
                        pass
                except OSError:
                    break
                # Group termination is asynchronous for the grandchild on Unix.
                self.assertLess(time.monotonic(), deadline, "descendant survived cleanup")
                time.sleep(0.05)

    def test_command_timeout_reaps_owned_process(self):
        children = []
        original = qualify.subprocess.Popen

        def record(*args, **kwargs):
            child = original(*args, **kwargs)
            if args[0][0] == sys.executable:
                children.append(child)
            return child

        with patch.object(qualify.subprocess, "Popen", side_effect=record):
            with self.assertRaisesRegex(RuntimeError, "command timed out"):
                qualify.run([sys.executable, "-c", "import time; time.sleep(60)"],
                            cwd=Path.cwd(), timeout=0.1)
        self.assertEqual(len(children), 1)
        self.assertIsNotNone(children[0].poll())

    def test_install_uses_private_root_and_exact_installed_executable(self):
        scratch = None

        def installed(binary, directory):
            nonlocal scratch
            scratch = directory
            expected = directory / "install/bin" / ("tgrep.exe" if os.name == "nt" else "tgrep")
            self.assertEqual(binary, expected)
            self.assertTrue(directory.is_dir())
            raise RuntimeError("injected smoke failure")

        with patch.object(qualify, "run") as run, patch.object(qualify, "smoke", side_effect=installed):
            run.return_value.stderr = ""
            with self.assertRaisesRegex(RuntimeError, "injected smoke failure"):
                qualify.installed(Path.cwd())
        command = run.call_args.args[0]
        self.assertEqual(command[:3], ["cargo", "install", "--path"])
        self.assertIn("--locked", command)
        self.assertEqual(command[-2:], ["--root", scratch / "install"])
        self.assertFalse(scratch.exists())

    def test_fixture_git_environment_is_private(self):
        with patch.dict(os.environ, {"GIT_DIR": "foreign", "GIT_CONFIG_COUNT": "9"}):
            env = qualify.fixture_environment(Path("private-home"))
        self.assertNotIn("GIT_DIR", env)
        self.assertNotIn("GIT_CONFIG_COUNT", env)
        self.assertEqual(env["GIT_CONFIG_NOSYSTEM"], "1")
        self.assertEqual(env["USERPROFILE"], "private-home")
        self.assertEqual(env["HOME"], "private-home")

    def test_registration_must_belong_to_owned_server(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "marker.json"
            with qualify.process([sys.executable, "-c", "import time; time.sleep(60)"]) as child:
                self.assertFalse(qualify.marker_owned(path, child))
                path.write_text(json.dumps({"pid": child.pid + 1}), encoding="utf-8")
                self.assertFalse(qualify.marker_owned(path, child))
                path.write_text(json.dumps({"pid": child.pid}), encoding="utf-8")
                self.assertTrue(qualify.marker_owned(path, child))


if __name__ == "__main__":
    unittest.main()
