//! rustos-image: build a disk image from a kernel file, and inspect one.
//!
//! NOTE on scope: `build_image` writes a kernel-only ESP (just the
//! kernel file in the ESP root). That is a correctly partitioned
//! container, but it is NOT UEFI-bootable on its own — OVMF finds no
//! `efi/boot/bootx64.efi` and drops to the UEFI shell. The bootable
//! image comes from `rustos-bootloader` (see the rustos-build README
//! "Boot flow"). This crate is the low-level GPT/FAT machinery plus
//! the inspector, whose FAT reader walks the whole ESP tree
//! (subdirectories, FAT12/16/32, long file names).
//!
//! Std-only. Implements, by hand:
//! * CRC-32/IEEE (for GPT headers and partition-entry arrays)
//! * Protective MBR + GPT (primary + backup), or a plain MBR
//! * A minimal FAT16 writer for the EFI System Partition (ESP)

use std::fs;
use std::path::{Path, PathBuf};

pub const SECTOR_SIZE: usize = 512;
pub const ESP_START_LBA: u64 = 2048; // 1 MiB alignment
pub const GPT_ENTRIES_LBA: u64 = 2;
pub const GPT_NUM_ENTRIES: u64 = 128;
pub const GPT_ENTRY_SIZE: u64 = 128;
pub const GPT_FIRST_USABLE_LBA: u64 = 34;

/// EFI System Partition type GUID, stored in GPT on-disk byte order.
pub const ESP_TYPE_GUID: [u8; 16] = [
    0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9, 0x3b,
];
/// Deterministic unique GUID for the ESP entry (not random: reproducible images).
pub const ESP_UNIQUE_GUID: [u8; 16] = [
    0x52, 0x55, 0x53, 0x54, 0x4f, 0x53, 0x2d, 0x45, 0x53, 0x50, 0x2d, 0x30, 0x30, 0x30, 0x30, 0x31,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionScheme {
    Gpt,
    Mbr,
}

impl PartitionScheme {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "gpt" => Some(PartitionScheme::Gpt),
            "mbr" => Some(PartitionScheme::Mbr),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ImageOptions {
    pub kernel_path: PathBuf,
    pub output_path: PathBuf,
    pub size_mb: u64,
    pub scheme: PartitionScheme,
}

impl Default for ImageOptions {
    fn default() -> Self {
        Self {
            kernel_path: PathBuf::from("kernel.efi"),
            output_path: PathBuf::from("disk.img"),
            size_mb: 64,
            scheme: PartitionScheme::Gpt,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ImageInfo {
    pub size_bytes: u64,
    pub total_sectors: u64,
    pub esp_start_lba: u64,
    pub esp_sectors: u64,
    pub kernel_file_name: String,
    pub kernel_size: u64,
    pub scheme: PartitionScheme,
}

#[derive(Debug, Clone)]
pub struct PartitionInfo {
    pub index: usize,
    pub start_lba: u64,
    pub end_lba: u64,
    pub type_desc: String,
    pub bootable: bool,
}

#[derive(Debug, Clone)]
pub struct ImageReport {
    pub scheme: PartitionScheme,
    pub size_bytes: u64,
    pub partitions: Vec<PartitionInfo>,
    pub esp_files: Vec<String>,
    pub esp_start_lba: u64,
}

/// CRC-32/IEEE (poly 0xEDB88320, init 0xFFFFFFFF, final xor).
pub fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    crc ^ 0xFFFF_FFFF
}

fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn put_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
fn get_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}
fn get_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}
fn get_u64(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        buf[off], buf[off + 1], buf[off + 2], buf[off + 3], buf[off + 4], buf[off + 5],
        buf[off + 6], buf[off + 7],
    ])
}

/// 8.3 name for the kernel inside the ESP, e.g. "KERNEL.EFI".
pub fn kernel_esp_name(kernel_path: &Path) -> String {
    match kernel_path.extension().and_then(|e| e.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("efi") => "KERNEL.EFI".to_string(),
        _ => "KERNEL.BIN".to_string(),
    }
}

fn fat83_name(name: &str) -> [u8; 11] {
    let mut out = [b' '; 11];
    let (stem, ext) = match name.split_once('.') {
        Some((s, e)) => (s, e),
        None => (name, ""),
    };
    for (i, b) in stem.bytes().take(8).enumerate() {
        out[i] = b.to_ascii_uppercase();
    }
    for (i, b) in ext.bytes().take(3).enumerate() {
        out[8 + i] = b.to_ascii_uppercase();
    }
    out
}

/// Write a FAT16 volume into `esp` (exactly `esp_sectors * 512` bytes),
/// containing a single file `name_83` with `file_data`.
/// Returns the number of sectors used by the FAT (for tests/inspection).
pub fn write_fat16(esp: &mut [u8], name: &str, file_data: &[u8]) -> Result<u16, String> {
    let total_sectors = (esp.len() / SECTOR_SIZE) as u64;
    if total_sectors < 64 {
        return Err("ESP too small for FAT16 (need >= 64 sectors)".to_string());
    }
    let sectors_per_cluster: u16 = if total_sectors < 32768 { 1 } else { 4 };
    let reserved: u16 = 1;
    let num_fats: u8 = 2;
    let root_entries: u16 = 512;
    let root_dir_sectors: u64 = ((root_entries as u64 * 32) + SECTOR_SIZE as u64 - 1) / SECTOR_SIZE as u64;
    // Iterate FAT size to a fixed point.
    let mut fat_sectors: u64 = 16;
    for _ in 0..4 {
        let data_sectors = total_sectors
            .saturating_sub(reserved as u64 + num_fats as u64 * fat_sectors + root_dir_sectors);
        let clusters = data_sectors / sectors_per_cluster as u64;
        let need = ((clusters + 2) * 2 + SECTOR_SIZE as u64 - 1) / SECTOR_SIZE as u64;
        if need == fat_sectors {
            break;
        }
        fat_sectors = need.max(1);
    }
    let clusters_needed = ((file_data.len() as u64
        + (sectors_per_cluster as u64 * SECTOR_SIZE as u64) - 1)
        / (sectors_per_cluster as u64 * SECTOR_SIZE as u64))
        .max(1);
    let data_sectors = total_sectors
        .saturating_sub(reserved as u64 + num_fats as u64 * fat_sectors + root_dir_sectors);
    let total_clusters = data_sectors / sectors_per_cluster as u64;
    if clusters_needed > total_clusters {
        return Err(format!(
            "kernel ({} bytes, {} clusters) does not fit in ESP ({} clusters)",
            file_data.len(),
            clusters_needed,
            total_clusters
        ));
    }

    // --- Boot sector / BPB ---
    let mut bs = vec![0u8; SECTOR_SIZE];
    bs[0] = 0xEB;
    bs[1] = 0x3C;
    bs[2] = 0x90;
    bs[3..11].copy_from_slice(b"RUSTOS  ");
    put_u16(&mut bs, 11, SECTOR_SIZE as u16);
    bs[13] = sectors_per_cluster as u8;
    put_u16(&mut bs, 14, reserved);
    bs[16] = num_fats;
    put_u16(&mut bs, 17, root_entries);
    if total_sectors < 65535 {
        put_u16(&mut bs, 19, total_sectors as u16);
    } else {
        put_u32(&mut bs, 32, total_sectors as u32);
    }
    bs[21] = 0xF8; // fixed-disk media
    put_u16(&mut bs, 22, fat_sectors as u16);
    put_u16(&mut bs, 24, 63); // sectors/track
    put_u16(&mut bs, 26, 255); // heads
    put_u32(&mut bs, 28, ESP_START_LBA as u32);
    bs[36] = 0x80;
    bs[38] = 0x29; // extended boot signature
    put_u32(&mut bs, 39, 0x5255_5354); // volume id "RUST"
    bs[43..54].copy_from_slice(b"RUSTOS     ");
    bs[54..62].copy_from_slice(b"FAT16   ");
    bs[510] = 0x55;
    bs[511] = 0xAA;
    esp[..SECTOR_SIZE].copy_from_slice(&bs);

    // --- FATs: cluster chain 2 .. 2+clusters_needed-1, then EOC ---
    let fat_off = reserved as usize * SECTOR_SIZE;
    let fat_bytes = fat_sectors as usize * SECTOR_SIZE;
    for fat_idx in 0..num_fats as usize {
        let base = fat_off + fat_idx * fat_bytes;
        let mut fat = vec![0u8; fat_bytes];
        put_u16(&mut fat, 0, 0xFFF8);
        put_u16(&mut fat, 2, 0xFFFF);
        for c in 0..clusters_needed {
            let cluster = 2 + c;
            let next: u16 = if c + 1 == clusters_needed {
                0xFFFF
            } else {
                (cluster + 1) as u16
            };
            put_u16(&mut fat, (cluster as usize) * 2, next);
        }
        esp[base..base + fat_bytes].copy_from_slice(&fat);
    }

    // --- Root directory: one entry for the kernel ---
    let root_off = (reserved as usize + num_fats as usize * fat_sectors as usize) * SECTOR_SIZE;
    let mut entry = [0u8; 32];
    entry[0..11].copy_from_slice(&fat83_name(name));
    entry[11] = 0x20; // archive
    put_u16(&mut entry, 26, 2); // first cluster
    put_u32(&mut entry, 28, file_data.len() as u32);
    esp[root_off..root_off + 32].copy_from_slice(&entry);

    // --- Data area ---
    let data_off = root_off + root_dir_sectors as usize * SECTOR_SIZE;
    esp[data_off..data_off + file_data.len()].copy_from_slice(file_data);
    Ok(fat_sectors as u16)
}

fn write_protective_mbr(img: &mut [u8], total_sectors: u64) {
    let mut mbr = vec![0u8; SECTOR_SIZE];
    // One partition entry at 446: type 0xEE, LBA 1, size min(total-1, u32::MAX)
    let off = 446;
    mbr[off] = 0x00;
    mbr[off + 1] = 0x02; // CHS start (approx)
    mbr[off + 2] = 0x00;
    mbr[off + 3] = 0x00;
    mbr[off + 4] = 0xEE;
    mbr[off + 5] = 0xFF;
    mbr[off + 6] = 0xFF;
    mbr[off + 7] = 0xFF;
    put_u32(&mut mbr, off + 8, 1);
    put_u32(&mut mbr, off + 12, (total_sectors - 1).min(u32::MAX as u64) as u32);
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    img[..SECTOR_SIZE].copy_from_slice(&mbr);
}

fn write_mbr_partition(img: &mut [u8], esp_sectors: u64) {
    let mut mbr = vec![0u8; SECTOR_SIZE];
    let off = 446;
    mbr[off] = 0x80; // bootable
    mbr[off + 4] = 0x0E; // FAT16 LBA
    put_u32(&mut mbr, off + 8, ESP_START_LBA as u32);
    put_u32(&mut mbr, off + 12, esp_sectors.min(u32::MAX as u64) as u32);
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    img[..SECTOR_SIZE].copy_from_slice(&mbr);
}

fn write_gpt(img: &mut [u8], total_sectors: u64, esp_start: u64, esp_end: u64) {
    let entries_bytes = (GPT_NUM_ENTRIES * GPT_ENTRY_SIZE) as usize;
    let mut entries = vec![0u8; entries_bytes];
    // Entry 0: ESP
    entries[0..16].copy_from_slice(&ESP_TYPE_GUID);
    entries[16..32].copy_from_slice(&ESP_UNIQUE_GUID);
    put_u64(&mut entries, 32, esp_start);
    put_u64(&mut entries, 40, esp_end);
    put_u64(&mut entries, 48, 0); // attributes
    let name_utf16: Vec<u8> = "EFI System"
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    entries[56..56 + name_utf16.len()].copy_from_slice(&name_utf16);
    let entries_crc = crc32_ieee(&entries);
    // Primary entries at LBA 2..33
    let eoff = GPT_ENTRIES_LBA as usize * SECTOR_SIZE;
    img[eoff..eoff + entries_bytes].copy_from_slice(&entries);
    // Backup entries at last-32 .. last-1
    let boff = (total_sectors - 33) as usize * SECTOR_SIZE;
    img[boff..boff + entries_bytes].copy_from_slice(&entries);

    let make_header = |my_lba: u64, alt_lba: u64, part_lba: u64| -> Vec<u8> {
        let mut h = vec![0u8; SECTOR_SIZE];
        h[0..8].copy_from_slice(b"EFI PART");
        put_u32(&mut h, 8, 0x0001_0000);
        put_u32(&mut h, 12, 92);
        // CRC at 16 filled in below
        put_u32(&mut h, 20, 0); // reserved
        put_u64(&mut h, 24, my_lba);
        put_u64(&mut h, 32, alt_lba);
        put_u64(&mut h, 40, GPT_FIRST_USABLE_LBA);
        put_u64(&mut h, 48, total_sectors - 34);
        h[56..72].copy_from_slice(&ESP_UNIQUE_GUID);
        put_u64(&mut h, 72, part_lba);
        put_u32(&mut h, 80, GPT_NUM_ENTRIES as u32);
        put_u32(&mut h, 84, GPT_ENTRY_SIZE as u32);
        put_u32(&mut h, 88, entries_crc);
        let crc = crc32_ieee(&h[..92]);
        put_u32(&mut h, 16, crc);
        h
    };
    let primary = make_header(1, total_sectors - 1, GPT_ENTRIES_LBA);
    img[SECTOR_SIZE..2 * SECTOR_SIZE].copy_from_slice(&primary);
    let backup = make_header(total_sectors - 1, 1, total_sectors - 33);
    let boff_h = (total_sectors - 1) as usize * SECTOR_SIZE;
    img[boff_h..boff_h + SECTOR_SIZE].copy_from_slice(&backup);
}

/// Build the image described by `opts`. Returns layout info.
pub fn build_image(opts: &ImageOptions) -> Result<ImageInfo, String> {
    if opts.size_mb < 16 {
        return Err(format!(
            "size-mb must be >= 16, got {} (need room for ESP + backup GPT)",
            opts.size_mb
        ));
    }
    let kernel = fs::read(&opts.kernel_path)
        .map_err(|e| format!("cannot read kernel {}: {}", opts.kernel_path.display(), e))?;
    if kernel.is_empty() {
        return Err("kernel file is empty".to_string());
    }
    let total_sectors = opts.size_mb * 1024 * 1024 / SECTOR_SIZE as u64;
    let size_bytes = total_sectors * SECTOR_SIZE as u64;
    let esp_sectors = match opts.scheme {
        PartitionScheme::Gpt => total_sectors - ESP_START_LBA - 33,
        PartitionScheme::Mbr => total_sectors - ESP_START_LBA,
    };
    let esp_start = ESP_START_LBA;
    let esp_end = esp_start + esp_sectors - 1;
    let mut img = vec![0u8; size_bytes as usize];

    match opts.scheme {
        PartitionScheme::Gpt => {
            write_protective_mbr(&mut img, total_sectors);
            write_gpt(&mut img, total_sectors, esp_start, esp_end);
        }
        PartitionScheme::Mbr => write_mbr_partition(&mut img, esp_sectors),
    }
    let esp_off = esp_start as usize * SECTOR_SIZE;
    let esp_len = esp_sectors as usize * SECTOR_SIZE;
    let name = kernel_esp_name(&opts.kernel_path);
    write_fat16(&mut img[esp_off..esp_off + esp_len], &name, &kernel)?;

    if let Some(parent) = opts.output_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create output dir: {}", e))?;
        }
    }
    fs::write(&opts.output_path, &img)
        .map_err(|e| format!("cannot write {}: {}", opts.output_path.display(), e))?;
    Ok(ImageInfo {
        size_bytes,
        total_sectors,
        esp_start_lba: esp_start,
        esp_sectors,
        kernel_file_name: name,
        kernel_size: kernel.len() as u64,
        scheme: opts.scheme,
    })
}

/// Next cluster in a FAT12 chain: entries are packed 12-bit values,
/// two entries per three bytes.
fn fat12_next(fat: &[u8], cluster: u32) -> u32 {
    let off = cluster as usize + (cluster as usize) / 2;
    if off + 1 >= fat.len() {
        return 0xFFF; // out of range reads as end-of-chain
    }
    let b0 = fat[off] as u32;
    let b1 = fat[off + 1] as u32;
    if cluster % 2 == 0 {
        b0 | ((b1 & 0x0F) << 8)
    } else {
        (b0 >> 4) | (b1 << 4)
    }
}

/// FAT type, decided by cluster count (Microsoft's rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FatType {
    Fat12,
    Fat16,
    Fat32,
}

/// A read view over a FAT volume inside `img` (the ESP), for inspection.
/// Handles FAT12/16/32, subdirectory chains, and long file names (LFN).
struct FatView<'a> {
    img: &'a [u8],
    esp_off: usize,
    bytes_per_sector: u64,
    sectors_per_cluster: u64,
    reserved: u64,
    fats: u64,
    fat_sectors: u64,
    root_cluster: u32, // FAT32 only
    fat_type: FatType,
    root_dir_sectors: u64,
    first_data_sector: u64,
}

impl<'a> FatView<'a> {
    /// Parse the BPB at `esp_start` (in 512-byte sectors). `None` when
    /// the volume does not look like a FAT filesystem.
    fn parse(img: &'a [u8], esp_start: u64) -> Option<Self> {
        let off = esp_start as usize * SECTOR_SIZE;
        if off + SECTOR_SIZE > img.len() {
            return None;
        }
        let bs = &img[off..off + SECTOR_SIZE];
        let bytes_per_sector = get_u16(bs, 11) as u64;
        if bytes_per_sector == 0 || bytes_per_sector % SECTOR_SIZE as u64 != 0 {
            return None;
        }
        let sectors_per_cluster = bs[13] as u64;
        if sectors_per_cluster == 0 {
            return None;
        }
        let reserved = get_u16(bs, 14) as u64;
        let fats = bs[16] as u64;
        let root_entries = get_u16(bs, 17) as u64;
        let total_sectors = {
            let t16 = get_u16(bs, 19) as u64;
            if t16 != 0 { t16 } else { get_u32(bs, 32) as u64 }
        };
        let fat_sectors = {
            let f16 = get_u16(bs, 22) as u64;
            if f16 != 0 { f16 } else { get_u32(bs, 36) as u64 }
        };
        if fats == 0 || fat_sectors == 0 || total_sectors == 0 {
            return None;
        }
        let root_dir_sectors =
            ((root_entries * 32) + (bytes_per_sector - 1)) / bytes_per_sector;
        let first_data_sector = reserved + fats * fat_sectors + root_dir_sectors;
        if first_data_sector >= total_sectors {
            return None;
        }
        let clusters =
            (total_sectors - first_data_sector) / sectors_per_cluster;
        let fat_type = if clusters < 4085 {
            FatType::Fat12
        } else if clusters < 65525 {
            FatType::Fat16
        } else {
            FatType::Fat32
        };
        let root_cluster = if fat_type == FatType::Fat32 {
            get_u32(bs, 44)
        } else {
            0
        };
        Some(Self {
            img,
            esp_off: off,
            bytes_per_sector,
            sectors_per_cluster,
            reserved,
            fats,
            fat_sectors,
            root_cluster,
            fat_type,
            root_dir_sectors,
            first_data_sector,
        })
    }

    /// Byte offset of a sector (relative to the start of `img`).
    fn sector_off(&self, sector: u64) -> usize {
        self.esp_off + sector as usize * self.bytes_per_sector as usize
    }

    /// Byte offset of the first byte of `cluster`'s data.
    fn cluster_off(&self, cluster: u32) -> usize {
        self.sector_off(
            self.first_data_sector + (cluster as u64 - 2) * self.sectors_per_cluster,
        )
    }

    /// The next cluster in `cluster`'s chain (first FAT copy).
    /// Returns `None` for end-of-chain / free / out of range.
    fn next_cluster(&self, cluster: u32) -> Option<u32> {
        let fat_off = self.sector_off(self.reserved);
        let (eoc, mask): (u32, u32) = match self.fat_type {
            FatType::Fat12 => (0xFF8, 0xFFF),
            FatType::Fat16 => (0xFFF8, 0xFFFF),
            FatType::Fat32 => (0x0FFF_FFF8, 0x0FFF_FFFF),
        };
        let value = match self.fat_type {
            FatType::Fat12 => {
                let fat_off = self.sector_off(self.reserved);
                let fat = &self.img[fat_off..];
                fat12_next(fat, cluster)
            }
            FatType::Fat16 => {
                let off = fat_off + cluster as usize * 2;
                if off + 1 >= self.img.len() {
                    return None;
                }
                get_u16(self.img, off) as u32
            }
            FatType::Fat32 => {
                let off = fat_off + cluster as usize * 4;
                if off + 3 >= self.img.len() {
                    return None;
                }
                get_u32(self.img, off) & mask
            }
        };
        if value < 2 || value >= eoc {
            None
        } else {
            Some(value)
        }
    }

    /// Walk one directory — the root (FAT12/16: fixed region; FAT32:
    /// `root_cluster` chain) or a subdirectory chain starting at
    /// `first_cluster` — appending `(path, size)` for every file below
    /// `prefix` (subdirectories recurse, `.`/`..` skipped).
    fn walk_dir(
        &self,
        first_cluster: Option<u32>,
        prefix: &str,
        out: &mut Vec<(String, u32)>,
    ) {
        // Collect the directory's bytes: either the fixed root region
        // or the cluster chain, with a loop guard against corrupt FATs.
        let mut dir_bytes: Vec<u8> = Vec::new();
        match first_cluster {
            None => {
                // Fixed root directory (FAT12/16).
                let root_off = self.sector_off(self.reserved + self.fats * self.fat_sectors);
                let len = self.root_dir_sectors as usize * self.bytes_per_sector as usize;
                if root_off + len <= self.img.len() {
                    dir_bytes.extend_from_slice(&self.img[root_off..root_off + len]);
                }
            }
            Some(mut cluster) => {
                let mut seen = 0usize;
                while cluster >= 2 && seen < 1_000_000 {
                    seen += 1;
                    let off = self.cluster_off(cluster);
                    let len =
                        self.sectors_per_cluster as usize * self.bytes_per_sector as usize;
                    if off + len > self.img.len() {
                        break;
                    }
                    dir_bytes.extend_from_slice(&self.img[off..off + len]);
                    match self.next_cluster(cluster) {
                        Some(next) => cluster = next,
                        None => break,
                    }
                }
            }
        }

        // LFN entries precede their short entry; sequence numbers run
        // highest-first, so collect slots and join in sequence order.
        let mut lfn_slots: Vec<(u8, [u16; 13])> = Vec::new();
        let mut i = 0;
        while i + 32 <= dir_bytes.len() {
            let e = &dir_bytes[i..i + 32];
            i += 32;
            if e[0] == 0x00 {
                break; // end of directory
            }
            if e[0] == 0xE5 {
                lfn_slots.clear(); // deleted entry: drop any pending LFN
                continue;
            }
            let attr = e[11];
            if attr == 0x0F {
                // Long-name entry: 13 UCS-2 chars at 1..11, 14..26, 28..32.
                let seq = e[0] & 0x1F;
                let mut chars = [0u16; 13];
                for (j, &off) in [1usize, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30]
                    .iter()
                    .enumerate()
                {
                    chars[j] = u16::from_le_bytes([e[off], e[off + 1]]);
                }
                lfn_slots.push((seq, chars));
                continue;
            }
            if attr == 0x08 {
                lfn_slots.clear(); // volume label: not a file
                continue;
            }
            // Short entry: resolve the name (LFN wins when present).
            let name = match lfn_name(&lfn_slots) {
                Some(n) if !n.is_empty() => n,
                _ => short_name(e),
            };
            lfn_slots.clear();
            if name == "." || name == ".." {
                continue;
            }
            let first_cluster = get_u16(e, 26) as u32;
            let size = get_u32(e, 28);
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", prefix, name)
            };
            if attr & 0x10 != 0 {
                // Subdirectory: recurse (cluster 0 means empty dir).
                if first_cluster >= 2 {
                    self.walk_dir(Some(first_cluster), &path, out);
                }
            } else {
                out.push((path, size));
            }
        }
    }

    /// Every file in the volume, as `"path (N bytes)"` strings.
    fn list_files(&self) -> Vec<String> {
        let mut files = Vec::new();
        match self.fat_type {
            FatType::Fat32 => self.walk_dir(Some(self.root_cluster), "", &mut files),
            _ => self.walk_dir(None, "", &mut files),
        }
        files
            .iter()
            .map(|(p, s)| format!("{} ({} bytes)", p, s))
            .collect()
    }
}

/// Reassemble a long file name from collected LFN slots, or `None`
/// when there are none. Slots arrive highest-sequence-first.
fn lfn_name(slots: &[(u8, [u16; 13])]) -> Option<String> {
    if slots.is_empty() {
        return None;
    }
    let mut ordered = slots.to_vec();
    ordered.sort_by_key(|(seq, _)| *seq);
    let mut out = String::new();
    for (_, chars) in ordered {
        for &c in chars.iter() {
            if c == 0x0000 {
                return Some(out);
            }
            if c == 0xFFFF {
                continue;
            }
            if let Some(ch) = char::from_u32(c as u32) {
                out.push(ch);
            }
        }
    }
    Some(out)
}

/// 8.3 short name of a directory entry, e.g. `KERNEL.BIN`.
fn short_name(e: &[u8]) -> String {
    let stem = String::from_utf8_lossy(&e[0..8]).trim_end().to_string();
    let ext = String::from_utf8_lossy(&e[8..11]).trim_end().to_string();
    if ext.is_empty() {
        stem
    } else {
        format!("{}.{}", stem, ext)
    }
}

fn fat_root_files(img: &[u8], esp_start: u64) -> Vec<String> {
    // Despite the name (kept for the call sites): lists every file in
    // the ESP, recursing into subdirectories, with long names resolved.
    match FatView::parse(img, esp_start) {
        Some(view) => view.list_files(),
        None => Vec::new(),
    }
}

/// Parse an image produced by `build_image` (or a compatible one).
pub fn inspect_image(path: &Path) -> Result<ImageReport, String> {
    let img = fs::read(path).map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
    if img.len() < 2 * SECTOR_SIZE || img[510] != 0x55 || img[511] != 0xAA {
        return Err("not a rustos-image: missing MBR boot signature 0x55AA".to_string());
    }
    let _total_sectors = (img.len() / SECTOR_SIZE) as u64;
    // GPT?
    if img.len() >= 2 * SECTOR_SIZE && &img[SECTOR_SIZE..SECTOR_SIZE + 8] == b"EFI PART" {
        let hdr = &img[SECTOR_SIZE..2 * SECTOR_SIZE];
        let stored_crc = get_u32(hdr, 16);
        let mut hdr92 = hdr[..92].to_vec();
        hdr92[16..20].copy_from_slice(&[0, 0, 0, 0]);
        if crc32_ieee(&hdr92) != stored_crc {
            return Err("GPT header CRC32 mismatch".to_string());
        }
        let entries_lba = get_u64(hdr, 72);
        let num = get_u32(hdr, 80) as usize;
        let entry_size = get_u32(hdr, 84) as usize;
        let mut partitions = Vec::new();
        let mut esp_start = ESP_START_LBA;
        for i in 0..num {
            let eoff = entries_lba as usize * SECTOR_SIZE + i * entry_size;
            if eoff + entry_size > img.len() {
                break;
            }
            let e = &img[eoff..eoff + entry_size];
            if e[..16].iter().all(|&b| b == 0) {
                continue;
            }
            let start = get_u64(e, 32);
            let end = get_u64(e, 40);
            let is_esp = e[..16] == ESP_TYPE_GUID;
            if is_esp {
                esp_start = start;
            }
            partitions.push(PartitionInfo {
                index: i,
                start_lba: start,
                end_lba: end,
                type_desc: if is_esp {
                    "EFI System Partition".to_string()
                } else {
                    "unknown GPT partition".to_string()
                },
                bootable: false,
            });
        }
        let esp_files = fat_root_files(&img, esp_start);
        return Ok(ImageReport {
            scheme: PartitionScheme::Gpt,
            size_bytes: img.len() as u64,
            partitions,
            esp_files,
            esp_start_lba: esp_start,
        });
    }
    // Plain MBR: one entry at 446.
    let off = 446;
    let ptype = img[off + 4];
    let start = get_u32(&img, off + 8) as u64;
    let sectors = get_u32(&img, off + 12) as u64;
    let partitions = vec![PartitionInfo {
        index: 0,
        start_lba: start,
        end_lba: start + sectors.saturating_sub(1),
        type_desc: format!("MBR partition type 0x{:02X}", ptype),
        bootable: img[off] == 0x80,
    }];
    let esp_files = fat_root_files(&img, start);
    Ok(ImageReport {
        scheme: PartitionScheme::Mbr,
        size_bytes: img.len() as u64,
        partitions,
        esp_files,
        esp_start_lba: start,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_path(prefix: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("rustos-image-test-{}-{}-{}", std::process::id(), n, prefix))
    }

    fn dummy_kernel(name: &str, size: usize) -> PathBuf {
        let p = tmp_path(name);
        let mut data = Vec::with_capacity(size);
        for i in 0..size {
            data.push((i % 251) as u8);
        }
        // Fake MZ/ELF-ish header so the file is not all zeros.
        if size >= 4 {
            data[0] = 0x7F;
            data[1] = b'E';
            data[2] = b'L';
            data[3] = b'F';
        }
        fs::write(&p, &data).unwrap();
        p
    }

    #[test]
    fn crc32_known_vector() {
        assert_eq!(crc32_ieee(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32_ieee(b""), 0);
    }

    #[test]
    fn gpt_build_layout_and_signature() {
        let kernel = dummy_kernel("k.efi", 4096);
        let out = tmp_path("disk.img");
        let info = build_image(&ImageOptions {
            kernel_path: kernel.clone(),
            output_path: out.clone(),
            size_mb: 16,
            scheme: PartitionScheme::Gpt,
        })
        .unwrap();
        assert_eq!(info.size_bytes, 16 * 1024 * 1024);
        assert_eq!(info.esp_start_lba, 2048);
        assert_eq!(info.kernel_file_name, "KERNEL.EFI");
        let img = fs::read(&out).unwrap();
        // MBR signature
        assert_eq!((img[510], img[511]), (0x55, 0xAA));
        // Protective MBR type 0xEE
        assert_eq!(img[446 + 4], 0xEE);
        // GPT signature + header CRC validates
        assert_eq!(&img[512..520], b"EFI PART");
        let hdr = &img[512..1024];
        let stored = get_u32(hdr, 16);
        let mut h92 = hdr[..92].to_vec();
        h92[16..20].copy_from_slice(&[0, 0, 0, 0]);
        assert_eq!(crc32_ieee(&h92), stored);
        // Backup GPT header at last sector
        let last = (img.len() - SECTOR_SIZE)..img.len();
        assert_eq!(&img[last][..8], b"EFI PART");
        // ESP BPB: FAT16, bytes/sector 512, boot sig
        let esp = &img[2048 * SECTOR_SIZE..];
        assert_eq!(get_u16(esp, 11), 512);
        assert_eq!(&esp[54..62], b"FAT16   ");
        assert_eq!((esp[510], esp[511]), (0x55, 0xAA));
        let _ = fs::remove_file(&kernel);
        let _ = fs::remove_file(&out);
    }

    #[test]
    fn inspect_lists_kernel_in_esp() {
        let kernel = dummy_kernel("k2.bin", 8192);
        let out = tmp_path("disk2.img");
        build_image(&ImageOptions {
            kernel_path: kernel.clone(),
            output_path: out.clone(),
            size_mb: 16,
            scheme: PartitionScheme::Gpt,
        })
        .unwrap();
        let rep = inspect_image(&out).unwrap();
        assert_eq!(rep.scheme, PartitionScheme::Gpt);
        assert_eq!(rep.partitions.len(), 1);
        assert_eq!(rep.partitions[0].start_lba, 2048);
        assert_eq!(rep.esp_files.len(), 1);
        assert!(rep.esp_files[0].starts_with("KERNEL.BIN"), "{}", rep.esp_files[0]);
        assert!(rep.esp_files[0].contains("8192"), "{}", rep.esp_files[0]);
        let _ = fs::remove_file(&kernel);
        let _ = fs::remove_file(&out);
    }

    #[test]
    fn mbr_build_and_inspect() {
        let kernel = dummy_kernel("k3.efi", 1024);
        let out = tmp_path("disk3.img");
        build_image(&ImageOptions {
            kernel_path: kernel.clone(),
            output_path: out.clone(),
            size_mb: 16,
            scheme: PartitionScheme::Mbr,
        })
        .unwrap();
        let img = fs::read(&out).unwrap();
        assert_eq!((img[510], img[511]), (0x55, 0xAA));
        assert_eq!(img[446 + 4], 0x0E);
        let rep = inspect_image(&out).unwrap();
        assert_eq!(rep.scheme, PartitionScheme::Mbr);
        assert_eq!(rep.esp_files.len(), 1);
        assert!(rep.esp_files[0].starts_with("KERNEL.EFI"));
        let _ = fs::remove_file(&kernel);
        let _ = fs::remove_file(&out);
    }

    #[test]
    fn rejects_missing_kernel_and_tiny_image() {
        let out = tmp_path("disk4.img");
        let err = build_image(&ImageOptions {
            kernel_path: PathBuf::from("/nonexistent/kernel.efi"),
            output_path: out.clone(),
            size_mb: 64,
            scheme: PartitionScheme::Gpt,
        })
        .unwrap_err();
        assert!(err.contains("cannot read kernel"), "{}", err);
        let kernel = dummy_kernel("k4.efi", 100);
        let err = build_image(&ImageOptions {
            kernel_path: kernel.clone(),
            output_path: out.clone(),
            size_mb: 8,
            scheme: PartitionScheme::Gpt,
        })
        .unwrap_err();
        assert!(err.contains(">= 16"), "{}", err);
        let _ = fs::remove_file(&kernel);
    }

    #[test]
    fn inspect_rejects_garbage() {
        let p = tmp_path("garbage.img");
        fs::write(&p, vec![0u8; 4096]).unwrap();
        assert!(inspect_image(&p).is_err());
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn lfn_name_reassembles_sequences() {
        // "a-very-long-filename.txt" is 24 chars: two LFN entries, the
        // second sequence stored first on disk (highest-first order).
        let mk = |s: &str| -> [u16; 13] {
            let mut out = [0xFFFFu16; 13];
            for (i, c) in s.chars().enumerate() {
                out[i] = c as u16;
            }
            if s.len() < 13 {
                out[s.len()] = 0x0000;
            }
            out
        };
        let slots = vec![(2u8, mk("ilename.txt")), (1u8, mk("a-very-long-f"))];
        assert_eq!(
            lfn_name(&slots).as_deref(),
            Some("a-very-long-filename.txt")
        );
        // Exactly 13 chars fit in one entry.
        let single = vec![(1u8, mk("kernel-x86_64"))];
        assert_eq!(lfn_name(&single).as_deref(), Some("kernel-x86_64"));
        assert_eq!(lfn_name(&[]), None);
    }

    #[test]
    fn fat12_entry_decoding() {
        // FAT bytes: [F8 FF] [FF] then cluster 2 -> 3, 3 -> 4, 4 -> EOC.
        let fat = [0xF8u8, 0xFF, 0xFF, 0x03, 0x40, 0x00, 0xFF, 0x0F];
        assert_eq!(fat12_next(&fat, 2), 3);
        assert_eq!(fat12_next(&fat, 3), 4);
        assert_eq!(fat12_next(&fat, 4), 0xFFF);
    }

    #[test]
    fn inspect_lists_subdirectory_and_skips_volume_label() {
        // Build a normal single-file image, then hand-patch a
        // subdirectory + volume label into its FAT16 ESP.
        let kernel = dummy_kernel("k5.bin", 8192);
        let out = tmp_path("disk5.img");
        build_image(&ImageOptions {
            kernel_path: kernel.clone(),
            output_path: out.clone(),
            size_mb: 16,
            scheme: PartitionScheme::Gpt,
        })
        .unwrap();
        let mut img = fs::read(&out).unwrap();
        let esp = ESP_START_LBA as usize * SECTOR_SIZE;
        let bs = &img[esp..esp + SECTOR_SIZE];
        let reserved = get_u16(bs, 14) as usize;
        let fats = bs[16] as usize;
        let fat_sectors = get_u16(bs, 22) as usize;
        let root_off = esp + (reserved + fats * fat_sectors) * SECTOR_SIZE;
        let root_dir_sectors = 32usize; // 512 entries * 32 bytes
        let data_off = root_off + root_dir_sectors * SECTOR_SIZE;
        let cluster_off = |c: usize| data_off + (c - 2) * SECTOR_SIZE;
        let set_fat16 = |img: &mut [u8], cluster: usize, value: u16| {
            for f in 0..fats {
                let off = esp + (reserved + f * fat_sectors) * SECTOR_SIZE + cluster * 2;
                img[off..off + 2].copy_from_slice(&value.to_le_bytes());
            }
        };
        // Root slot 1 (after KERNEL.BIN): subdirectory "SUBDIR", cluster 18.
        let mut sub = [0u8; 32];
        sub[0..11].copy_from_slice(b"SUBDIR     ");
        sub[11] = 0x10;
        sub[26..28].copy_from_slice(&18u16.to_le_bytes());
        img[root_off + 32..root_off + 64].copy_from_slice(&sub);
        // Root slot 2: volume label — must be skipped by the listing.
        let mut label = [0u8; 32];
        label[0..11].copy_from_slice(b"MYLABEL    ");
        label[11] = 0x08;
        img[root_off + 64..root_off + 96].copy_from_slice(&label);
        // Cluster 18 holds the subdirectory: one file entry, then END.
        set_fat16(&mut img, 18, 0xFFFF);
        let mut inner = [0u8; 32];
        inner[0..11].copy_from_slice(b"INNER   TXT");
        inner[11] = 0x20;
        inner[26..28].copy_from_slice(&19u16.to_le_bytes());
        inner[28..32].copy_from_slice(&42u32.to_le_bytes());
        let c18 = cluster_off(18);
        img[c18..c18 + 32].copy_from_slice(&inner);
        set_fat16(&mut img, 19, 0xFFFF);
        let c19 = cluster_off(19);
        img[c19..c19 + 42].copy_from_slice(&[0xAB; 42]);

        let out2 = tmp_path("disk5b.img");
        fs::write(&out2, &img).unwrap();
        let rep = inspect_image(&out2).unwrap();
        assert!(
            rep.esp_files.iter().any(|f| f == "KERNEL.BIN (8192 bytes)"),
            "{:?}",
            rep.esp_files
        );
        assert!(
            rep.esp_files.iter().any(|f| f == "SUBDIR/INNER.TXT (42 bytes)"),
            "{:?}",
            rep.esp_files
        );
        assert!(
            !rep.esp_files.iter().any(|f| f.contains("MYLABEL")),
            "{:?}",
            rep.esp_files
        );
        let _ = fs::remove_file(&kernel);
        let _ = fs::remove_file(&out);
        let _ = fs::remove_file(&out2);
    }
}
