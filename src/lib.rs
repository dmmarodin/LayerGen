mod data_set;
mod step;

pub use data_set::*;
pub use step::*;

use std::marker::PhantomData;

pub struct PipelineBuilder<T, S> {
    steps: S,
    _marker: PhantomData<T>,
}

impl<T> PipelineBuilder<T, EmptyStep> {
    pub fn new() -> Self {
        PipelineBuilder {
            steps: EmptyStep,
            _marker: PhantomData,
        }
    }
}

impl<T, S> PipelineBuilder<T, S> {
    pub fn add_step<U>(self, step: U) -> PipelineBuilder<T, Chain<S, U>>
    where
        S: Step<T>,
        U: Step<T>,
    {
        PipelineBuilder {
            steps: Chain {
                first: self.steps,
                second: step,
            },
            _marker: PhantomData,
        }
    }

    pub fn build(self) -> Pipeline<S>
    where
        S: Step<T>,
    {
        Pipeline::new(self.steps)
    }
}

pub struct Pipeline<S> {
    steps: S,
}

impl<S> Pipeline<S> {
    pub fn new(steps: S) -> Self {
        Pipeline { steps }
    }

    pub fn run<T>(&self, grid: &mut DataSet<T>)
    where
        S: Step<T>,
    {
        self.steps.run(grid);
    }
}
