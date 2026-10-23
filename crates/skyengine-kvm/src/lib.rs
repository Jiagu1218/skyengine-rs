//! skyengine-kvm：宿主侧 Rust 实现的 J2ME 虚拟机（CLDC-1.1 + MIDP-2.0）。
//!
//! 架构（plan.md）：
//! - `classfile/`：JVM class 文件解析（常量池两遍解析、字节码解码、
//!   tableswitch/lookupswitch 4 字节对齐、jsr/ret、wide）。
//! - `runtime/`：类空间（加载-链接-解析）、堆对象/数组、UTF-16 字符串驻留、
//!   非移动 mark-sweep GC、monitor 表。
//! - `interp/`：可恢复帧解释器（全操作码）、natives + bootstrap 类声明式注册表。
//! - `scheduler.rs`：协作式绿线程调度、阻塞/唤醒、Timer 队列。
//! - `cldc/`：java.lang / java.util / java.io 原生实现。
//! - `midp/`：lcdui/game、rms、media、io、midlet 生命周期。
//! - `host/`：HostContext trait（引擎服务抽象）+ 测试用 MockHostContext。

pub mod classfile;
/// java.lang / java.util / java.io 原生实现（T014 起）。
pub mod cldc;
/// 取证用运行时开关（drawRegion transform A/B 变体，GUI F9 切换）。
pub mod debug_switch;
/// HostContext trait（kvm → 引擎唯一服务通道，T012）+ MockHostContext。
pub mod host;
/// jar（zip）解包（T013）：install_jar 的库内自持实现。
pub mod jar;
pub mod interp;
/// javax/microedition MIDP 原生实现（lcdui 的 Image/PNG 解码 T018 起）。
pub mod midp;
pub mod runtime;
/// 协作式绿线程调度器（T011）：线程状态机、轮转 poll、Timer 队列。
pub mod scheduler;
/// 方法级 baseline JIT（x86_64 宿主；v1 叶子整数方法子集）。
#[cfg(target_arch = "x86_64")]
pub mod jit;
/// Vm 门面（T013）：状态机 + install_jar/start_midlet/poll/事件入队/shutdown。
pub mod vm;

pub use vm::{PollResult, Vm, VmConfig, VmError, VmEvent, VmState};

use runtime::ObjRef;

/// 绿线程让出种类（T033 让出协议）。
///
/// native（sleep/yield/join）无法直接挂起线程——它们不在调度器语境里，
/// 故以 `KvmError::Yield` 错误协议上抛：解释器 native 分派点转
/// `InsnOutcome::Yielded` → `RunOutcome::Yielded`，调度器 poll 据此
/// 转移线程状态（Sleep → BlockedWake 定时休眠；Cpu → 保持 Runnable
/// 天然轮转下个线程）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum YieldKind {
    /// 定时休眠：绝对到期时刻（毫秒，宿主时钟）。
    Sleep { wake_at_ms: u64 },
    /// 让出 CPU：仅交出本轮执行权（Thread.yield）。
    Cpu,
    /// `Object.wait`（T034）：完全释放 `obj` 的监视器并进入等待队列
    /// （实际释放/入队由解释器 native 分派点经 `Monitors.wait` 桥接，
    /// 未持锁 → IllegalMonitorStateException），线程转
    /// `ThreadState::BlockedWait`；定时版（`wait(ms)`）携带绝对唤醒
    /// 时刻（无限等待为 None），到期视作内部通知走"重新持有监视器"
    /// 移交路径（JLS 17.14 超时醒后须重新竞争锁）。`timeout_ms` 为
    /// 原始超时参数（0=无限；负值已在 native 侧抛 IAE），仅语义记录。
    /// `Thread.join`（T035 语义修复）：等待的目标 Thread 对象。由调度器
    /// 解析目标 id 并转入 `ThreadState::BlockedJoin`，目标线程死亡时唤醒
    ///（此前"2ms 定时重入"版让出后 pc 已越过调用点，醒来无论目标死活
    /// 都续跑——嵌套 join 下提前返回，属实现 bug，本次随 T035 修复）。
    Join {
        /// join 的目标 Thread 实例。
        target: ObjRef,
    },
    Wait {
        /// 等待的锁对象。
        obj: ObjRef,
        /// 超时参数（毫秒；0=无限等待）。
        timeout_ms: u64,
        /// 绝对唤醒时刻（无限等待 None；超时版 = now + ms）。
        wake_at_ms: Option<u64>,
    },
}

/// 对外错误类型：引擎侧通过它区分"VM 内部故障"与"可重入失败"。
#[derive(Debug, thiserror::Error)]
pub enum KvmError {
    #[error("class format error: {0}")]
    ClassFormat(String),
    #[error("class not found: {0}")]
    ClassNotFound(String),
    /// 解析/链接失败（NoSuchField/NoSuchMethod/循环继承等）。
    /// 解释器将其映射为对应的 Java Error 抛给 guest。
    #[error("linkage error: {0}")]
    Linkage(String),
    #[error("unsupported class file version: {0}")]
    UnsupportedVersion(String),
    #[error("out of memory")]
    OutOfMemory,
    /// 原生方法主动抛出的 Java 异常（T014）：解释器 invoke 原生分派点
    /// 拦截该变体并转成 `Unwind::Thrown` 走常规异常处理器搜索；
    /// 若逃逸到解释器之外（不经过 invoke 路径），视为内部错误。
    #[error("java exception thrown from native: {0:?}")]
    Throw(ObjRef),
    /// 绿线程让出请求（T033）：sleep/yield/join 轮询版经此上抛，
    /// 解释器 native 分派点转 `RunOutcome::Yielded`，调度器消费。
    /// 与 Throw 同为"native 分派点拦截"的错误协议；若逃逸到解释器
    /// 之外（不经 invoke 路径，理论不可能），视为内部错误。
    #[error("green thread yield requested: {0:?}")]
    Yield(YieldKind),
    #[error("internal vm error: {0}")]
    Internal(String),
}
