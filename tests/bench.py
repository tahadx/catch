#!/usr/bin/env python3
"""Concurrency benchmark for the Catch backend.

Measures aggregate and per-file throughput for 1, 2, 3, 5 and 10 parallel
transfers against a controlled local HTTP server. The server throttles each
connection to a fixed rate, so aggregate numbers show how well Catch uses
several connections instead of measuring the loopback link or an ISP.

Everything runs in a throwaway HOME under /tmp/opencode; no state, download
file or process from the real user is touched. This is a harness for humans
and release checks, not part of the unit-test suite.

Usage:
    python3 tests/bench.py --binary target/release/catch
    python3 tests/bench.py --levels 1,3,10 --rate-mib 16 --json bench.json
"""

from __future__ import annotations

import argparse
import http.server
import json
import os
import shutil
import signal
import socketserver
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

CHUNK = 256 * 1024
SCRATCH = Path(tempfile.gettempdir()) / "opencode"


@dataclass
class ServerStats:
    requests: int = 0
    bytes_sent: int = 0
    lock: threading.Lock = field(default_factory=threading.Lock)

    def reset(self) -> None:
        with self.lock:
            self.requests = 0
            self.bytes_sent = 0


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "catch-bench"

    def log_message(self, *args: Any) -> None:  # noqa: A003 - stdlib signature
        pass

    def do_GET(self) -> None:  # noqa: N802 - stdlib signature
        bench: "BenchServer" = self.server.bench  # type: ignore[attr-defined]
        with bench.stats.lock:
            bench.stats.requests += 1
        payload = bench.payload
        self.send_response(200)
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Content-Type", "application/octet-stream")
        self.end_headers()
        rate = bench.rate_bytes_per_sec
        started = time.monotonic()
        sent = 0
        try:
            while sent < len(payload):
                end = min(sent + CHUNK, len(payload))
                self.wfile.write(payload[sent:end])
                self.wfile.flush()
                sent = end
                if rate > 0:
                    target = sent / rate
                    lag = target - (time.monotonic() - started)
                    if lag > 0:
                        time.sleep(lag)
        except (BrokenPipeError, ConnectionResetError):
            pass
        with bench.stats.lock:
            bench.stats.bytes_sent += sent


class BenchServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, payload: bytes, rate_bytes_per_sec: int):
        super().__init__(("127.0.0.1", 0), Handler)
        self.bench = self
        self.payload = payload
        self.rate_bytes_per_sec = rate_bytes_per_sec
        self.stats = ServerStats()


def parse_levels(raw: str) -> list[int]:
    levels = []
    for part in raw.split(","):
        part = part.strip()
        if part:
            levels.append(int(part))
    if not levels or any(n < 1 or n > 64 for n in levels):
        raise SystemExit("--levels must be a comma-separated list in 1..64")
    return levels


def read_processes(binary: str) -> list[dict[str, Any]]:
    """Catch processes from this benchmark, found by their environment tag."""
    marker = b"CATCH_BENCH=1"
    real = os.path.realpath(binary)
    found = []
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        pid = int(entry)
        try:
            with open(f"/proc/{pid}/environ", "rb") as f:
                if marker not in f.read():
                    continue
            exe = os.readlink(f"/proc/{pid}/exe")
            if os.path.realpath(exe) != real:
                continue
            with open(f"/proc/{pid}/stat", "rb") as f:
                stat = f.read().decode("utf-8", "replace")
            close = stat.rfind(")")
            fields = stat[close + 2 :].split()
            cpu_ticks = int(fields[11]) + int(fields[12])  # utime + stime
            with open(f"/proc/{pid}/statm") as f:
                rss_pages = int(f.read().split()[1])
            found.append({"pid": pid, "cpu_ticks": cpu_ticks, "rss_pages": rss_pages})
        except (FileNotFoundError, ProcessLookupError, PermissionError, IndexError, ValueError):
            continue
    return found


def kill_bench_processes(binary: str) -> None:
    for proc in read_processes(binary):
        try:
            os.kill(proc["pid"], signal.SIGKILL)
        except ProcessLookupError:
            pass


def seed_state(state_path: Path, urls: list[str]) -> None:
    state_path.parent.mkdir(parents=True, exist_ok=True)
    downloads = []
    for index, url in enumerate(urls):
        downloads.append(
            {
                "id": f"bench-{index:04}",
                "url": url,
                "filename": f"file-{index:04}.bin",
                "title": "",
                "kind": "file",
                "status": "queued",
                "bytes": 0,
                "total": 0,
                "error": "",
                "pid": 0,
                "claimed_at": 0,
                "sched_hold": False,
                "audio_only": False,
                "etag": "",
                "last_modified": "",
            }
        )
    state_path.write_text(
        json.dumps({"version": 1, "downloads": downloads, "saved": {}, "asks": []}),
        encoding="utf-8",
    )


def run_level(
    args: argparse.Namespace,
    binary: str,
    env: dict[str, str],
    server: BenchServer,
    level: int,
    downloads_dir: Path,
    state_path: Path,
) -> dict[str, Any]:
    urls = [f"http://127.0.0.1:{server.server_address[1]}/file-{i:04}.bin" for i in range(level)]
    shutil.rmtree(downloads_dir, ignore_errors=True)
    downloads_dir.mkdir(parents=True, exist_ok=True)
    state_path.unlink(missing_ok=True)
    seed_state(state_path, urls)
    server.stats.reset()
    kill_bench_processes(binary)

    cmd = [binary, "drain", "--max", str(level)]
    started = time.monotonic()
    server_cpu_start = time.process_time()
    proc = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=60)
    if proc.returncode != 0:
        raise RuntimeError(f"drain failed ({proc.returncode}): {proc.stderr.strip()}")

    cpu_ticks = 0
    cpu_seen: dict[int, int] = {}
    peak_rss_pages = 0
    done_at: dict[str, float] = {}
    status_latencies: list[float] = []
    statused_at = 0.0
    deadline = started + args.timeout
    last_poll = 0.0
    while True:
        now = time.monotonic()
        for sample in read_processes(binary):
            previous = cpu_seen.get(sample["pid"], 0)
            cpu_ticks += max(0, sample["cpu_ticks"] - previous)
            cpu_seen[sample["pid"]] = sample["cpu_ticks"]
            peak_rss_pages = max(peak_rss_pages, sample["rss_pages"])
        if now - last_poll >= 0.02:
            last_poll = now
            state = json.loads(state_path.read_text(encoding="utf-8"))
            rows = state.get("downloads", [])
            for row in rows:
                if row.get("status") in ("done", "error", "cancelled"):
                    done_at.setdefault(row["id"], now - started)
            if all(row.get("status") in ("done", "error", "cancelled") for row in rows) and rows:
                break
        # One status read while the queue is busy: CLI responsiveness under
        # state-file contention.
        if now - statused_at >= 0.25 and now - started < args.timeout:
            statused_at = now
            t0 = time.monotonic()
            try:
                subprocess.run(
                    [binary, "status"],
                    env=env,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    timeout=10,
                    check=False,
                )
                status_latencies.append((time.monotonic() - t0) * 1000)
            except subprocess.TimeoutExpired:
                status_latencies.append(10000.0)
        if now > deadline:
            break
        time.sleep(0.01)

    server_cpu = time.process_time() - server_cpu_start
    elapsed = time.monotonic() - started
    state = json.loads(state_path.read_text(encoding="utf-8"))
    rows = state.get("downloads", [])
    done = [row for row in rows if row.get("status") == "done"]
    errors = [row for row in rows if row.get("status") == "error"]
    cancelled = [row for row in rows if row.get("status") == "cancelled"]
    total_bytes = sum(int(row.get("bytes") or 0) for row in done)
    latencies = sorted(done_at.values())

    def percentile(values: list[float], fraction: float) -> float:
        if not values:
            return 0.0
        index = min(len(values) - 1, int(len(values) * fraction))
        return values[index]

    with server.stats.lock:
        requests = server.stats.requests
        sent = server.stats.bytes_sent
    result = {
        "concurrency": level,
        "wall_seconds": round(elapsed, 3),
        "aggregate_mib_per_second": round(total_bytes / elapsed / (1024 * 1024), 2)
        if elapsed
        else 0.0,
        "per_file_mib_per_second": round(
            (total_bytes / max(1, len(done))) / elapsed / (1024 * 1024), 2
        )
        if elapsed
        else 0.0,
        "files_done": len(done),
        "files_error": len(errors),
        "files_cancelled": len(cancelled),
        "bytes_on_disk": total_bytes,
        "http_requests": requests,
        "retries": max(0, requests - level),
        "catch_cpu_seconds": round(cpu_ticks / os.sysconf("SC_CLK_TCK"), 3),
        "server_cpu_seconds": round(server_cpu, 3),
        "peak_catch_rss_mib": round(peak_rss_pages * os.sysconf("SC_PAGE_SIZE") / (1024 * 1024), 1),
        "file_latency_seconds": {
            "min": round(latencies[0], 3) if latencies else 0.0,
            "p50": round(percentile(latencies, 0.5), 3),
            "p95": round(percentile(latencies, 0.95), 3),
            "max": round(latencies[-1], 3) if latencies else 0.0,
        },
        "status_latency_millis": {
            "count": len(status_latencies),
            "p50": round(percentile(sorted(status_latencies), 0.5), 1),
            "max": round(max(status_latencies), 1) if status_latencies else 0.0,
        },
    }
    if errors:
        result["errors"] = [row.get("error", "") for row in errors[:3]]
    if cancelled:
        result["cancelled_ids"] = [row.get("id") for row in cancelled[:3]]
    kill_bench_processes(binary)
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/release/catch")
    parser.add_argument("--size-mib", type=int, default=32, help="payload size per file")
    parser.add_argument(
        "--rate-mib",
        type=int,
        default=12,
        help="per-connection throttle; 0 = unthrottled loopback",
    )
    parser.add_argument("--levels", default="1,2,3,5,10")
    parser.add_argument("--segments", default="4",
                        help="comma list of CATCH_SEGMENTS values to compare (1 disables)")
    parser.add_argument("--timeout", type=float, default=180.0, help="seconds per level")
    parser.add_argument("--json", default="", help="also write the raw results here")
    args = parser.parse_args()

    binary = os.path.realpath(args.binary)
    if not os.access(binary, os.X_OK):
        print(f"bench: {binary} is not executable (build it first)", file=sys.stderr)
        return 1
    levels = parse_levels(args.levels)
    size = max(1, args.size_mib) * 1024 * 1024
    rate = max(0, args.rate_mib) * 1024 * 1024

    SCRATCH.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="catch-bench.", dir=SCRATCH))
    home = root / "home"
    downloads = root / "downloads"
    state_home = root / "state"
    config_home = root / "config"
    for path in (home, downloads, state_home, config_home):
        path.mkdir(parents=True, exist_ok=True)
    state_path = state_home / "omarchy" / "catch" / "state.json"

    env = dict(os.environ)
    env.update(
        {
            "HOME": str(home),
            "XDG_STATE_HOME": str(state_home),
            "XDG_CONFIG_HOME": str(config_home),
            "XDG_DOWNLOAD_DIR": str(downloads),
            "CATCH_BENCH": "1",
            "CATCH_MAX_ATTEMPTS": "3",
            "CATCH_STALL_SECS": "60",
            "CATCH_RESPONSE_SECS": "30",
        }
    )
    env.pop("CATCH_CATEGORIES", None)

    payload = os.urandom(1024) * (size // 1024) + os.urandom(size % 1024)
    server = BenchServer(payload, rate)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()

    # A shipped binary run from bin/dist self-configures links and browser
    # hosts on its first invocation. That is one-time install work, not
    # transfer throughput, so let it happen before the first timed level.
    subprocess.run(
        [binary, "status"],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=30,
        check=False,
    )

    print(
        f"bench: {binary} | {args.size_mib} MiB/file | "
        f"{args.rate_mib} MiB/s per connection | levels {levels}"
    )
    seg_values = [int(x.strip()) for x in args.segments.split(",") if x.strip()]
    results = []
    try:
        for seg in seg_values:
            run_env = dict(env)
            run_env["CATCH_SEGMENTS"] = str(seg)
            print(f"bench: segments={seg}")
            for level in levels:
                result = run_level(args, binary, run_env, server, level, downloads, state_path)
                result["segments"] = seg
                results.append(result)
                print(
                    "  seg={segments} c={concurrency:>2} wall={wall_seconds:>6.2f}s "
                    "agg={aggregate_mib_per_second:>7.2f} MiB/s "
                    "per-file={per_file_mib_per_second:>6.2f} MiB/s "
                    "req={http_requests} retries={retries} "
                    "catch-cpu={catch_cpu_seconds:>6.2f}s "
                    "rss={peak_catch_rss_mib:>5.1f} MiB "
                    "p50={p50:.2f}s p95={p95:.2f}s status-max={status_max:.1f}ms".format(
                        **result,
                        p50=result["file_latency_seconds"]["p50"],
                        p95=result["file_latency_seconds"]["p95"],
                        status_max=result["status_latency_millis"]["max"],
                    )
                )
    finally:
        server.shutdown()
        server.server_close()
        kill_bench_processes(binary)
        shutil.rmtree(root, ignore_errors=True)

    if len(results) > 1:
        baseline = results[0]
        print("bench: scaling vs c=1")
        for result in results[1:]:
            speedup = (
                result["aggregate_mib_per_second"] / baseline["aggregate_mib_per_second"]
                if baseline["aggregate_mib_per_second"]
                else 0.0
            )
            print(
                f"  c={result['concurrency']:>2}: "
                f"aggregate x{speedup:.2f}, "
                f"{result['aggregate_mib_per_second']:.1f} MiB/s"
            )

    if args.json:
        Path(args.json).write_text(
            json.dumps({"config": vars(args), "results": results}, indent=2),
            encoding="utf-8",
        )
        print(f"bench: wrote {args.json}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
