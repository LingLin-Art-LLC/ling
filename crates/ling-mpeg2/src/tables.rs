//! Constant tables from ISO/IEC 13818-2 Annex B (and 11172-2): scan orders, default
//! quantiser matrices and every variable-length code, expanded at compile time into
//! direct lookup tables (`const fn`, no runtime initialisation, no statics with interior
//! mutability).
//!
//! Small VLC tables are `[u16; 1 << BITS]` indexed by the next `BITS` bits of the stream;
//! each entry is `value << 8 | code_length` (length 0 = invalid code).

/// Zig-zag scan: scan position -> natural (raster) coefficient index.
pub const ZIGZAG: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27,
    20, 13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58,
    59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// Alternate (vertical) scan, used when `alternate_scan` = 1.
pub const ALT_SCAN: [u8; 64] = [
    0, 8, 16, 24, 1, 9, 2, 10, 17, 25, 32, 40, 48, 56, 57, 49, 41, 33, 26, 18, 3, 11, 4, 12, 19,
    27, 34, 42, 50, 58, 35, 43, 51, 59, 20, 28, 5, 13, 6, 14, 21, 29, 36, 44, 52, 60, 37, 45, 53,
    61, 22, 30, 7, 15, 23, 31, 38, 46, 54, 62, 39, 47, 55, 63,
];

/// Default intra quantiser matrix, natural order.
pub const DEFAULT_INTRA: [u8; 64] = [
    8, 16, 19, 22, 26, 27, 29, 34, //
    16, 16, 22, 24, 27, 29, 34, 37, //
    19, 22, 26, 27, 29, 34, 34, 38, //
    22, 22, 26, 27, 29, 34, 37, 40, //
    22, 26, 27, 29, 32, 35, 40, 48, //
    26, 27, 29, 32, 35, 40, 48, 58, //
    26, 27, 29, 34, 38, 46, 56, 69, //
    27, 29, 35, 38, 46, 56, 69, 83,
];

/// Non-linear quantiser scale (q_scale_type = 1), indexed by quantiser_scale_code.
pub const NONLINEAR_QSCALE: [u8; 32] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 18, 20, 22, 24, 28, 32, 36, 40, 44, 48, 52, 56, 64,
    72, 80, 88, 96, 104, 112,
];

const fn build<const N: usize>(bits: u32, codes: &[(u16, u8, u8)]) -> [u16; N] {
    let mut t = [0u16; N];
    let mut k = 0;
    while k < codes.len() {
        let (code, len, val) = codes[k];
        let shift = bits - len as u32;
        let start = (code as usize) << shift;
        let n = 1usize << shift;
        let mut i = 0;
        while i < n {
            t[start + i] = ((val as u16) << 8) | len as u16;
            i += 1;
        }
        k += 1;
    }
    t
}

// ---- B-1 macroblock_address_increment ----
pub const MBA_BITS: u32 = 11;
pub const MBA_STUFFING: u16 = 34;
pub const MBA_ESCAPE: u16 = 35;
pub static MBA: [u16; 2048] = build(
    MBA_BITS,
    &[
        (1, 1, 1),
        (3, 3, 2),
        (2, 3, 3),
        (3, 4, 4),
        (2, 4, 5),
        (3, 5, 6),
        (2, 5, 7),
        (7, 7, 8),
        (6, 7, 9),
        (0xb, 8, 10),
        (0xa, 8, 11),
        (9, 8, 12),
        (8, 8, 13),
        (7, 8, 14),
        (6, 8, 15),
        (0x17, 10, 16),
        (0x16, 10, 17),
        (0x15, 10, 18),
        (0x14, 10, 19),
        (0x13, 10, 20),
        (0x12, 10, 21),
        (0x23, 11, 22),
        (0x22, 11, 23),
        (0x21, 11, 24),
        (0x20, 11, 25),
        (0x1f, 11, 26),
        (0x1e, 11, 27),
        (0x1d, 11, 28),
        (0x1c, 11, 29),
        (0x1b, 11, 30),
        (0x1a, 11, 31),
        (0x19, 11, 32),
        (0x18, 11, 33),
        (0xf, 11, MBA_STUFFING as u8),
        (0x8, 11, MBA_ESCAPE as u8),
    ],
);

// ---- B-2..B-4 macroblock_type ----
pub const MB_QUANT: u8 = 1;
pub const MB_FWD: u8 = 2;
pub const MB_BWD: u8 = 4;
pub const MB_PAT: u8 = 8;
pub const MB_INTRA: u8 = 16;
pub const MBTYPE_BITS: u32 = 6;
pub static MBTYPE_I: [u16; 64] =
    build(MBTYPE_BITS, &[(1, 1, MB_INTRA), (1, 2, MB_INTRA | MB_QUANT)]);
pub static MBTYPE_P: [u16; 64] = build(
    MBTYPE_BITS,
    &[
        (1, 1, MB_FWD | MB_PAT),
        (1, 2, MB_PAT),
        (1, 3, MB_FWD),
        (3, 5, MB_INTRA),
        (2, 5, MB_QUANT | MB_FWD | MB_PAT),
        (1, 5, MB_QUANT | MB_PAT),
        (1, 6, MB_QUANT | MB_INTRA),
    ],
);
pub static MBTYPE_B: [u16; 64] = build(
    MBTYPE_BITS,
    &[
        (2, 2, MB_FWD | MB_BWD),
        (3, 2, MB_FWD | MB_BWD | MB_PAT),
        (2, 3, MB_BWD),
        (3, 3, MB_BWD | MB_PAT),
        (2, 4, MB_FWD),
        (3, 4, MB_FWD | MB_PAT),
        (3, 5, MB_INTRA),
        (2, 5, MB_QUANT | MB_FWD | MB_BWD | MB_PAT),
        (3, 6, MB_QUANT | MB_FWD | MB_PAT),
        (2, 6, MB_QUANT | MB_BWD | MB_PAT),
        (1, 6, MB_QUANT | MB_INTRA),
    ],
);

// ---- B-9 coded_block_pattern (indexed by cbp value) ----
const CBP_CODES: [(u16, u8); 64] = [
    (0x01, 9), (0x0b, 5), (0x09, 5), (0x0d, 6), (0x0d, 4), (0x17, 7), (0x13, 7), (0x1f, 8),
    (0x0c, 4), (0x16, 7), (0x12, 7), (0x1e, 8), (0x13, 5), (0x1b, 8), (0x17, 8), (0x13, 8),
    (0x0b, 4), (0x15, 7), (0x11, 7), (0x1d, 8), (0x11, 5), (0x19, 8), (0x15, 8), (0x11, 8),
    (0x0f, 6), (0x0f, 8), (0x0d, 8), (0x03, 9), (0x0f, 5), (0x0b, 8), (0x07, 8), (0x07, 9),
    (0x0a, 4), (0x14, 7), (0x10, 7), (0x1c, 8), (0x0e, 6), (0x0e, 8), (0x0c, 8), (0x02, 9),
    (0x10, 5), (0x18, 8), (0x14, 8), (0x10, 8), (0x0e, 5), (0x0a, 8), (0x06, 8), (0x06, 9),
    (0x12, 5), (0x1a, 8), (0x16, 8), (0x12, 8), (0x0d, 5), (0x09, 8), (0x05, 8), (0x05, 9),
    (0x0c, 5), (0x08, 8), (0x04, 8), (0x04, 9), (0x07, 3), (0x0a, 5), (0x08, 5), (0x0c, 6),
];
const fn cbp_list() -> [(u16, u8, u8); 64] {
    let mut l = [(0u16, 0u8, 0u8); 64];
    let mut i = 0;
    while i < 64 {
        l[i] = (CBP_CODES[i].0, CBP_CODES[i].1, i as u8);
        i += 1;
    }
    l
}
pub const CBP_BITS: u32 = 9;
pub static CBP: [u16; 512] = build(CBP_BITS, &cbp_list());

// ---- B-10 motion_code (magnitude; sign bit follows for non-zero) ----
pub const MOTION_BITS: u32 = 10;
pub static MOTION: [u16; 1024] = build(
    MOTION_BITS,
    &[
        (1, 1, 0),
        (1, 2, 1),
        (1, 3, 2),
        (1, 4, 3),
        (3, 6, 4),
        (5, 7, 5),
        (4, 7, 6),
        (3, 7, 7),
        (0xb, 9, 8),
        (0xa, 9, 9),
        (9, 9, 10),
        (0x11, 10, 11),
        (0x10, 10, 12),
        (0xf, 10, 13),
        (0xe, 10, 14),
        (0xd, 10, 15),
        (0xc, 10, 16),
    ],
);

// ---- B-12 / B-13 dct_dc_size ----
pub const DC_LUMA_BITS: u32 = 9;
pub static DC_LUMA: [u16; 512] = build(
    DC_LUMA_BITS,
    &[
        (4, 3, 0),
        (0, 2, 1),
        (1, 2, 2),
        (5, 3, 3),
        (6, 3, 4),
        (0xe, 4, 5),
        (0x1e, 5, 6),
        (0x3e, 6, 7),
        (0x7e, 7, 8),
        (0xfe, 8, 9),
        (0x1fe, 9, 10),
        (0x1ff, 9, 11),
    ],
);
pub const DC_CHROMA_BITS: u32 = 10;
pub static DC_CHROMA: [u16; 1024] = build(
    DC_CHROMA_BITS,
    &[
        (0, 2, 0),
        (1, 2, 1),
        (2, 2, 2),
        (6, 3, 3),
        (0xe, 4, 4),
        (0x1e, 5, 5),
        (0x3e, 6, 6),
        (0x7e, 7, 7),
        (0xfe, 8, 8),
        (0x1fe, 9, 9),
        (0x3fe, 10, 10),
        (0x3ff, 10, 11),
    ],
);

// ---- B-14 / B-15 DCT coefficients ----
//
// Entry: `len | run << 8 | level << 16` (len excludes the sign bit). run == DCT_EOB / DCT_ESC
// mark the special codes; len == 0 is an invalid code.
pub const DCT_EOB: u32 = 64;
pub const DCT_ESC: u32 = 65;

/// Number of (run, level) codes per run, run 0..31.
const RUN_COUNTS: [u8; 32] = [
    40, 18, 5, 4, 3, 3, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1,
];

/// Table B-14, (code, length) for run 0 levels 1..40, run 1 levels 1..18, run 2 ...
const B14_CODES: [(u16, u8); 111] = [
    // run 0
    (0x3, 2), (0x4, 4), (0x5, 5), (0x6, 7), (0x26, 8), (0x21, 8), (0xa, 10), (0x1d, 12),
    (0x18, 12), (0x13, 12), (0x10, 12), (0x1a, 13), (0x19, 13), (0x18, 13), (0x17, 13),
    (0x1f, 14), (0x1e, 14), (0x1d, 14), (0x1c, 14), (0x1b, 14), (0x1a, 14), (0x19, 14),
    (0x18, 14), (0x17, 14), (0x16, 14), (0x15, 14), (0x14, 14), (0x13, 14), (0x12, 14),
    (0x11, 14), (0x10, 14), (0x18, 15), (0x17, 15), (0x16, 15), (0x15, 15), (0x14, 15),
    (0x13, 15), (0x12, 15), (0x11, 15), (0x10, 15),
    // run 1
    (0x3, 3), (0x6, 6), (0x25, 8), (0xc, 10), (0x1b, 12), (0x16, 13), (0x15, 13), (0x1f, 15),
    (0x1e, 15), (0x1d, 15), (0x1c, 15), (0x1b, 15), (0x1a, 15), (0x19, 15), (0x13, 16),
    (0x12, 16), (0x11, 16), (0x10, 16),
    // run 2..6
    (0x5, 4), (0x4, 7), (0xb, 10), (0x14, 12), (0x14, 13),
    (0x7, 5), (0x24, 8), (0x1c, 12), (0x13, 13),
    (0x6, 5), (0xf, 10), (0x12, 12),
    (0x7, 6), (0x9, 10), (0x12, 13),
    (0x5, 6), (0x1e, 12), (0x14, 16),
    // run 7..16
    (0x4, 6), (0x15, 12),
    (0x7, 7), (0x11, 12),
    (0x5, 7), (0x11, 13),
    (0x27, 8), (0x10, 13),
    (0x23, 8), (0x1a, 16),
    (0x22, 8), (0x19, 16),
    (0x20, 8), (0x18, 16),
    (0xe, 10), (0x17, 16),
    (0xd, 10), (0x16, 16),
    (0x8, 10), (0x15, 16),
    // run 17..31
    (0x1f, 12), (0x1a, 12), (0x19, 12), (0x17, 12), (0x16, 12),
    (0x1f, 13), (0x1e, 13), (0x1d, 13), (0x1c, 13), (0x1b, 13),
    (0x1f, 16), (0x1e, 16), (0x1d, 16), (0x1c, 16), (0x1b, 16),
];

/// Table B-15 (intra_vlc_format = 1), same layout.
const B15_CODES: [(u16, u8); 111] = [
    // run 0
    (0x02, 2), (0x06, 3), (0x07, 4), (0x1c, 5), (0x1d, 5), (0x05, 6), (0x04, 6), (0x7b, 7),
    (0x7c, 7), (0x23, 8), (0x22, 8), (0xfa, 8), (0xfb, 8), (0xfe, 8), (0xff, 8),
    (0x1f, 14), (0x1e, 14), (0x1d, 14), (0x1c, 14), (0x1b, 14), (0x1a, 14), (0x19, 14),
    (0x18, 14), (0x17, 14), (0x16, 14), (0x15, 14), (0x14, 14), (0x13, 14), (0x12, 14),
    (0x11, 14), (0x10, 14), (0x18, 15), (0x17, 15), (0x16, 15), (0x15, 15), (0x14, 15),
    (0x13, 15), (0x12, 15), (0x11, 15), (0x10, 15),
    // run 1
    (0x02, 3), (0x06, 5), (0x79, 7), (0x27, 8), (0x20, 8), (0x16, 13), (0x15, 13), (0x1f, 15),
    (0x1e, 15), (0x1d, 15), (0x1c, 15), (0x1b, 15), (0x1a, 15), (0x19, 15), (0x13, 16),
    (0x12, 16), (0x11, 16), (0x10, 16),
    // run 2..6
    (0x05, 5), (0x07, 7), (0xfc, 8), (0x0c, 10), (0x14, 13),
    (0x07, 5), (0x26, 8), (0x1c, 12), (0x13, 13),
    (0x06, 6), (0xfd, 8), (0x12, 12),
    (0x07, 6), (0x04, 9), (0x12, 13),
    (0x06, 7), (0x1e, 12), (0x14, 16),
    // run 7..16
    (0x04, 7), (0x15, 12),
    (0x05, 7), (0x11, 12),
    (0x78, 7), (0x11, 13),
    (0x7a, 7), (0x10, 13),
    (0x21, 8), (0x1a, 16),
    (0x25, 8), (0x19, 16),
    (0x24, 8), (0x18, 16),
    (0x05, 9), (0x17, 16),
    (0x07, 9), (0x16, 16),
    (0x0d, 10), (0x15, 16),
    // run 17..31
    (0x1f, 12), (0x1a, 12), (0x19, 12), (0x17, 12), (0x16, 12),
    (0x1f, 13), (0x1e, 13), (0x1d, 13), (0x1c, 13), (0x1b, 13),
    (0x1f, 16), (0x1e, 16), (0x1d, 16), (0x1c, 16), (0x1b, 16),
];

/// Two-level DCT coefficient lookup. Peek 16 bits `v`: if `v >= 0x400` use `a[v >> 8]`
/// (all codes not starting with six zeros are at most 8 bits long), else `b[v]`.
pub struct DctTable {
    pub a: [u32; 256],
    pub b: [u32; 1024],
}

const fn put_code(t: &DctTable, code: u16, len: u8, entry: u32) -> DctTable {
    let mut a = t.a;
    let mut b = t.b;
    let v = (code as u32) << (16 - len as u32);
    let n = 1u32 << (16 - len as u32);
    if v >= 0x400 {
        let mut i = v >> 8;
        while i <= (v + n - 1) >> 8 {
            a[i as usize] = entry;
            i += 1;
        }
    } else {
        let mut i = v;
        while i < v + n {
            b[i as usize] = entry;
            i += 1;
        }
    }
    DctTable { a, b }
}

const fn build_dct(codes: &[(u16, u8); 111], eob: (u16, u8)) -> DctTable {
    let mut t = DctTable { a: [0; 256], b: [0; 1024] };
    let mut k = 0usize;
    let mut run = 0usize;
    while run < 32 {
        let mut lvl = 1u32;
        while lvl <= RUN_COUNTS[run] as u32 {
            let (code, len) = codes[k];
            t = put_code(&t, code, len, len as u32 | (run as u32) << 8 | lvl << 16);
            k += 1;
            lvl += 1;
        }
        run += 1;
    }
    t = put_code(&t, eob.0, eob.1, eob.1 as u32 | DCT_EOB << 8);
    t = put_code(&t, 1, 6, 6 | DCT_ESC << 8);
    t
}

pub static B14: DctTable = build_dct(&B14_CODES, (0x2, 2));
pub static B15: DctTable = build_dct(&B15_CODES, (0x6, 4));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dct_tables_complete_and_prefix_free() {
        // Every 8-bit prefix not starting with 000000 must decode in table A.
        for t in [&B14, &B15] {
            for i in 4..256 {
                assert!(t.a[i] & 0x1f != 0, "hole at a[{i:#x}]");
            }
        }
        assert_eq!(RUN_COUNTS.iter().map(|&c| c as usize).sum::<usize>(), 111);
    }

    #[test]
    fn mba_spot_checks() {
        assert_eq!(MBA[0x400 * 2 - 1 + 1 - 1] >> 8, 1); // "1..." -> 1
        assert_eq!(MBA[0x8], (MBA_ESCAPE << 8) | 11);
        assert_eq!(MBA[0x18], (33 << 8) | 11);
    }
}
