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

    pub fn build<F>(
        self,
        width: usize,
        height: usize,
        depth: usize,
        initializer: F,
    ) -> Pipeline<T, S>
    where
        S: Step<T>,
        F: FnMut((usize, usize, usize)) -> T,
        T: Sync + Send + Sized,
    {
        let dataset = DataSet::new(width, height, depth, initializer).unwrap();
        Pipeline::new(self.steps, Some(dataset))
    }
}

pub struct Pipeline<T, S> {
    dataset: Option<DataSet<T>>,
    steps: S,
}

impl<T, S> Pipeline<T, S> {
    pub fn new(steps: S, dataset: Option<DataSet<T>>) -> Self {
        Pipeline { dataset, steps }
    }

    pub fn run(mut self) -> Result<DataSet<T>, String>
    where
        S: Step<T>,
    {
        if self.dataset.is_none() {
            return Err("Dataset not initialized".into());
        }

        let mut dataset = self.dataset.take().unwrap();

        self.steps.run(&mut dataset)?;

        Ok(dataset)
    }
}
