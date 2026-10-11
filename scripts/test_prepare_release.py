# Copyright (c) Microsoft Corporation. All rights reserved.
"""Exercise release tagging against isolated local Git remotes, without GitHub."""

import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("prepare_release.py").resolve()
TAG = "v1.2.0"
TAG_REF = f"refs/tags/{TAG}"


class PrepareReleaseTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="tgrep-release-test-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.remote = self.root / "origin.git"
        self.checkout = self.root / "checkout"
        self.checkout.mkdir()
        self.env = dict(os.environ, GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull,
                        GIT_AUTHOR_NAME="Release Test", GIT_AUTHOR_EMAIL="test@example.invalid",
                        GIT_COMMITTER_NAME="Release Test",
                        GIT_COMMITTER_EMAIL="test@example.invalid", GIT_TERMINAL_PROMPT="0")
        self.git("init", "--bare", str(self.remote))
        self.git("init", "--initial-branch=main")
        (self.checkout / "Cargo.toml").write_text(
            '[workspace.package]\nversion = "1.2.0"\n', encoding="utf-8"
        )
        self.git("add", "Cargo.toml")
        self.git("commit", "-m", "Release fixture")
        self.sha = self.git("rev-parse", "HEAD")
        self.git("remote", "add", "origin", str(self.remote))
        self.git("push", "origin", "HEAD:refs/heads/main")
        self.output = self.root / "github-output"

    def git(self, *args):
        return subprocess.run(
            ["git", *args], cwd=self.checkout, env=self.env, check=True,
            capture_output=True, text=True, timeout=30
        ).stdout.strip()

    def run_release(self, *, tag=TAG, event="workflow_dispatch", ref="refs/heads/main",
                    sha=None, verify_only=False):
        self.output.write_text("", encoding="utf-8")
        env = dict(self.env, GITHUB_EVENT_NAME=event, GITHUB_REF=ref,
                   GITHUB_SHA=self.sha if sha is None else sha, RELEASE_TAG=tag,
                   GITHUB_OUTPUT=str(self.output))
        args = [sys.executable, "-B", str(SCRIPT)]
        if verify_only:
            args.append("--verify-only")
        return subprocess.run(args, cwd=self.checkout, env=env, capture_output=True,
                              text=True, timeout=30)

    def assert_success(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.output.read_text(encoding="utf-8"),
                         f"tag={TAG}\nsha={self.sha}\n")

    def assert_failure(self, result, message):
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(message, result.stderr)
        self.assertEqual(self.output.read_text(encoding="utf-8"), "")

    def remote_tag(self):
        return self.git("ls-remote", "--tags", "origin", TAG_REF, f"{TAG_REF}^{{}}")

    def other_commit(self):
        return self.git("commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "Another snapshot")

    def test_manual_release_creates_tag_at_exact_snapshot_and_emits_outputs(self):
        self.assert_success(self.run_release())
        self.assertEqual(self.remote_tag(), f"{self.sha}\t{TAG_REF}")

    def test_rerun_reuses_matching_tag_without_moving_it(self):
        self.assert_success(self.run_release())
        original = self.remote_tag()
        self.assert_success(self.run_release())
        self.assertEqual(self.remote_tag(), original)

    def test_main_advancing_does_not_change_the_dispatched_snapshot(self):
        later = self.other_commit()
        self.git("push", "origin", f"{later}:refs/heads/main")
        self.assert_success(self.run_release())
        self.assertEqual(self.remote_tag(), f"{self.sha}\t{TAG_REF}")

    def test_rejects_other_branches_events_and_mismatched_push_refs(self):
        for args in ({"ref": "refs/heads/feature"}, {"ref": TAG_REF},
                     {"event": "schedule"},
                     {"event": "push"}, {"event": "push", "ref": "refs/tags/v1.1.0"}):
            with self.subTest(args=args):
                self.assert_failure(self.run_release(**args), "error:")
                self.assertEqual(self.remote_tag(), "")

    def test_rejects_version_mismatches_and_untrusted_tag_text_before_writing(self):
        for tag in ("", "1.2.0", "v1.1.0", "v1.2.0\nsha=bad", "v1.2.0;echo bad"):
            with self.subTest(tag=tag):
                self.assert_failure(self.run_release(tag=tag), "must match workspace version")
                self.assertEqual(self.git("ls-remote", "--tags", "origin"), "")

    def test_rejects_checkout_mismatching_workflow_sha(self):
        self.assert_failure(self.run_release(sha=self.other_commit()), "exact commit")
        self.assertEqual(self.remote_tag(), "")

    def test_rejects_non_commit_or_non_object_workflow_sha(self):
        for sha in ("HEAD", "refs/heads/main", self.sha[:12], "0" * 40,
                    self.git("rev-parse", "HEAD^{tree}"),
                    self.git("rev-parse", "HEAD:Cargo.toml")):
            with self.subTest(sha=sha):
                self.assert_failure(self.run_release(sha=sha), "error:")
                self.assertEqual(self.remote_tag(), "")

    def test_rejects_conflicting_existing_tag_without_moving_it(self):
        other = self.other_commit()
        self.git("push", "origin", f"{other}:{TAG_REF}")
        original = self.remote_tag()
        self.assert_failure(self.run_release(), "already points to")
        self.assertEqual(self.remote_tag(), original)

    def test_existing_annotated_tag_is_peeled_and_preserved(self):
        self.git("tag", "-a", TAG, self.sha, "-m", "Annotated release")
        self.git("push", "origin", TAG_REF)
        original = self.remote_tag()
        self.assertIn(f"{self.sha}\t{TAG_REF}^{{}}", original)
        self.assert_success(self.run_release())
        self.assert_success(self.run_release(event="push", ref=TAG_REF))
        self.assertEqual(self.remote_tag(), original)

    def test_annotated_tag_object_sha_matches_shallow_checkout(self):
        self.git("tag", "-a", TAG, self.sha, "-m", "Annotated release")
        self.git("push", "origin", TAG_REF)
        tag_sha = self.git("rev-parse", TAG_REF)
        original = self.remote_tag()
        self.assertNotEqual(tag_sha, self.sha)

        self.checkout = self.root / "shallow"
        self.checkout.mkdir()
        self.git("init")
        self.git("remote", "add", "origin", str(self.remote))
        self.git("fetch", "--no-tags", "--depth=1", "origin", tag_sha)
        self.git("checkout", "--detach", "FETCH_HEAD")
        self.assertEqual(self.git("rev-parse", "--is-shallow-repository"), "true")
        self.assertEqual(self.git("cat-file", "-t", tag_sha), "tag")
        for verify_only in (False, True):
            with self.subTest(verify_only=verify_only):
                self.assert_success(self.run_release(
                    event="push", ref=TAG_REF, sha=tag_sha, verify_only=verify_only
                ))
                self.assertEqual(self.remote_tag(), original)

    def test_annotated_workflow_sha_must_match_checked_out_commit(self):
        self.git("tag", "-a", TAG, self.other_commit(), "-m", "Different snapshot")
        self.git("push", "origin", TAG_REF)
        tag_sha = self.git("rev-parse", TAG_REF)
        original = self.remote_tag()
        self.assert_failure(
            self.run_release(event="push", ref=TAG_REF, sha=tag_sha), "exact commit"
        )
        self.assertEqual(self.remote_tag(), original)

    def test_matching_tag_push_uses_existing_tag(self):
        self.git("push", "origin", f"{self.sha}:{TAG_REF}")
        self.assert_success(self.run_release(event="push", ref=TAG_REF))

    def test_missing_push_tag_is_not_recreated(self):
        self.assert_failure(self.run_release(event="push", ref=TAG_REF), "refusing to recreate")
        self.assertEqual(self.remote_tag(), "")

    def test_verify_only_never_creates_tag(self):
        self.assert_failure(self.run_release(verify_only=True), "refusing to recreate")
        self.assertEqual(self.remote_tag(), "")
        self.assert_success(self.run_release())
        self.assert_success(self.run_release(verify_only=True))

    def test_publish_verification_rejects_deleted_or_changed_tag(self):
        self.assert_success(self.run_release())
        self.git("push", "origin", f":{TAG_REF}")
        self.assert_failure(self.run_release(verify_only=True), "refusing to recreate")
        self.assertEqual(self.remote_tag(), "")
        other = self.other_commit()
        self.git("push", "origin", f"{other}:{TAG_REF}")
        self.assert_failure(self.run_release(verify_only=True), "already points to")
        self.assertEqual(self.remote_tag(), f"{other}\t{TAG_REF}")

    def test_failed_remote_read_is_not_treated_as_absent_tag(self):
        self.git("remote", "set-url", "origin", str(self.root / "missing.git"))
        self.assert_failure(self.run_release(), "ls-remote")

    def test_failed_push_is_reported_without_success_outputs(self):
        self.git("remote", "set-url", "--push", "origin", str(self.root / "missing.git"))
        self.assert_failure(self.run_release(), "push")
        self.assertEqual(self.remote_tag(), "")


if __name__ == "__main__":
    unittest.main()
