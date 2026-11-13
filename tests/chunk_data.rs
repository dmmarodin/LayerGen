use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use bevy_app::App;

use layergen_rs::*;

fn config() -> PipelineConfig {
    PipelineConfig::new(
        WorldBounds::new((-4, 0, 0), (8, 4, 4)).unwrap(),
        ChunkSize::new(4, 2, 2).unwrap(),
        7,
    )
}

/// Stand-in for a small analytic world model (e.g. tectonics).
struct World {
    offset: i64,
}

/// Per-chunk data: one value per (x, y) column plus a chunk-level marker.
#[derive(Clone, Debug, PartialEq)]
struct Columns {
    heights: DataSet<i64>,
    marker: i64,
    column_samples: usize,
    right_marker: Option<i64>,
    suffix_saw_right_marker: Option<i64>,
}

fn columns(context: &StepContext, world: &World) -> Columns {
    let (width, height, _) = context.dimensions();
    let coord = context.chunk_coord();

    Columns {
        heights: DataSet::new(width, height, 1, |_| 0).unwrap(),
        marker: i64::from(coord.0) * 100
            + i64::from(coord.1) * 10
            + i64::from(coord.2)
            + world.offset,
        column_samples: 0,
        right_marker: None,
        suffix_saw_right_marker: None,
    }
}

fn column_height(x: i64, y: i64, world: &World) -> i64 {
    x + y * 10 + world.offset
}

/// Samples the world model once per column, not once per cell.
struct SampleColumns {
    calls: Arc<AtomicUsize>,
}

impl ChunkStep<i64, Columns, World> for SampleColumns {
    fn run(
        &self,
        _data: &mut DataSet<i64>,
        chunk: &mut Columns,
        context: &StepContext,
        world: &World,
    ) -> PipelineStepResult {
        let origin = context.origin();

        for (height, x, y, _) in chunk.heights.iter_mut() {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *height = column_height(origin.0 + x as i64, origin.1 + y as i64, world);
            chunk.column_samples += 1;
        }

        Ok(())
    }
}

/// Reads column data to fill cells.
struct FillCells;

impl ChunkStep<i64, Columns, World> for FillCells {
    fn run(
        &self,
        data: &mut DataSet<i64>,
        chunk: &mut Columns,
        context: &StepContext,
        _world: &World,
    ) -> PipelineStepResult {
        let origin = context.origin();

        for (value, x, y, z) in data.iter_mut() {
            *value = chunk.heights[(x, y, 0)] + (origin.2 + z as i64);
        }

        Ok(())
    }
}

struct AddThousand;

impl Step<i64> for AddThousand {
    fn run(&self, data: &mut DataSet<i64>, _context: &StepContext) -> PipelineStepResult {
        for (value, _, _, _) in data.iter_mut() {
            *value += 1_000;
        }

        Ok(())
    }
}

struct AddOffset;

impl SharedStep<i64, World> for AddOffset {
    fn run(
        &self,
        data: &mut DataSet<i64>,
        _context: &StepContext,
        world: &World,
    ) -> PipelineStepResult {
        for (value, _, _, _) in data.iter_mut() {
            *value += world.offset;
        }

        Ok(())
    }
}

fn expected_cell(position: GlobalPosition) -> i64 {
    let world = World { offset: 7 };

    column_height(position.0, position.1, &world) + position.2 + 1_000 + world.offset
}

fn pipeline(
    calls: Arc<AtomicUsize>,
) -> Pipeline<
    i64,
    impl ChunkStep<i64, Columns, World>,
    impl Fn(GlobalPosition) -> i64 + Send + Sync + 'static,
    World,
    Columns,
> {
    PipelineBuilder::new(config())
        .prepare(|config| {
            Ok(World {
                offset: config.seed() as i64,
            })
        })
        .chunk_data(columns)
        .add_chunk_step(SampleColumns { calls })
        .add_chunk_step(FillCells)
        .add_step(AddThousand)
        .add_shared_step(AddOffset)
        .build(|_| 0)
}

fn validate_chunk(chunk: &Chunk<i64, Columns>) {
    let context = chunk.context();
    let (width, height, depth) = context.dimensions();
    let origin = context.origin();
    let world = World { offset: 7 };

    assert_eq!(chunk.chunk_data().column_samples, width * height);
    assert_eq!(chunk.chunk_data().heights.dimensions(), (width, height, 1));

    for (value, x, y, _) in chunk.chunk_data().heights.iter() {
        assert_eq!(
            *value,
            column_height(origin.0 + x as i64, origin.1 + y as i64, &world)
        );
    }

    assert_eq!(chunk.data().dimensions(), (width, height, depth));

    for (value, x, y, z) in chunk.data().iter() {
        let global = context.global_position((x, y, z)).unwrap();
        assert_eq!(*value, expected_cell(global));
    }
}

#[test]
fn chunk_steps_share_column_data_with_local_and_shared_steps() {
    let calls = Arc::new(AtomicUsize::new(0));
    let pipeline = pipeline(Arc::clone(&calls));

    let world = pipeline.prepare_shared().unwrap();
    let chunk = pipeline
        .generate_chunk_with_shared(ChunkCoord(1, 1, 1), &world)
        .unwrap();

    validate_chunk(&chunk);

    // 4x2 columns sampled once each, not once per 4x2x2 cell.
    assert_eq!(calls.load(Ordering::SeqCst), 8);
    assert_eq!(chunk.chunk_data().marker, 111 + 7);

    let (data, chunk_data) = chunk.into_parts();
    assert_eq!(data.len(), 16);
    assert_eq!(chunk_data.column_samples, 8);
}

#[test]
fn chunk_data_initializer_runs_per_chunk_and_reads_shared_data() {
    let pipeline = pipeline(Arc::new(AtomicUsize::new(0)));
    let world = pipeline.prepare_shared().unwrap();

    let first = pipeline
        .generate_chunk_with_shared(ChunkCoord(0, 0, 0), &world)
        .unwrap();
    let second = pipeline
        .generate_chunk_with_shared(ChunkCoord(1, 0, 1), &world)
        .unwrap();

    assert_eq!(first.chunk_data().marker, 7);
    assert_eq!(second.chunk_data().marker, 101 + 7);
}

#[test]
fn pipelines_without_chunk_data_use_unit() {
    let pipeline = PipelineBuilder::new(config())
        .add_step(AddThousand)
        .build(|position| position.0);

    let chunk: Chunk<i64> = pipeline.generate_chunk(ChunkCoord(0, 0, 0)).unwrap();

    assert_eq!(chunk.chunk_data(), &());
    assert_eq!(chunk.data()[(0, 0, 0)], 996);
}

fn wait_until(app: &mut App, mut condition: impl FnMut(&App) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);

    loop {
        app.update();

        if condition(app) {
            return;
        }

        assert!(Instant::now() < deadline, "Chunk data test timed out");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn async_runtime_returns_chunk_data() {
    let mut app = App::new();

    app.add_plugins(LayerGenPlugin::new(pipeline(Arc::new(AtomicUsize::new(0)))));

    app.world_mut()
        .resource_mut::<ChunkGenerator<i64, Columns>>()
        .request_batch(ChunkCoord(1, 0, 0))
        .unwrap();

    wait_until(&mut app, |app| {
        app.world()
            .resource::<ChunkGenerator<i64, Columns>>()
            .completed_count()
            > 0
    });

    let completion = app
        .world_mut()
        .resource_mut::<ChunkGenerator<i64, Columns>>()
        .take_completed()
        .unwrap();

    validate_chunk(&completion.result.unwrap());
}

#[test]
fn managed_chunks_expose_base_chunk_data_unchanged_by_edits() {
    let identity = PersistenceIdentity {
        world: WorldId("chunk-data-test".to_owned()),
        generation: GenerationId("v1".to_owned()),
    };

    let storage = Arc::new(MemoryChunkStorage::<i64>::default());
    let mut app = App::new();

    app.add_plugins(LayerGenPlugin::new(pipeline(Arc::new(AtomicUsize::new(0)))));
    app.add_plugins(ChunkManagerPlugin::<i64, Columns>::with_chunk_data(
        identity, storage,
    ));

    let coord = ChunkCoord(0, 1, 0);

    app.world_mut()
        .resource_mut::<ChunkManager<i64, Columns>>()
        .request_load(coord)
        .unwrap();

    wait_until(&mut app, |app| {
        app.world()
            .resource::<ChunkManager<i64, Columns>>()
            .completed_count()
            > 0
    });

    let mut manager = app.world_mut().resource_mut::<ChunkManager<i64, Columns>>();

    assert_eq!(
        manager.take_completed().unwrap().result,
        Ok(ManagedOutcome::Loaded { installed: true }),
    );

    let before = manager.chunk(coord).unwrap().chunk_data().clone();
    assert_eq!(before.marker, 10 + 7);

    let position = manager.chunk(coord).unwrap().context().origin();
    assert!(manager.set_global(position, -1).unwrap());

    let chunk = manager.chunk(coord).unwrap();
    assert_eq!(chunk.data()[(0, 0, 0)], -1);
    assert_eq!(chunk.chunk_data(), &before);
}

/// Reads the right-hand neighbor's prefix chunk data and checks the
/// NeighborChunks contract from inside the step.
struct ReadRightNeighbor;

impl NeighborhoodChunkStep<i64, Columns, World> for ReadRightNeighbor {
    fn run(
        &self,
        _input: &NeighborhoodView<'_, i64>,
        chunks: &NeighborChunks<'_, Columns>,
        _output: &mut DataSet<i64>,
        output_chunk: &mut Columns,
        context: &StepContext,
        _world: &World,
    ) -> PipelineStepResult {
        let origin = context.origin();
        let (width, height, depth) = context.dimensions();
        let width = width as i64;

        // Requested chunk entry is the prefix snapshot.
        let own = chunks.get(context.chunk_coord())?;
        assert_eq!(own.marker, output_chunk.marker);
        assert_eq!(own.right_marker, None);

        // Undeclared chunk and undeclared position are errors.
        assert!(chunks.get(ChunkCoord(9, 9, 9)).is_err());
        assert!(chunks
            .get_global((origin.0 + width + 1, origin.1, origin.2))
            .is_err());

        // Declared outside-world read is Ok(None) on the left world edge.
        if origin.0 == -4 {
            assert_eq!(chunks.get_global((origin.0 - 1, origin.1, origin.2))?, None);
        }

        // Far corner of the declared region is readable when in-world.
        let corner = (
            origin.0 + width,
            origin.1 + height as i64,
            origin.2 + depth as i64,
        );
        if config().world().contains(corner) {
            assert!(chunks.get_global(corner)?.is_some());
        }

        output_chunk.right_marker = chunks
            .get_global((origin.0 + width, origin.1, origin.2))?
            .map(|right| right.marker);

        Ok(())
    }
}

struct CopyRightMarker;

impl ChunkStep<i64, Columns, World> for CopyRightMarker {
    fn run(
        &self,
        _data: &mut DataSet<i64>,
        chunk: &mut Columns,
        _context: &StepContext,
        _world: &World,
    ) -> PipelineStepResult {
        chunk.suffix_saw_right_marker = chunk.right_marker;
        Ok(())
    }
}

#[test]
fn neighborhood_reads_dependency_chunk_data_and_suffix_sees_output() {
    let pipeline = pipeline(Arc::new(AtomicUsize::new(0)))
        .with_chunk_neighborhood(
            Neighborhood::radius(1),
            NeighborhoodLimits::default(),
            ReadRightNeighbor,
        )
        .add_chunk_step(CopyRightMarker);

    let world = pipeline.prepare_shared().unwrap();

    let left = pipeline
        .generate_chunk_with_shared(ChunkCoord(0, 1, 1), &world)
        .unwrap();

    validate_chunk(&left);
    assert_eq!(left.chunk_data().right_marker, Some(111 + 7));
    assert_eq!(left.chunk_data().suffix_saw_right_marker, Some(111 + 7));

    // Right world edge: the declared right neighbor is outside the world.
    let right = pipeline
        .generate_chunk_with_shared(ChunkCoord(1, 1, 1), &world)
        .unwrap();

    validate_chunk(&right);
    assert_eq!(right.chunk_data().right_marker, None);
}

#[test]
fn cell_only_neighborhood_steps_still_work_with_chunk_data() {
    struct Identity;

    impl NeighborhoodStep<i64, World> for Identity {
        fn run(
            &self,
            input: &NeighborhoodView<'_, i64>,
            output: &mut DataSet<i64>,
            context: &StepContext,
            _world: &World,
        ) -> PipelineStepResult {
            for (value, x, y, z) in output.iter_mut() {
                let global = context.global_position((x, y, z)).unwrap();
                *value = *input.get_global(global)?.unwrap();
            }

            Ok(())
        }
    }

    let pipeline = pipeline(Arc::new(AtomicUsize::new(0))).with_neighborhood(
        Neighborhood::radius(1),
        NeighborhoodLimits::default(),
        Identity,
    );

    let world = pipeline.prepare_shared().unwrap();
    let chunk = pipeline
        .generate_chunk_with_shared(ChunkCoord(1, 0, 0), &world)
        .unwrap();

    validate_chunk(&chunk);
}

/// Reads only horizontal neighbors; checks Z reads are undeclared.
struct HorizontalOnly;

impl NeighborhoodChunkStep<i64, Columns, World> for HorizontalOnly {
    fn run(
        &self,
        input: &NeighborhoodView<'_, i64>,
        chunks: &NeighborChunks<'_, Columns>,
        _output: &mut DataSet<i64>,
        _output_chunk: &mut Columns,
        context: &StepContext,
        _world: &World,
    ) -> PipelineStepResult {
        let origin = context.origin();
        let depth = context.dimensions().2 as i64;

        // Z radius 0: the chunk above and the cell above are undeclared.
        assert!(chunks
            .get_global((origin.0, origin.1, origin.2 + depth))
            .is_err());
        assert!(input
            .get_global((origin.0, origin.1, origin.2 - 1))
            .is_err());

        // X/Y radius 1 still reaches the right-hand neighbor.
        let width = context.dimensions().0 as i64;
        assert!(chunks
            .get_global((origin.0 + width, origin.1, origin.2))
            .is_ok());

        Ok(())
    }
}

#[test]
fn per_axis_radius_skips_unneeded_dependency_chunks() {
    let count_prefix_samples = |neighborhood: Neighborhood| {
        let calls = Arc::new(AtomicUsize::new(0));
        let pipeline = pipeline(Arc::clone(&calls)).with_chunk_neighborhood(
            neighborhood,
            NeighborhoodLimits::default(),
            HorizontalOnly,
        );
        let world = pipeline.prepare_shared().unwrap();

        pipeline
            .generate_chunk_with_shared(ChunkCoord(0, 0, 0), &world)
            .unwrap();

        calls.load(Ordering::SeqCst)
    };

    // Chunk (0,0,0) with X/Y radius 1: chunks x 0..=1, y 0..=1, z 0 → 4
    // prefix chunks of 8 columns each (a symmetric radius would add z 1).
    assert_eq!(count_prefix_samples(Neighborhood::axes(1, 1, 0)), 4 * 8);
    assert_eq!(Neighborhood::axes(1, 1, 0).cell_radius(), 1);
    assert_eq!(Neighborhood::radius(2).radii(), (2, 2, 2));
}
