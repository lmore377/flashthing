//! Hardware tests for the fastboot transport.
//!
//! These need a Car Thing attached, either already in fastboot or in USB mode (buttons 1 & 4 held at power-on) so it
//! can be bootstrapped there. They are opt-in:
//!
//! ```bash
//! cargo test --test fastboot_hardware -- --ignored --test-threads 1
//! ```
//!
//! Nothing here writes to the eMMC. The download test fills the device's DRAM scratch buffer and stops short of
//! `flash:`, and the alias test only touches u-boot's in-memory environment — no `saveenv`.

use flashthing::{Fastboot, fastboot::MAX_COMMAND_BYTES};

fn connect() -> Fastboot<flashthing::NativeUsb> {
  pollster::block_on(Fastboot::connect(None)).expect("could not reach the device in fastboot")
}

#[test]
#[ignore]
fn reports_a_usable_download_buffer() {
  let fastboot = connect();
  let size = pollster::block_on(fastboot.max_download_size());

  // u-boot's default is 112 MiB; anything smaller than a single chunk would make chunked writes pointless.
  assert!(
    size >= 8 * 1024 * 1024,
    "device reported a {size}-byte download buffer, which is smaller than one chunk"
  );
}

#[test]
#[ignore]
fn raw_partition_alias_round_trips() {
  let fastboot = connect();

  // the widest range the stock table has, to prove a realistic alias still fits in FASTBOOT_COMMAND_LEN
  let (lba, sectors) = (3_256_496u64, 4_476_752u64);
  assert!(
    format!("oem console setenv fastboot_raw_partition_ft {lba} {sectors}").len() <= MAX_COMMAND_BYTES,
    "the alias command has outgrown u-boot's command buffer"
  );

  pollster::block_on(fastboot.set_raw_target("ft", lba, sectors)).expect("could not set the raw target");

  let set = pollster::block_on(fastboot.console("printenv fastboot_raw_partition_ft")).expect("printenv failed");
  assert!(
    set.contains(&format!("{lba} {sectors}")),
    "u-boot did not store the range we asked for: {set:?}"
  );

  pollster::block_on(fastboot.clear_raw_target("ft")).expect("could not clear the raw target");

  let cleared = pollster::block_on(fastboot.console("printenv fastboot_raw_partition_ft")).expect("printenv failed");
  assert!(
    cleared.contains("not defined"),
    "the alias outlived the write it was made for: {cleared:?}"
  );
}

#[test]
#[ignore]
fn download_fills_the_scratch_buffer() {
  let fastboot = connect();

  // a pattern rather than zeroes, so the checksum below cannot pass on a buffer that was never filled
  let payload: Vec<u8> = (0..64 * 1024u32).map(|i| (i % 251) as u8).collect();

  // `download` takes a plain `Fn`, so the running total lives in a Cell rather than being captured by mut
  let last_sent = std::cell::Cell::new(0usize);
  pollster::block_on(fastboot.download(&payload, |sent, total| {
    assert_eq!(total, payload.len());
    assert!(sent > last_sent.get(), "progress went backwards");
    last_sent.set(sent);
  }))
  .expect("download failed");
  assert_eq!(last_sent.get(), payload.len(), "progress never reached the end");

  // checksum the DRAM the download landed in, which is the only way to prove the bytes arrived intact. u-boot
  // prints `crc32 for <start> ... <end> ==> <hex>`; this build has no md5sum/sha1sum.
  let expected = format!("{:08x}", crc32(&payload));
  let output = pollster::block_on(fastboot.console(&format!("crc32 0x6000000 {:x}", payload.len())))
    .expect("crc32 failed");
  assert!(
    output.to_lowercase().contains(&expected),
    "the device's copy does not match what we sent: {output:?} (expected {expected})"
  );
}

/// CRC-32/ISO-HDLC, matching u-boot's `crc32`. Inline so the test needs no dependency.
fn crc32(input: &[u8]) -> u32 {
  let mut crc = 0xffff_ffffu32;
  for &byte in input {
    crc ^= byte as u32;
    for _ in 0..8 {
      // the reflected polynomial, which is what makes this the same crc32 zlib and u-boot compute
      crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
    }
  }
  !crc
}

#[cfg(test)]
mod tests {
  use super::crc32;

  #[test]
  fn crc32_matches_the_known_vectors() {
    assert_eq!(crc32(b""), 0x0000_0000);
    assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    assert_eq!(crc32(b"The quick brown fox jumps over the lazy dog"), 0x414f_a339);
  }

  /// The first 256 bytes of the download test's payload, checked against what the device reported for the same
  /// range, so a wrong checksum here can't be mistaken for a bad transfer.
  #[test]
  fn crc32_matches_what_the_device_computed() {
    let payload: Vec<u8> = (0..256u32).map(|i| (i % 251) as u8).collect();
    assert_eq!(crc32(&payload), 0x5708_a3cc);
  }
}

// ---------------------------------------------------------------------------
// eMMC write tests
//
// These are the only tests here that commit to flash. They need a scratch LBA
// pointed at space the device is not using, given as FLASHTHING_SCRATCH_LBA,
// and they skip themselves when it is unset so that nobody writes over a
// stranger's eMMC by running `--ignored`.
//
// The scratch window is backed up into DRAM before anything is written and put
// back afterwards, so even a scratch range that is not empty survives intact —
// provided the run finishes. See SCRATCH_SECTORS for the size.
// ---------------------------------------------------------------------------

/// Size of the scratch window: enough for three chunks at the 8 MiB ceiling.
const SCRATCH_SECTORS: u64 = 40960; // 20 MiB
const SCRATCH_BYTES: usize = SCRATCH_SECTORS as usize * 512;

/// Where the backup of the scratch window is parked. Above the 112 MiB fastboot download buffer at 0x6000000, so a
/// download can't tread on it.
const BACKUP_ADDR: u64 = 0x1000_0000;
/// Where read-backs land, clear of both the download buffer and the backup.
const VERIFY_ADDR: u64 = 0x1400_0000;

/// The chunk size `write_raw` settles on, given a device that reports at least this much download buffer.
const CHUNK_BYTES: usize = 8 * 1024 * 1024;

fn scratch_lba() -> Option<u64> {
  match std::env::var("FLASHTHING_SCRATCH_LBA") {
    Ok(raw) => Some(raw.trim().parse().expect("FLASHTHING_SCRATCH_LBA must be a decimal LBA")),
    Err(_) => {
      eprintln!("skipping: set FLASHTHING_SCRATCH_LBA to an LBA in unused space to run the write tests");
      None
    }
  }
}

/// A cheap deterministic pattern. `seed` makes each phase's payload distinguishable from the last.
fn pattern(len: usize, seed: u32) -> Vec<u8> {
  let mut state = seed | 1;
  (0..len)
    .map(|_| {
      state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
      (state >> 24) as u8
    })
    .collect()
}

/// Read `len` bytes from `lba` into scratch DRAM and ask u-boot to checksum them.
fn device_crc(fastboot: &Fastboot<flashthing::NativeUsb>, lba: u64, len: usize) -> u32 {
  let sectors = len.div_ceil(512);
  let read = pollster::block_on(fastboot.console(&format!("mmc read {VERIFY_ADDR:#x} {lba:x} {sectors:x}")))
    .expect("mmc read failed");
  assert!(read.contains("OK"), "mmc read did not report OK: {read:?}");

  let output = pollster::block_on(fastboot.console(&format!("crc32 {VERIFY_ADDR:#x} {len:x}"))).expect("crc32 failed");
  let hex = output
    .rsplit("==>")
    .next()
    .expect("crc32 printed no result")
    .split_whitespace()
    .next()
    .expect("crc32 printed no value");
  u32::from_str_radix(hex, 16).unwrap_or_else(|_| panic!("could not parse the crc out of {output:?}"))
}

fn write_scratch(fastboot: &Fastboot<flashthing::NativeUsb>, lba: u64, data: &[u8], sparse: bool) -> Vec<f64> {
  let percents = std::cell::RefCell::new(Vec::new());
  let mut source = flashthing::payload::inline_source(data);

  pollster::block_on(fastboot.write_raw(lba, source.as_mut(), data.len(), sparse, |progress| {
    percents.borrow_mut().push(progress.percent);
  }))
  .expect("write_raw failed");

  percents.into_inner()
}

#[test]
#[ignore]
fn writes_to_the_emmc_and_puts_it_back() {
  let Some(lba) = scratch_lba() else { return };
  let fastboot = connect();

  assert!(
    pollster::block_on(fastboot.max_download_size()) >= CHUNK_BYTES,
    "the device's download buffer is too small for these tests to chunk the way they assume"
  );

  // --- back the scratch window up into DRAM ---------------------------------
  let original_crc = device_crc(&fastboot, lba, SCRATCH_BYTES);
  let backup = pollster::block_on(fastboot.console(&format!(
    "mmc read {BACKUP_ADDR:#x} {lba:x} {SCRATCH_SECTORS:x}"
  )))
  .expect("could not back the scratch window up");
  assert!(backup.contains("OK"), "backup read did not report OK: {backup:?}");
  eprintln!("backed up {SCRATCH_BYTES} bytes at LBA {lba} (crc {original_crc:08x})");

  // everything below runs inside a catch so a failed assertion still restores the eMMC
  let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    // --- a write spanning several chunks ------------------------------------
    let payload = pattern(SCRATCH_BYTES, 0xa5a5_0001);
    let percents = write_scratch(&fastboot, lba, &payload, false);

    assert_eq!(
      device_crc(&fastboot, lba, SCRATCH_BYTES),
      crc32(&payload),
      "a {SCRATCH_BYTES}-byte write across {} chunks did not land intact",
      SCRATCH_BYTES.div_ceil(CHUNK_BYTES)
    );

    assert!(
      percents.windows(2).all(|pair| pair[1] >= pair[0]),
      "progress went backwards: {percents:?}"
    );
    assert!(
      percents.last().is_some_and(|last| (last - 100.0).abs() < f64::EPSILON),
      "progress ended at {:?} rather than 100%",
      percents.last()
    );

    // --- a payload whose tail is not a whole sector --------------------------
    let ragged = pattern(1024 * 1024 + 100, 0xa5a5_0002);
    write_scratch(&fastboot, lba, &ragged, false);

    assert_eq!(
      device_crc(&fastboot, lba, ragged.len()),
      crc32(&ragged),
      "the ragged tail of a write was not preserved"
    );

    // the rest of that final sector has to be zero padding, not whatever the chunk buffer happened to hold
    let pad = 512 - (ragged.len() % 512);
    let padded_crc = device_crc(&fastboot, lba, ragged.len() + pad);
    let mut expected = ragged.clone();
    expected.extend(std::iter::repeat_n(0u8, pad));
    assert_eq!(
      padded_crc,
      crc32(&expected),
      "the tail sector was padded with something other than zeroes"
    );

    // --- sparse skips all-zero chunks rather than zeroing them ---------------
    let known = pattern(SCRATCH_BYTES, 0xa5a5_0003);
    write_scratch(&fastboot, lba, &known, false);

    // chunk 0 and chunk 2 carry data, chunk 1 is entirely zero and should be skipped
    let mut sparse_payload = pattern(SCRATCH_BYTES, 0xa5a5_0004);
    sparse_payload[CHUNK_BYTES..2 * CHUNK_BYTES].fill(0);
    write_scratch(&fastboot, lba, &sparse_payload, true);

    // what the eMMC should hold now: the new payload everywhere except the skipped chunk, which kept the old bytes
    let mut expected = sparse_payload.clone();
    expected[CHUNK_BYTES..2 * CHUNK_BYTES].copy_from_slice(&known[CHUNK_BYTES..2 * CHUNK_BYTES]);
    assert_eq!(
      device_crc(&fastboot, lba, SCRATCH_BYTES),
      crc32(&expected),
      "a sparse write did not leave the skipped chunk's previous contents in place"
    );

    // and the same payload written without sparse must actually zero that chunk
    write_scratch(&fastboot, lba, &sparse_payload, false);
    assert_eq!(
      device_crc(&fastboot, lba, SCRATCH_BYTES),
      crc32(&sparse_payload),
      "a non-sparse write skipped a zero chunk it should have written"
    );

    // --- the alias must not outlive the writes -------------------------------
    let alias = pollster::block_on(fastboot.console("printenv fastboot_raw_partition_ft")).expect("printenv failed");
    assert!(
      alias.contains("not defined"),
      "write_raw left its raw alias behind: {alias:?}"
    );
  }));

  // --- put the original bytes back -----------------------------------------
  let restore = pollster::block_on(fastboot.console(&format!(
    "mmc write {BACKUP_ADDR:#x} {lba:x} {SCRATCH_SECTORS:x}"
  )))
  .expect("could not restore the scratch window");
  assert!(restore.contains("OK"), "restore write did not report OK: {restore:?}");

  let restored_crc = device_crc(&fastboot, lba, SCRATCH_BYTES);
  eprintln!("restored LBA {lba} (crc {restored_crc:08x})");

  if let Err(panic) = outcome {
    std::panic::resume_unwind(panic);
  }

  assert_eq!(
    restored_crc, original_crc,
    "the scratch window was not restored to what it held before the test"
  );
}

#[test]
#[ignore]
fn runs_a_flash_config_end_to_end() {
  let Some(lba) = scratch_lba() else { return };

  // a config that exercises the parts of the translation layer a real archive leans on: both vendor-command
  // outcomes (dropped and rewritten), the non-device steps, and a streamed user-area write
  let payload = pattern(3 * 1024 * 1024, 0xa5a5_0005);
  let dir = std::env::temp_dir().join("flashthing-fastboot-e2e");
  std::fs::create_dir_all(&dir).unwrap();
  std::fs::write(dir.join("payload.bin"), &payload).unwrap();
  std::fs::write(
    dir.join("meta.json"),
    format!(
      r#"{{
        "name": "flashthing fastboot hardware test",
        "version": "0.0.0",
        "description": "drives the fastboot translation layer against a real device",
        "metadataVersion": 2,
        "steps": [
          {{ "type": "log", "value": "hello from the hardware test" }},
          {{ "type": "wait", "value": {{ "type": "time", "time": 10 }} }},
          {{ "type": "bulkcmd", "value": "amlmmc key" }},
          {{ "type": "bulkcmd", "value": "mmc dev 1" }},
          {{ "type": "writeUserArea", "value": {{ "lba": {lba}, "data": {{ "filePath": "./payload.bin" }} }} }}
        ]
      }}"#
    ),
  )
  .unwrap();

  let steps = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
  let progress = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
  let callback = {
    let (steps, progress) = (steps.clone(), progress.clone());
    std::sync::Arc::new(move |event: flashthing::Event| match event {
      flashthing::Event::Step(index, _) => steps.lock().unwrap().push(index),
      flashthing::Event::FlashProgress(value) => progress.lock().unwrap().push(value.percent),
      _ => {}
    }) as flashthing::Callback
  };

  let mut flasher = pollster::block_on(flashthing::FastbootFlasher::from_directory(dir.clone(), Some(callback)))
    .expect("could not open the config");
  assert_eq!(flasher.num_steps(), 5);
  pollster::block_on(flasher.flash()).expect("flashing failed");

  assert_eq!(
    *steps.lock().unwrap(),
    vec![1, 2, 3, 4, 5],
    "every step should have announced itself, in order"
  );
  let progress = progress.lock().unwrap().clone();
  assert!(!progress.is_empty(), "the user-area write reported no progress");
  assert!(
    progress.last().is_some_and(|last| (last - 100.0).abs() < f64::EPSILON),
    "progress ended at {:?} rather than 100%",
    progress.last()
  );

  let fastboot = flasher.device();
  assert_eq!(
    device_crc(fastboot, lba, payload.len()),
    crc32(&payload),
    "the config's payload did not land on the eMMC intact"
  );

  std::fs::remove_dir_all(&dir).ok();
}

#[test]
#[ignore]
fn writes_and_persists_the_uboot_environment() {
  if scratch_lba().is_none() {
    return;
  }
  let fastboot = connect();

  // `env import -t` merges rather than replacing, so adding a throwaway key and removing it again leaves the
  // device's real environment as it was
  pollster::block_on(fastboot.write_env("flashthing_test_key=hello world\n", true)).expect("write_env failed");

  let set = pollster::block_on(fastboot.console("printenv flashthing_test_key")).expect("printenv failed");
  assert!(
    set.contains("hello world"),
    "the environment did not take the value we imported: {set:?}"
  );

  // prove it reached the env partition and not just the in-memory copy: reload from storage, discarding the
  // running environment, and the key has to still be there
  let reloaded = pollster::block_on(fastboot.console("env import -d -b")).expect("env reload failed");
  assert!(
    !reloaded.to_lowercase().contains("error"),
    "could not reload the environment from storage: {reloaded:?}"
  );
  let persisted = pollster::block_on(fastboot.console("printenv flashthing_test_key")).expect("printenv failed");
  assert!(
    persisted.contains("hello world"),
    "the value never reached the env partition: {persisted:?}"
  );

  pollster::block_on(fastboot.console("setenv flashthing_test_key")).expect("could not unset the key");
  pollster::block_on(fastboot.console("saveenv")).expect("saveenv failed");

  let cleared = pollster::block_on(fastboot.console("printenv flashthing_test_key")).expect("printenv failed");
  assert!(
    cleared.contains("not defined"),
    "the throwaway key outlived the test: {cleared:?}"
  );
}

/// Exercises `flash:<gpt name>`, which u-boot resolves through its own partition lookup rather than a raw alias.
///
/// Needs FLASHTHING_SCRATCH_PARTITION naming a partition whose first few MiB can be trampled; only that prefix is
/// touched, and it is backed up into DRAM and restored the same way the raw tests do it.
#[test]
#[ignore]
fn writes_a_partition_by_gpt_name() {
  let Ok(name) = std::env::var("FLASHTHING_SCRATCH_PARTITION") else {
    eprintln!("skipping: set FLASHTHING_SCRATCH_PARTITION to a partition whose start can be overwritten");
    return;
  };
  let fastboot = connect();

  let size = pollster::block_on(fastboot.getvar(&format!("partition-size:{name}")))
    .unwrap_or_else(|err| panic!("the device does not know a partition called {name:?}: {err}"));
  // u-boot answers partition-size in bytes, not sectors, despite the name
  eprintln!("{name} is {} bytes", size.trim());

  // resolve where it starts so the backup and the verification read the same bytes `flash:` will write
  let listing = pollster::block_on(fastboot.console("mmc part")).expect("mmc part failed");
  let start = partition_start(&listing, &name)
    .unwrap_or_else(|| panic!("could not find {name:?} in the device's partition table"));

  let payload = pattern(4 * 1024 * 1024, 0xa5a5_0006);
  let sectors = payload.len() as u64 / 512;

  let original_crc = device_crc(&fastboot, start, payload.len());
  let backup = pollster::block_on(fastboot.console(&format!("mmc read {BACKUP_ADDR:#x} {start:x} {sectors:x}")))
    .expect("could not back the partition prefix up");
  assert!(backup.contains("OK"), "backup read did not report OK: {backup:?}");
  eprintln!("backed up the first {} bytes of {name} at LBA {start} (crc {original_crc:08x})", payload.len());

  let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    let mut source = flashthing::payload::inline_source(&payload);
    pollster::block_on(fastboot.write_by_name(&name, source.as_mut(), payload.len(), |_| {}))
      .expect("write_by_name failed");

    assert_eq!(
      device_crc(&fastboot, start, payload.len()),
      crc32(&payload),
      "flashing {name} by GPT name did not land the payload at its start"
    );
  }));

  let restore = pollster::block_on(fastboot.console(&format!("mmc write {BACKUP_ADDR:#x} {start:x} {sectors:x}")))
    .expect("could not restore the partition prefix");
  assert!(restore.contains("OK"), "restore write did not report OK: {restore:?}");
  eprintln!("restored {name} (crc {:08x})", device_crc(&fastboot, start, payload.len()));

  if let Err(panic) = outcome {
    std::panic::resume_unwind(panic);
  }
}

/// Pull a partition's start LBA out of `mmc part` output, whose rows read `  3\t0x00026000\t0x00127fff\t"root_a"`.
fn partition_start(listing: &str, name: &str) -> Option<u64> {
  listing.lines().find_map(|line| {
    let mut fields = line.split_whitespace();
    let _index = fields.next()?;
    let start = fields.next()?.strip_prefix("0x")?;
    let _end = fields.next()?;
    let found = fields.next()?.trim_matches('"');
    (found == name).then(|| u64::from_str_radix(start, 16).ok())?
  })
}

#[cfg(test)]
mod partition_parsing {
  use super::partition_start;

  const LISTING: &str = "\nPartition Map for mmc device 0  --   Partition Type: EFI\n\nPart\tStart LBA\tEnd LBA\t\tName\n\tAttributes\n  1\t0x00002000\t0x00005fff\t\"env\"\n\tattrs:\t0x0000000000000000\n  3\t0x00026000\t0x00127fff\t\"root_a\"\n";

  #[test]
  fn finds_a_partition_start_and_ignores_the_noise() {
    assert_eq!(partition_start(LISTING, "env"), Some(0x2000));
    assert_eq!(partition_start(LISTING, "root_a"), Some(0x26000));
    assert_eq!(partition_start(LISTING, "nope"), None);
  }
}
