//! The on-disk form of the amlogic bootloader.
//!
//! On eMMC a bootloader is preceded by a 512-byte info sector, so BL2 starts at LBA 1 — where the mask ROM reads
//! it. A stock `bootloader.dump` is bare; vendor u-boot builds the sector itself, nothing else does.
//!
//! See `docs/bootloader.md` for the layout, the two copies, and EXT_CSD `PARTITION_CONFIG`.

/// Size of the info sector, and therefore the offset the bootloader image itself sits at.
pub const INFO_SECTOR_BYTES: usize = 512;

/// Largest a bootloader image is taken to be, and the cap on what [`to_boot_image`] returns.
///
/// This is the size of a stock `bootloader.dump`, so it is the right bound for "is this payload a bootloader or a
/// whole-disk image?". It is *not* what gets written to a boot hwpart — see [`BOOT_HWPART_BYTES`].
pub const BOOT_IMAGE_BYTES: usize = 4 * 1024 * 1024;

/// How much of a boot image is written to an eMMC boot hwpart.
///
/// `BOOT_SIZE_MULT` is factory-set per eMMC chip, and Car Things exist with both 4 MiB and 2 MiB boot hwparts. A
/// 4 MiB write to a 2 MiB part is rejected outright (`MMC: block number 0x1001 exceeds max(0x1000)`), so everything
/// is sized for the smaller one. Nothing is lost: an info sector plus a real bootloader comes to about 1.3 MiB, and
/// the rest of a stock dump is zero padding. Content past this bound is an error rather than a silent truncation.
pub const BOOT_HWPART_BYTES: usize = 2 * 1024 * 1024;

/// First bytes of the *stock* Car Thing BL2.
///
/// A landmark when reading hex dumps, and **not** usable as an "is this bare?" test: BL2 is encrypted, so this is
/// one build's first ciphertext block, not a magic. Differently-signed bootloaders share none of it.
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
/// The sector is a fixed shape — small header fields, zero padding, and a checksum of everything ahead of it — so
/// it can be recognised where the encrypted BL2 behind it cannot. An all-zero sector passes the same test.
///
/// This does *not* protect whole-disk images: `unbrick.bin`'s own LBA 0 is high-entropy and reads as bare. The
/// size bound in the caller is what keeps those from being shifted.
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

  /// An all-zero sector satisfies the same shape test, so it reads as prepared.
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

  /// A real bootloader plus its info sector fits a 2 MiB hwpart with room to spare; only padding is ever cut.
  #[test]
  fn a_real_bootloader_fits_the_smaller_hwpart() {
    let bootloader = crate::BOOTLOADER_BIN;
    assert!(bootloader.len() + INFO_SECTOR_BYTES < BOOT_HWPART_BYTES);

    let image = to_boot_image(bootloader);
    assert!(image[BOOT_HWPART_BYTES.min(image.len())..].iter().all(|&b| b == 0));
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
