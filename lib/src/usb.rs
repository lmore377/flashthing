use std::{future::Future, time::Duration};

use crate::{DeviceMode, Result};

/// Timeout for every control transfer and for the descriptor reads done while probing a device.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// Which USB personality the transport should look for when claiming a device.
///
/// The Car Thing shows up as three different devices depending on what is running on it, and the two we can drive
/// speak completely different protocols over completely different interfaces:
///
/// - [`UsbTarget::Maskrom`] is the amlogic boot ROM / vendor burn-mode identity (`1b8e:c003`), driven by [`crate::AmlogicSoC`].
/// - [`UsbTarget::Fastboot`] is our mainline u-boot's fastboot gadget (`18d1:fada`), driven by [`crate::Fastboot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbTarget {
  /// The amlogic mask ROM or vendor burn-mode bootloader.
  Maskrom,
  /// The fastboot gadget exposed by mainline u-boot.
  Fastboot,
}

impl UsbTarget {
  /// The vendor and product ID this target enumerates as.
  pub fn ids(self) -> (u16, u16) {
    match self {
      Self::Maskrom => (crate::VENDOR_ID, crate::PRODUCT_ID),
      Self::Fastboot => (crate::VENDOR_ID_FASTBOOT, crate::PRODUCT_ID_FASTBOOT),
    }
  }
}

/// The USB surface the device protocols need, independent of how the host reaches the device.
pub trait UsbTransport {
  fn control_out(
    &self,
    request: u8,
    value: u16,
    index: u16,
    data: &[u8],
    timeout: Duration,
  ) -> impl Future<Output = Result<usize>>;

  fn control_in(
    &self,
    request: u8,
    value: u16,
    index: u16,
    buf: &mut [u8],
    timeout: Duration,
  ) -> impl Future<Output = Result<usize>>;

  fn bulk_out(&self, data: &[u8], timeout: Duration) -> impl Future<Output = Result<usize>>;

  fn bulk_in(&self, buf: &mut [u8], timeout: Duration) -> impl Future<Output = Result<usize>>;

  /// Which mode the device is currently in, without claiming it.
  fn mode(&self) -> impl Future<Output = DeviceMode>;

  /// Drop any existing handle and claim the device again under `target`'s identity.
  ///
  /// This is also how the transport is told the device changed shape underneath it: a mask-ROM device that has just
  /// been handed our FIP re-enumerates as a fastboot gadget with a different interface layout, so the caller
  /// re-acquires with the new target rather than reusing the old endpoints.
  fn acquire(&self, target: UsbTarget) -> impl Future<Output = Result<()>>;
}
