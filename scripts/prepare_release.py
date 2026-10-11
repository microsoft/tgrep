# Copyright (c) Microsoft Corporation. All rights reserved.
"""Pin a release to its workflow snapshot; only manual main runs create tags."""

import argparse
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib


def git(*args):
    return subprocess.run(
        ["git", *args], check=True, capture_output=True, text=True, timeout=60
    ).stdout.strip()


def remote_tag_commit(tag):
    ref = f"refs/tags/{tag}"
    peeled = f"{ref}^{{}}"
    refs = {}
    for line in git("ls-remote", "--tags", "origin", ref, peeled).splitlines():
        sha, name = line.split("\t")
        if name not in (ref, peeled) or name in refs:
            raise ValueError(f"unexpected remote tag reference: {name!r}")
        refs[name] = sha
    if ref not in refs:
        if refs:
            raise ValueError(f"remote tag {tag!r} has no direct reference")
        return None
    return refs.get(peeled, refs[ref])


def prepare_release(event, ref, sha, tag, *, verify_only=False):
    tag_ref = f"refs/tags/{tag}"
    if event == "workflow_dispatch":
        if ref != "refs/heads/main":
            raise ValueError("manual releases must run from main")
    elif event == "push":
        if ref != tag_ref:
            raise ValueError("push releases must use the pushed tag")
    else:
        raise ValueError(f"unsupported release event: {event!r}")

    with Path("Cargo.toml").open("rb") as manifest:
        version = tomllib.load(manifest)["workspace"]["package"]["version"]
    if not isinstance(version, str) or tag != f"v{version}":
        raise ValueError(f"release tag {tag!r} must match workspace version v{version}")
    git("check-ref-format", tag_ref)
    if not re.fullmatch(r"(?:[0-9a-fA-F]{40}|[0-9a-fA-F]{64})", sha):
        raise ValueError("workflow SHA must be a full Git object ID")
    # Peel the captured object, never a tag ref that could have moved meanwhile.
    sha = git("rev-parse", "--verify", "--end-of-options", f"{sha}^{{commit}}")
    if git("rev-parse", "--verify", "HEAD^{commit}") != sha:
        raise ValueError("checkout does not match the workflow's exact commit")

    existing = remote_tag_commit(tag)
    if existing is None:
        if event != "workflow_dispatch" or verify_only:
            raise ValueError(f"release tag {tag!r} is missing; refusing to recreate it")
        # A non-forced push cannot replace a concurrently created tag.
        git("push", "origin", f"{sha}:{tag_ref}")
    elif existing != sha:
        raise ValueError(f"release tag {tag!r} already points to {existing}, not {sha}")

    if remote_tag_commit(tag) != sha:
        raise ValueError(f"release tag {tag!r} changed or disappeared during verification")
    print(f"Verified release tag {tag} at {sha}")
    return tag, sha


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--verify-only", action="store_true",
                        help="verify an existing tag without creating one")
    args = parser.parse_args()
    try:
        output = Path(os.environ["GITHUB_OUTPUT"])
        tag, sha = prepare_release(
            os.environ["GITHUB_EVENT_NAME"],
            os.environ["GITHUB_REF"],
            os.environ["GITHUB_SHA"],
            os.environ["RELEASE_TAG"],
            verify_only=args.verify_only,
        )
        with output.open("a", encoding="utf-8", newline="\n") as stream:
            stream.write(f"tag={tag}\nsha={sha}\n")
    except subprocess.CalledProcessError as error:
        print(f"error: {error}\n{error.stderr.rstrip()}", file=sys.stderr)
        return 1
    except (OSError, ValueError, KeyError, subprocess.TimeoutExpired) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
