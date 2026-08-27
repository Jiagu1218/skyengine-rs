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
    pub semihosting_exit_reason: Option<u32>,
    /// 未映射/越权访问的故障描述；在下一次 step 出口转为 `Err`。
    pub fault: Option<String>,
}

/// 借用式的客户机总线。生命周期限制在一次 step 会话内，
/// 内核持有它而主循环保留对 `GuestMemory` 的独占所有权。
pub(crate) struct GuestBus<'a> {
    pub(crate) memory: &'a mut GuestMemory,
    pub(crate) state: GuestBusState,
}

impl<'a> GuestBus<'a> {
    pub(crate) fn new(memory: &'a mut GuestMemory) -> GuestBus<'a> {
        GuestBus {
            memory,
            state: GuestBusState::default(),
        }
    }

    fn record_fault(&mut self, address: u32, kind: &str, detail: &str) {
        if self.state.fault.is_none() {
            self.state.fault = Some(format!("guest bus {kind} at {address:#010x}: {detail}"));
        }
    }
}

impl MemoryInterface for GuestBus<'_> {
    fn load_8(&mut self, addr: u32, _access: MemoryAccess) -> u8 {
        // 数据读路径无法与取指区分，X 页按可读对待（设计 D6）。
        match self.memory.read_u8(GuestAddr(addr)) {
            Ok(value) => value,
            Err(error) => {
                self.record_fault(addr, "read8", &error.to_string());
                0
            }
        }
    }

    fn load_16(&mut self, addr: u32, _access: MemoryAccess) -> u16 {
        let addr = addr & !1;
        match self.memory.read_u16(GuestAddr(addr)) {
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
            aligned >= BUS_TRAP_BASE && aligned < BUS_TRAP_BASE + BUS_TRAP_SLOT_COUNT * 4;
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
        match self.memory.read_u32(GuestAddr(aligned)) {
            Ok(value) => value,
            Err(error) => {
                self.record_fault(aligned, "read32", &error.to_string());
                0
            }
        }
    }

    fn store_8(&mut self, addr: u32, value: u8, _access: MemoryAccess) {
        if let Err(error) = self.memory.write_u8(GuestAddr(addr), value) {
            self.record_fault(addr, "write8", &error.to_string());
        }
    }

    fn store_16(&mut self, addr: u32, value: u16, _access: MemoryAccess) {
        let addr = addr & !1;
        if let Err(error) = self.memory.write_u16(GuestAddr(addr), value) {
            self.record_fault(addr, "write16", &error.to_string());
        }
    }

    fn store_32(&mut self, addr: u32, value: u32, _access: MemoryAccess) {
        let aligned = addr & !3;
        if let Err(error) = self.memory.write_u32(GuestAddr(aligned), value) {
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
