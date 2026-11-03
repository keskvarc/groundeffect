# GroundEffect engineering tenets

These rules apply to all code, tests, tooling, and documentation in this project.
Read the relevant design and development-plan sections before changing behavior.

## Architecture before implementation

- Define the problem, inputs, outputs, ownership, failure behavior, and acceptance
  criteria before coding. Resolve consequential ambiguity instead of hiding it
  behind a convenient default.
- Build small, working vertical slices. Each stage must leave a coherent,
  inspectable system, not disconnected scaffolding or a dashboard over fake data.
- Keep design documents aligned with implementation. Label proposals, measured
  behavior, unsupported capabilities, and experimental results distinctly.
- Justify changes to architectural boundaries in a short decision note before
  implementation. Prefer an existing mechanism over a new service or protocol.

## Elegant code is simple, explicit, and cohesive

- Optimize for correctness, clarity, and ease of change, not cleverness or line
  count. Use small functions and modules with one meaningful responsibility.
- Give concepts precise names and types. Make units, coordinates, asset identity,
  lifecycle states, and ownership unambiguous.
- Prefer straightforward data flow and composition. Avoid speculative
  abstractions, unnecessary traits/interfaces, generic frameworks, configuration
  switches, and dependencies without a concrete current use.
- Centralize policy and shared domain logic. Remove true duplication, but do not
  couple unrelated behavior merely because a few lines look similar.
- Use comments to explain non-obvious intent or constraints, not to narrate code.
  Remove dead paths and stale documentation as part of the change that replaces
  them. Do not retain obsolete implementations as informal backups.

## One authoritative engine; thin clients

- Rust owns domain rules, preparation, readiness, persistence, activation, and
  asset lifecycle. The Go/Charm client owns terminal interaction and presentation.
- Both TUI and scripted commands use the same engine operations. Do not implement
  a second readiness calculator, job manager, or storage policy in the client.
- Keep control IPC typed, versioned, bounded, and explicit about failures. No
  parsing human-facing command output as a machine interface.
- Closing or losing a client must not cancel engine jobs or interrupt active
  scenery. Slow UI output must never backpressure scenery delivery.
- Keep the interface terminal-only. Do not introduce a browser runtime or desktop
  toolkit to solve a terminal interaction problem.

## Automatic operation is the product contract

- Normal use is start GroundEffect, start X-Plane, and fly. Do not make manual
  region selection, long regional compilation, or a country-wide source import
  prerequisites for an ordinary flight.
- Schedule work from actual asset/mip demand and bounded look-ahead. Automatic
  operation is not a regional batch job hidden behind an "auto" button.
- Distinguish cached delivery, cold streaming and complete offline exports.
  A cold request may wait for bounded fetch/build work; never label that path
  ready, instantaneous or guaranteed stall-free.
- Keep filesystem delivery small. Source fetch/build runs on bounded independent
  workers; a cold read may depend on their completion, but a cache hit must not.
- Manage a global byte budget for assets, sources, staging and retained versions.
  Protect in-use data and reserve space before work; do not grow by region count.

## Correct scenery is a hard invariant

- A terrain footprint is geographic identity, not output pixel count. Source
  sampling zoom, resampling, and DDS dimensions must not move that footprint.
- Use explicit terrain texture references and preserve mesh/resource semantics.
  Verify projection, orientation, mip layout, and boundary behavior independently.
- Published assets have stable lengths, complete mip chains, and immutable bytes.
  Do not mutate a file behind an open handle or a cached generation.
- Readiness means verified dependency completeness at the declared quality.
  A filename, live PID, successful health endpoint, or progress percentage is not
  evidence of ready scenery.
- Never substitute provisional black images, malformed DDSs, invented lighting,
  or silently reduced quality to make a failed operation appear successful.

## Performance comes from removing work

- No network, source-geometry query, rasterization, or image encoding in ready
  scenery's filesystem request path. Serve prepared local assets.
- Measure representative end-to-end workloads before selecting an optimization.
  Compare identical logical assets and quality, and distinguish preparation,
  process startup, cold storage, warm cache, and simulator frame time.
- Bound memory bytes, disk reservations, file descriptors, tasks, queues, and
  concurrency. Reserve expensive resources before dispatching work.
- Use batching and appropriate native libraries. Do not rewrite proven codecs or
  geospatial algorithms merely to avoid a language boundary.
- Do not add a custom filesystem, archive format, busy polling, broad privilege,
  or global system tuning without a demonstrated requirement and measured benefit.

## Fail clearly; publish atomically

- Propagate actionable errors with context. No broad catch-and-ignore, silent
  defaults, success-shaped fallbacks, or panics on external input.
- Validate untrusted paths, lengths, indices, and protocol values at boundaries.
  Do not allow source files or terminal text to escape their intended context.
- Persist staged work and final publication deliberately. A crash or full disk
  must not promote incomplete work or damage a prior valid generation.
- Keep active assets pinned. Cleanup and updates cannot remove flight dependencies.
  Do not alter a user's simulator installation without an explicit operation.
- Never run downloaders, parsers, or UI code with administrative privileges.
  Do not modify unrelated repositories or user data as an implementation shortcut.

## Test behavior and preserve reproducibility

- Write focused tests for introduced behavior and its meaningful failure cases.
  Prefer real temporary files and actual protocol/CLI integration where those are
  the boundary being changed; mocks alone do not prove integration.
- Test resource limits, malformed/truncated inputs, interrupted publication,
  concurrent ownership, and recovery where relevant. Check final encoded assets,
  not only intermediate images.
- Run the affected language's formatter, compiler, linter, and focused tests.
  A UI build is not proof of scenery correctness, and a live process is not proof
  of service readiness.
- Keep dependency lockfiles and reproducible development/build commands.
  Exclude binaries, build caches, private paths, source datasets, and secrets from
  commits. Generated protocol artifacts must have a documented source of truth.
- Commit coherent, reviewable changes. Do not mix unrelated cleanup into a feature
  or claim simulator validation that has not actually happened.

## Definition of done

The declared slice works end to end, its relevant failure paths are exercised,
required checks pass, and its behavior and limits are documented accurately.
Anything unverified or blocked is stated plainly, not hidden behind an alpha
label or a successful-looking interface.
