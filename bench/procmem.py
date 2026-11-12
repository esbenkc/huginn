"""Process-tree memory sampling on macOS via libproc (no fork per sample).

ProcessTreeSampler(root_pid) runs a thread that, every `interval` seconds,
finds every process in the tree under root_pid (children by ppid, recursively,
plus any process in root_pid's process group) and sums their current
resident size and phys_footprint. It keeps the peak of each sum.

Why both numbers:
- resident_size (RSS) is what `ps` and `/usr/bin/time -l` report. Summing RSS
  over processes counts shared pages (system frameworks, shared memory) once per
  process, so it overstates multi-process browsers.
- phys_footprint is what Activity Monitor calls "Memory": private dirty +
  compressed + IOKit/GPU memory attributed to the process. It does not double
  count shared framework pages.
"""
import ctypes
import threading
import time

_libproc = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
_libproc.proc_listchildpids.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_int]
_libproc.proc_listpids.argtypes = [ctypes.c_uint32, ctypes.c_uint32, ctypes.c_void_p, ctypes.c_int]
_libproc.proc_pid_rusage.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_void_p]

PROC_PGRP_ONLY = 2
RUSAGE_INFO_V4 = 4
_MAXPIDS = 4096


class _RusageV4(ctypes.Structure):
    _fields_ = [("uuid", ctypes.c_uint8 * 16), ("f", ctypes.c_uint64 * 36)]


_RESIDENT = 6
_FOOTPRINT = 7
_LIFETIME_MAX_FOOTPRINT = 28


def _children(pid, buf):
    ctypes.memset(buf, 0, ctypes.sizeof(buf))
    n = _libproc.proc_listchildpids(pid, buf, ctypes.sizeof(buf))
    if n <= 0:
        return []
    return [p for p in buf if p > 0]


def _pgrp(pgid, buf):
    ctypes.memset(buf, 0, ctypes.sizeof(buf))
    n = _libproc.proc_listpids(PROC_PGRP_ONLY, pgid, buf, ctypes.sizeof(buf))
    if n <= 0:
        return []
    return [p for p in buf if p > 0]


def tree_pids(root, pgid=None):
    buf = (ctypes.c_int * _MAXPIDS)()
    seen = {root}
    stack = [root]
    while stack:
        p = stack.pop()
        for c in _children(p, buf):
            if c not in seen:
                seen.add(c)
                stack.append(c)
    if pgid is not None:
        for p in _pgrp(pgid, buf):
            if p not in seen:
                seen.add(p)
                # Pick up children of group members too.
                stack.append(p)
        while stack:
            p = stack.pop()
            for c in _children(p, buf):
                if c not in seen:
                    seen.add(c)
                    stack.append(c)
    return seen


def rusage(pid):
    info = _RusageV4()
    if _libproc.proc_pid_rusage(pid, RUSAGE_INFO_V4, ctypes.byref(info)) != 0:
        return None
    return info.f[_RESIDENT], info.f[_FOOTPRINT], info.f[_LIFETIME_MAX_FOOTPRINT]


class ProcessTreeSampler:
    def __init__(self, root_pid, pgid=None, interval=0.02):
        self.root = root_pid
        self.pgid = pgid
        self.interval = interval
        self.peak_rss = 0
        self.peak_footprint = 0
        self.max_procs = 0
        self.samples = 0
        self.lifetime_max_footprint = {}  # pid -> bytes, per-process peak reported by the kernel
        self.names = {}
        self.pids = set()
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, daemon=True)

    def start(self):
        self._thread.start()
        return self

    def stop(self):
        self._stop.set()
        self._thread.join()

    def sample_once(self):
        pids = tree_pids(self.root, self.pgid)
        rss = fp = 0
        n = 0
        for p in pids:
            r = rusage(p)
            if r is None:
                continue
            n += 1
            rss += r[0]
            fp += r[1]
            if r[2] > self.lifetime_max_footprint.get(p, 0):
                self.lifetime_max_footprint[p] = r[2]
            if p not in self.names:
                self.names[p] = _proc_name(p)
        self.pids |= pids
        self.samples += 1
        self.peak_rss = max(self.peak_rss, rss)
        self.peak_footprint = max(self.peak_footprint, fp)
        self.max_procs = max(self.max_procs, n)

    def _run(self):
        while not self._stop.is_set():
            self.sample_once()
            self._stop.wait(self.interval)


_libproc.proc_name.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_uint32]


def _proc_name(pid):
    buf = ctypes.create_string_buffer(256)
    if _libproc.proc_name(pid, buf, 256) <= 0:
        return "?"
    return buf.value.decode(errors="replace")


if __name__ == "__main__":
    import subprocess
    import sys

    p = subprocess.Popen(sys.argv[1:], start_new_session=True)
    s = ProcessTreeSampler(p.pid, pgid=p.pid).start()
    t0 = time.perf_counter()
    p.wait()
    s.stop()
    print(f"wall {1000 * (time.perf_counter() - t0):.0f} ms, peak tree RSS {s.peak_rss / 2**20:.1f} MiB, "
          f"peak tree footprint {s.peak_footprint / 2**20:.1f} MiB, max procs {s.max_procs}, samples {s.samples}")
    print("processes:", sorted(set(s.names.values())))
