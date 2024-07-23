use rayon::prelude::*;
use std::ops::{Index, IndexMut};

pub struct DataSet<T> {
    data: Vec<T>,
    width: usize,
    height: usize,
    depth: usize,
}

impl<T> DataSet<T>
where
    T: Send + Sync,
{
    pub fn new<F>(
        width: usize,
        height: usize,
        depth: usize,
        mut initializer: F,
    ) -> Result<Self, &'static str>
    where
        F: FnMut((usize, usize, usize)) -> T,
    {
        if width == 0 || height == 0 || depth == 0 {
            return Err("Failed to create DataSet: dimensions must be greater than zero");
        }

        let mut data = Vec::with_capacity(width * height * depth);

        for z in 0..depth {
            for y in 0..height {
                for x in 0..width {
                    data.push(initializer((x, y, z)));
                }
            }
        }

        Ok(DataSet {
            data,
            width,
            height,
            depth,
        })
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

    pub fn par_iter(&self) -> impl ParallelIterator<Item = (&T, usize, usize, usize)> + '_ {
        let width = self.width;
        let height = self.height;

        self.data.par_iter().enumerate().map(move |(i, item)| {
            let x = i % width;
            let y = (i / width) % height;
            let z = i / (width * height);
            (item, x, y, z)
        })
    }

    pub fn par_iter_mut(
        &mut self,
    ) -> impl ParallelIterator<Item = (&mut T, usize, usize, usize)> + '_ {
        let width = self.width;
        let height = self.height;

        self.data.par_iter_mut().enumerate().map(move |(i, item)| {
            let x = i % width;
            let y = (i / width) % height;
            let z = i / (width * height);
            (item, x, y, z)
        })
    }
}

impl<T> IntoIterator for DataSet<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.data.into_iter()
    }
}

impl<T> Index<(usize, usize, usize)> for DataSet<T>
where
    T: Send + Sync,
{
    type Output = T;

    fn index(&self, index: (usize, usize, usize)) -> &Self::Output {
        let (x, y, z) = index;
        self.get(x, y, z).expect("Index out of bounds")
    }
}

impl<T> IndexMut<(usize, usize, usize)> for DataSet<T>
where
    T: Send + Sync,
{
    fn index_mut(&mut self, index: (usize, usize, usize)) -> &mut Self::Output {
        let (x, y, z) = index;
        self.get_mut(x, y, z).expect("Index out of bounds")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iter_dataset() {
        let init_data = (0, 0, 0);
        let data = DataSet::new(10, 10, 10, |_| init_data).unwrap();

        for datum in data.into_iter() {
            assert_eq!(datum, init_data);
        }
    }

    #[test]
    fn iter_mut_dataset() {
        let init_data = (0, 0, 0);
        let data = DataSet::new(10, 10, 10, |_| init_data).unwrap();
        let mut items: Vec<_> = data.into_iter().collect();

        for item in &mut items {
            item.0 = 10;
        }

        for item in &items {
            assert_eq!(item.0, 10);
        }
    }

    #[test]
    fn test_par_iter() {
        let init_data = (0, 0, 0);
        let data = DataSet::new(10, 10, 10, |_| init_data).unwrap();

        data.par_iter().for_each(|(v, _x, _y, _z)| {
            assert_eq!(*v, init_data);
        });
    }

    #[test]
    fn par_iter_mut() {
        let init_data = (0, 0, 0);
        let mut data = DataSet::new(10, 10, 10, |_| init_data).unwrap();

        data.par_iter_mut().for_each(|(v, _x, _y, _z)| {
            assert_eq!(*v, init_data);
        });
    }

    #[test]
    #[should_panic]
    fn create_zero_size() {
        DataSet::new(5, 0, 5, |_| 2.0).unwrap();
    }

    #[test]
    fn get_item_out_of_bounds() {
        let data = DataSet::new(10, 10, 10, |_| 1).unwrap();
        assert_eq!(data.get(100, 0, 0), None);
    }

    #[test]
    fn test_indexing() {
        let mut data = DataSet::new(10, 10, 10, |_| (0, 0, 0)).unwrap();
        data[(0, 0, 0)] = (1, 2, 3);
        assert_eq!(data[(0, 0, 0)], (1, 2, 3));
    }
}
