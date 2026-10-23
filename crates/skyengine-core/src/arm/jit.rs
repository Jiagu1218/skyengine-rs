//! JIT dispatcher: bridges the interpreter loop in `ext` to translated Thumb
//! blocks from `skyengine-jit`.
//!
//! Ownership and aliasing model: the engine is single-threaded and
//! [`crate::arm::ExtRuntime`] moves after construction (it is returned by
//! value), so the state lives behind a `Box` in the runtime and the
//! write-barrier thread-local holds a raw pointer into that stable heap
//! allocation. The barrier stays installed for the cache's whole lifetime —
//! host-side writes between blocks (trap services such as guest memcpy or
//! memset) can overwrite translated code just like in-block helper writes,
//! and both funnel into `BlockCache::invalidate_written`.
//!
//! The translator backend only exists on x86_64 hosts. Everywhere else
//! [`JitState::try_new_boxed`] always returns `None`, so the engine stays
//! on the interpreter unchanged (the aarch64 backend is a later phase).

/// Result of one JIT dispatch attempt at the interpreter loop's head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockOutcome {
    /// The block ran to completion; `cpu` now holds the exit state. The
    /// payload is the number of guest instructions executed.
    Executed(u32),
    /// A memory helper faulted mid-block. `cpu` is synced with the faulting
    /// instruction's pc restored; the interpreter must re-execute that one
    /// instruction so the regular fault/fallback paths handle it.
    Faulted,
    /// The block starting here could not be translated (unsupported first
    /// instruction); interpret one step.
    Unsupported,
}

#[cfg(target_arch = "x86_64")]
mod imp {
    use std::cell::Cell;

    use skyengine_arm::{ArmCpu, GuestAddr, GuestMemory, set_write_barrier};
    use skyengine_jit::{
        BlockCache, BlockContext, BlockEntry, ExecMemory, probe_exec_memory, translate_thumb_block,
    };

    use super::BlockOutcome;

    /// Code arena size. Blocks are at most ~3 KiB of machine code, so this
    /// holds thousands of translations; when it fills the arena resets and
    /// the cache flushes wholesale (see [`JitState::publish`]).
    const ARENA_SIZE: usize = 4 << 20;

    /// Signature of a published block: the context pointer arrives in the
    /// first-argument register of the host calling convention (RCX on Win64,
    /// RDI on SysV), matching the translator's `abi::CTX`.
    type BlockFn = unsafe extern "C" fn(*mut BlockContext);

    // Thread-local bridge for the global write-barrier callback: points at
    // the owning engine's block cache, or null when no JIT is active on
    // this thread.
    thread_local! {
        static BARRIER_CACHE: Cell<*mut BlockCache> = const { Cell::new(std::ptr::null_mut()) };
    }

    /// Write-barrier callback: drop translated blocks whose guest code the
    /// successful write overlapped.
    fn write_barrier(start: u32, len: u32) {
        BARRIER_CACHE.with(|cell| {
            let pointer = cell.get();
            if !pointer.is_null() {
                // SAFETY: the pointer is installed by `JitState::install_barrier`
                // and removed in `Drop` before the cache dies; the engine is
                // single-threaded, so nothing else can observe it meanwhile.
                unsafe { (*pointer).invalidate_written(start, len) };
            }
        });
    }

    unsafe extern "C" fn helper_read8(context: *mut BlockContext, address: u32) -> u32 {
        let context = unsafe { &mut *context };
        // SAFETY: `ctx.mem` is refreshed by the dispatcher before every block
        // execution and points at the engine's `GuestMemory`.
        let memory = unsafe { &mut *(context.mem as *mut GuestMemory) };
        match memory.read_u8(GuestAddr(address)) {
            Ok(value) => u32::from(value),
            Err(_) => {
                context.fault = address;
                0
            }
        }
    }

    unsafe extern "C" fn helper_read16(context: *mut BlockContext, address: u32) -> u32 {
        let context = unsafe { &mut *context };
        let memory = unsafe { &mut *(context.mem as *mut GuestMemory) };
        match memory.read_u16(GuestAddr(address)) {
            Ok(value) => u32::from(value),
            Err(_) => {
                context.fault = address;
                0
            }
        }
    }

    unsafe extern "C" fn helper_read32(context: *mut BlockContext, address: u32) -> u32 {
        let context = unsafe { &mut *context };
        let memory = unsafe { &mut *(context.mem as *mut GuestMemory) };
        match memory.read_u32(GuestAddr(address)) {
            Ok(value) => value,
            Err(_) => {
                context.fault = address;
                0
            }
        }
    }

    unsafe extern "C" fn helper_write8(context: *mut BlockContext, address: u32, value: u32) {
        let context = unsafe { &mut *context };
        let memory = unsafe { &mut *(context.mem as *mut GuestMemory) };
        if memory.write_u8(GuestAddr(address), value as u8).is_err() {
            context.fault = address;
        }
    }

    unsafe extern "C" fn helper_write16(context: *mut BlockContext, address: u32, value: u32) {
        let context = unsafe { &mut *context };
        let memory = unsafe { &mut *(context.mem as *mut GuestMemory) };
        if memory.write_u16(GuestAddr(address), value as u16).is_err() {
            context.fault = address;
        }
    }

    unsafe extern "C" fn helper_write32(context: *mut BlockContext, address: u32, value: u32) {
        let context = unsafe { &mut *context };
        let memory = unsafe { &mut *(context.mem as *mut GuestMemory) };
        if memory.write_u32(GuestAddr(address), value).is_err() {
            context.fault = address;
        }
    }

    /// Persistent JIT state: the code arena, the block cache and the shared
    /// register/flag context. Lives behind a `Box` in `ExtRuntime` so the
    /// write-barrier pointer stays valid while the runtime itself moves.
    pub(crate) struct JitState {
        arena: ExecMemory,
        cache: BlockCache,
        context: BlockContext,
        /// `GuestMemory::structure_epoch` snapshot: module (un)maps and
        /// permission splits replace guest code wholesale, so any advance
        /// flushes the whole cache.
        structure_epoch: u32,
    }

    impl JitState {
        /// Build JIT state for `memory` when `SKYENGINE_JIT` opts in. Returns
        /// `None` to keep the engine on the interpreter.
        pub fn try_new_boxed(memory: &GuestMemory) -> Option<Box<Self>> {
            if std::env::var_os("SKYENGINE_JIT").is_none() {
                return None;
            }
            Self::new_boxed(memory)
        }

        /// Build JIT state unconditionally (the exec-memory probe must still
        /// pass). The state is boxed **before** the write barrier is
        /// installed: the barrier's thread-local pointer must reference the
        /// cache at its final, stable address, so callers never see an
        /// uninstalled or half-moved state.
        pub fn new_boxed(memory: &GuestMemory) -> Option<Box<Self>> {
            let report = probe_exec_memory();
            if !report.canary_ok {
                return None;
            }
            let arena = ExecMemory::new(ARENA_SIZE, report.backend)
                .map_err(|error| eprintln!("[jit] code arena unavailable: {error:?}"))
                .ok()?;
            let context = BlockContext::new(
                helper_read8,
                helper_read16,
                helper_read32,
                helper_write8,
                helper_write16,
                helper_write32,
                std::ptr::null_mut(),
            );
            let mut state = Box::new(Self {
                arena,
                cache: BlockCache::new(),
                context,
                structure_epoch: memory.structure_epoch(),
            });
            state.install_barrier();
            eprintln!("[jit] enabled (backend {:?})", report.backend);
            Some(state)
        }

        /// Point the thread-local write barrier at this cache. Multiple
        /// engines on one thread would each steal the pointer; the engine is
        /// documented single-engine-per-thread, and `Drop` only clears a
        /// pointer it still owns.
        fn install_barrier(&mut self) {
            set_write_barrier(Some(write_barrier));
            BARRIER_CACHE.with(|cell| cell.set(std::ptr::from_mut(&mut self.cache)));
        }

        /// Test-only: number of currently cached translations.
        #[cfg(test)]
        pub(crate) fn cached_blocks(&self) -> usize {
            self.cache.len()
        }

        /// Try to execute the guest block starting at `pc` (Thumb state).
        pub fn execute_block(
            &mut self,
            cpu: &mut ArmCpu,
            memory: &mut GuestMemory,
            pc: u32,
        ) -> BlockOutcome {
            let epoch = memory.structure_epoch();
            if epoch != self.structure_epoch {
                self.structure_epoch = epoch;
                self.cache.clear();
            }
            let Some(entry) = self.cache.lookup(pc, true) else {
                return self.translate_and_run(cpu, memory, pc);
            };
            self.run_entry(cpu, memory, entry)
        }

        fn translate_and_run(
            &mut self,
            cpu: &mut ArmCpu,
            memory: &mut GuestMemory,
            pc: u32,
        ) -> BlockOutcome {
            let block =
                translate_thumb_block(&|address| memory.fetch_u16(GuestAddr(address)).ok(), pc);
            if block.is_empty() {
                return BlockOutcome::Unsupported;
            }
            let Some(entry) = self.publish(&block) else {
                return BlockOutcome::Unsupported;
            };
            self.cache.insert(entry);
            self.run_entry(cpu, memory, entry)
        }

        /// Publish `block` into the arena, resetting the arena and flushing
        /// the cache when it is full (the reset invalidates every other
        /// translation, which `clear` reflects).
        fn publish(&mut self, block: &skyengine_jit::ThumbBlock) -> Option<BlockEntry> {
            let code = match self.arena.publish(&block.code) {
                Ok(code) => code,
                Err(_) => {
                    self.arena.reset().ok()?;
                    self.cache.clear();
                    self.arena.publish(&block.code).ok()?
                }
            };
            Some(BlockEntry {
                code,
                guest_pc: block.guest_start,
                guest_end: block.guest_end,
                instructions: block.instructions,
                thumb: true,
            })
        }

        fn run_entry(
            &mut self,
            cpu: &mut ArmCpu,
            memory: &mut GuestMemory,
            entry: BlockEntry,
        ) -> BlockOutcome {
            // Guest state → context.
            for index in 0..16 {
                self.context.regs[index] = cpu.register(index);
            }
            let cpsr = cpu.cpsr();
            self.context.flags = [
                (cpsr >> 31) as u8 & 1,
                (cpsr >> 30) as u8 & 1,
                (cpsr >> 29) as u8 & 1,
                (cpsr >> 28) as u8 & 1,
            ];
            self.context.thumb = 1;
            self.context.fault = 0;
            self.context.mem = std::ptr::from_mut(memory).cast();

            let entry_address = self.arena.entry(entry.code);
            // SAFETY: `entry` points at published executable code produced by
            // the translator for this exact context layout.
            let block: BlockFn = unsafe { std::mem::transmute(entry_address) };
            // SAFETY: the context outlives the call; the block only touches the
            // context, the helper table and host stack below the dispatcher.
            unsafe { block(std::ptr::from_mut(&mut self.context)) };

            // Context → guest state.
            for index in 0..15 {
                cpu.set_register(index, self.context.regs[index]);
            }
            cpu.restore_pc_and_state(self.context.regs[15], self.context.thumb != 0);
            cpu.restore_flags(self.context.flags);
            if self.context.fault != 0 {
                return BlockOutcome::Faulted;
            }
            BlockOutcome::Executed(entry.instructions)
        }
    }

    impl Drop for JitState {
        fn drop(&mut self) {
            BARRIER_CACHE.with(|cell| {
                if cell.get() == std::ptr::from_mut(&mut self.cache) {
                    cell.set(std::ptr::null_mut());
                }
            });
        }
    }

    impl std::fmt::Debug for JitState {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("JitState")
                .field("arena_free", &self.arena.free())
                .field("cached_blocks", &self.cache.len())
                .field("structure_epoch", &self.structure_epoch)
                .finish()
        }
    }
}

#[cfg(not(target_arch = "x86_64"))]
mod imp {
    use super::BlockOutcome;
    use skyengine_arm::{ArmCpu, GuestMemory};

    /// Placeholder for hosts without a translator backend: the Thumb
    /// translator only exists on x86_64, so `try_new_boxed` always answers
    /// "no JIT" and the engine stays on the interpreter.
    #[allow(dead_code)]
    pub(crate) struct JitState {
        /// The struct is never constructed outside this module's control.
        never: (),
    }

    impl JitState {
        pub fn try_new_boxed(_memory: &GuestMemory) -> Option<Box<Self>> {
            None
        }

        /// Unreachable: `try_new_boxed` never yields a state, so the
        /// dispatcher in `ext` never calls this.
        #[allow(dead_code)]
        pub fn execute_block(
            &mut self,
            _cpu: &mut ArmCpu,
            _memory: &mut GuestMemory,
            _pc: u32,
        ) -> BlockOutcome {
            BlockOutcome::Unsupported
        }
    }

    impl std::fmt::Debug for JitState {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.debug_struct("JitState").finish()
        }
    }
}

pub(crate) use imp::JitState;

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;
    use skyengine_arm::{ArmCpu, GuestAddr, GuestMemory, Permissions};

    const CODE_BASE: u32 = 0x0010_0000;
    const STACK_BASE: u32 = 0x0020_0000;
    /// Sentinel ARM-state return address: `bx lr` exits every harness loop
    /// on the Thumb flag, and no fetch ever touches it.
    const EXIT_BASE: u32 = 0x0030_0000;

    fn setup(code: &[u16]) -> (ArmCpu, GuestMemory) {
        let mut memory = GuestMemory::new();
        let bytes: Vec<u8> = code.iter().flat_map(|half| half.to_le_bytes()).collect();
        memory
            .map_bytes(
                GuestAddr(CODE_BASE),
                bytes,
                Permissions::READ_WRITE_EXECUTE,
                "code",
            )
            .unwrap();
        memory
            .map(
                GuestAddr(STACK_BASE),
                0x200,
                Permissions::READ_WRITE,
                "stack",
            )
            .unwrap();
        let mut cpu = ArmCpu::new();
        cpu.restore_pc_and_state(CODE_BASE, true);
        cpu.set_register(13, STACK_BASE + 0x100);
        cpu.set_register(14, EXIT_BASE);
        (cpu, memory)
    }

    fn run_interpreter(code: &[u16]) -> (ArmCpu, GuestMemory) {
        let (mut cpu, mut memory) = setup(code);
        for _ in 0..64 {
            if !cpu.is_thumb() {
                break;
            }
            cpu.step(&mut memory).unwrap();
        }
        (cpu, memory)
    }

    fn run_jit(code: &[u16]) -> (ArmCpu, GuestMemory) {
        let (mut cpu, mut memory) = setup(code);
        let mut jit = JitState::new_boxed(&memory).expect("exec memory probe");
        for _ in 0..64 {
            if !cpu.is_thumb() {
                break;
            }
            let pc = cpu.pc().0;
            match jit.execute_block(&mut cpu, &mut memory, pc) {
                BlockOutcome::Executed { .. } => {}
                BlockOutcome::Faulted | BlockOutcome::Unsupported => {
                    panic!("jit dispatch failed at pc {:#010x}", cpu.pc().0)
                }
            }
        }
        (cpu, memory)
    }

    fn nzcv(cpu: &ArmCpu) -> u32 {
        cpu.cpsr() & 0xF000_0000
    }

    #[test]
    fn translated_blocks_match_interpreter_state() {
        let code: [u16; 12] = [
            0x2005, // movs r0, #5
            0x2107, // movs r1, #7
            0x1842, // adds r2, r0, r1
            0x0093, // lsls r3, r2, #2
            0x1A0C, // subs r4, r1, r0
            0x9000, // str r0, [sp, #0]
            0x9D00, // ldr r5, [sp, #0]
            0x4281, // cmp r0, r1
            0xDC00, // blt over the next insn (taken: 5 < 7, skips movs r6)
            0x2601, // movs r6, #1
            0x1C57, // adds r7, r2, #1
            0x4770, // bx lr
        ];
        let (interp_cpu, interp_memory) = run_interpreter(&code);
        let (jit_cpu, jit_memory) = run_jit(&code);

        for index in 0..15 {
            assert_eq!(
                jit_cpu.register(index),
                interp_cpu.register(index),
                "register {index}"
            );
        }
        assert_eq!(jit_cpu.pc().0, interp_cpu.pc().0);
        assert_eq!(jit_cpu.is_thumb(), interp_cpu.is_thumb());
        assert_eq!(nzcv(&jit_cpu), nzcv(&interp_cpu));
        // r2 = 12, r3 = 48, r4 = 2, r5 = 5, r6 skipped, r7 = 13.
        assert_eq!(jit_cpu.register(2), 12);
        assert_eq!(jit_cpu.register(3), 48);
        assert_eq!(jit_cpu.register(4), 2);
        assert_eq!(jit_cpu.register(5), 5);
        assert_eq!(jit_cpu.register(6), 0);
        assert_eq!(jit_cpu.register(7), 13);
        assert_eq!(
            jit_memory.read(GuestAddr(STACK_BASE + 0x100), 4).unwrap(),
            interp_memory
                .read(GuestAddr(STACK_BASE + 0x100), 4)
                .unwrap(),
        );
    }

    #[test]
    fn helper_fault_rewinds_to_the_faulting_instruction() {
        let code: [u16; 4] = [
            0x2001, // movs r0, #1
            0x6008, // str r0, [r1, #0]  (r1 unmapped → fault)
            0x2202, // movs r2, #2       (must not execute)
            0x4770, // bx lr
        ];
        let (mut cpu, mut memory) = setup(&code);
        cpu.set_register(1, 0x5000_0000);
        let mut jit = JitState::new_boxed(&memory).expect("exec memory probe");

        // First block: movs commits, then the str faults.
        match jit.execute_block(&mut cpu, &mut memory, CODE_BASE) {
            BlockOutcome::Faulted => {}
            other => panic!("expected Faulted, got {other:?}"),
        }
        assert_eq!(cpu.register(0), 1);
        assert_eq!(cpu.pc().0, CODE_BASE + 2, "pc must rewind to the str");
        assert_eq!(
            cpu.register(2),
            0,
            "instructions after the fault must not run"
        );
        // The interpreter re-executes the faulting instruction and reports
        // the same memory error the non-JIT engine would.
        assert!(cpu.step(&mut memory).is_err());
        assert_eq!(cpu.pc().0, CODE_BASE + 2, "failed step must not advance pc");
    }

    #[test]
    fn write_barrier_invalidates_overwritten_code() {
        let code: [u16; 2] = [
            0x2001, // movs r0, #1
            0x4770, // bx lr
        ];
        let (mut cpu, mut memory) = setup(&code);
        let mut jit = JitState::new_boxed(&memory).expect("exec memory probe");

        match jit.execute_block(&mut cpu, &mut memory, CODE_BASE) {
            BlockOutcome::Executed { .. } => {}
            other => panic!("first execution failed: {other:?}"),
        }
        assert_eq!(cpu.register(0), 1);
        assert_eq!(jit.cached_blocks(), 1);

        // Overwrite the guest code through the memory API: the write
        // barrier must drop the stale translation.
        memory.write_u16(GuestAddr(CODE_BASE), 0x2002).unwrap();
        assert_eq!(
            jit.cached_blocks(),
            0,
            "overwritten code must invalidate the block"
        );

        match jit.execute_block(&mut cpu, &mut memory, CODE_BASE) {
            BlockOutcome::Executed { .. } => {}
            other => panic!("re-translation failed: {other:?}"),
        }
        assert_eq!(
            cpu.register(0),
            2,
            "re-translated code must produce the new value"
        );
    }

    #[test]
    fn structure_epoch_change_flushes_the_cache() {
        let code: [u16; 2] = [
            0x2001, // movs r0, #1
            0x4770, // bx lr
        ];
        let (mut cpu, mut memory) = setup(&code);
        let mut jit = JitState::new_boxed(&memory).expect("exec memory probe");
        match jit.execute_block(&mut cpu, &mut memory, CODE_BASE) {
            BlockOutcome::Executed { .. } => {}
            other => panic!("first execution failed: {other:?}"),
        }
        assert_eq!(jit.cached_blocks(), 1);

        memory
            .map(
                GuestAddr(0x0040_0000),
                0x1000,
                Permissions::READ_WRITE,
                "extra",
            )
            .unwrap();
        match jit.execute_block(&mut cpu, &mut memory, CODE_BASE) {
            BlockOutcome::Executed { .. } => {}
            other => panic!("post-flush execution failed: {other:?}"),
        }
        assert_eq!(
            jit.cached_blocks(),
            1,
            "the flushed block must re-translate"
        );
    }

    #[test]
    fn unsupported_first_instruction_reports_unsupported() {
        let code: [u16; 2] = [
            0xDF00, // svc #0 — bails translation
            0x4770, // bx lr
        ];
        let (mut cpu, mut memory) = setup(&code);
        let mut jit = JitState::new_boxed(&memory).expect("exec memory probe");
        match jit.execute_block(&mut cpu, &mut memory, CODE_BASE) {
            BlockOutcome::Unsupported => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
        assert_eq!(
            cpu.pc().0,
            CODE_BASE,
            "pc must stay at the first instruction"
        );
    }

    /// Regression (gahhx cfunction.ext @0x2358): the screen-buffer malloc
    /// caps its size at `min(free - 0x5000, 0x4b << 13)`. The JIT once took
    /// the `ble` (skipping the cap) with free = 0x3FDB10, allocating
    /// 0x3F8B14 bytes instead of 0x96004 and corrupting the guest heap.
    #[test]
    fn gahhx_malloc_cap_matches_interpreter() {
        let code: [u16; 12] = [
            0x2105, // movs r1, #5
            0x0309, // lsls r1, r1, #12        -> r1 = 0x5000
            0x1A44, // subs r4, r0, r1          -> r4 = free - 0x5000
            0x204B, // movs r0, #0x4b
            0x0340, // lsls r0, r0, #13         -> r0 = 0x96000 (cap)
            0x4284, // cmp r4, r0
            0xDD00, // ble +16 (offset 0: skips only the cap insn)
            0x1C04, // adds r4, r0, #0          (the cap)
            0x2701, // movs r7, #1
            0x04BF, // lsls r7, r7, #18
            0x1C20, // adds r0, r4, #0
            0x4770, // bx lr
        ];
        let (interp_cpu, interp_memory) = {
            let (mut cpu, mut memory) = setup(&code);
            cpu.set_register(0, 0x003F_DB10); // platform free memory
            for _ in 0..64 {
                if !cpu.is_thumb() {
                    break;
                }
                cpu.step(&mut memory).unwrap();
            }
            (cpu, memory)
        };
        let (jit_cpu, jit_memory) = {
            let (mut cpu, mut memory) = setup(&code);
            cpu.set_register(0, 0x003F_DB10);
            let mut jit = JitState::new_boxed(&memory).expect("exec memory probe");
            for _ in 0..64 {
                if !cpu.is_thumb() {
                    break;
                }
                let pc = cpu.pc().0;
                match jit.execute_block(&mut cpu, &mut memory, pc) {
                    BlockOutcome::Executed(_) => {}
                    other => panic!("jit dispatch failed at pc {:#010x}: {other:?}", cpu.pc().0),
                }
            }
            (cpu, memory)
        };

        for index in 0..15 {
            assert_eq!(
                jit_cpu.register(index),
                interp_cpu.register(index),
                "register {index}"
            );
        }
        assert_eq!(jit_cpu.pc().0, interp_cpu.pc().0);
        assert_eq!(nzcv(&jit_cpu), nzcv(&interp_cpu));
        // The cap must apply: 0x3F8B10 > 0x96000, so r4 = 0x96000.
        assert_eq!(interp_cpu.register(4), 0x0009_6000, "interpreter cap");
        assert_eq!(jit_cpu.register(4), 0x0009_6000, "jit must cap too");
        assert_eq!(jit_cpu.register(0), 0x0009_6000);
        assert_eq!(
            jit_memory.read(GuestAddr(STACK_BASE), 4).unwrap(),
            interp_memory.read(GuestAddr(STACK_BASE), 4).unwrap(),
        );
    }
}
