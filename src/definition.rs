use std::sync::Arc;

use crate::{Chunk, ChunkCoord, ChunkStep, GlobalPosition, Pipeline, PipelineConfig};

/// A reusable definition accepted by LayerGenPlugin.
///
/// Implementations separate shared preparation from individual chunk execution.
pub trait GenerationDefinition<T>: Send + Sync + 'static
where
    T: Send + Sync + 'static,
{
    type Shared: Send + Sync + 'static;

    /// Consumer-defined per-chunk data returned alongside the cells.
    /// `()` when the pipeline does not declare chunk data.
    type ChunkData: Send + Sync + 'static;

    fn config(&self) -> PipelineConfig;

    fn requires_preparation(&self) -> bool;

    fn prepare_shared(&self) -> Result<Arc<Self::Shared>, String>;

    fn generate_chunk_with_shared(
        &self,
        coord: ChunkCoord,
        shared: &Self::Shared,
    ) -> Result<Chunk<T, Self::ChunkData>, String>;
}

impl<T, S, F, D, C> GenerationDefinition<T> for Pipeline<T, S, F, D, C>
where
    T: Send + Sync + 'static,
    D: Send + Sync + 'static,
    C: Send + Sync + 'static,
    S: ChunkStep<T, C, D>,
    F: Fn(GlobalPosition) -> T + Send + Sync + 'static,
{
    type Shared = D;
    type ChunkData = C;

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
    ) -> Result<Chunk<T, C>, String> {
        Pipeline::generate_chunk_with_shared(self, coord, shared)
    }
}
