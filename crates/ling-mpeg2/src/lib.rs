//! Freestanding MPEG-1 / MPEG-2 video decoder (`no_std` + `alloc`) for LingOS.
//!
//! Supported: MPEG-2 Main profile (4:2:0) at any level, frame pictures (progressive and
//! interlaced: field DCT, frame / field / dual-prime prediction) and field pictures (both
//! fields in one [`Decoder::decode`] call or in separate calls; 16x8 and dual-prime
//! prediction), quant_matrix_extension, q_scale_type, intra_vlc_format, alternate_scan,
//! intra_dc_precision 8..11, sequence_extension size bits, closed GOP / broken link; and
//! MPEG-1 (D-pictures are skipped). 4:2:2 / 4:4:4 and the scalable extensions return
//! [`Error::Unsupported`].
//!
//! The IDCT is FFmpeg's "simple" integer IDCT, and dequantisation / mismatch control /
//! motion compensation follow the standard exactly, so the output matches FFmpeg
//! bit for bit when FFmpeg uses its simple IDCT (`-idct simple`).
//!
//! Decoding never panics on corrupt input: every slice error is caught and decoding
//! resumes at the next slice start code. No recursion, no large stack arrays.
//!
//! [`frame_to_rgba`] converts a decoded [`Frame`] to 0xFFRRGGBB (BT.601 limited range),
//! optionally deinterlacing it with an edge-directed, comb-adaptive intra-frame filter.
#![cfg_attr(not(test), no_std)]
extern crate alloc;

mod bits;
mod idct;
mod mc;
mod rgb;
mod slice;
mod tables;

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;
use bits::Bits;

pub use rgb::frame_to_rgba;

/// Decoder error. Slice-level corruption is never reported (it is concealed by skipping to
/// the next slice); these are header-level problems.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Unsupported(&'static str),
    Invalid(&'static str),
}

/// A decoded picture in display order.
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Luma, `width` x `height`, stride `width`.
    pub y: Vec<u8>,
    /// Chroma, `(width+1)/2` x `(height+1)/2`, stride `(width+1)/2`.
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub pts: i64,
    pub progressive: bool,
    pub top_field_first: bool,
    /// Display aspect ratio of the whole frame, reduced (e.g. (4,3)).
    pub dar: (u32, u32),
    /// I picture.
    pub key: bool,
}

pub(crate) const PIC_I: u8 = 1;
pub(crate) const PIC_P: u8 = 2;
pub(crate) const PIC_B: u8 = 3;

pub(crate) const PS_TOP: u8 = 1;
pub(crate) const PS_FRAME: u8 = 3;

#[derive(Clone, Copy)]
pub(crate) struct Seq {
    pub width: u32,
    pub height: u32,
    pub aspect: u8,
    pub frame_rate_code: u8,
    pub mpeg2: bool,
    pub progressive_seq: bool,
    pub low_delay: bool,
    pub disp_w: u32,
    pub disp_h: u32,
    pub frn: u8,
    pub frd: u8,
}

impl Seq {
    const fn empty() -> Seq {
        Seq {
            width: 0,
            height: 0,
            aspect: 0,
            frame_rate_code: 0,
            mpeg2: false,
            progressive_seq: true,
            low_delay: false,
            disp_w: 0,
            disp_h: 0,
            frn: 0,
            frd: 0,
        }
    }
}

#[derive(Clone)]
pub(crate) struct Matrices {
    pub intra: [u8; 64],
    pub non_intra: [u8; 64],
    pub c_intra: [u8; 64],
    pub c_non_intra: [u8; 64],
}

impl Matrices {
    const fn default() -> Matrices {
        Matrices {
            intra: tables::DEFAULT_INTRA,
            non_intra: [16; 64],
            c_intra: tables::DEFAULT_INTRA,
            c_non_intra: [16; 64],
        }
    }
}

/// Picture header + picture coding extension of the picture being decoded.
#[derive(Clone, Copy)]
pub(crate) struct PicHdr {
    pub coding_type: u8,
    pub f_code: [[u8; 2]; 2],
    pub full_pel: [bool; 2],
    pub dc_prec: u8,
    pub structure: u8,
    pub tff: bool,
    pub fpfd: bool,
    pub concealment: bool,
    pub q_scale_type: bool,
    pub intra_vlc: bool,
    pub alt_scan: bool,
    pub progressive_frame: bool,
}

impl PicHdr {
    const fn empty() -> PicHdr {
        PicHdr {
            coding_type: 0,
            f_code: [[15; 2]; 2],
            full_pel: [false; 2],
            dc_prec: 0,
            structure: PS_FRAME,
            tff: true,
            fpfd: true,
            concealment: false,
            q_scale_type: false,
            intra_vlc: false,
            alt_scan: false,
            progressive_frame: true,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Meta {
    pts: i64,
    width: u32,
    height: u32,
    progressive: bool,
    tff: bool,
    key: bool,
    dar: (u32, u32),
}

/// A picture buffer (MB-aligned planes).
pub(crate) struct PicBuf {
    pub y: Vec<u8>,
    pub cb: Vec<u8>,
    pub cr: Vec<u8>,
    pub ys: usize,
    pub yh: usize,
    pub cs: usize,
    pub ch: usize,
    meta: Meta,
}

impl PicBuf {
    fn new(mbw: usize, mbh: usize) -> PicBuf {
        let ys = mbw * 16;
        let yh = mbh * 16;
        PicBuf {
            y: vec![16; ys * yh],
            cb: vec![128; ys * yh / 4],
            cr: vec![128; ys * yh / 4],
            ys,
            yh,
            cs: ys / 2,
            ch: yh / 2,
            meta: Meta::default(),
        }
    }
}

#[derive(Clone, Copy)]
struct FieldPending {
    slot: usize,
    structure: u8,
    coding_type: u8,
    meta: Meta,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Cur {
    /// No picture in progress.
    None,
    /// The current picture is being dropped (missing reference, unsupported, ...).
    Skip,
    /// Slices of the current picture go to `cur_slot`.
    Active,
}

/// MPEG-1 / MPEG-2 video decoder. See the crate docs.
pub struct Decoder {
    pub(crate) seq: Seq,
    seq_valid: bool,
    seq_err: Option<Error>,
    pub(crate) mats: Matrices,
    pub(crate) hdr: PicHdr,
    have_pic_hdr: bool,
    cur: Cur,
    pub(crate) cur_slot: usize,
    cur_type: u8,
    cur_meta: Meta,
    pub(crate) second_field: bool,
    field_pending: Option<FieldPending>,
    pub(crate) mbw: usize,
    pub(crate) mbh: usize,
    pub(crate) bufs: Vec<PicBuf>,
    pub(crate) fwd: Option<usize>,
    pub(crate) bwd: Option<usize>,
    bwd_pending_out: bool,
    closed_gop: bool,
    broken_link: bool,
    out: VecDeque<Frame>,
    pts: i64,
    buf: Vec<u8>,
    // Slice / macroblock state.
    pub(crate) blk: [[i16; 64]; 6],
    pub(crate) rowmask: [u8; 6],
    pub(crate) pred: [u8; 384],
    pub(crate) dc_pred: [i32; 3],
    /// pmv[r][s][t]
    pub(crate) pmv: [[[i32; 2]; 2]; 2],
    pub(crate) qcode: u32,
    pub(crate) prev_dirs: u8,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Decoder {
        Decoder {
            seq: Seq::empty(),
            seq_valid: false,
            seq_err: None,
            mats: Matrices::default(),
            hdr: PicHdr::empty(),
            have_pic_hdr: false,
            cur: Cur::None,
            cur_slot: 0,
            cur_type: 0,
            cur_meta: Meta::default(),
            second_field: false,
            field_pending: None,
            mbw: 0,
            mbh: 0,
            bufs: Vec::new(),
            fwd: None,
            bwd: None,
            bwd_pending_out: false,
            closed_gop: false,
            broken_link: false,
            out: VecDeque::new(),
            pts: 0,
            buf: Vec::new(),
            blk: [[0; 64]; 6],
            rowmask: [0; 6],
            pred: [0; 384],
            dc_pred: [0; 3],
            pmv: [[[0; 2]; 2]; 2],
            qcode: 1,
            prev_dirs: 0,
        }
    }

    /// Sequence header (+ extensions) from Matroska CodecPrivate / MP4 esds; may be empty.
    pub fn with_header(seq: &[u8]) -> Result<Decoder, Error> {
        let mut d = Decoder::new();
        if !seq.is_empty() {
            d.decode(seq, 0)?;
            if !d.seq_valid {
                return Err(Error::Invalid("no sequence header"));
            }
        }
        Ok(d)
    }

    /// Decode one coded picture (or both fields of a frame, or several whole pictures),
    /// possibly preceded by sequence / GOP headers. `pts` is attached to the picture(s).
    pub fn decode(&mut self, data: &[u8], pts: i64) -> Result<(), Error> {
        self.pts = pts;
        self.seq_err = None;
        let mut buf = core::mem::take(&mut self.buf);
        buf.clear();
        buf.extend_from_slice(data);
        self.process(&buf);
        self.end_picture();
        self.buf = buf;
        match self.seq_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Next picture in display order.
    pub fn next_frame(&mut self) -> Option<Frame> {
        self.out.pop_front()
    }

    /// End of stream: output the held-back reference picture.
    pub fn flush(&mut self) {
        self.end_picture();
        if let Some(fp) = self.field_pending.take() {
            self.finish_frame(fp.slot, fp.coding_type, fp.meta);
        }
        if self.bwd_pending_out {
            if let Some(b) = self.bwd {
                self.emit(b);
            }
            self.bwd_pending_out = false;
        }
    }

    /// After a seek: drop references and queued output. Decoding restarts at the next I
    /// picture; B pictures missing a reference are dropped.
    pub fn reset(&mut self) {
        self.cur = Cur::None;
        self.have_pic_hdr = false;
        self.field_pending = None;
        self.fwd = None;
        self.bwd = None;
        self.bwd_pending_out = false;
        self.closed_gop = false;
        self.broken_link = false;
        self.out.clear();
    }

    pub fn width(&self) -> u32 {
        self.seq.width
    }

    pub fn height(&self) -> u32 {
        self.seq.height
    }

    /// Frame rate as (numerator, denominator), (0, 1) if unknown.
    pub fn frame_rate(&self) -> (u32, u32) {
        const FR: [(u32, u32); 9] = [
            (0, 1),
            (24000, 1001),
            (24, 1),
            (25, 1),
            (30000, 1001),
            (30, 1),
            (50, 1),
            (60000, 1001),
            (60, 1),
        ];
        let (n, d) = FR.get(self.seq.frame_rate_code as usize).copied().unwrap_or((0, 1));
        (n * (self.seq.frn as u32 + 1), d * (self.seq.frd as u32 + 1))
    }

    // ------------------------------------------------------------------------------------

    fn process(&mut self, buf: &[u8]) {
        let mut next = find_start(buf, 0);
        while let Some(sc) = next {
            let code = buf[sc + 3];
            let ps = sc + 4;
            next = find_start(buf, ps);
            let pe = next.unwrap_or(buf.len());
            let payload = &buf[ps.min(pe)..pe];
            match code {
                0x00 => {
                    self.end_picture();
                    self.picture_header(payload);
                }
                0x01..=0xAF => {
                    if !self.have_pic_hdr {
                        continue;
                    }
                    if self.cur == Cur::None {
                        self.cur = if self.start_picture() { Cur::Active } else { Cur::Skip };
                    }
                    if self.cur == Cur::Active {
                        let mut bs = Bits::new(payload);
                        let _ = self.decode_slice(&mut bs, code as u32);
                    }
                }
                0xB3 => {
                    self.end_picture();
                    self.sequence_header(payload);
                }
                0xB5 => self.extension(payload),
                0xB8 => {
                    self.end_picture();
                    if payload.len() >= 4 {
                        let mut b = Bits::new(payload);
                        b.skip(25);
                        let closed = b.get1();
                        let broken = b.get1();
                        self.closed_gop = closed;
                        if broken {
                            self.broken_link = true;
                        }
                    }
                }
                0xB7 => self.end_picture(),
                _ => {}
            }
        }
    }

    fn sequence_header(&mut self, p: &[u8]) {
        if p.len() < 8 {
            self.seq_valid = false;
            self.seq_err = Some(Error::Invalid("truncated sequence header"));
            return;
        }
        let mut b = Bits::new(p);
        let w = b.get(12);
        let h = b.get(12);
        let aspect = b.get(4) as u8;
        let frc = b.get(4) as u8;
        b.skip(18 + 1 + 10 + 1);
        let mut m = Matrices::default();
        if b.get1() {
            for i in 0..64 {
                m.intra[tables::ZIGZAG[i] as usize] = b.get(8) as u8;
            }
            m.c_intra = m.intra;
        }
        if b.get1() {
            for i in 0..64 {
                m.non_intra[tables::ZIGZAG[i] as usize] = b.get(8) as u8;
            }
            m.c_non_intra = m.non_intra;
        }
        if b.overrun() {
            self.seq_valid = false;
            self.seq_err = Some(Error::Invalid("truncated sequence header"));
            return;
        }
        sanitize(&mut m);
        self.mats = m;
        self.seq = Seq { width: w, height: h, aspect, frame_rate_code: frc, ..Seq::empty() };
        self.seq_valid = w > 0 && h > 0;
        if !self.seq_valid {
            self.seq_err = Some(Error::Invalid("zero picture size"));
        }
    }

    fn extension(&mut self, p: &[u8]) {
        if p.is_empty() {
            return;
        }
        let mut b = Bits::new(p);
        let id = b.get(4);
        match id {
            1 => {
                // sequence_extension
                if p.len() < 6 {
                    return;
                }
                let _profile_level = b.get(8);
                let progressive = b.get1();
                let chroma = b.get(2) as u8;
                let hse = b.get(2);
                let vse = b.get(2);
                b.skip(12 + 1 + 8);
                let low_delay = b.get1();
                let frn = b.get(2) as u8;
                let frd = b.get(5) as u8;
                self.seq.mpeg2 = true;
                self.seq.progressive_seq = progressive;
                self.seq.width = (self.seq.width & 0xfff) | (hse << 12);
                self.seq.height = (self.seq.height & 0xfff) | (vse << 12);
                self.seq.frn = frn;
                self.seq.frd = frd;
                self.seq.low_delay = low_delay;
                if chroma != 1 {
                    self.seq_valid = false;
                    self.seq_err = Some(Error::Unsupported("only 4:2:0 chroma is supported"));
                }
            }
            2 => {
                // sequence_display_extension
                b.skip(3);
                if b.get1() {
                    b.skip(24);
                }
                let dw = b.get(14);
                b.skip(1);
                let dh = b.get(14);
                if !b.overrun() {
                    self.seq.disp_w = dw;
                    self.seq.disp_h = dh;
                }
            }
            3 => {
                // quant_matrix_extension
                let mut m = self.mats.clone();
                if b.get1() {
                    for i in 0..64 {
                        m.intra[tables::ZIGZAG[i] as usize] = b.get(8) as u8;
                    }
                    m.c_intra = m.intra;
                }
                if b.get1() {
                    for i in 0..64 {
                        m.non_intra[tables::ZIGZAG[i] as usize] = b.get(8) as u8;
                    }
                    m.c_non_intra = m.non_intra;
                }
                if b.get1() {
                    for i in 0..64 {
                        m.c_intra[tables::ZIGZAG[i] as usize] = b.get(8) as u8;
                    }
                }
                if b.get1() {
                    for i in 0..64 {
                        m.c_non_intra[tables::ZIGZAG[i] as usize] = b.get(8) as u8;
                    }
                }
                if !b.overrun() {
                    sanitize(&mut m);
                    self.mats = m;
                }
            }
            5 => {
                self.seq_valid = false;
                self.seq_err = Some(Error::Unsupported("scalable MPEG-2 streams"));
            }
            8 => {
                // picture_coding_extension
                if !self.have_pic_hdr || self.cur != Cur::None || p.len() < 5 {
                    return;
                }
                let h = &mut self.hdr;
                h.f_code[0][0] = b.get(4) as u8;
                h.f_code[0][1] = b.get(4) as u8;
                h.f_code[1][0] = b.get(4) as u8;
                h.f_code[1][1] = b.get(4) as u8;
                h.dc_prec = b.get(2) as u8;
                h.structure = b.get(2) as u8;
                h.tff = b.get1();
                h.fpfd = b.get1();
                h.concealment = b.get1();
                h.q_scale_type = b.get1();
                h.intra_vlc = b.get1();
                h.alt_scan = b.get1();
                let _rff = b.get1();
                let _chroma_420_type = b.get1();
                h.progressive_frame = b.get1();
                h.full_pel = [false; 2];
            }
            _ => {}
        }
    }

    fn picture_header(&mut self, p: &[u8]) {
        self.have_pic_hdr = false;
        if p.len() < 4 {
            return;
        }
        let mut b = Bits::new(p);
        let _tref = b.get(10);
        let ct = b.get(3) as u8;
        let _vbv = b.get(16);
        let mut h = PicHdr::empty();
        h.coding_type = ct;
        if ct == PIC_P || ct == PIC_B {
            h.full_pel[0] = b.get1();
            let f = b.get(3) as u8;
            h.f_code[0] = [f, f];
        }
        if ct == PIC_B {
            h.full_pel[1] = b.get1();
            let f = b.get(3) as u8;
            h.f_code[1] = [f, f];
        }
        self.hdr = h;
        self.have_pic_hdr = true;
    }

    fn dar(&self) -> (u32, u32) {
        let s = &self.seq;
        let (w, h) = (s.width, s.height);
        if s.mpeg2 {
            let nom = match s.aspect {
                2 => (4, 3),
                3 => (16, 9),
                4 => (221, 100),
                _ => return reduce(w, h),
            };
            // FFmpeg's heuristic: honour the display extension only if it yields 4:3 / 16:9.
            if s.disp_w != 0 && s.disp_h != 0 {
                let n = nom.0 as u64 * w as u64 * s.disp_h as u64;
                let d = nom.1 as u64 * h as u64 * s.disp_w as u64;
                if n * 3 == d * 4 {
                    return (4, 3);
                }
                if n * 9 == d * 16 {
                    return (16, 9);
                }
            }
            nom
        } else {
            // MPEG-1 pel aspect ratio (height / width of a pel) * 10000.
            const PAR: [u32; 15] = [
                0, 10000, 6735, 7031, 7615, 8055, 8437, 8935, 9157, 9815, 10255, 10695, 10950,
                11575, 12015,
            ];
            match PAR.get(s.aspect as usize) {
                Some(&p) if p != 0 => {
                    // DAR = w / (h * par); snap to 4:3 / 16:9 within 1 %, else n/1000.
                    let n = w as u64 * 10000;
                    let d = (h as u64 * p as u64).max(1);
                    let near = |a: u64, b: u64| {
                        let x = n * b * 100;
                        let y = d * a * 100;
                        x.abs_diff(y) * 100 <= y
                    };
                    if near(4, 3) {
                        (4, 3)
                    } else if near(16, 9) {
                        (16, 9)
                    } else if p == 10000 {
                        reduce(w, h)
                    } else {
                        let q = ((n * 1000 + d / 2) / d) as u32;
                        reduce(q.max(1), 1000)
                    }
                }
                _ => reduce(w, h),
            }
        }
    }

    /// Make sure the picture buffers match the sequence size. Returns false if impossible.
    fn ensure_layout(&mut self) -> bool {
        let s = self.seq;
        if s.width == 0 || s.height == 0 || s.width > 4096 || s.height > 4096 {
            return false;
        }
        let mbw = s.width.div_ceil(16) as usize;
        let mbh = if s.mpeg2 && !s.progressive_seq {
            2 * s.height.div_ceil(32) as usize
        } else {
            s.height.div_ceil(16) as usize
        };
        if mbw != self.mbw || mbh != self.mbh || self.bufs.len() != 3 {
            // Size change: the held reference is output (with its own size), then dropped.
            if let Some(fp) = self.field_pending.take() {
                self.finish_frame(fp.slot, fp.coding_type, fp.meta);
            }
            if self.bwd_pending_out {
                if let Some(b) = self.bwd {
                    self.emit(b);
                }
                self.bwd_pending_out = false;
            }
            self.fwd = None;
            self.bwd = None;
            self.mbw = mbw;
            self.mbh = mbh;
            self.bufs.clear();
            for _ in 0..3 {
                self.bufs.push(PicBuf::new(mbw, mbh));
            }
        }
        true
    }

    /// Called at the first slice of a picture: choose the target buffer. False = drop it.
    fn start_picture(&mut self) -> bool {
        if !self.seq_valid {
            return false;
        }
        let h = self.hdr;
        if !(PIC_I..=PIC_B).contains(&h.coding_type) || h.structure == 0 {
            return false;
        }
        if !self.seq.mpeg2 && h.structure != PS_FRAME {
            return false;
        }
        if !self.ensure_layout() {
            return false;
        }
        let is_field = h.structure != PS_FRAME;
        if let Some(fp) = self.field_pending.take() {
            if is_field
                && fp.structure != h.structure
                && (fp.coding_type == PIC_B) == (h.coding_type == PIC_B)
            {
                self.cur_slot = fp.slot;
                self.cur_type = fp.coding_type;
                self.cur_meta = fp.meta;
                self.second_field = true;
                return true;
            }
            // Orphan first field: output what we have.
            self.finish_frame(fp.slot, fp.coding_type, fp.meta);
        }
        match h.coding_type {
            PIC_P => {
                if self.bwd.is_none() {
                    return false;
                }
            }
            PIC_B => {
                if self.bwd.is_none() || (self.fwd.is_none() && !self.closed_gop) {
                    return false;
                }
            }
            _ => {}
        }
        let mut slot = 0;
        while Some(slot) == self.fwd || Some(slot) == self.bwd {
            slot += 1;
        }
        self.cur_slot = slot;
        self.cur_type = h.coding_type;
        self.second_field = false;
        self.cur_meta = Meta {
            pts: self.pts,
            width: self.seq.width,
            height: self.seq.height,
            progressive: if self.seq.mpeg2 { h.progressive_frame && !is_field } else { true },
            tff: if is_field { h.structure == PS_TOP } else { h.tff || !self.seq.mpeg2 },
            key: h.coding_type == PIC_I,
            dar: self.dar(),
        };
        true
    }

    /// End of the current picture (frame or field).
    fn end_picture(&mut self) {
        if self.cur == Cur::Active {
            if self.hdr.structure != PS_FRAME && !self.second_field {
                self.field_pending = Some(FieldPending {
                    slot: self.cur_slot,
                    structure: self.hdr.structure,
                    coding_type: self.cur_type,
                    meta: self.cur_meta,
                });
            } else {
                self.finish_frame(self.cur_slot, self.cur_type, self.cur_meta);
            }
        }
        self.cur = Cur::None;
        self.have_pic_hdr = false;
    }

    fn finish_frame(&mut self, slot: usize, ctype: u8, meta: Meta) {
        if slot >= self.bufs.len() {
            return;
        }
        self.bufs[slot].meta = meta;
        if ctype == PIC_B {
            self.emit(slot);
        } else {
            if self.bwd_pending_out {
                if let Some(b) = self.bwd {
                    self.emit(b);
                }
            }
            self.fwd = self.bwd;
            self.bwd = Some(slot);
            self.bwd_pending_out = true;
            if self.seq.low_delay {
                // No B pictures in this sequence: nothing can come before it.
                self.emit(slot);
                self.bwd_pending_out = false;
            }
            if ctype == PIC_I && self.broken_link {
                self.fwd = None;
                self.broken_link = false;
            }
            if ctype == PIC_P {
                self.closed_gop = false;
            }
        }
    }

    fn emit(&mut self, slot: usize) {
        let pb = &self.bufs[slot];
        let m = pb.meta;
        let w = (m.width as usize).min(pb.ys);
        let h = (m.height as usize).min(pb.yh);
        let cw = w.div_ceil(2);
        let ch = h.div_ceil(2);
        let mut y = Vec::with_capacity(w * h);
        for r in 0..h {
            y.extend_from_slice(&pb.y[r * pb.ys..r * pb.ys + w]);
        }
        let mut u = Vec::with_capacity(cw * ch);
        let mut v = Vec::with_capacity(cw * ch);
        for r in 0..ch {
            u.extend_from_slice(&pb.cb[r * pb.cs..r * pb.cs + cw]);
            v.extend_from_slice(&pb.cr[r * pb.cs..r * pb.cs + cw]);
        }
        self.out.push_back(Frame {
            width: w as u32,
            height: h as u32,
            y,
            u,
            v,
            pts: m.pts,
            progressive: m.progressive,
            top_field_first: m.tff,
            dar: m.dar,
            key: m.key,
        });
    }
}

fn sanitize(m: &mut Matrices) {
    for t in [&mut m.intra, &mut m.non_intra, &mut m.c_intra, &mut m.c_non_intra] {
        for v in t.iter_mut() {
            if *v == 0 {
                *v = 1;
            }
        }
    }
}

fn gcd64(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

fn reduce(w: u32, h: u32) -> (u32, u32) {
    let g = gcd64(w as u64, h as u64).max(1) as u32;
    (w / g, (h / g).max(1))
}

/// Position of the next `00 00 01 xx` at or after `from`.
fn find_start(b: &[u8], from: usize) -> Option<usize> {
    let mut j = from;
    while j + 3 < b.len() {
        let c = b[j + 2];
        if c > 1 {
            j += 3;
        } else if c == 1 && b[j] == 0 && b[j + 1] == 0 {
            return Some(j);
        } else {
            j += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests;
