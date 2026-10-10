# Copyright (c) Microsoft Corporation. All rights reserved.

from pathlib import Path
import stat
import tempfile
import unittest
import zipfile

import prepare_legacy


class LegacyPreparationTests(unittest.TestCase):
    def archive(self, root, extra=(), comment=prepare_legacy.BASELINE.encode("ascii")):
        path = root / "source.zip"
        with zipfile.ZipFile(path, "w") as archive:
            archive.comment = comment
            for name in ("Cargo.toml", "Cargo.lock", "tgrep-core/Cargo.toml", "tgrep-cli/Cargo.toml"):
                archive.writestr(name, "unchanged baseline\n")
            for item in extra:
                if isinstance(item, str):
                    raw = zipfile.ZipInfo("placeholder")
                    raw.filename = raw.orig_filename = item
                    item = raw
                archive.writestr(item, "not an owned regular source file\n")
        return path

    def test_requires_exact_archive_and_never_reuses_an_existing_output(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive = self.archive(root)
            destination = root / "baseline"
            result = prepare_legacy.prepare(archive, destination)
            self.assertEqual(result["baseline"], prepare_legacy.BASELINE)
            self.assertEqual((destination / "Cargo.lock").read_text(), "unchanged baseline\n")
            self.assertIn("tgrep_core::shared::SharedBase::open", Path(result["probe"]).read_text())
            with self.assertRaises(FileExistsError):
                prepare_legacy.prepare(archive, destination)
            self.archive(root, comment=b"different source")
            with self.assertRaisesRegex(ValueError, "pinned baseline"):
                prepare_legacy.prepare(archive, root / "wrong")
            self.assertFalse((root / "wrong").exists())

    def test_rejects_unsafe_or_aliased_members_before_writing_any_source(self):
        link = zipfile.ZipInfo("link")
        link.external_attr = (stat.S_IFLNK | 0o777) << 16
        for member in ("../escape", "/absolute", "nested\\escape", "C:stream", "cargo.toml", link):
            with self.subTest(member=member), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                archive = self.archive(root, (member,))
                with self.assertRaises(ValueError):
                    prepare_legacy.prepare(archive, root / "baseline")
                self.assertFalse((root / "baseline").exists())

    def test_rejects_file_and_child_conflicts_before_writing_any_source(self):
        for members in (
            ("conflict", "conflict/child"),
            ("conflict/child", "conflict"),
            ("Conflict", "conflict/nested/child"),
            ("tgrep-core/examples",),
            ("tgrep-core/examples/legacy_reader.rs",),
            ("TGREP-CORE/Examples/Legacy_Reader.rs",),
        ):
            with self.subTest(members=members), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                archive = self.archive(root, members)
                with self.assertRaises(ValueError):
                    prepare_legacy.prepare(archive, root / "baseline")
                self.assertFalse((root / "baseline").exists())
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive = self.archive(root, ("tgrep-core/examples/", "nested/", "nested/child"))
            result = prepare_legacy.prepare(archive, root / "baseline")
            self.assertTrue(Path(result["probe"]).is_file())
            self.assertTrue((root / "baseline" / "nested" / "child").is_file())


if __name__ == "__main__":
    unittest.main()
