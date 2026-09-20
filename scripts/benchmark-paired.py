#!/usr/bin/env python3
"""Compare prebuilt tgrep binaries on one clean, pinned Git corpus.

Build both revisions separately with ``cargo build --release --locked`` before
running this script. It never builds binaries, checks out revisions, or downloads
a corpus. --work must be a new directory outside --repo-path. Linux requires GNU
sort; the bounded in-memory Windows fallback is only for small smoke fixtures.

Only compatible index formats can share the baseline-built index. Measurements
include CLI/process/IPC/output costs, not server startup or parity sorting.
"""

import argparse
import contextlib
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import random
import re
import shutil
import socket
import statistics
import subprocess
import sys
import tempfile
import time
import traceback
from collections import Counter
from datetime import datetime, timezone


INDEX_FILES = ("files.bin", "lookup.bin", "index.bin", "meta.json", "files-extra.bin")
LABELS = ("baseline", "candidate")
BLOCK_ORDERS = {
    "abba": ("baseline", "candidate", "candidate", "baseline"),
    "abba-baab": (
        "baseline", "candidate", "candidate", "baseline",
        "candidate", "baseline", "baseline", "candidate",
    ),
}
WARM_SEED = 159
REPEAT_SEED = 20260919
READ_SIZE = 1024 * 1024
SMOKE_MAX_BYTES = 16 * 1024 * 1024
SMOKE_MAX_LINES = 250000
DISCOVERY_GRACE = 5.0
RPC_MAX_BYTES = 1024 * 1024


class BenchmarkError(RuntimeError):
    pass


class DiscoveryPending(BenchmarkError):
    pass


def positive_integer(value):
    try:
        number = int(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("must be a positive integer") from error
    if number <= 0:
        raise argparse.ArgumentTypeError("must be a positive integer")
    return number


def positive_seconds(value):
    try:
        number = float(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("must be finite and greater than zero") from error
    if not math.isfinite(number) or number <= 0:
        raise argparse.ArgumentTypeError("must be finite and greater than zero")
    return number


def commit_sha(value):
    if not re.fullmatch(r"[0-9a-fA-F]{40}", value):
        raise argparse.ArgumentTypeError("must be a full 40-character hexadecimal SHA")
    return value.lower()


def argument_parser():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("baseline", "candidate", "repo-path", "queries", "work"):
        parser.add_argument("--" + name, type=Path, required=True)
    for name in ("baseline-sha", "candidate-sha", "corpus-sha"):
        parser.add_argument("--" + name, type=commit_sha, required=True)
    parser.add_argument("--repeats", type=positive_integer, default=5)
    parser.add_argument("--blocks", choices=BLOCK_ORDERS, default="abba-baab")
    parser.add_argument("--query-timeout", type=positive_seconds, default=120.0)
    parser.add_argument("--startup-timeout", type=positive_seconds, default=900.0)
    return parser


def utc_now():
    return datetime.now(timezone.utc).isoformat()


def decode(data):
    return data.decode("utf-8", errors="replace") if isinstance(data, bytes) else data


def run_command(command, timeout, **kwargs):
    try:
        return subprocess.run(
            [str(part) for part in command], stdin=subprocess.DEVNULL,
            timeout=timeout, check=False, **kwargs,
        )
    except subprocess.TimeoutExpired as error:
        raise BenchmarkError(
            f"Command timed out after {timeout}s: {command!r}; "
            f"stderr={decode(error.stderr)!r}"
        ) from error


def require_success(result, context):
    if result.returncode != 0:
        raise BenchmarkError(
            f"{context}: exit code {result.returncode}; "
            f"stderr={decode(result.stderr)!r}"
        )


def hash_file(path):
    digest = hashlib.sha256()
    size = 0
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(READ_SIZE), b""):
            digest.update(chunk)
            size += len(chunk)
    return {"sha256": digest.hexdigest(), "bytes": size}


def index_manifest(index):
    manifest = {}
    for name in INDEX_FILES:
        path = index / name
        if not path.is_file():
            raise BenchmarkError(f"Required index file missing: {path}")
        manifest[name] = hash_file(path)
    return manifest


def require_manifest(expected, actual):
    changed = [name for name in INDEX_FILES if expected.get(name) != actual.get(name)]
    if changed:
        raise BenchmarkError("Shared index changed: " + ", ".join(changed))


def create_work_directory(work, corpus):
    if work.exists() or work.is_symlink():
        raise BenchmarkError(f"--work must be a new directory: {work}")
    work = work.resolve()
    corpus = corpus.resolve(strict=True)
    if not corpus.is_dir():
        raise BenchmarkError(f"Corpus is not a directory: {corpus}")
    if work == corpus or corpus in work.parents:
        raise BenchmarkError("--work (including index and temporary files) must be outside corpus")
    work.mkdir(parents=True, exist_ok=False)
    (work / "logs").mkdir()
    (work / "tmp").mkdir()
    return work, corpus


def corpus_snapshot(corpus, temp_dir, timeout):
    prefix = ["git", "--no-optional-locks", "-C", str(corpus)]

    def git(arguments, stdout=subprocess.PIPE):
        result = run_command(prefix + arguments, timeout, stdout=stdout, stderr=subprocess.PIPE)
        require_success(result, f"git {arguments!r}")
        if result.stderr:
            raise BenchmarkError(f"git {arguments!r} wrote stderr: {decode(result.stderr)}")
        return result.stdout

    top = Path(os.fsdecode(git(["rev-parse", "--show-toplevel"]).rstrip(b"\r\n"))).resolve()
    revisions = git(["rev-parse", "HEAD", "HEAD^{tree}"]).decode("ascii").splitlines()
    if len(revisions) != 2:
        raise BenchmarkError(f"Unexpected Git revision response: {revisions!r}")
    status = git(["status", "--porcelain=v1", "-z", "--untracked-files=all",
                  "--ignore-submodules=none"])
    with tempfile.TemporaryFile(dir=temp_dir) as listing:
        git(["ls-files", "-z"], stdout=listing)
        listing.seek(0)
        count = sum(chunk.count(b"\0") for chunk in iter(lambda: listing.read(READ_SIZE), b""))
    return {
        "path": str(corpus), "git_root": str(top), "commit": revisions[0],
        "tree": revisions[1], "status_porcelain_v1_z": decode(status),
        "clean": not status, "tracked_file_count": count,
    }


def require_corpus(snapshot, corpus, sha, initial=None):
    if Path(snapshot["git_root"]) != corpus:
        raise BenchmarkError("--repo-path must be the Git working tree root")
    if snapshot["commit"] != sha:
        raise BenchmarkError(f"Corpus HEAD {snapshot['commit']} does not match {sha}")
    if not snapshot["clean"]:
        raise BenchmarkError(f"Corpus is dirty: {snapshot['status_porcelain_v1_z']!r}")
    if initial is not None and snapshot != initial:
        raise BenchmarkError("Corpus metadata changed during the benchmark")


def choose_sort_backend(timeout):
    if os.name == "nt":
        return {"kind": "python", "max_bytes": SMOKE_MAX_BYTES, "max_lines": SMOKE_MAX_LINES}
    executable = shutil.which("sort")
    if executable is None:
        raise BenchmarkError("GNU sort is required on non-Windows hosts; no tools are installed")
    result = run_command([executable, "--version"], timeout,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    require_success(result, "sort --version")
    if result.stderr or b"GNU coreutils" not in result.stdout:
        raise BenchmarkError("A GNU coreutils sort is required for bounded external sorting")
    return {"kind": "gnu", "executable": executable,
            "version": decode(result.stdout).splitlines()[0],
            "buffer_size": "128M", "parallel": 1, "locale": "C"}


def fingerprint_output(path, temp_dir, backend, timeout):
    """Hash sorted LF-delimited byte records, retaining duplicates and CR bytes.

    GNU sort adds a missing final LF; the smoke implementation does the same.
    No text decoding, set conversion, or whitespace normalization is performed.
    """
    digest = hashlib.sha256()
    if backend["kind"] == "python":
        if path.stat().st_size > SMOKE_MAX_BYTES:
            raise BenchmarkError("Output exceeds Windows smoke limit; use Linux with GNU sort")
        lines = []
        with path.open("rb") as source:
            for line in source:
                if len(lines) >= SMOKE_MAX_LINES:
                    raise BenchmarkError("Output exceeds Windows smoke line limit")
                lines.append(line[:-1] if line.endswith(b"\n") else line)
        for line in sorted(lines):
            digest.update(line)
            digest.update(b"\n")
        return {"sha256": digest.hexdigest(), "lines": len(lines)}
    if backend["kind"] != "gnu":
        raise BenchmarkError(f"Unknown fingerprint backend: {backend!r}")
    with tempfile.TemporaryDirectory(prefix="sort-", dir=temp_dir) as scratch:
        sorted_path = Path(scratch) / "sorted.out"
        command = [
            backend["executable"], "--buffer-size=128M", "--parallel=1",
            "--temporary-directory", scratch, "--output", str(sorted_path), "--", str(path),
        ]
        result = run_command(command, timeout, stdout=subprocess.DEVNULL,
                             stderr=subprocess.PIPE, env={**os.environ, "LC_ALL": "C"})
        require_success(result, "External output sort")
        if result.stderr:
            raise BenchmarkError(f"External output sort wrote stderr: {decode(result.stderr)}")
        count = 0
        with sorted_path.open("rb") as source:
            for chunk in iter(lambda: source.read(READ_SIZE), b""):
                digest.update(chunk)
                count += chunk.count(b"\n")
    return {"sha256": digest.hexdigest(), "lines": count}


def validate_query_result(returncode, stderr, expected_code=None, context="Query"):
    if returncode not in (0, 1) or stderr:
        raise BenchmarkError(
            f"{context}: invalid result: exit={returncode}, stderr={decode(stderr)!r}"
        )
    if expected_code is not None and returncode != expected_code:
        raise BenchmarkError(f"{context}: expected exit {expected_code}, got {returncode}")


def require_parity(reference, observed, context):
    if reference != observed:
        raise BenchmarkError(f"{context}: output parity mismatch: {reference!r} != {observed!r}")


def query_command(binary, corpus, index, pattern):
    return [str(binary), "--color", "never", "--index-path", str(index),
            "--", pattern, str(corpus)]


def run_query(binary, corpus, index, pattern, timeout, stdout, expected_code=None):
    command = query_command(binary, corpus, index, pattern)
    started = time.perf_counter_ns()
    result = run_command(command, timeout, stdout=stdout, stderr=subprocess.PIPE)
    elapsed = (time.perf_counter_ns() - started) / 1_000_000
    validate_query_result(result.returncode, result.stderr, expected_code, repr(pattern))
    return elapsed, result.returncode


def probe_route(binary, corpus, index, timeout):
    command = [str(binary), "--color", "never", "--index-path", str(index),
               "--files", "--stats", "--", str(corpus)]
    result = run_command(command, timeout, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    text = decode(result.stderr)
    if result.returncode not in (0, 1) or not re.fullmatch(
        r"Filename search completed in [0-9]+(?:\.[0-9]+)?ms \(via server\)\r?\n?", text
    ):
        raise BenchmarkError(
            f"Server-route probe failed or fell back: exit={result.returncode}, stderr={text!r}"
        )
    return {"command": command, "exit_code": result.returncode, "stderr": text}


def trace_pattern(line):
    prefix = "[trace] search: pattern="
    if not line.startswith(prefix):
        raise BenchmarkError(f"Malformed content-search trace: {line!r}")
    payload = line[len(prefix):]
    decoder = json.JSONDecoder()
    try:
        pattern, end = decoder.raw_decode(payload)
    except json.JSONDecodeError:
        # Rust Debug uses braced Unicode escapes, unlike JSON. Tokenizing every
        # escape prevents changing a literal backslash followed by "u{...}".
        def rust_escape(match):
            token = match.group()
            if token.startswith(r"\u{"):
                code = int(token[3:-1], 16)
                if code > 0x10FFFF or 0xD800 <= code <= 0xDFFF:
                    raise BenchmarkError(f"Invalid Rust Debug Unicode escape: {token}")
                return json.dumps(chr(code))[1:-1]
            return r"\u0000" if token == r"\0" else token

        payload = re.sub(r"\\(?:u\{[0-9a-fA-F]{1,6}\}|.)", rust_escape, payload)
        try:
            pattern, end = decoder.raw_decode(payload)
        except json.JSONDecodeError as error:
            raise BenchmarkError(f"Cannot decode content-search trace pattern: {line!r}") from error
    milliseconds = r"[0-9]+(?:\.[0-9]+)?"
    suffix = (
        r" case_insensitive=false raw_candidates=[0-9]+ candidates=[0-9]+ matches=[0-9]+ "
        rf"elapsed={milliseconds}ms \(index={milliseconds}ms resolve={milliseconds}ms "
        rf"search={milliseconds}ms\)\r?\n?"
    )
    if not isinstance(pattern, str) or not re.fullmatch(suffix, payload[end:]):
        raise BenchmarkError(f"Malformed content-search trace fields: {line!r}")
    return pattern


def content_server_check(logfile, patterns, repeats):
    expected = Counter({pattern: count * (repeats + 2)
                        for pattern, count in Counter(patterns).items()})
    observed = Counter()
    errors = []
    trace_lines = 0
    with logfile.open("r", encoding="utf-8", newline="") as log:
        for line_number, line in enumerate(log, 1):
            if not line.startswith("[trace] search:"):
                continue
            trace_lines += 1
            try:
                observed[trace_pattern(line)] += 1
            except BenchmarkError as error:
                errors.append({"line": line_number, "error": str(error)})
    return {
        "log": str(logfile), "log_fingerprint": hash_file(logfile),
        "passes_per_query": repeats + 2, "expected_count": sum(expected.values()),
        "observed_count": trace_lines, "expected_per_pattern": dict(expected),
        "observed_per_pattern": dict(observed), "missing": dict(expected - observed),
        "extra": dict(observed - expected), "parse_errors": errors,
        "match": not errors and expected == observed,
    }


def require_content_server_check(check):
    if not check["match"]:
        raise BenchmarkError(
            f"Content-server trace mismatch: expected {check['expected_count']}, "
            f"observed {check['observed_count']}; missing={check['missing']!r}, "
            f"extra={check['extra']!r}, parse_errors={check['parse_errors']!r}; see {check['log']}"
        )


def discovery_port(info, expected_pid):
    if not isinstance(info, dict) or type(info.get("pid")) is not int or info["pid"] <= 0:
        raise BenchmarkError(f"Malformed serve.json: {info!r}")
    if info["pid"] != expected_pid:
        raise DiscoveryPending(f"Stale serve.json PID {info['pid']}; owned PID is {expected_pid}")
    port = info.get("port")
    if type(port) is not int or not 1 <= port <= 65535:
        raise BenchmarkError(f"Malformed serve.json port: {port!r}")
    return port


def read_discovery(index, expected_pid):
    info = json.loads((index / "serve.json").read_text(encoding="utf-8"))
    return discovery_port(info, expected_pid)


def parse_status_response(message):
    if (not isinstance(message, dict) or message.get("jsonrpc") != "2.0"
            or type(message.get("id")) is not int or message["id"] != 1):
        raise BenchmarkError(f"Malformed status RPC envelope: {message!r}")
    if "error" in message:
        raise BenchmarkError(f"Status RPC error: {message['error']!r}")
    if not isinstance(message.get("result"), dict):
        raise BenchmarkError(f"Malformed status RPC result: {message!r}")
    return message["result"]


def request_status(port, timeout):
    with socket.create_connection(("127.0.0.1", port), timeout=timeout) as connection:
        connection.sendall(b'{"jsonrpc":"2.0","method":"status","id":1}\n')
        with connection.makefile("rb") as response:
            line = response.readline(RPC_MAX_BYTES + 1)
    if len(line) > RPC_MAX_BYTES or not line.endswith(b"\n"):
        raise BenchmarkError("Status RPC returned an oversized or incomplete response")
    try:
        message = json.loads(line)
    except (ValueError, UnicodeError) as error:
        raise BenchmarkError(f"Malformed status RPC JSON: {line!r}") from error
    return parse_status_response(message)


def status_ready(status):
    if not isinstance(status, dict):
        raise BenchmarkError(f"Malformed status: {status!r}")
    for name in ("last_reconcile_at", "last_reconcile_error",
                 "reconcile_running", "indexing", "hidden_complete"):
        if name not in status:
            raise BenchmarkError(f"Status is missing {name}: {status!r}")
    if status["last_reconcile_error"] is not None:
        raise BenchmarkError(f"Reconciliation failed: {status['last_reconcile_error']!r}")
    last = status["last_reconcile_at"]
    if last is not None and (type(last) is not int or last < 0):
        raise BenchmarkError(f"Malformed last_reconcile_at: {last!r}")
    for name in ("reconcile_running", "indexing", "hidden_complete", "flushing",
                 "reconcile_pending", "reconcile_overdue", "watcher_active"):
        if name in status and type(status[name]) is not bool:
            raise BenchmarkError(f"Malformed status flag {name}: {status[name]!r}")
    if status.get("watcher_active") or any(
        status.get(name, "disabled") != "disabled"
        for name in ("watch_mode_requested", "watch_mode_active")
    ):
        raise BenchmarkError(f"Server did not honor --no-watch: {status!r}")
    return (
        last is not None and status["hidden_complete"]
        and not any(status.get(name, False) for name in (
            "reconcile_running", "indexing", "flushing", "reconcile_pending", "reconcile_overdue"
        ))
    )


def require_alive(process, logfile):
    code = process.poll()
    if code is not None:
        raise BenchmarkError(f"Owned server PID {process.pid} exited {code}; see {logfile}")


def wait_ready(process, index, timeout, logfile):
    started = time.monotonic()
    deadline = started + timeout
    transient_since = None
    observation = "no discovery attempted"
    while time.monotonic() < deadline:
        require_alive(process, logfile)
        try:
            port = read_discovery(index, process.pid)
        except (FileNotFoundError, DiscoveryPending) as error:
            observation = str(error)
        except (json.JSONDecodeError, UnicodeError) as error:
            observation = f"Malformed discovery: {error}"
            if transient_since is None:
                transient_since = time.monotonic()
        else:
            try:
                status = request_status(port, min(5.0, max(0.001, deadline - time.monotonic())))
            except (ConnectionError, TimeoutError) as error:
                observation = f"Startup connection error: {error}"
                if transient_since is None:
                    transient_since = time.monotonic()
            else:
                transient_since = None
                observation = repr(status)
                require_alive(process, logfile)
                if status_ready(status):
                    return status, (time.monotonic() - started) * 1000
        if transient_since is not None and time.monotonic() - transient_since >= DISCOVERY_GRACE:
            raise BenchmarkError(f"Persistent startup discovery failure: {observation}; see {logfile}")
        time.sleep(min(0.1, max(0.0, deadline - time.monotonic())))
    raise BenchmarkError(f"Startup timeout after {timeout}s; last observation: {observation}; see {logfile}")


def checked_status(process, index, logfile, reference, timeout):
    require_alive(process, logfile)
    status = request_status(read_discovery(index, process.pid), min(5.0, timeout))
    require_alive(process, logfile)
    if not status_ready(status):
        raise BenchmarkError(f"Server is not idle/complete: {status!r}; see {logfile}")
    if status["last_reconcile_at"] != reference["last_reconcile_at"]:
        raise BenchmarkError(f"Reconciliation overlapped the block: {reference!r} -> {status!r}")
    return status


def stop_owned_process(process):
    forced = False
    if process.poll() is None:
        try:
            process.terminate()
        except ProcessLookupError:
            if process.poll() is None:
                raise
    try:
        code = process.wait(timeout=15)
    except subprocess.TimeoutExpired:
        forced = True
        process.kill()
        code = process.wait(timeout=15)
    return {"exit_code": code, "forced_kill": forced}


@contextlib.contextmanager
def managed_server(binary, corpus, index, logfile, timeout, record):
    command = [str(binary), "serve", str(corpus), "--index-path", str(index), "--no-watch"]
    record["server_command"] = command
    record["server_log"] = str(logfile)
    with logfile.open("wb") as log:
        process = subprocess.Popen(command, stdin=subprocess.DEVNULL,
                                   stdout=log, stderr=subprocess.STDOUT)
        record["pid"] = process.pid
        try:
            status, elapsed = wait_ready(process, index, timeout, logfile)
            record["ready_status"] = status
            record["startup_until_reconciled_ms"] = elapsed
            yield process
            require_alive(process, logfile)
        finally:
            record["shutdown"] = stop_owned_process(process)


def proc_text(name):
    if not sys.platform.startswith("linux"):
        return None
    path = Path("/proc") / name
    try:
        return path.read_text(encoding="utf-8")
    except OSError as error:
        return {"unavailable": str(error)}


def memory_snapshot():
    text = proc_text("meminfo")
    if not isinstance(text, str):
        return text
    return dict(line.split(":", 1) for line in text.splitlines() if ":" in line)


def host_metadata():
    cpuinfo = proc_text("cpuinfo")
    model = cpuinfo
    if isinstance(cpuinfo, str):
        model = next((line.split(":", 1)[1].strip() for line in cpuinfo.splitlines()
                      if line.split(":", 1)[0].strip() in ("model name", "Hardware")), None)
    names = ("RUNNER_OS", "RUNNER_ARCH", "ImageOS", "ImageVersion", "GITHUB_SERVER_URL",
             "GITHUB_REPOSITORY", "GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT")
    environment = {name: os.environ.get(name) for name in names}
    run_url = None
    if environment["GITHUB_REPOSITORY"] and environment["GITHUB_RUN_ID"]:
        run_url = (
            f"{environment['GITHUB_SERVER_URL'] or 'https://github.com'}/"
            f"{environment['GITHUB_REPOSITORY']}/actions/runs/{environment['GITHUB_RUN_ID']}"
        )
    return {
        "platform": platform.platform(), "machine": platform.machine(),
        "python": sys.version, "python_executable": sys.executable,
        "cpu_count": os.cpu_count(), "cpu_model": model,
        "github_environment": environment, "github_run_url": run_url,
    }


def shuffled_ids(count, seed):
    ids = list(range(1, count + 1))
    random.Random(seed).shuffle(ids)
    return ids


def distribution(values):
    if not values:
        return None
    ordered = sorted(values)
    rank = (len(ordered) - 1) * 0.9
    low = math.floor(rank)
    high = math.ceil(rank)
    return {
        "n": len(ordered), "median_ms": statistics.median(ordered),
        "min_ms": ordered[0], "max_ms": ordered[-1],
        "p90_ms": ordered[low] + (ordered[high] - ordered[low]) * (rank - low),
    }


def effect(baseline, candidate):
    ratio = candidate / baseline if baseline > 0 else None
    return {
        "candidate_over_baseline": ratio,
        "change_percent": (ratio - 1) * 100 if ratio is not None else None,
    }


def summarize_measurements(measurements):
    summary = {label: distribution([row["ms"] for row in measurements if row["label"] == label])
               for label in LABELS}
    baseline, candidate = (summary[label] for label in LABELS)
    summary.update(effect(baseline["median_ms"], candidate["median_ms"])
                   if baseline and candidate else
                   {"candidate_over_baseline": None, "change_percent": None})
    blocks = []
    for block in sorted({row["block"] for row in measurements}):
        rows = [row for row in measurements if row["block"] == block]
        blocks.append({"block": block, "label": rows[0]["label"],
                       **distribution([row["ms"] for row in rows])})
    summary["per_block"] = blocks
    by_block = {row["block"]: row for row in blocks}
    pairs = []
    for first in sorted(number for number in by_block if number % 2 == 1):
        if first + 1 not in by_block:
            continue
        pair = {by_block[number]["label"]: by_block[number] for number in (first, first + 1)}
        if set(pair) != set(LABELS):
            raise BenchmarkError(f"Blocks {first}/{first + 1} are not a baseline/candidate pair")
        base, cand = (pair[label]["median_ms"] for label in LABELS)
        pairs.append({"blocks": [first, first + 1], "baseline_median_ms": base,
                      "candidate_median_ms": cand, **effect(base, cand)})
    summary["paired_block_ratios"] = pairs
    ratios = [pair["candidate_over_baseline"] for pair in pairs
              if pair["candidate_over_baseline"] is not None]
    summary["median_paired_block_ratio"] = statistics.median(ratios) if ratios else None
    return summary


def summarize(samples, patterns):
    queries = []
    for query_id, pattern in enumerate(patterns, 1):
        rows = [sample for sample in samples if sample["query_id"] == query_id]
        queries.append({"query_id": query_id, "pattern": pattern, **summarize_measurements(rows)})
    grouped = {}
    for sample in samples:
        key = (sample["block"], sample["label"], sample["repeat"])
        group = grouped.setdefault(key, {})
        if sample["query_id"] in group:
            raise BenchmarkError(f"Duplicate timed query in suite {key}: {sample['query_id']}")
        group[sample["query_id"]] = sample["ms"]
    suites = []
    for (block, label, repeat), times in sorted(grouped.items()):
        if set(times) == set(range(1, len(patterns) + 1)):
            suites.append({"block": block, "label": label, "repeat": repeat, "ms": sum(times.values())})
    return {"queries": queries, "suite": {**summarize_measurements(suites), "repetitions": suites}}


def markdown_text(value):
    # Entities avoid table/backtick parsing without changing the displayed pattern.
    return (str(value).replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
            .replace("|", "&#124;").replace("`", "&#96;").replace("\\", "&#92;")
            .replace("*", "&#42;").replace("_", "&#95;")
            .replace("\r", "&#13;").replace("\n", "&#10;"))


def number(value, digits=3):
    return "-" if value is None else f"{value:.{digits}f}"


def render_markdown(report):
    lines = [
        "# Paired tgrep comparison", "", f"**Run status: {report['status']}**",
        "Only completed, validated blocks enter the summaries. Partial/failed runs are not conclusions.",
        "", f"Baseline: {report['baseline_sha']}", f"Candidate: {report['candidate_sha']}",
        f"Corpus: {report['corpus_sha']}", "",
    ]
    for error in report.get("errors", []):
        lines.append(f"**Error ({markdown_text(error['phase'])}):** {markdown_text(error['message'])}")
    lines += [
        "", "## Method", "",
        "- One host and physical corpus; one baseline-built shared index, with byte manifests "
        "before/after each sequential --no-watch server block. Compatible index formats only.",
        f"- Order: {report['method']['block_order']}; {report['method']['repeats']} full suites/block. "
        "Original-order content parity, then a fixed shuffled warm suite before timing.",
        "- Each timed CLI writes stdout to the null device; stderr must be empty and exit codes "
        "must match the baseline. No timed --stats. Startup, parity, sorting and route probes are excluded.",
        "- Every block must contain exactly repeats+2 completed content-search server traces per "
        "input query (parity, warm and timed passes), matched by decoded pattern including duplicates. "
        "Missing, extra or malformed traces invalidate the block; raw logs remain untouched.",
        "- All times are wall-clock milliseconds including process/IPC/output costs. Suite times "
        "are sums of every query in a repetition, then summarized (not sums of query medians).",
        "- P90 uses linear interpolation at (n-1)*0.9. Ratios are candidate/baseline; below 1 "
        "and negative changes mean faster. Adjacent two-block pairs compare block medians.",
        "- Warm-server/OS caches on one host/corpus do not establish cold performance or a general "
        "guarantee. Samples are correlated; no statistical significance is claimed.",
        "- Caller build contract for each supplied revision: cargo build --release --locked. "
        "Build commands/revision labels are not independently attested; binary SHA256s are recorded.",
        "- Parity canonicalizes LF-delimited output ordering only, retains duplicates and byte "
        "content, and adds a missing final LF. Windows sorting is bounded to small smoke fixtures.",
        "", "## Query results", "",
        "| # | Pattern | Baseline median | min | max | p90 | Candidate median | min | max | p90 | C/B | Change % | Median paired C/B |",
        "|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
    ]
    summary = report["summary"]
    rows = summary["queries"] + [{"query_id": "-", "pattern": "TOTAL SUITE", **summary["suite"]}]
    for row in rows:
        cells = [str(row["query_id"]), markdown_text(row["pattern"])]
        for label in LABELS:
            stats = row[label] or {}
            cells += [number(stats.get(key)) for key in ("median_ms", "min_ms", "max_ms", "p90_ms")]
        cells += [number(row["candidate_over_baseline"]), number(row["change_percent"], 2),
                  number(row["median_paired_block_ratio"])]
        lines.append("| " + " | ".join(cells) + " |")
    lines += ["", "## Per-block medians (ms)", "",
              "| Pattern | Block | Label | Median |", "|---|---:|---|---:|"]
    for row in rows:
        for block in row["per_block"]:
            lines.append(f"| {markdown_text(row['pattern'])} | {block['block']} | "
                         f"{block['label']} | {number(block['median_ms'])} |")
    lines += ["", "## Summed suites per repetition (ms)", "",
              "| Block | Label | Repeat | Suite |", "|---:|---|---:|---:|"]
    for suite in summary["suite"]["repetitions"]:
        lines.append(f"| {suite['block']} | {suite['label']} | {suite['repeat']} | {number(suite['ms'])} |")
    lines += ["", "## Block validation", "",
              "| Block | Label | Status | Manifest unchanged | Content-server traces |",
              "|---:|---|---|---|---|"]
    for block in report["blocks"]:
        check = block.get("content_server_check")
        traces = (f"{check['observed_count']}/{check['expected_count']} "
                  f"({'matched' if check['match'] else 'FAILED'})" if check else "not checked")
        lines.append(f"| {block['block']} | {block['label']} | {block['status']} | "
                     f"{block.get('manifest_match', 'not checked')} | {traces} |")
    lines += ["", "Full environment, binary hashes, corpus snapshots, raw samples, paired ratios, "
              "status observations and manifests are in comparison.json. Server/index logs are in logs/.", ""]
    return "\n".join(lines)


def write_report(work, report):
    complete = {block["block"] for block in report["blocks"] if block["status"] == "complete"}
    report["summary"] = summarize(
        [sample for sample in report["samples"] if sample["block"] in complete], report["queries"]
    )
    report["updated_at"] = utc_now()
    for name, content in (
        ("comparison.json", json.dumps(report, indent=2, allow_nan=False) + "\n"),
        ("comparison.md", render_markdown(report)),
    ):
        temporary = work / (name + ".tmp")
        temporary.write_text(content, encoding="utf-8")
        temporary.replace(work / name)


def build_index(binary, corpus, index, logfile, timeout, record):
    command = [str(binary), "index", str(corpus), "--index-path", str(index)]
    record.update({"command": command, "log": str(logfile)})
    started = time.perf_counter_ns()
    with logfile.open("wb") as log:
        result = run_command(command, timeout, stdout=log, stderr=subprocess.STDOUT)
    record.update({"exit_code": result.returncode,
                   "elapsed_ms": (time.perf_counter_ns() - started) / 1_000_000})
    require_success(result, f"Baseline index build; see {logfile}")


def execute_block(args, work, corpus, binary, report, record, reference):
    index = work / "index"
    temp_dir = work / "tmp"
    logfile = work / "logs" / f"server-{record['block']:02d}-{record['label']}.log"
    patterns = report["queries"]
    record["memory_before"] = memory_snapshot()
    record["manifest_before"] = index_manifest(index)
    require_manifest(report["index_manifest"], record["manifest_before"])
    try:
        with managed_server(binary, corpus, index, logfile, args.startup_timeout, record) as process:
            record["route_probe"] = probe_route(binary, corpus, index, args.query_timeout)
            record["parity"] = []
            for query_id, pattern in enumerate(patterns, 1):
                require_alive(process, logfile)
                with tempfile.TemporaryDirectory(prefix="parity-", dir=temp_dir) as scratch:
                    output = Path(scratch) / "output"
                    with output.open("wb") as capture:
                        elapsed, code = run_query(binary, corpus, index, pattern, args.query_timeout, capture)
                    observed = {**fingerprint_output(output, temp_dir, report["sort"], args.query_timeout),
                                "exit_code": code}
                record["parity"].append({"query_id": query_id, "pattern": pattern,
                                         "first_pass_ms": elapsed, **observed})
                if query_id not in reference:
                    if record["block"] != 1 or record["label"] != "baseline":
                        raise BenchmarkError("Missing first-block baseline parity reference")
                    reference[query_id] = observed
                require_parity(reference[query_id], observed, f"Block {record['block']}, query {query_id}")
            record["warm_order"] = shuffled_ids(len(patterns), WARM_SEED)
            for query_id in record["warm_order"]:
                require_alive(process, logfile)
                run_query(binary, corpus, index, patterns[query_id - 1], args.query_timeout,
                          subprocess.DEVNULL, reference[query_id]["exit_code"])
            record["pre_timed_status"] = checked_status(
                process, index, logfile, record["ready_status"], args.query_timeout
            )
            record["repeat_statuses"] = []
            for repeat in range(1, args.repeats + 1):
                order = shuffled_ids(len(patterns), REPEAT_SEED + repeat - 1)
                for position, query_id in enumerate(order, 1):
                    require_alive(process, logfile)
                    pattern = patterns[query_id - 1]
                    elapsed, _ = run_query(binary, corpus, index, pattern, args.query_timeout,
                                           subprocess.DEVNULL, reference[query_id]["exit_code"])
                    report["samples"].append({
                        "block": record["block"], "label": record["label"], "repeat": repeat,
                        "position": position, "query_id": query_id, "pattern": pattern, "ms": elapsed,
                    })
                record["repeat_statuses"].append(checked_status(
                    process, index, logfile, record["pre_timed_status"], args.query_timeout
                ))
                print(f"Block {record['block']} {record['label']}: repeat {repeat}/{args.repeats} complete",
                      flush=True)
            record["final_status"] = checked_status(
                process, index, logfile, record["pre_timed_status"], args.query_timeout
            )
        record["content_server_check"] = content_server_check(logfile, patterns, args.repeats)
        require_content_server_check(record["content_server_check"])
    finally:
        record["memory_after"] = memory_snapshot()
        record["manifest_after"] = index_manifest(index)
        record["manifest_match"] = record["manifest_before"] == record["manifest_after"]
        require_manifest(report["index_manifest"], record["manifest_after"])
    record["status"] = "complete"


def record_error(report, phase, error):
    report["errors"].append({
        "phase": phase, "type": type(error).__name__, "message": str(error),
        "traceback": "".join(traceback.format_exception(type(error), error, error.__traceback__)),
    })
    report["status"] = "failed"
    print(f"ERROR ({phase}): {error}", file=sys.stderr, flush=True)


def run_benchmark(args):
    work, corpus = create_work_directory(args.work, args.repo_path)
    report = {
        "schema_version": 1, "status": "running", "started_at": utc_now(),
        "baseline_sha": args.baseline_sha, "candidate_sha": args.candidate_sha,
        "corpus_sha": args.corpus_sha, "work": str(work), "queries": [],
        "blocks": [], "samples": [], "errors": [], "binaries": {},
        "method": {
            "block_order": args.blocks, "labels": list(BLOCK_ORDERS[args.blocks]),
            "repeats": args.repeats, "warm_seed": WARM_SEED,
            "repeat_seeds": [REPEAT_SEED + repeat for repeat in range(args.repeats)],
            "query_timeout_seconds": args.query_timeout,
            "startup_and_index_timeout_seconds": args.startup_timeout,
            "build_commands": {label: ["cargo", "build", "--release", "--locked"] for label in LABELS},
            "build_provenance": "Caller contract, not executed or independently verified by this script",
            "search_flags": ["--color", "never"], "pattern_mode": "regex (tgrep default)",
            "index": "Built once by baseline; same corpus and index paths for every sequential server",
            "timing": "perf_counter_ns around CLI subprocess; stdout DEVNULL, stderr checked, no --stats",
            "warmup": "Original-order output parity pass, then identical fixed-seed shuffled full suite",
            "content_server_validation": (
                "After shutdown, match completed search trace counts by decoded pattern: "
                "(repeats+2) per input query, including duplicates; raw logs are preserved"
            ),
            "fingerprint": "SHA256 of bytewise sorted LF records; duplicates retained; final LF normalized",
            "p90": "Linear interpolation at (n-1)*0.9",
            "suite": "Sum of query times within each repetition; statistics over those sums",
            "pairing": "Adjacent blocks (1,2), (3,4), ...; candidate/baseline block-median ratios",
            "scope": "One host/corpus, warm server/OS caches; no cold guarantee or significance claim",
        },
    }
    reference = {}
    initial = None
    try:
        write_report(work, report)
        report["host"] = host_metadata()
        query_path = args.queries.resolve(strict=True)
        query_data = json.loads(query_path.read_text(encoding="utf-8"))
        patterns = query_data.get("queries") if isinstance(query_data, dict) else None
        if not isinstance(patterns, list) or not patterns or any(
            not isinstance(pattern, str) or "\0" in pattern for pattern in patterns
        ):
            raise BenchmarkError("Queries JSON must contain a nonempty 'queries' array of NUL-free strings")
        report["queries"] = patterns
        report["queries_file"] = {"path": str(query_path), **hash_file(query_path)}
        for label in LABELS:
            binary = getattr(args, label).resolve(strict=True)
            if not binary.is_file() or (os.name != "nt" and not os.access(binary, os.X_OK)):
                raise BenchmarkError(f"Binary is not an executable file: {binary}")
            report["binaries"][label] = {"path": str(binary), **hash_file(binary)}
        initial = corpus_snapshot(corpus, work / "tmp", args.startup_timeout)
        report["corpus_before"] = initial
        require_corpus(initial, corpus, args.corpus_sha)
        report["sort"] = choose_sort_backend(args.query_timeout)
        report["index_build"] = {}
        print("Building the shared index once with baseline", flush=True)
        build_index(report["binaries"]["baseline"]["path"], corpus, work / "index",
                    work / "logs" / "index-baseline.log", args.startup_timeout, report["index_build"])
        report["index_manifest"] = index_manifest(work / "index")
        write_report(work, report)
        for block, label in enumerate(BLOCK_ORDERS[args.blocks], 1):
            record = {"block": block, "label": label, "status": "running"}
            report["blocks"].append(record)
            print(f"Starting block {block}/{len(BLOCK_ORDERS[args.blocks])}: {label}", flush=True)
            try:
                execute_block(args, work, corpus, report["binaries"][label]["path"],
                              report, record, reference)
            except (Exception, KeyboardInterrupt) as error:
                record["status"] = "failed"
                record["error"] = str(error)
                raise
            finally:
                report["parity_reference"] = {str(key): value for key, value in reference.items()}
                write_report(work, report)
            print(f"Block {block} complete; shared index unchanged", flush=True)
    except (Exception, KeyboardInterrupt) as error:
        record_error(report, "benchmark", error)
    finally:
        if initial is not None:
            try:
                final = corpus_snapshot(corpus, work / "tmp", args.startup_timeout)
                report["corpus_after"] = final
                require_corpus(final, corpus, args.corpus_sha, initial)
            except (Exception, KeyboardInterrupt) as error:
                record_error(report, "final corpus validation", error)
        for label, binary in report["binaries"].items():
            try:
                final_hash = hash_file(Path(binary["path"]))
                binary["after"] = final_hash
                if any(binary[key] != final_hash[key] for key in ("sha256", "bytes")):
                    raise BenchmarkError(f"{label} binary changed during benchmark")
            except (Exception, KeyboardInterrupt) as error:
                record_error(report, "final binary validation", error)
        report["status"] = "failed" if report["errors"] else "complete"
        report["finished_at"] = utc_now()
        write_report(work, report)
    if report["status"] == "complete":
        suite = report["summary"]["suite"]
        print(f"Median summed suite: baseline {suite['baseline']['median_ms']:.3f} ms, "
              f"candidate {suite['candidate']['median_ms']:.3f} ms; "
              f"candidate/baseline {number(suite['candidate_over_baseline'])}, "
              f"change {number(suite['change_percent'], 2)}%", flush=True)
    print(f"Results ({report['status']}): {work / 'comparison.json'} and {work / 'comparison.md'}",
          flush=True)
    return report


def main(argv=None):
    args = argument_parser().parse_args(argv)
    try:
        report = run_benchmark(args)
    except (Exception, KeyboardInterrupt) as error:
        print(f"ERROR: {error}", file=sys.stderr, flush=True)
        return 1
    return 0 if report["status"] == "complete" else 1


if __name__ == "__main__":
    sys.exit(main())
