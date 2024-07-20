#![allow(unused_assignments)]
#![allow(dead_code)]

use std::any::Any;
use std::convert::AsRef;

trait StepConfig: Any + Send + Sync + AsRef<dyn Any> {}

#[derive(Debug)]
struct WorldGenConfig {
    map_size: usize,
}

#[derive(Debug)]
struct NoiseStepConfig {
    seed: u64,
    octaves: u32,
}

impl NoiseStepConfig {
    fn new(seed: u64, octaves: u32) -> Self {
        Self { seed, octaves }
    }
}

impl AsRef<dyn Any> for NoiseStepConfig {
    fn as_ref(&self) -> &(dyn Any) {
        self
    }
}
impl StepConfig for NoiseStepConfig {}

#[derive(Debug)]
struct VoxelChunk;

type StepFunction<'a> = Box<dyn Fn(&mut VoxelChunk, &dyn StepConfig) + 'a>;

struct Step<'a> {
    function: StepFunction<'a>,
    config: Box<dyn StepConfig + 'static>,
}

impl<'a> Step<'a> {
    fn new<C: StepConfig + 'a>(function: StepFunction<'a>, config: C) -> Self {
        Self {
            function,
            config: Box::new(config),
        }
    }

    fn run(&self, chunk: &mut VoxelChunk) {
        (self.function)(chunk, &*self.config);
    }
}

struct Pipeline<'a> {
    steps: Vec<Step<'a>>,
    shared_config: WorldGenConfig,
}

impl<'a> Pipeline<'a> {
    fn run(&self, chunk: &mut VoxelChunk) {
        for step in &self.steps {
            step.run(chunk);
        }
    }
}

// --- Example Usage ---

fn main() {
    let noise_config = NoiseStepConfig::new(12345, 8);

    let noise_step = Step::new(
        Box::new(|_chunk, config| {
            // Correct downcasting:
            let noise_config = config
                .as_ref() // Convert to &dyn Any
                .downcast_ref::<NoiseStepConfig>()
                .unwrap();

            println!("Noise config: {:?}", noise_config);
        }),
        noise_config,
    );

    let pipeline = Pipeline {
        steps: vec![noise_step],
        shared_config: WorldGenConfig { map_size: 256 },
    };

    let mut chunk = VoxelChunk;
    pipeline.run(&mut chunk);
}
