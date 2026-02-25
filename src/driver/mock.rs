use crate::driver::Driver;

pub struct MockDriver {}

impl MockDriver {
    pub(crate) fn new() -> Self {
        MockDriver {}
    }
}

impl Driver for MockDriver {
    fn wait(&self) {
        panic!("runtime stalled: main task is pending but no tasks are ready");
    }

    fn submit<O, R>(
        &self,
        _op: O,
        _task: std::sync::Arc<crate::task::Task>,
    ) -> Result<R, std::io::Error>
    where
        O: crate::op::Op<Output = R> + 'static,
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "MockDriver does not support I/O operation submission",
        ))
    }
}
