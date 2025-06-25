//SPDX-License-Identifier: MIT

use std::{
    fs::{self, File, Metadata},
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    os::{fd::AsRawFd, linux::fs::MetadataExt, unix::fs::FileTypeExt, unix::fs::PermissionsExt},
    path::Path,
    str::FromStr,
};

use clap::{Arg, ArgAction, ArgMatches, Command, crate_name, crate_version};
use libc::{_SC_PAGE_SIZE, _SC_PAGESIZE, ioctl, lseek, read, sysconf};
use linux_raw_sys::ioctl::BLKGETSIZE64;
use uuid::Uuid;

const SWAP_SIGNATURE: &[u8] = "SWAPSPACE2".as_bytes();
const SWAP_SIGNATURE_SZ: usize = 10;
const SWAP_VERSION: u8 = 1;
const MIN_SWAP_PAGES: u128 = 10;

#[repr(C)]
struct SwapHeader {
    bootbits: [u8; 1024],
    version: u8,
    last_page: u32,
    nr_badpages: u32,
    uuid: [u8; 16],
    volume_name: [u8; 16],
    padding: [u32; 117],
    badpages: [u32; 100],
}

fn getsize(fd: &File, stat: &Metadata, devname: &str) -> Result<u128, std::io::Error> {
    let devsize: u128;
    /* for block devices, ioctl call with manual size reading as a backup method */
    if stat.file_type().is_block_device() {
        let mut sz: u128 = 0;
        let err = unsafe { ioctl(fd.as_raw_fd(), BLKGETSIZE64 as u64, &mut sz) };

        if sz == 0 || err < 0 {
            let f_size = fs::File::open(format!("/sys/class/block/{}/size", devname))?;

            let reader = BufReader::new(f_size);
            let vec: Vec<Result<u128, _>> = reader
                .lines()
                .map(|v| v.unwrap().parse::<u128>())
                .collect::<Vec<Result<u128, _>>>();
            sz = vec[0].clone().unwrap_or(0);
            devsize = sz * 512;
        } else {
            devsize = sz;
        }
    } else {
        devsize = stat.st_size() as u128;
    }

    Ok(devsize)
}

unsafe fn check_blocks(
    file: &mut File,
    pagesize: usize,
    pages: u128,
    verbose: bool,
) -> Result<Vec<u32>, std::io::Error> {
    let mut bad_pages: Vec<u32> = Vec::new();
    let mut buffer = vec![0u8; pagesize];
    let end = pagesize as u64 * pages as u64;

    let fd = file.as_raw_fd();
    let mut bytes: libc::ssize_t;

    for current_page in 0..pages {
        let offset = current_page as u64 * pagesize as u64;
        if offset > end {
            break;
        }

        unsafe {
            if lseek(fd, offset as i64, libc::SEEK_SET) < 0 {
                panic!("Failed to seek");
            }

            bytes = read(fd, buffer.as_mut_ptr() as *mut std::ffi::c_void, pagesize);
            if bytes < 0 || bytes != pagesize as isize {
                bad_pages.push(current_page as u32);
            }
        }
        if bad_pages.len() >= 640 {
            panic!("Too many bad pages detected: {}", bad_pages.len());
        }
    }
    if verbose {
        println!("{} bad pages", bad_pages.len())
    }
    file.seek(SeekFrom::Start(0))?;

    Ok(bad_pages)
}

unsafe fn write_signature_page(
    pagesize: usize,
    pages: u128,
    uuid: Uuid,
    label: &str,
    badpages: &mut Vec<u32>,
    verbose: bool,
) -> Vec<u8> {
    //let mut buf = Box::<[u8]>::new_uninit_slice(pagesize);
    //buf.as_mut_ptr().write_bytes(0, pagesize);
    let mut buf = vec![0u8; pagesize];
    unsafe {
        buf.as_mut_ptr().write_bytes(0, pagesize);
    }

    //fill up swap header
    let swap_hdr = unsafe { &mut *(buf.as_mut_ptr() as *mut SwapHeader) };
    swap_hdr.version = SWAP_VERSION;
    swap_hdr.last_page = (pages - 1) as u32;
    swap_hdr.nr_badpages = badpages.len() as u32;
    swap_hdr.badpages[..badpages.len()].copy_from_slice(badpages.as_mut_slice());
    if !uuid.is_nil() {
        swap_hdr.uuid = *uuid.as_bytes();
    }

    if !label.is_empty() {
        let lb = label.as_bytes();
        let lblen = lb.len().min(swap_hdr.volume_name.len());
        swap_hdr.volume_name[..lblen].copy_from_slice(&lb[..lblen]);

        if lb.len() > swap_hdr.volume_name.len() && verbose {
            println!("Label '{}' truncated", label);
        }
    }

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

pub fn mkswap(args: &ArgMatches) -> std::io::Result<()> {
    let verbose = args.get_flag("verbose");
    let checkflag: bool = args.get_flag("check");
    let createflag: bool = args.get_flag("file");
    let filesize: u64 = *args.get_one::<u64>("filesize").unwrap_or(&0);

    // CHECK DEVICE ARGUMENT, Make sure it is compatible with the file creation functionality
    let device = args.get_one::<String>("device").unwrap_or_else(|| {
        eprintln!("mkswap: missing required argument 'device'");
        std::process::exit(1);
    });

    let label = args
        .get_one::<String>("label")
        .map(|s| s.as_str())
        .unwrap_or("");

    let dev = Path::new(device.as_str());
    let devname = if let Some(str) = dev.file_name().unwrap().to_str() {
        str
    } else {
        device.strip_prefix("/dev/").unwrap_or(device)
    };

    let uuid = match args.get_one::<String>("uuid") {
        Some(str) => Uuid::from_str(str).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid UUID '{}': {}", str, e),
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

    let stblksize: u64 = stat.st_blksize();
    let pagesize: u128 = if stblksize == 0 {
        let mut sz = unsafe { sysconf(_SC_PAGESIZE) };
        if sz <= 0 {
            sz = unsafe { sysconf(_SC_PAGE_SIZE) };
            if sz <= 0 {
                return Err(std::io::Error::other(
                    "Failed to determine page size, please check your system configuration"
                ));
            }
        }
        (sz as u64).into()
    } else {
        stblksize.into()
    };

    let devsize: u128 = if createflag {
        filesize as u128
    } else {
        getsize(&fd, &stat, devname).map_err(std::io::Error::other)?
    };

    let pages: u128 = devsize / pagesize;

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

    let mut badpages = if checkflag {
        unsafe { check_blocks(&mut fd, pagesize as usize, pages, verbose)? }
    } else {
        vec![0; 100]
    };

    // initialize and write swap header information to a buffer
    let mut buf = unsafe {
        write_signature_page(
            pagesize as usize,
            pages,
            uuid,
            label,
            &mut badpages,
            verbose,
        )
    };

    //write swap signature to buffer
    let _ = &buf[(pagesize as usize - SWAP_SIGNATURE_SZ)..pagesize as usize]
        .copy_from_slice(SWAP_SIGNATURE);

    fd.write_all(&buf)?;
    fd.flush()?;
    fd.sync_all()?;

    println!(
        "Setting up swapspace version 1, size = {}KiB\n{}{}, UUID={}",
        (((pages - 1) * pagesize as u128) / 1024),
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
                .required(true)
                .action(ArgAction::Set)
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
            Arg::new("check")
                .long("check")
                .short('c')
                .action(ArgAction::SetTrue)
                .help("check the device for bad pages before writing to it"),
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

pub fn run(args: &[String]) -> Result<(), std::io::Error> {
    let matches = clapp().try_get_matches_from(args).map_err(|e| {
        eprintln!("Error parsing arguments: {}", e);
        std::io::Error::new(std::io::ErrorKind::InvalidInput, e)
    })?;

    if let Err(e) = mkswap(&matches) {
        eprintln!("{}", e);
        return Err(e);
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    run(&args[..]).map_err(|e| {
        eprintln!("{}", e);
        std::io::Error::other(e)
    })?;
    Ok(())
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
        assert!(result.is_ok());
        //delete file after test
        let _ = fs::remove_file("swapfile_test");
    }

    #[test]
    fn test_invalid_device() {
        let args = vec![String::from("mkswap"), String::from("/invalid/device")];
        let result = run(&args);
        assert!(result.is_err());
    }

    #[test]
    fn test_without_args() {
        let args = vec![String::from("mkswap")];
        let result = run(&args);
        assert!(result.is_err());
    }

    #[test]
    fn test_with_invalid_uuid() {
        let args = vec![
            String::from("mkswap"),
            String::from("/dev/sda1"),
            String::from("--uuid"),
            String::from("invalid-uuid"),
        ];
        let result = run(&args);
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("Invalid UUID"));
    }
}
