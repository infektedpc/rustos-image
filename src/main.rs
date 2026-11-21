use rustos_image::{build_image, inspect_image, ImageOptions, PartitionScheme};
use std::path::PathBuf;
use std::process::ExitCode;

fn usage() -> &'static str {
    "rustos-image — build/inspect RustOS disk images\n\n\
     USAGE:\n  \
     rustos-image build --kernel <elf> --out <disk.img> [--size-mb 64] [--partition gpt|mbr]\n  \
     rustos-image inspect <disk.img>\n"
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| w[1].clone())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprint!("{}", usage());
        return ExitCode::from(2);
    }
    match args[1].as_str() {
        "build" => {
            let kernel = match flag_value(&args[2..], "--kernel") {
                Some(v) => PathBuf::from(v),
                None => {
                    eprintln!("error: build requires --kernel <elf>");
                    eprint!("{}", usage());
                    return ExitCode::from(2);
                }
            };
            let out = match flag_value(&args[2..], "--out") {
                Some(v) => PathBuf::from(v),
                None => {
                    eprintln!("error: build requires --out <disk.img>");
                    eprint!("{}", usage());
                    return ExitCode::from(2);
                }
            };
            let size_mb: u64 = flag_value(&args[2..], "--size-mb")
                .and_then(|v| v.parse().ok())
                .unwrap_or(64);
            let scheme = match flag_value(&args[2..], "--partition") {
                Some(v) => match PartitionScheme::parse(&v) {
                    Some(s) => s,
                    None => {
                        eprintln!("error: --partition must be gpt or mbr, got '{}'", v);
                        return ExitCode::from(2);
                    }
                },
                None => PartitionScheme::Gpt,
            };
            match build_image(&ImageOptions {
                kernel_path: kernel,
                output_path: out.clone(),
                size_mb,
                scheme,
            }) {
                Ok(info) => {
                    println!(
                        "built {} ({} bytes, {:?}, ESP LBA {} +{} sectors, {} {} bytes)",
                        out.display(),
                        info.size_bytes,
                        info.scheme,
                        info.esp_start_lba,
                        info.esp_sectors,
                        info.kernel_file_name,
                        info.kernel_size
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {}", e);
                    ExitCode::from(1)
                }
            }
        }
        "inspect" => {
            if args.len() < 3 {
                eprintln!("error: inspect requires <disk.img>");
                return ExitCode::from(2);
            }
            match inspect_image(&PathBuf::from(&args[2])) {
                Ok(rep) => {
                    println!("image: {} ({} bytes)", args[2], rep.size_bytes);
                    println!(
                        "scheme: {}",
                        match rep.scheme {
                            PartitionScheme::Gpt => "GPT",
                            PartitionScheme::Mbr => "MBR",
                        }
                    );
                    for p in &rep.partitions {
                        println!(
                            "  partition {}: LBA {}-{}  {}  bootable={}",
                            p.index, p.start_lba, p.end_lba, p.type_desc, p.bootable
                        );
                    }
                    println!("ESP files (LBA {}):", rep.esp_start_lba);
                    if rep.esp_files.is_empty() {
                        println!("  (none)");
                    }
                    for f in &rep.esp_files {
                        println!("  {}", f);
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {}", e);
                    ExitCode::from(1)
                }
            }
        }
        "-h" | "--help" | "help" => {
            print!("{}", usage());
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("error: unknown subcommand '{}'", other);
            eprint!("{}", usage());
            ExitCode::from(2)
        }
    }
}
