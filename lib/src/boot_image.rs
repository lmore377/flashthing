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

/// Leading bytes shared by every signed amlogic bootloader image we handle — the encrypted BL2 header.
///
/// `superbird.bl2.encrypted.bin`, `superbird.bootloader.img` and a stock `bootloader.dump` all begin with these,
/// which is what makes it a usable "is this a bare image?" test.
const BL2_SIGNATURE: [u8; 8] = [0x0c, 0x62, 0x7a, 0x15, 0xbe, 0x94, 0x07, 0xb2];

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

/// Whether `data` is a bare bootloader image that still needs an info sector in front of it.
pub fn needs_info_sector(data: &[u8]) -> bool {
  data.starts_with(&BL2_SIGNATURE)
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

  #[test]
  fn bare_images_get_an_info_sector() {
    let mut bare = BL2_SIGNATURE.to_vec();
    bare.extend_from_slice(&[0xab; 64]);

    let image = to_boot_image(&bare);
    assert_eq!(&image[..INFO_SECTOR_BYTES], &info_sector());
    assert_eq!(&image[INFO_SECTOR_BYTES..], &bare[..]);
  }

  #[test]
  fn prepared_images_are_left_alone() {
    let mut prepared = info_sector().to_vec();
    prepared.extend_from_slice(&BL2_SIGNATURE);

    assert_eq!(to_boot_image(&prepared), prepared);
  }

  #[test]
  fn oversized_images_are_capped() {
    let mut bare = BL2_SIGNATURE.to_vec();
    bare.resize(BOOT_IMAGE_BYTES, 0);

    assert_eq!(to_boot_image(&bare).len(), BOOT_IMAGE_BYTES);
  }
}
