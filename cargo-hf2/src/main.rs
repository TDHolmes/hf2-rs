use colored::*;
use crc_any::CRCu16;
use goblin::elf::program_header::*;
use hidapi::{HidApi, HidDevice};

use maplit::hashmap;
use std::{
    fs::File,
    io::Read,
    path::PathBuf,
    process::{Command, Stdio},
    time::Instant,
};
use structopt::StructOpt;

fn main() {
    // Initialize the logging backend.
    pretty_env_logger::init();

    // Get commandline options.
    // Skip the first arg which is the calling application name.
    let opt = Opt::from_iter(std::env::args().skip(1));

    // Try and get the cargo project information.
    let project = cargo_project::Project::query(".").expect("Couldn't parse the Cargo.toml");

    // Decide what artifact to use.
    let artifact = if let Some(bin) = &opt.bin {
        cargo_project::Artifact::Bin(bin)
    } else if let Some(example) = &opt.example {
        cargo_project::Artifact::Example(example)
    } else {
        cargo_project::Artifact::Bin(project.name())
    };

    // Decide what profile to use.
    let profile = if opt.release {
        cargo_project::Profile::Release
    } else {
        cargo_project::Profile::Dev
    };

    // Try and get the artifact path.
    let path = project
        .path(
            artifact,
            profile,
            opt.target.as_deref(),
            "x86_64-unknown-linux-gnu",
        )
        .expect("Couldn't find the build result");

    // Remove first two args which is the calling application name and the `hf2` command from cargo.
    let mut args: Vec<_> = std::env::args().skip(2).collect();

    // todo, keep as iter. difficult because we want to filter map remove two items at once.
    // Remove our args as cargo build does not understand them.
    let flags = ["--pid", "--vid"].iter();
    for flag in flags {
        if let Some(index) = args.iter().position(|x| x == flag) {
            args.remove(index);
            args.remove(index);
        }
    }

    let status = Command::new("cargo")
        .arg("build")
        .args(args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap()
        .wait()
        .unwrap();

    if !status.success() {
        exit_with_process_status(status)
    }

    let api = HidApi::new().expect("Couldn't find system usb");

    let d = if let (Some(v), Some(p)) = (opt.vid, opt.pid) {
        api.open(v, p)
            .expect("Are you sure device is plugged in and in bootloader mode?")
    } else {
        println!(
            "    {} for a connected device with known vid/pid pair.",
            "Searching".green().bold(),
        );

        let mut device: Option<HidDevice> = None;

        let vendor = hashmap! {
            0x1D50 => vec![0x6110, 0x6112],
            0x239A => vec![0x0035, 0x002D, 0x0015, 0x001B, 0xB000, 0x0024, 0x000F, 0x0013, 0x0021, 0x0022, 0x0031, 0x002B, 0x0037, 0x0035, 0x002F, 0x002B, 0x0033, 0x0034, 0x003D, 0x0018, 0x001C, 0x001E, 0x0027, 0x0022],
            0x04D8 => vec![0xEDB3, 0xEDBE, 0xEF66],
            0x2341 => vec![0x024E, 0x8053, 0x024D],
            0x16D0 => vec![0x0CDA],
            0x03EB => vec![0x2402],
            0x2886 => vec![0x000D, 0x002F],
            0x1B4F => vec![0x0D23, 0x0D22],
            0x1209 => vec![0x4D44, 0x2017],
        };

        for device_info in api.device_list() {
            if let Some(products) = vendor.get(&device_info.vendor_id()) {
                if products.contains(&device_info.product_id()) {
                    if let Ok(d) = device_info.open_device(&api) {
                        device = Some(d);
                        break;
                    }
                }
            }
        }
        device.expect("Are you sure device is plugged in and in bootloader mode?")
    };

    println!(
        "    {} {:?} {:?}",
        "Trying ".green().bold(),
        d.get_manufacturer_string(),
        d.get_product_string()
    );

    println!("    {} {:?}", "Flashing".green().bold(), path);

    let (binary, address) = elf_to_bin(path);

    // Start timer.
    let instant = Instant::now();

    let bininfo = hf2::bin_info(&d).expect("bin_info failed");
    log::debug!("{:?}", bininfo);

    flash_bin(&binary, address, &bininfo, &d);

    // Stop timer.
    let elapsed = instant.elapsed();
    println!(
        "    {} in {}s",
        "Finished".green().bold(),
        elapsed.as_millis() as f32 / 1000.0
    );
}

#[cfg(unix)]
fn exit_with_process_status(status: std::process::ExitStatus) -> ! {
    use std::os::unix::process::ExitStatusExt;
    let status = status.code().or_else(|| status.signal()).unwrap_or(1);
    std::process::exit(status)
}

#[cfg(not(unix))]
fn exit_with_process_status(status: std::process::ExitStatus) -> ! {
    let status = status.code().unwrap_or(1);
    std::process::exit(status)
}

/// Returns a contiguous bin with 0s between non-contiguous sections and starting address from an elf.
fn elf_to_bin(path: PathBuf) -> (Vec<u8>, u32) {
    let mut file = File::open(path).unwrap();
    let mut buffer = vec![];
    file.read_to_end(&mut buffer).unwrap();

    let binary = goblin::elf::Elf::parse(&buffer.as_slice()).expect("Couldn't parse elf");

    // we need to fill any noncontigous section space with zeros to send over to uf2 bootloader in one batch (for some reason)
    // todo this is a mess
    let (data, _, start_address) = binary
        .program_headers
        .iter()
        .filter(|ph| ph.p_type == PT_LOAD && ph.p_filesz > 0)
        .fold(
            (vec![], 0x0, 0x0),
            move |(mut data, last_address, start_address), ph| {
                log::debug!("{:?}", ph);

                let current_address = ph.p_filesz + ph.p_paddr;

                //first time through we dont want any of the padding zeros and we want to set the starting address
                if data.is_empty() {
                    data.extend_from_slice(&buffer[ph.p_offset as usize..][..ph.p_filesz as usize]);

                    (data, current_address, ph.p_paddr)
                }
                //other times through pad any space between sections and maintain the starting address
                else {
                    for _ in 0..(current_address - last_address) {
                        data.push(0x0);
                    }

                    data.extend_from_slice(&buffer[ph.p_offset as usize..][..ph.p_filesz as usize]);

                    (data, current_address, start_address)
                }
            },
        );

    (data, start_address as u32)
}

/// Flash, Verify and restart into app.
fn flash_bin(binary: &[u8], address: u32, bininfo: &hf2::BinInfoResponse, d: &HidDevice) {
    assert!(!binary.is_empty(), "Elf has nothing to flash?");

    let mut binary = binary.to_owned();

    //pad zeros to page size
    let padded_num_pages = (binary.len() as f64 / f64::from(bininfo.flash_page_size)).ceil() as u32;
    let padded_size = padded_num_pages * bininfo.flash_page_size;
    log::debug!(
        "binary is {} bytes, padding to {} bytes",
        binary.len(),
        padded_size
    );
    for _i in 0..(padded_size as usize - binary.len()) {
        binary.push(0x0);
    }

    if bininfo.mode != hf2::BinInfoMode::Bootloader {
        let _ = hf2::start_flash(&d).expect("start_flash failed");
    }
    flash(&binary, address, &bininfo, &d);
    verify(&binary, address, &bininfo, &d);
    let _ = hf2::reset_into_app(&d).expect("reset_into_app failed");
}

/// Flashes binary writing a single page at a time.
fn flash(binary: &[u8], address: u32, bininfo: &hf2::BinInfoResponse, d: &HidDevice) {
    for (page_index, page) in binary.chunks(bininfo.flash_page_size as usize).enumerate() {
        let target_address = address + bininfo.flash_page_size * page_index as u32;

        let _ = hf2::write_flash_page(&d, target_address, page.to_vec())
            .expect("write_flash_page failed");
    }
}

/// Verifys checksum of binary.
fn verify(binary: &[u8], address: u32, bininfo: &hf2::BinInfoResponse, d: &HidDevice) {
    // get checksums of existing pages
    let top_address = address + binary.len() as u32;
    let max_pages = bininfo.max_message_size / 2 - 2;
    let steps = max_pages * bininfo.flash_page_size;
    let mut device_checksums = vec![];

    for target_address in (address..top_address).step_by(steps as usize) {
        let pages_left = (top_address - target_address) / bininfo.flash_page_size;

        let num_pages = if pages_left < max_pages {
            pages_left
        } else {
            max_pages
        };
        let chk =
            hf2::checksum_pages(&d, target_address, num_pages).expect("checksum_pages failed");
        device_checksums.extend_from_slice(&chk.checksums[..]);
    }

    let mut binary_checksums = vec![];

    //collect and sums so we can view all mismatches, not just first
    for page in binary.chunks(bininfo.flash_page_size as usize) {
        let mut xmodem = CRCu16::crc16xmodem();
        xmodem.digest(&page);

        binary_checksums.push(xmodem.get_crc());
    }

    //only check as many as our binary has
    assert_eq!(
        &binary_checksums[..binary_checksums.len()],
        &device_checksums[..binary_checksums.len()]
    );
}

fn parse_hex_16(input: &str) -> Result<u16, std::num::ParseIntError> {
    if input.starts_with("0x") {
        u16::from_str_radix(&input[2..], 16)
    } else {
        input.parse::<u16>()
    }
}

#[derive(Debug, StructOpt)]
struct Opt {
    // `cargo build` arguments
    #[structopt(name = "binary", long = "bin")]
    bin: Option<String>,
    #[structopt(name = "example", long = "example")]
    example: Option<String>,
    #[structopt(name = "package", short = "p", long = "package")]
    package: Option<String>,
    #[structopt(name = "release", long = "release")]
    release: bool,
    #[structopt(name = "target", long = "target")]
    target: Option<String>,
    #[structopt(name = "PATH", long = "manifest-path", parse(from_os_str))]
    manifest_path: Option<PathBuf>,
    #[structopt(long)]
    no_default_features: bool,
    #[structopt(long)]
    all_features: bool,
    #[structopt(long)]
    features: Vec<String>,

    #[structopt(name = "pid", long = "pid", parse(try_from_str = parse_hex_16))]
    pid: Option<u16>,
    #[structopt(name = "vid", long = "vid",  parse(try_from_str = parse_hex_16))]
    vid: Option<u16>,
}
