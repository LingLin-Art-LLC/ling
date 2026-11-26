//! LingJS — a JavaScript engine for Horizon (the browser) and LingOS.
//!
//! Freestanding: `no_std` + `alloc`, one dependency (`libm`). Source is lexed and parsed into a
//! syntax tree ([`parser`]), scope-analysed and compiled to stack bytecode ([`compiler`]), and run
//! by a bytecode VM ([`vm`]) over a mark-sweep heap ([`object`]).
//!
//! ## The host (the page)
//! Objects the page owns (elements, the document, the window) are *host objects*: ordinary
//! engine objects carrying a host class and id. Reading, writing or calling one does not call
//! back into the embedder; the VM **suspends** and [`Engine::run`] returns [`Status::Host`] with
//! the request. The embedder does the work and calls [`Engine::resume`] with the answer. Nothing
//! in the engine is re-entrant, so it works the same under a JIT, an interpreter or a kernel.
//!
//! Builtins that call back into script (`Array.prototype.map`, `sort`, iteration ...) are written
//! in JavaScript ([`prelude`]), so a callback that touches the page suspends like any other code.
//!
//! ## Jobs and modules
//! Promise reactions and `queueMicrotask` callbacks wait in the engine's job queue: after a run,
//! [`Engine::has_jobs`] says whether there are any and [`Engine::run_jobs`] drains it (a job may
//! stop for the host like any run; [`Engine::resume`] then finishes it and drains on).
//! [`Engine::eval_module`] runs an ES module; its imports (and `import()`) ask the host for sources
//! with [`HostReq::Import`]. Generators and async functions suspend by moving their frame into the
//! generator object, so they too can stop for the host in the middle.
//!
//! The sandbox: an instruction budget per run, a call-depth limit, a heap limit; `eval` and `Function()`
//! compile at run time (global scope) under the same budget; the embedder decides what else the
//! global object offers.

#![cfg_attr(not(test), no_std)]
#![allow(clippy::too_many_arguments, clippy::new_without_default)]

extern crate alloc;

pub mod ast;
pub mod bigint;
pub mod atoms;
mod builtins;
mod builtins2;
pub mod compiler;
pub mod lexer;
pub mod num;
pub mod object;
pub mod parser;
mod prelude;
pub mod regex;
pub mod strs;
pub mod value;
pub mod vm;

pub use object::ObjRef;
pub use strs::JsStr;
pub use value::Value;
pub use vm::{Engine, HostReq, Status};
