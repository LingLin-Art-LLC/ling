//! H.264 / AVC encoder for screen recording (no_std + alloc).
//!
//! Constrained Baseline profile, CAVLC, progressive 4:2:0 8-bit, one
//! reference frame, IDR + P frames. Tuned for desktop content:
//!
//! * P_Skip for macroblocks whose source did not change since the last frame
//!   (a still desktop costs a few bytes per frame), and for any macroblock
//!   whose residual quantises to nothing at the skip motion vector;
//! * full-pel motion search from predicted / neighbouring / previous-frame
//!   candidates, a diamond refinement, and a horizontal + vertical line search
//!   that finds scrolling and dragged windows (the slice keeps the vectors it
//!   found as hints for the following macroblocks);
//! * Intra 4x4 (all nine modes) and Intra 16x16 with SATD mode decisions, so
//!   text stays sharp;
//! * no deblocking filter (sharp edges of text and UI would only be blurred);
//! * independent slices of whole macroblock rows, encoded concurrently: the
//!   caller runs [`Encoder::encode_slice`] for every slice on any cores it
//!   likes between [`Encoder::begin_frame`] and [`Encoder::end_frame`].
//!   Slice encoding never allocates, so it can run on kernel worker cores.
//! * constant QP with a bitrate cap: the QP rises above the configured base
//!   while the stream runs over the cap (a leaky bucket) and falls back after.
//!
//! The reconstruction is the exact picture a conforming decoder produces
//! (checked against this crate's decoder and ffmpeg in the tests).
//!
//! Output: one access unit per frame as length-prefixed NAL units (the MP4
//! `avc1` sample format, 4-byte lengths); [`Encoder::avcc`] gives the
//! matching AVCDecoderConfigurationRecord, [`to_annexb`] converts samples to
//! an Annex-B byte stream.

mod bw;
mod cavlc;
mod dsp;
mod slice;

use alloc::vec;
use alloc::vec::Vec;
use core::cell::UnsafeCell;

pub use slice::SliceStats;

/// Encoder settings.
#[derive(Clone, Debug)]
pub struct EncConfig {
    /// picture size (even; any size, padded to whole macroblocks internally)
    pub width: usize,
    pub height: usize,
    /// nominal frame rate (VUI timing, level choice)
    pub fps: u32,
    /// base quantiser 10..=51: the quality while the bitrate cap is not hit (lower is better)
    pub qp: u8,
    /// the highest QP the bitrate cap may push frames to
    pub max_qp: u8,
    /// bitrate cap in kbit/s (0 = constant QP, no cap)
    pub bitrate_kbps: u32,
    /// frames between keyframes (IDR); 0 = 2 seconds' worth
    pub gop: u32,
    /// slices per picture (0 = 1); more slices = more parallelism, slightly bigger files
    pub slices: usize,
}

impl Default for EncConfig {
    fn default() -> Self {
        EncConfig { width: 1920, height: 1080, fps: 30, qp: 24, max_qp: 38, bitrate_kbps: 10_000, gop: 0, slices: 1 }
    }
}

/// A frame to encode: 0x00RRGGBB pixels (`px[y * stride + x]`), at the
/// configured size or exactly twice it (then 2x2 pixels are averaged).
#[derive(Clone, Copy)]
pub struct Source<'a> {
    pub px: &'a [u32],
    pub stride: usize,
    pub width: usize,
    pub height: usize,
}

#[derive(Clone, Copy)]
struct Fp {
    src: [*mut u8; 3],
    psrc: [*const u8; 3],
    rec: [*mut u8; 3],
    refp: [*const u8; 3],
    mv: *mut [i16; 2],
    pmv: *const [i16; 2],
    kind: *mut u8,
    nnz: *mut [u8; 16],
    nnzc: *mut [u8; 8],
    i4: *mut [u8; 16],
    cqp: *mut u8,
}

struct Planes {
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl Planes {
    fn new(ys: usize, cs: usize, h: usize, fill: u8) -> Self {
        Planes { y: vec![fill; ys * h], u: vec![128; cs * h / 2], v: vec![128; cs * h / 2] }
    }
}

struct Bufs {
    src: Planes,
    psrc: Planes,
    rec: Planes,
    refp: Planes,
    mv: Vec<[i16; 2]>,
    pmv: Vec<[i16; 2]>,
    kind: Vec<u8>,
    nnz: Vec<[u8; 16]>,
    nnzc: Vec<[u8; 8]>,
    i4: Vec<[u8; 16]>,
    cqp: Vec<u8>,
}

/// One slice's output: the RBSP scratch and the escaped NAL (4-byte length first).
struct SliceOut {
    rbsp: Vec<u8>,
    nal: Vec<u8>,
    /// bytes of `nal` in use; 0 = not encoded / overflowed
    len: usize,
    over: bool,
    qp: i32,
    stats: SliceStats,
}

/// Per-frame parameters the slices share.
#[derive(Clone, Copy)]
struct Cur {
    key: bool,
    qp: i32,
    frame_num: u32,
    idr_id: u32,
}

/// One encoded frame.
pub struct Packet<'a> {
    /// length-prefixed NAL units (4-byte big-endian lengths)
    pub data: &'a [u8],
    pub key: bool,
    pub qp: i32,
    pub stats: SliceStats,
}

pub struct Encoder {
    cfg: EncConfig,
    mbw: usize,
    mbh: usize,
    /// (first macroblock row, rows) per slice
    slices: Vec<(usize, usize)>,
    bufs: UnsafeCell<Bufs>,
    outs: Vec<UnsafeCell<SliceOut>>,
    fp: Fp,
    cur: Cur,
    frame_num: u32,
    idr_count: u32,
    since_key: u32,
    force_key: bool,
    /// leaky bucket: bits over the cap (negative = credit)
    bucket: i64,
    rc_qp: i32,
    level: u8,
    sps: Vec<u8>,
    pps: Vec<u8>,
    out: Vec<u8>,
    /// coded_block_pattern -> me(v) code number (intra, inter)
    cbp_code: [[u8; 48]; 2],
}

// Slices touch disjoint rows of the shared buffers (see `encode_slice`).
unsafe impl Sync for Encoder {}
unsafe impl Send for Encoder {}

const LOG2_MAX_FRAME_NUM: u32 = 8;

impl Encoder {
    pub fn new(mut cfg: EncConfig) -> Encoder {
        cfg.width = (cfg.width.max(16) + 1) & !1;
        cfg.height = (cfg.height.max(16) + 1) & !1;
        cfg.fps = cfg.fps.clamp(1, 240);
        cfg.qp = cfg.qp.clamp(10, 51);
        cfg.max_qp = cfg.max_qp.clamp(cfg.qp, 51);
        if cfg.gop == 0 {
            cfg.gop = cfg.fps * 2;
        }
        let mbw = cfg.width.div_ceil(16);
        let mbh = cfg.height.div_ceil(16);
        let n = cfg.slices.clamp(1, mbh);
        let mut slices = Vec::with_capacity(n);
        let mut r = 0;
        for i in 0..n {
            let rows = (mbh - r) / (n - i);
            slices.push((r, rows));
            r += rows;
        }
        let (ys, cs, h) = (mbw * 16, mbw * 8, mbh * 16);
        let mbs = mbw * mbh;
        let bufs = Bufs {
            src: Planes::new(ys, cs, h, 0),
            psrc: Planes::new(ys, cs, h, 0),
            rec: Planes::new(ys, cs, h, 0),
            refp: Planes::new(ys, cs, h, 0),
            mv: vec![[0; 2]; mbs],
            pmv: vec![[0; 2]; mbs],
            kind: vec![0; mbs],
            nnz: vec![[0; 16]; mbs],
            nnzc: vec![[0; 8]; mbs],
            i4: vec![[2; 16]; mbs],
            cqp: vec![255; mbs],
        };
        let outs = slices
            .iter()
            .map(|&(_, rows)| {
                let cap = rows * mbw * 640 + 4096;
                UnsafeCell::new(SliceOut { rbsp: vec![0; cap], nal: vec![0; cap + cap / 2 + 8], len: 0, over: false, qp: 0, stats: SliceStats::default() })
            })
            .collect();
        let mut cbp_code = [[0u8; 48]; 2];
        for i in 0..48 {
            cbp_code[0][crate::tables::GOLOMB_TO_INTRA_CBP[i] as usize] = i as u8;
            cbp_code[1][crate::tables::GOLOMB_TO_INTER_CBP[i] as usize] = i as u8;
        }
        let level = pick_level(mbw * mbh, cfg.fps);
        let mut e = Encoder {
            rc_qp: cfg.qp as i32,
            cfg,
            mbw,
            mbh,
            slices,
            bufs: UnsafeCell::new(bufs),
            outs,
            fp: Fp {
                src: [core::ptr::null_mut(); 3],
                psrc: [core::ptr::null(); 3],
                rec: [core::ptr::null_mut(); 3],
                refp: [core::ptr::null(); 3],
                mv: core::ptr::null_mut(),
                pmv: core::ptr::null(),
                kind: core::ptr::null_mut(),
                nnz: core::ptr::null_mut(),
                nnzc: core::ptr::null_mut(),
                i4: core::ptr::null_mut(),
                cqp: core::ptr::null_mut(),
            },
            cur: Cur { key: true, qp: 26, frame_num: 0, idr_id: 0 },
            frame_num: 0,
            idr_count: 0,
            since_key: 0,
            force_key: true,
            bucket: 0,
            level,
            sps: Vec::new(),
            pps: Vec::new(),
            out: Vec::new(),
            cbp_code,
        };
        e.sps = e.make_sps();
        e.pps = e.make_pps();
        e
    }

    pub fn config(&self) -> &EncConfig {
        &self.cfg
    }

    pub fn slice_count(&self) -> usize {
        self.slices.len()
    }

    /// The SPS NAL unit (header byte + escaped payload).
    pub fn sps(&self) -> &[u8] {
        &self.sps
    }

    pub fn pps(&self) -> &[u8] {
        &self.pps
    }

    /// AVCDecoderConfigurationRecord for an MP4 `avcC` box.
    pub fn avcc(&self) -> Vec<u8> {
        let mut v = vec![1, self.sps[1], self.sps[2], self.sps[3], 0xFF, 0xE1];
        v.extend_from_slice(&(self.sps.len() as u16).to_be_bytes());
        v.extend_from_slice(&self.sps);
        v.push(1);
        v.extend_from_slice(&(self.pps.len() as u16).to_be_bytes());
        v.extend_from_slice(&self.pps);
        v
    }

    /// The next frame will be a keyframe.
    pub fn request_keyframe(&mut self) {
        self.force_key = true;
    }

    /// The QP the next frame will use (rate control state).
    pub fn current_qp(&self) -> i32 {
        self.rc_qp
    }

    /// Start a frame: picks its type and QP. `dt_us` = time since the previous
    /// encoded frame (feeds the bitrate cap).
    pub fn begin_frame(&mut self, dt_us: u64) {
        // rate control: drain the bucket for the elapsed time, then pick the QP
        if self.cfg.bitrate_kbps > 0 {
            let rate = self.cfg.bitrate_kbps as i64 * 1000;
            self.bucket -= rate * dt_us.min(5_000_000) as i64 / 1_000_000;
            // at most half a second of credit; then one QP step per 1/8 s over the cap
            self.bucket = self.bucket.max(-rate / 2);
            let over = if self.bucket > 0 { (self.bucket * 8 / rate) as i32 } else { 0 };
            let want = (self.cfg.qp as i32 + over).clamp(self.cfg.qp as i32, self.cfg.max_qp as i32);
            // move at most 2 steps per frame up, 1 down
            self.rc_qp = if want > self.rc_qp { (self.rc_qp + 2).min(want) } else { (self.rc_qp - 1).max(want) };
        } else {
            self.rc_qp = self.cfg.qp as i32;
        }
        let key = self.force_key || self.since_key >= self.cfg.gop;
        if key {
            self.frame_num = 0;
            self.since_key = 0;
            self.force_key = false;
        }
        self.cur = Cur { key, qp: self.rc_qp, frame_num: self.frame_num, idr_id: self.idr_count & 0xFFFF };
        if key {
            self.idr_count += 1;
        }
        let b = self.bufs.get_mut();
        self.fp = Fp {
            src: [b.src.y.as_mut_ptr(), b.src.u.as_mut_ptr(), b.src.v.as_mut_ptr()],
            psrc: [b.psrc.y.as_ptr(), b.psrc.u.as_ptr(), b.psrc.v.as_ptr()],
            rec: [b.rec.y.as_mut_ptr(), b.rec.u.as_mut_ptr(), b.rec.v.as_mut_ptr()],
            refp: [b.refp.y.as_ptr(), b.refp.u.as_ptr(), b.refp.v.as_ptr()],
            mv: b.mv.as_mut_ptr(),
            pmv: b.pmv.as_ptr(),
            kind: b.kind.as_mut_ptr(),
            nnz: b.nnz.as_mut_ptr(),
            nnzc: b.nnzc.as_mut_ptr(),
            i4: b.i4.as_mut_ptr(),
            cqp: b.cqp.as_mut_ptr(),
        };
        for o in self.outs.iter_mut() {
            let o = o.get_mut();
            o.len = 0;
            o.over = false;
        }
    }

    /// Convert and encode slice `i` of the frame begun by `begin_frame`.
    ///
    /// # Safety
    /// Between `begin_frame` and `end_frame` each slice index may be encoded
    /// by one thread at a time; different slices may run concurrently (they
    /// touch disjoint rows of the shared buffers). The encoder must not move
    /// or be used otherwise meanwhile.
    pub unsafe fn encode_slice(&self, i: usize, src: &Source) {
        let qp = self.cur.qp;
        self.encode_slice_qp(i, src, qp);
    }

    unsafe fn encode_slice_qp(&self, i: usize, src: &Source, qp: i32) {
        let (r0, rows) = self.slices[i];
        self.convert(src, r0, rows);
        let o = &mut *self.outs[i].get();
        slice::encode(self, &self.fp, self.cur, qp, r0, rows, o);
    }

    /// Finish the frame: re-encodes (at a higher QP) any slice that overflowed
    /// its buffer, joins the slices, updates rate control and swaps the
    /// reference. `src` must be the frame given to the slices.
    pub fn end_frame(&mut self, src: &Source) -> Packet<'_> {
        for i in 0..self.slices.len() {
            let mut tries = 0;
            loop {
                let (len, over, qp) = {
                    let o = self.outs[i].get_mut();
                    (o.len, o.over, o.qp)
                };
                if len > 0 && !over {
                    break;
                }
                // not run (caller skipped it) or too big: run here, coarser after an overflow
                let q = if over { (qp + 6).min(51) } else { self.cur.qp };
                if tries > 0 || over {
                    let o = self.outs[i].get_mut();
                    let cap = o.rbsp.len() * 2;
                    o.rbsp.resize(cap, 0);
                    o.nal.resize(cap + cap / 2 + 8, 0);
                }
                unsafe { self.encode_slice_qp(i, src, q) };
                tries += 1;
                if tries > 8 {
                    break;
                }
            }
        }
        let mut stats = SliceStats::default();
        self.out.clear();
        for o in self.outs.iter_mut() {
            let o = o.get_mut();
            self.out.extend_from_slice(&o.nal[..o.len]);
            stats.add(&o.stats);
        }
        let bits = self.out.len() as i64 * 8;
        if self.cfg.bitrate_kbps > 0 {
            self.bucket += bits;
        }
        let b = self.bufs.get_mut();
        core::mem::swap(&mut b.rec, &mut b.refp);
        core::mem::swap(&mut b.src, &mut b.psrc);
        core::mem::swap(&mut b.mv, &mut b.pmv);
        self.frame_num = (self.frame_num + 1) & ((1 << LOG2_MAX_FRAME_NUM) - 1);
        self.since_key += 1;
        Packet { data: &self.out, key: self.cur.key, qp: self.cur.qp, stats }
    }

    /// Encode a whole frame on this thread.
    pub fn encode(&mut self, src: &Source, dt_us: u64) -> Packet<'_> {
        self.begin_frame(dt_us);
        for i in 0..self.slices.len() {
            unsafe { self.encode_slice(i, src) };
        }
        self.end_frame(src)
    }

    /// The reconstructed (decoder-side) picture of the last encoded frame:
    /// (Y, U, V, luma stride, chroma stride), macroblock-aligned.
    pub fn recon(&self) -> (&[u8], &[u8], &[u8], usize, usize) {
        let b = unsafe { &*self.bufs.get() };
        (&b.refp.y, &b.refp.u, &b.refp.v, self.mbw * 16, self.mbw * 8)
    }

    /// The source picture of the last encoded frame, as converted (same layout as `recon`).
    pub fn source(&self) -> (&[u8], &[u8], &[u8], usize, usize) {
        let b = unsafe { &*self.bufs.get() };
        (&b.psrc.y, &b.psrc.u, &b.psrc.v, self.mbw * 16, self.mbw * 8)
    }

    /// BGRX -> Y'CbCr 4:2:0 (BT.709, limited range) for macroblock rows
    /// `r0 .. r0 + rows`, padding to whole macroblocks by edge repetition.
    unsafe fn convert(&self, s: &Source, r0: usize, rows: usize) {
        let (w, h) = (self.cfg.width, self.cfg.height);
        let ys = self.mbw * 16;
        let cs = self.mbw * 8;
        let half = s.width >= w * 2 && s.height >= h * 2;
        let (sw, sh) = (s.width.max(1), s.height.max(1));
        let ycount = rows * 16;
        let y0 = r0 * 16;
        let py = core::slice::from_raw_parts_mut(self.fp.src[0].add(y0 * ys), ycount * ys);
        let pu = core::slice::from_raw_parts_mut(self.fp.src[1].add(y0 / 2 * cs), ycount / 2 * cs);
        let pv = core::slice::from_raw_parts_mut(self.fp.src[2].add(y0 / 2 * cs), ycount / 2 * cs);
        // pixel (x, y) of the picture, edges repeated
        let at = |x: usize, y: usize| -> (i32, i32, i32) {
            let (x, y) = (x.min(w - 1), y.min(h - 1));
            let c = if half {
                let (sx, sy) = (x * 2, y * 2);
                let r0 = sy * s.stride + sx;
                let r1 = r0 + s.stride;
                let p = [s.px.get(r0), s.px.get(r0 + 1), s.px.get(r1), s.px.get(r1 + 1)];
                let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
                for q in p {
                    let q = q.copied().unwrap_or(0);
                    r += q >> 16 & 0xFF;
                    g += q >> 8 & 0xFF;
                    b += q & 0xFF;
                }
                return (((r + 2) >> 2) as i32, ((g + 2) >> 2) as i32, ((b + 2) >> 2) as i32);
            } else if sw == w && sh == h {
                s.px.get(y * s.stride + x).copied().unwrap_or(0)
            } else {
                s.px.get((y * sh / h) * s.stride + x * sw / w).copied().unwrap_or(0)
            };
            ((c >> 16 & 0xFF) as i32, (c >> 8 & 0xFF) as i32, (c & 0xFF) as i32)
        };
        let fast = !half && sw == w && sh == h && s.stride >= w && s.px.len() >= (h - 1) * s.stride + w;
        for cy in 0..ycount / 2 {
            let gy = y0 + cy * 2;
            for cx in 0..cs {
                let gx = cx * 2;
                let (mut sr, mut sg, mut sb) = (0, 0, 0);
                for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                    let (x, y) = (gx + dx, gy + dy);
                    let (r, g, b) = if fast && x < w && y < h {
                        let c = *s.px.get_unchecked(y * s.stride + x);
                        ((c >> 16 & 0xFF) as i32, (c >> 8 & 0xFF) as i32, (c & 0xFF) as i32)
                    } else {
                        at(x, y)
                    };
                    py[(cy * 2 + dy) * ys + x] = ((11966 * r + 40254 * g + 4064 * b + (16 << 16) + 32768) >> 16) as u8;
                    sr += r;
                    sg += g;
                    sb += b;
                }
                pu[cy * cs + cx] = ((-6597 * sr - 22187 * sg + 28784 * sb + (512 << 16) + 131072) >> 18) as u8;
                pv[cy * cs + cx] = ((28784 * sr - 26147 * sg - 2637 * sb + (512 << 16) + 131072) >> 18) as u8;
            }
        }
    }

    fn make_sps(&self) -> Vec<u8> {
        let mut raw = vec![0u8; 128];
        let n = {
            let mut b = bw::Bw::new(&mut raw);
            b.put(66, 8); // profile_idc: Baseline
            b.put(0xC0, 8); // constraint_set0 + set1: Constrained Baseline
            b.put(self.level as u32, 8);
            b.ue(0); // seq_parameter_set_id
            b.ue(LOG2_MAX_FRAME_NUM - 4);
            b.ue(2); // pic_order_cnt_type: from frame_num (no reordering)
            b.ue(1); // max_num_ref_frames
            b.flag(false); // gaps_in_frame_num_value_allowed_flag
            b.ue(self.mbw as u32 - 1);
            b.ue(self.mbh as u32 - 1);
            b.flag(true); // frame_mbs_only_flag
            b.flag(true); // direct_8x8_inference_flag
            let (cr, cb) = ((self.mbw * 16 - self.cfg.width) / 2, (self.mbh * 16 - self.cfg.height) / 2);
            b.flag(cr > 0 || cb > 0);
            if cr > 0 || cb > 0 {
                b.ue(0);
                b.ue(cr as u32);
                b.ue(0);
                b.ue(cb as u32);
            }
            b.flag(true); // vui_parameters_present_flag
            b.flag(false); // aspect_ratio_info_present_flag
            b.flag(false); // overscan_info_present_flag
            b.flag(true); // video_signal_type_present_flag
            b.put(5, 3); // video_format: unspecified
            b.flag(false); // video_full_range_flag: limited
            b.flag(true); // colour_description_present_flag
            b.put(1, 8); // BT.709 primaries
            b.put(1, 8); // BT.709 transfer
            b.put(1, 8); // BT.709 matrix
            b.flag(false); // chroma_loc_info_present_flag
            b.flag(true); // timing_info_present_flag
            b.put(1, 32); // num_units_in_tick
            b.put(self.cfg.fps * 2, 32); // time_scale
            b.flag(false); // fixed_frame_rate_flag (frames may be dropped)
            b.flag(false); // nal_hrd
            b.flag(false); // vcl_hrd
            b.flag(false); // pic_struct_present_flag
            b.flag(true); // bitstream_restriction_flag
            b.flag(true); // motion_vectors_over_pic_boundaries_flag
            b.ue(0); // max_bytes_per_pic_denom
            b.ue(0); // max_bits_per_mb_denom
            b.ue(16); // log2_max_mv_length_horizontal
            b.ue(16); // log2_max_mv_length_vertical
            b.ue(0); // max_num_reorder_frames
            b.ue(1); // max_dec_frame_buffering
            b.trailing();
            b.len()
        };
        nal(0x67, &raw[..n])
    }

    fn make_pps(&self) -> Vec<u8> {
        let mut raw = vec![0u8; 32];
        let n = {
            let mut b = bw::Bw::new(&mut raw);
            b.ue(0); // pic_parameter_set_id
            b.ue(0); // seq_parameter_set_id
            b.flag(false); // entropy_coding_mode_flag: CAVLC
            b.flag(false); // bottom_field_pic_order_in_frame_present_flag
            b.ue(0); // num_slice_groups_minus1
            b.ue(0); // num_ref_idx_l0_default_active_minus1
            b.ue(0); // num_ref_idx_l1_default_active_minus1
            b.flag(false); // weighted_pred_flag
            b.put(0, 2); // weighted_bipred_idc
            b.se(0); // pic_init_qp_minus26
            b.se(0); // pic_init_qs_minus26
            b.se(0); // chroma_qp_index_offset
            b.flag(true); // deblocking_filter_control_present_flag
            b.flag(false); // constrained_intra_pred_flag
            b.flag(false); // redundant_pic_cnt_present_flag
            b.trailing();
            b.len()
        };
        nal(0x68, &raw[..n])
    }
}

fn nal(header: u8, rbsp: &[u8]) -> Vec<u8> {
    let mut v = vec![0u8; rbsp.len() * 3 / 2 + 2];
    v[0] = header;
    let n = bw::escape(rbsp, &mut v[1..]).unwrap_or(0);
    v.truncate(n + 1);
    v
}

/// The lowest level (x10) whose frame size and macroblock rate fit.
fn pick_level(mbs: usize, fps: u32) -> u8 {
    const LEVELS: [(u8, usize, usize); 11] = [
        (30, 1620, 40500),
        (31, 3600, 108000),
        (32, 5120, 216000),
        (41, 8192, 245760),
        (42, 8704, 522240),
        (50, 22080, 589824),
        (51, 36864, 983040),
        (52, 36864, 2073600),
        (60, 139264, 4177920),
        (61, 139264, 8355840),
        (62, 139264, 16711680),
    ];
    let rate = mbs * fps as usize;
    for &(l, fs, mbps) in &LEVELS {
        if mbs <= fs && rate <= mbps {
            return l;
        }
    }
    62
}

/// Length-prefixed sample (4-byte lengths) -> Annex-B NAL units with start codes.
pub fn to_annexb(sample: &[u8], out: &mut Vec<u8>) {
    let mut i = 0;
    while i + 4 <= sample.len() {
        let n = u32::from_be_bytes([sample[i], sample[i + 1], sample[i + 2], sample[i + 3]]) as usize;
        i += 4;
        let end = (i + n).min(sample.len());
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&sample[i..end]);
        i = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Decoder;

    /// A moving test picture: gradients, sharp "text", a block that moves by
    /// odd amounts (half-pel chroma), and noise in one corner.
    fn picture(w: usize, h: usize, t: usize) -> Vec<u32> {
        let mut px = vec![0u32; w * h];
        let mut seed = 12345u32.wrapping_add(t as u32 * 7919);
        for y in 0..h {
            for x in 0..w {
                let mut c = (((x * 255 / w) as u32) << 16) | (((y * 255 / h) as u32) << 8) | ((x + y + t) as u32 & 0xFF);
                if (x / 3 + y / 5 + t / 4) % 7 == 0 && y > h / 2 {
                    c = 0x101010;
                }
                let (bx, by) = (10 + t * 3, 8 + t);
                if x >= bx && x < bx + 40 && y >= by && y < by + 30 {
                    c = if (x - bx) % 4 < 2 { 0xFFFF00 } else { 0x0000C0 };
                }
                if x > w * 3 / 4 && y < h / 4 {
                    seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
                    c = seed >> 8;
                }
                px[y * w + x] = c & 0xFF_FFFF;
            }
        }
        px
    }

    fn run(w: usize, h: usize, qp: u8, slices: usize, frames: usize, kbps: u32) {
        let mut e = Encoder::new(EncConfig { width: w, height: h, fps: 30, qp, max_qp: 51, bitrate_kbps: kbps, gop: 7, slices });
        let mut d = Decoder::from_avcc(&e.avcc()).unwrap();
        for t in 0..frames {
            // every few frames repeat the previous picture (skip paths)
            let px = picture(w, h, if t % 3 == 2 { t - 1 } else { t });
            let src = Source { px: &px, stride: w, width: w, height: h };
            let data = e.encode(&src, 33_333).data.to_vec();
            d.decode_sample(&data).unwrap();
            let f = d.next_frame().expect("a picture per frame");
            let (ry, ru, rv, ys, cs) = e.recon();
            for y in 0..h {
                assert_eq!(&f.y[y * f.y_stride..y * f.y_stride + w], &ry[y * ys..y * ys + w], "luma row {y} frame {t} qp {qp} slices {slices}");
            }
            for y in 0..h / 2 {
                assert_eq!(&f.u[y * f.c_stride..y * f.c_stride + w / 2], &ru[y * cs..y * cs + w / 2], "cb row {y} frame {t}");
                assert_eq!(&f.v[y * f.c_stride..y * f.c_stride + w / 2], &rv[y * cs..y * cs + w / 2], "cr row {y} frame {t}");
            }
        }
    }

    #[test]
    fn recon_matches_decoder() {
        for &(qp, slices) in &[(10u8, 1usize), (24, 3), (34, 2), (51, 5)] {
            run(176, 144, qp, slices, 12, 0);
        }
    }

    #[test]
    fn odd_sizes_and_rate_control() {
        run(150, 86, 20, 2, 10, 0);
        run(320, 180, 24, 4, 16, 200);
    }

    #[test]
    fn static_frames_are_tiny() {
        let (w, h) = (320, 240);
        let mut e = Encoder::new(EncConfig { width: w, height: h, slices: 3, ..EncConfig::default() });
        let px = picture(w, h, 0);
        let src = Source { px: &px, stride: w, width: w, height: h };
        let first = e.encode(&src, 33_333).data.len();
        let second = e.encode(&src, 33_333).data.len();
        assert!(first > 1000 && second < 40, "first {first} second {second}");
    }
}
