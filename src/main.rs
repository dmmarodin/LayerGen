use layergen_rs::*;
use rayon::prelude::*;

#[derive(Clone)]
struct Voxel {
    x: usize,
    y: usize,
    z: usize,
    temperature: f32,
}

impl Voxel {
    fn new(x: usize, y: usize, z: usize) -> Self {
        Voxel {
            x,
            y,
            z,
            temperature: 0.0,
        }
    }
}

struct TemperatureStep;

impl Step<Voxel> for TemperatureStep {
    fn run(&self, grid: &mut DataSet<Voxel>) {
        grid.par_iter_mut().for_each(|(voxel, _x, _y, _z)| {
            // Lógica para alterar a temperatura
            voxel.temperature += 1.0;
        });
    }
}

fn main() {
    let mut grid = DataSet::new(10, 10, 50, |x, y, z| Voxel::new(x, y, z));

    let pipeline = PipelineBuilder::new().add_step(TemperatureStep).build();

    pipeline.run(&mut grid);

    for z in 0..grid.depth {
        for y in 0..grid.height {
            for x in 0..grid.width {
                let voxel = grid.get(x, y, z).unwrap();
                println!(
                    "Voxel ({}, {}, {}): Temperature = {}",
                    voxel.x, voxel.y, voxel.z, voxel.temperature
                );
            }
        }
    }
}
