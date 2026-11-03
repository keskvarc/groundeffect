# GroundEffect: terminal interface

Date: 2026-11-02  
Status: implemented in `client/` (Bubble Tea v2, Lip Gloss v2, Bubbles v2) over
the version-1 local Unix socket protocol (`engine/src/service.rs`).

## Implemented views and controls

- **`1 Stream`**: Flight telemetry (`UDP 49000` RPOS lat/lon, ground speed,
  AGL, track, flight phase, look-ahead cone stats), tile load progress, cache
  budget and delivery throughput, and the spatial **GRID** radar display:
  - `load` (Green `■`): Both `.ter` and `.dds` loaded by X-Plane (or cached
    within active proximity).
  - `ter` (Cyan `■`): `.ter` definition read by X-Plane; `.dds` payload not yet
    loaded.
  - `bld` (Amber `■`): Tile currently being fetched/encoded by a worker.
  - `cache` (Steel `■`): Tile `.dds` cached on disk, awaiting simulator read.
  - `·cold`: Prepared in the virtual directory tree, not yet built.
- **`2 Generations`**: Offline native export catalog, configuration summary,
  active job progress, and verification triggers.
- **`3 Guardrails`**: Runtime invariants, installation target paths, and
  explicit failure diagnostics.

Keyboard shortcuts: `1/2/3` or `Tab`/`Shift+Tab` switch tabs, `s` toggles the
FUSE reader mount, `m` cycles the tile imagery provider (`bing`,
`arcgis_world_imagery`, `arcgis_clarity`, `usgs_imagery_only`), `[` and `]`
adjust texture resolution (`256`, `512`, `1024`, `2048`), `p` triggers offline
preparation, `v` verifies the selected generation, and `q` detaches the TUI
without stopping the engine.

## Direction

**Terminal-only, but not a wall of logs or an old desktop form squeezed into a
terminal.** The experience should feel like a purpose-built night-flight
instrument panel: strong hierarchy, generous spacing, restrained color, clear
states, and immediate keyboard navigation.

There is no planned GTK, browser, or Electron interface. A full-screen TUI is the
interactive experience; scriptable commands remain first-class.

## Framework recommendation

Use the actual Charm ecosystem for the proposed terminal client:

| Component | Responsibility |
|---|---|
| Go + Bubble Tea | Event-driven model/update/view application |
| Lip Gloss | Layout, typography, spacing, borders, and color profiles |
| Bubbles | Inputs, selection lists, tables, progress and viewport components |
| Rust engine | Preparation, catalog, readiness, activation and asset lifecycle |
| Local Unix socket | Versioned commands, snapshots and bounded progress events |

These are supported capabilities of the upstream frameworks, not promises of a
finished design. Sources are [R33-R35 in the research ledger](research.md#source-ledger).
Pin compatible releases when implementing; do not mix examples from different
Charm major versions.

### Why not force the entire application into one language?

The terminal client does not decode imagery, build DDSs, query OSM, or answer
X-Plane file requests. Go is a reasonable choice for this isolated presentation
layer, while the engine remains Rust. Client GC is in a separate process, not a
pause in the engine, although CPU/memory consumption can still contend with the
simulator.

The cost is real: two toolchains, two binaries, and a control protocol to maintain.
This is a proposed tradeoff for using Charm directly, not a performance
requirement. Ratatui + Crossterm is the credible all-Rust alternative if minimizing
build/packaging complexity takes priority. It can achieve the same aesthetic,
but needs its own component composition and styling.

No Go/Rust FFI, embedded Go runtime, or shell-command scraping is proposed.

## Visual language

Use graphite/slate surfaces, warm amber for lighting/attention, a restrained cool
accent for navigation, and quiet green for complete/ready states. Respect light
themes too. Color conveys emphasis, not the only evidence of state.

Prefer a few well-composed panels to a grid of boxes around every label.
Use compact sparklines and progress indicators where they answer a question.
Animations should communicate work or a transition, not run as perpetual
decoration.

No mandatory Nerd Font or emoji glyphs. Support Unicode line/block drawing where
reliable, with an ASCII fallback. Truecolor is an enhancement, not a prerequisite;
256-color, monochrome, and no-color output remain usable.

The following is a layout sketch, **not application output**. All values are
illustrative, and the ASCII sketch does not represent the final styling:

```text
  GROUNDEFFECT                                      ENGINE CONNECTED
  Night scenery. Ready before you fly.

  Overview    Coverage    Jobs    Storage    Settings
  ---------------------------------------------------------------

  ACTIVE SCENERY                         PREPARATION
  North Carolina                         Virginia / balanced
  READY  /  balanced profile              Compiling textures
  Native files                           [============------] 67%
  Verified local coverage                 1,840 / 2,750 assets
  No network required                     ETA calculating...

  COVERAGE                               STORAGE
  #####....                               Active         18.4 GiB
  ####++...   # ready                      Staging         3.2 GiB
  ..++++...   + preparing                  Budget         80.0 GiB
  .........   . not prepared               Reserved        8.0 GiB

  RECENT ACTIVITY
  21:24  North Carolina generation activated
  21:25  Virginia compilation started

  / search     ctrl+p commands     enter inspect     ? help     q close
```

The coverage overview is derived from existing manifests, not a new tile server
or a continuously rerendered OSM map. It must distinguish complete coverage from
preparing, reduced-quality, missing, and failed states in both text and color.

## Primary views

| View | Purpose |
|---|---|
| Overview | Is the selected coverage ready, what is active, and what needs attention? |
| Coverage | Search/select regions and inspect complete DSF coverage, quality, source age and missing dependencies |
| Jobs | Real stage progress, throughput, memory reservation, retry/resume/cancel controls |
| Storage | Active/staged/source space, budget, update reservations and safe reclaim candidates |
| Settings | Quality/exposure profiles, source policy, paths, budgets and terminal preferences |

Failures appear beside the affected item with a concise cause and relevant
action. A log viewer is secondary, not the primary interface. An unavailable
metric says unavailable; native-file mode must not invent per-read latency
telemetry that no reader is collecting.

Show an ETA only when there is enough measured work and a known remaining scope.
Separate downloading, importing, compiling, packaging, and verification.
Never use an animated spinner or high completion percentage as evidence that
coverage is ready.

## Interaction

Use arrows and optional j/k, Tab/Shift-Tab for focus, Enter to inspect/confirm,
Escape to return, `/` for search, and Ctrl-P for a searchable command palette.
Display contextual keys instead of requiring a memorized shortcut sheet.
Mouse support is optional; every operation must work without it.

Keep high-impact actions explicit. Activation displays the generation/profile
and any required simulator restart or scenery reload. Canceling a job is a
separate action. Deleting source data or reclaiming storage identifies exactly
what would be removed and cannot include active dependencies.

Quitting the TUI detaches the client. It does not cancel engine jobs or stop
mounted scenery. Reconnecting restores authoritative state rather than
reconstructing jobs from terminal history.

## Shell behavior

Proposed entry points, not currently available commands:

| Invocation | Behavior |
|---|---|
| `groundeffect` | Opens the TUI when attached to a usable terminal; otherwise displays usage without terminal control sequences |
| `groundeffect tui` | Explicit interactive mode; reports a clear error if no suitable terminal exists |
| `groundeffect status` | Compact human-readable state, suitable for ordinary shell use |
| `groundeffect status --json` | Machine-readable structured output with no styling |
| `groundeffect prepare ...` | Starts a preparation job using the same engine commands as the TUI |
| `groundeffect verify ...` | Explicit integrity verification, with meaningful exit status |

Normal commands do not enter alternate-screen mode. Keep diagnostics separate
from structured output; errors must have nonzero status. Honor `NO_COLOR` and
terminal capabilities. Noninteractive commands cannot wait invisibly for a
confirmation dialog; require explicit arguments or report what is missing.

## Engine boundary and performance

The Rust service owns all source policy, validation, job state, disk budgeting,
and activation decisions. The client cannot mark coverage ready based on a
progress counter. Both interactive and scriptable actions use the same commands.

Use a small versioned local protocol with request IDs, bounded message sizes,
explicit errors, snapshots and progress subscriptions. Coalesce intermediate
progress updates for slow clients, while retaining authoritative job outcomes
for snapshot/reconnect. Do not pipe images, features, or per-read traces through
UI IPC.

The client must not block its update/view loop waiting on the socket. Put I/O in
commands, bound pending work, and show stale/disconnected state explicitly.
Activation/retry requests need engine-side duplicate protection where replay
would have side effects.

Render on state/input changes. Begin with progress/metrics updates around
2-4 Hz and bounded animation only while visible work is active; measure actual
client CPU before increasing it. No busy polling or permanent 60-fps dashboard.
The engine never waits for a UI redraw, log viewport, or connected terminal.

## Terminal robustness and acceptance

Support common Linux terminals, SSH, tmux, resize, a compact 80x24 layout, and
narrower single-column layouts where useful. Large terminals can show
additional detail without making the small layout incomplete.

Test keyboard focus, contrast and non-color state cues, Unicode widths, pasted
input, and long region names/errors. Render provider filenames and logs as text,
not executable terminal escape sequences.

Restore terminal modes/cursor on normal exit and handled interrupts; use
best-effort cleanup on panic. A hard process kill cannot guarantee cleanup.
Test client loss, slow output, reconnect, service version mismatch, and multiple
clients without affecting scenery or engine job execution.

Implement a small themed navigation prototype to validate appearance early, but
do not build a complete UI around an unverified scenery engine. The architecture's
native-file correctness and backend measurement gates still precede the full
interface.
