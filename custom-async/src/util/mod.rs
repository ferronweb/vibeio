use crate::current_driver;

#[inline]
pub fn supports_completion() -> bool {
    current_driver().is_some_and(|driver| driver.supports_completion())
}
