mod mock;
// TODO: implement I/O driver for async runtime

use std::sync::Arc;

use crate::{driver::mock::MockDriver, op::Op, task::Task};

pub trait Driver {
    /// Waits for the I/O.
    fn wait(&self);

    /// Submits an I/O operation.
    fn submit<O, R>(&self, op: O, task: Arc<Task>) -> Result<R, std::io::Error>
    where
        O: Op<Output = R> + 'static;
}

pub enum AnyDriver {
    Mock(MockDriver),
}

impl AnyDriver {
    pub(crate) fn new_mock() -> Self {
        AnyDriver::Mock(MockDriver::new())
    }

    pub(crate) fn wait(&self) {
        match self {
            AnyDriver::Mock(driver) => driver.wait(),
        }
    }

    pub(crate) fn submit<O, R>(&self, op: O, task: Arc<Task>) -> Result<R, std::io::Error>
    where
        O: Op<Output = R> + 'static,
    {
        match self {
            AnyDriver::Mock(driver) => driver.submit(op, task),
        }
    }
}
