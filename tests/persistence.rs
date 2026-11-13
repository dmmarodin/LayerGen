use std::sync::{Arc, Barrier};

use layergen_rs::*;

fn config() -> PipelineConfig {
    PipelineConfig::new(
        WorldBounds::new((-3, 0, 0), (5, 2, 1)).unwrap(),
        ChunkSize::new(2, 2, 1).unwrap(),
        7,
    )
}

fn identity() -> PersistenceIdentity {
    PersistenceIdentity {
        world: WorldId("test-world".to_owned()),
        generation: GenerationId("terrain-v1".to_owned()),
    }
}

fn base(coord: ChunkCoord) -> Chunk<i64> {
    PipelineBuilder::new(config())
        .build(|position| position.0 + position.1 * 10)
        .generate_chunk(coord)
        .unwrap()
}

fn managed(coord: ChunkCoord) -> ManagedChunk<i64> {
    ManagedChunk::from_base(base(coord), identity(), None).unwrap()
}

#[test]
fn edits_are_sparse_and_reverting_removes_replacements() {
    let mut chunk = managed(ChunkCoord(0, 0, 0));

    assert!(!chunk.is_dirty());
    assert_eq!(chunk.edit_revision(), 0);

    assert!(chunk.set_local((0, 0, 0), 99).unwrap());
    assert_eq!(chunk.data()[(0, 0, 0)], 99);
    assert_eq!(chunk.base_data()[(0, 0, 0)], -3);
    assert_eq!(chunk.replacements().get(&0), Some(&99));
    assert_eq!(chunk.edit_revision(), 1);
    assert!(chunk.is_dirty());

    // No-op edits do not advance the revision.
    assert!(!chunk.set_local((0, 0, 0), 99).unwrap());
    assert_eq!(chunk.edit_revision(), 1);

    assert!(chunk.set_local((0, 0, 0), -3).unwrap());
    assert!(chunk.replacements().is_empty());
    assert_eq!(chunk.edit_revision(), 2);

    // Conservative until a revision-safe save is acknowledged.
    assert!(chunk.is_dirty());
}

#[test]
fn global_edits_respect_residency_and_partial_edges() {
    let mut chunk = managed(ChunkCoord(2, 0, 0));

    assert_eq!(chunk.data().dimensions(), (1, 2, 1));
    assert!(chunk.set_global((1, 1, 0), 50).unwrap());
    assert_eq!(chunk.data()[(0, 1, 0)], 50);

    assert_eq!(chunk.set_local((1, 0, 0), 10), Err(EditError::OutsideChunk),);

    assert_eq!(
        chunk.set_global((0, 0, 0), 10),
        Err(EditError::OutsideChunk),
    );

    assert_eq!(
        chunk.set_global((2, 0, 0), 10),
        Err(EditError::OutsideChunk),
    );
}

#[test]
fn sparse_records_round_trip_over_regenerated_base() {
    let storage = MemoryChunkStorage::default();
    let coord = ChunkCoord(0, 0, 0);
    let mut original = managed(coord);

    original.set_local((1, 1, 0), 77).unwrap();

    let record = original.snapshot_record();
    let key = record.key();

    let revision = storage.compare_exchange(&key, None, record).unwrap();

    let restored =
        ManagedChunk::from_base(base(coord), identity(), storage.load(&key).unwrap()).unwrap();

    assert_eq!(restored.data().as_slice(), original.data().as_slice());
    assert_eq!(restored.replacements().len(), 1);
    assert_eq!(restored.storage_revision(), Some(revision));
    assert!(!restored.is_dirty());
}

#[test]
fn stale_save_cannot_overwrite_newer_storage() {
    let storage = MemoryChunkStorage::default();
    let mut chunk = managed(ChunkCoord(0, 0, 0));

    chunk.set_local((0, 0, 0), 10).unwrap();

    let first = chunk.snapshot_record();
    let key = first.key();

    let revision = storage.compare_exchange(&key, None, first.clone()).unwrap();

    let result = storage.compare_exchange(&key, None, first);

    assert_eq!(
        result,
        Err(PersistenceError::Conflict {
            expected: None,
            actual: Some(revision),
        }),
    );

    assert_eq!(
        storage.load(&key).unwrap().unwrap().record.replacements[&0],
        10,
    );
}

#[test]
fn empty_records_preserve_revision_history() {
    let storage = MemoryChunkStorage::default();
    let mut chunk = managed(ChunkCoord(0, 0, 0));

    chunk.set_local((0, 0, 0), 10).unwrap();

    let first = chunk.snapshot_record();
    let key = first.key();

    let first_revision = storage.compare_exchange(&key, None, first).unwrap();

    chunk.set_local((0, 0, 0), -3).unwrap();

    let second_revision = storage
        .compare_exchange(&key, Some(first_revision), chunk.snapshot_record())
        .unwrap();

    assert!(second_revision.value() > first_revision.value());

    let stored = storage.load(&key).unwrap().unwrap();
    assert!(stored.record.replacements.is_empty());

    // The record must not become "absent" after reverting everything.
    assert!(matches!(
        storage.compare_exchange(&key, None, chunk.snapshot_record()),
        Err(PersistenceError::Conflict { .. }),
    ));
}

#[test]
fn incompatible_generation_is_rejected_not_hidden() {
    let storage = MemoryChunkStorage::default();
    let chunk = managed(ChunkCoord(0, 0, 0));

    let record = chunk.snapshot_record();
    let key = record.key();

    storage.compare_exchange(&key, None, record).unwrap();

    let mut incompatible = identity();
    incompatible.generation = GenerationId("terrain-v2".to_owned());

    let result = ManagedChunk::from_base(
        base(ChunkCoord(0, 0, 0)),
        incompatible,
        storage.load(&key).unwrap(),
    );

    assert!(matches!(result, Err(PersistenceError::IdentityMismatch),));
}

#[test]
fn invalid_replacement_indices_are_rejected() {
    let chunk = managed(ChunkCoord(2, 0, 0));
    let mut record = chunk.snapshot_record();

    // Partial edge chunk contains only two cells.
    record.replacements.insert(2, 99);

    assert!(matches!(
        record.validate(),
        Err(PersistenceError::InvalidRecord(_)),
    ));

    let storage = MemoryChunkStorage::default();
    let key = record.key();

    assert!(matches!(
        storage.compare_exchange(&key, None, record),
        Err(PersistenceError::InvalidRecord(_)),
    ));

    assert!(storage.load(&key).unwrap().is_none());
}

#[test]
fn changed_seed_is_rejected() {
    let chunk = managed(ChunkCoord(0, 0, 0));
    let mut record = chunk.snapshot_record();

    record.config = PipelineConfig::new(config().world(), config().chunk_size(), 8);

    let result = ManagedChunk::from_base(
        base(ChunkCoord(0, 0, 0)),
        identity(),
        Some(StoredChunk {
            revision: StorageRevision::new(1).unwrap(),
            record,
        }),
    );

    assert!(matches!(
        result,
        Err(PersistenceError::ConfigurationMismatch),
    ));
}

#[test]
fn concurrent_conditional_writes_have_exactly_one_winner() {
    let storage = Arc::new(MemoryChunkStorage::default());
    let barrier = Arc::new(Barrier::new(2));

    let record = managed(ChunkCoord(0, 0, 0)).snapshot_record();
    let key = record.key();

    let mut handles = Vec::new();

    for value in [10, 20] {
        let storage = Arc::clone(&storage);
        let barrier = Arc::clone(&barrier);
        let key = key.clone();
        let mut record = record.clone();

        record.replacements.insert(0, value);

        handles.push(std::thread::spawn(move || {
            barrier.wait();
            storage.compare_exchange(&key, None, record)
        }));
    }

    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);

    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(PersistenceError::Conflict { .. })))
            .count(),
        1,
    );
}
