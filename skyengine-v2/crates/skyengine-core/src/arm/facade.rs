//! [skyengine] `GuestCpu` 门面：以旧自研 `ArmCpu` 的方法形状驱动 vendored
//! `Arm7tdmiCore<GuestBus>`。
//!
//! ## 借用模型
//!
//! 宿主服务（dispatch）需要 `&mut ExtRuntime`（memory + 服务状态），而内核
//! 执行需要 `&mut GuestMemory`。为让借用检查满足且不引入 UnsafeCell：
//!
//! - `GuestCpu` 在 **非激活态** 只保存寄存器/CPSR 的快照（值语义），
//!   供 dispatch/trace 自由读取；
//! - `activate(&mut memory)` 把快照装回 vendored 内核执行一步，
//!   `Drop` 时把内核最新状态写回快照并释放内存借用。
//!
//! 快照装载/回收每步各一次（约几十 ns），远小于指令本身的解释成本。

use super::bus::GuestBus;
use crate::arm::memory::GuestMemory;
use crate::{Error, Result};
use arm7tdmi::{Arm7tdmiCore, CpuState};

/// 非激活态的寄存器快照：runner 与宿主服务的共享读视图。
#[derive(Clone, Debug)]
struct CpuSnapshot {
    gpr: [u32; 15],
    pc: u32,
    thumb: bool,
    negative: bool,
    zero: bool,
    carry: bool,
    overflow: bool,
    irq_disabled: bool,
    fiq_disabled: bool,
    mode_raw: u32,
}

impl Default for CpuSnapshot {
    fn default() -> Self {
        // 复刻 arm7tdmi 默认 CPSR (0x0000_00D3)：SVC 态、IRQ/FIQ 关闭、ARM 态。
        CpuSnapshot {
            gpr: [0; 15],
            pc: 0,
            thumb: false,
            negative: false,
            zero: false,
            carry: false,
            overflow: false,
            irq_disabled: true,
            fiq_disabled: true,
            mode_raw: 0xd3 & 0x7f,
        }
    }
}

/// SWI 宿主消费标记：内核 hook 返回 true 时记录注释值，step 后由 runner 判定。
pub(crate) struct GuestCpu {
    snapshot: CpuSnapshot,
    /// 半宿主 SYS_EXIT 退出原因：step 出口判定后写入，跨 activate 持久。
    semihosting_exit_reason: Option<u32>,
    // ---- 总线信号跨 activate 持久化（bus 本体随每次激活重建） ----
    /// 陷阱窗口取指记录的槽位。
    pending_trap: Option<u32>,
    /// 取指命中 RETURN_SENTINEL 页（0xffffff00 起）。
    sentinel_hit: bool,
    /// 本步被宿主消费的 SWI 注释值。
    pending_swi: Option<u32>,
}

/// 激活期视图：装回了真实内核。Drop 时状态写回快照。
pub(crate) struct ActiveCpu<'a> {
    outer: &'a mut GuestCpu,
    core: Arm7tdmiCore<GuestBus<'a>>,
}

impl GuestCpu {
    pub(crate) fn empty() -> Self {
        GuestCpu {
            snapshot: CpuSnapshot::default(),
            semihosting_exit_reason: None,
            pending_trap: None,
            sentinel_hit: false,
            pending_swi: None,
        }
    }

    /// 非激活读视图（dispatch / trace / 错误上下文用）。
    pub(crate) fn register_static(&self, index: usize) -> u32 {
        debug_assert_ne!(index, 15);
        self.snapshot.gpr[index]
    }

    /// 非激活写视图（宿主服务写返回值 r0 等）。
    pub(crate) fn set_register_static(&mut self, index: usize, value: u32) {
        debug_assert_ne!(index, 15);
        self.snapshot.gpr[index] = value;
    }

    pub(crate) fn pc_static(&self) -> u32 {
        self.snapshot.pc
    }

    pub(crate) fn cpsr_static(&self) -> u32 {
        let mut raw = 0u32;
        if self.snapshot.negative {
            raw |= 1 << 31;
        }
        if self.snapshot.zero {
            raw |= 1 << 30;
        }
        if self.snapshot.carry {
            raw |= 1 << 29;
        }
        if self.snapshot.overflow {
            raw |= 1 << 28;
        }
        if self.snapshot.thumb {
            raw |= 1 << 5;
        }
        raw
    }

    pub(crate) fn is_thumb_static(&self) -> bool {
        self.snapshot.thumb
    }

    /// 非激活态写 PC：bit0 携带 Thumb 含义（与旧核 set_pc 一致）。
    pub(crate) fn set_pc_static(&mut self, address: u32) {
        self.snapshot.thumb = address & 1 != 0;
        if self.snapshot.thumb {
            self.snapshot.pc = address & !1;
        } else {
            self.snapshot.pc = address & !3;
        }
    }


    /// 装载内存引用进入激活态。结束时（作用域结束）状态写回快照。
    pub(crate) fn activate<'a>(&'a mut self, memory: &'a mut GuestMemory) -> ActiveCpu<'a> {
        let mut bus = GuestBus::new(memory);
        // 播种上一步遗留的总线信号（bus 随激活重建，信号必须外置持久）
        bus.state.pending_trap = self.pending_trap;
        bus.state.pending_sentinel_hit = self.sentinel_hit;
        bus.state.pending_swi = self.pending_swi;
        let mut core = Arm7tdmiCore::new(bus);

        // --- 恢复快照 ---
        core.gpr.copy_from_slice(&self.snapshot.gpr);
        core.set_reg(15, self.snapshot.pc);
        core.cpsr.set_N(self.snapshot.negative);
        core.cpsr.set_Z(self.snapshot.zero);
        core.cpsr.set_C(self.snapshot.carry);
        core.cpsr.set_V(self.snapshot.overflow);
        core.cpsr.set_irq_disabled(self.snapshot.irq_disabled);
        core.cpsr.set_fiq_disabled(self.snapshot.fiq_disabled);
        core.cpsr.set_state(if self.snapshot.thumb {
            CpuState::THUMB
        } else {
            CpuState::ARM
        });
        // 单一 User/System 场景；bank 视图与通用视图一致。
        core.banks.gpr_banked_r13[0] = self.snapshot.gpr[13];
        core.banks.gpr_banked_r14[0] = self.snapshot.gpr[14];
        // 流水线重取指：显式 reload 保证 pipeline 有效。
        if self.snapshot.thumb {
            core.reload_pipeline16();
        } else {
            core.reload_pipeline32();
        }

        ActiveCpu { outer: self, core }
    }
}

impl ActiveCpu<'_> {
    fn save_back(&mut self) {
        let core = &self.core;
        self.outer.snapshot.gpr.copy_from_slice(&core.gpr);
        // PC 折算：get_next_pc 是"下一条待执行地址"，与旧核 pc() 一致。
        self.outer.snapshot.pc = core.get_next_pc();
        self.outer.snapshot.thumb = core.cpsr.state() == CpuState::THUMB;
        self.outer.snapshot.negative = core.cpsr.N();
        self.outer.snapshot.zero = core.cpsr.Z();
        self.outer.snapshot.carry = core.cpsr.C();
        self.outer.snapshot.overflow = core.cpsr.V();
        self.outer.snapshot.irq_disabled = core.cpsr.irq_disabled();
        self.outer.snapshot.fiq_disabled = core.cpsr.fiq_disabled();
        self.outer.snapshot.mode_raw = core.cpsr.get() & 0x1f;
        // 未被 runner 消费的信号回收到外层，等待下一次 activate 播种回去。
        self.outer.pending_trap = self.core.bus.state.pending_trap.take().or(self.outer.pending_trap);
        self.outer.sentinel_hit =
            self.outer.sentinel_hit || self.core.bus.state.pending_sentinel_hit;
        self.outer.pending_swi = self.core.bus.state.pending_swi.take().or(self.outer.pending_swi);
    }

    pub(crate) fn bus_memory_mut(&mut self) -> &mut GuestMemory {
        self.core.bus.memory
    }

    pub(crate) fn register(&self, index: usize) -> u32 {
        debug_assert_ne!(index, 15);
        self.core.get_reg(index)
    }

    pub(crate) fn set_register(&mut self, index: usize, value: u32) {
        debug_assert_ne!(index, 15);
        if index == 15 {
            self.set_pc(value);
            return;
        }
        if (8..=14).contains(&index) {
            self.core.set_reg_user(index, value);
        }
        self.core.set_reg(index, value);
    }

    pub(crate) fn pc(&self) -> u32 {
        self.core.get_next_pc()
    }

    pub(crate) fn set_pc(&mut self, address: u32) {
        let thumb = address & 1 != 0;
        let base = if thumb { address & !1 } else { address & !3 };
        self.core
            .cpsr
            .set_state(if thumb { CpuState::THUMB } else { CpuState::ARM });
        self.core.set_reg(15, base);
        if thumb {
            self.core.reload_pipeline16();
        } else {
            self.core.reload_pipeline32();
        }
    }

    pub(crate) fn is_thumb(&self) -> bool {
        self.core.cpsr.state() == CpuState::THUMB
    }

    pub(crate) fn cpsr(&self) -> u32 {
        let flags = self.core.cpsr.get() & 0xf000_0000;
        let thumb = u32::from(self.is_thumb()) << 5;
        flags | thumb
    }

    pub(crate) fn is_sentinel_hit(&mut self) -> bool {
        let hit = self.core.bus.state.pending_sentinel_hit;
        self.core.bus.state.pending_sentinel_hit = false;
        if hit {
            self.outer.sentinel_hit = false;
        }
        hit
    }

    pub(crate) fn take_pending_trap(&mut self) -> Option<u32> {
        let slot = self.core.bus.state.pending_trap.take();
        if slot.is_some() {
            self.outer.pending_trap = None;
        }
        slot
    }

    pub(crate) fn take_semihosting_exit_reason(&mut self) -> Option<u32> {
        self.outer.semihosting_exit_reason.take()
    }

    /// 执行一条指令。
    /// - 陷阱窗口命中：返回 Ok(Some(slot))；
    /// - SWI 已被 bus hook 消费：在本步出口用实时寄存器完成半宿主判定，
    ///   命中 SYS_EXIT 则记录退出原因；未知 SWI 保持旧核的 fail-fast 错误。
    pub(crate) fn step(&mut self) -> Result<Option<u32>> {
        // 进入本步之前遗留的 SWI 判定已在上一步处理；先消费本步标记占位，
        // core.step() 之后统一读取。
        self.core.step();
        let state = &mut self.core.bus.state;

        if let Some(comment) = state.pending_swi.take() {
            self.outer.pending_swi = None;
            if comment == super::bus::SEMIHOSTING_SWI_COMMENT {
                if self.core.get_reg(0) == super::bus::SEMIHOSTING_SYS_EXIT {
                    self.outer.semihosting_exit_reason = Some(self.core.get_reg(1));
                    return Ok(None);
                }
                // 与旧核一致：0x123456 但 r0 不是 0x18 时报错而不是继续。
                return Err(Error::ArmFault(format!(
                    "unsupported semihosting SWI operation r0={:#010x}",
                    self.core.get_reg(0)
                )));
            }
            // 其余注释值按旧语义报错（MRP guest 不使用 SVC 向量）。
            return Err(Error::ArmFault(format!(
                "unsupported A32 instruction (svc #{comment:#x})"
            )));
        }

        if let Some(fault) = state.fault.take() {
            return Err(Error::ArmFault(fault));
        }
        Ok(state.pending_trap.take())
    }

}

impl Drop for ActiveCpu<'_> {
    fn drop(&mut self) {
        self.save_back();
    }
}


