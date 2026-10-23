//! skyengine-jit: JIT support for SkyEngine.
//!
//! Layered on top of the interpreter, never replacing it:
//!
//! - [`probe_exec_memory`] (P0) detects at runtime whether the platform
//!   allows anonymous executable memory. Hardened targets (e.g. code-signing
//!   policies) may deny it; the engine then keeps running on the interpreter.
//! - [`ExecMemory`] (P1) is the bump-allocated code arena backing all
//!   generated blocks.
//! - [`emitter`] (P1, x86_64) is the native-code emitter skeleton; the
//!   aarch64 backend arrives in a later phase.
//! - [`BlockCache`] (P1) maps guest entry PCs to translated blocks.
//! - [`translate_thumb_block`] (P2, x86_64) translates a run of Thumb-1
//!   guest instructions into a single native block mirroring the
//!   interpreter's step-by-step semantics, including its fault-visible
//!   memory-access ordering.
//!
//! Later phases build differential verification against the interpreter
//! (P3) and the aarch64 backend (P4) on top.

mod cache;
#[cfg(target_arch = "x86_64")]
mod emitter;
mod exec;
mod probe;
mod sys;
#[cfg(target_arch = "x86_64")]
mod translate;

pub use cache::{BlockCache, BlockEntry};
#[cfg(target_arch = "x86_64")]
pub use emitter::{Cc, Label, Reg32, X86Emitter};
pub use exec::{CodePtr, ExecMemory, ExecMemoryError};
pub use probe::{ExecBackend, ExecMemoryReport, probe_exec_memory};
#[cfg(target_arch = "x86_64")]
pub use translate::{BlockContext, ReadFn, ThumbBlock, WriteFn, ctx_off, translate_thumb_block};
