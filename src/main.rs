// SPDX-FileCopyrightText: Copyright 2026 silverbaum
// SPDX-License-Identifier: MIT

use core::ffi::{c_int, c_ulong, c_void};
use std::{
    fs::{File, Permissions},
    io::{BufRead, BufReader, Cursor, Seek, SeekFrom, Write},
    mem::size_of,
    os::{
        fd::{AsFd, AsRawFd},
        linux::fs::MetadataExt,
        unix::fs::{FileTypeExt, PermissionsExt},
    },
    path::Path,
    str::FromStr,
};

use clap::{Arg, ArgAction, ArgMatches, Command, crate_name, crate_version};
use libc::{_SC_PAGE_SIZE, _SC_PAGESIZE, geteuid, ioctl, pread, sysconf};
use nix::{
    fcntl::{FallocateFlags, fallocate}, // to avoid holes in created files
    sys::statfs::{BTRFS_SUPER_MAGIC, fstatfs}, // to detect filesystems
};
use uuid::Uuid;

// OS constants
const BLKGETSIZE64: u32 = 2148012658;
const FS_IOC_SETFLAGS: u32 = 1074292226;
const FS_IOC_GETFLAGS: u32 = 2148034049;
const FS_NOCOW_FL: u32 = 8388608;

// mkswap constants
const BOOTBITS_SIZE: usize = 1024;
const SWAP_SIGNATURE: &[u8] = "SWAPSPACE2".as_bytes();
const SWAP_SIGNATURE_SZ: usize = 10;
const SWAP_LABEL_LENGTH: usize = 16;
const SWAP_UUID_LENGTH: usize = 16;
const SWAP_VERSION: u32 = 1;
const MIN_SWAP_PAGES: u32 = 10;
const UUID_LENGTH: usize = 16;

#[derive(Debug, Clone)]
enum MkswapError {
    TooLongLabel,
    TooFewPages { pages: u32 },
    MaxBadPagesExceeded { max_badpages: usize },
    SwapAreaTooSmall { min_swapsize: u64 },
    IoError { msg: String },
    UsageError { msg: String },
}

impl std::fmt::Display for MkswapError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::TooLongLabel => write!(
                f,
                "Label is too long, maximum size is {SWAP_LABEL_LENGTH} characters"
            ),
            Self::TooFewPages { pages } => write!(
                f,
                "Too few pages for a swap area ({pages}), minimum is {MIN_SWAP_PAGES}"
            ),
            Self::MaxBadPagesExceeded { max_badpages } => {
                write!(f, "Too many bad pages: {max_badpages}")
            }
            Self::SwapAreaTooSmall { min_swapsize } => write!(
                f,
                "Swap area needs to be at least {} KiB",
                min_swapsize >> 10
            ),
            Self::IoError { msg } => write!(f, "{}", msg),
            Self::UsageError { msg } => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for MkswapError {}

impl From<std::io::Error> for MkswapError {
    fn from(e: std::io::Error) -> Self {
        MkswapError::IoError { msg: e.to_string() }
    }
}

#[derive(Clone, Copy)]
enum Endian {
    Native,
    Little,
    Big,
}

impl Endian {
    // Converts a native-endian value to this endianness.
    fn convert(&self, value: u32) -> u32 {
        match self {
            Self::Native => value,
            Self::Little => value.to_le(),
            Self::Big => value.to_be(),
        }
    }
}

#[repr(C)]
struct SwapHeader {
    bootbits: [i8; 1024],
    version: u32,
    last_page: u32,
    nr_badpages: u32,
    uuid: [u8; UUID_LENGTH],
    label: [u8; SWAP_LABEL_LENGTH],
    padding: [u32; 117],
    badpages: [u32; 1],
}

impl SwapHeader {
    fn new() -> Self {
        Self {
            bootbits: [0; BOOTBITS_SIZE],
            version: SWAP_VERSION,
            last_page: 0,
            nr_badpages: 0,
            uuid: [0; SWAP_UUID_LENGTH],
            label: [0; SWAP_LABEL_LENGTH],
            padding: [0; 117],
            badpages: [0],
        }
    }

    fn label(mut self, swaplabel: &str) -> Result<Self, MkswapError> {
        if swaplabel.len() > SWAP_LABEL_LENGTH {
            return Err(MkswapError::TooLongLabel);
        }
        let label_bytes = swaplabel.as_bytes();
        let lblen = label_bytes.len().min(SWAP_LABEL_LENGTH);
        self.label[..lblen].copy_from_slice(&label_bytes[..lblen]);

        Ok(self)
    }

    fn uuid(mut self, uuid: Uuid) -> Self {
        self.uuid = *uuid.as_bytes();
        self
    }

    fn pages(mut self, pages: u32) -> Result<Self, MkswapError> {
        if pages < MIN_SWAP_PAGES {
            return Err(MkswapError::TooFewPages { pages });
        }
        self.last_page = pages - 1;
        Ok(self)
    }

    fn nr_badpages(mut self, badpages: &[u32], pagesize: usize) -> Result<Self, MkswapError> {
        // space between swap signature and start of badpages
        let max_badpages = ((pagesize - SWAP_SIGNATURE_SZ)
            - std::mem::offset_of!(SwapHeader, badpages))
            / size_of::<u32>();

        if badpages.len() > max_badpages {
            return Err(MkswapError::MaxBadPagesExceeded { max_badpages });
        }

        self.nr_badpages = badpages.len() as u32;
        Ok(self)
    }

    // Sets the endianness of all relevant fields
    // (version, nr_badpages, last_page).
    // Should be used last, after the fields are set
    fn set_endian(mut self, endianness: Endian) -> Self {
        self.version = endianness.convert(self.version);
        self.last_page = endianness.convert(self.last_page);
        self.nr_badpages = endianness.convert(self.nr_badpages);
        self
    }

    // Writes header fields into a signature page i.e. a buffer of size 'pagesize'
    fn write_to<W: std::io::Write + std::io::Seek>(
        &self,
        mut writer: W,
        pagesize: usize,
    ) -> std::io::Result<()> {
        writer.write_all(&[0u8; BOOTBITS_SIZE])?;
        writer.write_all(&self.version.to_ne_bytes())?;
        writer.write_all(&self.last_page.to_ne_bytes())?;
        writer.write_all(&self.nr_badpages.to_ne_bytes())?;
        writer.write_all(&self.uuid)?;
        writer.write_all(&self.label)?;

        writer.seek(SeekFrom::Start((pagesize - SWAP_SIGNATURE_SZ) as u64))?;
        writer.write_all(SWAP_SIGNATURE)?;
        writer.flush()?;
        Ok(())
    }
}

fn getpagesize() -> Result<usize, std::io::Error> {
    let mut sz = unsafe { sysconf(_SC_PAGESIZE) };
    if sz <= 0 {
        sz = unsafe { sysconf(_SC_PAGE_SIZE) };
    }

    if sz <= 0 {
        return Err(std::io::Error::other(
            "Failed to determine page size, please check your system configuration",
        ));
    }

    TryInto::<usize>::try_into(sz as u64).map_err(|_| {
        std::io::Error::other(format!(
            "Page size too large, max page size: {}",
            usize::MAX
        ))
    })
}

fn get_blockdev_size(fd: &File, devname: &str) -> Result<u64, std::io::Error> {
    let mut sz: u64 = 0;
    let err = unsafe { ioctl(fd.as_raw_fd(), BLKGETSIZE64 as c_ulong, &mut sz) };

    if sz == 0 || err < 0 {
        let f_size = File::open(format!("/sys/class/block/{devname}/size"))?;

        let mut reader = BufReader::new(f_size);
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            return Err(std::io::Error::other(format!(
                "empty size file for block device {devname}"
            )));
        }

        let sectors = line.trim().parse::<u64>().map_err(|e| {
            std::io::Error::other(format!(
                "Invalid size value for block device {devname}: {e}"
            ))
        })?;

        // get size in bytes by multiplying value from /sys/class, which is in 512 byte sectors
        match sectors.checked_mul(512) {
            Some(sz) => Ok(sz),
            None => Err(std::io::Error::other(
                "Unable to determine size of block device",
            )),
        }
    } else {
        Ok(sz)
    }
}

fn check_device(
    fd: &File,
    pagesize: usize,
    pages: u32,
    offset: u64,
    verbose: bool,
) -> Result<Vec<u32>, std::io::Error> {
    let mut buf = vec![0u8; pagesize];
    let mut badpages: Vec<u32> = Vec::new();
    for page in 1..pages {
        let bytes = unsafe {
            pread(
                fd.as_raw_fd(),
                buf.as_mut_ptr() as *mut c_void,
                pagesize,
                offset as i64 + (page as i64 * pagesize as i64),
            )
        };
        if bytes < pagesize as isize {
            badpages.push(page);
            if verbose {
                eprintln!("bad page at index {page}");
            }
        }
    }
    Ok(badpages)
}

fn open_device(
    device_path: &Path,
    devname: &str,
    createflag: bool,
    filesize: u64,
) -> Result<File, std::io::Error> {
    let mut options = std::fs::OpenOptions::new();
    let file = match options
        .create(createflag)
        .write(true)
        .read(true)
        .truncate(false)
        .append(false)
        .open(device_path)
    {
        Ok(f) => f,
        Err(e) => {
            return Err(std::io::Error::other(format!(
                "failed to open {devname}: {e}",
            )));
        }
    };

    if createflag {
        let fd = file.as_raw_fd();
        file.set_permissions(Permissions::from_mode(0o600))?;

        // check for COW filesystems
        let stat_fs = fstatfs(file.as_fd())?;
        if stat_fs.filesystem_type() == BTRFS_SUPER_MAGIC {
            let mut flags: c_int = 0;
            let err = unsafe { ioctl(fd, FS_IOC_GETFLAGS as c_ulong, &mut flags) };
            if err < 0 {
                return Err(std::io::Error::last_os_error());
            }

            // set NOCOW to disable copy-on-write for proper swapping
            // without this flag, on COW filesystems, swapon syscall fails
            flags |= FS_NOCOW_FL as c_int;

            let err = unsafe { ioctl(fd.as_raw_fd(), FS_IOC_SETFLAGS as c_ulong, &mut flags) };
            if err < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }

        // fallocate to avoid holes in the created file
        if let Err(e) = fallocate(
            file.as_fd(),
            FallocateFlags::FALLOC_FL_ZERO_RANGE,
            0,
            filesize as i64,
        ) {
            std::io::Error::other(format!(
                "rsmkswap: {}: Fallocate failed: {}",
                device_path.to_string_lossy(),
                e.desc()
            ));
        }
    }

    Ok(file)
}

fn mkswap(matches: &ArgMatches) -> Result<(), MkswapError> {
    let verboseflag = matches.get_flag("verbose");
    let checkflag = matches.get_flag("check");
    let forceflag = matches.get_flag("force");
    let offset = *matches.get_one::<u64>("offset").unwrap_or(&0u64);

    let Some(device) = matches.get_one::<String>("device") else {
        return Err(MkswapError::UsageError {
            msg: String::from(
                "Nowhere to set up swap on?\nTry 'rsmkswap --help' for more information.",
            ),
        });
    };
    let devpath = Path::new(device.as_str());
    let devname = devpath
        .file_name()
        .and_then(|os| os.to_str())
        .unwrap_or_else(|| device.strip_prefix("/dev/").unwrap_or(device));

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

    let sys_pagesize: usize =
        getpagesize().map_err(|e| MkswapError::IoError { msg: e.to_string() })?;
    let pagesize = match matches.get_one::<usize>("pagesize") {
        Some(sz) => {
            if !forceflag
                && (*sz <= size_of::<SwapHeader>() + SWAP_SIGNATURE_SZ || !sz.is_power_of_two())
            {
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

    let min_swapsize = (MIN_SWAP_PAGES as u64).saturating_mul(pagesize as u64);

    let createflag = matches.get_flag("file");
    let filesize = *matches.get_one::<u64>("filesize").unwrap_or(&0);
    if createflag && filesize < min_swapsize {
        return Err(MkswapError::SwapAreaTooSmall { min_swapsize });
    }

    let mut fd = open_device(devpath, devname, createflag, filesize)?;

    let stat = fd.metadata()?;
    if stat.st_uid() != 0 && unsafe { geteuid() } == 0 {
        eprintln!(
            "rsmkswap: {}: insecure file owner {}, fix with: chown 0:0 {}",
            devname,
            stat.st_uid(),
            devpath.display()
        );
    }

    let devsize = if createflag {
        filesize
    } else if stat.file_type().is_block_device() {
        get_blockdev_size(&fd, devname)?
    } else {
        stat.st_size()
    };

    let swapsize = devsize.saturating_sub(offset);
    if swapsize < min_swapsize {
        return Err(MkswapError::SwapAreaTooSmall { min_swapsize });
    }

    let pages: u32 = ((devsize - offset) / pagesize as u64)
        .try_into()
        .map_err(|_| MkswapError::IoError {
            msg: format!(
                "swap area is too large: max size is {} GiB",
                (u32::MAX as usize * pagesize) >> 30
            ),
        })?;

    let badpages = if checkflag {
        check_device(&fd, pagesize, pages, offset, verboseflag)?
    } else {
        Vec::new()
    };

    let hdr = SwapHeader::new()
        .label(label)?
        .pages(pages)?
        .uuid(uuid)
        .nr_badpages(&badpages, pagesize)?
        .set_endian(endianness);

    let mut sigpage = Cursor::new(vec![0u8; pagesize]);
    hdr.write_to(&mut sigpage, pagesize)?;

    if checkflag && !badpages.is_empty() {
        sigpage.seek(SeekFrom::Start(
            std::mem::offset_of!(SwapHeader, badpages) as u64
        ))?;
        for &page in &badpages {
            sigpage.write_all(&endianness.convert(page).to_ne_bytes())?;
        }
    }

    let sigpage = sigpage.into_inner();

    // Skip past bootbits to avoid overwriting data

    fd.seek(SeekFrom::Start(offset + BOOTBITS_SIZE as u64))?;
    fd.write_all(&sigpage[BOOTBITS_SIZE..])?;

    fd.flush()?;
    fd.sync_all()?;

    println!(
        "Setting up swapspace version 1, size = {} KiB ({} bytes)\n{}{}, UUID={}",
        (pages - 1) as usize * (pagesize / 1024),
        (pages - 1) as usize * pagesize,
        if label.is_empty() {
            "no label"
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

fn run(args: &[String]) -> Result<(), MkswapError> {
    let matches = clapp().get_matches_from(args);
    mkswap(&matches)
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if let Err(e) = run(&argv) {
        eprintln!("rsmkswap: error: {}", e);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn test_valid_args() {
        let args = vec![
            String::from("rsmkswap"),
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

        let mut buf = vec![0u8; 4096];

        let mut fd = File::open("swapfile_test").unwrap();
        fd.read_exact(&mut buf).unwrap();
        let _ = std::fs::remove_file("swapfile_test");

        let sig = &buf[4086..];

        assert!(result.is_ok());
        assert_eq!(SWAP_SIGNATURE, sig);
    }

    #[test]
    fn test_invalid_device() {
        let args = vec![
            String::from("mkswap"),
            String::from("/rsmkswaptest/invalid/device"),
        ];
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
            String::from("WAAAYTOOLONGOFALABELFORASWAPFILE"),
        ];
        let result = run(&args);
        assert!(result.err().unwrap().to_string().contains(
            format!("Label is too long, maximum size is {SWAP_LABEL_LENGTH} characters").as_str()
        ));
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
                .contains("Swap area needs to be at least")
        );
        let _ = std::fs::remove_file("swapfile_too_small");
    }
    #[test]
    fn test_all_features() {
        let args = vec![
            String::from("mkswap"),
            String::from("swapfile"),
            String::from("-F"),
            String::from("--size"),
            String::from("45056"),
            String::from("--uuid"),
            String::from("123e4567-e89b-12d3-a456-426614174000"),
            String::from("--label"),
            String::from("SWAPTEST"),
            String::from("--check"),
            String::from("--offset"),
            String::from("4096"),
            String::from("--pagesize"),
            String::from("4096"),
        ];
        let result = run(&args);
        let offset: usize = 4096;

        let mut buf = vec![0u8; 4096 + offset];
        let mut fd = File::open("swapfile").unwrap();
        fd.read_exact(&mut buf).unwrap();
        let sig = &buf[4086 + offset..];
        let _ = std::fs::remove_file("swapfile");

        assert!(result.is_ok());
        assert_eq!(SWAP_SIGNATURE, sig);
    }
}
