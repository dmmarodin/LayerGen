//! Headless end-to-end managed persistence smoke test.
//!
//! Exercises shared preparation, base generation, sparse edits, save
//! acknowledgement, reload, and persisted reversion against two adjacent chunks
//! using `MemoryChunkStorage`. It does not test process restarts; the `durable`
//! example covers that.

use std::{
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use bevy_app::App;

use layergen_rs::*;

const TEST_CHUNK: ChunkCoord = ChunkCoord(1, 1, 0);
const SECOND_CHUNK: ChunkCoord = ChunkCoord(2, 1, 0);
const EDIT_POSITION: GlobalPosition = (16, 16, 0);
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Voxel {
    solid: bool,
}

impl Voxel {
    fn new(_position: GlobalPosition) -> Self {
        Self { solid: false }
    }
}

/// Consumer-defined shared processing data: a 2D heightmap stored with depth 1.
struct GenerationData {
    origin: GlobalPosition,
    heightmap: DataSet<i64>,
}

fn example_height(x: i64, y: i64, seed: u64) -> i64 {
    8 + (x + y + (seed % 7) as i64).rem_euclid(12)
}

fn prepare_generation_data(config: &PipelineConfig) -> Result<GenerationData, String> {
    let origin = config.world().origin();
    let (width, height, _) = config.world().dimensions();

    let width =
        usize::try_from(width).map_err(|_| "Heightmap width does not fit usize".to_owned())?;
    let height =
        usize::try_from(height).map_err(|_| "Heightmap height does not fit usize".to_owned())?;

    let heightmap = DataSet::new(width, height, 1, |(x, y, _)| {
        example_height(origin.0 + x as i64, origin.1 + y as i64, config.seed())
    })
    .map_err(str::to_owned)?;

    Ok(GenerationData { origin, heightmap })
}

struct TerrainStep;

impl SharedStep<Voxel, GenerationData> for TerrainStep {
    fn run(
        &self,
        data: &mut DataSet<Voxel>,
        context: &StepContext,
        shared: &GenerationData,
    ) -> PipelineStepResult {
        for (voxel, x, y, z) in data.iter_mut() {
            let global = context
                .global_position((x, y, z))
                .ok_or_else(|| "Invalid local voxel position".to_owned())?;

            let map_x = usize::try_from(global.0 - shared.origin.0)
                .map_err(|_| "Invalid heightmap X coordinate".to_owned())?;
            let map_y = usize::try_from(global.1 - shared.origin.1)
                .map_err(|_| "Invalid heightmap Y coordinate".to_owned())?;

            let surface = shared
                .heightmap
                .get(map_x, map_y, 0)
                .ok_or_else(|| "Missing heightmap sample".to_owned())?;

            voxel.solid = global.2 < *surface;
        }

        Ok(())
    }
}

fn build_app() -> App {
    let config = PipelineConfig::new(
        WorldBounds::new((0, 0, 0), (64, 64, 32)).unwrap(),
        ChunkSize::new(16, 16, 16).unwrap(),
        12345,
    );

    let pipeline = PipelineBuilder::new(config)
        .prepare(prepare_generation_data)
        .add_shared_step(TerrainStep)
        .build(Voxel::new);

    let storage = Arc::new(MemoryChunkStorage::<Voxel>::default());

    let identity = PersistenceIdentity {
        world: WorldId("layergen-smoke-world".to_owned()),
        generation: GenerationId("example-heightmap-v1".to_owned()),
    };

    let mut app = App::new();

    app.add_plugins(
        LayerGenPlugin::new(pipeline).with_limits(GenerationLimits::new(2, 8).unwrap()),
    )
    .add_plugins(
        ChunkManagerPlugin::new(identity, storage)
            .with_limits(ChunkManagerLimits::new(8, 2, 4).unwrap()),
    );

    app
}

fn manager(app: &App) -> &ChunkManager<Voxel> {
    app.world().resource::<ChunkManager<Voxel>>()
}

fn with_manager<R>(app: &mut App, f: impl FnOnce(&mut ChunkManager<Voxel>) -> R) -> R {
    f(&mut app.world_mut().resource_mut::<ChunkManager<Voxel>>())
}

fn generator(app: &App) -> &ChunkGenerator<Voxel> {
    app.world().resource::<ChunkGenerator<Voxel>>()
}

/// Runs the app until every request has completed, returning outcomes in
/// completion order. Only the manager's completion channel is consumed.
fn wait_for(
    app: &mut App,
    mut requests: Vec<(ManagedRequestId, ChunkCoord)>,
) -> Vec<(ChunkCoord, ManagedOutcome)> {
    let deadline = Instant::now() + TIMEOUT;
    let mut outcomes = Vec::new();

    while !requests.is_empty() {
        app.update();

        if let PreparationStatus::Failed(error) = generator(app).preparation_status() {
            panic!("Preparation failed: {error}");
        }

        let m = manager(app);
        assert!(m.outstanding_count() <= m.limits().max_outstanding());
        assert!(m.storage_jobs_count() <= m.limits().max_storage_jobs());

        while let Some(completion) = with_manager(app, |m| m.take_completed()) {
            let index = requests
                .iter()
                .position(|entry| *entry == (completion.request_id, completion.coord))
                .expect("Unexpected or duplicate managed completion");

            requests.swap_remove(index);

            let outcome = completion.result.unwrap_or_else(|error| {
                panic!(
                    "Operation {} for {:?} failed: {}",
                    completion.request_id.value(),
                    completion.coord,
                    error,
                )
            });

            outcomes.push((completion.coord, outcome));
        }

        assert!(Instant::now() < deadline, "Smoke test timed out");
        thread::sleep(Duration::from_millis(1));
    }

    outcomes
}

fn load(app: &mut App, coords: &[ChunkCoord]) {
    let requests = coords
        .iter()
        .map(|&coord| {
            let id = with_manager(app, |m| m.request_load(coord))
                .expect("Load request should be accepted");
            (id, coord)
        })
        .collect();

    for (coord, outcome) in wait_for(app, requests) {
        assert_eq!(
            outcome,
            ManagedOutcome::Loaded { installed: true },
            "Load of {coord:?}",
        );
    }
}

fn save(app: &mut App, coord: ChunkCoord, expected_storage_revision: u64) {
    let id = with_manager(app, |m| m.request_save(coord)).expect("Save should be accepted");

    let outcomes = wait_for(app, vec![(id, coord)]);
    assert_eq!(outcomes.len(), 1);

    match &outcomes[0].1 {
        ManagedOutcome::Saved {
            edit_revision,
            storage_revision,
        } => {
            // Edit revisions belong to each residency; each save here follows
            // exactly one edit in its residency.
            assert_eq!(*edit_revision, 1);
            assert_eq!(storage_revision.value(), expected_storage_revision);
        }
        other => panic!("Expected save completion, got {other:?}"),
    }
}

/// Validates every generated base cell and every current cell.
///
/// When `replacement` is present, exactly the cell at `EDIT_POSITION` differs
/// from the generated base.
fn validate_chunk(manager: &ChunkManager<Voxel>, coord: ChunkCoord, replacement: Option<&Voxel>) {
    let chunk = manager
        .chunk(coord)
        .expect("Validated chunk must be resident");

    let context = chunk.context();
    let seed = context.config().seed();

    assert_eq!(chunk.data().dimensions(), (16, 16, 16));
    assert!(!chunk.is_dirty());
    assert_eq!(
        chunk.replacements().len(),
        usize::from(replacement.is_some())
    );

    for (voxel, x, y, z) in chunk.data().iter() {
        let global = context.global_position((x, y, z)).unwrap();

        let expected_base = Voxel {
            solid: global.2 < example_height(global.0, global.1, seed),
        };

        assert_eq!(
            chunk.base_data().get(x, y, z),
            Some(&expected_base),
            "Generated base mismatch at {global:?}",
        );

        let expected_current = if global == EDIT_POSITION {
            replacement.unwrap_or(&expected_base)
        } else {
            &expected_base
        };

        assert_eq!(
            voxel, expected_current,
            "Resident value mismatch at {global:?}"
        );
    }

    if let Some(replacement) = replacement {
        let (located_coord, local) = manager.config().locate(EDIT_POSITION).unwrap();
        assert_eq!(located_coord, coord);

        let index = chunk
            .data()
            .linear_index(local.0, local.1, local.2)
            .unwrap();
        assert_eq!(chunk.replacements().get(&index), Some(replacement));
    }
}

#[test]
fn managed_persistence_round_trip_with_shared_preparation() {
    let mut app = build_app();

    // Initial load: shared preparation, base generation, no stored records.
    load(&mut app, &[TEST_CHUNK, SECOND_CHUNK]);

    assert_eq!(manager(&app).resident_count(), 2);
    assert_eq!(manager(&app).outstanding_count(), 0);
    assert_eq!(
        generator(&app).preparation_status(),
        &PreparationStatus::Ready
    );
    assert_eq!(generator(&app).outstanding_count(), 0);

    validate_chunk(manager(&app), TEST_CHUNK, None);
    validate_chunk(manager(&app), SECOND_CHUNK, None);
    assert_eq!(
        manager(&app).chunk(TEST_CHUNK).unwrap().storage_revision(),
        None
    );

    // Edit one cell away from its generated value and save it.
    let base = manager(&app)
        .get_global(EDIT_POSITION)
        .expect("Edit target must be resident")
        .clone();
    let edited = Voxel { solid: !base.solid };

    assert!(with_manager(&mut app, |m| m.set_global(EDIT_POSITION, edited.clone())).unwrap());

    let chunk = manager(&app).chunk(TEST_CHUNK).unwrap();
    assert!(chunk.is_dirty());
    assert_eq!(chunk.edit_revision(), 1);
    assert_eq!(chunk.replacements().len(), 1);

    save(&mut app, TEST_CHUNK, 1);

    assert!(!manager(&app).chunk(TEST_CHUNK).unwrap().is_dirty());
    assert!(!manager(&app).is_saving(TEST_CHUNK));

    // Unload and reload: the saved replacement is restored over the base.
    with_manager(&mut app, |m| m.unload(TEST_CHUNK)).unwrap();
    assert!(manager(&app).chunk(TEST_CHUNK).is_none());
    assert!(manager(&app).get_global(EDIT_POSITION).is_none());

    load(&mut app, &[TEST_CHUNK]);
    validate_chunk(manager(&app), TEST_CHUNK, Some(&edited));

    let chunk = manager(&app).chunk(TEST_CHUNK).unwrap();
    assert_eq!(chunk.storage_revision().unwrap().value(), 1);
    // A new residency starts its local edit revision at zero.
    assert_eq!(chunk.edit_revision(), 0);

    // Revert to the generated value: the replacement disappears but the chunk
    // is still dirty until the save is acknowledged.
    assert!(with_manager(&mut app, |m| m.set_global(EDIT_POSITION, base.clone())).unwrap());

    let chunk = manager(&app).chunk(TEST_CHUNK).unwrap();
    assert!(chunk.is_dirty());
    assert!(chunk.replacements().is_empty());
    assert_eq!(chunk.edit_revision(), 1);

    save(&mut app, TEST_CHUNK, 2);

    let chunk = manager(&app).chunk(TEST_CHUNK).unwrap();
    assert!(!chunk.is_dirty());
    assert!(chunk.replacements().is_empty());

    // Final reload: base restored, empty record retains its storage revision.
    with_manager(&mut app, |m| m.unload(TEST_CHUNK)).unwrap();
    load(&mut app, &[TEST_CHUNK]);

    validate_chunk(manager(&app), TEST_CHUNK, None);
    validate_chunk(manager(&app), SECOND_CHUNK, None);
    assert_eq!(manager(&app).get_global(EDIT_POSITION), Some(&base));
    assert_eq!(
        manager(&app)
            .chunk(TEST_CHUNK)
            .unwrap()
            .storage_revision()
            .unwrap()
            .value(),
        2,
    );

    assert_eq!(manager(&app).resident_count(), 2);
    assert_eq!(manager(&app).pending_count(), 0);
    assert_eq!(manager(&app).completed_count(), 0);
    assert_eq!(generator(&app).outstanding_count(), 0);
    assert_eq!(
        generator(&app).preparation_status(),
        &PreparationStatus::Ready
    );
}
