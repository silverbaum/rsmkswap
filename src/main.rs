use std::{
    env, fs, io::{self, BufRead, Write}, os::linux::fs::MetadataExt
    };
use libc::{sysconf, _SC_PAGESIZE, _SC_PAGE_SIZE};
use uuid::Uuid;

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
    volume_name: [u8; 16],
    padding: [u32; 117],
    badpages: [u32; 1]
}

/*
struct MkSwap {
    swap_hdr: SwapHdr,
    sigpage: Box<[u8]>,
    devname: String,
    devstat: Metadata,
    fd: fs::File,
    pages: u128,
    pagesize: i64,
    filesize: u128,
}
*/

/* ensure last_page written into right place in memory */


fn main() -> io::Result<()> {

    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        println!("Usage: {} device", args[0]);
        return Ok(());
    }

    let devstr = format!("{}", args[1]);
    let devname = devstr.strip_prefix("/dev/").unwrap_or(&devstr);
    println!("{devname}\n");

    let dev = std::path::Path::new(devstr.as_str());
    
/*
    let mut fd = fs::OpenOptions::new()
                    .write(true)
                    .create(true).truncate(false)
                    .append(false).open(dev)?;
                */

    // if S_ISBLK(dev) then
    // let fs = unsafe { open(devstr.as_ptr() as *const i8, O_RDWR | O_EXCL)  };
    // let mut fd = unsafe { fs::File::from_raw_fd(fs)};
    // else: basic open
    
    

    //swapsize is different!! should be same as devsize? (swapon)
    let mut fd = fs::File::options().create(true)
                                    .read(true)
                                    .write(true)
                                    .truncate(false)
                                    .append(false)
                                    .open(dev)?;
    //let mut fd = fs::File::create(dev)?;
    let stat = fd.metadata()?;

    let devsize: u32;
  
    /*Read block size from sys/class */
    if stat.st_mode() == 25008 {
        let f_size = fs::File::open(format!("/sys/class/block/{devname}/size"))?;

        //let mut devszstr = String::new();
        //f_size.read_to_string(&mut devszstr)?;
        //devsize = devszstr.parse().expect("Couldnt convert device size to uint_128");

        //horrendous but it may work, returns size in sectors
        let reader = io::BufReader::new(f_size);
        let vec: Vec<Result<u32, _>> = reader.lines().map(|v| v.unwrap().parse()).collect();
        devsize = vec[0].clone().unwrap();
        
    } else {
        devsize = (stat.st_size() as u32)/512;
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
    //4194304
    

    println!("{:?}", stat.permissions());
    println!("{:?}\nlength: {:?}\nst_size(): {:?}\nblocks: {:?}\nblksize: {:?}\n{:?}\n{:?}\nmode: {:?}\ndevsize: {}", 
    stat.file_type(), stat.len(), stat.st_size(), stat.st_blocks(), stat.st_blksize(), stat.st_ino(), stat.st_rdev(), stat.st_mode(), devsize);

    
    assert!(pagesize > 0);
    assert!(devsize > 0);

    
    let pages = (devsize*512) / pagesize as u32;
    let lastpage = pages - 1;

    assert!(pages > 0);
    debug_assert_eq!(pages, ((devsize*512)/4096));

    if stat.st_uid() != 0 {
        println!("{}: {}: insecure file owner {}, fix with: chown 0:0 {}",
            args[0], args[1], stat.st_uid(), args[1]);
    }

    if pages < 10 {
        println!("swap space needs to be at least {}KiB",
                10 * pagesize / 1024);
        return Ok(());
    }

    println!("number of pages: {pages}");
    println!("last page: {}", pages - 1);
    //blks / (ctl.pagesize / 1024)

    let mut buf= Box::<[u8]>::new_uninit_slice(pagesize.try_into().unwrap());  //new_zeroed_slice in nightly build might be preferrable
    
    unsafe {
        buf.as_mut_ptr().write_bytes(0, pagesize as usize); //initialize with memset
    }
    
    let swap_hdr = buf.as_mut_ptr() as *mut SwapHdr;
 

    unsafe {
        (*swap_hdr).version = SWAP_VERSION;

        (*swap_hdr).last_page = lastpage as u32; //overflow?
        /* One possibility is that last_page evaluates as 0 and swapon reads it
         * (swapon gets the size from casting the header pointer to a swap_header
         *  struct and reading from it), multiplies last_page+1 by the pagesize, which results in 0+1*4096 = 4096*/
         /* The failure point is the literal swapon call, which gives the error "Invalid argument" */

        (*swap_hdr).uuid = *Uuid::new_v4().as_bytes();
        

        println!("\nswap_hdr: {:p}\nversion: {:p}\nlast_page: {}, {:p}\nuuid: {:?}, {:p}\n", 
        &(*swap_hdr), &(*swap_hdr).version, (*swap_hdr).last_page, &(*swap_hdr).last_page, (*swap_hdr).uuid, &(*swap_hdr).uuid );

        
    }
        
    /* Swap signature */
    let mut pos = 0;
    while pos < SWAP_SIGNATURE.len() {
        /* check the position pos+pagesize-10 */
        let _ = buf[pos+(pagesize as usize-10)].write(SWAP_SIGNATURE[pos]);
        pos += 1;
    }


    let buf = unsafe {buf.assume_init()};
    //fd.seek(io::SeekFrom::Start(1024))?;
    fd.write_all(&buf)?;
    fd.flush()?;
    fd.sync_all()?;

    println!("Setting up swapspace version 1, size = {}KiB", (((pages-1) * pagesize as u32) / 1024));

    Ok(())
}

/*
    let ainitbuf = unsafe {buf.assume_init()};
    let (_, offsetbuf) = ainitbuf.split_at(1024);
    fd.seek(io::SeekFrom::Start(1024))?;
    fd.write_all(offsetbuf)?;
    fd.flush()?;
    fd.sync_all()?; 
    */