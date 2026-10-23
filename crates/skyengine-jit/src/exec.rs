//! Bump-allocated executable-code arena.
//!
//! Holds all JIT-generated native code for one engine instance. Blocks are
//! only ever appended: invalidation unlinks a block from the [`crate::cache`]
//! lookup but never frees arena space. When the arena is exhausted the caller
//! resets it (dropping every translated block) and translation restarts from
//! the hot-path counters — the standard flush-and-refill JIT code cache
//! strategy.
//!
//! Backend behaviour (from [`crate::probe_exec_memory`]):
//!
//! - `DualMapping`: two aliases of one memfd — writes go through the RW
//!   alias, execution through the RX alias. No protection flips, ever.
//! - `ProtectFlip`: one anonymous mapping, flipped RW↔RX around each
//!   publish. One extra `mprotect` pair per published block — negligible
//!   next to translation cost.
//! - `RwxMapping`: one RWX mapping; write and execute in place.

use crate::probe::ExecBackend;
use crate::sys;
use core::fmt;

/// Code alignment inside the arena. 16 bytes keeps every entry a safe
/// jump target and bounds the aarch64 I-cache flush granularity.
const CODE_ALIGN: usize = 16;

/// Failure modes of the arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecMemoryError {
    /// The platform has no usable executable-memory backend.
    NotSupported,
    /// The backing syscall failed (mmap / mprotect / VirtualAlloc).
    AllocFailed,
    /// The arena is exhausted; reset it before publishing more code.
    Full,
}

impl fmt::Display for ExecMemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSupported => write!(f, "no executable-memory backend on this platform"),
            Self::AllocFailed => write!(f, "executable-memory allocation syscall failed"),
            Self::Full => write!(f, "code arena is exhausted"),
        }
    }
}

impl std::error::Error for ExecMemoryError {}

/// Offset of a published block inside its [`ExecMemory`] arena. Stable across
/// `publish` calls until [`ExecMemory::reset`] invalidates every code pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodePtr(pub u32);

/// Bump-allocated executable-code arena backed by the probed backend.
#[derive(Debug)]
pub struct ExecMemory {
    /// Chosen backend; drives the flip logic (linux) — informational on
    /// Windows, where only the RWX tier exists.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    backend: ExecBackend,
    /// Writable alias (equals `exec_base` on single-mapping backends).
    write_base: *mut u8,
    /// Executable alias.
    exec_base: *mut u8,
    size: usize,
    used: usize,
    /// memfd behind the DualMapping backend; -1 otherwise (linux only).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fd: i32,
    /// Whether the ProtectFlip mapping is currently sealed RX.
    sealed: bool,
}

// The arena is owned exclusively by the engine thread; raw pointers are never
// handed out as `&`-references, only as entry addresses.
unsafe impl Send for ExecMemory {}
unsafe impl Sync for ExecMemory {}

impl ExecMemory {
    /// Create an arena of at least `size` bytes using the probed backend.
    pub fn new(size: usize, backend: ExecBackend) -> Result<Self, ExecMemoryError> {
        let size = sys::round_to_page(size.max(1));
        let (write_base, exec_base, fd) = match backend {
            ExecBackend::NotSupported => return Err(ExecMemoryError::NotSupported),
            #[cfg(target_os = "linux")]
            ExecBackend::DualMapping => {
                let fd = unsafe { sys::memfd_create(c"skyengine-jit".as_ptr().cast(), 0) };
                if fd < 0 {
                    return Err(ExecMemoryError::AllocFailed);
                }
                if unsafe { sys::ftruncate(fd, size as i64) } < 0 {
                    unsafe { sys::close(fd) };
                    return Err(ExecMemoryError::AllocFailed);
                }
                let write = unsafe {
                    sys::mmap(
                        core::ptr::null_mut(),
                        size,
                        sys::PROT_READ | sys::PROT_WRITE,
                        sys::MAP_SHARED,
                        fd,
                        0,
                    )
                };
                if write as usize == sys::MAP_FAILED {
                    unsafe { sys::close(fd) };
                    return Err(ExecMemoryError::AllocFailed);
                }
                let exec = unsafe {
                    sys::mmap(
                        core::ptr::null_mut(),
                        size,
                        sys::PROT_READ | sys::PROT_EXEC,
                        sys::MAP_SHARED,
                        fd,
                        0,
                    )
                };
                if exec as usize == sys::MAP_FAILED {
                    unsafe {
                        sys::munmap(write, size);
                        sys::close(fd);
                    }
                    return Err(ExecMemoryError::AllocFailed);
                }
                (write, exec, fd)
            }
            #[cfg(target_os = "linux")]
            ExecBackend::ProtectFlip => {
                let p = unsafe {
                    sys::mmap(
                        core::ptr::null_mut(),
                        size,
                        sys::PROT_READ | sys::PROT_WRITE,
                        sys::MAP_PRIVATE | sys::MAP_ANONYMOUS,
                        -1,
                        0,
                    )
                };
                if p as usize == sys::MAP_FAILED {
                    return Err(ExecMemoryError::AllocFailed);
                }
                (p, p, -1)
            }
            #[cfg(any(target_os = "linux", windows))]
            ExecBackend::RwxMapping => {
                #[cfg(target_os = "linux")]
                let p = unsafe {
                    sys::mmap(
                        core::ptr::null_mut(),
                        size,
                        sys::PROT_READ | sys::PROT_WRITE | sys::PROT_EXEC,
                        sys::MAP_PRIVATE | sys::MAP_ANONYMOUS,
                        -1,
                        0,
                    )
                };
                #[cfg(windows)]
                let p = unsafe {
                    sys::VirtualAlloc(
                        core::ptr::null_mut(),
                        size,
                        sys::MEM_COMMIT | sys::MEM_RESERVE,
                        sys::PAGE_EXECUTE_READWRITE,
                    )
                };
                #[cfg(target_os = "linux")]
                if p as usize == sys::MAP_FAILED {
                    return Err(ExecMemoryError::AllocFailed);
                }
                #[cfg(windows)]
                if p.is_null() {
                    return Err(ExecMemoryError::AllocFailed);
                }
                (p, p, -1)
            }
            #[cfg(not(target_os = "linux"))]
            ExecBackend::DualMapping | ExecBackend::ProtectFlip => {
                // Dual-mapping and flip tiers are linux-only for now.
                return Err(ExecMemoryError::NotSupported);
            }
            #[cfg(not(any(target_os = "linux", windows)))]
            _ => return Err(ExecMemoryError::NotSupported),
        };
        Ok(Self {
            backend,
            write_base,
            exec_base,
            size,
            used: 0,
            fd,
            // DualMapping never needs flips; anonymous mappings start RW.
            sealed: false,
        })
    }

    /// Total arena capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.size
    }

    /// Bytes still available for publishing.
    pub fn free(&self) -> usize {
        self.size - self.used
    }

    /// Copy `code` into the arena and make it executable. The returned
    /// pointer stays valid until [`Self::reset`].
    pub fn publish(&mut self, code: &[u8]) -> Result<CodePtr, ExecMemoryError> {
        let len = code.len();
        let span = (len + CODE_ALIGN - 1) & !(CODE_ALIGN - 1);
        if span > self.free() {
            return Err(ExecMemoryError::Full);
        }
        let offset = self.used;
        unsafe {
            self.unseal()?;
            core::ptr::copy_nonoverlapping(code.as_ptr(), self.write_base.add(offset), len);
            sys::sync_icache(self.exec_base.add(offset), len);
            self.seal()?;
        }
        self.used += span;
        Ok(CodePtr(offset as u32))
    }

    /// Native entry address for a published code pointer.
    pub fn entry(&self, ptr: CodePtr) -> *const u8 {
        unsafe { self.exec_base.add(ptr.0 as usize) }
    }

    /// Drop every published block and reuse the arena from the start.
    /// **Invalidates every previously returned [`CodePtr`]** — the caller
    /// must clear its block cache in the same breath.
    pub fn reset(&mut self) -> Result<(), ExecMemoryError> {
        self.used = 0;
        unsafe {
            self.unseal()?;
        }
        Ok(())
    }

    /// Open the ProtectFlip mapping for writing (no-op otherwise).
    unsafe fn unseal(&mut self) -> Result<(), ExecMemoryError> {
        #[cfg(target_os = "linux")]
        if self.backend == ExecBackend::ProtectFlip && self.sealed {
            if unsafe {
                sys::mprotect(self.write_base, self.size, sys::PROT_READ | sys::PROT_WRITE)
            } != 0
            {
                return Err(ExecMemoryError::AllocFailed);
            }
            self.sealed = false;
        }
        #[cfg(not(target_os = "linux"))]
        let _ = &mut self.sealed;
        Ok(())
    }

    /// Seal the ProtectFlip mapping for execution (no-op otherwise).
    unsafe fn seal(&mut self) -> Result<(), ExecMemoryError> {
        #[cfg(target_os = "linux")]
        if self.backend == ExecBackend::ProtectFlip && !self.sealed {
            if unsafe { sys::mprotect(self.write_base, self.size, sys::PROT_READ | sys::PROT_EXEC) }
                != 0
            {
                return Err(ExecMemoryError::AllocFailed);
            }
            self.sealed = true;
        }
        #[cfg(not(target_os = "linux"))]
        let _ = &mut self.sealed;
        Ok(())
    }
}

impl Drop for ExecMemory {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        unsafe {
            if self.exec_base != self.write_base {
                sys::munmap(self.exec_base, self.size);
            }
            sys::munmap(self.write_base, self.size);
            if self.fd >= 0 {
                sys::close(self.fd);
            }
        }
        #[cfg(windows)]
        unsafe {
            sys::VirtualFree(self.write_base, 0, sys::MEM_RELEASE);
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        let _ = self;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::probe_exec_memory;
    use core::mem;

    fn fresh_arena(size: usize) -> ExecMemory {
        let report = probe_exec_memory();
        assert_ne!(report.backend, ExecBackend::NotSupported);
        ExecMemory::new(size, report.backend).expect("arena allocation")
    }

    #[test]
    fn not_supported_backend_is_rejected() {
        assert_eq!(
            ExecMemory::new(4096, ExecBackend::NotSupported).unwrap_err(),
            ExecMemoryError::NotSupported
        );
    }

    #[test]
    fn size_is_page_rounded() {
        let arena = fresh_arena(1);
        assert_eq!(arena.capacity(), sys::PAGE_SIZE);
    }

    #[test]
    fn publish_rejects_oversized_code() {
        let mut arena = fresh_arena(4096);
        let huge = vec![0xC3u8; 5000];
        assert_eq!(arena.publish(&huge).unwrap_err(), ExecMemoryError::Full);
    }

    #[test]
    fn publish_tracks_alignment_and_space() {
        let mut arena = fresh_arena(4096);
        let ret = [0xC3u8];
        let p1 = arena.publish(&ret).unwrap();
        assert_eq!(p1, CodePtr(0));
        assert_eq!(arena.free(), arena.capacity() - CODE_ALIGN);
        let p2 = arena.publish(&[0u8; 17]).unwrap();
        assert_eq!(p2, CodePtr(CODE_ALIGN as u32));
        // 17 bytes consume two alignment slots (32 total after the first 16).
        assert_eq!(arena.free(), arena.capacity() - 3 * CODE_ALIGN);
    }

    #[test]
    fn reset_reclaims_the_arena() {
        let mut arena = fresh_arena(4096);
        arena.publish(&[0xC3u8; 1024]).unwrap();
        assert_eq!(arena.free(), arena.capacity() - 1024);
        arena.reset().unwrap();
        assert_eq!(arena.free(), arena.capacity());
        assert_eq!(arena.publish(&[0xC3]).unwrap(), CodePtr(0));
    }

    /// The P1 acceptance test: emit → publish → execute → correct return.
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn publish_executes_published_code() {
        use crate::emitter::{Reg32, X86Emitter};

        let mut arena = fresh_arena(64 * 1024);

        // Block 1: canary. mov eax, 0x5EED ; ret
        let mut e = X86Emitter::new();
        e.mov_r32_imm32(Reg32::Eax, 0x5EED);
        e.ret();
        let canary = arena.publish(e.bytes()).unwrap();
        let f: unsafe extern "C" fn() -> u32 = unsafe { mem::transmute(arena.entry(canary)) };
        assert_eq!(unsafe { f() }, 0x5EED);

        // Block 2 (published after block 1, no stale-code effects):
        // mov eax, 0x11 ; mov ecx, 0x22 ; add eax, ecx ; ret  => 0x33
        let mut e = X86Emitter::new();
        e.mov_r32_imm32(Reg32::Eax, 0x11);
        e.mov_r32_imm32(Reg32::Ecx, 0x22);
        e.add_r32_r32(Reg32::Eax, Reg32::Ecx);
        e.ret();
        let add = arena.publish(e.bytes()).unwrap();
        let f: unsafe extern "C" fn() -> u32 = unsafe { mem::transmute(arena.entry(add)) };
        assert_eq!(unsafe { f() }, 0x33);

        // Block 1 still runs after later publishes.
        let f: unsafe extern "C" fn() -> u32 = unsafe { mem::transmute(arena.entry(canary)) };
        assert_eq!(unsafe { f() }, 0x5EED);
    }
}
