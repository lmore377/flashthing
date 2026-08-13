//! Android fastboot client.
//!
//! Wire protocol: a command is a single ASCII string of at most 64 bytes on the bulk OUT endpoint. The device answers
//! with one or more 64-byte packets on the bulk IN endpoint, each carrying a four-character tag:
//!
//! ```text
//! INFO<text>     informational, keep reading
//! TEXT<text>     continuation of the previous INFO (rare)
//! OKAY<payload>  terminal success
//! FAIL<reason>   terminal failure
//! DATA<8 hex>    device is ready to receive that many bytes
//! ```
//!
//! Everything flashthing does to a device running mainline u-boot goes through here. See [`super::flash`] for how
//! flash configs are translated into these calls.

use std::{sync::Arc, time::Duration};

use crate::{
  BL2_BIN, Callback, DeviceMode, Error, Event, FIP_BIN, Result,
  aml::{AmlogicSoC, RESET_SETTLE},
  time::sleep,
  usb::{COMMAND_TIMEOUT, UsbTarget, UsbTransport},
};

/// u-boot's `FASTBOOT_COMMAND_LEN`. Anything longer is silently truncated on the device, so we refuse it here.
pub const MAX_COMMAND_BYTES: usize = 64;

/// Every reply packet is one 64-byte bulk IN transfer.
const PACKET_BYTES: usize = 64;

/// Bulk OUT slice size during a download.
const TRANSFER_CHUNK_BYTES: usize = 1024 * 1024;

/// Fallback when the device doesn't report `max-download-size`: u-boot's default `CONFIG_FASTBOOT_BUF_SIZE`.
pub const DEFAULT_MAX_DOWNLOAD_BYTES: usize = 0x7000000; // 112 MiB

/// `CONFIG_FASTBOOT_BUF_ADDR` on our u-boot — where a download lands in DRAM.
pub const FASTBOOT_BUF_ADDR: u32 = 0x6000000;

/// Timeout for the bulk OUT slices of a download.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout for the reply to a command that blocks on the eMMC.
///
/// `flash:` returns nothing until the write has committed, and `erase:` on a large range can take minutes, so these
/// cannot share the 10-second command timeout.
const COMMIT_TIMEOUT: Duration = Duration::from_secs(300);

/// A parsed fastboot reply.
#[derive(Debug, Clone)]
pub struct FastbootReply {
  /// Whether the terminal packet was OKAY rather than FAIL.
  pub ok: bool,
  /// Every INFO line, in order. u-boot sends one console line per packet.
  pub info: Vec<String>,
  /// Payload of the terminal OKAY/FAIL packet.
  pub response: String,
}

impl FastbootReply {
  /// The INFO lines joined back into the console output they came from.
  pub fn text(&self) -> String {
    self.info.join("\n")
  }
}

fn fastboot_error(command: &str, reason: impl Into<String>) -> Error {
  Error::Fastboot {
    command: command.to_owned(),
    reason: reason.into(),
  }
}

struct FastbootInner<U: UsbTransport> {
  soc: AmlogicSoC<U>,
  callback: Option<Callback>,
  max_download: std::sync::Mutex<Option<usize>>,
}

/// The fastboot half of flashthing: a device running our mainline u-boot.
///
/// Unlike [`AmlogicSoC`] this speaks no vendor protocol at all — partitions, raw LBA ranges and the u-boot
/// environment are all reached through standard fastboot commands plus u-boot's `oem console` escape hatch.
///
/// The type keeps an [`AmlogicSoC`] inside it because getting *to* fastboot from a stock device still requires the
/// amlogic mask ROM exactly once: BL2 into SRAM, our signed FIP over the AMLC handshake, and u-boot then comes up in
/// DRAM and drops straight into the fastboot gadget.
pub struct Fastboot<U: UsbTransport> {
  inner: Arc<FastbootInner<U>>,
}

impl<U: UsbTransport> Clone for Fastboot<U> {
  fn clone(&self) -> Self {
    Self {
      inner: self.inner.clone(),
    }
  }
}

impl<U: UsbTransport> Fastboot<U> {
  /// Bring a device up in fastboot and claim it, using the bundled BL2 and FIP.
  ///
  /// See [`Fastboot::init_with`] for the details and for supplying your own bootloader.
  pub async fn init(transport: U, callback: Option<Callback>) -> Result<Self> {
    Self::init_with(transport, BL2_BIN, FIP_BIN, callback).await
  }

  /// Bring a device up in fastboot and claim it.
  ///
  /// What happens depends on how the device enumerated:
  ///
  /// - already in fastboot: claimed as-is, no bootstrap.
  /// - mask ROM (buttons 1 & 4 held at power-on): `bl2` is RAM-booted and fed `fip`, and the device is reclaimed
  ///   under its fastboot identity once it re-enumerates.
  /// - vendor burn mode: rejected. Burn mode is already past the point where the mask ROM will accept a different
  ///   bootloader, so the device has to be power-cycled back into USB mode first.
  ///
  /// # Parameters
  /// - `transport`: the USB backend to drive the device through
  /// - `bl2`: mask-ROM-signed BL2, used only when the device still needs bootstrapping
  /// - `fip`: signed mainline u-boot FIP streamed to the SoC during the BL2 sequence
  /// - `callback`: Optional callback function to receive status updates
  pub async fn init_with(transport: U, bl2: &[u8], fip: &[u8], callback: Option<Callback>) -> Result<Self> {
    if let Some(callback) = &callback {
      callback(Event::FindingDevice);
    };

    let mode = transport.mode().await;
    if let Some(callback) = &callback {
      callback(Event::DeviceMode(mode));
    };

    match mode {
      DeviceMode::Fastboot => tracing::info!("device found in fastboot!"),
      DeviceMode::Usb => tracing::info!("device booted in usb mode - ram-booting mainline u-boot to reach fastboot"),
      DeviceMode::UsbBurn => {
        tracing::error!(
          "device is in vendor burn mode, which cannot be moved to fastboot. power cycle the car thing while holding \
           buttons 1 & 4 to get back to the mask rom"
        );
        return Err(Error::WrongMode);
      }
      DeviceMode::Normal => {
        tracing::error!(
          "device is booted in normal mode. make sure to power on the car thing while holding buttons 1 & 4"
        );
        return Err(Error::WrongMode);
      }
      DeviceMode::NotFound => {
        tracing::error!("device not found!! make sure to power on the car thing while holding buttons 1 & 4");
        return Err(Error::NotFound);
      }
    };

    let target = if mode == DeviceMode::Usb {
      UsbTarget::Maskrom
    } else {
      UsbTarget::Fastboot
    };
    transport.acquire(target).await?;

    let soc = AmlogicSoC::adopt(transport, callback.clone());

    if mode == DeviceMode::Usb {
      soc.bl2_stream(bl2, fip).await?;

      if let Some(callback) = &callback {
        callback(Event::Resetting);
      };
      tracing::debug!("u-boot is starting, waiting for the fastboot gadget to enumerate");
      sleep(RESET_SETTLE).await;
      soc.transport().acquire(UsbTarget::Fastboot).await?;
    }

    let device = Self {
      inner: Arc::new(FastbootInner {
        soc,
        callback,
        max_download: std::sync::Mutex::new(None),
      }),
    };

    // proves the gadget is actually answering rather than merely enumerated, and gives the log something useful.
    match device.getvar("version-bootloader").await {
      Ok(version) => tracing::info!("connected to fastboot, bootloader {}", version.trim()),
      Err(err) => tracing::warn!("connected to fastboot but version-bootloader failed: {}", err),
    }

    Ok(device)
  }

  fn transport(&self) -> &U {
    self.inner.soc.transport()
  }

  fn emit(&self, event: Event) {
    if let Some(callback) = &self.inner.callback {
      callback(event);
    };
  }

  // ---- protocol primitives -------------------------------------------------

  /// Send one command and read packets until OKAY or FAIL. Never errors on FAIL — inspect [`FastbootReply::ok`].
  pub async fn send(&self, command: &str) -> Result<FastbootReply> {
    self.send_with_timeout(command, COMMAND_TIMEOUT).await
  }

  async fn send_with_timeout(&self, command: &str, timeout: Duration) -> Result<FastbootReply> {
    let bytes = command.as_bytes();
    if bytes.len() > MAX_COMMAND_BYTES {
      // u-boot truncates rather than complaining, which turns an over-long `setenv` into a silently wrong one.
      return Err(fastboot_error(
        command,
        format!(
          "command is {} bytes, over the {}-byte fastboot limit",
          bytes.len(),
          MAX_COMMAND_BYTES
        ),
      ));
    }

    tracing::debug!("fastboot >> {}", command);
    self.transport().bulk_out(bytes, COMMAND_TIMEOUT).await?;
    self.read_reply(command, timeout).await
  }

  /// Send one command and error unless it comes back OKAY.
  pub async fn expect(&self, command: &str) -> Result<FastbootReply> {
    let reply = self.send(command).await?;
    if !reply.ok {
      return Err(fastboot_error(command, reply.response));
    }
    Ok(reply)
  }

  async fn read_reply(&self, command: &str, timeout: Duration) -> Result<FastbootReply> {
    let mut info: Vec<String> = Vec::new();
    let mut buf = [0u8; PACKET_BYTES];

    loop {
      let read = self.transport().bulk_in(&mut buf, timeout).await?;
      if read < 4 {
        return Err(fastboot_error(command, "device sent a truncated reply"));
      }

      let text = String::from_utf8_lossy(&buf[..read]);
      let (tag, body) = text.split_at(4);
      tracing::trace!("fastboot << {}{}", tag, body);

      match tag {
        "INFO" => info.push(body.to_owned()),
        // a continuation of the previous line rather than a new one
        "TEXT" => match info.last_mut() {
          Some(last) => last.push_str(body),
          None => info.push(body.to_owned()),
        },
        "OKAY" => {
          return Ok(FastbootReply {
            ok: true,
            info,
            response: body.to_owned(),
          });
        }
        "FAIL" => {
          return Ok(FastbootReply {
            ok: false,
            info,
            response: body.to_owned(),
          });
        }
        other => return Err(fastboot_error(command, format!("unexpected reply tag {:?}", other))),
      }
    }
  }

  // ---- commands ------------------------------------------------------------

  /// Read a device variable.
  pub async fn getvar(&self, name: &str) -> Result<String> {
    let command = format!("getvar:{}", name);
    Ok(self.expect(&command).await?.response)
  }

  /// Largest single download the device will accept, cached after the first ask.
  pub async fn max_download_size(&self) -> usize {
    if let Some(cached) = *self.inner.max_download.lock().expect("max download mutex poisoned") {
      return cached;
    }

    let size = match self.getvar("max-download-size").await {
      Ok(raw) => parse_size(raw.trim()).unwrap_or(DEFAULT_MAX_DOWNLOAD_BYTES),
      Err(err) => {
        tracing::debug!("device did not report max-download-size ({}), assuming default", err);
        DEFAULT_MAX_DOWNLOAD_BYTES
      }
    };

    *self.inner.max_download.lock().expect("max download mutex poisoned") = Some(size);
    size
  }

  /// Upload a buffer into the device's scratch buffer at [`FASTBOOT_BUF_ADDR`].
  ///
  /// The data stays there until the next download, so a following `flash:` — or an `oem console` reading that
  /// address — sees it.
  ///
  /// `on_progress` is called with the number of bytes sent so far after every slice.
  pub async fn download<F: Fn(usize, usize)>(&self, data: &[u8], on_progress: F) -> Result<()> {
    let size = data.len();
    if size == 0 {
      return Err(Error::InvalidOperation("refusing to download an empty image".into()));
    }

    let command = format!("download:{:08x}", size);
    tracing::debug!("fastboot >> {}", command);
    self.transport().bulk_out(command.as_bytes(), COMMAND_TIMEOUT).await?;

    let mut buf = [0u8; PACKET_BYTES];
    let read = self.transport().bulk_in(&mut buf, COMMAND_TIMEOUT).await?;
    if read < 4 {
      return Err(fastboot_error(&command, "device sent a truncated reply"));
    }
    let ack = String::from_utf8_lossy(&buf[..read]).into_owned();
    if !ack.starts_with("DATA") {
      return Err(fastboot_error(&command, ack[4.min(ack.len())..].to_owned()));
    }

    let accepted = usize::from_str_radix(ack[4..12.min(ack.len())].trim(), 16)
      .map_err(|_| fastboot_error(&command, format!("could not parse the DATA reply {:?}", ack)))?;
    if accepted != size {
      return Err(fastboot_error(
        &command,
        format!("device accepted {} bytes but we offered {}", accepted, size),
      ));
    }

    let mut sent = 0;
    while sent < size {
      let end = std::cmp::min(sent + TRANSFER_CHUNK_BYTES, size);
      self.transport().bulk_out(&data[sent..end], DOWNLOAD_TIMEOUT).await?;
      sent = end;
      on_progress(sent, size);
    }

    let reply = self.read_reply(&command, COMMAND_TIMEOUT).await?;
    if !reply.ok {
      return Err(fastboot_error(&command, reply.response));
    }
    Ok(())
  }

  /// Write whatever was last downloaded to a partition (GPT name, raw alias, or `mmc0boot0`/`mmc0boot1`).
  pub async fn flash(&self, partition: &str) -> Result<()> {
    let command = format!("flash:{}", partition);
    let reply = self.send_with_timeout(&command, COMMIT_TIMEOUT).await?;
    if !reply.ok {
      return Err(fastboot_error(&command, reply.response));
    }
    Ok(())
  }

  /// Erase a partition.
  pub async fn erase(&self, partition: &str) -> Result<()> {
    let command = format!("erase:{}", partition);
    let reply = self.send_with_timeout(&command, COMMIT_TIMEOUT).await?;
    if !reply.ok {
      return Err(fastboot_error(&command, reply.response));
    }
    Ok(())
  }

  /// Run a u-boot command and return its console output.
  ///
  /// `oem console <cmd>` resets u-boot's console ring buffer, runs the command, then replays the buffer as INFO
  /// packets — so it is execute-and-read in a single round trip.
  ///
  /// Note that the OKAY reports the *drain* succeeding, not the command: a u-boot command that fails still comes back
  /// OKAY with the complaint in the output. Callers that care have to read the text.
  pub async fn console(&self, command: &str) -> Result<String> {
    let full = format!("oem console {}", command);
    let reply = self.send_with_timeout(&full, COMMIT_TIMEOUT).await?;
    if !reply.ok {
      return Err(fastboot_error(&full, reply.response));
    }
    Ok(reply.text())
  }

  /// Drain whatever u-boot has printed since the last reset, running nothing.
  pub async fn drain_console(&self) -> Result<String> {
    Ok(self.send("oem console").await?.text())
  }

  /// Point a throwaway fastboot partition alias at a raw LBA range so plain `flash:` can write anywhere.
  ///
  /// u-boot resolves an unknown flash target by looking up `fastboot_raw_partition_<alias>` = `"<start_lba>
  /// <sector_count>"`, which means we get its sparse-image handling and bounds checks for free instead of
  /// hand-rolling `mmc write`. Keep `alias` short: the whole command has to fit in [`MAX_COMMAND_BYTES`].
  pub async fn set_raw_target(&self, alias: &str, start_lba: u64, sector_count: u64) -> Result<()> {
    self
      .console(&format!(
        "setenv fastboot_raw_partition_{} {} {}",
        alias, start_lba, sector_count
      ))
      .await?;
    Ok(())
  }

  /// Unset a raw alias created by [`Fastboot::set_raw_target`].
  pub async fn clear_raw_target(&self, alias: &str) -> Result<()> {
    self
      .console(&format!("setenv fastboot_raw_partition_{}", alias))
      .await?;
    Ok(())
  }

  /// Select an eMMC hardware partition (0 = user area, 1 = boot0, 2 = boot1).
  ///
  /// Flashing `mmc0boot0`/`mmc0boot1` leaves the hwpart selected — `fb_mmc_boot_ops` never restores it — so anything
  /// that touches the user area afterwards has to switch back or it writes into a boot partition.
  pub async fn select_hwpart(&self, hwpart: u8) -> Result<()> {
    if hwpart > 2 {
      return Err(Error::InvalidOperation(format!(
        "eMMC hwpart must be 0, 1 or 2, got {hwpart}"
      )));
    }
    self.console(&format!("mmc dev 0 {}", hwpart)).await?;
    Ok(())
  }

  /// Reboot the device into its normal boot flow.
  ///
  /// The handle is dead once this returns — the device drops off the bus. Callers that want to keep talking to it
  /// have to re-acquire under whichever identity it comes back as.
  pub async fn reboot(&self) -> Result<()> {
    self.expect("reboot").await?;
    self.emit(Event::Resetting);
    Ok(())
  }

  /// Reboot back into fastboot.
  pub async fn reboot_bootloader(&self) -> Result<()> {
    self.expect("reboot-bootloader").await?;
    self.emit(Event::Resetting);
    Ok(())
  }

  /// Hand the device back to the boot ROM (`1b8e:c003`) — our u-boot's `oem maskrom`.
  pub async fn reboot_maskrom(&self) -> Result<()> {
    self.expect("oem maskrom").await?;
    self.emit(Event::Resetting);
    Ok(())
  }

  /// Leave fastboot and continue booting.
  pub async fn continue_boot(&self) -> Result<()> {
    self.expect("continue").await?;
    Ok(())
  }

  /// Set the active A/B slot.
  pub async fn set_active_slot(&self, slot: &str) -> Result<()> {
    self.expect(&format!("set_active:{}", slot)).await?;
    Ok(())
  }
}

#[cfg(not(target_arch = "wasm32"))]
impl Fastboot<crate::native::NativeUsb> {
  /// Find the locally connected device and bring it up in fastboot, bootstrapping through the mask ROM if needed.
  pub async fn connect(callback: Option<Callback>) -> Result<Self> {
    let transport = crate::native::NativeUsb::new(callback.clone());
    Self::init(transport, callback).await
  }

  /// Like [`Fastboot::connect`] but RAM-booting a bootloader you supply instead of the bundled [`FIP_BIN`].
  pub async fn connect_with(bl2: &[u8], fip: &[u8], callback: Option<Callback>) -> Result<Self> {
    let transport = crate::native::NativeUsb::new(callback.clone());
    Self::init_with(transport, bl2, fip, callback).await
  }
}

#[cfg(target_arch = "wasm32")]
impl Fastboot<crate::web::WebUsb> {
  /// Choose a device over WebUSB and bring it up in fastboot, bootstrapping through the mask ROM if needed.
  ///
  /// # Parameters
  /// - `await_gesture`: Called with a reason string when the browser needs a click before it will open the device
  ///   chooser; must resolve once the user has clicked
  /// - `callback`: Optional callback function to receive status updates
  pub async fn connect(await_gesture: js_sys::Function, callback: Option<Callback>) -> Result<Self> {
    let transport = crate::web::WebUsb::new(await_gesture, callback.clone());
    Self::init(transport, callback).await
  }

  /// Like [`Fastboot::connect`] but RAM-booting a bootloader you supply instead of the bundled [`FIP_BIN`].
  pub async fn connect_with(
    await_gesture: js_sys::Function,
    bl2: &[u8],
    fip: &[u8],
    callback: Option<Callback>,
  ) -> Result<Self> {
    let transport = crate::web::WebUsb::new(await_gesture, callback.clone());
    Self::init_with(transport, bl2, fip, callback).await
  }
}

/// Parse a `max-download-size` reply, which u-boot reports in hex but other bootloaders report in decimal.
fn parse_size(raw: &str) -> Option<usize> {
  let value = match raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
    Some(hex) => usize::from_str_radix(hex, 16).ok()?,
    None => raw.parse().ok()?,
  };
  (value > 0).then_some(value)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_both_max_download_size_spellings() {
    assert_eq!(parse_size("0x7000000"), Some(0x7000000));
    assert_eq!(parse_size("0X7000000"), Some(0x7000000));
    assert_eq!(parse_size("117440512"), Some(117440512));
    assert_eq!(parse_size("0"), None);
    assert_eq!(parse_size("nonsense"), None);
  }
}
