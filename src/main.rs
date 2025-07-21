//SPDX-License-Identifier: MIT

use std::{
    fs::{self, File, Metadata},
    io::{BufRead, BufReader, Write},
    os::{
        fd::AsRawFd, linux::fs::MetadataExt, raw::c_char, raw::c_uchar, unix::fs::FileTypeExt,
        unix::fs::PermissionsExt,
    },
    path::Path,
    str::FromStr,
};

use clap::{Arg, ArgAction, ArgMatches, Command, crate_name, crate_version};
use libc::{_SC_PAGE_SIZE, _SC_PAGESIZE, ioctl, sysconf};
use linux_raw_sys::ioctl::BLKGETSIZE64;
use uuid::Uuid;

const SWAP_SIGNATURE: &[u8] = "SWAPSPACE2".as_bytes();
const SWAP_SIGNATURE_SZ: usize = 10;
const SWAP_LABEL_LENGTH: usize = 16;
const SWAP_VERSION: u32 = 1;
const MIN_SWAP_PAGES: u64 = 10;

#[repr(C)]
struct SwapHeader {
    bootbits: [c_char; 1024],
    version: u32,
    last_page: u32,
    nr_badpages: u32,
    uuid: [c_uchar; 16],
    volume_name: [u8; SWAP_LABEL_LENGTH],
    padding: [u32; 117],
    badpages: [u32; 1],
}

fn getsize(fd: &File, stat: &Metadata, devname: &str) -> Result<u64, std::io::Error> {
    let devsize: u64;
    /* for block devices, ioctl call with manual size reading as a backup method */
    if stat.file_type().is_block_device() {
        let mut sz: u64 = 0;
        let err = unsafe { ioctl(fd.as_raw_fd(), BLKGETSIZE64 as u64, &mut sz) };

        if sz == 0 || err < 0 {
            let f_size = fs::File::open(format!("/sys/class/block/{}/size", devname))?;

            let reader = BufReader::new(f_size);
            let vec: Vec<Result<u64, _>> = reader
                .lines()
                .map(|v| v.unwrap().parse::<u64>())
                .collect::<Vec<Result<u64, _>>>();
            sz = vec[0].clone().unwrap_or(0);
            devsize = sz * 512;
        } else {
            devsize = sz;
        }
    } else {
        devsize = stat.st_size();
    }

    Ok(devsize)
}

unsafe fn write_signature_page(
    pagesize: usize,
    pages: u64,
    uuid: Uuid,
    label: &str,
    badpages: [u32; 1],
    verbose: bool,
) -> Vec<u8> {
    let mut header = SwapHeader {
        bootbits: [0; 1024],
        version: SWAP_VERSION,
        last_page: (pages - 1) as u32,
        nr_badpages: 0, // Assumes no bad pages
        uuid: *uuid.as_bytes(),
        volume_name: [0; SWAP_LABEL_LENGTH],
        padding: [0; 117],
        badpages,
    };

    let label_bytes = label.as_bytes();
    let lblen = label_bytes.len().min(SWAP_LABEL_LENGTH);
    if label.len() > SWAP_LABEL_LENGTH && verbose {
        eprintln!("swap label was truncated");
    }

    let label_buf = unsafe {
        std::slice::from_raw_parts_mut(
            header.volume_name.as_mut_ptr() as *mut u8,
            SWAP_LABEL_LENGTH,
        )
    };
    label_buf[..lblen].copy_from_slice(&label_bytes[..lblen]);

    let mut buf = vec![0u8; pagesize];

    let header_bytes = unsafe {
        std::slice::from_raw_parts(
            (&header as *const SwapHeader) as *const u8,
            std::mem::size_of::<SwapHeader>(),
        )
    };

    buf[0..header_bytes.len()].copy_from_slice(header_bytes);

    let signature_offset = pagesize - SWAP_SIGNATURE.len();
    buf[signature_offset..].copy_from_slice(SWAP_SIGNATURE);

    buf
}

fn open_device(
    device: &String,
    dev: &Path,
    createflag: bool,
    filesize: u64,
) -> Result<File, std::io::Error> {
    let mut options = fs::OpenOptions::new();
    let fd = match options
        .create(false)
        .create_new(createflag)
        .write(true)
        .read(true)
        .truncate(false)
        .append(false)
        .open(dev)
    {
        Ok(f) => f,
        Err(e) => {
            return Err(std::io::Error::other(format!(
                "failed to open {}: {}",
                device, e
            )));
        }
    };

    if createflag {
        fd.set_permissions(fs::Permissions::from_mode(0o600))?;
        fd.set_len(filesize)?;
    }

    Ok(fd)
}

pub fn mkswap(args: &ArgMatches) -> Result<(), std::io::Error> {
    let verbose = args.get_flag("verbose");
    let createflag: bool = args.get_flag("file");
    let filesize: u64 = *args.get_one::<u64>("filesize").unwrap_or(&0);

    let device = args
        .get_one::<String>("device")
        .expect("missing required argument device");

    let label = args
        .get_one::<String>("label")
        .map(|s| s.as_str())
        .unwrap_or("");

    let dev = Path::new(device.as_str());
    let devname = {
        if let Some(n) = dev.file_name().and_then(|o| o.to_str()) {
            n
        } else {
            device.strip_prefix("/dev/").unwrap_or(device)
        }
    };

    let uuid = match args.get_one::<String>("uuid") {
        Some(str) => Uuid::from_str(str).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid UUID '{str}': {e}"),
            )
        })?,
        None => Uuid::new_v4(),
    };

    let mut fd = open_device(device, dev, createflag, filesize)?;

    let stat = fd.metadata()?;
    if stat.st_uid() != 0 {
        println!(
            "mkswap: {}: insecure file owner {}, fix with: chown 0:0 {}",
            device,
            stat.st_uid(),
            device
        );
    }

    let pagesize: u64 = {
        let mut sz = unsafe { sysconf(_SC_PAGESIZE) };
        if sz < 512 {
            sz = unsafe { sysconf(_SC_PAGE_SIZE) };
        }
        if sz <= 0 {
            return Err(std::io::Error::other(
                "Failed to determine page size, please check your system configuration",
            ));
        } else {
            sz as u64
        }
    };

    let devsize = if createflag {
        filesize
    } else {
        getsize(&fd, &stat, devname).map_err(std::io::Error::other)? as u64
    };

    let pages = devsize / pagesize;

    if pages < MIN_SWAP_PAGES {
        if createflag {
            fs::remove_file(dev)?;
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "Device {} is too small for a swap area, minimum size is {}KiB",
                devname,
                (MIN_SWAP_PAGES * pagesize) / 1024
            ),
        ));
    }

    let badpages = [0u32; 1];

    // initialize and write swap header information to a buffer
    let mut buf =
        unsafe { write_signature_page(pagesize as usize, pages, uuid, label, badpages, verbose) };

    //write swap signature to buffer
    let _ = &buf[(pagesize as usize - SWAP_SIGNATURE_SZ)..pagesize as usize]
        .copy_from_slice(SWAP_SIGNATURE);

    fd.write_all(&buf)?;
    fd.flush()?;
    fd.sync_all()?;

    println!(
        "Setting up swapspace version 1, size = {}KiB\n{}{}, UUID={}",
        (((pages - 1) * pagesize as u64) / 1024),
        if label.is_empty() {
            "No label"
        } else {
            "LABEL="
        },
        &label[..label.len().min(16)], //truncate given too long of a label.
        uuid
    );

    Ok(())
}

pub fn clapp() -> Command {
    Command::new(crate_name!())
        .version(crate_version!())
        .about("Set up a Linux swap area")
        .infer_long_args(true)
        .arg(
            Arg::new("device")
                .action(ArgAction::Set)
                .required(true)
                .help("block device or swap file"),
        )
        .arg(
            Arg::new("label")
                .short('l')
                .long("label")
                .action(ArgAction::Set)
                .help("set a label"),
        )
        .arg(
            Arg::new("uuid")
                .short('u')
                .long("uuid")
                .action(ArgAction::Set)
                .help("set the UUID to use"),
        )
        .arg(
            Arg::new("file")
                .short('F')
                .long("file")
                .action(ArgAction::SetTrue)
                .help("create a swap file"),
        )
        .arg(
            Arg::new("filesize")
                .short('s')
                .long("size")
                .action(ArgAction::Set)
                .value_parser(clap::value_parser!(u64))
                .value_name("SIZE")
                .help("size of the swap file in bytes"),
        )
        .arg(
            Arg::new("verbose")
                .short('v')
                .long("verbose")
                .action(ArgAction::SetTrue)
                .help("verbose output"),
        )
}

fn run(args: &[String]) -> Result<(), std::io::Error> {
    let matches = clapp().get_matches_from(args);
    mkswap(&matches)
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if let Err(e) = run(&argv[..]) {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_args() {
        let args = vec![
            String::from("mkswap"),
            String::from("swapfile_test"),
            String::from("--label"),
            String::from("test_swap"),
            String::from("-F"),
            String::from("--size"),
            String::from("65535"),
            String::from("--uuid"),
            String::from("123e4567-e89b-12d3-a456-426614174000"),
        ];
        let result = run(&args);
        let _ = fs::remove_file("swapfile_test");
        assert!(result.is_ok());
    }

    #[test]
    fn test_invalid_device() {
        let args = vec![String::from("mkswap"), String::from("/invalid/device")];
        let result = run(&args);
        assert!(result.is_err());
    }

    #[test]
    fn test_with_invalid_uuid() {
        let args = vec![
            String::from("mkswap"),
            String::from("-F"),
            String::from("swapfile_invalid_uuid"),
            String::from("--uuid"),
            String::from("invalid-uuid"),
        ];
        let result = run(&args);
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("Invalid UUID"));
    }

    #[test]
    fn test_with_too_small_device() {
        let args = vec![
            String::from("mkswap"),
            String::from("swapfile_too_small"),
            String::from("--file"),
            String::from("--size"),
            String::from("1024"),
        ];
        let result = run(&args);
        assert!(result.is_err());
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("is too small for a swap area")
        );
    }
    #[test]
    fn test_all_features() {
        let args = vec![
            String::from("mkswap"),
            String::from("swapfile"),
            String::from("-F"),
            String::from("--size"),
            String::from("65536"),
            String::from("--uuid"),
            String::from("123e4567-e89b-12d3-a456-426614174000"),
            String::from("--label"),
            String::from("SWAPTEST"),
            String::from("--verbose"),
        ];
        let result = run(&args);
        assert!(result.is_ok());
        let _ = fs::remove_file("swapfile");
    }
}
