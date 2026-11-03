# GroundEffect research and evidence

Research date: 2026-10-30. This is a primary-source-led design review, not a
completed comparative benchmark.

## Method and evidence labels

**Verified** means a reviewed primary source supports the stated behavior.
**Observed** means a local experiment reported it, with the limitations below.
**Proposed** means an engineering choice or acceptance target.
**Unresolved** means a simulator test or comparative measurement is still needed.

Search-generated summaries were used only to discover sources. Some contradicted
the actual specifications, including terrain LIT discovery, the location of
`LOAD_CENTER`, and unsupported claims about lighting distances. Those statements
and purported quotations are not accepted evidence.

Most source links are living documents. Kernel-sensitive FUSE conclusions use
Linux 6.18 documentation rather than assuming everything in a latest-version
page exists on the target kernel. Freeze exact library/tool releases and source
revisions during implementation; a repository's default branch is not a
reproducible dependency pin.

## Findings that determine the architecture

### 1. X-Plane already has the required terrain emission hook

**Verified:** `.ter` files explicitly support base and night textures. Night
emission is added to the terrain; an implicit `_LIT` suffix is not enough.
`LOAD_CENTER` changes texture loading resolution with distance, applies to the
night texture too, and recommends DDS for fast low-resolution reloads [R01].

**Consequence:** a stock scenery-compatible solution is feasible without a new
rendering plugin. This does not establish exact load radii, loader concurrency,
read offsets, or the behavior of different base/night image dimensions.

The older specification remains useful for its defined resource semantics; it
is not proof of every detail of the current X-Plane 12 renderer.

### 2. Linux can deliver immutable assets without a userspace read daemon

**Verified:** EROFS and SquashFS are kernel read-only filesystems with compression
and compact tree representations. Compression extent/block size affects access
amplification. EROFS explicitly cautions against large clusters for random access
[R04-R06].

**Proposed:** compare them with ordinary prepared ext4 files. A kernel image may
remove FUSE request/response overhead and reduce storage, but there is no
GroundEffect workload measurement proving it beats ext4.

**Unresolved:** night DDS compressibility, metadata cost, image build/update
amplification, kernel decompression memory, and mount/deployment overhead.

### 3. Cached FUSE and direct I/O make different tradeoffs

**Verified:** FUSE direct I/O bypasses page cache and has no readahead; cached mode
can satisfy reads from cache and supports mmap [R09].

**Consequence:** selecting direct I/O simply because it sounds lower-level is
not justified. Immutable local assets are a strong case for letting Linux cache
reads. Cached FUSE still has misses, daemon scheduling, potential duplicate
caching, and connection-lifetime concerns.

### 4. Advanced transports are not free upgrades

**Verified:** Linux 6.18 documents FUSE-over-io_uring as an evolving interface,
with some request types still requiring `/dev/fuse`. Passthrough setup requires
`CAP_SYS_ADMIN` in the reviewed documentation [R10, R11].

**Consequence:** probe kernel/library support and measure benefit before using
either. FUSE-over-io_uring, ordinary file io_uring, passthrough, and splice are
different mechanisms, not interchangeable names for guaranteed zero-copy.

### 5. Existing on-demand filesystem infrastructure deserves evaluation

**Verified:** Nydus distinguishes FUSE-oriented RAFS v5 from EROFS-compatible
RAFS v6. Its fscache guide requires EROFS/cachefiles on-demand kernel
configuration [R07, R08]. CacheFiles documents exclusive `/dev/cachefiles`
ownership and restrictions on externally modifying its active cache [R28].

**Consequence:** Nydus is a credible later alternative to building a remote blob
filesystem. Container startup claims do not establish simulator performance.
The documented older v5 cache/chunk assumptions cannot be treated as v6 facts.

### 6. Accurate geometry is not the same as measured illumination

**Verified:** OSM `lit` has explicit on/off/intermittent semantics. EOG's VIIRS
products use coarse radiance grids and include observation-quality information;
zero radiance is not always evidence that no lights exist [R14, R15].

**Inference:** OSM plus VIIRS can create geographically plausible emission with
honest confidence levels. They cannot reconstruct a complete, physically exact
street-light inventory, visible color spectrum, ground reflection, or operating
schedule. A rendering design should not describe those inferred properties as
surveyed truth.

### 7. Source processing libraries are not the problem to replace wholesale

**Verified:** libosmium is a native OSM processing library; PROJ implements
coordinate transformations; GDAL supports tiled/overview raster preparation;
SQLite's R-tree supports spatial overlap queries [R16-R19].

**Consequence:** use them outside the flight path. A spatial index can be correct
and still be the wrong runtime representation for a texture loader.

## Historical local evidence, not GroundEffect benchmarks

Earlier AutoOrtho experiments on one dense Raleigh footprint reported:

| Stage | Earlier Python path | Native raster experiment |
|---|---:|---:|
| Source query | 0.693 s | 0.548 s |
| VIIRS processing | 0.105 s | 0.085 s |
| Rasterization | 4.948 s | 0.069 s |
| Upscale | 0.035 s | Approximately zero |
| DDS encode | 0.081 s | 0.005 s |
| Total | 5.862 s | 0.707 s |
| Output | 4096-square DDS, 11,184,952 bytes | 1024-square DDS, 699,192 bytes |

Input counts were 9,593 roads, 30,939 buildings, and 365 landuse polygons.
The earlier raster time included profiling overhead; the output dimensions also
differ. These are not controlled language benchmarks, repeated tail-latency
measurements, or an end-to-end simulator comparison.

The useful observation is narrower: batching removed much Python per-feature
overhead, but the native on-demand path still spent most of its time obtaining
source geometry. Changing language alone did not remove the loading dependency.

A separate prototype read/decompressed a prepared 1024-square lighting block in
roughly 1.7-4.5 ms in a small set of trials. That excluded final DDS creation and
did not establish cold-device behavior, country-scale capacity, or simulator
frame-time impact. It motivates preparing data; it does not prove a specific
archive format is optimal.

An earlier filesystem scan counted about 1.28 million North America terrain
definitions. This is an input-file observation, **not** the number of unique
textures needed for one flight or the required number of prepared DDSs.

Earlier FUSE logs included prefix reads near offset 0 and tail reads near the
small mip levels. Such logs include kernel behavior/readahead and are not an
authoritative record of simulator-issued system calls. Capture both layers.

The raw experiment results remain in the originating local session artifacts
(`lighting-baseline.json`, `lighting-native.json`, and the atlas probe), not as
a fabricated GroundEffect benchmark suite.

## Unresolved questions with explicit decision gates

| Question | Required evidence |
|---|---|
| Can base/LIT dimensions differ without undesirable `LOAD_CENTER` behavior? | Small static X-Plane fixture and approach/cruise reload trace |
| What does X-Plane actually read, and when? | Application-side trace plus filesystem/backend trace |
| Is EROFS better than ext4 or SquashFS here? | Identical asset corpus, matched options, cold/warm replay and simulator runs |
| Is custom FUSE worth its failure/maintenance surface? | Material storage or distribution advantage without unacceptable tail latency |
| How much space does a region actually require? | Unique reachable footprints and measured urban/rural/empty distributions |
| Which mip filter/exposure avoids dark RGB loss without excessive bloom? | Independent DDS decode plus fixed-exposure simulator comparisons |
| Does any newer DDS format help? | Specific supported simulator version and direct load test; no BC7 assumption |
| Can online refinement update already loaded textures safely? | Documented or experimentally established loader behavior, before promising it |
| What preparation throughput is sustainable on this machine? | Bounded full-region run with CPU/RAM/temp/disk measurements |

## Terminal-only interface revision

The desktop-interface recommendation was superseded by the requested
terminal-only experience. The engine recommendation stays Rust.

**Verified:** Bubble Tea is a Go TUI framework with a model/update/view event
architecture. Lip Gloss provides styling/layout and terminal color-profile
adaptation. Bubbles supplies reusable inputs, tables, progress indicators, and
other components [R33-R35].

**Verified:** Ratatui provides Rust terminal widgets/layout, while Crossterm
provides terminal input/output, raw/alternate-screen modes, and resize events
[R36, R37]. A polished interface is possible with either stack; neither
automatically supplies the product's visual design.

**Proposed:** use the actual Charm stack in a small Go terminal client, connected
to the Rust engine over the already-planned local control socket. This trades an
additional toolchain and protocol boundary for direct access to Charm's
components. It does not move rendering or Go garbage collection into the
simulator's read path. Shared-machine CPU and memory contention still apply.

Ratatui/Crossterm remains the simpler all-Rust alternative. No comparative UI
performance claim or implemented frontend is implied. See
[the terminal interface proposal](terminal-ui.md) for the visual and interaction
design.

## Source ledger

All entries below were consulted directly. Findings are paraphrased; no
unverified developer quotation is used.

| ID | Primary source | What it establishes / limitation |
|---|---|---|
| R01 | [Laminar terrain specification](https://developer.x-plane.com/article/terrain-type-ter-file-format-specification/) | Explicit `LIT_TEX`, additive emission, NOWRAP, and `.ter` `LOAD_CENTER`; not precise current loader timing |
| R02 | [Laminar DDSTool manual](https://developer.x-plane.com/docs/scenery/ddstool-manual/) | DXT1/3/5 and alpha differences; warns uncompressed DDS generally is not for scenery; does not establish BC7 support |
| R03 | [Ortho4XP geographic utilities](https://github.com/oscarpilote/Ortho4XP/blob/master/src/O4_Geo_Utils.py) | `wgs84_to_orthogrid`, `st_coord`, and Web Mercator pixel scale corroborate the 16-web-tile footprint |
| R04 | [Linux EROFS documentation](https://docs.kernel.org/filesystems/erofs.html) | Inode/directory layout, compressed clusters, chunk layouts, and deduplication mechanisms |
| R05 | [EROFS project FAQ](https://erofs.docs.kernel.org/en/latest/faq.html) | Compression/cluster/metadata/dedup comparison caveats and random-access warnings; project claims require local validation |
| R06 | [Linux SquashFS documentation](https://docs.kernel.org/filesystems/squashfs.html) | Read-only compression, packed metadata, block sizes, decompression modes; settings depend on kernel configuration |
| R07 | [Nydus design](https://github.com/dragonflyoss/nydus/blob/master/docs/nydus-design.md) | v5/v6 distinction and immutable blob architecture; detailed layout explicitly describes v5 |
| R08 | [Nydus EROFS fscache guide](https://github.com/dragonflyoss/nydus/blob/master/docs/nydus-fscache.md) | On-demand EROFS integration and required kernel features; older installation recipes are not current deployment instructions |
| R09 | [Linux 6.18 FUSE I/O modes](https://docs.kernel.org/6.18/filesystems/fuse/fuse-io.html) | Cached/direct I/O behavior, readahead and mmap distinctions |
| R10 | [Linux 6.18 FUSE-over-io_uring](https://docs.kernel.org/6.18/filesystems/fuse/fuse-io-uring.html) | Evolving protocol, per-CPU queues, mixed request handling; no generic latency guarantee |
| R11 | [Linux 6.18 FUSE passthrough](https://docs.kernel.org/6.18/filesystems/fuse/fuse-passthrough.html) | Backing registration, `CAP_SYS_ADMIN`, resource-accounting and stacking concerns |
| R12 | [libfuse low-level example](https://libfuse.github.io/doxygen/hello__ll_8c.html) | Low-level lookup/stat/read/session interface and threaded loop configuration |
| R13 | [fuser API](https://docs.rs/fuser/latest/fuser/) and [manifest](https://github.com/cberner/fuser/blob/master/Cargo.toml) | Native Rust FUSE candidate and explicitly experimental async feature; pin and audit the selected release |
| R14 | [OSM `lit` key](https://wiki.openstreetmap.org/wiki/Key:lit) | Lighting presence, no/disused/intermittent values, and distinction from missing data |
| R15 | [EOG VIIRS products](https://eogdata.mines.edu/products/vnl/) | 15-arcsecond grids, radiance units, product extent, filtering, and coverage-quality caveats |
| R16 | [libosmium manual](https://osmcode.org/libosmium/manual.html) | Mature native OSM processing and explicit memory-efficiency design |
| R17 | [PROJ overview](https://proj.org/en/stable/about.html) | Coordinate-transformation library and API rather than a new projection implementation |
| R18 | [GDAL COG driver](https://gdal.org/en/stable/drivers/raster/cog.html) | Tiled/overview raster preparation, compression choices, and temporary-space costs |
| R19 | [SQLite R-tree](https://www.sqlite.org/rtree.html) | Spatial range queries and float32/outward-rounded bounds; index bounds are not exact source geometry |
| R20 | [Rust ownership](https://doc.rust-lang.org/book/ch04-01-what-is-ownership.html) | Compile-time ownership model; not a comparative performance study |
| R21 | [Tokio synchronous/async bridging](https://tokio.rs/tokio/topics/bridging) | Isolating async work rather than forcing the whole application into one runtime model |
| R22 | [gtk-rs](https://gtk-rs.org/) | Previously evaluated desktop option; superseded by the terminal-only requirement |
| R23 | [Linux `pread(2)`](https://man7.org/linux/man-pages/man2/pread.2.html) | Explicit-offset reads without shared file-position changes; short reads remain possible |
| R24 | [Linux `mmap(2)`](https://man7.org/linux/man-pages/man2/mmap.2.html) | File-backed mappings and mapping semantics; not proof mmap beats reads |
| R25 | [Linux `io_uring_setup(2)`](https://man7.org/linux/man-pages/man2/io_uring_setup.2.html) | Shared queues, setup features, and polling CPU tradeoffs |
| R26 | [systemd resource controls](https://www.freedesktop.org/software/systemd/man/latest/systemd.resource-control.html) | Accounting, MemoryHigh throttling, MemoryMax/OOM behavior; availability requires correct controller delegation |
| R27 | [AMD Compressonator](https://github.com/GPUOpen-Tools/compressonator) | Linux-capable codec SDK with BC1 block API; not selected as the fastest encoder without benchmarking |
| R28 | [Linux CacheFiles documentation](https://docs.kernel.org/filesystems/caching/cachefiles.html) | Cache device ownership, management, and restrictions on manipulating active cache contents |
| R29 | [Linux `fsync(2)`](https://man7.org/linux/man-pages/man2/fsync.2.html) | Flushing file data does not by itself persist the containing directory entry |
| R30 | [OpenStreetMap copyright/licensing](https://www.openstreetmap.org/copyright) | ODbL, attribution, and data-use obligations; exact distribution model needs its own review |
| R31 | [Linux `posix_fadvise(2)`](https://man7.org/linux/man-pages/man2/posix_fadvise.2.html) | Advisory prefetch/eviction, not binding guarantees; DONTNEED may not evict requested pages |
| R32 | [Linux VM controls](https://docs.kernel.org/admin-guide/sysctl/vm.html) | System-wide VM/cache controls; inappropriate to change casually on a shared desktop |
| R33 | [Charm Bubble Tea](https://github.com/charmbracelet/bubbletea) | Go TUI framework, model/update/view architecture, keyboard/mouse handling and terminal rendering |
| R34 | [Charm Lip Gloss](https://github.com/charmbracelet/lipgloss) | Declarative styling/layout and color-profile downsampling |
| R35 | [Charm Bubbles](https://github.com/charmbracelet/bubbles) | Reusable TUI inputs, tables, progress indicators and other components |
| R36 | [Ratatui](https://ratatui.rs/) | Rust terminal widgets and constraint-based layouts; marketing performance claims are not GroundEffect measurements |
| R37 | [Crossterm](https://github.com/crossterm-rs/crossterm) | Rust terminal I/O, colors, keyboard/mouse events, resize events, raw and alternate-screen modes |

## Bottom line

The research supports moving source processing and encoding out of flight-time
delivery, preserving immutable DDS semantics, and using native Linux facilities
before inventing a filesystem. It does **not** yet identify a universally fastest
backend, a guaranteed full-country preparation time, or a zero-stall online
streaming solution.
