# Where the bootloader actually lives

A stock `bootloader.dump` is a bare bootloader image — signed BL2 first, then the FIP. That is **not** the form the
SoC expects to find on eMMC, and writing it raw at offset 0 produces a device that will not boot. This is the
single most expensive mistake to make when building a stock restore, because everything reads back byte-perfect
and the device just sits at a black screen.

## The info sector

Everywhere a bootloader lives on eMMC, the image is preceded by a 512-byte **info sector**, so that BL2 itself
begins at LBA 1. The mask ROM reads BL2 from **LBA 1, not LBA 0**.

```text
LBA 0     (offset 0)       info sector (storage_emmc_boot_info, 512 B)
LBA 1     (offset 0x200)   BL2, signed + encrypted (64 KiB)
LBA 129   (offset 0x10200) FIP header (16 KiB)
LBA 161   (offset 0x14200) per-entry headers, DDR firmware, RSA signatures
LBA 433+  (offset 0x36200) BL3X blob — exact offset varies per build, the FIP header points at it
```

The mask ROM tries LBA 0 first, fails its check, and retries at LBA 1. A single `CHK:1F` in the mask-ROM boot tag
is therefore **expected and normal**, not a symptom.

BL2 never reads the info sector — its only job is to occupy LBA 0 as a spacer. Zeroed fields boot, an all-zero
sector boots, and garbage with a deliberately wrong checksum boots. A well-formed one is written anyway because it
is free and keeps the image byte-compatible with vendor tooling. For a Car Thing it is all zeroes except:

| Offset  | Field           | Value                                                   |
| ------- | --------------- | ------------------------------------------------------- |
| `0x000` | `version`       | 1                                                        |
| `0x004` | `rsv_base_addr` | `0x12000` sectors — the amlogic reserved region at 36 MiB |
| `0x008` | `dtb.addr`      | 0 (vendor leaves this zero)                              |
| `0x00c` | `dtb.size`      | 0                                                        |
| `0x010` | `ddr.addr`      | `0x4000` sectors, relative to the reserved region        |
| `0x014` | `ddr.size`      | 4 sectors                                                |
| `0x1fc` | `checksum`      | wrapping sum of the u32s ahead of it — `0x16005` here    |

`flashthing::boot_image::info_sector()` builds exactly this, and `to_boot_image()` prepends it to a bare image
while passing an already-prepared one through untouched.

## Two copies, and which one boots

The same info-sector-plus-image layout exists in two places:

- **User-area LBA 0** — the mirror. On a Car Thing this is what actually boots.
- **boot0** (eMMC hwpart 1) — the backup.

Evidence for that ordering, measured on a device restored from a fully zeroed eMMC: it boots with **boot1 blank**
and EXT_CSD `PARTITION_CONFIG = 0x00`, so nothing is pointing the ROM at a boot hwpart at all. Amlogic's own
documentation frames the hwparts as primary and the user-area mirror as "BL2 fallback path 2"; on this hardware
the observed behaviour is the other way round.

Vendor u-boot writes both copies when it handles `amlmmc write bootloader`, and leaves boot1 alone. A restore
should do the same.

## EXT_CSD `PARTITION_CONFIG`

Byte 179 selects which boot hwpart the mask ROM reads. Stock Car Things ship `0x50` — boot ack on, boot1, user
access — and it has been seen resetting to `0` across a power cycle.

It is **not** needed if the user-area mirror is intact; a restored device boots fine at `0x00`. It only matters
for a restore that populates the boot hwparts alone. Reading or setting it needs a u-boot built with
`CONFIG_SUPPORT_EMMC_BOOT`, since `mmc partconf` is otherwise not compiled in.

## What a stock restore has to write

In order:

1. The stock partition payloads at their stock LBAs.
2. The amlogic MPT at **LBA 73728** (`MPT\0`, 18 entries on stock). `unbrick.bin` spans LBA 0–125000 and carries
   it, so a separate `mpt.bin` is redundant if you write that first.
3. The bootloader **last**, as an info sector plus the image, to user-area LBA 0 *and* boot0.

Writing the bootloader last matters: a failure partway through then leaves a device that falls into the mask ROM
rather than one that half-boots.

## Doing it in flashthing

`restorePartition` with the name `bootloader` handles all of this. It is deliberately not a plain partition
write — it builds the on-disk image and lays down both copies:

```json
{ "type": "restorePartition", "value": { "name": "bootloader", "data": { "filePath": "bootloader.dump" } } }
```

`writeBootPartition` remains the low-level escape hatch for writing one specific hwpart, and also accepts either
form:

```json
{ "type": "writeBootPartition", "value": { "hwpart": 1, "data": { "filePath": "stock-boot-partition.bin" } } }
```

Both run the payload through `to_boot_image()`, which decides what to do by looking at the first bytes: a bare
image starts with the encrypted BL2 header, an already-prepared one starts with an info sector. So a recipe can
carry whichever file it happens to have.

The result is capped at 4 MiB, the eMMC boot hwpart size on a Car Thing. That only discards trailing padding —
real content is around 1.3 MiB. **Caveat:** `BOOT_SIZE_MULT` is factory-set per eMMC chip and 2 MiB variants
exist in the wild, where a 4 MiB write would be rejected with `MMC: block number 0x1001 exceeds max(0x1000)`.
Flashthing does not currently detect this.

## Gotchas worth knowing

- **A flaky USB port looks exactly like a corrupt eMMC.** The Car Thing's port is notoriously unreliable, and a
  bad one makes the AMLC handshake die mid-sequence with `error reading ack: Input/Output Error`, which reads as
  "BL2 rejected the payload" when it is really just the cable. Worse, a failed AMLC attempt leaves the mask ROM
  answering no control transfers at all until it is physically replugged — so the symptom persists after the port
  is fine again. If the bootstrap fails, **move ports before changing anything on disk**; check which bus the
  device enumerated on and compare against a run that worked.
- **`mmc erase` is TRIM, not zero.** For confirmed zeros, write zeroes explicitly.
- **The emmckey enclave at LBA 73760–74271** refuses vendor `amlmmc` erases with `Emmckey: Access range is
  illegal!` until `amlmmc key` un-protects it. Mainline u-boot's `mmc erase` is not subject to this.
- **Only a cold power cycle proves a boot chain.** A warm `reset` with USB attached lands in the mask ROM
  regardless of what is on eMMC, so it can never tell you whether a restore worked.
