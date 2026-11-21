# rustos-image

Builds a bootable RustOS disk image from a kernel ELF/binary plus
bootloader files. Std-only — no external crates. The FAT16 writer and
the GPT/MBR writers are implemented in this crate (`src/lib.rs`).

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
and type, and the files in the ESP root directory.

## Limitations

- FAT16 only (no FAT32, no long filenames — 8.3 names only).
- One file is placed in the ESP: the kernel. No bootloader files
  yet — add them by extending `write_fat16` / `build_image`.
- No UEFI bootloader is bundled; the image is a correctly
  partitioned container ready for one.
