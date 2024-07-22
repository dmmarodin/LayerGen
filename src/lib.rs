use rayon::prelude::*;
use std::marker::PhantomData;

/// Estrutura de dados para representar uma grade de voxels genérica.
pub struct DataSet<T> {
    pub data: Vec<T>,
    pub width: usize,
    pub height: usize,
    pub depth: usize,
}

impl<T> DataSet<T>
where
    T: Send + Sync,
{
    pub fn new<F>(width: usize, height: usize, depth: usize, mut initializer: F) -> Self
    where
        F: FnMut(usize, usize, usize) -> T,
    {
        let mut data = Vec::with_capacity(width * height * depth);
        for z in 0..depth {
            for y in 0..height {
                for x in 0..width {
                    data.push(initializer(x, y, z));
                }
            }
        }
        DataSet {
            data,
            width,
            height,
            depth,
        }
    }

    pub fn get(&self, x: usize, y: usize, z: usize) -> Option<&T> {
        if x < self.width && y < self.height && z < self.depth {
            Some(&self.data[x + y * self.width + z * self.width * self.height])
        } else {
            None
        }
    }

    pub fn get_mut(&mut self, x: usize, y: usize, z: usize) -> Option<&mut T> {
        if x < self.width && y < self.height && z < self.depth {
            Some(&mut self.data[x + y * self.width + z * self.width * self.height])
        } else {
            None
        }
    }

    pub fn neighbors(&self, x: usize, y: usize, z: usize) -> Vec<Option<&T>> {
        vec![self.get(x, y + 1, z), self.get(x, y - 1, z)]
    }

    pub fn par_iter_mut(
        &mut self,
    ) -> impl ParallelIterator<Item = (&mut T, usize, usize, usize)> + '_ {
        let width = self.width;
        let height = self.height;
        // let depth = self.depth;

        self.data.par_iter_mut().enumerate().map(move |(i, voxel)| {
            let x = i % width;
            let y = (i / width) % height;
            let z = i / (width * height);
            (voxel, x, y, z)
        })
    }
}

/// Traço para representar um passo no pipeline.
pub trait Step<T> {
    fn run(&self, grid: &mut DataSet<T>);
}

/// Estrutura para construir um pipeline.
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

/// Estrutura para representar um pipeline.
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

/// Passo vazio para inicializar o pipeline.
pub struct EmptyStep;

impl<T> Step<T> for EmptyStep {
    fn run(&self, _grid: &mut DataSet<T>) {}
}

/// Estrutura para encadear dois passos do pipeline.
pub struct Chain<A, B> {
    first: A,
    second: B,
}

impl<T, A, B> Step<T> for Chain<A, B>
where
    A: Step<T>,
    B: Step<T>,
{
    fn run(&self, grid: &mut DataSet<T>) {
        self.first.run(grid);
        self.second.run(grid);
    }
}
