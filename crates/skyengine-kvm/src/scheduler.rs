//! 协作式绿线程调度器（T011）。
//!
//! ## 模型
//!
//! - 每个绿线程持有一套独立帧栈（`ThreadEntry::frames`）；`poll` 挑一个
//!   Runnable 线程，把其帧栈交给 `interp::run` 执行到让出点
//!   （预算耗尽 / 根帧返回 / 异常逃逸 / monitorenter 阻塞）。
//! - 与解释器通过 `Machine`（cs/heap/monitors/natives 的独占引用集合）
//!   对接：引擎侧持有全局状态，逐次构造 Machine 传入 poll。
//!   （计划中的 `&mut Vm` 在库内尚无对应类型，`Machine` 即既有执行
//!   上下文，语义等价。）
//! - 轮转策略：自 current 的下一位置起循环扫描，取首个 Runnable；
//!   仅剩一个 Runnable 时自然回到自身（等效"优先 current"）。
//! - `next_wake` 以绝对到期时刻为键：`poll(now)` 先把 `<= now` 到期的
//!   BlockedWake 全部唤醒为 Runnable；无 Runnable 时经
//!   `AllBlocked.next_wake` 上报最小键，宿主据此决定下一次 poll 时刻。
//! - Timer 队列（`timers`，T034 实化）：到期项为 `TimerCb::RunTask`
//!   （TimerTask 实例引用）——惰性查取消标志（task 自身或所属
//!   Timer 的 cancelled 字段）、period 任务按 fixed-rate 重排
//!   （nextTime += |period|）、为 `run()V` 压帧 spawn 绿线程。

use std::collections::{BTreeMap, HashMap};

use crate::cldc::util::{timer_task_is_cancelled, timer_task_requeue};
use crate::interp::{push_entry_frame, runtime_class_of, run, Frame, Machine, RunOutcome};
use crate::runtime::{ObjRef, ThreadId};
use crate::KvmError;

/// 线程状态。`BlockedMonitor` 的键与 monitor 表（runtime/monitor.rs）
/// 的标识一致：等待的锁对象 `ObjRef`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadState {
    /// 可运行：poll 可挑选执行。
    Runnable,
    /// 定时休眠：绝对到期时刻（毫秒）。
    BlockedWake(u64),
    /// monitorenter 竞争失败：等待的锁对象。
    BlockedMonitor(ObjRef),
    /// `Object.wait`（T034）：已完全释放监视器并进入其等待队列
    /// （`Monitors::wait`）；唤醒走 reacquire_ready 移交路径
    /// （重持锁 + 恢复重入深度），定时版另登记 next_wake。
    BlockedWait(ObjRef),
    /// `Thread.join`（T035）：等待的目标线程；其死亡时经
    /// `join_waits` 反查唤醒（登记与重解析见 poll 段 1d）。
    BlockedJoin(ThreadId),
    /// 阻塞于宿主 I/O（T012 HostContext 接线后使用）。
    BlockedIo,
    /// 帧栈跑空或异常逃逸，线程终止。
    Dead,
}

/// 单个绿线程：标识、状态、帧栈与名字（调试/异常报告用）。
pub struct ThreadEntry {
    pub id: ThreadId,
    pub state: ThreadState,
    pub frames: Vec<Frame>,
    pub name: String,
}

/// `poll` 一次的结果。
#[derive(Debug, PartialEq, Eq)]
pub enum PollOutcome {
    /// 驱动了一个线程到让出点。
    Ran(ThreadId),
    /// 无 Runnable 但仍有存活线程；`next_wake` 为最近定时唤醒时刻。
    AllBlocked { next_wake: Option<u64> },
    /// 无存活线程（空表或全部 Dead）。
    Idle,
}

/// Timer 到期回调（T034 实化）：`RunTask` 携带 TimerTask 实例引用，
/// 到期时（惰性查取消后）为 `run()V` 压帧 spawn 绿线程；period 任务
/// 按 fixed-rate 重排（见 `run_due_timers`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerCb {
    /// 执行 TimerTask.run（task 实例引用；NULL 为防御占位，触发即跳过）。
    RunTask(ObjRef),
}

/// 协作式调度器。
pub struct Scheduler {
    threads: Vec<ThreadEntry>,
    /// 最近一次 poll 驱动的线程；轮转自此之后扫描。
    current: Option<ThreadId>,
    /// 绝对到期时刻 → 待唤醒线程。
    next_wake: BTreeMap<u64, Vec<ThreadId>>,
    /// Timer 队列：绝对到期时刻 → 回调。
    timers: BTreeMap<u64, Vec<TimerCb>>,
    /// 被等待线程 → join 等待者列表（T035：BlockedJoin 唤醒反查）。
    join_waits: HashMap<ThreadId, Vec<ThreadId>>,
    /// 目标尚未 spawn 的 join（等待者, 目标对象）——逐轮重解析（T035）。
    pending_joins: Vec<(ThreadId, ObjRef)>,
    /// ThreadId 分配器（自 1 起）。
    next_id: u32,
}

impl Scheduler {
    pub fn new() -> Self {
        Self {
            threads: Vec::new(),
            current: None,
            next_wake: BTreeMap::new(),
            timers: BTreeMap::new(),
            join_waits: HashMap::new(),
            pending_joins: Vec::new(),
            next_id: 1,
        }
    }

    /// 入列新线程（Runnable），返回分配的 ThreadId。
    pub fn spawn(&mut self, frames: Vec<Frame>, name: impl Into<String>) -> ThreadId {
        let id = ThreadId(self.next_id);
        self.next_id += 1;
        self.threads.push(ThreadEntry {
            id,
            state: ThreadState::Runnable,
            frames,
            name: name.into(),
        });
        id
    }

    /// 线程状态查询（线程不存在返回 None）。
    pub fn state(&self, id: ThreadId) -> Option<ThreadState> {
        self.threads.iter().find(|t| t.id == id).map(|t| t.state)
    }

    /// 线程是否已终止（帧栈跑空/异常逃逸；条目缺失亦视作死）。
    /// T021 `Vm.poll` 段三用于判定 paint 回调线程已结束。
    pub fn is_dead(&self, id: ThreadId) -> bool {
        matches!(self.state(id), Some(ThreadState::Dead) | None)
    }

    /// 最近驱动的线程。
    pub fn current(&self) -> Option<ThreadId> {
        self.current
    }

    /// 定时阻塞：进入 BlockedWake(wake_at) 并登记 next_wake。
    pub fn block_wake(&mut self, id: ThreadId, wake_at: u64) {
        self.set_state(id, ThreadState::BlockedWake(wake_at));
        self.next_wake.entry(wake_at).or_default().push(id);
    }

    /// monitor 阻塞（monitorenter 竞争失败或 Object.wait）。
    pub fn block_monitor(&mut self, id: ThreadId, key: ObjRef) {
        self.set_state(id, ThreadState::BlockedMonitor(key));
    }

    /// 唤醒：任意阻塞态 → Runnable，并从 next_wake 摘除残留登记。
    /// 唤醒等待指定线程死亡的全部 joiner（T035）。
    fn wake_joiners(&mut self, dead: ThreadId) {
        if let Some(joiners) = self.join_waits.remove(&dead) {
            for joiner in joiners {
                self.wake(joiner);
            }
        }
    }

    pub fn wake(&mut self, id: ThreadId) {
        self.next_wake
            .retain(|_, ids| {
                ids.retain(|&i| i != id);
                !ids.is_empty()
            });
        self.set_state(id, ThreadState::Runnable);
    }

    /// 唤醒等待指定 monitor 的全部线程，返回被唤醒者（按入表序）。
    pub fn notify_monitor_waiters(&mut self, key: ObjRef) -> Vec<ThreadId> {
        let mut woken = Vec::new();
        for t in &mut self.threads {
            if t.state == ThreadState::BlockedMonitor(key) {
                t.state = ThreadState::Runnable;
                woken.push(t.id);
            }
        }
        woken
    }

    /// 登记一个 Timer 回调。
    pub fn schedule_timer(&mut self, at_ms: u64, cb: TimerCb) {
        self.timers.entry(at_ms).or_default().push(cb);
    }

    /// 触发 now_ms（含）之前到期的全部 Timer，按时刻升序出队。
    pub fn fire_due(&mut self, now_ms: u64) -> Vec<TimerCb> {
        let due: Vec<u64> = self.timers.range(..=now_ms).map(|(&k, _)| k).collect();
        let mut fired = Vec::new();
        for k in due {
            if let Some(cbs) = self.timers.remove(&k) {
                fired.extend(cbs);
            }
        }
        fired
    }

    /// 存活线程数（非 Dead；T034 Timer 测试观测 run 线程消亡用）。
    pub fn alive_count(&self) -> usize {
        self.threads
            .iter()
            .filter(|t| t.state != ThreadState::Dead)
            .count()
    }

    /// Timer 到期处理（T034 触发链，poll 唤醒段调用）：
    /// 1. **惰性删除**：task.cancelled 或所属 Timer.cancelled → 跳过
    ///    （取消只置标志，触发时才出队，避免 native 侧无调度器通道）；
    /// 2. **fixed-rate 重排**：period != 0 → nextTime += |period| 写回
    ///    并重新入队。JDK 区分 fixed-delay（触发后 now+period）与
    ///    fixed-rate（nextTime += period），游戏帧循环对两者无感，
    ///    统一取 fixed-rate 简化（漂移不累积，长时间运行更接近真机）；
    /// 3. **spawn run 线程**：按 task 运行时类解析 `run()V`（用户子类
    ///    覆写优先）压入口帧，spawn 匿名 "timer-task" 绿线程——与
    ///    Vm.poll 的 paint/键回调同款"请求 → 构帧 → spawn"协议。
    fn run_due_timers(&mut self, now_ms: u64, machine: &mut Machine) -> Result<(), KvmError> {
        for cb in self.fire_due(now_ms) {
            let TimerCb::RunTask(task) = cb;
            if task.is_null() || timer_task_is_cancelled(machine.cs, machine.heap, task)? {
                continue;
            }
            // fixed-rate 重排：写回 nextTime 并按新时刻重新入队。
            if let Some(next_at) = timer_task_requeue(machine.cs, machine.heap, task)? {
                self.schedule_timer(next_at, cb);
            }
            let class = runtime_class_of(machine.cs, machine.heap, task)?;
            let mut frames = Vec::new();
            push_entry_frame(machine, &mut frames, class, "run", "()V", Some(task), Vec::new())?;
            self.spawn(frames, "timer-task");
        }
        Ok(())
    }

    fn set_state(&mut self, id: ThreadId, state: ThreadState) {
        if let Some(t) = self.threads.iter_mut().find(|t| t.id == id) {
            t.state = state;
        }
    }

    fn index_of(&self, id: ThreadId) -> Option<usize> {
        self.threads.iter().position(|t| t.id == id)
    }

    /// 调度心跳：唤醒到期线程 → 轮转挑 Runnable → 驱动到让出点。
    ///
    /// 单次驱动至多 `budget` 条指令；让出点后控制权返回宿主，
    /// 绿线程状态保存在 `ThreadEntry`（帧栈/pc 可续跑）。
    pub fn poll(
        &mut self,
        now_ms: u64,
        machine: &mut Machine,
        budget: u64,
    ) -> Result<PollOutcome, KvmError> {
        // 1) 唤醒 now_ms（含）前到期的 BlockedWake。定时 wait（T034）
        //    到期**不直接 Runnable**：先标记等待者为已通知，由下方
        //    1b 的 reacquire_ready 走"重新持有监视器（恢复重入深度）
        //    + 转 Runnable"移交——JLS 17.14 超时醒后须重新竞争锁，
        //    直接续跑会让后续 monitorexit 落 NotOwner。
        let due: Vec<u64> = self.next_wake.range(..=now_ms).map(|(&k, _)| k).collect();
        for k in due {
            for id in self.next_wake.remove(&k).unwrap_or_default() {
                if let Some(ThreadState::BlockedWait(obj)) = self.state(id) {
                    let _ = machine.monitors.mark_waiter_notified(obj, id);
                    continue;
                }
                self.set_state(id, ThreadState::Runnable);
            }
        }

        // 1b) monitor 推进（T034）：
        //     - 已通知且监视器空闲的 wait 等待者 → 移交重持有 +
        //       转 Runnable（notify 的实际唤醒点：notify 只标记，
        //       调用者 monitorexit 释放锁后才可能空闲）；
        //     - BlockedMonitor 线程（monitorenter 竞争失败）在锁空闲
        //       后唤醒重试（轮询式重抢；CLDC 无公平性要求，锁空闲
        //       即给机会，与"新进入者可先于等待者"的宽放一致）。
        for (_, id) in machine.monitors.reacquire_ready() {
            self.wake(id);
        }
        let retry: Vec<ThreadId> = self
            .threads
            .iter()
            .filter_map(|t| match t.state {
                ThreadState::BlockedMonitor(obj) if !machine.monitors.is_locked(obj) => {
                    Some(t.id)
                }
                _ => None,
            })
            .collect();
        for id in retry {
            self.wake(id);
        }

        // 1c) Timer 到期触发（T034）：run 线程在本轮即参与下方轮转。
        self.run_due_timers(now_ms, machine)?;

        // 1d) join 等待重解析（T035）：Yield(Join) 登记的目标未
        //     spawn 时逐轮重查；已 spawn 且存活 → BlockedJoin；
        //     已死 → 直接唤醒（pc 已越过 join，续跑即返回后文）。
        let pending = std::mem::take(&mut self.pending_joins);
        for (joiner, target) in pending {
            match machine.cs.thread_ids.get(&target).copied() {
                Some(tid) if machine.cs.thread_alive.get(&tid) == Some(&false) => {
                    self.wake(joiner);
                }
                Some(tid) => {
                    self.set_state(joiner, ThreadState::BlockedJoin(tid));
                    self.join_waits.entry(tid).or_default().push(joiner);
                }
                None => self.pending_joins.push((joiner, target)),
            }
        }

        // 2) 自 current 之后循环扫描，取首个 Runnable（单线程自然回到自身）。
        let len = self.threads.len();
        if len == 0 {
            return Ok(PollOutcome::Idle);
        }
        let start = match self.current.and_then(|id| self.index_of(id)) {
            Some(i) => (i + 1) % len,
            None => 0,
        };
        let mut picked: Option<usize> = None;
        for step in 0..len {
            let i = (start + step) % len;
            if self.threads[i].state == ThreadState::Runnable {
                if self.threads[i].frames.is_empty() {
                    // 防御：Runnable 但帧栈已空（如外部清栈）→ 判死。
                    self.threads[i].state = ThreadState::Dead;
                    // T033：生命周期表同步（isAlive 面）。
                    let dead_id = self.threads[i].id;
                    machine.cs.thread_alive.insert(dead_id, false);
                    self.wake_joiners(dead_id);
                    continue;
                }
                picked = Some(i);
                break;
            }
        }

        let Some(idx) = picked else {
            // 无 Runnable：仍有存活线程 → AllBlocked（上报最近唤醒时刻）；
            // 否则 Idle。
            if self.threads.iter().all(|t| t.state == ThreadState::Dead) {
                return Ok(PollOutcome::Idle);
            }
            return Ok(PollOutcome::AllBlocked {
                next_wake: self.next_wake.keys().next().copied(),
            });
        };

        // 3) 驱动选中线程到让出点（run 复用 T009 的预算让出机制）。
        let id = self.threads[idx].id;
        let insn_budget = u32::try_from(budget).unwrap_or(u32::MAX);
        let outcome = run(machine, &mut self.threads[idx].frames, id, Some(insn_budget))?;
        self.current = Some(id);
        match outcome {
            // 预算耗尽：保持 Runnable，状态可续跑（下次 poll 轮到别的线程）。
            RunOutcome::BudgetExhausted => {}
            // JIT 嵌套执行界专用（调度器域不可达）：按预算耗尽容错。
            RunOutcome::FramePopped => {}
            // 根帧返回：帧栈已弹空 → 终止（T033 同步登记生命周期表）。
            RunOutcome::Returned(_) => {
                eprintln!("DEAD(ret) name={}", self.threads[idx].name);
                self.threads[idx].state = ThreadState::Dead;
                machine.cs.thread_alive.insert(id, false);
                self.wake_joiners(id);
            }
            // 异常逃逸根帧：真实 KVM 终止该线程（向 MIDlet 的传播面后续任务接）。
            RunOutcome::Uncaught(ref unwind) => {
                // T045：轻量死亡原因记录——根因文本（异常类名 / 故障形态）
                // 无条件写入 cs.last_uncaught，供 Vm 侧升格 fatal（错误画面
                // 根因）或判定 destroyApp 回调的拒绝销毁；trace 开关只控打印。
                let detail = match unwind {
                    crate::interp::Unwind::Thrown(obj) => {
                        match crate::interp::runtime_class_of(
                            machine.cs,
                            machine.heap,
                            *obj,
                        ) {
                            Ok(class) => machine.cs.name_of(class).to_string(),
                            Err(error) => format!("(class lookup failed: {error})"),
                        }
                    }
                    crate::interp::Unwind::Fault(fault) => format!("fault {fault:?}"),
                };
                machine.cs.last_uncaught = Some((id, detail.clone()));
                // 调试面：SKYENGINE_KVM_TRACE_UNWIND=1 时打印根因类名。
                if std::env::var_os("SKYENGINE_KVM_TRACE_UNWIND").is_some() {
                    eprintln!("[kvm] thread {id:?} died: uncaught {detail}");
                }
                self.threads[idx].state = ThreadState::Dead;
                machine.cs.thread_alive.insert(id, false);
                self.wake_joiners(id);
            }
            // monitorenter 竞争失败：pc 未推进，等待锁释放后重试。
            RunOutcome::BlockedOnMonitor(obj) => {
                self.threads[idx].state = ThreadState::BlockedMonitor(obj);
            }
            // T033 让出协议：sleep/join 到期休眠（绝对时刻登记 next_wake，
            // 唤醒后从让出调用的下一条指令续跑）；yield 保持 Runnable，
            // 天然轮转到下个线程。
            RunOutcome::Yielded(crate::YieldKind::Sleep { wake_at_ms }) => {
                self.block_wake(id, wake_at_ms);
            }
            RunOutcome::Yielded(crate::YieldKind::Cpu) => {}
            // Thread.join（T035）：转入 pending_joins，由下轮段 1d
            // 解析目标（未 spawn 重查 / 存活 BlockedJoin / 已死唤醒）。
            RunOutcome::Yielded(crate::YieldKind::Join { target }) => {
                self.pending_joins.push((id, target));
            }
            // Object.wait（T034）：监视器的释放与入队已在解释器分派点
            // 经 Monitors.wait 完成，这里仅登记状态；无限等待不登
            // next_wake（无唤醒源则如实 AllBlocked），定时版登记
            // （唤醒段的移交语义见 1）。
            RunOutcome::Yielded(crate::YieldKind::Wait { obj, wake_at_ms, .. }) => {
                self.set_state(id, ThreadState::BlockedWait(obj));
                if let Some(at) = wake_at_ms {
                    self.next_wake.entry(at).or_default().push(id);
                }
            }
        }
        Ok(PollOutcome::Ran(id))
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}
