//! What the core needs from an input device: read its host slots, move it.
//!
//! The trait is object safe so the executor can hold `Arc<dyn HostSwitchable>`
//! and the runtime can hand it the native HID++ implementation
//! ([`logitech::LogitechHidpp`]) without touching anything above it.

pub mod discovery;
pub mod logitech;
#[cfg(test)]
mod replay_support;

use async_trait::async_trait;

use crate::types::{DeviceId, HostIndex, HostInfo};

#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    /// The device is not on this machine right now (another host owns it).
    #[error("device not present")]
    NotFound,
    /// The device layer refused the operation; the payload is for the log.
    #[error("{0}")]
    Tool(String),
    #[error("device did not answer in time")]
    Timeout,
}

#[async_trait]
pub trait HostSwitchable: Send + Sync {
    fn id(&self) -> &DeviceId;
    fn name(&self) -> &str;
    async fn host_info(&self) -> Result<HostInfo, DeviceError>;
    async fn switch_to_host(&self, index: HostIndex) -> Result<(), DeviceError>;
}
