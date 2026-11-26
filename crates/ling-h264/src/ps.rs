//! Sequence and picture parameter sets (7.3.2.1, 7.3.2.2) and VUI (E.1.1).

use crate::bits::BitReader;
use crate::tables::*;
use crate::Error;

#[derive(Clone)]
pub struct Sps {
    pub profile_idc: u8,
    pub level_idc: u8,
    pub id: u32,
    pub chroma_format_idc: u32,
    pub scaling_present: bool,
    /// 6 4x4 lists + 2 8x8 lists, raster order (resolved with fall-back rule A)
    pub sl4: [[u8; 16]; 6],
    pub sl8: [[u8; 64]; 2],
    pub log2_max_frame_num: u32,
    pub poc_type: u32,
    pub log2_max_poc_lsb: u32,
    pub delta_pic_order_always_zero: bool,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub offset_for_ref_frame: alloc::vec::Vec<i32>,
    pub max_num_ref_frames: u32,
    pub gaps_allowed: bool,
    pub mb_width: u32,
    pub mb_height: u32,
    pub frame_mbs_only: bool,
    pub mbaff: bool,
    pub direct_8x8_inference: bool,
    pub crop_left: u32,
    pub crop_right: u32,
    pub crop_top: u32,
    pub crop_bottom: u32,
    pub full_range: bool,
    pub matrix_coefficients: u32,
    pub max_num_reorder_frames: Option<u32>,
    pub max_dec_frame_buffering: Option<u32>,
}

impl Sps {
    pub fn width(&self) -> u32 {
        self.mb_width * 16
    }
    pub fn height(&self) -> u32 {
        self.mb_height * 16
    }
    pub fn max_frame_num(&self) -> u32 {
        1 << self.log2_max_frame_num
    }
    /// MaxDpbFrames from the level limits (A.3.1, Table A-1).
    pub fn max_dpb_frames(&self) -> u32 {
        let mbs = match self.level_idc {
            9 => 396,
            10 => 396,
            11 => 900,
            12 | 13 | 20 => 2376,
            21 => 4752,
            22 | 30 => 8100,
            31 => 18000,
            32 => 20480,
            40 | 41 => 32768,
            42 => 34816,
            50 => 110400,
            51 | 52 => 184320,
            _ => 696320,
        };
        let frame = (self.mb_width * self.mb_height).max(1);
        let mut n = (mbs / frame).clamp(1, 16);
        if let Some(b) = self.max_dec_frame_buffering {
            n = b.max(1);
        }
        n.max(self.max_num_ref_frames).min(16)
    }
    /// Number of frames that may have to be held back for display reordering.
    pub fn reorder_depth(&self) -> u32 {
        if let Some(r) = self.max_num_reorder_frames {
            return r.min(16);
        }
        // POC type 2 ties output order to decoding order. (Baseline streams with POC type 0
        // may still reorder P pictures, so the profile alone says nothing.)
        if self.poc_type == 2 {
            return 0;
        }
        if let Some(b) = self.max_dec_frame_buffering {
            return b.min(16);
        }
        self.max_dpb_frames()
    }
}

fn scaling_list(br: &mut BitReader, out: &mut [u8], scan: &[u8], default: &[u8]) {
    let n = out.len();
    let mut last = 8i32;
    let mut next = 8i32;
    for j in 0..n {
        if next != 0 {
            let delta = br.se();
            next = (last + delta + 256).rem_euclid(256);
            if j == 0 && next == 0 {
                out.copy_from_slice(default);
                return;
            }
        }
        let v = if next == 0 { last } else { next };
        out[scan[j] as usize] = v as u8;
        last = v;
    }
}

/// Parse the scaling lists of an SPS or PPS. `fb4`/`fb8` are the fall-back lists for list 0/3 and 6/7.
fn parse_scaling(
    br: &mut BitReader,
    n8: usize,
    sl4: &mut [[u8; 16]; 6],
    sl8: &mut [[u8; 64]; 2],
    fb4: [[u8; 16]; 2],
    fb8: [[u8; 64]; 2],
) {
    for i in 0..6 {
        if br.flag() {
            let def = if i < 3 { &DEFAULT_4X4_INTRA } else { &DEFAULT_4X4_INTER };
            let mut l = [0u8; 16];
            scaling_list(br, &mut l, &ZIGZAG4, def);
            sl4[i] = l;
        } else {
            sl4[i] = match i {
                0 => fb4[0],
                3 => fb4[1],
                _ => sl4[i - 1],
            };
        }
    }
    for i in 0..n8 {
        let present = br.flag();
        if i >= 2 {
            // 4:4:4 chroma 8x8 lists: parsed but unused
            if present {
                let mut l = [0u8; 64];
                scaling_list(br, &mut l, &ZIGZAG8, &DEFAULT_8X8_INTRA);
            }
            continue;
        }
        if present {
            let def = if i == 0 { &DEFAULT_8X8_INTRA } else { &DEFAULT_8X8_INTER };
            let mut l = [0u8; 64];
            scaling_list(br, &mut l, &ZIGZAG8, def);
            sl8[i] = l;
        } else {
            sl8[i] = fb8[i];
        }
    }
}

fn skip_hrd(br: &mut BitReader) {
    let cpb_cnt = br.ue() + 1;
    br.u(4);
    br.u(4);
    for _ in 0..cpb_cnt.min(32) {
        br.ue();
        br.ue();
        br.u(1);
    }
    br.u(5);
    br.u(5);
    br.u(5);
    br.u(5);
}

pub fn parse_sps(br: &mut BitReader) -> Result<Sps, Error> {
    let profile_idc = br.u(8) as u8;
    let _constraints = br.u(8);
    let level_idc = br.u(8) as u8;
    let id = br.ue();
    if id > 31 {
        return Err(Error::Invalid("sps id"));
    }
    let mut sps = Sps {
        profile_idc,
        level_idc,
        id,
        chroma_format_idc: 1,
        scaling_present: false,
        sl4: [[16; 16]; 6],
        sl8: [[16; 64]; 2],
        log2_max_frame_num: 4,
        poc_type: 0,
        log2_max_poc_lsb: 4,
        delta_pic_order_always_zero: false,
        offset_for_non_ref_pic: 0,
        offset_for_top_to_bottom_field: 0,
        offset_for_ref_frame: alloc::vec::Vec::new(),
        max_num_ref_frames: 0,
        gaps_allowed: false,
        mb_width: 0,
        mb_height: 0,
        frame_mbs_only: true,
        mbaff: false,
        direct_8x8_inference: true,
        crop_left: 0,
        crop_right: 0,
        crop_top: 0,
        crop_bottom: 0,
        full_range: false,
        matrix_coefficients: 2,
        max_num_reorder_frames: None,
        max_dec_frame_buffering: None,
    };
    if matches!(profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        sps.chroma_format_idc = br.ue();
        if sps.chroma_format_idc == 3 {
            br.u(1);
        }
        let bd_luma = br.ue();
        let bd_chroma = br.ue();
        if bd_luma != 0 || bd_chroma != 0 {
            return Err(Error::Unsupported("bit depth > 8"));
        }
        if br.flag() {
            return Err(Error::Unsupported("lossless transform bypass"));
        }
        if br.flag() {
            sps.scaling_present = true;
            let n8 = if sps.chroma_format_idc != 3 { 2 } else { 6 };
            let mut sl4 = [[16u8; 16]; 6];
            let mut sl8 = [[16u8; 64]; 2];
            parse_scaling(br, n8, &mut sl4, &mut sl8, [DEFAULT_4X4_INTRA, DEFAULT_4X4_INTER], [DEFAULT_8X8_INTRA, DEFAULT_8X8_INTER]);
            sps.sl4 = sl4;
            sps.sl8 = sl8;
        }
    }
    if sps.chroma_format_idc != 1 {
        return Err(Error::Unsupported("chroma format other than 4:2:0"));
    }
    sps.log2_max_frame_num = br.ue() + 4;
    if sps.log2_max_frame_num > 16 {
        return Err(Error::Invalid("log2_max_frame_num"));
    }
    sps.poc_type = br.ue();
    match sps.poc_type {
        0 => {
            sps.log2_max_poc_lsb = br.ue() + 4;
            if sps.log2_max_poc_lsb > 16 {
                return Err(Error::Invalid("log2_max_poc_lsb"));
            }
        }
        1 => {
            sps.delta_pic_order_always_zero = br.flag();
            sps.offset_for_non_ref_pic = br.se();
            sps.offset_for_top_to_bottom_field = br.se();
            let n = br.ue();
            if n > 255 {
                return Err(Error::Invalid("poc cycle"));
            }
            for _ in 0..n {
                let v = br.se();
                sps.offset_for_ref_frame.push(v);
            }
        }
        2 => {}
        _ => return Err(Error::Invalid("poc type")),
    }
    sps.max_num_ref_frames = br.ue();
    if sps.max_num_ref_frames > 16 {
        return Err(Error::Invalid("max_num_ref_frames"));
    }
    sps.gaps_allowed = br.flag();
    sps.mb_width = br.ue() + 1;
    let map_units = br.ue() + 1;
    sps.frame_mbs_only = br.flag();
    if !sps.frame_mbs_only {
        sps.mbaff = br.flag();
    }
    sps.mb_height = map_units * if sps.frame_mbs_only { 1 } else { 2 };
    if sps.mb_width > 1024 || sps.mb_height > 1024 || sps.mb_width * sps.mb_height > 139264 {
        return Err(Error::Unsupported("picture too large"));
    }
    sps.direct_8x8_inference = br.flag();
    if br.flag() {
        // CropUnitX = 2, CropUnitY = 2 * (2 - frame_mbs_only) for 4:2:0
        let cy = 2 * if sps.frame_mbs_only { 1 } else { 2 };
        sps.crop_left = br.ue().saturating_mul(2);
        sps.crop_right = br.ue().saturating_mul(2);
        sps.crop_top = br.ue().saturating_mul(cy);
        sps.crop_bottom = br.ue().saturating_mul(cy);
        if sps.crop_left.saturating_add(sps.crop_right) >= sps.width() || sps.crop_top.saturating_add(sps.crop_bottom) >= sps.height() {
            sps.crop_left = 0;
            sps.crop_right = 0;
            sps.crop_top = 0;
            sps.crop_bottom = 0;
        }
    }
    if br.flag() {
        // VUI
        if br.flag() {
            let idc = br.u(8);
            if idc == 255 {
                br.u(16);
                br.u(16);
            }
        }
        if br.flag() {
            br.u(1);
        }
        if br.flag() {
            br.u(3);
            sps.full_range = br.flag();
            if br.flag() {
                br.u(8);
                br.u(8);
                sps.matrix_coefficients = br.u(8);
            }
        }
        if br.flag() {
            br.ue();
            br.ue();
        }
        if br.flag() {
            br.u(32);
            br.u(32);
            br.u(1);
        }
        let nal_hrd = br.flag();
        if nal_hrd {
            skip_hrd(br);
        }
        let vcl_hrd = br.flag();
        if vcl_hrd {
            skip_hrd(br);
        }
        if nal_hrd || vcl_hrd {
            br.u(1);
        }
        br.u(1); // pic_struct_present
        if br.flag() {
            br.u(1);
            br.ue();
            br.ue();
            br.ue();
            br.ue();
            let reorder = br.ue();
            let buffering = br.ue();
            if !br.overrun() {
                sps.max_num_reorder_frames = Some(reorder.min(16));
                sps.max_dec_frame_buffering = Some(buffering.min(16));
            }
        }
    }
    if br.err {
        // A truncated VUI is tolerated; the core fields were read before it.
        sps.max_num_reorder_frames = None;
        sps.max_dec_frame_buffering = None;
    }
    Ok(sps)
}

#[derive(Clone)]
pub struct Pps {
    pub id: u32,
    pub sps_id: u32,
    pub cabac: bool,
    pub bottom_field_pic_order_in_frame_present: bool,
    pub num_ref_idx_default: [u32; 2],
    pub weighted_pred: bool,
    pub weighted_bipred_idc: u32,
    pub pic_init_qp: i32,
    pub chroma_qp_offset: [i32; 2],
    pub deblocking_filter_control_present: bool,
    pub constrained_intra_pred: bool,
    pub redundant_pic_cnt_present: bool,
    pub transform_8x8_mode: bool,
    /// Final scaling lists (raster order) once resolved against the SPS.
    pub sl4: [[u8; 16]; 6],
    pub sl8: [[u8; 64]; 2],
}

pub fn parse_pps(br: &mut BitReader, spss: &[Option<Sps>]) -> Result<Pps, Error> {
    let id = br.ue();
    let sps_id = br.ue();
    if id > 255 || sps_id > 31 {
        return Err(Error::Invalid("pps id"));
    }
    let sps = spss[sps_id as usize].as_ref().ok_or(Error::Invalid("pps references missing sps"))?;
    let cabac = br.flag();
    let bfpo = br.flag();
    let num_slice_groups = br.ue() + 1;
    if num_slice_groups > 1 {
        return Err(Error::Unsupported("FMO (slice groups)"));
    }
    let l0 = br.ue() + 1;
    let l1 = br.ue() + 1;
    if l0 > 32 || l1 > 32 {
        return Err(Error::Invalid("num_ref_idx_default"));
    }
    let weighted_pred = br.flag();
    let weighted_bipred_idc = br.u(2);
    if weighted_bipred_idc > 2 {
        return Err(Error::Invalid("weighted_bipred_idc"));
    }
    let pic_init_qp = 26 + br.se();
    let _qs = br.se();
    let cqo = br.se();
    if !(0..=51).contains(&pic_init_qp) || !(-12..=12).contains(&cqo) {
        return Err(Error::Invalid("pps qp"));
    }
    let deblocking_filter_control_present = br.flag();
    let constrained_intra_pred = br.flag();
    let redundant_pic_cnt_present = br.flag();
    let mut pps = Pps {
        id,
        sps_id,
        cabac,
        bottom_field_pic_order_in_frame_present: bfpo,
        num_ref_idx_default: [l0, l1],
        weighted_pred,
        weighted_bipred_idc,
        pic_init_qp,
        chroma_qp_offset: [cqo, cqo],
        deblocking_filter_control_present,
        constrained_intra_pred,
        redundant_pic_cnt_present,
        transform_8x8_mode: false,
        sl4: sps.sl4,
        sl8: sps.sl8,
    };
    if br.more_rbsp_data() {
        pps.transform_8x8_mode = br.flag();
        if br.flag() {
            let n8 = if pps.transform_8x8_mode { if sps.chroma_format_idc == 3 { 6 } else { 2 } } else { 0 };
            let (fb4, fb8) = if sps.scaling_present {
                ([sps.sl4[0], sps.sl4[3]], [sps.sl8[0], sps.sl8[1]])
            } else {
                ([DEFAULT_4X4_INTRA, DEFAULT_4X4_INTER], [DEFAULT_8X8_INTRA, DEFAULT_8X8_INTER])
            };
            let mut sl4 = [[16u8; 16]; 6];
            let mut sl8 = [[16u8; 64]; 2];
            parse_scaling(br, n8, &mut sl4, &mut sl8, fb4, fb8);
            if n8 == 0 {
                sl8 = fb8;
            }
            pps.sl4 = sl4;
            pps.sl8 = sl8;
        }
        let c2 = br.se();
        if !(-12..=12).contains(&c2) {
            return Err(Error::Invalid("second chroma qp offset"));
        }
        pps.chroma_qp_offset[1] = c2;
    }
    if br.err {
        return Err(Error::Invalid("truncated pps"));
    }
    Ok(pps)
}
