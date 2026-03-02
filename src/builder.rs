use crate::driver::AnyDriver;

/// I/O driver selection for the async runtime.
///
/// This enum allows choosing which I/O driver to use when building the runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DriverKind {
    /// Uses the Mio driver for I/O operations.
    Mio,
    /// Uses the mock driver for testing purposes.
    Mock,
    /// Uses the io_uring driver (Linux only).
    #[cfg(target_os = "linux")]
    IoUring,
}

impl DriverKind {
    /// Creates a new runtime I/O driver from this kind.
    #[inline]
    pub(crate) fn into_driver(self) -> Result<AnyDriver, std::io::Error> {
        match self {
            DriverKind::Mio => AnyDriver::new_mio(),
            DriverKind::Mock => Ok(AnyDriver::new_mock()),
            #[cfg(target_os = "linux")]
            DriverKind::IoUring => AnyDriver::new_uring(1024),
        }
    }
}

/// Builder for configuring and creating an async runtime.
///
/// Provides a convenient way to configure the runtime's I/O driver
/// before building it.
///
/// # Examples
///
/// ```
/// use custom_async::RuntimeBuilder;
///
/// let runtime = RuntimeBuilder::new()
///     .build();
/// ```
pub struct RuntimeBuilder {
    driver_kind: Option<DriverKind>,
}

impl RuntimeBuilder {
    /// Creates a new runtime builder with default configuration.
    ///
    /// By default, the builder will select the best available driver for the platform.
    pub fn new() -> Self {
        Self { driver_kind: None }
    }

    /// Creates a new runtime builder with the specified driver kind.
    pub fn with_driver(driver_kind: DriverKind) -> Self {
        Self {
            driver_kind: Some(driver_kind),
        }
    }

    /// Sets the I/O driver for the runtime.
    pub fn driver(mut self, driver_kind: DriverKind) -> Self {
        self.driver_kind = Some(driver_kind);
        self
    }

    /// Builds the async runtime with the configured settings.
    ///
    /// If no driver was explicitly set, selects the best available driver for the platform.
    pub fn build(self) -> Result<crate::executor::Runtime, std::io::Error> {
        let driver = if let Some(driver_kind) = self.driver_kind {
            driver_kind.into_driver()?
        } else {
            AnyDriver::new_best()?
        };
        Ok(crate::executor::new_runtime(driver))
    }
}

impl Default for RuntimeBuilder {
    fn default() -> Self {
        Self::new()
    }
}
