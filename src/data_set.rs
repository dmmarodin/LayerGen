use std::ops::{Index, IndexMut};

#[cfg(feature = "rayon")]
use rayon::prelude::*;

#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataSet<T> {
    data: Vec<T>,
    width: usize,
    height: usize,
    depth: usize,
}

fn checked_volume(width: usize, height: usize, depth: usize) -> Result<usize, &'static str> {
    if width == 0 || height == 0 || depth == 0 {
        return Err("Dataset dimensions must be greater than zero");
    }

    width
        .checked_mul(height)
        .and_then(|area| area.checked_mul(depth))
        .ok_or("Dataset dimensions overflow usize")
}

impl<T> DataSet<T> {
    pub fn new<F>(
        width: usize,
        height: usize,
        depth: usize,
        mut initializer: F,
    ) -> Result<Self, &'static str>
    where
        F: FnMut((usize, usize, usize)) -> T,
    {
        let volume = checked_volume(width, height, depth)?;

        let mut data = Vec::new();
        data.try_reserve_exact(volume)
            .map_err(|_| "Unable to reserve dataset storage")?;

        for z in 0..depth {
            for y in 0..height {
                for x in 0..width {
                    data.push(initializer((x, y, z)));
                }
            }
        }

        Ok(Self {
            data,
            width,
            height,
            depth,
        })
    }

    pub fn dimensions(&self) -> (usize, usize, usize) {
        (self.width, self.height, self.depth)
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn as_slice(&self) -> &[T] {
        &self.data
    }

    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.data
    }

    pub fn linear_index(&self, x: usize, y: usize, z: usize) -> Option<usize> {
        if x >= self.width || y >= self.height || z >= self.depth {
            return None;
        }

        // Construction validates that the total volume fits in usize.
        Some(x + y * self.width + z * self.width * self.height)
    }

    pub fn position(&self, index: usize) -> Option<(usize, usize, usize)> {
        if index >= self.data.len() {
            return None;
        }

        Some((
            index % self.width,
            (index / self.width) % self.height,
            index / (self.width * self.height),
        ))
    }

    pub fn get(&self, x: usize, y: usize, z: usize) -> Option<&T> {
        self.linear_index(x, y, z).map(|index| &self.data[index])
    }

    pub fn get_mut(&mut self, x: usize, y: usize, z: usize) -> Option<&mut T> {
        let index = self.linear_index(x, y, z)?;
        Some(&mut self.data[index])
    }

    /// Six face-adjacent neighbors in this order:
    /// [+x, -x, +y, -y, +z, -z].
    ///
    /// This is dataset-local access, not cross-chunk access.
    /// An invalid center returns six absent neighbors.
    pub fn neighbors(&self, x: usize, y: usize, z: usize) -> [Option<&T>; 6] {
        if self.linear_index(x, y, z).is_none() {
            return [None; 6];
        }

        [
            x.checked_add(1).and_then(|nx| self.get(nx, y, z)),
            x.checked_sub(1).and_then(|nx| self.get(nx, y, z)),
            y.checked_add(1).and_then(|ny| self.get(x, ny, z)),
            y.checked_sub(1).and_then(|ny| self.get(x, ny, z)),
            z.checked_add(1).and_then(|nz| self.get(x, y, nz)),
            z.checked_sub(1).and_then(|nz| self.get(x, y, nz)),
        ]
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&T, usize, usize, usize)> + '_ {
        let width = self.width;
        let height = self.height;

        self.data.iter().enumerate().map(move |(index, item)| {
            (
                item,
                index % width,
                (index / width) % height,
                index / (width * height),
            )
        })
    }

    pub fn iter_mut(
        &mut self,
    ) -> impl ExactSizeIterator<Item = (&mut T, usize, usize, usize)> + '_ {
        let width = self.width;
        let height = self.height;

        self.data.iter_mut().enumerate().map(move |(index, item)| {
            (
                item,
                index % width,
                (index / width) % height,
                index / (width * height),
            )
        })
    }
}

#[cfg(feature = "rayon")]
impl<T: Sync> DataSet<T> {
    pub fn par_iter(&self) -> impl ParallelIterator<Item = (&T, usize, usize, usize)> + '_ {
        let width = self.width;
        let height = self.height;

        self.data.par_iter().enumerate().map(move |(index, item)| {
            (
                item,
                index % width,
                (index / width) % height,
                index / (width * height),
            )
        })
    }
}

#[cfg(feature = "rayon")]
impl<T: Send> DataSet<T> {
    pub fn par_iter_mut(
        &mut self,
    ) -> impl ParallelIterator<Item = (&mut T, usize, usize, usize)> + '_ {
        let width = self.width;
        let height = self.height;

        self.data
            .par_iter_mut()
            .enumerate()
            .map(move |(index, item)| {
                (
                    item,
                    index % width,
                    (index / width) % height,
                    index / (width * height),
                )
            })
    }
}

impl<T> IntoIterator for DataSet<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.data.into_iter()
    }
}

impl<T> Index<(usize, usize, usize)> for DataSet<T> {
    type Output = T;

    fn index(&self, position: (usize, usize, usize)) -> &Self::Output {
        self.get(position.0, position.1, position.2)
            .expect("Dataset index out of bounds")
    }
}

impl<T> IndexMut<(usize, usize, usize)> for DataSet<T> {
    fn index_mut(&mut self, position: (usize, usize, usize)) -> &mut Self::Output {
        self.get_mut(position.0, position.1, position.2)
            .expect("Dataset index out of bounds")
    }
}

#[cfg(feature = "serde")]
impl<'de, T> serde::Deserialize<'de> for DataSet<T>
where
    T: serde::Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct StoredDataSet<T> {
            data: Vec<T>,
            width: usize,
            height: usize,
            depth: usize,
        }

        let stored = <StoredDataSet<T> as serde::Deserialize>::deserialize(deserializer)?;

        let expected = checked_volume(stored.width, stored.height, stored.depth)
            .map_err(serde::de::Error::custom)?;

        if stored.data.len() != expected {
            return Err(serde::de::Error::custom(
                "Dataset storage length does not match its dimensions",
            ));
        }

        Ok(Self {
            data: stored.data,
            width: stored.width,
            height: stored.height,
            depth: stored.depth,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinate_index_round_trip() {
        let data = DataSet::new(3, 4, 5, |position| position).unwrap();

        for (value, x, y, z) in data.iter() {
            let index = data.linear_index(x, y, z).unwrap();
            assert_eq!(data.position(index), Some((x, y, z)));
            assert_eq!(*value, (x, y, z));
        }

        assert_eq!(data.position(data.len()), None);
    }

    #[test]
    fn rejects_zero_and_overflowing_dimensions() {
        assert!(DataSet::new(1, 0, 1, |_| ()).is_err());
        assert!(DataSet::new(usize::MAX, 2, 1, |_| ()).is_err());
    }

    #[test]
    fn sequential_iteration_mutates_dataset() {
        let mut data = DataSet::new(3, 4, 5, |_| 0).unwrap();

        for (value, x, y, z) in data.iter_mut() {
            *value = x + y + z;
        }

        assert_eq!(data[(2, 3, 4)], 9);
    }

    #[test]
    fn indexing_and_bounds() {
        let mut data = DataSet::new(2, 2, 2, |_| 0).unwrap();

        data[(1, 1, 1)] = 42;

        assert_eq!(data.get(1, 1, 1), Some(&42));
        assert_eq!(data.get(2, 0, 0), None);
        assert_eq!(data.get_mut(0, 2, 0), None);
    }

    #[test]
    fn neighbors_have_correct_values() {
        let data = DataSet::new(5, 5, 5, |position| position).unwrap();
        let neighbors = data.neighbors(2, 2, 2);

        assert_eq!(
            neighbors.map(|value| value.copied()),
            [
                Some((3, 2, 2)),
                Some((1, 2, 2)),
                Some((2, 3, 2)),
                Some((2, 1, 2)),
                Some((2, 2, 3)),
                Some((2, 2, 1)),
            ]
        );
    }

    #[test]
    fn boundary_neighbor_counts() {
        let data = DataSet::new(5, 5, 5, |_| 0).unwrap();
        let count = |x, y, z| {
            data.neighbors(x, y, z)
                .iter()
                .filter(|neighbor| neighbor.is_some())
                .count()
        };

        assert_eq!(count(0, 0, 0), 3); // Corner.
        assert_eq!(count(0, 0, 2), 4); // Edge.
        assert_eq!(count(0, 2, 2), 5); // Face.
        assert_eq!(count(2, 2, 2), 6); // Interior.
    }

    #[test]
    fn invalid_neighbor_center_is_safe() {
        let data = DataSet::new(2, 2, 2, |_| 0).unwrap();

        for position in [(2, 0, 0), (usize::MAX, 0, 0)] {
            assert!(data
                .neighbors(position.0, position.1, position.2)
                .iter()
                .all(Option::is_none));
        }
    }

    #[test]
    fn clone_is_independent() {
        let data = DataSet::new(2, 2, 2, |_| 0).unwrap();
        let mut cloned = data.clone();

        cloned[(0, 0, 0)] = 99;

        assert_eq!(data[(0, 0, 0)], 0);
    }

    #[test]
    fn storage_does_not_require_send_or_sync() {
        use std::rc::Rc;

        let data = DataSet::new(1, 1, 1, |_| Rc::new(7)).unwrap();

        assert_eq!(**data.get(0, 0, 0).unwrap(), 7);
    }

    #[cfg(feature = "rayon")]
    #[test]
    fn parallel_iteration_mutates_dataset() {
        let mut data = DataSet::new(3, 4, 5, |_| 0).unwrap();

        data.par_iter_mut().for_each(|(value, x, y, z)| {
            *value = x + y + z;
        });

        data.par_iter().for_each(|(value, x, y, z)| {
            assert_eq!(*value, x + y + z);
        });
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_round_trip() {
        let data = DataSet::new(3, 4, 5, |position| position).unwrap();
        let json = serde_json::to_string(&data).unwrap();
        let restored: DataSet<(usize, usize, usize)> = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.dimensions(), data.dimensions());
        assert_eq!(restored.as_slice(), data.as_slice());
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_rejects_invalid_storage() {
        for json in [
            r#"{"data":[],"width":1,"height":1,"depth":1}"#,
            r#"{"data":[],"width":0,"height":1,"depth":1}"#,
        ] {
            assert!(serde_json::from_str::<DataSet<u8>>(json).is_err());
        }
    }
}
