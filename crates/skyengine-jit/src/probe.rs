//! Executable-memory capability probe.
//!
//! Tries, in order of preference:
//!
//! 1. [`ExecBackend::DualMapping`] �?memfd-backed RW + RX alias pair (W^X
//!    friendly, no protection flips needed when patching code).
//! 2. [`ExecBackend::ProtectFlip`] �?anonymous RW mapping flipped to RX via
//!    `mprotect` before execution.
//! 3. [`ExecBackend::RwxMapping`] �?single anonymous RWX mapping (weakest;
//!    hardened policies usually reject the initial mmap outright).
//!
//! Each tier must both allocate and execute a canary function returning
//! `0x5EED`. If no tier works, the engine stays on the interpreter
//! ([`ExecBackend::NotSupported`]).
//!
//! The canary runs without a SIGSEGV guard: on hardened systems the denial is
//! expected at the syscall layer (mmap/mprotect simply fail), not as a fault
//! on execution. A crash here is a bug worth seeing in the logs.

use core::mem;

use crate::sys;

/// Executable-memory backend selected by [`probe_exec_memory`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecBackend {
    /// memfd-backed dual mapping: one RW alias for writing, one RX alias for
    /// execution.
    DualMapping,
    /// Anonymous RW mapping flipped to RX with `mprotect` before execution.
    ProtectFlip,
    /// Single anonymous RWX mapping.
    RwxMapping,
    /// No executable anonymous memory available: stay on the interpreter.
    NotSupported,
}

/// Result of a probe run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecMemoryReport {
    /// Best backend that both allocated and executed the canary.
    pub backend: ExecBackend,
    /// Whether the canary executed and returned the expected magic value
    /// (always `true` when `backend != NotSupported`).
    pub canary_ok: bool,
}

const CANARY_MAGIC: u32 = 0x5EED;
const PROBE_SIZE: usize = 4096;

// x86_64: mov eax, 0x5EED ; ret
#[cfg(target_arch = "x86_64")]
const CANARY: &[u8] = &[0xB8, 0xED, 0x5E, 0x00, 0x00, 0xC3];

// aarch64: movz w0, #0x5EED ; ret
#[cfg(target_arch = "aarch64")]
const CANARY: &[u8] = &[0xA0, 0xDD, 0x8B, 0x52, 0xC0, 0x03, 0x5F, 0xD6];

/// A live probe mapping. `write` and `exec` alias the same physical page for
/// the single-mapping tiers; `needs_flip` marks ProtectFlip mappings whose
/// protection still has to be switched to RX after the canary is written.
struct ProbeMapping {
    write: *mut u8,
    exec: *mut u8,
    /// memfd backing the dual mapping; -1 for anonymous tiers (linux only).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fd: i32,
    needs_flip: bool,
}

impl Drop for ProbeMapping {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        unsafe {
            if self.exec != self.write {
                sys::munmap(self.exec, PROBE_SIZE);
            }
            sys::munmap(self.write, PROBE_SIZE);
            if self.fd >= 0 {
                sys::close(self.fd);
            }
        }
        #[cfg(windows)]
        unsafe {
            sys::VirtualFree(self.write, 0, sys::MEM_RELEASE);
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        let _ = self;
    }
}

#[cfg(target_os = "linux")]
fn try_dual_mapping() -> Option<ProbeMapping> {
    unsafe {
        let fd = sys::memfd_create(c"skyengine-jit".as_ptr().cast(), 0);
        if fd < 0 {
            return None;
        }
        if sys::ftruncate(fd, PROBE_SIZE as i64) < 0 {
            sys::close(fd);
            return None;
        }
        let write = sys::mmap(
            core::ptr::null_mut(),
            PROBE_SIZE,
            sys::PROT_READ | sys::PROT_WRITE,
            sys::MAP_SHARED,
            fd,
            0,
        );
        if write as usize == sys::MAP_FAILED {
            sys::close(fd);
            return None;
        }
        let exec = sys::mmap(
            core::ptr::null_mut(),
            PROBE_SIZE,
            sys::PROT_READ | sys::PROT_EXEC,
            sys::MAP_SHARED,
            fd,
            0,
        );
        if exec as usize == sys::MAP_FAILED {
            sys::munmap(write, PROBE_SIZE);
            sys::close(fd);
            return None;
        }
        Some(ProbeMapping {
            write,
            exec,
            fd,
            needs_flip: false,
        })
    }
}

#[cfg(target_os = "linux")]
fn try_protect_flip() -> Option<ProbeMapping> {
    unsafe {
        let p = sys::mmap(
            core::ptr::null_mut(),
            PROBE_SIZE,
            sys::PROT_READ | sys::PROT_WRITE,
            sys::MAP_PRIVATE | sys::MAP_ANONYMOUS,
            -1,
            0,
        );
        if p as usize == sys::MAP_FAILED {
            return None;
        }
        Some(ProbeMapping {
            write: p,
            exec: p,
            fd: -1,
            needs_flip: true,
        })
    }
}

#[cfg(target_os = "linux")]
fn try_rwx_mapping() -> Option<ProbeMapping> {
    unsafe {
        let p = sys::mmap(
            core::ptr::null_mut(),
            PROBE_SIZE,
            sys::PROT_READ | sys::PROT_WRITE | sys::PROT_EXEC,
            sys::MAP_PRIVATE | sys::MAP_ANONYMOUS,
            -1,
            0,
        );
        if p as usize == sys::MAP_FAILED {
            return None;
        }
        Some(ProbeMapping {
            write: p,
            exec: p,
            fd: -1,
            needs_flip: false,
        })
    }
}

#[cfg(windows)]
fn try_rwx_mapping() -> Option<ProbeMapping> {
    unsafe {
        let p = sys::VirtualAlloc(
            core::ptr::null_mut(),
            PROBE_SIZE,
            sys::MEM_COMMIT | sys::MEM_RESERVE,
            sys::PAGE_EXECUTE_READWRITE,
        );
        if p.is_null() {
            return None;
        }
        Some(ProbeMapping {
            write: p,
            exec: p,
            fd: -1,
            needs_flip: false,
        })
    }
}

fn open_backend(backend: ExecBackend) -> Option<ProbeMapping> {
    match backend {
        ExecBackend::NotSupported => None,
        #[cfg(target_os = "linux")]
        ExecBackend::DualMapping => try_dual_mapping(),
        #[cfg(target_os = "linux")]
        ExecBackend::ProtectFlip => try_protect_flip(),
        #[cfg(any(target_os = "linux", windows))]
        ExecBackend::RwxMapping => try_rwx_mapping(),
        #[cfg(not(target_os = "linux"))]
        ExecBackend::DualMapping | ExecBackend::ProtectFlip => None,
        #[cfg(not(any(target_os = "linux", windows)))]
        ExecBackend::RwxMapping => None,
    }
}

/// Write the canary into the mapping and execute it. Returns `false` if the
/// protection flip fails (only possible for ProtectFlip mappings).
unsafe fn run_canary(mapping: &mut ProbeMapping) -> bool {
    unsafe {
        core::ptr::copy_nonoverlapping(CANARY.as_ptr(), mapping.write, CANARY.len());
        sys::sync_icache(mapping.exec, CANARY.len());
        if mapping.needs_flip && mprotect_rx(mapping.exec, PROBE_SIZE) != 0 {
            return false;
        }
        let canary: unsafe extern "C" fn() -> u32 = mem::transmute(mapping.exec);
        canary() == CANARY_MAGIC
    }
}

#[cfg(target_os = "linux")]
#[inline]
unsafe fn mprotect_rx(addr: *mut u8, len: usize) -> i32 {
    unsafe { sys::mprotect(addr, len, sys::PROT_READ | sys::PROT_EXEC) }
}

#[cfg(not(target_os = "linux"))]
#[inline]
unsafe fn mprotect_rx(_addr: *mut u8, _len: usize) -> i32 {
    0
}

/// Probe executable-memory capabilities once. Cheap enough to run at engine
/// startup; the caller caches the result.
pub fn probe_exec_memory() -> ExecMemoryReport {
    let tiers = [
        ExecBackend::DualMapping,
        ExecBackend::ProtectFlip,
        ExecBackend::RwxMapping,
    ];
    for tier in tiers {
        if let Some(mut mapping) = open_backend(tier) {
            if unsafe { run_canary(&mut mapping) } {
                return ExecMemoryReport {
                    backend: tier,
                    canary_ok: true,
                };
            }
            // Allocated but could not execute (e.g. mprotect flip denied):
            // fall through to the next tier.
        }
    }
    ExecMemoryReport {
        backend: ExecBackend::NotSupported,
        canary_ok: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_finds_a_backend_and_executes_canary() {
        // Dev hosts (Windows/Linux/OHOS emulator) must expose at least one
        // tier; only hardened targets legitimately report NotSupported.
        let report = probe_exec_memory();
        assert_ne!(report.backend, ExecBackend::NotSupported);
        assert!(report.canary_ok);
    }

    #[test]
    fn canary_bytes_match_target_encoding() {
        // x86_64: B8 ED 5E 00 00 C3 (mov eax,0x5EED; ret)
        #[cfg(target_arch = "x86_64")]
        assert_eq!(CANARY, &[0xB8, 0xED, 0x5E, 0x00, 0x00, 0xC3]);
        // aarch64: 528BDDA0 (movz w0,#0x5EED) + D65F03C0 (ret), little-endian
        #[cfg(target_arch = "aarch64")]
        assert_eq!(CANARY, &[0xA0, 0xDD, 0x8B, 0x52, 0xC0, 0x03, 0x5F, 0xD6]);
    }
}
