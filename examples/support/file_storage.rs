use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    marker::PhantomData,
    path::{Path, PathBuf},
    sync::Mutex,
};

use layergen_rs::{
    ChunkCoord, ChunkKey, ChunkRecord, ChunkSize, ChunkStorage, GenerationId, PersistenceError,
    PersistenceIdentity, PipelineConfig, StorageRevision, StoredChunk, WorldBounds, WorldId,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

const FORMAT_VERSION: u32 = 1;
const MAX_RECORD_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ID_BYTES: usize = 128;

/// Explicit consumer-side wire representation of PipelineConfig.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub origin: (i64, i64, i64),
    pub world_dimensions: (u32, u32, u32),
    pub chunk_dimensions: (u32, u32, u32),
    pub seed: u64,
}

impl FileConfig {
    pub fn pipeline_config(self) -> Result<PipelineConfig, PersistenceError> {
        let world = WorldBounds::new(self.origin, self.world_dimensions)
            .map_err(|message| invalid(message.to_owned()))?;

        let chunk = ChunkSize::new(
            self.chunk_dimensions.0,
            self.chunk_dimensions.1,
            self.chunk_dimensions.2,
        )
        .map_err(|message| invalid(message.to_owned()))?;

        Ok(PipelineConfig::new(world, chunk, self.seed))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(
    deny_unknown_fields,
    bound(serialize = "T: Serialize", deserialize = "T: DeserializeOwned")
)]
struct DiskRecord<T> {
    format_version: u32,
    storage_revision: u64,
    world: String,
    generation: String,
    config: FileConfig,
    coord: (u32, u32, u32),
    dimensions: (usize, usize, usize),

    // A sequence lets decoding explicitly reject duplicate indices.
    replacements: Vec<(usize, T)>,
}

/// Consumer-side JSON filesystem backend.
///
/// One live backend owns the storage directory through an OS file lock.
/// A mutex serializes load and compare_exchange within that backend.
///
/// Successful writes synchronize the temporary file before replacement.
/// Directory metadata is not synchronized: power-loss durability is not
/// promised.
///
/// The byte limit bounds serialized input, not arbitrary allocations or
/// custom deserialization behavior inside T.
pub struct FileChunkStorage<T> {
    root: PathBuf,
    wire_config: FileConfig,
    config: PipelineConfig,
    gate: Mutex<()>,

    // Keep this handle alive: dropping it releases the OS lock.
    _directory_lock: File,
    marker: PhantomData<T>,
}

impl<T> FileChunkStorage<T>
where
    T: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    pub fn open(root: impl AsRef<Path>, wire_config: FileConfig) -> Result<Self, PersistenceError> {
        let config = wire_config.pipeline_config()?;
        fs::create_dir_all(root.as_ref()).map_err(backend)?;

        let root = fs::canonicalize(root.as_ref()).map_err(backend)?;
        let lock_path = root.join(".layergen.lock");

        let directory_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .map_err(backend)?;

        fs2::FileExt::try_lock_exclusive(&directory_lock).map_err(|error| {
            PersistenceError::Backend(format!(
                "Cannot exclusively lock storage directory: {error}. \
                 Another backend/process may already own it"
            ))
        })?;

        Ok(Self {
            root,
            wire_config,
            config,
            gate: Mutex::new(()),
            _directory_lock: directory_lock,
            marker: PhantomData,
        })
    }

    pub fn config(&self) -> PipelineConfig {
        self.config
    }

    fn record_path(&self, key: &ChunkKey) -> Result<PathBuf, PersistenceError> {
        validate_id(&key.world.0)?;

        // Hex encoding is reversible and has no path separators or reserved
        // filename characters. Do not use untrusted world IDs as paths.
        let mut encoded = String::with_capacity(key.world.0.len() * 2);

        for byte in key.world.0.bytes() {
            use std::fmt::Write as _;
            write!(&mut encoded, "{byte:02x}").expect("Writing into a String cannot fail");
        }

        let world_directory = self.root.join(format!("world-{encoded}"));

        Ok(world_directory.join(format!(
            "{}-{}-{}.json",
            key.coord.0, key.coord.1, key.coord.2
        )))
    }

    /// Caller must hold gate.
    fn load_locked(&self, key: &ChunkKey) -> Result<Option<StoredChunk<T>>, PersistenceError> {
        let path = self.record_path(key)?;

        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(backend(error)),
        };

        // take() also bounds reads if a file grows after it is opened.
        let mut bytes = Vec::new();
        file.take(MAX_RECORD_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(backend)?;

        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(invalid("Record exceeds the serialized size limit"));
        }

        let disk: DiskRecord<T> = serde_json::from_slice(&bytes)
            .map_err(|error| invalid(format!("Cannot decode record: {error}")))?;

        if disk.format_version != FORMAT_VERSION {
            return Err(invalid("Unsupported record format version"));
        }

        validate_id(&disk.world)?;
        validate_id(&disk.generation)?;

        let revision = StorageRevision::new(disk.storage_revision)?;
        let config = disk.config.pipeline_config()?;

        let mut replacements = BTreeMap::new();

        for (index, value) in disk.replacements {
            if replacements.insert(index, value).is_some() {
                return Err(invalid("Duplicate replacement index"));
            }
        }

        let record = ChunkRecord {
            identity: PersistenceIdentity {
                world: WorldId(disk.world),
                generation: GenerationId(disk.generation),
            },
            config,
            coord: ChunkCoord(disk.coord.0, disk.coord.1, disk.coord.2),
            dimensions: disk.dimensions,
            replacements,
        };

        record.validate()?;

        if record.key() != *key {
            return Err(invalid("Record does not match its storage key"));
        }

        if record.config != self.config {
            return Err(PersistenceError::ConfigurationMismatch);
        }

        Ok(Some(StoredChunk { revision, record }))
    }
}

impl<T> ChunkStorage<T> for FileChunkStorage<T>
where
    T: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    fn load(&self, key: &ChunkKey) -> Result<Option<StoredChunk<T>>, PersistenceError> {
        let _guard = self
            .gate
            .lock()
            .map_err(|_| backend("Storage mutex poisoned"))?;

        self.load_locked(key)
    }

    fn compare_exchange(
        &self,
        key: &ChunkKey,
        expected: Option<StorageRevision>,
        record: ChunkRecord<T>,
    ) -> Result<StorageRevision, PersistenceError> {
        record.validate()?;
        validate_id(&record.identity.world.0)?;
        validate_id(&record.identity.generation.0)?;

        if record.key() != *key {
            return Err(invalid("Record does not match its storage key"));
        }

        if record.config != self.config {
            return Err(PersistenceError::ConfigurationMismatch);
        }

        let _guard = self
            .gate
            .lock()
            .map_err(|_| backend("Storage mutex poisoned"))?;

        // The mutex and retained directory lock protect this entire
        // comparison-and-replacement operation.
        let existing = self.load_locked(key)?;
        let actual = existing.as_ref().map(|stored| stored.revision);

        if actual != expected {
            return Err(PersistenceError::Conflict { expected, actual });
        }

        if let Some(existing) = &existing {
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

        let disk = DiskRecord {
            format_version: FORMAT_VERSION,
            storage_revision: revision.value(),
            world: record.identity.world.0,
            generation: record.identity.generation.0,
            config: self.wire_config,
            coord: (record.coord.0, record.coord.1, record.coord.2),
            dimensions: record.dimensions,
            replacements: record.replacements.into_iter().collect(),
        };

        // This may allocate before the size check; T serialization is
        // consumer-controlled and not a total memory-budget guarantee.
        let bytes = serde_json::to_vec(&disk).map_err(backend)?;

        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(invalid("Record exceeds the serialized size limit"));
        }

        let path = self.record_path(key)?;
        let parent = path
            .parent()
            .ok_or_else(|| backend("Record path has no parent"))?;

        fs::create_dir_all(parent).map_err(backend)?;

        // Keep the temporary file on the same filesystem as its destination.
        let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(backend)?;

        temporary.write_all(&bytes).map_err(backend)?;
        temporary.as_file().sync_all().map_err(backend)?;

        // tempfile handles replacement of an existing destination on Windows
        // as well as Unix. Never remove the old record before replacement.
        temporary.persist(&path).map_err(backend)?;

        Ok(revision)
    }
}

fn validate_id(id: &str) -> Result<(), PersistenceError> {
    if id.is_empty() || id.len() > MAX_ID_BYTES {
        return Err(invalid(format!(
            "Identity must contain between 1 and {MAX_ID_BYTES} UTF-8 bytes"
        )));
    }

    Ok(())
}

fn backend(error: impl std::fmt::Display) -> PersistenceError {
    PersistenceError::Backend(error.to_string())
}

fn invalid(message: impl Into<String>) -> PersistenceError {
    PersistenceError::InvalidRecord(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn specification() -> FileConfig {
        FileConfig {
            origin: (-8, -8, 0),
            world_dimensions: (16, 16, 16),
            chunk_dimensions: (8, 8, 8),
            seed: 12345,
        }
    }

    fn record(config: PipelineConfig) -> ChunkRecord<u8> {
        ChunkRecord {
            identity: PersistenceIdentity {
                world: WorldId("test/world".to_owned()),
                generation: GenerationId("test-v1".to_owned()),
            },
            config,
            coord: ChunkCoord(0, 0, 0),
            dimensions: (8, 8, 8),
            replacements: BTreeMap::from([(0, 7)]),
        }
    }

    #[test]
    fn reopen_preserves_state_and_empty_record_revision() {
        let directory = tempfile::tempdir().unwrap();

        let storage = FileChunkStorage::<u8>::open(directory.path(), specification()).unwrap();

        let original = record(storage.config());
        let key = original.key();

        let first = storage.compare_exchange(&key, None, original).unwrap();

        assert_eq!(first.value(), 1);
        drop(storage);

        let storage = FileChunkStorage::<u8>::open(directory.path(), specification()).unwrap();

        let loaded = storage.load(&key).unwrap().unwrap();
        assert_eq!(loaded.revision, first);
        assert_eq!(loaded.record.replacements.get(&0), Some(&7));

        let mut reverted = loaded.record;
        reverted.replacements.clear();

        let second = storage
            .compare_exchange(&key, Some(first), reverted)
            .unwrap();

        assert_eq!(second.value(), 2);
        drop(storage);

        let storage = FileChunkStorage::<u8>::open(directory.path(), specification()).unwrap();

        let loaded = storage.load(&key).unwrap().unwrap();
        assert_eq!(loaded.revision, second);
        assert!(loaded.record.replacements.is_empty());
    }

    #[test]
    fn stale_write_does_not_change_record() {
        let directory = tempfile::tempdir().unwrap();
        let storage = FileChunkStorage::<u8>::open(directory.path(), specification()).unwrap();

        let original = record(storage.config());
        let key = original.key();

        let first = storage
            .compare_exchange(&key, None, original.clone())
            .unwrap();

        assert!(matches!(
            storage.compare_exchange(&key, None, original),
            Err(PersistenceError::Conflict { .. })
        ));

        let loaded = storage.load(&key).unwrap().unwrap();
        assert_eq!(loaded.revision, first);
        assert_eq!(loaded.record.replacements.get(&0), Some(&7));
    }

    #[test]
    fn second_backend_cannot_own_directory() {
        let directory = tempfile::tempdir().unwrap();
        let first = FileChunkStorage::<u8>::open(directory.path(), specification()).unwrap();

        assert!(FileChunkStorage::<u8>::open(directory.path(), specification()).is_err());

        drop(first);

        assert!(FileChunkStorage::<u8>::open(directory.path(), specification()).is_ok());
    }

    #[test]
    fn malformed_records_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let storage = FileChunkStorage::<u8>::open(directory.path(), specification()).unwrap();

        let original = record(storage.config());
        let key = original.key();
        storage.compare_exchange(&key, None, original).unwrap();

        fs::write(storage.record_path(&key).unwrap(), b"{\"truncated\":").unwrap();

        assert!(matches!(
            storage.load(&key),
            Err(PersistenceError::InvalidRecord(_))
        ));
    }

    #[test]
    fn configuration_mismatch_is_rejected_after_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let storage = FileChunkStorage::<u8>::open(directory.path(), specification()).unwrap();

        let original = record(storage.config());
        let key = original.key();
        storage.compare_exchange(&key, None, original).unwrap();
        drop(storage);

        let changed = FileConfig {
            seed: 999,
            ..specification()
        };

        let storage = FileChunkStorage::<u8>::open(directory.path(), changed).unwrap();

        assert!(matches!(
            storage.load(&key),
            Err(PersistenceError::ConfigurationMismatch)
        ));
    }

    #[test]
    fn generation_mismatch_cannot_replace_existing_record() {
        let directory = tempfile::tempdir().unwrap();
        let storage = FileChunkStorage::<u8>::open(directory.path(), specification()).unwrap();

        let original = record(storage.config());
        let key = original.key();

        let revision = storage
            .compare_exchange(&key, None, original.clone())
            .unwrap();

        let mut changed = original;
        changed.identity.generation = GenerationId("test-v2".to_owned());

        assert!(matches!(
            storage.compare_exchange(&key, Some(revision), changed),
            Err(PersistenceError::IdentityMismatch)
        ));

        assert_eq!(storage.load(&key).unwrap().unwrap().revision, revision);
    }

    #[test]
    fn abandoned_temporary_file_does_not_replace_committed_record() {
        let directory = tempfile::tempdir().unwrap();
        let storage = FileChunkStorage::<u8>::open(directory.path(), specification()).unwrap();

        let original = record(storage.config());
        let key = original.key();

        let revision = storage.compare_exchange(&key, None, original).unwrap();

        let path = storage.record_path(&key).unwrap();
        fs::write(
            path.parent().unwrap().join("abandoned-write.tmp"),
            b"incomplete",
        )
        .unwrap();

        assert_eq!(storage.load(&key).unwrap().unwrap().revision, revision);
    }
}
