use crate::{Error, Result};

use super::{GuestAddr, GuestMemory};

use std::sync::atomic::Ordering;

const LEGACY_NULL_DATA_LEN: u32 = 8;

#[derive(Clone, Debug)]
pub struct ArmCpu {
    registers: [u32; 16],
    negative: bool,
    zero: bool,
    carry: bool,
    overflow: bool,
    saturation: bool,
    thumb: bool,
    semihosting_exit_reason: Option<u32>,
    legacy_null_data: Option<[u8; LEGACY_NULL_DATA_LEN as usize]>,
}

impl Default for ArmCpu {
    fn default() -> Self {
        Self::new()
    }
}

impl ArmCpu {
    pub fn new() -> Self {
        Self {
            registers: [0; 16],
            negative: false,
            zero: false,
            carry: false,
            overflow: false,
            saturation: false,
            thumb: false,
            semihosting_exit_reason: None,
            legacy_null_data: None,
        }
    }

    pub fn register(&self, index: usize) -> u32 {
        self.registers[index]
    }

    pub fn set_register(&mut self, index: usize, value: u32) {
        self.registers[index] = value;
    }

    pub fn pc(&self) -> GuestAddr {
        GuestAddr(self.registers[15])
    }

    pub fn is_thumb(&self) -> bool {
        self.thumb
    }

    pub fn set_pc(&mut self, address: u32) {
        self.thumb = address & 1 != 0;
        self.registers[15] = if self.thumb {
            address & !1
        } else {
            address & !3
        };
    }

    /// Restore the N/Z/C/V condition flags from a translated-block context.
    /// Saturation is not modeled by translated blocks and is preserved.
    pub fn restore_flags(&mut self, flags: [u8; 4]) {
        self.negative = flags[0] != 0;
        self.zero = flags[1] != 0;
        self.carry = flags[2] != 0;
        self.overflow = flags[3] != 0;
    }

    /// Restore the pc and Thumb state as reported by a translated block's
    /// exit: the exit pc is already interlock-clean, and the target state
    /// comes from the block's own Thumb tracking, not from pc bit 0.
    pub fn restore_pc_and_state(&mut self, pc: u32, thumb: bool) {
        self.registers[15] = pc;
        self.thumb = thumb;
    }

    /// Consume the pending semihosting exit reason, if any.
    pub fn take_semihosting_exit_reason(&mut self) -> Option<u32> {
        self.semihosting_exit_reason.take()
    }

    /// Enable private scratch data at unmapped addresses 0..8 for legacy code.
    /// Instruction fetches and host memory accesses remain permission checked.
    pub fn allow_legacy_null_data_accesses(&mut self) {
        // Keep legacy low-address scratch state inside the CPU so NULL remains
        // unmapped to host services and instruction fetches.
        self.legacy_null_data = Some([0; LEGACY_NULL_DATA_LEN as usize]);
    }

    pub fn cpsr(&self) -> u32 {
        (u32::from(self.negative) << 31)
            | (u32::from(self.zero) << 30)
            | (u32::from(self.carry) << 29)
            | (u32::from(self.overflow) << 28)
            | (u32::from(self.saturation) << 27)
            | (u32::from(self.thumb) << 5)
    }

    pub fn step(&mut self, memory: &mut GuestMemory) -> Result<()> {
        crate::memory::CURRENT_PC.store(self.registers[15], Ordering::Relaxed);
        // The step functions advance the PC before executing so instruction
        // semantics can read the architecturally correct PC value. A failed
        // step must therefore rewind it: the faulting instruction has not
        // committed, and hosts need to re-execute it after compatibility
        // fallbacks (such as mapping a missing memory window) succeed.
        let pc_before = self.registers[15];
        let result = if self.thumb {
            self.step_thumb(memory)
        } else {
            self.step_arm(memory)
        };
        if result.is_err() && self.registers[15] != pc_before {
            // Only the PC advance of the faulting instruction is undone. The
            // thumb flag cannot change on a faulting data access because
            // control-flow instructions do not access memory.
            self.registers[15] = pc_before;
        }
        result
    }

    fn step_arm(&mut self, memory: &mut GuestMemory) -> Result<()> {
        let address = self.registers[15];
        if address & 3 != 0 {
            return Err(Error::ArmFault(format!("unaligned A32 PC {address:#010x}")));
        }
        let instruction = memory.fetch_u32(GuestAddr(address))?;
        self.registers[15] = address.wrapping_add(4);

        let condition = (instruction >> 28) as u8;
        if condition == 0xf {
            return self.execute_unconditional_arm(instruction, address);
        }
        if !self.condition_passed(condition) {
            return Ok(());
        }

        if instruction == 0xef12_3456 && self.registers[0] == 0x18 {
            self.semihosting_exit_reason = Some(self.registers[1]);
            return Ok(());
        }

        // Fast dispatch: plain loads/stores, branches, and ALU operations
        // never match the special-case encodings checked further below, so
        // route them straight to their handlers. The guards are derived from
        // those masks: every special case lives in the bits 27..25 = 000/001
        // space (semihosting is checked above), the 001 space only contains
        // MSR #imm (Rd == PC), and the 000-space aliases all have bit 4 set
        // (multiply/extra load-store/branch-exchange family), Rd == PC
        // (MRS/MSR), or the S flag clear with a test opcode (TST/TEQ/CMP/CMN
        // cover the signed halfword multiply aliases, which never set S).
        match (instruction >> 25) & 0x7 {
            0b010 | 0b011 => return self.single_transfer(memory, instruction, address),
            0b100 => return self.block_transfer(memory, instruction, address),
            0b101 => return self.branch(instruction, address),
            0b001 if (instruction >> 12) & 0xF != 0xF => {
                return self.data_processing(instruction, address);
            }
            0b000
                if instruction & 0x10 == 0
                    && (instruction >> 12) & 0xF != 0xF
                    && (instruction & 0x0010_0000 != 0 || (instruction >> 23) & 0x3 != 0b10) =>
            {
                return self.data_processing(instruction, address);
            }
            _ => {}
        }

        if instruction & 0x0fff_fff0 == 0x012f_ff10 {
            let target = self.read_arm_register((instruction & 0xf) as usize, address);
            self.set_pc(target);
            return Ok(());
        }
        if instruction & 0x0fff_fff0 == 0x012f_ff20 {
            // BXJ degrades to BX when Jazelle is not active; no Jazelle
            // state is modelled, matching the ARM926EJ-S fallback.
            let target = self.read_arm_register((instruction & 0xf) as usize, address);
            self.set_pc(target);
            return Ok(());
        }
        if instruction & 0x0fff_fff0 == 0x012f_ff30 {
            let target = self.read_arm_register((instruction & 0xf) as usize, address);
            self.registers[14] = address.wrapping_add(4);
            self.set_pc(target);
            return Ok(());
        }
        if instruction & 0x0fff_0ff0 == 0x016f_0f10 {
            // CLZ Rd, Rm (ARMv5T). Without this the encoding silently aliases to
            // data processing (CMN pc, Rm, lsl ...) and leaves Rd unwritten, which
            // corrupts soft-float helpers such as __aeabi_dadd / __aeabi_i2d.
            let rd = ((instruction >> 12) & 0xf) as usize;
            let rm = (instruction & 0xf) as usize;
            if rd == 15 || rm == 15 {
                return Err(Error::ArmFault(format!(
                    "clz uses PC at {address:#010x}"
                )));
            }
            self.registers[rd] = self.registers[rm].leading_zeros();
            return Ok(());
        }
        if instruction & 0x0fbf_0fff == 0x010f_0000 {
            // MRS Rd, CPSR
            let rd = ((instruction >> 12) & 0xf) as usize;
            if rd == 15 {
                return Err(self.unsupported_arm(instruction, address));
            }
            self.registers[rd] = self.cpsr();
            return Ok(());
        }
        if instruction & 0x0fbf_0fff == 0x014f_0000 {
            // MRS Rd, SPSR: privileged and unused by MRP guests.
            return Err(self.unsupported_arm(instruction, address));
        }
        if instruction & 0x0ff0_f0f0 == 0x0120_f000 {
            // MSR CPSR_<fields>, Rm
            let field_mask = ((instruction >> 16) & 0xf) as u8;
            let value = self.read_arm_register((instruction & 0xf) as usize, address);
            return self.write_cpsr_fields(field_mask, value, instruction, address);
        }
        if instruction & 0x0ff0_f000 == 0x0320_f000 {
            // MSR CPSR_<fields>, #imm
            let field_mask = ((instruction >> 16) & 0xf) as u8;
            let rotate = ((instruction >> 8) & 0xf) * 2;
            let value = (instruction & 0xff).rotate_right(rotate);
            return self.write_cpsr_fields(field_mask, value, instruction, address);
        }
        if instruction & 0x0f90_00f0 == 0x0100_0050 {
            // QADD/QSUB/QDADD/QDSUB (ARMv5TE) would otherwise silently alias
            // to data processing and leave Rd unwritten.
            return self.saturating_arithmetic(instruction, address);
        }
        if instruction & 0x0ff0_00f0 == 0x0120_0070 {
            // BKPT would otherwise silently alias to data processing.
            return Err(self.unsupported_arm(instruction, address));
        }
        if instruction & 0x0ff0_0090 == 0x0100_0080 {
            return self.signed_halfword_multiply(instruction, address, true);
        }
        if instruction & 0x0ff0_f090 == 0x0160_0080 {
            return self.signed_halfword_multiply(instruction, address, false);
        }
        if instruction & 0x0ff0_0090 == 0x0140_0080 {
            // SMLALBB/BT/TB/TT (ARMv5TE) would otherwise silently alias to
            // data processing and overwrite a destination register.
            return self.signed_halfword_multiply_long(instruction, address);
        }
        if instruction & 0x0ff0_f090 == 0x0120_0080 {
            // SMULWB/WT (ARMv5TE) would otherwise silently alias to data
            // processing.
            return self.signed_word_halfword_multiply(instruction, address, false);
        }
        if instruction & 0x0ff0_0090 == 0x0120_0080 {
            // SMLAWB/WT (ARMv5TE) would otherwise silently alias to data
            // processing.
            return self.signed_word_halfword_multiply(instruction, address, true);
        }
        if instruction & 0x0fc0_00f0 == 0x0000_0090 {
            return self.multiply(instruction, address);
        }
        if instruction & 0x0f80_00f0 == 0x0080_0090 {
            return self.multiply_long(instruction, address);
        }
        if instruction & 0x0fb0_0ff0 == 0x0100_0090 {
            // SWP/SWPB share the halfword encoding space; dispatch first.
            return self.swap(memory, instruction, address);
        }
        if instruction & 0x0e10_00f0 == 0x0000_00d0 {
            // LDRD (ARMv5TE): bits 7-4 = 1101 with L = 0.
            return self.doubleword_transfer(memory, instruction, address, true);
        }
        if instruction & 0x0e10_00f0 == 0x0000_00f0 {
            // STRD (ARMv5TE): bits 7-4 = 1111 with L = 0.
            return self.doubleword_transfer(memory, instruction, address, false);
        }
        if instruction & 0x0e00_0090 == 0x0000_0090 {
            return self.halfword_transfer(memory, instruction, address);
        }
        match instruction & 0x0e00_0000 {
            0x0a00_0000 => self.branch(instruction, address),
            0x0800_0000 => self.block_transfer(memory, instruction, address),
            _ if instruction & 0x0c00_0000 == 0x0400_0000 => {
                self.single_transfer(memory, instruction, address)
            }
            _ if instruction & 0x0c00_0000 == 0 => self.data_processing(instruction, address),
            _ => Err(self.unsupported_arm(instruction, address)),
        }
    }

    fn execute_unconditional_arm(&mut self, instruction: u32, address: u32) -> Result<()> {
        if instruction & 0x0e00_0000 == 0x0a00_0000 {
            let high = (instruction >> 23) & 2;
            let immediate = ((instruction & 0x00ff_ffff) << 2) | high;
            let offset = sign_extend(immediate, 26);
            self.registers[14] = address.wrapping_add(4);
            self.thumb = true;
            self.registers[15] = address.wrapping_add(8).wrapping_add_signed(offset) & !1;
            return Ok(());
        }
        if instruction & 0x0d2f_f000 == 0x0500_f000 {
            // PLD/PLDW/PLI are cache hints; a NOP is architecturally valid
            // and avoids faulting on compiler-emitted preload instructions.
            return Ok(());
        }
        Err(self.unsupported_arm(instruction, address))
    }

    fn branch(&mut self, instruction: u32, address: u32) -> Result<()> {
        let link = instruction & (1 << 24) != 0;
        let offset = sign_extend((instruction & 0x00ff_ffff) << 2, 26);
        if link {
            self.registers[14] = address.wrapping_add(4);
        }
        self.registers[15] = address.wrapping_add(8).wrapping_add_signed(offset) & !3;
        Ok(())
    }

    fn data_processing(&mut self, instruction: u32, address: u32) -> Result<()> {
        let immediate = instruction & (1 << 25) != 0;
        let opcode = ((instruction >> 21) & 0xf) as u8;
        let set_flags = instruction & (1 << 20) != 0;
        let rn = ((instruction >> 16) & 0xf) as usize;
        let rd = ((instruction >> 12) & 0xf) as usize;
        let left = self.read_arm_register(rn, address);
        let (right, shifter_carry) = self.decode_operand2(instruction, immediate, address)?;

        let mut arithmetic_flags = None;
        let result = match opcode {
            0 => left & right,
            1 => left ^ right,
            2 => {
                let flags = add_with_carry(left, !right, true);
                arithmetic_flags = Some(flags);
                flags.0
            }
            3 => {
                let flags = add_with_carry(right, !left, true);
                arithmetic_flags = Some(flags);
                flags.0
            }
            4 => {
                let flags = add_with_carry(left, right, false);
                arithmetic_flags = Some(flags);
                flags.0
            }
            5 => {
                let flags = add_with_carry(left, right, self.carry);
                arithmetic_flags = Some(flags);
                flags.0
            }
            6 => {
                let flags = add_with_carry(left, !right, self.carry);
                arithmetic_flags = Some(flags);
                flags.0
            }
            7 => {
                let flags = add_with_carry(right, !left, self.carry);
                arithmetic_flags = Some(flags);
                flags.0
            }
            8 => left & right,
            9 => left ^ right,
            10 => {
                let flags = add_with_carry(left, !right, true);
                arithmetic_flags = Some(flags);
                flags.0
            }
            11 => {
                let flags = add_with_carry(left, right, false);
                arithmetic_flags = Some(flags);
                flags.0
            }
            12 => left | right,
            13 => right,
            14 => left & !right,
            15 => !right,
            _ => unreachable!(),
        };

        let test_only = matches!(opcode, 8..=11);
        if !test_only {
            self.write_arm_register(rd, result);
        }
        if set_flags || test_only {
            self.negative = result & 0x8000_0000 != 0;
            self.zero = result == 0;
            if let Some((_, carry, overflow)) = arithmetic_flags {
                self.carry = carry;
                self.overflow = overflow;
            } else {
                self.carry = shifter_carry;
            }
        }
        Ok(())
    }

    fn decode_operand2(
        &self,
        instruction: u32,
        immediate: bool,
        address: u32,
    ) -> Result<(u32, bool)> {
        if immediate {
            let value = instruction & 0xff;
            let rotate = ((instruction >> 8) & 0xf) * 2;
            let result = value.rotate_right(rotate);
            return Ok((
                result,
                if rotate == 0 {
                    self.carry
                } else {
                    result & 0x8000_0000 != 0
                },
            ));
        }
        let rm = (instruction & 0xf) as usize;
        let value = self.read_arm_register(rm, address);
        let shift_type = ((instruction >> 5) & 3) as u8;
        if instruction & (1 << 4) == 0 {
            let amount = (instruction >> 7) & 0x1f;
            Ok(shift_immediate(value, shift_type, amount, self.carry))
        } else {
            if instruction & (1 << 7) != 0 {
                return Err(self.unsupported_arm(instruction, address));
            }
            let rs = ((instruction >> 8) & 0xf) as usize;
            let amount = self.read_arm_register(rs, address) & 0xff;
            Ok(shift_register(value, shift_type, amount, self.carry))
        }
    }

    fn multiply(&mut self, instruction: u32, address: u32) -> Result<()> {
        if instruction & (1 << 23) != 0 {
            return Err(self.unsupported_arm(instruction, address));
        }
        let accumulate = instruction & (1 << 21) != 0;
        let set_flags = instruction & (1 << 20) != 0;
        let rd = ((instruction >> 16) & 0xf) as usize;
        let rn = ((instruction >> 12) & 0xf) as usize;
        let rs = ((instruction >> 8) & 0xf) as usize;
        let rm = (instruction & 0xf) as usize;
        if rd == 15 || rn == 15 || rs == 15 || rm == 15 {
            return Err(Error::ArmFault(format!(
                "multiply uses PC at {address:#010x}"
            )));
        }
        let mut result = self.registers[rm].wrapping_mul(self.registers[rs]);
        if accumulate {
            result = result.wrapping_add(self.registers[rn]);
        }
        self.registers[rd] = result;
        if set_flags {
            self.negative = result & 0x8000_0000 != 0;
            self.zero = result == 0;
        }
        Ok(())
    }

    fn signed_halfword_multiply(
        &mut self,
        instruction: u32,
        address: u32,
        accumulate: bool,
    ) -> Result<()> {
        let rd = ((instruction >> 16) & 0xf) as usize;
        let rn = ((instruction >> 12) & 0xf) as usize;
        let rs = ((instruction >> 8) & 0xf) as usize;
        let rm = (instruction & 0xf) as usize;
        if rd == 15 || rs == 15 || rm == 15 || (accumulate && rn == 15) {
            return Err(Error::ArmFault(format!(
                "signed halfword multiply uses PC at {address:#010x}"
            )));
        }
        let first = signed_halfword(self.registers[rm], instruction & (1 << 5) != 0);
        let second = signed_halfword(self.registers[rs], instruction & (1 << 6) != 0);
        let product = i32::from(first).wrapping_mul(i32::from(second)) as u32;
        self.registers[rd] = if accumulate {
            product.wrapping_add(self.registers[rn])
        } else {
            product
        };
        Ok(())
    }

    fn signed_halfword_multiply_long(
        &mut self,
        instruction: u32,
        address: u32,
    ) -> Result<()> {
        let rd_hi = ((instruction >> 16) & 0xf) as usize;
        let rd_lo = ((instruction >> 12) & 0xf) as usize;
        let rs = ((instruction >> 8) & 0xf) as usize;
        let rm = (instruction & 0xf) as usize;
        if rd_hi == 15 || rd_lo == 15 || rs == 15 || rm == 15 || rd_hi == rd_lo {
            return Err(Error::ArmFault(format!(
                "signed halfword long multiply has invalid registers at {address:#010x}"
            )));
        }
        let first = i32::from(signed_halfword(self.registers[rm], instruction & (1 << 5) != 0));
        let second = i32::from(signed_halfword(self.registers[rs], instruction & (1 << 6) != 0));
        let product = i64::from(first) * i64::from(second);
        let accumulator = ((u64::from(self.registers[rd_hi]) << 32)
            | u64::from(self.registers[rd_lo])) as i64;
        let result = accumulator.wrapping_add(product);
        self.registers[rd_lo] = result as u32;
        self.registers[rd_hi] = (result as u64 >> 32) as u32;
        Ok(())
    }

    fn signed_word_halfword_multiply(
        &mut self,
        instruction: u32,
        address: u32,
        accumulate: bool,
    ) -> Result<()> {
        let rd = ((instruction >> 16) & 0xf) as usize;
        let rn = ((instruction >> 12) & 0xf) as usize;
        let rs = ((instruction >> 8) & 0xf) as usize;
        let rm = (instruction & 0xf) as usize;
        if rd == 15 || rs == 15 || rm == 15 || (accumulate && rn == 15) {
            return Err(Error::ArmFault(format!(
                "signed word multiply uses PC at {address:#010x}"
            )));
        }
        let half = i64::from(signed_halfword(self.registers[rs], instruction & (1 << 6) != 0));
        // Rd = (Rm * half) >> 16, optionally accumulating Rn on top.
        let product = (i64::from(self.registers[rm] as i32) * half) >> 16;
        let result = if accumulate {
            product.wrapping_add(i64::from(self.registers[rn] as i32))
        } else {
            product
        };
        self.registers[rd] = result as u32;
        Ok(())
    }

    fn saturating_arithmetic(&mut self, instruction: u32, address: u32) -> Result<()> {
        let operation = ((instruction >> 21) & 3) as u8;
        let rn = ((instruction >> 16) & 0xf) as usize;
        let rd = ((instruction >> 12) & 0xf) as usize;
        let rm = (instruction & 0xf) as usize;
        if rd == 15 || rn == 15 || rm == 15 {
            return Err(Error::ArmFault(format!(
                "saturating arithmetic uses PC at {address:#010x}"
            )));
        }
        let left = i64::from(self.registers[rm] as i32);
        let right = i64::from(self.registers[rn] as i32);
        let (doubled, doubled_saturated) = if operation >= 2 {
            saturate_to_i32(right * 2)
        } else {
            (right as i32, false)
        };
        let sum = if operation & 1 == 0 {
            left + i64::from(doubled)
        } else {
            left - i64::from(doubled)
        };
        let (result, saturated) = saturate_to_i32(sum);
        if doubled_saturated || saturated {
            self.saturation = true;
        }
        self.registers[rd] = result as u32;
        Ok(())
    }

    fn multiply_long(&mut self, instruction: u32, address: u32) -> Result<()> {
        let signed = instruction & (1 << 22) != 0;
        let accumulate = instruction & (1 << 21) != 0;
        let set_flags = instruction & (1 << 20) != 0;
        let rd_hi = ((instruction >> 16) & 0xf) as usize;
        let rd_lo = ((instruction >> 12) & 0xf) as usize;
        let rs = ((instruction >> 8) & 0xf) as usize;
        let rm = (instruction & 0xf) as usize;
        if rd_hi == 15 || rd_lo == 15 || rs == 15 || rm == 15 || rd_hi == rd_lo {
            return Err(Error::ArmFault(format!(
                "long multiply has invalid registers at {address:#010x}"
            )));
        }

        let mut result = if signed {
            i64::from(self.registers[rm] as i32).wrapping_mul(i64::from(self.registers[rs] as i32))
                as u64
        } else {
            u64::from(self.registers[rm]).wrapping_mul(u64::from(self.registers[rs]))
        };
        if accumulate {
            let accumulator =
                (u64::from(self.registers[rd_hi]) << 32) | u64::from(self.registers[rd_lo]);
            result = result.wrapping_add(accumulator);
        }
        self.registers[rd_lo] = result as u32;
        self.registers[rd_hi] = (result >> 32) as u32;
        if set_flags {
            self.negative = result & (1_u64 << 63) != 0;
            self.zero = result == 0;
        }
        Ok(())
    }

    fn single_transfer(
        &mut self,
        memory: &mut GuestMemory,
        instruction: u32,
        address: u32,
    ) -> Result<()> {
        let register_offset = instruction & (1 << 25) != 0;
        let pre_index = instruction & (1 << 24) != 0;
        let add = instruction & (1 << 23) != 0;
        let byte = instruction & (1 << 22) != 0;
        let write_back = instruction & (1 << 21) != 0;
        let load = instruction & (1 << 20) != 0;
        let rn = ((instruction >> 16) & 0xf) as usize;
        let rd = ((instruction >> 12) & 0xf) as usize;
        let base = self.read_arm_register(rn, address);
        let offset = if register_offset {
            if instruction & (1 << 4) != 0 {
                return Err(self.unsupported_arm(instruction, address));
            }
            let rm = (instruction & 0xf) as usize;
            let value = self.read_arm_register(rm, address);
            let shift_type = ((instruction >> 5) & 3) as u8;
            let amount = (instruction >> 7) & 0x1f;
            shift_immediate(value, shift_type, amount, self.carry).0
        } else {
            instruction & 0xfff
        };
        let adjusted = if add {
            base.wrapping_add(offset)
        } else {
            base.wrapping_sub(offset)
        };
        let transfer_address = if pre_index { adjusted } else { base };

        if load {
            let value = if byte {
                u32::from(self.read_data_byte(memory, GuestAddr(transfer_address))?)
            } else {
                self.read_data_word(memory, GuestAddr(transfer_address))?
            };
            if rd == 15 {
                self.set_pc(value);
            } else {
                self.registers[rd] = value;
            }
        } else {
            let value = if rd == 15 {
                address.wrapping_add(12)
            } else {
                self.registers[rd]
            };
            if byte {
                self.write_data_byte(memory, GuestAddr(transfer_address), value as u8)?;
            } else {
                self.write_data_word(memory, GuestAddr(transfer_address), value)?;
            }
        }
        if !pre_index || write_back {
            if rn == 15 {
                return Err(Error::ArmFault(format!(
                    "load/store writes back PC at {address:#010x}"
                )));
            }
            self.registers[rn] = adjusted;
        }
        Ok(())
    }

    fn halfword_transfer(
        &mut self,
        memory: &mut GuestMemory,
        instruction: u32,
        address: u32,
    ) -> Result<()> {
        let pre_index = instruction & (1 << 24) != 0;
        let add = instruction & (1 << 23) != 0;
        let immediate = instruction & (1 << 22) != 0;
        let write_back = instruction & (1 << 21) != 0;
        let load = instruction & (1 << 20) != 0;
        let rn = ((instruction >> 16) & 0xf) as usize;
        let rd = ((instruction >> 12) & 0xf) as usize;
        let operation = ((instruction >> 5) & 3) as u8;
        let base = self.read_arm_register(rn, address);
        let offset = if immediate {
            ((instruction >> 4) & 0xf0) | (instruction & 0xf)
        } else {
            self.read_arm_register((instruction & 0xf) as usize, address)
        };
        let adjusted = if add {
            base.wrapping_add(offset)
        } else {
            base.wrapping_sub(offset)
        };
        let transfer_address = if pre_index { adjusted } else { base };
        if load {
            let value = match operation {
                1 => u32::from(self.read_data_halfword(memory, GuestAddr(transfer_address))?),
                2 => i32::from(self.read_data_byte(memory, GuestAddr(transfer_address))? as i8)
                    as u32,
                3 => self.read_signed_halfword(memory, GuestAddr(transfer_address))?,
                _ => return Err(self.unsupported_arm(instruction, address)),
            };
            if rd == 15 {
                self.set_pc(value);
            } else {
                self.registers[rd] = value;
            }
        } else if operation == 1 {
            self.write_data_halfword(
                memory,
                GuestAddr(transfer_address),
                self.registers[rd] as u16,
            )?;
        } else {
            return Err(self.unsupported_arm(instruction, address));
        }
        if !pre_index || write_back {
            self.registers[rn] = adjusted;
        }
        Ok(())
    }

    fn swap(
        &mut self,
        memory: &mut GuestMemory,
        instruction: u32,
        address: u32,
    ) -> Result<()> {
        let byte = instruction & (1 << 22) != 0;
        let rn = ((instruction >> 16) & 0xf) as usize;
        let rt = ((instruction >> 12) & 0xf) as usize;
        let rm = (instruction & 0xf) as usize;
        if rn == 15 || rt == 15 || rm == 15 {
            return Err(Error::ArmFault(format!(
                "swap uses PC at {address:#010x}"
            )));
        }
        let target = GuestAddr(self.registers[rn]);
        let loaded = if byte {
            u32::from(self.read_data_byte(memory, target)?)
        } else {
            self.read_data_word(memory, target)?
        };
        let stored = self.registers[rm];
        if byte {
            self.write_data_byte(memory, target, stored as u8)?;
        } else {
            self.write_data_word(memory, target, stored)?;
        }
        self.registers[rt] = loaded;
        Ok(())
    }

    fn doubleword_transfer(
        &mut self,
        memory: &mut GuestMemory,
        instruction: u32,
        address: u32,
        load: bool,
    ) -> Result<()> {
        let pre_index = instruction & (1 << 24) != 0;
        let add = instruction & (1 << 23) != 0;
        let immediate = instruction & (1 << 22) != 0;
        let write_back = instruction & (1 << 21) != 0;
        let rn = ((instruction >> 16) & 0xf) as usize;
        let rt = ((instruction >> 12) & 0xf) as usize;
        if rt >= 14 {
            return Err(Error::ArmFault(format!(
                "doubleword transfer uses PC pair at {address:#010x}"
            )));
        }
        let base = self.read_arm_register(rn, address);
        let offset = if immediate {
            ((instruction >> 4) & 0xf0) | (instruction & 0xf)
        } else {
            self.read_arm_register((instruction & 0xf) as usize, address)
        };
        let adjusted = if add {
            base.wrapping_add(offset)
        } else {
            base.wrapping_sub(offset)
        };
        let transfer_address = if pre_index { adjusted } else { base };
        if load {
            self.registers[rt] = self.read_data_word(memory, GuestAddr(transfer_address))?;
            self.registers[rt + 1] =
                self.read_data_word(memory, GuestAddr(transfer_address.wrapping_add(4)))?;
        } else {
            self.write_data_word(memory, GuestAddr(transfer_address), self.registers[rt])?;
            self.write_data_word(
                memory,
                GuestAddr(transfer_address.wrapping_add(4)),
                self.registers[rt + 1],
            )?;
        }
        if !pre_index || write_back {
            if rn == 15 {
                return Err(Error::ArmFault(format!(
                    "doubleword transfer writes back PC at {address:#010x}"
                )));
            }
            self.registers[rn] = adjusted;
        }
        Ok(())
    }

    fn block_transfer(
        &mut self,
        memory: &mut GuestMemory,
        instruction: u32,
        address: u32,
    ) -> Result<()> {
        let pre_index = instruction & (1 << 24) != 0;
        let add = instruction & (1 << 23) != 0;
        let user_mode = instruction & (1 << 22) != 0;
        let write_back = instruction & (1 << 21) != 0;
        let load = instruction & (1 << 20) != 0;
        let rn = ((instruction >> 16) & 0xf) as usize;
        let register_list = (instruction & 0xffff) as u16;
        if user_mode || register_list == 0 || rn == 15 {
            return Err(self.unsupported_arm(instruction, address));
        }
        let count = register_list.count_ones();
        let base = self.registers[rn];
        let mut transfer_address = if add {
            base.wrapping_add(if pre_index { 4 } else { 0 })
        } else {
            base.wrapping_sub(4 * count)
                .wrapping_add(if pre_index { 0 } else { 4 })
        };
        let mut loaded_pc = None;
        for register in 0..16 {
            if register_list & (1 << register) == 0 {
                continue;
            }
            if load {
                let value = memory.read_u32(GuestAddr(transfer_address))?;
                if register == 15 {
                    loaded_pc = Some(value);
                } else {
                    self.registers[register] = value;
                }
            } else {
                let value = if register == 15 {
                    address.wrapping_add(12)
                } else {
                    self.registers[register]
                };
                memory.write_u32(GuestAddr(transfer_address), value)?;
            }
            transfer_address = transfer_address.wrapping_add(4);
        }
        if write_back {
            self.registers[rn] = if add {
                base.wrapping_add(4 * count)
            } else {
                base.wrapping_sub(4 * count)
            };
        }
        if let Some(pc) = loaded_pc {
            self.set_pc(pc);
        }
        Ok(())
    }

    fn step_thumb(&mut self, memory: &mut GuestMemory) -> Result<()> {
        let address = self.registers[15];
        if address & 1 != 0 {
            return Err(Error::ArmFault(format!(
                "unaligned Thumb PC {address:#010x}"
            )));
        }
        let instruction = memory.fetch_u16(GuestAddr(address))?;
        self.registers[15] = address.wrapping_add(2);

        match instruction & 0xf800 {
            0x0000 | 0x0800 | 0x1000 => {
                let operation = ((instruction >> 11) & 3) as u8;
                let amount = u32::from((instruction >> 6) & 0x1f);
                let source = usize::from((instruction >> 3) & 7);
                let destination = usize::from(instruction & 7);
                let (result, carry) =
                    shift_immediate(self.registers[source], operation, amount, self.carry);
                self.registers[destination] = result;
                self.set_nz(result);
                self.carry = carry;
                Ok(())
            }
            0x1800 => self.thumb_add_sub(instruction),
            0x2000 | 0x2800 | 0x3000 | 0x3800 => self.thumb_immediate(instruction),
            0x4800 => {
                let destination = usize::from((instruction >> 8) & 7);
                let offset = u32::from(instruction & 0xff) * 4;
                let base = address.wrapping_add(4) & !3;
                self.registers[destination] =
                    memory.read_u32(GuestAddr(base.wrapping_add(offset)))?;
                Ok(())
            }
            0x6000 | 0x6800 | 0x7000 | 0x7800 => self.thumb_immediate_transfer(memory, instruction),
            0x8000 | 0x8800 => self.thumb_halfword_transfer(memory, instruction),
            0x9000 | 0x9800 => self.thumb_sp_transfer(memory, instruction),
            0xa000 | 0xa800 => {
                let destination = usize::from((instruction >> 8) & 7);
                let base = if instruction & (1 << 11) != 0 {
                    self.registers[13]
                } else {
                    address.wrapping_add(4) & !3
                };
                self.registers[destination] = base.wrapping_add(u32::from(instruction & 0xff) * 4);
                Ok(())
            }
            0xc000 | 0xc800 => self.thumb_multiple_transfer(memory, instruction),
            0xe000 => {
                let offset = sign_extend(u32::from(instruction & 0x7ff) << 1, 12);
                self.registers[15] = address.wrapping_add(4).wrapping_add_signed(offset) & !1;
                Ok(())
            }
            0xe800 => self.thumb_bl_suffix(instruction, address, false),
            0xf000 => {
                let offset = sign_extend(u32::from(instruction & 0x7ff) << 12, 23);
                self.registers[14] = address.wrapping_add(4).wrapping_add_signed(offset);
                Ok(())
            }
            0xf800 => self.thumb_bl_suffix(instruction, address, true),
            _ => match instruction & 0xfc00 {
                0x4000 => self.thumb_alu(instruction),
                0x4400 => self.thumb_high_register(instruction, address),
                0x5000 | 0x5400 | 0x5800 | 0x5c00 => {
                    self.thumb_register_transfer(memory, instruction)
                }
                _ => match instruction & 0xff00 {
                    0xb000 => {
                        let offset = u32::from(instruction & 0x7f) * 4;
                        self.registers[13] = if instruction & (1 << 7) != 0 {
                            self.registers[13].wrapping_sub(offset)
                        } else {
                            self.registers[13].wrapping_add(offset)
                        };
                        Ok(())
                    }
                    0xb400 | 0xb500 | 0xbc00 | 0xbd00 => self.thumb_push_pop(memory, instruction),
                    0xd000..=0xde00 => {
                        let condition = ((instruction >> 8) & 0xf) as u8;
                        if self.condition_passed(condition) {
                            let offset = sign_extend(u32::from(instruction & 0xff) << 1, 9);
                            self.registers[15] =
                                address.wrapping_add(4).wrapping_add_signed(offset) & !1;
                        }
                        Ok(())
                    }
                    0xdf00 if instruction == 0xdfab && self.registers[0] == 3 => {
                        // The verified Thumb semihosting character-write form uses
                        // operation 3 with r1 pointing at the byte to consume.
                        memory.read_u8(GuestAddr(self.registers[1]))?;
                        Ok(())
                    }
                    _ => Err(self.unsupported_thumb(instruction, address)),
                },
            },
        }
    }

    fn thumb_add_sub(&mut self, instruction: u16) -> Result<()> {
        let immediate = instruction & (1 << 10) != 0;
        let subtract = instruction & (1 << 9) != 0;
        let operand = usize::from((instruction >> 6) & 7);
        let source = usize::from((instruction >> 3) & 7);
        let destination = usize::from(instruction & 7);
        let right = if immediate {
            operand as u32
        } else {
            self.registers[operand]
        };
        let flags = if subtract {
            add_with_carry(self.registers[source], !right, true)
        } else {
            add_with_carry(self.registers[source], right, false)
        };
        self.registers[destination] = flags.0;
        self.set_arithmetic_flags(flags);
        Ok(())
    }

    fn thumb_immediate(&mut self, instruction: u16) -> Result<()> {
        let operation = ((instruction >> 11) & 3) as u8;
        let destination = usize::from((instruction >> 8) & 7);
        let immediate = u32::from(instruction & 0xff);
        match operation {
            0 => {
                self.registers[destination] = immediate;
                self.set_nz(immediate);
            }
            1 => self.set_arithmetic_flags(add_with_carry(
                self.registers[destination],
                !immediate,
                true,
            )),
            2 => {
                let flags = add_with_carry(self.registers[destination], immediate, false);
                self.registers[destination] = flags.0;
                self.set_arithmetic_flags(flags);
            }
            3 => {
                let flags = add_with_carry(self.registers[destination], !immediate, true);
                self.registers[destination] = flags.0;
                self.set_arithmetic_flags(flags);
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    fn thumb_alu(&mut self, instruction: u16) -> Result<()> {
        let operation = ((instruction >> 6) & 0xf) as u8;
        let source = usize::from((instruction >> 3) & 7);
        let destination = usize::from(instruction & 7);
        let left = self.registers[destination];
        let right = self.registers[source];
        let mut write_result = true;
        let mut arithmetic = None;
        let mut shifter_carry = None;
        let result = match operation {
            0 => left & right,
            1 => left ^ right,
            2..=4 | 7 => {
                let shift_type = match operation {
                    2 => 0,
                    3 => 1,
                    4 => 2,
                    _ => 3,
                };
                let (result, carry) = shift_register(left, shift_type, right & 0xff, self.carry);
                shifter_carry = Some(carry);
                result
            }
            5 => {
                let flags = add_with_carry(left, right, self.carry);
                arithmetic = Some(flags);
                flags.0
            }
            6 => {
                let flags = add_with_carry(left, !right, self.carry);
                arithmetic = Some(flags);
                flags.0
            }
            8 => {
                write_result = false;
                left & right
            }
            9 => {
                let flags = add_with_carry(0, !right, true);
                arithmetic = Some(flags);
                flags.0
            }
            10 => {
                write_result = false;
                let flags = add_with_carry(left, !right, true);
                arithmetic = Some(flags);
                flags.0
            }
            11 => {
                write_result = false;
                let flags = add_with_carry(left, right, false);
                arithmetic = Some(flags);
                flags.0
            }
            12 => left | right,
            13 => left.wrapping_mul(right),
            14 => left & !right,
            15 => !right,
            _ => unreachable!(),
        };
        if write_result {
            self.registers[destination] = result;
        }
        self.set_nz(result);
        if let Some(flags) = arithmetic {
            self.carry = flags.1;
            self.overflow = flags.2;
        } else if let Some(carry) = shifter_carry {
            self.carry = carry;
        }
        Ok(())
    }

    fn thumb_high_register(&mut self, instruction: u16, address: u32) -> Result<()> {
        let operation = ((instruction >> 8) & 3) as u8;
        let source =
            usize::from((instruction >> 3) & 7) | (usize::from(instruction & (1 << 6) != 0) << 3);
        let destination =
            usize::from(instruction & 7) | (usize::from(instruction & (1 << 7) != 0) << 3);
        let right = self.read_thumb_register(source, address);
        match operation {
            0 => {
                let result = self
                    .read_thumb_register(destination, address)
                    .wrapping_add(right);
                self.write_thumb_register(destination, result);
            }
            1 => self.set_arithmetic_flags(add_with_carry(
                self.read_thumb_register(destination, address),
                !right,
                true,
            )),
            2 => self.write_thumb_register(destination, right),
            3 => {
                if instruction & (1 << 7) != 0 {
                    self.registers[14] = address.wrapping_add(2) | 1;
                }
                self.set_pc(right);
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    fn thumb_register_transfer(
        &mut self,
        memory: &mut GuestMemory,
        instruction: u16,
    ) -> Result<()> {
        let operation = ((instruction >> 9) & 7) as u8;
        let offset = self.registers[usize::from((instruction >> 6) & 7)];
        let base = self.registers[usize::from((instruction >> 3) & 7)];
        let destination = usize::from(instruction & 7);
        let address = GuestAddr(base.wrapping_add(offset));
        match operation {
            0 => self.write_data_word(memory, address, self.registers[destination])?,
            1 => self.write_data_halfword(memory, address, self.registers[destination] as u16)?,
            2 => self.write_data_byte(memory, address, self.registers[destination] as u8)?,
            3 => {
                self.registers[destination] =
                    i32::from(self.read_data_byte(memory, address)? as i8) as u32
            }
            4 => self.registers[destination] = self.read_data_word(memory, address)?,
            5 => self.registers[destination] = u32::from(self.read_data_halfword(memory, address)?),
            6 => self.registers[destination] = u32::from(self.read_data_byte(memory, address)?),
            7 => self.registers[destination] = self.read_signed_halfword(memory, address)?,
            _ => unreachable!(),
        }
        Ok(())
    }

    fn thumb_immediate_transfer(
        &mut self,
        memory: &mut GuestMemory,
        instruction: u16,
    ) -> Result<()> {
        let byte = instruction & (1 << 12) != 0;
        let load = instruction & (1 << 11) != 0;
        let immediate = u32::from((instruction >> 6) & 0x1f);
        let base = self.registers[usize::from((instruction >> 3) & 7)];
        let destination = usize::from(instruction & 7);
        let offset = if byte { immediate } else { immediate * 4 };
        let address = GuestAddr(base.wrapping_add(offset));
        match (load, byte) {
            (false, false) => self.write_data_word(memory, address, self.registers[destination])?,
            (false, true) => {
                self.write_data_byte(memory, address, self.registers[destination] as u8)?
            }
            (true, false) => self.registers[destination] = self.read_data_word(memory, address)?,
            (true, true) => {
                self.registers[destination] = u32::from(self.read_data_byte(memory, address)?)
            }
        }
        Ok(())
    }

    fn thumb_halfword_transfer(
        &mut self,
        memory: &mut GuestMemory,
        instruction: u16,
    ) -> Result<()> {
        let load = instruction & (1 << 11) != 0;
        let offset = u32::from((instruction >> 6) & 0x1f) * 2;
        let base = self.registers[usize::from((instruction >> 3) & 7)];
        let destination = usize::from(instruction & 7);
        let address = GuestAddr(base.wrapping_add(offset));
        if load {
            self.registers[destination] = u32::from(self.read_data_halfword(memory, address)?);
        } else {
            self.write_data_halfword(memory, address, self.registers[destination] as u16)?;
        }
        Ok(())
    }

    fn thumb_sp_transfer(&mut self, memory: &mut GuestMemory, instruction: u16) -> Result<()> {
        let load = instruction & (1 << 11) != 0;
        let destination = usize::from((instruction >> 8) & 7);
        let address = GuestAddr(self.registers[13].wrapping_add(u32::from(instruction & 0xff) * 4));
        if load {
            self.registers[destination] = memory.read_u32(address)?;
        } else {
            memory.write_u32(address, self.registers[destination])?;
        }
        Ok(())
    }

    fn thumb_push_pop(&mut self, memory: &mut GuestMemory, instruction: u16) -> Result<()> {
        let pop = instruction & (1 << 11) != 0;
        let extra = instruction & (1 << 8) != 0;
        let register_list = instruction & 0xff;
        let count = register_list.count_ones() + u32::from(extra);
        if count == 0 {
            return Err(Error::ArmFault("empty Thumb push/pop register list".into()));
        }
        if pop {
            let mut address = self.registers[13];
            for register in 0..8 {
                if register_list & (1 << register) != 0 {
                    self.registers[register] = memory.read_u32(GuestAddr(address))?;
                    address = address.wrapping_add(4);
                }
            }
            let loaded_pc = if extra {
                let value = memory.read_u32(GuestAddr(address))?;
                address = address.wrapping_add(4);
                Some(value)
            } else {
                None
            };
            self.registers[13] = address;
            if let Some(pc) = loaded_pc {
                self.set_pc(pc);
            }
        } else {
            let start = self.registers[13].wrapping_sub(count * 4);
            let mut address = start;
            for register in 0..8 {
                if register_list & (1 << register) != 0 {
                    memory.write_u32(GuestAddr(address), self.registers[register])?;
                    address = address.wrapping_add(4);
                }
            }
            if extra {
                memory.write_u32(GuestAddr(address), self.registers[14])?;
            }
            self.registers[13] = start;
        }
        Ok(())
    }

    fn thumb_multiple_transfer(
        &mut self,
        memory: &mut GuestMemory,
        instruction: u16,
    ) -> Result<()> {
        let load = instruction & (1 << 11) != 0;
        let base_register = usize::from((instruction >> 8) & 7);
        let register_list = instruction & 0xff;
        if register_list == 0 {
            return Err(Error::ArmFault(
                "empty Thumb multiple-transfer register list".into(),
            ));
        }
        let base = self.registers[base_register];
        let mut address = base;
        for register in 0..8 {
            if register_list & (1 << register) == 0 {
                continue;
            }
            if load {
                self.registers[register] = memory.read_u32(GuestAddr(address))?;
            } else {
                memory.write_u32(GuestAddr(address), self.registers[register])?;
            }
            address = address.wrapping_add(4);
        }
        // Thumb LDMIA suppresses writeback when the base register is also in
        // the load list. The loaded value must remain visible in that case.
        if !load || register_list & (1 << base_register) == 0 {
            self.registers[base_register] =
                base.wrapping_add(register_list.count_ones().wrapping_mul(4));
        }
        Ok(())
    }

    fn thumb_bl_suffix(&mut self, instruction: u16, address: u32, stay_thumb: bool) -> Result<()> {
        let target = self.registers[14].wrapping_add(u32::from(instruction & 0x7ff) << 1);
        self.registers[14] = address.wrapping_add(2) | 1;
        if stay_thumb {
            self.thumb = true;
            self.registers[15] = target & !1;
        } else {
            self.thumb = false;
            self.registers[15] = target & !3;
        }
        Ok(())
    }

    fn read_thumb_register(&self, index: usize, instruction_address: u32) -> u32 {
        if index == 15 {
            instruction_address.wrapping_add(4)
        } else {
            self.registers[index]
        }
    }

    fn read_signed_halfword(&self, memory: &GuestMemory, address: GuestAddr) -> Result<u32> {
        Ok(i32::from(self.read_data_halfword(memory, address)? as i16) as u32)
    }

    /// Legacy reentrant code resolves its writable-data root through the null
    /// page: `LDR ip,[sb]` loads the module's parent/root slot (0 when the
    /// module owns its data), then `LDR ip,[ip]` reads the root static base
    /// from address 0. While the null scratch cell at 0..4 has never been
    /// written by the guest, reads return the current static base (r9), which
    /// is what the hosting platform maintains at the root slot. A guest store
    /// to the cell overrides the root with the stored value.
    /// The fallback only models the first cell (accesses fully inside bytes
    /// 0..4): the second cell keeps the plain stored-scratch semantics.
    fn legacy_null_root_bytes(&self, offset: usize, len: usize) -> Option<u32> {
        if offset + len > 4 {
            return None;
        }
        let data = self.legacy_null_data.as_ref()?;
        if data[0..4].iter().any(|byte| *byte != 0) {
            return None;
        }
        let root = self.registers[9];
        Some(
            (root >> (offset * 8))
                & match len {
                    1 => 0xff,
                    2 => 0xffff,
                    _ => 0xffff_ffff,
                },
        )
    }

    fn read_data_byte(&self, memory: &GuestMemory, address: GuestAddr) -> Result<u8> {
        if let Some(offset) = self.legacy_null_data_offset(memory, address, 1) {
            if let Some(value) = self.legacy_null_root_bytes(offset, 1) {
                return Ok(value as u8);
            }
            return Ok(self.legacy_null_data.as_ref().unwrap()[offset]);
        }
        memory.read_u8(address)
    }

    fn write_data_byte(
        &mut self,
        memory: &mut GuestMemory,
        address: GuestAddr,
        value: u8,
    ) -> Result<()> {
        if let Some(offset) = self.legacy_null_data_offset(memory, address, 1) {
            self.legacy_null_data.as_mut().unwrap()[offset] = value;
            return Ok(());
        }
        memory.write_u8(address, value)
    }

    fn read_data_halfword(&self, memory: &GuestMemory, address: GuestAddr) -> Result<u16> {
        if let Some(offset) = self.legacy_null_data_offset(memory, address, 2) {
            if let Some(value) = self.legacy_null_root_bytes(offset, 2) {
                return Ok(value as u16);
            }
            let data = self.legacy_null_data.as_ref().unwrap();
            return Ok(u16::from_le_bytes([data[offset], data[offset + 1]]));
        }
        memory.read_u16(address)
    }

    fn write_data_halfword(
        &mut self,
        memory: &mut GuestMemory,
        address: GuestAddr,
        value: u16,
    ) -> Result<()> {
        if let Some(offset) = self.legacy_null_data_offset(memory, address, 2) {
            self.legacy_null_data.as_mut().unwrap()[offset..offset + 2]
                .copy_from_slice(&value.to_le_bytes());
            return Ok(());
        }
        memory.write_u16(address, value)
    }

    fn read_data_word(&self, memory: &GuestMemory, address: GuestAddr) -> Result<u32> {
        if let Some(offset) = self.legacy_null_data_offset(memory, address, 4) {
            if let Some(value) = self.legacy_null_root_bytes(offset, 4) {
                return Ok(value);
            }
            let data = self.legacy_null_data.as_ref().unwrap();
            return Ok(u32::from_le_bytes(
                data[offset..offset + 4].try_into().unwrap(),
            ));
        }
        memory.read_u32(address)
    }

    fn write_data_word(
        &mut self,
        memory: &mut GuestMemory,
        address: GuestAddr,
        value: u32,
    ) -> Result<()> {
        if let Some(offset) = self.legacy_null_data_offset(memory, address, 4) {
            self.legacy_null_data.as_mut().unwrap()[offset..offset + 4]
                .copy_from_slice(&value.to_le_bytes());
            return Ok(());
        }
        memory.write_u32(address, value)
    }

    fn legacy_null_data_offset(
        &self,
        memory: &GuestMemory,
        address: GuestAddr,
        len: u32,
    ) -> Option<usize> {
        self.legacy_null_data.as_ref()?;
        address
            .0
            .checked_add(len)
            .filter(|end| *end <= LEGACY_NULL_DATA_LEN)
            .filter(|_| !memory.is_mapped(address, len as usize))?;
        Some(address.0 as usize)
    }

    fn write_thumb_register(&mut self, index: usize, value: u32) {
        if index == 15 {
            self.thumb = true;
            self.registers[15] = value & !1;
        } else {
            self.registers[index] = value;
        }
    }

    fn set_nz(&mut self, result: u32) {
        self.negative = result & 0x8000_0000 != 0;
        self.zero = result == 0;
    }

    fn set_arithmetic_flags(&mut self, flags: (u32, bool, bool)) {
        self.set_nz(flags.0);
        self.carry = flags.1;
        self.overflow = flags.2;
    }

    fn unsupported_thumb(&self, instruction: u16, address: u32) -> Error {
        Error::ArmFault(format!(
            "unsupported Thumb-1 instruction {instruction:#06x} at PC {address:#010x}; cpsr={:#010x}",
            self.cpsr()
        ))
    }

    fn read_arm_register(&self, index: usize, instruction_address: u32) -> u32 {
        if index == 15 {
            instruction_address.wrapping_add(8)
        } else {
            self.registers[index]
        }
    }

    fn write_arm_register(&mut self, index: usize, value: u32) {
        if index == 15 {
            // ARMv5T interworking: an ALU result written to PC switches to
            // Thumb when bit 0 is set, matching BX / set_pc semantics.
            self.thumb = value & 1 != 0;
            self.registers[15] = if self.thumb {
                value & !1
            } else {
                value & !3
            };
        } else {
            self.registers[index] = value;
        }
    }

    /// Apply an MSR field-mask write. Only the flag (f) field is modelled;
    /// control/extension/status fields are rejected so mode changes cannot
    /// silently disappear.
    fn write_cpsr_fields(
        &mut self,
        field_mask: u8,
        value: u32,
        instruction: u32,
        address: u32,
    ) -> Result<()> {
        if field_mask & 0x8 != 0 {
            self.negative = value & 0x8000_0000 != 0;
            self.zero = value & 0x4000_0000 != 0;
            self.carry = value & 0x2000_0000 != 0;
            self.overflow = value & 0x1000_0000 != 0;
            // The sticky Q (saturation) flag shares the flags byte.
            self.saturation = value & 0x0800_0000 != 0;
        }
        if field_mask & 0x7 != 0 {
            return Err(self.unsupported_arm(instruction, address));
        }
        Ok(())
    }

    fn condition_passed(&self, condition: u8) -> bool {
        match condition {
            0x0 => self.zero,
            0x1 => !self.zero,
            0x2 => self.carry,
            0x3 => !self.carry,
            0x4 => self.negative,
            0x5 => !self.negative,
            0x6 => self.overflow,
            0x7 => !self.overflow,
            0x8 => self.carry && !self.zero,
            0x9 => !self.carry || self.zero,
            0xa => self.negative == self.overflow,
            0xb => self.negative != self.overflow,
            0xc => !self.zero && self.negative == self.overflow,
            0xd => self.zero || self.negative != self.overflow,
            0xe => true,
            _ => false,
        }
    }

    fn unsupported_arm(&self, instruction: u32, address: u32) -> Error {
        Error::ArmFault(format!(
            "unsupported A32 instruction {instruction:#010x} at PC {address:#010x}; cpsr={:#010x}",
            self.cpsr()
        ))
    }
}

fn add_with_carry(left: u32, right: u32, carry: bool) -> (u32, bool, bool) {
    let unsigned = u64::from(left) + u64::from(right) + u64::from(carry);
    let result = unsigned as u32;
    let signed = i64::from(left as i32) + i64::from(right as i32) + i64::from(carry);
    (
        result,
        unsigned > u64::from(u32::MAX),
        signed < i64::from(i32::MIN) || signed > i64::from(i32::MAX),
    )
}

fn saturate_to_i32(value: i64) -> (i32, bool) {
    if value > i64::from(i32::MAX) {
        (i32::MAX, true)
    } else if value < i64::from(i32::MIN) {
        (i32::MIN, true)
    } else {
        (value as i32, false)
    }
}

fn signed_halfword(value: u32, top: bool) -> i16 {
    let halfword = if top { value >> 16 } else { value };
    halfword as u16 as i16
}

fn shift_immediate(value: u32, shift_type: u8, amount: u32, carry: bool) -> (u32, bool) {
    match (shift_type, amount) {
        (0, 0) => (value, carry),
        (0, amount) => (value << amount, value & (1 << (32 - amount)) != 0),
        (1, 0) => (0, value & 0x8000_0000 != 0),
        (1, amount) => (value >> amount, value & (1 << (amount - 1)) != 0),
        (2, 0) => {
            let sign = value & 0x8000_0000 != 0;
            (if sign { u32::MAX } else { 0 }, sign)
        }
        (2, amount) => (
            ((value as i32) >> amount) as u32,
            value & (1 << (amount - 1)) != 0,
        ),
        (3, 0) => {
            let result = (u32::from(carry) << 31) | (value >> 1);
            (result, value & 1 != 0)
        }
        (3, amount) => {
            let result = value.rotate_right(amount);
            (result, result & 0x8000_0000 != 0)
        }
        _ => unreachable!(),
    }
}

fn shift_register(value: u32, shift_type: u8, amount: u32, carry: bool) -> (u32, bool) {
    if amount == 0 {
        return (value, carry);
    }
    match shift_type {
        0 if amount < 32 => (value << amount, value & (1 << (32 - amount)) != 0),
        0 if amount == 32 => (0, value & 1 != 0),
        0 => (0, false),
        1 if amount < 32 => (value >> amount, value & (1 << (amount - 1)) != 0),
        1 if amount == 32 => (0, value & 0x8000_0000 != 0),
        1 => (0, false),
        2 if amount < 32 => (
            ((value as i32) >> amount) as u32,
            value & (1 << (amount - 1)) != 0,
        ),
        2 => {
            let sign = value & 0x8000_0000 != 0;
            (if sign { u32::MAX } else { 0 }, sign)
        }
        3 => {
            let rotate = amount & 31;
            if rotate == 0 {
                (value, value & 0x8000_0000 != 0)
            } else {
                let result = value.rotate_right(rotate);
                (result, result & 0x8000_0000 != 0)
            }
        }
        _ => unreachable!(),
    }
}

fn sign_extend(value: u32, bits: u32) -> i32 {
    ((value << (32 - bits)) as i32) >> (32 - bits)
}

#[cfg(test)]
mod tests;
