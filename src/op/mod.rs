// TODO: implement I/O operation trait

pub trait Op {
    /// I/O operation return type
    type Output;

    /// Executes the I/O operation.
    fn execute(&self) -> Result<Self::Output, std::io::Error>;
}
