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
