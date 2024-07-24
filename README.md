# LayerGen

A Rust data processing pipeline meant for calculating multi-dimensional data at realtime.
Initially created for on the fly procedural terrain generation, but can be used for iterating any sequence of steps on top of a data structure.

## Installation

The lib is not yet available as a crate, but you can add it as a github repo.

Add the dependency in `cargo.toml`:

```toml
[dependencies]
layergen = { git = "git@github.com:dmmarodin/LayerGen.git", branch="main" }
```

## Usage

The data pipeline requires three different objects: a data structure, processing steps and the pipeline itself.

The data structure can be any concrete type and will be mutated by the steps.
For this example, we will think in term of voxels.

```rust
enum Biome {
    Plains,
    Ocean
}

struct Voxel {
    pub pos_x: uint,
    pub pos_y: uint,
    pub pos_z: uint,
    pub biome: Biome
}
```

Now create one or more steps that will transform your dataset. All steps implement the `Step<T>` trait for the data structure.

```rust
struct BiomeDecisionStep;

impl Step<Voxel> for BiomeDecisionStep {
    fn run(&self, dataset: &mut DataSet<Voxel>) -> PipelineStepResult {
        dataset.par_iter_mut().for_each(|(voxel, _x, _y, _z)| {
            voxel.biome = if voxel.pos_y > 10 { Biome::Plains } else { Biome::Ocean };
        });
        Ok(())
    }
}
```

With your steps created, now you have to prepare a DataSet, which is an abstraction that represents a grouping of 'units' of your structure, providing several ways for the step to interact with the dataset, including neighbor lookups and parallel iteration.

The DataSet takes in the dimensions of the 'chunk' of data in 3D space, and an initialization
closure used to populate the set.

```rust
let mut dataset = DataSet::new(10, 10, 50,
    |pos| Voxel {
        pos_x: pos.0,
        pos_y: pos.1,
        pos_z: pos.2,
        biome: Biome::Ocean
    }).unwrap();
```

Now create an instance of `Pipeline`, adding each step with `add_step`, and then calling `run` with the DataSet.

The steps will run sequentially for each unit of the target struct inside the DataSet.

```rust
    let pipeline = PipelineBuilder::new()
        .add_step(BiomeDecisionStep)
        // .add_step() - add any other steps
        .build()
        .run(&mut dataset);
```

The pipeline mutates the data directly to reduce the performance hit from memory allocation, which is desired for on the fly procedural generation for games, as it reduces stuttering.

## License

This project is licensed under Apache 2.0. See LICENSE file for more info.
