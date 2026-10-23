//! monitor 表（T008）：synchronized 与 Object.wait/notify/notifyAll 语义。
//!
//! CLDC 的多线程为协作式绿线程（D4），monitor 状态可用纯 Rust 表达
//! 而无需原子操作：每线程要么持锁、要么在显式让出点阻塞。表以
//! `ObjRef` 为键外置于对象头；解释器在 monitorenter 前完成空引用
//! 检查（null 不进表，与堆的 null_deref 约定一致）。
//!
//! 语义要点：
//! - 重入：同线程重复 enter 递增计数，exit 递减，归零释放；
//! - wait：**完全释放**监视器（记录 wait 前重入深度）并入 FIFO
//!   等待队列；被通知后仍须等监视器空闲才重新持有，重新持有时
//!   恢复原重入深度（JLS 17.14 wait/notify 协议）；
//! - notify/notifyAll 只标记、不立即移交：调度器经 `reacquire_ready`
//!   完成"已通知 && 空闲 → 重新持有"的实际推进；
//! - 无公平性保证（CLDC 不要求）：监视器空闲时新进入者可先于
//!   已通知的等待者获得锁。

use std::collections::{HashMap, VecDeque};

use crate::runtime::{ObjRef, ThreadId};

/// monitor 语义违规（解释器映射为 Java 异常）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MonitorFault {
    /// 未持有监视器的线程调用 exit/wait/notify/notifyAll
    /// → java.lang.IllegalMonitorStateException。
    NotOwner,
}

/// monitorenter 的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnterOutcome {
    /// 获得监视器（首入或重入），携带新的重入计数。
    Acquired(u32),
    /// 被其他线程持有：解释器应阻塞当前线程（调度器稍后重试 enter）。
    Contended(ThreadId),
}

/// 等待者：wait 时记录重入深度，notify 置标记。
#[derive(Clone, Copy, Debug)]
struct Waiter {
    thread: ThreadId,
    /// wait 前的重入深度，重新持有时恢复。
    recursion: u32,
    notified: bool,
}

/// 单个对象的监视器：持有线程、进入计数、等待队列。
#[derive(Debug, Default)]
struct Monitor {
    owner: Option<ThreadId>,
    recursion: u32,
    waiters: VecDeque<Waiter>,
}

/// monitor 表：全部对象的监视器状态。
#[derive(Debug, Default)]
pub struct Monitors {
    entries: HashMap<ObjRef, Monitor>,
}

impl Monitors {
    pub fn new() -> Self {
        Self::default()
    }

    /// monitorenter：无主 → 获得；自持 → 重入；他持 → Contended。
    pub fn enter(&mut self, obj: ObjRef, thread: ThreadId) -> EnterOutcome {
        let entry = self.entries.entry(obj).or_default();
        match entry.owner {
            None => {
                entry.owner = Some(thread);
                entry.recursion = 1;
                EnterOutcome::Acquired(1)
            }
            Some(owner) if owner == thread => {
                entry.recursion += 1;
                EnterOutcome::Acquired(entry.recursion)
            }
            Some(owner) => EnterOutcome::Contended(owner),
        }
    }

    /// monitorexit：重入递减，归零释放。未持有 → NotOwner。
    pub fn exit(&mut self, obj: ObjRef, thread: ThreadId) -> Result<(), MonitorFault> {
        let Some(entry) = self.entries.get_mut(&obj) else {
            return Err(MonitorFault::NotOwner);
        };
        if entry.owner != Some(thread) {
            return Err(MonitorFault::NotOwner);
        }
        entry.recursion -= 1;
        if entry.recursion == 0 {
            entry.owner = None;
            self.prune(obj);
        }
        Ok(())
    }

    /// 是否持有（重入计数 > 0 即持有）。
    pub fn is_owned_by(&self, obj: ObjRef, thread: ThreadId) -> bool {
        self.entries
            .get(&obj)
            .is_some_and(|entry| entry.owner == Some(thread))
    }

    /// 监视器是否被任一线程持有（无表项/无主 = 空闲）。
    /// T034 调度器推进段用于唤醒 BlockedMonitor 线程重试 monitorenter。
    pub fn is_locked(&self, obj: ObjRef) -> bool {
        self.entries.get(&obj).is_some_and(|entry| entry.owner.is_some())
    }

    /// 标记指定线程的等待者为已通知（T034 定时 wait 到期用：超时
    /// 视作内部通知，走 reacquire_ready 的"重新持有 + 恢复重入深度"
    /// 移交路径——直接 Runnable 会让后续 monitorexit 落 NotOwner）。
    /// 返回是否命中等待者。
    pub fn mark_waiter_notified(&mut self, obj: ObjRef, thread: ThreadId) -> bool {
        if let Some(entry) = self.entries.get_mut(&obj)
            && let Some(waiter) = entry.waiters.iter_mut().find(|w| w.thread == thread) {
                waiter.notified = true;
                return true;
            }
        false
    }

    /// Object.wait：必须持有；完全释放（记录重入深度）并入 FIFO 等待队列。
    pub fn wait(&mut self, obj: ObjRef, thread: ThreadId) -> Result<(), MonitorFault> {
        let Some(entry) = self.entries.get_mut(&obj) else {
            return Err(MonitorFault::NotOwner);
        };
        if entry.owner != Some(thread) {
            return Err(MonitorFault::NotOwner);
        }
        let recursion = entry.recursion;
        entry.owner = None;
        entry.recursion = 0;
        entry.waiters.push_back(Waiter { thread, recursion, notified: false });
        Ok(())
    }

    /// notify：必须持有；标记最早未通知的等待者（FIFO），返回标记数（0/1）。
    pub fn notify(&mut self, obj: ObjRef, thread: ThreadId) -> Result<usize, MonitorFault> {
        let Some(entry) = self.entries.get_mut(&obj) else {
            return Err(MonitorFault::NotOwner);
        };
        if entry.owner != Some(thread) {
            return Err(MonitorFault::NotOwner);
        }
        for waiter in &mut entry.waiters {
            if !waiter.notified {
                waiter.notified = true;
                return Ok(1);
            }
        }
        Ok(0)
    }

    /// notifyAll：必须持有；标记全部等待者，返回标记数。
    pub fn notify_all(&mut self, obj: ObjRef, thread: ThreadId) -> Result<usize, MonitorFault> {
        let Some(entry) = self.entries.get_mut(&obj) else {
            return Err(MonitorFault::NotOwner);
        };
        if entry.owner != Some(thread) {
            return Err(MonitorFault::NotOwner);
        }
        let mut marked = 0;
        for waiter in &mut entry.waiters {
            if !waiter.notified {
                waiter.notified = true;
                marked += 1;
            }
        }
        Ok(marked)
    }

    /// 调度器推进：把"已通知 && 监视器空闲"的等待者移出队列并重新
    /// 持有（恢复 wait 前重入深度），返回 (对象, 线程) 供调度器置
    /// Runnable。FIFO；每对象每轮至多移交一个（其余等下一轮空闲）。
    pub fn reacquire_ready(&mut self) -> Vec<(ObjRef, ThreadId)> {
        let mut ready = Vec::new();
        for (obj, entry) in &mut self.entries {
            if entry.owner.is_some() {
                continue;
            }
            if let Some(index) = entry.waiters.iter().position(|w| w.notified) {
                let waiter = entry.waiters.remove(index).unwrap();
                entry.owner = Some(waiter.thread);
                entry.recursion = waiter.recursion;
                ready.push((*obj, waiter.thread));
            }
        }
        ready
    }

    /// 线程终结/中断清理：释放其持有的全部监视器、从所有等待队列
    /// 移除该线程。返回释放的监视器数（死线程无需调度器重试）。
    pub fn release_all(&mut self, thread: ThreadId) -> usize {
        let mut released = 0;
        for entry in self.entries.values_mut() {
            if entry.owner == Some(thread) {
                entry.owner = None;
                entry.recursion = 0;
                released += 1;
            }
            entry.waiters.retain(|waiter| waiter.thread != thread);
        }
        self.entries
            .retain(|_, entry| entry.owner.is_some() || !entry.waiters.is_empty());
        released
    }

    /// 无主且无等待者的表项回收（防表随临时对象膨胀）。
    fn prune(&mut self, obj: ObjRef) {
        self.entries.retain(|&key, entry| {
            key != obj || entry.owner.is_some() || !entry.waiters.is_empty()
        });
    }
}
