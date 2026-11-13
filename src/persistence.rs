use std::{
    collections::{BTreeMap, HashMap},
    error::Error,
    fmt,
    sync::Mutex,
};

use crate::{
    Chunk, ChunkCoord, DataSet, GlobalPosition, LocalPosition, PipelineConfig, StepContext,
};

/// Consumer-defined identity of a persistent world.
///
/// Must be stable across application restarts.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WorldId(pub String);

/// Consumer-defined identity of compatible generation behavior.
///
/// Change this when algorithm changes make previous replacements incompatible.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GenerationId(pub String);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistenceIdentity {
    pub world: WorldId,
    pub generation: GenerationId,
}

/// Generation identity deliberately is not part of the lookup key.
///
/// An incompatible existing record must be rejected, not treated as absent.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ChunkKey {
    pub world: WorldId,
    pub coord: ChunkCoord,
}

/// A monotonically increasing revision assigned by storage.
///
/// None represents an absent record. Existing records start at revision 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorageRevision(u64);

impl StorageRevision {
    pub fn new(value: u64) -> Result<Self, PersistenceError> {
        if value == 0 {
            return Err(PersistenceError::InvalidRecord(
                "Storage revisions must be greater than zero".to_owned(),
            ));
        }

        Ok(Self(value))
    }

    pub fn value(self) -> u64 {
        self.0
    }
}

/// Latest replacement state, not an edit log.
///
/// Indices follow DataSet's linear indexing order.
/// Storage revision is supplied separately by the backend.
#[derive(Clone, Debug)]
pub struct ChunkRecord<T> {
    pub identity: PersistenceIdentity,
    pub config: PipelineConfig,
    pub coord: ChunkCoord,
    pub dimensions: (usize, usize, usize),
    pub replacements: BTreeMap<usize, T>,
}

impl<T> ChunkRecord<T> {
    pub fn key(&self) -> ChunkKey {
        ChunkKey {
            world: self.identity.world.clone(),
            coord: self.coord,
        }
    }

    /// Validates the record's internal spatial consistency.
    ///
    /// Backends should also impose their own decoded-size limits.
    pub fn validate(&self) -> Result<(), PersistenceError> {
        let context = self
            .config
            .context(self.coord)
            .map_err(|message| PersistenceError::InvalidRecord(message.to_owned()))?;

        if self.dimensions != context.dimensions() {
            return Err(PersistenceError::InvalidRecord(
                "Stored dimensions do not match the configured chunk".to_owned(),
            ));
        }

        let volume = self
            .dimensions
            .0
            .checked_mul(self.dimensions.1)
            .and_then(|area| area.checked_mul(self.dimensions.2))
            .ok_or_else(|| {
                PersistenceError::InvalidRecord("Stored chunk volume overflow".to_owned())
            })?;

        if self.replacements.keys().any(|index| *index >= volume) {
            return Err(PersistenceError::InvalidRecord(
                "Replacement index is outside the chunk".to_owned(),
            ));
        }

        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct StoredChunk<T> {
    pub revision: StorageRevision,
    pub record: ChunkRecord<T>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PersistenceError {
    Backend(String),
    InvalidRecord(String),
    IdentityMismatch,
    ConfigurationMismatch,
    Conflict {
        expected: Option<StorageRevision>,
        actual: Option<StorageRevision>,
    },
    RevisionExhausted,
}

impl fmt::Display for PersistenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend(message) => {
                write!(formatter, "Persistence backend failed: {message}")
            }
            Self::InvalidRecord(message) => {
                write!(formatter, "Invalid chunk record: {message}")
            }
            Self::IdentityMismatch => {
                write!(formatter, "Saved world or generation identity mismatch")
            }
            Self::ConfigurationMismatch => {
                write!(formatter, "Saved pipeline configuration mismatch")
            }
            Self::Conflict { expected, actual } => {
                write!(
                    formatter,
                    "Storage revision conflict: expected {expected:?}, \
                     found {actual:?}"
                )
            }
            Self::RevisionExhausted => {
                write!(formatter, "Storage revision space exhausted")
            }
        }
    }
}

impl Error for PersistenceError {}

/// Consumer-provided storage.
///
/// These synchronous methods are intended to be called from background jobs,
/// not directly from Bevy frame systems when the backend may block.
///
/// Required backend guarantees:
///
/// - load returns a consistent record/revision pair.
/// - compare_exchange atomically checks and replaces one record.
/// - successful writes return a strictly newer revision.
/// - conflicts do not modify stored state.
/// - empty replacement records preserve revision history.
/// - errors may have an indeterminate commit outcome; callers must not assume
///   an error proves nothing was written.
///
/// Durability across process exit depends on the backend.
pub trait ChunkStorage<T>: Send + Sync + 'static {
    fn load(&self, key: &ChunkKey) -> Result<Option<StoredChunk<T>>, PersistenceError>;

    fn compare_exchange(
        &self,
        key: &ChunkKey,
        expected: Option<StorageRevision>,
        record: ChunkRecord<T>,
    ) -> Result<StorageRevision, PersistenceError>;
}

/// Test/example backend. Does not survive process exit.
///
/// One mutex protects the revision comparison and replacement atomically.
pub struct MemoryChunkStorage<T> {
    records: Mutex<HashMap<ChunkKey, StoredChunk<T>>>,
}

impl<T> Default for MemoryChunkStorage<T> {
    fn default() -> Self {
        Self {
            records: Mutex::new(HashMap::new()),
        }
    }
}

impl<T> ChunkStorage<T> for MemoryChunkStorage<T>
where
    T: Clone + Send + 'static,
{
    fn load(&self, key: &ChunkKey) -> Result<Option<StoredChunk<T>>, PersistenceError> {
        let records = self
            .records
            .lock()
            .map_err(|_| PersistenceError::Backend("Storage mutex poisoned".to_owned()))?;

        Ok(records.get(key).cloned())
    }

    fn compare_exchange(
        &self,
        key: &ChunkKey,
        expected: Option<StorageRevision>,
        record: ChunkRecord<T>,
    ) -> Result<StorageRevision, PersistenceError> {
        record.validate()?;

        if record.key() != *key {
            return Err(PersistenceError::InvalidRecord(
                "Record does not match its storage key".to_owned(),
            ));
        }

        let mut records = self
            .records
            .lock()
            .map_err(|_| PersistenceError::Backend("Storage mutex poisoned".to_owned()))?;

        let actual = records.get(key).map(|stored| stored.revision);

        if actual != expected {
            return Err(PersistenceError::Conflict { expected, actual });
        }

        // Reject replacement of incompatible existing data even when the
        // caller happens to know its storage revision.
        if let Some(existing) = records.get(key) {
            if existing.record.identity != record.identity {
                return Err(PersistenceError::IdentityMismatch);
            }

            if existing.record.config != record.config {
                return Err(PersistenceError::ConfigurationMismatch);
            }
        }

        let next = actual
            .map_or(0, StorageRevision::value)
            .checked_add(1)
            .ok_or(PersistenceError::RevisionExhausted)?;

        let revision = StorageRevision::new(next)?;

        records.insert(key.clone(), StoredChunk { revision, record });

        Ok(revision)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditError {
    OutsideChunk,
    RevisionExhausted,
}

impl fmt::Display for EditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutsideChunk => {
                write!(formatter, "Edit position is outside the chunk")
            }
            Self::RevisionExhausted => {
                write!(formatter, "Edit revision space exhausted")
            }
        }
    }
}

impl Error for EditError {}

/// A resident chunk with immutable generated base and tracked replacements.
///
/// No mutable dataset access is exposed.
///
/// Clone must produce independent editable cell values. Using shared interior
/// mutability inside T can bypass editing/revision tracking and is unsupported
/// by this managed-state contract.
///
/// Chunk data `C` is the generated base chunk data. It is never persisted
/// and is not updated by edits.
pub struct ManagedChunk<T, C = ()> {
    identity: PersistenceIdentity,
    base: Chunk<T, C>,
    current: DataSet<T>,
    replacements: BTreeMap<usize, T>,
    edit_revision: u64,
    saved_edit_revision: u64,
    storage_revision: Option<StorageRevision>,
}

impl<T: Clone + PartialEq, C> ManagedChunk<T, C> {
    /// Validates saved state before applying any replacements.
    pub fn from_base(
        base: Chunk<T, C>,
        identity: PersistenceIdentity,
        stored: Option<StoredChunk<T>>,
    ) -> Result<Self, PersistenceError> {
        if let Some(saved) = &stored {
            if saved.record.identity != identity {
                return Err(PersistenceError::IdentityMismatch);
            }

            if saved.record.config != base.context().config() {
                return Err(PersistenceError::ConfigurationMismatch);
            }

            if saved.record.coord != base.coord()
                || saved.record.dimensions != base.data().dimensions()
            {
                return Err(PersistenceError::InvalidRecord(
                    "Saved record does not match the generated chunk".to_owned(),
                ));
            }

            saved.record.validate()?;
        }

        let mut current = base.data().clone();
        let mut replacements = BTreeMap::new();
        let mut storage_revision = None;

        if let Some(saved) = stored {
            storage_revision = Some(saved.revision);

            for (index, value) in saved.record.replacements {
                // A backend may contain redundant base-valued replacements.
                // Normalize them in memory; subsequent saves omit them.
                if value != base.data().as_slice()[index] {
                    current.as_mut_slice()[index] = value.clone();
                    replacements.insert(index, value);
                }
            }
        }

        Ok(Self {
            identity,
            base,
            current,
            replacements,
            edit_revision: 0,
            saved_edit_revision: 0,
            storage_revision,
        })
    }

    pub fn context(&self) -> StepContext {
        self.base.context()
    }

    pub fn coord(&self) -> ChunkCoord {
        self.base.coord()
    }

    pub fn data(&self) -> &DataSet<T> {
        &self.current
    }

    pub fn base_data(&self) -> &DataSet<T> {
        self.base.data()
    }

    /// Generated base chunk data. Edits do not update it.
    pub fn chunk_data(&self) -> &C {
        self.base.chunk_data()
    }

    pub fn replacements(&self) -> &BTreeMap<usize, T> {
        &self.replacements
    }

    /// Revision local to this resident instance, not a persistent identifier.
    pub fn edit_revision(&self) -> u64 {
        self.edit_revision
    }

    pub fn storage_revision(&self) -> Option<StorageRevision> {
        self.storage_revision
    }

    /// Conservative dirty tracking.
    ///
    /// Reverting an edit still advances the revision. A save is required to
    /// establish that the current state is persisted.
    pub fn is_dirty(&self) -> bool {
        self.edit_revision != self.saved_edit_revision
    }

    /// Returns false for an unchanged value without advancing the revision.
    pub fn set_local(&mut self, local: LocalPosition, value: T) -> Result<bool, EditError> {
        let index = self
            .current
            .linear_index(local.0, local.1, local.2)
            .ok_or(EditError::OutsideChunk)?;

        if self.current.as_slice()[index] == value {
            return Ok(false);
        }

        let next = self
            .edit_revision
            .checked_add(1)
            .ok_or(EditError::RevisionExhausted)?;

        if self.base.data().as_slice()[index] == value {
            self.replacements.remove(&index);
        } else {
            self.replacements.insert(index, value.clone());
        }

        self.current.as_mut_slice()[index] = value;
        self.edit_revision = next;

        Ok(true)
    }

    pub fn set_global(&mut self, position: GlobalPosition, value: T) -> Result<bool, EditError> {
        let (coord, local) = self
            .context()
            .config()
            .locate(position)
            .ok_or(EditError::OutsideChunk)?;

        if coord != self.coord() {
            return Err(EditError::OutsideChunk);
        }

        self.set_local(local, value)
    }

    /// Produces an owned snapshot suitable for a background save.
    ///
    /// The manager will associate this with a residency token and captured
    /// edit revision before dispatch.
    pub fn snapshot_record(&self) -> ChunkRecord<T> {
        ChunkRecord {
            identity: self.identity.clone(),
            config: self.context().config(),
            coord: self.coord(),
            dimensions: self.current.dimensions(),
            replacements: self.replacements.clone(),
        }
    }

    /// Called only by the library manager after matching a save operation
    /// to its resident chunk.
    pub(crate) fn acknowledge_save(
        &mut self,
        captured_edit_revision: u64,
        revision: StorageRevision,
    ) -> Result<(), PersistenceError> {
        if captured_edit_revision > self.edit_revision
            || captured_edit_revision < self.saved_edit_revision
        {
            return Err(PersistenceError::InvalidRecord(
                "Save acknowledgement has an invalid edit revision".to_owned(),
            ));
        }

        if self
            .storage_revision
            .is_some_and(|previous| revision.value() <= previous.value())
        {
            return Err(PersistenceError::InvalidRecord(
                "Backend returned a non-increasing storage revision".to_owned(),
            ));
        }

        self.storage_revision = Some(revision);
        self.saved_edit_revision = captured_edit_revision;

        Ok(())
    }
}
