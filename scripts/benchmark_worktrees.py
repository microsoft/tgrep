#!/usr/bin/env python3
"""Large-repository multi-worktree benchmark for managed v2 shared indexes.

Clones (or reuses) a large repository such as chromium/chromium, adds several
linked Git worktrees and measures:

* the ordinary base index build (`tgrep index`) and its server's readiness;
* the managed v2 base attachment: a cold committed-tree generation build plus
  full verification of the primary checkout;
* `git worktree add` and attach-to-ready time for each new linked worktree,
  which should reuse the published base generation;
* per-query latency on the ordinary base index, the managed primary (base)
  view and every worktree view, with backend and result-parity checks.

Example (see SHARED_INDEX_BENCHMARKS.md#multi-worktree-large-repository-benchmark):

  cargo build --release -p tgrep-cli
  python -B scripts/benchmark_worktrees.py --bench-dir /tmp/tgrep-wt-bench --worktrees 3

Exit status: 0 when every check passed, 1 when a parity, backend, reuse or
private-overlay check failed, and 2 on a harness or command error.
"""

import argparse
from contextlib import ExitStack
import datetime
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import time

import benchmark_shared as owned


SCHEMA = "tgrep.worktree-benchmark.v1"
SCRIPTS = Path(__file__).resolve().parent
CHECKOUT = SCRIPTS.parent
DEFAULT_QUERIES = SCRIPTS / "benchmark-queries-chromium.json"
EXE = ".exe" if os.name == "nt" else ""
KIB, MIB, GIB = 1 << 10, 1 << 20, 1 << 30
PROFILE = {
    "content": "raw-git-blob-auto-v1",
    "coverage": "tracked-regular-files-v1",
    "max_blob_bytes": 64 * MIB,
}
ORDINARY_BACKEND = "server"
MANAGED_BACKEND = "shared daemon v2"
MARKER = "tgrep_worktree_bench_marker"
UNTRACKED = "tgrep_worktree_bench_untracked.txt"
DIRTY_SUFFIXES = (".cc", ".h", ".c", ".cpp", ".rs", ".go", ".py", ".txt", ".md")
SUMMARY = re.compile(r"^(\d+) matches \((\d+) matched lines\) in ([\d.]+)ms \(via ([^)]+)\)")
LOCAL = re.compile(r"^Search completed in ([\d.]+)ms: (\d+) matches \((\d+) matched lines\)")
PLAN = re.compile(r"\(candidates: (\d+)/(\d+); raw candidates: (\d+)/(\d+)\)\s*$")
PHASES = (
    ("queue_us", "Queue"),
    ("base_materialization_us", "Base materialization"),
    ("discovery_us", "Discovery"),
    ("content_verification_us", "Content verification"),
    ("assembly_us", "Assembly"),
    ("checkpoint_us", "Checkpoint"),
    ("namespace_catalog_us", "Namespace catalog"),
    ("admitted_us", "Admitted lifetime"),
)
RECONCILE_COUNTERS = (
    "full", "files_discovered", "files_read", "bytes_read", "files_extracted",
    "base_reused", "overlay_reused", "content_reads_avoided", "peak_workers",
    "peak_private_bytes",
)


class BenchError(RuntimeError):
    """A harness, command or protocol failure (exit status 2)."""


def log(message):
    stamp = datetime.datetime.now().strftime("%H:%M:%S")
    print(f"[{stamp}] {message}", file=sys.stderr, flush=True)


def decode(data):
    return data.decode("utf-8", errors="replace") if isinstance(data, bytes) else (data or "")


def percentile(ordered, fraction):
    return ordered[max(0, math.ceil(len(ordered) * fraction) - 1)]


def summarize(samples):
    """Nearest-rank summary; p50 of three samples is their median."""
    ordered = sorted(samples)
    if not ordered:
        return {"count": 0, "min": None, "p50": None, "p95": None, "mean": None, "max": None}
    return {
        "count": len(ordered), "min": ordered[0], "p50": percentile(ordered, .5),
        "p95": percentile(ordered, .95), "mean": sum(ordered) / len(ordered),
        "max": ordered[-1],
    }


def parse_stats(stderr):
    """Parse `--stats` diagnostics: backend, counts, server time and narrowing."""
    stats = {"backend": None, "matches": None, "matched_lines": None, "server_ms": None,
             "candidates": None, "total_files": None}
    for line in decode(stderr).splitlines():
        line = line.strip()
        if match := SUMMARY.match(line):
            stats.update(matches=int(match[1]), matched_lines=int(match[2]),
                         server_ms=float(match[3]), backend=match[4])
        elif match := LOCAL.match(line):
            stats.update(server_ms=float(match[1]), matches=int(match[2]),
                         matched_lines=int(match[3]), backend="local index")
        elif line.startswith("Query plan: ") and (match := PLAN.search(line)):
            stats.update(candidates=int(match[1]), total_files=int(match[2]))
    return stats


def output_digest(stdout):
    """Order-insensitive digest of rendered rows, so parallel output order is irrelevant."""
    rows = sorted(line.rstrip(b"\r") for line in stdout.split(b"\n") if line)
    return hashlib.sha256(b"\n".join(rows)).hexdigest()


def minimum_private_bytes(work):
    # Mirrors tgrep-core WorkLimits::minimum_private_bytes.
    return 4 * work["sort_buffer_bytes"] + 6 * work["blob_bytes"] + 256 * KIB


def build_policy(args):
    policy = {
        "schema": 2, "storage": "managed",
        "retention": {"mode": "retain-all"}, "advancement": {"mode": "fixed"},
        "work": {
            "max_views": max(8, args.worktrees + 2), "max_leases": 256,
            "workers": args.workers, "queue_items": 64,
            "staging_bytes": args.staging_bytes, "private_work_bytes": args.private_work_bytes,
            "sort_buffer_bytes": args.sort_buffer_bytes,
            "blob_bytes": PROFILE["max_blob_bytes"] + 1,
            "operation_timeout_ms": int(args.timeout * 1000),
            "page_objects": 64, "max_cursors": 16, "cursor_lifetime_ms": 30000,
            "max_receipts": 4096, "metadata_bytes": args.metadata_bytes,
        },
        "collection": {
            "schedule": {"mode": "disabled"}, "on_pressure": False,
            "checkpoint_grace_ms": 0, "generation_grace_ms": 0,
            "max_duration_ms": 1000, "max_examined": 64, "max_removed": 16,
            "max_delete_bytes": MIB, "chunk_bytes": 64 * KIB, "max_pages": 4, "retry_ms": 100,
        },
    }
    validate_policy(policy)
    return policy


def validate_policy(policy):
    """Reject budgets the daemon would refuse, before any expensive work starts."""
    work = policy["work"]
    if not 1 <= work["workers"] <= 64:
        raise BenchError("--workers must be in 1..=64")
    if not 1 <= work["max_views"] <= 1024:
        raise BenchError("too many worktrees for one managed namespace (max_views <= 1024)")
    if work["sort_buffer_bytes"] < 16 * KIB:
        raise BenchError("--sort-buffer-bytes must be at least 16 KiB")
    if work["metadata_bytes"] < MIB:
        raise BenchError("--metadata-bytes must be at least 1 MiB")
    if work["staging_bytes"] < 1:
        raise BenchError("--staging-bytes must be positive")
    required = minimum_private_bytes(work) * work["workers"]
    if required > work["private_work_bytes"]:
        raise BenchError(
            f"--private-work-bytes must be at least {required} for {work['workers']} worker(s): "
            "each worker reserves 4 sort buffers, 6 blob buffers and 256 KiB")
    if work["operation_timeout_ms"] < 1:
        raise BenchError("--timeout must be positive")


def find_binary(explicit=None):
    candidates = [explicit] if explicit else [
        os.environ.get("TGREP_BIN"),
        CHECKOUT / "target" / "release" / f"tgrep{EXE}",
        shutil.which("tgrep"),
    ]
    for candidate in candidates:
        if candidate and Path(candidate).is_file():
            return Path(candidate).resolve()
    if explicit:
        raise BenchError(f"tgrep binary not found: {explicit}")
    return None


def load_queries(path, limit=None):
    data = json.loads(Path(path).read_text(encoding="utf-8"))
    queries = data["queries"] if isinstance(data, dict) else data
    if not queries or not all(isinstance(q, str) and q for q in queries):
        raise BenchError(f"{path}: expected a nonempty list of query strings")
    if limit:
        queries = queries[:limit]
    return queries, data.get("repo_url") if isinstance(data, dict) else None


def repo_name(url):
    return url.rstrip("/").rsplit("/", 1)[-1].removesuffix(".git") or "repo"


def dir_size(path):
    total = files = 0
    for directory, _, names in os.walk(path):
        for name in names:
            try:
                total += os.lstat(os.path.join(directory, name)).st_size
                files += 1
            except OSError:
                pass
    return {"bytes": total, "files": files}


def run(argv, *, cwd=None, env=None, timeout=None, allowed=(0,), stdout=subprocess.PIPE,
        stderr=subprocess.PIPE):
    argv = [str(arg) for arg in argv]
    try:
        result = subprocess.run(argv, cwd=cwd, env=env, timeout=timeout, stdin=subprocess.DEVNULL,
                                stdout=stdout, stderr=stderr)
    except subprocess.TimeoutExpired as error:
        raise BenchError(f"timed out after {timeout}s: {argv}") from error
    except FileNotFoundError as error:
        raise BenchError(f"executable unavailable: {argv[0]}") from error
    if result.returncode not in allowed:
        raise BenchError(f"{argv}: exit {result.returncode}\nstdout: {decode(result.stdout)[-4000:]}"
                         f"\nstderr: {decode(result.stderr)[-4000:]}")
    return result


class Child:
    """An owned background process tree whose output goes to a log file."""

    def __init__(self, argv, log_path, *, cwd, env, stdin=subprocess.DEVNULL):
        self.log = log_path
        self.handle = open(log_path, "wb")
        try:
            self.owned = owned.OwnedProcess([str(arg) for arg in argv], cwd=cwd, env=env,
                                            stdin=stdin, stdout=self.handle,
                                            stderr=subprocess.STDOUT)
        except BaseException:
            self.handle.close()
            raise

    @property
    def process(self):
        return self.owned.process

    def alive(self):
        return self.process.poll() is None

    def resources(self):
        try:
            sample = owned.process_resources(self.process)
        except Exception as error:  # noqa: BLE001 - resource sampling is best effort
            return {"error": str(error)}
        return {key: sample[key]["value"] for key in ("cpu_seconds", "rss_bytes", "private_bytes")}

    def stop(self):
        try:
            if self.process.stdin is not None and not self.process.stdin.closed:
                self.process.stdin.close()
            self.owned.stop()
        finally:
            self.handle.close()

    def tail(self, limit=4000):
        try:
            return self.log.read_text(encoding="utf-8", errors="replace")[-limit:]
        except OSError:
            return ""


def wait_until(probe, description, timeout, child=None, interval=.05, max_interval=1.0):
    deadline = time.monotonic() + timeout
    while True:
        if child is not None and not child.alive():
            raise BenchError(f"{description}: process exited with {child.process.returncode}\n"
                             f"{child.tail()}")
        value = probe()
        if value:
            return value
        if time.monotonic() >= deadline:
            raise BenchError(f"timed out after {timeout}s waiting for {description}")
        time.sleep(interval)
        interval = min(max_interval, interval * 1.5)


def marker_owned(path, child):
    try:
        marker = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError):
        return False
    return isinstance(marker, dict) and marker.get("pid") == child.process.pid


def owner_holding(path, claim):
    try:
        data = path.read_bytes()
    except OSError:
        return False
    line, newline, _ = data.partition(b"\n")
    if not newline:
        return False
    frame = json.loads(line)
    if frame.get("holding") is not True or frame.get("claim") != claim:
        raise BenchError(f"owner-hold did not prove the requested claim: {frame}")
    return True


class Benchmark:
    def __init__(self, args, binary):
        self.args = args
        self.binary = binary
        self.bench = args.bench_dir.resolve()
        self.logs = self.bench / "logs"
        self.env = dict(os.environ, GIT_TERMINAL_PROMPT="0")
        self.run_id = f"wtb{os.getpid()}x{int(time.time())}"
        self.stack = ExitStack()
        self.failures = []
        self.query_results = None
        self.repo = None
        self.base_commit = None
        self.ordinary_index = self.bench / "ordinary-index"
        self.storage = self.bench / "managed-storage"
        self.worktree_dir = self.bench / "worktrees"
        self.worktrees = []
        self.daemon = None
        self.holder = None
        self.claim = None
        self.leases = []
        self.sequence = 0

    # -- helpers -----------------------------------------------------------

    def git(self, *args, cwd=None, timeout=None, allowed=(0,)):
        return run(["git", *args], cwd=cwd or self.repo, env=self.env, timeout=timeout,
                   allowed=allowed)

    def git_text(self, *args, **kwargs):
        return decode(self.git(*args, **kwargs).stdout).strip()

    def tgrep(self, *args, cwd=None, timeout=None, allowed=(0,)):
        return run([self.binary, *args], cwd=cwd or self.repo, env=self.env,
                   timeout=timeout or self.args.timeout, allowed=allowed)

    def spawn(self, name, argv, stdin=subprocess.DEVNULL):
        child = Child(argv, self.logs / f"{name}.log", cwd=self.repo, env=self.env, stdin=stdin)
        self.stack.callback(child.stop)
        return child

    def fail(self, kind, message):
        log(f"CHECK FAILED ({kind}): {message}")
        self.failures.append({"kind": kind, "message": message})

    def manage(self, method, params, timeout=120):
        result = run([self.binary, "shared", "manage", self.repo, method, "--params",
                      json.dumps(params)], cwd=self.repo, env=self.env, timeout=timeout,
                     allowed=(0, 1, 2))
        try:
            response = json.loads(result.stdout)
        except json.JSONDecodeError as error:
            raise BenchError(f"managed {method}: exit {result.returncode}, invalid JSON "
                             f"{decode(result.stdout)[-2000:]!r}\n{decode(result.stderr)[-2000:]}") from error
        if response.get("ok") is not True:
            raise BenchError(f"managed {method} failed: {response}")
        return response["result"]

    # -- setup -------------------------------------------------------------

    def prepare_repository(self, repo_url):
        repo = (self.args.repo_path or self.bench / repo_name(repo_url)).resolve()
        cloned = None
        if not (repo / ".git").exists():
            if repo.exists() and any(repo.iterdir()):
                raise BenchError(f"{repo} exists but is not a Git checkout")
            if self.args.no_clone:
                raise BenchError(f"{repo} is missing and --no-clone was given")
            argv = ["git", "clone", "--single-branch", "--no-tags",
                    "-c", "core.autocrlf=false", "-c", "core.longpaths=true"]
            if self.args.depth:
                argv += ["--depth", str(self.args.depth)]
            if self.args.branch:
                argv += ["--branch", self.args.branch]
            log(f"Cloning {repo_url} into {repo} (this can take a long time)")
            start = time.perf_counter()
            run([*argv, repo_url, repo], env=self.env, stdout=None, stderr=None)
            cloned = time.perf_counter() - start
        if not (repo / ".git").is_dir():
            raise BenchError(f"{repo} must be a primary checkout with a .git directory, "
                             "not a linked worktree")
        self.repo = repo
        self.base_commit = self.git_text("rev-parse", "HEAD")
        tracked = self.git("ls-files", "-z", timeout=self.args.timeout).stdout.count(b"\0")
        autocrlf = self.git_text("config", "--get", "core.autocrlf", allowed=(0, 1)) or "unset"
        if autocrlf.lower() in ("true", "input") and os.name == "nt":
            log("warning: core.autocrlf is enabled; CRLF checkouts force full private overlays")
        return {
            "name": repo.name, "path": str(repo), "url": repo_url,
            "commit": self.base_commit, "tracked_files": tracked, "autocrlf": autocrlf,
            "clone_seconds": cloned,
        }

    def resolve_revisions(self):
        revisions = [self.git_text("rev-parse", "--verify", f"{rev}^{{commit}}")
                     for rev in self.args.worktree_rev]
        if len(revisions) > self.args.worktrees:
            raise BenchError("more --worktree-rev values than --worktrees")
        return revisions + [self.base_commit] * (self.args.worktrees - len(revisions))

    def remove_worktree(self, path):
        if path.exists():
            self.git("worktree", "remove", "--force", path, allowed=(0, 128),
                     timeout=self.args.timeout)
            if path.exists():
                owned.remove_fixture(path)
        self.git("worktree", "prune")

    # -- ordinary baseline -------------------------------------------------

    def ordinary_phase(self):
        if self.ordinary_index.exists():
            owned.remove_fixture(self.ordinary_index)
        log("Ordinary: building base index with `tgrep index`")
        with open(self.logs / "ordinary-index.log", "wb") as output:
            start = time.perf_counter()
            run([self.binary, "index", self.repo, "--index-path", self.ordinary_index],
                cwd=self.repo, env=self.env, timeout=self.args.timeout,
                stdout=output, stderr=subprocess.STDOUT)
            index_seconds = time.perf_counter() - start
        log(f"Ordinary: index built in {index_seconds:.1f}s")
        server = self.spawn("ordinary-serve", [
            self.binary, "serve", self.repo, "--index-path", self.ordinary_index, "--no-watch"])
        start = time.perf_counter()

        def ready():
            status = decode(self.tgrep("status", self.repo, "--index-path", self.ordinary_index,
                                       allowed=(0, 1, 2), timeout=60).stdout)
            return ("Server status for" in status and "Indexing:   complete" in status
                    and "Hidden coverage: complete" in status)

        wait_until(ready, "ordinary server readiness", self.args.timeout, server)
        load_seconds = time.perf_counter() - start
        log(f"Ordinary: server ready in {load_seconds:.1f}s")
        return {"index_seconds": index_seconds, "server_ready_seconds": load_seconds,
                "index_size": dir_size(self.ordinary_index), "server": server.resources()}

    # -- managed v2 --------------------------------------------------------

    def token(self):
        self.sequence += 1
        return {"scope": self.claim["owner"], "sequence": self.sequence,
                "token": f"{self.run_id}-{self.sequence}"}

    def attach(self, root, lease, revision):
        request = {"token": self.token(), "request": {
            "root": str(root), "revision": revision, "profile": PROFILE, "lease": lease,
            "owner": self.claim["owner"], "accept_current": None, "migratable": True,
            "allocation_version": 1,
        }}
        before = self.daemon.resources()
        start = time.perf_counter()
        accepted = self.manage("views.attach", request)
        self.leases.append(lease)
        record = None

        def completed():
            nonlocal record
            record = self.manage("operations.inspect", {"id": accepted["id"]})
            if record["state"] in ("failed", "cancelled") or record.get("error") is not None:
                raise BenchError(f"managed attach of {root} did not succeed: {record}")
            return record["state"] == "completed"

        wait_until(completed, f"attach operation for {root}", self.args.timeout, self.daemon)
        receipt_seconds = time.perf_counter() - start
        view = record["result"]["current"]["id"]
        status = None

        def ready():
            nonlocal status
            status = self.manage("views.status", {"id": view})
            return status["ready"] is True and status["work"] is None

        wait_until(ready, f"managed view {view} readiness", self.args.timeout, self.daemon)
        ready_seconds = time.perf_counter() - start
        after = self.daemon.resources()
        reconcile = record["result"].get("reconcile") or {}
        cpu = None
        if isinstance(before.get("cpu_seconds"), (int, float)) and isinstance(
                after.get("cpu_seconds"), (int, float)):
            cpu = after["cpu_seconds"] - before["cpu_seconds"]
        return {
            "root": str(root), "lease": lease, "revision": revision, "view": view,
            "ready_seconds": ready_seconds, "receipt_seconds": receipt_seconds,
            "incarnation": record["result"]["current"]["current"].get("incarnation"),
            "build": record["result"].get("build"),
            "reconcile": {key: reconcile.get(key) for key in RECONCILE_COUNTERS},
            "timings_us": reconcile.get("timings", {}),
            "overlay": status.get("overlay"),
            "daemon_cpu_seconds": cpu, "daemon_after": after,
        }

    def managed_phase(self):
        if self.storage.exists():
            owned.remove_fixture(self.storage)
        self.storage.mkdir(parents=True)
        policy = build_policy(self.args)
        policy_path = self.bench / "managed-policy.json"
        policy_path.write_text(json.dumps(policy, indent=2), encoding="utf-8")
        marker = self.repo / ".git" / "tgrep-daemon-v2.json"
        log("Managed: starting repository daemon")
        start = time.perf_counter()
        self.daemon = self.spawn("managed-daemon", [
            self.binary, "serve", self.repo, "--shared", "--shared-storage", self.storage,
            "--shared-policy", policy_path, "--no-watch"])
        wait_until(lambda: marker_owned(marker, self.daemon), "managed daemon registration",
                   120, self.daemon)
        daemon_seconds = time.perf_counter() - start
        hello = self.manage("hello", {})
        if hello.get("storage") != "managed":
            raise BenchError(f"daemon did not select managed storage: {hello}")
        self.claim = self.manage("owners.prepare", {"token": f"{self.run_id}-owner"})["claim"]
        claim_path = self.bench / "owner-claim.json"
        claim_path.write_text(json.dumps(self.claim), encoding="utf-8")
        self.holder = self.spawn("owner-hold", [
            self.binary, "shared", "owner-hold", "--claim", claim_path], stdin=subprocess.PIPE)
        wait_until(lambda: owner_holding(self.holder.log, self.claim), "owner-hold proof", 60,
                   self.holder)
        self.manage("owners.register", {"claim": self.claim})

        log("Managed: attaching primary checkout (cold base generation build + verification)")
        base = self.attach(self.repo, f"{self.run_id}-base", self.base_commit)
        log(f"Managed: base view ready in {base['ready_seconds']:.1f}s")
        base_storage = dir_size(self.storage)

        revisions = self.resolve_revisions()
        self.worktree_dir.mkdir(parents=True, exist_ok=True)
        for number, revision in enumerate(revisions, 1):
            path = self.worktree_dir / f"wt-{number}"
            if path.exists():
                log(f"Removing stale worktree {path}")
            self.remove_worktree(path)
            log(f"Worktree {number}: git worktree add at {revision[:12]}")
            start = time.perf_counter()
            with open(self.logs / f"worktree-add-{number}.log", "wb") as output:
                run(["git", "worktree", "add", "--detach", path, revision], cwd=self.repo,
                    env=self.env, timeout=self.args.timeout, stdout=output,
                    stderr=subprocess.STDOUT)
            checkout_seconds = time.perf_counter() - start
            entry = {"number": number, "path": path, "revision": revision,
                     "checkout_seconds": checkout_seconds, "dirty": None}
            self.worktrees.append(entry)
            if number == len(revisions) and self.args.dirty_files:
                entry["dirty"] = self.make_dirty(path, self.args.dirty_files)
            log(f"Worktree {number}: checkout {checkout_seconds:.1f}s; attaching")
            entry["attach"] = self.attach(path, f"{self.run_id}-wt-{number}", revision)
            log(f"Worktree {number}: view ready in {entry['attach']['ready_seconds']:.1f}s")
            build = entry["attach"]["build"] or {}
            if revision == self.base_commit:
                entry["reuse_ok"] = (build.get("reused_generation") is True
                                     and build.get("published") is False
                                     and build.get("blobs_read") == 0
                                     and entry["attach"]["incarnation"] == base["incarnation"])
                if not entry["reuse_ok"]:
                    self.fail("reuse", f"worktree {number} did not reuse the base generation: {build}")
        return {
            "daemon_start_seconds": daemon_seconds, "policy": policy,
            "capabilities": hello.get("capabilities"), "base": base,
            "storage_after_base": base_storage, "storage_after_worktrees": dir_size(self.storage),
        }

    def make_dirty(self, path, count):
        # Default queries skip hidden paths, so only dirty files they can see.
        names = [name for name in decode(self.git("ls-files", "-z", cwd=path).stdout).split("\0")
                 if name and not any(part.startswith(".") for part in name.split("/"))]
        text = [name for name in names if name.endswith(DIRTY_SUFFIXES)] or names
        regular = []
        step = max(1, len(text) // (count * 4))
        for name in text[::step]:
            target = path / name
            if not target.is_symlink() and target.is_file():
                regular.append(name)
                if len(regular) == count:
                    break
        for index, name in enumerate(regular):
            with open(path / name, "ab") as handle:
                handle.write(f"\n// {MARKER} {index}\n".encode())
        ignored = self.git("check-ignore", "-q", UNTRACKED, cwd=path, allowed=(0, 1)).returncode == 0
        if not ignored:
            (path / UNTRACKED).write_text(f"{MARKER} untracked\n", encoding="utf-8")
        log(f"Dirtied {len(regular)} tracked file(s){'' if ignored else ' and 1 untracked file'}")
        return {"modified": regular, "untracked": None if ignored else UNTRACKED,
                "expected_matches": len(regular) + (0 if ignored else 1)}

    # -- queries -----------------------------------------------------------

    def targets(self, ordinary):
        targets = []
        if ordinary:
            targets.append({"label": "ordinary-base", "root": self.repo, "backend": ORDINARY_BACKEND,
                            "args": ["--index-path", str(self.ordinary_index)], "clean": True})
        targets.append({"label": "managed-base", "root": self.repo, "backend": MANAGED_BACKEND,
                        "args": [], "clean": True})
        for entry in self.worktrees:
            targets.append({"label": f"worktree-{entry['number']}", "root": entry["path"],
                            "backend": MANAGED_BACKEND, "args": [],
                            "clean": entry["dirty"] is None
                            and entry["revision"] == self.base_commit})
        return targets

    def search(self, target, pattern, capture=False, extra=()):
        argv = [str(self.binary), *target["args"], "--stats", "--color", "never", "-n",
                *self.args.search_arg, *extra, "--", pattern, "."]
        start = time.perf_counter()
        try:
            result = subprocess.run(argv, cwd=target["root"], env=self.env,
                                    stdin=subprocess.DEVNULL, stderr=subprocess.PIPE,
                                    stdout=subprocess.PIPE if capture else subprocess.DEVNULL,
                                    timeout=self.args.query_timeout)
        except subprocess.TimeoutExpired as error:
            raise BenchError(f"{target['label']}: query {pattern!r} timed out") from error
        wall_ms = (time.perf_counter() - start) * 1000
        if result.returncode not in (0, 1):
            raise BenchError(f"{target['label']}: query {pattern!r} exited {result.returncode}: "
                             f"{decode(result.stderr)[-2000:]}")
        return wall_ms, parse_stats(result.stderr), result.stdout, decode(result.stderr)

    def query_phase(self, queries, targets):
        results = {query: {"targets": {}} for query in queries}
        log(f"Queries: warm-up and parity pass over {len(targets)} target(s)")
        for query in queries:
            for target in targets:
                _, stats, stdout, stderr = self.search(target, query, capture=True,
                                                       extra=("--sort", "path"))
                if stats["backend"] != target["backend"]:
                    raise BenchError(f"{target['label']}: expected backend {target['backend']!r} "
                                     f"for {query!r}, got stderr:\n{stderr[-2000:]}")
                results[query]["targets"][target["label"]] = {
                    "matches": stats["matches"], "matched_lines": stats["matched_lines"],
                    "candidates": stats["candidates"], "total_files": stats["total_files"],
                    "digest": output_digest(stdout), "wall_ms": [], "server_ms": [],
                }
        self.check_parity(results, targets)
        log(f"Queries: {self.args.repeat} timed round(s) of {len(queries)} queries")
        try:
            for round_number in range(self.args.repeat):
                for query in queries:
                    for target in targets:
                        record = results[query]["targets"][target["label"]]
                        try:
                            wall_ms, stats, _, stderr = self.search(target, query)
                        except BenchError as error:
                            # One slow query must not discard every other
                            # measurement; it is reported as a failure.
                            record.setdefault("errors", []).append(str(error))
                            self.fail("query", f"round {round_number}: {error}")
                            continue
                        if stats["backend"] != target["backend"]:
                            self.fail("backend", f"{target['label']} {query!r} round {round_number}: "
                                      f"{stderr[-500:]}")
                            continue
                        record["wall_ms"].append(wall_ms)
                        record["server_ms"].append(stats["server_ms"])
        finally:
            for query in queries:
                for record in results[query]["targets"].values():
                    record["wall"] = summarize(record["wall_ms"])
                    record["server"] = summarize(record["server_ms"])
            self.query_results = results
        return results

    def check_parity(self, results, targets):
        reference = targets[0]["label"]
        clean = [target["label"] for target in targets if target["clean"]]
        for query, result in results.items():
            expected = result["targets"][reference]
            mismatched = []
            for target in targets:
                actual = result["targets"][target["label"]]
                same = all(actual[key] == expected[key]
                           for key in ("matches", "matched_lines", "digest"))
                if not same:
                    mismatched.append(target["label"])
            result["parity"] = {"reference": reference, "mismatched": mismatched}
            failed = [label for label in mismatched if label in clean]
            if failed:
                self.fail("parity", f"{query!r}: {failed} differ from {reference}")

    def check_private_overlay(self, targets):
        dirty = next((entry for entry in self.worktrees if entry["dirty"]), None)
        if dirty is None:
            return None
        observed = {}
        for target in targets:
            _, stats, _, stderr = self.search(target, MARKER, extra=("-F",))
            if stats["backend"] != target["backend"]:
                self.fail("backend", f"{target['label']} marker query: {stderr[-500:]}")
            observed[target["label"]] = stats["matches"] or 0
        label = f"worktree-{dirty['number']}"
        expected = {key: (dirty["dirty"]["expected_matches"] if key == label else 0)
                    for key in observed}
        if observed != expected:
            self.fail("overlay", f"private marker matches {observed}, expected {expected}")
        return {"observed": observed, "expected": expected, "ok": observed == expected}

    # -- teardown ----------------------------------------------------------

    def teardown(self):
        cleanup = {"detached": [], "errors": []}
        if self.daemon is not None and self.daemon.alive() and self.claim is not None:
            for lease in self.leases:
                try:
                    detached = self.manage("views.detach", {"owner": self.claim["owner"],
                                                            "lease": lease})
                    cleanup["detached"].append({"lease": lease, **{
                        key: detached.get(key) for key in ("lease_released", "root_handles_released")}})
                except BenchError as error:
                    cleanup["errors"].append(str(error))
            try:
                self.manage("owners.release", {"claim": self.claim})
            except BenchError as error:
                cleanup["errors"].append(str(error))
        if self.holder is not None and self.holder.alive():
            self.holder.process.stdin.close()
            try:
                self.holder.process.wait(timeout=30)
            except subprocess.TimeoutExpired:
                cleanup["errors"].append("owner-hold did not exit after stdin closed")
        try:
            self.stack.close()
        except Exception as error:  # noqa: BLE001 - report and continue cleanup
            cleanup["errors"].append(f"stopping owned processes: {error}")
        if self.args.keep or self.repo is None:
            cleanup["kept"] = True
            return cleanup
        for entry in self.worktrees:
            try:
                log(f"Removing worktree {entry['path']}")
                self.remove_worktree(entry["path"])
            except (BenchError, OSError) as error:
                cleanup["errors"].append(f"removing {entry['path']}: {error}")
        for path in (self.storage, self.ordinary_index):
            try:
                if path.exists():
                    owned.remove_fixture(path)
            except OSError as error:
                cleanup["errors"].append(f"removing {path}: {error}")
        return cleanup


def tool_version(argv):
    try:
        return decode(run(argv, timeout=30).stdout).strip().splitlines()[0]
    except (BenchError, IndexError) as error:
        return f"unavailable: {error}"


def source_commit():
    try:
        commit = decode(run(["git", "rev-parse", "HEAD"], cwd=CHECKOUT, timeout=30).stdout).strip()
        dirty = bool(decode(run(["git", "status", "--porcelain", "--untracked-files=no"],
                                cwd=CHECKOUT, timeout=60).stdout).strip())
        return {"commit": commit, "dirty": dirty}
    except BenchError as error:
        return {"commit": None, "dirty": None, "reason": str(error)}


def jsonable(value):
    if isinstance(value, Path):
        return str(value)
    if isinstance(value, dict):
        return {key: jsonable(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [jsonable(item) for item in value]
    return value


def seconds(value):
    return "-" if value is None else f"{value:,.1f}"


def millis(value):
    return "-" if value is None else f"{value:,.1f}"


def render_markdown(report):
    repo, env, params = report["repository"], report["environment"], report["parameters"]
    managed, ordinary = report.get("managed") or {}, report.get("ordinary")
    worktrees = report.get("worktrees") or []
    lines = [
        f"# tgrep multi-worktree benchmark: {repo['name']} @ {repo['commit'][:12]}", "",
        f"- Host: {env['platform']}, {env['cpus']} logical CPUs; Python {env['python']}",
        f"- tgrep: {env['tgrep_version']} (source {env['source'].get('commit') or 'unknown'}); "
        f"Git: {env['git_version']}",
        f"- Tracked files: {repo['tracked_files']:,}; core.autocrlf: {repo['autocrlf']}; "
        f"worktrees: {len(worktrees)}; timed rounds: {params['repeat']}; "
        f"managed workers: {params['workers']}",
        "- Native watching is disabled (`--no-watch`) to isolate builds and attachments; "
        "the file cache is not flushed between phases.", "",
        "## Index and attach", "",
        "| Step | Wall (s) | Details |", "| --- | ---: | --- |",
    ]
    if ordinary:
        size = ordinary["index_size"]["bytes"] / GIB
        lines.append(f"| Ordinary base index (`tgrep index`) | {seconds(ordinary['index_seconds'])} "
                     f"| {size:.2f} GiB index |")
        lines.append(f"| Ordinary server load to ready | {seconds(ordinary['server_ready_seconds'])} | |")
    if managed:
        lines.append(f"| Managed daemon start | {seconds(managed['daemon_start_seconds'])} | |")
        base = managed["base"]
        build, rec = base["build"] or {}, base["reconcile"]
        lines.append(
            f"| Managed base attach (cold generation build + primary verification) "
            f"| {seconds(base['ready_seconds'])} | published={build.get('published')}, "
            f"blobs_read={build.get('blobs_read', 0):,}, files_read={rec.get('files_read') or 0:,}, "
            f"storage {managed['storage_after_base']['bytes'] / GIB:.2f} GiB |")
    for entry in worktrees:
        attach = entry["attach"]
        build, rec = attach["build"] or {}, attach["reconcile"]
        dirty = f", dirty files={len(entry['dirty']['modified'])}" if entry.get("dirty") else ""
        lines.append(f"| Worktree {entry['number']}: `git worktree add` "
                     f"| {seconds(entry['checkout_seconds'])} | revision {entry['revision'][:12]} |")
        lines.append(
            f"| Worktree {entry['number']}: attach to ready | {seconds(attach['ready_seconds'])} "
            f"| reused_generation={build.get('reused_generation')}, "
            f"blobs_read={build.get('blobs_read', 0):,}, files_read={rec.get('files_read') or 0:,}, "
            f"base_reused={rec.get('base_reused') or 0:,}, "
            f"files_extracted={rec.get('files_extracted') or 0:,}{dirty} |")
    if managed:
        lines.append(f"| Managed storage after all attachments | - "
                     f"| {managed['storage_after_worktrees']['bytes'] / GIB:.2f} GiB |")
    if managed:
        views = [("Base", managed["base"])] + [
            (f"Worktree {entry['number']}", entry["attach"]) for entry in worktrees]
        lines += ["", "## Managed attach phases (seconds, from operation receipts)", "",
                  "| View | " + " | ".join(name for _, name in PHASES) + " |",
                  "| --- |" + " ---: |" * len(PHASES)]
        for name, attach in views:
            timings = attach["timings_us"]
            lines.append(f"| {name} | " + " | ".join(
                seconds(timings[key] / 1e6) if key in timings else "-" for key, _ in PHASES) + " |")
    queries = report.get("queries") or {}
    if queries:
        labels = list(next(iter(queries.values()))["targets"])
        lines += ["", "## Query latency (ms, p50 client wall time; fresh CLI process per query)", "",
                  "| Query | " + " | ".join(labels) + " | Matches |",
                  "| --- |" + " ---: |" * (len(labels) + 1)]
        totals = {label: 0.0 for label in labels}
        server_totals = {label: 0.0 for label in labels}
        for query, result in queries.items():
            cells = []
            for label in labels:
                record = result["targets"][label]
                errors = len(record.get("errors", ()))
                cells.append(millis(record["wall"]["p50"]) + (f" ({errors} failed)" if errors else ""))
                totals[label] += record["wall"]["p50"] or 0
                server_totals[label] += record["server"]["p50"] or 0
            reference = result["targets"][labels[0]]["matches"]
            flag = " (mismatch)" if result["parity"]["mismatched"] else ""
            shown = query.replace("|", "\\|")
            lines.append(f"| `{shown}` | " + " | ".join(cells) + f" | {reference:,}{flag} |")
        lines.append("| **Sum of p50 (client)** | " + " | ".join(
            f"**{millis(totals[label])}**" for label in labels) + " | |")
        lines.append("| Sum of p50 (server-reported) | " + " | ".join(
            millis(server_totals[label]) for label in labels) + " | |")
    checks = report["checks"]
    lines += ["", "## Checks", ""]
    lines.append(f"- Result parity across clean targets: {'ok' if checks['parity_ok'] else 'FAILED'}")
    lines.append(f"- Expected backends on every run: {'ok' if checks['backend_ok'] else 'FAILED'}")
    lines.append(f"- Every timed query completed: {'ok' if checks.get('query_ok', True) else 'FAILED'}")
    lines.append(f"- Same-commit worktrees reused the base generation: "
                 f"{'ok' if checks['reuse_ok'] else 'FAILED'}")
    overlay = report.get("private_overlay")
    if overlay is not None:
        lines.append(f"- Private overlay isolated to the dirty worktree: "
                     f"{'ok' if overlay['ok'] else 'FAILED'} ({overlay['observed']})")
    for failure in report["failures"]:
        lines.append(f"- FAILED {failure['kind']}: {failure['message']}")
    return "\n".join(lines) + "\n"


def parse_args(argv=None):
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--queries", type=Path, default=DEFAULT_QUERIES,
                        help="query JSON ({repo_url, queries}); patterns are regexes")
    parser.add_argument("--query-limit", type=int, default=None, help="use only the first N queries")
    parser.add_argument("--repo-url", help="repository to clone (default: repo_url from --queries)")
    parser.add_argument("--repo-path", type=Path,
                        help="existing primary checkout, or where to clone (default: BENCH_DIR/NAME)")
    parser.add_argument("--no-clone", action="store_true", help="fail instead of cloning")
    parser.add_argument("--depth", type=int, default=2,
                        help="shallow clone depth (0 for full history; default 2)")
    parser.add_argument("--branch", help="branch to clone (default: remote HEAD)")
    parser.add_argument("--bench-dir", type=Path, default=Path(os.environ.get(
        "TEMP" if os.name == "nt" else "TMPDIR", "/tmp")) / "tgrep-worktree-bench")
    parser.add_argument("--tgrep-bin", type=Path,
                        help="tgrep binary (default: $TGREP_BIN, target/release, then PATH)")
    parser.add_argument("--worktrees", type=int, default=3, help="linked worktrees to add")
    parser.add_argument("--worktree-rev", action="append", default=[],
                        help="revision for the next worktree (repeatable; default base HEAD)")
    parser.add_argument("--dirty-files", type=int, default=0,
                        help="append a marker to N tracked files (plus one untracked file) in "
                             "the last worktree before attaching it")
    parser.add_argument("--repeat", type=int, default=3, help="timed rounds per query and target")
    parser.add_argument("--search-arg", action="append", default=[],
                        help="extra flag for every search, e.g. --search-arg=-F (repeatable)")
    parser.add_argument("--skip-ordinary", action="store_true",
                        help="skip the ordinary `tgrep index` / `tgrep serve` baseline")
    parser.add_argument("--keep", action="store_true",
                        help="keep worktrees, managed storage and the ordinary index")
    parser.add_argument("--timeout", type=float, default=4 * 3600,
                        help="seconds allowed for each build/attach/checkout (default 4h)")
    parser.add_argument("--query-timeout", type=float, default=600)
    parser.add_argument("--workers", type=int, default=1, help="managed namespace operation workers")
    parser.add_argument("--private-work-bytes", type=int, default=8 * GIB)
    parser.add_argument("--staging-bytes", type=int, default=8 * GIB)
    parser.add_argument("--metadata-bytes", type=int, default=4 * GIB)
    parser.add_argument("--sort-buffer-bytes", type=int, default=16 * MIB)
    parser.add_argument("--results-json", type=Path,
                        help="report path (default: BENCH_DIR/results/worktree-benchmark-TIME.json)")
    parser.add_argument("--results-md", type=Path, help="Markdown summary path (default: next to JSON)")
    args = parser.parse_args(argv)
    if args.worktrees < 1:
        parser.error("--worktrees must be at least 1")
    if args.repeat < 1:
        parser.error("--repeat must be at least 1")
    if args.dirty_files < 0 or args.depth < 0:
        parser.error("--dirty-files and --depth must be nonnegative")
    if args.query_limit is not None and args.query_limit < 1:
        parser.error("--query-limit must be positive")
    if len(args.worktree_rev) > args.worktrees:
        parser.error("more --worktree-rev values than --worktrees")
    return args


def main(argv=None):
    args = parse_args(argv)
    try:
        build_policy(args)
        queries, query_repo = load_queries(args.queries, args.query_limit)
        repo_url = args.repo_url or query_repo
        if not repo_url and not args.repo_path:
            raise BenchError("no repository: pass --repo-url or --repo-path")
        binary = find_binary(args.tgrep_bin)
        if binary is None:
            raise BenchError("tgrep binary not found; build with `cargo build --release -p tgrep-cli` "
                             "or pass --tgrep-bin")
    except BenchError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    bench = Benchmark(args, binary)
    bench.bench.mkdir(parents=True, exist_ok=True)
    bench.logs.mkdir(parents=True, exist_ok=True)
    started = datetime.datetime.now(datetime.timezone.utc)
    report = {
        "schema": SCHEMA, "started": started.isoformat(),
        "parameters": jsonable({key: value for key, value in vars(args).items()}),
        "environment": {
            "platform": platform.platform(), "machine": platform.machine(),
            "cpus": os.cpu_count(), "python": platform.python_version(),
            "tgrep": str(binary), "tgrep_version": tool_version([binary, "--version"]),
            "git_version": tool_version(["git", "--version"]), "source": source_commit(),
        },
        "repository": None, "ordinary": None, "managed": None, "worktrees": [],
        "queries": None, "private_overlay": None, "failures": bench.failures,
    }
    error = None
    try:
        report["repository"] = bench.prepare_repository(repo_url or str(args.repo_path))
        if not args.skip_ordinary:
            report["ordinary"] = bench.ordinary_phase()
        report["managed"] = bench.managed_phase()
        targets = bench.targets(not args.skip_ordinary)
        bench.query_phase(queries, targets)
        report["private_overlay"] = bench.check_private_overlay(targets)
    except (BenchError, OSError, KeyError, ValueError) as caught:
        error = caught
        log(f"error: {caught}")
    except KeyboardInterrupt:
        error = BenchError("interrupted")
        log("interrupted; cleaning up")
    finally:
        # Completed rounds are kept even when a later query aborts the run.
        report["queries"] = bench.query_results
        report["worktrees"] = [
            {key: value for key, value in entry.items()} for entry in bench.worktrees]
        report["cleanup"] = bench.teardown()
    report["finished"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
    report["error"] = None if error is None else str(error)
    kinds = {failure["kind"] for failure in bench.failures}
    report["checks"] = {
        "parity_ok": "parity" not in kinds, "backend_ok": "backend" not in kinds,
        "reuse_ok": "reuse" not in kinds, "overlay_ok": "overlay" not in kinds,
        "query_ok": "query" not in kinds,
        "ok": error is None and not bench.failures,
    }
    report = jsonable(report)
    stamp = started.strftime("%Y%m%dT%H%M%SZ")
    results_json = args.results_json or bench.bench / "results" / f"worktree-benchmark-{stamp}.json"
    results_json.parent.mkdir(parents=True, exist_ok=True)
    results_json.write_text(json.dumps(report, indent=2), encoding="utf-8")
    log(f"Wrote {results_json}")
    if report["repository"] is not None:
        markdown = render_markdown(report)
        results_md = args.results_md or results_json.with_suffix(".md")
        results_md.parent.mkdir(parents=True, exist_ok=True)
        results_md.write_text(markdown, encoding="utf-8")
        log(f"Wrote {results_md}")
        print(markdown)
    if error is not None:
        return 2
    return 0 if report["checks"]["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
