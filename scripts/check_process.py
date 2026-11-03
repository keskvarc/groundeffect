#!/usr/bin/env python3
"""Exercise the actual Rust process and Go CLI; no source datasets required."""

import json
from contextlib import closing
import errno
import hashlib
import fcntl
import os
from pathlib import Path
import socket
import pty
import select
import shutil
import struct
import subprocess
import tempfile
import termios
import time
import unittest


ROOT = Path(__file__).resolve().parents[1]
ENGINE = ROOT / "bin/groundeffect-engine"
CLIENT = ROOT / "bin/groundeffect"


class ProcessTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="ge-check-")
        self.data = Path(self.temporary.name) / "engine"
        self.data.mkdir(mode=0o700)
        self.log = (self.data / "log").open("w")
        self.process = None
        self.start()

    def tearDown(self):
        if self.process and self.process.poll() is None:
            if self.rpc("status")["result"].get("stream_config") is not None:
                self.assertIsNone(self.rpc("stream_stop")["error"])
            self.process.terminate()
            self.process.wait(timeout=10)
        if self.process is None:
            self.rpc("shutdown")
        self.log.close()
        mount = str(self.data / "reader")
        if any(line.split()[4] == mount for line in Path("/proc/self/mountinfo").read_text().splitlines()):
            fusermount = (
                shutil.which("fusermount3")
                or shutil.which("fusermount")
                or "/run/wrappers/bin/fusermount3"
            )
            subprocess.run([fusermount, "-u", mount], check=True, timeout=5)
        self.temporary.cleanup()

    def start(self):
        self.process = subprocess.Popen(
            [ENGINE, "serve", "--data-dir", self.data], stdout=self.log, stderr=self.log
        )
        for _ in range(100):
            try:
                self.rpc("status")
                return
            except (OSError, ValueError):
                if self.process.poll() is not None:
                    self.fail((self.data / "log").read_text())
                time.sleep(0.02)
        self.fail("engine readiness deadline exceeded")

    def rpc(self, method, params=None, version=1):
        with socket.socket(socket.AF_UNIX) as conn:
            conn.settimeout(4)
            conn.connect(str(self.data / "control.sock"))
            conn.sendall(json.dumps({"version": version, "id": "test", "method": method,
                                     "params": {} if params is None else params}).encode() + b"\n")
            return json.loads(conn.makefile("rb").readline(256 * 1024 + 1))

    def test_protocol_ownership_and_cli(self):
        self.assertIn("unsupported protocol", self.rpc("status", version=2)["error"])
        self.assertIn("unknown engine method", self.rpc("unknown")["error"])
        self.assertIn("configure", self.rpc("prepare")["error"])
        second = subprocess.run([ENGINE, "serve", "--data-dir", self.data],
                                capture_output=True, timeout=5)
        self.assertNotEqual(second.returncode, 0)
        self.assertIn(b"another engine", second.stderr)
        state = json.loads(subprocess.check_output(
            [CLIENT, "--data-dir", self.data, "--json", "status"], timeout=5
        ))
        self.assertEqual(state["version"], 1)
        self.assertEqual(state["generations"], [])
        self.assertEqual(os.stat(self.data / "control.sock").st_mode & 0o777, 0o600)
        nonterminal = subprocess.run([CLIENT, "--data-dir", self.data, "tui"],
                                    capture_output=True, timeout=5)
        self.assertNotEqual(nonterminal.returncode, 0)
        self.assertIn(b"requires a terminal", nonterminal.stderr)
        self.assertIsNone(self.rpc("shutdown")["error"])
        self.process.wait(timeout=5)
        self.assertFalse((self.data / "control.sock").exists())

    def test_request_limits_and_recovery(self):
        with socket.socket(socket.AF_UNIX) as conn:
            conn.settimeout(4)
            conn.connect(str(self.data / "control.sock"))
            conn.sendall(b"x" * (256 * 1024 + 1))
            self.assertIn("bounded JSON", json.loads(conn.makefile("rb").readline())["error"])
        state = self.rpc("status")["result"]
        self.process.terminate()
        self.process.wait(timeout=5)
        state["job"] = {
            "id": "interrupted", "kind": "prepare", "generation": "interrupted",
            "state": "running", "stage": "compiling", "done": 5, "total": 10,
            "error": None, "started": 1, "finished": None,
        }
        (self.data / "state.json").write_text(json.dumps(state))
        self.start()
        recovered = self.rpc("status")["result"]
        self.assertEqual(recovered["job"]["state"], "failed")
        self.assertIn("not ready", recovered["job"]["error"])
        self.assertEqual(recovered["generations"], [])

    def test_terminal_cleanup_keeps_engine_alive(self):
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        process = subprocess.Popen(
            [CLIENT, "--data-dir", self.data, "tui"], stdin=slave, stdout=slave,
            stderr=slave, env={**os.environ, "TERM": "xterm-256color"},
        )
        os.close(slave)
        output = bytearray()
        try:
            deadline = time.monotonic() + 10
            while b"GROUNDEFFECT" not in output and time.monotonic() < deadline:
                if select.select([master], [], [], 0.1)[0]:
                    output.extend(os.read(master, 65536))
            self.assertIn(b"GROUNDEFFECT", output)
            os.write(master, b"q")
            process.wait(timeout=10)
            while select.select([master], [], [], 0.1)[0]:
                try:
                    chunk = os.read(master, 65536)
                except OSError as error:
                    if error.errno != 5:
                        raise
                    break
                if not chunk:
                    break
                output.extend(chunk)
            self.assertEqual(process.returncode, 0)
            self.assertIn(b"\x1b[?1049h", output)
            self.assertIn(b"\x1b[?1049l", output)
            self.assertIsNone(self.process.poll())
            self.assertIsNone(self.rpc("status")["error"])
        finally:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=10)
            os.close(master)

    def test_sibling_engine_launcher(self):
        self.process.terminate()
        self.process.wait(timeout=5)
        self.process = None
        launched = subprocess.run(
            [CLIENT, "--data-dir", self.data, "--json", "engine", "start"],
            capture_output=True, timeout=10,
        )
        self.assertEqual(launched.returncode, 0, launched.stderr)
        self.assertEqual(json.loads(launched.stdout)["version"], 1)
        self.assertIsNone(self.rpc("status")["error"])

    def test_automatic_reader_filesystem_and_lifecycle(self):
        mesh = Path(self.temporary.name) / "mesh"
        (mesh / "terrain").mkdir(parents=True)
        (mesh / "textures").mkdir()
        folder = mesh / "Earth nav data/+30-080"
        folder.mkdir(parents=True)
        terrain = ("A\n800\nTERRAIN\nLOAD_CENTER 35.85344 -78.70605 7930 4096\n"
                   "BASE_TEX_NOWRAP ../textures/25760_18432_BI16.dds\n"
                   "LOAD_CENTER_BORDER 35.85344 -78.70605 7930 128\n"
                   "BORDER_TEX ../textures/mask.png\n")
        from PIL import Image
        Image.new("L", (128, 128), 127).save(mesh / "textures/mask.png")
        (mesh / "terrain/a.ter").write_text(terrain)
        (mesh / "terrain/wet.ter").write_text(terrain + "WET\n")
        (mesh / "terrain/invalid.ter").write_text(terrain + "UNQUALIFIED_DIRECTIVE\n")
        def atom(name, body):
            return name + struct.pack("<I", len(body) + 8) + body
        dsf = b"XPLNEDSF\x01\0\0\0" + atom(b"NFED", atom(b"TRET", b"terrain_Water\0terrain/a.ter\0terrain/wet.ter\0"))
        payload_size = 65 * 1024 * 1024
        dsf += b"MESH" + struct.pack("<I", payload_size + 8)
        checksum, mesh_hash = hashlib.md5(), hashlib.sha256()
        with (folder / "+35-079.dsf").open("wb") as file:
            file.write(dsf)
            checksum.update(dsf)
            mesh_hash.update(dsf)
            block = b"\0" * 65536
            for _ in range(payload_size // len(block)):
                file.write(block)
                checksum.update(block)
                mesh_hash.update(block)
            trailer = checksum.digest()
            file.write(trailer)
            mesh_hash.update(trailer)
        expected_mesh_hash = mesh_hash.hexdigest()
        osm = Path(self.temporary.name) / "roads.rds1"
        recs = b"".join(
            struct.pack(
                "<QfffffIffff",
                25 + index,
                1.0,
                35.85344 + index * 0.00002,
                35.85344 + index * 0.00002,
                -78.73,
                -78.68,
                2,
                35.85344 + index * 0.00002,
                -78.73,
                35.85344 + index * 0.00002,
                -78.68,
            )
            for index in range(3)
        )
        subcells = [recs if s in (13 * 16 + 4, 13 * 16 + 5) else b"" for s in range(256)]
        rds1 = bytearray(b"RDS1" + struct.pack("<IIIhhIQ", 16, 1, 0, 35, -79, 0, 32) + b"\x00" * (256 * 12))
        for s, payload in enumerate(subcells):
            struct.pack_into("<QI", rds1, 32 + s * 12, len(rds1), len(payload))
            rds1.extend(payload)
        osm.write_bytes(rds1)
        config = {"source_package": str(mesh), "mount_point": str(self.data / "reader"),
                  "osm_db": str(osm), "viirs": None, "source_revision": "process-test",
                  "size": 1024, "exposure": 0.35, "budget_mib": 512}
        relative = dict(config, osm_db=os.path.relpath(osm))
        self.assertIn("absolute", self.rpc("stream_configure", relative)["error"])
        owned = self.data / "owned-roads.rds1"
        owned.write_bytes(osm.read_bytes())
        inside = dict(config, osm_db=str(owned))
        self.assertIn("outside", self.rpc("stream_configure", inside)["error"])
        self.assertIsNone(self.rpc("stream_configure", config)["error"])
        started = self.rpc("stream_start")
        self.assertIsNone(started["error"], started["error"])
        self.assertTrue(started["result"]["stream"]["mounted"])
        self.assertEqual(started["result"]["stream"]["cold_builds"], 0)
        self.assertIn("reader", self.rpc("shutdown")["error"])
        probe = r'''
import hashlib, io, json, mmap, os, pathlib, struct, sys
from PIL import Image
root = pathlib.Path(sys.argv[1])
dsf = (root / "Earth nav data/+30-080/+35-079.dsf").read_bytes()
texts = [(root / "terrain" / name).read_text() for name in ["a.ter","wet.ter"]]
assert all("LOAD_CENTER_BORDER 35.85344 -78.70605 7930 128" in text for text in texts)
assert (root / "textures/mask.png").read_bytes() == (pathlib.Path(sys.argv[2]) / "textures/mask.png").read_bytes()
bindings = [[line.split()[1] for line in text.splitlines() if line.startswith(("BASE_TEX","LIT_TEX"))] for text in texts]
assert bindings[0] == bindings[1]
day, night = [root / "terrain" / path for path in bindings[0]]
assert day.stat().st_size == 699192
fd = os.open(night,os.O_RDONLY)
with mmap.mmap(fd, 0, flags=mmap.MAP_PRIVATE, prot=mmap.PROT_READ) as mapped:
    header = mapped[:128]
assert len(header) == 128 and header[:4] == b"DDS "
assert struct.unpack_from("<I",header,12)[0] == 1024
assert os.pread(fd,4,8) == header[8:12]
print(json.dumps({"phase":"initial-read"}),flush=True)
sys.stdin.readline()
data = os.pread(fd,699192,0)
assert len(data) == 699192 and os.pread(fd,10,699192) == b""
assert data[:128] == header
assert os.pread(fd,57,127) == data[127:184]
with mmap.mmap(fd, 0, access=mmap.ACCESS_READ) as mapped:
    assert mapped[:] == data
with Image.open(io.BytesIO(data)) as image:
    assert image.size == (1024,1024)
    assert sum(image.convert("RGB").getextrema()[0]) > 0
try:
    os.open(night,os.O_WRONLY)
except OSError as e:
    assert e.errno in (13,30)
else:
    raise AssertionError("readonly mount accepted write")
for _ in range(65):
    try:
        (root / "terrain/invalid.ter").read_text()
    except OSError as error:
        assert error.errno == 5
    else:
        raise AssertionError("unqualified terrain was accepted")
if len(sys.argv) > 3:
    source = pathlib.Path(sys.argv[2]) / "Earth nav data/+30-080/+35-079.dsf"
    mesh_fd = os.open(root / "Earth nav data/+30-080/+35-079.dsf", os.O_RDONLY)
    assert os.pread(mesh_fd,12,0) == b"XPLNEDSF\x01\0\0\0"
    source.rename(source.with_suffix(".original"))
    source.write_bytes(b"replaced borrowed mesh")
    os.posix_fadvise(mesh_fd, 0, 0, os.POSIX_FADV_DONTNEED)
    try:
        os.pread(mesh_fd,12,0)
    except OSError as error:
        assert error.errno == 5
    else:
        raise AssertionError("replaced borrowed mesh was served through its old handle")
    os.close(mesh_fd)
print(json.dumps({"dsf":hashlib.sha256(dsf).hexdigest(),"dds":hashlib.sha256(data).hexdigest()}),flush=True)
sys.stdin.readline()
os.close(fd)
'''
        child = subprocess.Popen(["python", "-c", probe, str(self.data / "reader"), str(mesh)],
                                 stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            self.assertTrue(select.select([child.stdout], [], [], 15)[0], "reader probe timed out")
            line = child.stdout.readline()
            if not line:
                _, errors = child.communicate(timeout=5)
                self.fail(errors + "\n" + (self.data / "log").read_text())
            self.assertEqual(json.loads(line), {"phase":"initial-read"})
            headers = self.rpc("status")["result"]["stream"]
            self.assertEqual(headers["cold_builds"], 2)
            self.assertEqual(headers["network_requests"], 0)
            self.assertGreater(headers["dds_payload_reads"], 0)
            self.assertTrue(any(offset == 0 and 128 < length <= 699192
                                for offset, length in headers["dds_read_samples"]))
            self.assertGreater(headers["open_handles"], 0)
            self.assertIn("in use", self.rpc("stream_stop")["error"])
            child.stdin.write("\n")
            child.stdin.flush()
            self.assertTrue(select.select([child.stdout], [], [], 15)[0], "payload probe timed out")
            line = child.stdout.readline()
            if not line:
                _, errors = child.communicate(timeout=5)
                self.fail(errors + "\n" + (self.data / "log").read_text())
            observed = json.loads(line)
            self.assertEqual(observed["dsf"], expected_mesh_hash)
            active = self.rpc("status")["result"]["stream"]
            self.assertGreater(active["reads"], 0)
            self.assertGreater(active["active_consumers"], 0)
            self.assertGreater(active["open_handles"], 0)
            self.assertGreater(sum(active["mip_reads"]), 0)
            self.assertGreaterEqual(active["failures"], 65)
            self.assertIn("invalid.ter", active["last_error"])
            self.assertLessEqual((self.data / "log").read_text().count("reader filesystem:"), 64)
            self.assertIn("in use", self.rpc("stream_stop")["error"])
            self.assertIn("changing", self.rpc("stream_configure", config)["error"])
            child.stdin.write("\n")
            child.stdin.flush()
            _, errors = child.communicate(timeout=10)
            self.assertEqual(child.returncode, 0, errors)
        finally:
            if child.poll() is None:
                child.terminate()
                child.wait(timeout=10)
            for stream in [child.stdin, child.stdout, child.stderr]:
                stream.close()
        self.assertIsNone(self.rpc("stream_stop")["error"])
        self.assertFalse(self.rpc("status")["result"]["stream_enabled"])
        self.assertIsNone(self.rpc("stream_start")["error"])
        warm = subprocess.run(["python", "-c", probe, str(self.data / "reader"), str(mesh)],
                              input="\n", capture_output=True, text=True, timeout=10)
        self.assertEqual(warm.returncode, 0, warm.stderr)
        self.assertEqual(json.loads(warm.stdout.splitlines()[-1]), observed)
        stats = self.rpc("status")["result"]["stream"]
        self.assertEqual(stats["cold_builds"], 0)
        self.assertGreater(stats["cache_hits"], 0)
        retained_directory = os.open(self.data / "reader", os.O_RDONLY | os.O_DIRECTORY)
        try:
            stopped = self.rpc("stream_stop")
            self.assertIsNone(stopped["error"], stopped["error"])
            self.assertFalse(stopped["result"]["stream"]["mounted"])
            self.assertFalse(stopped["result"]["stream_enabled"])
            with self.assertRaises(OSError) as disconnected:
                os.stat("unseen-after-stop", dir_fd=retained_directory)
            self.assertIn(disconnected.exception.errno, (errno.ENOTCONN, errno.EIO))
            self.assertIsNone(self.rpc("stream_start")["error"])
            restarted = subprocess.run(
                ["python", "-c", probe, str(self.data / "reader"), str(mesh), "replace-mesh"],
                input="\n", capture_output=True, text=True, timeout=10,
            )
            self.assertEqual(restarted.returncode, 0, restarted.stderr)
            self.assertEqual(json.loads(restarted.stdout.splitlines()[-1]), observed)
            self.assertEqual(self.rpc("status")["result"]["stream"]["cold_builds"], 0)
            self.assertIsNone(self.rpc("stream_stop")["error"])
        finally:
            os.close(retained_directory)


class SourcePreparationTests(unittest.TestCase):
    def test_dds_probe_deadline_stops_only_the_owned_capture_and_target(self):
        unrelated = subprocess.Popen(["sleep", "30"])
        try:
            result = subprocess.run(
                ["python", "-c",
                 "import os,subprocess; from dds_probe import watch_capture; "
                 "target=subprocess.Popen(['sleep','30']); print(target.pid,flush=True); "
                 "watch_capture(target.pid,0.2); os.execvp('sleep',['sleep','30'])"],
                cwd=ROOT / "scripts", capture_output=True, text=True, timeout=5,
            )
            self.assertEqual(result.returncode, -2, result.stderr)
            self.assertIsNone(unrelated.poll())
            status = Path(f"/proc/{int(result.stdout.strip())}/status")
            if status.exists():
                self.assertIn("State:\tZ", status.read_text())
        finally:
            unrelated.terminate()
            unrelated.wait(timeout=5)

    def test_dds_probe_discovers_only_its_live_descendant(self):
        from dds_probe import owned_simulator

        child = subprocess.Popen(["sleep", "30"])
        try:
            executable = Path(os.readlink(f"/proc/{child.pid}/exe"))
            self.assertEqual(owned_simulator(os.getpid(), executable), child.pid)
        finally:
            child.terminate()
            child.wait(timeout=5)
        with self.assertRaisesRegex(ValueError, "no live X-Plane"):
            owned_simulator(os.getpid(), executable)

    def test_dds_probe_uses_verified_bindings_with_overlay_names(self):
        from dds_probe import reference_paths

        with tempfile.TemporaryDirectory(prefix="ge-probe-") as name:
            root = Path(name)
            (root / "scenery/terrain").mkdir(parents=True)
            (root / "scenery/textures").mkdir()
            assets = []
            for index, suffix in enumerate(["BI18_water_overlay", "BI16"]):
                prefix = f"textures/25760_18432_{suffix}"
                for ending in ["_GE.dds", "_GE_LIT.dds"]:
                    path = root / "scenery" / (prefix + ending)
                    path.write_bytes(b"verified reference")
                    assets.append({"path": str(path.relative_to(root / "scenery")),
                                   "bytes": path.stat().st_size,
                                   "sha256": hashlib.sha256(path.read_bytes()).hexdigest()})
                path = root / "scenery/terrain" / f"{index}.ter"
                path.write_text(
                    f"A\n800\nTERRAIN\n\nLOAD_CENTER {35 + index} -78 7930 1024\n"
                    f"BASE_TEX_NOWRAP ../{prefix}_GE.dds\n"
                    f"LIT_TEX_NOWRAP ../{prefix}_GE_LIT.dds\n"
                )
                assets.append({"path": str(path.relative_to(root / "scenery")),
                               "bytes": path.stat().st_size,
                               "sha256": hashlib.sha256(path.read_bytes()).hexdigest()})
            (root / "manifest.json").write_text(json.dumps({"assets": assets}))
            selected = reference_paths(root, 35, -78)
            self.assertEqual(len(selected), 4)
            self.assertTrue(any("water_overlay" in path.name for path in selected))
            selected[0].write_bytes(b"corrupt reference")
            with self.assertRaisesRegex(ValueError, "reference length changed"):
                reference_paths(root, 35, -78)

    def test_dds_trace_distinguishes_full_reads_from_mappings_and_partial_reads(self):
        from analyze_dds_trace import analyze

        trace = [
            '10 100.0 read(3</scenery/day.dds>, ""..., 128) = 128 <0.1>\n',
            '10 100.1 pread64(3</scenery/day.dds>,  <unfinished ...>\n',
            '11 100.2 mmap(NULL, 699192, PROT_READ, MAP_PRIVATE, '
            '4</scenery/night.dds>, 0) = 0x12345000 <0.1>\n',
            '10 100.3 <... pread64 resumed>""..., 8, 699184) = 8 <0.1>\n',
            '12 100.4 read(5</scenery/day.dds>, ""..., 699192) = 699192 <0.1>\n',
        ]
        result = analyze(trace, {"day.dds": 699192, "night.dds": 699192})
        self.assertEqual(result["files"]["day.dds"]["full_file_reads"], 1)
        self.assertEqual(result["files"]["day.dds"]["unknown_offset_reads"], 1)
        self.assertEqual(result["files"]["night.dds"]["read_bytes"], 0)
        self.assertEqual(result["files"]["night.dds"]["mappings"], 1)
        tail = next(op for op in result["operations"] if op["operation"] == "pread64")
        self.assertEqual(tail["offset"], 699184)
        with self.assertRaisesRegex(ValueError, "inside a syscall"):
            analyze(trace[:2], {"day.dds": 699192})

    def test_mosaic_preserves_valid_overlaps_and_explicit_alpha(self):
        from PIL import Image
        from prepare_naip import warp_local_mosaic

        with tempfile.TemporaryDirectory(prefix="ge-mosaic-") as name:
            path = Path(name)
            sources = []
            for index, color in enumerate([(200, 20, 20), (20, 200, 20)]):
                image = Image.new("RGB", (256, 256), color)
                if index:
                    image.paste((0, 0, 0), (0, 0, 32, 256))
                png = path / f"{index}.png"
                raster = path / f"{index}.tif"
                image.save(png)
                subprocess.run([
                    "gdal_translate", "-q", "-a_srs", "EPSG:3857",
                    "-a_ullr", str(index * 128), "256", str(index * 128 + 256), "0",
                    "-a_nodata", "0", str(png), str(raster),
                ], check=True, capture_output=True, timeout=10)
                sources.append(str(raster))
            target = path / "mosaic.tif"
            warp_local_mosaic(sources, target, [0, 0, 384, 256], 1, os.environ)
            png = path / "mosaic.png"
            subprocess.run(["gdal_translate", "-q", "-of", "PNG", str(target), str(png)],
                           check=True, capture_output=True, timeout=10)
            with Image.open(png) as result:
                self.assertEqual(result.getpixel((64, 64)), (200, 20, 20, 255))
                self.assertEqual(result.getpixel((140, 64)), (200, 20, 20, 255))
                self.assertEqual(result.getpixel((300, 64)), (20, 200, 20, 255))


if __name__ == "__main__":
    unittest.main()
