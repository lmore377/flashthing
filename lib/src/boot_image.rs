//! The on-disk form of the amlogic bootloader.
//!
//! A stock `bootloader.dump` is a bare bootloader image: signed BL2 first, then the FIP. That is *not* what the
//! SoC expects to find on eMMC. Both places a bootloader lives — the eMMC boot hwparts and the user-area mirror at
//! LBA 0 — hold a 512-byte **info sector** first, so that BL2 itself begins at LBA 1. The mask ROM reads BL2 from
//! LBA 1, not LBA 0.
//!
//! Vendor u-boot builds that info sector itself, which is why `amlmmc write bootloader` can be handed a bare dump.
//! Nothing outside vendor u-boot does, so anything writing a bootloader over fastboot (or any other raw path) has
//! to prepend it — a bare dump written at offset 0 puts every byte one sector early and simply will not boot.
//!
//! See `docs/bootloader.md` for the full layout and how the two copies are used.

/// Size of the info sector, and therefore the offset the bootloader image itself sits at.
pub const INFO_SECTOR_BYTES: usize = 512;

/// How much of a bootloader image lands on disk, matching the eMMC boot hwpart size on a Car Thing.
pub const BOOT_IMAGE_BYTES: usize = 4 * 1024 * 1024;

/// First bytes of the *stock* Car Thing BL2.
///
/// Kept as a cross-check and a landmark when reading hex dumps. It is tempting to use as an "is this a bare image?"
/// test, since `superbird.bl2.encrypted.bin`, `superbird.bootloader.img` and a stock `bootloader.dump` all begin
/// with it — but all three derive from the same stock BL2, and BL2 is encrypted, so this is one build's first
/// ciphertext block rather than a magic. An 8.9.2 thinglabs `bootloader.dump` does not contain the sequence
/// anywhere. Testing for it classifies every differently-signed bootloader as already prepared, which writes it a
/// sector early — the one mistake on this path that reads back perfect and never boots.
pub const STOCK_BL2_PREFIX: [u8; 8] = [0x0c, 0x62, 0x7a, 0x15, 0xbe, 0x94, 0x07, 0xb2];

/// Offset past the info sector's defined fields; everything from here to the checksum is reserved.
const INFO_SECTOR_RESERVED_FROM: usize = 0x18;

/// Build the info sector for a Car Thing.
///
/// This is amlogic's `storage_emmc_boot_info`. BL2 never reads it — its only job is to occupy LBA 0 — but a
/// well-formed one is free and keeps the image byte-compatible with vendor tooling.
pub fn info_sector() -> [u8; INFO_SECTOR_BYTES] {
  let mut sector = [0u8; INFO_SECTOR_BYTES];

  let mut put = |offset: usize, value: u32| sector[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
  put(0x000, 1); // version
  put(0x004, 0x12000); // rsv_base_addr, in sectors: the amlogic reserved region at 36 MiB
  put(0x008, 0); // dtb.addr — vendor leaves these zero
  put(0x00c, 0); // dtb.size
  put(0x010, 0x4000); // ddr.addr, in sectors, relative to the reserved region
  put(0x014, 4); // ddr.size, in sectors

  // The checksum is a wrapping sum of every u32 ahead of it, and lives in the last one.
  let checksum = sector[..INFO_SECTOR_BYTES - 4]
    .chunks_exact(4)
    .fold(0u32, |acc, word| acc.wrapping_add(u32::from_le_bytes(word.try_into().unwrap())));
  sector[INFO_SECTOR_BYTES - 4..].copy_from_slice(&checksum.to_le_bytes());

  sector
}

/// Whether `data` already opens with an info sector.
///
/// Detecting the sector is far more reliable than detecting the bootloader behind it. BL2 is encrypted, so its
/// leading bytes differ per build and per signing key and cannot be recognised at all; an info sector is a fixed
/// shape — a handful of small header fields, ~480 bytes of zero padding, and a checksum of everything ahead of it
/// in the last word. High-entropy ciphertext does not accidentally take that shape.
///
/// An all-zero sector passes too, which is intended: that is what `unbrick.bin` and other whole-disk images carry
/// at LBA 0, and it boots.
fn has_info_sector(data: &[u8]) -> bool {
  if data.len() < INFO_SECTOR_BYTES {
    return false;
  }
  if data[INFO_SECTOR_RESERVED_FROM..INFO_SECTOR_BYTES - 4].iter().any(|&byte| byte != 0) {
    return false;
  }

  let checksum = data[..INFO_SECTOR_BYTES - 4]
    .chunks_exact(4)
    .fold(0u32, |acc, word| acc.wrapping_add(u32::from_le_bytes(word.try_into().unwrap())));
  checksum == u32::from_le_bytes(data[INFO_SECTOR_BYTES - 4..INFO_SECTOR_BYTES].try_into().unwrap())
}

/// Whether `data` is a bare bootloader image that still needs an info sector in front of it — i.e. anything not
/// already carrying one.
///
/// Defaulting to "bare" is deliberate. Getting it wrong in this direction writes a spurious 512 bytes ahead of an
/// image that did not need them, which is visible immediately; getting it wrong the other way puts a whole
/// bootloader one sector early, where every byte reads back correct and the device simply never boots.
pub fn needs_info_sector(data: &[u8]) -> bool {
  !has_info_sector(data)
}

/// Put a bootloader image into the form the SoC expects to find on eMMC.
///
/// A bare image gets an info sector prepended; one that already has it is passed through untouched, so callers can
/// hand this either a stock `bootloader.dump` or a pre-built boot-partition image without having to know which.
/// The result is capped at [`BOOT_IMAGE_BYTES`], which only ever discards trailing padding — the real content is
/// around 1.3 MiB.
pub fn to_boot_image(data: &[u8]) -> Vec<u8> {
  if !needs_info_sector(data) {
    return data[..data.len().min(BOOT_IMAGE_BYTES)].to_vec();
  }

  let mut image = Vec::with_capacity(BOOT_IMAGE_BYTES.min(data.len() + INFO_SECTOR_BYTES));
  image.extend_from_slice(&info_sector());
  image.extend_from_slice(data);
  image.truncate(BOOT_IMAGE_BYTES);
  image
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The values read off a Car Thing that boots, so a refactor cannot quietly change what we write.
  #[test]
  fn info_sector_matches_hardware() {
    let sector = info_sector();
    assert_eq!(&sector[0x000..0x004], &1u32.to_le_bytes());
    assert_eq!(&sector[0x004..0x008], &0x12000u32.to_le_bytes());
    assert_eq!(&sector[0x010..0x014], &0x4000u32.to_le_bytes());
    assert_eq!(&sector[0x014..0x018], &4u32.to_le_bytes());
    // checksum = 1 + 0x12000 + 0x4000 + 4
    assert_eq!(&sector[0x1fc..0x200], &0x16005u32.to_le_bytes());
    assert!(sector[0x018..0x1fc].iter().all(|&b| b == 0));
  }

  /// A bare image is anything that does not open with an info sector, whatever its leading bytes happen to be.
  #[test]
  fn bare_images_get_an_info_sector() {
    let mut bare = STOCK_BL2_PREFIX.to_vec();
    bare.extend_from_slice(&[0xab; 1024]);

    let image = to_boot_image(&bare);
    assert_eq!(&image[..INFO_SECTOR_BYTES], &info_sector());
    assert_eq!(&image[INFO_SECTOR_BYTES..], &bare[..]);
  }

  /// The regression that cost a flash: a bootloader signed with a different key shares none of the stock BL2's
  /// leading bytes, and was therefore taken for an already-prepared image and written a sector early.
  #[test]
  fn differently_signed_bootloaders_are_still_bare() {
    // Stand-in for an 8.9.2 thinglabs dump: ciphertext from byte 0, no recognisable header anywhere.
    let bare: Vec<u8> = (0..4096u32).map(|i| i.wrapping_mul(2654435761).to_le_bytes()[0]).collect();
    assert!(!bare.starts_with(&STOCK_BL2_PREFIX));
    assert!(needs_info_sector(&bare));

    let image = to_boot_image(&bare);
    assert_eq!(&image[..INFO_SECTOR_BYTES], &info_sector());
    assert_eq!(&image[INFO_SECTOR_BYTES..], &bare[..]);
  }

  #[test]
  fn prepared_images_are_left_alone() {
    let mut prepared = info_sector().to_vec();
    prepared.extend_from_slice(&STOCK_BL2_PREFIX);

    assert_eq!(to_boot_image(&prepared), prepared);
  }

  /// What `unbrick.bin` and other whole-disk images carry at LBA 0. Shifting one of those by a sector would
  /// destroy the whole image, so the all-zero sector has to read as prepared.
  #[test]
  fn an_all_zero_info_sector_counts_as_prepared() {
    let mut prepared = vec![0u8; INFO_SECTOR_BYTES];
    prepared.extend_from_slice(&STOCK_BL2_PREFIX);

    assert!(!needs_info_sector(&prepared));
    assert_eq!(to_boot_image(&prepared), prepared);
  }

  /// A payload too short to contain a sector cannot be carrying one.
  #[test]
  fn short_payloads_are_bare() {
    assert!(needs_info_sector(&[0u8; INFO_SECTOR_BYTES - 1]));
  }

  /// A sector-shaped block whose checksum does not add up is not an info sector.
  #[test]
  fn a_bad_checksum_is_not_an_info_sector() {
    let mut sector = info_sector();
    sector[0x1fc] ^= 0xff;

    assert!(needs_info_sector(&sector));
  }

  #[test]
  fn oversized_images_are_capped() {
    let mut bare = STOCK_BL2_PREFIX.to_vec();
    bare.resize(BOOT_IMAGE_BYTES, 0);

    // Zero padding after a non-zero header still fails the checksum, so this stays bare.
    assert!(needs_info_sector(&bare));
    assert_eq!(to_boot_image(&bare).len(), BOOT_IMAGE_BYTES);
  }
}
