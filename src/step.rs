use crate::{DataSet, StepContext};

pub type PipelineStepResult = Result<(), String>;

/// A chunk-local processing step.
///
/// This existing contract remains unchanged.
pub trait Step<T>: Send + Sync + 'static {
    fn run(&self, data: &mut DataSet<T>, context: &StepContext) -> PipelineStepResult;
}

/// A processing step that also reads prepared shared data.
///
/// Each job owns its output dataset. Shared data is borrowed immutably.
pub trait SharedStep<T, D>: Send + Sync + 'static {
    fn run(&self, data: &mut DataSet<T>, context: &StepContext, shared: &D) -> PipelineStepResult;
}

/// A processing step that can also read and write per-chunk data `C`.
///
/// `C` is consumer-defined chunk-level data produced alongside the dense
/// cell dataset: per-chunk values, a depth-1 column dataset, or both.
/// It is part of the deterministic generated base, like `T`.
///
/// `Step` and `SharedStep` implementations participate through the
/// `LocalStep` and `SharedOnly` adapters, without seeing `C`.
pub trait ChunkStep<T, C, D>: Send + Sync + 'static {
    fn run(
        &self,
        data: &mut DataSet<T>,
        chunk: &mut C,
        context: &StepContext,
        shared: &D,
    ) -> PipelineStepResult;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct EmptyStep;

impl<T, C, D> ChunkStep<T, C, D> for EmptyStep {
    fn run(
        &self,
        _data: &mut DataSet<T>,
        _chunk: &mut C,
        _context: &StepContext,
        _shared: &D,
    ) -> PipelineStepResult {
        Ok(())
    }
}

impl<T> Step<T> for EmptyStep {
    fn run(&self, _data: &mut DataSet<T>, _context: &StepContext) -> PipelineStepResult {
        Ok(())
    }
}

impl<T, D> SharedStep<T, D> for EmptyStep {
    fn run(
        &self,
        _data: &mut DataSet<T>,
        _context: &StepContext,
        _shared: &D,
    ) -> PipelineStepResult {
        Ok(())
    }
}

/// Adapter used by PipelineBuilder::add_step.
///
/// A local step can participate in a pipeline with shared data
/// without needing to know its shared-data type.
#[derive(Clone, Debug)]
pub struct LocalStep<S>(pub S);

impl<T, D, S> SharedStep<T, D> for LocalStep<S>
where
    S: Step<T>,
{
    fn run(&self, data: &mut DataSet<T>, context: &StepContext, _shared: &D) -> PipelineStepResult {
        self.0.run(data, context)
    }
}

impl<T, C, D, S> ChunkStep<T, C, D> for LocalStep<S>
where
    S: Step<T>,
{
    fn run(
        &self,
        data: &mut DataSet<T>,
        _chunk: &mut C,
        context: &StepContext,
        _shared: &D,
    ) -> PipelineStepResult {
        self.0.run(data, context)
    }
}

/// Adapter used by PipelineBuilder::add_shared_step.
///
/// A shared step can participate in a pipeline with chunk data
/// without needing to know the chunk data type.
#[derive(Clone, Debug)]
pub struct SharedOnly<S>(pub S);

impl<T, C, D, S> ChunkStep<T, C, D> for SharedOnly<S>
where
    S: SharedStep<T, D>,
{
    fn run(
        &self,
        data: &mut DataSet<T>,
        _chunk: &mut C,
        context: &StepContext,
        shared: &D,
    ) -> PipelineStepResult {
        self.0.run(data, context, shared)
    }
}

#[derive(Clone, Debug)]
pub struct Chain<A, B> {
    pub first: A,
    pub second: B,
}

impl<T, A, B> Step<T> for Chain<A, B>
where
    A: Step<T>,
    B: Step<T>,
{
    fn run(&self, data: &mut DataSet<T>, context: &StepContext) -> PipelineStepResult {
        Step::run(&self.first, data, context)?;
        Step::run(&self.second, data, context)
    }
}

impl<T, D, A, B> SharedStep<T, D> for Chain<A, B>
where
    A: SharedStep<T, D>,
    B: SharedStep<T, D>,
{
    fn run(&self, data: &mut DataSet<T>, context: &StepContext, shared: &D) -> PipelineStepResult {
        SharedStep::run(&self.first, data, context, shared)?;
        SharedStep::run(&self.second, data, context, shared)
    }
}

impl<T, C, D, A, B> ChunkStep<T, C, D> for Chain<A, B>
where
    A: ChunkStep<T, C, D>,
    B: ChunkStep<T, C, D>,
{
    fn run(
        &self,
        data: &mut DataSet<T>,
        chunk: &mut C,
        context: &StepContext,
        shared: &D,
    ) -> PipelineStepResult {
        ChunkStep::run(&self.first, data, chunk, context, shared)?;
        ChunkStep::run(&self.second, data, chunk, context, shared)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChunkCoord, ChunkSize, PipelineConfig, WorldBounds};

    struct FailingStep;

    impl Step<u32> for FailingStep {
        fn run(&self, _data: &mut DataSet<u32>, _context: &StepContext) -> PipelineStepResult {
            Err("Planned error".into())
        }
    }

    struct IncrementStep;

    impl Step<u32> for IncrementStep {
        fn run(&self, data: &mut DataSet<u32>, _context: &StepContext) -> PipelineStepResult {
            data[(0, 0, 0)] += 1;
            Ok(())
        }
    }

    #[test]
    fn failure_skips_later_step() {
        let config = PipelineConfig::new(
            WorldBounds::new((0, 0, 0), (1, 1, 1)).unwrap(),
            ChunkSize::new(1, 1, 1).unwrap(),
            0,
        );

        let context = config.context(ChunkCoord(0, 0, 0)).unwrap();
        let mut data = DataSet::new(1, 1, 1, |_| 0).unwrap();

        let chain = Chain {
            first: FailingStep,
            second: IncrementStep,
        };

        assert!(Step::run(&chain, &mut data, &context).is_err());
        assert_eq!(data[(0, 0, 0)], 0);
    }
}
