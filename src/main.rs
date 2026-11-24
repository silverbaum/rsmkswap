// SPDX-FileCopyrightText: Copyright 2026 silverbaum
// SPDX-License-Identifier: MIT

use std::{
    fs::{self, File, Metadata, OpenOptions},
    io,
    os::{
        fd::{AsFd, AsRawFd},
        unix::fs::{FileExt, FileTypeExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    str::FromStr,
};

use clap::{Arg, ArgAction, ArgMatches, Command, crate_name, crate_version};
use libc::{
    _SC_PAGESIZE, O_EXCL, O_NONBLOCK, c_int, c_long, dev_t, geteuid, ioctl, off_t, sysconf,
};
use nix::{
    errno::Errno,
    fcntl::{FallocateFlags, fallocate},
    sys::{
        stat::{major, minor},
        statfs::{BTRFS_SUPER_MAGIC, fstatfs},
    },
};
use uuid::Uuid;

// Linux encodes size_t in this request number, but returns a u64.
const BLKGETSIZE64: nix::sys::ioctl::ioctl_num_type =
    nix::request_code_read!(0x12, 114, size_of::<usize>());
// The inode flag requests encode long, but their argument points to an int.
const FS_IOC_SETFLAGS: nix::sys::ioctl::ioctl_num_type =
    nix::request_code_write!(b'f', 2, size_of::<c_long>());
const FS_IOC_GETFLAGS: nix::sys::ioctl::ioctl_num_type =
    nix::request_code_read!(b'f', 1, size_of::<c_long>());
const FS_NOCOW_FL: c_int = 0x0080_0000;

const BOOTBITS_SIZE: usize = 1024;
const BAD_PAGES_OFFSET: usize = 1536;
const SWAP_SIGNATURE: &[u8] = b"SWAPSPACE2";
const MIN_PAGE_SIZE: usize = BAD_PAGES_OFFSET + SWAP_SIGNATURE.len();
const SWAP_LABEL_LENGTH: usize = 16;
const SWAP_VERSION: u32 = 1;
const MIN_SWAP_PAGES: u32 = 10;

#[derive(Debug)]
enum MkswapError {
    TooLongLabel,
    MaxBadPagesExceeded {
        max_badpages: usize,
    },
    SwapAreaTooSmall {
        min_swapsize: u64,
    },
    Io {
        operation: &'static str,
        path: Option<PathBuf>,
        source: io::Error,
    },
    BlockSize {
        path: PathBuf,
        sysfs_path: PathBuf,
        ioctl_error: io::Error,
        sysfs_error: io::Error,
    },
    UsageError {
        msg: String,
    },
}

impl std::fmt::Display for MkswapError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::TooLongLabel => write!(
                f,
                "Label is too long, maximum size is {SWAP_LABEL_LENGTH} bytes"
            ),
            Self::MaxBadPagesExceeded { max_badpages } => {
                write!(f, "Too many bad pages: {max_badpages}")
            }
            Self::SwapAreaTooSmall { min_swapsize } => write!(
                f,
                "Swap area needs to be at least {} KiB",
                min_swapsize >> 10
            ),
            Self::Io {
                operation,
                path,
                source,
            } => match path {
                Some(path) => write!(f, "{operation} {}: {source}", path.display()),
                None => write!(f, "{operation}: {source}"),
            },
            Self::BlockSize {
                path,
                sysfs_path,
                ioctl_error,
                sysfs_error,
            } => write!(
                f,
                "get block size of {}: BLKGETSIZE64: {ioctl_error}; {}: {sysfs_error}",
                path.display(),
                sysfs_path.display(),
            ),
            Self::UsageError { msg } => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for MkswapError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::BlockSize { ioctl_error, .. } => Some(ioctl_error),
            _ => None,
        }
    }
}

fn target_error<E: Into<io::Error>>(
    operation: &'static str,
    path: &Path,
) -> impl FnOnce(E) -> MkswapError {
    move |source| MkswapError::Io {
        operation,
        path: Some(path.to_owned()),
        source: source.into(),
    }
}

#[derive(Clone, Copy)]
enum Endian {
    Native,
    Little,
    Big,
}

impl Endian {
    fn bytes(self, value: u32) -> [u8; 4] {
        match self {
            Self::Native => value.to_ne_bytes(),
            Self::Little => value.to_le_bytes(),
            Self::Big => value.to_be_bytes(),
        }
    }
}

fn encode_header(
    pagesize: usize,
    pages: u32,
    label: &str,
    uuid: Uuid,
    endianness: Endian,
    badpages: &[u32],
) -> Result<Vec<u8>, MkswapError> {
    let max_badpages = ((pagesize - MIN_PAGE_SIZE) / size_of::<u32>()).min(u32::MAX as usize);
    if badpages.len() > max_badpages {
        return Err(MkswapError::MaxBadPagesExceeded { max_badpages });
    }
    // The first 1024 bytes of the swap area are reserved and stay untouched.
    let mut header = Vec::with_capacity(pagesize - BOOTBITS_SIZE);
    header.extend_from_slice(&endianness.bytes(SWAP_VERSION));
    header.extend_from_slice(&endianness.bytes(pages - 1));
    header.extend_from_slice(&endianness.bytes(badpages.len() as u32));
    header.extend_from_slice(uuid.as_bytes());
    header.extend_from_slice(label.as_bytes());
    header.resize(BAD_PAGES_OFFSET - BOOTBITS_SIZE, 0);
    for &page in badpages {
        header.extend_from_slice(&endianness.bytes(page));
    }
    header.resize(pagesize - BOOTBITS_SIZE - SWAP_SIGNATURE.len(), 0);
    header.extend_from_slice(SWAP_SIGNATURE);
    Ok(header)
}

fn getpagesize() -> Result<usize, MkswapError> {
    let size = unsafe { sysconf(_SC_PAGESIZE) };
    usize::try_from(size)
        .ok()
        .filter(|&size| size != 0)
        .ok_or_else(|| MkswapError::Io {
            operation: "get system page size",
            path: None,
            source: io::Error::last_os_error(),
        })
}

fn sysfs_block_size_path(device: dev_t) -> PathBuf {
    PathBuf::from(format!(
        "/sys/dev/block/{}:{}/size",
        major(device),
        minor(device)
    ))
}

fn read_sysfs_block_size(path: &Path) -> io::Result<u64> {
    parse_sysfs_block_size(&fs::read_to_string(path)?)
}

fn parse_sysfs_block_size(input: &str) -> io::Result<u64> {
    let sectors = input
        .trim()
        .parse::<u64>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    sectors
        .checked_mul(512)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "block size exceeds u64"))
}

fn get_blockdev_size(file: &File, path: &Path, device: dev_t) -> Result<u64, MkswapError> {
    let mut size = 0u64;
    if unsafe { ioctl(file.as_raw_fd(), BLKGETSIZE64, &mut size) } >= 0 {
        return Ok(size);
    }

    let ioctl_error = io::Error::last_os_error();
    let sysfs_path = sysfs_block_size_path(device);
    read_sysfs_block_size(&sysfs_path).map_err(|sysfs_error| MkswapError::BlockSize {
        path: path.to_owned(),
        sysfs_path,
        ioctl_error,
        sysfs_error,
    })
}

fn check_device(fd: &File, pagesize: usize, pages: u32, offset: u64, verbose: bool) -> Vec<u32> {
    let mut buf = vec![0u8; pagesize];
    let mut badpages: Vec<u32> = Vec::new();
    for page in 1..pages {
        let position = offset + u64::from(page) * pagesize as u64;
        if !matches!(fd.read_at(&mut buf, position), Ok(bytes) if bytes == pagesize) {
            badpages.push(page);
            if verbose {
                eprintln!("bad page at index {page}");
            }
        }
    }
    badpages
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TargetKind {
    File,
    BlockDevice,
}

fn target_kind(metadata: &Metadata, path: &Path) -> Result<TargetKind, MkswapError> {
    let kind = metadata.file_type();
    if kind.is_file() {
        Ok(TargetKind::File)
    } else if kind.is_block_device() {
        Ok(TargetKind::BlockDevice)
    } else {
        Err(MkswapError::UsageError {
            msg: format!("{} is not a regular file or block device", path.display()),
        })
    }
}

fn open_existing_target(path: &Path) -> Result<(File, u64), MkswapError> {
    let metadata = fs::metadata(path).map_err(target_error("stat", path))?;
    let kind = target_kind(&metadata, path)?;
    // On Linux, opening a block device with O_EXCL claims it until close.
    let (flags, operation) = match kind {
        TargetKind::BlockDevice => (O_EXCL | O_NONBLOCK, "claim block device"),
        TargetKind::File => (O_NONBLOCK, "open"),
    };
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(flags)
        .open(path)
        .map_err(target_error(operation, path))?;
    let metadata = file
        .metadata()
        .map_err(target_error("stat opened target", path))?;
    if target_kind(&metadata, path)? != kind {
        return Err(MkswapError::UsageError {
            msg: format!("target type changed while opening {}", path.display()),
        });
    }

    let size = match kind {
        TargetKind::BlockDevice => get_blockdev_size(&file, path, metadata.rdev())?,
        TargetKind::File => {
            let mode = metadata.mode() & 0o7777;
            if mode & !0o600 != 0 {
                eprintln!(
                    "rsmkswap: {}: insecure file permissions {mode:04o}; expected 0600",
                    path.display()
                );
            }
            if metadata.uid() != 0 && unsafe { geteuid() } == 0 {
                eprintln!(
                    "rsmkswap: {}: insecure file owner {}; expected root",
                    path.display(),
                    metadata.uid()
                );
            }
            metadata.len()
        }
    };
    Ok((file, size))
}

fn create_swap_file(path: &Path, size: off_t) -> Result<File, MkswapError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(target_error("create", path))?;

    let stat = fstatfs(file.as_fd()).map_err(target_error("get filesystem type of", path))?;
    if stat.filesystem_type() == BTRFS_SUPER_MAGIC {
        disable_copy_on_write(&file).map_err(target_error("disable copy-on-write on", path))?;
    }
    loop {
        match fallocate(file.as_fd(), FallocateFlags::empty(), 0, size) {
            Err(Errno::EINTR) => continue,
            result => break result,
        }
    }
    .map_err(target_error("allocate", path))?;

    let length = file
        .metadata()
        .map_err(target_error("stat allocated file", path))?
        .len();
    if length != size as u64 {
        return Err(target_error("verify allocation of", path)(
            io::Error::other(format!("expected {size} bytes, got {length}")),
        ));
    }
    Ok(file)
}

fn disable_copy_on_write(file: &File) -> io::Result<()> {
    let fd = file.as_raw_fd();
    let mut flags: c_int = 0;
    if unsafe { ioctl(fd, FS_IOC_GETFLAGS, &mut flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    flags |= FS_NOCOW_FL;
    if unsafe { ioctl(fd, FS_IOC_SETFLAGS, &flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn swap_pages(size: u64, offset: u64, pagesize: usize) -> Result<u32, MkswapError> {
    let pages = size.saturating_sub(offset) / pagesize as u64;
    if pages < u64::from(MIN_SWAP_PAGES) {
        return Err(MkswapError::SwapAreaTooSmall {
            min_swapsize: u64::from(MIN_SWAP_PAGES).saturating_mul(pagesize as u64),
        });
    }
    u32::try_from(pages).map_err(|_| MkswapError::UsageError {
        msg: format!(
            "swap area is too large: max size is {} GiB",
            (u128::from(u32::MAX) * pagesize as u128) >> 30
        ),
    })
}

fn mkswap(matches: &ArgMatches) -> Result<(), MkswapError> {
    let verboseflag = matches.get_flag("verbose");
    let checkflag = matches.get_flag("check");
    let forceflag = matches.get_flag("force");
    let offset = *matches.get_one::<u64>("offset").unwrap_or(&0u64);

    let devpath = matches
        .get_one::<PathBuf>("device")
        .expect("required by Clap");

    let label = matches
        .get_one::<String>("label")
        .map_or("", String::as_str);
    if label.len() > SWAP_LABEL_LENGTH {
        return Err(MkswapError::TooLongLabel);
    }

    let endianness = match matches.get_one::<String>("endianness") {
        Some(str) => match str.to_lowercase().as_str() {
            "native" => Endian::Native,
            "little" => Endian::Little,
            "big" => Endian::Big,
            _ => {
                return Err(MkswapError::UsageError {
                    msg: format!("invalid endianness {} is not supported", str),
                });
            }
        },
        None => Endian::Native,
    };

    let uuid = match matches.get_one::<String>("uuid") {
        Some(str) => Uuid::from_str(str).map_err(|e| MkswapError::UsageError {
            msg: format!("Invalid UUID '{str}': {e}"),
        })?,
        None => Uuid::new_v4(),
    };

    let sys_pagesize = getpagesize()?;
    let pagesize = match matches.get_one::<usize>("pagesize") {
        Some(sz) => {
            if *sz < MIN_PAGE_SIZE || (!forceflag && !sz.is_power_of_two()) {
                return Err(MkswapError::UsageError {
                    msg: format!("Bad user-specified page size {}", *sz),
                });
            }

            if *sz != sys_pagesize {
                eprintln!(
                    "Using user-specified page size {}, instead of the system value {}",
                    *sz, sys_pagesize
                );
            }
            *sz
        }
        None => sys_pagesize,
    };

    let (file, pages) = if let Some(&size) = matches.get_one::<u64>("filesize") {
        let pages = swap_pages(size, offset, pagesize)?;
        let allocation_size = off_t::try_from(size).map_err(|_| MkswapError::UsageError {
            msg: format!("swap file size exceeds the maximum file offset: {size} bytes"),
        })?;
        (create_swap_file(devpath, allocation_size)?, pages)
    } else {
        let (file, size) = open_existing_target(devpath)?;
        (file, swap_pages(size, offset, pagesize)?)
    };

    let badpages = if checkflag {
        check_device(&file, pagesize, pages, offset, verboseflag)
    } else {
        Vec::new()
    };

    let header = encode_header(pagesize, pages, label, uuid, endianness, &badpages)?;
    file.write_all_at(&header, offset + BOOTBITS_SIZE as u64)
        .map_err(target_error("write swap header to", devpath))?;
    file.sync_all().map_err(target_error("sync", devpath))?;

    let bytes = u64::from(pages - 1) * pagesize as u64;
    println!(
        "Setting up swapspace version 1, size = {} KiB ({} bytes)\n{}{}, UUID={}",
        bytes / 1024,
        bytes,
        if label.is_empty() {
            "no label"
        } else {
            "LABEL="
        },
        label,
        uuid
    );

    Ok(())
}

fn clapp() -> Command {
    Command::new(crate_name!())
        .version(crate_version!())
        .about("Set up a Linux swap area")
        .infer_long_args(true)
        .arg(
            Arg::new("device")
                .action(ArgAction::Set)
                .value_parser(clap::value_parser!(PathBuf))
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
                .help("create a new swap file; the path must not exist"),
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
        .arg(
            Arg::new("verbose")
                .short('v')
                .long("verbose")
                .action(ArgAction::SetTrue)
                .help("verbose output"),
        )
        .arg(
            Arg::new("force")
                .short('f')
                .long("force")
                .action(ArgAction::SetTrue)
                .help("allow swap size area to be larger than device"),
        )
        .arg(
            Arg::new("endianness")
                .short('e')
                .long("endianness")
                .action(ArgAction::Set)
                .value_parser(clap::value_parser!(String))
                .help("specify the endianness to use (native, little, or big)"),
        )
        .arg(
            Arg::new("offset")
                .short('o')
                .long("offset")
                .action(ArgAction::Set)
                .value_parser(clap::value_parser!(u64))
                .help("specify the offset in the device"),
        )
}

fn main() {
    let matches = clapp().get_matches();
    if let Err(e) = mkswap(&matches) {
        eprintln!("rsmkswap: error: {}", e);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_page_count_and_indexes_share_the_header_byte_order() {
        let header = encode_header(4096, 512, "", Uuid::nil(), Endian::Big, &[3, 258])
            .expect("encode a header with two known bad pages");
        assert_eq!(&header[0..12], &[0, 0, 0, 1, 0, 0, 1, 255, 0, 0, 0, 2]);
        assert_eq!(&header[512..520], &[0, 0, 0, 3, 0, 0, 1, 2]);
        assert_eq!(&header[3062..], b"SWAPSPACE2");
    }

    #[test]
    fn bad_page_table_cannot_overlap_the_signature() {
        let header = encode_header(4096, 640, "", Uuid::nil(), Endian::Big, &[1; 637])
            .expect("a full bad-page table fits");
        assert_eq!(&header[3056..], b"\x00\x00\x00\x01\x00\x00SWAPSPACE2");
        assert!(matches!(
            encode_header(4096, 640, "", Uuid::nil(), Endian::Native, &[1; 638]),
            Err(MkswapError::MaxBadPagesExceeded { max_badpages: 637 })
        ));
    }

    #[test]
    fn sysfs_size_path_uses_device_numbers() {
        assert_eq!(
            sysfs_block_size_path(nix::sys::stat::makedev(259, 65537)),
            Path::new("/sys/dev/block/259:65537/size"),
        );
    }

    #[test]
    fn sysfs_size_is_in_512_byte_sectors_and_rejects_overflow() {
        assert_eq!(
            parse_sysfs_block_size("16\n").expect("valid sector count"),
            8192
        );
        for input in ["", "36028797018963968\n"] {
            assert_eq!(
                parse_sysfs_block_size(input)
                    .expect_err("invalid size")
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
    }
}
