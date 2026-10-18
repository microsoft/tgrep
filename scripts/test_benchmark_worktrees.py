import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest import mock

import benchmark_worktrees as wt


def args(**overrides):
    values = dict(worktrees=3, workers=1, staging_bytes=wt.GIB, private_work_bytes=wt.GIB,
                  sort_buffer_bytes=16 * wt.MIB, metadata_bytes=64 * wt.MIB, timeout=60.0)
    values.update(overrides)
    return argparse.Namespace(**values)


class StatsParsing(unittest.TestCase):
    def test_server_summary_and_plan(self):
        stats = wt.parse_stats(
            "Query plan: Any([...]) (candidates: 12/34567; raw candidates: 15/34567)\n"
            "1,000 ignored\n"
            "42 matches (40 matched lines) in 12.5ms (via shared daemon v2)\n")
        self.assertEqual(stats["backend"], "shared daemon v2")
        self.assertEqual((stats["matches"], stats["matched_lines"]), (42, 40))
        self.assertEqual(stats["server_ms"], 12.5)
        self.assertEqual((stats["candidates"], stats["total_files"]), (12, 34567))

    def test_no_narrowing_and_ordinary_server(self):
        stats = wt.parse_stats(b"0 matches (0 matched lines) in 3.0ms (via server)"
                               b" (no index narrowing)\r\n")
        self.assertEqual((stats["backend"], stats["matches"]), ("server", 0))

    def test_local_index_summary(self):
        stats = wt.parse_stats("Search completed in 7.25ms: 3 matches (2 matched lines)\n")
        self.assertEqual(stats["backend"], "local index")
        self.assertEqual((stats["server_ms"], stats["matches"], stats["matched_lines"]), (7.25, 3, 2))

    def test_missing_summary_has_no_backend(self):
        self.assertIsNone(wt.parse_stats("warning: no index\n")["backend"])


class Summaries(unittest.TestCase):
    def test_nearest_rank(self):
        summary = wt.summarize([3.0, 1.0, 2.0])
        self.assertEqual((summary["min"], summary["p50"], summary["p95"], summary["max"]),
                         (1.0, 2.0, 3.0, 3.0))
        self.assertEqual(summary["mean"], 2.0)
        self.assertIsNone(wt.summarize([])["p50"])

    def test_digest_ignores_row_order_and_crlf(self):
        self.assertEqual(wt.output_digest(b"b:1:x\r\na:2:y\n"), wt.output_digest(b"a:2:y\nb:1:x\n"))
        self.assertNotEqual(wt.output_digest(b"a:1:x\n"), wt.output_digest(b"a:1:y\n"))


class Policy(unittest.TestCase):
    def test_default_policy_is_valid(self):
        policy = wt.build_policy(args())
        self.assertEqual(policy["schema"], 2)
        self.assertEqual(policy["work"]["blob_bytes"], wt.PROFILE["max_blob_bytes"] + 1)
        self.assertEqual(policy["work"]["max_views"], 8)
        self.assertEqual(wt.build_policy(args(worktrees=20))["work"]["max_views"], 22)
        self.assertEqual(policy["work"]["operation_timeout_ms"], 60000)

    def test_private_work_must_cover_every_worker(self):
        one = wt.minimum_private_bytes({"sort_buffer_bytes": 16 * wt.MIB,
                                        "blob_bytes": wt.PROFILE["max_blob_bytes"] + 1})
        wt.build_policy(args(workers=2, private_work_bytes=2 * one))
        with self.assertRaisesRegex(wt.BenchError, "private-work-bytes"):
            wt.build_policy(args(workers=2, private_work_bytes=2 * one - 1))

    def test_bounds(self):
        for overrides, message in (({"workers": 0}, "workers"), ({"workers": 65}, "workers"),
                                   ({"sort_buffer_bytes": 1024}, "sort-buffer"),
                                   ({"metadata_bytes": 1024}, "metadata"),
                                   ({"worktrees": 1023}, "max_views")):
            with self.subTest(overrides=overrides), self.assertRaisesRegex(wt.BenchError, message):
                wt.build_policy(args(**overrides))


class Arguments(unittest.TestCase):
    def test_defaults(self):
        parsed = wt.parse_args([])
        self.assertEqual((parsed.worktrees, parsed.repeat, parsed.dirty_files, parsed.depth),
                         (3, 3, 0, 2))
        self.assertEqual(parsed.queries, wt.DEFAULT_QUERIES)
        queries, url = wt.load_queries(parsed.queries)
        self.assertTrue(queries)
        self.assertEqual(wt.repo_name(url), "chromium")

    def test_rejects_invalid_counts(self):
        for argv in (["--worktrees", "0"], ["--repeat", "0"], ["--dirty-files", "-1"],
                     ["--worktrees", "1", "--worktree-rev", "a", "--worktree-rev", "b"]):
            with self.subTest(argv=argv), mock.patch("sys.stderr"), \
                    self.assertRaises(SystemExit):
                wt.parse_args(argv)

    def test_query_file_shapes(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "q.json"
            path.write_text(json.dumps(["a", "b", "c"]), encoding="utf-8")
            self.assertEqual(wt.load_queries(path, 2), (["a", "b"], None))
            path.write_text(json.dumps({"queries": []}), encoding="utf-8")
            with self.assertRaises(wt.BenchError):
                wt.load_queries(path)


def synthetic_report():
    timing = {"queue_us": 1000, "discovery_us": 2_000_000, "content_verification_us": 3_000_000}
    attach = {"ready_seconds": 10.0, "receipt_seconds": 9.0, "incarnation": 1, "timings_us": timing,
              "build": {"published": True, "blobs_read": 100, "reused_generation": False},
              "reconcile": {"files_read": 100, "base_reused": 0, "files_extracted": 0}}
    worktree_attach = dict(attach, ready_seconds=2.0, build={
        "published": False, "blobs_read": 0, "reused_generation": True},
        reconcile={"files_read": 100, "base_reused": 100, "files_extracted": 0})
    target = {"matches": 5, "matched_lines": 5, "wall": {"p50": 12.0}, "server": {"p50": 2.0}}
    return {
        "repository": {"name": "repo", "commit": "0123456789abcdef", "tracked_files": 100,
                       "autocrlf": "false"},
        "environment": {"platform": "test", "cpus": 4, "python": "3", "tgrep_version": "tgrep 1",
                        "git_version": "git 2", "source": {"commit": "abc"}},
        "parameters": {"repeat": 3, "workers": 1},
        "ordinary": {"index_seconds": 30.0, "server_ready_seconds": 1.0,
                     "index_size": {"bytes": wt.GIB}},
        "managed": {"daemon_start_seconds": .5, "base": attach,
                    "storage_after_base": {"bytes": wt.GIB},
                    "storage_after_worktrees": {"bytes": wt.GIB}},
        "worktrees": [{"number": 1, "revision": "fedcba9876543210", "checkout_seconds": 5.0,
                       "dirty": {"modified": ["a.cc"]}, "attach": worktree_attach}],
        "queries": {"a|b": {"targets": {"ordinary-base": target, "worktree-1": target},
                            "parity": {"mismatched": []}}},
        "private_overlay": {"ok": True, "observed": {"worktree-1": 1}},
        "checks": {"parity_ok": True, "backend_ok": True, "reuse_ok": True},
        "failures": [],
    }


class Markdown(unittest.TestCase):
    def test_renders_every_section(self):
        markdown = wt.render_markdown(synthetic_report())
        for expected in ("# tgrep multi-worktree benchmark: repo @ 0123456789ab",
                         "| Ordinary base index (`tgrep index`) | 30.0 | 1.00 GiB index |",
                         "| Worktree 1: attach to ready | 2.0 | reused_generation=True",
                         "dirty files=1", "| Base | 0.0 | - | 2.0 | 3.0 |",
                         "| `a\\|b` | 12.0 | 12.0 | 5 |", "| **Sum of p50 (client)** | **12.0** |",
                         "| Sum of p50 (server-reported) | 2.0 | 2.0 | |",
                         "- Private overlay isolated to the dirty worktree: ok"):
            self.assertIn(expected, markdown)


def binary_or_none():
    try:
        return wt.find_binary()
    except wt.BenchError:
        return None


@unittest.skipUnless(shutil.which("git"), "git is required")
@unittest.skipUnless(binary_or_none(), "set TGREP_BIN or build target/release/tgrep")
class EndToEnd(unittest.TestCase):
    """Run the full harness on a tiny repository with the built binary."""

    def test_small_repository(self):
        with tempfile.TemporaryDirectory(prefix="tgrep-wt-bench-test-") as directory:
            root = Path(directory).resolve()
            repo = root / "repo"
            (repo / "src").mkdir(parents=True)
            for number in range(24):
                (repo / "src" / f"file{number}.cc").write_text(
                    f"int value{number} = {number};\n// TODO fix {number}\n", encoding="utf-8")
            (repo / "README.md").write_text("hello world\n", encoding="utf-8")
            env = dict(os.environ, GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@example.invalid",
                       GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@example.invalid")
            for argv in (["init", "-q"], ["config", "core.autocrlf", "false"], ["add", "."],
                         ["commit", "-qm", "init"]):
                subprocess.run(["git", *argv], cwd=repo, env=env, check=True,
                               stdout=subprocess.DEVNULL)
            queries = root / "queries.json"
            queries.write_text(json.dumps({"queries": ["TODO", "value1[0-9]", "hello"]}),
                               encoding="utf-8")
            results = root / "out" / "result.json"
            argv = ["--repo-path", str(repo), "--bench-dir", str(root / "bench"),
                    "--queries", str(queries), "--worktrees", "2", "--dirty-files", "2",
                    "--repeat", "1", "--timeout", "300", "--query-timeout", "120",
                    "--results-json", str(results), "--private-work-bytes", str(wt.GIB),
                    "--staging-bytes", str(wt.GIB), "--metadata-bytes", str(64 * wt.MIB)]
            with mock.patch("sys.stdout"):
                status = wt.main(argv)
            report = json.loads(results.read_text(encoding="utf-8"))
            self.assertEqual(status, 0, json.dumps(
                {key: report.get(key) for key in ("error", "failures", "cleanup")}, indent=2))
            self.assertTrue(report["checks"]["ok"])
            self.assertEqual(len(report["worktrees"]), 2)
            first = report["worktrees"][0]
            self.assertTrue(first["reuse_ok"])
            self.assertEqual(first["attach"]["build"]["blobs_read"], 0)
            self.assertTrue(report["managed"]["base"]["build"]["published"])
            self.assertEqual(report["private_overlay"]["observed"]["worktree-2"], 3)
            self.assertEqual(report["private_overlay"]["observed"]["managed-base"], 0)
            todo = report["queries"]["TODO"]["targets"]
            self.assertEqual({record["matches"] for record in todo.values()}, {24})
            self.assertEqual(set(todo), {"ordinary-base", "managed-base", "worktree-1", "worktree-2"})
            self.assertFalse(report["cleanup"]["errors"])
            self.assertTrue(results.with_suffix(".md").is_file())
            self.assertFalse((root / "bench" / "worktrees" / "wt-1").exists())
            self.assertFalse((root / "bench" / "managed-storage").exists())
            self.assertTrue((repo / ".git").is_dir())


if __name__ == "__main__":
    unittest.main()
