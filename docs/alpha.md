# Alpha 0.1: implemented contract

This is a native-file scenery compiler, Rust control engine and actual Charm
terminal client. It is not a streaming replacement for global AutoOrtho coverage.
Asset verification does not qualify appearance or load time in X-Plane.

## Architecture decisions

- Preserve the source DSF byte-for-byte, including mesh, elevation and UV
  mappings. Read its terrain table with bounded atom lengths and MD5 validation.
  Do not invent geometry from imagery, OSM, VIIRS or filenames.
- The accepted orthophoto terrain convention uses aligned 16-by-16 Web Mercator
  tile footprints. Cross-check filename bounds against the terrain loading
  center. Changing output dimensions never changes geographic coverage.
- Rust owns configuration validation, one job, publication, verification and
  explicit installation. Go owns terminal presentation. Both CLI and TUI use the
  same local control operations.
- A small, bounded, atomic JSON snapshot is the alpha catalog. There is one
  writer and at most 32 registered generations; a second catalog database would
  add no present consistency benefit. SQLite is used read-only for the existing
  source dataset, not as another asset-delivery layer.
- Compile in a staging directory, verify all manifest hashes and local resource
  dependencies, sync assets, then rename the whole generation. Only afterward
  register it in the catalog. An interrupted or unregistered directory is not a
  published generation. No automatic deletion or resumable jobs in this alpha.
- Terrains with the same footprint, source and loading center reference one
  canonical texture pair. Hard-linked aliases alone would remove duplicate
  disk bytes but leave different simulator texture identities. Source DDS
  providers and loading-center semantics remain part of reuse identity; an
  explicit raster overrides the old provider choice.
- Simulator reads use ordinary prepared files. No running engine, database
  query, network fetch, rasterizer, codec, FUSE mount or terminal is required.
- GDAL is an offline preparation dependency. The locked Nix environment pins a
  compatible 3.11 SDK separately: the newer 3.13 C API is incompatible with the
  selected Rust wrapper. Native bindings are generated using Nix's bindgen hook.
  This does not put GDAL on the simulator's read path.

## Commands and control boundary

Global options precede the command:

```sh
bin/groundeffect --help
bin/groundeffect engine start
bin/groundeffect configure --config /absolute/path/to/preview.json
bin/groundeffect prepare --wait
bin/groundeffect --json status
bin/groundeffect verify --generation GENERATION_ID --wait
bin/groundeffect tui
bin/groundeffect engine stop
```

Default data directory: `$XDG_DATA_HOME/groundeffect`, or
`~/.local/share/groundeffect`. Use `--data-dir /absolute/path` for an independent
workspace. The directory must be owned by the current non-root user and private
(0700). The socket is 0600; an exclusive file lock prevents a second owner.

`tui` starts/reconnects to the sibling engine binary. Closing the TUI does not
cancel a job. CLI commands other than `engine start` expect an existing engine.
`engine stop` refuses while a job is running; already prepared scenery is
independent of the engine. A killed engine records an interrupted job as failed
on its next start. Staged/unregistered output is retained for inspection, not
silently promoted or automatically resumed.

The version-1 protocol is one JSON request and response line per Unix connection.
The source of truth is `engine/src/model.rs` and method dispatch in
`engine/src/service.rs`; `client/protocol.go` is the thin typed client.
Operations: `status`, `configure`, `prepare`, `verify`, `install`, `shutdown`.
Configuration fields match `PrepareConfig`; verify takes
`{"generation":"ID"}`, install adds `"target":"/absolute/new/package/path"`.

Example request:

```json
{"version":1,"id":"example","method":"status","params":{}}
```

Response: `version`, matching `id`, either `result` (engine snapshot) or `error`.
Frames are bounded to 256 KiB. Control connections are serviced serially with
two-second read/write timeouts; expensive preparation/verification runs on one
independent worker. Slow clients cannot backpressure native scenery reads.
The dashboard polls at five seconds idle/one second during work; requests and
terminal rendering never run in the data path.

Keys: `1/2/3` or Tab navigate, `p` requests preparation with confirmation,
`j/k` select a generation, `v` verifies it, `q` quits only the UI.
Configuration and installation paths are supplied through the scriptable CLI.
Non-TTY use has an explicit error; use `--json status` instead.

The dashboard explicitly shows the local engine as connecting, connected or
disconnected, based on its control responses. Its installation indicator and
Generations-page detail describe the engine's recorded installation, not a
fresh check of scenery priority or proof of an X-Plane terrain load. Native-file
delivery has no live simulator connection; do not present one as connected.

## Source inputs and quality

Start from `examples/preview.json`, replacing all paths and provenance.

- Use the **physical** source package with an uncompressed version-1,
  terrain-only DSF and `terrain/*.ter` files, not empty FUSE mount placeholders.
  Built-in `terrain_Water` is preserved. The qualified external decal is
  `lib/g10/decals/maquify_2_green_key.dcl`; X-Plane's standard library remains
  an explicit external dependency.
- The conservative terrain directive allowlist rejects unsupported materials
  rather than silently dropping dependencies. Border masks are copied and
  sealed. Source LIT bindings are replaced, base/LIT bindings and loading-center
  pixel dimensions are updated together, other accepted semantics are preserved.
- `imagery_raster`: a complete, north-up, pixel-aligned EPSG:3857 GeoTIFF with
  tagged 8-bit RGB bands and valid masks. Entire native source-window masks are
  checked, not just output sample locations. Missing coverage fails preparation.
- Set `imagery_raster` to `null` to use complete standard BC1/BC3 source DDS mip
  chains. This does **not** import AutoOrtho's partial/compressed cache format.
- `osm_db`: the legacy normalized `ways_idx` R-tree / `ways_data` schema.
  Queries stream roads/buildings in a read transaction; no country-wide load.
- `viirs`: local derived RDT1 data, or explicit `null` for no relative dimming.
  Decompression sizes, index offsets and decoded lengths are validated.

Profiles are explicit: 256, 512 or 1024 square output, with complete DDS mip
chains. Raster imagery and inferred emission use BC1; supplied base DDS retains
its BC1/BC3 block format. A 1024 BC1 texture occupies 699,192 bytes.
Alpha profiles are previews,
not a claim of native 4096-pixel orthophoto fidelity. Record the **actual source
sampling resolution**, especially when upsampling a smaller footprint.

Lighting uses inferred road emission and building centroids. Legacy normalized
records do not retain trustworthy per-lamp locations or explicit `lit=no`
semantics; do not describe the result as a lamp survey. Landuse flooding is not
implemented. RDT1 lacks observation-quality metadata, so absent cells are
unknown and do not force surveyed darkness. Relative radiance dimming uses
`log1p(radiance)/log1p(25)` clamped to 0.25..1; these are aesthetic heuristics,
not calibrated photometry. Emission support has a four-metre half-width,
road/building energy weights 0.035/0.018 and warm RGB weights 1/0.65/0.25,
multiplied by source tier and declared exposure. Mips average linear light
before sRGB encoding; BC1 quantization still limits very dim small features.

For open US daytime input:

```sh
nix develop
python scripts/prepare_naip.py --config /absolute/path/to/preview.json --year 2022
```

This optional offline helper asks the Planetary Computer NAIP STAC catalog for
the **whole mesh's imagery footprints**, pre-samples individual COG overviews
with four bounded GDAL workers, groups those local previews by CRS, then warps
the local mosaic to an aligned RGB/alpha raster. This deliberately bounds remote
work per source before mosaicking: a huge remote VRT can suffer request churn
even with explicit virtual overviews. No native-resolution regional download
is required. TLS verification stays on.
Completed source previews are atomically cached beside the output in
`<raster>.sources/`, keyed by source URL/item and sampling profile, with integrity
hashes. A failed final warp does not discard successful downloads. This is
preparation input, never a simulator read dependency; the selected cached inputs
remain pinned until deliberately removed/refreshed.
The helper records source URLs/items and the sampling grid in a sidecar.
It does not certify coverage: the compiler checks the completed raster.
NAIP source rights and mesh source rights remain separate; no source datasets
are committed and no general redistribution permission is implied.

## Resource and failure limits

One worker, one decoded VIIRS tile (maximum 32 MiB), GDAL cache 64 MiB,
SQLite cache approximately 4 MiB, maximum 100,000 queried features/footprint,
1 MiB feature JSON, maximum 4096-square source windows, 256 MiB DSF and 4096
terrain definitions. Output budget is 32..16384 MiB **logical bytes per job**,
reserved conservatively with a further 64 MiB free-space margin. This is not a
global cleanup quota or a filesystem space reservation against other programs.
Old generations and failed staging still consume disk; inspect before removal.
Do not delete an installed or in-use generation.
The helper's retained source cache is capped at three GiB per output, with up
to four 32-MiB previews in staging; it does not evict other source products.

The NAIP helper has separate caps: 3-by-3 degrees, 2000 STAC items, 16 MiB per
catalog response, two GiB uncompressed final raster estimate, three GiB
intermediate estimate, four source workers with three-minute subprocess
deadlines, bounded HTTP retries, one-hour local mosaic deadlines and a
32-dataset VRT pool. Dependencies also
consume memory; these component limits are not a measured total-RSS promise.

Hashes and structural checks run before publication and before installation.
Starting a re-verification clears its recorded verified flag; failure never
restores it. A catalog persistence failure does not expose a successful job.
Read-only permissions express engine immutability, not protection against the
owner deliberately changing files. Run verification after external changes.

## Observed Raleigh alpha run

The local KRDU preview has been compiled from the physical AutoOrtho North
America package's `Earth nav data/+30-080/+35-079.dsf`. The existing mesh and
terrain bindings supplied the mapping; imagery and lighting did not invent it.

| Item | Observed result |
|---|---|
| Source mesh | 29,035,189 bytes, preserved byte-for-byte |
| Terrain definitions / geographic footprints | 636 / 321 |
| Daytime input | 324 USDA NAIP 2022 sources, prepared as 12,288-by-15,360 EPSG:3857 RGB/alpha |
| Sampling | z14, 9.5546 projected metres/pixel; output is an explicit 1024-pixel preview |
| Published assets | 1,280, including 642 canonical DDS files |
| Package bytes | 478,059,031, approximately 455.9 MiB |
| Offline compilation | Approximately 79 seconds on this workstation, excluding source acquisition |
| Source-cache retry | All 324 previews reused; byte-identical raster rebuilt without COG downloads |

The engine accepted complete source-window coverage and published only after
checking all dependency hashes. An independent Pillow decoder then decoded
every mip of all 642 DDS files, with the control engine stopped. A real-footprint
comparison against a separate GDAL crop had mean absolute RGB error 4.51/255;
the vertically flipped comparison was 31.96/255. Separately, a synthetic quadrant
image decoded identically to Laminar's official DDSTool output, confirming row
orientation without guessing a vertical flip.

A 1,000-operation **warm** local-file probe of open/fstat/pread-4-KiB/close
measured approximately 0.010 ms p50 / 0.020 ms p99, after hashing and decoding had
warmed the files. This is neither cold storage nor whole-DDS throughput nor
X-Plane loading/frame-time evidence.

The actual Charm client rendered during preparation; exiting it left that job
running. Explicit installation was exercised only in a temporary directory:
the new link resolved correctly and existing/broken links were refused. No
simulator installation or scenery priority was changed.

This package is a terrain-only base mesh. Retain the normal airport/overlay
packages for 3D buildings, road lights and other objects; those are not generated
by the inferred texture-emission layer.

## Installation and simulator qualification

Only with X-Plane stopped and an intentional scenery priority decision:

```sh
bin/groundeffect install --generation GENERATION_ID \
  --target "/absolute/X-Plane/Custom Scenery/z_groundeffect_rdu_preview" --wait
```

This creates a **new** symlink and refuses any existing target, including an
existing/broken symlink. It never replaces AutoOrtho, modifies another
application, edits `scenery_packs.ini` or changes a live flight. The original
package need not remain mounted. Installation is not proof of correct scenery
load order: put this base mesh below airports/overlays and above a competing
base mesh deliberately, then inspect X-Plane's load log.

Remaining gates: daylight landmark alignment, water boundaries, low/high
altitude night appearance, distance transitions, real cold/warm simulator load
time, frame-time traces and source-to-output visual fidelity. No claim of
optimal performance or photometric accuracy substitutes for those measurements.
