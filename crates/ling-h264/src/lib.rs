//! ling-h264: a freestanding H.264 / AVC decoder (no_std + alloc).
//!
//! Supports Constrained Baseline, Main and High profile progressive 8-bit 4:2:0 streams:
//! CAVLC and CABAC, I/P/B slices, intra 4x4/8x8/16x16 and PCM, quarter-pel motion compensation
//! with multiple references, list reordering, spatial and temporal direct modes, explicit and
//! implicit weighted prediction, 8x8 transform with scaling matrices, the deblocking filter,
//! DPB management (sliding window, MMCO, POC types 0/1/2) with display-order output and cropping.
//!
//! Interlaced coding (field pictures, MBAFF), 4:2:2/4:4:4, high bit depth, SP/SI slices,
//! FMO/ASO and data partitioning are rejected with `Error::Unsupported`.

#![no_std]
#![allow(clippy::needless_range_loop, clippy::too_many_arguments, clippy::manual_range_contains)]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod bits;
mod cabac;
mod cabac_se;
mod cabac_tables;
mod cavlc;
mod color;
mod deblock;
pub mod enc;
mod idct;
mod inter;
mod intra;
mod mb;
mod pic;
mod ps;
mod slice;
mod tables;

pub use color::frame_to_rgba;

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;
use bits::BitReader;
use mb::{Dequant, SliceCtx};
use pic::*;
use ps::{Pps, Sps};
use slice::{SliceHeader, SliceType};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// A valid stream using a feature this decoder does not implement.
    Unsupported(&'static str),
    /// A corrupt or truncated stream.
    Invalid(&'static str),
}

/// A decoded picture in display order (YUV 4:2:0, cropped).
#[derive(Clone)]
pub struct Frame {
    /// cropped display size
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub y_stride: usize,
    pub c_stride: usize,
    pub poc: i32,
    /// VUI says BT.709 matrix coefficients (otherwise BT.601)
    pub bt709: bool,
    /// VUI video_full_range_flag
    pub full_range: bool,
}

struct CurState {
    slot: usize,
    sh: SliceHeader,
    sps: Sps,
    slice_count: u32,
    top: i32,
    bottom: i32,
    poc_msb: i32,
    frame_num_offset: i32,
    first_mb_last: u32,
}

pub struct Decoder {
    spss: Vec<Option<Sps>>,
    ppss: Vec<Option<Pps>>,
    nal_len: usize,
    cavlc: Box<tables::CavlcTables>,
    pics: Vec<Pic>,
    fs: FrameState,
    cur: Option<CurState>,
    out: VecDeque<Frame>,
    need_idr: bool,
    next_id: u32,
    rbsp: Vec<u8>,
    dq: Option<(([[u8; 16]; 6], [[u8; 64]; 2]), Box<Dequant>)>,
    // POC / frame_num state
    prev_ref_poc_msb: i32,
    prev_ref_poc_lsb: i32,
    prev_frame_num_offset: i32,
    prev_frame_num: u32,
    prev_ref_frame_num: u32,
    max_long_term_idx: Option<u32>,
    /// dimensions of the pictures in the DPB
    dims: (u32, u32),
    reorder: u32,
    dpb_size: u32,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        Decoder {
            spss: vec![None; 32],
            ppss: vec![None; 256],
            nal_len: 4,
            cavlc: Box::new(tables::CavlcTables::new()),
            pics: Vec::new(),
            fs: FrameState::default(),
            cur: None,
            out: VecDeque::new(),
            need_idr: true,
            next_id: 1,
            rbsp: Vec::new(),
            dq: None,
            prev_ref_poc_msb: 0,
            prev_ref_poc_lsb: 0,
            prev_frame_num_offset: 0,
            prev_frame_num: 0,
            prev_ref_frame_num: 0,
            max_long_term_idx: None,
            dims: (0, 0),
            reorder: 16,
            dpb_size: 16,
        }
    }

    /// Configure from an MP4 avcC box payload (AVCDecoderConfigurationRecord).
    pub fn from_avcc(avcc: &[u8]) -> Result<Self, Error> {
        let mut d = Decoder::new();
        if avcc.len() < 7 || avcc[0] != 1 {
            return Err(Error::Invalid("avcC header"));
        }
        d.nal_len = (avcc[4] & 3) as usize + 1;
        if d.nal_len == 3 {
            return Err(Error::Invalid("avcC NAL length size"));
        }
        let mut p = 5;
        let mut first_err = None;
        for set in 0..2 {
            if p >= avcc.len() {
                return Err(Error::Invalid("truncated avcC"));
            }
            let n = if set == 0 { avcc[p] & 31 } else { avcc[p] } as usize;
            p += 1;
            for _ in 0..n {
                if p + 2 > avcc.len() {
                    return Err(Error::Invalid("truncated avcC"));
                }
                let len = ((avcc[p] as usize) << 8) | avcc[p + 1] as usize;
                p += 2;
                if p + len > avcc.len() {
                    return Err(Error::Invalid("truncated avcC"));
                }
                if let Err(e) = d.decode_nal(&avcc[p..p + len]) {
                    first_err.get_or_insert(e);
                }
                p += len;
            }
        }
        if let Some(e) = first_err {
            return Err(e);
        }
        Ok(d)
    }

    /// Decode one MP4 sample (length-prefixed NAL units). Frames come out in display order.
    pub fn decode_sample(&mut self, sample: &[u8]) -> Result<(), Error> {
        let mut p = 0;
        let mut first_err = None;
        while p + self.nal_len <= sample.len() {
            let mut len = 0usize;
            for i in 0..self.nal_len {
                len = (len << 8) | sample[p + i] as usize;
            }
            p += self.nal_len;
            if len > sample.len() - p {
                first_err.get_or_insert(Error::Invalid("NAL length beyond sample"));
                break;
            }
            if let Err(e) = self.decode_nal(&sample[p..p + len]) {
                first_err.get_or_insert(e);
            }
            p += len;
        }
        // an MP4 sample is one access unit
        self.finish_picture();
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Decode Annex-B bytes (start-code delimited NAL units).
    pub fn decode_annexb(&mut self, data: &[u8]) -> Result<(), Error> {
        let mut first_err = None;
        let mut i = 0;
        let n = data.len();
        // find first start code
        let find = |from: usize| -> Option<usize> {
            let mut k = from;
            while k + 3 <= n {
                if data[k] == 0 && data[k + 1] == 0 && data[k + 2] == 1 {
                    return Some(k);
                }
                k += 1;
            }
            None
        };
        let mut start = match find(0) {
            Some(s) => s + 3,
            None => return Ok(()),
        };
        loop {
            let next = find(start);
            let mut end = next.unwrap_or(n);
            // trailing zero bytes belong to the next start code (zero_byte / trailing_zero_8bits)
            while end > start && data[end - 1] == 0 {
                end -= 1;
            }
            if end > start {
                if let Err(e) = self.decode_nal(&data[start..end]) {
                    first_err.get_or_insert(e);
                }
            }
            match next {
                Some(s) => {
                    start = s + 3;
                }
                None => break,
            }
            i += 1;
            if i > n {
                break;
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Pop the next frame in display order.
    pub fn next_frame(&mut self) -> Option<Frame> {
        self.out.pop_front()
    }

    /// End of stream: finish the current picture and output every frame still held for reordering.
    pub fn flush(&mut self) {
        self.finish_picture();
        self.bump_all();
    }

    // ------------------------------------------------------------------ NAL units

    fn decode_nal(&mut self, nal: &[u8]) -> Result<(), Error> {
        if nal.is_empty() {
            return Ok(());
        }
        if nal[0] & 0x80 != 0 {
            return Err(Error::Invalid("forbidden_zero_bit"));
        }
        let ref_idc = (nal[0] >> 5) & 3;
        let ty = nal[0] & 31;
        let mut rbsp = core::mem::take(&mut self.rbsp);
        bits::unescape(&nal[1..], &mut rbsp);
        let r = match ty {
            1 | 5 => self.decode_slice(ty, ref_idc, &rbsp),
            2..=4 => Err(Error::Unsupported("data partitioning")),
            7 => {
                let mut br = BitReader::new(&rbsp);
                match ps::parse_sps(&mut br) {
                    Ok(sps) => {
                        let id = sps.id as usize;
                        self.spss[id] = Some(sps);
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            }
            8 => {
                let mut br = BitReader::new(&rbsp);
                match ps::parse_pps(&mut br, &self.spss) {
                    Ok(pps) => {
                        let id = pps.id as usize;
                        self.ppss[id] = Some(pps);
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            }
            9 => {
                self.finish_picture();
                Ok(())
            }
            10 | 11 => {
                self.finish_picture();
                Ok(())
            }
            _ => Ok(()),
        };
        self.rbsp = rbsp;
        r
    }

    fn decode_slice(&mut self, nal_type: u8, ref_idc: u8, rbsp: &[u8]) -> Result<(), Error> {
        let mut br = BitReader::new(rbsp);
        let sh = slice::parse_slice_header(&mut br, nal_type, ref_idc, &self.spss, &self.ppss)?;
        if sh.redundant_pic_cnt > 0 {
            return Ok(());
        }
        // a new picture?
        if let Some(c) = &self.cur {
            let o = &c.sh;
            let new = sh.first_mb == 0
                || sh.first_mb <= c.first_mb_last
                || sh.frame_num != o.frame_num
                || sh.pps_id != o.pps_id
                || sh.idr != o.idr
                || (sh.nal_ref_idc == 0) != (o.nal_ref_idc == 0)
                || (sh.idr && sh.idr_pic_id != o.idr_pic_id)
                || sh.poc_lsb != o.poc_lsb
                || sh.delta_poc_bottom != o.delta_poc_bottom
                || sh.delta_poc != o.delta_poc;
            if new {
                self.finish_picture();
            }
        }
        if self.need_idr && !sh.idr && self.cur.is_none() {
            return Ok(());
        }
        let pps = self.ppss[sh.pps_id as usize].clone().ok_or(Error::Invalid("missing pps"))?;
        let sps = self.spss[pps.sps_id as usize].clone().ok_or(Error::Invalid("missing sps"))?;
        if self.cur.is_none() {
            if sh.first_mb != 0 {
                self.need_idr = true;
                return Err(Error::Invalid("picture without its first slice"));
            }
            if let Err(e) = self.start_picture(&sh, &sps) {
                self.need_idr = true;
                return Err(e);
            }
        }
        let r = self.decode_slice_data(&sh, &sps, &pps, rbsp, &mut br);
        if r.is_err() {
            // drop the damaged picture and resynchronise at the next IDR
            if let Some(c) = self.cur.take() {
                self.pics[c.slot].busy = false;
                self.pics[c.slot].reference = REF_NONE;
                self.pics[c.slot].output_needed = false;
            }
            self.need_idr = true;
        }
        r
    }

    fn decode_slice_data(&mut self, sh: &SliceHeader, sps: &Sps, pps: &Pps, rbsp: &[u8], br: &mut BitReader) -> Result<(), Error> {
        let cur = self.cur.as_mut().unwrap();
        if cur.sps.mb_width != sps.mb_width || cur.sps.mb_height != sps.mb_height {
            return Err(Error::Invalid("sps change inside a picture"));
        }
        cur.first_mb_last = sh.first_mb;
        let slice_num = cur.slice_count;
        cur.slice_count += 1;
        let slot = cur.slot;
        let lists = self.build_lists(sh, sps)?;
        // dequantisation tables
        let key = (pps.sl4, pps.sl8);
        if self.dq.as_ref().map(|d| d.0 != key).unwrap_or(true) {
            self.dq = Some((key, Dequant::new(pps)));
        }
        self.fs.slice_params.push(SliceParams {
            disable_deblock: sh.disable_deblock,
            alpha_offset: sh.alpha_offset,
            beta_offset: sh.beta_offset,
        });
        let ids = [
            lists[0].iter().map(|&i| self.pics[i].id).collect::<Vec<u32>>(),
            lists[1].iter().map(|&i| self.pics[i].id).collect::<Vec<u32>>(),
        ];
        let mut pic = core::mem::take(&mut self.pics[slot]);
        while pic.slice_refs.len() < slice_num as usize {
            pic.slice_refs.push([Vec::new(), Vec::new()]);
        }
        pic.slice_refs.push(ids);
        let dq = &self.dq.as_ref().unwrap().1;
        let result = (|| -> Result<(), Error> {
            let mut ctx = SliceCtx::new(sps, pps, sh, &mut pic, &self.pics, &mut self.fs, lists, slice_num, dq)?;
            let total = (sps.mb_width * sps.mb_height) as usize;
            let mut addr = sh.first_mb as usize;
            if pps.cabac {
                br.byte_align();
                let start = br.pos() / 8;
                let mut d = cabac_se::CabacDec {
                    c: cabac::Cabac::new(rbsp, start, sh.slice_type == SliceType::I, sh.cabac_init_idc, sh.qp),
                    len: rbsp.len(),
                };
                loop {
                    if addr >= total {
                        return Err(Error::Invalid("slice runs past the picture"));
                    }
                    ctx.set_mb(addr);
                    let skip = sh.slice_type != SliceType::I && d.skip_flag(&ctx);
                    if skip {
                        ctx.decode_skip()?;
                    } else {
                        ctx.decode_mb(&mut d)?;
                    }
                    if d.c.overrun() {
                        return Err(Error::Invalid("CABAC data overrun"));
                    }
                    addr += 1;
                    ctx.after_mb(addr);
                    if d.end_of_slice() {
                        break;
                    }
                }
            } else {
                let mut d = cavlc::Cavlc { br: BitReader::new(rbsp), t: &self.cavlc };
                d.br.set_pos(br.pos());
                loop {
                    if sh.slice_type != SliceType::I {
                        let run = d.br.ue() as usize;
                        if run > total - addr {
                            return Err(Error::Invalid("mb_skip_run"));
                        }
                        for _ in 0..run {
                            ctx.set_mb(addr);
                            ctx.decode_skip()?;
                            addr += 1;
                            ctx.after_mb(addr);
                        }
                        if !d.br.more_rbsp_data() {
                            break;
                        }
                    }
                    if addr >= total {
                        return Err(Error::Invalid("slice runs past the picture"));
                    }
                    ctx.set_mb(addr);
                    ctx.decode_mb(&mut d)?;
                    if d.br.overrun() {
                        return Err(Error::Invalid("CAVLC data overrun"));
                    }
                    addr += 1;
                    ctx.after_mb(addr);
                    if !d.br.more_rbsp_data() {
                        break;
                    }
                }
            }
            Ok(())
        })();
        self.pics[slot] = pic;
        result
    }

    // ------------------------------------------------------------------ pictures

    fn start_picture(&mut self, sh: &SliceHeader, sps: &Sps) -> Result<(), Error> {
        let dims = (sps.mb_width, sps.mb_height);
        if sh.idr {
            self.need_idr = false;
        }
        if dims != self.dims {
            if !sh.idr {
                self.need_idr = true;
                return Err(Error::Invalid("resolution change without IDR"));
            }
            self.bump_all();
            for p in self.pics.iter_mut() {
                p.reference = REF_NONE;
                p.output_needed = false;
                p.busy = false;
            }
            self.dims = dims;
        }
        self.reorder = sps.reorder_depth();
        self.dpb_size = sps.max_dpb_frames().max(self.reorder).max(1);
        let max_frame_num = sps.max_frame_num();
        // frame_num gap: insert "non-existing" frames (8.2.5.2)
        if !sh.idr && sh.frame_num != self.prev_ref_frame_num && sh.frame_num != (self.prev_ref_frame_num + 1) % max_frame_num {
            let mut fnum = (self.prev_ref_frame_num + 1) % max_frame_num;
            let gap = (sh.frame_num + max_frame_num - fnum) % max_frame_num;
            // only the last max_num_ref_frames ones can survive the sliding window
            let keep = sps.max_num_ref_frames.max(1);
            if gap > keep {
                fnum = (sh.frame_num + max_frame_num - keep) % max_frame_num;
            }
            let mut guard = 0;
            while fnum != sh.frame_num && guard < 32 {
                guard += 1;
                self.sliding_window(sps, fnum);
                let slot = self.alloc_slot(sps)?;
                let id = self.next_id;
                self.next_id = self.next_id.wrapping_add(1);
                let p = &mut self.pics[slot];
                p.busy = true;
                p.non_existing = true;
                p.reference = REF_SHORT;
                p.output_needed = false;
                p.frame_num = fnum;
                p.poc = 0;
                p.id = id;
                p.slice_refs.clear();
                for v in p.mb_intra.iter_mut() {
                    *v = true;
                }
                self.prev_frame_num = fnum;
                self.prev_ref_frame_num = fnum;
                fnum = (fnum + 1) % max_frame_num;
            }
        }
        // picture order count (8.2.1)
        let (top, bottom, poc_msb, fno) = self.compute_poc(sh, sps);
        let slot = self.alloc_slot(sps)?;
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        {
            let p = &mut self.pics[slot];
            p.busy = true;
            p.non_existing = false;
            p.reference = REF_NONE;
            p.output_needed = false;
            p.frame_num = sh.frame_num;
            p.poc = top.min(bottom);
            p.id = id;
            p.long_term_idx = 0;
            p.full_range = sps.full_range;
            p.bt709 = sps.matrix_coefficients == 1;
            p.crop = [sps.crop_left, sps.crop_right, sps.crop_top, sps.crop_bottom];
            for v in p.mb_intra.iter_mut() {
                *v = true;
            }
            for l in 0..2 {
                p.refi[l].fill(-1);
            }
        }
        self.fs.reset(sps.mb_width as usize, sps.mb_height as usize);
        self.cur = Some(CurState { slot, sh: sh.clone(), sps: sps.clone(), slice_count: 0, top, bottom, poc_msb, frame_num_offset: fno, first_mb_last: 0 });
        Ok(())
    }

    fn alloc_slot(&mut self, sps: &Sps) -> Result<usize, Error> {
        let (w, h) = (sps.mb_width as usize, sps.mb_height as usize);
        let slot = match self.pics.iter().position(|p| !p.busy) {
            Some(s) => s,
            None => {
                if self.pics.len() >= 40 {
                    return Err(Error::Invalid("DPB overflow"));
                }
                self.pics.push(Pic::default());
                self.pics.len() - 1
            }
        };
        self.pics[slot].alloc(w, h);
        Ok(slot)
    }

    fn compute_poc(&self, sh: &SliceHeader, sps: &Sps) -> (i32, i32, i32, i32) {
        let max_frame_num = sps.max_frame_num() as i32;
        match sps.poc_type {
            0 => {
                let (prev_msb, prev_lsb) = if sh.idr { (0, 0) } else { (self.prev_ref_poc_msb, self.prev_ref_poc_lsb) };
                let max_lsb = 1i32 << sps.log2_max_poc_lsb;
                let lsb = sh.poc_lsb as i32;
                let msb = if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
                    prev_msb + max_lsb
                } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
                    prev_msb - max_lsb
                } else {
                    prev_msb
                };
                let top = msb + lsb;
                (top, top + sh.delta_poc_bottom, msb, 0)
            }
            1 => {
                let fno = if sh.idr {
                    0
                } else if self.prev_frame_num > sh.frame_num {
                    self.prev_frame_num_offset + max_frame_num
                } else {
                    self.prev_frame_num_offset
                };
                let n = sps.offset_for_ref_frame.len() as i32;
                let mut abs = if n != 0 { fno + sh.frame_num as i32 } else { 0 };
                if sh.nal_ref_idc == 0 && abs > 0 {
                    abs -= 1;
                }
                let mut expected = 0i32;
                if abs > 0 {
                    let delta_cycle: i32 = sps.offset_for_ref_frame.iter().fold(0i32, |a, &b| a.wrapping_add(b));
                    let cycle = (abs - 1) / n;
                    let in_cycle = (abs - 1) % n;
                    expected = cycle.wrapping_mul(delta_cycle);
                    for i in 0..=in_cycle as usize {
                        expected = expected.wrapping_add(sps.offset_for_ref_frame[i]);
                    }
                }
                if sh.nal_ref_idc == 0 {
                    expected = expected.wrapping_add(sps.offset_for_non_ref_pic);
                }
                let top = expected.wrapping_add(sh.delta_poc[0]);
                let bottom = top.wrapping_add(sps.offset_for_top_to_bottom_field).wrapping_add(sh.delta_poc[1]);
                (top, bottom, 0, fno)
            }
            _ => {
                let fno = if sh.idr {
                    0
                } else if self.prev_frame_num > sh.frame_num {
                    self.prev_frame_num_offset + max_frame_num
                } else {
                    self.prev_frame_num_offset
                };
                let t = if sh.idr {
                    0
                } else if sh.nal_ref_idc == 0 {
                    2 * (fno + sh.frame_num as i32) - 1
                } else {
                    2 * (fno + sh.frame_num as i32)
                };
                (t, t, 0, fno)
            }
        }
    }

    /// Sliding window marking (8.2.5.3) before adding a short-term frame with frame_num `fnum`.
    fn sliding_window(&mut self, sps: &Sps, cur_frame_num: u32) {
        let max = sps.max_num_ref_frames.max(1) as usize;
        let max_frame_num = sps.max_frame_num() as i32;
        loop {
            let n = self.pics.iter().filter(|p| p.busy && p.reference != REF_NONE).count();
            if n < max {
                break;
            }
            // remove the short-term frame with the smallest FrameNumWrap
            let mut best: Option<(usize, i32)> = None;
            for (i, p) in self.pics.iter().enumerate() {
                if p.busy && p.reference == REF_SHORT {
                    let w = if p.frame_num > cur_frame_num { p.frame_num as i32 - max_frame_num } else { p.frame_num as i32 };
                    if best.map(|b| w < b.1).unwrap_or(true) {
                        best = Some((i, w));
                    }
                }
            }
            match best {
                Some((i, _)) => self.unref(i),
                None => break,
            }
        }
    }

    fn unref(&mut self, i: usize) {
        let p = &mut self.pics[i];
        p.reference = REF_NONE;
        if !p.output_needed {
            p.busy = false;
        }
    }

    fn build_lists(&mut self, sh: &SliceHeader, sps: &Sps) -> Result<[Vec<usize>; 2], Error> {
        let mut lists = [Vec::new(), Vec::new()];
        if sh.slice_type == SliceType::I {
            return Ok(lists);
        }
        let cur_slot = self.cur.as_ref().unwrap().slot;
        let cur_poc = self.pics[cur_slot].poc;
        let max_frame_num = sps.max_frame_num() as i32;
        let cur_fn = sh.frame_num;
        for (i, p) in self.pics.iter_mut().enumerate() {
            if i != cur_slot && p.busy && p.reference == REF_SHORT {
                p.frame_num_wrap = if p.frame_num > cur_fn { p.frame_num as i32 - max_frame_num } else { p.frame_num as i32 };
            }
        }
        let refs = |r: u8| -> Vec<usize> { (0..self.pics.len()).filter(|&i| i != cur_slot && self.pics[i].busy && self.pics[i].reference == r).collect() };
        let mut short = refs(REF_SHORT);
        let mut long = refs(REF_LONG);
        long.sort_by_key(|&i| self.pics[i].long_term_idx);
        if sh.slice_type == SliceType::P {
            short.sort_by_key(|&i| -self.pics[i].frame_num_wrap);
            lists[0] = short;
            lists[0].extend_from_slice(&long);
        } else {
            short.retain(|&i| !self.pics[i].non_existing);
            let mut before: Vec<usize> = short.iter().copied().filter(|&i| self.pics[i].poc < cur_poc).collect();
            let mut after: Vec<usize> = short.iter().copied().filter(|&i| self.pics[i].poc > cur_poc).collect();
            before.sort_by_key(|&i| -self.pics[i].poc);
            after.sort_by_key(|&i| self.pics[i].poc);
            lists[0] = before.clone();
            lists[0].extend_from_slice(&after);
            lists[0].extend_from_slice(&long);
            lists[1] = after;
            lists[1].extend_from_slice(&before);
            lists[1].extend_from_slice(&long);
            if lists[1].len() > 1 && lists[0] == lists[1] {
                lists[1].swap(0, 1);
            }
        }
        for l in 0..2 {
            let num = sh.num_ref_idx[l] as usize;
            if num == 0 {
                lists[l].clear();
                continue;
            }
            let mut list: Vec<Option<usize>> = lists[l].iter().take(num).map(|&i| Some(i)).collect();
            list.resize(num + 1, None);
            let mut ri = 0usize;
            let mut pred = cur_fn as i32;
            for &(idc, v) in &sh.rplm[l] {
                if ri >= num {
                    return Err(Error::Invalid("too many list modifications"));
                }
                let found;
                if idc < 2 {
                    let abs = v as i32 + 1;
                    if abs > max_frame_num {
                        return Err(Error::Invalid("abs_diff_pic_num"));
                    }
                    let mut nw = if idc == 0 { pred - abs } else { pred + abs };
                    if nw < 0 {
                        nw += max_frame_num;
                    } else if nw >= max_frame_num {
                        nw -= max_frame_num;
                    }
                    pred = nw;
                    let pn = if nw > cur_fn as i32 { nw - max_frame_num } else { nw };
                    found = (0..self.pics.len()).find(|&i| i != cur_slot && self.pics[i].busy && self.pics[i].reference == REF_SHORT && self.pics[i].frame_num_wrap == pn);
                    let f = found.ok_or(Error::Invalid("list modification: missing short-term picture"))?;
                    for c in (ri + 1..=num).rev() {
                        list[c] = list[c - 1];
                    }
                    list[ri] = Some(f);
                    ri += 1;
                    let mut n = ri;
                    for c in ri..=num {
                        let keep = match list[c] {
                            Some(i) => !(self.pics[i].reference == REF_SHORT && self.pics[i].frame_num_wrap == pn),
                            None => true,
                        };
                        if keep {
                            list[n] = list[c];
                            n += 1;
                        }
                    }
                } else {
                    found = (0..self.pics.len()).find(|&i| i != cur_slot && self.pics[i].busy && self.pics[i].reference == REF_LONG && self.pics[i].long_term_idx == v);
                    let f = found.ok_or(Error::Invalid("list modification: missing long-term picture"))?;
                    for c in (ri + 1..=num).rev() {
                        list[c] = list[c - 1];
                    }
                    list[ri] = Some(f);
                    ri += 1;
                    let mut n = ri;
                    for c in ri..=num {
                        let keep = match list[c] {
                            Some(i) => !(self.pics[i].reference == REF_LONG && self.pics[i].long_term_idx == v),
                            None => true,
                        };
                        if keep {
                            list[n] = list[c];
                            n += 1;
                        }
                    }
                }
            }
            list.truncate(num);
            // conceal missing entries with the first valid one
            let fallback = list.iter().flatten().next().copied().or_else(|| lists[l].first().copied());
            let fb = match fallback {
                Some(f) => f,
                None => return Err(Error::Invalid("empty reference list")),
            };
            lists[l] = list.into_iter().map(|e| e.unwrap_or(fb)).collect();
        }
        Ok(lists)
    }

    /// Finish the picture being decoded: deblock, mark, store, output.
    fn finish_picture(&mut self) {
        let c = match self.cur.take() {
            Some(c) => c,
            None => return,
        };
        let slot = c.slot;
        let sh = &c.sh;
        let sps = &c.sps;
        {
            let pic = &mut self.pics[slot];
            if self.fs.slice_params.iter().any(|p| p.disable_deblock != 1) {
                let y0 = self.fs.dbk_next_row;
                deblock::deblock_rows(&mut self.fs, pic, y0, usize::MAX);
            }
            let y0 = self.fs.out_next_row;
            let mbh = pic.planes.h / 16;
            pic.copy_out_rows(y0, mbh);
            pic.planes.extend_edges();
        }
        let mut mmco5 = false;
        // output handling of prior pictures at IDR / MMCO5
        if sh.idr || sh.has_mmco5() {
            if sh.idr && sh.no_output_of_prior_pics {
                for (i, p) in self.pics.iter_mut().enumerate() {
                    if i != slot {
                        p.output_needed = false;
                    }
                }
            }
            self.bump_all_except(slot);
        }
        // reference marking (8.2.5)
        if sh.nal_ref_idc != 0 {
            if sh.idr {
                for (i, p) in self.pics.iter_mut().enumerate() {
                    if i != slot && p.reference != REF_NONE {
                        p.reference = REF_NONE;
                        if !p.output_needed {
                            p.busy = false;
                        }
                    }
                }
                if sh.long_term_reference {
                    self.pics[slot].reference = REF_LONG;
                    self.pics[slot].long_term_idx = 0;
                    self.max_long_term_idx = Some(0);
                } else {
                    self.pics[slot].reference = REF_SHORT;
                    self.max_long_term_idx = None;
                }
            } else {
                let mut cur_long = false;
                if sh.adaptive_marking {
                    let max_frame_num = sps.max_frame_num() as i32;
                    let cur_fn = sh.frame_num as i32;
                    for p in self.pics.iter_mut() {
                        if p.busy && p.reference == REF_SHORT {
                            p.frame_num_wrap = if p.frame_num as i32 > cur_fn { p.frame_num as i32 - max_frame_num } else { p.frame_num as i32 };
                        }
                    }
                    for m in sh.mmcos.clone() {
                        match m.op {
                            1 => {
                                let pn = cur_fn - (m.diff_pic_nums as i32 + 1);
                                if let Some(i) = (0..self.pics.len()).find(|&i| i != slot && self.pics[i].busy && self.pics[i].reference == REF_SHORT && self.pics[i].frame_num_wrap == pn) {
                                    self.unref(i);
                                }
                            }
                            2 => {
                                if let Some(i) = (0..self.pics.len()).find(|&i| i != slot && self.pics[i].busy && self.pics[i].reference == REF_LONG && self.pics[i].long_term_idx == m.long_term_pic_num) {
                                    self.unref(i);
                                }
                            }
                            3 => {
                                let pn = cur_fn - (m.diff_pic_nums as i32 + 1);
                                if let Some(j) = (0..self.pics.len()).find(|&i| i != slot && self.pics[i].busy && self.pics[i].reference == REF_LONG && self.pics[i].long_term_idx == m.long_term_frame_idx) {
                                    self.unref(j);
                                }
                                if let Some(i) = (0..self.pics.len()).find(|&i| i != slot && self.pics[i].busy && self.pics[i].reference == REF_SHORT && self.pics[i].frame_num_wrap == pn) {
                                    self.pics[i].reference = REF_LONG;
                                    self.pics[i].long_term_idx = m.long_term_frame_idx;
                                }
                            }
                            4 => {
                                let max = m.max_long_term_frame_idx_plus1;
                                for i in 0..self.pics.len() {
                                    if i != slot && self.pics[i].busy && self.pics[i].reference == REF_LONG && self.pics[i].long_term_idx + 1 > max {
                                        self.unref(i);
                                    }
                                }
                                self.max_long_term_idx = if max == 0 { None } else { Some(max - 1) };
                            }
                            5 => {
                                for i in 0..self.pics.len() {
                                    if i != slot && self.pics[i].busy && self.pics[i].reference != REF_NONE {
                                        self.unref(i);
                                    }
                                }
                                self.max_long_term_idx = None;
                                mmco5 = true;
                            }
                            6 => {
                                if let Some(j) = (0..self.pics.len()).find(|&i| i != slot && self.pics[i].busy && self.pics[i].reference == REF_LONG && self.pics[i].long_term_idx == m.long_term_frame_idx) {
                                    self.unref(j);
                                }
                                self.pics[slot].reference = REF_LONG;
                                self.pics[slot].long_term_idx = m.long_term_frame_idx;
                                cur_long = true;
                            }
                            _ => {}
                        }
                    }
                } else {
                    self.sliding_window(sps, sh.frame_num);
                }
                if !cur_long {
                    self.pics[slot].reference = REF_SHORT;
                }
                // never keep more references than allowed (corrupt streams)
                let max = sps.max_num_ref_frames.max(1) as usize;
                let mut guard = 0;
                while self.pics.iter().filter(|p| p.busy && p.reference != REF_NONE).count() > max && guard < 40 {
                    guard += 1;
                    let victim = (0..self.pics.len())
                        .filter(|&i| i != slot && self.pics[i].busy && self.pics[i].reference == REF_SHORT)
                        .min_by_key(|&i| self.pics[i].frame_num_wrap);
                    match victim {
                        Some(v) => self.unref(v),
                        None => break,
                    }
                }
            }
        }
        // POC / frame_num state for the next picture
        let mut top = c.top;
        let mut bottom = c.bottom;
        if mmco5 {
            let t = top.min(bottom);
            top -= t;
            bottom -= t;
            self.pics[slot].poc = top.min(bottom);
            self.pics[slot].frame_num = 0;
        }
        let _ = bottom;
        if sh.nal_ref_idc != 0 {
            if mmco5 {
                self.prev_ref_poc_msb = 0;
                self.prev_ref_poc_lsb = top;
            } else {
                self.prev_ref_poc_msb = c.poc_msb;
                self.prev_ref_poc_lsb = sh.poc_lsb as i32;
            }
            self.prev_ref_frame_num = if mmco5 { 0 } else { sh.frame_num };
        }
        self.prev_frame_num_offset = if mmco5 { 0 } else { c.frame_num_offset };
        self.prev_frame_num = if mmco5 { 0 } else { sh.frame_num };
        // store for output
        self.pics[slot].output_needed = true;
        self.pics[slot].busy = true;
        // bumping: DPB fullness and reorder depth
        loop {
            let waiting = self.pics.iter().filter(|p| p.busy && p.output_needed).count() as u32;
            let used = self.pics.iter().filter(|p| p.busy && (p.output_needed || p.reference != REF_NONE)).count() as u32;
            if waiting > self.reorder || (used > self.dpb_size && waiting > 0) {
                if !self.bump_one() {
                    break;
                }
            } else {
                break;
            }
        }
        for p in self.pics.iter_mut() {
            if p.busy && !p.output_needed && p.reference == REF_NONE {
                p.busy = false;
            }
        }
    }

    /// Output the waiting picture with the smallest POC. Returns false if none.
    fn bump_one(&mut self) -> bool {
        let best = (0..self.pics.len()).filter(|&i| self.pics[i].busy && self.pics[i].output_needed).min_by_key(|&i| self.pics[i].poc);
        match best {
            Some(i) => {
                self.output(i);
                true
            }
            None => false,
        }
    }

    fn bump_all(&mut self) {
        while self.bump_one() {}
    }

    fn bump_all_except(&mut self, slot: usize) {
        loop {
            let best = (0..self.pics.len()).filter(|&i| i != slot && self.pics[i].busy && self.pics[i].output_needed).min_by_key(|&i| self.pics[i].poc);
            match best {
                Some(i) => self.output(i),
                None => break,
            }
        }
    }

    fn output(&mut self, i: usize) {
        let p = &mut self.pics[i];
        p.output_needed = false;
        if p.reference == REF_NONE {
            p.busy = false;
        }
        if p.non_existing {
            return;
        }
        let pl = &p.planes;
        let [cl, cr, ct, cb] = p.crop;
        let w = (pl.w as u32).saturating_sub(cl + cr) as usize;
        let h = (pl.h as u32).saturating_sub(ct + cb) as usize;
        let (cl, ct) = (cl as usize, ct as usize);
        let cw = w.div_ceil(2);
        let chh = h.div_ceil(2);
        // normally the frame was filled row by row during decoding
        if let Some(mut f) = p.pending.take() {
            if f.width as usize == w && f.height as usize == h && f.y.len() == w * h && f.u.len() == cw * chh && f.v.len() == cw * chh {
                f.poc = p.poc;
                f.bt709 = p.bt709;
                f.full_range = p.full_range;
                self.out.push_back(f);
                return;
            }
        }
        // otherwise copy it now, appending rows (no zero-fill pass over the new buffers)
        let mut f = Frame {
            width: w as u32,
            height: h as u32,
            y: Vec::with_capacity(w * h),
            u: Vec::with_capacity(cw * chh),
            v: Vec::with_capacity(cw * chh),
            y_stride: w,
            c_stride: cw,
            poc: p.poc,
            bt709: p.bt709,
            full_range: p.full_range,
        };
        for y in 0..h {
            let o = pl.yoff(cl, ct + y);
            f.y.extend_from_slice(&pl.y[o..o + w]);
        }
        for y in 0..chh {
            let o = pl.coff(cl / 2, ct / 2 + y);
            f.u.extend_from_slice(&pl.u[o..o + cw]);
            f.v.extend_from_slice(&pl.v[o..o + cw]);
        }
        self.out.push_back(f);
    }
}

#[cfg(test)]
mod tests;
