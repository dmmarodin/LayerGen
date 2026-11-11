use std::sync::Arc;

use crate::{Chunk, ChunkCoord, GlobalPosition, Pipeline, PipelineConfig, SharedStep};

/// A reusable definition accepted by LayerGenPlugin.
///
/// Implementations separate shared preparation from individual chunk execution.
pub trait GenerationDefinition<T>: Send + Sync + 'static
where
    T: Send + Sync + 'static,
{
    type Shared: Send + Sync + 'static;

    fn config(&self) -> PipelineConfig;

    fn requires_preparation(&self) -> bool;

    fn prepare_shared(&self) -> Result<Arc<Self::Shared>, String>;

    fn generate_chunk_with_shared(
        &self,
        coord: ChunkCoord,
        shared: &Self::Shared,
    ) -> Result<Chunk<T>, String>;
}

impl<T, S, F, D> GenerationDefinition<T> for Pipeline<T, S, F, D>
where
    T: Send + Sync + 'static,
    D: Send + Sync + 'static,
    S: SharedStep<T, D>,
    F: Fn(GlobalPosition) -> T + Send + Sync + 'static,
{
    type Shared = D;

    fn config(&self) -> PipelineConfig {
        Pipeline::config(self)
    }

    fn requires_preparation(&self) -> bool {
        Pipeline::requires_preparation(self)
    }

    fn prepare_shared(&self) -> Result<Arc<D>, String> {
        Pipeline::prepare_shared(self)
    }

    fn generate_chunk_with_shared(
        &self,
        coord: ChunkCoord,
        shared: &D,
    ) -> Result<Chunk<T>, String> {
        Pipeline::generate_chunk_with_shared(self, coord, shared)
    }
}
