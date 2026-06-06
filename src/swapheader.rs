use std::os::{raw::c_char, raw::c_uchar};
use uuid::Uuid;

pub const SWAP_SIGNATURE: &[u8] = "SWAPSPACE2".as_bytes();
pub const SWAP_SIGNATURE_SZ: usize = 10;
pub const SWAP_LABEL_LENGTH: usize = 16;
pub const SWAP_VERSION: u32 = 1;
pub const MIN_SWAP_PAGES: u32 = 10;

#[derive(Debug, Clone)]
pub enum MkswapError {
    TooLongLabel,
    TooFewPages {
        pages: u32,
    },
    MaxBadPagesExceeded {
        bad_pages: usize,
        max_badpages: usize,
    },
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
            Self::MaxBadPagesExceeded {
                bad_pages,
                max_badpages,
            } => write!(f, "Too many bad pages detected: {bad_pages}/{max_badpages}"),
        }
    }
}

impl std::error::Error for MkswapError {}

#[repr(C)]
pub struct SwapHeader {
    bootbits: [c_char; 1024],
    version: u32,
    last_page: u32,
    nr_badpages: u32,
    uuid: [c_uchar; 16],
    volume_name: [c_uchar; SWAP_LABEL_LENGTH],
    padding: [u32; 117],
    badpages: [u32; 1],
}

impl SwapHeader {
    pub fn new() -> Self {
        Self {
            bootbits: [0i8; 1024],
            version: SWAP_VERSION,
            last_page: 0,
            nr_badpages: 0,
            uuid: [0u8; 16],
            volume_name: [0u8; SWAP_LABEL_LENGTH],
            padding: [0u32; 117],
            badpages: [0],
        }
    }

    pub fn label(mut self, label: String) -> Result<Self, MkswapError> {
        if label.len() > SWAP_LABEL_LENGTH {
            return Err(MkswapError::TooLongLabel);
        }
        let label_bytes = label.as_bytes();
        let lblen = label_bytes.len().min(SWAP_LABEL_LENGTH);
        self.volume_name[..lblen].copy_from_slice(&label_bytes[..lblen]);

        Ok(self)
    }

    pub fn uuid(mut self, uuid: Uuid) -> Self {
        self.uuid = *uuid.as_bytes();
        self
    }

    pub fn pages(mut self, pages: u32) -> Result<Self, MkswapError> {
        if pages < MIN_SWAP_PAGES {
            return Err(MkswapError::TooFewPages { pages });
        }
        self.last_page = pages - 1;
        Ok(self)
    }
    pub fn bad_pages(mut self, badpages: Vec<u32>, pagesize: usize) -> Result<Self, MkswapError> {
        self.nr_badpages = (badpages.len() as u32).saturating_sub(1);
        //the max amount of bad pages that can be displayed in the header
        let max_badpages = (pagesize
            - 1024 * size_of::<u8>() //bootbits
            - 120 * size_of::<i32>() //version, last page, badpages vector
            - 32 * size_of::<u8>() //uuid + label
            - SWAP_SIGNATURE_SZ)
            / size_of::<i32>();

        if self.nr_badpages > max_badpages as u32 {
            return Err(MkswapError::MaxBadPagesExceeded {
                bad_pages: self.nr_badpages as usize,
                max_badpages,
            });
        }
        Ok(self)
    }
}
