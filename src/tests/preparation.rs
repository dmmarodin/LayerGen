use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use bevy_app::App;

use layergen_rs::{
    ChunkCoord, ChunkGenerator, ChunkSize, DataSet, GenerationError,
    GenerationLimits, LayerGenPlugin, PipelineBuilder, PipelineConfig,
    PipelineStepResult, PreparationError, PreparationStatus, RequestError,
    SharedStep, Step, StepContext, WorldBounds,
};

fn config() -> PipelineConfig {
    PipelineConfig::new(
        WorldBounds::new((0, 0, 0), (8, 2, 2)).unwrap(),
        ChunkSize::new(2, 2, 2).unwrap(),
        7,
    )
}

fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);

    loop {
        if condition() {
            return;
        }

        assert!(Instant::now() < deadline, "Test timed out");
        thread::sleep(Duration::from_millis(1));
    }
}

fn wait_for_completions<T: Send + Sync + 'static>(
    app: &mut App,
    expected: usize,
) {
    wait_until(|| {
        app.update();

        let generation = app.world().resource::<ChunkGenerator<T>>();

        assert!(
            generation.in_flight_count()
                <= generation.limits().max_in_flight()
        );

        generation.completed_count() >= expected
    });
}

struct SharedData {
    value: i64,
}

struct ReadShared {
    observed_addresses: Arc<Mutex<Vec<usize>>>,
}

impl SharedStep<i64, SharedData> for ReadShared {
    fn run(
        &self,
        data: &mut DataSet<i64>,
        _context: &StepContext,
        shared: &SharedData,
    ) -> PipelineStepResult {
        self.observed_addresses
            .lock()
            .unwrap()
            .push(shared as *const SharedData as usize);

        for (value, _, _, _) in data.iter_mut() {
            *value = shared.value;
        }

        Ok(())
    }
}

struct Increment;

impl Step<i64> for Increment {
    fn run(
        &self,
        data: &mut DataSet<i64>,
        _context: &StepContext,
    ) -> PipelineStepResult {
        for (value, _, _, _) in data.iter_mut() {
            *value += 1;
        }

        Ok(())
    }
}

#[test]
fn prepares_once_and_shares_one_instance_between_requests() {
    let preparation_calls = Arc::new(AtomicUsize::new(0));
    let observed_addresses = Arc::new(Mutex::new(Vec::new()));

    let calls = Arc::clone(&preparation_calls);

    let pipeline = PipelineBuilder::new(config())
        .prepare(move |config| {
            calls.fetch_add(1, Ordering::SeqCst);

            Ok(SharedData {
                value: config.seed() as i64,
            })
        })
        .add_shared_step(ReadShared {
            observed_addresses: Arc::clone(&observed_addresses),
        })
        .add_step(Increment)
        .build(|_| 0i64);

    let mut app = App::new();
    app.add_plugins(
        LayerGenPlugin::new(pipeline)
            .with_limits(GenerationLimits::new(2, 4).unwrap()),
    );

    assert_eq!(preparation_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        app.world()
            .resource::<ChunkGenerator<i64>>()
            .preparation_status(),
        &PreparationStatus::Unprepared,
    );

    {
        let mut generation =
            app.world_mut().resource_mut::<ChunkGenerator<i64>>();

        generation.request_batch(ChunkCoord(0, 0, 0)).unwrap();
        generation.request_batch(ChunkCoord(1, 0, 0)).unwrap();
    }

    wait_for_completions::<i64>(&mut app, 2);

    {
        let mut generation =
            app.world_mut().resource_mut::<ChunkGenerator<i64>>();

        for _ in 0..2 {
            let chunk = generation.take_completed().unwrap().result.unwrap();

            assert!(chunk.data().as_slice().iter().all(|value| *value == 8));
        }

        // A later request must reuse the same preparation.
        generation.request_batch(ChunkCoord(2, 0, 0)).unwrap();
    }

    wait_for_completions::<i64>(&mut app, 1);

    let addresses = observed_addresses.lock().unwrap();

    assert_eq!(addresses.len(), 3);
    assert!(addresses.iter().all(|address| *address == addresses[0]));
    assert_eq!(preparation_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn preparation_starts_without_any_chunk_requests() {
    let calls = Arc::new(AtomicUsize::new(0));
    let preparation_calls = Arc::clone(&calls);

    let pipeline = PipelineBuilder::new(config())
        .prepare(move |_| {
            preparation_calls.fetch_add(1, Ordering::SeqCst);
            Ok(SharedData { value: 42 })
        })
        .build(|_| 0i64);

    let mut app = App::new();
    app.add_plugins(LayerGenPlugin::new(pipeline));

    wait_until(|| {
        app.update();

        matches!(
            app.world()
                .resource::<ChunkGenerator<i64>>()
                .preparation_status(),
            PreparationStatus::Ready
        )
    });

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        app.world()
            .resource::<ChunkGenerator<i64>>()
            .outstanding_count(),
        0,
    );
}

#[test]
fn chunk_jobs_wait_for_preparation_and_queue_limits_still_apply() {
    let preparation_started = Arc::new(AtomicBool::new(false));
    let release_preparation = Arc::new(AtomicBool::new(false));
    let initializer_calls = Arc::new(AtomicUsize::new(0));

    let started = Arc::clone(&preparation_started);
    let release = Arc::clone(&release_preparation);
    let initialized = Arc::clone(&initializer_calls);

    let pipeline = PipelineBuilder::new(config())
        .prepare(move |_| {
            started.store(true, Ordering::SeqCst);
            let deadline = Instant::now() + Duration::from_secs(10);

            while !release.load(Ordering::SeqCst) {
                if Instant::now() >= deadline {
                    return Err("Preparation gate timed out".to_owned());
                }

                thread::sleep(Duration::from_millis(1));
            }

            Ok(SharedData { value: 42 })
        })
        .build(move |_| {
            initialized.fetch_add(1, Ordering::SeqCst);
            0i64
        });

    let mut app = App::new();
    app.add_plugins(
        LayerGenPlugin::new(pipeline)
            .with_limits(GenerationLimits::new(1, 2).unwrap()),
    );

    {
        let mut generation =
            app.world_mut().resource_mut::<ChunkGenerator<i64>>();

        generation.request_batch(ChunkCoord(0, 0, 0)).unwrap();
        generation.request_batch(ChunkCoord(1, 0, 0)).unwrap();

        assert_eq!(
            generation.request_batch(ChunkCoord(2, 0, 0)),
            Err(RequestError::CapacityReached { limit: 2 }),
        );
    }

    app.update();

    wait_until(|| preparation_started.load(Ordering::SeqCst));

    // While the preparation worker is gated, no chunk can dispatch.
    for _ in 0..5 {
        app.update();
    }

    {
        let generation = app.world().resource::<ChunkGenerator<i64>>();

        assert_eq!(
            generation.preparation_status(),
            &PreparationStatus::Preparing,
        );
        assert_eq!(generation.queued_count(), 2);
        assert_eq!(generation.in_flight_count(), 0);
        assert_eq!(generation.completed_count(), 0);
        assert_eq!(initializer_calls.load(Ordering::SeqCst), 0);
    }

    release_preparation.store(true, Ordering::SeqCst);
    wait_for_completions::<i64>(&mut app, 2);

    assert_eq!(initializer_calls.load(Ordering::SeqCst), 16);
}

#[test]
fn preparation_error_fails_queued_requests_and_rejects_new_ones() {
    let initializer_calls = Arc::new(AtomicUsize::new(0));
    let initialized = Arc::clone(&initializer_calls);

    let pipeline = PipelineBuilder::new(config())
        .prepare(|_| -> Result<SharedData, String> {
            Err("Planned preparation failure".to_owned())
        })
        .build(move |_| {
            initialized.fetch_add(1, Ordering::SeqCst);
            0i64
        });

    let mut app = App::new();
    app.add_plugins(LayerGenPlugin::new(pipeline));

    let ids = {
        let mut generation =
            app.world_mut().resource_mut::<ChunkGenerator<i64>>();

        vec![
            generation.request_batch(ChunkCoord(0, 0, 0)).unwrap(),
            generation.request_batch(ChunkCoord(1, 0, 0)).unwrap(),
        ]
    };

    wait_for_completions::<i64>(&mut app, 2);

    let expected_error =
        PreparationError::Processing("Planned preparation failure".to_owned());

    let mut generation =
        app.world_mut().resource_mut::<ChunkGenerator<i64>>();

    assert_eq!(
        generation.preparation_status(),
        &PreparationStatus::Failed(expected_error.clone()),
    );
    assert_eq!(generation.queued_count(), 0);
    assert_eq!(generation.in_flight_count(), 0);
    assert_eq!(generation.outstanding_count(), 2);
    assert_eq!(initializer_calls.load(Ordering::SeqCst), 0);

    assert_eq!(
        generation.request_batch(ChunkCoord(2, 0, 0)),
        Err(RequestError::PreparationFailed(expected_error.clone())),
    );

    let mut seen = Vec::new();

    while let Some(completion) = generation.take_completed() {
        assert!(ids.contains(&completion.request_id));
        assert!(!seen.contains(&completion.request_id));
        seen.push(completion.request_id);

        assert!(matches!(
            completion.result,
            Err(GenerationError::Preparation(error))
                if error == expected_error
        ));
    }

    assert_eq!(seen.len(), 2);
    assert_eq!(generation.outstanding_count(), 0);
}

#[test]
fn preparation_panic_is_reported() {
    let pipeline = PipelineBuilder::new(config())
        .prepare(|_| -> Result<SharedData, String> {
            panic!("Planned preparation panic");
        })
        .build(|_| 0i64);

    let mut app = App::new();
    app.add_plugins(LayerGenPlugin::new(pipeline));

    app.world_mut()
        .resource_mut::<ChunkGenerator<i64>>()
        .request_batch(ChunkCoord(0, 0, 0))
        .unwrap();

    wait_for_completions::<i64>(&mut app, 1);

    let mut generation =
        app.world_mut().resource_mut::<ChunkGenerator<i64>>();

    assert_eq!(
        generation.preparation_status(),
        &PreparationStatus::Failed(PreparationError::Panicked(
            "Planned preparation panic".to_owned(),
        )),
    );

    let completion = generation.take_completed().unwrap();

    assert!(matches!(
        completion.result,
        Err(GenerationError::Preparation(PreparationError::Panicked(message)))
            if message == "Planned preparation panic"
    ));
}
