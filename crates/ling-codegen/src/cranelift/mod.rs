pub mod aot;
pub(crate) mod branchfuse;
#[cfg(not(target_arch = "wasm32"))]
pub mod fnprof;
/// wasm has no JIT (so no per-function profiler): translate.rs still asks.
#[cfg(target_arch = "wasm32")]
pub(crate) mod fnprof {
    pub(crate) fn current() -> Option<i64> {
        None
    }
}
#[cfg(not(target_arch = "wasm32"))]
pub mod jit;
pub mod numtype;
pub mod runtime;
pub mod translate;

pub use aot::CraneliftBackend;
#[cfg(not(target_arch = "wasm32"))]
pub use jit::JitBackend;
