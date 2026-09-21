//! What the core needs from a display: select an input source, and — when
//! the display answers — ask which one it is showing.
//!
//! Object safe for the same reason as [`crate::device::HostSwitchable`]: the
//! only implementation is [`ddc`], which writes DDC/CI itself, but tests swap
//! in their own.

pub mod capabilities;
pub mod ddc;
pub(crate) mod ioav_ffi;

use async_trait::async_trait;

#[derive(Debug, thiserror::Error)]
pub enum DisplayError {
    /// The display layer refused the write; the payload is for the log.
    #[error("{0}")]
    Tool(String),
    #[error("display did not answer in time")]
    Timeout,
}

#[async_trait]
pub trait DisplayInput: Send + Sync {
    fn name(&self) -> &str;
    async fn set_input(&self, code: u8) -> Result<(), DisplayError>;

    /// The input source the display is showing right now, if it will say.
    ///
    /// `None` means "we do not know" — a display that cannot be read, a
    /// reply we did not understand, or a backend that never asks. It is never
    /// an error: a caller that cannot read must fall back to what it would
    /// have done without the answer.
    async fn current_input(&self) -> Option<u8> {
        None
    }
}
