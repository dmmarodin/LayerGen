use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use bevy::prelude::*;

use layergen_rs::{
    ChunkCoord, ChunkGenerator, ChunkManager, ChunkManagerLimits, ChunkManagerPlugin, ChunkSize,
    DataSet, GenerationId, GenerationLimits, GlobalPosition, LayerGenPlugin, ManagedOutcome,
    ManagedRequestId, MemoryChunkStorage, PersistenceIdentity, PipelineBuilder, PipelineConfig,
    PipelineStepResult, PreparationStatus, SharedStep, StepContext, WorldBounds, WorldId,
};

const TEST_CHUNK: ChunkCoord = ChunkCoord(1, 1, 0);
const SECOND_CHUNK: ChunkCoord = ChunkCoord(2, 1, 0);
const EDIT_POSITION: GlobalPosition = (16, 16, 0);

fn main() {
    let config = PipelineConfig::new(
        WorldBounds::new((0, 0, 0), (64, 64, 32)).expect("Example world bounds must be valid"),
        ChunkSize::new(16, 16, 16).expect("Example chunk dimensions must be valid"),
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

    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "LayerGen — managed persistence smoke test".into(),
                resolution: (960, 540).into(),
                ..default()
            }),
            ..default()
        }))
        .add_plugins(
            LayerGenPlugin::new(pipeline).with_limits(GenerationLimits::new(2, 8).unwrap()),
        )
        .add_plugins(
            ChunkManagerPlugin::new(identity, storage)
                .with_limits(ChunkManagerLimits::new(8, 2, 4).unwrap()),
        )
        .insert_resource(ClearColor(Color::srgb(0.04, 0.05, 0.08)))
        .add_systems(Startup, (setup_camera, start_smoke_test))
        .add_systems(Update, advance_smoke_test)
        .run();
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Voxel {
    solid: bool,
}

impl Voxel {
    fn new(_position: GlobalPosition) -> Self {
        Self { solid: false }
    }
}

/// Consumer-defined shared processing data.
///
/// This small example represents a 2D heightmap using depth 1.
struct GenerationData {
    origin: GlobalPosition,
    heightmap: DataSet<i64>,
}

fn example_height(x: i64, y: i64, seed: u64) -> i64 {
    8 + (x + y + (seed % 7) as i64).rem_euclid(12)
}

fn prepare_generation_data(config: &PipelineConfig) -> Result<GenerationData, String> {
    info!("Preparing the example heightmap");

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

    info!("Prepared {} heightmap samples", heightmap.len());

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SmokePhase {
    LoadingInitial,
    SavingEdit,
    LoadingEdited,
    SavingRevert,
    LoadingReverted,
    Finished,
}

#[derive(Resource)]
struct SmokeTest {
    phase: SmokePhase,
    requests: Vec<(ManagedRequestId, ChunkCoord)>,
    started: Instant,
    base_value: Option<Voxel>,
    edited_value: Option<Voxel>,
}

fn setup_camera(mut commands: Commands) {
    commands.spawn(Camera2d);
}

fn start_smoke_test(mut commands: Commands, mut manager: ResMut<ChunkManager<Voxel>>) {
    let mut requests = Vec::new();

    for coord in [TEST_CHUNK, SECOND_CHUNK] {
        let id = manager
            .request_load(coord)
            .expect("Initial smoke-test load should be accepted");

        info!("Queued managed load {} for {:?}", id.value(), coord);
        requests.push((id, coord));
    }

    commands.insert_resource(SmokeTest {
        phase: SmokePhase::LoadingInitial,
        requests,
        started: Instant::now(),
        base_value: None,
        edited_value: None,
    });
}

/// Validates every generated base cell and every current cell.
///
/// During edited reload, exactly one current cell differs from base.
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
        usize::from(replacement.is_some()),
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
            "Resident value mismatch at {global:?}",
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

    info!(
        "Validated chunk {:?}: {} cells, {} replacements",
        coord,
        chunk.data().len(),
        chunk.replacements().len(),
    );
}

fn advance_smoke_test(
    generation: Res<ChunkGenerator<Voxel>>,
    mut manager: ResMut<ChunkManager<Voxel>>,
    mut smoke: ResMut<SmokeTest>,
) {
    if smoke.phase == SmokePhase::Finished {
        return;
    }

    if let PreparationStatus::Failed(error) = generation.preparation_status() {
        panic!("Smoke-test preparation failed: {error}");
    }

    assert!(
        smoke.started.elapsed() < Duration::from_secs(30),
        "LayerGen persistence smoke test timed out in {:?}",
        smoke.phase,
    );

    assert!(manager.outstanding_count() <= manager.limits().max_outstanding());
    assert!(manager.storage_jobs_count() <= manager.limits().max_storage_jobs());

    // Only the manager completion channel is consumed here.
    // Reading the generator's preparation status does not consume its channel.
    while let Some(completion) = manager.take_completed() {
        let index = smoke
            .requests
            .iter()
            .position(|entry| *entry == (completion.request_id, completion.coord))
            .expect("Unexpected or duplicate managed completion");

        smoke.requests.swap_remove(index);

        let outcome = completion.result.unwrap_or_else(|error| {
            panic!(
                "Smoke-test operation {} for {:?} failed during {:?}: {}",
                completion.request_id.value(),
                completion.coord,
                smoke.phase,
                error,
            );
        });

        match smoke.phase {
            SmokePhase::LoadingInitial
            | SmokePhase::LoadingEdited
            | SmokePhase::LoadingReverted => {
                assert_eq!(outcome, ManagedOutcome::Loaded { installed: true },);
            }
            SmokePhase::SavingEdit | SmokePhase::SavingRevert => {
                let expected_revision = if smoke.phase == SmokePhase::SavingEdit {
                    1
                } else {
                    2
                };

                match outcome {
                    ManagedOutcome::Saved {
                        edit_revision,
                        storage_revision,
                    } => {
                        // Edit revisions belong to each residency. Each save
                        // here follows one edit in its respective residency.
                        assert_eq!(edit_revision, 1);
                        assert_eq!(storage_revision.value(), expected_revision,);
                    }
                    other => panic!("Expected save completion, got {other:?}"),
                }
            }
            SmokePhase::Finished => unreachable!(),
        }

        info!(
            "Completed managed operation {} for {:?} during {:?}",
            completion.request_id.value(),
            completion.coord,
            smoke.phase,
        );
    }

    if !smoke.requests.is_empty() {
        return;
    }

    assert_eq!(manager.outstanding_count(), 0);

    match smoke.phase {
        SmokePhase::LoadingInitial => {
            assert_eq!(manager.resident_count(), 2);
            assert_eq!(generation.preparation_status(), &PreparationStatus::Ready,);
            assert_eq!(generation.outstanding_count(), 0);

            validate_chunk(&manager, TEST_CHUNK, None);
            validate_chunk(&manager, SECOND_CHUNK, None);

            assert_eq!(manager.chunk(TEST_CHUNK).unwrap().storage_revision(), None,);

            let base = manager
                .get_global(EDIT_POSITION)
                .expect("Edit target must be resident")
                .clone();

            let edited = Voxel { solid: !base.solid };

            assert!(manager.set_global(EDIT_POSITION, edited.clone()).unwrap());

            let chunk = manager.chunk(TEST_CHUNK).unwrap();
            assert!(chunk.is_dirty());
            assert_eq!(chunk.edit_revision(), 1);
            assert_eq!(chunk.replacements().len(), 1);

            smoke.base_value = Some(base);
            smoke.edited_value = Some(edited);

            let id = manager.request_save(TEST_CHUNK).unwrap();
            smoke.requests.push((id, TEST_CHUNK));
            smoke.phase = SmokePhase::SavingEdit;

            info!("Edited {:?}; queued first save", EDIT_POSITION);
        }
        SmokePhase::SavingEdit => {
            assert!(!manager.chunk(TEST_CHUNK).unwrap().is_dirty());
            assert!(!manager.is_saving(TEST_CHUNK));

            manager.unload(TEST_CHUNK).unwrap();
            assert!(manager.chunk(TEST_CHUNK).is_none());
            assert!(manager.get_global(EDIT_POSITION).is_none());

            let id = manager.request_load(TEST_CHUNK).unwrap();
            smoke.requests.push((id, TEST_CHUNK));
            smoke.phase = SmokePhase::LoadingEdited;

            info!("First save acknowledged; unloaded and requested reload");
        }
        SmokePhase::LoadingEdited => {
            validate_chunk(&manager, TEST_CHUNK, smoke.edited_value.as_ref());

            assert_eq!(
                manager
                    .chunk(TEST_CHUNK)
                    .unwrap()
                    .storage_revision()
                    .unwrap()
                    .value(),
                1,
            );

            // A newly loaded residency starts its local edit revision at zero.
            assert_eq!(manager.chunk(TEST_CHUNK).unwrap().edit_revision(), 0,);

            let base = smoke.base_value.as_ref().unwrap().clone();

            assert!(manager.set_global(EDIT_POSITION, base).unwrap());

            let chunk = manager.chunk(TEST_CHUNK).unwrap();
            assert!(chunk.is_dirty());
            assert!(chunk.replacements().is_empty());
            assert_eq!(chunk.edit_revision(), 1);

            let id = manager.request_save(TEST_CHUNK).unwrap();
            smoke.requests.push((id, TEST_CHUNK));
            smoke.phase = SmokePhase::SavingRevert;

            info!("Edited value restored; reverted to base and queued save");
        }
        SmokePhase::SavingRevert => {
            let chunk = manager.chunk(TEST_CHUNK).unwrap();
            assert!(!chunk.is_dirty());
            assert!(chunk.replacements().is_empty());

            manager.unload(TEST_CHUNK).unwrap();

            let id = manager.request_load(TEST_CHUNK).unwrap();
            smoke.requests.push((id, TEST_CHUNK));
            smoke.phase = SmokePhase::LoadingReverted;

            info!("Revert saved; unloaded and requested final reload");
        }
        SmokePhase::LoadingReverted => {
            validate_chunk(&manager, TEST_CHUNK, None);
            validate_chunk(&manager, SECOND_CHUNK, None);

            assert_eq!(manager.get_global(EDIT_POSITION), smoke.base_value.as_ref(),);

            let chunk = manager.chunk(TEST_CHUNK).unwrap();

            // Empty records retain their storage revision.
            assert_eq!(chunk.storage_revision().unwrap().value(), 2);
            assert!(chunk.replacements().is_empty());
            assert!(!chunk.is_dirty());

            assert_eq!(manager.resident_count(), 2);
            assert_eq!(manager.pending_count(), 0);
            assert_eq!(manager.completed_count(), 0);
            assert_eq!(generation.outstanding_count(), 0);
            assert_eq!(generation.preparation_status(), &PreparationStatus::Ready,);

            smoke.phase = SmokePhase::Finished;

            info!(
                "LayerGen managed persistence smoke test PASSED in {:?}",
                smoke.started.elapsed(),
            );
            info!(
                "Verified shared preparation, base generation, sparse edits, \
                 save acknowledgement, reload, and persisted reversion"
            );
        }
        SmokePhase::Finished => unreachable!(),
    }
}
