"""Checkout qualification, not an agent runtime or a release/signing pipeline.

Run with Python 3.11+ and Cargo/Git on PATH:
  python -B scripts/qualification/qualify.py dependencies
  python -B scripts/qualification/qualify.py installed

`dependencies` checks both locked dependency graphs and all four Linux fuzz
targets (a C++ compiler is required by libfuzzer-sys; nightly/cargo-fuzz are not).
`installed` performs the documented release-profile cargo install into a private
temporary root, then exercises that exact executable, never a PATH/debug binary.
Fixtures, Git configuration, storage, logs and installation are private and
removed after owned servers stop, including on failures/timeouts. Cargo's normal
download/build cache is reused. On WSL, run from a native Linux checkout and use
a native TMPDIR, not /mnt/...; native-path regressions belong in cargo test.
"""

import argparse
from contextlib import contextmanager
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time


CHECKOUT = Path(__file__).resolve().parents[2]
LOCKFILES = ("Cargo.lock", "fuzz/Cargo.lock", "vendor/ignore/Cargo.lock")
FUZZ_TARGETS = ("fuzz_trigram", "fuzz_query", "fuzz_ondisk", "fuzz_reader")
PATTERN = "qualification_hit"


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


@contextmanager
def unchanged_locks(checkout):
    before = {name: (checkout / name).read_bytes() for name in LOCKFILES}
    try:
        yield
    finally:
        changed = [
            name for name, content in before.items()
            if not (checkout / name).is_file()
            or (checkout / name).read_bytes() != content
        ]
        require(not changed, f"qualification changed lockfiles: {changed}")


def stop(child, job=None):
    # These are only children created here, never a PID discovered from a marker.
    if os.name == "nt":
        try:
            job.terminate()
        finally:
            if child.poll() is None:
                child.kill()
            child.wait(timeout=10)
    else:
        # Each child owns a new process group, including Cargo/Git descendants.
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    child.wait(timeout=10)


@contextmanager
def process(argv, **kwargs):
    job = None
    if os.name == "nt":
        from windows_job import WindowsJob
        job = WindowsJob()
    try:
        child = subprocess.Popen(
            [str(arg) for arg in argv], stdin=subprocess.DEVNULL,
            start_new_session=os.name != "nt",
            creationflags=4 if job is not None else 0,  # CREATE_SUSPENDED
            **kwargs,
        )
        try:
            if job is not None:
                job.assign_and_resume(child.pid)
            yield child
        finally:
            try:
                stop(child, job)
            finally:
                for pipe in (child.stdout, child.stderr):
                    if pipe is not None:
                        pipe.close()
                if job is not None:
                    # Popen otherwise retains this owned handle until garbage collection.
                    child._handle.Close()
    finally:
        if job is not None:
            job.close()


def run(argv, *, cwd, env=None, timeout=60, expected=0):
    with process(
        argv, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, encoding="utf-8", errors="strict",
    ) as child:
        try:
            stdout, stderr = child.communicate(timeout=timeout)
        except subprocess.TimeoutExpired as error:
            raise RuntimeError(
                f"command timed out after {timeout}s: {argv}\n"
                f"stdout: {error.stdout!r}\nstderr: {error.stderr!r}"
            ) from error
        result = subprocess.CompletedProcess(argv, child.returncode, stdout, stderr)
        require(
            result.returncode == expected,
            f"{argv}: expected exit {expected}, got {result.returncode}\n"
            f"stdout:\n{stdout}\nstderr:\n{stderr}",
        )
        return result


def check_graph(metadata, checkout):
    packages = metadata["packages"]
    ignores = [package for package in packages if package["name"] == "ignore"]
    require(len(ignores) == 1, f"expected only one vendored ignore, got {ignores}")
    ignore = ignores[0]
    require(
        ignore["version"] == "0.4.25"
        and ignore["source"] is None
        and Path(ignore["manifest_path"]).resolve()
        == (checkout / "vendor/ignore/Cargo.toml").resolve(),
        f"ignore must resolve to the locked 0.4.25 path dependency: {ignore}",
    )
    core = next(package for package in packages if package["name"] == "tgrep-core")
    node = next(node for node in metadata["resolve"]["nodes"] if node["id"] == core["id"])
    require(
        [dep["pkg"] for dep in node["deps"] if dep["name"] == "ignore"] == [ignore["id"]],
        "tgrep-core must directly resolve to vendored ignore",
    )


def dependencies(checkout):
    for manifest in ("Cargo.toml", "fuzz/Cargo.toml"):
        result = run(
            ["cargo", "metadata", "--format-version", "1", "--locked",
             "--manifest-path", checkout / manifest],
            cwd=checkout, timeout=300,
        )
        check_graph(json.loads(result.stdout), checkout)
        print(f"{manifest}: only path-vendored ignore 0.4.25", flush=True)
    run(
        ["cargo", "check", "--locked", "--manifest-path", checkout / "fuzz/Cargo.toml",
         *(arg for target in FUZZ_TARGETS for arg in ("--bin", target))],
        cwd=checkout, timeout=600,
    )
    print(f"Checked all four fuzz targets: {', '.join(FUZZ_TARGETS)}", flush=True)


def fixture_environment(home):
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    env.update({
        "HOME": str(home), "USERPROFILE": str(home),
        "XDG_CONFIG_HOME": str(home), "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_GLOBAL": str(home / "gitconfig"), "GIT_TERMINAL_PROMPT": "0",
        "GIT_AUTHOR_NAME": "Qualification", "GIT_COMMITTER_NAME": "Qualification",
        "GIT_AUTHOR_EMAIL": "qualification@example.invalid",
        "GIT_COMMITTER_EMAIL": "qualification@example.invalid",
    })
    return env


@contextmanager
def server(binary, root, options, env, log):
    with log.open("wb") as output:
        try:
            with process(
                [binary, "serve", root, *options], cwd=root, env=env,
                stdout=output, stderr=subprocess.STDOUT,
            ) as child:
                yield child
        except BaseException:
            output.close()
            print(f"Server log ({log}):\n{log.read_text(encoding='utf-8', errors='replace')}",
                  flush=True)
            raise


def wait_for(child, probe, description, timeout=30):
    deadline = time.monotonic() + timeout
    while True:
        require(child.poll() is None, f"{description}: server exited ({child.returncode})")
        remaining = deadline - time.monotonic()
        require(remaining > 0, f"timed out waiting for {description}")
        if probe(remaining):
            return
        time.sleep(min(0.05, max(0, deadline - time.monotonic())))


def marker_owned(path, child):
    try:
        marker = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        return False
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        print(f"Waiting for complete registration {path}: {error}", flush=True)
        return False
    return isinstance(marker, dict) and marker.get("pid") == child.pid


def assert_parity(indexed, scanned, backend):
    require(indexed.returncode == scanned.returncode, "indexed/scan exit-code mismatch")
    require(indexed.stdout == scanned.stdout,
            f"indexed/scan output mismatch:\n{indexed.stdout!r}\n{scanned.stdout!r}")
    require(backend in indexed.stderr,
            f"expected backend {backend!r}, got stderr:\n{indexed.stderr}")
    require("(via filesystem walk)" in scanned.stderr
            or "Brute-force search completed" in scanned.stderr,
            f"expected filesystem scan control, got stderr:\n{scanned.stderr}")


def smoke(binary, scratch):
    require(binary.is_file(), f"installed executable missing: {binary}")
    home, root, linked, storage = (scratch / name for name in ("home", "repo", "linked", "storage"))
    for directory in (home, root, storage):
        directory.mkdir()
    env = fixture_environment(home)

    def git(*args):
        return run(["git", *args], cwd=root, env=env).stdout.strip()

    def cli(where, *args, timeout=30, expected=0):
        return run([binary, *args], cwd=where, env=env, timeout=timeout, expected=expected)

    def parity(where, backend, *, hidden=False, content_backend=None):
        for flags in (["--files"], ["-n", "--with-filename", "-F", "--", PATTERN]):
            options = ["--stats", "--sort", "path", "--color", "never"]
            if hidden:
                options.append("--hidden")
                if where == root:
                    # Explicit scans can inspect Git internals; ordinary indexes cannot.
                    options.extend(["--glob", "!.git"])
            indexed = cli(where, *options, *flags, ".")
            scanned = cli(where, "--no-index", *options, *flags, ".")
            expected_backend = content_backend if content_backend and flags[0] != "--files" else backend
            assert_parity(indexed, scanned, expected_backend)
            require(bool(indexed.stdout.strip()), "fixture must produce nonempty results")

    git("init", "-q")
    git("config", "core.autocrlf", "false")
    for name in ("keep.txt", "gone.txt", ".hidden"):
        (root / name).write_text(f"{PATTERN} base {name}\n", encoding="utf-8")
    (root / ".gitignore").write_text("ignored.txt\n", encoding="utf-8")
    git("add", ".")
    git("commit", "-qm", "qualification base")
    revision = git("rev-parse", "HEAD")
    (root / "ignored.txt").write_text(f"{PATTERN} ignored\n", encoding="utf-8")

    cli(root, "index", ".")
    parity(root, "(via local index)", content_backend="Search completed in ")
    parity(root, "(via local index)", content_backend="Search completed in ", hidden=True)
    with server(binary, root, ["--no-watch"], env, scratch / "ordinary.log") as child:
        wait_for(child, lambda _: marker_owned(root / ".tgrep/serve.json", child),
                 "ordinary registration")

        def ordinary_ready(remaining):
            status = cli(root, "status", ".", timeout=remaining).stdout
            return ("Server status for" in status and "Indexing:   complete" in status
                    and "Hidden coverage: complete" in status)

        wait_for(child, ordinary_ready, "ordinary complete coverage")
        parity(root, "(via server)")
        parity(root, "(via server)", hidden=True)
    print("Installed binary: ordinary local index and server parity", flush=True)

    git("worktree", "add", "-q", "--detach", str(linked), revision)
    # An intentionally stale ordinary index must never mask shared fallback.
    cli(linked, "index", ".")
    (linked / "keep.txt").write_text(f"{PATTERN} private\n", encoding="utf-8")
    (linked / "gone.txt").unlink()
    (linked / "new.txt").write_text(f"{PATTERN} new\n", encoding="utf-8")
    options = ["--shared", "--shared-storage", storage, "--no-watch"]
    marker = root / ".git/tgrep-daemon-v1.json"

    def attach(child, lease):
        wait_for(child, lambda _: marker_owned(marker, child), "shared registration")
        attached = json.loads(cli(
            linked, "shared", "attach", ".", "--revision", revision, "--lease", lease,
        ).stdout)
        require(attached["lease"] == lease, f"wrong attachment: {attached}")
        wait_for(child, lambda remaining: json.loads(cli(
            linked, "status", ".", timeout=remaining,
        ).stdout)["ready"] is True, "shared readiness")

    with server(binary, root, options, env, scratch / "shared.log") as child:
        attach(child, "qualification-first")
        parity(linked, "(via shared daemon v1)")
        parity(linked, "(via shared daemon v1)", hidden=True)
        (linked / "refreshed.txt").write_text(f"{PATTERN} refreshed\n", encoding="utf-8")
        refreshed = json.loads(cli(
            linked, "shared", "refresh", ".", "--lease", "qualification-first",
            "--changed", "refreshed.txt",
        ).stdout)
        require(refreshed["ready"] is True and isinstance(refreshed["processed_epoch"], int),
                f"refresh did not acknowledge ready epoch: {refreshed}")
        parity(linked, "(via shared daemon v1)")
        cli(linked, "--", "[", ".", expected=2)
    (linked / "late.txt").write_text(f"{PATTERN} after daemon stop\n", encoding="utf-8")
    parity(linked, "scanning filesystem (not a legacy index)")
    parity(linked, "scanning filesystem (not a legacy index)", hidden=True)
    print("Installed binary: shared attach/refresh and stale-daemon scan fallback", flush=True)

    with server(binary, root, options, env, scratch / "restarted.log") as child:
        attach(child, "qualification-restarted")
        parity(linked, "(via shared daemon v1)")
        detached = json.loads(cli(
            linked, "shared", "detach", ".", "--lease", "qualification-restarted",
        ).stdout)
        require(detached["detached"] is True and detached["remaining_leases"] == 0
                and detached["registration_warning"] is None, f"detach failed: {detached}")
        git("worktree", "remove", "--force", str(linked))
        require(not linked.exists(), "detached worktree was not removed")
        require(child.poll() is None, "detach must not stop the repository daemon")
    print("Installed binary: restart, detach and worktree deletion while daemon lives", flush=True)


@contextmanager
def private_directory():
    temporary = tempfile.TemporaryDirectory(prefix="tgrep-qualification-")
    try:
        yield Path(temporary.name).resolve()
    finally:
        deadline = time.monotonic() + 5
        while True:
            try:
                temporary.cleanup()
                break
            except PermissionError as error:
                if os.name != "nt" or time.monotonic() >= deadline:
                    raise
                # Windows can retain an executable image briefly after process exit.
                print(f"Retrying private fixture cleanup: {error}", flush=True)
                time.sleep(0.05)


def installed(checkout):
    with private_directory() as scratch:
        install_root = scratch / "install"
        print("Installing checkout into private root (release profile)", flush=True)
        result = run(
            ["cargo", "install", "--path", checkout / "tgrep-cli", "--locked",
             "--root", install_root],
            cwd=checkout, timeout=1200,
        )
        print(result.stderr, flush=True)
        binary = install_root / "bin" / ("tgrep.exe" if os.name == "nt" else "tgrep")
        print(f"Qualifying installed executable: {binary}", flush=True)
        smoke(binary, scratch)


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("mode", choices=("dependencies", "installed"))
    args = parser.parse_args()
    with unchanged_locks(CHECKOUT):
        {"dependencies": dependencies, "installed": installed}[args.mode](CHECKOUT)
    print("Qualification passed; all three lockfiles unchanged.", flush=True)


if __name__ == "__main__":
    def interrupted(signum, _frame):
        raise RuntimeError(f"qualification interrupted by signal {signum}")

    signal.signal(signal.SIGTERM, interrupted)
    main()
