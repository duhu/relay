//! Relay core: UI-independent switching logic.
//!
//! This crate holds the configuration model, the switch plan builder, the
//! switch state machine, the device and display layers, the HID trigger source
//! and the runtime that wires them together. Everything but the IOKit FFI in
//! [`trigger::presence`] and [`permissions`] is testable without hardware.

pub mod config;
pub mod config_watch;
pub mod coordinator;
pub mod device;
pub mod display;
pub mod executor;
pub mod log;
pub mod permissions;
pub mod plan;
pub mod runtime;
pub mod trigger;
pub mod types;
