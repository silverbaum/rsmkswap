//SPDX-License-Identifier: MIT
use std::{
    env, fs, io::{self, BufRead, Write}, os::linux::fs::MetadataExt
    };
use libc::{sysconf, _SC_PAGESIZE, _SC_PAGE_SIZE};
use uuid::Uuid;

const SWAP_SIGNATURE: &[u8] = b"SWAPSPACE2";
const SWAP_SIGNATURE_SZ: usize = 10;
const SWAP_UUID_LENGTH: usize = 16;
const SWAP_VERSION: u8 = 1;

#[repr(C)]
struct SwapHeader {
    bootbits: [u8; 1024],
    version: u8,
    last_page: u32,
    nr_badpages: u32,
    uuid: [u8; SWAP_UUID_LENGTH],
    volume_name: [u8; 16],
    padding: [u32; 117],
    badpages: [u32; 1]
}


fn main() -> io::Result<()> {

    let args: Vec<String> = env::args().collect();
    if args.len() < 2  || args.len() > 2 {
        println!("Usage: {} device", args[0]);
        return Ok(());
    }

    let devstr = format!("{}", args[1]);
    let devname = devstr.strip_prefix("/dev/").unwrap_or(&devstr);

    let dev = std::path::Path::new(devstr.as_str());
    

    let mut fd = fs::File::options().create(true)
                                    .write(true)
                                    .truncate(false)
                                    .append(false)
                                    .open(dev)?;

    let stat = fd.metadata()?;

    if stat.st_uid() != 0 {
        println!("{}: {}: insecure file owner {}, fix with: chown 0:0 {}",
            args[0], args[1], stat.st_uid(), args[1]);
    }

    let devsize: u128;
  
    /*For block devices, read block size from sys/class/block */
    if stat.st_mode() == 25008 {
        let f_size = fs::File::open(format!("/sys/class/block/{devname}/size"))?;

        //horrendous but it may work, returns size in sectors
        let reader = io::BufReader::new(f_size);
        let vec: Vec<Result<u128, _>> = reader.lines().map(|v| v.unwrap().parse::<u128>()).collect::<Vec<Result<u128, _>>>();
        devsize = vec[0].clone().unwrap();
        
    } else {
        devsize = (stat.st_size() as u128)/512;
        assert_eq!(stat.st_size(), stat.len());
    }
    

    let mut pagesize: i64 =  unsafe {sysconf(_SC_PAGESIZE)};
    if pagesize <= 0 {
        pagesize = unsafe {sysconf(_SC_PAGE_SIZE)};
        if pagesize <= 0 {
            pagesize = stat.st_blksize() as i64;
            if pagesize <= 0 {
                pagesize = 4096;
            }
        }
    }
    
    assert!(pagesize > 0);
    assert!(devsize > 0);

    let pages = (devsize*512) / pagesize as u128;
    let lastpage = pages - 1;

    if pages < 10 {
        println!("swap space needs to be at least {}KiB",
                10 * pagesize / 1024);
        return Ok(());
    }

    assert!(pages > 0);
    assert!(lastpage > 0);
    debug_assert_eq!(pages, ((devsize*512)/4096));

    

    

    let mut buf= Box::<[u8]>::new_uninit_slice(pagesize as usize);
    
    unsafe {
        buf.as_mut_ptr().write_bytes(0, pagesize as usize); //initialize buffer
    }


    let swap_hdr = buf.as_mut_ptr() as *mut SwapHeader;
 
    unsafe {
        (*swap_hdr).version = SWAP_VERSION;
        (*swap_hdr).last_page = lastpage as u32;
        (*swap_hdr).uuid = *Uuid::new_v4().as_bytes();
    }
        
    /* Swap signature */
    let mut pos = 0;
    while pos < SWAP_SIGNATURE.len() {

        let _ = buf[pos+(pagesize as usize-SWAP_SIGNATURE_SZ)].write(SWAP_SIGNATURE[pos]);
        pos += 1;
    }


    let buf = unsafe {buf.assume_init()};
    fd.write_all(&buf)?;
    fd.flush()?;
    fd.sync_all()?;

    println!("Setting up swapspace version 1, size = {}KiB", (((pages-1) * pagesize as u128) / 1024));

    Ok(())
}
