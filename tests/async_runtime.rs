use std::{
    thread,
    time::{Duration, Instant},
};

use bevy_app::App;

use layergen_rs::{
    ChunkCoord, ChunkGenerator, ChunkSize, DataSet, GenerationError, GenerationLimits,
    LayerGenPlugin, PipelineBuilder, PipelineConfig, PipelineStepResult, RequestError, Step,
    StepContext, WorldBounds,
};

fn config() -> PipelineConfig {
    PipelineConfig::new(
        WorldBounds::new((10, 20, 30), (8, 2, 2)).unwrap(),
        ChunkSize::new(2, 2, 2).unwrap(),
        7,
    )
}

fn basic_app(limits: GenerationLimits) -> App {
    let pipeline = PipelineBuilder::new(config()).build(|position| position.0);

    let mut app = App::new();
    app.add_plugins(LayerGenPlugin::new(pipeline).with_limits(limits));
    app
}

fn wait_for_completions<T: Send + Sync + 'static>(app: &mut App, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);

    loop {
        app.update();

        let generation = app.world().resource::<ChunkGenerator<T>>();

        assert!(generation.in_flight_count() <= generation.limits().max_in_flight());

        if generation.completed_count() >= expected {
            return;
        }

        assert!(
            Instant::now() < deadline,
            "Timed out waiting for {expected} completions: \
             queued={}, in_flight={}, completed={}",
            generation.queued_count(),
            generation.in_flight_count(),
            generation.completed_count(),
        );

        // Tests may sleep; library polling systems never do.
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn limits_reject_invalid_values() {
    assert!(GenerationLimits::new(0, 1).is_err());
    assert!(GenerationLimits::new(1, 0).is_err());
    assert!(GenerationLimits::new(3, 2).is_err());
    assert!(GenerationLimits::new(2, 3).is_ok());
}

#[test]
fn rejects_outside_requests_without_using_capacity() {
    let mut app = basic_app(GenerationLimits::new(1, 2).unwrap());

    let mut generation = app.world_mut().resource_mut::<ChunkGenerator<i64>>();

    let coord = ChunkCoord(4, 0, 0);

    assert_eq!(
        generation.request_batch(coord),
        Err(RequestError::OutsideWorld(coord)),
    );

    assert_eq!(generation.outstanding_count(), 0);
    assert_eq!(generation.queued_count(), 0);
}

#[test]
fn duplicate_is_reserved_until_completion_is_consumed() {
    let mut app = basic_app(GenerationLimits::new(1, 2).unwrap());
    let coord = ChunkCoord(0, 0, 0);

    let first_id = {
        let mut generation = app.world_mut().resource_mut::<ChunkGenerator<i64>>();

        let id = generation.request_batch(coord).unwrap();

        assert_eq!(
            generation.request_batch(coord),
            Err(RequestError::AlreadyOutstanding(coord)),
        );

        id
    };

    wait_for_completions::<i64>(&mut app, 1);

    let mut generation = app.world_mut().resource_mut::<ChunkGenerator<i64>>();

    assert_eq!(
        generation.request_batch(coord),
        Err(RequestError::AlreadyOutstanding(coord)),
    );

    let completion = generation.take_completed().unwrap();

    assert_eq!(completion.request_id, first_id);
    assert!(completion.result.is_ok());
    assert_eq!(generation.outstanding_count(), 0);

    let second_id = generation.request_batch(coord).unwrap();

    assert_ne!(first_id, second_id);
}

#[test]
fn unconsumed_completions_continue_using_capacity() {
    let mut app = basic_app(GenerationLimits::new(1, 2).unwrap());

    {
        let mut generation = app.world_mut().resource_mut::<ChunkGenerator<i64>>();

        generation.request_batch(ChunkCoord(0, 0, 0)).unwrap();
        generation.request_batch(ChunkCoord(1, 0, 0)).unwrap();

        assert_eq!(
            generation.request_batch(ChunkCoord(2, 0, 0)),
            Err(RequestError::CapacityReached { limit: 2 }),
        );
    }

    wait_for_completions::<i64>(&mut app, 2);

    let mut generation = app.world_mut().resource_mut::<ChunkGenerator<i64>>();

    assert_eq!(generation.queued_count(), 0);
    assert_eq!(generation.in_flight_count(), 0);
    assert_eq!(generation.completed_count(), 2);
    assert_eq!(generation.outstanding_count(), 2);

    assert_eq!(
        generation.request_batch(ChunkCoord(2, 0, 0)),
        Err(RequestError::CapacityReached { limit: 2 }),
    );

    generation.take_completed().unwrap();

    assert!(generation.request_batch(ChunkCoord(2, 0, 0)).is_ok());
    assert_eq!(generation.outstanding_count(), 2);
}

#[test]
fn dispatcher_respects_in_flight_limit() {
    let mut app = basic_app(GenerationLimits::new(1, 4).unwrap());

    {
        let mut generation = app.world_mut().resource_mut::<ChunkGenerator<i64>>();

        for x in 0..4 {
            generation.request_batch(ChunkCoord(x, 0, 0)).unwrap();
        }
    }

    app.update();

    {
        let generation = app.world().resource::<ChunkGenerator<i64>>();

        // Collection occurs before dispatch, so even an already-finished
        // first task remains tracked until the next update.
        assert_eq!(generation.in_flight_count(), 1);
        assert_eq!(generation.queued_count(), 3);
    }

    wait_for_completions::<i64>(&mut app, 4);
}

struct AddSeed;

impl Step<i64> for AddSeed {
    fn run(&self, data: &mut DataSet<i64>, context: &StepContext) -> PipelineStepResult {
        let seed = context.config().seed() as i64;

        for (value, _, _, _) in data.iter_mut() {
            *value += seed;
        }

        Ok(())
    }
}

#[test]
fn multiple_requests_have_independent_correct_results() {
    let pipeline = PipelineBuilder::new(config())
        .add_step(AddSeed)
        .build(|position| position.0);

    let mut app = App::new();
    app.add_plugins(
        LayerGenPlugin::new(pipeline).with_limits(GenerationLimits::new(2, 4).unwrap()),
    );

    let expected = {
        let mut generation = app.world_mut().resource_mut::<ChunkGenerator<i64>>();

        (0..4)
            .map(|x| {
                let coord = ChunkCoord(x, 0, 0);
                let id = generation.request_batch(coord).unwrap();
                (id, coord)
            })
            .collect::<Vec<_>>()
    };

    wait_for_completions::<i64>(&mut app, 4);

    let mut generation = app.world_mut().resource_mut::<ChunkGenerator<i64>>();

    let mut seen = Vec::new();

    while let Some(completion) = generation.take_completed() {
        assert!(expected.contains(&(completion.request_id, completion.coord)));
        assert!(!seen.contains(&completion.request_id));

        seen.push(completion.request_id);

        let chunk = completion.result.unwrap();
        let context = chunk.context();

        assert_eq!(chunk.coord(), completion.coord);
        assert_eq!(chunk.data().dimensions(), (2, 2, 2));

        for (value, x, y, z) in chunk.data().iter() {
            let global = context.global_position((x, y, z)).unwrap();
            assert_eq!(*value, global.0 + 7);
        }
    }

    assert_eq!(seen.len(), 4);
    assert_eq!(generation.outstanding_count(), 0);
}

struct FailingStep;

impl Step<i64> for FailingStep {
    fn run(&self, _data: &mut DataSet<i64>, _context: &StepContext) -> PipelineStepResult {
        Err("Planned step failure".to_owned())
    }
}

#[test]
fn step_failure_is_returned_as_a_completion() {
    let pipeline = PipelineBuilder::new(config())
        .add_step(FailingStep)
        .build(|_| 0i64);

    let mut app = App::new();
    app.add_plugins(LayerGenPlugin::new(pipeline));

    let request_id = app
        .world_mut()
        .resource_mut::<ChunkGenerator<i64>>()
        .request_batch(ChunkCoord(0, 0, 0))
        .unwrap();

    wait_for_completions::<i64>(&mut app, 1);

    let mut generation = app.world_mut().resource_mut::<ChunkGenerator<i64>>();

    let completion = generation.take_completed().unwrap();

    assert_eq!(completion.request_id, request_id);

    assert!(matches!(
        completion.result,
        Err(GenerationError::Processing(message))
            if message == "Planned step failure"
    ));

    assert_eq!(generation.outstanding_count(), 0);
}

#[test]
fn initializer_panic_is_returned_as_a_completion() {
    let pipeline = PipelineBuilder::<i64>::new(config()).build(|_| {
        panic!("Planned initializer panic");
    });

    let mut app = App::new();
    app.add_plugins(LayerGenPlugin::new(pipeline));

    app.world_mut()
        .resource_mut::<ChunkGenerator<i64>>()
        .request_batch(ChunkCoord(0, 0, 0))
        .unwrap();

    wait_for_completions::<i64>(&mut app, 1);

    let completion = app
        .world_mut()
        .resource_mut::<ChunkGenerator<i64>>()
        .take_completed()
        .unwrap();

    assert!(matches!(
        completion.result,
        Err(GenerationError::Panicked(message))
            if message == "Planned initializer panic"
    ));
}
