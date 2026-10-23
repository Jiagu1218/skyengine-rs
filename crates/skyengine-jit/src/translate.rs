//! Thumb-1 block translator (P2).
//!
//! Translates a linear guest Thumb basic block into one native x86_64
//! function with the signature
//!
//! ```text
//! unsafe extern "C" fn(*mut BlockContext) -> u32   // always 0 in v1
//! ```
//!
//! The context block is both the register file and the call interface:
//! the context pointer register doubles as the helpers' first argument on
//! both supported ABIs (SysV `Rdi`, Win64 `Rcx`). All guest loads and
//! stores go through host helper calls so memory semantics (permission
//! checks, legacy null-page scratch, the future write barrier) stay in
//! exactly one place.
//!
//! Block lifecycle: a block starts at a guest pc and ends at the first
//! control-transfer instruction (B/Bcc/BX/POP {pc}/MOV or ADD to PC), the
//! first instruction the translator does not support, a fetch failure or a
//! size cap. Unsupported instructions end the block **before** themselves,
//! so the translated prefix stays cached and the interpreter resumes at
//! the unsupported instruction.
//!
//! Fault protocol: before every helper call the emitted code stores the
//! current instruction's pc into `regs[15]`; on failure the helper records
//! the faulting guest address in [`BlockContext::fault`]. The emitted code
//! checks that field right after the call and returns early — the
//! dispatcher syncs the register file and re-executes the faulting
//! instruction on the interpreter, which reproduces the host's
//! fault/retry/error path byte for byte (nothing of the faulting
//! instruction has committed, matching `Cpu::step`'s rewind semantics).

use crate::emitter::{Cc, Reg32, X86Emitter};

/// Host calling convention: register carrying the context pointer (helper
/// argument 1) and registers carrying helper arguments 2 and 3.
#[cfg(target_os = "windows")]
mod abi {
    use crate::emitter::Reg32;
    pub const CTX: Reg32 = Reg32::Ecx;
    pub const ARG2: Reg32 = Reg32::Edx;
    pub const ARG3: Reg32 = Reg32::R8d;
}
#[cfg(not(target_os = "windows"))]
mod abi {
    use crate::emitter::Reg32;
    pub const CTX: Reg32 = Reg32::Edi;
    pub const ARG2: Reg32 = Reg32::Esi;
    pub const ARG3: Reg32 = Reg32::Edx;
}

use abi::{ARG2, ARG3, CTX};

/// Field offsets inside [`BlockContext`] used by emitted machine code.
/// `repr(C)` layout is load-bearing: the test suite asserts every offset.
pub mod ctx_off {
    pub const fn reg(n: usize) -> u32 {
        (n * 4) as u32
    }
    pub const REG_PC: u32 = reg(15);
    pub const READ8: u32 = 64;
    pub const READ16: u32 = 72;
    pub const READ32: u32 = 80;
    pub const WRITE8: u32 = 88;
    pub const WRITE16: u32 = 96;
    pub const WRITE32: u32 = 104;
    pub const FLAG_N: u32 = 112;
    pub const FLAG_Z: u32 = 113;
    pub const FLAG_C: u32 = 114;
    pub const FLAG_V: u32 = 115;
    pub const THUMB: u32 = 116;
    pub const FAULT: u32 = 120;
    /// Host memory object pointer — touched only by helper functions.
    pub const MEM: u32 = 128;
}

/// Guest-memory read helper: returns the value at `addr`; on failure sets
/// `fault` and returns an arbitrary value.
pub type ReadFn = unsafe extern "C" fn(*mut BlockContext, u32) -> u32;

/// Guest-memory write helper: stores `value` at `addr`; on failure sets
/// `fault`.
pub type WriteFn = unsafe extern "C" fn(*mut BlockContext, u32, u32);

/// Register file and call context shared by the dispatcher, emitted blocks
/// and the memory helpers.
#[repr(C)]
pub struct BlockContext {
    /// Guest registers r0-r15. `regs[15]` doubles as the exit pc: blocks
    /// store either the next sequential pc, a branch target or — before
    /// each memory helper call — the current instruction's pc for fault
    /// rewind.
    pub regs: [u32; 16],
    pub read8: ReadFn,
    pub read16: ReadFn,
    pub read32: ReadFn,
    pub write8: WriteFn,
    pub write16: WriteFn,
    pub write32: WriteFn,
    /// N, Z, C, V — each byte is 0 or 1.
    pub flags: [u8; 4],
    /// 1 while executing Thumb code; updated by BX / POP {pc} / writes to
    /// PC so the dispatcher can switch between the JIT and the interpreter.
    pub thumb: u8,
    /// Faulting guest address of the most recent failed helper call; 0
    /// means no fault.
    pub fault: u32,
    /// Host memory object handed to the helpers (never touched by emitted
    /// code).
    pub mem: *mut core::ffi::c_void,
}

impl BlockContext {
    /// Context wired to a host memory object. Flags start clear.
    pub fn new(
        read8: ReadFn,
        read16: ReadFn,
        read32: ReadFn,
        write8: WriteFn,
        write16: WriteFn,
        write32: WriteFn,
        mem: *mut core::ffi::c_void,
    ) -> Self {
        Self {
            regs: [0; 16],
            read8,
            read16,
            read32,
            write8,
            write16,
            write32,
            flags: [0; 4],
            thumb: 1,
            fault: 0,
            mem,
        }
    }
}

/// Result of translating one guest basic block.
#[derive(Debug, Clone)]
pub struct ThumbBlock {
    /// Native x86_64 machine code, ready to publish into the code arena.
    pub code: Vec<u8>,
    /// Guest pc the block was translated from.
    pub guest_start: u32,
    /// Guest pc just past the last translated instruction.
    pub guest_end: u32,
    /// Number of translated guest instructions.
    pub instructions: u32,
}

impl ThumbBlock {
    /// True when the translator produced no code (the first instruction is
    /// unsupported); the caller must interpret one instruction instead.
    pub fn is_empty(&self) -> bool {
        self.instructions == 0
    }
}

/// Maximum guest instructions per block.
const MAX_BLOCK_INSNS: u32 = 32;
/// Stop translating (and exit the block) once the code grows past this.
const MAX_BLOCK_CODE: usize = 3072;

enum InsnOutcome {
    /// Sequential flow; the block may continue at pc + 2.
    Continue,
    /// Control transfer emitted; the block is complete.
    BlockEnd,
    /// Unsupported instruction; end the block before it.
    Bail,
}

/// Translate one Thumb basic block starting at `start_pc`. `fetch` returns
/// the guest halfword at a pc, or `None` when the fetch would fault.
pub fn translate_thumb_block(fetch: &dyn Fn(u32) -> Option<u16>, start_pc: u32) -> ThumbBlock {
    let mut t = ThumbTranslator {
        e: X86Emitter::new(),
    };
    // Reserve the helper-call frame: Win64 shadow space + 16-byte stack
    // alignment (see X86Emitter::sub_rsp_imm8). The context pointer rides
    // a caller-saved register, which every helper call clobbers — spill it
    // to the frame slot so `emit_fault_check` can reload it afterwards.
    t.e.sub_rsp_imm8(40);
    t.e.mov_frame_ptr_store(CTX);
    let mut pc = start_pc;
    let mut instructions: u32 = 0;
    let complete = loop {
        if instructions >= MAX_BLOCK_INSNS || t.e.len() >= MAX_BLOCK_CODE {
            break false;
        }
        let Some(insn) = (fetch)(pc) else {
            break false;
        };
        match t.translate_insn(pc, insn) {
            InsnOutcome::Continue => {
                pc = pc.wrapping_add(2);
                instructions += 1;
            }
            InsnOutcome::BlockEnd => {
                pc = pc.wrapping_add(2);
                instructions += 1;
                break true;
            }
            InsnOutcome::Bail => break false,
        }
    };
    if !complete && instructions == 0 {
        return ThumbBlock {
            code: Vec::new(),
            guest_start: start_pc,
            guest_end: start_pc,
            instructions: 0,
        };
    }
    if !complete {
        t.emit_exit(pc);
    }
    ThumbBlock {
        code: t.e.bytes().to_vec(),
        guest_start: start_pc,
        guest_end: pc,
        instructions,
    }
}

struct ThumbTranslator {
    e: X86Emitter,
}

fn sign_extend(value: u32, bits: u32) -> i32 {
    ((value << (32 - bits)) as i32) >> (32 - bits)
}

impl ThumbTranslator {
    // ------------------------------------------------------------------
    // Emission helpers
    // ------------------------------------------------------------------

    /// Store `next_pc` into regs[15] and return to the dispatcher.
    fn emit_exit(&mut self, next_pc: u32) {
        self.e.mov_mem_imm32(CTX, ctx_off::REG_PC, next_pc);
        self.emit_return();
    }

    /// Return to the dispatcher with regs[15] already set.
    fn emit_return(&mut self) {
        self.e.add_rsp_imm8(40);
        self.e.xor_r32_r32(Reg32::Eax, Reg32::Eax);
        self.e.ret();
    }

    /// Capture N and Z from the flags of the preceding instruction.
    fn emit_nz(&mut self) {
        self.e.setcc_mem(Cc::S, CTX, ctx_off::FLAG_N);
        self.e.setcc_mem(Cc::E, CTX, ctx_off::FLAG_Z);
    }

    /// Capture N, Z, C, V from an add (`sub = false`) or sub/cmp/neg
    /// (`sub = true`). ARM's C is the *not-borrow*: the inverse of x86's
    /// CF for subtractions.
    fn emit_add_sub_flags(&mut self, sub: bool) {
        self.e
            .setcc_mem(if sub { Cc::Ae } else { Cc::B }, CTX, ctx_off::FLAG_C);
        self.e.setcc_mem(Cc::O, CTX, ctx_off::FLAG_V);
        self.emit_nz();
    }

    /// After a helper call: return early when the helper recorded a fault
    /// (regs[15] holds the faulting instruction's pc). The helper clobbers
    /// every caller-saved register, so the context pointer is reloaded from
    /// the frame slot first — the load helper's return value stays in Eax.
    fn emit_fault_check(&mut self) {
        let ok = self.e.new_label();
        self.e.mov_frame_ptr_load(CTX);
        self.e.cmp_mem_imm8(CTX, ctx_off::FAULT, 0);
        self.e.jcc_label(Cc::E, ok);
        self.emit_return();
        self.e.bind(ok);
    }

    /// `set_pc` semantics with the target value in Eax: thumb = bit 0,
    /// pc = thumb ? value & !1 : value & !3.
    fn emit_set_pc_exit(&mut self) {
        self.e.mov_r32_r32(Reg32::Edx, Reg32::Eax);
        self.e.and_r32_imm32(Reg32::Edx, 1);
        self.e.mov_mem8_r8(CTX, ctx_off::THUMB, Reg32::Edx);
        self.e.and_r32_imm32(Reg32::Eax, 0xFFFF_FFFE);
        let thumb_case = self.e.new_label();
        self.e.test_r32_r32(Reg32::Edx, Reg32::Edx);
        self.e.jcc_label(Cc::Ne, thumb_case);
        self.e.and_r32_imm32(Reg32::Eax, 0xFFFF_FFFC);
        self.e.bind(thumb_case);
        self.e.mov_mem_r32(CTX, ctx_off::REG_PC, Reg32::Eax);
        self.emit_return();
    }

    /// Branch to `label` when the guest condition `cond` (0..=14) holds.
    fn emit_cond_jump(&mut self, cond: u8, label: crate::emitter::Label) {
        let e = &mut self.e;
        match cond {
            0x0 => {
                e.cmp_mem8_imm8(CTX, ctx_off::FLAG_Z, 0);
                e.jcc_label(Cc::Ne, label);
            }
            0x1 => {
                e.cmp_mem8_imm8(CTX, ctx_off::FLAG_Z, 0);
                e.jcc_label(Cc::E, label);
            }
            0x2 => {
                e.cmp_mem8_imm8(CTX, ctx_off::FLAG_C, 0);
                e.jcc_label(Cc::Ne, label);
            }
            0x3 => {
                e.cmp_mem8_imm8(CTX, ctx_off::FLAG_C, 0);
                e.jcc_label(Cc::E, label);
            }
            0x4 => {
                e.cmp_mem8_imm8(CTX, ctx_off::FLAG_N, 0);
                e.jcc_label(Cc::Ne, label);
            }
            0x5 => {
                e.cmp_mem8_imm8(CTX, ctx_off::FLAG_N, 0);
                e.jcc_label(Cc::E, label);
            }
            0x6 => {
                e.cmp_mem8_imm8(CTX, ctx_off::FLAG_V, 0);
                e.jcc_label(Cc::Ne, label);
            }
            0x7 => {
                e.cmp_mem8_imm8(CTX, ctx_off::FLAG_V, 0);
                e.jcc_label(Cc::E, label);
            }
            // HI = C && !Z
            0x8 => {
                let skip = e.new_label();
                e.cmp_mem8_imm8(CTX, ctx_off::FLAG_C, 0);
                e.jcc_label(Cc::E, skip);
                e.cmp_mem8_imm8(CTX, ctx_off::FLAG_Z, 0);
                e.jcc_label(Cc::E, label);
                e.bind(skip);
            }
            // LS = !C || Z
            0x9 => {
                e.cmp_mem8_imm8(CTX, ctx_off::FLAG_C, 0);
                e.jcc_label(Cc::E, label);
                e.cmp_mem8_imm8(CTX, ctx_off::FLAG_Z, 0);
                e.jcc_label(Cc::Ne, label);
            }
            // GE = N == V
            0xa => {
                e.mov_r8_mem(Reg32::Eax, CTX, ctx_off::FLAG_N);
                e.cmp_r8_mem(Reg32::Eax, CTX, ctx_off::FLAG_V);
                e.jcc_label(Cc::E, label);
            }
            // LT = N != V
            0xb => {
                e.mov_r8_mem(Reg32::Eax, CTX, ctx_off::FLAG_N);
                e.cmp_r8_mem(Reg32::Eax, CTX, ctx_off::FLAG_V);
                e.jcc_label(Cc::Ne, label);
            }
            // GT = !Z && N == V
            0xc => {
                let skip = e.new_label();
                e.cmp_mem8_imm8(CTX, ctx_off::FLAG_Z, 0);
                e.jcc_label(Cc::Ne, skip);
                e.mov_r8_mem(Reg32::Eax, CTX, ctx_off::FLAG_N);
                e.cmp_r8_mem(Reg32::Eax, CTX, ctx_off::FLAG_V);
                e.jcc_label(Cc::E, label);
                e.bind(skip);
            }
            // LE = Z || N != V
            0xd => {
                e.cmp_mem8_imm8(CTX, ctx_off::FLAG_Z, 0);
                e.jcc_label(Cc::Ne, label);
                e.mov_r8_mem(Reg32::Eax, CTX, ctx_off::FLAG_N);
                e.cmp_r8_mem(Reg32::Eax, CTX, ctx_off::FLAG_V);
                e.jcc_label(Cc::Ne, label);
            }
            // AL
            0xe => e.jmp_label(label),
            _ => unreachable!("SWI is routed to the bail path before decoding"),
        }
    }

    /// Load helper with the guest address already computed in ARG2.
    fn emit_load_addr_in_arg2(&mut self, pc: u32, helper: u32, dst: usize) {
        self.e.mov_mem_imm32(CTX, ctx_off::REG_PC, pc);
        self.e.call_mem(CTX, helper);
        self.emit_fault_check();
        self.e.mov_mem_r32(CTX, ctx_off::reg(dst), Reg32::Eax);
    }

    /// Store helper with the guest address already computed in ARG2.
    fn emit_store_addr_in_arg2(&mut self, pc: u32, helper: u32, src: usize) {
        self.e.mov_r32_mem(Reg32::Eax, CTX, ctx_off::reg(src));
        self.e.mov_r32_r32(ARG3, Reg32::Eax);
        self.e.mov_mem_imm32(CTX, ctx_off::REG_PC, pc);
        self.e.call_mem(CTX, helper);
        self.emit_fault_check();
    }

    /// Load with address `const_addr`, or `[base] + disp` when `const_addr`
    /// is `None`.
    fn emit_load(
        &mut self,
        pc: u32,
        helper: u32,
        const_addr: Option<u32>,
        base: Option<usize>,
        disp: u32,
        dst: usize,
    ) {
        match const_addr {
            Some(addr) => self.e.mov_r32_imm32(ARG2, addr),
            None => {
                self.e.mov_r32_mem(ARG2, CTX, ctx_off::reg(base.unwrap()));
                if disp != 0 {
                    self.e.add_r32_imm32(ARG2, disp);
                }
            }
        }
        self.emit_load_addr_in_arg2(pc, helper, dst);
    }

    /// Store with address `const_addr`, or `[base] + disp` when `const_addr`
    /// is `None`.
    fn emit_store(
        &mut self,
        pc: u32,
        helper: u32,
        const_addr: Option<u32>,
        base: Option<usize>,
        disp: u32,
        src: usize,
    ) {
        match const_addr {
            Some(addr) => self.e.mov_r32_imm32(ARG2, addr),
            None => {
                self.e.mov_r32_mem(ARG2, CTX, ctx_off::reg(base.unwrap()));
                if disp != 0 {
                    self.e.add_r32_imm32(ARG2, disp);
                }
            }
        }
        self.emit_store_addr_in_arg2(pc, helper, src);
    }

    // ------------------------------------------------------------------
    // Instruction translation (mirrors Cpu::step_thumb exactly)
    // ------------------------------------------------------------------

    fn translate_insn(&mut self, pc: u32, insn: u16) -> InsnOutcome {
        match insn & 0xf800 {
            0x0000 | 0x0800 | 0x1000 => self.f1_shift(pc, insn),
            0x1800 => self.f2_add_sub(insn),
            0x2000 | 0x2800 | 0x3000 | 0x3800 => self.f3_immediate(insn),
            0x4800 => self.f_literal_load(pc, insn),
            0x6000 | 0x6800 | 0x7000 | 0x7800 => self.f_imm_transfer(pc, insn),
            0x8000 | 0x8800 => self.f_halfword_transfer(pc, insn),
            0x9000 | 0x9800 => self.f_sp_transfer(pc, insn),
            0xa000 | 0xa800 => self.f10_address(pc, insn),
            // LDM/STM and the BL/BLX prefix pairs stay on the interpreter.
            0xc000 | 0xc800 | 0xe800 | 0xf000 | 0xf800 => InsnOutcome::Bail,
            0xe000 => {
                let offset = sign_extend(u32::from(insn & 0x7ff) << 1, 12);
                let target = pc.wrapping_add(4).wrapping_add_signed(offset) & !1;
                self.emit_exit(target);
                InsnOutcome::BlockEnd
            }
            _ => match insn & 0xfc00 {
                0x4000 => self.f4_alu(insn),
                0x4400 => self.f5_high_register(pc, insn),
                0x5000 | 0x5400 | 0x5800 | 0x5c00 => self.f6_register_transfer(pc, insn),
                _ => match insn & 0xff00 {
                    0xb000 => self.b0_sp_adjust(insn),
                    0xb400 | 0xb500 => self.f14_push(pc, insn),
                    0xbc00 | 0xbd00 => self.f14_pop(pc, insn),
                    0xd000..=0xde00 => self.bcc(pc, insn),
                    // SWI (incl. semihosting), BKPT, hints: interpreter.
                    _ => InsnOutcome::Bail,
                },
            },
        }
    }

    /// F1: LSL/LSR/ASR immediate.
    fn f1_shift(&mut self, _pc: u32, insn: u16) -> InsnOutcome {
        let op = ((insn >> 11) & 3) as u8;
        let amount = ((insn >> 6) & 0x1f) as u8;
        let rs = ((insn >> 3) & 7) as usize;
        let rd = (insn & 7) as usize;
        self.e.mov_r32_mem(Reg32::Eax, CTX, ctx_off::reg(rs));
        match (op, amount) {
            // lsl #0: value unchanged, C unchanged.
            (0, 0) => {
                self.e.test_r32_r32(Reg32::Eax, Reg32::Eax);
                self.emit_nz();
            }
            (0, n) => {
                self.e.shl_r32_imm8(Reg32::Eax, n);
                self.emit_shift_flags();
            }
            // lsr #0: result 0, C = old bit 31.
            (1, 0) => {
                self.e.bt_r32_imm8(Reg32::Eax, 31);
                self.e.setcc_mem(Cc::B, CTX, ctx_off::FLAG_C);
                self.e.xor_r32_r32(Reg32::Eax, Reg32::Eax);
                self.emit_nz();
            }
            (1, n) => {
                self.e.shr_r32_imm8(Reg32::Eax, n);
                self.emit_shift_flags();
            }
            // asr #0: sign fill, C = old bit 31.
            (2, 0) => {
                self.e.bt_r32_imm8(Reg32::Eax, 31);
                self.e.setcc_mem(Cc::B, CTX, ctx_off::FLAG_C);
                self.e.sar_r32_imm8(Reg32::Eax, 31);
                self.emit_nz();
            }
            (2, n) => {
                self.e.sar_r32_imm8(Reg32::Eax, n);
                self.emit_shift_flags();
            }
            _ => unreachable!("F1 encodes only shift types 0..=2"),
        }
        self.e.mov_mem_r32(CTX, ctx_off::reg(rd), Reg32::Eax);
        InsnOutcome::Continue
    }

    fn emit_shift_flags(&mut self) {
        self.e.setcc_mem(Cc::B, CTX, ctx_off::FLAG_C);
        self.emit_nz();
    }

    /// F2: ADD/SUB register or 3-bit immediate.
    fn f2_add_sub(&mut self, insn: u16) -> InsnOutcome {
        let immediate = insn & (1 << 10) != 0;
        let subtract = insn & (1 << 9) != 0;
        let rn = ((insn >> 6) & 7) as usize;
        let rs = ((insn >> 3) & 7) as usize;
        let rd = (insn & 7) as usize;
        self.e.mov_r32_mem(Reg32::Eax, CTX, ctx_off::reg(rs));
        if immediate {
            self.e.mov_r32_imm32(Reg32::Edx, u32::from((insn >> 6) & 7));
        } else {
            self.e.mov_r32_mem(Reg32::Edx, CTX, ctx_off::reg(rn));
        }
        if subtract {
            self.e.sub_r32_r32(Reg32::Eax, Reg32::Edx);
        } else {
            self.e.add_r32_r32(Reg32::Eax, Reg32::Edx);
        }
        self.emit_add_sub_flags(subtract);
        self.e.mov_mem_r32(CTX, ctx_off::reg(rd), Reg32::Eax);
        InsnOutcome::Continue
    }

    /// F3: MOV/CMP/ADD/SUB 8-bit immediate.
    fn f3_immediate(&mut self, insn: u16) -> InsnOutcome {
        let op = ((insn >> 11) & 3) as u8;
        let rd = ((insn >> 8) & 7) as usize;
        let imm = u32::from(insn & 0xff);
        match op {
            // MOV: N/Z from the immediate, C/V unchanged.
            0 => {
                self.e.mov_r32_imm32(Reg32::Eax, imm);
                self.e.test_r32_r32(Reg32::Eax, Reg32::Eax);
                self.emit_nz();
                self.e.mov_mem_r32(CTX, ctx_off::reg(rd), Reg32::Eax);
            }
            // CMP
            1 => {
                self.e.mov_r32_mem(Reg32::Eax, CTX, ctx_off::reg(rd));
                self.e.cmp_r32_imm32(Reg32::Eax, imm);
                self.emit_add_sub_flags(true);
            }
            // ADD
            2 => {
                self.e.mov_r32_mem(Reg32::Eax, CTX, ctx_off::reg(rd));
                self.e.add_r32_imm32(Reg32::Eax, imm);
                self.emit_add_sub_flags(false);
                self.e.mov_mem_r32(CTX, ctx_off::reg(rd), Reg32::Eax);
            }
            // SUB
            3 => {
                self.e.mov_r32_mem(Reg32::Eax, CTX, ctx_off::reg(rd));
                self.e.sub_r32_imm32(Reg32::Eax, imm);
                self.emit_add_sub_flags(true);
                self.e.mov_mem_r32(CTX, ctx_off::reg(rd), Reg32::Eax);
            }
            _ => unreachable!(),
        }
        InsnOutcome::Continue
    }

    /// F4 (0x48xx): LDR literal.
    fn f_literal_load(&mut self, pc: u32, insn: u16) -> InsnOutcome {
        let rd = ((insn >> 8) & 7) as usize;
        let addr = (pc.wrapping_add(4) & !3).wrapping_add(u32::from(insn & 0xff) * 4);
        self.emit_load(pc, ctx_off::READ32, Some(addr), None, 0, rd);
        InsnOutcome::Continue
    }

    /// F7: LDR/STR word or byte, 5-bit immediate offset.
    fn f_imm_transfer(&mut self, pc: u32, insn: u16) -> InsnOutcome {
        let byte = insn & (1 << 12) != 0;
        let load = insn & (1 << 11) != 0;
        let imm5 = u32::from((insn >> 6) & 0x1f);
        let rn = ((insn >> 3) & 7) as usize;
        let rd = (insn & 7) as usize;
        let disp = if byte { imm5 } else { imm5 * 4 };
        if load {
            let helper = if byte {
                ctx_off::READ8
            } else {
                ctx_off::READ32
            };
            self.emit_load(pc, helper, None, Some(rn), disp, rd);
        } else {
            let helper = if byte {
                ctx_off::WRITE8
            } else {
                ctx_off::WRITE32
            };
            self.emit_store(pc, helper, None, Some(rn), disp, rd);
        }
        InsnOutcome::Continue
    }

    /// F8: LDRH/STRH, 5-bit immediate offset (scaled by 2).
    fn f_halfword_transfer(&mut self, pc: u32, insn: u16) -> InsnOutcome {
        let load = insn & (1 << 11) != 0;
        let disp = u32::from((insn >> 6) & 0x1f) * 2;
        let rn = ((insn >> 3) & 7) as usize;
        let rd = (insn & 7) as usize;
        if load {
            self.emit_load(pc, ctx_off::READ16, None, Some(rn), disp, rd);
        } else {
            self.emit_store(pc, ctx_off::WRITE16, None, Some(rn), disp, rd);
        }
        InsnOutcome::Continue
    }

    /// F9: LDR/STR [sp, imm8*4].
    fn f_sp_transfer(&mut self, pc: u32, insn: u16) -> InsnOutcome {
        let load = insn & (1 << 11) != 0;
        let rd = ((insn >> 8) & 7) as usize;
        let disp = u32::from(insn & 0xff) * 4;
        if load {
            self.emit_load(pc, ctx_off::READ32, None, Some(13), disp, rd);
        } else {
            self.emit_store(pc, ctx_off::WRITE32, None, Some(13), disp, rd);
        }
        InsnOutcome::Continue
    }

    /// F10: ADD rd, pc/sp, imm8*4 (ADR). No flags.
    fn f10_address(&mut self, pc: u32, insn: u16) -> InsnOutcome {
        let rd = ((insn >> 8) & 7) as usize;
        let imm = u32::from(insn & 0xff) * 4;
        if insn & (1 << 11) != 0 {
            // ADD rd, sp, imm
            self.e.mov_r32_mem(Reg32::Eax, CTX, ctx_off::reg(13));
            self.e.add_r32_imm32(Reg32::Eax, imm);
            self.e.mov_mem_r32(CTX, ctx_off::reg(rd), Reg32::Eax);
        } else {
            // ADR: (pc + 4) & !3 + imm is a translate-time constant.
            let value = (pc.wrapping_add(4) & !3).wrapping_add(imm);
            self.e.mov_mem_imm32(CTX, ctx_off::reg(rd), value);
        }
        InsnOutcome::Continue
    }

    /// F4 (0x40xx): data-processing register operations. Register shifts,
    /// ADC and SBC stay on the interpreter.
    fn f4_alu(&mut self, insn: u16) -> InsnOutcome {
        let op = ((insn >> 6) & 0xf) as u8;
        let rs = ((insn >> 3) & 7) as usize;
        let rd = (insn & 7) as usize;
        if matches!(op, 2..=7) {
            return InsnOutcome::Bail;
        }
        self.e.mov_r32_mem(Reg32::Eax, CTX, ctx_off::reg(rd));
        self.e.mov_r32_mem(Reg32::Edx, CTX, ctx_off::reg(rs));
        let store = match op {
            // AND
            0 => {
                self.e.and_r32_r32(Reg32::Eax, Reg32::Edx);
                self.emit_nz();
                true
            }
            // EOR
            1 => {
                self.e.xor_r32_r32(Reg32::Eax, Reg32::Edx);
                self.emit_nz();
                true
            }
            // TST
            8 => {
                self.e.test_r32_r32(Reg32::Eax, Reg32::Edx);
                self.emit_nz();
                false
            }
            // NEG: result = 0 - rs; negate the SOURCE, not rd. ARM C =
            // (rs == 0) = !x86 CF.
            9 => {
                self.e.mov_r32_r32(Reg32::Eax, Reg32::Edx);
                self.e.neg_r32(Reg32::Eax);
                self.emit_add_sub_flags(true);
                true
            }
            // CMP
            10 => {
                self.e.sub_r32_r32(Reg32::Eax, Reg32::Edx);
                self.emit_add_sub_flags(true);
                false
            }
            // CMN
            11 => {
                self.e.add_r32_r32(Reg32::Eax, Reg32::Edx);
                self.emit_add_sub_flags(false);
                false
            }
            // ORR
            12 => {
                self.e.or_r32_r32(Reg32::Eax, Reg32::Edx);
                self.emit_nz();
                true
            }
            // MUL: imul leaves SF/ZF undefined; re-test for N/Z, C/V stay.
            13 => {
                self.e.imul_r32_r32(Reg32::Eax, Reg32::Edx);
                self.e.test_r32_r32(Reg32::Eax, Reg32::Eax);
                self.emit_nz();
                true
            }
            // BIC
            14 => {
                self.e.not_r32(Reg32::Edx);
                self.e.and_r32_r32(Reg32::Eax, Reg32::Edx);
                self.emit_nz();
                true
            }
            // MVN: result = NOT(rs).
            15 => {
                self.e.mov_r32_r32(Reg32::Eax, Reg32::Edx);
                self.e.not_r32(Reg32::Eax);
                self.e.test_r32_r32(Reg32::Eax, Reg32::Eax);
                self.emit_nz();
                true
            }
            _ => unreachable!("2..=7 handled above"),
        };
        if store {
            self.e.mov_mem_r32(CTX, ctx_off::reg(rd), Reg32::Eax);
        }
        InsnOutcome::Continue
    }

    /// F5: ADD/CMP/MOV high-register and BX/BLX.
    fn f5_high_register(&mut self, pc: u32, insn: u16) -> InsnOutcome {
        let op = ((insn >> 8) & 3) as u8;
        let insn32 = u32::from(insn);
        let hs = (((insn32 >> 3) & 7) | ((insn32 >> 6) & 1) << 3) as usize;
        let hd = ((insn32 & 7) | ((insn32 >> 7) & 1) << 3) as usize;
        // Architectural pc reads are address + 4.
        if hs == 15 {
            self.e.mov_r32_imm32(Reg32::Edx, pc.wrapping_add(4));
        } else {
            self.e.mov_r32_mem(Reg32::Edx, CTX, ctx_off::reg(hs));
        }
        match op {
            // ADD: no flags; a result written to PC stays Thumb (matching
            // write_thumb_register).
            0 => {
                if hd == 15 {
                    self.e.mov_r32_imm32(Reg32::Eax, pc.wrapping_add(4));
                    self.e.add_r32_r32(Reg32::Eax, Reg32::Edx);
                    self.e.and_r32_imm32(Reg32::Eax, 0xFFFF_FFFE);
                    self.e.mov_mem_r32(CTX, ctx_off::REG_PC, Reg32::Eax);
                    self.e.mov_mem8_imm8(CTX, ctx_off::THUMB, 1);
                    self.emit_return();
                    InsnOutcome::BlockEnd
                } else {
                    self.e.mov_r32_mem(Reg32::Eax, CTX, ctx_off::reg(hd));
                    self.e.add_r32_r32(Reg32::Eax, Reg32::Edx);
                    self.e.mov_mem_r32(CTX, ctx_off::reg(hd), Reg32::Eax);
                    InsnOutcome::Continue
                }
            }
            // CMP: flags only.
            1 => {
                if hd == 15 {
                    self.e.mov_r32_imm32(Reg32::Eax, pc.wrapping_add(4));
                } else {
                    self.e.mov_r32_mem(Reg32::Eax, CTX, ctx_off::reg(hd));
                }
                self.e.cmp_r32_r32(Reg32::Eax, Reg32::Edx);
                self.emit_add_sub_flags(true);
                InsnOutcome::Continue
            }
            // MOV: a result written to PC stays Thumb.
            2 => {
                if hd == 15 {
                    self.e.and_r32_imm32(Reg32::Edx, 0xFFFF_FFFE);
                    self.e.mov_mem_r32(CTX, ctx_off::REG_PC, Reg32::Edx);
                    self.e.mov_mem8_imm8(CTX, ctx_off::THUMB, 1);
                    self.emit_return();
                    InsnOutcome::BlockEnd
                } else {
                    self.e.mov_mem_r32(CTX, ctx_off::reg(hd), Reg32::Edx);
                    InsnOutcome::Continue
                }
            }
            // BX / BLX (bit 7 set): LR = next insn | 1, then set_pc(target).
            3 => {
                if insn & (1 << 7) != 0 {
                    self.e.mov_mem_imm32(CTX, ctx_off::reg(14), (pc + 2) | 1);
                }
                self.e.mov_r32_r32(Reg32::Eax, Reg32::Edx);
                self.emit_set_pc_exit();
                InsnOutcome::BlockEnd
            }
            _ => unreachable!(),
        }
    }

    /// F6: register-offset loads/stores. LDRSB/LDRSH stay on the
    /// interpreter.
    fn f6_register_transfer(&mut self, pc: u32, insn: u16) -> InsnOutcome {
        let op = ((insn >> 9) & 7) as u8;
        if op == 3 || op == 7 {
            return InsnOutcome::Bail;
        }
        let ro = ((insn >> 6) & 7) as usize;
        let rb = ((insn >> 3) & 7) as usize;
        let rd = (insn & 7) as usize;
        self.e.mov_r32_mem(ARG2, CTX, ctx_off::reg(rb));
        self.e.mov_r32_mem(Reg32::Eax, CTX, ctx_off::reg(ro));
        self.e.add_r32_r32(ARG2, Reg32::Eax);
        match op {
            0 => self.emit_store_addr_in_arg2(pc, ctx_off::WRITE32, rd),
            1 => self.emit_store_addr_in_arg2(pc, ctx_off::WRITE16, rd),
            2 => self.emit_store_addr_in_arg2(pc, ctx_off::WRITE8, rd),
            4 => self.emit_load_addr_in_arg2(pc, ctx_off::READ32, rd),
            5 => self.emit_load_addr_in_arg2(pc, ctx_off::READ16, rd),
            6 => self.emit_load_addr_in_arg2(pc, ctx_off::READ8, rd),
            _ => unreachable!("3/7 handled above"),
        }
        InsnOutcome::Continue
    }

    /// ADD/SUB sp, imm8*4. No flags.
    fn b0_sp_adjust(&mut self, insn: u16) -> InsnOutcome {
        let offset = u32::from(insn & 0x7f) * 4;
        self.e.mov_r32_mem(Reg32::Eax, CTX, ctx_off::reg(13));
        if insn & (1 << 7) != 0 {
            self.e.sub_r32_imm32(Reg32::Eax, offset);
        } else {
            self.e.add_r32_imm32(Reg32::Eax, offset);
        }
        self.e.mov_mem_r32(CTX, ctx_off::reg(13), Reg32::Eax);
        InsnOutcome::Continue
    }

    /// F14 push. SP is updated only after every store succeeds, matching
    /// the interpreter's fault-visible ordering. Store addresses are
    /// recomputed from the original SP per element (`sp - count*4 + k*4`)
    /// so no host register carries state across helper calls — the block
    /// must never touch callee-saved registers.
    fn f14_push(&mut self, pc: u32, insn: u16) -> InsnOutcome {
        let extra = insn & (1 << 8) != 0;
        let list = insn & 0xff;
        let count = list.count_ones() + u32::from(extra);
        if count == 0 {
            return InsnOutcome::Bail;
        }
        let base = (count * 4).wrapping_neg();
        let mut k = 0u32;
        for r in 0..8 {
            if list & (1 << r) == 0 {
                continue;
            }
            self.emit_store_at_sp_slot(pc, base.wrapping_add(k * 4), r);
            k += 1;
        }
        if extra {
            self.emit_store_at_sp_slot(pc, base.wrapping_add(k * 4), 14);
        }
        // Commit SP only after every store succeeded.
        self.e.mov_r32_mem(Reg32::Eax, CTX, ctx_off::reg(13));
        self.e.sub_r32_imm32(Reg32::Eax, count * 4);
        self.e.mov_mem_r32(CTX, ctx_off::reg(13), Reg32::Eax);
        InsnOutcome::Continue
    }

    /// F14 pop. POP {pc} ends the block with full set_pc semantics. SP is
    /// committed only after every load succeeded; registers popped before
    /// a faulting load stay written, exactly like the interpreter.
    fn f14_pop(&mut self, pc: u32, insn: u16) -> InsnOutcome {
        let extra = insn & (1 << 8) != 0;
        let list = insn & 0xff;
        let count = list.count_ones() + u32::from(extra);
        if count == 0 {
            return InsnOutcome::Bail;
        }
        let mut k = 0u32;
        for r in 0..8 {
            if list & (1 << r) == 0 {
                continue;
            }
            self.emit_load_at_sp_slot(pc, k * 4, r);
            k += 1;
        }
        if extra {
            // The loaded pc value rides in Edx across the SP commit.
            self.emit_load_at_sp_slot_raw(pc, k * 4);
            self.e.mov_r32_r32(Reg32::Edx, Reg32::Eax);
        }
        self.e.mov_r32_mem(Reg32::Eax, CTX, ctx_off::reg(13));
        self.e.add_r32_imm32(Reg32::Eax, count * 4);
        self.e.mov_mem_r32(CTX, ctx_off::reg(13), Reg32::Eax);
        if extra {
            self.e.mov_r32_r32(Reg32::Eax, Reg32::Edx);
            self.emit_set_pc_exit();
            InsnOutcome::BlockEnd
        } else {
            InsnOutcome::Continue
        }
    }

    /// Store `regs[src]` at `sp + slot_disp` (helper call, fault-checked).
    fn emit_store_at_sp_slot(&mut self, pc: u32, slot_disp: u32, src: usize) {
        self.e.mov_r32_mem(ARG2, CTX, ctx_off::reg(13));
        self.e.add_r32_imm32(ARG2, slot_disp);
        self.e.mov_r32_mem(Reg32::Eax, CTX, ctx_off::reg(src));
        self.e.mov_r32_r32(ARG3, Reg32::Eax);
        self.e.mov_mem_imm32(CTX, ctx_off::REG_PC, pc);
        self.e.call_mem(CTX, ctx_off::WRITE32);
        self.emit_fault_check();
    }

    /// Load `regs[dst]` from `sp + slot_disp` (helper call, fault-checked).
    fn emit_load_at_sp_slot(&mut self, pc: u32, slot_disp: u32, dst: usize) {
        self.emit_load_at_sp_slot_raw(pc, slot_disp);
        self.e.mov_mem_r32(CTX, ctx_off::reg(dst), Reg32::Eax);
    }

    /// Load into Eax from `sp + slot_disp` without writing it back to the
    /// register file (used by POP {pc}).
    fn emit_load_at_sp_slot_raw(&mut self, pc: u32, slot_disp: u32) {
        self.e.mov_r32_mem(ARG2, CTX, ctx_off::reg(13));
        if slot_disp != 0 {
            self.e.add_r32_imm32(ARG2, slot_disp);
        }
        self.e.mov_mem_imm32(CTX, ctx_off::REG_PC, pc);
        self.e.call_mem(CTX, ctx_off::READ32);
        self.emit_fault_check();
    }

    /// Conditional branch; the block ends on both paths.
    fn bcc(&mut self, pc: u32, insn: u16) -> InsnOutcome {
        let cond = ((insn >> 8) & 0xf) as u8;
        let offset = sign_extend(u32::from(insn & 0xff) << 1, 9);
        let target = pc.wrapping_add(4).wrapping_add_signed(offset) & !1;
        let taken = self.e.new_label();
        self.emit_cond_jump(cond, taken);
        self.emit_exit(pc.wrapping_add(2));
        self.e.bind(taken);
        self.emit_exit(target);
        InsnOutcome::BlockEnd
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::ExecMemory;
    use crate::probe::probe_exec_memory;
    use core::mem;
    use std::collections::HashMap;

    // ------------------------------------------------------------------
    // Layout guarantees
    // ------------------------------------------------------------------

    #[test]
    fn ctx_offsets_match_repr_c_layout() {
        assert_eq!(mem::offset_of!(BlockContext, regs), 0);
        assert_eq!(mem::offset_of!(BlockContext, read8) as u32, ctx_off::READ8);
        assert_eq!(
            mem::offset_of!(BlockContext, read16) as u32,
            ctx_off::READ16
        );
        assert_eq!(
            mem::offset_of!(BlockContext, read32) as u32,
            ctx_off::READ32
        );
        assert_eq!(
            mem::offset_of!(BlockContext, write8) as u32,
            ctx_off::WRITE8
        );
        assert_eq!(
            mem::offset_of!(BlockContext, write16) as u32,
            ctx_off::WRITE16
        );
        assert_eq!(
            mem::offset_of!(BlockContext, write32) as u32,
            ctx_off::WRITE32
        );
        assert_eq!(mem::offset_of!(BlockContext, flags) as u32, ctx_off::FLAG_N);
        assert_eq!(mem::offset_of!(BlockContext, thumb) as u32, ctx_off::THUMB);
        assert_eq!(mem::offset_of!(BlockContext, fault) as u32, ctx_off::FAULT);
        assert_eq!(mem::offset_of!(BlockContext, mem) as u32, ctx_off::MEM);
        for n in 0..16 {
            assert_eq!(
                mem::offset_of!(BlockContext, regs) as u32 + ctx_off::reg(n),
                ctx_off::reg(n)
            );
        }
        assert_eq!(ctx_off::reg(15), ctx_off::REG_PC);
    }

    // ------------------------------------------------------------------
    // Mock guest memory and helper functions
    // ------------------------------------------------------------------

    const MOCK_BASE: u32 = 0x1000;
    const MOCK_LEN: u32 = 0x8000;

    struct MockMem {
        data: Vec<u8>,
    }

    impl MockMem {
        fn new() -> Self {
            Self {
                data: vec![0; MOCK_LEN as usize],
            }
        }

        fn offset(&self, addr: u32, len: u32) -> Option<usize> {
            let start = addr.checked_sub(MOCK_BASE)?;
            let end = start.checked_add(len)?;
            (end <= MOCK_LEN).then_some(start as usize)
        }
    }

    unsafe extern "C" fn mock_read8(ctx: *mut BlockContext, addr: u32) -> u32 {
        let ctx = unsafe { &mut *ctx };
        let mem = unsafe { &*(ctx.mem as *const MockMem) };
        match mem.offset(addr, 1) {
            Some(o) => u32::from(mem.data[o]),
            None => {
                ctx.fault = addr;
                0
            }
        }
    }

    unsafe extern "C" fn mock_read16(ctx: *mut BlockContext, addr: u32) -> u32 {
        let ctx = unsafe { &mut *ctx };
        let mem = unsafe { &*(ctx.mem as *const MockMem) };
        match mem.offset(addr, 2) {
            Some(o) => u32::from(u16::from_le_bytes([mem.data[o], mem.data[o + 1]])),
            None => {
                ctx.fault = addr;
                0
            }
        }
    }

    unsafe extern "C" fn mock_read32(ctx: *mut BlockContext, addr: u32) -> u32 {
        let ctx = unsafe { &mut *ctx };
        let mem = unsafe { &*(ctx.mem as *const MockMem) };
        match mem.offset(addr, 4) {
            Some(o) => u32::from_le_bytes(mem.data[o..o + 4].try_into().unwrap()),
            None => {
                ctx.fault = addr;
                0
            }
        }
    }

    unsafe extern "C" fn mock_write8(ctx: *mut BlockContext, addr: u32, value: u32) {
        let ctx = unsafe { &mut *ctx };
        let mem = unsafe { &mut *(ctx.mem as *mut MockMem) };
        match mem.offset(addr, 1) {
            Some(o) => mem.data[o] = value as u8,
            None => ctx.fault = addr,
        }
    }

    unsafe extern "C" fn mock_write16(ctx: *mut BlockContext, addr: u32, value: u32) {
        let ctx = unsafe { &mut *ctx };
        let mem = unsafe { &mut *(ctx.mem as *mut MockMem) };
        match mem.offset(addr, 2) {
            Some(o) => {
                mem.data[o..o + 2].copy_from_slice(&(value as u16).to_le_bytes());
            }
            None => ctx.fault = addr,
        }
    }

    unsafe extern "C" fn mock_write32(ctx: *mut BlockContext, addr: u32, value: u32) {
        let ctx = unsafe { &mut *ctx };
        let mem = unsafe { &mut *(ctx.mem as *mut MockMem) };
        match mem.offset(addr, 4) {
            Some(o) => mem.data[o..o + 4].copy_from_slice(&value.to_le_bytes()),
            None => ctx.fault = addr,
        }
    }

    // ------------------------------------------------------------------
    // Test harness: translate, publish, run, follow exits
    // ------------------------------------------------------------------

    /// A tiny dispatcher: translates blocks on demand, follows regs[15]
    /// exits until the pc leaves the guest code window, a fault occurs or
    /// a step budget is exhausted.
    struct Runner {
        code: Vec<u8>,
        arena: ExecMemory,
        cache: HashMap<u32, (crate::exec::CodePtr, u32)>, // pc -> (code ptr, insn count)
    }

    impl Runner {
        fn new(code_words: &[u16]) -> Self {
            let mut code = Vec::new();
            for w in code_words {
                code.extend_from_slice(&w.to_le_bytes());
            }
            let report = probe_exec_memory();
            let arena = ExecMemory::new(256 * 1024, report.backend).expect("arena");
            Self {
                code,
                arena,
                cache: HashMap::new(),
            }
        }

        fn fetch(&self, pc: u32) -> Option<u16> {
            let off = pc.checked_sub(MOCK_BASE)?;
            let off = off as usize;
            if off + 2 <= self.code.len() {
                Some(u16::from_le_bytes([self.code[off], self.code[off + 1]]))
            } else {
                None
            }
        }

        /// Run until pc leaves [MOCK_BASE, MOCK_BASE+len(code)) or fault.
        /// Returns the exit pc.
        fn run(&mut self, ctx: &mut BlockContext, max_blocks: usize) -> u32 {
            let mut steps = 0;
            loop {
                let pc = ctx.regs[15];
                let code_len = self.code.len() as u32;
                if ctx.fault != 0 || ctx.thumb == 0 || pc < MOCK_BASE || pc >= MOCK_BASE + code_len
                {
                    return pc;
                }
                steps += 1;
                assert!(steps <= max_blocks, "block budget exhausted");
                let entry = match self.cache.get(&pc) {
                    Some((ptr, _)) => *ptr,
                    None => {
                        let block = translate_thumb_block(&|p| self.fetch(p), pc);
                        if block.is_empty() {
                            // Unsupported first instruction: emulate the
                            // dispatcher's fallback by failing loudly (the
                            // real dispatcher interprets it).
                            panic!("empty block at {pc:#010x}");
                        }
                        let ptr = self.arena.publish(&block.code).expect("publish");
                        self.cache.insert(pc, (ptr, block.instructions));
                        ptr
                    }
                };
                let f: unsafe extern "C" fn(*mut BlockContext) -> u32 =
                    unsafe { mem::transmute(self.arena.entry(entry)) };
                unsafe { f(ctx) };
            }
        }
    }

    fn make_ctx(mem: *mut MockMem) -> BlockContext {
        BlockContext::new(
            mock_read8,
            mock_read16,
            mock_read32,
            mock_write8,
            mock_write16,
            mock_write32,
            mem.cast(),
        )
    }

    fn setup(code_words: &[u16]) -> (Box<MockMem>, Box<BlockContext>, Runner) {
        let mut mem = Box::new(MockMem::new());
        let ctx = Box::new(make_ctx(&mut *mem));
        (mem, ctx, Runner::new(code_words))
    }

    // ------------------------------------------------------------------
    // Tests
    // ------------------------------------------------------------------

    #[test]
    fn alu_loop_with_bne() {
        // mov r0, #10 ; mov r1, #0 ; loop: add r1, #3 ; sub r0, #1 ; bne loop
        let (_mem, mut ctx, mut runner) = setup(&[0x200A, 0x2100, 0x3103, 0x3801, 0xD1FC]);
        let code_end = MOCK_BASE + 10;
        ctx.regs[15] = MOCK_BASE;
        let exit = runner.run(&mut ctx, 100);
        assert_eq!(exit, code_end);
        assert_eq!(ctx.regs[0], 0);
        assert_eq!(ctx.regs[1], 30);
        assert_eq!(ctx.fault, 0);
        // Last flag setter is the final `sub r0,#1` with r0 = 1-1 = 0.
        assert_eq!(ctx.flags[1], 1, "Z set by the final subtraction");
    }

    #[test]
    fn shifts_mirror_edge_cases() {
        // #0 shifts are register moves with special carry semantics:
        //   lsr r3, r2, #0 -> 0, C = bit31 of r2
        //   asr r4, r2, #0 -> sign fill, C = bit31 of r2
        //   lsl r1, r2, #0 -> copy, C *preserved*
        // The lsl runs last so C preservation is observable in the final
        // flags: asr set C = bit31 = 1 and lsl #0 must not clear it.
        let (_mem, mut ctx, mut runner) = setup(&[0x0813, 0x1014, 0x0011]);
        ctx.regs[15] = MOCK_BASE;
        ctx.regs[2] = 0x8000_0040;
        ctx.flags[2] = 0; // C = 0: lsr #0 must set it from bit31
        ctx.flags[3] = 0; // V = 0: shifts never touch V
        let exit = runner.run(&mut ctx, 10);
        assert_eq!(exit, MOCK_BASE + 6);
        assert_eq!(ctx.regs[3], 0, "lsr #0 zeroes the operand");
        assert_eq!(ctx.regs[4], 0xFFFF_FFFF, "asr #0 sign-fills");
        assert_eq!(ctx.regs[1], 0x8000_0040, "lsl #0 copies the operand");
        assert_eq!(ctx.flags[0], 1, "N from the lsl copy of 0x80000040");
        assert_eq!(ctx.flags[1], 0, "Z clear: final result is nonzero");
        assert_eq!(ctx.flags[2], 1, "C set by asr, preserved by lsl #0");
        assert_eq!(ctx.flags[3], 0, "V untouched by shifts");
    }

    #[test]
    fn memory_ops_roundtrip() {
        // mov r0, #0x42
        // mov r1, #0x10 ; lsl r1, r1, #8 -> r1 = MOCK_BASE
        // str r0, [r1, #4]   ; word
        // strb r0, [r1, #1]  ; byte
        // strh r0, [r1, #12] ; halfword
        // ldr r2, [r1, #4]   ; reads back 0x42
        // ldrb r3, [r1, #1]
        // ldrh r4, [r1, #12]
        // str r0, [r1, r2]   ; register offset (r2 = 0x42)
        // ldr r6, [r1, r2]   ; reads the reg-offset store back
        // The stores overlap the guest code window, but the block is
        // translated before it executes, so the overwritten bytes are
        // never fetched again.
        let (mem, mut ctx, mut runner) = setup(&[
            0x2042, // mov r0, #0x42
            0x2110, // mov r1, #0x10
            0x0209, // lsl r1, r1, #8 -> r1 = 0x1000 = MOCK_BASE
            0x6048, // str r0, [r1, #4]
            0x7048, // strb r0, [r1, #1]
            0x8188, // strh r0, [r1, #12]
            0x684A, // ldr r2, [r1, #4]
            0x784B, // ldrb r3, [r1, #1]
            0x898C, // ldrh r4, [r1, #12]
            0x5088, // str r0, [r1, r2]
            0x588E, // ldr r6, [r1, r2]
        ]);
        ctx.regs[15] = MOCK_BASE;
        let exit = runner.run(&mut ctx, 10);
        assert_eq!(exit, MOCK_BASE + 22);
        assert_eq!(ctx.fault, 0);
        assert_eq!(ctx.regs[2], 0x42);
        assert_eq!(ctx.regs[3], 0x42);
        assert_eq!(ctx.regs[4], 0x42);
        assert_eq!(ctx.regs[6], 0x42);
        // Word store at r1+4.
        assert_eq!(&mem.data[4..8], &[0x42, 0, 0, 0]);
        // Byte store at r1+1.
        assert_eq!(mem.data[1], 0x42);
        // Halfword store at r1+12.
        assert_eq!(&mem.data[12..14], &[0x42, 0]);
        // Register-offset word store at r1 + r2 = MOCK_BASE+0x42.
        assert_eq!(&mem.data[0x42..0x46], &[0x42, 0, 0, 0]);
    }

    #[test]
    fn literal_load_and_adr() {
        // +0:  adr r0, #0       -> r0 = Align(pc+4) = MOCK_BASE+4
        // +2:  ldr r1, [pc, #4] -> loads the word at MOCK_BASE+8
        // +4:  b +8             -> skips the pool, lands at +12
        // +6:  padding (pool alignment)
        // +8:  literal pool word 0x1234_5678
        // +12: b +72            -> leaves the code window at MOCK_BASE+0x48
        let (mut mem, mut ctx, mut runner) = setup(&[
            0xA000, // adr r0, #0
            0x4901, // ldr r1, [pc, #4]
            0xE002, // b (from +4: target = +4+4+(2<<1) = +12)
            0x0000, // alignment padding, never executed
            0x5678, // literal pool, low half
            0x1234, // literal pool, high half
            0xE01C, // b (from +12: target = +12+4+(28<<1) = +72)
        ]);
        // The literal pool lives at guest MOCK_BASE+8; seed it into the
        // mock data memory the load helpers read from.
        mem.data[8..12].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        ctx.regs[15] = MOCK_BASE;
        let exit = runner.run(&mut ctx, 10);
        assert_eq!(exit, MOCK_BASE + 0x48);
        assert_eq!(ctx.fault, 0);
        assert_eq!(ctx.regs[0], MOCK_BASE + 4, "adr reads the aligned pc");
        assert_eq!(ctx.regs[1], 0x1234_5678, "literal pool load");
    }

    #[test]
    fn adr_reads_aligned_pc() {
        // adr r0, #4 -> (0x1000+4)&!3 + 4 = 0x1008
        let (_mem, mut ctx, mut runner) = setup(&[0xA001]);
        ctx.regs[15] = MOCK_BASE;
        let exit = runner.run(&mut ctx, 5);
        assert_eq!(exit, MOCK_BASE + 2);
        assert_eq!(ctx.regs[0], MOCK_BASE + 8);
    }

    #[test]
    fn push_pop_roundtrip_and_pc_exit() {
        // push {r0, r1, lr} ; pop {r2, r3, pc}
        // With lr = target|1 (thumb), pop must exit at target with thumb=1.
        let (mem, mut ctx, mut runner) = setup(&[
            0xB503, // push {r0, r1, lr}
            0xBD0C, // pop {r2, r3, pc}
        ]);
        let target = MOCK_BASE + 0x100;
        ctx.regs[15] = MOCK_BASE;
        ctx.regs[0] = 0xAAAA_0001;
        ctx.regs[1] = 0xBBBB_0002;
        ctx.regs[13] = MOCK_BASE + 0x2000;
        ctx.regs[14] = target | 1;
        let exit = runner.run(&mut ctx, 10);
        assert_eq!(exit, target);
        assert_eq!(ctx.thumb, 1);
        assert_eq!(ctx.regs[2], 0xAAAA_0001);
        assert_eq!(ctx.regs[3], 0xBBBB_0002);
        // sp restored
        assert_eq!(ctx.regs[13], MOCK_BASE + 0x2000);
        // stack memory untouched below the round trip window
        let sp = (MOCK_BASE + 0x2000 - 12) as usize - MOCK_BASE as usize;
        assert_eq!(&mem.data[sp..sp + 4], &0xAAAA_0001u32.to_le_bytes());
    }

    #[test]
    fn bx_switches_to_arm_mode() {
        // bx r4 with even target -> thumb = 0, pc = target & !3
        let (_mem, mut ctx, mut runner) = setup(&[0x4720]); // bx r4
        ctx.regs[15] = MOCK_BASE;
        ctx.regs[4] = 0x2000_0006; // bit0 = 0 -> ARM; bit1 set so & !3 matters
        let exit = runner.run(&mut ctx, 5);
        assert_eq!(ctx.thumb, 0);
        assert_eq!(ctx.regs[15], 0x2000_0004);
        assert_eq!(exit, 0x2000_0004);
    }

    #[test]
    fn blx_hi_form_sets_lr_and_branches() {
        // 0x4480... blx r0: encoding 0100 0111 1 Rm(000) 000 -> 0x4780
        let (_mem, mut ctx, mut runner) = setup(&[0x4780]); // blx r0
        ctx.regs[15] = MOCK_BASE;
        ctx.regs[0] = MOCK_BASE + 0x40;
        let exit = runner.run(&mut ctx, 5);
        assert_eq!(exit, MOCK_BASE + 0x40);
        assert_eq!(ctx.regs[14], (MOCK_BASE + 2) | 1);
        assert_eq!(ctx.thumb, 0); // even target
        assert_eq!(ctx.regs[15] & !3, MOCK_BASE + 0x40);
    }

    #[test]
    fn bcc_composite_conditions() {
        // bhi +0x10 (halfword offset -> byte target +0x20 from pc+4)
        // fallthrough exit at +2 ; taken target at +0x24
        let (_mem, mut ctx, mut runner) = setup(&[0xD810]);
        ctx.regs[15] = MOCK_BASE;
        ctx.flags[2] = 1; // C = 1
        ctx.flags[1] = 0; // Z = 0 -> HI true
        let exit = runner.run(&mut ctx, 5);
        assert_eq!(exit, MOCK_BASE + 4 + 0x20);

        // Same branch with Z = 1: HI false -> fallthrough.
        let (_mem, mut ctx, mut runner) = setup(&[0xD810]);
        ctx.regs[15] = MOCK_BASE;
        ctx.flags[2] = 1;
        ctx.flags[1] = 1;
        let exit = runner.run(&mut ctx, 5);
        assert_eq!(exit, MOCK_BASE + 2);

        // bgt: N==V && !Z
        let (_mem, mut ctx, mut runner) = setup(&[0xDC10]);
        ctx.regs[15] = MOCK_BASE;
        ctx.flags[0] = 1; // N
        ctx.flags[3] = 1; // V (N == V)
        ctx.flags[1] = 0; // !Z
        let exit = runner.run(&mut ctx, 5);
        assert_eq!(exit, MOCK_BASE + 4 + 0x20);

        // GT fails when N != V
        let (_mem, mut ctx, mut runner) = setup(&[0xDC10]);
        ctx.regs[15] = MOCK_BASE;
        ctx.flags[0] = 1;
        ctx.flags[3] = 0;
        ctx.flags[1] = 0;
        let exit = runner.run(&mut ctx, 5);
        assert_eq!(exit, MOCK_BASE + 2);
    }

    #[test]
    fn fault_on_store_rewinds_pc_and_keeps_earlier_effects() {
        // mov r0, #0x42 ; str r0, [r0, #0] (fault: 0x42 unmapped)
        let (_mem, mut ctx, mut runner) = setup(&[0x2042, 0x6000]);
        ctx.regs[15] = MOCK_BASE;
        let exit = runner.run(&mut ctx, 5);
        assert_eq!(ctx.fault, 0x42);
        assert_eq!(
            exit,
            MOCK_BASE + 2,
            "regs[15] must rewind to the faulting insn"
        );
        assert_eq!(ctx.regs[0], 0x42, "earlier instructions stay committed");
    }

    #[test]
    fn faulting_push_leaves_sp_unchanged() {
        // push {r0} with sp pointing below the mock window
        let (_mem, mut ctx, mut runner) = setup(&[0xB401]);
        ctx.regs[15] = MOCK_BASE;
        ctx.regs[13] = MOCK_BASE; // start = MOCK_BASE-4 -> unmapped
        let exit = runner.run(&mut ctx, 5);
        assert_eq!(ctx.fault, MOCK_BASE - 4);
        assert_eq!(ctx.regs[13], MOCK_BASE, "sp must not update on fault");
        assert_eq!(exit, MOCK_BASE);
    }

    #[test]
    fn bail_on_bl_leaves_empty_block() {
        // BL prefix (0xf000) as the first instruction.
        let block = translate_thumb_block(&|_| Some(0xF000), MOCK_BASE);
        assert!(block.is_empty());
        assert_eq!(block.guest_end, MOCK_BASE);

        // SWI mid-block: only the prefix translates; exit sits at the SWI.
        let block = translate_thumb_block(
            &|pc| Some(if pc == MOCK_BASE { 0x2001 } else { 0xDF00 }),
            MOCK_BASE,
        );
        assert_eq!(block.instructions, 1);
        assert_eq!(block.guest_end, MOCK_BASE + 2);
    }

    #[test]
    fn swi_mid_block_exits_before_itself() {
        // mov r0, #7 ; swi 0 — run must stop at the SWI with r0 committed.
        let (_mem, mut ctx, mut runner) = setup(&[0x2007, 0xDF00]);
        ctx.regs[15] = MOCK_BASE;
        // The runner panics on empty blocks, so drive one block manually.
        let block = translate_thumb_block(&|p| runner.fetch(p), MOCK_BASE);
        assert_eq!(block.instructions, 1);
        assert_eq!(block.guest_end, MOCK_BASE + 2);
        let ptr = runner.arena.publish(&block.code).unwrap();
        let f: unsafe extern "C" fn(*mut BlockContext) -> u32 =
            unsafe { mem::transmute(runner.arena.entry(ptr)) };
        unsafe { f(&mut *ctx) };
        assert_eq!(ctx.regs[0], 7);
        assert_eq!(ctx.regs[15], MOCK_BASE + 2);
        assert_eq!(ctx.fault, 0);
    }

    #[test]
    fn f4_ops_and_flags() {
        // movs r0, #0xF0        (0x20F0)
        // movs r1, #0x0F        (0x210F)
        // movs r2, #0x25        (0x2225)
        // ands r0, r1           -> 0, Z=1
        // muls r0, r2           -> 0 (still Z=1)
        // movs r3, #5 ; negs r3, r3 -> 0xFFFFFFFB, N=1, C=0 (5 != 0)
        // negs r6, r3           -> 0 - 0xFFFFFFFB = 5 (rd != rs source)
        // bics r4, r1 with r4=0xFF -> 0xF0
        // mvns r5, r1 -> 0xFFFFFFF0, N=1
        // tst-like cmp r2, #0x25 -> Z=1
        let (_mem, mut ctx, mut runner) = setup(&[
            0x20F0, // mov r0, #0xF0
            0x210F, // mov r1, #0x0F
            0x2225, // mov r2, #0x25
            0x4008, // ands r0, r1
            0x4350, // muls r0, r2
            0x2305, // mov r3, #5
            0x425B, // negs r3, r3
            0x425E, // negs r6, r3 -> 5
            0x24FF, // mov r4, #0xFF
            0x438C, // bics r4, r1
            0x43CD, // mvns r5, r1 -> 0xFFFFFFF0
            0x2A25, // cmp r2, #0x25
        ]);
        ctx.regs[15] = MOCK_BASE;
        let exit = runner.run(&mut ctx, 20);
        assert_eq!(exit, MOCK_BASE + 24);
        assert_eq!(ctx.regs[0], 0);
        assert_eq!(ctx.regs[3], 0xFFFF_FFFB);
        assert_eq!(ctx.regs[6], 5, "negs r6, r3 must negate the source r3");
        assert_eq!(ctx.regs[4], 0xF0);
        assert_eq!(ctx.regs[5], 0xFFFF_FFF0);
        assert_eq!(ctx.flags[1], 1, "Z from final cmp r2,#0x25");
    }

    #[test]
    fn f5_hi_register_moves() {
        // F5: 010001 op h1 h2 rs rd (hs = h2:rs, hd = h1:rd)
        // add r7, r8: 010001 00 0 1 000 111 = 0x4447
        // mov r9, r7: 010001 10 1 0 111 001 = 0x46B9
        // cmp r7, r9: 010001 01 0 1 001 111 = 0x454F
        let (_mem, mut ctx, mut runner) = setup(&[
            0x4447, // add r7, r8
            0x46B9, // mov r9, r7
            0x454F, // cmp r7, r9
        ]);
        ctx.regs[15] = MOCK_BASE;
        ctx.regs[7] = 3;
        ctx.regs[8] = 4;
        let exit = runner.run(&mut ctx, 10);
        assert_eq!(exit, MOCK_BASE + 6);
        assert_eq!(ctx.regs[7], 7);
        assert_eq!(ctx.regs[9], 7);
        assert_eq!(ctx.flags[1], 1, "cmp equal sets Z");
        assert_eq!(ctx.flags[2], 1, "cmp equal sets C (no borrow)");
    }

    #[test]
    fn mov_pc_stays_thumb_and_masks_bit0() {
        // mov pc, r6: 010001 10 1 0 110 111 = 0x46B7
        let (_mem, mut ctx, mut runner) = setup(&[0x46B7]);
        ctx.regs[15] = MOCK_BASE;
        ctx.regs[6] = (MOCK_BASE + 0x80) | 1;
        let exit = runner.run(&mut ctx, 5);
        assert_eq!(exit, MOCK_BASE + 0x80);
        assert_eq!(ctx.thumb, 1, "MOV pc always stays Thumb");
    }

    #[test]
    fn sp_adjust_and_sp_relative_access() {
        // sub sp, #0x10 (0xB090) ; mov r0, #9 (0x2009) ; str r0, [sp, #4] (0x9001)
        // ldr r1, [sp, #4] (0x9901) ; add sp, #0x10 (0xB010)
        let (_mem, mut ctx, mut runner) = setup(&[0xB090, 0x2009, 0x9001, 0x9901, 0xB010]);
        ctx.regs[15] = MOCK_BASE;
        ctx.regs[13] = MOCK_BASE + 0x1000;
        let exit = runner.run(&mut ctx, 10);
        assert_eq!(exit, MOCK_BASE + 10);
        assert_eq!(ctx.regs[13], MOCK_BASE + 0x1000);
        assert_eq!(ctx.regs[1], 9);
    }

    #[test]
    fn fetch_failure_ends_block_cleanly() {
        // Single instruction at the very end of the code window: the block
        // translates it, then the fetch of the successor fails and the
        // block exits at the following pc.
        let (_mem, mut ctx, mut runner) = setup(&[0x2201]);
        ctx.regs[15] = MOCK_BASE;
        let exit = runner.run(&mut ctx, 5);
        assert_eq!(exit, MOCK_BASE + 2);
        assert_eq!(ctx.regs[2], 1);
    }

    #[test]
    fn block_caps_at_max_instructions() {
        let mut words = Vec::new();
        for _ in 0..40 {
            words.push(0x2201u16); // mov r2, #1
        }
        let block = translate_thumb_block(
            &|pc| {
                let off = (pc - MOCK_BASE) as usize / 2;
                words.get(off).copied()
            },
            MOCK_BASE,
        );
        assert_eq!(block.instructions, 32);
        assert_eq!(block.guest_end, MOCK_BASE + 64);
    }

    // Probe for the gahhx malloc-cap miscompile (core regression
    // gahhx_malloc_cap_matches_interpreter). Replicates the exact guest
    // sequence from cfunction.ext @ 0x10002358: with free = 0x3FDB10 the
    // request r4 = free - 0x5000 = 0x3F8B10 exceeds the 0x96000 screen
    // cap, so BLE must NOT be taken and `adds r4, r0, #0` clamps r4.
    #[test]
    fn gahhx_malloc_cap_probe() {
        let (_mem, mut ctx, mut runner) = setup(&[
            0x2105, // +0:  movs r1, #5
            0x0309, // +2:  lsls r1, r1, #12      -> r1 = 0x5000
            0x1A44, // +4:  subs r4, r0, r1       -> r4 = 0x3F8B10
            0x204B, // +6:  movs r0, #0x4b
            0x0340, // +8:  lsls r0, r0, #13      -> r0 = 0x96000
            0x4284, // +10: cmp r4, r0
            0xDD00, // +12: ble +0 (target +16; NOT taken: r4 > r0)
            0x1C04, // +14: adds r4, r0, #0       -> cap: r4 = 0x96000
            0x2701, // +16: movs r7, #1
            0x04BF, // +18: lsls r7, r7, #18      -> r7 = 0x40000
            0x1C20, // +20: adds r0, r4, #0
            0x4770, // +22: bx lr
        ]);
        ctx.regs[15] = MOCK_BASE;
        ctx.regs[14] = (MOCK_BASE + 0x100) | 1; // bx lr exits the window
        ctx.regs[0] = 0x003F_DB10; // platform free memory
        let exit = runner.run(&mut ctx, 10);
        assert_eq!(exit, MOCK_BASE + 0x100, "bx lr must exit");
        assert_eq!(ctx.regs[4], 0x0009_6000, "r4 must be capped");
        assert_eq!(ctx.regs[0], 0x0009_6000, "r0 must be capped r4");
        assert_eq!(ctx.regs[7], 0x0004_0000);
        // The final flag setter is `adds r0, r4, #0` (0x96000 + 0):
        // N=0 Z=0 C=0 V=0.
        assert_eq!(ctx.flags[0], 0, "N");
        assert_eq!(ctx.flags[1], 0, "Z");
        assert_eq!(ctx.flags[2], 0, "C: no carry in the final add");
        assert_eq!(ctx.flags[3], 0, "V");
    }
}
