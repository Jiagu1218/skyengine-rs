//! monitor 表（T008）测试：重入、竞争、wait/notify 协议、
//! FIFO 顺序、非持有者违规、线程清理。

use skyengine_kvm::runtime::monitor::{EnterOutcome, MonitorFault, Monitors};
use skyengine_kvm::runtime::{ObjRef, ThreadId};

const T1: ThreadId = ThreadId(1);
const T2: ThreadId = ThreadId(2);
const T3: ThreadId = ThreadId(3);

const A: ObjRef = ObjRef(1);
const B: ObjRef = ObjRef(2);

/// 重入计数与配对释放；释放后 exit 再报违规。
#[test]
fn enter_exit_reentry_counts() {
    let mut monitors = Monitors::new();
    assert_eq!(monitors.enter(A, T1), EnterOutcome::Acquired(1));
    assert_eq!(monitors.enter(A, T1), EnterOutcome::Acquired(2));
    assert_eq!(monitors.enter(A, T1), EnterOutcome::Acquired(3));
    assert!(monitors.is_owned_by(A, T1));

    monitors.exit(A, T1).unwrap();
    assert!(monitors.is_owned_by(A, T1), "重入未归零仍是持有者");
    monitors.exit(A, T1).unwrap();
    monitors.exit(A, T1).unwrap();
    assert!(!monitors.is_owned_by(A, T1));

    assert_eq!(monitors.exit(A, T1), Err(MonitorFault::NotOwner));
}

/// 他线程持有时 enter 返回 Contended；释放后可获得。
#[test]
fn contention_blocks_then_resumes() {
    let mut monitors = Monitors::new();
    assert_eq!(monitors.enter(A, T1), EnterOutcome::Acquired(1));
    assert_eq!(monitors.enter(A, T2), EnterOutcome::Contended(T1));

    monitors.exit(A, T1).unwrap();
    assert_eq!(monitors.enter(A, T2), EnterOutcome::Acquired(1));
}

/// 非持有者/无主对象的 exit 违规。
#[test]
fn exit_faults_for_non_owner() {
    let mut monitors = Monitors::new();
    assert_eq!(monitors.exit(A, T1), Err(MonitorFault::NotOwner), "无主对象");
    assert_eq!(monitors.enter(A, T1), EnterOutcome::Acquired(1));
    assert_eq!(monitors.exit(A, T2), Err(MonitorFault::NotOwner), "他线程代释放");
}

/// wait 完全释放监视器；notify 只标记；监视器空闲后恢复原重入深度。
#[test]
fn wait_releases_and_notify_reacquires_with_depth() {
    let mut monitors = Monitors::new();
    assert_eq!(monitors.enter(A, T1), EnterOutcome::Acquired(1));
    assert_eq!(monitors.enter(A, T1), EnterOutcome::Acquired(2));

    // T1 wait：监视器空闲（T2 可入），重入深度 2 被记录。
    monitors.wait(A, T1).unwrap();
    assert!(!monitors.is_owned_by(A, T1));
    assert_eq!(monitors.enter(A, T2), EnterOutcome::Acquired(1));

    // T2 notify：只标记，T1 尚不能回归（监视器被 T2 持有）。
    assert_eq!(monitors.notify(A, T2).unwrap(), 1);
    assert!(monitors.reacquire_ready().is_empty(), "T2 未释放，T1 不得回归");

    monitors.exit(A, T2).unwrap();
    let ready = monitors.reacquire_ready();
    assert_eq!(ready, vec![(A, T1)]);
    assert!(monitors.is_owned_by(A, T1), "T1 重新持有");
    assert_eq!(monitors.enter(A, T1), EnterOutcome::Acquired(3), "恢复深度 2 后重入为 3");
}

/// 未持锁的 wait/notify/notifyAll 报违规。
#[test]
fn wait_notify_require_ownership() {
    let mut monitors = Monitors::new();
    assert_eq!(monitors.wait(A, T1), Err(MonitorFault::NotOwner));
    assert_eq!(monitors.notify(A, T1), Err(MonitorFault::NotOwner));
    assert_eq!(monitors.notify_all(A, T1), Err(MonitorFault::NotOwner));

    assert_eq!(monitors.enter(A, T1), EnterOutcome::Acquired(1));
    assert_eq!(monitors.wait(A, T2), Err(MonitorFault::NotOwner));
    assert_eq!(monitors.notify(A, T2), Err(MonitorFault::NotOwner));
    assert_eq!(monitors.notify_all(A, T2), Err(MonitorFault::NotOwner));
}

/// notify 按等待 FIFO 顺序标记；无等待者返回 0；已标记不重复计。
#[test]
fn notify_follows_fifo_order() {
    let mut monitors = Monitors::new();
    assert_eq!(monitors.enter(A, T1), EnterOutcome::Acquired(1));
    // T2、T3 依次入 wait 队列（各自先短暂持锁再 wait）。
    assert_eq!(monitors.enter(A, T2), EnterOutcome::Contended(T1));
    // T2/T3 需要持有才能 wait：借 T1 释放后的窗口依次进入。
    monitors.exit(A, T1).unwrap();
    assert_eq!(monitors.enter(A, T2), EnterOutcome::Acquired(1));
    monitors.wait(A, T2).unwrap();
    assert_eq!(monitors.enter(A, T3), EnterOutcome::Acquired(1));
    monitors.wait(A, T3).unwrap();

    // T1 持锁通知。
    assert_eq!(monitors.enter(A, T1), EnterOutcome::Acquired(1));
    assert_eq!(monitors.notify(A, T1).unwrap(), 1, "标记 T2（最早）");
    assert_eq!(monitors.notify(A, T1).unwrap(), 1, "标记 T3");
    assert_eq!(monitors.notify(A, T1).unwrap(), 0, "无未通知等待者");

    monitors.exit(A, T1).unwrap();
    assert_eq!(monitors.reacquire_ready(), vec![(A, T2)], "FIFO 回归");
    assert_eq!(monitors.reacquire_ready(), Vec::new(), "T2 持有期间 T3 不回归");
    // T2 退出后 T3 才能回归。
    monitors.exit(A, T2).unwrap();
    assert_eq!(monitors.reacquire_ready(), vec![(A, T3)]);
}

/// notifyAll 全量标记，逐个空闲窗口依次回归。
#[test]
fn notify_all_wakes_everyone() {
    let mut monitors = Monitors::new();
    assert_eq!(monitors.enter(A, T1), EnterOutcome::Acquired(1));
    assert_eq!(monitors.enter(A, T2), EnterOutcome::Contended(T1));
    monitors.exit(A, T1).unwrap();
    assert_eq!(monitors.enter(A, T2), EnterOutcome::Acquired(1));
    monitors.wait(A, T2).unwrap();
    assert_eq!(monitors.enter(A, T3), EnterOutcome::Acquired(1));
    monitors.wait(A, T3).unwrap();

    assert_eq!(monitors.enter(A, T1), EnterOutcome::Acquired(1));
    assert_eq!(monitors.notify_all(A, T1).unwrap(), 2);
    monitors.exit(A, T1).unwrap();

    assert_eq!(monitors.reacquire_ready(), vec![(A, T2)]);
    monitors.exit(A, T2).unwrap();
    assert_eq!(monitors.reacquire_ready(), vec![(A, T3)]);
}

/// 线程清理：释放其全部监视器并从等待队列移除。
#[test]
fn release_all_cleans_up_dead_thread() {
    let mut monitors = Monitors::new();
    // T1 持有 A、B 两个监视器；T2 在 B 上竞争失败。
    assert_eq!(monitors.enter(A, T1), EnterOutcome::Acquired(1));
    assert_eq!(monitors.enter(B, T1), EnterOutcome::Acquired(1));
    assert_eq!(monitors.enter(B, T2), EnterOutcome::Contended(T1));
    // T1 释放 A，T2 转而在 A 上 wait。
    monitors.exit(A, T1).unwrap();
    assert_eq!(monitors.enter(A, T2), EnterOutcome::Acquired(1));
    monitors.wait(A, T2).unwrap();

    // T2 死亡：应从 A 的等待队列移除，且不影响 T1 对 B 的持有。
    assert_eq!(monitors.release_all(T2), 0, "等待者无持有可释放");
    assert!(monitors.is_owned_by(B, T1));
    // T1 释放 A 后无任何等待者回归（T2 已被清除）。
    assert_eq!(monitors.enter(A, T1), EnterOutcome::Acquired(1));
    assert_eq!(monitors.notify(A, T1).unwrap(), 0);
    monitors.exit(A, T1).unwrap();
    assert!(monitors.reacquire_ready().is_empty());

    // T1 死亡：B 被释放。
    assert_eq!(monitors.release_all(T1), 1);
    assert!(!monitors.is_owned_by(B, T1));
    assert_eq!(monitors.enter(B, T3), EnterOutcome::Acquired(1));
}

/// reacquire 恢复深度后，后续 exit 次数与 wait 前一致。
#[test]
fn reacquired_depth_pairs_with_exit() {
    let mut monitors = Monitors::new();
    assert_eq!(monitors.enter(A, T1), EnterOutcome::Acquired(1));
    assert_eq!(monitors.enter(A, T1), EnterOutcome::Acquired(2));
    monitors.wait(A, T1).unwrap();

    assert_eq!(monitors.enter(A, T2), EnterOutcome::Acquired(1));
    assert_eq!(monitors.notify(A, T2).unwrap(), 1);
    monitors.exit(A, T2).unwrap();
    assert_eq!(monitors.reacquire_ready(), vec![(A, T1)]);

    // 恢复的深度 2 需要两次 exit 才真正释放。
    monitors.exit(A, T1).unwrap();
    assert!(monitors.is_owned_by(A, T1));
    monitors.exit(A, T1).unwrap();
    assert!(!monitors.is_owned_by(A, T1));
}
