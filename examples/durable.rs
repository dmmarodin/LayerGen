#[path = "support/file_storage.rs"]
mod file_storage;

use std::{
    error::Error,
    io,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use bevy_app::App;
use layergen_rs::{
    ChunkCoord, ChunkManager, ChunkManagerLimits, ChunkManagerPlugin, GenerationId,
    GenerationLimits, GlobalPosition, LayerGenPlugin, ManagedOutcome, ManagedRequestId,
    PersistenceIdentity, PipelineBuilder, WorldId,
};
use serde::{Deserialize, Serialize};

use file_storage::{FileChunkStorage, FileConfig};

const TEST_CHUNK: ChunkCoord = ChunkCoord(1, 1, 0);
const EDIT_POSITION: GlobalPosition = (16, 16, 0);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Voxel {
    solid: bool,
}

fn specification() -> FileConfig {
    FileConfig {
        origin: (0, 0, 0),
        world_dimensions: (64, 64, 32),
        chunk_dimensions: (16, 16, 16),
        seed: 12345,
    }
}

fn base_voxel(position: GlobalPosition) -> Voxel {
    Voxel {
        solid: position.2 < 12,
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();

    if arguments.len() != 2
        || !matches!(
            arguments[0].as_str(),
            "save" | "verify" | "revert" | "verify-base"
        )
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Usage: durable <save|verify|revert|verify-base> <directory>",
        )
        .into());
    }

    let command = arguments[0].as_str();

    let storage = Arc::new(FileChunkStorage::<Voxel>::open(
        &arguments[1],
        specification(),
    )?);

    let pipeline = PipelineBuilder::new(storage.config()).build(base_voxel);

    let identity = PersistenceIdentity {
        world: WorldId("layergen-durable-smoke-world".to_owned()),
        generation: GenerationId("flat-terrain-v1".to_owned()),
    };

    let mut app = App::new();

    app.add_plugins(
        LayerGenPlugin::new(pipeline).with_limits(GenerationLimits::new(2, 8).unwrap()),
    )
    .add_plugins(
        ChunkManagerPlugin::new(identity, storage)
            .with_limits(ChunkManagerLimits::new(8, 2, 2).unwrap()),
    );

    let request = app
        .world_mut()
        .resource_mut::<ChunkManager<Voxel>>()
        .request_load(TEST_CHUNK)?;

    let loaded = wait_for_completion(&mut app, request)?;

    if loaded != (ManagedOutcome::Loaded { installed: true }) {
        return Err(io::Error::other("Chunk was not installed").into());
    }

    match command {
        "save" => {
            // Require a fresh record so the restart sequence is unambiguous.
            validate_resident(&app, false, None)?;

            let edited = Voxel {
                solid: !base_voxel(EDIT_POSITION).solid,
            };

            let request = {
                let mut manager = app.world_mut().resource_mut::<ChunkManager<Voxel>>();

                if !manager.set_global(EDIT_POSITION, edited)? {
                    return Err(io::Error::other("Expected an effective edit").into());
                }

                manager.request_save(TEST_CHUNK)?
            };

            verify_save(wait_for_completion(&mut app, request)?, 1)?;
            validate_resident(&app, true, Some(1))?;

            println!("SAVE PASSED: replacement committed at revision 1");
        }
        "verify" => {
            validate_resident(&app, true, Some(1))?;
            println!("RESTART VERIFY PASSED: replacement restored at revision 1");
        }
        "revert" => {
            validate_resident(&app, true, Some(1))?;

            let request = {
                let mut manager = app.world_mut().resource_mut::<ChunkManager<Voxel>>();

                if !manager.set_global(EDIT_POSITION, base_voxel(EDIT_POSITION))? {
                    return Err(io::Error::other("Expected an effective revert").into());
                }

                manager.request_save(TEST_CHUNK)?
            };

            verify_save(wait_for_completion(&mut app, request)?, 2)?;
            validate_resident(&app, false, Some(2))?;

            println!("REVERT PASSED: empty replacement record committed at revision 2");
        }
        "verify-base" => {
            validate_resident(&app, false, Some(2))?;
            println!("RESTART VERIFY-BASE PASSED: base restored at revision 2");
        }
        _ => unreachable!("Command was validated"),
    }

    // Every operation was consumed and acknowledged before normal exit.
    let manager = app.world().resource::<ChunkManager<Voxel>>();

    if manager.outstanding_count() != 0 || manager.is_saving(TEST_CHUNK) {
        return Err(io::Error::other("Unexpected outstanding work at exit").into());
    }

    Ok(())
}

fn wait_for_completion(
    app: &mut App,
    request: ManagedRequestId,
) -> Result<ManagedOutcome, Box<dyn Error>> {
    let started = Instant::now();

    loop {
        app.update();

        let completion = app
            .world_mut()
            .resource_mut::<ChunkManager<Voxel>>()
            .take_completed();

        if let Some(completion) = completion {
            if completion.request_id != request || completion.coord != TEST_CHUNK {
                return Err(io::Error::other("Unexpected managed completion").into());
            }

            return Ok(completion.result?);
        }

        if started.elapsed() > Duration::from_secs(30) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Durable example operation timed out",
            )
            .into());
        }

        thread::sleep(Duration::from_millis(1));
    }
}

fn verify_save(outcome: ManagedOutcome, expected_revision: u64) -> Result<(), Box<dyn Error>> {
    match outcome {
        ManagedOutcome::Saved {
            edit_revision: 1,
            storage_revision,
        } if storage_revision.value() == expected_revision => Ok(()),
        other => {
            Err(io::Error::other(format!("Unexpected save acknowledgement: {other:?}")).into())
        }
    }
}

fn validate_resident(
    app: &App,
    edited: bool,
    expected_revision: Option<u64>,
) -> Result<(), Box<dyn Error>> {
    let manager = app.world().resource::<ChunkManager<Voxel>>();
    let chunk = manager
        .chunk(TEST_CHUNK)
        .ok_or_else(|| io::Error::other("Test chunk is not resident"))?;

    if chunk.is_dirty() {
        return Err(io::Error::other("Expected a clean chunk").into());
    }

    if chunk.storage_revision().map(|revision| revision.value()) != expected_revision {
        return Err(io::Error::other(format!(
            "Unexpected revision: expected {expected_revision:?}, found {:?}",
            chunk.storage_revision()
        ))
        .into());
    }

    if chunk.replacements().len() != usize::from(edited) {
        return Err(io::Error::other("Unexpected replacement count").into());
    }

    let context = chunk.context();

    for (voxel, x, y, z) in chunk.data().iter() {
        let position = context
            .global_position((x, y, z))
            .ok_or_else(|| io::Error::other("Invalid local position"))?;

        let base = base_voxel(position);

        if chunk.base_data().get(x, y, z) != Some(&base) {
            return Err(
                io::Error::other(format!("Generated base mismatch at {position:?}")).into(),
            );
        }

        let expected = if edited && position == EDIT_POSITION {
            Voxel { solid: !base.solid }
        } else {
            base
        };

        if voxel != &expected {
            return Err(io::Error::other(format!("Current value mismatch at {position:?}")).into());
        }
    }

    Ok(())
}
