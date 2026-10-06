"""Owned Windows process-tree lifetime for qualification, including exited parents."""

import ctypes
from ctypes import wintypes
import time


class BasicLimits(ctypes.Structure):
    _fields_ = [
        ("process_time", ctypes.c_int64), ("job_time", ctypes.c_int64),
        ("flags", wintypes.DWORD), ("min_working_set", ctypes.c_size_t),
        ("max_working_set", ctypes.c_size_t), ("active_limit", wintypes.DWORD),
        ("affinity", ctypes.c_size_t), ("priority", wintypes.DWORD),
        ("scheduling", wintypes.DWORD),
    ]


class IoCounters(ctypes.Structure):
    _fields_ = [(name, ctypes.c_uint64) for name in (
        "read_ops", "write_ops", "other_ops", "read_bytes", "write_bytes", "other_bytes",
    )]


class ExtendedLimits(ctypes.Structure):
    _fields_ = [
        ("basic", BasicLimits), ("io", IoCounters),
        ("process_memory", ctypes.c_size_t), ("job_memory", ctypes.c_size_t),
        ("peak_process_memory", ctypes.c_size_t), ("peak_job_memory", ctypes.c_size_t),
    ]


class Accounting(ctypes.Structure):
    _fields_ = [
        ("user_time", ctypes.c_int64), ("kernel_time", ctypes.c_int64),
        ("period_user_time", ctypes.c_int64), ("period_kernel_time", ctypes.c_int64),
        ("page_faults", wintypes.DWORD), ("total", wintypes.DWORD),
        ("active", wintypes.DWORD), ("terminated", wintypes.DWORD),
    ]


class ThreadEntry(ctypes.Structure):
    _fields_ = [
        ("size", wintypes.DWORD), ("usage", wintypes.DWORD),
        ("thread_id", wintypes.DWORD), ("process_id", wintypes.DWORD),
        ("base_priority", wintypes.LONG), ("delta_priority", wintypes.LONG),
        ("flags", wintypes.DWORD),
    ]


class WindowsJob:
    def __init__(self):
        self.kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        signatures = {
            "CreateJobObjectW": (wintypes.HANDLE, [ctypes.c_void_p, wintypes.LPCWSTR]),
            "SetInformationJobObject": (wintypes.BOOL, [
                wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD,
            ]),
            "QueryInformationJobObject": (wintypes.BOOL, [
                wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD, ctypes.c_void_p,
            ]),
            "AssignProcessToJobObject": (wintypes.BOOL, [wintypes.HANDLE, wintypes.HANDLE]),
            "TerminateJobObject": (wintypes.BOOL, [wintypes.HANDLE, wintypes.UINT]),
            "CloseHandle": (wintypes.BOOL, [wintypes.HANDLE]),
            "OpenProcess": (wintypes.HANDLE, [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]),
            "CreateToolhelp32Snapshot": (wintypes.HANDLE, [wintypes.DWORD, wintypes.DWORD]),
            "Thread32First": (wintypes.BOOL, [wintypes.HANDLE, ctypes.POINTER(ThreadEntry)]),
            "Thread32Next": (wintypes.BOOL, [wintypes.HANDLE, ctypes.POINTER(ThreadEntry)]),
            "OpenThread": (wintypes.HANDLE, [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]),
            "ResumeThread": (wintypes.DWORD, [wintypes.HANDLE]),
        }
        for name, (result, args) in signatures.items():
            function = getattr(self.kernel, name)
            function.restype = result
            function.argtypes = args
        self.handle = self.checked(self.kernel.CreateJobObjectW(None, None))
        limits = ExtendedLimits()
        limits.basic.flags = 0x2000  # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        try:
            self.checked(self.kernel.SetInformationJobObject(
                self.handle, 9, ctypes.byref(limits), ctypes.sizeof(limits),
            ))
        except BaseException:
            self.close()
            raise

    @staticmethod
    def checked(result):
        if not result:
            raise ctypes.WinError(ctypes.get_last_error())
        return result

    def assign_and_resume(self, pid):
        # The child was created suspended: it cannot spawn outside the job first.
        process = self.checked(self.kernel.OpenProcess(0x0101, False, pid))
        try:
            self.checked(self.kernel.AssignProcessToJobObject(self.handle, process))
        finally:
            self.checked(self.kernel.CloseHandle(process))
        snapshot = self.kernel.CreateToolhelp32Snapshot(4, 0)  # TH32CS_SNAPTHREAD
        if snapshot == ctypes.c_void_p(-1).value:
            raise ctypes.WinError(ctypes.get_last_error())
        try:
            entry = ThreadEntry()
            entry.size = ctypes.sizeof(entry)
            found = self.kernel.Thread32First(snapshot, ctypes.byref(entry))
            while found:
                if entry.process_id == pid:
                    thread = self.checked(self.kernel.OpenThread(2, False, entry.thread_id))
                    try:
                        previous = self.kernel.ResumeThread(thread)
                        if previous == 0xFFFFFFFF:
                            raise ctypes.WinError(ctypes.get_last_error())
                        if previous != 1:
                            raise RuntimeError(f"unexpected child suspension count: {previous}")
                        return
                    finally:
                        self.checked(self.kernel.CloseHandle(thread))
                entry.size = ctypes.sizeof(entry)
                found = self.kernel.Thread32Next(snapshot, ctypes.byref(entry))
            error = ctypes.get_last_error()
            if error != 18:  # ERROR_NO_MORE_FILES
                raise ctypes.WinError(error)
            raise RuntimeError(f"no suspended primary thread for owned process {pid}")
        finally:
            self.checked(self.kernel.CloseHandle(snapshot))

    def terminate(self):
        self.checked(self.kernel.TerminateJobObject(self.handle, 1))
        deadline = time.monotonic() + 10
        while True:
            accounting = Accounting()
            self.checked(self.kernel.QueryInformationJobObject(
                self.handle, 1, ctypes.byref(accounting), ctypes.sizeof(accounting), None,
            ))
            if accounting.active == 0:
                return
            if time.monotonic() >= deadline:
                raise RuntimeError(f"owned job still has {accounting.active} processes")
            time.sleep(0.01)

    def close(self):
        self.checked(self.kernel.CloseHandle(self.handle))
