use std::os::{raw::c_char, raw::c_uchar};
use uuid::Uuid;

pub const SWAP_SIGNATURE: &[u8] = "SWAPSPACE2".as_bytes();
pub const SWAP_SIGNATURE_SZ: usize = 10;
pub const SWAP_LABEL_LENGTH: usize = 16;
pub const SWAP_VERSION: u32 = 1;
pub const MIN_SWAP_PAGES: u32 = 10;

#[derive(Debug, Clone)]
pub enum SwapHeaderError {
    TooLongLabel,
    TooFewPages { pages: u32 },
}

impl std::fmt::Display for SwapHeaderError {
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
        }
    }
}
impl std::error::Error for SwapHeaderError {}

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

    pub fn label(mut self, label: String) -> Result<Self, SwapHeaderError> {
        if label.len() > SWAP_LABEL_LENGTH {
            return Err(SwapHeaderError::TooLongLabel);
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

    pub fn pages(mut self, pages: u32) -> Result<Self, SwapHeaderError> {
        if pages < MIN_SWAP_PAGES {
            return Err(SwapHeaderError::TooFewPages { pages });
        }
        self.last_page = pages - 1;
        Ok(self)
    }
}
