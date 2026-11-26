//! Slice header (7.3.3), ref_pic_list_modification, pred_weight_table, dec_ref_pic_marking.

use crate::bits::BitReader;
use crate::ps::{Pps, Sps};
use crate::Error;
use alloc::vec::Vec;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SliceType {
    P,
    B,
    I,
}

#[derive(Clone, Copy, Default)]
pub struct Mmco {
    pub op: u32,
    pub diff_pic_nums: u32,
    pub long_term_pic_num: u32,
    pub long_term_frame_idx: u32,
    pub max_long_term_frame_idx_plus1: u32,
}

#[derive(Clone)]
#[allow(dead_code)]
pub struct SliceHeader {
    pub nal_type: u8,
    pub nal_ref_idc: u8,
    pub first_mb: u32,
    pub slice_type: SliceType,
    pub pps_id: u32,
    pub frame_num: u32,
    pub idr: bool,
    pub idr_pic_id: u32,
    pub poc_lsb: u32,
    pub delta_poc_bottom: i32,
    pub delta_poc: [i32; 2],
    pub redundant_pic_cnt: u32,
    pub direct_spatial: bool,
    pub num_ref_idx: [u32; 2],
    /// (modification_of_pic_nums_idc, value) per list
    pub rplm: [Vec<(u32, u32)>; 2],
    pub luma_log2_wd: u32,
    pub chroma_log2_wd: u32,
    /// explicit weights [list][ref] = (luma w, luma o, cb w, cb o, cr w, cr o); flags whether present
    pub weights: [Vec<[i32; 6]>; 2],
    pub luma_weight_flag: [Vec<bool>; 2],
    pub chroma_weight_flag: [Vec<bool>; 2],
    pub no_output_of_prior_pics: bool,
    pub long_term_reference: bool,
    pub adaptive_marking: bool,
    pub mmcos: Vec<Mmco>,
    pub cabac_init_idc: u32,
    pub qp: i32,
    pub disable_deblock: u32,
    pub alpha_offset: i32,
    pub beta_offset: i32,
    /// bit offset of slice_data() in the RBSP
    pub data_bit: usize,
}

impl SliceHeader {
    pub fn has_mmco5(&self) -> bool {
        self.mmcos.iter().any(|m| m.op == 5)
    }
}

pub fn parse_slice_header(
    br: &mut BitReader,
    nal_type: u8,
    nal_ref_idc: u8,
    spss: &[Option<Sps>],
    ppss: &[Option<Pps>],
) -> Result<SliceHeader, Error> {
    let first_mb = br.ue();
    let st = br.ue();
    let slice_type = match st % 5 {
        0 => SliceType::P,
        1 => SliceType::B,
        2 => SliceType::I,
        3 | 4 => return Err(Error::Unsupported("SP/SI slices")),
        _ => return Err(Error::Invalid("slice type")),
    };
    if st > 9 {
        return Err(Error::Invalid("slice type"));
    }
    let pps_id = br.ue();
    if pps_id > 255 {
        return Err(Error::Invalid("pps id"));
    }
    let pps = ppss[pps_id as usize].as_ref().ok_or(Error::Invalid("slice references missing pps"))?;
    let sps = spss[pps.sps_id as usize].as_ref().ok_or(Error::Invalid("slice references missing sps"))?;
    if first_mb >= sps.mb_width * sps.mb_height {
        return Err(Error::Invalid("first_mb_in_slice"));
    }
    let frame_num = br.u(sps.log2_max_frame_num);
    if !sps.frame_mbs_only {
        if br.flag() {
            return Err(Error::Unsupported("field pictures (PAFF)"));
        }
        if sps.mbaff {
            return Err(Error::Unsupported("MBAFF"));
        }
    }
    let idr = nal_type == 5;
    let idr_pic_id = if idr { br.ue() } else { 0 };
    let mut poc_lsb = 0;
    let mut delta_poc_bottom = 0;
    let mut delta_poc = [0i32; 2];
    if sps.poc_type == 0 {
        poc_lsb = br.u(sps.log2_max_poc_lsb);
        if pps.bottom_field_pic_order_in_frame_present {
            delta_poc_bottom = br.se();
        }
    }
    if sps.poc_type == 1 && !sps.delta_pic_order_always_zero {
        delta_poc[0] = br.se();
        if pps.bottom_field_pic_order_in_frame_present {
            delta_poc[1] = br.se();
        }
    }
    let redundant_pic_cnt = if pps.redundant_pic_cnt_present { br.ue() } else { 0 };
    let mut direct_spatial = false;
    if slice_type == SliceType::B {
        direct_spatial = br.flag();
    }
    let mut num_ref_idx = [0u32; 2];
    if slice_type != SliceType::I {
        num_ref_idx = pps.num_ref_idx_default;
        if br.flag() {
            num_ref_idx[0] = br.ue() + 1;
            if slice_type == SliceType::B {
                num_ref_idx[1] = br.ue() + 1;
            }
        }
        if slice_type != SliceType::B {
            num_ref_idx[1] = 0;
        }
        if num_ref_idx[0] > 32 || num_ref_idx[1] > 32 {
            return Err(Error::Invalid("num_ref_idx_active"));
        }
    }
    let mut rplm: [Vec<(u32, u32)>; 2] = [Vec::new(), Vec::new()];
    if slice_type != SliceType::I {
        for list in 0..(if slice_type == SliceType::B { 2 } else { 1 }) {
            if br.flag() {
                loop {
                    let idc = br.ue();
                    if idc == 3 {
                        break;
                    }
                    if idc > 2 || rplm[list].len() > 64 || br.overrun() {
                        return Err(Error::Invalid("ref_pic_list_modification"));
                    }
                    let v = br.ue();
                    rplm[list].push((idc, v));
                }
            }
        }
    }
    let mut luma_log2_wd = 0;
    let mut chroma_log2_wd = 0;
    let mut weights: [Vec<[i32; 6]>; 2] = [Vec::new(), Vec::new()];
    let mut lwf: [Vec<bool>; 2] = [Vec::new(), Vec::new()];
    let mut cwf: [Vec<bool>; 2] = [Vec::new(), Vec::new()];
    if (pps.weighted_pred && slice_type == SliceType::P) || (pps.weighted_bipred_idc == 1 && slice_type == SliceType::B) {
        luma_log2_wd = br.ue();
        chroma_log2_wd = br.ue();
        if luma_log2_wd > 7 || chroma_log2_wd > 7 {
            return Err(Error::Invalid("log2 weight denom"));
        }
        for list in 0..(if slice_type == SliceType::B { 2 } else { 1 }) {
            for _ in 0..num_ref_idx[list] {
                let mut w = [1 << luma_log2_wd, 0, 1 << chroma_log2_wd, 0, 1 << chroma_log2_wd, 0];
                let lf = br.flag();
                if lf {
                    w[0] = br.se();
                    w[1] = br.se();
                }
                let cf = br.flag();
                if cf {
                    w[2] = br.se();
                    w[3] = br.se();
                    w[4] = br.se();
                    w[5] = br.se();
                }
                // only coded values are range checked: the default weight 1 << 7 is 128
                let coded = (if lf { &w[0..2] } else { &w[0..0] }).iter().chain((if cf { &w[2..6] } else { &w[2..2] }).iter());
                if coded.clone().any(|v| !(-128..=127).contains(v)) {
                    return Err(Error::Invalid("weight out of range"));
                }
                weights[list].push(w);
                lwf[list].push(lf);
                cwf[list].push(cf);
            }
        }
    }
    let mut no_output_of_prior_pics = false;
    let mut long_term_reference = false;
    let mut adaptive_marking = false;
    let mut mmcos = Vec::new();
    if nal_ref_idc != 0 {
        if idr {
            no_output_of_prior_pics = br.flag();
            long_term_reference = br.flag();
        } else {
            adaptive_marking = br.flag();
            if adaptive_marking {
                loop {
                    let op = br.ue();
                    if op == 0 {
                        break;
                    }
                    if op > 6 || mmcos.len() > 66 || br.overrun() {
                        return Err(Error::Invalid("mmco"));
                    }
                    let mut m = Mmco { op, ..Default::default() };
                    if op == 1 || op == 3 {
                        m.diff_pic_nums = br.ue();
                    }
                    if op == 2 {
                        m.long_term_pic_num = br.ue();
                    }
                    if op == 3 || op == 6 {
                        m.long_term_frame_idx = br.ue();
                    }
                    if op == 4 {
                        m.max_long_term_frame_idx_plus1 = br.ue();
                    }
                    mmcos.push(m);
                }
            }
        }
    }
    let mut cabac_init_idc = 0;
    if pps.cabac && slice_type != SliceType::I {
        cabac_init_idc = br.ue();
        if cabac_init_idc > 2 {
            return Err(Error::Invalid("cabac_init_idc"));
        }
    }
    let qp = pps.pic_init_qp + br.se();
    if !(0..=51).contains(&qp) {
        return Err(Error::Invalid("slice qp"));
    }
    let mut disable_deblock = 0;
    let mut alpha_offset = 0;
    let mut beta_offset = 0;
    if pps.deblocking_filter_control_present {
        disable_deblock = br.ue();
        if disable_deblock > 2 {
            return Err(Error::Invalid("disable_deblocking_filter_idc"));
        }
        if disable_deblock != 1 {
            alpha_offset = br.se() * 2;
            beta_offset = br.se() * 2;
            if !(-12..=12).contains(&alpha_offset) || !(-12..=12).contains(&beta_offset) {
                return Err(Error::Invalid("deblock offsets"));
            }
        }
    }
    if br.overrun() {
        return Err(Error::Invalid("truncated slice header"));
    }
    Ok(SliceHeader {
        nal_type,
        nal_ref_idc,
        first_mb,
        slice_type,
        pps_id,
        frame_num,
        idr,
        idr_pic_id,
        poc_lsb,
        delta_poc_bottom,
        delta_poc,
        redundant_pic_cnt,
        direct_spatial,
        num_ref_idx,
        rplm,
        luma_log2_wd,
        chroma_log2_wd,
        weights,
        luma_weight_flag: lwf,
        chroma_weight_flag: cwf,
        no_output_of_prior_pics,
        long_term_reference,
        adaptive_marking,
        mmcos,
        cabac_init_idc,
        qp,
        disable_deblock,
        alpha_offset,
        beta_offset,
        data_bit: br.pos(),
    })
}
