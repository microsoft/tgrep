#!/usr/bin/env python3
"""Bounded shared-worktree baseline. See SHARED_INDEX_BENCHMARKS.md."""

import argparse
import ctypes
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import random
import re
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import time


SCHEMA = "tgrep.shared-benchmark.v1"
MAX_FILES = 4096
MAX_TREE_BYTES = 32 * 1024 * 1024
QUERIES = ["symbol_000007", "shared_token", "absent_marker_xyz"]
RESOURCE_KEYS = (
    "cpu_seconds", "rss_bytes", "private_bytes", "pss_bytes",
    "read_transfer_bytes", "storage_read_bytes",
)


class CommandError(RuntimeError):
    """Invocation failure, distinct from process containment/cleanup failure."""


def metric(value=None, reason=None):
    if (value is None) != (reason is not None):
        raise ValueError("A missing metric must have a reason, and only missing metrics do")
    if reason is not None and (not isinstance(reason, str) or not reason):
        raise ValueError("A missing metric needs a nonempty explanation")
    if value is not None and (isinstance(value, bool) or not isinstance(value, (int, float))
                              or not math.isfinite(value) or value < 0):
        raise ValueError("Metric values must be finite nonnegative numbers")
    return {"value": value, "reason": reason}


def distribution(samples):
    ordered = sorted(samples)
    if not ordered:
        return {"count": 0, "p50_ms": None, "p95_ms": None, "max_ms": None,
                "reason": "no samples"}
    return {"count": len(ordered), "p50_ms": ordered[math.ceil(len(ordered) * .50) - 1],
            "p95_ms": ordered[math.ceil(len(ordered) * .95) - 1],
            "max_ms": ordered[-1], "reason": None}


def aggregate(snapshots):
    if not snapshots:
        raise ValueError("Cannot aggregate an empty process set")
    result = {}
    for key in RESOURCE_KEYS:
        missing = [s[key]["reason"] for s in snapshots if s[key]["value"] is None]
        result[key] = (metric(reason="; ".join(sorted(set(missing)))) if missing else
                       metric(sum(s[key]["value"] for s in snapshots)))
    return result


def resource_delta(before, after):
    result = {}
    for key in ("cpu_seconds", "read_transfer_bytes", "storage_read_bytes"):
        a, b = before[key], after[key]
        result[key] = (metric(reason=a["reason"] or b["reason"]) if
                       a["value"] is None or b["value"] is None else
                       metric(b["value"] - a["value"]))
    return result


if os.name == "nt":
    from ctypes import wintypes as wt

    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    psapi = ctypes.WinDLL("psapi", use_last_error=True)

    class IOCounters(ctypes.Structure):
        _fields_ = [(name, ctypes.c_ulonglong) for name in (
            "read_ops", "write_ops", "other_ops", "read_bytes", "write_bytes", "other_bytes")]

    class MemoryCounters(ctypes.Structure):
        _fields_ = [("cb", wt.DWORD), ("faults", wt.DWORD)] + [
            (name, ctypes.c_size_t) for name in (
                "peak_rss", "rss", "peak_pool", "pool", "peak_nonpaged",
                "nonpaged", "pagefile", "peak_pagefile", "private")]

    class BasicLimits(ctypes.Structure):
        _fields_ = [("process_time", ctypes.c_longlong), ("job_time", ctypes.c_longlong),
                    ("flags", wt.DWORD), ("min_ws", ctypes.c_size_t),
                    ("max_ws", ctypes.c_size_t), ("process_limit", wt.DWORD),
                    ("affinity", ctypes.c_size_t), ("priority", wt.DWORD),
                    ("scheduling", wt.DWORD)]

    class ExtendedLimits(ctypes.Structure):
        _fields_ = [("basic", BasicLimits), ("io", IOCounters)] + [
            (name, ctypes.c_size_t) for name in
            ("process_memory", "job_memory", "peak_process_memory", "peak_job_memory")]

    class Accounting(ctypes.Structure):
        _fields_ = [(name, ctypes.c_int64) for name in (
            "user_time", "kernel_time", "period_user_time", "period_kernel_time")] + [
            (name, wt.DWORD) for name in ("page_faults", "total", "active", "terminated")]

    class ThreadEntry(ctypes.Structure):
        _fields_ = [(name, wt.DWORD) for name in
                    ("size", "usage", "thread_id", "process_id")] + [
            ("base_priority", wt.LONG), ("delta_priority", wt.LONG), ("flags", wt.DWORD)]

    kernel.CreateJobObjectW.argtypes = [ctypes.c_void_p, wt.LPCWSTR]
    kernel.CreateJobObjectW.restype = wt.HANDLE
    kernel.SetInformationJobObject.argtypes = [
        wt.HANDLE, ctypes.c_int, ctypes.c_void_p, wt.DWORD]
    kernel.AssignProcessToJobObject.argtypes = [wt.HANDLE, wt.HANDLE]
    kernel.CloseHandle.argtypes = [wt.HANDLE]
    kernel.GetProcessTimes.argtypes = [wt.HANDLE] + [ctypes.POINTER(wt.FILETIME)] * 4
    kernel.GetProcessIoCounters.argtypes = [wt.HANDLE, ctypes.POINTER(IOCounters)]
    psapi.GetProcessMemoryInfo.argtypes = [
        wt.HANDLE, ctypes.POINTER(MemoryCounters), wt.DWORD]
    for name, (result, arguments) in {
        "QueryInformationJobObject": (wt.BOOL, [
            wt.HANDLE, ctypes.c_int, ctypes.c_void_p, wt.DWORD, ctypes.c_void_p]),
        "TerminateJobObject": (wt.BOOL, [wt.HANDLE, wt.UINT]),
        "CreateToolhelp32Snapshot": (wt.HANDLE, [wt.DWORD, wt.DWORD]),
        "Thread32First": (wt.BOOL, [wt.HANDLE, ctypes.POINTER(ThreadEntry)]),
        "Thread32Next": (wt.BOOL, [wt.HANDLE, ctypes.POINTER(ThreadEntry)]),
        "OpenThread": (wt.HANDLE, [wt.DWORD, wt.BOOL, wt.DWORD]),
        "ResumeThread": (wt.DWORD, [wt.HANDLE]),
        "GetVolumePathNameW": (wt.BOOL, [wt.LPCWSTR, wt.LPWSTR, wt.DWORD]),
        "GetDiskFreeSpaceW": (wt.BOOL, [wt.LPCWSTR] + [ctypes.POINTER(wt.DWORD)] * 4),
    }.items():
        getattr(kernel, name).restype = result
        getattr(kernel, name).argtypes = arguments

    class WindowsJob:
        """Adapted from scripts/qualification/windows_job.py at b767b93."""

        @staticmethod
        def checked(result):
            if not result:
                raise ctypes.WinError(ctypes.get_last_error())
            return result

        def __init__(self):
            self.handle = self.checked(kernel.CreateJobObjectW(None, None))
            limits = ExtendedLimits()
            limits.basic.flags = 0x2000  # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            try:
                self.checked(kernel.SetInformationJobObject(
                    self.handle, 9, ctypes.byref(limits), ctypes.sizeof(limits)))
            except BaseException:
                self.close()
                raise

        def assign_and_resume(self, process):
            self.checked(kernel.AssignProcessToJobObject(
                self.handle, wt.HANDLE(int(process._handle))))
            snapshot = kernel.CreateToolhelp32Snapshot(4, 0)  # TH32CS_SNAPTHREAD
            if snapshot == ctypes.c_void_p(-1).value:
                raise ctypes.WinError(ctypes.get_last_error())
            try:
                entry = ThreadEntry()
                entry.size = ctypes.sizeof(entry)
                found = kernel.Thread32First(snapshot, ctypes.byref(entry))
                while found:
                    if entry.process_id == process.pid:
                        thread = self.checked(kernel.OpenThread(2, False, entry.thread_id))
                        try:
                            previous = kernel.ResumeThread(thread)
                            if previous == 0xFFFFFFFF:
                                raise ctypes.WinError(ctypes.get_last_error())
                            if previous != 1:
                                raise RuntimeError(f"unexpected child suspension count: {previous}")
                            return
                        finally:
                            self.checked(kernel.CloseHandle(thread))
                    entry.size = ctypes.sizeof(entry)
                    found = kernel.Thread32Next(snapshot, ctypes.byref(entry))
                error = ctypes.get_last_error()
                if error != 18:  # ERROR_NO_MORE_FILES
                    raise ctypes.WinError(error)
                raise RuntimeError(f"no suspended primary thread for owned process {process.pid}")
            finally:
                self.checked(kernel.CloseHandle(snapshot))

        def terminate(self):
            self.checked(kernel.TerminateJobObject(self.handle, 1))
            deadline = time.monotonic() + 10
            while True:
                accounting = Accounting()
                self.checked(kernel.QueryInformationJobObject(
                    self.handle, 1, ctypes.byref(accounting), ctypes.sizeof(accounting), None))
                if accounting.active == 0:
                    return
                if time.monotonic() >= deadline:
                    raise RuntimeError(f"owned job still has {accounting.active} processes")
                time.sleep(.01)

        def close(self):
            self.checked(kernel.CloseHandle(self.handle))


def process_resources(process):
    unavailable = "not implemented on " + sys.platform
    result = {key: metric(reason=unavailable) for key in RESOURCE_KEYS}
    if sys.platform.startswith("linux"):
        proc = Path("/proc") / str(process.pid)
        # stat's comm can contain spaces and parentheses.
        fields = (proc / "stat").read_text().rsplit(")", 1)[1].split()
        result["cpu_seconds"] = metric(
            (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK"))
        status = dict(line.split(":", 1) for line in (proc / "status").read_text().splitlines())
        for key, field in (("rss_bytes", "VmRSS"), ("private_bytes", "RssAnon")):
            result[key] = metric(int(status[field].split()[0]) * 1024)
        try:
            smaps = dict(line.split(":", 1) for line in
                         (proc / "smaps_rollup").read_text().splitlines()[1:])
            result["pss_bytes"] = metric(int(smaps["Pss"].split()[0]) * 1024)
        except (OSError, KeyError) as error:
            result["pss_bytes"] = metric(reason=f"smaps_rollup unavailable: {error}")
        try:
            io = dict(line.split(":", 1) for line in (proc / "io").read_text().splitlines())
            result["read_transfer_bytes"] = metric(int(io["rchar"]))
            result["storage_read_bytes"] = metric(int(io["read_bytes"]))
        except OSError as error:
            for key in ("read_transfer_bytes", "storage_read_bytes"):
                result[key] = metric(reason=f"/proc/PID/io unavailable: {error}")
    elif os.name == "nt":
        handle = wt.HANDLE(int(process._handle))
        times = [wt.FILETIME() for _ in range(4)]
        if not kernel.GetProcessTimes(handle, *(ctypes.byref(t) for t in times)):
            raise ctypes.WinError(ctypes.get_last_error())
        result["cpu_seconds"] = metric(sum(
            (t.dwHighDateTime << 32) + t.dwLowDateTime for t in times[2:]) / 1e7)
        memory = MemoryCounters()
        memory.cb = ctypes.sizeof(memory)
        if not psapi.GetProcessMemoryInfo(handle, ctypes.byref(memory), memory.cb):
            raise ctypes.WinError(ctypes.get_last_error())
        result["rss_bytes"] = metric(memory.rss)
        result["private_bytes"] = metric(memory.private)
        result["pss_bytes"] = metric(reason="Windows does not expose Linux PSS")
        io = IOCounters()
        if not kernel.GetProcessIoCounters(handle, ctypes.byref(io)):
            raise ctypes.WinError(ctypes.get_last_error())
        result["read_transfer_bytes"] = metric(io.read_bytes)
        result["storage_read_bytes"] = metric(
            reason="GetProcessIoCounters includes cached and non-file I/O; no physical read attribution")
    return result


class OwnedProcess:
    """One owned process tree; no name-based/global process cleanup."""

    def __init__(self, argv, **kwargs):
        self.job = WindowsJob() if os.name == "nt" else None
        self.stopped = False
        try:
            self.process = subprocess.Popen(argv, start_new_session=os.name != "nt",
                                            creationflags=4 if self.job else 0, **kwargs)
        except BaseException as error:
            if self.job:
                self.job.close()
            if isinstance(error, FileNotFoundError):
                raise CommandError(f"Executable unavailable: {argv[0]}: {error}") from error
            raise
        try:
            if self.job:
                self.job.assign_and_resume(self.process)
        except BaseException:
            self.stop()
            for stream in (self.process.stdout, self.process.stderr):
                if stream is not None:
                    stream.close()
            raise

    def stop(self):
        if self.stopped:
            return
        process = self.process
        if self.job:
            try:
                try:
                    self.job.terminate()
                finally:
                    # Assignment itself can fail; the suspended child then
                    # belongs to no job and must be explicitly terminated.
                    if process.poll() is None:
                        process.kill()
                    process.wait(timeout=10)
            finally:
                process._handle.Close()
                self.job.close()
            self.stopped = True
            return
        if os.name != "nt":
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        elif process.poll() is None:
            process.terminate()
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            if os.name != "nt":
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            else:
                process.kill()
            process.wait(timeout=3)
        # A leader can exit before a child; finish its owned Unix group too.
        if os.name != "nt":
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        self.stopped = True


def command(argv, cwd=None, env=None, timeout=60, allowed=(0,)):
    owned = OwnedProcess([str(a) for a in argv], cwd=cwd, env=env,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        try:
            stdout, stderr = owned.process.communicate(timeout=timeout)
        except subprocess.TimeoutExpired as error:
            raise CommandError(f"{argv!r}: timed out after {timeout}s") from error
        if owned.process.returncode not in allowed:
            raise CommandError(f"{argv!r}: exit {owned.process.returncode}: "
                               f"{stderr.decode('utf-8', errors='replace')}")
        return stdout, stderr
    finally:
        try:
            owned.stop()
        finally:
            for stream in (owned.process.stdout, owned.process.stderr):
                stream.close()


def rpc(marker, method, params, timeout=30):
    request = {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}
    if "protocol" in marker:
        request.update({k: marker[k] for k in ("protocol", "instance", "repository")})
    with socket.create_connection(("127.0.0.1", marker["port"]), timeout=timeout) as sock:
        sock.settimeout(timeout)
        sock.sendall(json.dumps(request).encode() + b"\n")
        with sock.makefile("rb") as stream:
            line = stream.readline(64 * 1024 * 1024 + 1)
    if not line.endswith(b"\n") or len(line) > 64 * 1024 * 1024:
        raise RuntimeError("Incomplete/oversized RPC response")
    response = json.loads(line)
    if response.get("id") != 1 or "error" in response:
        raise RuntimeError(f"RPC {method}: {response}")
    result = response["result"]
    if "protocol" in marker and any(result[k] != marker[k] for k in
                                    ("protocol", "instance", "repository")):
        raise RuntimeError("RPC identity mismatch")
    return result


def canonical_matches(data):
    rows = []
    for line in data.splitlines():
        record = json.loads(line)
        if record["type"] in ("match", "context"):
            # Preserve every semantic field, including source byte offsets and
            # submatch spans. Only path separators and row order are normalized.
            record["data"]["path"]["text"] = record["data"]["path"]["text"].replace(
                "\\", "/").removeprefix("./")
            rows.append(record)
    return sorted(rows, key=lambda row: json.dumps(row, sort_keys=True))


def backend_proof(diagnostic, expected, files=False):
    via = {"ordinary": "server", "shared": "shared daemon v1", "scan": "filesystem walk"}
    if files:
        summary = r"Filename search completed(?: in [\d.]+ms)? \(via " + via[expected] + r"\)"
    elif expected == "scan":
        summary = (r"Brute-force search completed in [\d.]+ms \(\d+ files?\): "
                   r"\d+ matches \(\d+ matched lines\)")
    else:
        summary = (r"\d+ matches \(\d+ matched lines\) in [\d.]+ms \(via " +
                   via[expected] + r"\)(?: \(no index narrowing\))?")
    lines = diagnostic.splitlines()
    summaries = [line for line in lines if re.fullmatch(summary, line)]
    if len(summaries) != 1 or any(
            not re.fullmatch(summary, line) and not line.startswith("Query plan: ")
            for line in lines):
        raise RuntimeError(f"Expected {expected} backend, got diagnostics {diagnostic!r}")
    return {"backend": expected, "diagnostic": diagnostic}


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest()


def storage(paths):
    for attempt in range(5):
        try:
            files = [p for root in paths if root.exists() for p in root.rglob("*") if p.is_file()]
            return {"logical_bytes": sum(p.stat().st_size for p in files), "files": len(files)}
        except FileNotFoundError:
            # Ordinary startup reconciliation can atomically replace index files.
            if attempt == 4:
                raise
            time.sleep(.02)


def remove_fixture(root):
    def writable_retry(function, path, exception_info):
        if not isinstance(exception_info[1], PermissionError):
            raise exception_info[1]
        os.chmod(path, stat.S_IWRITE | stat.S_IREAD)
        function(path)

    # Git's loose objects are read-only on Windows. This callback changes only
    # files in the caller-owned mkdtemp fixture being removed.
    retries = []
    deadline = time.monotonic() + 5
    while True:
        try:
            shutil.rmtree(root, onerror=writable_retry)
            return retries
        except PermissionError as error:
            if time.monotonic() >= deadline:
                raise
            retries.append(str(error))
            print(f"Retrying owned fixture cleanup: {error}", file=sys.stderr)
            time.sleep(.05)


def allocation_unit(path):
    if os.name == "nt":
        volume = ctypes.create_unicode_buffer(32768)
        if not kernel.GetVolumePathNameW(str(path), volume, len(volume)):
            raise ctypes.WinError(ctypes.get_last_error())
        sectors, sector_bytes, free_clusters, total_clusters = [wt.DWORD() for _ in range(4)]
        if not kernel.GetDiskFreeSpaceW(
                volume.value, ctypes.byref(sectors), ctypes.byref(sector_bytes),
                ctypes.byref(free_clusters), ctypes.byref(total_clusters)):
            raise ctypes.WinError(ctypes.get_last_error())
        unit = sectors.value * sector_bytes.value
    else:
        info = os.statvfs(path)
        unit = info.f_frsize or info.f_bsize
    if unit <= 0:
        raise OSError("Filesystem did not report a positive allocation unit")
    return unit


def fixture_disk_reserve(files, file_bytes, count, allocation_bytes):
    if type(allocation_bytes) is not int or allocation_bytes <= 0:
        raise ValueError("Allocation unit must be a positive integer")
    unit = max(4096, allocation_bytes)
    # Allow worst-case CRLF expansion, allocation rounding and one metadata unit
    # per entry, then headroom for Git, both indexes and temporary publications.
    allocated_file = ((2 * file_bytes + unit - 1) // unit) * unit
    return (files + 8) * (allocated_file + unit) * (count + 1) * 4 + 256 * 1024 * 1024


class Fixture:
    def __init__(self, parent, args, count, scenario):
        self.root = Path(tempfile.mkdtemp(prefix="tgrep-shared-bench-", dir=parent)).resolve()
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.shared = self.root / "shared-storage"
        self.shared.mkdir()
        home = self.root / "isolated-home"
        home.mkdir()
        # Do not inherit user Git overrides, filters, templates, hooks or global ignores.
        self.env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
        self.env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull,
                        GIT_ATTR_NOSYSTEM="1", GIT_TERMINAL_PROMPT="0",
                        GIT_AUTHOR_NAME="tgrep benchmark", GIT_AUTHOR_EMAIL="bench@example.invalid",
                        GIT_COMMITTER_NAME="tgrep benchmark", GIT_COMMITTER_EMAIL="bench@example.invalid",
                        GIT_AUTHOR_DATE="2000-01-01T00:00:00Z",
                        GIT_COMMITTER_DATE="2000-01-01T00:00:00Z")
        self.env["RAYON_NUM_THREADS"] = str(args.threads)
        self.env.update(HOME=str(home), USERPROFILE=str(home),
                        XDG_CONFIG_HOME=str(home), APPDATA=str(home), LOCALAPPDATA=str(home))
        self.trees = []
        self.originals = []
        self.args = args
        self.scenario = scenario

    def git(self, *args, cwd=None):
        return command(["git", "-c", "core.hooksPath=" + os.devnull, *args],
                       cwd=cwd or self.repo, env=self.env,
                       timeout=self.args.timeout)[0].decode().strip()

    def populate(self, count):
        self.free_disk_before = shutil.disk_usage(self.root).free
        unit = allocation_unit(self.root)
        reserve = fixture_disk_reserve(self.args.files, self.args.file_bytes, count, unit)
        if self.free_disk_before < reserve:
            raise RuntimeError(f"Insufficient free disk: {self.free_disk_before} < "
                               f"{reserve} byte fixture/index reserve (allocation unit {unit})")
        self.git("init", "--quiet", "--template=")
        self.git("config", "core.autocrlf", "false")
        self.git("config", "core.safecrlf", "false")
        (self.repo / ".gitignore").write_bytes(b".tgrep/\nignored/\n")
        ending = b"crlf" if self.scenario == "crlf" else b"lf"
        (self.repo / ".gitattributes").write_bytes(b"*.txt text eol=" + ending + b"\n")
        (self.repo / ".hidden.txt").write_bytes(b"shared_token hidden\n")
        source = self.repo / "src"
        source.mkdir()
        rng = random.Random(self.args.seed)
        for i in range(self.args.files):
            content = f"symbol_{i:06d} shared_token\n"
            while len(content) < self.args.file_bytes:
                content += f"let item_{rng.randrange(100000):05d} = value_{rng.randrange(100000):05d};\n"
            (source / f"file{i:06d}.txt").write_bytes(
                (content[:self.args.file_bytes - 1] + "\n").encode())
        self.git("add", ".")
        self.git("commit", "--quiet", "-m", "deterministic benchmark base")
        self.revision = self.git("rev-parse", "HEAD")
        for i in range(count):
            tree = self.root / f"wt{i:02d}"
            self.git("worktree", "add", "--quiet", "--detach", str(tree), self.revision)
            self.trees.append(tree)
            if self.scenario == "divergent":
                (tree / "src" / "file000001.txt").write_bytes(
                    f"shared_token branch_{i}\n".encode())
                self.git("add", ".", cwd=tree)
                self.git("commit", "--quiet", "-m", f"branch {i}", cwd=tree)
                (tree / "src" / "file000002.txt").unlink()
                (tree / "private.txt").write_bytes(f"shared_token private_{i}\n".encode())
            (tree / "ignored").mkdir()
            (tree / "ignored" / "noise.txt").write_bytes(b"shared_token ignored\n")
            self.originals.append((tree / "src" / "file000000.txt").read_bytes())
        self.clean = [not self.git("status", "--porcelain", cwd=t) for t in self.trees]
        if self.scenario != "divergent" and not all(self.clean):
            raise RuntimeError("Expected clean Git worktrees, including transformed CRLF checkout")
        if self.scenario == "crlf" and not all(b"\r\n" in b for b in self.originals):
            raise RuntimeError("Git did not materialize clean CRLF worktrees")
        self.fingerprints = [self.fingerprint(t) for t in self.trees]

    @staticmethod
    def fingerprint(tree):
        return digest([(p.relative_to(tree).as_posix(), hashlib.sha256(p.read_bytes()).hexdigest())
                       for p in sorted(tree.rglob("*")) if p.is_file()
                       and ".tgrep" not in p.relative_to(tree).parts])

    def reset_churn(self):
        for tree, original in zip(self.trees, self.originals):
            (tree / "src" / "file000000.txt").write_bytes(original)
        if [self.fingerprint(t) for t in self.trees] != self.fingerprints:
            raise RuntimeError("Mode inputs differ from fixture baseline")


class Servers:
    def __init__(self, fixture, mode):
        self.fixture, self.mode = fixture, mode
        self.owned, self.logs, self.markers, self.attachments = [], [], [], []
        self.cleanup = {"processes": [], "detach_errors": [], "stop_errors": [],
                        "log_errors": [], "logs": []}

    def launch(self, root, extra):
        path = self.fixture.root / f"{self.mode}-{len(self.owned)}.log"
        stream = path.open("wb")
        self.logs.append((path, stream))
        owned = OwnedProcess([str(self.fixture.args.binary), "serve", str(root), *extra],
                             cwd=root, env=self.fixture.env, stdout=stream, stderr=stream)
        self.owned.append(owned)
        return owned

    def wait(self, read, predicate):
        deadline = time.monotonic() + self.fixture.args.timeout
        last = None
        while time.monotonic() < deadline:
            if any(p.process.poll() is not None for p in self.owned):
                raise RuntimeError("Owned server exited before readiness")
            try:
                last = read()
            except (FileNotFoundError, PermissionError, json.JSONDecodeError, ConnectionError) as error:
                last = str(error)
                time.sleep(.02)
                continue
            if predicate(last):
                return last
            time.sleep(.02)
        raise TimeoutError(f"Readiness deadline exceeded; last status: {last}")

    def statuses(self):
        if self.mode == "shared":
            return [rpc(self.markers[0], "lookup", {"root": str(t)},
                        self.fixture.args.timeout) for t in self.fixture.trees]
        return [rpc(m, "status", {}, self.fixture.args.timeout) for m in self.markers]

    def start(self):
        started = time.perf_counter()
        samples, attached = [], []
        if self.mode == "shared":
            owned = self.launch(self.fixture.repo, ["--shared", "--shared-storage",
                                                   str(self.fixture.shared)])
            path = self.fixture.repo / ".git" / "tgrep-daemon-v1.json"
            marker = self.wait(lambda: json.loads(path.read_bytes()),
                               lambda m: m["pid"] == owned.process.pid)
            hello = rpc(marker, "hello", {})
            self.markers.append(marker)
            listener_ms = (time.perf_counter() - started) * 1000
            for i, tree in enumerate(self.fixture.trees):
                lease = f"bench-{i}"
                # Register cleanup intent before attach, including response-loss paths.
                self.attachments.append((tree, lease))
                begin = time.perf_counter()
                result = self.lifecycle("attach", tree, lease, "--revision", self.fixture.revision)
                attach_ms = (time.perf_counter() - begin) * 1000
                ready = self.wait(lambda: rpc(marker, "lookup", {"root": str(tree)}),
                                  lambda s: s["ready"])
                samples.append({"view": i, "attach_build_register_ms": attach_ms,
                                "ready_ms": (time.perf_counter() - begin) * 1000})
                attached.append({"attach_build": result["attach_build"], "ready": ready})
        else:
            hello = None
            listener_ms = None
            for i, tree in enumerate(self.fixture.trees):
                begin = time.perf_counter()
                owned = self.launch(tree, [])
                marker = self.wait(lambda: json.loads((tree / ".tgrep" / "serve.json").read_bytes()),
                                   lambda m: m["pid"] == owned.process.pid)
                self.markers.append(marker)
                self.wait(lambda: rpc(marker, "status", {}),
                          lambda s: s["hidden_complete"] and not s["indexing"])
                samples.append({"view": i, "ready_ms": (time.perf_counter() - begin) * 1000})
        return {"total_ready_ms": (time.perf_counter() - started) * 1000,
                "listener_ms": metric(listener_ms) if listener_ms is not None else
                metric(reason="ordinary listeners are included in each per-view ready sample"),
                "per_view": samples,
                "attachments": attached, "hello": hello,
                "base_build_ms": metric(reason="no standalone daemon build timer; first attach "
                                              "includes build + registration, not initial reconcile"),
                "statuses": self.statuses()}

    def lifecycle(self, action, tree, lease, *extra):
        output, _ = command([self.fixture.args.binary, "shared", action, tree,
                             "--lease", lease, *extra], env=self.fixture.env,
                            timeout=self.fixture.args.timeout)
        return json.loads(output)

    def resources(self):
        per_process = [{"pid": p.process.pid, "metrics": process_resources(p.process)}
                       for p in self.owned]
        return {"processes": per_process,
                "aggregate": aggregate([p["metrics"] for p in per_process])}

    def sizes(self):
        if self.mode == "ordinary":
            return {"total": storage([t / ".tgrep" for t in self.fixture.trees])}
        return {"total": storage([self.fixture.shared]),
                "bases": storage([self.fixture.shared / "bases"]),
                "checkpoints": storage([self.fixture.shared / "overlays"])}

    def close(self):
        try:
            for tree, lease in reversed(self.attachments):
                try:
                    result = self.lifecycle("detach", tree, lease)
                    if not result["detached"] or result.get("registration_warning"):
                        raise RuntimeError(str(result))
                except Exception as error:
                    # Cleanup must continue for all owned processes, even on
                    # malformed RPC JSON. These errors fail qualification.
                    self.cleanup["detach_errors"].append(f"{type(error).__name__}: {error}")
        finally:
            try:
                for owned in reversed(self.owned):
                    try:
                        owned.stop()
                    except Exception as error:
                        self.cleanup["stop_errors"].append(f"pid={owned.process.pid}: {error}")
                    self.cleanup["processes"].append(
                        {"pid": owned.process.pid, "exited": owned.process.poll() is not None,
                         "returncode": owned.process.returncode})
            finally:
                for path, stream in self.logs:
                    try:
                        stream.close()
                        self.cleanup["logs"].append({"name": path.name, "text": path.read_text(
                            encoding="utf-8", errors="replace")[-16000:]})
                    except OSError as error:
                        self.cleanup["log_errors"].append(f"{path.name}: {error}")
                self.cleanup["ok"] = (
                    not any(self.cleanup[k] for k in ("detach_errors", "stop_errors", "log_errors"))
                    and all(p["exited"] for p in self.cleanup["processes"]))


def query(fixture, view, pattern, scan=False, context=False):
    argv = [fixture.args.binary, "--json", "--stats", "--color", "never", "-F"]
    if scan:
        argv.append("--no-index")
    if context:
        argv.extend(["-C", "1"])
    argv += ["--", pattern, "."]
    start = time.perf_counter()
    out, err = command(argv, cwd=fixture.trees[view], env=fixture.env,
                       timeout=fixture.args.timeout, allowed=(0, 1))
    elapsed = (time.perf_counter() - start) * 1000
    return canonical_matches(out), err.decode("utf-8", errors="replace"), elapsed


def equality(fixture, mode):
    checks = []
    for i, tree in enumerate(fixture.trees):
        for pattern, context in [(q, False) for q in QUERIES] + [("shared_token", True)]:
            indexed, stderr, _ = query(fixture, i, pattern, context=context)
            scanned, scan_stderr, _ = query(fixture, i, pattern, scan=True, context=context)
            backend_proof(stderr, mode)
            backend_proof(scan_stderr, "scan")
            if indexed != scanned:
                raise RuntimeError(f"Parity failure view={i} query={pattern}: "
                                   f"{stderr!r} {scan_stderr!r}; "
                                   f"indexed={indexed[:2]!r} scan={scanned[:2]!r}")
            if not context:
                expected_count = {"symbol_000007": 1, "shared_token": fixture.args.files,
                                  "absent_marker_xyz": 0}[pattern]
                if len(indexed) != expected_count:
                    raise RuntimeError(f"Unexpected fixture match count for {pattern}: "
                                       f"{len(indexed)} != {expected_count}")
            checks.append({"view": i, "query": pattern + (" -C 1" if context else ""), "equal": True,
                           "rows": len(indexed), "sha256": digest(indexed)})
        outputs = []
        for flags in ([], ["--no-index"]):
            out, err = command([fixture.args.binary, "--files", "--stats", "--hidden", *flags, "."],
                               cwd=tree, env=fixture.env, timeout=fixture.args.timeout)
            backend_proof(err.decode(errors="replace"), "scan" if flags else mode, files=True)
            outputs.append(sorted(p.replace("\\", "/").removeprefix("./")
                                  for p in out.decode().splitlines()))
        if outputs[0] != outputs[1]:
            raise RuntimeError(f"Filename parity failure view={i}")
        checks.append({"view": i, "query": "--files --hidden", "equal": True,
                       "rows": len(outputs[0]), "sha256": digest(outputs[0])})
    return checks


def idle_observation(servers, seconds):
    if not seconds:
        return {"reason": "idle observation selected only for largest LF fresh case",
                "samples": []}
    started = time.monotonic()
    before = servers.resources()
    samples, changed = [], []
    previous = [None] * len(servers.fixture.trees)
    while True:
        statuses = servers.statuses()
        elapsed = time.monotonic() - started
        observations = []
        for i, status in enumerate(statuses):
            if servers.mode == "shared":
                obs = {k: status[k] for k in ("ready", "reconcile_running",
                       "reconcile_attempts", "total_reads", "total_extractions", "last_success")}
                signature = (status["reconcile_attempts"], status["last_success"])
            else:
                obs = {k: status[k] for k in ("reconcile_running", "last_reconcile_at",
                       "last_reconcile_duration_ms", "reconcile_overdue")}
                signature = (status["last_reconcile_at"], status["last_reconcile_duration_ms"])
            observations.append(obs)
            if signature != previous[i]:
                changed.append({"elapsed_seconds": elapsed, "view": i, "status": status})
                previous[i] = signature
        samples.append({"elapsed_seconds": elapsed, "views": observations})
        if elapsed >= seconds:
            break
        time.sleep(min(1, seconds - elapsed))
    after = servers.resources()
    return {"reason": None, "samples": samples, "changed_statuses": changed,
            "resources_before": before, "resources_after": after,
            "resource_delta": resource_delta(before["aggregate"], after["aggregate"])}


def run_mode(fixture, mode, condition, result, idle_seconds):
    servers = Servers(fixture, mode)
    result.update(mode=mode, condition=condition, cleanup=servers.cleanup)
    try:
        result["startup"] = servers.start()
        result["storage_ready"] = servers.sizes()
        result["resources_ready"] = servers.resources()
        result["equality"] = equality(fixture, mode)
        result["backend_gate"] = {"indexed": mode, "control": "scan", "files_checked": True,
                                  "method": "strict positive --stats diagnostic on every invocation"}
        samples = []
        before = servers.resources()
        for repeat in range(fixture.args.samples_per_view):
            pattern = QUERIES[repeat % len(QUERIES)]
            for view in range(len(fixture.trees)):
                rows, stderr, elapsed = query(fixture, view, pattern)
                proof = backend_proof(stderr, mode)
                expected = next(c for c in result["equality"]
                                if c["view"] == view and c["query"] == pattern)
                if digest(rows) != expected["sha256"]:
                    raise RuntimeError(f"Warm query parity changed: view={view} query={pattern}")
                samples.append({"view": view, "query": pattern, "ms": elapsed, **proof})
        after = servers.resources()
        result["queries"] = {"samples": samples,
                             "latency": distribution([s["ms"] for s in samples]),
                             "resources_before": before, "resources_after": after,
                             "resource_delta": resource_delta(before["aggregate"], after["aggregate"])}
        result["churn"] = {"reason": "not the churn scenario", "samples": []}
        if fixture.scenario == "churn":
            samples, statuses = [], []
            before = servers.resources()
            for round_number in range(fixture.args.churn_rounds):
                writes = []
                for i, tree in enumerate(fixture.trees):
                    token = f"churn_{condition}_{round_number:04d}_{i:04d}"
                    started = time.perf_counter()
                    (tree / "src" / "file000000.txt").write_bytes(
                        fixture.originals[i] + token.encode() + b"\n")
                    writes.append((started, token))
                for i, (started, token) in enumerate(writes):
                    polls, diagnostics = 0, []
                    deadline = time.monotonic() + fixture.args.timeout
                    while time.monotonic() < deadline:
                        rows, stderr, _ = query(fixture, i, token)
                        polls += 1
                        try:
                            proof = backend_proof(stderr, mode)
                        except RuntimeError:
                            diagnostics.append(stderr)
                            proof = None
                        if len(rows) == 1 and proof is not None:
                            break
                        time.sleep(.02)
                    else:
                        raise TimeoutError(f"Churn never became indexed view={i} token={token}")
                    elapsed = (time.perf_counter() - started) * 1000
                    scan, stderr, _ = query(fixture, i, token, scan=True)
                    backend_proof(stderr, "scan")
                    if scan != rows:
                        raise RuntimeError(f"Churn parity failure view={i} token={token}: "
                                           f"stderr={stderr!r}, indexed={rows!r}, scan={scan!r}")
                    samples.append({"round": round_number, "view": i, "ms": elapsed,
                                    "polls": polls, "diagnostics": diagnostics, "equal": True, **proof})
                statuses.append(servers.statuses())
                time.sleep(fixture.args.churn_interval)
            after = servers.resources()
            result["churn"] = {
                "reason": None, "samples": samples, "statuses": statuses,
                "latency": distribution([s["ms"] for s in samples]),
                "resources_before": before, "resources_after": after,
                "resource_delta": resource_delta(before["aggregate"], after["aggregate"])}
            result["equality_after_churn"] = equality(fixture, mode)
        result["storage_after_churn"] = servers.sizes()
        result["refresh"] = {"reason": "ordinary server has no equivalent acknowledged hint RPC",
                             "samples": []}
        if mode == "shared":
            samples = []
            for i, (tree, lease) in enumerate(servers.attachments):
                started = time.perf_counter()
                status = servers.lifecycle("refresh", tree, lease, "--changed", "src/file000000.txt")
                samples.append({"view": i, "ms": (time.perf_counter() - started) * 1000,
                                "status": status})
            result["refresh"] = {"reason": None, "kind": "unchanged path hint acknowledgment",
                                 "samples": samples,
                                 "latency": distribution([s["ms"] for s in samples])}
        result["idle"] = idle_observation(servers, idle_seconds)
        result["storage_final"] = servers.sizes()
        result["resources_final"] = servers.resources()
        result["status_final"] = servers.statuses()
        result["ok"] = True
    finally:
        servers.close()
        result["storage_after_stop"] = servers.sizes()
        if not servers.cleanup["ok"]:
            raise RuntimeError(f"Owned server cleanup failed: {servers.cleanup}")


def require_fields(record, **fields):
    if not isinstance(record, dict):
        raise ValueError("Expected an evidence object")
    for key, kinds in fields.items():
        if type(record.get(key)) not in (kinds if isinstance(kinds, tuple) else (kinds,)):
            raise ValueError(f"Missing or invalid evidence field: {key}")
        if key not in record:
            raise ValueError(f"Missing evidence field: {key}")


def validate_parameters(params):
    require_fields(params, scenarios=list, worktrees=list, conditions=list, samples_per_view=int,
                   churn_rounds=int, churn_interval=(int, float), idle_seconds=(int, float),
                   files=int, file_bytes=int, seed=int, threads=int, timeout=(int, float),
                   binary_commit=str)
    if (not params["worktrees"] or
            any(type(n) is not int or not 1 <= n <= 32 for n in params["worktrees"])):
        raise ValueError("worktrees must contain counts in 1..32")
    if (not params["scenarios"] or
            any(type(s) is not str or s not in ("lf", "crlf", "divergent", "churn")
                for s in params["scenarios"])):
        raise ValueError("scenarios must contain lf, crlf, divergent or churn")
    if params["conditions"] not in (["fresh"], ["fresh", "restart"]):
        raise ValueError("conditions must be fresh or fresh,restart")
    for key in ("worktrees", "scenarios"):
        if len(params[key]) != len(set(params[key])):
            raise ValueError(f"Duplicate {key} are not allowed")
    for key, lower, upper in (
            ("files", 8, MAX_FILES), ("samples_per_view", 1, 1000), ("churn_rounds", 1, 100),
            ("churn_interval", 0, 10), ("idle_seconds", 0, 600), ("timeout", 1, 600),
            ("threads", 1, 8)):
        if not lower <= params[key] <= upper:
            raise ValueError(f"{key} must be {lower}..{upper}")
    if params["file_bytes"] < 256 or params["files"] * params["file_bytes"] > MAX_TREE_BYTES:
        raise ValueError("file_bytes must be >=256 with <=32 MiB generated text per tree")
    if not re.fullmatch("[0-9a-f]{40}", params["binary_commit"]):
        raise ValueError("binary_commit must be a full lowercase SHA")


def validate_mode_evidence(mode, case, params):
    name, count = mode["mode"], case["worktrees"]
    require_fields(mode, ok=bool, condition=str, equality=list, queries=dict,
                   startup=dict, storage_ready=dict, storage_after_churn=dict,
                   storage_final=dict, storage_after_stop=dict, resources_ready=dict,
                   resources_final=dict, status_final=list, backend_gate=dict,
                   churn=dict, refresh=dict, idle=dict, cleanup=dict)
    if mode["condition"] != case["condition"]:
        raise ValueError("Mode/case condition mismatch")

    def numbers(values):
        for value in values:
            metric(value)

    def counters(value, names, **extra):
        require_fields(value, **dict.fromkeys(names.split(), int), **extra)
        numbers(value[k] for k in names.split())

    def metrics(values, keys=RESOURCE_KEYS):
        if not isinstance(values, dict) or set(values) != set(keys):
            raise ValueError("Incomplete resource metrics")
        for item in values.values():
            require_fields(item, value=(int, float, type(None)), reason=(str, type(None)))
            metric(item["value"], item["reason"])

    def snapshot(value, expected_pids=None):
        require_fields(value, processes=list, aggregate=dict)
        pids = []
        for process in value["processes"]:
            require_fields(process, pid=int, metrics=dict)
            if process["pid"] <= 0:
                raise ValueError("Invalid resource PID")
            pids.append(process["pid"])
            metrics(process["metrics"])
        if (len(pids) != (count if name == "ordinary" else 1) or
                len(set(pids)) != len(pids) or
                (expected_pids is not None and set(pids) != expected_pids)):
            raise ValueError("Resource process inventory mismatch")
        metrics(value["aggregate"])
        if value["aggregate"] != aggregate([p["metrics"] for p in value["processes"]]):
            raise ValueError("Incorrect resource aggregation")
        return set(pids)

    pids = snapshot(mode["resources_ready"])
    snapshot(mode["resources_final"], pids)

    def resources(phase):
        require_fields(phase, resources_before=dict, resources_after=dict, resource_delta=dict)
        for key in ("resources_before", "resources_after"):
            snapshot(phase[key], pids)
        metrics(phase["resource_delta"], ("cpu_seconds", "read_transfer_bytes", "storage_read_bytes"))
        if phase["resource_delta"] != resource_delta(
                phase["resources_before"]["aggregate"], phase["resources_after"]["aggregate"]):
            raise ValueError("Incorrect resource delta")

    def samples(phase, active):
        require_fields(phase, samples=list, reason=(str, type(None)))
        if not active:
            if phase["samples"] or not phase["reason"]:
                raise ValueError("Inactive phase needs empty samples and an unavailable reason")
            return
        if phase["reason"] is not None or not phase["samples"]:
            raise ValueError("Active phase requires samples, not an unavailable reason")
        latency(phase)

    def latency(phase):
        require_fields(phase, samples=list, latency=dict)
        for sample in phase["samples"]:
            require_fields(sample, ms=(int, float), view=int)
            numbers([sample["ms"]])
            if not 0 <= sample["view"] < count:
                raise ValueError("Invalid sample view")
        if phase["latency"] != distribution([s["ms"] for s in phase["samples"]]):
            raise ValueError("Incorrect latency aggregation")

    build_fields = ("blob_bytes_read blobs_extracted blobs_read postings_reused "
                    "predecessor_posting_lists_read reused_indexed_files tracked_entries")
    reconcile_fields = ("base_files_copied base_reused bytes_read content_reads_avoided epoch "
                        "files_decoded files_discovered files_extracted files_read hint_lookups "
                        "overlay_reused postings_copied")

    def status(value, require_ready=False):
        require_fields(value, reconcile_running=bool)
        if name == "ordinary":
            counters(value, "num_files", hidden_complete=bool, indexing=bool,
                     last_reconcile_at=(int, type(None)),
                     last_reconcile_duration_ms=(int, type(None)), reconcile_overdue=bool)
            if require_ready and (not value["hidden_complete"] or value["indexing"]):
                raise ValueError("Ordinary status is not query-ready")
        else:
            counters(value, "base_sharing_views epoch published_epoch reconcile_attempts "
                     "total_reads total_extractions", ready=bool, root=str, view=str,
                     generation_build=dict, last_reconcile=dict,
                     last_success=(int, type(None)))
            counters(value["generation_build"], build_fields, published=bool, reused_generation=bool)
            counters(value["last_reconcile"], reconcile_fields, full=bool)
            if ((require_ready and not value["ready"]) or not 1 <= value["base_sharing_views"] <= count or
                    value["published_epoch"] > value["epoch"] or
                    value["last_reconcile"]["epoch"] > value["epoch"] or
                    value["total_reads"] < value["last_reconcile"]["files_read"] or
                    value["total_extractions"] < value["last_reconcile"]["files_extracted"]):
                raise ValueError("Inconsistent shared readiness/counters")

    def statuses(values):
        if not isinstance(values, list) or len(values) != count:
            raise ValueError("Incomplete per-view statuses")
        for value in values:
            status(value)
        if name == "shared" and (len({s["root"] for s in values}) != count or
                                 len({s["view"] for s in values}) != count):
            raise ValueError("Duplicate shared status views")

    startup = mode["startup"]
    require_fields(startup, total_ready_ms=(int, float), listener_ms=dict, base_build_ms=dict,
                   per_view=list, attachments=list, statuses=list, hello=(dict, type(None)))
    numbers([startup["total_ready_ms"]])
    for key in ("listener_ms", "base_build_ms"):
        require_fields(startup[key], value=(int, float, type(None)), reason=(str, type(None)))
        metric(**startup[key])
    if len(startup["per_view"]) != count:
        raise ValueError("Incomplete startup samples")
    for i, sample in enumerate(startup["per_view"]):
        require_fields(sample, view=int, ready_ms=(int, float))
        numbers([sample["ready_ms"]])
        if sample["view"] != i:
            raise ValueError("Startup view mismatch")
        if name == "shared":
            require_fields(sample, attach_build_register_ms=(int, float))
            numbers([sample["attach_build_register_ms"]])
            if sample["ready_ms"] < sample["attach_build_register_ms"]:
                raise ValueError("Readiness precedes attach")
    if startup["total_ready_ms"] < sum(s["ready_ms"] for s in startup["per_view"]):
        raise ValueError("Total startup omits per-view readiness")
    if name == "shared":
        require_fields(startup["hello"], protocol=int, limits=dict, capabilities=list)
        counters(startup["hello"]["limits"], "reconcile_workers query_workers views")
        if (len(startup["attachments"]) != count or startup["listener_ms"]["value"] is None or
                startup["listener_ms"]["value"] > startup["total_ready_ms"]):
            raise ValueError("Missing shared attach/listener evidence")
        for attachment in startup["attachments"]:
            require_fields(attachment, attach_build=dict, ready=dict)
            counters(attachment["attach_build"], build_fields, published=bool, reused_generation=bool)
            status(attachment["ready"], require_ready=True)
    elif startup["attachments"] or startup["hello"] is not None or startup["listener_ms"]["value"] is not None:
        raise ValueError("Unexpected ordinary attach/listener evidence")
    statuses(startup["statuses"])
    statuses(mode["status_final"])
    for key in ("storage_ready", "storage_after_churn", "storage_final", "storage_after_stop"):
        parts = ("total", "bases", "checkpoints") if name == "shared" else ("total",)
        require_fields(mode[key], **dict.fromkeys(parts, dict))
        for part in parts:
            counters(mode[key][part], "logical_bytes files")

    require_fields(mode["backend_gate"], indexed=str, control=str, files_checked=bool, method=str)
    if (mode["backend_gate"]["indexed"] != name or mode["backend_gate"]["control"] != "scan" or
            not mode["backend_gate"]["files_checked"] or not mode["backend_gate"]["method"]):
        raise ValueError("Incomplete backend gate")
    latency(mode["queries"])
    resources(mode["queries"])
    if [(s["view"], s["query"]) for s in mode["queries"]["samples"]] != [
            (v, QUERIES[r % len(QUERIES)])
            for r in range(params["samples_per_view"]) for v in range(count)]:
        raise ValueError("Query sample coverage/order mismatch")

    churn = mode["churn"]
    samples(churn, case["scenario"] == "churn")
    if case["scenario"] == "churn":
        resources(churn)
        require_fields(churn, statuses=list)
        if len(churn["statuses"]) != params["churn_rounds"]:
            raise ValueError("Incomplete churn status rounds")
        for values in churn["statuses"]:
            statuses(values)
        for sample in churn["samples"]:
            require_fields(sample, polls=int, diagnostics=list, backend=str)
            if sample["polls"] < 1 or sample["backend"] != name or not all(
                    isinstance(d, str) for d in sample["diagnostics"]):
                raise ValueError("Invalid churn polling evidence")

    refresh = mode["refresh"]
    samples(refresh, name == "shared")
    if name == "shared":
        require_fields(refresh, kind=str)
        if [s["view"] for s in refresh["samples"]] != list(range(count)):
            raise ValueError("Incomplete refresh samples")
        for sample in refresh["samples"]:
            require_fields(sample, status=dict)
            status(sample["status"])
            counters(sample["status"], "processed_epoch")
            if sample["status"]["processed_epoch"] > sample["status"]["epoch"]:
                raise ValueError("Invalid refresh acknowledgement epoch")

    idle = mode["idle"]
    seconds = (params["idle_seconds"] if case["scenario"] == "lf" and
               count == max(params["worktrees"]) and case["condition"] == "fresh" else 0)
    require_fields(idle, reason=(str, type(None)), samples=list)
    if not seconds:
        samples(idle, False)
    else:
        if idle["reason"] is not None or not idle["samples"]:
            raise ValueError("Missing requested idle observations")
        resources(idle)
        require_fields(idle, changed_statuses=list)
        previous, changes, elapsed = [None] * count, [], -1
        for sample in idle["samples"]:
            require_fields(sample, elapsed_seconds=(int, float), views=list)
            numbers([sample["elapsed_seconds"]])
            if sample["elapsed_seconds"] < elapsed or len(sample["views"]) != count:
                raise ValueError("Incomplete or unordered idle observations")
            elapsed = sample["elapsed_seconds"]
            for i, value in enumerate(sample["views"]):
                require_fields(value, reconcile_running=bool)
                if name == "shared":
                    counters(value, "reconcile_attempts total_reads total_extractions",
                             ready=bool, last_success=(int, type(None)))
                    signature = (value["reconcile_attempts"], value["last_success"])
                else:
                    require_fields(value, last_reconcile_at=(int, type(None)),
                                   last_reconcile_duration_ms=(int, type(None)), reconcile_overdue=bool)
                    signature = (value["last_reconcile_at"], value["last_reconcile_duration_ms"])
                if signature != previous[i]:
                    changes.append((elapsed, i, value))
                    previous[i] = signature
        if elapsed < seconds or len(idle["changed_statuses"]) != len(changes):
            raise ValueError("Incomplete idle duration/change evidence")
        for event, (elapsed, view, observation) in zip(idle["changed_statuses"], changes):
            require_fields(event, elapsed_seconds=(int, float), view=int, status=dict)
            status(event["status"])
            if (event["elapsed_seconds"] != elapsed or event["view"] != view or
                    any(event["status"][k] != v for k, v in observation.items())):
                raise ValueError("Idle change evidence disagrees with observations")

    cleanup = mode["cleanup"]
    require_fields(cleanup, ok=bool, processes=list, detach_errors=list, stop_errors=list,
                   log_errors=list, logs=list)
    for process in cleanup["processes"]:
        require_fields(process, pid=int, exited=bool, returncode=int)
        if not process["exited"]:
            raise ValueError("Cleanup process did not exit")
    if (not cleanup["ok"] or any(cleanup[k] for k in ("detach_errors", "stop_errors", "log_errors")) or
            len(cleanup["processes"]) != len(pids) or
            {p["pid"] for p in cleanup["processes"]} != pids):
        raise ValueError("Cleanup evidence does not cover owned processes")
    for log in cleanup["logs"]:
        require_fields(log, name=str, text=str)
    if (len(cleanup["logs"]) != len(pids) or
            {s["name"] for s in cleanup["logs"]} != {f"{name}-{i}.log" for i in range(len(pids))}):
        raise ValueError("Incomplete cleanup logs")


def validate(report, *, finalized=True):
    require_fields(report, schema=str, cases=list, parameters=dict, cleanup=dict, binary=dict,
                   host=dict, harness_sha256=str, queries=list, semantics=dict, unavailable=dict,
                   started_utc=str)
    if finalized:
        require_fields(report, ok=bool, error=type(None), finished_utc=str)
        if not report["ok"]:
            raise ValueError("Report is not successful")
    if report.get("schema") != SCHEMA or not report.get("cases"):
        raise ValueError("Missing schema/cases")
    params = report["parameters"]
    validate_parameters(params)
    require_fields(report["binary"], path=str, source_commit_attested=str, sha256=str, version=str)
    for value, length in ((report["harness_sha256"], 64), (report["binary"]["sha256"], 64),
                          (report["binary"]["source_commit_attested"], 40)):
        if not re.fullmatch(f"[0-9a-f]{{{length}}}", value):
            raise ValueError("Invalid source/binary provenance hash")
    if params["binary_commit"] != report["binary"]["source_commit_attested"] or report["queries"] != QUERIES:
        raise ValueError("Inconsistent binary/query provenance")
    require_fields(report["host"], platform=str, machine=str, processor=str, python=str,
                   logical_cpus=(int, type(None)), git=str, rustc=dict, cargo=dict, note=str)
    for key in ("rustc", "cargo"):
        value = report["host"][key]
        require_fields(value, value=(str, type(None)), reason=(str, type(None)))
        if ((value["value"] is None and not value["reason"]) or
                (value["value"] is not None and value["reason"] is not None)):
            raise ValueError("Invalid optional tool provenance")
    require_fields(report["unavailable"], ordinary_extraction_counters=dict,
                   whole_process_tree_cpu_io=dict)
    for value in report["unavailable"].values():
        require_fields(value, value=(int, float, type(None)), reason=(str, type(None)))
        metric(**value)
    for case in report["cases"]:
        require_fields(case, scenario=str, worktrees=int, condition=str, modes=list, pair_equal=bool,
                       revision=str, fixture_sha256=list, git_clean=list, order=list,
                       free_disk_before_fixture_bytes=int)
        if (not re.fullmatch("[0-9a-f]{40}", case["revision"]) or
                len(case["fixture_sha256"]) != case["worktrees"] or
                any(not isinstance(s, str) or not re.fullmatch("[0-9a-f]{64}", s)
                    for s in case["fixture_sha256"]) or len(case["git_clean"]) != case["worktrees"] or
                any(type(c) is not bool for c in case["git_clean"])):
            raise ValueError("Invalid per-view fixture provenance")
        for mode in case["modes"]:
            require_fields(mode, mode=str, equality=list)
        if case["order"] != [m["mode"] for m in case["modes"]]:
            raise ValueError("Mode execution order mismatch")
    expected = {(s, n, c) for s in params["scenarios"] for n in params["worktrees"]
                for c in params["conditions"]}
    actual = {(c["scenario"], c["worktrees"], c["condition"]) for c in report["cases"]}
    if actual != expected or len(report["cases"]) != len(expected):
        raise ValueError("Incomplete or duplicate matrix")
    for case in report["cases"]:
        if not case["pair_equal"] or len(case["modes"]) != 2:
            raise ValueError("Missing successful paired equality")
        if {m["mode"] for m in case["modes"]} != {"ordinary", "shared"}:
            raise ValueError("Expected one ordinary and one shared mode")
        if case["modes"][0]["equality"] != case["modes"][1]["equality"]:
            raise ValueError("Paired equality records differ")
        for mode in case["modes"]:
            validate_mode_evidence(mode, case, params)
            if not mode.get("ok") or not mode["cleanup"]["ok"]:
                raise ValueError("Incomplete run or cleanup")
            if not mode["equality"] or not all(c["equal"] for c in mode["equality"]):
                raise ValueError("Missing successful scan equality")
            expected_checks = {(v, q) for v in range(case["worktrees"])
                               for q in QUERIES + ["shared_token -C 1", "--files --hidden"]}
            if {(c["view"], c["query"]) for c in mode["equality"]} != expected_checks:
                raise ValueError("Incomplete per-view scan equality")
            if len(mode["equality"]) != len(expected_checks):
                raise ValueError("Duplicate per-view scan equality")
            for check in mode["equality"]:
                require_fields(check, equal=bool, view=int, query=str, rows=int, sha256=str)
                metric(check["rows"])
                if not re.fullmatch("[0-9a-f]{64}", check["sha256"]):
                    raise ValueError("Invalid equality digest")
            queries = mode["queries"]
            if len(queries["samples"]) != case["worktrees"] * params["samples_per_view"]:
                raise ValueError("Sample count mismatch")
            if distribution([s["ms"] for s in queries["samples"]]) != queries["latency"]:
                raise ValueError("Incorrect latency aggregation")
            for sample in queries["samples"]:
                backend_proof(sample["diagnostic"], mode["mode"])
                if sample["backend"] != mode["mode"]:
                    raise ValueError("Query backend mismatch")
            if set(mode["resources_final"]["aggregate"]) != set(RESOURCE_KEYS):
                raise ValueError("Incomplete resource metrics")
            for item in mode["resources_final"]["aggregate"].values():
                metric(item["value"], item["reason"])
            if case["scenario"] == "churn":
                samples = mode["churn"]["samples"]
                for sample in samples:
                    require_fields(sample, round=int, view=int, equal=bool, diagnostic=str)
                expected_churn = {(r, v) for r in range(params["churn_rounds"])
                                  for v in range(case["worktrees"])}
                if (len(samples) != len(expected_churn) or
                        {(s["round"], s["view"]) for s in samples} != expected_churn or
                        not all(s["equal"] for s in samples)):
                    raise ValueError("Incomplete churn samples/equality")
                for sample in samples:
                    backend_proof(sample["diagnostic"], mode["mode"])
                if distribution([s["ms"] for s in samples]) != mode["churn"]["latency"]:
                    raise ValueError("Incorrect churn aggregation")
                if mode["equality_after_churn"] != mode["equality"]:
                    raise ValueError("Post-churn corpus parity missing/changed")
    require_fields(report["cleanup"], ok=bool, fixtures=list)
    fixtures = report["cleanup"]["fixtures"]
    for fixture in fixtures:
        require_fields(fixture, path=str, removed=bool, permission_retries=list)
        if not fixture["removed"] or not fixture["path"] or not all(
                isinstance(r, str) for r in fixture["permission_retries"]):
            raise ValueError("Missing fixture cleanup evidence")
    if (not report["cleanup"]["ok"] or len(fixtures) != len(params["scenarios"]) * len(params["worktrees"]) or
            len({f["path"] for f in fixtures}) != len(fixtures)):
        raise ValueError("Fixture cleanup failed")


def tool_version(name):
    executable = shutil.which(name)
    if executable is None:
        return {"value": None, "reason": f"Optional {name} is not executable on harness PATH"}
    try:
        return {"value": command([executable, "--version"], timeout=5)[0].decode().strip(),
                "reason": None}
    except CommandError as error:
        return {"value": None, "reason": f"Optional {name} provenance unavailable: {error}"}


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--binary-commit", required=True,
                        help="Exact 40-hex source commit used to build --binary (caller attestation)")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--worktrees", default="1,4,16,32")
    parser.add_argument("--scenarios", default="lf,crlf,divergent,churn")
    parser.add_argument("--conditions", default="fresh,restart")
    parser.add_argument("--files", type=int, default=128,
                        help=f"Synthetic text files per tree (8..{MAX_FILES})")
    parser.add_argument("--file-bytes", type=int, default=4096)
    parser.add_argument("--seed", type=int, default=20261005)
    parser.add_argument("--threads", type=int, default=2,
                        help="RAYON_NUM_THREADS per process in both modes (bounds shared-host load)")
    parser.add_argument("--samples-per-view", type=int, default=9)
    parser.add_argument("--churn-rounds", type=int, default=6)
    parser.add_argument("--churn-interval", type=float, default=.1)
    parser.add_argument("--idle-seconds", type=float, default=130,
                        help="Observe largest LF fresh pair; 0 skips, 130 covers default 120s pass")
    parser.add_argument("--timeout", type=float, default=60,
                        help="Per-command/readiness deadline, seconds")
    parser.add_argument("--temp-parent", type=Path)
    parser.add_argument("--host-note", default="Shared machine; unrelated host activity not controlled")
    args = parser.parse_args(argv)
    args.binary = args.binary.resolve(strict=True)
    args.output = args.output.resolve()
    if args.output.exists():
        parser.error("--output must not already exist")
    try:
        args.worktrees = [int(n) for n in args.worktrees.split(",")]
    except ValueError:
        parser.error("--worktrees must be comma-separated integers")
    args.scenarios = args.scenarios.split(",")
    args.conditions = args.conditions.split(",")
    try:
        validate_parameters(vars(args))
    except ValueError as error:
        parser.error(str(error))
    return args


def main(argv=None):
    args = parse_args(argv)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    report = {"schema": SCHEMA, "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
              "parameters": {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
              "binary": {"path": str(args.binary), "source_commit_attested": args.binary_commit,
                         "sha256": hashlib.sha256(args.binary.read_bytes()).hexdigest(),
                         "version": command([args.binary, "--version"])[0].decode().strip()},
              "host": {"platform": platform.platform(), "machine": platform.machine(),
                       "processor": platform.processor(),
                       "logical_cpus": os.cpu_count(), "python": sys.version,
                       "git": command(["git", "--version"])[0].decode().strip(),
                       "rustc": tool_version("rustc"), "cargo": tool_version("cargo"),
                       "loadavg_start": list(os.getloadavg()) if hasattr(os, "getloadavg") else None,
                       "note": args.host_note},
              "harness_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              "queries": QUERIES, "cases": [], "cleanup": {"ok": True, "fixtures": []},
              "semantics": {
                  "condition": "fresh=no persisted index; restart=new processes, retained indexes/checkpoints; "
                               "OS caches never purged; Git fixture creation already warms file cache",
                  "queries": "fresh CLI process per sample, --json --stats -F --color never; includes discovery, "
                             "Git children, TCP, formatting and capture; warm after canonical match/context "
                             "scan parity including offsets/spans; nearest-rank percentiles",
                  "resources": "sum of long-lived owned server processes only; excludes harness, CLI clients "
                               "and transient Git children from CPU/memory; Linux proc I/O may include "
                               "waited-for children per kernel accounting; wall latencies include their work",
                  "memory": "endpoint samples, not peaks. RSS/working-set sum is not physical mmap dedup. "
                            "Linux private=RssAnon, PSS=smaps_rollup; Windows private=PrivateUsage",
                  "io": "Linux transfer=rchar (syscall bytes, including pipes); storage=read_bytes "
                        "(kernel storage attribution, not all mapped/cache traffic). "
                        "Windows transfer=ReadTransferCount (cached and non-file I/O too). "
                        "Neither is a direct device measurement",
                  "counters": "shared status: actual successful reconcile reads/extractions; failed attempts "
                              "not included in totals. Ordinary has no corresponding extraction counters",
                  "churn": "bounded all-view write bursts, then serial indexed visibility polls and scan checks; "
                           "latency is an observed upper bound including head-of-line measurement delay",
                  "storage": "best-effort file walk, retries concurrent atomic replacement; logical file lengths "
                             "not allocated blocks; excludes fixture and Git metadata",
              },
              "unavailable": {"ordinary_extraction_counters": metric(
                  reason="ordinary server status does not expose extraction/read counters"),
                  "whole_process_tree_cpu_io": metric(
                      reason="transient client/Git process resource accounting is not portable in this harness")}}
    error = None
    try:
        for scenario in args.scenarios:
            for count in args.worktrees:
                fixture = Fixture(args.temp_parent, args, count, scenario)
                cleanup = {"path": str(fixture.root), "removed": False}
                report["cleanup"]["fixtures"].append(cleanup)
                try:
                    fixture.populate(count)
                    # Alternate mode order between counts to expose, not hide, cache-order bias.
                    order = ["ordinary", "shared"] if args.worktrees.index(count) % 2 == 0 else [
                        "shared", "ordinary"]
                    for condition in args.conditions:
                        case = {"scenario": scenario, "worktrees": count, "condition": condition,
                                "revision": fixture.revision, "fixture_sha256": fixture.fingerprints,
                                "free_disk_before_fixture_bytes": fixture.free_disk_before,
                                "git_clean": fixture.clean, "order": order, "modes": [],
                                "pair_equal": False}
                        report["cases"].append(case)
                        for mode in order:
                            print(f"{scenario} {count} {condition} {mode}", file=sys.stderr, flush=True)
                            fixture.reset_churn()
                            result = {}
                            case["modes"].append(result)
                            idle = (args.idle_seconds if scenario == "lf" and
                                    count == max(args.worktrees) and condition == "fresh" else 0)
                            run_mode(fixture, mode, condition, result, idle)
                        case["pair_equal"] = case["modes"][0]["equality"] == case["modes"][1]["equality"]
                        if not case["pair_equal"]:
                            raise RuntimeError("Ordinary/shared outputs differ")
                finally:
                    if any(m["cleanup"].get("stop_errors") for c in report["cases"]
                           for m in c["modes"] if "cleanup" in m):
                        raise RuntimeError(f"Retaining fixture {fixture.root}: process-tree cleanup failed")
                    cleanup["permission_retries"] = remove_fixture(fixture.root)
                    cleanup["removed"] = not fixture.root.exists()
                    report["cleanup"]["ok"] &= cleanup["removed"]
        # The final status envelope is written below, after qualification succeeds.
        validate(report, finalized=False)
    except (Exception, KeyboardInterrupt) as caught:
        error = f"{type(caught).__name__}: {caught}"
        print(error, file=sys.stderr)
    finally:
        report["error"] = error
        report["ok"] = error is None
        report["finished_utc"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        report["host"]["loadavg_end"] = list(os.getloadavg()) if hasattr(os, "getloadavg") else None
        report["cleanup"]["ok"] &= all(f["removed"] for f in report["cleanup"]["fixtures"])
        with args.output.open("x", encoding="utf-8") as stream:
            json.dump(report, stream, indent=2, allow_nan=False)
            stream.write("\n")
    if error is None:
        print("scenario trees condition ordinary_p95_ms shared_p95_ms")
        for case in report["cases"]:
            modes = {m["mode"]: m for m in case["modes"]}
            print(case["scenario"], case["worktrees"], case["condition"],
                  *(round(modes[m]["queries"]["latency"]["p95_ms"], 3)
                    for m in ("ordinary", "shared")))
    return 1 if error else 0


if __name__ == "__main__":
    def interrupted(signum, frame):
        raise KeyboardInterrupt(f"signal {signum}")

    signal.signal(signal.SIGTERM, interrupted)
    raise SystemExit(main())
