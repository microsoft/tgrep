# Copyright (c) Microsoft Corporation. All rights reserved.

import copy
import hashlib
import json
from pathlib import Path
import tempfile
import unittest

import capture_managed_artifacts as capture


class ManagedArtifactTests(unittest.TestCase):
    def fixture(self, root):
        crate = root / "tgrep-cli"
        records = []
        for name, kind, source, test in (
            ("tgrep", ["bin"], "src/main.rs", False),
            ("shared_daemon", ["test"], "tests/shared_daemon.rs", True),
        ):
            path = root / "target" / "release" / (name + "-selected")
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(name.encode())
            records.append({
                "reason": "compiler-artifact",
                "manifest_path": str(crate / "Cargo.toml"),
                "target": {"name": name, "kind": kind, "src_path": str(crate / source)},
                "profile": {"test": test}, "features": ["managed-test-hooks"],
                "executable": str(path),
            })
        records.append({"reason": "build-finished", "success": True})
        return records

    def write_records(self, root, records):
        path = root / "cargo.jsonl"
        path.write_text("".join(json.dumps(record) + "\n" for record in records), encoding="utf-8")
        return path

    def test_retains_only_selected_bytes_and_records_hashes_and_cargo_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            records = self.fixture(root)
            stale = root / "target/release/shared_daemon-stale"
            stale.write_bytes(b"not the selected executable")
            destination = root / "retained"
            paths = capture.capture(root, self.write_records(root, records), destination)
            manifest = json.loads((destination / "selection.json").read_text(encoding="utf-8"))
            for role, record in zip(("cli", "harness"), records):
                expected = Path(record["executable"])
                self.assertEqual(paths[role], expected.resolve())
                retained = manifest["executables"][role]
                self.assertEqual((destination / retained["retained"]).read_bytes(), expected.read_bytes())
                self.assertEqual(retained["sha256"], hashlib.sha256(expected.read_bytes()).hexdigest())
                self.assertEqual(retained["cargo_artifact"], record)
            self.assertFalse((destination / stale.name).exists())
            output = root / "github-output"
            capture.github_outputs(paths, output)
            self.assertEqual(
                output.read_text(encoding="utf-8"),
                "".join(f"{role}={path.as_posix()}\n" for role, path in paths.items()),
            )
            with self.assertRaises(FileExistsError):
                capture.capture(root, root / "cargo.jsonl", destination)

    def test_rejects_missing_ambiguous_wrong_or_unfinished_artifacts_before_copying(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            original = self.fixture(root)
            cases = [
                original[:1] + original[2:],
                original[:-1],
                original + [original[-1]],
                original[:2] + [{"reason": "build-finished", "success": False}],
                [original[0], *original],
            ]
            for field, value in (("features", []), ("manifest_path", str(root / "Cargo.toml"))):
                changed = copy.deepcopy(original)
                changed[0][field] = value
                cases.append(changed)
            for field, value in (("src_path", str(root / "different.rs")), ("kind", ["example"])):
                changed = copy.deepcopy(original)
                changed[1]["target"][field] = value
                cases.append(changed)
            changed = copy.deepcopy(original)
            changed[1]["profile"]["test"] = False
            cases.append(changed)
            for index, records in enumerate(cases):
                with self.subTest(index=index):
                    destination = root / f"rejected-{index}"
                    with self.assertRaises(ValueError):
                        capture.capture(root, self.write_records(root, records), destination)
                    self.assertFalse(destination.exists())

    def test_missing_or_aliased_executables_and_multiline_outputs_are_errors(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            records = self.fixture(root)
            records[1]["executable"] = records[0]["executable"]
            with self.assertRaises(ValueError):
                capture.capture(root, self.write_records(root, records), root / "aliased")
            records[1]["executable"] = str(root / "missing")
            with self.assertRaises(FileNotFoundError):
                capture.capture(root, self.write_records(root, records), root / "missing-output")
            with self.assertRaises(ValueError):
                capture.github_outputs({"harness": Path("path\ninjected=value")}, root / "outputs")
            self.assertFalse((root / "outputs").exists())


if __name__ == "__main__":
    unittest.main()
