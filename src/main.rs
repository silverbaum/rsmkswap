use std::{
    env,
    fs,
    io, io::Write,
    os::{fd::AsRawFd, linux::fs::MetadataExt},
    path::Path,
    };
use libc::{sysconf, _SC_PAGESIZE, _SC_PAGE_SIZE, fsync};

const SWAP_SIGNATURE: &[u8] = b"SWAPSPACE2";
const SWAP_UUID_LENGTH: usize = 16;
const SWAP_VERSION: u8 = 1;

#[repr(C)]
struct SwapHdr {
    bootbits: [u8; 1024],
    version: u8,
    last_page: u32,
    nr_badpages: u32,
    uuid: [u8; SWAP_UUID_LENGTH],
    volume_name: [char; 16],
    padding: [u32; 117],
    badpages: [u32; 1]
}



fn main() -> io::Result<()> {
    
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        println!("Usage: {} device", args[0]);
        return Ok(());
    }


    let mut pagesize: i64 =  unsafe { sysconf(_SC_PAGESIZE)};
    if pagesize <= 0 {
        pagesize = unsafe {sysconf(_SC_PAGE_SIZE)};
        if pagesize <= 0 {
            panic!("can't determine pagesize\n");
        }
    }


    let devstr = format!("{}", args[1]);
    let dev = Path::new(devstr.as_str());
    

    let mut fd = fs::OpenOptions::new()
                    .write(true)
                    .create(false).truncate(false)
                    .append(false).open(dev)?;
    let stat = fd.metadata()?;

    let pages = stat.st_size() / pagesize as u64;
 
    let mut buf= Box::<[u8]>::new_uninit_slice(pagesize.try_into().unwrap());  //new_zeroed_slice in nightly build might be preferrable
    unsafe {
        buf.as_mut_ptr().write_bytes(0, 4096); //calloc
    }

    let swap_hdr: SwapHdr = SwapHdr { bootbits: [0;1024], version: SWAP_VERSION, last_page: (pages as u32)-1,
         nr_badpages: 0, uuid:[0; 16], volume_name:['0'; 16], padding:[0; 117], badpages:[0; 1]};
    
    buf[1024].write(swap_hdr.version);

    unsafe { 
        let lp = buf[1025..1028].align_to_mut::<u32>();
        lp.1.as_mut_ptr().write(swap_hdr.last_page);
    }

    let mut pos = 0;
    while pos < SWAP_SIGNATURE.len() {
        let _ = buf[pos+4086].write(SWAP_SIGNATURE[pos]);
        pos += 1;
    }

    let buf = unsafe {buf.assume_init()};
    let _ = fd.write(&buf);

    unsafe {
        fsync(fd.as_raw_fd());
    }

    println!("Setting up swapspace version 1, size = {}KiB", (pages-1) * pagesize as u64 / 1024);


    Ok(())
}

