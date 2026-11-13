use std::{
    any::Any,
    collections::{BTreeMap, HashMap, VecDeque},
    error::Error,
    fmt,
    marker::PhantomData,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::Arc,
};

use bevy_app::{App, Plugin, PreUpdate};
use bevy_ecs::prelude::*;
use bevy_tasks::{futures_lite::future, AsyncComputeTaskPool, Task, TaskPool};

use crate::{
    Chunk, ChunkCoord, ChunkGenerator, ChunkRecord, ChunkStorage, EditError, GenerationError,
    GlobalPosition, LayerGenSet, LocalPosition, ManagedChunk, PersistenceError,
    PersistenceIdentity, PipelineConfig, RequestError, RequestId, StorageRevision,
};

/// Unique within one manager instance.
///
/// Load operation IDs also serve as residency tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ManagedRequestId(u64);

impl ManagedRequestId {
    pub fn value(self) -> u64 {
        self.0
    }
}

/// Count limits, not byte budgets.
///
/// Outstanding operations include queued work, running work, and unconsumed
/// manager completions.
///
/// Resident capacity includes reservations for currently valid pending loads.
/// Invalidated loads still occupy operation capacity until resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkManagerLimits {
    max_outstanding: usize,
    max_storage_jobs: usize,
    max_resident: usize,
}

impl ChunkManagerLimits {
    pub fn new(
        max_outstanding: usize,
        max_storage_jobs: usize,
        max_resident: usize,
    ) -> Result<Self, &'static str> {
        if max_outstanding == 0 || max_storage_jobs == 0 || max_resident == 0 {
            return Err("Chunk manager limits must be greater than zero");
        }

        if max_storage_jobs > max_outstanding {
            return Err("Storage job limit cannot exceed outstanding operation capacity");
        }

        Ok(Self {
            max_outstanding,
            max_storage_jobs,
            max_resident,
        })
    }

    pub fn max_outstanding(self) -> usize {
        self.max_outstanding
    }

    pub fn max_storage_jobs(self) -> usize {
        self.max_storage_jobs
    }

    pub fn max_resident(self) -> usize {
        self.max_resident
    }
}

impl Default for ChunkManagerLimits {
    fn default() -> Self {
        Self {
            max_outstanding: 16,
            max_storage_jobs: 2,
            max_resident: 64,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChunkManagerError {
    OutsideWorld,
    AlreadyLoaded,
    AlreadyLoading,
    NotLoaded,
    SaveInFlight,
    Clean,
    Dirty,
    OperationCapacity,
    ResidentCapacity,
    IdExhausted,
    Edit(EditError),
    Request(RequestError),
    Generation(GenerationError),
    Persistence(PersistenceError),
    Panicked(String),
}

impl fmt::Display for ChunkManagerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutsideWorld => write!(formatter, "Position or chunk is outside the world"),
            Self::AlreadyLoaded => write!(formatter, "Chunk is already resident"),
            Self::AlreadyLoading => write!(formatter, "Chunk already has a valid pending load"),
            Self::NotLoaded => write!(formatter, "Chunk is not resident"),
            Self::SaveInFlight => write!(formatter, "Chunk has a queued or running save"),
            Self::Clean => write!(formatter, "Chunk has no unsaved edits"),
            Self::Dirty => write!(formatter, "Chunk has unsaved edits"),
            Self::OperationCapacity => write!(formatter, "Manager operation capacity reached"),
            Self::ResidentCapacity => write!(formatter, "Manager resident capacity reached"),
            Self::IdExhausted => write!(formatter, "Manager request ID space exhausted"),
            Self::Edit(error) => write!(formatter, "{error}"),
            Self::Request(error) => write!(formatter, "{error}"),
            Self::Generation(error) => write!(formatter, "{error}"),
            Self::Persistence(error) => write!(formatter, "{error}"),
            Self::Panicked(message) => write!(formatter, "Storage task panicked: {message}"),
        }
    }
}

impl Error for ChunkManagerError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManagedOutcome {
    /// False means the load was invalidated and did not install a chunk.
    Loaded { installed: bool },

    /// The captured snapshot was saved.
    ///
    /// More recent edits may still be dirty.
    Saved {
        edit_revision: u64,
        storage_revision: StorageRevision,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedCompletion {
    pub request_id: ManagedRequestId,
    pub coord: ChunkCoord,
    pub result: Result<ManagedOutcome, ChunkManagerError>,
}

struct Resident<T, C> {
    token: ManagedRequestId,
    chunk: ManagedChunk<T, C>,
}

struct SaveSnapshot<T> {
    residency: ManagedRequestId,
    edit_revision: u64,
    expected: Option<StorageRevision>,
    record: ChunkRecord<T>,
}

enum OperationState<T, C> {
    LoadQueued,
    Generating(RequestId),
    LoadReady(Chunk<T, C>),
    Loading(Task<Result<ManagedChunk<T, C>, ChunkManagerError>>),
    SaveQueued(SaveSnapshot<T>),
    Saving {
        residency: ManagedRequestId,
        edit_revision: u64,
        task: Task<Result<StorageRevision, ChunkManagerError>>,
    },
}

impl<T, C> OperationState<T, C> {
    fn is_storage_job(&self) -> bool {
        matches!(self, Self::Loading(_) | Self::Saving { .. })
    }
}

struct Operation<T, C> {
    coord: ChunkCoord,
    state: OperationState<T, C>,
}

/// Game-facing chunk ownership, editing, and persistence.
///
/// For this voxel type, this resource exclusively owns requests/completions
/// from ChunkGenerator. Do not use the raw generator channel concurrently.
///
/// Resident data is read-only through this API. All effective edits advance
/// the managed chunk revision and update its sparse replacements.
///
/// `C` is the pipeline's chunk data type (`()` unless declared). Chunk data
/// is generated base output: it is not persisted and edits do not update it.
#[derive(Resource)]
pub struct ChunkManager<T: Clone + PartialEq + Send + Sync + 'static, C: Send + Sync + 'static = ()>
{
    config: PipelineConfig,
    identity: PersistenceIdentity,
    storage: Arc<dyn ChunkStorage<T>>,
    limits: ChunkManagerLimits,
    next_id: Option<u64>,

    residents: HashMap<ChunkCoord, Resident<T, C>>,
    active_loads: HashMap<ChunkCoord, ManagedRequestId>,
    active_saves: HashMap<ChunkCoord, ManagedRequestId>,
    operations: BTreeMap<u64, Operation<T, C>>,
    completed: VecDeque<ManagedCompletion>,
}

impl<T: Clone + PartialEq + Send + Sync + 'static, C: Send + Sync + 'static> ChunkManager<T, C> {
    fn new(
        config: PipelineConfig,
        identity: PersistenceIdentity,
        storage: Arc<dyn ChunkStorage<T>>,
        limits: ChunkManagerLimits,
    ) -> Self {
        Self {
            config,
            identity,
            storage,
            limits,
            next_id: Some(1),
            residents: HashMap::new(),
            active_loads: HashMap::new(),
            active_saves: HashMap::new(),
            operations: BTreeMap::new(),
            completed: VecDeque::new(),
        }
    }

    pub fn config(&self) -> PipelineConfig {
        self.config
    }

    pub fn identity(&self) -> &PersistenceIdentity {
        &self.identity
    }

    pub fn limits(&self) -> ChunkManagerLimits {
        self.limits
    }

    pub fn resident_count(&self) -> usize {
        self.residents.len()
    }

    pub fn pending_count(&self) -> usize {
        self.operations.len()
    }

    pub fn completed_count(&self) -> usize {
        self.completed.len()
    }

    pub fn outstanding_count(&self) -> usize {
        self.operations.len() + self.completed.len()
    }

    pub fn storage_jobs_count(&self) -> usize {
        self.operations
            .values()
            .filter(|operation| operation.state.is_storage_job())
            .count()
    }

    pub fn is_loading(&self, coord: ChunkCoord) -> bool {
        self.active_loads.contains_key(&coord)
    }

    pub fn is_saving(&self, coord: ChunkCoord) -> bool {
        self.active_saves.contains_key(&coord)
    }

    pub fn chunk(&self, coord: ChunkCoord) -> Option<&ManagedChunk<T, C>> {
        self.residents.get(&coord).map(|resident| &resident.chunk)
    }

    pub fn resident_coords(&self) -> impl Iterator<Item = ChunkCoord> + '_ {
        self.residents.keys().copied()
    }

    /// None means outside-world or not resident.
    ///
    /// Use config().locate(position) to distinguish those cases.
    pub fn get_global(&self, position: GlobalPosition) -> Option<&T> {
        let (coord, local) = self.config.locate(position)?;

        self.chunk(coord)?.data().get(local.0, local.1, local.2)
    }

    pub fn set_local(
        &mut self,
        coord: ChunkCoord,
        local: LocalPosition,
        value: T,
    ) -> Result<bool, ChunkManagerError> {
        self.residents
            .get_mut(&coord)
            .ok_or(ChunkManagerError::NotLoaded)?
            .chunk
            .set_local(local, value)
            .map_err(ChunkManagerError::Edit)
    }

    pub fn set_global(
        &mut self,
        position: GlobalPosition,
        value: T,
    ) -> Result<bool, ChunkManagerError> {
        let (coord, local) = self
            .config
            .locate(position)
            .ok_or(ChunkManagerError::OutsideWorld)?;

        self.set_local(coord, local, value)
    }

    pub fn request_load(
        &mut self,
        coord: ChunkCoord,
    ) -> Result<ManagedRequestId, ChunkManagerError> {
        self.config
            .context(coord)
            .map_err(|_| ChunkManagerError::OutsideWorld)?;

        if self.residents.contains_key(&coord) {
            return Err(ChunkManagerError::AlreadyLoaded);
        }

        if self.active_loads.contains_key(&coord) {
            return Err(ChunkManagerError::AlreadyLoading);
        }

        if self.residents.len() + self.active_loads.len() >= self.limits.max_resident {
            return Err(ChunkManagerError::ResidentCapacity);
        }

        let id = self.allocate_id()?;

        self.active_loads.insert(coord, id);
        self.operations.insert(
            id.value(),
            Operation {
                coord,
                state: OperationState::LoadQueued,
            },
        );

        Ok(id)
    }

    /// Captures current sparse replacements immediately.
    ///
    /// Only one save may be queued/running per chunk. Editing can continue.
    /// Successful acknowledgement marks only the captured edit revision saved.
    pub fn request_save(
        &mut self,
        coord: ChunkCoord,
    ) -> Result<ManagedRequestId, ChunkManagerError> {
        if self.active_saves.contains_key(&coord) {
            return Err(ChunkManagerError::SaveInFlight);
        }

        let resident = self
            .residents
            .get(&coord)
            .ok_or(ChunkManagerError::NotLoaded)?;

        if !resident.chunk.is_dirty() {
            return Err(ChunkManagerError::Clean);
        }

        // Check capacity before cloning the snapshot.
        self.check_capacity()?;

        let snapshot = SaveSnapshot {
            residency: resident.token,
            edit_revision: resident.chunk.edit_revision(),
            expected: resident.chunk.storage_revision(),
            record: resident.chunk.snapshot_record(),
        };

        let id = self.allocate_id()?;

        self.active_saves.insert(coord, id);
        self.operations.insert(
            id.value(),
            Operation {
                coord,
                state: OperationState::SaveQueued(snapshot),
            },
        );

        Ok(id)
    }

    /// Unloads a clean resident chunk.
    ///
    /// Pending loads are invalidated through discard(), not this method.
    pub fn unload(&mut self, coord: ChunkCoord) -> Result<(), ChunkManagerError> {
        if self.active_saves.contains_key(&coord) {
            return Err(ChunkManagerError::SaveInFlight);
        }

        let resident = self
            .residents
            .get(&coord)
            .ok_or(ChunkManagerError::NotLoaded)?;

        if resident.chunk.is_dirty() {
            return Err(ChunkManagerError::Dirty);
        }

        self.residents.remove(&coord);
        Ok(())
    }

    /// Explicitly drops unsaved resident edits or invalidates a pending load.
    ///
    /// Does not undo saved records.
    /// Does not stop an already-running generation/storage task.
    ///
    /// Rejected while saving, because a running save could still commit after
    /// discard and race a new residency.
    pub fn discard(&mut self, coord: ChunkCoord) -> Result<(), ChunkManagerError> {
        if self.active_saves.contains_key(&coord) {
            return Err(ChunkManagerError::SaveInFlight);
        }

        let removed_resident = self.residents.remove(&coord).is_some();
        let removed_load = self.active_loads.remove(&coord).is_some();

        if !removed_resident && !removed_load {
            return Err(ChunkManagerError::NotLoaded);
        }

        Ok(())
    }

    /// Releases one manager operation slot.
    pub fn take_completed(&mut self) -> Option<ManagedCompletion> {
        self.completed.pop_front()
    }

    fn check_capacity(&self) -> Result<(), ChunkManagerError> {
        if self.outstanding_count() >= self.limits.max_outstanding {
            return Err(ChunkManagerError::OperationCapacity);
        }

        Ok(())
    }

    fn allocate_id(&mut self) -> Result<ManagedRequestId, ChunkManagerError> {
        self.check_capacity()?;

        let value = self.next_id.ok_or(ChunkManagerError::IdExhausted)?;
        self.next_id = value.checked_add(1);

        Ok(ManagedRequestId(value))
    }

    fn load_is_current(&self, coord: ChunkCoord, id: ManagedRequestId) -> bool {
        self.active_loads.get(&coord) == Some(&id)
    }

    fn finish(
        &mut self,
        id: ManagedRequestId,
        coord: ChunkCoord,
        result: Result<ManagedOutcome, ChunkManagerError>,
    ) {
        // Never clear a newer load's token when an old operation finishes.
        if self.active_loads.get(&coord) == Some(&id) {
            self.active_loads.remove(&coord);
        }

        if self.active_saves.get(&coord) == Some(&id) {
            self.active_saves.remove(&coord);
        }

        self.completed.push_back(ManagedCompletion {
            request_id: id,
            coord,
            result,
        });
    }
}

/// Install after LayerGenPlugin<T, C> (same cell and chunk data types).
///
/// This plugin consumes all raw generation completions for T.
/// Do not concurrently request/consume ChunkGenerator<T> from game systems.
///
/// Synchronous backend methods run on background compute-pool tasks.
/// This is the initial native execution model; a blocking backend can occupy
/// pool workers. Dedicated I/O scheduling is not implemented yet.
pub struct ChunkManagerPlugin<
    T: Clone + PartialEq + Send + Sync + 'static,
    C: Send + Sync + 'static = (),
> {
    identity: PersistenceIdentity,
    storage: Arc<dyn ChunkStorage<T>>,
    limits: ChunkManagerLimits,
    _marker: PhantomData<fn() -> C>,
}

impl<T: Clone + PartialEq + Send + Sync + 'static> ChunkManagerPlugin<T, ()> {
    /// Manager for a pipeline without chunk data.
    pub fn new(identity: PersistenceIdentity, storage: Arc<dyn ChunkStorage<T>>) -> Self {
        Self::with_chunk_data(identity, storage)
    }
}

impl<T: Clone + PartialEq + Send + Sync + 'static, C: Send + Sync + 'static>
    ChunkManagerPlugin<T, C>
{
    /// Manager for a pipeline declaring chunk data `C`.
    ///
    /// Name `C` explicitly, e.g. `ChunkManagerPlugin::<Voxel, Climate>::with_chunk_data`.
    pub fn with_chunk_data(
        identity: PersistenceIdentity,
        storage: Arc<dyn ChunkStorage<T>>,
    ) -> Self {
        Self {
            identity,
            storage,
            limits: ChunkManagerLimits::default(),
            _marker: PhantomData,
        }
    }

    pub fn with_limits(mut self, limits: ChunkManagerLimits) -> Self {
        self.limits = limits;
        self
    }
}

impl<T: Clone + PartialEq + Send + Sync + 'static, C: Send + Sync + 'static> Plugin
    for ChunkManagerPlugin<T, C>
{
    fn build(&self, app: &mut App) {
        assert!(
            !app.world().contains_resource::<ChunkManager<T, C>>(),
            "Only one ChunkManager per voxel and chunk data type may be installed"
        );

        let generator = app
            .world()
            .get_resource::<ChunkGenerator<T, C>>()
            .expect(
                "Install LayerGenPlugin before ChunkManagerPlugin, \n                 with matching cell and chunk data types",
            );

        assert_eq!(
            generator.outstanding_count(),
            0,
            "Install ChunkManagerPlugin before submitting raw generation requests"
        );

        let config = generator.config();

        app.insert_resource(ChunkManager::<T, C>::new(
            config,
            self.identity.clone(),
            Arc::clone(&self.storage),
            self.limits,
        ));

        app.add_systems(
            PreUpdate,
            advance_manager::<T, C>.after(LayerGenSet::Collect),
        );
    }
}

fn advance_manager<T: Clone + PartialEq + Send + Sync + 'static, C: Send + Sync + 'static>(
    mut manager: ResMut<ChunkManager<T, C>>,
    mut generator: ResMut<ChunkGenerator<T, C>>,
) {
    // First transfer completed base generation to the corresponding manager
    // operation. This releases the raw generator's outstanding slots.
    while let Some(completion) = generator.take_completed() {
        let operation_id = manager
            .operations
            .iter()
            .find_map(|(id, operation)| match &operation.state {
                OperationState::Generating(request) if *request == completion.request_id => {
                    Some(*id)
                }
                _ => None,
            })
            .expect(
                "ChunkManager received an unowned generation completion; \
                 do not use the raw generator channel while managed",
            );

        let mut operation = manager
            .operations
            .remove(&operation_id)
            .expect("Generation operation must exist");

        let id = ManagedRequestId(operation_id);

        if !manager.load_is_current(operation.coord, id) {
            manager.finish(
                id,
                operation.coord,
                Ok(ManagedOutcome::Loaded { installed: false }),
            );
            continue;
        }

        match completion.result {
            Ok(base) => {
                operation.state = OperationState::LoadReady(base);
                manager.operations.insert(operation_id, operation);
            }
            Err(error) => {
                manager.finish(
                    id,
                    operation.coord,
                    Err(ChunkManagerError::Generation(error)),
                );
            }
        }
    }

    // Poll existing jobs before dispatching new jobs. Collecting all available
    // results first prevents queued work from missing newly released slots.
    let running_ids: Vec<_> = manager
        .operations
        .iter()
        .filter(|(_, operation)| operation.state.is_storage_job())
        .map(|(id, _)| *id)
        .collect();

    for operation_id in running_ids {
        let mut operation = manager
            .operations
            .remove(&operation_id)
            .expect("Storage operation must exist");

        let id = ManagedRequestId(operation_id);

        let result = match &mut operation.state {
            OperationState::Loading(task) => match future::block_on(future::poll_once(task)) {
                Some(result) => {
                    if !manager.load_is_current(operation.coord, id) {
                        Some(Ok(ManagedOutcome::Loaded { installed: false }))
                    } else {
                        Some(result.map(|chunk| {
                            manager
                                .residents
                                .insert(operation.coord, Resident { token: id, chunk });

                            ManagedOutcome::Loaded { installed: true }
                        }))
                    }
                }
                None => None,
            },
            OperationState::Saving {
                residency,
                edit_revision,
                task,
            } => future::block_on(future::poll_once(task)).map(|result| {
                result.and_then(|revision| {
                    let resident = manager
                        .residents
                        .get_mut(&operation.coord)
                        .filter(|resident| resident.token == *residency)
                        .ok_or_else(|| {
                            ChunkManagerError::Persistence(PersistenceError::InvalidRecord(
                                "Save residency token mismatch".to_owned(),
                            ))
                        })?;

                    resident
                        .chunk
                        .acknowledge_save(*edit_revision, revision)
                        .map_err(ChunkManagerError::Persistence)?;

                    Ok(ManagedOutcome::Saved {
                        edit_revision: *edit_revision,
                        storage_revision: revision,
                    })
                })
            }),
            _ => unreachable!("Only running storage operations are polled"),
        };

        if let Some(result) = result {
            manager.finish(id, operation.coord, result);
        } else {
            manager.operations.insert(operation_id, operation);
        }
    }

    let mut storage_jobs = manager.storage_jobs_count();

    // Ordered by request ID for predictable first-in dispatch.
    let pending_ids: Vec<_> = manager.operations.keys().copied().collect();

    for operation_id in pending_ids {
        let operation = manager
            .operations
            .remove(&operation_id)
            .expect("Pending operation must exist");

        let id = ManagedRequestId(operation_id);
        let coord = operation.coord;

        match operation.state {
            OperationState::LoadQueued => {
                if !manager.load_is_current(coord, id) {
                    manager.finish(id, coord, Ok(ManagedOutcome::Loaded { installed: false }));
                    continue;
                }

                match generator.request_batch(coord) {
                    Ok(request) => {
                        manager.operations.insert(
                            operation_id,
                            Operation {
                                coord,
                                state: OperationState::Generating(request),
                            },
                        );
                    }
                    Err(RequestError::CapacityReached { .. })
                    | Err(RequestError::AlreadyOutstanding(_)) => {
                        // An invalidated older generation can still reserve
                        // this coordinate until its completion is collected.
                        manager.operations.insert(
                            operation_id,
                            Operation {
                                coord,
                                state: OperationState::LoadQueued,
                            },
                        );
                    }
                    Err(error) => {
                        manager.finish(id, coord, Err(ChunkManagerError::Request(error)));
                    }
                }
            }
            OperationState::LoadReady(base) => {
                if !manager.load_is_current(coord, id) {
                    manager.finish(id, coord, Ok(ManagedOutcome::Loaded { installed: false }));
                    continue;
                }

                if storage_jobs >= manager.limits.max_storage_jobs {
                    manager.operations.insert(
                        operation_id,
                        Operation {
                            coord,
                            state: OperationState::LoadReady(base),
                        },
                    );
                    continue;
                }

                let storage = Arc::clone(&manager.storage);
                let identity = manager.identity.clone();

                let key = crate::ChunkKey {
                    world: identity.world.clone(),
                    coord,
                };

                let pool = AsyncComputeTaskPool::get_or_init(TaskPool::new);

                let task = pool.spawn(async move {
                    run_storage_job(|| {
                        let stored = storage.load(&key).map_err(ChunkManagerError::Persistence)?;

                        ManagedChunk::from_base(base, identity, stored)
                            .map_err(ChunkManagerError::Persistence)
                    })
                });

                storage_jobs += 1;

                manager.operations.insert(
                    operation_id,
                    Operation {
                        coord,
                        state: OperationState::Loading(task),
                    },
                );
            }
            OperationState::SaveQueued(snapshot) => {
                if storage_jobs >= manager.limits.max_storage_jobs {
                    manager.operations.insert(
                        operation_id,
                        Operation {
                            coord,
                            state: OperationState::SaveQueued(snapshot),
                        },
                    );
                    continue;
                }

                let storage = Arc::clone(&manager.storage);
                let pool = AsyncComputeTaskPool::get_or_init(TaskPool::new);

                let residency = snapshot.residency;
                let edit_revision = snapshot.edit_revision;

                let task = pool.spawn(async move {
                    run_storage_job(|| {
                        let key = snapshot.record.key();

                        storage
                            .compare_exchange(&key, snapshot.expected, snapshot.record)
                            .map_err(ChunkManagerError::Persistence)
                    })
                });

                storage_jobs += 1;

                manager.operations.insert(
                    operation_id,
                    Operation {
                        coord,
                        state: OperationState::Saving {
                            residency,
                            edit_revision,
                            task,
                        },
                    },
                );
            }
            state => {
                manager
                    .operations
                    .insert(operation_id, Operation { coord, state });
            }
        }
    }
}

fn run_storage_job<R>(
    job: impl FnOnce() -> Result<R, ChunkManagerError>,
) -> Result<R, ChunkManagerError> {
    match catch_unwind(AssertUnwindSafe(job)) {
        Ok(result) => result,
        Err(payload) => Err(ChunkManagerError::Panicked(panic_message(payload))),
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
