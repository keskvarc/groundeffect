# GroundEffect: proposed Linux architecture

Date: 2026-10-30  
Status: researched proposal; implementation and backend selection gates remain open.

Implementation scope and alpha decisions are documented in
[the alpha contract](alpha.md) and [automatic prototype](automatic-prototype.md).
This broader design remains the expansion
proposal, not a claim that every subsystem below is implemented.

Source identifiers refer to [the research ledger](research.md#source-ledger).
Performance targets refer to [the benchmark plan](benchmark-plan.md), not
achieved results.

## 1. Recommendation

**Build an automatic, cache-first scenery streamer with immutable asset identity,
not a manual regional compilation workflow.**

Decision revision, 2026-10-31: the user's normal workflow must not require region
selection or long-running regional preparation. The native-file alpha remains
the correctness/performance reference, not the finished product workflow.
Do not expand it by repeatedly compiling whole regions before testing demand
loading. This supersedes the earlier ready-coverage-first product recommendation.

Rust owns a bounded fetch/build scheduler and persistent cache. A small read-only
filesystem presents the existing mesh/terrain mappings and fixed DDS layouts.
Cached requests read immutable encoded bytes. Cold requests coalesce into
prioritized worker jobs for the required mip/range; they may wait for those jobs.
Look-ahead should make that cold dependency uncommon during flight, not hide it.

A cached Rust FUSE reader is the leading prototype for this dynamic interface.
Measure real X-Plane reads before choosing chunk granularity, encoding strategy
or transport optimization. EROFS/SquashFS remain local packaging candidates;
remote prepared-asset delivery is another candidate where data and rights exist.
None is evidence of fastest performance or guaranteed cold-load latency.

### Decisions and their confidence

| Decision | Recommendation | Status |
|---|---|---|
| Normal workflow | Automatic demand loading and look-ahead; no manual regional preparation | Product requirement |
| Cached flight reads | Read immutable encoded bytes; no source-data processing | Architectural requirement |
| Cold flight reads | Bounded, coalesced fetch/build work off the filesystem callback; explicit waits/errors | Prototype acceptance requirement |
| Application language | Rust; narrow, documented native boundaries | Recommended |
| Delivery baseline | Ordinary read-only files on ext4 | Required comparison |
| Linux packaging | EROFS, beginning with conservative LZ4 configurations | Local-cache/export candidate; not yet benchmarked |
| Alternative packaging | SquashFS with comparable compression and concurrency | Required comparison |
| Custom filesystem | Cached, read-only FUSE over immutable assets | Whole-DDS prototype implemented; mip-granular generation and look-ahead remain open |
| Advanced FUSE transport | Passthrough / FUSE-over-io_uring | Experimental optimization gate |
| Runtime data store | Immutable files/images; SQLite for control metadata | Recommended |
| Lighting source | Offline, shared geographic emission pyramid | Recommended |
| Interface | Terminal-only TUI and scriptable CLI; Go/Charm client over local IPC | Recommended; after core validation |

## 2. Scope and honest guarantees

GroundEffect has two related jobs: deliver daytime orthophotos and add
geographically aligned night emission. A fast lighting subsystem does not fix a
daytime imagery downloader that stalls the same simulator loader.

The default product workflow is **automatic streaming**: actual filesystem demand
sets priority; access patterns and optional flight telemetry guide bounded
look-ahead. The user does not choose and compile a region before each flight.
A route is an optional hint, not proof of X-Plane's entire request set.

There is a physical limit to the alternative: a never-seen high-resolution image
cannot be downloaded, decoded, and encoded with a guaranteed sub-millisecond
deadline over an arbitrary network. Neither Rust nor io_uring changes that.

| Mode | Contract |
|---|---|
| Automatic streaming | Cache-first delivery; cold data fetched/built automatically with bounded concurrency, look-ahead and explicit failure behavior; not an offline or zero-stall guarantee |
| Cached working set | The committed, protected requested/prefetched data needs no fetch/build for those reads; not proof that every mip of every DSF dependency is local |
| Complete offline export | All package dependencies and mip chains exist locally at declared quality; ordinary native-file delivery remains valid |

### Flight-aware working set

Decision revision, 2026-10-31: airport setup and taxi commonly provide several
minutes of useful look-ahead. Exploit that time after the airport becomes
interactive; do not spend it holding the simulator behind a regional build.
This is the next runtime design, not implemented behaviour.

| Phase | Immediate requirement | Opportunistic work |
|---|---|---|
| Airport bootstrap | Verified mesh/material dependencies and near-field assets required for the first usable frame; broad valid coarse coverage | Establish departure context without building every distant full-resolution DDS |
| Parked/setup | Protect the airport working set and surrounding coarse coverage | Prepare taxi/runway surroundings, departure/climb corridor and turn margin |
| Taxi/departure | Runway and near-ground detail ahead of the aircraft | Extend the predicted climb corridor before it becomes visible |
| Cruise | Maintain coarse coverage in every direction and the current visible working set | Rolling route/heading corridor, detail appropriate to altitude and visibility |
| Descent/arrival | Prepare destination surroundings and approach detail before descent | Approach alternatives, vectors and missed-approach/turn margin |

Use measured live position, ground state, altitude, speed and course when
available. Source/request footprints are demand evidence, not exact aircraft
telemetry. Camera/frustum data and a flight plan are optional hints; neither can
be the sole coverage boundary.

Every need identifies a geographic footprint, quality/mip requirement, reason,
predicted visibility deadline and estimated fetch/build cost. The ordering is:
actual blocked reads; airport/near-field bootstrap; missing coarse safety
coverage; imminent departure/route coverage; speculative detail. Within a
prediction class, order by deadline slack (time until needed minus conservative
measured delivery cost), not FIFO or distance to the footprint centre alone.
Account for footprint edges, dependencies, turns and geographic scale.

Prefetch shares the existing source/cache pipeline, coalesces identical needs
and reserves bytes before dispatch. Reserve service capacity for new actual
demand; background work must not occupy every worker behind a long uninterruptible
source request. Cancel obsolete queued predictions on a route/position change,
reconsider in-flight work at bounded chunk boundaries, and retain completed
useful cache data. Apply ageing within speculative work without outranking
blocked simulator reads.

Adapt the look-ahead horizon to observed delivery latency, throughput, aircraft
motion and cache capacity. Do not hard-code a five-minute departure timer or
assume that every pilot waits. Preserve an all-direction coarse guard band even
when fine coverage follows a corridor. Pin the usable local/visible set and
imminent departure/arrival needs; evict unpinned low-value detail behind the
aircraft before sacrificing safety coverage.

Coarse-first delivery must use real, geographically correct source products at
a declared quality. Finer mip/range bytes retain fixed file identity and layout;
never swap provisional full-resolution textures behind open handles. The current
whole-DDS source adapter cannot fulfil this contract merely by adding a priority
queue: fast prefix/mip delivery and source-coverage qualification are prerequisites.
Arbitrary network failure cannot support an unconditional no-gap guarantee.

### Optional SimBrief route foresight

Navigraph documents retrieval of a user's most recently generated OFP as XML or
JSON using a configured Pilot ID or username:
https://developers.navigraph.com/docs/simbrief/fetching-ofp-data

Fetch/import a plan through an optional source adapter, validate its actual
schema and geographic route data, and normalize it into route legs outside the
filesystem path. Verify that it is relevant to the active departure/flight;
"latest" alone is not proof that it is current. Show rejected, stale or unavailable
plan status explicitly without disabling ordinary position/demand-driven flight.

A relevant plan provides early departure, route and destination context before
takeoff. It does not authorize downloading the entire route at full detail.
Maintain a rolling, budgeted corridor and use sparse coarse destination
coverage only when higher-priority local/departure needs are satisfied.
Live movement and actual demand override the planned route during taxi,
vectors, holds or diversion. Re-import/re-fetch on an explicit flight change or
bounded freshness policy, not continuous API polling.

Do not silently serve a black texture, return an incomplete DDS, or call
coarse imagery high-resolution. Do not promise that X-Plane will notice a later
replacement and upgrade a texture already loaded into VRAM.
Do not publish a sparse native DDS whose absent ranges read as zeros. A streamed
file has a fixed valid header, full logical mip layout and deterministic data
identity; missing requested ranges must be resolved or fail explicitly. Byte
identity/source-version policy must be qualified before mixing mip sources.

The objective is negligible **GroundEffect-induced** loading delay and frame
stalls. X-Plane's mesh processing, GPU uploads, other scenery/plugins, operating
system contention, and storage faults remain outside a filesystem's control.

## 3. System boundaries

```text
 AUTOMATIC WORKERS / CONTROL                   FLIGHT

 permitted imagery sources ----+
 lighting source cells --------+--> bounded fetch / build workers
 VIIRS + quality metadata ------+               |
 existing DSF / terrain mesh ---+               v
                                    verified source products
                                    + immutable encoded mip/ranges
                                               |
                                     validate / commit / cache
                                               |
                                     protected cached working set
                                               |
                                     read-only filesystem ------> X-Plane
                                               |
                                      demand / miss / look-ahead
                                               |
                                      bounded priority scheduler

 Go/Charm TUI + CLI ----------> Rust user control service
                                no image data transported through UI IPC

 COMPLETE EXPORT / BENCHMARK: immutable native files / local images -> X-Plane
```

Keep three execution responsibilities separate, without inventing a fleet of
services. A Rust engine provides the control service; bounded preparation workers
are isolated processes where useful. A small Go terminal client provides the TUI
and scriptable commands. The dynamic reader is a separate, small failure
domain. See the [terminal interface proposal](terminal-ui.md).

| Responsibility | Permitted work | Must not depend on |
|---|---|---|
| Fetch/build | Download, reproject, build requested encoded data, verify; prefetch before demand where possible | Terminal lifetime; unbounded tasks or regional queues |
| Control | Catalog, progress, budgets, source policy, activation, readiness | Terminal client lifetime |
| Flight delivery | Immutable cached reads; coalesce misses and await bounded worker completion | UI lifetime or source processing inside callbacks; cached hits must not depend on fetch/build |

Cold reads can depend on worker/network latency. Separating the callback from
source work does not eliminate that dependency. For a complete native/local-image
export only, delivery belongs to Linux and needs no GroundEffect read daemon.

## 4. Technology selection

### Rust application core

Rust offers native compilation, ownership-based memory management, and strong
types for coordinates, asset identity, and state transitions [R20]. This is a
safety/maintainability recommendation, not evidence that equivalent Rust code
beats equivalent C++.

C++ is a credible alternative and remains appropriate for existing libraries.
Do not rewrite proven C/C++ geospatial or codec implementations simply to make
the application all-Rust. Python remains useful for experiments, not as the
default per-feature or per-read execution layer. Go is proposed only for the
terminal client so it can use the actual Charm ecosystem; it does not duplicate
the Rust control service or run scenery delivery. Its garbage collection is
isolated from the engine process, although its CPU/RAM use still needs budgeting.

Use a bounded CPU pool for compilation. Restrict Tokio/HTTP async work to
network/control operations; async does not make CPU rasterization cheaper
[R21]. Acquire memory/job permits **before** queuing large inputs, not after
spawning thousands of blocked tasks.

Use stable Rust, Cargo lockfiles, pinned Go modules for the terminal client, and a
pinned Nix development/package definition when implementation begins. Enable
release optimization and benchmark with debug symbols retained separately. SIMD
should use tested runtime dispatch; do not assume every Linux user has the build
machine's CPU instruction set.

### Reuse mature components

| Work | Initial choice / evaluation |
|---|---|
| OSM decoding and topology | Osmium/libosmium, with bounded or disk-backed node-location storage [R16] |
| Coordinate transformations | PROJ; simple Web Mercator math only behind independently checked tests [R17] |
| VIIRS/raster preparation | GDAL and tiled/overview raster products, potentially COG [R18] |
| Lighting accumulation | Native batched rasterization; typed, compact feature arrays |
| Image decode | Existing native JPEG/PNG libraries, outside flight delivery |
| DDS encoding | Existing BC1/BC3 implementation; evaluate a narrow Compressonator core integration [R27] |
| Progress/catalog | SQLite transactions; source spatial indexing may use its R-tree [R19] |
| Terminal UI | Go with Bubble Tea, Lip Gloss, and Bubbles; Ratatui/Crossterm is the all-Rust alternative [R33-R37] |

Do not create a new geospatial archive format or filesystem protocol in the
first milestone. If a custom pack becomes necessary, specify its minimal
versioned format only after native-file and kernel-image measurements.

## 5. Scenery integration and correctness

Reuse compatible DSF mesh input initially. Building a new global mesh generator
would add a separate, substantial problem without improving texture delivery.
Preserve mesh coordinates, normals, surfaces, water/border behavior, exclusions,
and terrain-resource mappings. Import only reachable resources, not every file
in an installed country directory.

Compile explicit references in each terrain definition [R01]:

```text
BASE_TEX_NOWRAP ../imagery/<asset>.dds
LIT_TEX_NOWRAP ../night/<asset>.dds
```

The paths illustrate directories in the **same scenery package**. They are not a
claim that arbitrary cross-package relative references work.

`LIT_TEX` must be explicit; terrain companions are not discovered by suffix.
`LOAD_CENTER` is a `.ter` directive, affects base/night/composite textures, and
can trigger distance-dependent resolution reloads. Preserve its placement and
meaning. The documented sharing constraint is one art-resource reference per
texture per DSF tile; storage deduplication must not accidentally merge logical
resources with incompatible loading centers [R01].

### Geographic invariant

For the supported Ortho4XP naming convention, a texture footprint spans 16 by
16 web tiles at its filename zoom. Source sampling resolution and output DDS
size must not alter those bounds. The upstream orthogrid and texture-coordinate
code independently corroborates that layout [R03].

Use explicit types for source web tiles, terrain footprints, pixels, and output
dimensions. Centralize row/column and XYZ/TMS conversion. Test latitude scale,
axis order, texture orientation, boundaries, and antimeridian wrapping.

Do not extrapolate Web Mercator through the poles. Unsupported projection or
data coverage must be declared; leave unsupported DSF areas inactive rather than
stretching the last valid source tile across them.

### DDS contract

Every published file has a validated header, fixed byte length, complete mip
chain, correct block offsets, and immutable contents. Random reads, tail-first
reads, short reads, EOF, concurrent opens, and reopened handles must agree.

DXT1/BC1 is the conservative opaque-emission baseline supported by Laminar's
documentation. DXT5/BC3 is useful when alpha is needed; its additional alpha
precision does not itself solve dark RGB quantization. BC7 support has not been
established by the sources reviewed here. Uncompressed DDS is not a safe
scenery fallback according to DDSTool's manual [R02].

**Mandatory simulator gate:** verify different base/LIT dimensions and their
interaction with `LOAD_CENTER`. A smaller LIT texture could save substantial
storage and VRAM, but a shared loading directive means its behavior cannot be
assumed. Start with a known-correct matched-dimension fixture, then measure the
smaller-LIT candidate before selecting production dimensions.

## 6. Lighting pipeline

### Separate evidence from inference

OSM supplies mapped geometry and some explicit lighting attributes. It does not
contain a complete inventory of lamps, power, color, or operating schedules.
Missing `lit` is unknown, not `lit=no`. Explicit no/disused/intermittent values
need distinct treatment; mapped street lamps are stronger local evidence than a
generic road class [R14].

VIIRS annual products provide coarse upward-radiance observations on a
15-arcsecond grid, with product-specific coverage and quality layers [R15].
That grid is not a surveyed lamp map or a direct ground-illuminance measurement.
An isolated source may contribute to a pixel without revealing its exact
location. Do not infer individual fixtures or their colors from that pixel.

Use VIIRS as a confidence-aware coarse brightness prior, not a literal multiplier
that declares missing observations unlit. Do not claim global polar coverage for
products whose documented extent is only 75 N to 65 S.

### Compile a shared geographic source pyramid

Production normal use must not require users to import a country PBF or query a
nationwide geometry database per texture. Prefer compact, spatially indexed
lighting products retrieved incrementally. Their availability, provenance,
distribution rights and required detail are source-adapter gates, not existing
infrastructure to assume. The alpha's borrowed OSM database is a reference input,
not the desired deployment dependency.

Stream OSM once into spatially grouped, compact source records. Resolve topology
and geometry before rasterization. A spatial database is acceptable during
preparation, but no geometry query belongs in a texture read.

Render canonical geographic cells with halos wide enough for the lighting
kernel and resampling filter. Clip after accumulation. Choose cell size from
measured build memory, seams, and source reuse, not from an arbitrary DDS size.

Accumulate emission in linear-light floating-point buffers, with widths/support
expressed in meters and fractional pixel coverage. Construct area-aware mips
before final quantization. Independently validate the conversion to X-Plane's
accepted texture representation rather than assuming a file's numeric values
are physically calibrated radiance.

The canonical source can retain separate road, mapped-lamp, building/landuse,
and coarse-observation contributions if this materially improves tuning.
Do not multiply channels without a measured storage or usability benefit.
Building footprints alone do not prove glowing rooftops or window positions.

Keep geometry/source products independent of imagery provider and user exposure.
Final **compiled DDS identity**, however, includes every value that changes
bytes: source versions, footprint, projection, renderer version, VIIRS product
and quality policy, exposure/color preset, dimensions, format, and mip filter.

A brightness change may reuse the source pyramid, but still requires recompiling
affected DDS assets. Show that cost and activate the completed preset coherently.
There is no promised instantaneous global slider with zero preprocessing.

### Near, distant, and airport appearance

Terrain emission complements, rather than replaces, native 3D lights. X-Plane
controls filtering, projected size, atmosphere, and texture loading. A static
texture cannot implement a reliable camera-relative fade; do not bake a fake
distance gradient into each terrain square [R01].

Avoid zoom-dependent meter widths and discontinuous mips. Assess cruise, descent,
and approach with native roads/autogen present, including potential double
lighting. Do not disable native lights to conceal incorrect texture energy.

Airport-provided imagery, draped polygons, and other higher-priority resources
may cover the underlying terrain. Diagnose those layers separately; a dark
rectangle near an aircraft is not automatically a missing GroundEffect texture.

## 7. Storage and delivery selection

### A. Ordinary files: baseline and complete-export backend

Prepared DDSs on ext4 require no custom read daemon or additional mount helper.
The kernel handles the same random reads X-Plane normally makes. A terminal
client or control-process crash cannot disrupt those file reads.

Costs include inode/directory count, duplicate content, raw DDS capacity, and
per-generation update space. These are reasons to measure packaging, not reasons
to discard the baseline without testing.

### B. EROFS: leading Linux-specific packaging candidate

Package immutable prepared scenery into spatially sensible image shards.
EROFS supports a kernel-native read-only tree, optional compression, compact
metadata, and deduplication features [R04, R05]. Its source documentation
explicitly warns that large compressed clusters can hurt random-read latency.

Benchmark LZ4 with small clusters first, against uncompressed images and ordinary
files. Night DDSs and repetitive terrain metadata may compress well; already
compressed imagery might not. These are workload hypotheses, not measured ratios.
Measure metadata compression and deduplication separately with the pinned kernel
and image-builder versions.

Size and number of shards must balance rebuilding, shared-border duplication,
mount count, lookup cost, and update space. Do not choose one mount per web tile,
one enormous mutable world image, or one million loose source records by habit.

Kernel image mounting normally needs privileged setup. Prefer a narrowly scoped,
auditable mount helper or administrator-installed mount configuration; never run
downloaders/parsers/the terminal client with `CAP_SYS_ADMIN`. Native-file mode remains usable
without that setup. Mount visibility to the actual desktop X-Plane process is an
integration requirement, not something a private service namespace guarantees.

### C. SquashFS: credible competitor

SquashFS offers mature read-only compression, metadata packing, and
deduplication. Its compression block size and decompressor concurrency affect
random-read performance [R06]. Compare equivalent algorithms and settings;
comparing default gzip/128 KiB against EROFS LZ4/small clusters is not a clean
filesystem comparison.

Select it if the actual latency/storage tradeoff is better. Kernel delivery is a
design direction, not loyalty to one filesystem.

### D. Thin FUSE reader: automatic-loader prototype

Automatic operation supplies the distribution requirement for evaluating this
reader; it does not prove which implementation is fastest. Logical files expose
fixed DDS layouts. Committed local chunks contain encoded bytes. Missing ranges
are coalesced into bounded worker jobs; callbacks do not create pixels or encode
BC blocks themselves, but cold completion may depend on such work.

Metadata operations use a sealed index. A read translates file offset to local
chunk ranges, reads bounded frames, decompresses only necessary frames, and
replies. Test independently decodable 16/64/128 KiB frames; choose from traces.
Avoid a whole-11-MiB decompression for a small mip-tail read.
Trace whether X-Plane actually reads mip tails selectively before claiming that
mip-granular fetching saves transfer, CPU or storage. A fixed 4096 BC1 file has
11,184,952 logical bytes; its 1024-and-smaller payload is about 683 KiB only if
those are the requested levels. Do not fabricate lower levels from unrelated
imagery or change a footprint based on requested range size.

Use cached FUSE by default: direct I/O bypasses page cache and disables readahead
[R09]. Use stable inode identities within a generation and long attribute
lifetimes only because contents are immutable. Avoid unbounded backing-FD and
decompressed-buffer caches.

`fuser` is a candidate native Rust low-level implementation; libfuse3 is the
comparison implementation [R12, R13]. Audit the selected release's cancellation,
threading, protocol, and kernel-feature support. Experimental async support does
not imply FUSE-over-io_uring support.

A FUSE reader crash breaks its connection; restarting a process does not
transparently repair X-Plane's existing handles. This is a real reliability cost
relative to ordinary local files or a local kernel image.

### E. Nydus / EROFS-on-demand: later distribution candidate

Nydus RAFS v6 can use EROFS with on-demand blob delivery through cachefiles
[R07, R08]. It is worth evaluating before inventing a remote chunk-distribution
stack, but is not the first required dependency.

It needs prebuilt compatible data, appropriate kernel configuration, daemon and
cache ownership, and an operational mounting model. It cannot convert provider
JPEGs into DDS inside a kernel cache. A remote cache miss still incurs network
latency.

The older Nydus design document explicitly describes RAFS v5 details; do not
transfer its chunk size or eviction assumptions to v6 uncritically. CacheFiles'
documented device ownership and cache-management rules also make it more than a
private directory one application can modify arbitrarily [R08, R28].

### Advanced I/O is not the first architecture decision

Use `pread` for explicit-offset data reads and optionally mmap a modest,
validated immutable index [R23, R24]. mmap can page-fault; it does not make
uncached storage access free.

Evaluate ordinary io_uring only if queueing/syscall overhead is measurable.
Busy polling consumes CPU that X-Plane may need [R25]. FUSE-over-io_uring is a
different interface, documented as still evolving in Linux 6.18 and requiring
handling of some requests through `/dev/fuse` [R10].

FUSE passthrough requires `CAP_SYS_ADMIN` in the reviewed kernel documentation
and registers real backing files [R11]. It is not a general solution for reading
arbitrarily compressed subranges of a shared pack. No broad privilege grant is
justified by an unmeasured zero-copy claim.

## 8. Capacity and resource budgets

A prepared source atlas and a prepared simulator representation serve different
purposes. A tiny source atlas does not imply tiny final scenery. If compiling DDS
still happens on first open, the central architectural mistake remains.

For square BC1 DDSs with a 128-byte header and a full chain through 1x1:

| Dimension | Bytes per file | 100,000 unique files, before filesystem overhead |
|---|---:|---:|
| 1024 | 699,192 | About 65 GiB |
| 4096 | 11,184,952 | About 1.02 TiB |

These are format calculations, not a prediction of regional coverage or
compression. Count actual unique reachable footprints and measure representative
urban, suburban, rural, water, and empty data before projecting a country.
Account separately for daytime textures, source data, source pyramids, temporary
files, two live generations, verification data, and kernel/VRAM working sets.

Use byte budgets, not only item counts. Initial tuning hypotheses:

| Resource | Initial policy, subject to measurement |
|---|---|
| Optional reader heap/buffers | Start with a 256 MiB budget; expose overhead/index size separately |
| Reader work | Fixed bounded queue/pool; no thread or task per file |
| Compilation memory | Reserve per-job scratch and decoded-input bytes before dispatch |
| CPU during flight | Bound demand/look-ahead workers; prioritize required reads; pause optional bulk exports |
| Network | Per-provider concurrency/rate limits, bounded retries, resumable permitted downloads |
| Disk | Explicit user budget plus temporary/update reservations; never evict active-generation dependencies |
| File descriptors | Bounded shard handle cache, leaving headroom for simulator/system use |
| Telemetry/logging | Aggregate counters and bounded histograms; no synchronous per-read log write |

The proposed initial managed-disk cap is 128 GiB, configurable and not yet
implemented. It includes cached assets, owned source products, staging and
retained versions; borrowed external inputs must be reported separately. Track
unique resident bytes rather than a virtual DDS's apparent size. Deduplicate
unchanged data and reclaim unpinned obsolete/cold data under an explicit policy.
Never discard data needed by an open/in-flight request or complete installed
offline export. Refuse work or declare reduced coverage if protected data and
reservations cannot fit. Compression savings are not assumed in that budget.

Keep runtime pages on the fast local device when possible and archives on slower
storage; do not imply an HDD can meet NVMe latency. Measure page-cache usage,
compressed backing pages, transient decompression buffers, and simulator VRAM
rather than claiming the application's RSS is the whole memory footprint.

Use systemd/cgroup resource controls where available, with permissions and
controller availability checked [R26]. MemoryHigh can throttle; MemoryMax can
kill. Do not rely on killing a flight-critical FUSE reader as normal budgeting.

### Implemented bounded shared-imagery acquisition

Native discovery now establishes read-only DDS mapping and payload-bearing
initial FUSE faults; it does not establish selective CPU mip consumption.
The source slice now shares one exact EPSG:3857 export between a deterministic
2-by-2 group of existing footprints. Double both the geographic extent and PNG
dimensions so pixel spacing, orientation and each child's footprint remain
unchanged. Near the declared source-envelope boundary, use a deterministic
single-footprint request at the same quality instead of extending the envelope.

The Rust provider owns a four-entry, 64-MiB RGBA source pool, with at most the
two existing workers acquiring sources. Same-parent requests coalesce; unrelated
acquisitions do not hold the pool mutex during HTTP or decoding. Immutable Arc
leases keep evicted source buffers alive until their worker releases them.
In-use leases and acquisitions remain within existing worker reservations;
reserve the pool's full capacity in the same global budget before starting.
No source files, extra workers, regional jobs or new client policy are added.

Check PNG shape, projection, extent and tagged RGBA8 before pooling. Check actual
alpha coverage on each requested child's exact crop; uncovered pixels outside
that crop must neither invalidate covered imagery nor be synthesized. Source
errors are not cached as readiness and remain visible through the existing
delivery failure path. Version the day/night identity recipe before changing
source sampling; prior committed bytes and ledger hashes remain immutable.

Four-quadrant/orientation, coverage, resolution-key separation, concurrent
coalescing, eviction leases and error recovery are exercised. Four actual
adjacent complete DDS builds used one parent acquisition; repeating them
preserved their bytes without additional HTTP. See the measured source slice
in `automatic-prototype.md`. Wider coverage and the airport latency target are
not thereby qualified.

## 9. Readiness, publication, and recovery

Use a small transactional control catalog and explicit states:

```text
absent -> fetching -> imported -> compiling -> validating -> ready -> active
                        \-------------> failed <---------------/
```

This is the complete-export workflow, not a claim that mounted streaming data
is wholly ready. Streamed data units need explicit absent/fetching/building/
committed/pinned states and residency accounting.
The arrows illustrate workflow, not permissions to promote failed work.
Record completed cells/assets for resumption. A source change invalidates the
appropriate dependency graph; it does not require deleting all source databases.

Complete offline readiness requires the dependency closure of each **active DSF**,
not just aircraft coordinates: all terrain definitions, base/night textures, masks, and
other required package-local resources are complete. A route or selected region
chooses complete DSF coverage units. Areas outside installed GroundEffect base
mesh coverage can use X-Plane's normal scenery selection, but priority and
boundary behavior must be validated.

For streaming, validate the full metadata/resource mapping and deterministic
asset recipes before exposure. Network-backed logical assets are not complete
offline exports. Validate/commit each requested data unit atomically, track
residency and pinning, and surface failed downloads/encoding explicitly.
Do not disguise absent data as black/provisional DDSs. Qualify cold failure and
offline startup before selecting a fallback policy; a missing texture does not
automatically make X-Plane select a different base mesh.

Write a generation in staging; validate all indices, sizes, mip offsets, and
content hashes; flush data and relevant directories; atomically publish on the
same filesystem; only then commit its ready state [R29]. Record sufficient state
to reconcile a crash between filesystem publication and catalog commit.

Verify full contents when building/installing. Startup uses sealed manifests and
bounded checks, not a country-wide hash pass or directory scan. Explicit deep
verification remains available separately. Unexpected later corruption is an
error, not a reason to synthesize plausible-looking replacement bytes.

Pin a generation for the simulator session. Default activation is before launch
or an explicit scenery reload, not live replacement of individual DDSs.
Open handles and active mounts retain their generation until release. Garbage
collection respects session pins and reserves update capacity before starting.
For a streaming session, pin source/recipe identity, open/in-flight data and the
protected working set. Demand may add committed ranges, not replace previously
served bytes. Complete offline exports retain their whole dependency closure.

If mount/setup/readiness fails, leave the prior complete generation active and
report the actual failure. If none exists, refuse activation. Never advertise
success because a directory happens to exist.

## 10. Control plane and Linux integration

The implemented native CLI is documented in [the alpha contract](alpha.md).
The [automatic prototype](automatic-prototype.md) implements read-only mounting,
demand-only whole-DDS workers, bounded cache/identity storage, in-use protection,
desired autostart and shared CLI/TUI controls. Mip-granular source work, look-ahead
and production source distribution remain proposed.
Use XDG paths and a permission-restricted local Unix socket for the user service.

The interface is terminal-only: a full-screen TUI for interactive use and normal
commands for shell automation. No GTK, browser, or Electron frontend is planned.
The implemented Go/Charm client uses typed/versioned control messages over the local
Unix socket; rendering, filtering, and navigation stay in the client, while
readiness and all mutations remain authoritative in the Rust engine.

Normal operation is start/reconnect to GroundEffect's service and start X-Plane.
Explicit prepare/export commands remain diagnostics or offline options, not
mandatory flight steps. Display mount/service health, actual simulator I/O
activity, cache residency/hits, fetching/building work and failures distinctly.
Do not substitute process detection for evidence that X-Plane is reading the
service, or show a fake live simulator connection.

The TUI displays readiness, quality, storage estimates, preparation ETA, and
failures. Closing it must not cancel engine jobs, unmount active scenery, or stop
the reader. Noninteractive commands support explicit machine-readable output
without terminal control sequences. The [terminal interface proposal](terminal-ui.md)
defines appearance, capabilities, and failure handling.

A readiness test must probe actual resource reads, not merely an HTTP health
endpoint or live PID.

Optional position/flight-plan telemetry may guide preparation or page-cache
prefetch later. It is not required for correct ready coverage. If a simulator
plugin is introduced, it must do no disk/network work on simulator callbacks;
its exact supported SDK integration needs separate validation.

No RT scheduling, global VM tuning, busy-spin loops, or automatic system-wide
cache eviction. Do not automatically stop unrelated cachefiles services.

## 11. Data rights and reproducibility

Use only imagery sources whose terms permit the intended downloading, caching,
transformation, and any distribution. Ordinary access to a web tile endpoint
is not permission to bulk download or redistribute it.

Retain source identifiers, dates/versions, licenses, and required attribution in
manifests. OSM is ODbL; distinguish source/derived databases and rendered products
before distributing them, rather than assuming everything has one license
[R30]. Review the exact VIIRS product terms separately.

Choose the project's code license explicitly before release. Do not copy
AutoOrtho implementation code into an otherwise unlicensed new repository as a
shortcut. The limited real alpha import is documented in the alpha contract;
it is not a global migration or permission to redistribute source scenery.

## 12. Implementation sequence and stop gates

| Gate | Deliverable | Proceed only when |
|---|---|---|
| G0: fixtures | Small dense-city, rural, boundary, and airport data sets; repeatable simulator scene | Geographic/mesh/resource mappings independently understood |
| G1: native truth | One complete static package with validated daytime and night DDSs | Correct appearance, mip loading, `LOAD_CENTER`, and dimension behavior in X-Plane |
| G2: trace and capacity | Simulator/application I/O traces, native baseline, measured per-area storage | Hot-path workload and preparation/storage costs are known |
| G3: automatic loader | Demand-driven read-only prototype, fixed DDS layout, cached/cold mip-range replay, native-file comparison | Actual simulator reads establish granularity; cold latency and daemon failure are explicit; no manual regional preparation |
| G4: bounded working set | Coalesced priority scheduling, look-ahead, shared source products, global disk quota and safe eviction | No OOM, unlimited work, removed pinned data or hidden regional batch; source availability/rights qualified |
| G5: lifecycle | Readiness, activation, crash recovery, garbage collection, installation | Fault matrix passes and active flights survive non-reader control/client failure |
| G6: usability | Terminal-only Charm TUI and scriptable CLI, profiles, explainable errors/ETA | Client disconnects and rendering add no dependency to flight delivery |
| G7: production expansion | Broader source/mesh support and measured local/remote packaging choices | Automatic cache-miss behavior remains explicit; quality, disk and failure contracts survive real flights |

No country-wide import should be a prerequisite for discovering a broken
projection or unreadable DDS. No full interface should precede proof of the simulator-facing
path. No backend earns the label fastest until the actual replay and simulator
measurements support it.
