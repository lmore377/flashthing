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
| `writeUserArea` at LBA 0, up to 4 MiB                    | the same, with an info sector prepended if it lacks one          |
| `writeLargeMemory`                                       | the same, at the step's disk address ÷ 512                      |
| `writeBootPartition`                                     | `flash:mmc0boot0` / `flash:mmc0boot1`, then a hwpart reset      |
| `restorePartition`                                       | the stock partition's LBA range, or a GPT name if it isn't one  |
| `restorePartition` named `bootloader`                    | info sector + image, to user-area LBA 0 *and* both boot hwparts   |
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

- **A bootloader is preceded on disk by a 512-byte info sector**, so BL2 starts at LBA 1 where the mask ROM reads
  it. Written raw, a bare `bootloader.dump` lands a sector early and never boots. `restorePartition bootloader`,
  `writeBootPartition` and a `writeUserArea` at LBA 0 all build it for you — see
  [the bootloader docs](./bootloader.md).
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
- **Sparse erases first, then skips.** `--sparse` erases the whole-erase-group span of the target range and only
  then skips chunks that are entirely zero, because the erase has already put them where the image wants them. The
  result is identical to a non-sparse write; it just moves far less data. Partial erase groups at either end are
  left alone — erasing those would take neighbouring data with them — so chunks overlapping them are written
  normally. If the erase fails, skipping is abandoned and every chunk is written, so a zero in the image is never
  silently a no-op.

## In the browser

The wasm bindings drive the same transport over WebUSB, with two constraints that are easy to trip over.

**It needs a secure context.** `navigator.usb` is simply absent over plain HTTP, so serving the page at
`http://192.168.x.x:5173` looks identical to "this browser has no WebUSB". `http://localhost` *is* a secure context,
so local testing needs no TLS at all; for LAN testing, either front the dev server with a self-signed TLS proxy or
launch Chrome with `--unsafely-treat-insecure-origin-as-secure=http://<ip>:<port>`.

**The bootstrap costs two permission grants.** The mask ROM and the fastboot gadget are different USB identities, so
the grant the user gives for `1b8e:c003` does not carry over to `18d1:fada`. The device also reports no serial
number in the mask ROM, so Chromium can only hold that grant against the live connection and drops it the moment the
reset disconnects it. This is why `awaitGesture` is called with a reason: the second call is a `reconnect`, and the
page has to explain to the user why it is asking again. `WebUsb` polls `getDevices()` for a couple of seconds before
prompting, so a device that *is* already granted reconnects without bothering anyone.

A run that starts from the mask ROM therefore looks like:

```text
findingDevice -> deviceMode:usb -> connecting -> connected -> bl2Boot
   -> resetting -> connecting -> awaitGesture(reconnect) -> connected
```

## Throughput

Measured on a Car Thing running u-boot `2026.07-rc2`, writing the same 64 MiB of incompressible data to the same
LBA over both transports, release build, three runs each:

| transport                          | throughput          |
| ---------------------------------- | ------------------- |
| fastboot                           | 6.95 / 6.95 / 6.92 MiB/s |
| amlogic burn mode                  | 5.72 / 5.69 / 5.67 MiB/s |
| fastboot, sparse, all-zero payload | 64 MiB skipped in 0.1s   |

Fastboot is about 22% faster, and the reason is the round trips: burn mode stages a chunk into DRAM and then issues
a separate `mmc write` bulkcmd, while fastboot's `flash:` commits what was just downloaded. Both use 8 MiB chunks,
so it is otherwise like for like.

The bootstrap costs the same either way — 10.2s from the mask ROM to fastboot against 10.1s to vendor burn mode —
since both stream a bootloader of roughly the same size over the same AMLC handshake.

The sparse row is not really a throughput figure; it is the all-zero skip doing nothing at all. Burn mode has the
same trick, so it is not a fastboot win, but it is the difference between seconds and minutes on a mostly-empty
rootfs or an unbrick image.

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
