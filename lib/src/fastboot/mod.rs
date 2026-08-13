//! Flashing a Car Thing that runs mainline u-boot.
//!
//! This is the second of flashthing's two transports. [`crate::AmlogicSoC`] drives amlogic's vendor burn-mode
//! protocol — bulkcmd, the MPT partition table, DRAM staging — which is what a stock device offers. This module
//! drives standard Android fastboot instead, which is what our mainline u-boot offers, and it reaches everything
//! burn mode reached by leaning on u-boot's own `flash:`, raw partition aliases and `oem console`.
//!
//! Both accept the same `meta.json`. [`FastbootFlasher`] translates each step into its fastboot equivalent rather
//! than requiring a second archive format.
//!
//! Getting *to* fastboot from a stock device still needs the amlogic mask ROM exactly once, since the boot ROM is
//! burned in silicon and speaks nothing else: [`Fastboot::init`] RAM-boots BL2 and feeds it a signed mainline FIP
//! over the AMLC handshake, and u-boot then comes up in DRAM and drops straight into the fastboot gadget. That
//! bootstrap reuses [`crate::AmlogicSoC::bl2_stream`]; nothing else in here touches a vendor protocol.

mod client;
mod flash;

pub use client::{DEFAULT_MAX_DOWNLOAD_BYTES, FASTBOOT_BUF_ADDR, Fastboot, FastbootReply, MAX_COMMAND_BYTES};
pub use flash::{FastbootFlasher, translate_vendor_command};
