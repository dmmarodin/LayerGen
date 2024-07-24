use layergen_rs::*;
use rayon::prelude::*;

struct Voxel {
    x: usize,
    y: usize,
    z: usize,
    temperature: f32,
}

impl Voxel {
    fn new(pos: (usize, usize, usize)) -> Self {
        Voxel {
            x: pos.0,
            y: pos.1,
            z: pos.2,
            temperature: 0.0,
        }
    }
}

struct TemperatureStep;

impl Step<Voxel> for TemperatureStep {
    fn run(&self, dataset: &mut DataSet<Voxel>) -> PipelineStepResult {
        dataset.par_iter_mut().for_each(|(voxel, _x, _y, _z)| {
            voxel.temperature += 1.0;
        });
        Ok(())
    }
}

fn main() {
    let dataset = PipelineBuilder::new()
        .add_step(TemperatureStep)
        .build(10, 10, 50, |pos| Voxel::new(pos))
        .run()
        .expect("Error running pipeline");

    for z in 0..9 {
        for y in 0..9 {
            for x in 0..9 {
                let voxel = dataset.get(x, y, z).unwrap();
                println!(
                    "Voxel ({}, {}, {}): Temperature = {}",
                    voxel.x, voxel.y, voxel.z, voxel.temperature
                );
            }
        }
    }
}
