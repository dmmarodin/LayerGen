use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use bevy_app::App;

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
        world: WorldId("manager-test".to_owned()),
        generation: GenerationId("terrain-v1".to_owned()),
    }
}

fn app_with_storage(
    storage: Arc<dyn ChunkStorage<i64>>,
    limits: ChunkManagerLimits,
) -> App {
    let pipeline = PipelineBuilder::new(config())
        .build(|position| position.0 + position.1 * 10);

    let mut app = App::new();

    app.add_plugins(LayerGenPlugin::new(pipeline));

    app.add_plugins(
        ChunkManagerPlugin::new(identity(), storage)
            .with_limits(limits),
    );

    app
}

fn wait_until(app: &mut App, mut condition: impl FnMut(&App) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);

    loop {
        app.update();

        {
            let manager = app.world().resource::<ChunkManager<i64>>();

            assert!(
                manager.storage_jobs_count()
                    <= manager.limits().max_storage_jobs()
            );

            assert!(
                manager.outstanding_count()
                    <= manager.limits().max_outstanding()
            );
        }

        if condition(app) {
            return;
        }

        assert!(Instant::now() < deadline, "Manager test timed out");
        thread::sleep(Duration::from_millis(1));
    }
}

fn wait_for_completion(app: &mut App) -> ManagedCompletion {
    wait_until(app, |app| {
        app.world()
            .resource::<ChunkManager<i64>>()
            .completed_count()
            > 0
    });

    app.world_mut()
        .resource_mut::<ChunkManager<i64>>()
        .take_completed()
        .unwrap()
}

fn load(app: &mut App, coord: ChunkCoord) {
    app.world_mut()
        .resource_mut::<ChunkManager<i64>>()
        .request_load(coord)
        .unwrap();

    assert_eq!(
        wait_for_completion(app).result,
        Ok(ManagedOutcome::Loaded { installed: true }),
    );
}

#[test]
fn edit_save_unload_reload_round_trip() {
    let storage = Arc::new(MemoryChunkStorage::default());

    let mut app = app_with_storage(
        storage,
        ChunkManagerLimits::default(),
    );

    let coord = ChunkCoord(0, 0, 0);
    load(&mut app, coord);

    {
        let mut manager =
            app.world_mut().resource_mut::<ChunkManager<i64>>();

        assert_eq!(manager.get_global((-3, 0, 0)), Some(&-3));

        manager.set_global((-3, 0, 0), 99).unwrap();

        assert_eq!(manager.unload(coord), Err(ChunkManagerError::Dirty));

        manager.request_save(coord).unwrap();

        assert_eq!(
            manager.discard(coord),
            Err(ChunkManagerError::SaveInFlight),
        );
    }

    assert!(matches!(
        wait_for_completion(&mut app).result,
        Ok(ManagedOutcome::Saved { edit_revision: 1, .. }),
    ));

    {
        let mut manager =
            app.world_mut().resource_mut::<ChunkManager<i64>>();

        assert!(!manager.chunk(coord).unwrap().is_dirty());
        manager.unload(coord).unwrap();
    }

    load(&mut app, coord);

    let manager = app.world().resource::<ChunkManager<i64>>();

    assert_eq!(manager.get_global((-3, 0, 0)), Some(&99));
    assert_eq!(manager.chunk(coord).unwrap().replacements().len(), 1);
    assert!(!manager.chunk(coord).unwrap().is_dirty());
}

#[test]
fn edits_after_save_capture_remain_dirty() {
    let mut app = app_with_storage(
        Arc::new(MemoryChunkStorage::default()),
        ChunkManagerLimits::default(),
    );

    let coord = ChunkCoord(0, 0, 0);
    load(&mut app, coord);

    {
        let mut manager =
            app.world_mut().resource_mut::<ChunkManager<i64>>();

        manager.set_global((-3, 0, 0), 10).unwrap();
        manager.request_save(coord).unwrap();

        // Snapshot was already captured, even though dispatch happens later.
        manager.set_global((-3, 0, 0), 20).unwrap();

        assert_eq!(
            manager.request_save(coord),
            Err(ChunkManagerError::SaveInFlight),
        );
    }

    assert!(matches!(
        wait_for_completion(&mut app).result,
        Ok(ManagedOutcome::Saved { edit_revision: 1, .. }),
    ));

    {
        let mut manager =
            app.world_mut().resource_mut::<ChunkManager<i64>>();

        assert_eq!(manager.get_global((-3, 0, 0)), Some(&20));
        assert!(manager.chunk(coord).unwrap().is_dirty());

        manager.request_save(coord).unwrap();
    }

    assert!(matches!(
        wait_for_completion(&mut app).result,
        Ok(ManagedOutcome::Saved { edit_revision: 2, .. }),
    ));

    let manager = app.world().resource::<ChunkManager<i64>>();
    assert!(!manager.chunk(coord).unwrap().is_dirty());
}

#[test]
fn reverting_saved_edits_persists_an_empty_record() {
    let storage = Arc::new(MemoryChunkStorage::default());

    let mut app = app_with_storage(
        storage.clone(),
        ChunkManagerLimits::default(),
    );

    let coord = ChunkCoord(0, 0, 0);
    load(&mut app, coord);

    {
        let mut manager =
            app.world_mut().resource_mut::<ChunkManager<i64>>();

        manager.set_global((-3, 0, 0), 10).unwrap();
        manager.request_save(coord).unwrap();
    }

    wait_for_completion(&mut app).result.unwrap();

    {
        let mut manager =
            app.world_mut().resource_mut::<ChunkManager<i64>>();

        manager.set_global((-3, 0, 0), -3).unwrap();
        manager.request_save(coord).unwrap();
    }

    wait_for_completion(&mut app).result.unwrap();

    let key = ChunkKey {
        world: identity().world,
        coord,
    };

    let stored = storage.load(&key).unwrap().unwrap();

    assert!(stored.record.replacements.is_empty());
    assert_eq!(stored.revision.value(), 2);
}

struct GatedLoadStorage {
    inner: MemoryChunkStorage<i64>,
    started: AtomicBool,
    release: AtomicBool,
}

impl Default for GatedLoadStorage {
    fn default() -> Self {
        Self {
            inner: MemoryChunkStorage::default(),
            started: AtomicBool::new(false),
            release: AtomicBool::new(false),
        }
    }
}

impl ChunkStorage<i64> for GatedLoadStorage {
    fn load(
        &self,
        key: &ChunkKey,
    ) -> Result<Option<StoredChunk<i64>>, PersistenceError> {
        self.started.store(true, Ordering::SeqCst);

        let deadline = Instant::now() + Duration::from_secs(10);

        while !self.release.load(Ordering::SeqCst) {
            if Instant::now() >= deadline {
                return Err(PersistenceError::Backend(
                    "Load gate timed out".to_owned(),
                ));
            }

            thread::sleep(Duration::from_millis(1));
        }

        self.inner.load(key)
    }

    fn compare_exchange(
        &self,
        key: &ChunkKey,
        expected: Option<StorageRevision>,
        record: ChunkRecord<i64>,
    ) -> Result<StorageRevision, PersistenceError> {
        self.inner.compare_exchange(key, expected, record)
    }
}

#[test]
fn stale_storage_load_cannot_replace_new_residency() {
    let storage = Arc::new(GatedLoadStorage::default());

    let mut app = app_with_storage(
        storage.clone(),
        ChunkManagerLimits::new(4, 2, 1).unwrap(),
    );

    let coord = ChunkCoord(0, 0, 0);

    let old_id = app
        .world_mut()
        .resource_mut::<ChunkManager<i64>>()
        .request_load(coord)
        .unwrap();

    wait_until(&mut app, |_| storage.started.load(Ordering::SeqCst));

    let new_id = {
        let mut manager =
            app.world_mut().resource_mut::<ChunkManager<i64>>();

        manager.discard(coord).unwrap();
        manager.request_load(coord).unwrap()
    };

    assert_ne!(old_id, new_id);

    storage.release.store(true, Ordering::SeqCst);

    wait_until(&mut app, |app| {
        app.world()
            .resource::<ChunkManager<i64>>()
            .completed_count()
            == 2
    });

    let mut manager =
        app.world_mut().resource_mut::<ChunkManager<i64>>();

    let first = manager.take_completed().unwrap();
    let second = manager.take_completed().unwrap();

    let completions = [first, second];

    let old = completions
        .iter()
        .find(|completion| completion.request_id == old_id)
        .unwrap();

    let new = completions
        .iter()
        .find(|completion| completion.request_id == new_id)
        .unwrap();

    assert_eq!(
        old.result,
        Ok(ManagedOutcome::Loaded { installed: false }),
    );

    assert_eq!(
        new.result,
        Ok(ManagedOutcome::Loaded { installed: true }),
    );

    assert_eq!(manager.resident_count(), 1);
    assert!(!manager.is_loading(coord));
}

#[test]
fn invalidated_queued_load_is_not_installed() {
    let mut app = app_with_storage(
        Arc::new(MemoryChunkStorage::default()),
        ChunkManagerLimits::default(),
    );

    let coord = ChunkCoord(0, 0, 0);

    {
        let mut manager =
            app.world_mut().resource_mut::<ChunkManager<i64>>();

        manager.request_load(coord).unwrap();
        manager.discard(coord).unwrap();
    }

    assert_eq!(
        wait_for_completion(&mut app).result,
        Ok(ManagedOutcome::Loaded { installed: false }),
    );

    assert_eq!(
        app.world()
            .resource::<ChunkManager<i64>>()
            .resident_count(),
        0,
    );
}

#[test]
fn unconsumed_completions_keep_operation_capacity_reserved() {
    let mut app = app_with_storage(
        Arc::new(MemoryChunkStorage::default()),
        ChunkManagerLimits::new(1, 1, 2).unwrap(),
    );

    app.world_mut()
        .resource_mut::<ChunkManager<i64>>()
        .request_load(ChunkCoord(0, 0, 0))
        .unwrap();

    wait_until(&mut app, |app| {
        app.world()
            .resource::<ChunkManager<i64>>()
            .completed_count()
            == 1
    });

    let mut manager =
        app.world_mut().resource_mut::<ChunkManager<i64>>();

    assert_eq!(
        manager.request_load(ChunkCoord(1, 0, 0)),
        Err(ChunkManagerError::OperationCapacity),
    );

    manager.take_completed().unwrap();

    manager.request_load(ChunkCoord(1, 0, 0)).unwrap();
}

#[test]
fn pending_loads_reserve_resident_capacity() {
    let mut app = app_with_storage(
        Arc::new(MemoryChunkStorage::default()),
        ChunkManagerLimits::new(4, 1, 1).unwrap(),
    );

    let mut manager =
        app.world_mut().resource_mut::<ChunkManager<i64>>();

    manager.request_load(ChunkCoord(0, 0, 0)).unwrap();

    assert_eq!(
        manager.request_load(ChunkCoord(1, 0, 0)),
        Err(ChunkManagerError::ResidentCapacity),
    );
}

struct FailingSaveStorage {
    inner: MemoryChunkStorage<i64>,
}

impl ChunkStorage<i64> for FailingSaveStorage {
    fn load(
        &self,
        key: &ChunkKey,
    ) -> Result<Option<StoredChunk<i64>>, PersistenceError> {
        self.inner.load(key)
    }

    fn compare_exchange(
        &self,
        _key: &ChunkKey,
        _expected: Option<StorageRevision>,
        _record: ChunkRecord<i64>,
    ) -> Result<StorageRevision, PersistenceError> {
        Err(PersistenceError::Backend(
            "Planned save failure".to_owned(),
        ))
    }
}

#[test]
fn failed_save_keeps_dirty_state_and_releases_save_lock() {
    let mut app = app_with_storage(
        Arc::new(FailingSaveStorage {
            inner: MemoryChunkStorage::default(),
        }),
        ChunkManagerLimits::default(),
    );

    let coord = ChunkCoord(0, 0, 0);
    load(&mut app, coord);

    {
        let mut manager =
            app.world_mut().resource_mut::<ChunkManager<i64>>();

        manager.set_global((-3, 0, 0), 99).unwrap();
        manager.request_save(coord).unwrap();
    }

    assert!(matches!(
        wait_for_completion(&mut app).result,
        Err(ChunkManagerError::Persistence(
            PersistenceError::Backend(message)
        )) if message == "Planned save failure"
    ));

    let mut manager =
        app.world_mut().resource_mut::<ChunkManager<i64>>();

    assert!(manager.chunk(coord).unwrap().is_dirty());
    assert_eq!(manager.chunk(coord).unwrap().storage_revision(), None);
    assert!(!manager.is_saving(coord));

    manager.request_save(coord).unwrap();
}

#[test]
fn external_write_causes_conflict_without_marking_chunk_clean() {
    let storage = Arc::new(MemoryChunkStorage::default());

    let mut app = app_with_storage(
        storage.clone(),
        ChunkManagerLimits::default(),
    );

    let coord = ChunkCoord(0, 0, 0);
    load(&mut app, coord);

    let record = {
        let manager = app.world().resource::<ChunkManager<i64>>();
        manager.chunk(coord).unwrap().snapshot_record()
    };

    let key = record.key();

    let external_revision = storage
        .compare_exchange(&key, None, record)
        .unwrap();

    {
        let mut manager =
            app.world_mut().resource_mut::<ChunkManager<i64>>();

        manager.set_global((-3, 0, 0), 99).unwrap();
        manager.request_save(coord).unwrap();
    }

    assert_eq!(
        wait_for_completion(&mut app).result,
        Err(ChunkManagerError::Persistence(
            PersistenceError::Conflict {
                expected: None,
                actual: Some(external_revision),
            }
        )),
    );

    let manager = app.world().resource::<ChunkManager<i64>>();

    assert!(manager.chunk(coord).unwrap().is_dirty());
    assert_eq!(manager.get_global((-3, 0, 0)), Some(&99));
}
