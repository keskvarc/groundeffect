# Development plan

The engineering rules in `../AGENTS.md` are binding. Each stage is a working
vertical slice, not permission to create speculative infrastructure.

## Stages

| Stage | Deliverable | Acceptance |
|---|---|---|
| 1. Contracts and fixtures | Typed scenery identifiers, geographic bounds, DDS layout, DSF resource reader | Malformed data rejected; real source DSF bindings inspected; no mesh invention |
| 2. Native compiler | Existing DSF copied byte-for-byte; terrain bindings preserved; complete local base/night DDS assets | Dependency closure, mip sizes, projection and output hashes verified |
| 3. Lighting preparation | Bounded local OSM/VIIRS adapter and native emission rasterization | Real Raleigh data; explicit source limitations; no source queries during flight |
| 4. Transactional lifecycle | Persistent jobs, staged generations, verification and explicit activation | Restart and failures do not promote partial data or replace unrelated scenery |
| 5. Control boundary | Versioned local Unix-socket operations shared by CLI/TUI | Bounded requests, explicit errors, disconnect-independent jobs |
| 6. Terminal experience | Charm dashboard, navigation and real job progress; scriptable commands | No simulated readiness; responsive compact layout; client exit leaves engine intact |
| 7. Alpha integration | Reproducible build, focused tests and a real KRDU preparation run | Actual package inspected independently; exact unverified simulator behavior documented |
| 8. Simulator qualification | Controlled daylight/night/cruise/approach and loading traces | Correct native-file reference before filesystem optimization |
| 9. Automatic loader | Demand-driven virtual scenery, fixed DDS/mip layouts, coalesced workers, look-ahead and global bounded cache | No manual regional prep; cached/cold reads measured against the native reference; failures, source identity and safe eviction explicit |
| 10. Production expansion | Incremental source products, broader coverage, resumability and measured packaging/transport choices | Automatic workflow, declared quality and storage bounds survive cold startup, real flights and outages |

Stages 1-7 define the first alpha. Stages 8-10 are not represented as complete
merely because the alpha builds.

Stages 1-7 are now implemented, including the complete real Raleigh preparation
and independent encoded-asset inspection. See the [observed alpha
run](alpha.md#observed-raleigh-alpha-run) for evidence and limits. Stage 8 remains
partially qualified, not a completed visual/performance benchmark. Stage 9 now
has a real whole-DDS automatic prototype, bounded cache and shared controls;
individual mip generation, look-ahead and representative flight performance
remain open. See [the prototype contract](automatic-prototype.md).

## Automatic-flight critical path

Execution revision, 2026-10-31: the original Stage 9 is too broad to manage the
remaining delivery. A working filesystem, resolved defect or completed
infrastructure task is not the product milestone. The first product milestone
is an interactive airport loaded automatically, without regional preparation.
The automatic simulator performance gate is still failed.

The flight-phase and optional-route design exists in
[the design specification](design.md#flight-aware-working-set), but the critical
renderer/source contract is not yet fully qualified. Do not describe the
proposal as a finished implementation specification.

| Milestone | Status | Deliverable and exit gate |
|---|---|---|
| 1. Lock the renderer/source contract | In progress | Native discovery records 16 complete-length read-only mappings across 12 DDSs, and saved FUSE telemetry proves distant `LOAD_CENTER` DDS faults read `[0, 131072]` plus `[630784, 699192]` (`mip 2..10`). Implemented pre-rendered USGS Web Mercator tile-pyramid acquisition (`tile-group2-v1`, `0.708s` for 4 adjacent DDSs), active-quadrant 2-by-2 source sharing, non-evicting sibling `Day` completion, coastline-mask-verified offshore crops (`coastal-tile-group2-v1`), and bounded transient retry (`1.04s` across all 4 previously failing coastal/inland parent groups). Paired first-interactive simulator timing remains unqualified. Candidate acceptance: at most 30 seconds added Resume-to-interaction latency over the paired native run. See `automatic-prototype.md`. |
| 2. First usable automatic airport | Not delivered | One airport becomes interactive with correct nearby day/night scenery and required distant coverage, within the recorded budget, without regional preparation. Use the existing provider/cache pipeline; remove unnecessary source work rather than add another preparation system. Resolve unsupported coverage explicitly, not with placeholder pixels. This is the first user-visible product gate. |
| 3. Flight-aware rolling coverage | Not implemented | Add verified live position/ground-state/altitude/velocity input and use it in the same bounded scheduler. Parked/taxi time prepares departure coverage; motion and measured costs set a rolling horizon. Actual reads retain reserved capacity and priority. Exercise turns, stale telemetry, cancellation, coalescing and quota protection; a route is not required. |
| 4. Flyable automatic alpha | Not qualified | Cold airport startup, at least five minutes parked, taxi, climb, cruise turns and descent/arrival work in a representative flight; repeat warm. Record time to interaction, delivery waits, demand backlog, coverage and owned bytes against the declared budgets. Visual inspection verifies alignment and missing patches; valid file delivery alone cannot pass. Cached working-set behavior during source interruption is qualified without promising no gaps on arbitrary networks. |
| 5. Optional SimBrief foresight | Not implemented | Validate/import a relevant published plan, then feed its departure/route/destination hints into the existing rolling scheduler. Demonstrate stale/unrelated plan rejection and live-motion override. No plan or unavailable SimBrief must leave the ordinary automatic workflow usable. |

Milestone 2 depends on 1; 3 needs the usable delivery contract from 2 and verified
live input; 4 depends on 3. SimBrief does not gate the first automatic flight and
must not postpone it. Telemetry qualification can proceed independently of
source qualification, but it is not a substitute for the airport gate.

The immediate decision is not another unrestricted cold simulator retry:
measure the unresolved DDS contract with verified data. If X-Plane needs whole
files, optimize shared source acquisition and complete-asset prediction rather
than keep pursuing an ineffective header/mip shortcut. If the chosen source or
delivery model cannot meet the airport gate, revise that boundary explicitly
before expanding coverage or adding UI/transport features.

## Revised product workflow

The 2026-10-31 requirement is automatic operation without manually selected
regions or long regional preparation. The native alpha is the reference fixture,
not the final normal-use workflow. Do not expand it into many regional batches
and label the scheduler automatic.

The implemented runtime observes real file/mip intersections, supplies immutable
cached bytes and resolves/coalesces cold work through two bounded workers.
The next slice must select finer granularity and look-ahead from actual X-Plane
demand rather than assume that whole-DDS cold generation is fast enough.
It must report actual cold
latency and failure behavior, not claim that network/source work disappears.
Start with the existing Raleigh scene and neighbouring demand, not a country
import. Specify and exercise a global owned-storage quota and in-use pinning.
The candidate and stop gates are in [the revised design](design.md).

The next-runtime delivery slice now retains and recovers its exact owned FUSE
connection, validates large borrowed DSFs without caching mesh copies, and keeps
ordinary cached/mmap delivery after the unsuccessful direct-I/O experiment.
The actual 147-MB US mesh passed byte-identity delivery without imagery work.
Efficient immutable DDS range products, remaining imagery coverage/decode
boundaries and flight-aware scheduling are still open; these fixes are not a
claim that the failed simulator cold-start gate now passes.

## Alpha scope

The first alpha is a **native-file scenery preparation workflow** for an existing
terrain-only DSF, with a Rust engine and Go/Charm client.

The DSF supplies geometry and UV mappings. Terrain definitions supply texture
bindings. Neither OSM nor VIIRS substitutes for either. Do not construct a flat
mesh, crop away required dependencies, or call a lighting-only cache a complete
scenery package.

The compiler accepts local prepared imagery, with a georeferenced raster adapter
for an explicitly prepared Web Mercator raster. It compiles daytime and night
textures outside the flight path and retains source DSF bytes unchanged.
The alpha quality profile and any resampling are declared in its manifest.

Reuse local normalized OSM/VIIRS data read-only. Their classification and missing
quality metadata limit photometric fidelity; do not describe them as a complete
lamp survey. New raw PBF/quality-aware VIIRS ingestion is a later source-adapter
stage, not a second flight-time code path.

The existing AutoOrtho mesh is available, but its daytime cache is incomplete:
the inspected Raleigh DSF references 321 unique imagery footprints and only a
subset have complete cached imagery. Missing imagery must block preparation,
not become a black placeholder. An open georeferenced source such as USDA NAIP
is a suitable separate preparation input for the US, subject to verified
coverage and provenance.

A ready alpha generation means local package assets are structurally verified.
It does **not** mean appearance or load time has been validated in X-Plane.
Simulator qualification remains explicit.

## Deliberate limits

- One bounded preparation/verification job at a time; no unbounded task spawning.
- The native reference exports ordinary files. The separate automatic prototype
  uses FUSE; EROFS and Nydus are not dependencies.
- No automatic imagery-provider scraping or assumed redistribution rights.
- No rewriting an existing simulator configuration or activating scenery as a
  side effect of preparation.
- No complete global mesh importer, regional atlas optimization, live preset
  replacement, or promised zero-stall online streaming.

## Build layout

`engine/` contains the Rust domain, preparation, lifecycle and socket boundary.
`client/` contains the Go terminal client and CLI, sharing its protocol client.
Tests stay beside the behavior they verify, with integration tests for process
and filesystem boundaries. Source data, local configuration, outputs and build
artifacts stay outside version control.
