# Copyright (c) Microsoft Corporation. All rights reserved.
"""Preserve the exact managed measurement executables selected by Cargo JSON."""

import argparse
import hashlib
import json
from pathlib import Path
import shutil


CHECKOUT = Path(__file__).resolve().parents[2]


def capture(checkout, messages, destination):
    crate = (checkout / "tgrep-cli").resolve()
    targets = {
        "cli": ("tgrep", ["bin"], crate / "src/main.rs", False),
        "harness": ("shared_daemon", ["test"], crate / "tests/shared_daemon.rs", True),
    }
    candidates = {role: [] for role in targets}
    completed = []
    with messages.open(encoding="utf-8") as stream:
        for line in stream:
            if not line.strip():
                continue
            record = json.loads(line)
            if record.get("reason") == "build-finished":
                completed.append(record.get("success"))
            if record.get("reason") != "compiler-artifact" or not record.get("executable"):
                continue
            target = record["target"]
            for role, (name, kind, source, test) in targets.items():
                if (target["name"] == name and target["kind"] == kind
                        and Path(target["src_path"]).resolve() == source
                        and Path(record["manifest_path"]).resolve() == crate / "Cargo.toml"
                        and record["profile"]["test"] is test
                        and "managed-test-hooks" in record["features"]):
                    candidates[role].append(record)
    if completed != [True] or any(len(records) != 1 for records in candidates.values()):
        raise ValueError("require one successful Cargo build and exactly one CLI and harness artifact")

    selected = {role: records[0] for role, records in candidates.items()}
    paths = {
        role: Path(record["executable"]).resolve(strict=True)
        for role, record in selected.items()
    }
    if len(set(paths.values())) != 2 or not all(path.is_file() for path in paths.values()):
        raise ValueError("measurement executables must be two distinct regular files")
    destination.mkdir(parents=True, exist_ok=False)
    manifest = {"schema_version": 1, "executables": {}}
    for role, path in paths.items():
        retained = destination / path.name
        with path.open("rb") as source, retained.open("xb") as output:
            shutil.copyfileobj(source, output, length=65536)
        with retained.open("rb") as source:
            digest = hashlib.file_digest(source, "sha256").hexdigest()
        manifest["executables"][role] = {
            "source": str(path), "retained": retained.name,
            "sha256": digest, "bytes": retained.stat().st_size,
            "cargo_artifact": selected[role],
        }
    with (destination / "selection.json").open("x", encoding="utf-8", newline="\n") as output:
        json.dump(manifest, output, indent=2)
        output.write("\n")
    return paths


def github_outputs(paths, destination):
    values = {role: path.as_posix() for role, path in paths.items()}
    if any("\n" in value or "\r" in value for value in values.values()):
        raise ValueError("GitHub output paths must be single-line values")
    with destination.open("a", encoding="utf-8", newline="\n") as output:
        for role, value in values.items():
            output.write(f"{role}={value}\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("messages", type=Path)
    parser.add_argument("destination", type=Path)
    parser.add_argument("--github-output", type=Path)
    args = parser.parse_args()
    paths = capture(CHECKOUT, args.messages, args.destination)
    if args.github_output is not None:
        github_outputs(paths, args.github_output)
    print(json.dumps({role: str(path) for role, path in paths.items()}))


if __name__ == "__main__":
    main()
