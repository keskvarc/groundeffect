#!/usr/bin/env python3
"""Prepare an explicit NAIP preview input; never runs in the simulator path."""

import argparse
import collections
import concurrent.futures
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import ssl
import shutil
import subprocess
import sys
import tempfile
import threading
import urllib.parse
import urllib.request


def fetch(url):
    with urllib.request.urlopen(url, timeout=60) as response:
        raw = response.read(16 * 1024 * 1024 + 1)
    if len(raw) > 16 * 1024 * 1024:
        raise ValueError("STAC response exceeds 16 MiB")
    return json.loads(raw)


def run(command, env, timeout=3600):
    subprocess.run(command, env=env, check=True, timeout=timeout)


def file_hash(path):
    digest = hashlib.sha256()
    with path.open("rb") as file:
        for block in iter(lambda: file.read(65536), b""):
            digest.update(block)
    return digest.hexdigest()


def sync_directory(path):
    directory = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


def warp_local_mosaic(mosaics, prepared, bounds, resolution, env):
    run([
        "gdalwarp", "-t_srs", "EPSG:3857", "-tr", str(resolution), str(resolution),
        "-tap", "-te", *map(str, bounds), "-r", "average", "-wm", "64",
        "-wo", "INIT_DEST=0", "-wo", "UNIFIED_SRC_NODATA=YES", "-dstnodata", "None",
        "-dstalpha", "-of", "GTiff", "-co", "COMPRESS=ZSTD",
        "-co", "TILED=YES", "-co", "BIGTIFF=IF_SAFER",
        *mosaics, str(prepared),
    ], env)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--engine", type=Path, default=Path("bin/groundeffect-engine"))
    parser.add_argument("--year", type=int, required=True)
    parser.add_argument("--sampling-zoom", type=int, choices=range(12, 17), default=14)
    args = parser.parse_args()
    if os.geteuid() == 0:
        raise ValueError("do not run imagery preparation as root")
    config = json.loads(args.config.read_text())
    output = Path(config["imagery_raster"])
    if not output.is_absolute():
        raise ValueError("imagery_raster must be absolute")
    sidecar = output.with_suffix(output.suffix + ".sources.json")
    if os.path.lexists(output) or os.path.lexists(sidecar):
        raise FileExistsError(f"will not replace existing imagery: {output}")
    report = json.loads(subprocess.check_output(
        [str(args.engine.resolve()), "plan", "--config", str(args.config)], timeout=30
    ))
    west, south, east, north = report["geographic"]
    if east - west > 3 or north - south > 3:
        raise ValueError("alpha NAIP helper is limited to 3 by 3 degrees")
    query = urllib.parse.urlencode({
        "collections": "naip", "bbox": f"{west},{south},{east},{north}",
        "datetime": f"{args.year}-01-01T00:00:00Z/{args.year}-12-31T23:59:59Z",
        "limit": 500,
    })
    url = f"https://planetarycomputer.microsoft.com/api/stac/v1/search?{query}"
    items = {}
    while url:
        page = fetch(url)
        for item in page["features"]:
            items[item["id"]] = item
        if len(items) > 2000:
            raise ValueError("NAIP item limit exceeded")
        next_links = [link for link in page.get("links", []) if link["rel"] == "next"]
        url = next_links[0]["href"] if next_links else None
        if url and not url.startswith("https://planetarycomputer.microsoft.com/"):
            raise ValueError("unexpected STAC pagination host")
    if not items:
        raise ValueError("no NAIP imagery found for this footprint/year")
    sources = []
    resolution = 2 * math.pi * 6378137 / (256 * 2 ** args.sampling_zoom)
    ground_resolution = resolution * math.cos(math.radians((north + south) / 2))
    intermediate_bytes = 0
    for item in sorted(items.values(), key=lambda item: (item["properties"]["datetime"], item["id"])):
        href = item["assets"]["image"]["href"]
        if not href.startswith("https://naipeuwest.blob.core.windows.net/naip/") or "?" in href:
            raise ValueError(f"unexpected NAIP asset URL: {href}")
        epsg = item["properties"].get("proj:epsg")
        if epsg is None:
            epsg = item["assets"]["image"].get("proj:epsg")
        if not isinstance(epsg, int) or not (26901 <= epsg <= 26923 or 32601 <= epsg <= 32623):
            raise ValueError("alpha NAIP helper requires a metre-based US UTM source CRS")
        shape = item["properties"].get("proj:shape", item["assets"]["image"].get("proj:shape"))
        gsd = item["properties"]["gsd"]
        if (not isinstance(shape, list) or len(shape) != 2
                or not all(isinstance(n, int) and 0 < n <= 65536 for n in shape)
                or not isinstance(gsd, (int, float)) or not (0.1 <= gsd <= 5)):
            raise ValueError("NAIP item lacks bounded shape/GSD metadata")
        intermediate_bytes += math.ceil(shape[0] * gsd / ground_resolution) * math.ceil(shape[1] * gsd / ground_resolution) * 3
        sources.append((epsg, href, item["id"]))
    if intermediate_bytes > 3 * 1024**3:
        raise ValueError("source preview intermediates exceed 3 GiB estimate")
    env = os.environ.copy()
    ca = ssl.get_default_verify_paths().cafile
    if ca:
        env["CURL_CA_BUNDLE"] = ca
        env["GDAL_HTTP_CA_BUNDLE"] = ca
    env.update({
        "GDAL_DISABLE_READDIR_ON_OPEN": "EMPTY_DIR",
        "CPL_VSIL_CURL_ALLOWED_EXTENSIONS": ".tif",
        "GDAL_HTTP_MAX_RETRY": "3", "GDAL_HTTP_RETRY_DELAY": "1",
        "GDAL_HTTP_CONNECTTIMEOUT": "20", "GDAL_HTTP_TIMEOUT": "60",
        "GDAL_CACHEMAX": "64", "CPL_VSIL_CURL_CACHE_SIZE": str(32 * 1024 * 1024),
        "GDAL_MAX_DATASET_POOL_SIZE": "32",
        "GDAL_MAX_DATASET_POOL_RAM_USAGE": str(64 * 1024 * 1024),
    })
    output.parent.mkdir(parents=True, exist_ok=True)
    cache = output.with_suffix(output.suffix + ".sources")
    cache.mkdir(mode=0o700, exist_ok=True)
    sync_directory(output.parent)
    cache_lock = (cache / "lock").open("a")
    try:
        fcntl.flock(cache_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError as error:
        raise ValueError(f"another imagery preparation owns {cache}") from error
    cached_bytes = sum(path.stat().st_size for path in cache.glob("*/image.tif"))
    if cached_bytes > 3 * 1024**3:
        raise ValueError(f"source cache budget exceeded; inspect {cache} before removing unused inputs")
    cache_size_lock = threading.Lock()
    bounds = report["mercator"]
    pixels = math.ceil((bounds[2] - bounds[0]) / resolution) * math.ceil((bounds[3] - bounds[1]) / resolution)
    if pixels * 4 > 2 * 1024**3:
        raise ValueError("prepared raster exceeds 2 GiB uncompressed alpha budget")
    if shutil.disk_usage(output.parent).free < intermediate_bytes + pixels * 4 + 128 * 1024**2:
        raise ValueError("insufficient free space for source previews and final raster")
    print(f"Preparing {len(items)} NAIP COG previews; "
          f"{pixels:,} output pixels at z{args.sampling_zoom}", flush=True)
    with tempfile.TemporaryDirectory(prefix="naip-", dir=output.parent) as temp:
        work = Path(temp)
        groups = collections.defaultdict(list)

        def sample_source(entry):
            nonlocal cached_bytes
            index, (epsg, href, item_id) = entry
            identity = hashlib.sha256(json.dumps({
                "href": href, "item": item_id, "resolution": ground_resolution,
                "adapter": 1, "bands": [1, 2, 3], "resampling": "average",
            }, sort_keys=True).encode()).hexdigest()
            cached = cache / identity
            if cached.exists():
                metadata = cached / "metadata.json"
                if metadata.stat().st_size > 4096:
                    raise ValueError(f"invalid source cache metadata: {metadata}")
                record = json.loads(metadata.read_text())
                preview = cached / "image.tif"
                if preview.stat().st_size > 32 * 1024**2 or file_hash(preview) != record["sha256"]:
                    raise ValueError(f"cached preview is corrupt: {preview}")
                return index, epsg, str(preview)
            staging = work / f"source-{index:04d}"
            staging.mkdir()
            preview = staging / "image.tif"
            run([
                "gdal_translate", "-q", "-r", "average", "-tr",
                str(ground_resolution), str(ground_resolution),
                "-b", "1", "-b", "2", "-b", "3", "-co", "COMPRESS=ZSTD",
                "-co", "TILED=YES", "/vsicurl/" + href, str(preview),
            ], env, timeout=180)
            if preview.stat().st_size > 32 * 1024**2:
                raise ValueError("source preview exceeds 32 MiB file limit")
            metadata = staging / "metadata.json"
            metadata.write_text(json.dumps({"sha256": file_hash(preview), "source": href}) + "\n")
            for path in [preview, metadata]:
                path.chmod(0o444)
                with path.open("rb") as file:
                    os.fsync(file.fileno())
            sync_directory(staging)
            with cache_size_lock:
                if cached_bytes + preview.stat().st_size > 3 * 1024**3:
                    raise ValueError(f"source cache budget exceeded; inspect {cache}")
                size = preview.stat().st_size
                os.rename(staging, cached)
                cached_bytes += size
                sync_directory(cache)
            return index, epsg, str(cached / "image.tif")

        pool = concurrent.futures.ThreadPoolExecutor(max_workers=4)
        intermediate_actual = 0
        try:
            futures = [pool.submit(sample_source, entry) for entry in enumerate(sources)]
            for index, future in enumerate(concurrent.futures.as_completed(futures), 1):
                source_index, epsg, preview = future.result()
                intermediate_actual += Path(preview).stat().st_size
                if intermediate_actual > 3 * 1024**3:
                    raise ValueError("actual source previews exceed 3 GiB budget")
                if cached_bytes > 3 * 1024**3:
                    raise ValueError("actual persistent source cache exceeds 3 GiB budget")
                groups[epsg].append((source_index, preview))
                if index % 16 == 0 or index == len(sources):
                    print(f"Source previews: {index}/{len(sources)}", flush=True)
        finally:
            pool.shutdown(wait=True, cancel_futures=True)
        mosaics = []
        for epsg, previews in sorted(groups.items()):
            paths = work / f"{epsg}.txt"
            paths.write_text("\n".join(path for _, path in sorted(previews)) + "\n")
            rgb = work / f"{epsg}-rgb.vrt"
            run(["gdalbuildvrt", "-strict", "-input_file_list", str(paths), str(rgb)], env)
            mosaics.append(str(rgb))
        prepared = work / "prepared.tif"
        warp_local_mosaic(mosaics, prepared, bounds, resolution, env)
        provenance = {
            "collection": "USDA NAIP via Microsoft Planetary Computer",
            "catalog": "https://planetarycomputer.microsoft.com/dataset/naip",
            "sampling_zoom": args.sampling_zoom, "mercator_resolution_m": resolution,
            "intermediate_utm_resolution_m": ground_resolution,
            "requested_year": args.year, "source_items": list(items),
            "sources": [href for _, href, _ in sources],
            "bounds": bounds,
            "note": "Preview resampling, not native NAIP resolution; compiler rejects missing coverage.",
        }
        with prepared.open("rb") as file:
            os.fsync(file.fileno())
        with sidecar.open("x") as file:
            file.write(json.dumps(provenance, indent=2) + "\n")
            file.flush()
            os.fsync(file.fileno())
        os.link(prepared, output)
        sync_directory(output.parent)
    print(f"Prepared local input: {output}. Run GroundEffect prepare to verify all texture coverage.")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        print(f"prepare_naip: {error}", file=sys.stderr)
        sys.exit(1)
