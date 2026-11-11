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

#[derive(Clone, Copy, Debug, Default)]
pub struct EmptyStep;

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
