# LayerGen Roadmap and Context

## Primary goal

Unblock game development with the smallest correct implementation of a
Bevy-specific, domain-agnostic dense 3D chunk-processing library.

Keep game semantics in the consumer. LayerGen supplies dense storage, spatial
addressing, pipeline execution, asynchronous lifecycle, bounded neighborhood
processing, resident chunk management, tracked editing, and persistence
integration.

Prefer explicit limitations over silently approximating unsupported behavior.

## Technology

- Rust edition 2021.
- Bevy 0.20.
- Lightweight library dependencies:
  - bevy_app
  - bevy_ecs
  - bevy_tasks with multi_threaded enabled
- Optional Rayon support, enabled by default.
- Optional Serde support.
- Optional serde_json, tempfile, and fs2, used only by the consumer-side
  durable example (`durable-example` feature), not by the core storage API.
- Current asynchronous runtime targets native platforms.
- No guarantee of nonblocking CPU generation or blocking storage on WebAssembly.

## Scope and core principles

- Dense Vec<T> storage.
- Finite logical world with explicit aligned chunks.
- World dimensions are metadata, not an instruction to allocate the world.
- Consumers request chunk coordinates, not arbitrary overlapping boxes.
- Reusable pipeline definitions are separate from request execution.
- Base generation is independent of request order, resident neighbors,
  and player edits.
- Determinism requires consumer algorithms to use stable inputs, coordinates,
  and seeds rather than shared mutable RNG state or execution order.
- Shared processing data is consumer-defined and read immutably by chunk jobs.
- Steps write only their assigned output chunk.
- Cross-chunk inputs are stage-specific and stable.
- No transparent unlimited recursive neighbor generation.
- Persistence stores latest sparse replacements, not an edit-event history.
- Saved replacements are applied after the complete base-generation pipeline.
- Residency tokens prevent stale async loads from installing obsolete chunks.
- Save acknowledgement preserves edits newer than the saved snapshot.
- Storage revision checks prevent stale conditional writes.
- Chunk management and editing are common library APIs.
- Entity data, inventories, collision rules, and placement rules remain
  consumer-owned and separate from the voxel dataset.

## Current files

### Library and example

- Cargo.toml
- src/lib.rs
- src/data_set.rs
- src/spatial.rs
- src/step.rs
- src/definition.rs
- src/neighborhood.rs
- src/runtime.rs
- src/persistence.rs
- src/manager.rs
### Examples

- examples/durable.rs
- examples/support/file_storage.rs

### Integration tests

- tests/async_runtime.rs
- tests/preparation.rs
- tests/smoke.rs
- tests/neighborhood.rs
- tests/persistence.rs
- tests/manager.rs
- tests/chunk_data.rs

Additional unit tests are embedded in library modules. The filesystem backend
tests are embedded in the durable example and run with
`cargo test --all-features --example durable`.

## Milestone status

| Milestone | Status |
|---|---|
| Hardened dense storage | Implemented |
| Spatial foundation | Implemented |
| Reusable local/shared pipelines | Implemented |
| Async Bevy runtime | Implemented |
| Once-per-generator shared preparation | Implemented |
| One bounded neighborhood stage | Implemented |
| Sparse persistence records and conditional storage | Implemented |
| Managed residency, editing, and revisions | Implemented |
| Per-chunk consumer data (`C`) | Implemented |
| Durable backend and restart validation | Implemented as a consumer-side example |
| Save-and-unload and shutdown/drain workflow | Next |
| General staged neighborhoods | Deferred until needed |

“Implemented persistence” means the storage contract, in-memory backend, and
runtime integration. The library still does not bundle durable storage: the
filesystem backend is a consumer-side example that validates the contract
across normal process restarts.

## Implemented and locally validated

### 1. Hardened dense storage

DataSet<T> provides:

- Checked, nonzero dimensions.
- Checked dimension multiplication.
- Fallible storage reservation.
- Indexing order: x + y * width + z * width * height.
- Checked coordinate/index conversion.
- get/get_mut and tuple indexing.
- Sequential coordinate-aware iterators.
- Optional Rayon coordinate-aware iterators.
- Six dataset-local face neighbors.
- Clone and owned iteration.
- Optional serialization and validated deserialization.

Ordinary storage operations do not require Send or Sync.

Rayon bounds are restricted to parallel operations:

- Immutable iteration requires T: Sync.
- Mutable iteration requires T: Send.

Invalid neighbor centers return six absent neighbors.

DataSet::neighbors is NOT cross-chunk access.

### 2. Spatial foundation

Types:

- GlobalPosition = (i64, i64, i64)
- LocalPosition = (usize, usize, usize)
- ChunkCoord(u32, u32, u32)
- ChunkSize
- WorldBounds
- PipelineConfig
- StepContext
- Chunk<T>

Coordinate contract:

- Global coordinates are signed.
- Chunk coordinates are nonnegative offsets relative to the world origin.
- Negative world origins are supported.
- World bounds are minimum-inclusive and maximum-exclusive.
- Configured chunks are fixed and aligned.
- Edge datasets contain only valid world cells and may be smaller.
- ChunkSize and world dimensions use u32.
- Spatial configuration does not allocate world volume.

PipelineConfig contains world bounds, configured chunk size, and seed.

Raw Chunk<T> exposes mutable data. Managed chunks deliberately do not.

### 3. Reusable pipelines and generation definitions

PipelineBuilder builds a definition without initializing voxel storage or
executing preparation.

Local step contract:

    Step<T>::run(
        &self,
        data: &mut DataSet<T>,
        context: &StepContext,
    ) -> PipelineStepResult

Shared-data step contract:

    SharedStep<T, D>::run(
        &self,
        data: &mut DataSet<T>,
        context: &StepContext,
        shared: &D,
    ) -> PipelineStepResult

Steps execute sequentially inside a chunk job.

Step definitions require Send + Sync + 'static.

Initializers receive global coordinates:

    Fn(GlobalPosition) -> T + Send + Sync + 'static

Runtime voxel types require Send + Sync + 'static.

Builder operations:

- new(config)
- prepare(callback), before adding steps
- add_step(local_step)
- add_shared_step(shared_step)
- build(initializer)

LocalStep adapts existing Step implementations for shared-data pipelines.

Pipeline defaults to shared-data type ().

GenerationDefinition<T> separates shared preparation from chunk execution.
Both regular and neighborhood pipelines implement this contract.

Synchronous diagnostic methods:

- generate_chunk(coord)
  - Available for pipelines without pending preparation.
- prepare_shared()
  - Obtains shared data synchronously.
  - Does not cache preparation.
  - Repeated direct calls repeat preparation.
- generate_chunk_with_shared(coord, &shared)
  - Executes with explicitly supplied shared data.

Generation returns base data only. Persistence application belongs to the
managed loading layer.

### 4. Bevy asynchronous generation runtime

Public types include:

- LayerGenPlugin<T>
- ChunkGenerator<T>
- RequestId
- ChunkCompletion<T>
- GenerationLimits
- RequestError
- GenerationError
- PreparationError
- PreparationStatus
- LayerGenSet

The resource depends on voxel type, not the concrete pipeline/shared-data type.

One generation definition per voxel type per Bevy world.

The plugin owns the definition.

Scheduling:

- PreUpdate: collect preparation and generation results.
- PostUpdate: start preparation or dispatch queued chunk jobs.
- Update: consumers normally handle results.

Execution uses AsyncComputeTaskPool.

poll_once checks readiness without waiting for the task to finish.

Headless App tests use fallback task-pool initialization.

### 5. Raw generation request lifecycle

API:

    generator.request_batch(coord)
    generator.take_completed()

Despite its name, request_batch requests ONE chunk.

Lifecycle:

    request -> queued -> running -> completion -> caller owns chunk

Request IDs:

- Unique within one generator instance.
- Not persistent or globally unique.

Completions contain request ID, coordinate, and an owned Chunk or error.

Completion order is unspecified.

GenerationLimits bound:

- In-flight chunk jobs.
- Outstanding requests, including queued, running, and unconsumed completions.

Preparation is separate from the in-flight chunk-job count.

Duplicate coordinates remain reserved until completion consumption, including
failed completions.

Invalid coordinates are rejected before dispatch.

The raw generator has no automatic cache.

When ChunkManagerPlugin<T> is installed, the manager exclusively owns this
request/completion channel for T. Game systems must not concurrently use it.

### 6. Shared preparation lifecycle

Lifecycle:

    Unprepared -> Preparing -> Ready
                          \-> Failed

Contract:

- Starts automatically when the app runs, even without requests.
- Executes once per installed generator on a background task.
- Valid requests may queue during preparation.
- Chunk jobs cannot dispatch before readiness.
- Prepared data is shared immutably and retained by the ready worker/jobs.
- Pipelines without preparation begin Ready.
- No retry or hot replacement yet.

Preparation failure:

- Permanently sets Failed status.
- Resolves accepted queued requests as failed completions.
- Rejects new valid requests with PreparationFailed.
- Failure completions occupy capacity until consumed.

Ordinary processing errors and unwinding panics are reported.

Abort-mode panics and process termination cannot be recovered.

### 7. One bounded neighborhood stage

Implemented pipeline shape:

    Shared preparation
            |
    Chunk-local prefix
            |
    One neighborhood stage
            |
    Optional local/shared suffix

Public types include:

- Neighborhood
- NeighborhoodLimits
- NeighborhoodStep<T, D>
- NeighborhoodView
- NeighborhoodPipeline

Implemented builder shape:

    pipeline.with_neighborhood(
        Neighborhood::radius(1),
        limits,
        neighborhood_step,
    )
    .add_step(local_suffix)

The wrapper accepts a regular Pipeline, preventing accidental stacking of
neighborhood stages without dependency-footprint planning.

Input/output contract:

- Radius is symmetric and declared in cells.
- Inputs are immutable prefix-only data.
- Output starts from the requested chunk's prefix result.
- Output writes are limited to the requested chunk.
- Out-of-world reads within the declaration return Ok(None).
- Undeclared reads and missing internal cells are errors.
- Saved edits and resident neighbors are not neighborhood inputs.

Execution occurs inside the existing request task:

1. Validate the footprint and budgets before prefix execution.
2. Generate the requested prefix output.
3. Generate intersecting dependency chunks through the prefix only.
4. Copy intersections into an immutable halo.
5. Run the neighborhood step.
6. Run suffix steps.
7. Return only the requested chunk.

Dependencies execute sequentially:

- No nested task waits.
- No public neighbor requests or completions.
- No requirement that neighbors already be resident.
- No cross-request dependency cache or deduplication.

T: Clone is required for neighborhood pipelines, not ordinary storage or
ordinary pipelines.

NeighborhoodLimits bound:

- Prefix chunks intersecting the footprint, including the requested chunk.
- Total prefix cells processed.
- Temporary dataset cells.

Temporary accounting covers:

    halo + retained output + largest additional dependency

It does not cover arbitrary allocations inside callbacks, shared data, or T.
These are cell/work limits, not byte budgets.

Validated scenarios include reference results across chunks, reversed request
order, negative origins, partial edges, boundaries, limits, and async execution.

### 8. Sparse persistence foundation

Public types include:

- WorldId
- GenerationId
- PersistenceIdentity
- ChunkKey
- StorageRevision
- ChunkRecord<T>
- StoredChunk<T>
- ChunkStorage<T>
- MemoryChunkStorage<T>
- PersistenceError

Record contents:

- World and generation identities.
- PipelineConfig.
- Chunk coordinate and actual dimensions.
- Local linear index -> latest replacement value.

Storage supplies the revision separately.

Lookup key:

    world identity + chunk coordinate

Generation identity is intentionally not part of the key. Incompatible saved
records must be rejected, not silently hidden as absent.

Validation checks identities, configuration, dimensions, and replacement
indices before applying saved values.

The consumer must change GenerationId when algorithm changes make saved
replacements incompatible. PipelineConfig comparison also catches changed
seed, bounds, and chunk size.

Storage contract:

- load returns a consistent record/revision pair.
- compare_exchange atomically checks the expected revision and replaces state.
- Successful writes return strictly increasing revisions.
- Conflicts do not modify stored state.
- None means absent record; existing revisions start at 1.
- Empty replacement records preserve revision history.

Backend errors may have an indeterminate commit outcome. An error is not proof
that no write occurred.

MemoryChunkStorage uses a mutex for atomic comparison and replacement.
It does not survive process exit.

No serialization format, database, or filesystem layout is prescribed.

Optional DataSet Serde support does not constitute a durable record backend.

### 9. Managed chunks and editing

ManagedChunk<T> retains:

- Immutable generated base chunk.
- Current edited dataset.
- Sparse replacements.
- Local edit revision.
- Last acknowledged saved edit revision.
- Last acknowledged storage revision.

Managed persistence requires:

    T: Clone + PartialEq

The manager additionally requires Send + Sync + 'static.

Edits:

- Equal to current value: no change or revision increment.
- Different from generated base: insert/update replacement.
- Equal to generated base: remove replacement.
- Effective edits advance the local edit revision.

Dirty tracking is conservative: reverting an edit still requires save
acknowledgement to establish persisted state.

Loaded saved state starts clean at local edit revision zero.

Local edit revisions belong to one residency and are not persistent IDs.

Memory trade-off: roughly two dense datasets per resident chunk plus sparse
replacement storage.

Clone must produce independent editable values. Shared interior mutability
inside T can bypass tracking and is unsupported by this managed-state contract.

### 10. Library-owned chunk manager

Public types include:

- ChunkManager<T>
- ChunkManagerPlugin<T>
- ChunkManagerLimits
- ManagedRequestId
- ManagedCompletion
- ManagedOutcome
- ChunkManagerError

Install after LayerGenPlugin<T>, before submitting raw generation requests.

The manager owns:

- Resident chunks.
- Pending load reservations.
- Tracked editing.
- Background storage operations.
- Save snapshots and acknowledgements.
- Residency validation.
- Managed operation completions.

Game-facing API:

- request_load(coord)
- chunk(coord)
- get_global(position)
- set_local(coord, local, value)
- set_global(position, value)
- request_save(coord)
- unload(coord)
- discard(coord)
- take_completed()

Managed chunks expose read-only datasets.

Load flow:

    Generate complete deterministic base
        -> background load of saved record
        -> validate and apply replacements
        -> validate residency token
        -> install only if current

Invalidated successful loads report Loaded { installed: false }.

Old completions cannot clear a newer load token or install an old residency.

Save flow:

    Capture owned sparse snapshot and local edit revision
        -> background conditional storage write
        -> validate resident token and returned revision
        -> acknowledge captured edit revision

Only one save may be queued/running per chunk.

Editing may continue while saving. Successful acknowledgement does not mark
newer edits clean.

Snapshot cloning occurs in request_save; backend work occurs off-thread.

Failures preserve dirty state and the last acknowledged storage revision.

Conflicts are reported, not silently merged.

Explicit discard/reload is the initial recovery path when the consumer chooses
to abandon local changes.

Unload behavior:

- Clean resident chunk: allowed.
- Dirty resident chunk: rejected.
- Queued/running save: rejected.

Discard behavior:

- Drops unsaved resident edits or invalidates a pending load.
- Does not undo saved records.
- Does not cancel already-running tasks.
- Rejected while saving to prevent a late save racing a new residency.

Limits bound:

- Outstanding operations, including unconsumed completions.
- Concurrent storage jobs.
- Residents plus valid pending-load reservations.

Invalidated operations retain operation capacity until resolved.

Manager scheduling:

- PreUpdate, after LayerGenSet::Collect:
  - transfer base generation completions;
  - poll storage results;
  - validate/install/acknowledge;
  - submit queued work.

Game systems normally use the manager during Update.

Blocking backend calls currently occupy AsyncComputeTaskPool workers.
Dedicated I/O scheduling is not implemented.

### 11. Entity and game-state boundary

Entities and inventories are entirely separate from voxel generation/storage.

Examples:

- NPCs may be simulated at coordinates whose terrain is not resident.
- Furniture can have consumer-owned positions, footprints, and inventories.
- Collision and placement combine terrain queries with entity occupancy.

LayerGen does not define these semantics.

If the game also stores an occupancy marker in a voxel, updates to that marker
and separate entity records are not automatically one atomic transaction.

Cross-domain transaction coordination is outside the current implementation.

### 12. Per-chunk consumer data

Motivation: values coarser than cells (per chunk, per column) had nowhere to
live except inside every T, multiplying memory and repeated sampling of
shared world models (e.g. per-column climate derived from tectonics).

Contract:

    PipelineBuilder::new(config)
        .prepare(prepare_fn)                      // optional, first
        .chunk_data(|context, shared| C)          // optional, before steps
        .add_chunk_step(step)                     // ChunkStep<T, C, D>
        .add_step(step) / .add_shared_step(step)  // adapted, never see C
        .build(initializer)

    ChunkStep<T, C, D>::run(&mut DataSet<T>, &mut C, &StepContext, &D)

- C is consumer-defined and axis-agnostic: per-chunk values, a depth-1
  DataSet of columns, or both. C defaults to () everywhere.
- The chunk data initializer runs once per generated chunk, after cell
  initialization and before the first step, and may read shared data.
- Chunk<T, C> exposes chunk_data(), chunk_data_mut(), into_parts().
- GenerationDefinition<T> has associated type ChunkData
  (breaking only for custom GenerationDefinition implementors).
- Step and SharedStep participate through LocalStep and SharedOnly adapters.
  add_shared_step now wraps in SharedOnly (pipeline type names changed;
  behavior unchanged).
- ChunkGenerator<T, C = ()>, ChunkCompletion<T, C = ()>, LayerGenPlugin<T, C = ()>,
  ChunkManager<T, C = ()>, ManagedChunk<T, C = ()> carry C.
  ChunkManagerPlugin::new remains for C = (); pipelines with chunk data use
  ChunkManagerPlugin::<T, C>::with_chunk_data(identity, storage).
- One generator per (T, C) pair per Bevy world.

Neighborhood:

- with_neighborhood(N: NeighborhoodStep) still works (CellsOnly adapter).
- with_chunk_neighborhood(N: NeighborhoodChunkStep<T, C, D>) also receives
  NeighborChunks<C>: complete prefix chunk data of every planned chunk
  (requested chunk included, as an immutable snapshot), plus &mut output C.
- NeighborChunks::get(coord) errors outside the plan; get_global(position)
  follows NeighborhoodView semantics (Ok(None) outside world, Err undeclared).
- Neighborhood pipelines require C: Clone (requested snapshot).
- Retained dependency chunk data is not counted by NeighborhoodLimits.
- Suffix steps are ChunkSteps and see the neighborhood's output C.
- Neighborhood::axes(x, y, z) gives per-axis radii (radius(r) = axes(r, r, r)).
  Planning, readable bounds and limits use the per-axis extents, so column
  steps (axes(1, 1, 0)) skip vertical dependency chunks. cell_radius()
  returns the largest axis radius.

Managed state:

- C is deterministic generated base output: never persisted, never part of
  replacement records, and not recomputed by cell edits. An edited chunk's C
  can disagree with its edited cells; consumers own any such derivation.

Also: DataSet<T> derives PartialEq/Eq.

## Current working consumer patterns

### Generation only

    app.add_plugins(
        LayerGenPlugin::new(pipeline)
            .with_limits(generation_limits)
    );

Use ChunkGenerator<T> directly and own the returned chunks yourself.

### Managed residency and persistence

    app.add_plugins(
        LayerGenPlugin::new(pipeline)
            .with_limits(generation_limits)
    )
    .add_plugins(
        ChunkManagerPlugin::new(identity, storage)
            .with_limits(manager_limits)
    );

Use ChunkManager<T> for loading, reading, editing, saving, unloading, and
completion consumption.

Do not also request or consume raw generator completions for the same T.

Reading generator preparation status is allowed.

## Validation status

The user reports passing tests and a clean strict Clippy run for the current
library implementation.

Coverage includes:

- Dense storage and spatial validation.
- Async generation and preparation.
- Bounded neighborhood correctness.
- Sparse edit/revert behavior.
- Conditional writes and concurrent write conflicts.
- Identity/configuration rejection.
- Partial edge chunks.
- Managed save/reload round trips.
- Edits after save capture remaining dirty.
- Invalidated loads not installing stale state.
- Operation/resident capacity.
- Save failures retaining dirty state.

The headless smoke test (`tests/smoke.rs`, no window) exercises:

1. Prepare one 4096-sample heightmap.
2. Load two adjacent 4096-voxel chunks.
3. Validate all generated cells.
4. Edit one cell and save.
5. Unload/reload and validate the restored replacement.
6. Revert to generated base and save.
7. Unload/reload and validate an empty replacement record at revision 2.

It uses MemoryChunkStorage and does not test process restarts. Restart
persistence is validated separately by the headless durable example.

An earlier windowed preparation smoke test completed in approximately 741 ms.
That included startup/frame/window scheduling and is NOT an algorithm benchmark.

Do not optimize from that number alone.

## Current limitations

Not implemented:

- A bundled durable backend (the filesystem backend is consumer-side example
  code and its format is not mandatory).
- Power-loss durability: the example syncs the temporary file before
  replacement but does not sync directory metadata.
- Multi-process writers to one storage directory without respecting the
  example's directory lock.
- Record migration.
- Automatic conflict merging.
- Automatic saving or eviction.
- Combined save-and-unload operation.
- Shutdown/drain/flush API or exit-time durability guarantee.
- Public cancellation API.
- Request prioritization.
- Multiple neighborhood stages.
- Cross-request dependency caching/deduplication.
- Parallel pipeline branches.
- Multiple named generators for the same voxel type.
- Prepared-data retry/replacement.
- Byte-level memory budgets.
- Entity persistence or cross-domain transactions.

Additional constraints:

- Generation definitions still return base data only.
- Chunk data is base-only and not updated by edits.
- Persistence applies only to requested managed output, not neighborhood inputs.
- Two dense datasets are retained per managed resident chunk.
- Completion queues must be consumed to release operation capacity.
- Dropping the app does not guarantee running writes never committed.
- Count limits do not bound arbitrary allocations inside T or callbacks.

## Completed milestone: durable persistence example

Implemented in `examples/support/file_storage.rs` (`FileChunkStorage`) and
`examples/durable.rs`, as consumer-side code. Its format and filesystem layout
are not mandatory for LayerGen users, and ChunkManager has no filesystem
knowledge: it builds on the existing ChunkStorage contract.

Purpose: validate the contract against real serialization, record replacement,
and application restarts.

### Writer scope

- One application process per storage directory, enforced by an exclusive
  directory lock held for the backend's lifetime.
- Revision comparison and record replacement are serialized within the process.
- Atomic file replacement alone does NOT make revision comparison plus writing
  atomic between competing writers. Other programs must respect the lock.
- Do not claim multi-process conditional-write safety beyond this lock, or
  power-loss durability.

### Decisions made

- Versioned JSON records (format version 1).
- Serialized record size limit of 4 MiB; it bounds serialized input, not
  arbitrary allocations or decoded memory.
- Identities are limited to 1–128 UTF-8 bytes and validated before mapping to
  storage locations.
- Successful writes synchronize the temporary file before replacement; directory
  metadata is not synchronized.
- Abandoned temporary files do not replace committed records. This is not
  comprehensive crash-injection coverage.

### Validation

- Headless restart sequence across separate processes: `save`, `verify`,
  `revert`, `verify-base` (see README).
- Embedded tests cover revisions surviving reopen, empty records retaining
  history, stale conditional writes, exclusive directory ownership, malformed
  records, configuration and generation mismatch, and abandoned temporary files.

Keep the in-memory backend for fast tests.

## Next milestone: save-and-unload and shutdown/drain

Add explicit lifecycle support after durable storage behavior is understood.

Potential scope:

- Save-and-unload workflow that remains safe if edits happen during saving.
- Observable pending-save/drain status.
- Explicit admission policy during shutdown.
- Failure reporting and retry decisions.
- No silent data loss.
- No assumption that dropping tasks undoes storage effects.

Exact API remains proposed, not implemented.

Avoid an API that promises a future flush without a running application/task
lifecycle to execute it.

## Later milestone: general staged neighborhoods

Implement only when game requirements justify it.

Potential scope:

- Multiple neighborhood stages.
- Backward dependency-footprint planning.
- Explicit stage identities.
- Stage-specific intermediate snapshots.
- Optional caching with correct generation identity.
- Dependency deduplication across requests.
- Memory-aware scheduling.

Remain explicit about limits.
Do not introduce unlimited recursive fetching.

Long-range algorithms should use shared regional/global data or explicit
regional operations rather than enormous implicit halos.

Edited-neighbor derived processing would require a separate revision-aware
dependency contract. It must not silently change deterministic base semantics.

## Later improvements, driven by game needs

- Queued-request cancellation.
- Cooperative cancellation checkpoints.
- Request prioritization.
- Automatic residency policy.
- Multiple named generators.
- Prepared-data regeneration with identity/revision changes.
- Profiling and execution timing.
- Better structured processing errors.
- Regional operations.
- Byte-budget-aware dispatch.
- Reduced managed baseline memory usage.
- Dedicated storage/I/O scheduling.

## Validation commands

    cargo fmt
    cargo test
    cargo test --no-default-features
    cargo test --all-features
    cargo test --all-features --example durable
    cargo clippy --all-targets --all-features -- -D warnings

Headless smoke test (also part of `cargo test`):

    cargo test --test smoke

Durable restart test (fresh directory; run in order, one process each):

    cargo run --features durable-example --example durable -- save ./target/durable-smoke-1
    cargo run --features durable-example --example durable -- verify ./target/durable-smoke-1
    cargo run --features durable-example --example durable -- revert ./target/durable-smoke-1
    cargo run --features durable-example --example durable -- verify-base ./target/durable-smoke-1

## Working rules for future changes

- Keep the smallest correct implementation as the default.
- Preserve existing working consumer APIs where practical.
- Mark breaking changes explicitly.
- Separate proposed APIs from implemented APIs.
- Supply full replacement files when requested.
- Keep tests for existing milestones.
- Add reference-based correctness tests before optimization.
- Do not claim compilation until validated locally.
- Distinguish in-memory persistence from durable persistence.
- Distinguish atomic replacement from atomic conditional writing.
- Preserve stale-load and stale-save protections.
- Keep game semantics out of the library.
