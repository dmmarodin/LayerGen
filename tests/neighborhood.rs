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
        WorldBounds::new((-3, -2, -1), (5, 4, 3)).unwrap(),
        ChunkSize::new(2, 2, 2).unwrap(),
        7,
    )
}

fn source_value(position: GlobalPosition, seed: i64) -> i64 {
    position.0 + position.1 * 10 + position.2 * 100 + seed
}

struct Prefix;

impl Step<i64> for Prefix {
    fn run(&self, data: &mut DataSet<i64>, context: &StepContext) -> PipelineStepResult {
        for (value, x, y, z) in data.iter_mut() {
            let global = context.global_position((x, y, z)).unwrap();

            *value = source_value(global, context.config().seed() as i64);
        }

        Ok(())
    }
}

struct AverageBox {
    radius: i64,
}

impl<D: Send + Sync + 'static> NeighborhoodStep<i64, D> for AverageBox {
    fn run(
        &self,
        input: &NeighborhoodView<'_, i64>,
        output: &mut DataSet<i64>,
        _context: &StepContext,
        _shared: &D,
    ) -> PipelineStepResult {
        for (value, x, y, z) in output.iter_mut() {
            let mut total = 0;
            let mut count = 0;

            for dz in -self.radius..=self.radius {
                for dy in -self.radius..=self.radius {
                    for dx in -self.radius..=self.radius {
                        if let Some(sample) = input.get_offset((x, y, z), (dx, dy, dz))? {
                            total += *sample;
                            count += 1;
                        }
                    }
                }
            }

            *value = total / count;
        }

        Ok(())
    }
}

struct Suffix;

impl Step<i64> for Suffix {
    fn run(&self, data: &mut DataSet<i64>, _context: &StepContext) -> PipelineStepResult {
        for (value, _, _, _) in data.iter_mut() {
            *value += 1_000;
        }

        Ok(())
    }
}

fn reference(config: PipelineConfig, position: GlobalPosition, radius: i64) -> i64 {
    let mut total = 0;
    let mut count = 0;

    for dz in -radius..=radius {
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                let sample = (position.0 + dx, position.1 + dy, position.2 + dz);

                if config.world().contains(sample) {
                    total += source_value(sample, config.seed() as i64);
                    count += 1;
                }
            }
        }
    }

    total / count + 1_000
}

fn all_coords() -> Vec<ChunkCoord> {
    let mut coords = Vec::new();

    for z in 0..2 {
        for y in 0..2 {
            for x in 0..3 {
                coords.push(ChunkCoord(x, y, z));
            }
        }
    }

    coords
}

#[test]
fn chunks_match_whole_world_reference_in_both_request_orders() {
    let config = config();

    let pipeline = PipelineBuilder::new(config)
        .add_step(Prefix)
        .build(|_| 0i64)
        .with_neighborhood(
            Neighborhood::radius(1),
            NeighborhoodLimits::default(),
            AverageBox { radius: 1 },
        )
        .add_step(Suffix);

    let coords = all_coords();
    let mut first_results = Vec::new();

    for coord in &coords {
        let chunk = pipeline.generate_chunk(*coord).unwrap();
        let context = chunk.context();

        for (value, x, y, z) in chunk.data().iter() {
            let global = context.global_position((x, y, z)).unwrap();
            assert_eq!(*value, reference(config, global, 1));
        }

        first_results.push((*coord, chunk.into_data()));
    }

    for coord in coords.iter().rev() {
        let chunk = pipeline.generate_chunk(*coord).unwrap();

        let previous = first_results
            .iter()
            .find(|entry| entry.0 == *coord)
            .unwrap();

        assert_eq!(chunk.data().as_slice(), previous.1.as_slice());
    }

    // Last chunk is partial along X and Z.
    let edge = pipeline.generate_chunk(ChunkCoord(2, 1, 1)).unwrap();
    assert_eq!(edge.data().dimensions(), (1, 2, 1));
}

#[test]
fn neighborhood_can_cross_multiple_chunks() {
    let config = config();

    let pipeline = PipelineBuilder::new(config)
        .add_step(Prefix)
        .build(|_| 0i64)
        .with_neighborhood(
            Neighborhood::radius(3),
            NeighborhoodLimits::default(),
            AverageBox { radius: 3 },
        )
        .add_step(Suffix);

    for coord in all_coords() {
        let chunk = pipeline.generate_chunk(coord).unwrap();
        let context = chunk.context();

        for (value, x, y, z) in chunk.data().iter() {
            let global = context.global_position((x, y, z)).unwrap();

            assert_eq!(*value, reference(config, global, 3));
        }
    }
}

struct CheckBoundaryReads;

impl NeighborhoodStep<i64, ()> for CheckBoundaryReads {
    fn run(
        &self,
        input: &NeighborhoodView<'_, i64>,
        _output: &mut DataSet<i64>,
        _context: &StepContext,
        _shared: &(),
    ) -> PipelineStepResult {
        // At the world's minimum corner, declared outside-world reads
        // are explicitly absent.
        assert_eq!(input.get_offset((0, 0, 0), (-1, 0, 0))?, None);

        // The requested chunk spans local X 0..2; radius 1 extends
        // the readable box to local X -1..3, exclusive at 3.
        assert!(input.get_offset((0, 0, 0), (-2, 0, 0)).is_err());
        assert!(input.get_offset((0, 0, 0), (3, 0, 0)).is_err());

        // Invalid output-local positions are errors too.
        assert!(input.get_offset((2, 0, 0), (0, 0, 0)).is_err());

        assert!(input.get_offset((0, 0, 0), (0, 0, 0))?.is_some());

        Ok(())
    }
}

#[test]
fn outside_world_and_undeclared_reads_are_distinct() {
    let pipeline = PipelineBuilder::new(config())
        .build(|_| 0i64)
        .with_neighborhood(
            Neighborhood::radius(1),
            NeighborhoodLimits::default(),
            CheckBoundaryReads,
        );

    pipeline.generate_chunk(ChunkCoord(0, 0, 0)).unwrap();
}

#[test]
fn excessive_requirements_fail_before_prefix_initialization() {
    let calls = Arc::new(AtomicUsize::new(0));
    let initialized = Arc::clone(&calls);

    let pipeline = PipelineBuilder::new(config())
        .build(move |_| {
            initialized.fetch_add(1, Ordering::SeqCst);
            0i64
        })
        .with_neighborhood(
            Neighborhood::radius(1),
            NeighborhoodLimits::new(1, 125, 10_000).unwrap(),
            AverageBox { radius: 1 },
        );

    let error = pipeline.generate_chunk(ChunkCoord(0, 0, 0)).unwrap_err();

    assert!(error.contains("temporary cells"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn dependency_chunk_and_prefix_work_limits_are_enforced() {
    let chunk_limited = PipelineBuilder::new(config())
        .build(|_| 0i64)
        .with_neighborhood(
            Neighborhood::radius(1),
            NeighborhoodLimits::new(10_000, 1, 10_000).unwrap(),
            AverageBox { radius: 1 },
        );

    assert!(chunk_limited
        .generate_chunk(ChunkCoord(0, 0, 0))
        .unwrap_err()
        .contains("prefix chunks"));

    let work_limited = PipelineBuilder::new(config())
        .build(|_| 0i64)
        .with_neighborhood(
            Neighborhood::radius(1),
            NeighborhoodLimits::new(10_000, 125, 1).unwrap(),
            AverageBox { radius: 1 },
        );

    assert!(work_limited
        .generate_chunk(ChunkCoord(0, 0, 0))
        .unwrap_err()
        .contains("prefix cells"));
}

struct FailingNeighborhood;

impl NeighborhoodStep<i64, ()> for FailingNeighborhood {
    fn run(
        &self,
        _input: &NeighborhoodView<'_, i64>,
        _output: &mut DataSet<i64>,
        _context: &StepContext,
        _shared: &(),
    ) -> PipelineStepResult {
        Err("Planned neighborhood failure".to_owned())
    }
}

fn wait_for_completions(app: &mut App, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);

    loop {
        app.update();

        if app
            .world()
            .resource::<ChunkGenerator<i64>>()
            .completed_count()
            >= expected
        {
            return;
        }

        assert!(
            Instant::now() < deadline,
            "Neighborhood runtime test timed out"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn neighborhood_failure_becomes_async_completion() {
    let pipeline = PipelineBuilder::new(config())
        .build(|_| 0i64)
        .with_neighborhood(
            Neighborhood::radius(1),
            NeighborhoodLimits::default(),
            FailingNeighborhood,
        );

    let mut app = App::new();
    app.add_plugins(LayerGenPlugin::new(pipeline));

    app.world_mut()
        .resource_mut::<ChunkGenerator<i64>>()
        .request_batch(ChunkCoord(0, 0, 0))
        .unwrap();

    wait_for_completions(&mut app, 1);

    let completion = app
        .world_mut()
        .resource_mut::<ChunkGenerator<i64>>()
        .take_completed()
        .unwrap();

    assert!(matches!(
        completion.result,
        Err(GenerationError::Processing(message))
            if message.contains("Planned neighborhood failure")
    ));
}

#[test]
fn preparation_and_neighborhood_work_together_in_bevy() {
    let preparation_calls = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&preparation_calls);

    let pipeline = PipelineBuilder::new(config())
        .prepare(move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(42u32)
        })
        .add_step(Prefix)
        .build(|_| 0i64)
        .with_neighborhood(
            Neighborhood::radius(1),
            NeighborhoodLimits::default(),
            AverageBox { radius: 1 },
        )
        .add_step(Suffix);

    let mut app = App::new();
    app.add_plugins(LayerGenPlugin::new(pipeline));

    {
        let mut generation = app.world_mut().resource_mut::<ChunkGenerator<i64>>();

        generation.request_batch(ChunkCoord(0, 0, 0)).unwrap();
        generation.request_batch(ChunkCoord(1, 0, 0)).unwrap();
    }

    wait_for_completions(&mut app, 2);

    let mut generation = app.world_mut().resource_mut::<ChunkGenerator<i64>>();

    while let Some(completion) = generation.take_completed() {
        let chunk = completion.result.unwrap();
        let context = chunk.context();

        for (value, x, y, z) in chunk.data().iter() {
            let global = context.global_position((x, y, z)).unwrap();

            assert_eq!(*value, reference(config(), global, 1));
        }
    }

    assert_eq!(preparation_calls.load(Ordering::SeqCst), 1);
    assert_eq!(generation.outstanding_count(), 0);
}
