//! Constant tables from ITU-T H.264 (scans, VLCs, quantisation, deblocking).

use alloc::vec;
use alloc::vec::Vec;

/// 4x4 zig-zag (frame) scan: scan position -> raster index.
pub const ZIGZAG4: [u8; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];

/// 8x8 zig-zag (frame) scan: scan position -> raster index.
pub const ZIGZAG8: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21,
    28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61,
    54, 47, 55, 62, 63,
];

/// luma4x4BlkIdx -> (x, y) in 4x4 block units within the macroblock.
pub const BLK_X: [u8; 16] = [0, 1, 0, 1, 2, 3, 2, 3, 0, 1, 0, 1, 2, 3, 2, 3];
pub const BLK_Y: [u8; 16] = [0, 0, 1, 1, 0, 0, 1, 1, 2, 2, 3, 3, 2, 2, 3, 3];
/// (y*4 + x) -> luma4x4BlkIdx
pub const BLK_IDX: [u8; 16] = [0, 1, 4, 5, 2, 3, 6, 7, 8, 9, 12, 13, 10, 11, 14, 15];

pub const DEFAULT_4X4_INTRA: [u8; 16] = [6, 13, 20, 28, 13, 20, 28, 32, 20, 28, 32, 37, 28, 32, 37, 42];
pub const DEFAULT_4X4_INTER: [u8; 16] = [10, 14, 20, 24, 14, 20, 24, 27, 20, 24, 27, 30, 24, 27, 30, 34];
pub const DEFAULT_8X8_INTRA: [u8; 64] = [
    6, 10, 13, 16, 18, 23, 25, 27, 10, 11, 16, 18, 23, 25, 27, 29, 13, 16, 18, 23, 25, 27, 29, 31, 16, 18, 23, 25, 27,
    29, 31, 33, 18, 23, 25, 27, 29, 31, 33, 36, 23, 25, 27, 29, 31, 33, 36, 38, 25, 27, 29, 31, 33, 36, 38, 40, 27, 29,
    31, 33, 36, 38, 40, 42,
];
pub const DEFAULT_8X8_INTER: [u8; 64] = [
    9, 13, 15, 17, 19, 21, 22, 24, 13, 13, 17, 19, 21, 22, 24, 25, 15, 17, 19, 21, 22, 24, 25, 27, 17, 19, 21, 22, 24,
    25, 27, 28, 19, 21, 22, 24, 25, 27, 28, 30, 21, 22, 24, 25, 27, 28, 30, 32, 22, 24, 25, 27, 28, 30, 32, 33, 24, 25,
    27, 28, 30, 32, 33, 35,
];

/// normAdjust4x4 values v[m][0..3] (Eq. 8-315).
pub const NORM4: [[u16; 3]; 6] = [[10, 16, 13], [11, 18, 14], [13, 20, 16], [14, 23, 18], [16, 25, 20], [18, 29, 23]];
/// normAdjust8x8 values v[m][0..6] (Eq. 8-318).
pub const NORM8: [[u16; 6]; 6] = [
    [20, 18, 32, 19, 25, 24],
    [22, 19, 35, 21, 28, 26],
    [26, 23, 42, 24, 33, 31],
    [28, 25, 45, 26, 35, 33],
    [32, 28, 51, 30, 40, 38],
    [36, 32, 58, 34, 46, 43],
];

pub fn norm4(m: usize, pos: usize) -> u16 {
    let (i, j) = (pos >> 2, pos & 3);
    if i % 2 == 0 && j % 2 == 0 {
        NORM4[m][0]
    } else if i % 2 == 1 && j % 2 == 1 {
        NORM4[m][1]
    } else {
        NORM4[m][2]
    }
}

pub fn norm8(m: usize, pos: usize) -> u16 {
    let (i, j) = (pos >> 3, pos & 7);
    let k = if i % 4 == 0 && j % 4 == 0 {
        0
    } else if i % 2 == 1 && j % 2 == 1 {
        1
    } else if i % 4 == 2 && j % 4 == 2 {
        2
    } else if (i % 4 == 0 && j % 2 == 1) || (i % 2 == 1 && j % 4 == 0) {
        3
    } else if (i % 4 == 0 && j % 4 == 2) || (i % 4 == 2 && j % 4 == 0) {
        4
    } else {
        5
    };
    NORM8[m][k]
}

/// QPc as a function of qPi (Table 8-15), 8-bit.
pub const CHROMA_QP: [u8; 52] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 29, 30,
    31, 32, 32, 33, 34, 34, 35, 35, 36, 36, 37, 37, 37, 38, 38, 38, 39, 39, 39, 39,
];

pub fn chroma_qp(qp: i32, offset: i32) -> i32 {
    CHROMA_QP[(qp + offset).clamp(0, 51) as usize] as i32
}

pub const GOLOMB_TO_INTRA_CBP: [u8; 48] = [
    47, 31, 15, 0, 23, 27, 29, 30, 7, 11, 13, 14, 39, 43, 45, 46, 16, 3, 5, 10, 12, 19, 21, 26, 28, 35, 37, 42, 44, 1, 2,
    4, 8, 17, 18, 20, 24, 6, 9, 22, 25, 32, 33, 34, 36, 40, 38, 41,
];
pub const GOLOMB_TO_INTER_CBP: [u8; 48] = [
    0, 16, 1, 2, 4, 8, 32, 3, 5, 10, 12, 15, 47, 7, 11, 13, 14, 6, 9, 31, 35, 37, 42, 44, 33, 34, 36, 40, 39, 43, 45, 46,
    17, 18, 20, 24, 19, 21, 26, 28, 23, 27, 29, 30, 22, 25, 38, 41,
];

// ---- deblocking (Tables 8-16, 8-17) ----
pub const ALPHA: [u8; 52] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 4, 5, 6, 7, 8, 9, 10, 12, 13, 15, 17, 20, 22, 25, 28, 32, 36, 40,
    45, 50, 56, 63, 71, 80, 90, 101, 113, 127, 144, 162, 182, 203, 226, 255, 255,
];
pub const BETA: [u8; 52] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11,
    12, 12, 13, 13, 14, 14, 15, 15, 16, 16, 17, 17, 18, 18,
];
pub const TC0: [[u8; 3]; 52] = [
    [0, 0, 0], [0, 0, 0], [0, 0, 0], [0, 0, 0], [0, 0, 0], [0, 0, 0], [0, 0, 0], [0, 0, 0], [0, 0, 0], [0, 0, 0],
    [0, 0, 0], [0, 0, 0], [0, 0, 0], [0, 0, 0], [0, 0, 0], [0, 0, 0], [0, 0, 0], [0, 0, 1], [0, 0, 1], [0, 0, 1],
    [0, 0, 1], [0, 1, 1], [0, 1, 1], [1, 1, 1], [1, 1, 1], [1, 1, 1], [1, 1, 1], [1, 1, 2], [1, 1, 2], [1, 1, 2],
    [1, 1, 2], [1, 2, 3], [1, 2, 3], [2, 2, 3], [2, 2, 4], [2, 3, 4], [2, 3, 4], [3, 3, 5], [3, 4, 6], [3, 4, 6],
    [4, 5, 7], [4, 5, 8], [4, 6, 9], [5, 7, 10], [6, 8, 11], [6, 8, 13], [7, 10, 14], [8, 11, 16], [9, 12, 18],
    [10, 13, 20], [11, 15, 23], [13, 17, 25],
];

// ---- CAVLC (Tables 9-5, 9-7, 9-8, 9-9, 9-10) ----
pub(crate) const COEFF_TOKEN_LEN: [[u8; 68]; 4] = [
    [
        1, 0, 0, 0, 6, 2, 0, 0, 8, 6, 3, 0, 9, 8, 7, 5, 10, 9, 8, 6, 11, 10, 9, 7, 13, 11, 10, 8, 13, 13, 11, 9, 13, 13,
        13, 10, 14, 14, 13, 11, 14, 14, 14, 13, 15, 15, 14, 14, 15, 15, 15, 14, 16, 15, 15, 15, 16, 16, 16, 15, 16, 16,
        16, 16, 16, 16, 16, 16,
    ],
    [
        2, 0, 0, 0, 6, 2, 0, 0, 6, 5, 3, 0, 7, 6, 6, 4, 8, 6, 6, 4, 8, 7, 7, 5, 9, 8, 8, 6, 11, 9, 9, 6, 11, 11, 11, 7,
        12, 11, 11, 9, 12, 12, 12, 11, 12, 12, 12, 11, 13, 13, 13, 12, 13, 13, 13, 13, 13, 14, 13, 13, 14, 14, 14, 13,
        14, 14, 14, 14,
    ],
    [
        4, 0, 0, 0, 6, 4, 0, 0, 6, 5, 4, 0, 6, 5, 5, 4, 7, 5, 5, 4, 7, 5, 5, 4, 7, 6, 6, 4, 7, 6, 6, 4, 8, 7, 7, 5, 8, 8,
        7, 6, 9, 8, 8, 7, 9, 9, 8, 8, 9, 9, 9, 8, 10, 9, 9, 9, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10,
    ],
    [
        6, 0, 0, 0, 6, 6, 0, 0, 6, 6, 6, 0, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6,
        6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6,
    ],
];
pub(crate) const COEFF_TOKEN_BITS: [[u8; 68]; 4] = [
    [
        1, 0, 0, 0, 5, 1, 0, 0, 7, 4, 1, 0, 7, 6, 5, 3, 7, 6, 5, 3, 7, 6, 5, 4, 15, 6, 5, 4, 11, 14, 5, 4, 8, 10, 13, 4,
        15, 14, 9, 4, 11, 10, 13, 12, 15, 14, 9, 12, 11, 10, 13, 8, 15, 1, 9, 12, 11, 14, 13, 8, 7, 10, 9, 12, 4, 6, 5,
        8,
    ],
    [
        3, 0, 0, 0, 11, 2, 0, 0, 7, 7, 3, 0, 7, 10, 9, 5, 7, 6, 5, 4, 4, 6, 5, 6, 7, 6, 5, 8, 15, 6, 5, 4, 11, 14, 13, 4,
        15, 10, 9, 4, 11, 14, 13, 12, 8, 10, 9, 8, 15, 14, 13, 12, 11, 10, 9, 12, 7, 11, 6, 8, 9, 8, 10, 1, 7, 6, 5, 4,
    ],
    [
        15, 0, 0, 0, 15, 14, 0, 0, 11, 15, 13, 0, 8, 12, 14, 12, 15, 10, 11, 11, 11, 8, 9, 10, 9, 14, 13, 9, 8, 10, 9, 8,
        15, 14, 13, 13, 11, 14, 10, 12, 15, 10, 13, 12, 11, 14, 9, 12, 8, 10, 13, 8, 13, 7, 9, 12, 9, 12, 11, 10, 5, 8,
        7, 6, 1, 4, 3, 2,
    ],
    [
        3, 0, 0, 0, 0, 1, 0, 0, 4, 5, 6, 0, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27,
        28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55,
        56, 57, 58, 59, 60, 61, 62, 63,
    ],
];
pub(crate) const CHROMA_DC_COEFF_TOKEN_LEN: [u8; 20] = [2, 0, 0, 0, 6, 1, 0, 0, 6, 6, 3, 0, 6, 7, 7, 6, 6, 8, 8, 7];
pub(crate) const CHROMA_DC_COEFF_TOKEN_BITS: [u8; 20] = [1, 0, 0, 0, 7, 1, 0, 0, 4, 6, 1, 0, 3, 3, 2, 5, 2, 3, 2, 0];

pub(crate) const TOTAL_ZEROS_LEN: [&[u8]; 15] = [
    &[1, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 9],
    &[3, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 6, 6, 6, 6],
    &[4, 3, 3, 3, 4, 4, 3, 3, 4, 5, 5, 6, 5, 6],
    &[5, 3, 4, 4, 3, 3, 3, 4, 3, 4, 5, 5, 5],
    &[4, 4, 4, 3, 3, 3, 3, 3, 4, 5, 4, 5],
    &[6, 5, 3, 3, 3, 3, 3, 3, 4, 3, 6],
    &[6, 5, 3, 3, 3, 2, 3, 4, 3, 6],
    &[6, 4, 5, 3, 2, 2, 3, 3, 6],
    &[6, 6, 4, 2, 2, 3, 2, 5],
    &[5, 5, 3, 2, 2, 2, 4],
    &[4, 4, 3, 3, 1, 3],
    &[4, 4, 2, 1, 3],
    &[3, 3, 1, 2],
    &[2, 2, 1],
    &[1, 1],
];
pub(crate) const TOTAL_ZEROS_BITS: [&[u8]; 15] = [
    &[1, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 1],
    &[7, 6, 5, 4, 3, 5, 4, 3, 2, 3, 2, 3, 2, 1, 0],
    &[5, 7, 6, 5, 4, 3, 4, 3, 2, 3, 2, 1, 1, 0],
    &[3, 7, 5, 4, 6, 5, 4, 3, 3, 2, 2, 1, 0],
    &[5, 4, 3, 7, 6, 5, 4, 3, 2, 1, 1, 0],
    &[1, 1, 7, 6, 5, 4, 3, 2, 1, 1, 0],
    &[1, 1, 5, 4, 3, 3, 2, 1, 1, 0],
    &[1, 1, 1, 3, 3, 2, 2, 1, 0],
    &[1, 0, 1, 3, 2, 1, 1, 1],
    &[1, 0, 1, 3, 2, 1, 1],
    &[0, 1, 1, 2, 1, 3],
    &[0, 1, 1, 1, 1],
    &[0, 1, 1, 1],
    &[0, 1, 1],
    &[0, 1],
];
pub(crate) const CHROMA_DC_TOTAL_ZEROS_LEN: [&[u8]; 3] = [&[1, 2, 3, 3], &[1, 2, 2], &[1, 1]];
pub(crate) const CHROMA_DC_TOTAL_ZEROS_BITS: [&[u8]; 3] = [&[1, 1, 1, 0], &[1, 1, 0], &[1, 0]];
pub(crate) const RUN_LEN: [&[u8]; 7] = [
    &[1, 1],
    &[1, 2, 2],
    &[2, 2, 2, 2],
    &[2, 2, 2, 3, 3],
    &[2, 2, 3, 3, 3, 3],
    &[2, 3, 3, 3, 3, 3, 3],
    &[3, 3, 3, 3, 3, 3, 3, 4, 5, 6, 7, 8, 9, 10, 11],
];
pub(crate) const RUN_BITS: [&[u8]; 7] = [
    &[1, 0],
    &[1, 1, 0],
    &[3, 2, 1, 0],
    &[3, 2, 1, 1, 0],
    &[3, 2, 3, 2, 1, 0],
    &[3, 0, 1, 3, 2, 5, 4],
    &[7, 6, 5, 4, 3, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1],
];

/// A VLC (codes of up to `bits` bits, given to `build`) decoded by lookup on the next `l1` (<= 8) bits, plus a small
/// second-level table for the long codes sharing a first-level prefix (keeps the tables in L1
/// cache). Entry = (symbol << 8) | length, 0 = invalid; bit 15 set = second-level table index.
pub struct Vlc {
    l1: u32,
    pub table: Vec<u16>,
    /// second-level tables: (offset in `table`, index bits)
    subs: Vec<(u32, u32)>,
}

impl Vlc {
    fn build(bits: u32, codes: &[(u8, u8, u8)]) -> Vlc {
        // codes: (length, code, symbol); symbols < 128
        let l1 = bits.min(8);
        let mut table = vec![0u16; 1 << l1];
        let mut subs: Vec<(u32, u32)> = Vec::new();
        let mut need = [0u32; 256];
        for &(len, code, _) in codes {
            let len = len as u32;
            if len > l1 {
                let p = (code as usize) >> (len - l1);
                need[p] = need[p].max(len - l1);
            }
        }
        for p in 0..(1usize << l1) {
            if need[p] > 0 {
                let base = table.len();
                table.resize(base + (1 << need[p]), 0);
                table[p] = 0x8000 | subs.len() as u16;
                subs.push((base as u32, need[p]));
            }
        }
        for &(len, code, sym) in codes {
            if len == 0 {
                continue;
            }
            let len = len as u32;
            let e = ((sym as u16) << 8) | len as u16;
            let (base, shift) = if len <= l1 {
                ((code as usize) << (l1 - len), l1 - len)
            } else {
                let (b, nb) = subs[(table[(code as usize) >> (len - l1)] & 0x7fff) as usize];
                let rest = (code as usize) & ((1 << (len - l1)) - 1);
                (b as usize + (rest << (nb - (len - l1))), nb - (len - l1))
            };
            for i in 0..(1usize << shift) {
                table[base + i] = e;
            }
        }
        Vlc { l1, table, subs }
    }

    /// Decode from 32 peeked bits: returns (symbol, length); length 0 = invalid code.
    #[inline(always)]
    pub fn lookup(&self, peek: u32) -> (u32, u32) {
        let mut e = self.table[(peek >> (32 - self.l1)) as usize];
        if e & 0x8000 != 0 {
            let (b, nb) = self.subs[(e & 0x7fff) as usize];
            e = self.table[b as usize + ((peek << self.l1) >> (32 - nb)) as usize];
        }
        ((e >> 8) as u32, (e & 0xff) as u32)
    }
}

pub struct CavlcTables {
    /// coeff_token for nC ranges 0..2, 2..4, 4..8, 8..; symbol = total_coeff*4 + trailing_ones
    pub coeff_token: [Vlc; 4],
    pub chroma_dc_coeff_token: Vlc,
    /// total_zeros indexed by TotalCoeff-1
    pub total_zeros: Vec<Vlc>,
    pub chroma_dc_total_zeros: Vec<Vlc>,
    /// run_before indexed by min(zerosLeft,7)-1
    pub run_before: Vec<Vlc>,
}

impl CavlcTables {
    pub fn new() -> CavlcTables {
        let ct = |t: usize, bits: u32| {
            let mut codes = Vec::new();
            for i in 0..68 {
                codes.push((COEFF_TOKEN_LEN[t][i], COEFF_TOKEN_BITS[t][i], i as u8));
            }
            Vlc::build(bits, &codes)
        };
        let coeff_token = [ct(0, 16), ct(1, 14), ct(2, 10), ct(3, 6)];
        let mut codes = Vec::new();
        for i in 0..20 {
            codes.push((CHROMA_DC_COEFF_TOKEN_LEN[i], CHROMA_DC_COEFF_TOKEN_BITS[i], i as u8));
        }
        let chroma_dc_coeff_token = Vlc::build(8, &codes);
        let mk = |lens: &[u8], bits: &[u8], nb: u32| {
            let codes: Vec<(u8, u8, u8)> = lens.iter().zip(bits.iter()).enumerate().map(|(i, (&l, &b))| (l, b, i as u8)).collect();
            Vlc::build(nb, &codes)
        };
        let total_zeros = (0..15).map(|i| mk(TOTAL_ZEROS_LEN[i], TOTAL_ZEROS_BITS[i], 9)).collect();
        let chroma_dc_total_zeros = (0..3).map(|i| mk(CHROMA_DC_TOTAL_ZEROS_LEN[i], CHROMA_DC_TOTAL_ZEROS_BITS[i], 3)).collect();
        let run_before = (0..7).map(|i| mk(RUN_LEN[i], RUN_BITS[i], 11)).collect();
        CavlcTables { coeff_token, chroma_dc_coeff_token, total_zeros, chroma_dc_total_zeros, run_before }
    }
}
