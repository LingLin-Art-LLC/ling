// Symphonia
// Copyright (c) 2019-2022 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//
// Ported to LingOS (no_std + alloc) from symphonia-bundle-mp3 0.5.5 (src/common.rs): the
// symphonia-core codec / signal-spec helpers were dropped, `Layer::decode` takes a byte slice and
// the crate-local `AudioBuffer`.

use alloc::vec;
use alloc::vec::Vec;

use crate::bits::Result;

/// The MPEG audio version.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MpegVersion {
    /// Version 2.5
    Mpeg2p5,
    /// Version 2
    Mpeg2,
    /// Version 1
    Mpeg1,
}

/// The MPEG audio layer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MpegLayer {
    /// Layer 1
    Layer1,
    /// Layer 2
    Layer2,
    /// Layer 3
    Layer3,
}

/// For Joint Stereo channel mode, the mode extension describes the features and parameters of the
/// stereo encoding.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Joint Stereo in layer 3 may use both Mid-Side and Intensity encoding.
    Layer3 { mid_side: bool, intensity: bool },
    /// Joint Stereo in layers 1 and 2 may only use Intensity encoding on a set of bands. The range
    /// of bands using intensity encoding is `bound..32`.
    Intensity { bound: u32 },
}

/// The channel mode.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ChannelMode {
    /// Single mono audio channel.
    Mono,
    /// Dual mono audio channels.
    DualMono,
    /// Stereo channels.
    Stereo,
    /// Joint Stereo encoded channels (decodes to Stereo).
    JointStereo(Mode),
}

impl ChannelMode {
    /// Gets the number of channels.
    #[inline(always)]
    pub fn count(&self) -> usize {
        match self {
            ChannelMode::Mono => 1,
            _ => 2,
        }
    }

}

/// The emphasis applied during encoding.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Emphasis {
    /// No emphasis
    None,
    /// 50/15us
    Fifty15,
    /// CCIT J.17
    CcitJ17,
}

/// A MPEG 1, 2, or 2.5 audio frame header.
#[derive(Debug)]
pub struct FrameHeader {
    pub version: MpegVersion,
    pub layer: MpegLayer,
    pub bitrate: u32,
    pub sample_rate: u32,
    pub sample_rate_idx: usize,
    pub channel_mode: ChannelMode,
    #[allow(dead_code)]
    pub emphasis: Emphasis,
    #[allow(dead_code)]
    pub is_copyrighted: bool,
    #[allow(dead_code)]
    pub is_original: bool,
    #[allow(dead_code)]
    pub has_padding: bool,
    pub has_crc: bool,
    pub frame_size: usize,
}

impl FrameHeader {
    /// Returns true if this a MPEG1 frame, false otherwise.
    #[inline(always)]
    pub fn is_mpeg1(&self) -> bool {
        self.version == MpegVersion::Mpeg1
    }

    /// Returns true if this a MPEG2.5 frame, false otherwise.
    #[inline(always)]
    #[allow(dead_code)]
    pub fn is_mpeg2p5(&self) -> bool {
        self.version == MpegVersion::Mpeg2p5
    }



    /// Returns the number of audio samples in the frame per channel.
    pub fn duration(&self) -> u64 {
        match self.layer {
            MpegLayer::Layer1 => 384,
            MpegLayer::Layer2 => 1152,
            MpegLayer::Layer3 => 576 * self.n_granules() as u64,
        }
    }

    /// Returns the number of granules in the frame.
    #[inline(always)]
    pub fn n_granules(&self) -> usize {
        match self.version {
            MpegVersion::Mpeg1 => 2,
            _ => 1,
        }
    }

    /// Returns the number of channels per granule.
    #[inline(always)]
    pub fn n_channels(&self) -> usize {
        self.channel_mode.count()
    }

    /// Returns true if Intensity Stereo encoding is used, false otherwise.
    #[allow(dead_code)]
    #[inline(always)]
    pub fn is_intensity_stereo(&self) -> bool {
        match self.channel_mode {
            ChannelMode::JointStereo(Mode::Intensity { .. }) => true,
            ChannelMode::JointStereo(Mode::Layer3 { intensity, .. }) => intensity,
            _ => false,
        }
    }

    /// Get the side information length.
    #[inline(always)]
    pub fn side_info_len(&self) -> usize {
        match (self.version, self.channel_mode) {
            (MpegVersion::Mpeg1, ChannelMode::Mono) => 17,
            (MpegVersion::Mpeg1, _) => 32,
            (_, ChannelMode::Mono) => 9,
            (_, _) => 17,
        }
    }
}

/// Planar output for one frame (at most 1152 samples per channel, 2 channels). A stand-in for
/// symphonia-core's `AudioBuffer<f32>` with just the calls the layer decoders make.
pub struct AudioBuffer {
    buf: Vec<f32>,
    frames: usize,
}

impl AudioBuffer {
    pub const MAX_FRAMES: usize = 1152;

    pub fn new() -> Self {
        AudioBuffer { buf: vec![0.0; 2 * Self::MAX_FRAMES], frames: 0 }
    }

    pub fn clear(&mut self) {
        self.frames = 0;
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Extend the buffer by `n` frames (whose content the caller then writes).
    pub fn render_reserved(&mut self, n: Option<usize>) {
        self.frames = (self.frames + n.unwrap_or(0)).min(Self::MAX_FRAMES);
    }

    pub fn chan(&self, ch: usize) -> &[f32] {
        let base = ch.min(1) * Self::MAX_FRAMES;
        &self.buf[base..base + self.frames]
    }

    pub fn chan_mut(&mut self, ch: usize) -> &mut [f32] {
        let base = ch.min(1) * Self::MAX_FRAMES;
        &mut self.buf[base..base + self.frames]
    }
}

pub trait Layer {
    /// `reader` is the frame after its 4-byte header (CRC word, side info, data).
    fn decode(
        &mut self,
        reader: &[u8],
        header: &FrameHeader,
        out: &mut AudioBuffer,
    ) -> Result<()>;
}
