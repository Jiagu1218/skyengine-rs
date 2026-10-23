//! Platform FFI shared by the probe and the [`crate::exec`] arena.
//!
//! Declared by hand (no libc dependency): the JIT crate must stay dependency
//! free so it can be pulled into any SkyEngine target, including OHOS cross
//! builds.

pub const PAGE_SIZE: usize = 4096;

#[cfg(target_os = "linux")]
pub const PROT_READ: i32 = 0x1;
#[cfg(target_os = "linux")]
pub const PROT_WRITE: i32 = 0x2;
#[cfg(target_os = "linux")]
pub const PROT_EXEC: i32 = 0x4;
#[cfg(target_os = "linux")]
pub const MAP_SHARED: i32 = 0x01;
#[cfg(target_os = "linux")]
pub const MAP_PRIVATE: i32 = 0x02;
#[cfg(target_os = "linux")]
pub const MAP_ANONYMOUS: i32 = 0x20;
#[cfg(target_os = "linux")]
pub const MAP_FAILED: usize = usize::MAX;

#[cfg(target_os = "linux")]
unsafe extern "C" {
    pub fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    pub fn munmap(addr: *mut u8, len: usize) -> i32;
    pub fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
    pub fn memfd_create(name: *const u8, flags: u32) -> i32;
    pub fn ftruncate(fd: i32, len: i64) -> i32;
    pub fn close(fd: i32) -> i32;
}

#[cfg(windows)]
pub const MEM_COMMIT: u32 = 0x0000_1000;
#[cfg(windows)]
pub const MEM_RESERVE: u32 = 0x0000_2000;
#[cfg(windows)]
pub const MEM_RELEASE: u32 = 0x8000;
#[cfg(windows)]
pub const PAGE_EXECUTE_READWRITE: u32 = 0x40;

#[cfg(windows)]
unsafe extern "system" {
    pub fn VirtualAlloc(address: *mut u8, size: usize, alloc_type: u32, protect: u32) -> *mut u8;
    pub fn VirtualFree(address: *mut u8, size: usize, free_type: u32) -> i32;
}

/// Make freshly written code visible to the instruction stream. A no-op on
/// x86_64 (coherent I-cache); flushes the range on aarch64.
///
/// On aarch64 the cache-maintenance instructions (`dc cvau`/`ic ivau`) run
/// directly from userspace, so no libgcc `__clear_cache` is needed. Refering
/// to it would break OHOS: unlike Android bionic, OHOS musl does not export
/// the symbol, and dlopen(RTLD_NOW) then fails with "symbol not found".
#[inline]
pub unsafe fn sync_icache(exec: *mut u8, len: usize) {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        // CTR_EL0 (smallest cache-line encodings) is part of the aarch64
        // Linux userspace ABI. Line sizes in bytes: 4 << field.
        let ctr: u64;
        core::arch::asm!("mrs {0}, ctr_el0", out(reg) ctr, options(nostack, preserves_flags));
        let dline = (4usize << ((ctr >> 16) & 0xF)).max(16);
        let iline = (4usize << (ctr & 0xF)).max(16);
        let start = exec as usize;
        let end = start + len;
        let mut addr = start & !(dline - 1);
        while addr < end {
            core::arch::asm!("dc cvau, {0}", in(reg) addr, options(nostack, preserves_flags));
            addr += dline;
        }
        core::arch::asm!("dsb ish", options(nostack, preserves_flags));
        addr = start & !(iline - 1);
        while addr < end {
            core::arch::asm!("ic ivau, {0}", in(reg) addr, options(nostack, preserves_flags));
            addr += iline;
        }
        core::arch::asm!("dsb ish", options(nostack, preserves_flags));
        core::arch::asm!("isb", options(nostack, preserves_flags));
    }
    #[cfg(not(target_arch = "aarch64"))]
    let _ = (exec, len);
}

/// Round `size` up to a page boundary.
#[inline]
pub const fn round_to_page(size: usize) -> usize {
    (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}
