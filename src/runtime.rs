use std::{
    any::Any,
    collections::{HashSet, VecDeque},
    error::Error,
    fmt,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::Arc,
};

use crate::{Chunk, ChunkCoord, GenerationDefinition, PipelineConfig};
use bevy_app::{App, Plugin, PostUpdate, PreUpdate};
use bevy_ecs::prelude::*;
use bevy_tasks::{futures_lite::future, AsyncComputeTaskPool, Task, TaskPool};

type Worker<T, C> = dyn Fn(ChunkCoord) -> Result<Chunk<T, C>, String> + Send + Sync + 'static;

type PreparationWorker<T, C> =
    dyn Fn() -> Result<Arc<Worker<T, C>>, String> + Send + Sync + 'static;

type PreparationResult<T, C> = Result<Arc<Worker<T, C>>, PreparationError>;

enum RuntimeSource<T, C> {
    Ready(Arc<Worker<T, C>>),
    Prepare(Arc<PreparationWorker<T, C>>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RequestId(u64);

impl RequestId {
    pub fn value(self) -> u64 {
        self.0
    }
}

/// Count limits, not byte limits.
///
/// Outstanding includes queued, running, and unconsumed completed requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GenerationLimits {
    max_in_flight: usize,
    max_outstanding: usize,
}

impl GenerationLimits {
    pub fn new(max_in_flight: usize, max_outstanding: usize) -> Result<Self, &'static str> {
        if max_in_flight == 0 {
            return Err("Maximum in-flight jobs must be greater than zero");
        }

        if max_outstanding == 0 {
            return Err("Maximum outstanding requests must be greater than zero");
        }

        if max_in_flight > max_outstanding {
            return Err("Maximum in-flight jobs cannot exceed outstanding capacity");
        }

        Ok(Self {
            max_in_flight,
            max_outstanding,
        })
    }

    pub fn max_in_flight(self) -> usize {
        self.max_in_flight
    }

    pub fn max_outstanding(self) -> usize {
        self.max_outstanding
    }
}

impl Default for GenerationLimits {
    fn default() -> Self {
        Self {
            max_in_flight: 2,
            max_outstanding: 16,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreparationError {
    Processing(String),
    Panicked(String),
}

impl fmt::Display for PreparationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Processing(message) => {
                write!(formatter, "Preparation failed: {message}")
            }
            Self::Panicked(message) => {
                write!(formatter, "Preparation panicked: {message}")
            }
        }
    }
}

impl Error for PreparationError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreparationStatus {
    Unprepared,
    Preparing,
    Ready,
    Failed(PreparationError),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestError {
    OutsideWorld(ChunkCoord),
    AlreadyOutstanding(ChunkCoord),
    CapacityReached { limit: usize },
    PreparationFailed(PreparationError),
    IdExhausted,
}

impl fmt::Display for RequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutsideWorld(coord) => {
                write!(formatter, "Chunk {coord:?} is outside world bounds")
            }
            Self::AlreadyOutstanding(coord) => {
                write!(
                    formatter,
                    "Chunk {coord:?} already has an outstanding request"
                )
            }
            Self::CapacityReached { limit } => {
                write!(formatter, "Outstanding request capacity reached ({limit})")
            }
            Self::PreparationFailed(error) => {
                write!(formatter, "{error}")
            }
            Self::IdExhausted => {
                write!(formatter, "Request ID space exhausted")
            }
        }
    }
}

impl Error for RequestError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GenerationError {
    Processing(String),
    Panicked(String),
    Preparation(PreparationError),
}

impl fmt::Display for GenerationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Processing(message) => {
                write!(formatter, "Generation failed: {message}")
            }
            Self::Panicked(message) => {
                write!(formatter, "Generation panicked: {message}")
            }
            Self::Preparation(error) => {
                write!(formatter, "{error}")
            }
        }
    }
}

impl Error for GenerationError {}

#[derive(Debug)]
pub struct ChunkCompletion<T, C = ()> {
    pub request_id: RequestId,
    pub coord: ChunkCoord,
    pub result: Result<Chunk<T, C>, GenerationError>,
}

#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LayerGenSet {
    Collect,
    Dispatch,
}

struct QueuedRequest {
    request_id: RequestId,
    coord: ChunkCoord,
}

struct RunningRequest<T, C> {
    request_id: RequestId,
    coord: ChunkCoord,
    task: Task<Result<Chunk<T, C>, GenerationError>>,
}

#[derive(Resource)]
pub struct ChunkGenerator<T: Send + Sync + 'static, C: Send + Sync + 'static = ()> {
    config: PipelineConfig,
    limits: GenerationLimits,

    preparation_status: PreparationStatus,
    preparation_factory: Option<Arc<PreparationWorker<T, C>>>,
    preparation_task: Option<Task<PreparationResult<T, C>>>,
    worker: Option<Arc<Worker<T, C>>>,

    next_id: Option<u64>,
    outstanding_coords: HashSet<ChunkCoord>,
    queued: VecDeque<QueuedRequest>,
    running: Vec<RunningRequest<T, C>>,
    completed: VecDeque<ChunkCompletion<T, C>>,
}

impl<T: Send + Sync + 'static, C: Send + Sync + 'static> ChunkGenerator<T, C> {
    fn new(config: PipelineConfig, limits: GenerationLimits, source: &RuntimeSource<T, C>) -> Self {
        let (preparation_status, preparation_factory, worker) = match source {
            RuntimeSource::Ready(worker) => {
                (PreparationStatus::Ready, None, Some(Arc::clone(worker)))
            }
            RuntimeSource::Prepare(factory) => (
                PreparationStatus::Unprepared,
                Some(Arc::clone(factory)),
                None,
            ),
        };

        Self {
            config,
            limits,
            preparation_status,
            preparation_factory,
            preparation_task: None,
            worker,
            next_id: Some(1),
            outstanding_coords: HashSet::new(),
            queued: VecDeque::new(),
            running: Vec::new(),
            completed: VecDeque::new(),
        }
    }

    pub fn config(&self) -> PipelineConfig {
        self.config
    }

    pub fn limits(&self) -> GenerationLimits {
        self.limits
    }

    pub fn preparation_status(&self) -> &PreparationStatus {
        &self.preparation_status
    }

    /// Accepts one chunk request.
    ///
    /// Requests may queue while preparation is pending.
    /// A failed preparation permanently rejects new requests.
    ///
    /// Duplicate coordinates remain reserved until their completion
    /// is consumed, including failed completions.
    pub fn request_batch(&mut self, coord: ChunkCoord) -> Result<RequestId, RequestError> {
        if self.config.context(coord).is_err() {
            return Err(RequestError::OutsideWorld(coord));
        }

        if let PreparationStatus::Failed(error) = &self.preparation_status {
            return Err(RequestError::PreparationFailed(error.clone()));
        }

        if self.outstanding_coords.contains(&coord) {
            return Err(RequestError::AlreadyOutstanding(coord));
        }

        if self.outstanding_count() >= self.limits.max_outstanding {
            return Err(RequestError::CapacityReached {
                limit: self.limits.max_outstanding,
            });
        }

        let value = self.next_id.ok_or(RequestError::IdExhausted)?;
        let request_id = RequestId(value);

        self.next_id = value.checked_add(1);
        self.outstanding_coords.insert(coord);
        self.queued.push_back(QueuedRequest { request_id, coord });

        Ok(request_id)
    }

    /// Removes a completion and releases its outstanding request slot.
    ///
    /// Completion order is unspecified.
    pub fn take_completed(&mut self) -> Option<ChunkCompletion<T, C>> {
        let completion = self.completed.pop_front()?;
        self.outstanding_coords.remove(&completion.coord);
        Some(completion)
    }

    pub fn queued_count(&self) -> usize {
        self.queued.len()
    }

    /// Chunk jobs only; excludes the preparation job.
    ///
    /// Includes finished chunk tasks not yet collected.
    pub fn in_flight_count(&self) -> usize {
        self.running.len()
    }

    pub fn completed_count(&self) -> usize {
        self.completed.len()
    }

    pub fn outstanding_count(&self) -> usize {
        self.outstanding_coords.len()
    }
}

/// One pipeline per voxel type and chunk data type per Bevy world.
///
/// `C` is the definition's chunk data type (`()` unless declared).
/// The plugin owns the definition. Prepared data is retained immutably
/// by the ready worker and shared between chunk jobs.
pub struct LayerGenPlugin<T: Send + Sync + 'static, C: Send + Sync + 'static = ()> {
    config: PipelineConfig,
    limits: GenerationLimits,
    source: RuntimeSource<T, C>,
}

impl<T: Send + Sync + 'static, C: Send + Sync + 'static> LayerGenPlugin<T, C> {
    pub fn new<P>(pipeline: P) -> Self
    where
        P: GenerationDefinition<T, ChunkData = C>,
    {
        let config = pipeline.config();
        let requires_preparation = pipeline.requires_preparation();
        let pipeline = Arc::new(pipeline);

        let source = if requires_preparation {
            let factory: Arc<PreparationWorker<T, C>> = Arc::new(move || {
                let shared = pipeline.prepare_shared()?;
                let pipeline = Arc::clone(&pipeline);

                let worker: Arc<Worker<T, C>> = Arc::new(move |coord| {
                    pipeline.generate_chunk_with_shared(coord, shared.as_ref())
                });

                Ok(worker)
            });

            RuntimeSource::Prepare(factory)
        } else {
            // Only obtains already-ready shared data.
            let shared = pipeline
                .prepare_shared()
                .expect("Ready shared data must be available");

            let worker: Arc<Worker<T, C>> =
                Arc::new(move |coord| pipeline.generate_chunk_with_shared(coord, shared.as_ref()));

            RuntimeSource::Ready(worker)
        };

        Self {
            config,
            limits: GenerationLimits::default(),
            source,
        }
    }

    pub fn with_limits(mut self, limits: GenerationLimits) -> Self {
        self.limits = limits;
        self
    }
}

impl<T: Send + Sync + 'static, C: Send + Sync + 'static> Plugin for LayerGenPlugin<T, C> {
    fn build(&self, app: &mut App) {
        assert!(
            !app.world().contains_resource::<ChunkGenerator<T, C>>(),
            "Only one LayerGen pipeline per voxel and chunk data type may be installed"
        );

        app.insert_resource(ChunkGenerator::new(self.config, self.limits, &self.source));

        app.add_systems(
            PreUpdate,
            collect_completed::<T, C>.in_set(LayerGenSet::Collect),
        );

        app.add_systems(
            PostUpdate,
            dispatch_requests::<T, C>.in_set(LayerGenSet::Dispatch),
        );
    }
}

fn dispatch_requests<T: Send + Sync + 'static, C: Send + Sync + 'static>(
    mut generator: ResMut<ChunkGenerator<T, C>>,
) {
    if matches!(generator.preparation_status, PreparationStatus::Unprepared) {
        let factory = generator
            .preparation_factory
            .take()
            .expect("Unprepared generator must have a preparation factory");

        let pool = AsyncComputeTaskPool::get_or_init(TaskPool::new);

        generator.preparation_task = Some(pool.spawn(async move {
            match catch_unwind(AssertUnwindSafe(|| factory())) {
                Ok(result) => result.map_err(PreparationError::Processing),
                Err(payload) => Err(PreparationError::Panicked(panic_message(payload))),
            }
        }));

        generator.preparation_status = PreparationStatus::Preparing;
    }

    if !matches!(generator.preparation_status, PreparationStatus::Ready) {
        return;
    }

    if generator.queued.is_empty() || generator.running.len() >= generator.limits.max_in_flight {
        return;
    }

    let pool = AsyncComputeTaskPool::get_or_init(TaskPool::new);

    let worker = Arc::clone(
        generator
            .worker
            .as_ref()
            .expect("Ready generator must have a worker"),
    );

    while generator.running.len() < generator.limits.max_in_flight {
        let Some(request) = generator.queued.pop_front() else {
            break;
        };

        let worker = Arc::clone(&worker);
        let coord = request.coord;

        let task = pool.spawn(async move {
            match catch_unwind(AssertUnwindSafe(|| worker(coord))) {
                Ok(result) => result.map_err(GenerationError::Processing),
                Err(payload) => Err(GenerationError::Panicked(panic_message(payload))),
            }
        });

        generator.running.push(RunningRequest {
            request_id: request.request_id,
            coord,
            task,
        });
    }
}

fn collect_completed<T: Send + Sync + 'static, C: Send + Sync + 'static>(
    mut generator: ResMut<ChunkGenerator<T, C>>,
) {
    let preparation_result = generator
        .preparation_task
        .as_mut()
        .and_then(|task| future::block_on(future::poll_once(task)));

    if let Some(result) = preparation_result {
        generator.preparation_task = None;

        match result {
            Ok(worker) => {
                generator.worker = Some(worker);
                generator.preparation_status = PreparationStatus::Ready;
            }
            Err(error) => {
                generator.preparation_status = PreparationStatus::Failed(error.clone());

                // No chunk jobs have been dispatched before readiness.
                // Resolve every accepted queued request as a failure.
                while let Some(request) = generator.queued.pop_front() {
                    generator.completed.push_back(ChunkCompletion {
                        request_id: request.request_id,
                        coord: request.coord,
                        result: Err(GenerationError::Preparation(error.clone())),
                    });
                }
            }
        }
    }

    let mut index = 0;

    while index < generator.running.len() {
        // poll_once returns immediately; this does not wait for generation.
        let result = future::block_on(future::poll_once(&mut generator.running[index].task));

        match result {
            Some(result) => {
                let request = generator.running.swap_remove(index);

                generator.completed.push_back(ChunkCompletion {
                    request_id: request.request_id,
                    coord: request.coord,
                    result,
                });
            }
            None => {
                index += 1;
            }
        }
    }
}

fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_owned()
    } else {
        "Non-string panic payload".to_owned()
    }
}
