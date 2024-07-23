use rayon::prelude::*;

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
    pub fn new<F>(width: usize, height: usize, depth: usize, mut initializer: F) -> Self
    where
        F: FnMut((usize, usize, usize)) -> T,
    {
        if width == 0 || height == 0 || depth == 0 {
            panic!("DataSet cannot be created with zeroed size");
        }

        let mut data = Vec::with_capacity(width * height * depth);

        for z in 0..depth {
            for y in 0..height {
                for x in 0..width {
                    data.push(initializer((x, y, z)));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iter_dataset() {
        let init_data = (0, 0, 0);
        let data = DataSet::new(10, 10, 10, |_| init_data);

        for datum in data.into_iter() {
            assert_eq!(datum, init_data);
        }
    }

    #[test]
    fn par_iter_dataset() {
        let init_data = (0, 0, 0);
        let mut data = DataSet::new(10, 10, 10, |_| init_data);

        data.par_iter_mut().for_each(|(v, _x, _y, _z)| {
            assert_eq!(*v, init_data);
        });
    }

    #[test]
    #[should_panic]
    fn create_zero_size() {
        DataSet::new(5, 0, 5, |_| 2.0);
    }
}
