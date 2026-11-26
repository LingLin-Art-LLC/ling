// Symphonia
// Copyright (c) 2019-2022 The Project Symphonia Developers.
//
// Previous Author: Kostya Shishkov <kostya.shiskov@gmail.com>
//
// This source file includes code originally written for the NihAV
// project. With the author's permission, it has been relicensed for,
// and ported to the Symphonia project.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Ported to LingOS (no_std + alloc + libm) from symphonia-codec-aac 0.5.5
// (src/aac/mod.rs, src/common.rs). See LICENSE for which files are MPL-2.0.

//! AAC decoder for LingOS, freestanding (`no_std` + `alloc`, float maths via `libm`).
//!
//! Decodes raw AAC access units as stored in MP4 / DASH samples (no ADTS
//! header) given the `AudioSpecificConfig` from the track's `esds` box:
//!
//! ```ignore
//! let mut dec = ling_aac::AacDecoder::new(&asc)?;      // asc = esds DecoderSpecificInfo
//! let mut pcm = Vec::new();
//! for au in samples { dec.decode(au, &mut pcm)?; }     // interleaved f32, -1.0..1.0
//! let (rate, ch) = (dec.sample_rate(), dec.channels());
//! ```
//!
//! Supported: AAC-LC (object type 2, YouTube itag 140 = `mp4a.40.2`), mono /
//! stereo / up to 8 channels (channel configurations 1-6 and 8, output in AAC
//! element order), any of the 13 AAC sampling rates; PNS, intensity and M/S
//! stereo, TNS, pulse data, long/short/start/stop windows, sine/KBD windows.
//!
//! HE-AAC (`mp4a.40.5`, SBR) is decoded with the spectral band replication
//! tool in [`sbr`]: output is at twice the core rate. HE-AACv2 (`mp4a.40.29`,
//! parametric stereo) decodes its SBR mono signal and duplicates it to both
//! channels (the PS stereo image is not reconstructed); [`AacDecoder::ps_ignored`]
//! reports this. SBR signalled only implicitly (plain LC config at <= 24 kHz
//! that carries SBR extension payloads) is detected on the first frame:
//! [`AacDecoder::sample_rate`] doubles from that frame on.
//!
//! Not supported (returns [`AacError::Unsupported`]): AAC Main / SSR / LTP /
//! ER / LD / ELD object types, program config elements, coupling channels.
//!
//! Errors never panic: garbage or truncated access units return an error and
//! append nothing; the decoder stays usable for the next access unit.

#![cfg_attr(not(test), no_std)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::identity_op)]
#![allow(clippy::new_without_default)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

mod bits;
/// AAC-LC encoder (the screen recorder's audio).
pub mod enc;
mod common;
mod cpe;
mod dsp;
mod huffman;
mod ics;
mod imdct;
mod sbr;
mod tables;
mod window;

use bits::BitReader;
use common::*;

/// Decoder errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AacError {
    /// The bitstream is malformed.
    InvalidData(&'static str),
    /// A valid feature this decoder does not implement.
    Unsupported(&'static str),
    /// The access unit (or config) ended in the middle of a syntax element.
    EndOfData,
}

impl core::fmt::Display for AacError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            AacError::InvalidData(s) => write!(f, "aac: invalid data ({})", s),
            AacError::Unsupported(s) => write!(f, "aac: unsupported ({})", s),
            AacError::EndOfData => write!(f, "aac: unexpected end of data"),
        }
    }
}

/// MPEG-4 audio object types this crate cares about.
pub const AOT_AAC_MAIN: u8 = 1;
pub const AOT_AAC_LC: u8 = 2;
pub const AOT_AAC_SSR: u8 = 3;
pub const AOT_AAC_LTP: u8 = 4;
pub const AOT_SBR: u8 = 5;
pub const AOT_PS: u8 = 29;

/// A parsed `AudioSpecificConfig` (ISO/IEC 14496-3 1.6.2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioSpecificConfig {
    /// Core object type (2 = AAC-LC). For explicit HE-AAC configs this is the
    /// underlying core type, not 5 / 29.
    pub object_type: u8,
    /// Core (AAC) sampling rate.
    pub sample_rate: u32,
    /// Channel configuration mapped to a channel count (0 = PCE, unsupported).
    pub channels: u16,
    /// 960-sample frames (not supported by the decoder).
    pub short_frame: bool,
    /// SBR explicitly signalled present (`Some(true)`), absent (`Some(false)`),
    /// or not signalled (`None`, implicit signalling possible).
    pub sbr: Option<bool>,
    /// Parametric stereo explicitly signalled.
    pub ps: bool,
    /// Output sampling rate when SBR is present.
    pub ext_sample_rate: u32,
}

impl AudioSpecificConfig {
    fn read_object_type(bs: &mut BitReader<'_>) -> Result<u8, AacError> {
        let t = bs.read(5)?;
        Ok(if t == 31 { 32 + bs.read(6)? as u8 } else { t as u8 })
    }

    fn read_sampling_frequency(bs: &mut BitReader<'_>) -> Result<u32, AacError> {
        match bs.read(4)? {
            idx if idx < 15 => Ok(AAC_SAMPLE_RATES[idx as usize]),
            _ => bs.read(24),
        }
    }

    /// Parse an AudioSpecificConfig (the esds DecoderSpecificInfo payload).
    pub fn parse(buf: &[u8]) -> Result<Self, AacError> {
        let mut bs = BitReader::new(buf);
        let mut otype = Self::read_object_type(&mut bs)?;
        let sample_rate = Self::read_sampling_frequency(&mut bs)?;
        if sample_rate == 0 {
            return Err(AacError::InvalidData("asc: sample rate"));
        }
        let chan_cfg = bs.read(4)? as usize;
        let channels = if chan_cfg < AAC_CHANNELS.len() { AAC_CHANNELS[chan_cfg] } else { 0 };

        let mut sbr = None;
        let mut ps = false;
        let mut ext_sample_rate = 0;

        if otype == AOT_SBR || otype == AOT_PS {
            sbr = Some(true);
            ps = otype == AOT_PS;
            ext_sample_rate = Self::read_sampling_frequency(&mut bs)?;
            otype = Self::read_object_type(&mut bs)?;
            if otype == 22 {
                // ER BSAC: extension channel configuration
                bs.read(4)?;
            }
        }

        let short_frame;
        match otype {
            1 | 2 | 3 | 4 | 6 | 7 | 17 | 19 | 20 | 21 | 22 | 23 => {
                // GASpecificConfig
                short_frame = bs.read_bool()?;
                if bs.read_bool()? {
                    // dependsOnCoreCoder
                    bs.read(14)?;
                }
                let extension_flag = bs.read_bool()?;
                if channels == 0 {
                    return Err(AacError::Unsupported("asc: program config element"));
                }
                if otype == 6 || otype == 20 {
                    bs.read(3)?;
                }
                if extension_flag {
                    if otype == 22 {
                        bs.read(16)?;
                    }
                    if otype == 17 || otype == 19 || otype == 20 || otype == 23 {
                        bs.read(3)?;
                    }
                    // extensionFlag3 (reserved)
                    let _ = bs.read_bool();
                }
            }
            _ => return Err(AacError::Unsupported("asc: object type")),
        }
        if (17..=27).contains(&otype) || otype == 39 {
            let ep = bs.read(2)?;
            if ep >= 2 {
                return Err(AacError::Unsupported("asc: error protection config"));
            }
        }

        // Backward-compatible (hierarchical) SBR / PS signalling.
        if sbr.is_none() && bs.bits_left() >= 16 && bs.peek(11) == 0x2B7 {
            bs.skip(11)?;
            let ext_otype = Self::read_object_type(&mut bs)?;
            if ext_otype == AOT_SBR {
                let present = bs.read_bool()?;
                sbr = Some(present);
                if present {
                    ext_sample_rate = Self::read_sampling_frequency(&mut bs)?;
                    if bs.bits_left() >= 12 && bs.read(11)? == 0x548 {
                        ps = bs.read_bool()?;
                    }
                }
            }
        }

        if sbr == Some(true) && ext_sample_rate == 0 {
            ext_sample_rate = sample_rate * 2;
        }

        Ok(Self {
            object_type: otype,
            sample_rate,
            channels: channels as u16,
            short_frame,
            sbr,
            ps,
            ext_sample_rate,
        })
    }
}

/// AAC (LC, + SBR) decoder for raw access units.
pub struct AacDecoder {
    asc: AudioSpecificConfig,
    channels: usize,
    pairs: Vec<cpe::ChannelPair>,
    dsp: Box<dsp::Dsp>,
    books: Box<ics::Books>,
    sbinfo: GASubbandInfo,
    rate_idx: usize,
    lcg: Lcg,
    /// Per-channel time-domain output of the AAC core (1024 samples).
    core: Vec<Vec<f32>>,
    /// SBR state, one per channel element, created lazily.
    sbr: Option<Box<sbr::SbrDecoder>>,
    /// SBR is active (output at twice the core rate).
    sbr_on: bool,
    /// SBR allowed (not explicitly signalled absent).
    sbr_allowed: bool,
    ps_ignored: bool,
}

impl AacDecoder {
    /// Create a decoder from an AudioSpecificConfig (the esds DecoderSpecificInfo bytes).
    pub fn new(asc: &[u8]) -> Result<Self, AacError> {
        let asc = AudioSpecificConfig::parse(asc)?;
        if asc.object_type != AOT_AAC_LC {
            return Err(AacError::Unsupported("only the AAC-LC core is supported"));
        }
        if asc.short_frame {
            return Err(AacError::Unsupported("960-sample frames"));
        }
        if asc.channels == 0 || asc.channels > 8 {
            return Err(AacError::Unsupported("channel configuration"));
        }
        let sbinfo = GASubbandInfo::find(asc.sample_rate);
        let rate_idx = GASubbandInfo::find_idx(asc.sample_rate);
        let channels = asc.channels as usize;
        // Implicit SBR is only possible when the doubled rate stays <= 96 kHz;
        // explicit signalling wins either way.
        let sbr_allowed = match asc.sbr {
            Some(on) => on,
            None => asc.sample_rate <= 24000,
        };
        let sbr_on = asc.sbr == Some(true);
        Ok(Self {
            asc,
            channels,
            pairs: Vec::new(),
            dsp: Box::new(dsp::Dsp::new()),
            books: Box::new(ics::Books::new()),
            sbinfo,
            rate_idx,
            lcg: Lcg::new(0x1f2e3d4c),
            core: vec![vec![0.0; 1024]; channels],
            sbr: None,
            sbr_on,
            sbr_allowed,
            ps_ignored: false,
        })
    }

    /// The parsed AudioSpecificConfig.
    pub fn config(&self) -> &AudioSpecificConfig {
        &self.asc
    }

    /// Output sampling rate (twice the core rate while SBR is active).
    pub fn sample_rate(&self) -> u32 {
        if self.sbr_on {
            if self.asc.ext_sample_rate != 0 {
                self.asc.ext_sample_rate
            } else {
                self.asc.sample_rate * 2
            }
        } else {
            self.asc.sample_rate
        }
    }

    /// Output channel count. A mono HE-AACv2 (PS) stream is output as stereo.
    pub fn channels(&self) -> u16 {
        if self.channels == 1 && self.asc.ps {
            2
        } else {
            self.channels as u16
        }
    }

    /// Samples per channel produced by each access unit (1024, or 2048 with SBR).
    pub fn frame_len(&self) -> usize {
        if self.sbr_on {
            2048
        } else {
            1024
        }
    }

    /// True when SBR is active.
    pub fn sbr_active(&self) -> bool {
        self.sbr_on
    }

    /// True when the stream carries parametric stereo that is decoded as
    /// duplicated mono instead (HE-AACv2).
    pub fn ps_ignored(&self) -> bool {
        self.ps_ignored || self.asc.ps
    }

    /// Forget all inter-frame state (call after seeking).
    pub fn reset(&mut self) {
        for pair in self.pairs.iter_mut() {
            pair.reset();
        }
        if let Some(s) = self.sbr.as_mut() {
            s.reset();
        }
    }

    fn set_pair(&mut self, pair_no: usize, channel: usize, pair: bool) -> Result<(), AacError> {
        if (if pair { channel + 1 } else { channel }) >= self.channels {
            return Err(AacError::InvalidData("more channels than configured"));
        }
        if self.pairs.len() <= pair_no {
            self.pairs.push(cpe::ChannelPair::new(pair, channel, self.sbinfo));
        } else if self.pairs[pair_no].channel != channel || self.pairs[pair_no].is_pair != pair {
            // Element layout changed (should not happen in a valid stream): start over.
            self.pairs[pair_no] = cpe::ChannelPair::new(pair, channel, self.sbinfo);
        }
        Ok(())
    }

    /// Decode one raw AAC access unit (one MP4 sample) and append the
    /// interleaved f32 samples to `out`. Returns the number of samples per
    /// channel appended (1024, or 2048 with SBR). On error nothing is appended.
    pub fn decode(&mut self, au: &[u8], out: &mut Vec<f32>) -> Result<usize, AacError> {
        let mut bs = BitReader::new(au);
        let mut cur_pair = 0;
        let mut cur_ch = 0;
        // (element index, channel, is_pair, sbr payload bit range)
        let mut sbr_payloads: Vec<(usize, usize, usize)> = Vec::new();
        let mut last_elem: Option<usize> = None;

        while bs.bits_left() >= 3 {
            let id = bs.read(3)?;
            match id {
                0 | 3 => {
                    // ID_SCE / ID_LFE
                    let _tag = bs.read(4)?;
                    self.set_pair(cur_pair, cur_ch, false)?;
                    self.pairs[cur_pair].decode_ga_sce(&mut bs, &self.books, &mut self.lcg)?;
                    last_elem = Some(cur_pair);
                    cur_pair += 1;
                    cur_ch += 1;
                }
                1 => {
                    // ID_CPE
                    let _tag = bs.read(4)?;
                    self.set_pair(cur_pair, cur_ch, true)?;
                    self.pairs[cur_pair].decode_ga_cpe(&mut bs, &self.books, &mut self.lcg)?;
                    last_elem = Some(cur_pair);
                    cur_pair += 1;
                    cur_ch += 2;
                }
                2 => return Err(AacError::Unsupported("coupling channel element")),
                4 => {
                    // ID_DSE
                    let _tag = bs.read(4)?;
                    let align = bs.read_bool()?;
                    let mut count = bs.read(8)?;
                    if count == 255 {
                        count += bs.read(8)?;
                    }
                    if align {
                        bs.align();
                    }
                    bs.skip(count as usize * 8)?;
                }
                5 => return Err(AacError::Unsupported("program config element")),
                6 => {
                    // ID_FIL
                    let mut count = bs.read(4)? as usize;
                    if count == 15 {
                        count += bs.read(8)? as usize;
                        count -= 1;
                    }
                    if count > 0 && bs.bits_left() >= count * 8 {
                        let ext_type = bs.peek(4);
                        if (ext_type == 13 || ext_type == 14) && self.sbr_allowed {
                            if let Some(elem) = last_elem {
                                sbr_payloads.push((elem, bs.position(), count * 8));
                            }
                        }
                    }
                    bs.skip(count * 8)?;
                }
                _ => break, // ID_END
            }
        }

        if cur_pair == 0 {
            return Err(AacError::InvalidData("no audio elements"));
        }

        // Implicit SBR signalling: switch on at the first SBR payload.
        if !self.sbr_on && self.sbr_allowed && !sbr_payloads.is_empty() {
            self.sbr_on = true;
        }

        for pair in 0..cur_pair {
            self.pairs[pair].synth_audio(&mut self.dsp, self.rate_idx, &mut self.core);
        }

        let produced_channels = cur_ch;

        if !self.sbr_on {
            out.reserve(1024 * self.channels);
            for i in 0..1024 {
                for c in 0..self.channels {
                    out.push(if c < produced_channels { self.core[c][i] } else { 0.0 });
                }
            }
            return Ok(1024);
        }

        // SBR: upsample every channel through the QMF bank, applying the
        // envelope data of its element when present.
        let srate = self.asc.sample_rate;
        let sbr = self
            .sbr
            .get_or_insert_with(|| Box::new(sbr::SbrDecoder::new(srate, 8)));
        let mut hi = vec![vec![0f32; 2048]; self.channels];
        for pair in 0..cur_pair {
            let ch = self.pairs[pair].channel;
            let is_pair = self.pairs[pair].is_pair;
            let payload = sbr_payloads.iter().find(|p| p.0 == pair).map(|p| (p.1, p.2));
            let data = payload.map(|(start, len)| (au, start, len));
            if sbr.element(pair, is_pair, data).is_err() {
                // Bad SBR data: the element decodes as plain upsampled core.
            }
            if sbr.ps_seen() {
                self.ps_ignored = true;
            }
            let n = if is_pair { 2 } else { 1 };
            for k in 0..n {
                sbr.synthesize(pair, k, &self.core[ch + k], &mut hi[ch + k]);
            }
        }

        let out_ch = self.channels() as usize;
        out.reserve(2048 * out_ch);
        for i in 0..2048 {
            for c in 0..out_ch {
                let src = if c < self.channels { c } else { 0 };
                out.push(if src < produced_channels { hi[src][i] } else { 0.0 });
            }
        }
        Ok(2048)
    }
}
