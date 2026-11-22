# rustos-image

Builds a RustOS disk image from a kernel ELF/binary. Std-only — no
external crates. The FAT16 writer and the GPT/MBR writers are
implemented in this crate (`src/lib.rs`).

> **Honest scope:** `build_image` writes a kernel-only ESP — just
> `KERNEL.EFI` (or `KERNEL.BIN`) in the ESP root. That is a correctly
> partitioned container, but it is **not UEFI-bootable on its own**:
> OVMF finds no `efi/boot/bootx64.efi` and drops to the UEFI shell.
> The bootable image comes from `rustos-bootloader`, which embeds
> the bootloader crate's UEFI stage (`efi/boot/bootx64.efi`) plus the
> kernel (`kernel-x86_64`) and `boot.json` — see the rustos-build
> README "Boot flow". Use `rustos-image` for the low-level GPT/FAT
> machinery and for inspection, not as the boot path.

## Layout (default, GPT, 64 MiB)

| Region | LBA | Contents |
|---|---|---|
| Protective MBR | 0 | Partition type `0xEE`, boot signature `0x55AA` |
| GPT header | 1 | `EFI PART`, CRC32-checked |
| GPT entries | 2–33 | 128 entries, entry 0 = EFI System Partition |
| ESP (FAT16) | 2048–… | `KERNEL.EFI` (or `KERNEL.BIN`) |
| Backup GPT | last 33 sectors | Copy of entries + header |

MBR mode (`--partition mbr`) writes a single `0x0E` (FAT16 LBA)
partition at LBA 2048 instead of GPT.

## Build / test

```sh
cargo test
cargo build --release
```

## Usage

```sh
# Build a 64 MiB GPT image (default) from a kernel file
./target/debug/rustos-image build --kernel kernel.efi --out disk.img --size-mb 64

# MBR instead of GPT
./target/debug/rustos-image build --kernel kernel.bin --out disk.img --size-mb 32 --partition mbr

# Inspect an image: partitions + ESP files
./target/debug/rustos-image inspect disk.img
```

`build` requires `--kernel` (must exist) and `--out`. `--size-mb`
defaults to 64 and must be >= 16 (large enough for a 1 MiB-aligned
ESP plus backup GPT). The kernel is copied into the ESP as
`KERNEL.EFI` (for `.efi` input) or `KERNEL.BIN` (anything else).
`inspect` prints the partition scheme, each partition's LBA range
and type, and every file in the ESP — recursing into subdirectories
(FAT12/16/32, long file names resolved, volume labels skipped), so
it also lists the contents of a bootloader-built image
(`efi/boot/bootx64.efi`, `kernel-x86_64`, `boot.json`).

## Limitations

- FAT16 writer only (no FAT32 writer, no long filenames — 8.3
  names only when *writing*). The *reader* (`inspect`) handles
  FAT12/16/32, subdirectories, and long file names.
- One file is placed in the ESP by `build_image`: the kernel. No
  bootloader files — a `build_image` disk does not boot under UEFI
  (see the note at the top). The bootable path is
  `rustos-bootloader`.
- No UEFI bootloader is bundled here.
