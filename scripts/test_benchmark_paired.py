"""Stdlib-only unit tests; no real tgrep servers or benchmark corpus required."""

import contextlib
import hashlib
import importlib.util
import io
import itertools
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock


SPEC = importlib.util.spec_from_file_location(
    "benchmark_paired", Path(__file__).with_name("benchmark-paired.py")
)
bench = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bench)


def ready_status(**overrides):
    return {
        "last_reconcile_at": 123, "last_reconcile_error": None,
        "reconcile_running": False, "indexing": False, "hidden_complete": True,
        "watcher_active": False, "watch_mode_requested": "disabled",
        "watch_mode_active": "disabled", **overrides,
    }


def arguments(**overrides):
    values = {
        "baseline": Path("baseline"), "candidate": Path("candidate"),
        "baseline_sha": "a" * 40, "candidate_sha": "b" * 40, "corpus_sha": "c" * 40,
        "repo_path": Path("corpus"), "queries": Path("queries.json"), "work": Path("new-work"),
        "repeats": 2, "blocks": "abba-baab", "query_timeout": 120.0, "startup_timeout": 900.0,
    }
    values.update(overrides)
    return bench.argparse.Namespace(**values)


def search_trace(pattern):
    return (
        f"[trace] search: pattern={json.dumps(pattern)} case_insensitive=false "
        "raw_candidates=3 candidates=2 matches=1 elapsed=2.0ms "
        "(index=0.5ms resolve=0.5ms search=1.0ms)\n"
    )


class ArgumentsTests(unittest.TestCase):
    def parser_args(self):
        args = arguments()
        names = ("baseline", "candidate", "baseline_sha", "candidate_sha", "repo_path",
                 "corpus_sha", "queries", "work")
        return list(itertools.chain.from_iterable(
            ("--" + name.replace("_", "-"), str(getattr(args, name))) for name in names
        ))

    def test_defaults_and_required_fields(self):
        result = bench.argument_parser().parse_args(self.parser_args())
        self.assertEqual(result.repeats, 5)
        self.assertEqual(result.blocks, "abba-baab")
        self.assertEqual(result.query_timeout, 120)
        self.assertEqual(result.startup_timeout, 900)
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            bench.argument_parser().parse_args([])

    def test_positive_options_enforced(self):
        for flag, values in (
            ("--repeats", ("0", "-1", "1.5", "nan")),
            ("--query-timeout", ("0", "-1", "nan", "inf", "-inf")),
            ("--startup-timeout", ("0", "-1", "nan", "inf")),
        ):
            for value in values:
                with self.subTest(flag=flag, value=value):
                    with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                        bench.argument_parser().parse_args(self.parser_args() + [f"{flag}={value}"])

    def test_sha_validation(self):
        self.assertEqual(bench.commit_sha("A" * 40), "a" * 40)
        for value in ("a" * 39, "a" * 41, "g" * 40, ""):
            with self.assertRaises(bench.argparse.ArgumentTypeError):
                bench.commit_sha(value)

    def test_work_must_be_new_and_outside_corpus(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            corpus = root / "corpus"
            corpus.mkdir()
            for path in (corpus, corpus / "work", root):
                with self.subTest(path=path), self.assertRaises(bench.BenchmarkError):
                    bench.create_work_directory(path, corpus)
            work, actual = bench.create_work_directory(root / "work", corpus)
            self.assertEqual(actual, corpus)
            self.assertTrue((work / "tmp").is_dir())
            with self.assertRaises(bench.BenchmarkError):
                bench.create_work_directory(work, corpus)


class FingerprintTests(unittest.TestCase):
    def fingerprint(self, root, content):
        output = root / "output"
        output.write_bytes(content)
        return bench.fingerprint_output(output, root, {"kind": "python"}, 10)

    def test_order_independent_and_duplicate_sensitive(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            first = self.fingerprint(root, b"beta\nalpha\nalpha\n")
            self.assertEqual(first, self.fingerprint(root, b"alpha\nbeta\nalpha"))
            self.assertNotEqual(first, self.fingerprint(root, b"alpha\nbeta\n"))
            self.assertEqual(first["lines"], 3)
            self.assertEqual(first["sha256"], hashlib.sha256(b"alpha\nalpha\nbeta\n").hexdigest())

    def test_empty_and_raw_bytes_are_not_text_normalized(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.assertEqual(self.fingerprint(root, b""),
                             {"lines": 0, "sha256": hashlib.sha256(b"").hexdigest()})
            data = b"\xff\na\r\nx\0z\n\v\n\n"
            canonical = b"\n\v\na\r\nx\0z\n\xff\n"
            result = self.fingerprint(root, data)
            self.assertEqual(result["sha256"], hashlib.sha256(canonical).hexdigest())
            self.assertEqual(result["lines"], 5)

    def test_windows_fallback_is_bounded(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with mock.patch.object(bench, "SMOKE_MAX_BYTES", 2), self.assertRaises(bench.BenchmarkError):
                self.fingerprint(root, b"long\n")
            with mock.patch.object(bench, "SMOKE_MAX_LINES", 1), self.assertRaises(bench.BenchmarkError):
                self.fingerprint(root, b"a\nb\n")

    def test_external_sort_configuration_and_streamed_digest(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            output = root / "output"
            output.write_bytes(b"b\na\na")

            def sort(command, **kwargs):
                self.assertIn("--buffer-size=128M", command)
                self.assertIn("--parallel=1", command)
                self.assertEqual(kwargs["env"]["LC_ALL"], "C")
                self.assertEqual(kwargs["stdout"], subprocess.DEVNULL)
                scratch = Path(command[command.index("--temporary-directory") + 1])
                self.assertEqual(scratch.parent, root)
                Path(command[command.index("--output") + 1]).write_bytes(b"a\na\nb\n")
                return subprocess.CompletedProcess(command, 0, None, b"")

            with mock.patch.object(bench.subprocess, "run", side_effect=sort):
                result = bench.fingerprint_output(output, root, {"kind": "gnu", "executable": "sort"}, 10)
            self.assertEqual(result, self.fingerprint(root, b"a\nb\na"))
            self.assertEqual(list(root.iterdir()), [output])

    def test_external_sort_errors_and_timeouts_fail(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            output = root / "output"
            output.write_bytes(b"a\n")
            for result in (
                subprocess.CompletedProcess([], 2, None, b"sort error"),
                subprocess.CompletedProcess([], 0, None, b"sort warning"),
                subprocess.TimeoutExpired(["sort"], 1, stderr=b"timeout detail"),
            ):
                with self.subTest(result=result):
                    kwargs = {"side_effect": result} if isinstance(result, Exception) else {"return_value": result}
                    with mock.patch.object(bench.subprocess, "run", **kwargs), self.assertRaises(bench.BenchmarkError):
                        bench.fingerprint_output(output, root, {"kind": "gnu", "executable": "sort"}, 1)
            self.assertEqual(list(root.iterdir()), [output])


class QueryTests(unittest.TestCase):
    def test_result_validation_is_strict(self):
        for code in (0, 1):
            bench.validate_query_result(code, b"", code)
        for code, stderr, expected in ((2, b"", None), (-9, b"", None), (0, b" ", None),
                                       (1, b"warning", 1), (0, b"", 1), (1, b"", 0)):
            with self.subTest(code=code, stderr=stderr), self.assertRaises(bench.BenchmarkError):
                bench.validate_query_result(code, stderr, expected)

    def test_parity_checks_hash_lines_and_exit(self):
        reference = {"sha256": "abc", "lines": 2, "exit_code": 0}
        bench.require_parity(reference, dict(reference), "case")
        for key, value in (("sha256", "def"), ("lines", 1), ("exit_code", 1)):
            with self.subTest(key=key), self.assertRaisesRegex(bench.BenchmarkError, "parity mismatch"):
                bench.require_parity(reference, {**reference, key: value}, "case")

    def test_timed_query_discards_stdout_without_stats(self):
        result = subprocess.CompletedProcess([], 1, None, b"")
        with mock.patch.object(bench.subprocess, "run", return_value=result) as run:
            with mock.patch.object(bench.time, "perf_counter_ns", side_effect=[1000000, 2500000]):
                elapsed, code = bench.run_query("binary", "corpus", "index", "serve", 2, subprocess.DEVNULL, 1)
        self.assertEqual((elapsed, code), (1.5, 1))
        command = run.call_args.args[0]
        self.assertNotIn("--stats", command)
        self.assertEqual(command[-3:], ["--", "serve", "corpus"])
        self.assertEqual(run.call_args.kwargs["stdout"], subprocess.DEVNULL)
        self.assertEqual(run.call_args.kwargs["stderr"], subprocess.PIPE)
        self.assertEqual(run.call_args.kwargs["timeout"], 2)

    def test_route_probe_rejects_fallback_and_extra_warnings(self):
        good = b"Filename search completed in 1.2ms (via server)\n"
        for code, stderr, succeeds in (
            (0, good, True), (0, b"Filename search completed in 1.2ms (via local index)\n", False),
            (0, b"warning: fallback\n" + good, False), (0, b"", False), (2, good, False),
        ):
            with self.subTest(stderr=stderr, code=code):
                with mock.patch.object(bench.subprocess, "run",
                                       return_value=subprocess.CompletedProcess([], code, None, stderr)):
                    if succeeds:
                        self.assertEqual(bench.probe_route("bin", "repo", "index", 1)["stderr"], good.decode())
                    else:
                        with self.assertRaises(bench.BenchmarkError):
                            bench.probe_route("bin", "repo", "index", 1)

    def test_query_timeout_preserves_diagnostic(self):
        with mock.patch.object(
            bench.subprocess, "run",
            side_effect=subprocess.TimeoutExpired("query", 1, stderr=b"diagnostic"),
        ):
            with self.assertRaisesRegex(bench.BenchmarkError, "timed out.*diagnostic"):
                bench.run_query("binary", "corpus", "index", "pattern", 1, subprocess.DEVNULL)


class ContentServerTests(unittest.TestCase):
    def test_pattern_decoder_handles_quotes_backslashes_and_rust_debug_unicode(self):
        for pattern in ('#include "base/', "std::vector", r"literal\u{1b}", "\t\n", ""):
            with self.subTest(pattern=pattern):
                self.assertEqual(bench.trace_pattern(search_trace(pattern)), pattern)
        line = search_trace("placeholder").replace(
            '"placeholder"', r'"literal\\u{1b}\u{1b}\u{1f600}"'
        )
        self.assertEqual(bench.trace_pattern(line), "literal\\u{1b}\x1b" + chr(0x1F600))

    def test_pattern_decoder_rejects_malformed_or_wrong_mode_traces(self):
        good = search_trace("pattern")
        for line in (
            "[trace] search: missing pattern\n", good.replace("false", "true"),
            good.replace('"pattern"', r'"bad\q"'), good.replace('"pattern"', "null"),
            good.replace('"pattern"', r'"\u{110000}"'),
            good.replace('"pattern"', r'"\u{d800}"'), good.replace("search=1.0ms)", ""),
        ):
            with self.subTest(line=line), self.assertRaises(bench.BenchmarkError):
                bench.trace_pattern(line)

    def test_all_chromium_patterns_are_counted_and_logs_remain_raw(self):
        query_path = Path(__file__).with_name("benchmark-queries-chromium.json")
        patterns = json.loads(query_path.read_text(encoding="utf-8"))["queries"]
        self.assertEqual(len(patterns), 30)
        with tempfile.TemporaryDirectory() as directory:
            logfile = Path(directory) / "server.log"
            traces = [search_trace(pattern) for pattern in reversed(patterns) for _ in range(7)]
            raw = ("[trace] unrelated startup/status\n" + "".join(traces)).encode("utf-8")
            logfile.write_bytes(raw)
            check = bench.content_server_check(logfile, patterns, 5)
            bench.require_content_server_check(check)
            self.assertEqual(check["observed_count"], 210)
            self.assertEqual(check["expected_count"], 210)
            self.assertEqual(check["observed_per_pattern"], dict.fromkeys(patterns, 7))
            self.assertEqual(logfile.read_bytes(), raw)
            self.assertEqual(check["log_fingerprint"]["sha256"], hashlib.sha256(raw).hexdigest())

    def test_duplicate_input_patterns_require_all_passes(self):
        with tempfile.TemporaryDirectory() as directory:
            logfile = Path(directory) / "server.log"
            logfile.write_text(search_trace("same") * 6, encoding="utf-8")
            check = bench.content_server_check(logfile, ["same", "same"], 1)
            bench.require_content_server_check(check)
            self.assertEqual(check["expected_per_pattern"], {"same": 6})

    def test_missing_extra_wrong_pattern_and_malformed_traces_fail(self):
        for trace_list in (
            [search_trace("one")] * 2,
            [search_trace("one")] * 4,
            [search_trace("one")] * 2 + [search_trace("different")],
            [search_trace("one")] * 2 + ['[trace] search: pattern="one" broken\n'],
            [],
        ):
            with self.subTest(traces=trace_list), tempfile.TemporaryDirectory() as directory:
                logfile = Path(directory) / "server.log"
                logfile.write_text("".join(trace_list), encoding="utf-8")
                check = bench.content_server_check(logfile, ["one"], 1)
                self.assertFalse(check["match"])
                with self.assertRaisesRegex(bench.BenchmarkError, "Content-server trace mismatch"):
                    bench.require_content_server_check(check)


class ReadinessTests(unittest.TestCase):
    def test_discovery_requires_owned_pid_and_valid_port(self):
        self.assertEqual(bench.discovery_port({"pid": 42, "port": 3000}, 42), 3000)
        with self.assertRaisesRegex(bench.DiscoveryPending, "owned PID"):
            bench.discovery_port({"pid": 7, "port": 3000}, 42)
        for value in ({}, {"pid": True, "port": 3000}, {"pid": 0, "port": 3000},
                      {"pid": 42, "port": True},
                      {"pid": 42, "port": 65536}, {"pid": 42, "port": 0}):
            with self.subTest(value=value), self.assertRaises(bench.BenchmarkError):
                bench.discovery_port(value, 42)

    def test_readiness_rejects_incomplete_and_running_status(self):
        self.assertTrue(bench.status_ready(ready_status()))
        for field, value in (
            ("last_reconcile_at", None), ("hidden_complete", False), ("indexing", True),
            ("reconcile_running", True), ("flushing", True), ("reconcile_pending", True),
            ("reconcile_overdue", True),
        ):
            with self.subTest(field=field):
                self.assertFalse(bench.status_ready(ready_status(**{field: value})))

    def test_malformed_errors_and_watchers_fail_explicitly(self):
        malformed = ready_status()
        del malformed["last_reconcile_error"]
        cases = [
            malformed, ready_status(last_reconcile_error="failed"),
            ready_status(last_reconcile_error=""), ready_status(last_reconcile_at=True),
            ready_status(indexing=0), ready_status(hidden_complete="true"),
            ready_status(watcher_active=True), ready_status(watch_mode_active="native"),
        ]
        for status in cases:
            with self.subTest(status=status), self.assertRaises(bench.BenchmarkError):
                bench.status_ready(status)

    def test_rpc_socket_protocol(self):
        connection = mock.MagicMock()
        connection.makefile.return_value = io.BytesIO(
            json.dumps({"jsonrpc": "2.0", "id": 1, "result": ready_status()}).encode() + b"\n"
        )
        with mock.patch.object(bench.socket, "create_connection") as create:
            create.return_value.__enter__.return_value = connection
            self.assertEqual(bench.request_status(1234, 2), ready_status())
        create.assert_called_once_with(("127.0.0.1", 1234), timeout=2)
        connection.sendall.assert_called_once_with(b'{"jsonrpc":"2.0","method":"status","id":1}\n')

    def test_rpc_bad_envelopes_fail(self):
        for message in (
            {}, [], {"jsonrpc": "2.0", "id": 2, "result": {}},
            {"jsonrpc": "2.0", "id": True, "result": {}},
            {"jsonrpc": "2.0", "id": 1, "error": {"message": "bad"}},
            {"jsonrpc": "2.0", "id": 1, "result": []},
        ):
            with self.subTest(message=message), self.assertRaises(bench.BenchmarkError):
                bench.parse_status_response(message)

    def test_rpc_rejects_truncated_malformed_and_oversized_responses(self):
        for line in (b"", b"{}", b"{bad json}\n", b" " * 33 + b"\n"):
            with self.subTest(line=line):
                connection = mock.MagicMock()
                connection.makefile.return_value = io.BytesIO(line)
                with mock.patch.object(bench.socket, "create_connection") as create:
                    create.return_value.__enter__.return_value = connection
                    with mock.patch.object(bench, "RPC_MAX_BYTES", 32):
                        with self.assertRaises(bench.BenchmarkError):
                            bench.request_status(1234, 2)

    def test_wait_skips_stale_discovery_and_incomplete_status(self):
        process = mock.Mock(pid=42)
        process.poll.return_value = None
        with mock.patch.object(bench, "read_discovery",
                               side_effect=[bench.DiscoveryPending("stale"), 1234, 1234]):
            with mock.patch.object(bench, "request_status",
                                   side_effect=[ready_status(indexing=True), ready_status()]) as status:
                with mock.patch.object(bench.time, "monotonic", side_effect=itertools.count(0, 0.01)):
                    with mock.patch.object(bench.time, "sleep"):
                        observed, _ = bench.wait_ready(process, Path("index"), 10, "server.log")
        self.assertEqual(observed, ready_status())
        self.assertEqual(status.call_count, 2)

    def test_timeout_includes_last_observation(self):
        process = mock.Mock(pid=42)
        process.poll.return_value = None
        with mock.patch.object(bench, "read_discovery", return_value=1234):
            with mock.patch.object(bench, "request_status", return_value=ready_status(indexing=True)):
                with mock.patch.object(bench.time, "monotonic", side_effect=itertools.count()):
                    with mock.patch.object(bench.time, "sleep"):
                        with self.assertRaisesRegex(bench.BenchmarkError, "last observation:.*'indexing': True"):
                            bench.wait_ready(process, Path("index"), 10, "server.log")

    def test_persistent_connection_errors_have_short_grace(self):
        process = mock.Mock(pid=42)
        process.poll.return_value = None
        with mock.patch.object(bench, "read_discovery", return_value=1234):
            with mock.patch.object(bench, "request_status", side_effect=ConnectionRefusedError("refused")):
                with mock.patch.object(bench.time, "monotonic", side_effect=itertools.count()):
                    with mock.patch.object(bench.time, "sleep"):
                        with self.assertRaisesRegex(bench.BenchmarkError, "Persistent.*refused"):
                            bench.wait_ready(process, Path("index"), 900, "server.log")

    def test_brief_connection_error_can_recover(self):
        process = mock.Mock(pid=42)
        process.poll.return_value = None
        with mock.patch.object(bench, "read_discovery", return_value=1234):
            with mock.patch.object(bench, "request_status",
                                   side_effect=[ConnectionRefusedError("refused"), ready_status()]):
                with mock.patch.object(bench.time, "monotonic", side_effect=itertools.count(0, 0.01)):
                    with mock.patch.object(bench.time, "sleep"):
                        status, _ = bench.wait_ready(process, Path("index"), 10, "server.log")
        self.assertEqual(status, ready_status())

    def test_persistent_partial_discovery_is_not_readiness(self):
        process = mock.Mock(pid=42)
        process.poll.return_value = None
        with mock.patch.object(bench, "read_discovery",
                               side_effect=json.JSONDecodeError("partial", "{", 1)):
            with mock.patch.object(bench.time, "monotonic", side_effect=itertools.count()):
                with mock.patch.object(bench.time, "sleep"):
                    with self.assertRaisesRegex(bench.BenchmarkError, "Persistent.*Malformed discovery"):
                        bench.wait_ready(process, Path("index"), 900, "server.log")

    def test_dead_server_and_changed_reconcile_fail(self):
        process = mock.Mock(pid=42)
        process.poll.return_value = 17
        with self.assertRaisesRegex(bench.BenchmarkError, "exited 17"):
            bench.wait_ready(process, Path("index"), 10, "server.log")
        process.poll.return_value = None
        with mock.patch.object(bench, "read_discovery", return_value=1234):
            with mock.patch.object(bench, "request_status", return_value=ready_status(last_reconcile_at=124)):
                with self.assertRaisesRegex(bench.BenchmarkError, "overlapped"):
                    bench.checked_status(process, Path("index"), "server.log", ready_status(), 10)

    def test_cleanup_only_terminates_owned_process_and_escalates(self):
        process = mock.Mock(pid=42)
        process.poll.return_value = None
        process.wait.side_effect = [subprocess.TimeoutExpired("server", 15), -9]
        self.assertEqual(bench.stop_owned_process(process), {"exit_code": -9, "forced_kill": True})
        process.terminate.assert_called_once_with()
        process.kill.assert_called_once_with()
        self.assertEqual(process.wait.call_args_list, [mock.call(timeout=15), mock.call(timeout=15)])

    def test_startup_failure_still_cleans_up_and_keeps_log(self):
        with tempfile.TemporaryDirectory() as directory:
            logfile = Path(directory) / "server.log"
            process = mock.Mock(pid=42)
            process.poll.return_value = None
            process.wait.return_value = -15
            record = {}
            with mock.patch.object(bench.subprocess, "Popen", return_value=process):
                with mock.patch.object(bench, "wait_ready", side_effect=bench.BenchmarkError("not ready")):
                    with self.assertRaisesRegex(bench.BenchmarkError, "not ready"):
                        with bench.managed_server("binary", "corpus", "index", logfile, 1, record):
                            self.fail("Unready server was yielded")
            process.terminate.assert_called_once_with()
            self.assertTrue(logfile.exists())
            self.assertEqual(record["shutdown"]["exit_code"], -15)

    def test_server_death_at_block_end_is_not_a_successful_shutdown(self):
        with tempfile.TemporaryDirectory() as directory:
            logfile = Path(directory) / "server.log"
            process = mock.Mock(pid=42)
            process.poll.return_value = None
            process.wait.return_value = 17
            record = {}
            with mock.patch.object(bench.subprocess, "Popen", return_value=process):
                with mock.patch.object(bench, "wait_ready", return_value=(ready_status(), 1)):
                    with self.assertRaisesRegex(bench.BenchmarkError, "exited 17"):
                        with bench.managed_server("binary", "corpus", "index", logfile, 1, record):
                            process.poll.return_value = 17
            process.terminate.assert_not_called()
            process.wait.assert_called_once_with(timeout=15)
            self.assertEqual(record["shutdown"]["exit_code"], 17)


class ManifestAndCorpusTests(unittest.TestCase):
    def test_index_build_failures_keep_log_and_command(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            logfile = root / "index.log"
            for outcome in (
                subprocess.CompletedProcess([], 2, None, None),
                subprocess.TimeoutExpired("index", 1),
            ):
                with self.subTest(outcome=outcome):
                    record = {}

                    def build(command, **kwargs):
                        kwargs["stdout"].write(b"index diagnostic")
                        self.assertEqual(kwargs["stderr"], subprocess.STDOUT)
                        if isinstance(outcome, Exception):
                            raise outcome
                        return outcome

                    with mock.patch.object(bench.subprocess, "run", side_effect=build):
                        with self.assertRaises(bench.BenchmarkError):
                            bench.build_index("baseline", root, root / "index", logfile, 1, record)
                    self.assertEqual(logfile.read_bytes(), b"index diagnostic")
                    self.assertEqual(record["command"][0], "baseline")
                    self.assertEqual(record["log"], str(logfile))

    def test_manifest_detects_same_length_changed_bytes_and_missing_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in bench.INDEX_FILES:
                (root / name).write_bytes(b"abc")
            before = bench.index_manifest(root)
            bench.require_manifest(before, bench.index_manifest(root))
            (root / "files.bin").write_bytes(b"abd")
            with self.assertRaisesRegex(bench.BenchmarkError, "files.bin"):
                bench.require_manifest(before, bench.index_manifest(root))
            (root / "files-extra.bin").unlink()
            with self.assertRaisesRegex(bench.BenchmarkError, "files-extra.bin"):
                bench.index_manifest(root)

    def test_git_file_count_uses_nul_not_lines(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()

            def git(command, **kwargs):
                if "ls-files" in command:
                    self.assertIn("-z", command)
                    kwargs["stdout"].write(b"file\nwith-newline\0other\0")
                    return subprocess.CompletedProcess(command, 0, None, b"")
                if "--show-toplevel" in command:
                    output = str(root).encode() + b"\n"
                elif "HEAD" in command:
                    output = ("c" * 40 + "\n" + "d" * 40 + "\n").encode()
                else:
                    self.assertIn("--porcelain=v1", command)
                    output = b""
                return subprocess.CompletedProcess(command, 0, output, b"")

            with mock.patch.object(bench.subprocess, "run", side_effect=git):
                result = bench.corpus_snapshot(root, root, 10)
            self.assertEqual(result["tracked_file_count"], 2)
            bench.require_corpus(result, root, "c" * 40)
            for changed in (
                {**result, "clean": False, "status_porcelain_v1_z": " M file\0"},
                {**result, "commit": "a" * 40},
                {**result, "tree": "a" * 40},
            ):
                with self.assertRaises(bench.BenchmarkError):
                    bench.require_corpus(changed, root, "c" * 40, result)


class SummaryTests(unittest.TestCase):
    def test_distribution_and_linear_p90(self):
        self.assertEqual(bench.distribution([1, 2, 3, 4]),
                         {"n": 4, "median_ms": 2.5, "min_ms": 1, "max_ms": 4, "p90_ms": 3.7})
        self.assertEqual(bench.distribution([7])["p90_ms"], 7)
        self.assertIsNone(bench.distribution([]))

    def test_suite_median_is_not_sum_of_query_medians(self):
        samples = []
        values = ([1, 10], [10, 1], [10, 10])
        for block, label, scale in ((1, "baseline", 1), (2, "candidate", 0.5)):
            for repeat, timings in enumerate(values, 1):
                for query_id, value in enumerate(timings, 1):
                    samples.append({"block": block, "label": label, "repeat": repeat,
                                    "query_id": query_id, "ms": value * scale})
        result = bench.summarize(samples, ["one", "two"])
        suite = result["suite"]
        self.assertEqual(suite["baseline"]["median_ms"], 11)
        self.assertEqual(sum(row["baseline"]["median_ms"] for row in result["queries"]), 20)
        self.assertEqual(suite["candidate"]["median_ms"], 5.5)
        self.assertEqual(suite["candidate_over_baseline"], 0.5)
        self.assertEqual(suite["change_percent"], -50)
        self.assertEqual(suite["per_block"][0]["median_ms"], 11)
        self.assertEqual(suite["paired_block_ratios"][0]["blocks"], [1, 2])
        self.assertEqual(suite["median_paired_block_ratio"], 0.5)

    def test_pairing_handles_both_ab_and_ba(self):
        samples = [
            {"block": i, "label": label, "query_id": 1, "repeat": 1, "ms": value}
            for i, (label, value) in enumerate(zip(
                bench.BLOCK_ORDERS["abba-baab"], [10, 5, 10, 20, 30, 10, 20, 40]
            ), 1)
        ]
        suite = bench.summarize(samples, ["query"])["suite"]
        self.assertEqual([row["candidate_over_baseline"] for row in suite["paired_block_ratios"]],
                         [0.5, 0.5, 3, 2])
        self.assertEqual(suite["median_paired_block_ratio"], 1.25)

    def test_partial_suite_and_missing_label_do_not_invent_results(self):
        samples = [{"block": 1, "label": "baseline", "query_id": 1, "repeat": 1, "ms": 2}]
        summary = bench.summarize(samples, ["duplicate", "duplicate"])
        self.assertEqual(len(summary["queries"]), 2)
        self.assertIsNone(summary["queries"][0]["candidate"])
        self.assertIsNone(summary["queries"][0]["candidate_over_baseline"])
        self.assertEqual(summary["suite"]["repetitions"], [])
        self.assertIsNone(bench.effect(0, 0)["candidate_over_baseline"])
        with self.assertRaisesRegex(bench.BenchmarkError, "Duplicate"):
            bench.summarize(samples + samples, ["one"])

    def test_shuffle_is_repeatable_without_global_random_state(self):
        first = bench.shuffled_ids(30, bench.REPEAT_SEED)
        self.assertEqual(first, bench.shuffled_ids(30, bench.REPEAT_SEED))
        self.assertEqual(sorted(first), list(range(1, 31)))
        self.assertNotEqual(first, bench.shuffled_ids(30, bench.REPEAT_SEED + 1))
        self.assertEqual(bench.shuffled_ids(1, 1), [1])

    def test_markdown_patterns_are_escaped(self):
        self.assertEqual(bench.markdown_text("a|`b`<&*_\n"),
                         "a&#124;&#96;b&#96;&lt;&amp;&#42;&#95;&#10;")


class RunnerTests(unittest.TestCase):
    def run_fixture(self, root, fail_pattern=None, mutate_index=False, dirty_after=False,
                    omit_trace_pattern=None):
        corpus = root / "corpus"
        corpus.mkdir()
        binaries = {}
        for label in bench.LABELS:
            binary = root / label
            binary.write_bytes(label.encode())
            binary.chmod(0o700)
            binaries[label] = binary
        queries = root / "queries.json"
        queries.write_text(json.dumps({"queries": ["first", "second", "missing"]}), encoding="utf-8")
        args = arguments(**binaries, repo_path=corpus, queries=queries, work=root / "work")
        snapshot = {
            "path": str(corpus), "git_root": str(corpus), "commit": args.corpus_sha,
            "tree": "d" * 40, "clean": True, "status_porcelain_v1_z": "", "tracked_file_count": 3,
        }
        active = []
        calls = []
        stopped = []
        server_logs = {}
        persisted_blocks = []
        original_write = bench.write_report

        def write_report(work, report):
            original_write(work, report)
            persisted_blocks.append(sum(block["status"] == "complete" for block in report["blocks"]))

        def build(binary, corpus, index, logfile, timeout, record):
            index.mkdir()
            for name in bench.INDEX_FILES:
                (index / name).write_bytes(b"immutable")
            logfile.write_bytes(b"built")
            record["command"] = [binary, "index"]

        @contextlib.contextmanager
        def server(binary, corpus, index, logfile, timeout, record):
            self.assertEqual(active, [])
            active.append(record["block"])
            record["ready_status"] = ready_status()
            record["server_log"] = str(logfile)
            server_logs[record["block"]] = logfile
            logfile.write_bytes(b"server log\n")
            process = mock.Mock(pid=42)
            process.poll.return_value = None
            try:
                yield process
            finally:
                stopped.append(active.pop())
                record["shutdown"] = {"exit_code": 0, "forced_kill": False}
                if mutate_index:
                    (index / "files.bin").write_bytes(b"changed!!")

        def query(binary, corpus, index, pattern, timeout, stdout, expected_code=None):
            calls.append((active[0], str(index), pattern, expected_code, stdout == subprocess.DEVNULL))
            if fail_pattern == pattern:
                raise bench.BenchmarkError("simulated query failure")
            if pattern != omit_trace_pattern:
                with server_logs[active[0]].open("a", encoding="utf-8") as log:
                    log.write(search_trace(pattern))
            code = 1 if pattern == "missing" else 0
            if stdout != subprocess.DEVNULL and code == 0:
                stdout.write(pattern.encode() + b"\n")
            bench.validate_query_result(code, b"", expected_code)
            return (2 if Path(binary).name == "baseline" else 1), code

        with contextlib.ExitStack() as stack:
            final_snapshot = ({**snapshot, "clean": False, "status_porcelain_v1_z": " M changed\0"}
                              if dirty_after else snapshot)
            stack.enter_context(mock.patch.object(bench, "corpus_snapshot",
                                                  side_effect=[snapshot, final_snapshot]))
            stack.enter_context(mock.patch.object(bench, "choose_sort_backend", return_value={"kind": "python"}))
            build_mock = stack.enter_context(mock.patch.object(bench, "build_index", side_effect=build))
            stack.enter_context(mock.patch.object(bench, "managed_server", side_effect=server))
            stack.enter_context(mock.patch.object(bench, "probe_route", return_value={"stderr": "(via server)"}))
            stack.enter_context(mock.patch.object(bench, "checked_status", return_value=ready_status()))
            stack.enter_context(mock.patch.object(bench, "run_query", side_effect=query))
            stack.enter_context(mock.patch.object(bench, "host_metadata", return_value={}))
            stack.enter_context(mock.patch.object(bench, "memory_snapshot", return_value=None))
            stack.enter_context(mock.patch.object(bench, "write_report", side_effect=write_report))
            stack.enter_context(contextlib.redirect_stdout(io.StringIO()))
            stack.enter_context(contextlib.redirect_stderr(io.StringIO()))
            report = bench.run_benchmark(args)
        self.assertEqual(build_mock.call_count, 1)
        self.assertEqual(active, [])
        completed = sum(block["status"] == "complete" for block in report["blocks"])
        self.assertTrue(set(range(1, completed + 1)).issubset(persisted_blocks))
        return args, report, calls, stopped

    def test_full_mocked_run_is_paired_sequential_and_persistent(self):
        with tempfile.TemporaryDirectory() as directory:
            args, report, calls, stopped = self.run_fixture(Path(directory).resolve())
            self.assertEqual(report["status"], "complete")
            self.assertEqual(stopped, list(range(1, 9)))
            self.assertEqual(len({call[1] for call in calls}), 1)
            self.assertEqual(len(report["samples"]), 8 * args.repeats * 3)
            self.assertTrue(all(block["manifest_match"] for block in report["blocks"]))
            self.assertTrue(all(block["content_server_check"]["match"] for block in report["blocks"]))
            self.assertEqual(report["corpus_before"], report["corpus_after"])
            self.assertEqual(report["summary"]["suite"]["candidate_over_baseline"], 0.5)
            self.assertEqual(report, json.loads((args.work / "comparison.json").read_text(encoding="utf-8")))
            self.assertIn("TOTAL SUITE", (args.work / "comparison.md").read_text(encoding="utf-8"))
            self.assertEqual(list((args.work / "tmp").iterdir()), [])
            for block in range(1, 9):
                block_calls = [call for call in calls if call[0] == block]
                self.assertEqual([call[2] for call in block_calls[:3]], report["queries"])
                self.assertTrue(all(not call[4] for call in block_calls[:3]))
                self.assertTrue(all(call[4] for call in block_calls[3:]))
                self.assertEqual([call[2:] for call in block_calls], [call[2:] for call in calls[:12]])
            orders = [[sample["query_id"] for sample in report["samples"] if sample["block"] == block]
                      for block in range(1, 9)]
            self.assertTrue(all(order == orders[0] for order in orders))

    def test_failure_persists_error_and_stops_server(self):
        with tempfile.TemporaryDirectory() as directory:
            args, report, _, stopped = self.run_fixture(Path(directory).resolve(), fail_pattern="second")
            self.assertEqual(report["status"], "failed")
            self.assertEqual(stopped, [1])
            self.assertEqual(report["blocks"][0]["status"], "failed")
            self.assertIn("simulated query failure", report["errors"][0]["message"])
            self.assertIn("corpus_after", report)
            self.assertEqual(report["summary"]["suite"]["repetitions"], [])
            saved = json.loads((args.work / "comparison.json").read_text(encoding="utf-8"))
            self.assertEqual(saved["status"], "failed")

    def test_shutdown_index_mutation_invalidates_completed_timings(self):
        with tempfile.TemporaryDirectory() as directory:
            _, report, _, stopped = self.run_fixture(Path(directory).resolve(), mutate_index=True)
            self.assertEqual(report["status"], "failed")
            self.assertEqual(stopped, [1])
            self.assertFalse(report["blocks"][0]["manifest_match"])
            self.assertTrue(report["samples"])
            self.assertEqual(report["summary"]["suite"]["repetitions"], [])
            self.assertIn("Shared index changed", report["errors"][0]["message"])

    def test_dirty_corpus_after_run_fails_with_persistent_artifacts(self):
        with tempfile.TemporaryDirectory() as directory:
            args, report, _, stopped = self.run_fixture(Path(directory).resolve(), dirty_after=True)
            self.assertEqual(stopped, list(range(1, 9)))
            self.assertEqual(report["status"], "failed")
            self.assertEqual(report["errors"][0]["phase"], "final corpus validation")
            self.assertFalse(report["corpus_after"]["clean"])
            saved = json.loads((args.work / "comparison.json").read_text(encoding="utf-8"))
            self.assertEqual(saved["status"], "failed")
            self.assertIn("Corpus is dirty", saved["errors"][0]["message"])

    def test_silent_local_fallback_invalidates_block_even_with_correct_results(self):
        with tempfile.TemporaryDirectory() as directory:
            args, report, _, stopped = self.run_fixture(
                Path(directory).resolve(), omit_trace_pattern="second"
            )
            self.assertEqual(report["status"], "failed")
            self.assertEqual(stopped, [1])
            self.assertTrue(report["samples"])
            self.assertEqual(report["summary"]["suite"]["repetitions"], [])
            check = report["blocks"][0]["content_server_check"]
            self.assertFalse(check["match"])
            self.assertEqual(check["missing"], {"second": args.repeats + 2})
            saved = json.loads((args.work / "comparison.json").read_text(encoding="utf-8"))
            self.assertEqual(saved["blocks"][0]["content_server_check"], check)
            self.assertIn("Content-server trace mismatch", saved["errors"][0]["message"])


if __name__ == "__main__":
    unittest.main()
