//! [skyengine] `GuestCpu` 门面：持久化 vendored `Arm7tdmiCore<GuestBus>`。
//!
//! ## 借用模型（v2 持久化）
//!
//! 旧版每步 `activate()` 重建 Arm7tdmiCore + 重载流水线，开销 13-21x。
//! 新版：
//! - `GuestCpu` 直接持有 `Arm7tdmiCore<GuestBus>`
//! - `GuestBus` 用裸指针 `*mut GuestMemory` 持有内存，无生命周期参数
//! - `bind_memory()` 在 step 前把 `&mut GuestMemory` 指针装入 bus
//! - `unbind_memory()` 在 step 后清空指针，释放独占借用
//! - `step()` 直接调 `core.step()`，无快照装载/回收、无流水线重载
//! - dispatch 读寄存器从 core 直接读（memory 未绑定时安全）

use super::bus::GuestBus;
use crate::arm::memory::GuestMemory;
use crate::{Error, Result};
use arm7tdmi::{Arm7tdmiCore, CpuState};

/// SWI 宿主消费标记：内核 hook 返回 true 时记录注释值，step 后由 runner 判定。
pub(crate) struct GuestCpu {
    core: Arm7tdmiCore<GuestBus>,
    /// 半宿主 SYS_EXIT 退出原因：step 出口判定后写入，跨 activate 持久。
    semihosting_exit_reason: Option<u32>,
    /// 取指命中 RETURN_SENTINEL 页（0xffffff00 起）。
    sentinel_hit: bool,
    /// memory 是否已绑定（step 期间为 true，dispatch 期间为 false）
    memory_bound: bool,
    /// [skyengine] 用户可见的 PC（与旧快照模型一致）。
    /// 旧模型 snapshot.pc = get_next_pc()；新模型 core.pc 带流水线偏移。
    /// 此字段在以下时机更新：
    /// - set_pc_static() 时设为 address & !alignment
    /// - step() 后由 get_next_pc() 更新
    /// - pc_static() 直接返回此字段
    visible_pc: u32,
}

impl GuestCpu {
    pub(crate) fn empty() -> Self {
        let bus = GuestBus::from_raw(std::ptr::null_mut());
        GuestCpu {
            core: Arm7tdmiCore::new(bus),
            semihosting_exit_reason: None,
            sentinel_hit: false,
            memory_bound: false,
            visible_pc: 0,
        }
    }

    /// 绑定内存到 bus 的裸指针。返回 &mut Self 供链式调用。
    /// 调用方必须保证在 unbind 之前没有其他引用访问同一 GuestMemory。
    pub(crate) fn bind_memory(&mut self, memory: &mut GuestMemory) {
        self.core.bus.rebind(memory as *mut GuestMemory);
        self.memory_bound = true;
    }

    /// 解绑内存指针。step 结束后必须调用，以释放对 GuestMemory 的逻辑独占。
    pub(crate) fn unbind_memory(&mut self) {
        // 把 bus 信号回收到外层字段，然后清空指针
        self.sentinel_hit = self.sentinel_hit || self.core.bus.state.pending_sentinel_hit;
        self.core.bus.state.pending_sentinel_hit = false;
        // pending_trap 保留在 bus.state 中，下次 bind 时不需要重新播种
        // 因为 core 持久化，bus 也持久化
        self.core.bus.memory = std::ptr::null_mut();
        self.memory_bound = false;
    }

    // ---- 寄存器读写（直接访问 core，无需 activate） ----

    pub(crate) fn register_static(&self, index: usize) -> u32 {
        debug_assert_ne!(index, 15);
        self.core.get_reg(index)
    }

    pub(crate) fn set_register_static(&mut self, index: usize, value: u32) {
        debug_assert_ne!(index, 15);
        self.core.set_reg(index, value);
    }

    pub(crate) fn pc_static(&self) -> u32 {
        self.visible_pc
    }

    pub(crate) fn cpsr_static(&self) -> u32 {
        let flags = self.core.cpsr.get() & 0xf000_0000;
        let thumb = u32::from(self.core.cpsr.state() == CpuState::THUMB) << 5;
        flags | thumb
    }

    pub(crate) fn is_thumb_static(&self) -> bool {
        self.core.cpsr.state() == CpuState::THUMB
    }

    pub(crate) fn is_sentinel_hit_static(&self) -> bool {
        self.sentinel_hit
    }

    pub(crate) fn clear_pending_trap(&mut self) {
        if std::env::var_os("SKYENGINE_DEBUG_LOOP").is_some() {
            eprintln!("[clear_trap] old={:?}", self.core.bus.state.pending_trap);
        }
        self.core.bus.state.pending_trap = None;
    }

    pub(crate) fn take_semihosting_exit_reason_static(&mut self) -> Option<u32> {
        self.semihosting_exit_reason.take()
    }

    pub(crate) fn set_pc_static(&mut self, address: u32) {
        let thumb = address & 1 != 0;
        let base = if thumb { address & !1 } else { address & !3 };
        self.visible_pc = base;
        self.core
            .cpsr
            .set_state(if thumb { CpuState::THUMB } else { CpuState::ARM });
        self.core.set_reg(15, base);
        self.core.bus.state.pending_trap = None;
        // 如果 memory 已绑定，则立即重载流水线
        if self.memory_bound && !self.core.bus.memory.is_null() {
            if thumb {
                self.core.reload_pipeline16();
            } else {
                self.core.reload_pipeline32();
            }
        }
    }

    /// 初始化 CPU 状态（首次进入函数前调用，替代旧的 activate+set_pc 模式）
    pub(crate) fn initialize(&mut self, pc: u32, sp: u32, lr: u32) {
        let thumb = pc & 1 != 0;
        let base_pc = if thumb { pc & !1 } else { pc & !3 };
        self.core.cpsr.set_state(if thumb { CpuState::THUMB } else { CpuState::ARM });
        self.core.cpsr.set_irq_disabled(true);
        self.core.cpsr.set_fiq_disabled(true);
        // 设置 mode 为 SVC (0x13) — 与旧核默认一致
        // CPSR 低 5 位 = mode
        let old_cpsr = self.core.cpsr.get();
        self.core.cpsr.set((old_cpsr & !0x1f) | 0x13);
        self.core.set_reg(13, sp);
        self.core.set_reg(14, lr);
        self.core.set_reg(15, base_pc);
        self.core.bus.state.pending_trap = None;
        // 流水线将在 bind_memory 后由首次 step 自动取指
    }

    /// 执行一条指令。
    /// - 陷阱窗口命中：返回 Ok(Some(slot))。
    /// - SWI 已被 bus hook 消费：在本步出口用实时寄存器完成半宿主判定，
    ///   命中 SYS_EXIT 则记录退出原因；未知 SWI 保持旧核的 fail-fast 错误。
    pub(crate) fn step(&mut self) -> Result<Option<u32>> {
        debug_assert!(self.memory_bound, "step() called without bind_memory()");

        if std::env::var_os("SKYENGINE_DEBUG_LOOP").is_some() {
            let insn = self.core.get_decoded_opcode();
            let is_thumb = self.core.cpsr.state() == CpuState::THUMB;
            let insn_fmt = if is_thumb { format!("{:04x}", insn & 0xFFFF) } else { format!("{:08x}", insn) };
            eprintln!("[step] exec insn={insn_fmt} pc={:#010x} thumb={is_thumb} r0={:#010x} r1={:#010x} r2={:#010x} r3={:#010x} lr={:#010x} sp={:#010x}", self.core.get_next_pc(), self.core.get_reg(0), self.core.get_reg(1), self.core.get_reg(2), self.core.get_reg(3), self.core.get_reg(14), self.core.get_reg(13));
        }
        self.core.step();
        // 更新可见 PC（与旧快照模型 save_back 中 get_next_pc() 一致）
        self.visible_pc = self.core.get_next_pc();
        let state = &mut self.core.bus.state;

        if let Some(comment) = state.pending_swi.take() {
            if comment == super::bus::SEMIHOSTING_SWI_COMMENT {
                if self.core.get_reg(0) == super::bus::SEMIHOSTING_SYS_EXIT {
                    self.semihosting_exit_reason = Some(self.core.get_reg(1));
                    return Ok(None);
                }
                return Err(Error::ArmFault(format!(
                    "unsupported semihosting SWI operation r0={:#010x}",
                    self.core.get_reg(0)
                )));
            }
            return Err(Error::ArmFault(format!(
                "unsupported A32 instruction (svc #{comment:#x})"
            )));
        }

        if let Some(fault) = state.fault.take() {
            return Err(Error::ArmFault(fault));
        }

        // 检查 sentinel
        if state.pending_sentinel_hit {
            self.sentinel_hit = true;
            state.pending_sentinel_hit = false;
        }

        Ok(state.pending_trap.take())
    }

    /// 兼容旧接口：激活态的 set_pc（用于 ext.rs dispatch 路径中设置返回地址）
    pub(crate) fn set_pc(&mut self, address: u32) {
        self.set_pc_static(address);
    }

    /// 兼容旧接口：取半宿主退出原因
    pub(crate) fn take_semihosting_exit_reason(&mut self) -> Option<u32> {
        self.semihosting_exit_reason.take()
    }

    /// 兼容旧接口：取陷阱槽位
    pub(crate) fn take_pending_trap(&mut self) -> Option<u32> {
        let slot = self.core.bus.state.pending_trap.take();
        if std::env::var_os("SKYENGINE_DEBUG_LOOP").is_some() {
            eprintln!("[take_trap] slot={slot:?}");
        }
        slot
    }

    /// 兼容旧接口：sentinel 命中检查
    pub(crate) fn is_sentinel_hit(&mut self) -> bool {
        let hit = self.sentinel_hit;
        if hit {
            self.sentinel_hit = false;
        }
        hit
    }

    /// 兼容旧接口：获取 bus memory 的可变引用
    pub(crate) fn bus_memory_mut(&mut self) -> Option<&mut GuestMemory> {
        if self.core.bus.memory.is_null() {
            None
        } else {
            Some(unsafe { &mut *self.core.bus.memory })
        }
    }
}
