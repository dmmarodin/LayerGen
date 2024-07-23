use crate::data_set::DataSet;

pub trait Step<T> {
    fn run(&self, data: &mut DataSet<T>);
}

pub struct EmptyStep;

impl<T> Step<T> for EmptyStep {
    fn run(&self, _data: &mut DataSet<T>) {}
}

pub struct Chain<A, B> {
    pub first: A,
    pub second: B,
}

impl<T, A, B> Step<T> for Chain<A, B>
where
    A: Step<T>,
    B: Step<T>,
{
    fn run(&self, data: &mut DataSet<T>) {
        self.first.run(data);
        self.second.run(data);
    }
}
