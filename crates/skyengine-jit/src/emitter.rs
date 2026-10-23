//! x86_64 code emitter.
//!
//! Covers the operations the v1 memory-to-memory Thumb translator needs:
//! register and `[base+disp]` operands (with REX extension registers),
//! immediate groups, `setcc` flag capture, indirect `call` through the
//! context block, and forward/backward `rel32` labels.
//!
//! All data encodings operate on the 32-bit register halves, matching the
//! guest's 32-bit ARM semantics; writes implicitly zero-extend into the
//! full register.

/// 32-bit halves of the x86_64 general-purpose registers. The discriminant
/// is the hardware register number; values >= 8 require a REX prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reg32 {
    Eax = 0,
    Ecx = 1,
    Edx = 2,
    Ebx = 3,
    Esp = 4,
    Ebp = 5,
    Esi = 6,
    Edi = 7,
    R8d = 8,
    R9d = 9,
    R10d = 10,
    R11d = 11,
    R12d = 12,
    R13d = 13,
    R14d = 14,
    R15d = 15,
}

impl Reg32 {
    fn low(self) -> u8 {
        (self as u8) & 7
    }

    fn is_ext(self) -> bool {
        (self as u8) >= 8
    }
}

/// Hardware condition codes (the `cc` of `jcc`/`setcc`, in opcode order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cc {
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
    P = 0xA,
    Np = 0xB,
    L = 0xC,
    Ge = 0xD,
    Le = 0xE,
    G = 0xF,
}

/// Label handle for `rel32` branch patching.
pub type Label = usize;

#[derive(Debug)]
struct Fixup {
    /// Buffer offset of the rel32 displacement to patch.
    at: usize,
    label: Label,
}

/// Growable native-code buffer with label patching.
#[derive(Default)]
pub struct X86Emitter {
    buf: Vec<u8>,
    labels: Vec<Option<usize>>,
    fixups: Vec<Fixup>,
}

impl X86Emitter {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(256),
            labels: Vec::new(),
            fixups: Vec::new(),
        }
    }

    /// Bytes emitted so far.
    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.labels.clear();
        self.fixups.clear();
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    // ------------------------------------------------------------------
    // Labels
    // ------------------------------------------------------------------

    pub fn new_label(&mut self) -> Label {
        self.labels.push(None);
        self.labels.len() - 1
    }

    /// Bind `label` at the current position and patch every outstanding
    /// branch to it.
    pub fn bind(&mut self, label: Label) {
        let target = self.buf.len();
        self.labels[label] = Some(target);
        let mut i = 0;
        while i < self.fixups.len() {
            if self.fixups[i].label == label {
                let at = self.fixups.swap_remove(i).at;
                let rel = (target as i64 - (at as i64 + 4)) as i32;
                self.buf[at..at + 4].copy_from_slice(&rel.to_le_bytes());
            } else {
                i += 1;
            }
        }
    }

    fn emit_rel32(&mut self, label: Label) {
        if let Some(target) = self.labels[label] {
            let rel = (target as i64 - (self.buf.len() as i64 + 4)) as i32;
            self.buf.extend_from_slice(&rel.to_le_bytes());
        } else {
            self.fixups.push(Fixup {
                at: self.buf.len(),
                label,
            });
            self.buf.extend_from_slice(&[0, 0, 0, 0]);
        }
    }

    /// `E9 rel32` — jmp label.
    pub fn jmp_label(&mut self, label: Label) {
        self.buf.push(0xE9);
        self.emit_rel32(label);
    }

    /// `0F 8x rel32` — jcc label.
    pub fn jcc_label(&mut self, cc: Cc, label: Label) {
        self.buf.push(0x0F);
        self.buf.push(0x80 + cc as u8);
        self.emit_rel32(label);
    }

    // ------------------------------------------------------------------
    // Raw operand emission helpers
    // ------------------------------------------------------------------

    /// REX prefix when either register needs extension.
    fn rex2(&mut self, reg: Reg32, rm: Reg32) {
        let mut rex = 0x40;
        if reg.is_ext() {
            rex |= 0x04;
        }
        if rm.is_ext() {
            rex |= 0x01;
        }
        if rex != 0x40 {
            self.buf.push(rex);
        }
    }

    /// `opcode modrm(+sib)(+disp)` with a register field over `[base+disp]`.
    fn emit_op_mem(&mut self, opcode: u8, reg_field: u8, reg_ext: bool, base: Reg32, disp: u32) {
        if reg_ext || base.is_ext() {
            let mut rex = 0x40;
            if reg_ext {
                rex |= 0x04;
            }
            if base.is_ext() {
                rex |= 0x01;
            }
            self.buf.push(rex);
        }
        self.buf.push(opcode);
        self.emit_modrm_mem(reg_field, base, disp);
    }

    /// `0F op2 modrm(+sib)(+disp)` — two-byte opcode with memory operand.
    fn emit_op0f_mem(&mut self, op2: u8, reg_field: u8, base: Reg32, disp: u32) {
        if base.is_ext() {
            self.buf.push(0x41);
        }
        self.buf.push(0x0F);
        self.buf.push(op2);
        self.emit_modrm_mem(reg_field, base, disp);
    }

    /// ModRM(+SIB)(+displacement) for `[base + disp]`.
    fn emit_modrm_mem(&mut self, reg_field: u8, base: Reg32, disp: u32) {
        // mod=00 has no displacement but means RIP-relative for rbp and
        // no-base for SIB encodings, so rbp/rsp always take mod=01.
        let (mode, disp_len) = if disp == 0 && base != Reg32::Ebp && base != Reg32::Esp {
            (0x00u8, 0usize)
        } else if disp == 0 || disp <= 0x7f {
            (0x40, 1)
        } else {
            (0x80, 4)
        };
        self.buf.push(mode | (reg_field << 3) | base.low());
        if base == Reg32::Esp {
            self.buf.push(0x24); // SIB: index = none, base = rsp
        }
        match disp_len {
            1 => self.buf.push(disp as u8),
            4 => self.buf.extend_from_slice(&disp.to_le_bytes()),
            _ => {}
        }
    }

    /// `opcode modrm` register-to-register form.
    fn emit_rr(&mut self, opcode: u8, reg: Reg32, rm: Reg32) {
        self.rex2(reg, rm);
        self.buf.push(opcode);
        self.buf.push(0xC0 | (reg.low() << 3) | rm.low());
    }

    /// `81 /reg id` or `83 /reg ib` — immediate group on a register.
    fn emit_grp_imm(&mut self, reg_field: u8, rm: Reg32, imm: u32) {
        if rm.is_ext() {
            self.buf.push(0x41);
        }
        if imm <= 0x7f || imm >= 0xFFFF_FF80 {
            self.buf.push(0x83);
            self.buf.push(0xC0 | (reg_field << 3) | rm.low());
            self.buf.push(imm as u8);
        } else {
            self.buf.push(0x81);
            self.buf.push(0xC0 | (reg_field << 3) | rm.low());
            self.buf.extend_from_slice(&imm.to_le_bytes());
        }
    }

    // ------------------------------------------------------------------
    // Stack (block frame)
    // ------------------------------------------------------------------

    /// `48 83 EC ib` — sub rsp, imm8. Block prologue. 40 bytes covers the
    /// Win64 calling convention's 32-byte shadow space for helper calls
    /// and restores 16-byte stack alignment: a block entered with
    /// rsp ≡ 8 (mod 16) performs every `call` with rsp ≡ 0 (mod 16), as
    /// both the SysV and Win64 ABIs require. SysV simply ignores the
    /// extra 32 bytes.
    pub fn sub_rsp_imm8(&mut self, imm: u8) {
        self.buf.extend_from_slice(&[0x48, 0x83, 0xEC, imm]);
    }

    /// `48 83 C4 ib` — add rsp, imm8. Every block exit pairs this with
    /// the prologue's `sub`.
    pub fn add_rsp_imm8(&mut self, imm: u8) {
        self.buf.extend_from_slice(&[0x48, 0x83, 0xC4, imm]);
    }

    /// Frame slot (in the 40-byte prologue reservation) holding the spilled
    /// context pointer across helper calls. `[rsp+0x20, rsp+0x28)` sits just
    /// above the Win64 shadow space `[rsp, rsp+0x20)` and is never touched
    /// by callees.
    const FRAME_CTX_SLOT: u8 = 0x20;

    /// `48 89 /r` — mov [rsp+FRAME_CTX_SLOT], r64. Prologue spill of the
    /// context pointer: helper calls clobber every caller-saved register,
    /// including the one carrying the context, so it must survive in memory.
    pub fn mov_frame_ptr_store(&mut self, reg: Reg32) {
        let rex = 0x48 | if reg.is_ext() { 0x04 } else { 0 };
        self.buf
            .extend_from_slice(&[rex, 0x89, 0x40 | (reg.low() << 3) | 0x04, 0x24]);
        self.buf.push(Self::FRAME_CTX_SLOT);
    }

    /// `48 8B /r` — mov r64, [rsp+FRAME_CTX_SLOT]. Reload the context
    /// pointer after a helper call, before any context-relative access.
    pub fn mov_frame_ptr_load(&mut self, reg: Reg32) {
        let rex = 0x48 | if reg.is_ext() { 0x04 } else { 0 };
        self.buf
            .extend_from_slice(&[rex, 0x8B, 0x40 | (reg.low() << 3) | 0x04, 0x24]);
        self.buf.push(Self::FRAME_CTX_SLOT);
    }

    /// `C3` — near return.
    pub fn ret(&mut self) {
        self.buf.push(0xC3);
    }

    // ------------------------------------------------------------------
    // Data movement
    // ------------------------------------------------------------------

    /// `B8+r id` — mov r32, imm32.
    pub fn mov_r32_imm32(&mut self, dst: Reg32, imm: u32) {
        if dst.is_ext() {
            self.buf.push(0x41);
        }
        self.buf.push(0xB8 + dst.low());
        self.buf.extend_from_slice(&imm.to_le_bytes());
    }

    /// `89 /r` — mov r/m32, r32.
    pub fn mov_r32_r32(&mut self, dst: Reg32, src: Reg32) {
        self.emit_rr(0x89, src, dst);
    }

    /// `8B /r` — mov r32, [base+disp].
    pub fn mov_r32_mem(&mut self, dst: Reg32, base: Reg32, disp: u32) {
        self.emit_op_mem(0x8B, dst.low(), dst.is_ext(), base, disp);
    }

    /// `89 /r` — mov [base+disp], r32.
    pub fn mov_mem_r32(&mut self, base: Reg32, disp: u32, src: Reg32) {
        self.emit_op_mem(0x89, src.low(), src.is_ext(), base, disp);
    }

    /// `C7 /0 id` — mov dword [base+disp], imm32.
    pub fn mov_mem_imm32(&mut self, base: Reg32, disp: u32, imm: u32) {
        self.emit_op_mem(0xC7, 0, false, base, disp);
        self.buf.extend_from_slice(&imm.to_le_bytes());
    }

    /// `C6 /0 ib` — mov byte [base+disp], imm8.
    pub fn mov_mem8_imm8(&mut self, base: Reg32, disp: u32, imm: u8) {
        self.emit_op_mem(0xC6, 0, false, base, disp);
        self.buf.push(imm);
    }

    /// `88 /r` — mov byte [base+disp], r8 (low byte of `src`).
    pub fn mov_mem8_r8(&mut self, base: Reg32, disp: u32, src: Reg32) {
        debug_assert!(!src.is_ext(), "byte store from REX registers is not needed");
        self.emit_op_mem(0x88, src.low(), false, base, disp);
    }

    /// `8A /r` — mov r8 (low byte of `dst`), byte [base+disp].
    pub fn mov_r8_mem(&mut self, dst: Reg32, base: Reg32, disp: u32) {
        debug_assert!(!dst.is_ext(), "byte load into REX registers is not needed");
        self.emit_op_mem(0x8A, dst.low(), false, base, disp);
    }

    // ------------------------------------------------------------------
    // Arithmetic / logic
    // ------------------------------------------------------------------

    /// `01 /r` — add r/m32, r32.
    pub fn add_r32_r32(&mut self, dst: Reg32, src: Reg32) {
        self.emit_rr(0x01, src, dst);
    }

    /// `29 /r` — sub r/m32, r32.
    pub fn sub_r32_r32(&mut self, dst: Reg32, src: Reg32) {
        self.emit_rr(0x29, src, dst);
    }

    /// `21 /r` — and r/m32, r32.
    pub fn and_r32_r32(&mut self, dst: Reg32, src: Reg32) {
        self.emit_rr(0x21, src, dst);
    }

    /// `09 /r` — or r/m32, r32.
    pub fn or_r32_r32(&mut self, dst: Reg32, src: Reg32) {
        self.emit_rr(0x09, src, dst);
    }

    /// `31 /r` — xor r/m32, r32.
    pub fn xor_r32_r32(&mut self, dst: Reg32, src: Reg32) {
        self.emit_rr(0x31, src, dst);
    }

    /// `39 /r` — cmp r/m32, r32.
    pub fn cmp_r32_r32(&mut self, left: Reg32, right: Reg32) {
        self.emit_rr(0x39, right, left);
    }

    /// `3A /r` — cmp r8, byte [base+disp].
    pub fn cmp_r8_mem(&mut self, left: Reg32, base: Reg32, disp: u32) {
        debug_assert!(
            !left.is_ext(),
            "byte compare from REX registers is not needed"
        );
        self.emit_op_mem(0x3A, left.low(), false, base, disp);
    }

    /// `85 /r` — test r/m32, r32.
    pub fn test_r32_r32(&mut self, a: Reg32, b: Reg32) {
        self.emit_rr(0x85, b, a);
    }

    /// `81 /0` — add r32, imm32.
    pub fn add_r32_imm32(&mut self, dst: Reg32, imm: u32) {
        self.emit_grp_imm(0, dst, imm);
    }

    /// `81 /5` — sub r32, imm32.
    pub fn sub_r32_imm32(&mut self, dst: Reg32, imm: u32) {
        self.emit_grp_imm(5, dst, imm);
    }

    /// `81 /4` — and r32, imm32.
    pub fn and_r32_imm32(&mut self, dst: Reg32, imm: u32) {
        self.emit_grp_imm(4, dst, imm);
    }

    /// `81 /7` — cmp r32, imm32.
    pub fn cmp_r32_imm32(&mut self, left: Reg32, imm: u32) {
        self.emit_grp_imm(7, left, imm);
    }

    /// `83 /7 ib` — cmp dword [base+disp], imm8.
    pub fn cmp_mem_imm8(&mut self, base: Reg32, disp: u32, imm: i8) {
        self.emit_op_mem(0x83, 7, false, base, disp);
        self.buf.push(imm as u8);
    }

    /// `80 /7 ib` — cmp byte [base+disp], imm8. For testing single flag
    /// bytes packed next to neighbors (dword reads would pick those up).
    pub fn cmp_mem8_imm8(&mut self, base: Reg32, disp: u32, imm: i8) {
        self.emit_op_mem(0x80, 7, false, base, disp);
        self.buf.push(imm as u8);
    }

    // ------------------------------------------------------------------
    // Shifts and one-operand groups
    // ------------------------------------------------------------------

    /// `C1 /4 ib` — shl r32, imm8.
    pub fn shl_r32_imm8(&mut self, dst: Reg32, imm: u8) {
        self.shift_r32_imm8(dst, 4, imm);
    }

    /// `C1 /5 ib` — shr r32, imm8.
    pub fn shr_r32_imm8(&mut self, dst: Reg32, imm: u8) {
        self.shift_r32_imm8(dst, 5, imm);
    }

    /// `C1 /7 ib` — sar r32, imm8.
    pub fn sar_r32_imm8(&mut self, dst: Reg32, imm: u8) {
        self.shift_r32_imm8(dst, 7, imm);
    }

    fn shift_r32_imm8(&mut self, dst: Reg32, op: u8, imm: u8) {
        if dst.is_ext() {
            self.buf.push(0x41);
        }
        self.buf
            .extend_from_slice(&[0xC1, 0xC0 | (op << 3) | dst.low(), imm]);
    }

    /// `F7 /2` — not r32 (flags untouched).
    pub fn not_r32(&mut self, dst: Reg32) {
        self.emit_f7(2, dst);
    }

    /// `F7 /3` — neg r32.
    pub fn neg_r32(&mut self, dst: Reg32) {
        self.emit_f7(3, dst);
    }

    fn emit_f7(&mut self, reg_field: u8, rm: Reg32) {
        if rm.is_ext() {
            self.buf.push(0x41);
        }
        self.buf.push(0xF7);
        self.buf.push(0xC0 | (reg_field << 3) | rm.low());
    }

    /// `0F AF /r` — imul r32, r32 (SF/ZF undefined; caller re-tests).
    pub fn imul_r32_r32(&mut self, dst: Reg32, src: Reg32) {
        self.rex2(dst, src);
        self.buf.push(0x0F);
        self.buf.push(0xAF);
        self.buf.push(0xC0 | (dst.low() << 3) | src.low());
    }

    /// `0F BA /4 ib` — bt r32, imm8 (CF = bit).
    pub fn bt_r32_imm8(&mut self, rm: Reg32, bit: u8) {
        if rm.is_ext() {
            self.buf.push(0x41);
        }
        self.buf.push(0x0F);
        self.buf.push(0xBA);
        self.buf.push(0xC0 | (4 << 3) | rm.low());
        self.buf.push(bit);
    }

    // ------------------------------------------------------------------
    // Flags capture and calls
    // ------------------------------------------------------------------

    /// `0F 9x /r` — setcc byte [base+disp] (condition lives in the opcode).
    pub fn setcc_mem(&mut self, cc: Cc, base: Reg32, disp: u32) {
        self.emit_op0f_mem(0x90 + cc as u8, 0, base, disp);
    }

    /// `FF /2` — call [base+disp].
    pub fn call_mem(&mut self, base: Reg32, disp: u32) {
        self.emit_op_mem(0xFF, 2, false, base, disp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_documented_byte_sequences() {
        // mov eax, 0x5EED ; ret
        let mut e = X86Emitter::new();
        e.mov_r32_imm32(Reg32::Eax, 0x5EED);
        e.ret();
        assert_eq!(e.bytes(), &[0xB8, 0xED, 0x5E, 0x00, 0x00, 0xC3]);

        // mov eax, ecx (89 C8)
        let mut e = X86Emitter::new();
        e.mov_r32_r32(Reg32::Eax, Reg32::Ecx);
        assert_eq!(e.bytes(), &[0x89, 0xC8]);

        // add eax, ecx (01 C8) / sub eax, ecx (29 C8)
        let mut e = X86Emitter::new();
        e.add_r32_r32(Reg32::Eax, Reg32::Ecx);
        assert_eq!(e.bytes(), &[0x01, 0xC8]);
        let mut e = X86Emitter::new();
        e.sub_r32_r32(Reg32::Eax, Reg32::Ecx);
        assert_eq!(e.bytes(), &[0x29, 0xC8]);

        // shl eax, 3 (C1 E0 03)
        let mut e = X86Emitter::new();
        e.shl_r32_imm8(Reg32::Eax, 3);
        assert_eq!(e.bytes(), &[0xC1, 0xE0, 0x03]);
    }

    #[test]
    fn encodes_memory_operand_forms() {
        // mov eax, [rcx+0x40] (8B 41 40)
        let mut e = X86Emitter::new();
        e.mov_r32_mem(Reg32::Eax, Reg32::Ecx, 0x40);
        assert_eq!(e.bytes(), &[0x8B, 0x41, 0x40]);

        // mov [rcx+0x3C], eax (89 41 3C)
        let mut e = X86Emitter::new();
        e.mov_mem_r32(Reg32::Ecx, 0x3C, Reg32::Eax);
        assert_eq!(e.bytes(), &[0x89, 0x41, 0x3C]);

        // mov dword [rcx+0x3C], imm32 (C7 41 3C ...)
        let mut e = X86Emitter::new();
        e.mov_mem_imm32(Reg32::Ecx, 0x3C, 0x1234_5678);
        assert_eq!(e.bytes(), &[0xC7, 0x41, 0x3C, 0x78, 0x56, 0x34, 0x12]);

        // cmp dword [rcx+0x78], 0 (83 79 78 00)
        let mut e = X86Emitter::new();
        e.cmp_mem_imm8(Reg32::Ecx, 0x78, 0);
        assert_eq!(e.bytes(), &[0x83, 0x79, 0x78, 0x00]);

        // disp32 fallback: mov eax, [rcx+0x100] (8B 81 00 01 00 00)
        let mut e = X86Emitter::new();
        e.mov_r32_mem(Reg32::Eax, Reg32::Ecx, 0x100);
        assert_eq!(e.bytes(), &[0x8B, 0x81, 0x00, 0x01, 0x00, 0x00]);
    }

    #[test]
    fn encodes_rex_extension_registers() {
        // mov r8d, eax (41 89 C0)
        let mut e = X86Emitter::new();
        e.mov_r32_r32(Reg32::R8d, Reg32::Eax);
        assert_eq!(e.bytes(), &[0x41, 0x89, 0xC0]);

        // mov r8d, 0x10 (41 B8 10 00 00 00)
        let mut e = X86Emitter::new();
        e.mov_r32_imm32(Reg32::R8d, 0x10);
        assert_eq!(e.bytes(), &[0x41, 0xB8, 0x10, 0x00, 0x00, 0x00]);

        // mov esi, [rdi+0x40] on SysV needs no REX (8B 77 40)
        let mut e = X86Emitter::new();
        e.mov_r32_mem(Reg32::Esi, Reg32::Edi, 0x40);
        assert_eq!(e.bytes(), &[0x8B, 0x77, 0x40]);
    }

    #[test]
    fn encodes_flag_capture_and_call() {
        // setne byte [rcx+0x71] (0F 95 41 71)
        let mut e = X86Emitter::new();
        e.setcc_mem(Cc::Ne, Reg32::Ecx, 0x71);
        assert_eq!(e.bytes(), &[0x0F, 0x95, 0x41, 0x71]);

        // call [rcx+0x50] (FF 51 50)
        let mut e = X86Emitter::new();
        e.call_mem(Reg32::Ecx, 0x50);
        assert_eq!(e.bytes(), &[0xFF, 0x51, 0x50]);

        // bt eax, 31 (0F BA E0 1F)
        let mut e = X86Emitter::new();
        e.bt_r32_imm8(Reg32::Eax, 31);
        assert_eq!(e.bytes(), &[0x0F, 0xBA, 0xE0, 0x1F]);

        // not eax / neg eax (F7 D0 / F7 D8)
        let mut e = X86Emitter::new();
        e.not_r32(Reg32::Eax);
        assert_eq!(e.bytes(), &[0xF7, 0xD0]);
        let mut e = X86Emitter::new();
        e.neg_r32(Reg32::Eax);
        assert_eq!(e.bytes(), &[0xF7, 0xD8]);

        // imul eax, edx (0F AF C2)
        let mut e = X86Emitter::new();
        e.imul_r32_r32(Reg32::Eax, Reg32::Edx);
        assert_eq!(e.bytes(), &[0x0F, 0xAF, 0xC2]);

        // cmp al, byte [rcx+0x70] (3A 41 70)
        let mut e = X86Emitter::new();
        e.cmp_r8_mem(Reg32::Eax, Reg32::Ecx, 0x70);
        assert_eq!(e.bytes(), &[0x3A, 0x41, 0x70]);

        // mov byte [rcx+0x74], dl (88 51 74)
        let mut e = X86Emitter::new();
        e.mov_mem8_r8(Reg32::Ecx, 0x74, Reg32::Edx);
        assert_eq!(e.bytes(), &[0x88, 0x51, 0x74]);
    }

    #[test]
    fn patches_forward_and_backward_branches() {
        // Forward: jne over a two-byte nop; rel32 = 2.
        let mut e = X86Emitter::new();
        let skip = e.new_label();
        e.jcc_label(Cc::Ne, skip);
        e.xor_r32_r32(Reg32::Eax, Reg32::Eax);
        e.bind(skip);
        e.ret();
        assert_eq!(
            e.bytes(),
            &[0x0F, 0x85, 0x02, 0x00, 0x00, 0x00, 0x31, 0xC0, 0xC3]
        );

        // Backward: loop with jmp rel32 = -(5 + 2).
        let mut e = X86Emitter::new();
        let top = e.new_label();
        e.bind(top);
        e.add_r32_r32(Reg32::Eax, Reg32::Edx);
        e.jmp_label(top);
        assert_eq!(e.bytes(), &[0x01, 0xD0, 0xE9, 0xF9, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn emits_small_immediates_as_imm8_groups() {
        // add eax, 8 (83 C0 08)
        let mut e = X86Emitter::new();
        e.add_r32_imm32(Reg32::Eax, 8);
        assert_eq!(e.bytes(), &[0x83, 0xC0, 0x08]);

        // and eax, 0xFFFFFFFE (83 E0 FE)
        let mut e = X86Emitter::new();
        e.and_r32_imm32(Reg32::Eax, 0xFFFF_FFFE);
        assert_eq!(e.bytes(), &[0x83, 0xE0, 0xFE]);

        // sub ebx, 0x100 (81 EB 00 01 00 00)
        let mut e = X86Emitter::new();
        e.sub_r32_imm32(Reg32::Ebx, 0x100);
        assert_eq!(e.bytes(), &[0x81, 0xEB, 0x00, 0x01, 0x00, 0x00]);
    }
}
