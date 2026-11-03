# GroundEffect benchmark and acceptance plan

Status: proposed experiments, not executed GroundEffect results.

The purpose is to choose a design based on the actual simulator workload and to
prevent a fast microbenchmark from hiding slow startup, preparation, or frame
stalls. Source IDs refer to [research](research.md#source-ledger).

## 1. Establish a correct native-file reference

Create a small, fully prepared scenery package using existing compatible mesh.
Include a dense city, suburban/rural boundary, water, and an airport whose own
ground imagery can overlap terrain. Independently inspect terrain bounds,
orientation, pixel landmarks, DDS headers, block counts, and decoded mip levels.

Use the same daytime/night assets and simulator settings for every delivery
backend. Do not compare a 4096-square candidate with a 1024-square reference and
attribute all the difference to filesystem or language.

Test matched base/LIT sizes first, then smaller LIT dimensions. Check
`LOAD_CENTER` resolution reloads in both cases. This is a functional gate before
storage projections or a country import.

## 2. Capture two views of the workload

Capture simulator-issued file operations with an application/syscall-level
tracer, subject to ptrace/trace permissions. A short filtered strace capture is
useful for discovery, not final latency evidence: tracing itself changes timing.
An eBPF/perf approach may reduce disturbance where supported.

Record path/file identity, open flags, offset, requested/returned length, error,
thread identity, and monotonic start/end time. Include open/stat/access/readdir,
ordinary reads, pread/readv, and any file-backed mmap/page-fault behavior.

Separately capture backend/FUSE requests and block-device I/O. Kernel readahead
can transform one application read into different backend reads; do not label a
FUSE request log a faithful simulator API trace.

Use tracing to produce a replay specification, then measure final runs without
verbose per-operation tracing. Store raw histograms/counters without synchronous
per-read log writes. Record capture overhead explicitly.

## 3. Workloads

| Workload | Why it matters |
|---|---|
| Cold process, prepared local files | Common restart; distinguishes startup scanning from source preparation |
| Cache-cold local files | Real device and decompression latency |
| Warm repeated reads | Protocol, lookup, and copying overhead |
| Prefix plus mip-tail reads | Avoids full-image work for low-resolution loading |
| Full/random DDS reads | Confirms general correctness, not a special-case trace optimization |
| Dense-city night departure | High source complexity and concurrent resource loads |
| Cruise and high-speed boundary crossing | Sustained asset loading and far-field mip behavior |
| Long descent to approach | Increasing resolution demand and lighting transitions |
| Rural, ocean, empty-light cells | Compression/dedup savings and non-urban correctness |
| Large installed resource index | Startup/metadata scaling without reading every source file |
| Memory and device contention | Tail latency and page-cache reclamation under plausible flight load |
| Alternate starting location / teleport | Explicit limits of route prefetch; not silently declared no-stall |

Maintain both an offset-correct synthetic reader and a faithful trace replay.
A replay should preserve independent-thread scheduling and causal dependencies,
not serialize everything or blindly use an unrealistically huge queue.

For mmap workloads, ordinary read replay is not a substitute for testing actual
page-fault behavior.

## 4. Define coldness precisely

| Label | Meaning |
|---|---|
| Process-cold | Application restarted; disk assets and kernel cache may be warm |
| Data-page-cold | Relevant file data pages not cached; metadata state reported separately |
| Metadata-cold | Relevant inode/dentry/index pages cold too |
| Device-cold | Storage/controller state controlled or explicitly unknown |
| Asset-missing | Network/preparation required; outside ready-mode delivery |

Do not call a process restart a cold-storage test.

Prefer an isolated benchmark environment or dedicated test data/device.
Per-file `POSIX_FADV_DONTNEED` is advisory and does not prove eviction [R31].
Verify cache-cold claims using faults and physical I/O counters where possible.
Do not change global `drop_caches`, VM tuning, or other users' working sets on
the shared development desktop [R32].

## 5. Candidate matrix

Use one immutable corpus with identical logical bytes, then compare:

| Candidate | Variables |
|---|---|
| ext4 ordinary files | Physical layout; warm/cold; directory/resource count |
| EROFS | Uncompressed vs LZ4; small/larger clusters; metadata compression and dedup separately |
| SquashFS | Matched LZ4; block size; supported decompressor threading |
| Cached FUSE over local pack, conditional | Uncompressed / LZ4 / Zstd; independent 16/64/128 KiB frames; bounded worker/FD/cache budgets |
| FUSE direct I/O, diagnostic | Tests whether bypassing page cache helps this trace rather than assuming it |
| `pread` vs mmap vs ordinary io_uring, conditional | Same local data and queue depth; page faults, CPU, and tail latency |
| FUSE passthrough / FUSE-over-io_uring, optional | Supported pinned kernel/library configuration and explicit privilege model |
| Nydus RAFS v6, later | Fully local blobs first; controlled remote cache misses separately |

Do not test every cross-product immediately. First eliminate clearly dominated
backends on a representative corpus, then tune the surviving candidates.
Report features disabled because the kernel or userspace release lacks support.

Run on NVMe and HDD separately. Include mount/setup and image-building costs,
not just the read service once everything is attached and warm.

## 6. Measurements

Report p50/p95/p99/p99.9 and maximum latency, with sample counts, for end-to-end
lookup/open/read and for backend operations separately. Include error counts and
queueing time. An excellent median is insufficient.

Record wall time, process CPU, system CPU, context switches, major/minor faults,
bytes requested, physical bytes read, decompressed bytes, encoded bytes,
read amplification, page-cache working set, reader heap, peak worker memory,
FD count, task count, and active queue depth.

Measure startup as distinct phases: control readiness, filesystem availability,
simulator scenery load, and time to a usable scene. Report preparation/download
time independently, and also the **first-ever location** total. Do not make
startup appear instant by hiding an unbounded preflight compile.

Report image/source/temporary sizes and actual allocated disk blocks, inode
count, rebuild bytes after a small edit, update peak space, and mount count.
Distinguish application RSS from kernel cache and simulator VRAM.

For in-simulator runs record CPU/GPU frame time, p99/p99.9 and worst frames,
threshold exceedances, location/flight phase, and texture-load activity.
Compare against the native-file reference and ordinary default scenery. Capture
settings, weather, aircraft, plugins, and GPU state so the comparison is useful.

## 7. Provisional engineering targets

These are **initial targets for evaluation**, not performance claims. The
reference hardware, corpus, and queue depth must accompany every result.

| Metric | Initial gate |
|---|---|
| Ready-mode network/source/raster/encode operations caused by a read | Exactly zero |
| Startup work | No source DB opening, country directory scan, full-content hash pass, or full-image rebuild |
| Control + active-resource readiness | Aim for <= 2 s with prepared local data; report mount cost separately |
| Warm logical reads up to 128 KiB | Aim for p99.9 <= 2 ms |
| Cache-cold logical reads up to 128 KiB, NVMe | Aim for p99.9 <= 10 ms at the recorded replay concurrency |
| Cache-cold HDD behavior | Separate results and SLO; no NVMe guarantee copied to HDD |
| Backend selection | No material simulator startup/frame-time regression against native files without an explicit accepted tradeoff |
| Reader memory | Bounded by configured bytes plus measured index/fixed overhead, independent of flight duration |
| Corruption or missing dependency | Detected before activation where possible; otherwise explicit failure, never invented success bytes |
| Geometry/mip behavior | No tile swaps, flips, boundary seams, or zoom-dependent geographic footprint changes |

Revisit numerical targets after the native baseline. If the device/reference
cannot meet them, explain the constraint rather than quietly relabeling results.
Define an allowed frame-time/startup regression before choosing a storage-saving
backend; do not pick the tolerance after seeing the winner.

## 8. Preparation performance is part of the product

Measure source bytes/second, features/second, geographic area/second at specified
quality, DDSs/second, compression time, and end-to-end time to ready coverage.
Report geometry import, source-pyramid generation, shading/mips/encoding,
packaging, and verification separately.

Use dense, sparse, and empty distributions. Include peak RAM and swap, temporary
disk use, scratch I/O, abort/resume overhead, and incomplete-download cleanup.
Test the disk-backed node index on an extract large enough to expose memory
growth; a tiny fixture cannot establish country-scale safety.

Measure exposure/color-preset recompilation without reimporting geometry.
Avoid extrapolating one dense-city cell across an entire country without
stratified source counts and measured compression distributions.

## 9. Fault and lifecycle matrix

| Fault | Required behavior |
|---|---|
| Interrupted source download / provider timeout | Bounded retry, resumable staging where supported, clear failed/not-ready state |
| Invalid provider bytes / corrupt source | Rejected before publication; retained diagnostic context |
| Full disk during compilation or sync | No ready marker; prior complete generation remains usable |
| Worker crash / OOM | Only incomplete staging affected; resumable state and visible failure |
| Control/TUI crash during native-file flight | Existing simulator reads continue |
| Terminal disconnect, client quit, or output backpressure | Engine jobs and active scenery continue; reconnect obtains an authoritative snapshot |
| Optional FUSE reader crash | Explicit broken mount/session recovery guidance; no claim that restart repairs old handles |
| Truncated pack / bad index / overflowing offset | Bounds-checked rejection; no panic or out-of-range mapping |
| Changed settings/source during compile | Result tied to immutable input identity; no mixed preset generation |
| Concurrent activation/cleanup/open | Active generation pinned; no deletion of required assets |
| Mount helper unavailable / wrong namespace | Refuse that activation or use a fully prepared native-file backend explicitly |
| New process version after update | Existing active data/protocol version remains compatible or activation is refused |
| Readiness missing one rarely used mask/mip | Dependency verification catches it; no placeholder file |

Inject failures at data flush, rename, directory flush, and catalog-commit
boundaries. Verify reconciliation and persistence, not just ordinary shutdown.

## 10. Visual acceptance

Use independently decoded final DDSs plus fixed simulator scenes. Check the same
landmark at multiple filename zooms, mip levels, altitudes, and viewing angles.
Keep time, exposure, weather, and relevant renderer settings fixed.

Include cross-cell halos, cross-DSF boundaries, coasts, high latitude, and
antimeridian fixtures. Check faint-light loss and RGB artifacts after **final
BC encoding**, not only in a bright source PNG.

Compare explicit OSM lit/no/unknown cases; flag inferred lighting. VIIRS quality
gaps must not silently become proof of darkness. Assess native road/autogen
lights together with terrain emission.

At airports, distinguish GroundEffect base-mesh emission from airport-owned
overlays. Do not diagnose scenery layering from a screenshot alone.

## 11. Reproducibility and decision record

Record corpus/source hashes and rights, simulator/build version, kernel config,
filesystem/options, image-builder version/options, CPU, memory, storage device,
compiler flags, dependency lockfiles, settings, and workload/replay hash.

Randomize backend run order. Use repeated runs and confidence intervals;
high-percentile claims need enough observations. Do not present p99.9 from a few
hundred samples. Preserve failures and worst-case traces, not just averages.

The output is a decision report identifying the best measured latency/storage/
operational tradeoff, rejected alternatives, unresolved limits, and the precise
ready-coverage contract. Until that exists, EROFS and thin FUSE remain candidates,
not proven performance winners.
