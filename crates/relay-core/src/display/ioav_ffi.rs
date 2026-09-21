//! The private `IOKit` symbols that carry DDC on Apple Silicon.
//!
//! Apple ships no public header for `IOAVService`, but `IOKit.framework`
//! exports them all, which is how `m1ddc` — and every other Apple Silicon
//! DDC tool — reaches an external display. Keeping them in one file means a
//! macOS release that renames or drops them breaks exactly here.
//!
//! Both directions are declared, and no more: reading the current input
//! source (VCP 0x60) is what [`read_i2c`] carries, while Capabilities (0xF3)
//! and the other VCP features stay M4 work — an unused `extern` declaration
//! is a promise we have not tested.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_core_foundation::{CFAllocator, CFRetained, CFType};
use objc2_io_kit::io_service_t;

/// IOKit's `kern_return_t`-shaped status, and the only value that means the
/// write landed. Both come from `objc2_io_kit` so the ABI is declared once.
pub use objc2_io_kit::{kIOReturnSuccess as IO_RETURN_SUCCESS, IOReturn};

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    /// Opens the `IOAVService` behind a `DCPAVServiceProxy` registry entry.
    ///
    /// A null `allocator` is `kCFAllocatorDefault`. The result is a +1
    /// `CFTypeRef`, or null when the entry carries no AV service.
    fn IOAVServiceCreateWithService(
        allocator: Option<&CFAllocator>,
        service: io_service_t,
    ) -> Option<NonNull<CFType>>;

    /// Writes `buffer` to the I2C bus behind an `IOAVService`.
    fn IOAVServiceWriteI2C(
        service: &CFType,
        chip_address: u32,
        data_address: u32,
        buffer: *const u8,
        buffer_size: u32,
    ) -> IOReturn;

    /// Reads `output_buffer_size` bytes from the I2C bus behind an
    /// `IOAVService`, declared exactly as `m1ddc`'s `headers/i2c.h` does.
    fn IOAVServiceReadI2C(
        service: &CFType,
        chip_address: u32,
        offset: u32,
        output_buffer: *mut c_void,
        output_buffer_size: u32,
    ) -> IOReturn;
}

/// Opens the AV service of a `DCPAVServiceProxy`, released on drop.
///
/// # Safety
///
/// `service` must be a live `io_service_t` for a `DCPAVServiceProxy` entry.
pub unsafe fn create_with_service(service: io_service_t) -> Option<CFRetained<CFType>> {
    // SAFETY: the caller guarantees the registry entry. The call returns a
    // +1 reference, which `CFRetained::from_raw` takes ownership of.
    let raw = unsafe { IOAVServiceCreateWithService(None, service) }?;
    Some(unsafe { CFRetained::from_raw(raw) })
}

/// Writes one DDC/CI packet; returns the raw `IOReturn`.
///
/// # Safety
///
/// `service` must be an `IOAVService` — that is, the value
/// [`create_with_service`] returned. `CFType` is only the shape of the
/// pointer, so any other CF object would be handed to a private entry point
/// that does not check it.
pub unsafe fn write_i2c(
    service: &CFType,
    chip_address: u32,
    data_address: u32,
    data: &[u8],
) -> IOReturn {
    // SAFETY: the caller guarantees `service` is an AV service, and `data` is
    // a readable slice of exactly `data.len()` bytes, which is what
    // `buffer_size` promises.
    unsafe {
        IOAVServiceWriteI2C(
            service,
            chip_address,
            data_address,
            data.as_ptr(),
            data.len() as u32,
        )
    }
}

/// Reads one DDC/CI reply into `buf`; returns the raw `IOReturn`.
///
/// `offset` is the I2C data address the reply is fetched from — the same
/// address the request was written to.
///
/// # Safety
///
/// `service` must be an `IOAVService`, for the same reason as [`write_i2c`].
pub unsafe fn read_i2c(
    service: &CFType,
    chip_address: u32,
    offset: u32,
    buf: &mut [u8],
) -> IOReturn {
    // SAFETY: the caller guarantees `service` is an AV service, and `buf` is a
    // writable slice of exactly `buf.len()` bytes, which is what
    // `output_buffer_size` promises. IOKit writes bytes, so `c_void` and `u8`
    // point at the same thing.
    unsafe {
        IOAVServiceReadI2C(
            service,
            chip_address,
            offset,
            buf.as_mut_ptr().cast::<c_void>(),
            buf.len() as u32,
        )
    }
}
