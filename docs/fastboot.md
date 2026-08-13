# Flashing over Fastboot

FlashThing can talk to a Car Thing two ways.

- **Amlogic burn mode** — the vendor protocol (`bulkcmd`, DRAM staging, the MPT partition table). This is what a
  stock device offers and it is still the default.
- **Fastboot** — standard Android fastboot, which is what our mainline u-boot offers.

Both accept the same `meta.json`. There is no second archive format: each step is translated into its fastboot
equivalent at run time.

## Using it

```bash
# flash an archive over fastboot instead of burn mode
flashthing-cli --fastboot ./firmware.zip

# skip all-zero chunks on every raw write (big win on mostly-empty images)
flashthing-cli --fastboot --sparse ./firmware.zip

# run a single u-boot command and print its output
flashthing-cli --console "mmc part"
```

```typescript
// node
const flasher = new FlashThing(callback, { fastboot: true, forceSparse: true });
await flasher.openArchive('firmware.zip');
await flasher.flash();
```

```typescript
// browser
const flasher = new FlashThing(onEvent, { readAll, open, awaitGesture, fastboot: true });
flasher.openJson(metaJson);
await flasher.flash();
```

On Linux, `flashthing-cli --setup` installs udev rules for all three of the device's USB identities. A host set up
before fastboot support existed only has the first two, and will fail when the device re-enumerates — rerun it.

## Getting to fastboot

The amlogic boot ROM is burned in silicon and speaks nothing but the vendor mask-ROM protocol, so a stock device has
to be met on its own terms exactly once:

1. The device is powered on holding buttons 1 & 4, so it enumerates as the mask ROM (`1b8e:c003`).
2. `superbird.bl2.encrypted.bin` is streamed into SRAM and started; it brings up DRAM.
3. BL2 asks for the rest of the bootloader over the AMLC handshake, and is handed `carthing.fip.bin` — a signed
   mainline u-boot — instead of amlogic's `superbird.bootloader.img`.
4. u-boot comes up in DRAM, notices it was booted over USB, and drops straight into the fastboot gadget
   (`18d1:fada`, advertised as "Superbird" by "Thing Labs").

Everything after that is fastboot. A device already in fastboot skips the whole bootstrap.

A device that is *already in vendor burn mode* cannot be moved to fastboot — it is past the point where the mask ROM
will accept a different bootloader. Power cycle it back into USB mode first.

`carthing.fip.bin` is a build artifact, not a source file. Rebuild it with `fip-tool sign` whenever the u-boot it
was cut from changes: the bootstrap RAM-boots whatever is committed, and a FIP older than the `oem console` and
`oem maskrom` commands will silently break the translation layer. Callers tracking their own u-boot should pass
their own bytes to `Fastboot::connect_with` rather than use the bundled one.

## How steps are translated

| Step                                                     | Fastboot equivalent                                            |
| -------------------------------------------------------- | -------------------------------------------------------------- |
| `writeUserArea`                                          | raw LBA write through a `fastboot_raw_partition_*` alias        |
| `writeLargeMemory`                                       | the same, at the step's disk address ÷ 512                      |
| `writeBootPartition`                                     | `flash:mmc0boot0` / `flash:mmc0boot1`, then a hwpart reset      |
| `restorePartition`                                       | the stock partition's LBA range, or a GPT name if it isn't one  |
| `writeEnv`                                               | download + `env import -t` + `saveenv`                          |
| `bulkcmd`, `bulkcmdStat`                                 | rewritten vendor command through `oem console`                  |
| `identify`                                               | `getvar:product` and `getvar:version-bootloader`                |
| `validatePartitionSize`                                  | the stock table, or `getvar:partition-size:<name>`              |
| `bl2Boot`, `run`, `writeSimpleMemory`, `writeAMLCData`, `getBootAMLC` | skipped — the bootstrap already happened at connect time |
| `readSimpleMemory`, `readLargeMemory`                    | rejected; there is no fastboot equivalent                       |

Raw writes deliberately go through `flash:` rather than `mmc write`. u-boot resolves an unknown flash target by
looking up `fastboot_raw_partition_<name>` = `"<start_lba> <sector_count>"`, so pointing a throwaway alias at a
sector range gets us its bounds checking and sparse-image handling for free, and costs one round trip per chunk
instead of two.

### Vendor command rewriting

`bulkcmd` steps carry vendor burn-mode u-boot commands, which mostly still make sense — with three differences:

- Burn mode numbers the eMMC as device 1; ours is device 0. `mmc dev 1` becomes `mmc dev 0 0`.
- `amlmmc` is amlogic's fork of `mmc`; `read`/`write`/`erase` have the same argument shape, so the prefix is
  rewritten.
- `amlmmc key`, `amlmmc part` and `disk_initial` set up an amlogic partition table and key enclave that our layout
  doesn't have. They are dropped.

Anything else is passed through unchanged. Note that `oem console` reports the console *drain* succeeding, not the
command: a u-boot command that fails still comes back `OKAY` with the complaint in its output, so the output is
scanned for it.

## Things worth knowing

- **Flashing a boot hwpart leaves it selected.** u-boot's `fb_mmc_boot_ops` never restores the hwpart, so anything
  touching the user area afterwards would land in a boot partition. Every `writeBootPartition` is followed by a
  `mmc dev 0 0`.
- **Commands are capped at 64 bytes** (`FASTBOOT_COMMAND_LEN`) and u-boot *truncates* rather than complaining, which
  would turn an over-long `setenv` into a silently wrong one. FlashThing refuses to send one instead. This is why
  the raw alias is two characters.
- **Chunks are capped at 8 MiB**, well under the device's 112 MiB download buffer. A chunk is a serialized round
  trip — upload, then a blocking `flash:` while the eMMC commits, reporting nothing for the second half — so larger
  chunks just mean a longer frozen progress bar. Total transfer time is unchanged either way.
- **`writeEnv` goes through the download buffer.** The text is downloaded and parsed in place with
  `env import -t 0x6000000 <size>`, which sidesteps the 64-byte limit a `setenv` per variable would keep hitting.
  `saveenv` is what persists it to `uboot.env` on the FAT `env` partition.
- **Sparse skips, it doesn't zero.** `--sparse` leaves whatever was already on the eMMC wherever the image is all
  zeroes. Only use it when the target range is erased or its previous contents don't matter.

## Hardware tests

`lib/tests/fastboot_hardware.rs` drives the transport against a real device. Every write is verified by having
u-boot `crc32` what actually landed, rather than trusting the `OKAY`.

The read-only tests — the bootstrap, `getvar`, the raw-alias round trip and the download handshake — need nothing
but an attached device:

```bash
cargo test --release --test fastboot_hardware -- --ignored --test-threads 1
```

The tests that commit to eMMC skip themselves unless they are pointed at space that can be trampled, so running
`--ignored` on an unprepared device does nothing:

```bash
FLASHTHING_SCRATCH_LBA=2793472 \
FLASHTHING_SCRATCH_PARTITION=bandaid \
  cargo test --release --test fastboot_hardware -- --ignored --test-threads 1
```

- `FLASHTHING_SCRATCH_LBA` — an LBA with 20 MiB of unused space after it. Unallocated space past the last GPT
  partition is the usual choice; `mmc part` will show you where that starts.
- `FLASHTHING_SCRATCH_PARTITION` — a partition whose first 4 MiB can be overwritten.

Both targets are read into DRAM before anything is written and put back afterwards, including when an assertion
fails, so a scratch range that isn't empty still survives. `--test-threads 1` is required: they share one device.
