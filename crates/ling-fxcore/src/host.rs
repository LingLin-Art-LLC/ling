//! Hooks the host (the kernel) fills in.

use alloc::vec::Vec;

/// A decoded sample file: interleaved stereo and its rate (ling-music's `Decoded`).
pub struct Decoded {
    pub stereo: Vec<f32>,
    pub rate: u32,
}

static mut LOADER: Option<fn(&str) -> Option<Decoded>> = None;

/// Set the function that reads + decodes an audio file (the kernel: vfs + its WAV/FLAC reader).
pub fn set_audio_loader(f: fn(&str) -> Option<Decoded>) {
    unsafe { LOADER = Some(f) };
}

/// ling-music `decode::load` for the effects that take a sample (the Hypergrowl's source).
pub fn load_audio(path: &str) -> Result<Decoded, ()> {
    match unsafe { LOADER } {
        Some(f) => f(path).ok_or(()),
        None => Err(()),
    }
}
