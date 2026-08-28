//! [skyengine] 客户机内存总线：把 `GuestMemory` 的分区权限语义接到 vendored
//! arm7tdmi 内核的 `MemoryInterface` 上，并承接宿主集成的三类信号——
//! 陷阱窗口取指弹床、SWI 半宿主出口、以及 fail-fast 的访存故障。

use super::memory::{GuestAddr, GuestMemory};
use arm7tdmi::memory::{MemoryAccess, MemoryInterface};

/// 陷阱窗口基址；与 `ext.rs` 中 TRAP_BASE 保持一致（一致性由 ext 测试锚定）。
pub(crate) const BUS_TRAP_BASE: u32 = 0xff00_0000;
/// 平台槽位数量上限（同 ext.rs PLATFORM_SLOT_COUNT）。
pub(crate) const BUS_TRAP_SLOT_COUNT: u32 = 150;
/// 返回哨兵基址（同 ext.rs RETURN_SENTINEL）。
pub(crate) const RETURN_SENTINEL_BASE: u32 = 0xffff_ff00;

/// 与旧核 cpu.rs:96 相同的半宿主 SWI 注释值。
pub(crate) const SEMIHOSTING_SWI_COMMENT: u32 = 0x123456;
/// ARM semihosting SYS_EXIT 操作码（r0==0x18）。
pub(crate) const SEMIHOSTING_SYS_EXIT: u32 = 0x18;

/// 编码好的 `bx lr` 弹床指令字（条件 AL）。
const TRAMPOLINE_BX_LR: u32 = 0xe12f_ff1e;

/// 宿主集成信号袋：bus 写、runner 读。
#[derive(Debug, Default)]
pub(crate) struct GuestBusState {
    /// 取指命中 RETURN_SENTINEL（0xffff_ff00）：本步是 guest 返回边界。
    pub pending_sentinel_hit: bool,
    /// 本步发生了被宿主消费的 SWI，记录其注释值供 step 出口判定。
    pub pending_swi: Option<u32>,
    /// 取指落入陷阱窗口时记录的槽位（等待主循环派发）。
    pub pending_trap: Option<u32>,
    /// `swi #0x123456` 且 r0==0x18 时记录的退出原因。
    #[allow(dead_code)]
    pub semihosting_exit_reason: Option<u32>,
    /// 未映射/越权访问的故障描述；在下一次 step 出口转为 `Err`。
    pub fault: Option<String>,
}

/// [skyengine] 持久化客户机总线：用裸指针持有 GuestMemory，
/// 避免 Arm7tdmiCore 带生命周期参数，从而让 Core 可以持久存
/// 活在 GuestCpu 内而非每次 activate 重建。
///
/// ## 安全性
/// 裸指针的别名安全由调用方保证：每次 step 期间 GuestCpu 独占
/// &mut GuestMemory；dispatch 期间 GuestCpu 不活跃（core 不可
/// 调用 bus 方法）。这与旧版借用式 GuestBus 语义等价，只是绕
/// 过了 Rust 借用检查器的跨迭代生命周期限制。
pub(crate) struct GuestBus {
    pub(crate) memory: *mut GuestMemory,
    pub(crate) state: GuestBusState,
}

// 安全：GuestBus 的 MemoryInterface 实现仅通过 &mut self 访问
// *mut GuestMemory，且每次 step 期间调用方保证独占访问。
unsafe impl Send for GuestBus {}

impl GuestBus {
    /// 从可变引用构造。调用方必须保证在 bus 活跃期间
    /// 没有其他引用访问同一 GuestMemory。
    pub(crate) fn from_raw(memory: *mut GuestMemory) -> GuestBus {
        GuestBus {
            memory,
            state: GuestBusState::default(),
        }
    }

    /// 重置状态并更换底层内存指针（用于重新绑定到新的 &mut GuestMemory）
    pub(crate) fn rebind(&mut self, memory: *mut GuestMemory) {
        self.memory = memory;
        self.state = GuestBusState::default();
    }

    /// 获取内存的可变引用（仅在本 bus 独占期间安全调用）
    #[inline]
    fn memory_mut(&mut self) -> &mut GuestMemory {
        // 安全：调用方保证此期间独占 GuestMemory
        unsafe { &mut *self.memory }
    }

    /// 获取内存的共享引用
    #[inline]
    fn memory_ref(&self) -> &GuestMemory {
        // 安全：调用方保证此期间独占 GuestMemory
        unsafe { &*self.memory }
    }

    fn record_fault(&mut self, address: u32, kind: &str, detail: &str) {
        if self.state.fault.is_none() {
            self.state.fault = Some(format!("guest bus {kind} at {address:#010x}: {detail}"));
        }
    }
}

impl MemoryInterface for GuestBus {
    fn load_8(&mut self, addr: u32, _access: MemoryAccess) -> u8 {
        match self.memory_ref().read_u8(GuestAddr(addr)) {
            Ok(value) => value,
            Err(error) => {
                self.record_fault(addr, "read8", &error.to_string());
                0
            }
        }
    }

    fn load_16(&mut self, addr: u32, _access: MemoryAccess) -> u16 {
        let addr = addr & !1;
        // [skyengine] Thumb 取指也需要弹床拦截：当 guest 在 Thumb 模式下
        // bx lr 跳到 RETURN_SENTINEL 或落入陷阱窗口时，CPU 通过 load_16
        // 取指。必须与 load_32 一致地设置 pending 标记并返回有效指令。
        let in_trap_window =
            (BUS_TRAP_BASE..BUS_TRAP_BASE + BUS_TRAP_SLOT_COUNT * 4).contains(&(addr & !3));
        let is_sentinel = (addr & !3) >= 0xffff_ff00;
        if in_trap_window || is_sentinel {
            if in_trap_window {
                let slot = ((addr & !3) - BUS_TRAP_BASE) / 4;
                self.state.pending_trap = Some(slot);
            } else {
                self.state.pending_sentinel_hit = true;
            }
            // ARM `bx lr` 的低 16 位 = 0xFF1E，在 Thumb 解码空间里
            // 不合法，但下一步 step 出口会因 pending_sentinel_hit 而
            // 终止，不会真正执行。返回一个安全的 Thumb 指令：`bx lr`
            // (0x4770)。
            return 0x4770; // Thumb bx lr
        }
        match self.memory_ref().read_u16(GuestAddr(addr)) {
            Ok(value) => value,
            Err(error) => {
                self.record_fault(addr, "read16", &error.to_string());
                0
            }
        }
    }

    fn load_32(&mut self, addr: u32, _access: MemoryAccess) -> u32 {
        let aligned = addr & !3;
        // RETURN_SENTINEL（0xffff_ff00）与陷阱窗口同为弹床：取指返回 `bx lr`
        // 并把 SENTINEL 记为特殊槽位，使“执行到返回哨兵”表现为一条正常指令。
        // RUNNER 在 step 出口检查该标记并结束调用。
        let in_trap_window =
            (BUS_TRAP_BASE..BUS_TRAP_BASE + BUS_TRAP_SLOT_COUNT * 4).contains(&aligned);
        let is_sentinel = aligned >= 0xffff_ff00;
        let _ = RETURN_SENTINEL_BASE;
        if in_trap_window || is_sentinel {
            if in_trap_window {
                let slot = (aligned - BUS_TRAP_BASE) / 4;
                self.state.pending_trap = Some(slot);
            } else {
                self.state.pending_sentinel_hit = true;
            }
            return TRAMPOLINE_BX_LR;
        }
        match self.memory_ref().read_u32(GuestAddr(aligned)) {
            Ok(value) => value,
            Err(error) => {
                self.record_fault(aligned, "read32", &error.to_string());
                0
            }
        }
    }

    fn store_8(&mut self, addr: u32, value: u8, _access: MemoryAccess) {
        if let Err(error) = self.memory_mut().write_u8(GuestAddr(addr), value) {
            self.record_fault(addr, "write8", &error.to_string());
        }
    }

    fn store_16(&mut self, addr: u32, value: u16, _access: MemoryAccess) {
        let addr = addr & !1;
        if let Err(error) = self.memory_mut().write_u16(GuestAddr(addr), value) {
            self.record_fault(addr, "write16", &error.to_string());
        }
    }

    fn store_32(&mut self, addr: u32, value: u32, _access: MemoryAccess) {
        let aligned = addr & !3;
        if let Err(error) = self.memory_mut().write_u32(GuestAddr(aligned), value) {
            self.record_fault(aligned, "write32", &error.to_string());
        }
    }

    fn idle_cycle(&mut self) {}

    fn software_interrupt(&mut self, comment: u32) -> bool {
        // 宿主消费所有 SWI：半宿主出口（0x123456+r0=0x18）由 step 出口判定；
        // 其余值视作流水线继续（MRP guest 不使用 SVC 向量）。
        self.state.pending_swi = Some(comment);
        true
    }
}
