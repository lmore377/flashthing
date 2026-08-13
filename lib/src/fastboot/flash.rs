//! Translation layer: flash configs → fastboot.
//!
//! Flash archives are written against the amlogic vendor burn-mode protocol; a device running mainline u-boot
//! answers fastboot instead. Each step is translated here rather than requiring a second archive format.
//!
//! `docs/fastboot.md` has the step-by-step translation table and the reasoning behind it.

use std::{future::Future, pin::Pin, time::Duration};

use super::client::{FASTBOOT_BUF_ADDR, Fastboot};
use crate::{
  Callback, ERASE_GROUP_SECTORS, Error, Event, PART_SECTOR_SIZE, Result,
  config::{FlashConfig, FlashStep, WaitValue},
  flash::{FlashProgress, open_payload, read_payload, read_text},
  partitions::SUPERBIRD_PARTITIONS,
  payload::{PayloadSource, PayloadStore, inline_source},
  time::{Instant, sleep},
  usb::UsbTransport,
};

/// Alias used for raw-LBA writes.
///
/// Kept to two characters on purpose: the whole `oem console setenv fastboot_raw_partition_ft <lba> <sectors>` line
/// has to fit in u-boot's 64-byte fastboot command buffer, and a `mmcpart N` suffix would already overflow it.
const RAW_ALIAS: &str = "ft";

/// Host-side chunk ceiling.
///
/// The device would accept its full 112 MiB download buffer, but a chunk is a strictly serialized round trip —
/// upload, then a blocking `flash:` while the eMMC commits it, with nothing reported during the second half. At
/// 32 MiB that write is a 3-5 second dead stop; at 8 MiB it is under a second, which reads as continuous. The extra
/// `setenv`+`flash` round trips cost microseconds each and total transfer time is unchanged either way.
const MAX_CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// Share of a chunk credited to the upload half of its round trip.
///
/// Only the upload reports bytes; `flash:` returns nothing until the eMMC write is done. Crediting the upload with
/// the whole chunk would park the bar at the chunk boundary for the entire commit, so half is held back and lands
/// when the commit returns. The two halves take roughly comparable time in practice.
const UPLOAD_SHARE: f64 = 0.5;

/// Tracks how far through a single streamed payload we are, in the shape [`FlashProgress`] wants.
struct ProgressTracker {
  total: usize,
  done: usize,
  start: Instant,
  chunk_start: Instant,
  chunks: usize,
  avg_chunk_secs: f64,
  last_rate: f64,
}

impl ProgressTracker {
  fn new(total: usize) -> Self {
    Self {
      total,
      done: 0,
      start: Instant::now(),
      chunk_start: Instant::now(),
      chunks: 0,
      avg_chunk_secs: 0.0,
      last_rate: 0.0,
    }
  }

  fn begin_chunk(&mut self) {
    self.chunk_start = Instant::now();
  }

  /// Record a finished chunk, whether it was written or skipped as all-zero.
  fn complete_chunk(&mut self, length: usize) {
    let secs = self.chunk_start.elapsed().as_secs_f64();
    self.done += length;
    self.chunks += 1;
    self.avg_chunk_secs += (secs - self.avg_chunk_secs) / self.chunks as f64;
    self.last_rate = if secs > 0.0 {
      length as f64 / secs / 1024.0
    } else {
      0.0
    };
  }

  /// A progress reading, optionally crediting `pending` bytes of the chunk currently in flight.
  fn snapshot(&self, pending: f64) -> FlashProgress {
    let seen = (self.done as f64 + pending).min(self.total as f64);
    let elapsed_secs = self.start.elapsed().as_secs_f64();
    let bytes_per_sec = if elapsed_secs > 0.0 { seen / elapsed_secs } else { seen };
    let eta_secs = if bytes_per_sec > 0.0 {
      (self.total as f64 - seen) / bytes_per_sec
    } else {
      0.0
    };

    FlashProgress {
      percent: if self.total > 0 {
        seen / self.total as f64 * 100.0
      } else {
        100.0
      },
      elapsed: elapsed_secs * 1000.0,
      eta: eta_secs * 1000.0,
      rate: self.last_rate,
      avg_chunk_time: self.avg_chunk_secs * 1000.0,
      avg_rate: bytes_per_sec / 1024.0,
    }
  }
}

impl<U: UsbTransport> Fastboot<U> {
  /// Write a stream to a raw LBA range, one download-buffer-sized chunk at a time.
  ///
  /// Each chunk points the throwaway [`RAW_ALIAS`] at its own sector range and then flashes it, so u-boot does the
  /// actual block writes.
  ///
  /// `sparse` erases the range first, then skips the all-zero chunks the erase has already laid down — see
  /// `docs/fastboot.md`. If the erase fails, skipping is abandoned so a zero in the image is never a silent no-op.
  ///
  /// # Parameters
  /// - `start_lba`: absolute LBA on the eMMC user area; sector size is 512
  /// - `source`: the payload providing the data to write
  /// - `size`: total number of bytes to read from `source`
  /// - `sparse`: erase the range first, then skip chunks the erase already zeroed
  /// - `on_progress`: called after each upload slice and each committed chunk
  pub async fn write_raw<F: Fn(FlashProgress)>(
    &self,
    start_lba: u64,
    source: &mut dyn PayloadSource,
    size: usize,
    sparse: bool,
    on_progress: F,
  ) -> Result<()> {
    tracing::info!(
      "streaming {} bytes to the user area starting at LBA {} (sparse: {})",
      size,
      start_lba,
      sparse
    );

    let limit = self.max_download_size().await;
    // whole sectors only: the alias is expressed in sectors, so a chunk that isn't a multiple of one would leave the
    // next chunk's LBA off by a fraction of a sector.
    let chunk_bytes = (std::cmp::min(limit, MAX_CHUNK_BYTES) / PART_SECTOR_SIZE).max(1) * PART_SECTOR_SIZE;

    // Only the whole erase groups strictly inside the range can be erased; a partial group at either end would take
    // neighbouring data with it. Byte offsets, relative to the start of the payload.
    let (mut erased_from, mut erased_to) = (0usize, 0usize);
    if sparse {
      let span_sectors = size.div_ceil(PART_SECTOR_SIZE);
      let first = start_lba as usize;
      let start = first.div_ceil(ERASE_GROUP_SECTORS) * ERASE_GROUP_SECTORS;
      let end = (first + span_sectors) / ERASE_GROUP_SECTORS * ERASE_GROUP_SECTORS;

      if end > start {
        tracing::info!("erasing {} sectors at LBA {} before sparse write", end - start, start);
        match self.erase_raw(start as u64, (end - start) as u64).await {
          Ok(()) => {
            erased_from = (start - first) * PART_SECTOR_SIZE;
            erased_to = (end - first) * PART_SECTOR_SIZE;
          }
          // better to spend the bandwidth than to leave the caller's zeroes unwritten
          Err(err) => tracing::warn!("erase failed ({err}); writing every chunk instead of skipping zeroes"),
        }
      } else {
        tracing::info!("sparse write spans no whole erase group at LBA {first}; writing in full");
      }
    }

    let mut tracker = ProgressTracker::new(size);
    let mut buffer = vec![0u8; chunk_bytes];
    let mut lba = start_lba;
    let mut offset = 0;

    let result = async {
      while offset < size {
        tracker.begin_chunk();

        let length = std::cmp::min(chunk_bytes, size - offset);
        source.read_exact(&mut buffer[..length]).await?;
        let chunk_start = offset;
        offset += length;
        let sectors = length.div_ceil(PART_SECTOR_SIZE);

        let erased = chunk_start >= erased_from && chunk_start + length <= erased_to;
        if erased && buffer[..length].iter().all(|&byte| byte == 0) {
          tracing::debug!("skipping all-zero chunk at LBA {} (already erased)", lba);
          tracker.complete_chunk(length);
          on_progress(tracker.snapshot(0.0));
          lba += sectors as u64;
          continue;
        }

        // the tail chunk is padded out to a whole sector; u-boot writes in sectors either way, and the padding lands
        // in bytes the image didn't define.
        let padded = sectors * PART_SECTOR_SIZE;
        buffer[length..padded].fill(0);

        self.set_raw_target(RAW_ALIAS, lba, sectors as u64).await?;
        self
          .download(&buffer[..padded], |sent, _| {
            on_progress(tracker.snapshot(sent as f64 * UPLOAD_SHARE));
          })
          .await?;
        self.flash(RAW_ALIAS).await?;

        tracker.complete_chunk(length);
        on_progress(tracker.snapshot(0.0));
        lba += sectors as u64;
      }

      Ok(())
    }
    .await;

    // leave no stray alias behind: a later `flash ft` would otherwise hit a stale range instead of failing loudly.
    if let Err(err) = self.clear_raw_target(RAW_ALIAS).await {
      tracing::warn!("could not clear the raw partition alias: {}", err);
    }

    result
  }

  /// Erase a raw sector range through the throwaway alias.
  ///
  /// Kept separate from the write loop's use of the alias so a failed erase can be reported without abandoning the
  /// write: the caller falls back to writing every chunk.
  async fn erase_raw(&self, start_lba: u64, sectors: u64) -> Result<()> {
    self.set_raw_target(RAW_ALIAS, start_lba, sectors).await?;
    self.erase(RAW_ALIAS).await
  }

  /// Flash a partition the device resolves itself, by GPT name.
  ///
  /// Unlike a raw write this can't be split across chunks — u-boot restarts at the partition start for every
  /// `flash:` — so the image has to fit the device's download buffer.
  pub async fn write_by_name<F: Fn(FlashProgress)>(
    &self,
    name: &str,
    source: &mut dyn PayloadSource,
    size: usize,
    on_progress: F,
  ) -> Result<()> {
    let limit = self.max_download_size().await;
    if size > limit {
      return Err(Error::InvalidOperation(format!(
        "{} is {} bytes, larger than the device's {}-byte download buffer; it needs to be a sparse image",
        name, size, limit
      )));
    }

    let mut tracker = ProgressTracker::new(size);
    tracker.begin_chunk();

    let mut data = vec![0u8; size];
    source.read_exact(&mut data).await?;

    self
      .download(&data, |sent, _| {
        on_progress(tracker.snapshot(sent as f64 * UPLOAD_SHARE));
      })
      .await?;
    self.flash(name).await?;

    tracker.complete_chunk(size);
    on_progress(tracker.snapshot(0.0));
    Ok(())
  }

  /// Import a `key=value` environment into u-boot and, unless told otherwise, persist it.
  ///
  /// The text goes into the download buffer and `env import -t` parses it in place, which sidesteps the 64-byte
  /// command limit that a `setenv` per variable would keep running into. Our u-boot keeps its environment in
  /// `uboot.env` on the FAT `env` partition, so `saveenv` is what makes it stick.
  pub async fn write_env(&self, env: &str, save: bool) -> Result<()> {
    if !env.is_ascii() {
      return Err(Error::InvalidOperation("env data must be ascii".into()));
    }

    let normalised = if env.ends_with('\n') {
      env.to_owned()
    } else {
      format!("{}\n", env)
    };
    let bytes = normalised.as_bytes();

    self.download(bytes, |_, _| {}).await?;
    let output = self
      .console(&format!("env import -t {:#x} {}", FASTBOOT_BUF_ADDR, bytes.len()))
      .await?;
    if complains(&output) {
      return Err(Error::InvalidOperation(format!(
        "the bootloader rejected the environment: {}",
        output.trim()
      )));
    }

    if save {
      let saved = self.console("saveenv").await?;
      if complains(&saved) {
        return Err(Error::InvalidOperation(format!(
          "saving the environment failed: {}",
          saved.trim()
        )));
      }
    }

    Ok(())
  }

  /// Write an unbrick image over the start of the eMMC.
  ///
  /// Sparse is forced on: the image is mostly zeroes, and skipping those chunks is the difference between a couple
  /// of minutes and most of an hour.
  pub async fn unbrick_from(&self, source: &mut dyn PayloadSource, size: usize) -> Result<()> {
    tracing::info!("starting unbrick procedure...");

    self.write_raw(0, source, size, true, |progress| {
      tracing::info!(
        "unbrick progress: {:.1}% | elapsed: {:.1}s | eta: {:.1}s | rate: {:.2} KB/s | avg rate: {:.2} KB/s",
        progress.percent,
        progress.elapsed / 1000.0,
        progress.eta / 1000.0,
        progress.rate,
        progress.avg_rate
      );
    })
    .await?;

    tracing::info!("unbrick procedure completed successfully!");
    Ok(())
  }

  /// Unbrick using the image bundled with this crate.
  #[cfg(not(target_arch = "wasm32"))]
  pub async fn unbrick(&self) -> Result<()> {
    let cursor = std::io::Cursor::new(crate::UNBRICK_BIN_ZIP);
    let mut archive = zip::ZipArchive::new(cursor)?;
    let file = archive.by_name("unbrick.bin")?;

    let size = file.size() as usize;
    let mut source = crate::payload::BlockingSource(file);
    self.unbrick_from(&mut source, size).await
  }
}

/// Flashes a firmware configuration to a Superbird running mainline u-boot.
///
/// The fastboot counterpart of [`crate::Flasher`]: same configuration format, same events, different wire protocol.
pub struct FastbootFlasher<U: UsbTransport, S: PayloadStore> {
  fastboot: Fastboot<U>,
  store: S,
  config: FlashConfig,

  step: usize,
  callback: Option<Callback>,
  force_sparse: bool,
}

/// Decide whether a user-area write is really a bootloader needing an info sector, returning the lead to write
/// ahead of the rest of the payload. Consumes the payload's first sector, which comes back inside the lead.
///
/// The size bound is load-bearing: `unbrick.bin` also lands at LBA 0 without an info sector, and shifting a
/// whole-disk image by a sector would destroy it. See `docs/bootloader.md`.
async fn bootloader_lead(lba: u32, size: usize, source: &mut dyn PayloadSource) -> Result<Option<Vec<u8>>> {
  use crate::boot_image::{BOOT_IMAGE_BYTES, INFO_SECTOR_BYTES, info_sector, needs_info_sector};

  if lba != 0 || size > BOOT_IMAGE_BYTES || size < INFO_SECTOR_BYTES {
    return Ok(None);
  }

  let mut head = vec![0u8; INFO_SECTOR_BYTES];
  source.read_exact(&mut head).await?;

  if !needs_info_sector(&head) {
    return Ok(Some(head));
  }

  tracing::info!("payload at LBA 0 is a bare bootloader; prepending an info sector so BL2 lands at LBA 1");
  let mut lead = info_sector().to_vec();
  lead.extend_from_slice(&head);
  Ok(Some(lead))
}

/// A [`PayloadSource`] that yields some bytes of its own before handing over to another source.
///
/// Used to put bytes back that were read in order to classify a payload, and to insert an info sector ahead of a
/// bootloader that arrived without one.
struct PrefixedSource<'a> {
  prefix: Vec<u8>,
  offset: usize,
  rest: &'a mut dyn PayloadSource,
}

impl<'a> PrefixedSource<'a> {
  fn new(prefix: Vec<u8>, rest: &'a mut dyn PayloadSource) -> Self {
    Self {
      prefix,
      offset: 0,
      rest,
    }
  }
}

impl PayloadSource for PrefixedSource<'_> {
  fn read_exact<'b>(&'b mut self, buf: &'b mut [u8]) -> Pin<Box<dyn Future<Output = Result<()>> + 'b>> {
    Box::pin(async move {
      let taken = (self.prefix.len() - self.offset).min(buf.len());
      if taken > 0 {
        buf[..taken].copy_from_slice(&self.prefix[self.offset..self.offset + taken]);
        self.offset += taken;
      }
      if taken < buf.len() {
        self.rest.read_exact(&mut buf[taken..]).await?;
      }
      Ok(())
    })
  }
}

impl<U: UsbTransport, S: PayloadStore> FastbootFlasher<U, S> {
  /// Create a flasher over an already connected device and a payload store
  ///
  /// # Parameters
  /// - `fastboot`: connected device
  /// - `store`: resolves the file references in `config`
  /// - `config`: the parsed and validated flash configuration
  /// - `callback`: Optional callback function to receive status updates
  pub fn new(fastboot: Fastboot<U>, store: S, config: FlashConfig, callback: Option<Callback>) -> Self {
    Self {
      fastboot,
      store,
      config,
      step: 0,
      callback,
      force_sparse: false,
    }
  }

  /// Treat every raw write as sparse, regardless of what the configuration asked for.
  ///
  /// Safe whenever the target range is known to be erased or its previous contents are irrelevant, and a large win
  /// on images that are mostly empty. Not the default, because skipping a zero chunk leaves whatever was there
  /// before rather than zeroing it.
  pub fn force_sparse(mut self, force: bool) -> Self {
    self.force_sparse = force;
    self
  }

  /// The connected device, for operations outside the flash config.
  pub fn device(&self) -> &Fastboot<U> {
    &self.fastboot
  }

  /// Execute the flash process based on the loaded configuration
  pub async fn flash(&mut self) -> Result<()> {
    tracing::info!("beginning flashing process!");

    // same clone dance as the amlogic flasher: the store needs `&mut self` while the steps are being walked
    let steps = self.config.steps.clone();
    for step in &steps {
      tracing::trace!("starting step: {:?}", step);

      self.step += 1;
      if let Some(callback) = &self.callback {
        callback(Event::Step(self.step, step.clone()));
      }

      self.run_step(step).await?;
    }

    self.callback = None;
    Ok(())
  }

  /// Write a prepared boot image to an eMMC boot hwpart, then put the hwpart selection back.
  ///
  /// The image must already be in on-disk form — see [`crate::boot_image`].
  async fn write_boot_hwpart(&self, hwpart: u8, image: &[u8]) -> Result<()> {
    // mmc0boot0 / mmc0boot1 are u-boot's names for the eMMC boot hwparts.
    let target = match hwpart {
      1 => "mmc0boot0",
      2 => "mmc0boot1",
      other => {
        return Err(Error::InvalidOperation(format!(
          "boot hwpart must be 1 or 2, got {other}"
        )));
      }
    };

    // Sized for the smaller of the two boot hwparts in the wild; the tail of a stock dump is zero padding. Real
    // content past the bound would be silently lost, so refuse instead.
    let image = match image.split_at_checked(crate::boot_image::BOOT_HWPART_BYTES) {
      Some((head, tail)) => {
        if tail.iter().any(|&byte| byte != 0) {
          return Err(Error::InvalidOperation(format!(
            "boot image carries content past {} bytes and will not fit a 2 MiB boot hwpart",
            crate::boot_image::BOOT_HWPART_BYTES
          )));
        }
        head
      }
      None => image,
    };

    let mut tracker = ProgressTracker::new(image.len());
    tracker.begin_chunk();

    let progress = progress_reporter(&self.callback);
    self
      .fastboot
      .download(image, |sent, _| {
        progress(tracker.snapshot(sent as f64 * UPLOAD_SHARE));
      })
      .await?;
    self.fastboot.flash(target).await?;

    tracker.complete_chunk(image.len());
    progress(tracker.snapshot(0.0));

    // flashing a boot hwpart leaves it selected; anything touching the user area next would land in the wrong
    // place entirely.
    self.fastboot.select_hwpart(0).await
  }

  async fn run_step(&mut self, step: &FlashStep) -> Result<()> {
    match step {
      FlashStep::Log { value } => {
        tracing::info!(">> {:?}", value);
      }

      FlashStep::Wait { value } => match value {
        WaitValue::UserInput { .. } => return Err(Error::UnsupportedFeature(step.to_owned())),
        WaitValue::Time { time } => sleep(Duration::from_millis(*time)).await,
      },

      FlashStep::WriteUserArea { value } => {
        let sparse = value.sparse.unwrap_or(false) || self.force_sparse;
        let (size, mut source) = open_payload(&value.data, &mut self.store).await?;
        let progress = progress_reporter(&self.callback);

        // Some published archives write a bare bootloader dump straight to LBA 0 as a plain user-area write rather
        // than going through `restorePartition bootloader`. Without an info sector in front of it, BL2 lands at LBA
        // 0 instead of LBA 1 and the device never boots, so give it one here too.
        // The classifier consumes the payload's first sector, so it comes back inside `lead` and is already counted
        // in `size` — only the bytes `lead` adds beyond it change the total.
        let (lead, size) = match bootloader_lead(value.lba, size, source.as_mut()).await? {
          Some(lead) => {
            let total = size - crate::boot_image::INFO_SECTOR_BYTES + lead.len();
            (lead, total)
          }
          None => (Vec::new(), size),
        };

        let mut source = PrefixedSource::new(lead, source.as_mut());
        self
          .fastboot
          .write_raw(value.lba as u64, &mut source, size, sparse, progress)
          .await?;
      }

      // `writeLargeMemory` is a misnomer inherited from the vendor protocol: the address is a byte offset on disk,
      // not in memory, and the amlogic path stages it through DRAM only because burn mode has no other way to get
      // bytes to the eMMC. Over fastboot it is just a raw write at that offset.
      FlashStep::WriteLargeMemory { value } => {
        if !(value.address as usize).is_multiple_of(PART_SECTOR_SIZE) {
          return Err(Error::InvalidOperation(format!(
            "writeLargeMemory address {:#x} is not a multiple of the {}-byte sector size",
            value.address, PART_SECTOR_SIZE
          )));
        }
        let (size, mut source) = open_payload(&value.data, &mut self.store).await?;
        let progress = progress_reporter(&self.callback);
        self
          .fastboot
          .write_raw(
            value.address as u64 / PART_SECTOR_SIZE as u64,
            source.as_mut(),
            size,
            self.force_sparse,
            progress,
          )
          .await?;
      }

      FlashStep::WriteBootPartition { value } => {
        let data = read_payload(&value.data, &mut self.store).await?;
        // A bare bootloader dump gets its info sector here; an image that already has one is written as-is.
        let image = crate::boot_image::to_boot_image(&data);
        self.write_boot_hwpart(value.hwpart, &image).await?;
      }

      FlashStep::RestorePartition { value } if value.name == "bootloader" => {
        // Not a partition write: the image needs an info sector, and goes to the user-area mirror at LBA 0 and
        // both boot hwparts. A raw write here would land a sector early and leave the hwparts empty.
        // See `docs/bootloader.md`.
        let data = read_payload(&value.data, &mut self.store).await?;
        let image = crate::boot_image::to_boot_image(&data);

        let progress = progress_reporter(&self.callback);
        self
          .fastboot
          .write_raw(0, inline_source(&image).as_mut(), image.len(), false, progress)
          .await?;
        self.write_boot_hwpart(1, &image).await?;
        self.write_boot_hwpart(2, &image).await?;
      }

      FlashStep::RestorePartition { value } => {
        let name = value.name.as_str();
        let (size, mut source) = open_payload(&value.data, &mut self.store).await?;
        let progress = progress_reporter(&self.callback);

        // restorePartition names a partition in the *stock amlogic* layout, so it resolves against those offsets
        // rather than whatever GPT the device happens to be carrying right now.
        match SUPERBIRD_PARTITIONS.get(name) {
          Some(partition) => {
            let limit = partition.size * PART_SECTOR_SIZE;
            if partition.size > 0 && size > limit {
              return Err(Error::InvalidOperation(format!(
                "{} image is {} bytes but the partition only holds {}",
                name, size, limit
              )));
            }
            self
              .fastboot
              .write_raw(
                partition.offset as u64,
                source.as_mut(),
                size,
                self.force_sparse,
                progress,
              )
              .await?;
          }
          None => {
            tracing::info!("{} is not a stock partition, flashing it by GPT name", name);
            self.fastboot.write_by_name(name, source.as_mut(), size, progress).await?;
          }
        }
      }

      FlashStep::WriteEnv { value } => {
        let env = read_text(value, &mut self.store).await?;
        self.fastboot.write_env(&env, true).await?;
      }

      FlashStep::Bulkcmd { value } | FlashStep::BulkcmdStat { value, .. } => {
        let Some(translated) = translate_vendor_command(value) else {
          tracing::info!("skipping vendor-only command: {}", value);
          return Ok(());
        };
        if translated != *value {
          tracing::info!("{} -> {}", value, translated);
        }

        let output = self.fastboot.console(&translated).await?;
        if !output.trim().is_empty() {
          tracing::info!(">> {}", output.trim());
        }
        if output.to_lowercase().contains("unknown command") {
          return Err(Error::InvalidOperation(format!(
            "the bootloader does not understand {:?}",
            translated
          )));
        }
      }

      FlashStep::Identify { .. } => {
        let version = self
          .fastboot
          .getvar("version-bootloader")
          .await
          .unwrap_or_else(|_| "unknown".into());
        let product = self.fastboot.getvar("product").await.unwrap_or_else(|_| "unknown".into());
        tracing::info!("device: {}, bootloader {}", product.trim(), version.trim());
      }

      FlashStep::ValidatePartitionSize { value, .. } => {
        let name = value.name.as_str();
        match SUPERBIRD_PARTITIONS.get(name) {
          Some(partition) => tracing::info!("{}: {} sectors", name, partition.size),
          None => {
            let size = self.fastboot.getvar(&format!("partition-size:{}", name)).await?;
            tracing::info!("{}: {}", name, size.trim());
          }
        }
      }

      // mask-ROM-only steps. an archive carrying these is describing its own bootstrap, which happens at connect
      // time instead — by the time we get here the device is already running u-boot.
      FlashStep::Bl2Boot { .. }
      | FlashStep::Run { .. }
      | FlashStep::WriteSimpleMemory { .. }
      | FlashStep::WriteAMLCData { .. }
      | FlashStep::GetBootAMLC { .. } => {
        tracing::info!("skipping {:?}: the device is already booted into fastboot", step);
      }

      FlashStep::ReadSimpleMemory { .. } | FlashStep::ReadLargeMemory { .. } => {
        return Err(Error::UnsupportedFeature(step.to_owned()));
      }
    }

    Ok(())
  }

  /// get the total number of steps in the flash config
  pub fn num_steps(&self) -> usize {
    self.config.steps.len()
  }

  /// get current step in the flashing process
  pub fn current_step(&self) -> usize {
    self.step + 1
  }
}

#[cfg(not(target_arch = "wasm32"))]
impl FastbootFlasher<crate::native::NativeUsb, crate::native::FlashMode> {
  /// Create a flasher where the flash files are relative to `path`, which MUST be a directory.
  ///
  /// NOTE: Car Thing is expected to be plugged in at time of creation, either already in fastboot or in USB mode so
  /// it can be bootstrapped there.
  pub async fn from_directory(path: std::path::PathBuf, callback: Option<Callback>) -> Result<Self> {
    tracing::debug!("creating new fastboot flasher from directory at {:?}", &path);

    let config = FlashConfig::from_directory(&path)?;
    let fastboot = Fastboot::connect(callback.clone()).await?;

    Ok(Self::new(
      fastboot,
      crate::native::FlashMode::Directory(path),
      config,
      callback,
    ))
  }

  /// Create a flasher over a zip archive.
  ///
  /// NOTE: Car Thing is expected to be plugged in at time of creation.
  pub async fn from_archive(path: std::path::PathBuf, callback: Option<Callback>) -> Result<Self> {
    tracing::debug!("creating new fastboot flasher from archive at {:?}", &path);

    let mut zip = crate::flash::open_zip(&path)?;
    let config = FlashConfig::from_archive(&mut zip)?;
    let fastboot = Fastboot::connect(callback.clone()).await?;

    Ok(Self::new(
      fastboot,
      crate::native::FlashMode::Archive(zip),
      config,
      callback,
    ))
  }

  /// Create a flasher from a standalone `meta.json`, resolving files relative to the cwd.
  ///
  /// NOTE: Car Thing is expected to be plugged in at time of creation.
  pub async fn from_json(meta: String, callback: Option<Callback>) -> Result<Self> {
    tracing::debug!("creating new fastboot flasher from json string");

    let config = FlashConfig::from_standalone(&meta)?;
    let fastboot = Fastboot::connect(callback.clone()).await?;

    Ok(Self::new(
      fastboot,
      crate::native::FlashMode::Standalone,
      config,
      callback,
    ))
  }

  /// Create a flasher over a stock dump in a directory, using the built-in stock configuration.
  ///
  /// NOTE: Car Thing is expected to be plugged in at time of creation.
  pub async fn from_stock_directory(path: std::path::PathBuf, callback: Option<Callback>) -> Result<Self> {
    tracing::debug!("creating new stock fastboot flasher from directory at {:?}", &path);

    let config = FlashConfig::from_stock()?;
    let fastboot = Fastboot::connect(callback.clone()).await?;

    Ok(Self::new(
      fastboot,
      crate::native::FlashMode::Directory(path),
      config,
      callback,
    ))
  }

  /// Create a flasher over a stock dump in a zip archive, using the built-in stock configuration.
  ///
  /// NOTE: Car Thing is expected to be plugged in at time of creation.
  pub async fn from_stock_archive(path: std::path::PathBuf, callback: Option<Callback>) -> Result<Self> {
    tracing::debug!("creating new stock fastboot flasher from archive at {:?}", &path);

    let zip = crate::flash::open_zip(&path)?;
    let config = FlashConfig::from_stock()?;
    let fastboot = Fastboot::connect(callback.clone()).await?;

    Ok(Self::new(
      fastboot,
      crate::native::FlashMode::Archive(zip),
      config,
      callback,
    ))
  }
}

/// Adapt the caller's event callback into the progress sink the write helpers take.
fn progress_reporter(callback: &Option<Callback>) -> impl Fn(FlashProgress) + use<> {
  let callback = callback.clone();
  move |progress: FlashProgress| {
    if let Some(callback) = &callback {
      callback(Event::FlashProgress(progress));
    }
  }
}

/// Whether a u-boot console dump reads like a complaint.
///
/// `oem console` reports the console *drain* succeeding, not the command, so a failed `env import` still comes back
/// OKAY and the only signal is the text it printed.
fn complains(output: &str) -> bool {
  let lower = output.to_lowercase();
  lower.contains("error") || lower.contains("failed") || lower.contains("unknown command")
}

/// Rewrite a vendor burn-mode u-boot command for mainline u-boot, or return `None` if it has no meaning here.
///
/// Only a handful of differences matter in practice. Vendor burn mode reaches the eMMC as `mmc dev 1`; ours is
/// `mmc dev 0`. `amlmmc` is amlogic's fork of `mmc`, and its partition and key subcommands operate on an amlogic
/// partition table our layout doesn't have. Anything unrecognised is passed through — u-boot will say so if it
/// doesn't know the command.
pub fn translate_vendor_command(command: &str) -> Option<String> {
  let trimmed = command.trim();
  let lower = trimmed.to_lowercase();

  // vendor partition-table / key-enclave setup: no equivalent, and nothing downstream depends on it once we are
  // writing raw LBAs.
  if lower.starts_with("disk_initial")
    || lower.starts_with("amlmmc key")
    || lower.starts_with("amlmmc part")
    || lower.starts_with("amlmmc partition")
  {
    return None;
  }

  // vendor burn mode numbers the eMMC as device 1.
  if lower.starts_with("mmc dev 1") || lower.starts_with("amlmmc dev 1") {
    return Some("mmc dev 0 0".into());
  }

  // everything else amlmmc does that we care about (read/write/erase) has the same argument shape as plain mmc.
  if let Some(rest) = trimmed.strip_prefix("amlmmc") {
    return Some(format!("mmc{}", rest));
  }

  Some(trimmed.to_owned())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn drops_vendor_only_commands() {
    assert_eq!(translate_vendor_command("amlmmc key"), None);
    assert_eq!(translate_vendor_command("amlmmc part 1"), None);
    assert_eq!(translate_vendor_command("disk_initial 0"), None);
  }

  #[test]
  fn remaps_the_vendor_emmc_device_number() {
    assert_eq!(translate_vendor_command("mmc dev 1").as_deref(), Some("mmc dev 0 0"));
    assert_eq!(
      translate_vendor_command("amlmmc dev 1").as_deref(),
      Some("mmc dev 0 0")
    );
  }

  #[test]
  fn rewrites_amlmmc_to_mmc_and_passes_everything_else_through() {
    assert_eq!(
      translate_vendor_command("amlmmc write bootloader 0x1080000 0x0 0x1000").as_deref(),
      Some("mmc write bootloader 0x1080000 0x0 0x1000")
    );
    assert_eq!(translate_vendor_command("  saveenv  ").as_deref(), Some("saveenv"));
  }

  #[test]
  fn console_output_is_checked_for_complaints() {
    assert!(complains("Unknown command 'env' - try 'help'"));
    assert!(complains("## Error: failed to import"));
    assert!(!complains("Saving Environment to FAT... OK"));
  }

  #[test]
  fn progress_credits_pending_upload_bytes_without_overshooting() {
    let mut tracker = ProgressTracker::new(1000);
    tracker.begin_chunk();
    assert_eq!(tracker.snapshot(250.0).percent, 25.0);

    tracker.complete_chunk(500);
    assert_eq!(tracker.snapshot(0.0).percent, 50.0);
    // a pending credit larger than what is left cannot push the bar past the end
    assert_eq!(tracker.snapshot(9999.0).percent, 100.0);
  }

  /// Reading a `PrefixedSource` in odd-sized bites must not lose or duplicate the boundary.
  #[test]
  fn prefixed_source_splices_across_the_boundary() {
    let rest: Vec<u8> = (0..32u8).collect();
    let mut inner = crate::payload::BlockingSource(std::io::Cursor::new(rest.clone()));
    let mut source = PrefixedSource::new(vec![0xaa; 8], &mut inner);

    let mut out = vec![0u8; 40];
    pollster::block_on(source.read_exact(&mut out[..3])).unwrap();
    pollster::block_on(source.read_exact(&mut out[3..12])).unwrap();
    pollster::block_on(source.read_exact(&mut out[12..])).unwrap();

    assert_eq!(&out[..8], &[0xaa; 8]);
    assert_eq!(&out[8..], &rest[..]);
  }

  fn lead_for(lba: u32, payload: &[u8]) -> Option<Vec<u8>> {
    let mut source = crate::payload::BlockingSource(std::io::Cursor::new(payload.to_vec()));
    pollster::block_on(bootloader_lead(lba, payload.len(), &mut source)).unwrap()
  }

  /// The published 8.2.5 archive writes a bare dump straight to LBA 0; it has to gain an info sector.
  #[test]
  fn a_bare_bootloader_at_lba_0_gains_an_info_sector() {
    let payload = vec![0x5a; crate::boot_image::BOOT_IMAGE_BYTES];
    let lead = lead_for(0, &payload).expect("should be classified");

    assert_eq!(lead.len(), 2 * crate::boot_image::INFO_SECTOR_BYTES);
    assert_eq!(&lead[..crate::boot_image::INFO_SECTOR_BYTES], &crate::boot_image::info_sector());
    // the sector consumed to classify it is handed back intact
    assert_eq!(&lead[crate::boot_image::INFO_SECTOR_BYTES..], &payload[..crate::boot_image::INFO_SECTOR_BYTES]);
  }

  /// A whole-disk image also lands at LBA 0 and also lacks an info sector. Shifting it would destroy the image, so
  /// the size bound has to keep it out.
  #[test]
  fn a_whole_disk_image_at_lba_0_is_left_alone() {
    let payload = vec![0x5a; crate::boot_image::BOOT_IMAGE_BYTES + 1];
    assert!(lead_for(0, &payload).is_none());
  }

  /// An image that already carries an info sector keeps its byte count unchanged.
  #[test]
  fn a_prepared_bootloader_at_lba_0_is_not_shifted() {
    let mut payload = crate::boot_image::info_sector().to_vec();
    payload.resize(crate::boot_image::BOOT_IMAGE_BYTES, 0x5a);

    let lead = lead_for(0, &payload).expect("should be classified");
    assert_eq!(lead.len(), crate::boot_image::INFO_SECTOR_BYTES);
    assert_eq!(&lead[..], &payload[..crate::boot_image::INFO_SECTOR_BYTES]);
  }

  /// Anywhere but LBA 0 is a normal partition write and must never be touched.
  #[test]
  fn writes_away_from_lba_0_are_never_reshaped() {
    let payload = vec![0x5a; 4096];
    assert!(lead_for(1, &payload).is_none());
    assert!(lead_for(438272, &payload).is_none());
  }
}
