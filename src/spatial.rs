use bevy_ecs::prelude::Resource;

use crate::DataSet;

pub type GlobalPosition = (i64, i64, i64);
pub type LocalPosition = (usize, usize, usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChunkCoord(pub u32, pub u32, pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkSize {
    dimensions: (u32, u32, u32),
}

impl ChunkSize {
    pub fn new(width: u32, height: u32, depth: u32) -> Result<Self, &'static str> {
        if width == 0 || height == 0 || depth == 0 {
            return Err("Chunk dimensions must be greater than zero");
        }

        // Configured chunk storage must fit the target architecture.
        let volume = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|area| area.checked_mul(u64::from(depth)))
            .ok_or("Chunk dimensions overflow u64")?;

        usize::try_from(volume).map_err(|_| "Chunk volume does not fit usize")?;

        Ok(Self {
            dimensions: (width, height, depth),
        })
    }

    pub fn dimensions(self) -> (u32, u32, u32) {
        self.dimensions
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorldBounds {
    origin: GlobalPosition,
    dimensions: (u32, u32, u32),
}

impl WorldBounds {
    pub fn new(origin: GlobalPosition, dimensions: (u32, u32, u32)) -> Result<Self, &'static str> {
        let axes = [
            (origin.0, dimensions.0),
            (origin.1, dimensions.1),
            (origin.2, dimensions.2),
        ];

        for (start, size) in axes {
            if size == 0 {
                return Err("World dimensions must be greater than zero");
            }

            start
                .checked_add(i64::from(size))
                .ok_or("World exclusive boundary overflows i64")?;
        }

        Ok(Self { origin, dimensions })
    }

    pub fn origin(self) -> GlobalPosition {
        self.origin
    }

    pub fn dimensions(self) -> (u32, u32, u32) {
        self.dimensions
    }

    pub fn contains(self, position: GlobalPosition) -> bool {
        self.offset(position).is_some()
    }

    fn offset(self, position: GlobalPosition) -> Option<(u32, u32, u32)> {
        fn axis(value: i64, origin: i64, size: u32) -> Option<u32> {
            let offset = value.checked_sub(origin)?;
            let offset = u32::try_from(offset).ok()?;

            (offset < size).then_some(offset)
        }

        Some((
            axis(position.0, self.origin.0, self.dimensions.0)?,
            axis(position.1, self.origin.1, self.dimensions.1)?,
            axis(position.2, self.origin.2, self.dimensions.2)?,
        ))
    }
}

#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PipelineConfig {
    world: WorldBounds,
    chunk_size: ChunkSize,
    seed: u64,
}

impl PipelineConfig {
    pub fn new(world: WorldBounds, chunk_size: ChunkSize, seed: u64) -> Self {
        Self {
            world,
            chunk_size,
            seed,
        }
    }

    pub fn world(self) -> WorldBounds {
        self.world
    }

    pub fn chunk_size(self) -> ChunkSize {
        self.chunk_size
    }

    pub fn seed(self) -> u64 {
        self.seed
    }

    pub fn locate(self, position: GlobalPosition) -> Option<(ChunkCoord, LocalPosition)> {
        let offset = self.world.offset(position)?;
        let size = self.chunk_size.dimensions();

        let coord = ChunkCoord(offset.0 / size.0, offset.1 / size.1, offset.2 / size.2);

        let local = (
            usize::try_from(offset.0 % size.0).ok()?,
            usize::try_from(offset.1 % size.1).ok()?,
            usize::try_from(offset.2 % size.2).ok()?,
        );

        Some((coord, local))
    }

    pub fn context(self, coord: ChunkCoord) -> Result<StepContext, &'static str> {
        fn axis(
            coord: u32,
            chunk_size: u32,
            world_origin: i64,
            world_size: u32,
        ) -> Result<(i64, usize), &'static str> {
            let offset = u64::from(coord) * u64::from(chunk_size);

            if offset >= u64::from(world_size) {
                return Err("Requested chunk is outside world bounds");
            }

            // The bounds check above makes this conversion safe.
            let offset = offset as u32;
            let extent = chunk_size.min(world_size - offset);
            let extent = usize::try_from(extent).map_err(|_| "Chunk extent does not fit usize")?;

            Ok((world_origin + i64::from(offset), extent))
        }

        let origin = self.world.origin();
        let world = self.world.dimensions();
        let size = self.chunk_size.dimensions();

        let x = axis(coord.0, size.0, origin.0, world.0)?;
        let y = axis(coord.1, size.1, origin.1, world.1)?;
        let z = axis(coord.2, size.2, origin.2, world.2)?;

        Ok(StepContext {
            config: self,
            coord,
            origin: (x.0, y.0, z.0),
            dimensions: (x.1, y.1, z.1),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StepContext {
    config: PipelineConfig,
    coord: ChunkCoord,
    origin: GlobalPosition,
    dimensions: (usize, usize, usize),
}

impl StepContext {
    pub fn config(self) -> PipelineConfig {
        self.config
    }

    pub fn chunk_coord(self) -> ChunkCoord {
        self.coord
    }

    pub fn origin(self) -> GlobalPosition {
        self.origin
    }

    pub fn dimensions(self) -> (usize, usize, usize) {
        self.dimensions
    }

    pub fn global_position(self, local: LocalPosition) -> Option<GlobalPosition> {
        if local.0 >= self.dimensions.0
            || local.1 >= self.dimensions.1
            || local.2 >= self.dimensions.2
        {
            return None;
        }

        Some((
            self.origin.0.checked_add(i64::try_from(local.0).ok()?)?,
            self.origin.1.checked_add(i64::try_from(local.1).ok()?)?,
            self.origin.2.checked_add(i64::try_from(local.2).ok()?)?,
        ))
    }
}

/// One generated chunk: dense cell data plus consumer-defined chunk data.
///
/// Chunk data defaults to `()` for pipelines that do not declare it.
#[derive(Clone, Debug)]
pub struct Chunk<T, C = ()> {
    pub(crate) context: StepContext,
    pub(crate) data: DataSet<T>,
    pub(crate) chunk_data: C,
}

impl<T, C> Chunk<T, C> {
    pub fn context(&self) -> StepContext {
        self.context
    }

    pub fn coord(&self) -> ChunkCoord {
        self.context.chunk_coord()
    }

    pub fn origin(&self) -> GlobalPosition {
        self.context.origin()
    }

    pub fn data(&self) -> &DataSet<T> {
        &self.data
    }

    pub fn data_mut(&mut self) -> &mut DataSet<T> {
        &mut self.data
    }

    pub fn into_data(self) -> DataSet<T> {
        self.data
    }

    pub fn chunk_data(&self) -> &C {
        &self.chunk_data
    }

    pub fn chunk_data_mut(&mut self) -> &mut C {
        &mut self.chunk_data
    }

    pub fn into_parts(self) -> (DataSet<T>, C) {
        (self.data, self.chunk_data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> PipelineConfig {
        PipelineConfig::new(
            WorldBounds::new((-10, 0, 20), (10, 8, 9)).unwrap(),
            ChunkSize::new(4, 4, 4).unwrap(),
            123,
        )
    }

    #[test]
    fn global_local_round_trip() {
        let config = config();

        for position in [(-10, 0, 20), (-6, 4, 24), (-1, 7, 28)] {
            let (coord, local) = config.locate(position).unwrap();
            let context = config.context(coord).unwrap();

            assert_eq!(context.global_position(local), Some(position));
        }
    }

    #[test]
    fn edge_chunk_has_clipped_dimensions() {
        let context = config().context(ChunkCoord(2, 1, 2)).unwrap();

        assert_eq!(context.origin(), (-2, 4, 28));
        assert_eq!(context.dimensions(), (2, 4, 1));
        assert_eq!(context.global_position((1, 3, 0)), Some((-1, 7, 28)));
        assert_eq!(context.global_position((2, 0, 0)), None);
    }

    #[test]
    fn rejects_outside_positions_and_chunks() {
        let config = config();

        assert_eq!(config.locate((-11, 0, 20)), None);
        assert_eq!(config.locate((0, 0, 20)), None);
        assert!(config.context(ChunkCoord(3, 0, 0)).is_err());
        assert!(config.context(ChunkCoord(u32::MAX, 0, 0)).is_err());
    }

    #[test]
    fn rejects_invalid_configuration() {
        assert!(ChunkSize::new(0, 1, 1).is_err());
        assert!(ChunkSize::new(u32::MAX, u32::MAX, u32::MAX).is_err());
        assert!(WorldBounds::new((0, 0, 0), (0, 1, 1)).is_err());
        assert!(WorldBounds::new((i64::MAX, 0, 0), (1, 1, 1)).is_err());
    }
}
