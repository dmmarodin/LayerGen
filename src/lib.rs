mod data_set;
mod definition;
mod manager;
mod neighborhood;
mod persistence;
mod runtime;
mod spatial;
mod step;

pub use data_set::*;
pub use definition::*;
pub use manager::*;
pub use neighborhood::*;
pub use persistence::*;
pub use runtime::*;
pub use spatial::*;
pub use step::*;

use std::{marker::PhantomData, sync::Arc};

use bevy_ecs::prelude::Resource;

type PreparationFn<D> = dyn Fn(&PipelineConfig) -> Result<D, String> + Send + Sync + 'static;

enum SharedSource<D> {
    Ready(Arc<D>),
    Prepare(Arc<PreparationFn<D>>),
}

pub struct PipelineBuilder<T, S = EmptyStep, D = ()> {
    config: PipelineConfig,
    steps: S,
    shared: SharedSource<D>,
    _marker: PhantomData<fn() -> T>,
}

impl<T> PipelineBuilder<T, EmptyStep, ()> {
    pub fn new(config: PipelineConfig) -> Self {
        Self {
            config,
            steps: EmptyStep,
            shared: SharedSource::Ready(Arc::new(())),
            _marker: PhantomData,
        }
    }

    /// Defines background preparation.
    ///
    /// Call this before adding steps.
    /// Building the pipeline does not execute this function.
    ///
    /// The runtime executes it once per installed generator.
    pub fn prepare<D, F>(self, preparation: F) -> PipelineBuilder<T, EmptyStep, D>
    where
        D: Send + Sync + 'static,
        F: Fn(&PipelineConfig) -> Result<D, String> + Send + Sync + 'static,
    {
        PipelineBuilder {
            config: self.config,
            steps: EmptyStep,
            shared: SharedSource::Prepare(Arc::new(preparation)),
            _marker: PhantomData,
        }
    }
}

impl<T, S, D> PipelineBuilder<T, S, D>
where
    D: Send + Sync + 'static,
    S: SharedStep<T, D>,
{
    /// Adds an existing local step that does not read shared data.
    pub fn add_step<U>(self, step: U) -> PipelineBuilder<T, Chain<S, LocalStep<U>>, D>
    where
        U: Step<T>,
    {
        PipelineBuilder {
            config: self.config,
            steps: Chain {
                first: self.steps,
                second: LocalStep(step),
            },
            shared: self.shared,
            _marker: PhantomData,
        }
    }

    /// Adds a step that can read prepared shared data.
    pub fn add_shared_step<U>(self, step: U) -> PipelineBuilder<T, Chain<S, U>, D>
    where
        U: SharedStep<T, D>,
    {
        PipelineBuilder {
            config: self.config,
            steps: Chain {
                first: self.steps,
                second: step,
            },
            shared: self.shared,
            _marker: PhantomData,
        }
    }

    /// Builds a definition without preparing shared data or allocating chunks.
    pub fn build<F>(self, initializer: F) -> Pipeline<T, S, F, D>
    where
        T: Send + Sync + 'static,
        F: Fn(GlobalPosition) -> T + Send + Sync + 'static,
    {
        Pipeline {
            config: self.config,
            steps: self.steps,
            initializer,
            shared: self.shared,
            _marker: PhantomData,
        }
    }
}

#[derive(Resource)]
pub struct Pipeline<T, S, F, D = ()>
where
    T: Send + Sync + 'static,
    S: Send + Sync + 'static,
    F: Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    config: PipelineConfig,
    steps: S,
    initializer: F,
    shared: SharedSource<D>,
    _marker: PhantomData<fn() -> T>,
}

impl<T, S, F, D> Pipeline<T, S, F, D>
where
    T: Send + Sync + 'static,
    D: Send + Sync + 'static,
    S: SharedStep<T, D>,
    F: Fn(GlobalPosition) -> T + Send + Sync + 'static,
{
    pub fn config(&self) -> PipelineConfig {
        self.config
    }

    pub fn requires_preparation(&self) -> bool {
        matches!(&self.shared, SharedSource::Prepare(_))
    }

    /// Synchronously obtains shared data for diagnostic execution.
    ///
    /// This method does not cache preparation. Calling it repeatedly on
    /// a prepared pipeline runs preparation repeatedly.
    ///
    /// LayerGenPlugin instead calls it exactly once per generator.
    /// Panics propagate to direct callers; the runtime catches unwinding
    /// preparation panics.
    pub fn prepare_shared(&self) -> Result<Arc<D>, String> {
        match &self.shared {
            SharedSource::Ready(shared) => Ok(Arc::clone(shared)),
            SharedSource::Prepare(prepare) => prepare(&self.config).map(Arc::new),
        }
    }

    /// Synchronous diagnostic execution for pipelines with ready shared data.
    ///
    /// For pipelines configured with `.prepare(...)`, explicitly obtain
    /// shared data with `prepare_shared` and call
    /// `generate_chunk_with_shared`, or use the async runtime.
    pub fn generate_chunk(&self, coord: ChunkCoord) -> Result<Chunk<T>, String> {
        match &self.shared {
            SharedSource::Ready(shared) => self.generate_chunk_with_shared(coord, shared.as_ref()),
            SharedSource::Prepare(_) => Err(
                "Pipeline requires preparation; use the asynchronous runtime \
                 or prepare_shared + generate_chunk_with_shared"
                    .to_owned(),
            ),
        }
    }

    /// Processes one chunk using explicitly supplied immutable shared data.
    ///
    /// Returns base data only. Halos and persistence are not implemented.
    pub fn generate_chunk_with_shared(
        &self,
        coord: ChunkCoord,
        shared: &D,
    ) -> Result<Chunk<T>, String> {
        let context = self.config.context(coord).map_err(str::to_owned)?;
        let (width, height, depth) = context.dimensions();

        let mut data = DataSet::new(width, height, depth, |local| {
            let global = context
                .global_position(local)
                .expect("Validated chunk coordinates must fit world bounds");

            (self.initializer)(global)
        })
        .map_err(str::to_owned)?;

        SharedStep::run(&self.steps, &mut data, &context, shared)?;

        Ok(Chunk { context, data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_ecs::prelude::World;

    struct AddSeed;

    impl Step<u64> for AddSeed {
        fn run(&self, data: &mut DataSet<u64>, context: &StepContext) -> PipelineStepResult {
            for (value, _, _, _) in data.iter_mut() {
                *value += context.config().seed();
            }

            Ok(())
        }
    }

    fn config() -> PipelineConfig {
        PipelineConfig::new(
            WorldBounds::new((10, 20, 30), (5, 4, 4)).unwrap(),
            ChunkSize::new(4, 4, 4).unwrap(),
            7,
        )
    }

    #[test]
    fn pipeline_is_reusable_and_initializes_global_positions() {
        let pipeline = PipelineBuilder::new(config())
            .add_step(AddSeed)
            .build(|position| position.0 as u64);

        let first = pipeline.generate_chunk(ChunkCoord(0, 0, 0)).unwrap();
        let edge = pipeline.generate_chunk(ChunkCoord(1, 0, 0)).unwrap();
        let repeated = pipeline.generate_chunk(ChunkCoord(0, 0, 0)).unwrap();

        assert_eq!(first.data()[(0, 0, 0)], 17);
        assert_eq!(edge.origin(), (14, 20, 30));
        assert_eq!(edge.data().dimensions(), (1, 4, 4));
        assert_eq!(edge.data()[(0, 0, 0)], 21);
        assert_eq!(first.data().as_slice(), repeated.data().as_slice());
    }

    #[test]
    fn pipeline_can_be_inserted_into_bevy() {
        let pipeline = PipelineBuilder::new(config()).build(|_| 0u32);

        fn insert_resource<R: Resource>(world: &mut World, resource: R) {
            world.insert_resource(resource);
            assert!(world.contains_resource::<R>());
        }

        insert_resource(&mut World::new(), pipeline);
    }

    #[test]
    fn building_does_not_allocate_or_initialize_data() {
        let pipeline = PipelineBuilder::<u32>::new(config()).build(|_| {
            panic!("Initializer must not run during build");
        });

        assert_eq!(pipeline.config(), config());
    }

    #[test]
    fn building_does_not_prepare() {
        let pipeline = PipelineBuilder::<u32>::new(config())
            .prepare(|_| -> Result<u32, String> {
                panic!("Preparation must not run during build");
            })
            .build(|_| 0);

        assert!(pipeline.requires_preparation());
    }

    #[test]
    fn outside_request_fails_before_initialization() {
        let pipeline = PipelineBuilder::<u32>::new(config()).build(|_| {
            panic!("Initializer must not run for invalid requests");
        });

        assert!(pipeline.generate_chunk(ChunkCoord(99, 0, 0)).is_err());
    }

    #[test]
    fn prepared_pipeline_supports_explicit_synchronous_execution() {
        let pipeline = PipelineBuilder::<u32>::new(config())
            .prepare(|_| Ok(42u32))
            .build(|_| 0);

        assert!(pipeline.generate_chunk(ChunkCoord(0, 0, 0)).is_err());

        let shared = pipeline.prepare_shared().unwrap();
        let chunk = pipeline
            .generate_chunk_with_shared(ChunkCoord(0, 0, 0), &shared)
            .unwrap();

        assert_eq!(chunk.data().dimensions(), (4, 4, 4));
    }
}
