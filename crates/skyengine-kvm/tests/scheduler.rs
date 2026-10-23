//! 调度器测试（T011）：双线程轮转、定时唤醒、帧栈跑空判死、
//! AllBlocked 的 next_wake 上报、monitor 等待者唤醒、Timer 队列。
//!
//! 帧构造复用 interp.rs 的手法（`enter` 压入口帧）；
//! 类字节经共享构造器手构（见 `common/classbytes.rs`）。

#![allow(dead_code)]

mod common;

use common::classbytes::{build_class, exception_classes, provider, MethodSpec, Pool, ROOT};

use skyengine_kvm::interp::{enter, Frame, Machine, NativeCall, NativeDispatch};
use skyengine_kvm::host::mock::MockHostContext;
use skyengine_kvm::runtime::classspace::ClassSpace;
use skyengine_kvm::runtime::heap::Heap;
use skyengine_kvm::runtime::monitor::Monitors;
use skyengine_kvm::runtime::{ObjRef, Slot, ThreadId};
use skyengine_kvm::scheduler::{PollOutcome, Scheduler, ThreadState, TimerCb};
use skyengine_kvm::KvmError;

// ---- 测试用字节码 ---------------------------------------------------------

const ICONST_0: u8 = 0x03;
const GOTO: u8 = 0xa7;
const RETURN: u8 = 0xb1;

/// 带码静态方法（access 0x0008 = ACC_STATIC）。
fn m(name: &'static str, stack: u16, locals: u16, code: Vec<u8>) -> MethodSpec {
    MethodSpec {
        access: 0x0008,
        name,
        desc: "()V",
        code: Some((stack, locals, code)),
        handlers: Vec::new(),
    }
}

/// 死循环帧（iconst_0; goto -1）：永不返回，预算耗尽即让出。
const LOOP_CODE: [u8; 4] = [ICONST_0, GOTO, 0xFF, 0xFF];
/// 立即返回帧（return）：一次 poll 内跑空判死。
const DONE_CODE: [u8; 1] = [RETURN];

// ---- 最小 VM 夹具（同 interp.rs，native 用空实现） -------------------------

/// 空原生分派：调度器测试不触发 native 路径。
#[derive(Default)]
struct NoopNatives;

impl NativeDispatch for NoopNatives {
    fn call_native(
        &mut self,
        _cs: &mut ClassSpace,
        _heap: &mut Heap,
        _host: &mut dyn skyengine_kvm::host::HostContext,
        _call: NativeCall,
    ) -> Result<Option<Slot>, KvmError> {
        Ok(None)
    }
}

struct Vm {
    cs: ClassSpace,
    heap: Heap,
    monitors: Monitors,
    natives: NoopNatives,
    /// HostContext 通道（T019）；调度测试用 Mock 兜底。
    host: MockHostContext,
}

impl Vm {
    /// ROOT + 标准异常层次 + 用户类；object_class 回填给类空间。
    fn new(pool: &mut Pool, user: Vec<(String, Vec<u8>)>) -> Self {
        let mut entries = vec![(
            ROOT.to_string(),
            build_class(pool, ROOT, None, &[], &[], &[]),
        )];
        entries.extend(exception_classes(pool));
        entries.extend(user);
        let mut cs = ClassSpace::new(Box::new(provider(entries)));
        let root = cs.load_class(ROOT).unwrap();
        cs.object_class = Some(root);
        Self {
            cs,
            heap: Heap::new(),
            monitors: Monitors::new(),
            natives: NoopNatives,
            host: MockHostContext::new(240, 320),
        }
    }

    fn machine(&mut self) -> Machine<'_> {
        Machine {
            cs: &mut self.cs,
            heap: &mut self.heap,
            monitors: &mut self.monitors,
            natives: &mut self.natives,
            host: &mut self.host,
        }
    }

    /// 为静态入口方法构造全新帧栈（只压帧不执行）。
    fn entry_frames(&mut self, class: &str, name: &str) -> Vec<Frame> {
        let class_id = self.cs.load_class(class).unwrap();
        let mut machine = self.machine();
        let mut frames = Vec::new();
        enter(&mut machine, &mut frames, class_id, name, "()V", Vec::new()).unwrap();
        frames
    }
}

/// 建夹具：用户类 A 带 loop（死循环）与 done（立即返回）两个静态方法。
fn fixture() -> (Vm, Scheduler) {
    let mut pool = Pool::new();
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[
            m("loop", 1, 0, LOOP_CODE.to_vec()),
            m("done", 1, 0, DONE_CODE.to_vec()),
        ],
    );
    let vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    (vm, Scheduler::new())
}

// ---- 轮转 -----------------------------------------------------------------

/// 双线程各跑一段预算：poll 交替驱动 A/B（轮转发生）。
#[test]
fn round_robin_alternates_between_two_threads() {
    let (mut vm, mut sched) = fixture();
    let a = sched.spawn(vm.entry_frames("A", "loop"), "A");
    let b = sched.spawn(vm.entry_frames("A", "loop"), "B");
    let mut machine = vm.machine();
    // 死循环帧：每次 poll 都以预算耗尽让出，线程保持 Runnable。
    assert_eq!(sched.poll(0, &mut machine, 10).unwrap(), PollOutcome::Ran(a));
    assert_eq!(sched.state(a), Some(ThreadState::Runnable));
    // current=A → 下一轮从 B 起扫描。
    assert_eq!(sched.poll(0, &mut machine, 10).unwrap(), PollOutcome::Ran(b));
    assert_eq!(sched.poll(0, &mut machine, 10).unwrap(), PollOutcome::Ran(a));
    assert_eq!(sched.poll(0, &mut machine, 10).unwrap(), PollOutcome::Ran(b));
    assert_eq!(sched.current(), Some(b));
}

/// 单 Runnable 线程：轮转扫描绕一圈回到自身，连续驱动不饿死。
#[test]
fn single_runnable_thread_keeps_running() {
    let (mut vm, mut sched) = fixture();
    let a = sched.spawn(vm.entry_frames("A", "loop"), "A");
    let mut machine = vm.machine();
    for _ in 0..3 {
        assert_eq!(sched.poll(0, &mut machine, 5).unwrap(), PollOutcome::Ran(a));
    }
}

// ---- 定时唤醒 -------------------------------------------------------------

/// block_wake 未到期 → AllBlocked 并上报 wake_at；时间推进到点后回 Runnable。
#[test]
fn block_wake_returns_runnable_when_due() {
    let (mut vm, mut sched) = fixture();
    let a = sched.spawn(vm.entry_frames("A", "loop"), "A");
    sched.block_wake(a, 100);
    assert_eq!(sched.state(a), Some(ThreadState::BlockedWake(100)));

    let mut machine = vm.machine();
    // now=50：未到期。
    assert_eq!(
        sched.poll(50, &mut machine, 10).unwrap(),
        PollOutcome::AllBlocked { next_wake: Some(100) }
    );
    // now=100：到期唤醒并执行。
    assert_eq!(sched.poll(100, &mut machine, 10).unwrap(), PollOutcome::Ran(a));
    assert_eq!(sched.state(a), Some(ThreadState::Runnable));
}

/// 多个阻塞线程：AllBlocked 上报最小 wake_at；到点只唤醒到期者。
#[test]
fn all_blocked_reports_min_wake() {
    let (mut vm, mut sched) = fixture();
    let a = sched.spawn(vm.entry_frames("A", "loop"), "A");
    let b = sched.spawn(vm.entry_frames("A", "loop"), "B");
    sched.block_wake(a, 100);
    sched.block_wake(b, 50);

    let mut machine = vm.machine();
    assert_eq!(
        sched.poll(0, &mut machine, 10).unwrap(),
        PollOutcome::AllBlocked { next_wake: Some(50) }
    );
    // now=50：仅 B 到期。
    assert_eq!(sched.poll(50, &mut machine, 10).unwrap(), PollOutcome::Ran(b));
    assert_eq!(sched.state(a), Some(ThreadState::BlockedWake(100)));
    assert_eq!(sched.state(b), Some(ThreadState::Runnable));
}

/// 手工 wake 提前唤醒：next_wake 登记同步摘除，AllBlocked 不再误报。
#[test]
fn manual_wake_cleans_next_wake_registry() {
    let (mut vm, mut sched) = fixture();
    let a = sched.spawn(vm.entry_frames("A", "done"), "A");
    sched.block_wake(a, 100);
    sched.wake(a);
    assert_eq!(sched.state(a), Some(ThreadState::Runnable));

    let mut machine = vm.machine();
    // 提前于 100 也能跑。
    assert_eq!(sched.poll(10, &mut machine, 10).unwrap(), PollOutcome::Ran(a));
    // done 帧跑空 → Dead；无存活线程且 next_wake 已清 → Idle。
    assert_eq!(sched.state(a), Some(ThreadState::Dead));
    assert_eq!(sched.poll(10, &mut machine, 10).unwrap(), PollOutcome::Idle);
}

// ---- 判死与 Idle ----------------------------------------------------------

/// 帧栈跑空（根帧返回）→ Dead；随后 poll 无存活线程 → Idle。
#[test]
fn empty_frames_mark_dead_then_idle() {
    let (mut vm, mut sched) = fixture();
    let a = sched.spawn(vm.entry_frames("A", "done"), "A");
    let mut machine = vm.machine();
    assert_eq!(sched.poll(0, &mut machine, 10).unwrap(), PollOutcome::Ran(a));
    assert_eq!(sched.state(a), Some(ThreadState::Dead));
    assert_eq!(sched.poll(0, &mut machine, 10).unwrap(), PollOutcome::Idle);
    // 空调度器直接 Idle。
    let (_vm2, mut sched2) = fixture();
    assert_eq!(sched2.poll(0, &mut machine, 10).unwrap(), PollOutcome::Idle);
}

/// 部分存活部分 Dead：Dead 不参与轮转，存活者继续被驱动。
#[test]
fn dead_threads_are_skipped_in_rotation() {
    let (mut vm, mut sched) = fixture();
    let a = sched.spawn(vm.entry_frames("A", "done"), "A");
    let b = sched.spawn(vm.entry_frames("A", "loop"), "B");
    let mut machine = vm.machine();
    assert_eq!(sched.poll(0, &mut machine, 10).unwrap(), PollOutcome::Ran(a));
    assert_eq!(sched.state(a), Some(ThreadState::Dead));
    // 只剩 B：连续驱动。
    assert_eq!(sched.poll(0, &mut machine, 10).unwrap(), PollOutcome::Ran(b));
    assert_eq!(sched.poll(0, &mut machine, 10).unwrap(), PollOutcome::Ran(b));
}

// ---- monitor 等待者 -------------------------------------------------------

/// notify_monitor_waiters：唤醒等待同一把锁的全部线程并返回其 id。
#[test]
fn notify_monitor_waiters_wakes_all_on_key() {
    let (mut vm, mut sched) = fixture();
    let a = sched.spawn(vm.entry_frames("A", "loop"), "A");
    let b = sched.spawn(vm.entry_frames("A", "loop"), "B");
    let key = ObjRef(7);
    sched.block_monitor(a, key);
    sched.block_monitor(b, key);
    assert_eq!(sched.state(a), Some(ThreadState::BlockedMonitor(key)));

    let woken = sched.notify_monitor_waiters(key);
    assert_eq!(woken, vec![ThreadId(1), ThreadId(2)]);
    assert_eq!(sched.state(a), Some(ThreadState::Runnable));
    assert_eq!(sched.state(b), Some(ThreadState::Runnable));

    // 全部 Runnable 后 poll 可驱动。
    let mut machine = vm.machine();
    assert_eq!(sched.poll(0, &mut machine, 10).unwrap(), PollOutcome::Ran(a));
}

// ---- Timer 队列 -----------------------------------------------------------

/// fire_due：按时刻升序出队到期限的回调，未到期与重复入队正确处理。
/// （T034 实化后回调携带 TimerTask 引；出队语义用 NULL 占位验证，
/// 触发语义见 thread_semantics.rs 的 timer 段。）
#[test]
fn fire_due_drains_timers_up_to_now() {
    let (_vm, mut sched) = fixture();
    sched.schedule_timer(30, TimerCb::RunTask(ObjRef::NULL));
    sched.schedule_timer(30, TimerCb::RunTask(ObjRef::NULL));
    sched.schedule_timer(90, TimerCb::RunTask(ObjRef::NULL));
    // now=50：两个 t=30 到期。
    assert_eq!(sched.fire_due(50).len(), 2);
    // 已出队的不重复；t=90 到 now=100 才到期。
    assert!(sched.fire_due(50).is_empty());
    assert_eq!(sched.fire_due(100).len(), 1);
    assert!(sched.fire_due(1000).is_empty());
}
