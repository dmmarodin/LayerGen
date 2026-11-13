use std::{marker::PhantomData, sync::Arc};

use crate::{
    Chain, Chunk, ChunkCoord, ChunkStep, DataSet, EmptyStep, GenerationDefinition, GlobalPosition,
    LocalPosition, LocalStep, Pipeline, PipelineConfig, PipelineStepResult, SharedOnly, SharedStep,
    Step, StepContext,
};

/// Neighborhood radius in cells, per axis (symmetric along each axis).
///
/// Per-axis radii avoid planning dependency chunks along axes a step never
/// reads, e.g. column-wise steps that only need X/Y neighbors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Neighborhood {
    radii: (u32, u32, u32),
}

impl Neighborhood {
    /// Same radius along all three axes.
    pub fn radius(radius: u32) -> Self {
        Self {
            radii: (radius, radius, radius),
        }
    }

    /// Independent radius per axis; zero means no neighbors along that axis.
    pub fn axes(x: u32, y: u32, z: u32) -> Self {
        Self { radii: (x, y, z) }
    }

    pub fn radii(self) -> (u32, u32, u32) {
        self.radii
    }

    /// Largest per-axis radius (equals the radius for symmetric neighborhoods).
    pub fn cell_radius(self) -> u32 {
        self.radii.0.max(self.radii.1).max(self.radii.2)
    }
}

/// Bounds for one requested chunk's neighborhood processing.
///
/// These are cell/work counts, not byte budgets.
///
/// Temporary cells cover the datasets managed by this neighborhood wrapper.
/// Prefix callbacks, shared data, and allocations inside T are not covered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NeighborhoodLimits {
    max_temporary_cells: usize,
    max_dependency_chunks: usize,
    max_prefix_cells: usize,
}

impl NeighborhoodLimits {
    pub fn new(
        max_temporary_cells: usize,
        max_dependency_chunks: usize,
        max_prefix_cells: usize,
    ) -> Result<Self, &'static str> {
        if max_temporary_cells == 0 || max_dependency_chunks == 0 || max_prefix_cells == 0 {
            return Err("Neighborhood limits must be greater than zero");
        }

        Ok(Self {
            max_temporary_cells,
            max_dependency_chunks,
            max_prefix_cells,
        })
    }

    pub fn max_temporary_cells(self) -> usize {
        self.max_temporary_cells
    }

    pub fn max_dependency_chunks(self) -> usize {
        self.max_dependency_chunks
    }

    pub fn max_prefix_cells(self) -> usize {
        self.max_prefix_cells
    }
}

impl Default for NeighborhoodLimits {
    fn default() -> Self {
        Self {
            max_temporary_cells: 1_048_576,
            max_dependency_chunks: 125,
            max_prefix_cells: 4_194_304,
        }
    }
}

/// Reads stable prefix data and modifies only the requested output chunk.
pub trait NeighborhoodStep<T, D>: Send + Sync + 'static {
    fn run(
        &self,
        input: &NeighborhoodView<'_, T>,
        output: &mut DataSet<T>,
        context: &StepContext,
        shared: &D,
    ) -> PipelineStepResult;
}

/// Neighborhood step that also reads prefix chunk data of the requested and
/// dependency chunks, and writes the requested chunk's output chunk data.
pub trait NeighborhoodChunkStep<T, C, D>: Send + Sync + 'static {
    fn run(
        &self,
        input: &NeighborhoodView<'_, T>,
        chunks: &NeighborChunks<'_, C>,
        output: &mut DataSet<T>,
        output_chunk: &mut C,
        context: &StepContext,
        shared: &D,
    ) -> PipelineStepResult;
}

/// Adapter used by `with_neighborhood`: runs a cell-only neighborhood step
/// in a pipeline with chunk data, without exposing that data.
#[derive(Clone, Debug)]
pub struct CellsOnly<N>(pub N);

impl<T, C, D, N> NeighborhoodChunkStep<T, C, D> for CellsOnly<N>
where
    N: NeighborhoodStep<T, D>,
{
    fn run(
        &self,
        input: &NeighborhoodView<'_, T>,
        _chunks: &NeighborChunks<'_, C>,
        output: &mut DataSet<T>,
        _output_chunk: &mut C,
        context: &StepContext,
        shared: &D,
    ) -> PipelineStepResult {
        self.0.run(input, output, context, shared)
    }
}

/// Immutable prefix-stage chunk data for every chunk intersecting the
/// declared, world-clipped neighborhood (including the requested chunk).
///
/// Each entry is that chunk's complete prefix chunk data, not clipped
/// to the readable region.
pub struct NeighborChunks<'a, C> {
    chunks: &'a [Option<C>],
    first: ChunkCoord,
    last: ChunkCoord,
    readable_min: GlobalPosition,
    readable_max: GlobalPosition,
    context: StepContext,
}

impl<C> NeighborChunks<'_, C> {
    /// Returns:
    /// - Ok(value): a chunk intersecting the declared neighborhood.
    /// - Err: a chunk outside the declared neighborhood or missing input.
    pub fn get(&self, coord: ChunkCoord) -> Result<&C, String> {
        let index = chunk_index(self.first, self.last, coord)
            .ok_or_else(|| format!("Chunk {coord:?} is outside the declared neighborhood"))?;

        self.chunks
            .get(index)
            .and_then(Option::as_ref)
            .ok_or_else(|| format!("Missing internal neighborhood chunk data for {coord:?}"))
    }

    /// Reads chunk data of the chunk containing a global position.
    ///
    /// Same contract as `NeighborhoodView::get_global`:
    /// Ok(None) for declared outside-world positions, Err for undeclared reads.
    pub fn get_global(&self, position: GlobalPosition) -> Result<Option<&C>, String> {
        if !contains(self.readable_min, self.readable_max, position) {
            return Err(format!(
                "Read {position:?} is outside the declared neighborhood"
            ));
        }

        let Some((coord, _)) = self.context.config().locate(position) else {
            return Ok(None);
        };

        self.get(coord).map(Some)
    }
}

fn chunk_index(first: ChunkCoord, last: ChunkCoord, coord: ChunkCoord) -> Option<usize> {
    let inside = (first.0..=last.0).contains(&coord.0)
        && (first.1..=last.1).contains(&coord.1)
        && (first.2..=last.2).contains(&coord.2);

    if !inside {
        return None;
    }

    let width = (last.0 - first.0) as usize + 1;
    let height = (last.1 - first.1) as usize + 1;

    Some(
        (coord.0 - first.0) as usize
            + (coord.1 - first.1) as usize * width
            + (coord.2 - first.2) as usize * width * height,
    )
}

/// Immutable prefix-stage input.
///
/// The backing dataset is clipped to world bounds.
/// The readable declaration may extend beyond those bounds.
pub struct NeighborhoodView<'a, T> {
    data: &'a DataSet<Option<T>>,
    input_origin: GlobalPosition,
    readable_min: GlobalPosition,
    readable_max: GlobalPosition,
    context: StepContext,
}

impl<T> NeighborhoodView<'_, T> {
    /// Reads a global position.
    ///
    /// Returns:
    /// - Ok(Some(value)): valid world cell in the declared neighborhood.
    /// - Ok(None): outside-world cell in the declared neighborhood.
    /// - Err: undeclared read or missing internal input.
    pub fn get_global(&self, position: GlobalPosition) -> Result<Option<&T>, String> {
        if !contains(self.readable_min, self.readable_max, position) {
            return Err(format!(
                "Read {position:?} is outside the declared neighborhood"
            ));
        }

        if !self.context.config().world().contains(position) {
            return Ok(None);
        }

        let local = (
            local_axis(position.0, self.input_origin.0)?,
            local_axis(position.1, self.input_origin.1)?,
            local_axis(position.2, self.input_origin.2)?,
        );

        let cell = self
            .data
            .get(local.0, local.1, local.2)
            .ok_or_else(|| format!("Missing internal neighborhood cell at {position:?}"))?;

        cell.as_ref()
            .map(Some)
            .ok_or_else(|| format!("Uninitialized internal neighborhood cell at {position:?}"))
    }

    /// Reads relative to a valid output-local cell.
    pub fn get_offset(
        &self,
        local: LocalPosition,
        offset: (i64, i64, i64),
    ) -> Result<Option<&T>, String> {
        let global = self
            .context
            .global_position(local)
            .ok_or_else(|| "Invalid output-local neighborhood position".to_owned())?;

        let position = (
            global
                .0
                .checked_add(offset.0)
                .ok_or_else(|| "Neighborhood X offset overflow".to_owned())?,
            global
                .1
                .checked_add(offset.1)
                .ok_or_else(|| "Neighborhood Y offset overflow".to_owned())?,
            global
                .2
                .checked_add(offset.2)
                .ok_or_else(|| "Neighborhood Z offset overflow".to_owned())?,
        );

        self.get_global(position)
    }
}

/// One neighborhood stage between a prefix definition and local suffix steps.
///
/// This initial API only wraps a regular Pipeline, preventing accidental
/// stacking of neighborhood stages without dependency-footprint planning.
pub struct NeighborhoodPipeline<T, P, N, S = EmptyStep> {
    prefix: P,
    neighborhood: Neighborhood,
    limits: NeighborhoodLimits,
    step: N,
    suffix: S,
    _marker: PhantomData<fn() -> T>,
}

impl<T, S, F, D, C> Pipeline<T, S, F, D, C>
where
    T: Clone + Send + Sync + 'static,
    D: Send + Sync + 'static,
    C: Clone + Send + Sync + 'static,
    S: ChunkStep<T, C, D>,
    F: Fn(GlobalPosition) -> T + Send + Sync + 'static,
{
    /// Wraps this pipeline with a cell-only neighborhood stage.
    pub fn with_neighborhood<N>(
        self,
        neighborhood: Neighborhood,
        limits: NeighborhoodLimits,
        step: N,
    ) -> NeighborhoodPipeline<T, Self, CellsOnly<N>>
    where
        N: NeighborhoodStep<T, D>,
    {
        self.with_chunk_neighborhood(neighborhood, limits, CellsOnly(step))
    }

    /// Wraps this pipeline with a neighborhood stage that also reads prefix
    /// chunk data of dependency chunks and writes output chunk data.
    pub fn with_chunk_neighborhood<N>(
        self,
        neighborhood: Neighborhood,
        limits: NeighborhoodLimits,
        step: N,
    ) -> NeighborhoodPipeline<T, Self, N>
    where
        N: NeighborhoodChunkStep<T, C, D>,
    {
        NeighborhoodPipeline {
            prefix: self,
            neighborhood,
            limits,
            step,
            suffix: EmptyStep,
            _marker: PhantomData,
        }
    }
}

impl<T, P, N, S> NeighborhoodPipeline<T, P, N, S>
where
    T: Clone + Send + Sync + 'static,
    P: GenerationDefinition<T>,
    P::ChunkData: Clone,
    N: NeighborhoodChunkStep<T, P::ChunkData, P::Shared>,
    S: ChunkStep<T, P::ChunkData, P::Shared>,
{
    pub fn add_step<U>(self, step: U) -> NeighborhoodPipeline<T, P, N, Chain<S, LocalStep<U>>>
    where
        U: Step<T>,
    {
        self.chain(LocalStep(step))
    }

    pub fn add_shared_step<U>(
        self,
        step: U,
    ) -> NeighborhoodPipeline<T, P, N, Chain<S, SharedOnly<U>>>
    where
        U: SharedStep<T, P::Shared>,
    {
        self.chain(SharedOnly(step))
    }

    pub fn add_chunk_step<U>(self, step: U) -> NeighborhoodPipeline<T, P, N, Chain<S, U>>
    where
        U: ChunkStep<T, P::ChunkData, P::Shared>,
    {
        self.chain(step)
    }

    fn chain<U>(self, step: U) -> NeighborhoodPipeline<T, P, N, Chain<S, U>> {
        NeighborhoodPipeline {
            prefix: self.prefix,
            neighborhood: self.neighborhood,
            limits: self.limits,
            step: self.step,
            suffix: Chain {
                first: self.suffix,
                second: step,
            },
            _marker: PhantomData,
        }
    }

    /// Synchronous diagnostic execution for a pipeline without preparation.
    pub fn generate_chunk(&self, coord: ChunkCoord) -> Result<Chunk<T, P::ChunkData>, String> {
        if self.prefix.requires_preparation() {
            return Err(
                "Pipeline requires preparation; use the asynchronous runtime \
                 or prepare_shared + generate_chunk_with_shared"
                    .to_owned(),
            );
        }

        let shared = self.prefix.prepare_shared()?;

        self.generate_chunk_with_shared(coord, shared.as_ref())
    }
}

impl<T, P, N, S> GenerationDefinition<T> for NeighborhoodPipeline<T, P, N, S>
where
    T: Clone + Send + Sync + 'static,
    P: GenerationDefinition<T>,
    P::ChunkData: Clone,
    N: NeighborhoodChunkStep<T, P::ChunkData, P::Shared>,
    S: ChunkStep<T, P::ChunkData, P::Shared>,
{
    type Shared = P::Shared;
    type ChunkData = P::ChunkData;

    fn config(&self) -> PipelineConfig {
        self.prefix.config()
    }

    fn requires_preparation(&self) -> bool {
        self.prefix.requires_preparation()
    }

    fn prepare_shared(&self) -> Result<Arc<Self::Shared>, String> {
        self.prefix.prepare_shared()
    }

    fn generate_chunk_with_shared(
        &self,
        coord: ChunkCoord,
        shared: &Self::Shared,
    ) -> Result<Chunk<T, P::ChunkData>, String> {
        let config = self.prefix.config();
        let context = config.context(coord).map_err(str::to_owned)?;

        // Validate all planning and budgets before invoking any prefix work.
        let plan = Plan::new(config, context, self.neighborhood, self.limits)?;

        let mut input = DataSet::new(
            plan.input_dimensions.0,
            plan.input_dimensions.1,
            plan.input_dimensions.2,
            |_| None,
        )
        .map_err(str::to_owned)?;

        // The requested prefix result becomes output.
        let mut output = self
            .prefix
            .generate_chunk_with_shared(coord, shared)
            .map_err(|error| format!("Requested prefix failed: {error}"))?;

        copy_intersection(output.data(), context, &plan, &mut input)?;

        // Prefix chunk data for every planned chunk. The requested chunk's
        // entry is an immutable snapshot; output keeps the mutable original.
        // Planning bounded the chunk count by max_dependency_chunks.
        let chunk_count = chunk_index(plan.first, plan.last, plan.last)
            .map(|index| index + 1)
            .ok_or_else(|| "Invalid neighborhood chunk range".to_owned())?;

        let mut chunks: Vec<Option<P::ChunkData>> = (0..chunk_count).map(|_| None).collect();

        let requested_index = chunk_index(plan.first, plan.last, coord)
            .ok_or_else(|| "Requested chunk outside its neighborhood plan".to_owned())?;

        chunks[requested_index] = Some(output.chunk_data().clone());

        // Dependency chunks execute sequentially in this same request job.
        // They run through the prefix only; cells are dropped after copying
        // and chunk data is retained for the neighborhood step.
        for z in plan.first.2..=plan.last.2 {
            for y in plan.first.1..=plan.last.1 {
                for x in plan.first.0..=plan.last.0 {
                    let dependency_coord = ChunkCoord(x, y, z);

                    if dependency_coord == coord {
                        continue;
                    }

                    let dependency = self
                        .prefix
                        .generate_chunk_with_shared(dependency_coord, shared)
                        .map_err(|error| {
                            format!("Prefix dependency {dependency_coord:?} failed: {error}")
                        })?;

                    copy_intersection(dependency.data(), dependency.context(), &plan, &mut input)?;

                    let index = chunk_index(plan.first, plan.last, dependency_coord)
                        .ok_or_else(|| "Dependency chunk outside its plan".to_owned())?;

                    chunks[index] = Some(dependency.into_parts().1);
                }
            }
        }

        if input.as_slice().iter().any(Option::is_none) {
            return Err("Neighborhood planning left missing internal inputs".to_owned());
        }

        // The halo remains immutable. Output starts from the requested prefix.
        // A separate center snapshot is unnecessary: the halo already holds
        // cloned prefix values for every requested output cell.
        let view = NeighborhoodView {
            data: &input,
            input_origin: plan.input_min,
            readable_min: plan.readable_min,
            readable_max: plan.readable_max,
            context,
        };

        let neighbor_chunks = NeighborChunks {
            chunks: &chunks,
            first: plan.first,
            last: plan.last,
            readable_min: plan.readable_min,
            readable_max: plan.readable_max,
            context,
        };

        let Chunk {
            context: _,
            data: output_data,
            chunk_data: output_chunk,
        } = &mut output;

        self.step
            .run(
                &view,
                &neighbor_chunks,
                output_data,
                output_chunk,
                &context,
                shared,
            )
            .map_err(|error| format!("Neighborhood stage failed: {error}"))?;

        ChunkStep::run(&self.suffix, output_data, output_chunk, &context, shared)
            .map_err(|error| format!("Neighborhood suffix failed: {error}"))?;

        Ok(output)
    }
}

struct Plan {
    readable_min: GlobalPosition,
    readable_max: GlobalPosition,
    input_min: GlobalPosition,
    input_max: GlobalPosition,
    input_dimensions: (usize, usize, usize),
    first: ChunkCoord,
    last: ChunkCoord,
}

impl Plan {
    fn new(
        config: PipelineConfig,
        context: StepContext,
        neighborhood: Neighborhood,
        limits: NeighborhoodLimits,
    ) -> Result<Self, String> {
        let origin = context.origin();
        let dimensions = context.dimensions();
        let (rx, ry, rz) = neighborhood.radii();
        let (rx, ry, rz) = (i64::from(rx), i64::from(ry), i64::from(rz));

        let output_max = (
            checked_end(origin.0, dimensions.0)?,
            checked_end(origin.1, dimensions.1)?,
            checked_end(origin.2, dimensions.2)?,
        );

        let readable_min = (
            checked_sub_radius(origin.0, rx)?,
            checked_sub_radius(origin.1, ry)?,
            checked_sub_radius(origin.2, rz)?,
        );

        let readable_max = (
            checked_add_radius(output_max.0, rx)?,
            checked_add_radius(output_max.1, ry)?,
            checked_add_radius(output_max.2, rz)?,
        );

        let world_min = config.world().origin();
        let world_dimensions = config.world().dimensions();

        // WorldBounds construction validates these exclusive boundaries.
        let world_max = (
            world_min.0 + i64::from(world_dimensions.0),
            world_min.1 + i64::from(world_dimensions.1),
            world_min.2 + i64::from(world_dimensions.2),
        );

        let input_min = (
            readable_min.0.max(world_min.0),
            readable_min.1.max(world_min.1),
            readable_min.2.max(world_min.2),
        );

        let input_max = (
            readable_max.0.min(world_max.0),
            readable_max.1.min(world_max.1),
            readable_max.2.min(world_max.2),
        );

        let input_dimensions = (
            checked_extent(input_min.0, input_max.0)?,
            checked_extent(input_min.1, input_max.1)?,
            checked_extent(input_min.2, input_max.2)?,
        );

        let first = config
            .locate(input_min)
            .ok_or_else(|| "Invalid neighborhood minimum".to_owned())?
            .0;

        let last_position = (input_max.0 - 1, input_max.1 - 1, input_max.2 - 1);

        let last = config
            .locate(last_position)
            .ok_or_else(|| "Invalid neighborhood maximum".to_owned())?
            .0;

        let chunk_count = (u128::from(last.0) - u128::from(first.0) + 1)
            * (u128::from(last.1) - u128::from(first.1) + 1)
            * (u128::from(last.2) - u128::from(first.2) + 1);

        if chunk_count > limits.max_dependency_chunks as u128 {
            return Err(format!(
                "Neighborhood requires {chunk_count} prefix chunks; limit is {}",
                limits.max_dependency_chunks,
            ));
        }

        let halo_cells = volume(input_dimensions)?;
        let output_cells = volume(dimensions)?;
        let mut prefix_cells = 0usize;
        let mut largest_dependency = 0usize;

        // The chunk-count check above bounds this planning loop.
        for z in first.2..=last.2 {
            for y in first.1..=last.1 {
                for x in first.0..=last.0 {
                    let dependency_context =
                        config.context(ChunkCoord(x, y, z)).map_err(str::to_owned)?;

                    let cells = volume(dependency_context.dimensions())?;

                    prefix_cells = prefix_cells
                        .checked_add(cells)
                        .ok_or_else(|| "Prefix work count overflow".to_owned())?;

                    if dependency_context.chunk_coord() != context.chunk_coord() {
                        largest_dependency = largest_dependency.max(cells);
                    }
                }
            }
        }

        if prefix_cells > limits.max_prefix_cells {
            return Err(format!(
                "Neighborhood requires {prefix_cells} prefix cells; limit is {}",
                limits.max_prefix_cells,
            ));
        }

        // Halo + retained output + at most one other dependency.
        // The halo itself is the immutable center snapshot.
        let temporary_cells = halo_cells
            .checked_add(output_cells)
            .and_then(|cells| cells.checked_add(largest_dependency))
            .ok_or_else(|| "Neighborhood temporary cell count overflow".to_owned())?;

        if temporary_cells > limits.max_temporary_cells {
            return Err(format!(
                "Neighborhood requires {temporary_cells} temporary cells; limit is {}",
                limits.max_temporary_cells,
            ));
        }

        Ok(Self {
            readable_min,
            readable_max,
            input_min,
            input_max,
            input_dimensions,
            first,
            last,
        })
    }
}

fn copy_intersection<T: Clone>(
    source: &DataSet<T>,
    context: StepContext,
    plan: &Plan,
    input: &mut DataSet<Option<T>>,
) -> Result<(), String> {
    let origin = context.origin();
    let dimensions = context.dimensions();

    let source_max = (
        checked_end(origin.0, dimensions.0)?,
        checked_end(origin.1, dimensions.1)?,
        checked_end(origin.2, dimensions.2)?,
    );

    let min = (
        origin.0.max(plan.input_min.0),
        origin.1.max(plan.input_min.1),
        origin.2.max(plan.input_min.2),
    );

    let max = (
        source_max.0.min(plan.input_max.0),
        source_max.1.min(plan.input_max.1),
        source_max.2.min(plan.input_max.2),
    );

    for z in min.2..max.2 {
        for y in min.1..max.1 {
            for x in min.0..max.0 {
                let source_local = (
                    local_axis(x, origin.0)?,
                    local_axis(y, origin.1)?,
                    local_axis(z, origin.2)?,
                );

                let input_local = (
                    local_axis(x, plan.input_min.0)?,
                    local_axis(y, plan.input_min.1)?,
                    local_axis(z, plan.input_min.2)?,
                );

                let value = source
                    .get(source_local.0, source_local.1, source_local.2)
                    .ok_or_else(|| "Prefix dataset dimensions mismatch".to_owned())?;

                let destination = input
                    .get_mut(input_local.0, input_local.1, input_local.2)
                    .ok_or_else(|| "Neighborhood copy outside input bounds".to_owned())?;

                *destination = Some(value.clone());
            }
        }
    }

    Ok(())
}

fn contains(min: GlobalPosition, max: GlobalPosition, position: GlobalPosition) -> bool {
    position.0 >= min.0
        && position.0 < max.0
        && position.1 >= min.1
        && position.1 < max.1
        && position.2 >= min.2
        && position.2 < max.2
}

fn local_axis(value: i64, origin: i64) -> Result<usize, String> {
    let offset = value
        .checked_sub(origin)
        .ok_or_else(|| "Neighborhood coordinate subtraction overflow".to_owned())?;

    usize::try_from(offset).map_err(|_| "Neighborhood coordinate does not fit usize".to_owned())
}

fn checked_end(origin: i64, size: usize) -> Result<i64, String> {
    let size =
        i64::try_from(size).map_err(|_| "Neighborhood extent does not fit i64".to_owned())?;

    origin
        .checked_add(size)
        .ok_or_else(|| "Neighborhood exclusive boundary overflow".to_owned())
}

fn checked_extent(min: i64, max: i64) -> Result<usize, String> {
    let extent = max
        .checked_sub(min)
        .ok_or_else(|| "Neighborhood extent overflow".to_owned())?;

    if extent <= 0 {
        return Err("Neighborhood input extent must be positive".to_owned());
    }

    usize::try_from(extent).map_err(|_| "Neighborhood extent does not fit usize".to_owned())
}

fn checked_sub_radius(value: i64, radius: i64) -> Result<i64, String> {
    value
        .checked_sub(radius)
        .ok_or_else(|| "Neighborhood minimum coordinate overflow".to_owned())
}

fn checked_add_radius(value: i64, radius: i64) -> Result<i64, String> {
    value
        .checked_add(radius)
        .ok_or_else(|| "Neighborhood maximum coordinate overflow".to_owned())
}

fn volume(dimensions: (usize, usize, usize)) -> Result<usize, String> {
    dimensions
        .0
        .checked_mul(dimensions.1)
        .and_then(|area| area.checked_mul(dimensions.2))
        .ok_or_else(|| "Neighborhood volume overflow".to_owned())
}
