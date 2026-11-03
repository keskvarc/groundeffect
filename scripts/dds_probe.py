#!/usr/bin/env python3
"""Attach a bounded native DDS probe after the installed X-Plane 12 initializes."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import shutil
import signal
import subprocess
import time


def reference_paths(generation, latitude, longitude):
    scene = (generation / "scenery").resolve()
    manifest = json.loads((generation / "manifest.json").read_text())
    candidates = []
    for asset in manifest["assets"]:
        path = (scene / asset["path"]).resolve()
        if not path.is_relative_to(scene) or not path.is_file():
            raise ValueError(f"reference asset escapes or is missing: {asset['path']}")
        if path.stat().st_size != asset["bytes"]:
            raise ValueError(f"reference length changed: {asset['path']}")
        with path.open("rb") as file:
            if hashlib.file_digest(file, "sha256").hexdigest() != asset["sha256"]:
                raise ValueError(f"reference checksum changed: {asset['path']}")
        if path.suffix != ".ter":
            continue
        fields = [line.split() for line in path.read_text().splitlines() if line.strip()]
        center = next(f for f in fields if f[0] == "LOAD_CENTER")
        base = next(f[1] for f in fields if f[0] == "BASE_TEX_NOWRAP")
        lit = next(f[1] for f in fields if f[0] == "LIT_TEX_NOWRAP")
        lat, lon = map(float, center[1:3])
        pair = [(path.parent / name).resolve() for name in (base, lit)]
        if any(not p.is_relative_to(scene) or not p.is_file() for p in pair):
            raise ValueError(f"terrain binding escapes or is missing: {asset['path']}")
        distance = ((lat - latitude) * 111) ** 2 + ((lon - longitude) * 90) ** 2
        candidates.append({"lat": lat, "lon": lon, "distance": distance, "paths": pair})
    if not candidates:
        raise ValueError("reference has no qualified terrain mappings")
    chosen = [min(candidates, key=lambda a: a["distance"]),
              max(candidates, key=lambda a: a["distance"])]
    for axis in ("lat", "lon"):
        chosen.extend([min(candidates, key=lambda a: a[axis]),
                       max(candidates, key=lambda a: a[axis])])
    return sorted({path for item in chosen for path in item["paths"]})


def owned_simulator(parent, executable):
    pending, seen = [parent], set()
    while pending:
        pid = pending.pop()
        if pid in seen:
            continue
        seen.add(pid)
        if len(seen) > 512:
            raise ValueError("probe process tree exceeds its bound")
        try:
            if Path(os.readlink(f"/proc/{pid}/exe")) == executable:
                return pid
            children = Path(f"/proc/{pid}/task/{pid}/children").read_text()
        except FileNotFoundError:
            continue
        pending.extend(map(int, children.split()))
    raise ValueError("owned probe has no live X-Plane process")


def watch_capture(simulator, seconds):
    controller = os.pidfd_open(os.getpid())
    target = os.pidfd_open(simulator)
    child = os.fork()
    if child == 0:
        poller = select.poll()
        poller.register(controller, select.POLLIN)
        if not poller.poll(max(0, int(seconds * 1000))):
            try:
                signal.pidfd_send_signal(controller, signal.SIGINT)
            except ProcessLookupError:
                print("Capture controller already exited during timeout cleanup", flush=True)
        try:
            signal.pidfd_send_signal(target, signal.SIGTERM)
        except ProcessLookupError:
            os._exit(0)
        poller = select.poll()
        poller.register(target, select.POLLIN)
        if not poller.poll(10000):
            signal.pidfd_send_signal(target, signal.SIGKILL)
        os._exit(0)
    os.close(controller)
    os.close(target)


def stop_runtime(process):
    if process.poll() is not None:
        return
    os.killpg(process.pid, signal.SIGTERM)
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--generation", type=Path, required=True)
    parser.add_argument("--simulator", type=Path, required=True)
    parser.add_argument("--runtime", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--latitude", type=float, required=True)
    parser.add_argument("--longitude", type=float, required=True)
    args = parser.parse_args()
    if not (-90 <= args.latitude <= 90 and -180 <= args.longitude <= 180):
        parser.error("invalid probe position")
    strace = shutil.which("strace")
    if strace is None:
        parser.error("strace is required")
    output = args.output.resolve()
    attach = output.with_suffix(".attach")
    if attach.exists():
        parser.error("remove the previous probe's explicit attach marker first")
    paths = reference_paths(args.generation.resolve(), args.latitude, args.longitude)
    policy = {"texture_paths": list(map(str, paths)), "source": "fully verified native generation",
              "selection": "near/far and cardinal extremes from LOAD_CENTER metadata",
              "capture": "attach from the preserved ancestor PID after native initialization",
              "attach_marker": str(attach), "syscall_limit": 10000,
              "deadline_seconds": 300, "process_started_epoch": time.time(),
              "timing": "traced discovery only, not final latency qualification"}
    output.with_suffix(".policy.json").write_text(json.dumps(policy, indent=2))
    command = ["steam-run", str(args.runtime.resolve()), "--", "env",
               "SteamAppId=2014780", "SteamGameId=2014780", "./X-Plane-x86_64"]
    with output.with_suffix(".launch.log").open("w") as log:
        process = subprocess.Popen(command, cwd=args.simulator, stdout=log, stderr=log,
                                   start_new_session=True)
        print(f"Owned runtime group {process.pid}; waiting for menu/attach marker {attach}", flush=True)
        deadline = time.monotonic() + 300
        reason = None
        try:
            while process.poll() is None:
                if time.monotonic() >= deadline:
                    reason = "native DDS probe exceeded its 300-second deadline"
                    break
                if attach.exists():
                    pid = owned_simulator(os.getpid(), (args.simulator / "X-Plane-x86_64").resolve())
                    policy["simulator_pid"] = pid
                    policy["attached_epoch"] = time.time()
                    output.with_suffix(".policy.json").write_text(json.dumps(policy, indent=2))
                    trace = [strace, "-I", "1", "-f", "-qq", "-ttt", "-T", "-yy", "-s", "0",
                             "--syscall-limit=10000", "-e",
                             "trace=open,openat,openat2,close,read,pread64,readv,preadv,lseek,mmap,munmap",
                             "-o", str(output), "-p", str(pid)]
                    for path in paths:
                        trace.extend(["-P", str(path)])
                    print(f"Attaching to owned simulator PID {pid} from ancestor {os.getpid()}", flush=True)
                    watch_capture(pid, deadline - time.monotonic())
                    os.execv(strace, trace)
                try:
                    process.wait(timeout=0.5)
                except subprocess.TimeoutExpired:
                    pass
        finally:
            stop_runtime(process)
        if reason is not None:
            raise RuntimeError(reason)
        raise RuntimeError(f"X-Plane exited before attachment: {process.returncode}")


if __name__ == "__main__":
    main()
