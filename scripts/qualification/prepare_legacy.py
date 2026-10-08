# Copyright (c) Microsoft Corporation. All rights reserved.
"""Extract a pinned Git archive and add an independent old-core reader example.

The caller builds the archived CLI and example with Cargo --locked, then supplies
TGREP_V1_BINARY and TGREP_V1_READER to the managed native tests. No current core
code is compiled into either control executable. Existing output is never reused.
"""

import argparse
import json
from pathlib import Path, PurePosixPath
import shutil
import stat
import zipfile


BASELINE = "1120aca41dd192ae61bdd996e9886cb354a78a27"
MAX_ENTRIES = 50000
MAX_BYTES = 256 * 1024 * 1024
PROBE = r"""// Copyright (c) Microsoft Corporation. All rights reserved.
use std::path::PathBuf;

fn main() {
    let mut args = std::env::args_os().skip(1);
    let mode = args.next().expect("reader or shared");
    let path = PathBuf::from(args.next().expect("index directory"));
    assert!(args.next().is_none());
    let result = if mode == "reader" {
        tgrep_core::reader::IndexReader::open(&path).map(|_| ())
    } else if mode == "shared" {
        tgrep_core::shared::SharedBase::open(&path).map(|_| ())
    } else {
        std::process::exit(2);
    };
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
"""


def prepare(archive, destination):
    destination = Path(destination).resolve()
    with zipfile.ZipFile(archive) as source:
        if source.comment != BASELINE.encode("ascii"):
            raise ValueError("archive must be produced by git archive of the pinned baseline")
        members = source.infolist()
        if len(members) > MAX_ENTRIES or sum(item.file_size for item in members) > MAX_BYTES:
            raise ValueError("baseline archive exceeds the extraction bounds")
        seen = set()
        for item in members:
            path = PurePosixPath(item.filename)
            kind = stat.S_IFMT(item.external_attr >> 16)
            if (item.orig_filename != item.filename or path.is_absolute() or not path.parts
                    or any(part in (".", "..") for part in item.filename.split("/") if part)
                    or "\\" in item.filename or ":" in item.filename
                    or kind not in (0, stat.S_IFDIR, stat.S_IFREG)):
                raise ValueError(f"archive has a nonregular or unsafe entry: {item.filename}")
            key = str(path).casefold()
            if key in seen:
                raise ValueError(f"archive has aliased entries: {item.filename}")
            seen.add(key)
        required = ("cargo.toml", "cargo.lock", "tgrep-core/cargo.toml", "tgrep-cli/cargo.toml")
        if not all(name in seen for name in required):
            raise ValueError("baseline archive lacks the complete locked workspace")
        destination.mkdir(parents=True, exist_ok=False)
        for item in members:
            path = destination.joinpath(*PurePosixPath(item.filename).parts)
            if item.is_dir():
                path.mkdir(parents=True, exist_ok=True)
            else:
                path.parent.mkdir(parents=True, exist_ok=True)
                with source.open(item) as reader, path.open("xb") as writer:
                    shutil.copyfileobj(reader, writer, length=1024 * 1024)
        example = destination / "tgrep-core" / "examples" / "legacy_reader.rs"
        example.parent.mkdir(parents=True, exist_ok=True)
        with example.open("x", encoding="utf-8", newline="\n") as writer:
            writer.write(PROBE)
    return {"baseline": BASELINE, "source": str(destination), "probe": str(example)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    parser.add_argument("destination", type=Path)
    args = parser.parse_args()
    print(json.dumps(prepare(args.archive, args.destination)))


if __name__ == "__main__":
    main()
