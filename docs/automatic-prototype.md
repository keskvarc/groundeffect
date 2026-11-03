# Automatic reader prototype

This is an implementation decision, not a claim that global streaming is ready.
The native Raleigh generation remains the independent correctness baseline.

## Boundaries

- Rust owns one read-only FUSE mount, cache, workers and source policy inside the
  existing engine. Go controls and presents that same runtime over existing IPC.
- Physical AutoOrtho DSFs and supported terrain/material resources are borrowed,
  not produced by running AutoOrtho. DSFs remain byte-identical.
- Terrain references use canonical, explicit day/night names. Their footprint
  and loading centre, rather than the selected image dimensions, identify them.
- Built-in `terrain_Water` stays in the unchanged DSF table without being
  mistaken for a local file. Borrowed coastline PNGs, border references and
  `LOAD_CENTER_BORDER` declarations remain unchanged when base/night DDS
  dimensions change. Syntax and finite loading parameters are validated.
- No region is compiled in advance. Opening metadata does not fetch imagery;
  requests wholly inside the DDS header return its authoritative bytes without
  fetching imagery. Kernel readahead may extend application header reads into
  payload demand; those requests schedule at most two native fetch/build jobs.
- The first profile is complete 1024-pixel BC1 DDSs with matching day/night
  dimensions and all mips. This is texture-demand streaming, not mip-granular
  network or storage savings. Kernel read ranges and mip intersections are
  counted so real simulator behaviour can inform the next optimization.
- Cached data is immutable. Cold requests coalesce, wait for independently
  executed work, or fail explicitly. There are no provisional pixels or
  short successful reads before EOF.

## Sources and qualification

Day imagery uses the public USGS **Imagery Only** pre-rendered Web Mercator
tile pyramid (`/tile/{z}/{row}/{col}`), replacing dynamic two-step `/export`
rendering (`2.7s–5.5s` and `9.2–10.1 MiB` per 2×2 parent). Each `256×256` tile
response must be an exact-size 8-bit RGB JPEG/PNG or RGBA PNG; HTTPS
destinations, response bytes (`4 MiB` per tile) and request duration are
bounded, with up to 16 concurrent keep-alive tile requests per worker.

The provider acquires deterministic **2-by-2 source groups**, doubling both
the EPSG:3857 extent and pixel dimensions, then taking exact child crops. This
does not reduce DDS dimensions or move footprints. A four-entry RGBA pool has a
64-MiB capacity reserved in the same global budget, in addition to existing
worker reservations. Same-source acquisitions coalesce; source leases survive
pool eviction. Both footprint and requested source dimensions identify a pool
entry so boundary sampling cannot alias a different resolution. Each pooled
parent raster tracks its populated 4-bit quadrant mask and fetches only the
quadrants registered by loaded `.ter` definitions (upgrading the pooled entry
if an adjacent DSF later registers an additional quadrant), eliminating the 40%
unused-quadrant overhead on sparse 2×2 parents (`321` footprints across `133`
parents in `+35-079.dsf`).
Inland footprints still require
every requested crop pixel to have full alpha (`255`). When a footprint has a
verified borrowed `terrain/{row}_{col}_BI{zoom}_sea_overlay.ter` and matching
8-bit grayscale coastline mask `textures/{row}_{col}_ZL{zoom}.png`, transparent
offshore pixels beyond the NAIP boundary are permitted only where the coastline
mask is strictly zero (including a 1-pixel bilinear safety margin across scale
ratios); any uncovered land/coastal pixel (`mask > 0`) or completely transparent
crop still fails explicitly. Coastal footprints bind the mask path and stamp
under `"groundeffect/usgs-imageryonly/coastal-tile-group2-v1"` while inland
footprints use `"groundeffect/usgs-imageryonly/tile-group2-v1"`. Transient HTTP,
`Content-Length` truncation and tile stream-read errors retry up to three times
before failing; deterministic contract mismatches fail immediately.

Because Ortho4XP DSF `TERT` tables space the four child footprints of a 2-by-2
parent 30–60 entries apart, a four-entry `SourcePool` alone would evict parent
rasters before sibling children are demanded (282 acquisitions for 133 parents in
`+35-079.dsf`, and 1,370 acquisitions for 560 parents across an eight-DSF startup
trace). After waking the demanded child's FUSE reader, the worker immediately
materializes any remaining registered sibling `Day` specs from the already-ready
pooled parent raster whenever they fit in the managed cache budget without
eviction and without issuing additional network requests. Per-worker `Lighting`
renderers are also pooled so concurrent `Night` builds do not serialize on a
single SQLite/renderer mutex.

Service metadata describes mixed-scale Blue Marble/Landsat and mostly NAIP
imagery. It lists CONUS NAIP acquisition years 2017–2021 and a June 2024 service
refresh. This is **not** a newest-survey guarantee and is not pixel-identical to
the native alpha's NC 2022 source. Alaska SPOT data has download restrictions;
the prototype is restricted to a continental-US test envelope and makes no
worldwide imagery claim.

The first exact Raleigh-footprint, 512-pixel export returned a completely opaque
RGBA image in 1.385 seconds, including the metadata and image transfers. This
single observation is not a throughput or availability guarantee. The newer
USGS NAIP Plus export returned HTTP 502; Esri's NAIP service required a token.
Neither becomes an implicit fallback.

The real Raleigh source slice built four adjacent **complete 1024-pixel,
699,192-byte BC1 DDSs** from one 2048-pixel parent tile mosaic (`64` `z=14`
tiles, `2,035,357` network bytes vs `10,082,177` bytes via `/export`) in
**`0.708 seconds`** in release mode (`4.921s` in unoptimized debug mode).
Rebuilding those four textures from the pooled source preserved every byte and
added zero HTTP calls. Across all `54` coastal `_sea_overlay` footprints in
`+34-078.dsf` and `+35-078.dsf` (`656` tiles) and `20` `ZL18` footprints in
`+35-079.dsf` (`320` tiles), the USGS tile pyramid returned `200 OK` with zero
errors, and all four previously failing coastal/inland footprints completed live
qualification in **`1.04 seconds`** (`0.26s` per parent group vs `17.16s` with
`/export`). The normal immutable DDS cache still serves committed hits without
source lookup or encoding.

The optional live qualification is reproducible with:

```sh
nix develop --command cargo test streaming::source::tests::actual_adjacent_dds_builds_share_one_exact_export -- --ignored --nocapture
nix develop --command cargo test streaming::source::tests::actual_coastal_and_previously_failing_footprints_build -- --ignored --nocapture
```

The second qualification test exercises all four previously failing footprints
recorded in `.local/state/engine.log` (three coastal `_sea_overlay` footprints
in `+34-078.dsf` — `26096_18624_BI16`, `26048_18704_BI16`, `26032_18720_BI16` —
and the previously truncated inland footprint `25552_18160_BI16` in
`+36-081.dsf`), verifying 699,192-byte BC1 DDS generation and all 11 mips. All
13 `_sea_overlay` footprints in `+35-078.dsf` were also verified against their
borrowed coastline masks with zero uncovered land pixels.

It is deliberately excluded from ordinary offline checks. Prior source-recipe
objects remain accounted and immutable; the grouped recipe has a new identity
version rather than replacing served bytes.

Night emission initially reuses the already available, read-only normalized
OSM/VIIRS sources. Those borrowed files are reported separately from managed
storage. A 24.4-GiB national database is **not** the eventual installation
contract; compact, incrementally fetched lighting products remain a source
qualification gate. Road/building inference remains inference, not surveyed
per-lamp lighting.

References:

- https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryOnly/MapServer
- https://developers.arcgis.com/rest/services-reference/enterprise/export-map/
- https://developer.x-plane.com/article/terrain-type-ter-file-format-specification/

The published Laminar terrain reference documents `LOAD_CENTER` and border
textures but not `LOAD_CENTER_BORDER`. Support for the latter preserves actual
borrowed source declarations rather than inventing or rewriting their rendering
behaviour.

## Commands

Both commands and the dashboard use the same engine operations:

```sh
groundeffect [--data-dir PATH] stream configure --config FILE
groundeffect [--data-dir PATH] stream start
groundeffect [--data-dir PATH] stream stop
groundeffect [--data-dir PATH] stream install --target ABSOLUTE_PATH
```

The one-time JSON configuration has `source_package` (physical mesh, not an
AutoOrtho mount), `mount_point` (exactly absolute `DATA_DIR/reader`), `osm_db`,
optional `viirs`, `source_revision`, `size`, `exposure` and `budget_mib`. There is
no region field. Supported preview sizes are 256, 512 and 1024; exposure is
0..2 and the managed ceiling is 512..131072 MiB.
Borrowed source paths must be absolute and outside managed storage so autostart
does not depend on the launching terminal's working directory or misclassify
owned lighting data as external.

Installation creates a new symlink without replacing scenery or editing
`scenery_packs.ini`. The simulator must enable it above competing base meshes,
but below airports and overlays. Mount before starting X-Plane. Cached/native
packages are not transparent substitutes for existing handles after a crash.

## Ownership and lifecycle

The configured byte ceiling includes the reader's cache, staging reservations,
bounded identity ledger and other engine-managed files, including retained
native generations. Large mesh and lighting inputs remain borrowed; supported
small material resources are validated and cached.
Ordinary preparation cannot grow managed storage while this reader is mounted.

Cache publication is atomic and checksummed. A bounded persistent ledger retains
the checksum after eviction: a changed upstream image cannot silently recreate
different bytes under an identity already served. Updating source revision is
explicit and requires an idle, unmounted reader.

Files acquire pins before first read, not only after generation. The engine
refuses reader stop while
requests, handles or observed reader processes remain active; closing the TUI
never stops the reader. Configured desired operation survives an engine restart.
A crashed FUSE mount is an explicit disconnected filesystem, not something a
new PID can repair for existing simulator handles.

Lifecycle decision, 2026-10-31: retain the FUSE thread until it actually exits;
detaching a mount is not proof that its connection ended. Capture the owned
connection from the session's Linux `/proc/self/fdinfo` entry and hold its
owner-only fusectl `abort` file. After quiescing an idle reader, attempt ordinary
SDK unmount first; if the session remains alive, abort only that captured
connection and join without panics. Failed teardown keeps the thread and recovery
handle available for retry. Missing kernel recovery support is logged and a
teardown timeout remains an explicit error, not permission to restart over an
unresolved session. This does not identify which external reference caused the
observed disconnected trial.

Linux documents lazy-unmount connection lifetime and the owner-only abort
interface: https://docs.kernel.org/filesystems/fuse/fuse.html

Borrowed-mesh decision, 2026-10-31: supported US cells include DSFs larger than
the old 64-MiB cached-material limit (the inspected `+29-090.dsf` is
147,448,133 bytes). Do not fix this by multiplying mesh copies and worker
reservations. Validate a borrowed DSF's checksum in bounded chunks, inspect its
bounded definition table (4 MiB, at most 4,096 terrain names) using the shared
parser, and serve immutable ranges
from its read-only source descriptor. Preserve the shared 256-MiB DSF limit;
other cached materials retain their 64-MiB bound. Stamp checks before/after
validation and source-backed reads reject replacement or truncation; existing
kernel-cached bytes may continue to serve the already validated original data.
Each handle retains its
validated descriptor; warm ranges do not reparse mesh dependencies. Borrowed
mesh bytes do not consume the owned cache or imagery workers' staging space.

Filesystem failures are exposed in runtime counters and the last-error field,
not just stderr. Asset and filesystem error messages are truncated and limited
to their first 64 reports per runtime to bound repetitive diagnostics.

## Observed real reader delivery

Two physical cells, Raleigh `+35-079` and its eastern neighbour `+35-078`, were
read through the actual kernel FUSE mount without regional imagery preparation.
Both DSFs stayed byte-identical; each demanded day/night DDS was 699,192 bytes,
1024 pixels, with all 11 mips independently decoded.

| Complete texture | Cold read | Warm read | First read after engine restart |
|---|---:|---:|---:|
| Raleigh day | 3.150 s | 0.469 ms | 1.158 ms |
| Raleigh night | 0.123 s | 0.468 ms | 1.002 ms |
| Eastern neighbour day | 2.832 s | 0.449 ms | 0.938 ms |
| Eastern neighbour night | 0.102 s | 0.464 ms | 0.940 ms |

Raleigh night bytes matched the independently inspected native reference.
Cold delivery built six assets (two DSFs and four DDSs), used four imagery HTTP
requests and received 4,537,940 bytes. Warm and restart reads preserved all
hashes and added no network requests or builds. The six cached assets occupied
44,290,048 allocated bytes. This is one isolated complete-file workload, not a
throughput guarantee, a cold-storage benchmark or X-Plane loading time.
These measurements describe the initial copied-DSF prototype; borrowed-mesh
delivery is qualified separately below.

The real X-Plane process subsequently requested both day/night files through
the installed streaming package. A live mount alone was not used as proof.
Full cold startup and visual/performance qualification are separate gates;
whole-DDS downloads can still cause substantial loading waits.

### Cold simulator performance gate: failed

An eight-minute follow-up observation never reached an idle delivery queue:
typically 16 reads remained pending with both workers busy. A subsequent
snapshot showed 1,266 generated assets, 1,261 imagery HTTP requests and
1,385,985,001 received bytes, with no reported delivery errors. X-Plane's window
still showed the menu rather than a usable flight. This is not an acceptable
normal cold-start experience, despite correct demand delivery and bounded work.

The initial candidate needed to distinguish DDS-header inspection from payload
demand.
At that time even a header read dispatched complete image acquisition/encoding;
cached kernel I/O can also extend that inspection through readahead. Authoritative
fixed headers and measured per-DDS I/O policy are candidates to remove that work
without returning placeholders or short successful payload reads. Do not simply
increase concurrency or call this performance gate complete.

### Header-first experiment

The experiment used per-file direct I/O for generated day/night DDS opens to
prevent kernel
readahead from turning a 128-byte header inspection into a payload request.
Mesh, terrain and retained material files keep ordinary cached kernel I/O.
This was not a blanket direct-I/O performance recommendation.

Header-only ranges return exactly the same fixed header as the complete
encoder. Their declared dimensions, length and full mip layout do not change.
Any request extending into payload resolves its entire requested range or
fails; it never receives a short header-only success. Header inspection still
pins the identity before first payload read. Runtime counters distinguish
header-only and payload reads.

The experiment's acceptance required real FUSE header reads with zero image
acquisition/builds,
byte equality with the subsequent complete DDS, unchanged range/EOF/error/pin
behaviour, and another cold simulator run from an empty active cache. Direct-I/O
mmap behaviour and broader simulator compatibility are not assumed. Compare
warm complete/ranged delivery separately from cold-source savings.

The cold X-Plane experiment recorded zero header-only reads and continued
payload work with a saturated queue. The isolated header improvement therefore
did not establish a simulator speedup. The unproven direct-I/O override has been
removed; normal cached I/O and read-only mmap delivery are exercised by the
process fixture. Exact header-range handling and bounded read telemetry remain,
without claiming that X-Plane issues header-only requests.

Later teardown/source diagnostics also
reported an unmount-completion failure, unsupported material size and image
coverage/decode failures. Owned-connection teardown recovery and bounded borrowed
mesh delivery now address the lifecycle and raw-DSF size boundaries; imagery
coverage/decode failures remain separate blockers, not scheduling wins.
Do not keep restarting the simulator as a substitute for resolving those
boundaries. The next working-set design is in
[the flight-aware design](design.md#flight-aware-working-set), including optional
SimBrief route foresight.

### Delivery recovery qualification

The real process fixture now serves a 65-MiB mesh, checks source replacement on
an uncached range, exercises read-only DDS mmap, and stops/restarts while an old
directory descriptor keeps the detached filesystem referenced. Active ordinary
file readers still prevent stop.

The actual `+29-090.dsf` was then read through FUSE with all 147,448,133 bytes
matching an independent source SHA-256. First validated reader demand took
0.368 seconds; the complete read took 0.482 seconds. The borrowed source was
warmed by the independent hash, so this is not a cold-storage measurement.
There were zero imagery requests, generated assets, staging reservations or
delivery errors, and no mesh copy was added to the managed cache.
The reader stopped cleanly afterward; X-Plane was not launched.

### Native DDS mapping discovery

An actual X-Plane 12.4.4-r1 KRDU flight now has a qualified, bounded DDS syscall
capture. The probe verifies every native-reference asset's length and SHA-256,
chooses near/far and cardinal-extreme pairs from explicit terrain loading centres
and bindings, and lets the simulator initialize **without tracing**. After the
real menu is visually verified, an explicit attach marker makes the coordinator
exec the tracer, preserving its ancestor relationship. The simulator's
`TracerPid` was checked before Resume. No ptrace-policy changes or administrative
privileges were used.

The installed Steam manifest identifies X-Plane 12 as app **2014780**. Earlier
direct-launch probes incorrectly supplied 269950 and were not valid native
startup baselines. The corrected identifier reached the real menu. Launch-time
tracing also materially disturbed plugin initialization and has been replaced,
not retained as an alternative capture mode.

| Observed sample | Result |
|---|---|
| Verified native reference | 1,280 assets, 478,059,031 bytes; 642 complete DDSs |
| DDSs appearing in the capture | 12 distinct day/night files |
| Successful mappings | 16; `PROT_READ`, `MAP_PRIVATE`, offset zero, 699,192 bytes each |
| Explicit DDS `read`/`pread` calls | None in this sample |
| Rendered scene | Native KRDU night scene; not automatic cold scenery |

**A full-length mapping is not proof that every page or mip was consumed.**
The analyzer preserves that distinction, ordinary-read offset uncertainty and
unfinished/resumed syscall assembly. It rejects truncated captures and unknown
reference files. Raw traces, reference policy, analysis and screenshots remain
private local qualification artifacts, not shipped scenery or source datasets.

The process fixture now uses the same 1024-pixel DDS length and read-only
`MAP_PRIVATE` mode. Reading **only its first 128 mapped bytes** caused a real
kernel FUSE request `[0, 131072]` on this host. Saved simulator FUSE read
telemetry confirms that distant `LOAD_CENTER` DDS loads issue two kernel
readahead ranges per file totaling 199,480 bytes: `[0, 131072]` (page-0 header
fault expanded by 32-page `do_sync_mmap_readahead`) followed by
`[630784, 699192]` to EOF (fault at page 160 / byte 655,488, the exact start of
256×256 `mip 2`, backed up 6 pages by `do_sync_mmap_readahead`). Because
`MAP_PRIVATE` caches returned pages in the kernel `address_space`, FUSE reads
must always return authoritative immutable DDS bytes rather than provisional
payloads.

The capture has a 300-second total deadline and 10,000-syscall bound. A pidfd
watcher stops only the owned tracer/simulator; it does not discover targets by
name or signal unrelated processes. A deadline-interrupted tracer may exit
nonzero; only a complete, nonempty analysis is usable evidence. This remains a
diagnostic probe, not a normal-flight launcher or a performance benchmark.

A separate **untraced Steam launch** reached an observed rendered native-airport
frame by 88.047 seconds after the recorded Resume transition, with the
prior-session dialog still present. Dismissing that dialog checks UI input
separately; attempted view shortcuts did not demonstrate a changed view.
This is a warm-source observation upper bound,
not a precise first-interactive timestamp or a completed cold/warm benchmark.
The numerical automatic-airport candidate target is **no more than 30 seconds
of added Resume-to-interaction latency** over a paired native run; it is an
acceptance target, not a measured improvement.

The implementation direction is shared, bounded source acquisition for immutable
complete assets, rather than another header-only shortcut. Exact source-group
sampling, coverage/decode behavior and the paired first-interactive benchmark
still need qualification before this renderer/source milestone can pass.

## Acceptance

Exercise real FUSE metadata, full/ranged reads, EOF, readonly errors, canonical
sharing, cold coalescing, immutable warm restart, quota/pins, source failures and
stop protection. Independently decode generated DDSs and compare geography and
orientation. Activate separately from the native baseline, then distinguish
actual X-Plane requests from a healthy mount or a running process. Simulator
visual quality and load/frame-time results remain unqualified until observed.
