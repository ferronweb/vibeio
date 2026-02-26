mod accept;
mod connect;
mod read;
mod write;

use mio::Token;

pub use accept::AcceptOp;
pub use connect::ConnectOp;
pub use read::ReadOp;
pub use write::WriteOp;

pub trait Op {
    /// I/O operation return type
    type Output;

    /// Returns the token of the I/O source this operation targets.
    fn token(&self) -> Token;

    /// Executes the I/O operation.
    fn execute(&mut self) -> Result<Self::Output, std::io::Error>;
}
