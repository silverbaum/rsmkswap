//SPDX-License-Identifier: MIT

mod swapheader;

use std::{
    ffi::c_void,
    fs::{self, File, Metadata},
    io::{BufRead, BufReader, Write},
    os::{
        fd::AsRawFd,
        linux::fs::MetadataExt,
        unix::fs::{FileTypeExt, PermissionsExt},
    },
    path::Path,
    str::FromStr,
};
use swapheader::{MIN_SWAP_PAGES, MkswapError, SWAP_SIGNATURE, SWAP_SIGNATURE_SZ, SwapHeader};

use clap::{Arg, ArgAction, ArgMatches, Command, crate_name, crate_version};
use libc::{_SC_PAGE_SIZE, _SC_PAGESIZE, ioctl, pread, sysconf};
use linux_raw_sys::ioctl::BLKGETSIZE64;
use uuid::Uuid;

fn getpagesize() -> Result<usize, std::io::Error> {
    let mut sz = unsafe { sysconf(_SC_PAGESIZE) };
    if sz < 512 {
        sz = unsafe { sysconf(_SC_PAGE_SIZE) };
    }
    if sz <= 0 {
        Err(std::io::Error::other(
            "Failed to determine page size, please check your system configuration",
        ))
    } else {
        Ok(sz as usize)
    }
}

fn getsize(fd: &File, stat: &Metadata, devname: &str) -> Result<u64, std::io::Error> {
    let devsize: u64;
    /* for block devices, ioctl call with manual size reading as a backup method */
    if stat.file_type().is_block_device() {
        let mut sz: u64 = 0;
        let err = unsafe { ioctl(fd.as_raw_fd(), BLKGETSIZE64 as u64, &mut sz) };

        if sz == 0 || err < 0 {
            let f_size = fs::File::open(format!("/sys/class/block/{devname}/size"))?;

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

fn check_device(fd: &File, pagesize: usize, pages: u32) -> Result<Vec<u32>, std::io::Error> {
    let mut buf = vec![0u8; pagesize];
    let mut badpages: Vec<u32> = Vec::new();
    for page in 1..pages {
        let bytes = unsafe {
            pread(
                fd.as_raw_fd(),
                buf.as_mut_ptr() as *mut c_void,
                pagesize,
                page as i64 * pagesize as i64,
            )
        };
        if bytes < pagesize as isize {
            badpages.push(page);
            eprintln!("bad page at index {page}");
        }
    }
    Ok(badpages)
}

unsafe fn write_signature_page(
    pagesize: usize,
    pages: u32,
    badpages: Vec<u32>,
    uuid: Uuid,
    label: &str,
) -> Result<Vec<u8>, MkswapError> {
    let mut buf = vec![0u8; pagesize];

    let header = SwapHeader::new()
        .label(label.to_owned())?
        .pages(pages)?
        .bad_pages(badpages, pagesize)?
        .uuid(uuid);

    let header_bytes = unsafe {
        std::slice::from_raw_parts(
            (&header as *const SwapHeader) as *const u8,
            std::mem::size_of::<SwapHeader>(),
        )
    };
    buf[..header_bytes.len()].copy_from_slice(header_bytes);

    buf[pagesize - SWAP_SIGNATURE_SZ..].copy_from_slice(SWAP_SIGNATURE);

    Ok(buf)
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
                "failed to open {device}: {e}",
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
    //let verbose = args.get_flag("verbose"); //TODO
    let createflag: bool = args.get_flag("file");
    let checkflag: bool = args.get_flag("check");
    let filesize: u64 = match args.get_one::<u64>("filesize") {
        Some(fsz) => *fsz,
        None => 0,
    };
    let pagesize: usize = match args.get_one::<usize>("pagesize") {
        Some(psz) => {
            if psz.is_power_of_two() {
                *psz
            } else {
                return Err(std::io::Error::other("Pagesize must be power of two"));
            }
        }
        None => getpagesize()?,
    };

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

    let devsize = if createflag {
        filesize
    } else {
        getsize(&fd, &stat, devname).map_err(std::io::Error::other)?
    };

    let pages = (devsize / pagesize as u64) as u32;

    if pages < MIN_SWAP_PAGES && createflag {
        fs::remove_file(dev)?;
    }

    let badpages = if checkflag {
        check_device(&fd, pagesize, pages)?
    } else {
        vec![0]
    };

    let buf = unsafe {
        match write_signature_page(pagesize, pages, badpages, uuid, label) {
            Ok(buffer) => buffer,
            Err(MkswapError::TooFewPages { pages: _ }) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "Device {} is too small for a swap area, minimum size is {}KiB",
                        devname,
                        (MIN_SWAP_PAGES * pagesize as u32) / 1024
                    ),
                ));
            }
            Err(MkswapError::TooLongLabel) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    MkswapError::TooLongLabel,
                ));
            }
            Err(MkswapError::MaxBadPagesExceeded {
                bad_pages,
                max_badpages,
            }) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    MkswapError::MaxBadPagesExceeded {
                        bad_pages,
                        max_badpages,
                    },
                ));
            }
        }
    };

    fd.write_all(&buf)?;
    fd.flush()?;
    fd.sync_all()?;

    println!(
        "Setting up swapspace version 1, size = {}KiB\n{}{}, UUID={}",
        (((pages - 1) as usize * pagesize) / 1024),
        if label.is_empty() {
            "No label"
        } else {
            "LABEL="
        },
        &label,
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
                .short('L')
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
                .requires("filesize")
                .help("create a swap file"),
        )
        .arg(
            Arg::new("check")
                .short('c')
                .long("check")
                .action(ArgAction::SetTrue)
                .help("check the swap device for bad pages"),
        )
        .arg(
            Arg::new("filesize")
                .short('s')
                .long("size")
                .action(ArgAction::Set)
                .value_parser(clap::value_parser!(u64))
                .value_name("SIZE")
                .requires("file")
                .help("size of the swap file in bytes"),
        )
        .arg(
            Arg::new("pagesize")
                .short('P')
                .long("pagesize")
                .action(ArgAction::Set)
                .value_parser(clap::value_parser!(usize))
                .value_name("PAGESIZE")
                .help("set the pagesize of the target"),
        )
    /*
    .arg(
        Arg::new("verbose")
            .short('v')
            .long("verbose")
            .action(ArgAction::SetTrue)
            .help("verbose output"),
    )
    */
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
    use crate::swapheader::SWAP_LABEL_LENGTH;

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
    fn test_long_label() {
        let args = vec![
            String::from("mkswap"),
            String::from("-F"),
            String::from("swapfile_long_label"),
            String::from("--size"),
            String::from("65535"),
            String::from("--label"),
            String::from("WAAAYTOOLONGOFALABELFORASWAP"),
        ];
        let result = run(&args);
        assert!(result.err().unwrap().to_string().contains(
            format!("Label is too long, maximum size is {SWAP_LABEL_LENGTH} characters").as_str()
        ));
        let _ = fs::remove_file("swapfile_long_label");
    }

    #[test]
    fn test_with_invalid_uuid() {
        let args = vec![
            String::from("mkswap"),
            String::from("-F"),
            String::from("swapfile_invalid_uuid"),
            String::from("--size"),
            String::from("65535"),
            String::from("--uuid"),
            String::from("invalid-uuid"),
        ];
        let result = run(&args);
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("Invalid UUID"));
        let _ = fs::remove_file("swapfile_invalid_uuid");
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
        let _ = fs::remove_file("swapfile_too_small");
    }
    #[test]
    fn test_all_features() {
        let args = vec![
            String::from("mkswap"),
            String::from("swapfile"),
            String::from("-F"),
            String::from("--size"),
            String::from("65535"),
            String::from("--uuid"),
            String::from("123e4567-e89b-12d3-a456-426614174000"),
            String::from("--label"),
            String::from("SWAPTEST"),
            String::from("--check"),
        ];
        let result = run(&args);
        dbg!(&result);
        assert!(result.is_ok());
        let _ = fs::remove_file("swapfile");
    }
}
