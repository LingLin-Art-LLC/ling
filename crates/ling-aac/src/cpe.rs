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
// Ported to LingOS (no_std + alloc + libm) from symphonia-codec-aac 0.5.5.
// Changes: the noise generator is shared by the whole decoder (as in FFmpeg),
// output goes to plain per-channel slices.

use crate::bits::BitReader;
use crate::common::*;
use crate::dsp;
use crate::ics::{self, Books};
use crate::AacError;

#[derive(Clone)]
pub struct ChannelPair {
    pub is_pair: bool,
    pub channel: usize,
    ms_mask_present: u8,
    ms_used: [[bool; MAX_SFBS]; MAX_WINDOWS],
    pub ics0: ics::Ics,
    pub ics1: ics::Ics,
}

impl ChannelPair {
    pub fn new(is_pair: bool, channel: usize, sbinfo: GASubbandInfo) -> Self {
        Self {
            is_pair,
            channel,
            ms_mask_present: 0,
            ms_used: [[false; MAX_SFBS]; MAX_WINDOWS],
            ics0: ics::Ics::new(sbinfo),
            ics1: ics::Ics::new(sbinfo),
        }
    }

    pub fn reset(&mut self) {
        self.ics0.reset();
        self.ics1.reset();
    }

    pub fn decode_ga_sce(
        &mut self,
        bs: &mut BitReader<'_>,
        books: &Books,
        lcg: &mut Lcg,
    ) -> Result<(), AacError> {
        self.ics0.decode(bs, books, lcg, false)
    }

    pub fn decode_ga_cpe(
        &mut self,
        bs: &mut BitReader<'_>,
        books: &Books,
        lcg: &mut Lcg,
    ) -> Result<(), AacError> {
        let common_window = bs.read_bool()?;

        if common_window {
            self.ics0.info.decode(bs)?;
            if self.ics0.info.max_sfb >= self.ics0.get_bands().len() {
                return Err(AacError::InvalidData("aac: max_sfb out of range"));
            }

            self.ms_mask_present = bs.read(2)? as u8;
            match self.ms_mask_present {
                0 | 2 => {
                    let is_used = self.ms_mask_present == 2;
                    for g in 0..self.ics0.info.window_groups {
                        for sfb in 0..self.ics0.info.max_sfb {
                            self.ms_used[g][sfb] = is_used;
                        }
                    }
                }
                1 => {
                    for g in 0..self.ics0.info.window_groups {
                        for sfb in 0..self.ics0.info.max_sfb {
                            self.ms_used[g][sfb] = bs.read_bool()?;
                        }
                    }
                }
                _ => return Err(AacError::InvalidData("aac: invalid mid-side mask")),
            }

            self.ics1.info.copy_from_common(&self.ics0.info);
        }

        self.ics0.decode(bs, books, lcg, common_window)?;
        self.ics1.decode(bs, books, lcg, common_window)?;

        // Joint-stereo decoding
        if common_window {
            let bands = self.ics0.get_bands();
            let mut g = 0;

            for w in 0..self.ics0.info.num_windows {
                if w > 0 && !self.ics0.info.scale_factor_grouping[w - 1] {
                    g += 1;
                }

                for sfb in 0..self.ics0.info.max_sfb {
                    let start = w * 128 + bands[sfb];
                    let end = w * 128 + bands[sfb + 1];

                    if self.ics1.is_intensity(g, sfb) {
                        // Intensity stereo, section 4.6.8.2.3
                        let invert = self.ms_mask_present == 1 && self.ms_used[g][sfb];
                        let dir = if self.ics1.get_intensity_dir(g, sfb) { 1.0 } else { -1.0 };
                        let factor = if invert { -1.0 } else { 1.0 };
                        let scale = dir * factor * self.ics1.scales[g][sfb];

                        let left = &self.ics0.coeffs[start..end];
                        let right = &mut self.ics1.coeffs[start..end];
                        for (l, r) in left.iter().zip(right) {
                            *r = scale * l;
                        }
                    } else if self.ics0.is_noise(g, sfb) || self.ics1.is_noise(g, sfb) {
                        // Perceptual noise substitution, no joint-stereo decoding (4.6.13.3).
                    } else if self.ms_used[g][sfb] {
                        let mid = &mut self.ics0.coeffs[start..end];
                        let side = &mut self.ics1.coeffs[start..end];
                        for (m, s) in mid.iter_mut().zip(side) {
                            let tmp = *m - *s;
                            *m += *s;
                            *s = tmp;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Synthesize this element's channel(s) into `core[channel]` (and `core[channel + 1]`).
    pub fn synth_audio(&mut self, dsp: &mut dsp::Dsp, rate_idx: usize, core: &mut [alloc::vec::Vec<f32>]) {
        self.ics0.synth_channel(dsp, rate_idx, &mut core[self.channel]);
        if self.is_pair {
            self.ics1.synth_channel(dsp, rate_idx, &mut core[self.channel + 1]);
        }
    }
}
