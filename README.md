# LayerGen

A **Bevy-specific, domain-agnostic Rust library** for dense 3D chunk generation,
processing, editing, and persistence integration.

LayerGen was created for procedural terrain, but does not prescribe voxel
types or game semantics. Consumers define their cell data, processing rules,
shared generation data, and storage backend.

World dimensions are metadata—not a world-sized allocation. Generate explicit
aligned chunks, with edge chunks clipped to world bounds.

**Status:** early development; APIs may change. Targets Bevy 0.20 and native
asynchronous execution.

## Features

- Dense `DataSet<T>` with checked dimensions and coordinate-aware iteration.
- Signed global coordinates and aligned chunk addressing.
- Reusable pipelines with sequential processing steps.
- Background generation through Bevy's task pool.
- Once-per-generator preparation of consumer-defined shared data.
- One bounded cross-chunk neighborhood stage.
- Per-chunk consumer data (per-chunk or per-column values) alongside cells.
- Library-owned resident chunk management and tracked editing.
- Sparse latest-state replacements.
- Conditional storage writes and revision-safe save acknowledgements.
- Optional Rayon and Serde support.

LayerGen does **not** provide entity simulation, terrain rendering, collision
rules, or inventory management.

## Installation

LayerGen is not yet published on crates.io.

Add the Git dependency using the repository's package name:

```toml
[dependencies]
layergen = {
    package = "layergen-rs",
    git = "https://github.com/dmmarodin/LayerGen.git",
    branch = "main"
}
```

The examples below use `layergen` as the dependency alias.

For application integration, use the matching Bevy version:

```toml
bevy = "0.20"
```

### Features

| Feature | Default | Purpose |
|---|---|---|
| `rayon` | Enabled | Parallel dataset iteration |
| `serde` | Disabled | Dataset serialization and validated deserialization |
| `durable-example` | Disabled | Consumer-side JSON filesystem backend and headless restart test |

Disable Rayon if it is not needed:

```toml
layergen = {
    package = "layergen-rs",
    git = "https://github.com/dmmarodin/LayerGen.git",
    branch = "main",
    default-features = false
}
```

The `serde` feature does not provide a durable storage backend.

## Basic generation

Define your cell type and processing steps. A step receives both its dense
dataset and a spatial context.

```rust
use layergen::{
    ChunkCoord, ChunkSize, DataSet, PipelineBuilder, PipelineConfig,
    PipelineStepResult, Step, StepContext, WorldBounds,
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Voxel {
    solid: bool,
}

struct TerrainStep;

impl Step<Voxel> for TerrainStep {
    fn run(
        &self,
        data: &mut DataSet<Voxel>,
        context: &StepContext,
    ) -> PipelineStepResult {
        for (voxel, x, y, z) in data.iter_mut() {
            let global = context
                .global_position((x, y, z))
                .ok_or_else(|| "Invalid local position".to_owned())?;

            // This example uses the Z axis as elevation.
            voxel.solid = global.2 < 12;
        }

        Ok(())
    }
}

fn main() -> Result<(), String> {
    let config = PipelineConfig::new(
        WorldBounds::new((-32, -32, 0), (64, 64, 32))?,
        ChunkSize::new(16, 16, 16)?,
        12345,
    );

    let pipeline = PipelineBuilder::new(config)
        .add_step(TerrainStep)
        .build(|_global| Voxel { solid: false });

    let chunk = pipeline.generate_chunk(ChunkCoord(0, 0, 0))?;

    assert_eq!(chunk.origin(), (-32, -32, 0));
    assert_eq!(chunk.data().dimensions(), (16, 16, 16));

    Ok(())
}
```

Building a pipeline does not initialize cells or allocate chunk datasets.
The initializer runs when a chunk is generated and receives global coordinates.

`generate_chunk` is synchronous diagnostic execution. For generation during
gameplay, use the asynchronous Bevy runtime.

### Spatial contract

- Global coordinates are signed `(i64, i64, i64)`.
- Chunk coordinates are nonnegative offsets relative to the world origin.
- World bounds are minimum-inclusive and maximum-exclusive.
- Chunk requests are aligned, not arbitrary overlapping regions.
- Edge chunks contain only valid world cells.

Use `PipelineConfig::locate` to convert a global position into a chunk
coordinate and local position.

## Asynchronous Bevy generation

Install one generation definition per cell type:

```rust
use bevy::prelude::*;
use layergen::{
    ChunkCoord, ChunkGenerator, GenerationLimits, LayerGenPlugin,
};

fn install_generation(app: &mut App) {
    // `pipeline` is the definition assembled in the previous example.
    // app.add_plugins(
    //     LayerGenPlugin::new(pipeline)
    //         .with_limits(GenerationLimits::new(2, 8).unwrap())
    // );

    app.add_systems(Startup, request_chunk);
    app.add_systems(Update, receive_chunks);
}

fn request_chunk(mut generator: ResMut<ChunkGenerator<Voxel>>) {
    generator
        .request_batch(ChunkCoord(0, 0, 0))
        .expect("Chunk request should be accepted");
}

fn receive_chunks(mut generator: ResMut<ChunkGenerator<Voxel>>) {
    while let Some(completion) = generator.take_completed() {
        match completion.result {
            Ok(chunk) => {
                // Install or retain this owned chunk in your application.
                info!("Generated {:?}: {} cells", chunk.coord(), chunk.data().len());
            }
            Err(error) => error!("Generation failed: {error}"),
        }
    }
}
```

The systems above use the `Voxel` type from the basic example. Install the
pipeline before running them.

Despite its name, `request_batch` currently requests **one chunk**.

Generation limits bound in-flight jobs and outstanding requests. Outstanding
requests include queued work, running work, and unconsumed completions.
Consume completions to release capacity.

Duplicate coordinates are rejected until their previous completion is consumed.
Completion order is unspecified.

## Shared preparation

Prepare reusable consumer-defined data once per installed generator:

```rust
let pipeline = PipelineBuilder::new(config)
    .prepare(|config| {
        // Build a heightmap, lookup table, or other immutable shared data.
        create_shared_data(config)
    })
    .add_shared_step(TerrainStep)
    .build(Voxel::new);
```

This is an API sketch: `create_shared_data`, `TerrainStep`, and `Voxel::new`
are supplied by the consumer. The step must implement `SharedStep<T, D>`.

Preparation:

- Runs in the background once per installed generator.
- Allows requests to queue before readiness.
- Shares its result immutably among chunk jobs.
- Reports failures through preparation status and request completions.
- Has no retry or replacement mechanism yet.

Direct `prepare_shared()` calls are diagnostic and do not cache preparation.

## Per-chunk data

Values coarser than cells, such as one value per chunk or per column, can
live in a consumer-defined chunk data type instead of being repeated in every
cell:

```rust
#[derive(Clone)]
struct Climate {
    // One sample per (x, y) column: a depth-1 dataset.
    temperature: DataSet<f32>,
}

struct SampleClimate;

impl ChunkStep<Voxel, Climate, Tectonics> for SampleClimate {
    fn run(
        &self,
        _cells: &mut DataSet<Voxel>,
        climate: &mut Climate,
        context: &StepContext,
        tectonics: &Tectonics,
    ) -> PipelineStepResult {
        let origin = context.origin();

        for (value, x, y, _) in climate.temperature.iter_mut() {
            *value = tectonics.temperature_at(origin.0 + x as i64, origin.1 + y as i64);
        }

        Ok(())
    }
}

let pipeline = PipelineBuilder::new(config)
    .prepare(|config| Ok(Tectonics::generate(config.seed())))
    .chunk_data(|context, _tectonics| {
        let (width, height, _) = context.dimensions();
        Climate {
            temperature: DataSet::new(width, height, 1, |_| 0.0).unwrap(),
        }
    })
    .add_chunk_step(SampleClimate)
    .add_step(TerrainStep)
    .build(Voxel::new);

let chunk = pipeline.generate_chunk_with_shared(coord, &tectonics)?;
let climate: &Climate = chunk.chunk_data();
```

This is an API sketch: `Tectonics`, `Voxel`, and `TerrainStep` are supplied
by the consumer.

- Call `chunk_data` after `prepare` and before adding steps.
- The initializer runs once per generated chunk and may read shared data.
- `Step` and `SharedStep` implementations can be mixed in; they do not see
  chunk data.
- Chunk data defaults to `()`; existing pipelines are unaffected.
- Resources carry the type: `ChunkGenerator<Voxel, Climate>`,
  `ChunkManager<Voxel, Climate>`.
- Install the manager with
  `ChunkManagerPlugin::<Voxel, Climate>::with_chunk_data(identity, storage)`.
- Chunk data is generated base output. It is not persisted and is not
  recomputed when managed cells are edited.

## Bounded neighborhood processing

A regular pipeline can be wrapped with one neighborhood stage:

```rust
let pipeline = prefix
    .with_neighborhood(
        Neighborhood::radius(1),
        NeighborhoodLimits::default(),
        SmoothStep,
    )
    .add_step(FinalizeStep);
```

This is an API sketch. The consumer supplies a `NeighborhoodStep` and optional
local/shared suffix steps.

`Neighborhood::radius(r)` is symmetric on all axes. Use
`Neighborhood::axes(x, y, z)` when a step only reads along some axes (for
example `axes(1, 1, 0)` for column-wise steps): dependency chunks are planned
only along axes with a nonzero radius, and reads along a zero-radius axis are
undeclared errors.

Neighborhood inputs are immutable **prefix-only base data**. Dependencies are
generated inside the existing request task, without requiring resident
neighbors or issuing public neighbor requests.

- Output starts from the requested chunk's prefix result.
- The neighborhood step writes only that output chunk.
- Declared out-of-world reads return `Ok(None)`.
- Undeclared reads and missing internal inputs are errors.
- Limits bound dependency count, prefix work, and temporary dataset cells.
- Neighborhood pipelines require `T: Clone`.

Use `with_chunk_neighborhood` with a `NeighborhoodChunkStep` to also read the
prefix chunk data of every planned chunk through `NeighborChunks` (same
`Ok(None)` / error contract as cells) and write the output chunk data. This
requires the chunk data type to be `Clone`.

Multiple neighborhood stages and cross-request dependency caching are not yet
implemented.

`DataSet::neighbors` is separate: it provides only dataset-local face neighbors.

## Managed chunks, editing, and persistence

Use `ChunkManager<T>` when LayerGen should own resident chunks and track edits.

Install it **after** the generation plugin:

```rust
use std::sync::Arc;

use layergen::{
    ChunkManagerLimits, ChunkManagerPlugin, GenerationId,
    MemoryChunkStorage, PersistenceIdentity, WorldId,
};

let storage = Arc::new(MemoryChunkStorage::<Voxel>::default());

let identity = PersistenceIdentity {
    world: WorldId("my-world".to_owned()),
    generation: GenerationId("terrain-v1".to_owned()),
};

app.add_plugins(LayerGenPlugin::new(pipeline))
    .add_plugins(
        ChunkManagerPlugin::new(identity, storage)
            .with_limits(ChunkManagerLimits::default()),
    );
```

This installation snippet assumes an existing `app`, `pipeline`, and `Voxel`.

Managed cell types require:

```text
Clone + PartialEq + Send + Sync + 'static
```

### Game-facing operations

```rust
// These operations run inside a system with ResMut<ChunkManager<Voxel>>.

// Request a chunk; installation happens asynchronously.
manager.request_load(coord)?;

// Query resident data. None means outside-world or not resident.
let cell = manager.get_global(position);

// Track an edit after the chunk is resident.
manager.set_global(position, replacement)?;

// Capture a snapshot and queue a background save.
manager.request_save(coord)?;

// Inspect results and release operation capacity.
while let Some(completion) = manager.take_completed() {
    // Handle completion.result.
}

// Unload only after the chunk is clean and no save is pending.
manager.unload(coord)?;
```

These illustrate separate lifecycle operations, not a sequence that can all
complete synchronously in one frame.

**When a manager is installed, do not also request or consume raw generator
completions for that cell type.** The manager exclusively owns that channel.

### Sparse replacements and revisions

Loading follows:

```text
Generate complete deterministic base
    -> load and validate saved replacements
    -> apply replacements
    -> install only if the residency token is current
```

Editing a cell away from its generated value inserts a replacement.
Reverting it to the generated value removes that replacement.

Saving captures an owned snapshot:

- Only one save may be queued/running per chunk.
- Editing can continue during saving.
- Acknowledgement marks only the captured edit revision saved.
- Newer edits remain dirty.
- Storage writes conditionally check the expected persistent revision.
- Conflicts and failures are reported, not silently merged.

Empty replacement records preserve storage revision history.

Managed chunks retain generated base and current data—roughly two dense
datasets per resident chunk, plus sparse replacements.

### Unload and discard

- `unload`: rejects dirty chunks and chunks with a queued/running save.
- `discard`: explicitly drops unsaved edits or invalidates a pending load.
- `discard` does not undo saved data or stop already-running tasks.
- `discard` is rejected while saving.

There is no automatic saving, eviction, or shutdown flush.

### Storage backends

Implement `ChunkStorage<T>` to integrate your own files, database, or other
storage.

The backend must provide consistent loads and atomic conditional writes.
A simple read-then-write sequence is not sufficient for competing writers.

Two backends are available:

- **`MemoryChunkStorage`**: library-provided test backend. Does not survive
  application exit.
- **`FileChunkStorage`**: consumer-side example in
  `examples/support/file_storage.rs`. Uses versioned JSON records, an exclusive
  storage-directory lock, serialized conditional writes, and temporary-file
  replacement.

The filesystem example is not a public library backend or a mandatory storage
format. It validates persistence across normal process exit/restart.

Successful example writes synchronize the temporary file before replacement,
but do not synchronize directory metadata. **Power-loss durability is not
guaranteed.** Other programs must respect the storage-directory lock.

Saved records validate world/generation identities, pipeline configuration,
dimensions, and replacement indices. Change your generation identity when
algorithm changes make previous replacements incompatible.

Optional Serde support does not automatically serialize persistence records;
the filesystem example defines its own wire format.

## Game-state boundaries

LayerGen stores cells, not entities or inventories.

NPCs can be simulated independently of terrain residency. Furniture, inventory,
collision, and placement rules remain consumer-owned.

A game can combine terrain queries with entity occupancy, but updating a voxel
marker and a separate entity record is not automatically one transaction.

## Performance and limitations

- Chunk steps execute sequentially within each job.
- Independent chunk jobs execute through Bevy's task pool.
- Optional Rayon iteration can parallelize work inside a step.
- Neighborhood dependencies currently execute sequentially within the job.
- Count/cell limits are not complete byte-level memory budgets.
- Save snapshot cloning happens when a save is requested.
- Blocking storage methods currently run on background compute-pool tasks.
- Determinism depends on consumer algorithms and stable inputs.
- Native asynchronous execution is supported; nonblocking WASM execution is
  not guaranteed.

No hard real-time or frame-budget guarantee is provided.

See [roadmap.md](roadmap.md) for implemented contracts and remaining milestones.

## Testing

### Headless in-memory smoke test

The end-to-end test lives in `tests/smoke.rs` and needs no window or extra
features:

```bash
cargo test --test smoke
```

It prepares a heightmap, validates every cell of two adjacent chunks, and
exercises:

```text
load -> edit -> save -> unload -> reload
     -> revert -> save -> unload -> reload
```

This validates in-memory persistence across chunk reloads, not application
restarts.

### Durable persistence restart test

The headless `durable` example uses the consumer-side filesystem backend.
Each command below runs in a separate process.

Use a **fresh storage directory** and run the commands in order:

```bash
cargo run --features durable-example --example durable -- save ./target/durable-smoke-1
cargo run --features durable-example --example durable -- verify ./target/durable-smoke-1
cargo run --features durable-example --example durable -- revert ./target/durable-smoke-1
cargo run --features durable-example --example durable -- verify-base ./target/durable-smoke-1
```

| Command | Checks |
|---|---|
| `save` | Fresh base state; saves one replacement at revision 1 |
| `verify` | Restores the replacement after restart at revision 1 |
| `revert` | Removes the replacement and saves an empty record at revision 2 |
| `verify-base` | Restores base state after restart while retaining revision 2 |

Expected output:

```text
SAVE PASSED: replacement committed at revision 1
RESTART VERIFY PASSED: replacement restored at revision 1
REVERT PASSED: empty replacement record committed at revision 2
RESTART VERIFY-BASE PASSED: base restored at revision 2
```

Each process waits for its managed operation to complete before exiting.

The `save` command intentionally requires an absent record. To repeat the full
sequence, choose another fresh directory, such as
`./target/durable-smoke-2`.

Do not run multiple commands concurrently against the same directory. The
backend holds an exclusive directory lock for its lifetime.

These checks validate normal-restart persistence, **not power-loss recovery**.
The `target` directory is suitable for test data, but `cargo clean` removes it;
use a separate location for data you want to retain.

### Filesystem backend tests

Run the tests embedded in the durable example explicitly:

```bash
cargo test --all-features --example durable
```

Coverage includes:

- State and revisions surviving backend reopen.
- Empty replacement records retaining revision history.
- Stale conditional writes leaving stored state unchanged.
- Exclusive directory ownership.
- Malformed-record rejection.
- Configuration mismatch rejection.
- Generation identity mismatch rejection on replacement.
- Abandoned temporary files not replacing committed records.

The abandoned-file test is not comprehensive crash-injection coverage.

## Development checks

```bash
cargo fmt
cargo test
cargo test --no-default-features
cargo test --all-features
cargo test --all-features --example durable
cargo clippy --all-targets --all-features -- -D warnings
```

The explicit example-test command ensures the filesystem backend tests are run
alongside the library and integration tests.

## License

Licensed under Apache 2.0. See [LICENSE](LICENSE) for details.
