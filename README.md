# GroundEffect

A Linux-native orthophoto and night-lighting streamer for X-Plane 12, with
complete offline native exports.

GroundEffect delivers on-demand satellite orthophotos (`_BI16.dds`) and
physically motivated night-lighting textures (`_GE_LIT.dds`) through a
read-only Rust FUSE filesystem, controlled by a terminal-only Go/Charm
dashboard (`groundeffect`) over a local Unix socket.

## Highlights

- **Start and fly:** No manual region selection or multi-hour pre-flight batch
  compilation. When `source_package` is empty (`""`), GroundEffect compiles
  Laminar X-Plane 12 Global Scenery DSFs into orthophoto terrain meshes on the
  fly and extracts road vectors directly from the simulator's base DSFs.
- **Multiple imagery providers:** Switch live or offline between Bing Maps
  (`bing`), Esri World Imagery (`arcgis_world_imagery`), Esri Clarity
  (`arcgis_clarity`), and USGS Imagery Only (`usgs_imagery_only`).
- **Physically grounded night lighting:** Synthesizes `_GE_LIT.dds` textures
  and regional 3D spill lighting from road networks, VIIRS satellite night
  radiance, and daytime surface reflectance/canopy profiles.
- **Live telemetry & heading look-ahead:** Listens for X-Plane UDP `RPOS`
  telemetry (`127.0.0.1:49000`) to pre-warm tiles in a velocity-scaled forward
  cone and display a live aircraft radar grid in the terminal UI.
- **Bounded, immutable cache:** Enforces a strict disk budget (`budget_mib`)
  with atomic publication and pinned active file handles. Closing the TUI never
  interrupts the engine or active flight scenery.

---

## Linux Requirements & Installation

### Runtime requirements

- **Linux (`x86_64`)** with FUSE 3 enabled (`/dev/fuse` and `fusermount3` in
  `PATH`).
- **X-Plane 12 for Linux** (standalone installer or Steam). GroundEffect
  automatically locates Global Scenery via `$XPLANE_ROOT`,
  `~/.x-plane/x-plane_install_12.txt`, and Steam's `libraryfolders.vdf`.

### Option 1: Reproducible build with Nix (any Linux distro)

If you have [Nix](https://nixos.org/) with flakes enabled:

```sh
git clone https://github.com/keskvarc/groundeffect.git
cd groundeffect
nix develop --command make build
```

### Option 2: Native distro packages

GroundEffect requires **Rust** (1.85+ / Edition 2024), **Go** (1.24+), **GDAL**
development headers, **Clang/libclang** (used by `bindgen` for GDAL bindings),
**pkg-config**, and **FUSE 3**.

**Ubuntu / Debian:**
```sh
sudo apt update
sudo apt install -y build-essential pkg-config clang libclang-dev libgdal-dev fuse3 libfuse3-dev golang-go curl
# If distro Rust/Go are older than required, install latest via rustup.rs / go.dev
make build
```

**Fedora / RHEL:**
```sh
sudo dnf install -y gcc gcc-c++ make pkgconf-pkg-config clang clang-devel gdal-devel fuse3 fuse3-devel golang cargo rust
make build
```

**Arch Linux:**
```sh
sudo pacman -S --needed base-devel pkgconf clang gdal fuse3 go rust
make build
```

This produces two binaries in `./bin/`:
- `bin/groundeffect` — Terminal UI and CLI client
- `bin/groundeffect-engine` — Background Rust FUSE & compilation engine

---

## Quick Start (Automatic Streaming)

1. **Create your reader configuration** from `examples/stream.json`:

   ```sh
   mkdir -p ~/.config/groundeffect
   cp examples/stream.json ~/.config/groundeffect/stream.json
   ```

   Edit `~/.config/groundeffect/stream.json`:
   - `mount_point`: Must be `"<DATA_DIR>/reader"` (default:
     `"/home/<USER>/.local/share/groundeffect/reader"`).
   - `source_package`: Leave `""` to auto-compile X-Plane 12 Global Scenery
     DSFs on demand, or set to an existing Ortho4XP/AutoOrtho physical mesh
     directory.
   - `osm_db`: Path to your X-Plane 12 Global Scenery directory (for automatic
     DSF road extraction) or a normalized `.rds1` / `.sqlite` road archive.
   - `viirs`: Optional path to a `viirs.tiles` (`RDT1`) radiance file, or
     `null`.
   - `provider`: `"bing"` (default), `"arcgis_world_imagery"`,
     `"arcgis_clarity"`, or `"usgs_imagery_only"`.

2. **Configure, mount, and link into X-Plane 12:**

   ```sh
   bin/groundeffect stream configure --config ~/.config/groundeffect/stream.json
   bin/groundeffect stream start
   bin/groundeffect stream install --target "/path/to/X-Plane 12/Custom Scenery/z_groundeffect_stream"
   ```

3. **Order `scenery_packs.ini`:**
   Ensure `SCENERY_PACK Custom Scenery/z_groundeffect_stream/` is placed
   **below** airports and overlays (`Global Airports`, SimHeaven/X-World) and
   **above** any competing base meshes.

4. **Launch the dashboard & fly:**

   ```sh
   bin/groundeffect
   ```

   *(Optional)* For live aircraft position and directional look-ahead, enable
   **Send network data output -> UDP port 49000** in X-Plane 12's Network
   settings.

---

## Terminal UI (TUI) Guide

### Keyboard shortcuts

| Key | Action |
|---|---|
| `1` / `2` / `3` or `Tab` | Switch views (`1 Stream`, `2 Generations`, `3 Guardrails`) |
| `s` | Start or stop the FUSE streaming mount |
| `m` | Cycle imagery provider (`bing` → `arcgis` → `clarity` → `usgs`) |
| `[` / `]` | Decrease / increase DDS texture resolution (`256` / `512` / `1024` / `2048`) |
| `p` | Prepare offline native scenery generation |
| `v` | Verify selected generation (`j` / `k` to select) |
| `q` | Close the TUI (engine and mounted scenery stay running) |

### Radar Grid (`GRID`) Legend

When streaming is active, the right-hand radar grid displays tile states around
your aircraft (or active DSF region):

- **`load` (Green `■`)**: **DDS Loaded** — X-Plane has loaded both the terrain
  definition (`.ter`) and its `.dds` texture (or the cached tile is active
  within close range).
- **`ter` (Cyan `■`)**: **Terrain Only** — X-Plane has indexed the `.ter`
  definition, awaiting `.dds` payload reads.
- **`bld` (Amber `■`)**: **Building** — GroundEffect workers are actively
  fetching imagery and encoding the `.dds` pair.
- **`cache` (Steel `■`)**: **Cached** — Complete `.dds` assets are on disk in
  the managed cache and ready for immediate read hits.
- **`·cold`**: Registered in the virtual tree, not yet requested or built.

---

## Native Reference Export (Offline Compilation)

To compile a standalone, offline scenery generation that requires no running
engine or FUSE mount during flight:

```sh
bin/groundeffect configure --config /absolute/path/to/preview.json
bin/groundeffect prepare --wait
bin/groundeffect --json status
```

See `examples/preview.json` and [the implemented alpha contract](docs/alpha.md)
for offline GeoTIFF/NAIP preparation (`scripts/prepare_naip.py`), verification,
and installation.

## Development & Verification

```sh
make check        # Rust fmt, clippy (-D warnings), unit tests + Go fmt, vet, race tests
make integration  # End-to-end Rust daemon + Go CLI + FUSE lifecycle test suite
```

## Engineering and Design Documentation

| Document | Contents |
|---|---|
| [Engineering tenets](AGENTS.md) | Architecture before code, shared policy, immutable assets, clear failures and evidence-based quality |
| [Development plan](docs/development-plan.md) | Ordered vertical slices and acceptance gates |
| [Alpha contract](docs/alpha.md) | Offline compiler behavior, commands, formats, and resource limits |
| [Automatic prototype](docs/automatic-prototype.md) | Demand-only FUSE reader, bounded cache, source pooling, and lifecycle recovery |
| [Design](docs/design.md) | Architecture, technology choices, lighting model, readiness, and tradeoffs |
| [Terminal interface](docs/terminal-ui.md) | Charm TUI architecture, keyboard controls, and radar grid |
| [Research](docs/research.md) | Primary-source findings, evidence limitations, and source ledger |
| [Benchmark plan](docs/benchmark-plan.md) | Delivery backend evaluation and startup/streaming verification |

