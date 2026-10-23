use super::*;
use crate::Permissions;

fn code_memory(instructions: &[u32]) -> GuestMemory {
    let bytes = instructions
        .iter()
        .flat_map(|instruction| instruction.to_le_bytes())
        .collect();
    let mut memory = GuestMemory::new();
    memory
        .map_bytes(
            GuestAddr(0x1000),
            bytes,
            Permissions::READ_EXECUTE,
            "test code",
        )
        .unwrap();
    memory
        .map(
            GuestAddr(0x2000),
            0x100,
            Permissions::READ_WRITE,
            "test stack",
        )
        .unwrap();
    memory
}

fn thumb_code_memory(instructions: &[u16]) -> GuestMemory {
    let bytes = instructions
        .iter()
        .flat_map(|instruction| instruction.to_le_bytes())
        .collect();
    let mut memory = GuestMemory::new();
    memory
        .map_bytes(
            GuestAddr(0x1000),
            bytes,
            Permissions::READ_EXECUTE,
            "Thumb test code",
        )
        .unwrap();
    memory
}

#[test]
fn data_processing_updates_flags_and_honors_conditions() {
    let mut memory = code_memory(&[
        0xe3a0_0001,
        0xe280_0002,
        0xe350_0003,
        0x03a0_1007,
        0x13a0_1009,
    ]);
    let mut cpu = ArmCpu::new();
    cpu.set_pc(0x1000);
    for _ in 0..5 {
        cpu.step(&mut memory).unwrap();
    }
    assert_eq!(cpu.register(0), 3);
    assert_eq!(cpu.register(1), 7);
    assert!(cpu.zero);
}

#[test]
fn block_transfer_round_trips_a_push_and_pop() {
    let mut memory = code_memory(&[0xe3a0_3012, 0xe92d_4008, 0xe3a0_3000, 0xe8bd_4008]);
    let mut cpu = ArmCpu::new();
    cpu.set_pc(0x1000);
    cpu.set_register(13, 0x2100);
    cpu.set_register(14, 0x1234_5678);
    for _ in 0..4 {
        cpu.step(&mut memory).unwrap();
    }
    assert_eq!(cpu.register(3), 0x12);
    assert_eq!(cpu.register(14), 0x1234_5678);
    assert_eq!(cpu.register(13), 0x2100);
}

#[test]
fn long_multiply_supports_signed_results_and_accumulation() {
    let mut memory = code_memory(&[
        0xe0c1_c093, // smull r12, r1, r3, r0
        0xe0a5_4190, // umlal r4, r5, r0, r1
    ]);
    let mut cpu = ArmCpu::new();
    cpu.set_pc(0x1000);
    cpu.set_register(0, (-7_i32) as u32);
    cpu.set_register(3, 3);
    cpu.step(&mut memory).unwrap();
    assert_eq!(cpu.register(12), (-21_i32) as u32);
    assert_eq!(cpu.register(1), u32::MAX);

    cpu.set_register(0, u32::MAX);
    cpu.set_register(1, 2);
    cpu.set_register(4, 2);
    cpu.set_register(5, 1);
    cpu.step(&mut memory).unwrap();
    assert_eq!(cpu.register(4), 0);
    assert_eq!(cpu.register(5), 3);
}

#[test]
fn signed_halfword_multiply_selects_each_half_and_accumulates() {
    let mut memory = code_memory(&[
        0xe160_0584, // smulbb r0, r4, r5
        0xe161_05c4, // smulbt r1, r4, r5
        0xe162_05a4, // smultb r2, r4, r5
        0xe163_05e4, // smultt r3, r4, r5
        0xe100_6584, // smlabb r0, r4, r5, r6
    ]);
    let mut cpu = ArmCpu::new();
    cpu.set_pc(0x1000);
    cpu.set_register(4, 0xfff9_fffd); // top = -7, bottom = -3
    cpu.set_register(5, 0xfffe_0004); // top = -2, bottom = 4
    cpu.set_register(6, 20);

    for _ in 0..5 {
        cpu.step(&mut memory).unwrap();
    }

    assert_eq!(cpu.register(0), 8);
    assert_eq!(cpu.register(1), 6);
    assert_eq!(cpu.register(2), (-28_i32) as u32);
    assert_eq!(cpu.register(3), 14);
}

#[test]
fn signed_halfword_multiply_executes_gms_icon_offset_instruction() {
    let mut memory = code_memory(&[0xe160_008c]); // smulbb r0, r12, r0
    let mut cpu = ArmCpu::new();
    cpu.set_pc(0x1000);
    cpu.set_register(0, 22);
    cpu.set_register(12, 25);

    cpu.step(&mut memory).unwrap();

    assert_eq!(cpu.register(0), 550);
}

#[test]
fn thumb_arithmetic_and_conditional_branch_follow_pipeline_pc() {
    let mut memory = thumb_code_memory(&[
        0x2001, // movs r0, #1
        0x3002, // adds r0, #2
        0x2803, // cmp r0, #3
        0xd000, // beq to current PC + 4
        0x2109, // skipped
        0x2107, // movs r1, #7
    ]);
    let mut cpu = ArmCpu::new();
    cpu.set_pc(0x1001);
    for _ in 0..5 {
        cpu.step(&mut memory).unwrap();
    }
    assert!(cpu.is_thumb());
    assert_eq!(cpu.register(0), 3);
    assert_eq!(cpu.register(1), 7);
    assert_eq!(cpu.pc(), GuestAddr(0x100c));
}

#[test]
fn legacy_null_data_accesses_are_narrow_and_opt_in() {
    let mut strict_memory = code_memory(&[0xe590_1000]); // ldr r1, [r0]
    let mut strict_cpu = ArmCpu::new();
    strict_cpu.set_pc(0x1000);
    assert!(matches!(
        strict_cpu.step(&mut strict_memory),
        Err(Error::ArmFault(message)) if message.contains("unmapped guest access")
    ));

    let mut legacy_memory = code_memory(&[
        0xe590_1000, // ldr r1, [r0]
        0xe580_2000, // str r2, [r0]
        0xe590_3000, // ldr r3, [r0]
        0xe5d0_4000, // ldrb r4, [r0]
    ]);
    let mut legacy_cpu = ArmCpu::new();
    legacy_cpu.allow_legacy_null_data_accesses();
    legacy_cpu.set_pc(0x1000);
    legacy_cpu.set_register(2, 0x1234_5678);
    for _ in 0..4 {
        legacy_cpu.step(&mut legacy_memory).unwrap();
    }
    assert_eq!(legacy_cpu.register(1), 0);
    assert_eq!(legacy_cpu.register(3), 0x1234_5678);
    assert_eq!(legacy_cpu.register(4), 0x78);
    assert!(!legacy_memory.is_mapped(GuestAddr(0), 4));

    let instructions = [
        0x5e40, // ldrsh r0, [r0, r1]
        0x2102, // movs r1, #2
        0x5e40, // ldrsh r0, [r0, r1]
        0x2104, // movs r1, #4
        0x5e40, // ldrsh r0, [r0, r1]
    ];
    let mut halfword_memory = thumb_code_memory(&instructions);
    let mut halfword_cpu = ArmCpu::new();
    halfword_cpu.allow_legacy_null_data_accesses();
    halfword_cpu.set_pc(0x1001);
    for _ in 0..instructions.len() {
        halfword_cpu.step(&mut halfword_memory).unwrap();
    }
    assert_eq!(halfword_cpu.register(0), 0);

    let mut out_of_range_memory = code_memory(&[0xe590_1000]);
    let mut out_of_range_cpu = ArmCpu::new();
    out_of_range_cpu.allow_legacy_null_data_accesses();
    out_of_range_cpu.set_pc(0x1000);
    out_of_range_cpu.set_register(0, 8);
    assert!(matches!(
        out_of_range_cpu.step(&mut out_of_range_memory),
        Err(Error::ArmFault(message)) if message.contains("unmapped guest access")
    ));

    let mut protected_memory = code_memory(&[0xe590_1000]);
    protected_memory
        .map(
            GuestAddr(0),
            LEGACY_NULL_DATA_LEN as usize,
            Permissions::EXECUTE,
            "protected low data",
        )
        .unwrap();
    let mut protected_cpu = ArmCpu::new();
    protected_cpu.allow_legacy_null_data_accesses();
    protected_cpu.set_pc(0x1000);
    assert!(matches!(
        protected_cpu.step(&mut protected_memory),
        Err(Error::ArmFault(message)) if message.contains("permission fault")
    ));

    protected_cpu.set_pc(0x1000);
    protected_cpu.set_register(0, 0);
    let mut protected_store_memory = code_memory(&[0xe580_1000]); // str r1, [r0]
    protected_store_memory
        .map(
            GuestAddr(0),
            LEGACY_NULL_DATA_LEN as usize,
            Permissions::EXECUTE,
            "protected low data",
        )
        .unwrap();
    assert!(matches!(
        protected_cpu.step(&mut protected_store_memory),
        Err(Error::ArmFault(message)) if message.contains("permission fault")
    ));
}

#[test]
fn legacy_null_root_reads_mirror_r9_until_overwritten() {
    let mut memory = code_memory(&[
        0xe590_1000, // ldr r1, [r0]      -> r9 static base
        0xe5d0_2003, // ldrb r2, [r0, #3] -> top byte of r9
        0xe590_3000, // ldr r3, [r0]      -> still the r9 static base
        0xe580_4000, // str r4, [r0]      -> pin the root cell
        0xe590_5000, // ldr r5, [r0]      -> stored value wins
    ]);
    let mut cpu = ArmCpu::new();
    cpu.allow_legacy_null_data_accesses();
    cpu.set_pc(0x1000);
    cpu.set_register(9, 0x2000_205c);
    cpu.set_register(4, 0x1234_5678);
    for _ in 0..5 {
        cpu.step(&mut memory).unwrap();
    }
    assert_eq!(cpu.register(1), 0x2000_205c);
    assert_eq!(cpu.register(2), 0x20);
    assert_eq!(cpu.register(3), 0x2000_205c);
    assert_eq!(cpu.register(5), 0x1234_5678);
}

#[test]
fn legacy_null_root_fallback_covers_only_the_first_cell() {
    let instructions = [
        0x2104, // movs r1, #4
        0x5e40, // ldrsh r0, [r0, r1] -> second cell: stored scratch, not r9
    ];
    let mut memory = thumb_code_memory(&instructions);
    let mut cpu = ArmCpu::new();
    cpu.allow_legacy_null_data_accesses();
    cpu.set_pc(0x1001);
    cpu.set_register(9, 0x1234_5678);
    for _ in 0..instructions.len() {
        cpu.step(&mut memory).unwrap();
    }
    assert_eq!(cpu.register(0), 0);
}

#[test]
fn thumb_semihosting_character_write_validates_its_input() {
    let mut memory = thumb_code_memory(&[0xdfab, 0xdfab, 0xdf00]);
    memory
        .map(
            GuestAddr(0x2000),
            1,
            Permissions::READ_WRITE,
            "semihosting character",
        )
        .unwrap();
    memory.write_u8(GuestAddr(0x2000), b'x').unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_pc(0x1001);
    cpu.set_register(0, 3);
    cpu.set_register(1, 0x2000);

    cpu.step(&mut memory).unwrap();
    assert_eq!(cpu.pc(), GuestAddr(0x1002));

    cpu.set_register(1, 0);
    assert!(matches!(cpu.step(&mut memory), Err(Error::ArmFault(_))));

    cpu.set_pc(0x1005);
    cpu.set_register(1, 0x2000);
    assert!(
        matches!(cpu.step(&mut memory), Err(Error::ArmFault(message)) if message.contains("unsupported Thumb-1 instruction"))
    );
}

#[test]
fn arm_semihosting_exit_is_reported_and_other_forms_still_fault() {
    let mut memory = code_memory(&[0xef12_3456, 0xef12_3455, 0xef12_3456]);
    let mut cpu = ArmCpu::new();
    cpu.set_pc(0x1000);
    cpu.set_register(0, 0x18);
    cpu.set_register(1, 0x0002_0026);

    cpu.step(&mut memory).unwrap();
    assert_eq!(cpu.take_semihosting_exit_reason(), Some(0x0002_0026));
    assert_eq!(cpu.take_semihosting_exit_reason(), None);

    assert!(matches!(
        cpu.step(&mut memory),
        Err(Error::ArmFault(message)) if message.contains("unsupported A32 instruction")
    ));

    cpu.set_register(0, 0x17);
    assert!(matches!(
        cpu.step(&mut memory),
        Err(Error::ArmFault(message)) if message.contains("unsupported A32 instruction")
    ));
}

#[test]
fn clz_counts_leading_zeros_without_touching_flags() {
    // 0xe16fcf11 = clz ip, r1 (the exact encoding from the guest's __aeabi_i2d)
    let mut memory = code_memory(&[
        0xe16f_cf11, // clz ip, r1
        0xe16f_2f10, // clz r2, r0
        0x016f_3f10, // clzeq r3, r0 (cond=EQ)
    ]);
    let mut cpu = ArmCpu::new();
    cpu.set_pc(0x1000);
    cpu.set_register(1, 1);
    cpu.set_register(0, 0x0008_0000);
    cpu.step(&mut memory).unwrap();
    assert_eq!(cpu.register(12), 31);
    cpu.step(&mut memory).unwrap();
    assert_eq!(cpu.register(2), 12);

    // Zero input yields 32; the conditional form executes when Z is set.
    cpu.set_register(1, 0);
    cpu.set_register(0, 0);
    cpu.zero = true;
    cpu.set_pc(0x1008);
    cpu.step(&mut memory).unwrap();
    assert_eq!(cpu.register(3), 32);
}

#[test]
fn clz_supports_the_zero_operand() {
    let mut memory = code_memory(&[0xe16f_cf10]); // clz ip, r0
    let mut cpu = ArmCpu::new();
    cpu.set_pc(0x1000);
    cpu.set_register(0, 0xffff_ffff);
    cpu.step(&mut memory).unwrap();
    assert_eq!(cpu.register(12), 0);
}

#[test]
fn mrs_reads_the_current_program_status() {
    // 0xe10f2000 = mrs r2, apsr (the exact encoding from the guest)
    // 0xe3a03080 = mov r3, #0x80 (sets N, clears Z)
    let mut memory = code_memory(&[0xe10f_2000, 0xe10f_0000]);
    let mut cpu = ArmCpu::new();
    cpu.set_pc(0x1000);
    cpu.step(&mut memory).unwrap();
    assert_eq!(cpu.register(2), cpu.cpsr());

    cpu.set_pc(0x1004);
    cpu.step(&mut memory).unwrap();
    assert_eq!(cpu.register(0), cpu.cpsr());
}

#[test]
fn msr_updates_flags_and_rejects_control_field_writes() {
    // 0xe128f008 = msr apsr_nzcvq, r8 (flags-only, the guest's encoding)
    // 0xe121f008 = msr cpsr_c, r8 (control field: unsupported)
    let mut memory = code_memory(&[0xe128_f008, 0xe121_f008]);
    let mut cpu = ArmCpu::new();
    cpu.set_pc(0x1000);
    cpu.set_register(8, 0x8000_0000); // N=1, Z/C/V=0
    cpu.step(&mut memory).unwrap();
    assert!(cpu.negative);
    assert!(!cpu.zero);

    cpu.set_pc(0x1004);
    assert!(
        matches!(cpu.step(&mut memory), Err(Error::ArmFault(message)) if message.contains("unsupported A32 instruction"))
    );
}

#[test]
fn msr_immediate_form_updates_flags() {
    // 0xe328f480 = msr apsr_nzcvq, #0x80000000 (0x80 ror 8): N=1, Z/C/V=0
    let mut memory = code_memory(&[0xe328_f480]);
    let mut cpu = ArmCpu::new();
    cpu.set_pc(0x1000);
    cpu.carry = true;
    cpu.step(&mut memory).unwrap();
    assert!(cpu.negative);
    assert!(!cpu.zero);
    assert!(!cpu.carry);
    assert!(!cpu.overflow);
}

#[test]
fn bkpt_encoding_is_rejected_loudly() {
    // 0xe1200070 = bkpt #0
    let mut memory = code_memory(&[0xe120_0070]);
    let mut cpu = ArmCpu::new();
    cpu.set_pc(0x1000);
    assert!(
        matches!(cpu.step(&mut memory), Err(Error::ArmFault(message)) if message.contains("unsupported A32 instruction"))
    );
}

#[test]
fn qadd_family_saturates_and_tracks_the_sticky_q_flag() {
    // 0xe1020051 = qadd r0, r1, r2; 0xe1220051 = qsub r0, r1, r2
    // 0xe1420051 = qdadd r0, r1, r2; 0xe1620051 = qdsub r0, r1, r2
    let mut memory = code_memory(&[
        0xe102_0051,
        0xe122_0051,
        0xe142_0051,
        0xe162_0051,
        0xe10f_3000, // mrs r3, apsr
        0xe128_f004, // msr apsr_nzcvq, r4
        0xe10f_5000, // mrs r5, apsr
    ]);
    let mut cpu = ArmCpu::new();
    cpu.set_pc(0x1000);
    cpu.set_register(1, 0x7fff_ffff);
    cpu.set_register(2, 1);
    cpu.step(&mut memory).unwrap();
    assert_eq!(cpu.register(0), 0x7fff_ffff); // saturates high
    assert!(cpu.saturation);

    cpu.set_register(1, 5);
    cpu.set_register(2, 3);
    cpu.step(&mut memory).unwrap();
    assert_eq!(cpu.register(0), 2); // no saturation here
    assert!(cpu.saturation); // Q is sticky

    cpu.set_register(1, 0x7fff_ffff);
    cpu.set_register(2, 0x4000_0000);
    cpu.step(&mut memory).unwrap(); // qdadd: doubling and sum both saturate high
    assert_eq!(cpu.register(0), 0x7fff_ffff);

    cpu.set_register(1, 0);
    cpu.set_register(2, 0x7fff_ffff);
    cpu.step(&mut memory).unwrap(); // qdsub: doubling saturates, result in range
    assert_eq!(cpu.register(0), 0x8000_0001);
    assert!(cpu.saturation);

    cpu.step(&mut memory).unwrap(); // mrs r3, apsr
    assert!(cpu.register(3) & 0x0800_0000 != 0);

    cpu.set_register(4, 0);
    cpu.step(&mut memory).unwrap(); // msr clears the flags byte including Q
    cpu.step(&mut memory).unwrap(); // mrs r5, apsr
    assert!(cpu.register(5) & 0x0800_0000 == 0);
    assert!(!cpu.saturation);
}

#[test]
fn smlal_halfword_variants_accumulate_onto_the_register_pair() {
    // 0xe1410382 = smlalbb r0, r1, r2, r3 (RdLo=r0, RdHi=r1) etc.
    let mut memory = code_memory(&[
        0xe141_0382, // smlalbb r0, r1, r2, r3
        0xe141_03e2, // smlaltt r0, r1, r2, r3
        0xe141_03a2, // smlaltb r0, r1, r2, r3
        0xe141_03c2, // smlalbt r0, r1, r2, r3
        0xe141_03c2, // smlalbt r0, r1, r2, r3
    ]);
    let mut cpu = ArmCpu::new();
    cpu.set_pc(0x1000);
    cpu.set_register(2, 0xfff9_fffd); // top = -7, bottom = -3
    cpu.set_register(3, 0xfffe_0004); // top = -2, bottom = 4
    cpu.set_register(0, 5); // RdLo
    cpu.set_register(1, 0); // RdHi

    cpu.step(&mut memory).unwrap(); // (-3 * 4) + 5 = -7
    assert_eq!(cpu.register(0), (-7_i32) as u32);
    assert_eq!(cpu.register(1), 0xffff_ffff);

    cpu.step(&mut memory).unwrap(); // (-7 * -2) + (-7) = 7
    assert_eq!(cpu.register(0), 7);
    assert_eq!(cpu.register(1), 0);

    cpu.step(&mut memory).unwrap(); // (-7 * 4) + 7 = -21
    assert_eq!(cpu.register(0), (-21_i32) as u32);
    assert_eq!(cpu.register(1), 0xffff_ffff);

    cpu.step(&mut memory).unwrap(); // (-3 * -2) + (-21) = -15
    assert_eq!(cpu.register(0), (-15_i32) as u32);
    assert_eq!(cpu.register(1), 0xffff_ffff);

    cpu.set_register(0, 0xffff_ffff);
    cpu.set_register(1, 2);
    cpu.set_pc(0x1010);
    cpu.step(&mut memory).unwrap(); // (-3 * -2) + 0x2_ffffffff = 0x3_00000005
    assert_eq!(cpu.register(0), 5);
    assert_eq!(cpu.register(1), 3);
}

#[test]
fn smulw_and_smlaw_multiply_word_by_halfword() {
    // 0xe12002a1 = smulwb r0, r1, r2; 0xe12002e1 = smulwt r0, r1, r2
    // 0xe1203281 = smlawb r0, r1, r2, r3; 0xe12032c1 = smlawt r0, r1, r2, r3
    let mut memory = code_memory(&[
        0xe120_02a1,
        0xe120_02e1,
        0xe120_3281,
        0xe120_32c1,
    ]);
    let mut cpu = ArmCpu::new();
    cpu.set_pc(0x1000);
    cpu.set_register(1, 0x0001_0000); // 65536
    cpu.set_register(2, 0xfffe_0004); // top = -2, bottom = 4
    cpu.set_register(3, 10);

    cpu.step(&mut memory).unwrap(); // (65536 * 4) >> 16 = 4
    assert_eq!(cpu.register(0), 4);
    cpu.step(&mut memory).unwrap(); // (65536 * -2) >> 16 = -2
    assert_eq!(cpu.register(0), (-2_i32) as u32);
    cpu.step(&mut memory).unwrap(); // 4 + 10 = 14
    assert_eq!(cpu.register(0), 14);
    cpu.step(&mut memory).unwrap(); // -2 + 10 = 8
    assert_eq!(cpu.register(0), 8);
}

#[test]
fn swap_exchanges_a_register_with_memory() {
    // 0xe1012094 = swp r2, r4, [r1]; 0xe1412094 = swpb r2, r4, [r1]
    let mut memory = code_memory(&[0xe101_2094, 0xe141_2094]);
    memory.write_u32(GuestAddr(0x2000), 0x1122_3344).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_pc(0x1000);
    cpu.set_register(1, 0x2000);
    cpu.set_register(4, 0x5566_7799);

    cpu.step(&mut memory).unwrap();
    assert_eq!(cpu.register(2), 0x1122_3344);
    assert_eq!(memory.read_u32(GuestAddr(0x2000)).unwrap(), 0x5566_7799);

    cpu.set_register(4, 0xaabb_ccdd);
    cpu.step(&mut memory).unwrap();
    assert_eq!(cpu.register(2), 0x99);
    assert_eq!(memory.read_u32(GuestAddr(0x2000)).unwrap(), 0x5566_77dd);
}

#[test]
fn ldrd_and_strd_move_doublewords_with_writeback() {
    // 0xe1c120d0 = ldrd r2, r3, [r1]; 0xe1c120f8 = strd r2, r3, [r1, #8]
    // 0xe1e120d8 = ldrd r2, r3, [r1, #8]!; 0xe00120d0 = ldrd r2, r3, [r1], -r0
    let mut memory = code_memory(&[
        0xe1c1_20d0,
        0xe1c1_20f8,
        0xe1e1_20d8,
        0xe001_20d0,
    ]);
    memory.write_u32(GuestAddr(0x2000), 0x1111_1111).unwrap();
    memory.write_u32(GuestAddr(0x2004), 0x2222_2222).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_pc(0x1000);
    cpu.set_register(1, 0x2000);
    cpu.set_register(0, 8);

    cpu.step(&mut memory).unwrap(); // ldrd, no writeback
    assert_eq!(cpu.register(2), 0x1111_1111);
    assert_eq!(cpu.register(3), 0x2222_2222);
    assert_eq!(cpu.register(1), 0x2000);

    cpu.step(&mut memory).unwrap(); // strd [r1, #8]
    assert_eq!(memory.read_u32(GuestAddr(0x2008)).unwrap(), 0x1111_1111);
    assert_eq!(memory.read_u32(GuestAddr(0x200c)).unwrap(), 0x2222_2222);
    assert_eq!(cpu.register(1), 0x2000);

    cpu.step(&mut memory).unwrap(); // pre-indexed writeback
    assert_eq!(cpu.register(2), 0x1111_1111);
    assert_eq!(cpu.register(3), 0x2222_2222);
    assert_eq!(cpu.register(1), 0x2008);

    cpu.step(&mut memory).unwrap(); // post-indexed register: r1 -= r0
    assert_eq!(cpu.register(1), 0x2000);
    assert_eq!(cpu.register(2), 0x1111_1111);
    assert_eq!(cpu.register(3), 0x2222_2222);
}

#[test]
fn preload_hints_execute_as_nops() {
    // 0xf5d0f000 = pld [r0, #4]; 0xf590f004 = pldw [r0, #4]
    // 0xf7d0f004 = pld [r0, r4]; 0xf750f004 = pld [r0, -r4]
    let mut memory = code_memory(&[
        0xf5d0_f000,
        0xf590_f004,
        0xf7d0_f004,
        0xf750_f004,
    ]);
    let mut cpu = ArmCpu::new();
    cpu.set_pc(0x1000);
    cpu.set_register(0, 0xdead_beef);
    for _ in 0..4 {
        cpu.step(&mut memory).unwrap();
    }
    assert_eq!(cpu.register(0), 0xdead_beef);
    assert_eq!(cpu.pc(), GuestAddr(0x1010));
}

#[test]
fn bxj_degrades_to_bx_without_jazelle() {
    // 0xe12fff24 = bxj r4
    let mut memory = code_memory(&[0xe12f_ff24]);
    let mut cpu = ArmCpu::new();
    cpu.set_pc(0x1000);
    cpu.set_register(4, 0x2001);
    cpu.step(&mut memory).unwrap();
    assert!(cpu.is_thumb());
    assert_eq!(cpu.pc(), GuestAddr(0x2000));

    let mut cpu = ArmCpu::new();
    cpu.set_pc(0x1000);
    cpu.set_register(4, 0x2000);
    cpu.step(&mut memory).unwrap();
    assert!(!cpu.is_thumb());
    assert_eq!(cpu.pc(), GuestAddr(0x2000));
}

#[test]
fn alu_writes_to_pc_interwork_with_thumb() {
    // 0xe1a0f000 = mov pc, r0
    let mut memory = code_memory(&[0xe1a0_f000, 0xe1a0_f000]);
    let mut cpu = ArmCpu::new();
    cpu.set_pc(0x1000);
    cpu.set_register(0, 0x1009);
    cpu.step(&mut memory).unwrap();
    assert!(cpu.is_thumb());
    assert_eq!(cpu.pc(), GuestAddr(0x1008));

    cpu.set_pc(0x1004);
    cpu.set_register(0, 0x2000);
    cpu.step(&mut memory).unwrap();
    assert!(!cpu.is_thumb());
    assert_eq!(cpu.pc(), GuestAddr(0x2000));
}

#[test]
fn clz_rejects_pc_operands() {
    let mut memory = code_memory(&[0xe16f_cf1f]); // clz ip, pc
    let mut cpu = ArmCpu::new();
    cpu.set_pc(0x1000);
    assert!(matches!(cpu.step(&mut memory), Err(Error::ArmFault(_))));
}

#[test]
fn thumb_ldmia_preserves_a_loaded_base_register() {
    let mut memory = thumb_code_memory(&[
        0xca07, // ldmia r2!, {r0, r1, r2}
        0xca03, // ldmia r2!, {r0, r1}
    ]);
    memory
        .map(
            GuestAddr(0x2000),
            0x20,
            Permissions::READ_WRITE,
            "Thumb test data",
        )
        .unwrap();
    memory.write_u32(GuestAddr(0x2000), 0x1111_1111).unwrap();
    memory.write_u32(GuestAddr(0x2004), 0x2222_2222).unwrap();
    memory.write_u32(GuestAddr(0x2008), 0x200c).unwrap();
    memory.write_u32(GuestAddr(0x200c), 0x3333_3333).unwrap();
    memory.write_u32(GuestAddr(0x2010), 0x4444_4444).unwrap();

    let mut cpu = ArmCpu::new();
    cpu.set_pc(0x1001);
    cpu.set_register(2, 0x2000);
    cpu.step(&mut memory).unwrap();
    assert_eq!(cpu.register(0), 0x1111_1111);
    assert_eq!(cpu.register(1), 0x2222_2222);
    assert_eq!(cpu.register(2), 0x200c);

    cpu.step(&mut memory).unwrap();
    assert_eq!(cpu.register(0), 0x3333_3333);
    assert_eq!(cpu.register(1), 0x4444_4444);
    assert_eq!(cpu.register(2), 0x2014);
}

#[test]
fn failed_step_rewinds_pc_so_hosts_can_retry_after_mapping_missing_memory() {
    // str r0, [r1] with r1 pointing at unmapped memory.
    let mut memory = code_memory(&[0xe581_0000]);
    let mut cpu = ArmCpu::new();
    cpu.set_pc(0x1000);
    cpu.set_register(0, 0x1234);
    cpu.set_register(1, 0x9000);
    assert!(cpu.step(&mut memory).is_err());
    // The faulting instruction must not be skipped on retry.
    assert_eq!(cpu.pc().0, 0x1000);

    // A host fallback can map the missing window and re-execute the store.
    memory
        .map(GuestAddr(0x9000), 8, Permissions::READ_WRITE, "fallback window")
        .unwrap();
    cpu.step(&mut memory).unwrap();
    assert_eq!(memory.read_u32(GuestAddr(0x9000)).unwrap(), 0x1234);
    assert_eq!(cpu.pc().0, 0x1004);
}

#[test]
fn failed_thumb_step_rewinds_pc_to_the_faulting_halfword() {
    // strb r1, [r2, r0] with r2 pointing at unmapped memory.
    let mut memory = thumb_code_memory(&[0x5411]);
    let mut cpu = ArmCpu::new();
    cpu.set_pc(0x1001); // Thumb
    cpu.set_register(0, 0);
    cpu.set_register(1, 0x5a);
    cpu.set_register(2, 0x9000);
    assert!(cpu.step(&mut memory).is_err());
    assert_eq!(cpu.pc().0, 0x1000);

    memory
        .map(GuestAddr(0x9000), 8, Permissions::READ_WRITE, "fallback window")
        .unwrap();
    cpu.step(&mut memory).unwrap();
    assert_eq!(memory.read(GuestAddr(0x9000), 1).unwrap(), [0x5a]);
    assert_eq!(cpu.pc().0, 0x1002);
}

#[test]
#[ignore = "manual performance probe"]
fn bench_step_mips() {
    let iterations: u32 = std::env::var("BENCH_ITERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(20_000_000);
    let cases: &[(&str, &[u32], u32, u32)] = &[
        // (name, instructions, countdown register, instructions per iteration)
        (
            "alu_loop(sub/cmp/bne)",
            &[0xe250_0001, 0xe350_0000, 0x1aff_fffc],
            0,
            3,
        ),
        (
            "mem_loop(ldr/add/str/sub/cmp/bne)",
            &[
                0xe792_1003, // ldr r1, [r2, r3]
                0xe081_1101, // add r1, r1, r1, lsl #2
                0xe782_1003, // str r1, [r2, r3]
                0xe250_0001, // sub r0, r0, #1
                0xe350_0000, // cmp r0, #0
                0x1aff_fff9, // bne loop
            ],
            0,
            6,
        ),
        (
            "imm_loop(mov/add/cmp/bne)",
            &[
                0xe3a0_1005, // mov r1, #5
                0xe281_1001, // add r1, r1, #1
                0xe351_0000, // cmp r1, #0
                0xe250_0001, // sub r0, r0, #1
                0x1aff_fffa, // bne loop
            ],
            0,
            5,
        ),
    ];
    for (name, code, countdown, per_iteration) in cases {
        let mut memory = GuestMemory::new();
        let bytes: Vec<u8> = code
            .iter()
            .flat_map(|instruction| instruction.to_le_bytes())
            .collect();
        memory
            .map_bytes(
                GuestAddr(0x1000_0000),
                bytes,
                Permissions::READ_EXECUTE,
                "bench code",
            )
            .unwrap();
        memory
            .map(
                GuestAddr(0x2000_0000),
                0x10_0000,
                Permissions::READ_WRITE,
                "bench data",
            )
            .unwrap();
        let mut cpu = ArmCpu::new();
        cpu.set_pc(0x1000_0000);
        cpu.set_register(*countdown as usize, iterations);
        cpu.set_register(2, 0x2000_0000);
        cpu.set_register(3, 0x1000);
        let total = u64::from(iterations) * u64::from(*per_iteration);
        let start = std::time::Instant::now();
        for _ in 0..total {
            cpu.step(&mut memory).unwrap();
        }
        let elapsed = start.elapsed();
        let mips = total as f64 / elapsed.as_nanos() as f64 * 1000.0;
        println!("[bench] {name}: {mips:.1} MIPS ({elapsed:?} for {total} steps)");
    }
    // Thumb-mode probes: the CMAP engine executes Thumb, so measure the same
    // tight loops through step_thumb.
    let thumb_cases: &[(&str, &[u16], u32)] = &[
        // (name, instructions, instructions per iteration)
        (
            "thumb_alu_loop(subs/bne)",
            &[0x3801, 0xd1fd], // subs r0,#1; bne loop
            2,
        ),
        (
            "thumb_mem_loop(ldr/adds/str/subs/bne)",
            &[
                0x58d1, // ldr r1, [r2, r3]
                0x1849, // adds r1, r1, r1
                0x50d1, // str r1, [r2, r3]
                0x3801, // subs r0, #1
                0xd1fa, // bne loop
            ],
            5,
        ),
        (
            "thumb_stack_loop(push/pop/subs/bne)",
            &[
                0xb4f0, // push {r4-r7}
                0xbcf0, // pop {r4-r7}
                0x3801, // subs r0, #1
                0xd1fb, // bne loop
            ],
            4,
        ),
    ];
    for (name, code, per_iteration) in thumb_cases {
        let mut memory = GuestMemory::new();
        let bytes: Vec<u8> = code
            .iter()
            .flat_map(|instruction| instruction.to_le_bytes())
            .collect();
        memory
            .map_bytes(
                GuestAddr(0x1000_0000),
                bytes,
                Permissions::READ_EXECUTE,
                "bench thumb code",
            )
            .unwrap();
        memory
            .map(
                GuestAddr(0x2000_0000),
                0x10_0000,
                Permissions::READ_WRITE,
                "bench thumb data",
            )
            .unwrap();
        let mut cpu = ArmCpu::new();
        cpu.set_pc(0x1000_0001);
        cpu.set_register(0, iterations);
        cpu.set_register(2, 0x2000_0000);
        cpu.set_register(3, 0x1000);
        cpu.set_register(13, 0x2000_8000);
        let total = u64::from(iterations) * u64::from(*per_iteration);
        let start = std::time::Instant::now();
        for _ in 0..total {
            cpu.step(&mut memory).unwrap();
        }
        let elapsed = start.elapsed();
        let mips = total as f64 / elapsed.as_nanos() as f64 * 1000.0;
        println!("[bench] {name}: {mips:.1} MIPS ({elapsed:?} for {total} steps)");
    }
}
