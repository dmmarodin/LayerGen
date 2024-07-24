use crate::data_set::DataSet;

pub type PipelineStepResult = Result<(), String>;

pub trait Step<T> {
    fn run(&self, data: &mut DataSet<T>) -> PipelineStepResult;
}

pub struct EmptyStep;

impl<T> Step<T> for EmptyStep {
    fn run(&self, _data: &mut DataSet<T>) -> PipelineStepResult {
        Ok(())
    }
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
    fn run(&self, data: &mut DataSet<T>) -> PipelineStepResult {
        self.first.run(data)?;
        self.second.run(data)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub struct FailingStep;

    impl<T> Step<T> for FailingStep {
        fn run(&self, _: &mut DataSet<T>) -> PipelineStepResult {
            Err("Planned error".into())
        }
    }

    #[test]
    fn run_step() {
        let mut data = DataSet::new(5, 5, 5, |_| 0).unwrap();

        let step = EmptyStep {};
        let result = step.run(&mut data);

        assert!(result.is_ok());
    }

    #[test]
    fn run_failing_step() {
        let mut data = DataSet::new(5, 5, 5, |_| 0).unwrap();

        let step = FailingStep {};
        let result = step.run(&mut data);

        assert!(result.is_err());
    }
}
