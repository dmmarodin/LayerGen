# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

LayerGen (`layergen-rs` package, `layergen` lib alias) is a **Bevy-specific, domain-agnostic** Rust library for dense 3D chunk generation, processing, editing, and persistence integration. Targets Bevy 0.20, native platforms only. Early development; APIs may change. It deliberately has no voxel types or game semantics, and no rendering, collision, entity, or inventory logic. Don't add those. `README.md` documents the public API; `ROADMAP.md` holds design principles, limitations, and planned milestones. The next milestone is save-and-unload plus shutdown/drain, which is still only proposed. The durable filesystem backend is consumer-side example code (`examples/`), not part of the library.

## Commands

```bash
cargo fmt
cargo test                                          # lib unit tests
cargo test --no-default-features                    # without rayon
cargo test --all-features
cargo test --all-features --example durable         # filesystem backend tests live in the example
cargo clippy --all-targets --all-features -- -D warnings
cargo test <name>                                   # single test, by name substring

cargo test --test smoke                             # one integration test file
cargo run --features durable-example --example durable -- save ./target/durable-smoke-1   # also: verify | revert | verify-base
```

CI (`.github/workflows/rust.yml`) only runs `cargo build` and `cargo test`; the other commands above are the project's own bar before finishing work.

The lib depends only on `bevy_app`, `bevy_ecs`, `bevy_tasks`; there is no full `bevy` dependency and no windowed binary. Keep it that way, and keep tests headless (plain `bevy_app::App` driven with `app.update()`).

## Architecture

Flat module set in `src/`, re-exported wholesale from `lib.rs`. The layers build on each other:

1. **`data_set.rs` / `spatial.rs`**: `DataSet<T>` is dense `Vec<T>` indexed `x + y*w + z*w*h` with checked dimensions. Global coords are signed `i64`; `ChunkCoord(u32,u32,u32)` is a nonnegative offset from the world origin. World bounds are min-inclusive/max-exclusive and are metadata only, never a world-sized allocation. Edge chunks are clipped smaller. `DataSet::neighbors` is dataset-local only, not cross-chunk.
2. **`step.rs` / `definition.rs`**: `Step<T>` and `SharedStep<T, D>` run sequentially inside one chunk job. `PipelineBuilder` builds a reusable definition without allocating anything; the initializer closure runs per generated chunk. `GenerationDefinition<T>` separates once-per-generator shared preparation from per-chunk execution.
2b. **Chunk data `C`**: consumer-defined per-chunk data (per-chunk values / depth-1 column `DataSet`), declared with `.chunk_data(|ctx, shared| C)` after `prepare`, before steps. `ChunkStep<T, C, D>` is the internal composition trait; `Step`/`SharedStep` enter via `LocalStep`/`SharedOnly` adapters. `C = ()` default everywhere (`Chunk`, `ChunkGenerator`, `ChunkManager`, `ManagedChunk`, plugins); `GenerationDefinition::ChunkData`. `ChunkManagerPlugin::new` is only for `C = ()`; otherwise `::<T, C>::with_chunk_data`. Base-only: never persisted, not recomputed by edits.
3. **`neighborhood.rs`**: exactly one bounded cross-chunk stage. `with_chunk_neighborhood` + `NeighborhoodChunkStep` also get `NeighborChunks<C>` (prefix chunk data of all planned chunks, requires `C: Clone`). `Neighborhood::axes(x, y, z)` = per-axis radius (`radius(r)` = symmetric); planning and readable bounds are per axis. Neighbor inputs are immutable *prefix-only* base data, generated inside the requesting task (no resident neighbors, no public neighbor requests). Undeclared reads are errors. Multiple stages are explicitly deferred.
4. **`runtime.rs`**: `LayerGenPlugin` and `ChunkGenerator<T>` run jobs on Bevy's task pool with in-flight/outstanding limits. Duplicate coordinates are rejected until their completion is consumed. Completion order is unspecified. `request_batch` currently requests one chunk.
5. **`persistence.rs` / `manager.rs`**: `ChunkManagerPlugin` and `ChunkManager<T>` own resident chunks and tracked edits. Persistence stores sparse latest-state *replacements* (not event history) behind the `ChunkStorage<T>` trait, with conditional writes against an expected revision. `MemoryChunkStorage` is the only bundled backend.

### Invariants to preserve

- Base generation must be deterministic and independent of request order, neighbors, and edits. Saved replacements are applied only after the complete base pipeline.
- Loads carry a **residency token**; stale async loads must not install. Save acks mark only the *captured* edit revision saved, so newer edits stay dirty. Reverting a cell to its generated value removes the replacement; empty replacement records are kept to preserve revision history.
- When `ChunkManager<T>` is installed, it exclusively owns the generator completion channel for that cell type. `ChunkManagerPlugin` must be added after `LayerGenPlugin`.
- Managed chunks never expose mutable data; raw `Chunk<T>` does.
- `unload` rejects dirty or saving chunks; `discard` is rejected while saving. No automatic save, eviction, or shutdown flush exists yet.
- Prefer explicit errors/limitations over silently approximating unsupported behavior.
- Persistence lookup key is world identity + chunk coordinate; `GenerationId` is deliberately *not* in the key, so incompatible records are rejected rather than hidden as absent. Consumers must bump `GenerationId` when algorithm changes invalidate replacements.
- Storage contract: `compare_exchange` is atomic on expected revision, revisions start at 1 and strictly increase, conflicts leave state untouched, and a backend error does **not** prove no write occurred. Atomic file replacement alone is *not* an atomic conditional write.
- Persistence applies only to the requested managed output chunk, never to neighborhood inputs. Neighborhood pipelines require `T: Clone`; managed cells require `Clone + PartialEq + Send + Sync + 'static` and must not use shared interior mutability (it bypasses edit tracking).

### Runtime details

- One generation definition per cell type per Bevy world; the plugin owns it. Scheduling: `PreUpdate` collects results (`LayerGenSet::Collect`, manager runs after it), `PostUpdate` dispatches jobs, game systems use `Update`.
- Shared preparation starts when the app runs, even with no requests, and has no retry. On failure, status is permanently `Failed`, queued requests resolve as failed completions, and new ones are rejected. Failed completions still occupy capacity until consumed.
- Limits are counts, not byte budgets. They don't bound allocations inside `T` or callbacks.
- Blocking storage calls currently run on `AsyncComputeTaskPool` workers; there's no dedicated I/O scheduling. Panics that unwind are reported; abort-mode panics are not.
- Synchronous `generate_chunk` / `prepare_shared` are diagnostic only; `prepare_shared` does not cache.

## Working rules (from ROADMAP.md)

- Smallest correct implementation by default; preserve existing consumer APIs where practical and mark breaking changes explicitly.
- Keep proposed APIs separate from implemented ones (docs and comments).
- Keep tests for existing milestones. Add reference-based correctness tests before any optimization. Don't optimize from the old ~741 ms smoke-test number; it came from a windowed run and includes window/frame startup.
- Distinguish in-memory from durable persistence, and atomic replacement from atomic conditional writing. Preserve the stale-load and stale-save protections.
- Don't claim something compiles or passes until it's validated locally.
- Never commit or push changes. The user does all git commits and pushes themselves; leave changes in the working tree.

### Examples

`examples/support/file_storage.rs` is a **consumer-side** JSON filesystem `ChunkStorage` (directory lock, temp-file replace), not a public library backend. `examples/durable.rs` is a headless restart test around it. No power-loss durability guarantee.

## Tests

Integration tests live in root `tests/` (async_runtime, chunk_data, manager, neighborhood, persistence, preparation, smoke) and use the public API via `use layergen_rs::*`. Unit tests are inline in `data_set.rs`, `spatial.rs`, `step.rs`, and `lib.rs`. Tests that exercise the manager or runtime need a Bevy `App` with the plugins installed; see existing tests for the pattern.
