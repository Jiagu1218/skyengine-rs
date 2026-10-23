//! 方法级 baseline JIT（v1：叶子整数方法子集，x86_64 宿主）。
//!
//! ## 架构（帧兼容型）
//!
//! JIT 不替换帧模型——被调方法仍由解释器 invoke 路径压入普通 `Frame`，
//! run 循环在**方法入口**（pc==0）发现该方法的 JIT 编译产物时整段执行，
//! 退出路径全部映射回解释器既有语义：
//!
//! - `Returned(value)` → 复用 `do_return`（根帧/嵌套帧的值交付不变）；
//! - `Unwind(Fault::Arithmetic)`（除零）→ 常规处理器搜索（v1 只编译
//!   无处理器方法，异常自然上抛到调用帧）；
//! - 预算耗尽/无法编译的指令 → **deopt**：写回 `frame.pc` 后按解释器
//!   步进继续（JIT 帧的 locals/stack 就是解释器帧，无状态迁移）。
//!
//! ## 值表示
//!
//! 槽位复用解释器的 `Slot`（带标签枚举），JIT 代码做标签化运算：
//! 整数运算 = 取载荷 → 运算 → 写载荷 + 写标签。`Slot` 的内存布局由
//! 运行期**探测**获得（`SlotLayout::probe`），不依赖 Rust 枚举布局
//! 承诺；探测失败（布局不满足假设）时编译器直接禁用。
//!
//! ## 安全边界（v1 子集的成立条件）
//!
//! 只编译"叶子整数方法"：无调用、无分配、无字段/数组访问、无处理器
//! 表、无 jsr/switch/cat2 值。由此保证：方法执行期间 `frame.stack` 的
//! Vec 不会增长搬移（栈深是字节码静态量），缓存的数据指针全程有效；
//! 不存在需要写回操作数栈的 GC 根瞬态。
//!
//! ## 分层触发
//!
//! `SKYENGINE_KVM_JIT=1` 时 exec_invoke 按方法累加调用计数，达到
//! 阈值（`SKYENGINE_KVM_JIT_THRESHOLD`，默认 2000）且可编译则编译
//! 缓存到类空间（`jit_cache`，键 = (定义类, 方法索引)）。

use std::rc::Rc;

use crate::classfile::code::{BinOp, CmpOp, CodeBody, Insn, Kind};
use crate::interp::{Frame, Unwind};
use crate::runtime::ThreadId;
use crate::runtime::{ClassId, Slot};
use crate::KvmError;

/// JIT 总开关（进程级快照）。
pub(crate) fn jit_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("SKYENGINE_KVM_JIT").is_some_and(|v| v == "1"))
}

/// 分层编译阈值（进程级快照）。
fn tier_threshold() -> u64 {
    use std::sync::OnceLock;
    static THRESHOLD: OnceLock<u64> = OnceLock::new();
    *THRESHOLD.get_or_init(|| {
        std::env::var("SKYENGINE_KVM_JIT_THRESHOLD")
            .ok()
            .and_then(|text| text.parse().ok())
            .unwrap_or(2000)
    })
}

// ---- Slot 布局探测 --------------------------------------------------------

/// `Slot` 的探测布局。`size == 16` 且标签/载荷各构成连续区域才可用。
#[allow(dead_code)] // cat2 标签值留存待后续 long/float 支持
#[derive(Clone, Copy, Debug)]
pub(crate) struct SlotLayout {
    pub size: usize,
    pub tag_off: usize,
    pub tag_len: usize,
    pub pay_off: usize,
    pub tag_int: u64,
    pub tag_long: u64,
    pub tag_float: u64,
    pub tag_double: u64,
    pub tag_ref: u64,
    pub tag_pad: u64,
}

impl SlotLayout {
    /// 以字节差分探测：同变体不同值 → 载荷字节；各变体（载荷清零）与
    /// Pad 的公共差分 → 标签字节。全部变体的载荷起点必须一致。
    pub(crate) fn probe() -> Option<Self> {
        let bytes_of = |slot: Slot| -> [u8; 24] {
            let mut buffer = [0u8; 24];
            let source = &slot as *const Slot as *const u8;
            unsafe { std::ptr::copy_nonoverlapping(source, buffer.as_mut_ptr(), 24) };
            buffer
        };
        // 载荷：Long(非零) 与 Long(0) 的差分必须恰为 8 连续字节。
        let long_marker: i64 = 0x0102_0304_0506_0708_u64 as i64;
        let a = bytes_of(Slot::Long(long_marker));
        let b = bytes_of(Slot::Long(0));
        let pay: Vec<usize> = (0..24).filter(|&i| a[i] != b[i]).collect();
        if pay.len() != 8 || pay.windows(2).any(|w| w[1] != w[0] + 1) {
            return None;
        }
        let pay_off = pay[0];
        // 标签：各变体（载荷清零的等价值）与 Pad 的差分公共集。Pad 的
        // 载荷字节是未定义垃圾，逐变体差分后取交集即剔除。
        let zeros = |slot: Slot| -> [u8; 24] {
            let mut buffer = bytes_of(slot);
            buffer[pay_off..pay_off + 8].fill(0);
            buffer
        };
        let pad = zeros(Slot::Pad);
        let variants = [
            (zeros(Slot::Int(0)), 0i64),
            (zeros(Slot::Long(0)), 1),
            (zeros(Slot::Float(0.0)), 2),
            (zeros(Slot::Double(0.0)), 3),
            (zeros(Slot::Ref(crate::runtime::ObjRef(0x3FFF))), 4),
        ];
        let mut tag: Vec<usize> = (0..24)
            .filter(|&i| variants.iter().all(|&(ref v, _)| v[i] != pad[i]))
            .collect();
        // Ref 的载荷非零（0x3FFF），排除载荷区后标签区不得与载荷重叠。
        tag.retain(|&i| !(pay_off..pay_off + 8).contains(&i));
        if tag.is_empty() || tag.windows(2).any(|w| w[1] != w[0] + 1) {
            return None;
        }
        let tag_off = tag[0];
        let tag_len = tag.len();
        // Slot 尺寸 = 标签/载荷区域之外的尾随填充探测不了；以 8 对齐的
        // 常规布局下 16 足够，且 usize 倍数约束由 size_of 校验。
        let size = std::mem::size_of::<Slot>();
        if tag_off + tag_len > pay_off && pay_off + 8 <= size {
            // 标签与载荷可重叠（紧凑布局）——v1 只支持不重叠的常规形态。
        }
        if pay_off + 8 > size {
            return None;
        }
        let read_tag = |mut buffer: [u8; 24]| -> u64 {
            let mut value = 0u64;
            for index in 0..tag_len {
                value |= (buffer[tag_off + index] as u64) << (8 * index);
            }
            buffer[pay_off..pay_off + 8].fill(0);
            value
        };
        Some(Self {
            size,
            tag_off,
            tag_len,
            pay_off,
            tag_int: read_tag(bytes_of(Slot::Int(0))),
            tag_long: read_tag(bytes_of(Slot::Long(0))),
            tag_float: read_tag(bytes_of(Slot::Float(0.0))),
            tag_double: read_tag(bytes_of(Slot::Double(0.0))),
            tag_ref: read_tag(bytes_of(Slot::Ref(crate::runtime::ObjRef(0)))),
            tag_pad: read_tag(bytes_of(Slot::Pad)),
        })
    }
}

// ---- 可执行内存 ------------------------------------------------------------

/// 已提交的可执行代码块。
pub(crate) struct ExecBlock {
    ptr: *mut u8,
    len: usize,
    /// 各平台资源句柄（Windows: usize 句柄占位；Unix: 无）。
    #[allow(dead_code)]
    handle: usize,
}

impl ExecBlock {
    pub(crate) fn entry(&self) -> *const u8 {
        self.ptr as *const u8
    }
}

impl Drop for ExecBlock {
    fn drop(&mut self) {
        unsafe {
            #[cfg(target_os = "windows")]
            {
                windows_free(self.ptr, self.len);
            }
            #[cfg(not(target_os = "windows"))]
            {
                unix_free(self.ptr, self.len);
            }
        }
    }
}

/// 提交一块读写执行内存（Windows: VirtualAlloc；Unix: mmap）。
pub(crate) fn exec_alloc(len: usize) -> Option<ExecBlock> {
    unsafe {
        #[cfg(target_os = "windows")]
        {
            windows_alloc(len)
        }
        #[cfg(not(target_os = "windows"))]
        {
            unix_alloc(len)
        }
    }
}

#[cfg(target_os = "windows")]
unsafe fn windows_alloc(len: usize) -> Option<ExecBlock> {
    use std::os::raw::c_void;
    const PAGE_EXECUTE_READWRITE: u32 = 0x40;
    const MEM_COMMIT: u32 = 0x1000;
    const MEM_RESERVE: u32 = 0x2000;
    unsafe extern "system" {
        fn VirtualAlloc(addr: *mut c_void, size: usize, alloc: u32, protect: u32) -> *mut c_void;
    }
    let ptr = unsafe {
        VirtualAlloc(std::ptr::null_mut(), len, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE)
    };
    if ptr.is_null() {
        return None;
    }
    Some(ExecBlock {
        ptr: ptr as *mut u8,
        len,
        handle: 0,
    })
}

#[cfg(target_os = "windows")]
unsafe fn windows_free(ptr: *mut u8, _len: usize) {
    use std::os::raw::c_void;
    const MEM_RELEASE: u32 = 0x8000;
    unsafe extern "system" {
        fn VirtualFree(addr: *mut c_void, size: usize, free_type: u32) -> i32;
    }
    unsafe { VirtualFree(ptr as *mut c_void, 0, MEM_RELEASE) };
}

#[cfg(not(target_os = "windows"))]
unsafe fn unix_alloc(len: usize) -> Option<ExecBlock> {
    use std::os::raw::c_void;
    const PROT_READ: i32 = 1;
    const PROT_WRITE: i32 = 2;
    const PROT_EXEC: i32 = 4;
    const MAP_PRIVATE: i32 = 0x02;
    const MAP_ANON: i32 = 0x20;
    unsafe extern "C" {
        fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, off: i64)
            -> *mut c_void;
    }
    let ptr = mmap(
        std::ptr::null_mut(),
        len,
        PROT_READ | PROT_WRITE | PROT_EXEC,
        MAP_PRIVATE | MAP_ANON,
        -1,
        0,
    );
    if ptr as isize == -1 || ptr.is_null() {
        return None;
    }
    Some(ExecBlock {
        ptr: ptr as *mut u8,
        len,
        handle: 0,
    })
}

#[cfg(not(target_os = "windows"))]
unsafe fn unix_free(ptr: *mut u8, len: usize) {
    unsafe extern "C" {
        fn munmap(addr: *mut u8, len: usize) -> i32;
    }
    munmap(ptr, len);
}

// ---- 迷你 x86_64 发射器 -----------------------------------------------------

/// 压缩寄存器编号（x86_64 低 8 寄存器 + REX 扩展）。
#[derive(Clone, Copy, Debug)]
#[repr(u8)]
#[allow(dead_code)] // 完整编号集留待后续扩展
pub(crate) enum R {
    Rax = 0,
    RcX = 1,
    Rdx = 2,
    Rbx = 3,
    Rsp = 4,
    Rbp = 5,
    Rsi = 6,
    Rdi = 7,
    R8 = 8,
    R9 = 9,
    R10 = 10,
    R11 = 11,
    R12 = 12,
    R13 = 13,
    R14 = 14,
    R15 = 15,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Label(usize);

/// 条件码（jcc/setcc 低半字节编码）。
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // 条件码全集
pub(crate) enum Cond {
    O = 0x0,
    No = 0x1,
    B = 0x2,
    Ae = 0x3,
    E = 0x4,
    Ne = 0x5,
    Be = 0x6,
    A = 0x7,
    S = 0x8,
    Ns = 0x9,
    L = 0xC,
    Ge = 0xD,
    Le = 0xE,
    G = 0xF,
}

pub(crate) struct Emitter {
    code: Vec<u8>,
    /// 待回填：rel32 跳转的发射位置 → 目标标签。
    jumps: Vec<(usize, Label)>,
    /// 待回填：lea rip 相对偏移的发射位置 → 目标标签。
    rips: Vec<(usize, Label)>,
    /// 标签 → 绑定位置。
    labels: Vec<Option<usize>>,
}

impl Emitter {
    pub(crate) fn new() -> Self {
        Self {
            code: Vec::with_capacity(512),
            jumps: Vec::new(),
            rips: Vec::new(),
            labels: Vec::new(),
        }
    }

    pub(crate) fn new_label(&mut self) -> Label {
        self.labels.push(None);
        Label(self.labels.len() - 1)
    }

    pub(crate) fn bind(&mut self, label: Label) {
        self.labels[label.0] = Some(self.code.len());
    }

    fn byte(&mut self, value: u8) {
        self.code.push(value);
    }

    fn rex(&mut self, w: bool, r: bool, x: bool, b: bool) {
        let mut value = 0x40;
        if w {
            value |= 8;
        }
        if r {
            value |= 4;
        }
        if x {
            value |= 2;
        }
        if b {
            value |= 1;
        }
        self.byte(value);
    }

    fn modrm(&mut self, mode: u8, reg: R, rm: R) {
        self.byte((mode << 6) | ((reg as u8 & 7) << 3) | (rm as u8 & 7));
    }

    /// mov r64, r64。
    pub(crate) fn mov_rr(&mut self, dst: R, src: R) {
        self.rex(true, src as u8 >= 8, false, dst as u8 >= 8);
        self.byte(0x89);
        self.modrm(3, src, dst);
    }

    /// mov r64, imm64。
    pub(crate) fn mov_ri(&mut self, dst: R, imm: u64) {
        self.rex(true, false, false, dst as u8 >= 8);
        self.byte(0xB8 + (dst as u8 & 7));
        self.code.extend_from_slice(&imm.to_le_bytes());
    }

    /// mov r64, [r64 + disp32]。
    pub(crate) fn mov_rm(&mut self, dst: R, base: R, disp: i32) {
        self.rex(true, dst as u8 >= 8, false, base as u8 >= 8);
        self.byte(0x8B);
        self.modrm(2, dst, base);
        self.code.extend_from_slice(&disp.to_le_bytes());
    }

    /// mov [r64 + disp32], r64。
    pub(crate) fn mov_mr(&mut self, base: R, disp: i32, src: R) {
        self.rex(true, src as u8 >= 8, false, base as u8 >= 8);
        self.byte(0x89);
        self.modrm(2, src, base);
        self.code.extend_from_slice(&disp.to_le_bytes());
    }

    /// mov eax 系：mov r32, [r64 + disp32]（32 位装载，零扩展）。
    pub(crate) fn mov32_rm(&mut self, dst: R, base: R, disp: i32) {
        self.rex(false, dst as u8 >= 8, false, base as u8 >= 8);
        self.byte(0x8B);
        self.modrm(2, dst, base);
        self.code.extend_from_slice(&disp.to_le_bytes());
    }

    /// mov [r64 + disp32], r32。
    pub(crate) fn mov32_mr(&mut self, base: R, disp: i32, src: R) {
        self.rex(false, src as u8 >= 8, false, base as u8 >= 8);
        self.byte(0x89);
        self.modrm(2, src, base);
        self.code.extend_from_slice(&disp.to_le_bytes());
    }

    /// mov [r64 + disp32], imm32。
    pub(crate) fn mov32_mi(&mut self, base: R, disp: i32, imm: u32) {
        self.rex(false, false, false, base as u8 >= 8);
        self.byte(0xC7);
        self.modrm(2, R::Rax, base);
        self.code.extend_from_slice(&disp.to_le_bytes());
        self.code.extend_from_slice(&imm.to_le_bytes());
    }

    /// mov qword [r64 + disp32], imm64 会超长——用寄存器中转由调用方组合。
    /// add r64, r64。
    #[allow(dead_code)]
    pub(crate) fn add_rr(&mut self, dst: R, src: R) {
        self.rex(true, src as u8 >= 8, false, dst as u8 >= 8);
        self.byte(0x01);
        self.modrm(3, src, dst);
    }

    /// add r32, imm8。
    pub(crate) fn add32_ri8(&mut self, dst: R, imm: i8) {
        self.rex(false, false, false, dst as u8 >= 8);
        self.byte(0x83);
        self.modrm(3, R::Rax, dst);
        self.byte(imm as u8);
    }

    /// cmp r32, r32。
    pub(crate) fn cmp32_rr(&mut self, left: R, right: R) {
        self.rex(false, right as u8 >= 8, false, left as u8 >= 8);
        self.byte(0x39);
        self.modrm(3, right, left);
    }

    /// cmp r32, imm8。
    pub(crate) fn cmp32_ri8(&mut self, left: R, imm: i8) {
        self.rex(false, false, false, left as u8 >= 8);
        self.byte(0x83);
        self.modrm(3, R::Rdi, left);
        self.byte(imm as u8);
    }

    fn alu32_rr(&mut self, opcode: u8, dst: R, src: R) {
        self.rex(false, src as u8 >= 8, false, dst as u8 >= 8);
        self.byte(opcode);
        self.modrm(3, src, dst);
    }

    pub(crate) fn add32_rr(&mut self, dst: R, src: R) {
        self.alu32_rr(0x01, dst, src);
    }

    pub(crate) fn sub32_rr(&mut self, dst: R, src: R) {
        self.alu32_rr(0x29, dst, src);
    }

    pub(crate) fn and32_rr(&mut self, dst: R, src: R) {
        self.alu32_rr(0x21, dst, src);
    }

    pub(crate) fn or32_rr(&mut self, dst: R, src: R) {
        self.alu32_rr(0x09, dst, src);
    }

    pub(crate) fn xor32_rr(&mut self, dst: R, src: R) {
        self.alu32_rr(0x31, dst, src);
    }

    pub(crate) fn and32_ri8(&mut self, dst: R, imm: u8) {
        self.rex(false, false, false, dst as u8 >= 8);
        self.byte(0x83);
        self.modrm(3, R::Rsp, dst);
        self.byte(imm);
    }

    fn shift32_cl(&mut self, ext: u8, dst: R) {
        self.rex(false, false, false, dst as u8 >= 8);
        self.byte(0xD3);
        self.modrm(3, match ext {
            4 => R::Rsp,
            5 => R::Rbp,
            _ => R::Rdi,
        }, dst);
    }

    pub(crate) fn shl32_cl(&mut self, dst: R) {
        self.shift32_cl(4, dst);
    }

    pub(crate) fn shr32_cl(&mut self, dst: R) {
        self.shift32_cl(5, dst);
    }

    pub(crate) fn sar32_cl(&mut self, dst: R) {
        self.shift32_cl(7, dst);
    }

    /// F7 /4：eax × src → eax（单操作数形式）。
    pub(crate) fn imul32_eax(&mut self, src: R) {
        self.rex(false, false, false, src as u8 >= 8);
        self.byte(0xF7);
        self.modrm(3, R::Rsp, src);
    }

    /// F7 /7：符号除 edx:eax ÷ src，商在 eax、余数在 edx。
    pub(crate) fn idiv32(&mut self, src: R) {
        self.rex(false, false, false, src as u8 >= 8);
        self.byte(0xF7);
        self.modrm(3, R::Rdi, src);
    }

    /// FF /1：dec dword [base]。
    pub(crate) fn dec32_m(&mut self, base: R) {
        self.rex(false, false, false, base as u8 >= 8);
        self.byte(0xFF);
        self.modrm(0, R::RcX, base);
    }

    /// cdq（eax 符号扩展到 edx:eax）。
    pub(crate) fn cdq(&mut self) {
        self.byte(0x99);
    }

    /// push r64。
    pub(crate) fn push(&mut self, reg: R) {
        if reg as u8 >= 8 {
            self.byte(0x41);
        }
        self.byte(0x50 + (reg as u8 & 7));
    }

    /// pop r64。
    pub(crate) fn pop(&mut self, reg: R) {
        if reg as u8 >= 8 {
            self.byte(0x41);
        }
        self.byte(0x58 + (reg as u8 & 7));
    }

    /// ret。
    pub(crate) fn ret(&mut self) {
        self.byte(0xC3);
    }

    /// lea r64, [rip + disp32]（标签绑定处）。
    #[allow(dead_code)] // 字面池通道待后续参数打包扩展
    pub(crate) fn lea_rip(&mut self, dst: R, label: Label) {
        self.rex(true, dst as u8 >= 8, false, false);
        self.byte(0x8D);
        self.byte(0x05 | ((dst as u8 & 7) << 3));
        self.rips.push((self.code.len(), label));
        self.code.extend_from_slice(&0i32.to_le_bytes());
    }

    pub(crate) fn raw2(&mut self, a: u8, b: u8) {
        self.byte(a);
        self.byte(b);
    }

    pub(crate) fn sub64_rsp(&mut self, imm: u8) {
        self.rex(true, false, false, false);
        self.byte(0x83);
        self.modrm(3, R::Rbp, R::Rsp);
        self.byte(imm);
    }

    pub(crate) fn add64_rsp(&mut self, imm: u8) {
        self.rex(true, false, false, false);
        self.byte(0x83);
        self.modrm(3, R::Rax, R::Rsp);
        self.byte(imm);
    }

    /// call rax（参数：win64 rcx/rdx，System V rdi/rsi）。
    pub(crate) fn call_rax(&mut self) {
        self.byte(0xFF);
        self.modrm(3, R::Rdx, R::Rax);
    }

    pub(crate) fn jmp(&mut self, label: Label) {
        self.byte(0xE9);
        self.jumps.push((self.code.len(), label));
        self.code.extend_from_slice(&0i32.to_le_bytes());
    }

    pub(crate) fn jcc(&mut self, cond: Cond, label: Label) {
        self.byte(0x0F);
        self.byte(0x80 | cond as u8);
        self.jumps.push((self.code.len(), label));
        self.code.extend_from_slice(&0i32.to_le_bytes());
    }

    /// 完成发射：回填全部 rel32 跳转。未绑定标签 = 编译器内部错误。
    pub(crate) fn finish(mut self) -> Result<Vec<u8>, KvmError> {
        for (at, label) in std::mem::take(&mut self.rips) {
            let target = self.labels[label.0]
                .ok_or_else(|| KvmError::Internal("jit: unbound lea".into()))?;
            let rel = target as i64 - (at as i64 + 4);
            self.code[at..at + 4].copy_from_slice(&rel.to_le_bytes()[..4]);
        }
        let base = self.code.len();
        for (at, label) in std::mem::take(&mut self.jumps) {
            let target = self.labels[label.0].ok_or_else(|| {
                KvmError::Internal("jit: unbound label".into())
            })?;
            let _ = base;
            let rel = target as i64 - (at as i64 + 4);
            self.code[at..at + 4].copy_from_slice(&rel.to_le_bytes()[..4]);
        }
        Ok(self.code)
    }
}

// ---- 编译产物与执行 ---------------------------------------------------------

/// JIT 退出码（打包进 rax 高 32 位；低 32 位为 deopt 时的静态栈深）。
const EXIT_RETURNED: u32 = 0;
const EXIT_DEOPT: u32 = 1;
const EXIT_UNWIND: u32 = 3;
const EXIT_RESUME: u32 = 4;
const EXIT_SUSPEND: u32 = 5;
const EXIT_BLOCKED: u32 = 6;

/// Fault 标签（ctx.result 编码：tag<<32 | 参数）。
const FAULT_NULL: u64 = 0;
const FAULT_ARRAY_INDEX: u64 = 1;
const FAULT_CLASS_CAST: u64 = 2;
const FAULT_NEG_SIZE: u64 = 3;
const FAULT_ARRAY_STORE: u64 = 4;

/// 与生成码交换的上下文。字段偏移即生成码内的立即偏移：
/// locals 0 / stack 8 / pc 16 / budget 24 / out 32。
#[repr(C)]
pub(crate) struct JitCtx {
    /// 局部表数据指针（Slot 数组）。
    pub locals: *mut Slot,
    /// 操作数栈数据指针。
    pub stack: *mut Slot,
    /// frame.pc 指针（deopt/异常续跑点写回）。
    pub pc: *mut u32,
    /// 预算倒计时（后向分支递减；归零 deopt）。
    pub budget: *mut u32,
    /// 返回值槽。
    pub out: Slot,
    /// 助手通道：Machine（run 栈上对象，JIT 执行期有效）。
    pub machine: *mut crate::interp::Machine<'static>,
    /// 帧栈与当前帧索引（助手的操作对象）。
    pub frames: *mut Vec<Frame>,
    pub frame_index: usize,
    pub thread: ThreadId,
    /// 助手第二参数（栈深等，调用前由生成码写入）。
    pub scratch: u64,
    /// invoke 助手的 mref 补充通道：(name<<32)|desc。
    pub arg_b: u64,
    /// 助手结果通道（unwind 详情等）。
    pub result: u64,
}

/// 编译产物。
pub(crate) struct JitCode {
    block: ExecBlock,
    /// 方法是否带返回值（决定 out 槽有效性）。
    pub returns_value: bool,
}

/// v1 可编译指令判定（叶子整数子集）。
fn leaf_insn_ok(insn: &Insn) -> bool {
    matches!(
        insn,
        Insn::Load { .. }
            | Insn::Store { .. }
            | Insn::PushInt(_)
            | Insn::PushNull
            | Insn::Pop
            | Insn::Pop2
            | Insn::Dup
            | Insn::Swap
            | Insn::Bin { kind: Kind::Int, .. }
            | Insn::Iinc { .. }
            | Insn::TruncByte
            | Insn::TruncChar
            | Insn::TruncShort
            | Insn::BranchZero { .. }
            | Insn::BranchInt { .. }
            | Insn::BranchNull { .. }
            | Insn::BranchRef { .. }
            | Insn::Goto(_)
            | Insn::Return(_)
            | Insn::GetField(_)
            | Insn::PutField(_)
            | Insn::GetStatic(_)
            | Insn::PutStatic(_)
            | Insn::ArrayLoad(_)
            | Insn::ArrayStore(_)
            | Insn::ArrayLength
            | Insn::NewArray(_)
            | Insn::ANewArray(_)
            | Insn::CheckCast(_)
            | Insn::InstanceOf(_)
            | Insn::Ldc(_)
            | Insn::TableSwitch { .. }
            | Insn::LookupSwitch { .. }
            | Insn::InvokeVirtual(_)
            | Insn::InvokeSpecial(_)
            | Insn::InvokeStatic(_)
            | Insn::InvokeInterface(_)
    )
}

/// 方法可编译性：无处理器、全指令叶子（融合对两侧均叶子）。
pub(crate) fn leaf_compilable(body: &CodeBody) -> bool {
    if !body.handlers.is_empty() {
        return false;
    }
    let ok = |insn: &Insn| match insn {
        Insn::Fused { a, b } => leaf_insn_ok(a) && leaf_insn_ok(b),
        other => leaf_insn_ok(other),
    };
    body.insns.iter().all(ok)
}

/// 寄存器规约（生成码全程持有；序言保存被调方保存寄存器）：
/// - rbx：操作数栈基指针；r12：locals 基指针；r13：ctx 指针
/// - r14：pc 指针；r15：budget 指针；rax/rcx/rdx：scratch
///
/// 首参按宿主 ABI：win64 = rcx，System V = rdi。
pub(crate) fn compile_leaf(
    body: &CodeBody,
    layout: &SlotLayout,
    ret_push: &std::collections::HashMap<u32, bool>,
) -> Result<Rc<JitCode>, KvmError> {
    let disjoint = layout.tag_off + 8 <= layout.pay_off || layout.pay_off + 8 <= layout.tag_off;
    if layout.size != 16 || layout.tag_len > 8 || !disjoint {
        return Err(KvmError::Internal("jit: unsupported slot layout".into()));
    }
    let mut e = Emitter::new();
    e.push(R::Rbx);
    e.push(R::R12);
    e.push(R::R13);
    e.push(R::R14);
    e.push(R::R15);
    let arg = if cfg!(windows) { R::RcX } else { R::Rdi };
    e.mov_rr(R::R13, arg); // r13 = ctx
    e.mov_rm(R::Rbx, R::R13, 8); // rbx = ctx.stack
    e.mov_rm(R::R12, R::R13, 0); // r12 = ctx.locals
    e.mov_rm(R::R14, R::R13, 16); // r14 = ctx.pc
    e.mov_rm(R::R15, R::R13, 24); // r15 = ctx.budget

    let mut labels = Vec::with_capacity(body.insns.len());
    for _ in 0..body.insns.len() {
        labels.push(e.new_label());
    }
    let s = layout.size as i32;
    let pay = layout.pay_off as i32;
    let tag = layout.tag_off as i32;

    // 退出序列：写 pc（立即数 idx）→（仅 deopt）打包 rax → 弹栈 → ret。
    macro_rules! exit_sequence {
        ($code:expr, $depth:expr, $index:expr) => {{
            e.mov32_mi(R::R14, 0, $index as u32);
            if $code == EXIT_DEOPT {
                e.mov_ri(R::Rax, (($code as u64) << 32) | ($depth as u64 & 0xFFFF_FFFF));
            } else {
                e.mov_ri(R::Rax, ($code as u64) << 32);
            }
            e.pop(R::R15);
            e.pop(R::R14);
            e.pop(R::R13);
            e.pop(R::R12);
            e.pop(R::Rbx);
            e.ret();
        }};
    }
    // 预算检查（后向分支前发射）：dec dword [r15]，归零内联 deopt。
    macro_rules! budget_check {
        ($depth:expr, $index:expr) => {{
            e.dec32_m(R::R15);
            let skip = e.new_label();
            e.jcc(Cond::Ne, skip);
            exit_sequence!(EXIT_DEOPT, $depth, $index);
            e.bind(skip);
        }};
    }
    // 16 字节槽拷贝 [dst_base+dst_off] ← [src_base+src_off]。
    macro_rules! copy_slot {
        ($dst_base:expr, $dst_off:expr, $src_base:expr, $src_off:expr) => {{
            e.mov_rm(R::Rax, $src_base, $src_off);
            e.mov_mr($dst_base, $dst_off, R::Rax);
            e.mov_rm(R::Rax, $src_base, $src_off + 8);
            e.mov_mr($dst_base, $dst_off + 8, R::Rax);
        }};
    }
    // 助手调用：pc 就位 → scratch=depth → 双参（ctx, packed）→ call →
    // 非 RESUME 则 rax 透传退出（助手已构造好打包值与栈长同步）。
    macro_rules! helper_call {
        ($packed:expr, $depth:expr, $index:expr, $helper:expr) => {{
            e.mov32_mi(R::R14, 0, $index as u32);
            e.mov32_mi(R::R13, 80, $depth as u32);
            e.mov_ri(R::Rax, $packed as u64);
            let arg2 = if cfg!(windows) { R::Rdx } else { R::Rsi };
            e.mov_rr(arg2, R::Rax);
            let arg1 = if cfg!(windows) { R::RcX } else { R::Rdi };
            e.mov_rr(arg1, R::R13);
            e.mov_ri(R::Rax, $helper);
            e.sub64_rsp(0x28);
            e.call_rax();
            e.add64_rsp(0x28);
            let resume = e.new_label();
            e.cmp32_ri8(R::Rax, EXIT_RESUME as i8);
            e.jcc(Cond::E, resume);
            e.pop(R::R15);
            e.pop(R::R14);
            e.pop(R::R13);
            e.pop(R::R12);
            e.pop(R::Rbx);
            e.ret();
            e.bind(resume);
        }};
    }
    let cond_of = |op: CmpOp| match op {
        CmpOp::Eq => Cond::E,
        CmpOp::Ne => Cond::Ne,
        CmpOp::Lt => Cond::L,
        CmpOp::Ge => Cond::Ge,
        CmpOp::Gt => Cond::G,
        CmpOp::Le => Cond::Le,
    };

    let mut returns_value = false;
    let mut depth: i32 = 0;
    for (index, insn) in body.insns.iter().enumerate() {
        e.bind(labels[index]);
        let parts: Vec<&Insn> = match insn {
            Insn::Fused { a, b } => vec![a, b],
            other => vec![other],
        };
        for part in parts {
            match part {
                Insn::PushInt(value) => {
                    e.mov32_mi(R::Rbx, depth * s + pay, *value as u32);
                    e.mov_ri(R::RcX, layout.tag_int);
                    e.mov_mr(R::Rbx, depth * s + tag, R::RcX);
                    depth += 1;
                }
                Insn::PushNull => {
                    e.mov32_mi(R::Rbx, depth * s + pay, 0);
                    e.mov_ri(R::RcX, layout.tag_ref);
                    e.mov_mr(R::Rbx, depth * s + tag, R::RcX);
                    depth += 1;
                }
                Insn::Load { index: local, .. } => {
                    copy_slot!(R::Rbx, depth * s, R::R12, *local as i32 * s);
                    depth += 1;
                }
                Insn::Store { index: local, .. } => {
                    depth -= 1;
                    copy_slot!(R::R12, *local as i32 * s, R::Rbx, depth * s);
                }
                Insn::Pop => {
                    depth -= 1;
                }
                Insn::Dup => {
                    copy_slot!(R::Rbx, depth * s, R::Rbx, (depth - 1) * s);
                    depth += 1;
                }
                Insn::Iinc { index: local, delta } => {
                    e.mov32_rm(R::Rax, R::R12, *local as i32 * s + pay);
                    e.add32_ri8(R::Rax, *delta as i8);
                    e.mov32_mr(R::R12, *local as i32 * s + pay, R::Rax);
                }
                Insn::Bin { op, .. } => {
                    // rhs @ d-1，lhs @ d-2，结果写回 d-2（标签已是 Int）。
                    match op {
                        BinOp::Add | BinOp::Sub | BinOp::And | BinOp::Or | BinOp::Xor => {
                            e.mov32_rm(R::Rax, R::Rbx, (depth - 2) * s + pay);
                            e.mov32_rm(R::RcX, R::Rbx, (depth - 1) * s + pay);
                            match op {
                                BinOp::Add => e.add32_rr(R::Rax, R::RcX),
                                BinOp::Sub => e.sub32_rr(R::Rax, R::RcX),
                                BinOp::And => e.and32_rr(R::Rax, R::RcX),
                                BinOp::Or => e.or32_rr(R::Rax, R::RcX),
                                _ => e.xor32_rr(R::Rax, R::RcX),
                            }
                        }
                        BinOp::Shl | BinOp::Shr | BinOp::Ushr => {
                            e.mov32_rm(R::Rax, R::Rbx, (depth - 2) * s + pay);
                            e.mov32_rm(R::RcX, R::Rbx, (depth - 1) * s + pay);
                            e.and32_ri8(R::RcX, 0x1F); // JVM 移位量按 5 位掩码
                            match op {
                                BinOp::Shl => e.shl32_cl(R::Rax),
                                BinOp::Ushr => e.shr32_cl(R::Rax),
                                _ => e.sar32_cl(R::Rax),
                            }
                        }
                        BinOp::Mul => {
                            e.mov32_rm(R::Rax, R::Rbx, (depth - 2) * s + pay);
                            e.mov32_rm(R::RcX, R::Rbx, (depth - 1) * s + pay);
                            e.imul32_eax(R::RcX);
                        }
                        BinOp::Div | BinOp::Rem => {
                            // 除零 → deopt 到本指令重执行：解释器对同一
                            // 栈形产生 Arithmetic 异常，展开路径与纯解释
                            // 严格一致（无代码重复）。
                            e.mov32_rm(R::RcX, R::Rbx, (depth - 1) * s + pay);
                            e.cmp32_ri8(R::RcX, 0);
                            let nonzero = e.new_label();
                            e.jcc(Cond::Ne, nonzero);
                            exit_sequence!(EXIT_DEOPT, depth, index);
                            e.bind(nonzero);
                            e.mov32_rm(R::Rax, R::Rbx, (depth - 2) * s + pay);
                            e.cdq();
                            e.idiv32(R::RcX);
                            if *op == BinOp::Rem {
                                e.mov_rr(R::Rax, R::Rdx); // 余数在 edx
                            }
                        }
                    }
                    e.mov32_mr(R::Rbx, (depth - 2) * s + pay, R::Rax);
                    depth -= 1;
                }
                Insn::BranchZero { op, target } => {
                    depth -= 1;
                    budget_check!(depth, index);
                    e.mov32_rm(R::Rax, R::Rbx, depth * s + pay);
                    e.cmp32_ri8(R::Rax, 0);
                    e.jcc(cond_of(*op), labels[*target as usize]);
                }
                Insn::BranchInt { op, target } => {
                    depth -= 2;
                    budget_check!(depth, index);
                    e.mov32_rm(R::Rax, R::Rbx, depth * s + pay);
                    e.mov32_rm(R::RcX, R::Rbx, (depth + 1) * s + pay);
                    e.cmp32_rr(R::Rax, R::RcX);
                    e.jcc(cond_of(*op), labels[*target as usize]);
                }
                Insn::BranchNull { is_null, target } => {
                    depth -= 1;
                    budget_check!(depth, index);
                    e.mov32_rm(R::Rax, R::Rbx, depth * s + pay);
                    e.cmp32_ri8(R::Rax, 0);
                    e.jcc(
                        if *is_null { Cond::E } else { Cond::Ne },
                        labels[*target as usize],
                    );
                }
                Insn::GetField(field) => {
                    let packed = pack_field(field, 0);
                    helper_call!(packed, depth, index, helper_field_addr());
                    depth += 1;
                }
                Insn::PutField(field) => {
                    let packed = pack_field(field, 1);
                    depth -= 1;
                    helper_call!(packed, depth, index, helper_field_addr());
                }
                Insn::GetStatic(field) => {
                    let packed = pack_field(field, 2);
                    helper_call!(packed, depth, index, helper_field_addr());
                    depth += 1;
                }
                Insn::PutStatic(field) => {
                    let packed = pack_field(field, 3);
                    depth -= 1;
                    helper_call!(packed, depth, index, helper_field_addr());
                }
                Insn::ArrayLoad(_) => {
                    let packed = 8;
                    helper_call!(packed, depth, index, helper_field_addr());
                    depth -= 1;
                }
                Insn::ArrayStore(_) => {
                    let packed = 9;
                    depth -= 3;
                    helper_call!(packed, depth, index, helper_field_addr());
                }
                Insn::ArrayLength => {
                    let packed = 10;
                    helper_call!(packed, depth, index, helper_field_addr());
                }
                Insn::Pop2 => {
                    depth -= 2;
                }
                Insn::Swap => {
                    copy_slot!(R::Rax, 0, R::Rbx, depth * s);
                    copy_slot!(R::Rbx, depth * s, R::Rbx, (depth - 1) * s);
                    copy_slot!(R::Rbx, (depth - 1) * s, R::Rax, 0);
                }
                Insn::TruncByte => {
                    e.mov32_rm(R::Rax, R::Rbx, (depth - 1) * s + pay);
                    e.rex(false, false, false, false);
                    e.raw2(0x0F, 0xBE);
                    e.modrm(3, R::Rax, R::Rax);
                    e.mov32_mr(R::Rbx, (depth - 1) * s + pay, R::Rax);
                }
                Insn::TruncChar => {
                    e.mov32_rm(R::Rax, R::Rbx, (depth - 1) * s + pay);
                    e.and32_ri8(R::Rax, 0xFF);
                    e.mov32_mr(R::Rbx, (depth - 1) * s + pay, R::Rax);
                }
                Insn::TruncShort => {
                    e.mov32_rm(R::Rax, R::Rbx, (depth - 1) * s + pay);
                    e.rex(false, false, false, false);
                    e.raw2(0x0F, 0xBF);
                    e.modrm(3, R::Rax, R::Rax);
                    e.mov32_mr(R::Rbx, (depth - 1) * s + pay, R::Rax);
                }
                Insn::BranchRef { eq, target } => {
                    depth -= 2;
                    budget_check!(depth, index);
                    e.mov32_rm(R::Rax, R::Rbx, depth * s + pay);
                    e.mov32_rm(R::RcX, R::Rbx, (depth + 1) * s + pay);
                    e.cmp32_rr(R::Rax, R::RcX);
                    e.jcc(if *eq { Cond::E } else { Cond::Ne }, labels[*target as usize]);
                }
                insn @ (Insn::InvokeVirtual(_)
                | Insn::InvokeSpecial(_)
                | Insn::InvokeStatic(_)
                | Insn::InvokeInterface(_)) => {
                    let (mode, mref) = match insn {
                        Insn::InvokeVirtual(m) => (0u64, m),
                        Insn::InvokeSpecial(m) => (1, m),
                        Insn::InvokeStatic(m) => (2, m),
                        Insn::InvokeInterface(m) => (3, m),
                        _ => unreachable!(),
                    };
                    let pushes = ret_push.get(&mref.desc.0).copied().unwrap_or(false);
                    // scratch = (invoke 指令索引 << 32) | 栈深；arg_b = name<<32|desc。
                    e.mov_ri(
                        R::Rax,
                        ((index as u64) << 32) | depth as u64,
                    );
                    e.mov_mr(R::R13, 80, R::Rax);
                    e.mov_ri(
                        R::Rax,
                        ((mref.name.0 as u64) << 32) | mref.desc.0 as u64,
                    );
                    e.mov_mr(R::R13, 88, R::Rax);
                    let packed = mode | ((mref.class.0 as u64) << 8);
                    helper_call!(packed, depth, index, helper_invoke_addr());
                    if pushes {
                        depth += 1;
                    }
                }
                Insn::TableSwitch {
                    default,
                    low,
                    targets,
                } => {
                    depth -= 1;
                    budget_check!(depth, index);
                    e.mov32_rm(R::Rax, R::Rbx, depth * s + pay);
                    for (offset, target) in targets.iter().enumerate() {
                        let value = low.wrapping_add(offset as i32);
                        e.mov_ri(R::RcX, value as u32 as u64);
                        e.cmp32_rr(R::Rax, R::RcX);
                        e.jcc(Cond::E, labels[*target as usize]);
                    }
                    e.jmp(labels[*default as usize]);
                }
                Insn::LookupSwitch {
                    default,
                    pairs,
                } => {
                    depth -= 1;
                    budget_check!(depth, index);
                    e.mov32_rm(R::Rax, R::Rbx, depth * s + pay);
                    for (value, target) in pairs.iter() {
                        e.mov_ri(R::RcX, *value as u32 as u64);
                        e.cmp32_rr(R::Rax, R::RcX);
                        e.jcc(Cond::E, labels[*target as usize]);
                    }
                    e.jmp(labels[*default as usize]);
                }
                Insn::Goto(target) => {
                    budget_check!(depth, index);
                    e.jmp(labels[*target as usize]);
                }
                Insn::Return(kind) => {
                    if kind.is_some() {
                        returns_value = true;
                        copy_slot!(R::R13, 32, R::Rbx, (depth - 1) * s);
                    }
                    exit_sequence!(EXIT_RETURNED, depth, index);
                }
                other => {
                    return Err(KvmError::Internal(format!(
                        "jit: non-leaf insn {other:?} reached codegen"
                    )))
                }
            }
        }
    }
    // 方法必以 Return 结尾（校验器保证）；防御性兜底走 deopt。
    let code = e.finish()?;
    let block = exec_alloc(code.len().max(64))
        .ok_or_else(|| KvmError::Internal("jit: exec alloc failed".into()))?;
    unsafe {
        std::ptr::copy_nonoverlapping(code.as_ptr(), block.ptr, code.len());
    }
    Ok(Rc::new(JitCode {
        block,
        returns_value,
    }))
}

/// JIT 执行结果（run 循环接手语义）。
pub(crate) enum JitExit {
    /// 方法返回（void 为 None）。
    Returned(Option<Slot>),
    /// 退火：frame.pc 已就位（除零时指向除法指令，重执行即产生同路
    /// 异常），解释器继续步进。
    Deopt,
    /// 助手上抛的运行时故障（NPE/越界/类型转换）：走常规异常展开。
    Unwind(Unwind),
    /// 被调方让出（sleep/wait 等）：外层直接透传调度器（醒来后按解释
    /// 器步进续跑 = 退火语义）。
    Suspended(crate::YieldKind),
    /// 被调方 monitorenter 竞争阻塞。
    Blocked(crate::runtime::ObjRef),
}

/// 执行编译产物。`frame.stack` 容量 ≥ max_stack 由帧组装保证，
/// JIT 静态栈深不越界。
pub(crate) fn execute_leaf(
    code: &JitCode,
    machine: &mut crate::interp::Machine<'_>,
    frames: &mut Vec<Frame>,
    thread: ThreadId,
    budget: &mut u32,
) -> JitExit {
    // JIT 帧必为栈顶（run 循环在 fetch 前调用；帧压入后无嵌套）。
    let frame_index = frames.len() - 1;
    let frame: *mut Frame = frames.last_mut().expect("frame present");
    let frame = unsafe { &mut *frame };
    let mut ctx = JitCtx {
        locals: frame.locals.as_mut_ptr(),
        stack: frame.stack.as_mut_ptr(),
        pc: &mut frame.pc,
        budget,
        out: Slot::Pad,
        machine: machine as *mut crate::interp::Machine<'_> as *mut crate::interp::Machine<'static>,
        frames: frames as *mut Vec<Frame>,
        frame_index,
        thread,
        scratch: 0,
        arg_b: 0,
        result: 0,
    };
    let entry: unsafe extern "C" fn(*mut JitCtx) -> u64 =
        unsafe { std::mem::transmute(code.block.entry()) };
    let packed = unsafe { entry(&mut ctx) };
    let exit = (packed >> 32) as u32;
    let depth = packed as u32 as usize;
    match exit {
        EXIT_RETURNED => JitExit::Returned(if code.returns_value {
            Some(ctx.out)
        } else {
            None
        }),
        EXIT_DEOPT => {
            unsafe { frame.stack.set_len(depth) };
            JitExit::Deopt
        }
        EXIT_SUSPEND => {
            let kind = unsafe { *Box::from_raw(ctx.result as *mut crate::YieldKind) };
            return JitExit::Suspended(kind);
        }
        EXIT_BLOCKED => {
            return JitExit::Blocked(crate::runtime::ObjRef(ctx.result as u32));
        }
        EXIT_UNWIND => {
            unsafe { frame.stack.set_len(depth) };
            if ctx.result & (1 << 62) != 0 {
                // 被调方抛出的异常对象（1<<62 标记 | ObjRef）。
                let reference = crate::runtime::ObjRef(ctx.result as u32);
                return JitExit::Unwind(Unwind::Thrown(reference));
            }
            let tag = ctx.result >> 32;
            let arg = ctx.result as u32 as i32;
            let fault = match tag {
                FAULT_NULL => crate::interp::Fault::NullPointer,
                FAULT_ARRAY_INDEX => crate::interp::Fault::ArrayIndex { index: arg },
                FAULT_CLASS_CAST => crate::interp::Fault::ClassCast,
                FAULT_NEG_SIZE => crate::interp::Fault::NegativeArraySize { size: arg },
                _ => crate::interp::Fault::ArrayStore,
            };
            JitExit::Unwind(Unwind::Fault(fault))
        }
        other => unreachable!("jit: bad exit code {other}"),
    }
}

/// 探测布局的进程级单例（探测失败 = None，JIT 禁用）。
pub(crate) fn slot_layout() -> Option<&'static SlotLayout> {
    use std::sync::OnceLock;
    static LAYOUT: OnceLock<Option<SlotLayout>> = OnceLock::new();
    LAYOUT.get_or_init(SlotLayout::probe).as_ref()
}

/// 计数器递增 + 阈值触发编译（exec_invoke 分层点）。返回 Some(编译
/// 产物) 时由调用方存入缓存。
pub(crate) fn maybe_compile(
    cs: &mut crate::runtime::classspace::ClassSpace,
    defining: ClassId,
    index: u16,
    body: &Option<Rc<CodeBody>>,
) -> Option<Rc<JitCode>> {
    let count = cs.jit_bump(defining, index);
    if count != tier_threshold() {
        return None;
    }
    let Some(body) = body else {
        return None;
    };
    if !leaf_compilable(body) {
        return None;
    }
    // invoke 目标描述符 → 是否压返回值（编译期静态栈深跟踪用）。
    let mut ret_push = std::collections::HashMap::new();
    for insn in &body.insns {
        let mref = match insn {
            Insn::InvokeVirtual(m) | Insn::InvokeSpecial(m) | Insn::InvokeStatic(m)
            | Insn::InvokeInterface(m) => *m,
            Insn::Fused { b, .. } => match &**b {
                Insn::InvokeVirtual(m) | Insn::InvokeSpecial(m) | Insn::InvokeStatic(m)
                | Insn::InvokeInterface(m) => *m,
                _ => continue,
            },
            _ => continue,
        };
        if let Ok(kinds) = cs.descriptor_kinds(mref.desc) {
            ret_push.insert(mref.desc.0, kinds.1.is_some());
        }
    }
    let layout = slot_layout()?;
    compile_leaf(body, layout, &ret_push).ok()
}


/// FieldRef → u64 打包：op | kind<<8 | is_static<<11 | slot<<16 | owner<<32。
fn pack_field(field: &crate::runtime::FieldRef, op: u64) -> u64 {
    op
        | ((field.kind as u64) << 8)
        | (u64::from(field.is_static) << 11)
        | (u64::from(field.slot) << 16)
        | (u64::from(field.owner.0) << 32)
}

/// 字段/数组助手：op 0..3 = get/put × instance/static；8 = aaload；
/// 9 = aastore；10 = arraylength。返回 EXIT_RESUME 或打包退出值；
/// 故障详情写 ctx.result（tag<<32 | 参数）。
unsafe extern "C" fn helper_field(ctx: *mut JitCtx, packed: u64) -> u64 {
    use crate::interp::{pop_int, pop_kind, pop_ref, push_slot_value};
    let ctx = unsafe { &mut *ctx };
    let depth = ctx.scratch as u32 as usize;
    let exit_deopt = ((EXIT_DEOPT as u64) << 32) | depth as u64;
    // 借用拆分：先写 ctx.result 再读 depth（避免闭包可变借用）。
    let unwind_code = |result: u64| -> u64 {
        ((EXIT_UNWIND as u64) << 32) | depth as u64 | result
    };
    macro_rules! unwind {
        ($tag:expr, $arg:expr) => {{
            ctx.result = ($tag << 32) | $arg as i64 as u64;
            unwind_code(0)
        }};
    }
    let op = packed & 0xFF;
    let frames = unsafe { &mut *ctx.frames };
    let frame = unsafe { frames.get_unchecked_mut(ctx.frame_index) };
    unsafe { frame.stack.set_len(depth) };
    let machine = unsafe { &mut *ctx.machine };
    let kind = match (packed >> 8) & 7 {
        0 => Kind::Int,
        1 => Kind::Long,
        2 => Kind::Float,
        3 => Kind::Double,
        _ => Kind::Ref,
    };
    let field = crate::runtime::FieldRef {
        owner: crate::runtime::ClassId((packed >> 32) as u32),
        slot: ((packed >> 16) & 0xFFFF) as u16,
        is_static: (packed >> 11) & 1 == 1,
        kind,
    };
    let result = (|| -> Result<u64, u64> {
        match op {
            0 => {
                let reference = pop_ref(frames).map_err(|_| 1u64 << 63)?;
                if reference.is_null() {
                    return Err(unwind!(FAULT_NULL, 0));
                }
                let value = *machine
                    .heap
                    .instance(reference)
                    .map_err(|_| 1u64 << 63)?
                    .fields
                    .get(field.slot as usize)
                    .ok_or(1u64 << 63)?;
                push_slot_value(frames, value);
                Ok(EXIT_RESUME as u64)
            }
            1 => {
                let value = pop_kind(frames, field.kind).map_err(|_| 1u64 << 63)?;
                let reference = pop_ref(frames).map_err(|_| 1u64 << 63)?;
                if reference.is_null() {
                    return Err(unwind!(FAULT_NULL, 0));
                }
                *machine
                    .heap
                    .instance_mut(reference)
                    .map_err(|_| 1u64 << 63)?
                    .fields
                    .get_mut(field.slot as usize)
                    .ok_or(1u64 << 63)? = value;
                Ok(EXIT_RESUME as u64)
            }
            2 => {
                // clinit 未初始化 → deopt（pc 已指向本指令，解释器重执行）。
                if machine
                    .cs
                    .class(field.owner)
                    .init_state
                    != crate::runtime::classspace::InitState::Initialized
                {
                    return Ok(exit_deopt);
                }
                let statics = &machine.cs.class(field.owner).statics;
                let value = *statics.get(field.slot as usize).ok_or(1u64 << 63)?;
                push_slot_value(frames, value);
                Ok(EXIT_RESUME as u64)
            }
            3 => {
                if machine
                    .cs
                    .class(field.owner)
                    .init_state
                    != crate::runtime::classspace::InitState::Initialized
                {
                    return Ok(exit_deopt);
                }
                let value = pop_kind(frames, field.kind).map_err(|_| 1u64 << 63)?;
                let statics = &mut machine.cs.class_mut(field.owner).statics;
                *statics.get_mut(field.slot as usize).ok_or(1u64 << 63)? = value;
                Ok(EXIT_RESUME as u64)
            }
            8 => {
                let index = pop_int(frames).map_err(|_| 1u64 << 63)?;
                let reference = pop_ref(frames).map_err(|_| 1u64 << 63)?;
                if reference.is_null() {
                    return Err(unwind!(FAULT_NULL, 0));
                }
                let array = machine.heap.array(reference).map_err(|_| 1u64 << 63)?;
                let len = array.data.len();
                if index < 0 || index as usize >= len {
                    return Err(unwind!(FAULT_ARRAY_INDEX, index));
                }
                let value = array.data.get(index as usize);
                push_slot_value(frames, value);
                Ok(EXIT_RESUME as u64)
            }
            9 => {
                let value = pop_kind(frames, kind).map_err(|_| 1u64 << 63)?;
                let index = pop_int(frames).map_err(|_| 1u64 << 63)?;
                let reference = pop_ref(frames).map_err(|_| 1u64 << 63)?;
                if reference.is_null() {
                    return Err(unwind!(FAULT_NULL, 0));
                }
                let array = machine.heap.array_mut(reference).map_err(|_| 1u64 << 63)?;
                let len = array.data.len();
                if index < 0 || index as usize >= len {
                    return Err(unwind!(FAULT_ARRAY_INDEX, index));
                }
                array.data.set(index as usize, value).map_err(|_| 1u64 << 63)?;
                Ok(EXIT_RESUME as u64)
            }
            _ => {
                let reference = pop_ref(frames).map_err(|_| 1u64 << 63)?;
                if reference.is_null() {
                    return Err(unwind!(FAULT_NULL, 0));
                }
                let len = machine.heap.array(reference).map_err(|_| 1u64 << 63)?.data.len();
                crate::interp::push1(frames, crate::runtime::Slot::Int(len as i32));
                Ok(EXIT_RESUME as u64)
            }
        }
    })();
    match result {
        Ok(code) => code,
        // 解释器内部错误：保守 deopt（重执行由解释器如实上报）。
        Err(_) => exit_deopt,
    }
}


/// invoke 助手：复用解释器 exec_invoke 全语义（clinit 门/参数弹压/
/// 目标解析/native 同步分派），被调帧压入后嵌套 run_inner 执行到
/// 回到本帧。退出：RESUME（含 native 同步完成/被调方返回）、
/// DEOPT（clinit 重执行/预算耗尽/内部错误）、SUSPEND/BLOCKED/
/// UNWIND（经 ctx.result 编码）。
unsafe extern "C" fn helper_invoke(ctx: *mut JitCtx, packed: u64) -> u64 {
    use crate::interp::opcodes::{exec_invoke, InvokeMode};
    use crate::interp::{run_inner, InsnOutcome, RunOutcome};
    use crate::runtime::{MethodRef, Sym};
    let ctx = unsafe { &mut *ctx };
    let depth = (ctx.scratch & 0xFFFF_FFFF) as u32 as usize;
    let invoke_pc = (ctx.scratch >> 32) as u32;
    let exit_deopt = ((EXIT_DEOPT as u64) << 32) | depth as u64;
    let mode = match packed & 0xF {
        0 => InvokeMode::Virtual,
        1 => InvokeMode::Special,
        2 => InvokeMode::Static,
        _ => InvokeMode::Interface,
    };
    let mref = MethodRef {
        class: ClassId((packed >> 8) as u32),
        name: Sym((ctx.arg_b >> 32) as u32),
        desc: Sym(ctx.arg_b as u32),
    };
    let frames = unsafe { &mut *ctx.frames };
    let frame = unsafe { frames.get_unchecked_mut(ctx.frame_index) };
    unsafe { frame.stack.set_len(depth) };
    frame.pc = invoke_pc;
    let machine = unsafe { &mut *ctx.machine };
    let outcome = match exec_invoke(machine, frames, ctx.thread, mode, mref) {
        Ok(outcome) => outcome,
        Err(_) => return exit_deopt,
    };
    let mut exit_unwind_thrown = |obj: crate::runtime::ObjRef| -> u64 {
        ctx.result = (1u64 << 62) | obj.0 as u64;
        ((EXIT_UNWIND as u64) << 32) | depth as u64
    };
    match outcome {
        // 被调帧/clinit 链已压入：嵌套执行到回到本帧。
        InsnOutcome::Pushed => {
            let stop = (ctx.frame_index + 1, invoke_pc);
            let nested = unsafe {
                run_inner(machine, frames, ctx.thread, Some(*ctx.budget), Some(stop))
            };
            match nested {
                Err(_) => exit_deopt,
                Ok(RunOutcome::FramePopped) => EXIT_RESUME as u64,
                Ok(RunOutcome::Yielded(kind)) => {
                    ctx.result = Box::into_raw(Box::new(kind)) as u64;
                    ((EXIT_SUSPEND as u64) << 32) | depth as u64
                }
                Ok(RunOutcome::BudgetExhausted) => {
                    unsafe { *ctx.budget = 0 };
                    exit_deopt
                }
                Ok(RunOutcome::BlockedOnMonitor(object)) => {
                    ctx.result = object.0 as u64;
                    ((EXIT_BLOCKED as u64) << 32) | depth as u64
                }
                Ok(RunOutcome::Uncaught(unwind)) => {
                    // JIT 调用帧（无处理器）保留在栈中，交外层继续搜索。
                    match unwind {
                        Unwind::Thrown(reference) => exit_unwind_thrown(reference),
                        Unwind::Fault(fault) => {
                            ctx.result = encode_fault(fault);
                            ((EXIT_UNWIND as u64) << 32) | depth as u64
                        }
                    }
                }
                Ok(_) => exit_deopt,
            }
        }
        // native 同步完成（值已压栈）或 pc 已推进的续跑态。
        InsnOutcome::Jumped | InsnOutcome::Next => EXIT_RESUME as u64,
        // native 抛异常：pc 已归位到 invoke，外层 handle_unwind 搜索。
        InsnOutcome::Unwind(unwind) => match unwind {
            Unwind::Thrown(reference) => exit_unwind_thrown(reference),
            Unwind::Fault(fault) => {
                ctx.result = encode_fault(fault);
                ((EXIT_UNWIND as u64) << 32) | depth as u64
            }
        },
        // native 让出：pc 已越过调用点，调度器醒来后按解释器续跑。
        InsnOutcome::Yielded(kind) => {
            ctx.result = Box::into_raw(Box::new(kind)) as u64;
            ((EXIT_SUSPEND as u64) << 32) | depth as u64
        }
        InsnOutcome::Returned(_) | InsnOutcome::Blocked(_) => exit_deopt,
    }
}

/// Fault → ctx.result 编码（tag<<32 | 参数）。
fn encode_fault(fault: crate::interp::Fault) -> u64 {
    match fault {
        crate::interp::Fault::NullPointer => FAULT_NULL << 32,
        crate::interp::Fault::ArrayIndex { index } => (FAULT_ARRAY_INDEX << 32) | index as u64,
        crate::interp::Fault::ClassCast => FAULT_CLASS_CAST << 32,
        crate::interp::Fault::NegativeArraySize { size } => (FAULT_NEG_SIZE << 32) | size as u64,
        crate::interp::Fault::ArrayStore => FAULT_ARRAY_STORE << 32,
        crate::interp::Fault::Arithmetic | crate::interp::Fault::IllegalMonitor => {
            FAULT_ARRAY_STORE << 32
        }
    }
}


/// 助手入口地址（fn 指针 → usize，收窄 lint 噪声面）。
fn helper_field_addr() -> u64 {
    (helper_field as unsafe extern "C" fn(*mut JitCtx, u64) -> u64) as usize as u64
}

fn helper_invoke_addr() -> u64 {
    (helper_invoke as unsafe extern "C" fn(*mut JitCtx, u64) -> u64) as usize as u64
}
