//! java/lang/Thread 原生实现（T033）：绿线程的 Java 面。
//!
//! ## 架构（沿 T021 repaint 的"请求位 + Vm.poll 消费"先例）
//!
//! native `start()` 拿不到调度器（NativeFn 签名只有 cs/heap），故：
//! - **start**：校验后把 Thread 实例压 `cs.pending_threads` 请求队列；
//! - **spawn_pending**（本模块，Vm.poll 调度段前调用）：drain 队列，
//!   解析 run 目标（子类覆写 → Thread 实例为 receiver；否则 target
//!   字段 → Runnable 实例为 receiver），`push_entry_frame` 构帧 +
//!   `scheduler.spawn` 拉起真绿线程，登记三表
//!   （thread_objects/thread_ids/thread_alive）。
//!
//! ## 让出协议（KvmError::Yield，T033）
//!
//! sleep/yield/join（轮询版）无法在 native 里挂起线程，以
//! `Err(KvmError::Yield(kind))` 上抛：解释器 native 分派点转
//! `RunOutcome::Yielded`（pc 已推进，唤醒后续跑下一条指令），
//! 调度器转 BlockedWake（Sleep）或保持 Runnable（Cpu）。
//!
//! ## join：轮询版
//!
//! 简单正确版：目标未死则 `Yield(Sleep{now+2})` 定时重入 join；
//! 目标死后 join 自然返回。TODO 后续任务：改 BlockedJoin 精化 +
//! interrupt 打断 sleep/join 的唤醒面；wait/notify 见 T034+。

use crate::host::HostContext;
use crate::interp::{Frame, Machine, NativeCall, push_entry_frame, runtime_class_of};
use crate::runtime::classspace::ClassSpace;
use crate::runtime::heap::Heap;
use crate::runtime::{ObjRef, Slot, ThreadId};
use crate::scheduler::Scheduler;
use crate::{KvmError, YieldKind};

// ---- 小工具 ---------------------------------------------------------------

/// 接收者引用（this 为 null 属调用序错误，防御 NPE 语义）。
fn this_ref(call: &NativeCall) -> Result<ObjRef, KvmError> {
    call.this
        .filter(|r| !r.is_null())
        .ok_or_else(|| KvmError::Internal("native instance call on null this".into()))
}

/// Thread 实例字段的槽位（find_field 沿继承链）。
fn thread_field(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    obj: ObjRef,
    name: &str,
    desc: &str,
) -> Result<u16, KvmError> {
    let class = runtime_class_of(cs, heap, obj)?;
    let name_sym = cs.syms.intern(name);
    let desc_sym = cs.syms.intern(desc);
    let field = cs.find_field(class, name_sym, desc_sym)?;
    Ok(field.slot)
}

/// 读 Thread 实例整值字段（boolean 的槽位表示同为 Int）。
/// desc 按声明描述符（started/interrupted 为 "Z"，priority 为 "I"）。
fn get_int_field(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    obj: ObjRef,
    name: &str,
    desc: &str,
) -> Result<i32, KvmError> {
    let slot = thread_field(cs, heap, obj, name, desc)?;
    match heap.instance(obj)?.fields[slot as usize] {
        Slot::Int(value) => Ok(value),
        other => Err(KvmError::Internal(format!(
            "Thread.{name} expected int, got {other:?}"
        ))),
    }
}

/// 写 Thread 实例整值字段（desc 语义同 get_int_field）。
fn set_int_field(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    obj: ObjRef,
    name: &str,
    desc: &str,
    value: i32,
) -> Result<(), KvmError> {
    let slot = thread_field(cs, heap, obj, name, desc)?;
    heap.instance_mut(obj)?.fields[slot as usize] = Slot::Int(value);
    Ok(())
}

/// 读 Thread 实例引用字段。
fn get_ref_field(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    obj: ObjRef,
    name: &str,
    desc: &str,
) -> Result<ObjRef, KvmError> {
    let slot = thread_field(cs, heap, obj, name, desc)?;
    match heap.instance(obj)?.fields[slot as usize] {
        Slot::Ref(reference) => Ok(reference),
        other => Err(KvmError::Internal(format!(
            "Thread.{name} expected ref, got {other:?}"
        ))),
    }
}

/// 写 Thread 实例引用字段。
fn set_ref_field(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    obj: ObjRef,
    name: &str,
    desc: &str,
    value: ObjRef,
) -> Result<(), KvmError> {
    let slot = thread_field(cs, heap, obj, name, desc)?;
    heap.instance_mut(obj)?.fields[slot as usize] = Slot::Ref(value);
    Ok(())
}

/// 分配 Thread 实例并填 name/priority（构造器共用路径）。
fn init_thread(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    obj: ObjRef,
    target: Option<ObjRef>,
    name: Option<ObjRef>,
) -> Result<(), KvmError> {
    // 默认名 "Thread-{N}"：N 取全局计数器后自增（与 JVM 默认名形态一致）。
    let name_ref = match name {
        Some(reference) if !reference.is_null() => reference,
        _ => {
            let text = format!("Thread-{}", cs.thread_counter);
            cs.thread_counter += 1;
            heap.alloc_string_uninterned(text.encode_utf16().collect())?
        }
    };
    set_ref_field(cs, heap, obj, "target", "Ljava/lang/Runnable;", target.unwrap_or(ObjRef::NULL))?;
    set_ref_field(cs, heap, obj, "name", "Ljava/lang/String;", name_ref)?;
    // MIDP 规范 NORM_PRIORITY = 5。
    set_int_field(cs, heap, obj, "priority", "I", 5)?;
    Ok(())
}

// ---- 构造器 ----------------------------------------------------------------

pub(crate) fn thread_init(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    init_thread(cs, heap, this_ref(call)?, None, None)?;
    Ok(None)
}

pub(crate) fn thread_init_runnable(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let target = match call.args.first() {
        Some(Slot::Ref(reference)) => *reference,
        other => return Err(KvmError::Internal(format!("Thread(Runnable) arg: {other:?}"))),
    };
    init_thread(cs, heap, this_ref(call)?, Some(target), None)?;
    Ok(None)
}

pub(crate) fn thread_init_runnable_name(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let target = match call.args.first() {
        Some(Slot::Ref(reference)) => *reference,
        other => return Err(KvmError::Internal(format!("Thread(Runnable,String) arg0: {other:?}"))),
    };
    let name = match call.args.get(1) {
        Some(Slot::Ref(reference)) => *reference,
        other => return Err(KvmError::Internal(format!("Thread(Runnable,String) arg1: {other:?}"))),
    };
    init_thread(cs, heap, this_ref(call)?, Some(target), Some(name))?;
    Ok(None)
}

pub(crate) fn thread_init_name(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let name = match call.args.first() {
        Some(Slot::Ref(reference)) => *reference,
        other => return Err(KvmError::Internal(format!("Thread(String) arg: {other:?}"))),
    };
    init_thread(cs, heap, this_ref(call)?, None, Some(name))?;
    Ok(None)
}

// ---- 生命周期面 -------------------------------------------------------------

/// `start()V`：已 start（started 位）→ IllegalThreadStateException；
/// 否则置 started 并把 this 压 pending 队列（真 spawn 由 Vm.poll 消费）。
pub(crate) fn thread_start(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call)?;
    if get_int_field(cs, heap, this, "started", "Z")? != 0 {
        return Err(crate::cldc::lang::throw_java(
            cs,
            heap,
            "java/lang/IllegalThreadStateException",
            Some("thread already started"),
        ));
    }
    set_int_field(cs, heap, this, "started", "Z", 1)?;
    cs.pending_threads.push(this);
    Ok(None)
}

/// `currentThread()Thread`：查 thread_objects 命中即返回；
/// 未命中（主线程首调）懒建名为 "main" 的 Thread 对象补录三表。
pub(crate) fn thread_current_thread(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let id = call.thread;
    if let Some(&obj) = cs.thread_objects.get(&id) {
        return Ok(Some(Slot::Ref(obj)));
    }
    // 懒建主线程对象（或 clinit 语境 id=0 的防御对象）。
    let class = cs.load_class("java/lang/Thread")?;
    let template = cs.class(class).instance_template.clone();
    let obj = heap.alloc_instance(class, template)?;
    init_thread(cs, heap, obj, None, None)?;
    // 名覆盖为 "main"：不消耗默认名计数器（主线程懒建不该顶掉
    // Thread-0——首个无名构造的工作线程仍取 Thread-0）。
    cs.thread_counter = cs.thread_counter.saturating_sub(1);
    let main_name = heap.alloc_string_uninterned("main".encode_utf16().collect())?;
    set_ref_field(cs, heap, obj, "name", "Ljava/lang/String;", main_name)?;
    cs.thread_objects.insert(id, obj);
    cs.thread_ids.insert(obj, id);
    cs.thread_alive.entry(id).or_insert(true);
    Ok(Some(Slot::Ref(obj)))
}

/// `isAlive()Z`：对象未入三表（未 start / spawn 失败）= false；
/// 已登记查生命周期表。
pub(crate) fn thread_is_alive(
    cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call)?;
    let alive = cs
        .thread_ids
        .get(&this)
        .and_then(|id| cs.thread_alive.get(id).copied())
        .unwrap_or(false);
    Ok(Some(Slot::Int(i32::from(alive))))
}

// ---- 属性面 -----------------------------------------------------------------

pub(crate) fn thread_get_name(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let name = get_ref_field(cs, heap, this_ref(call)?, "name", "Ljava/lang/String;")?;
    Ok(Some(Slot::Ref(name)))
}

/// `setPriority(I)V`：MIDP 值域 1..10 钳制（MIN=1/NORM=5/MAX=10）。
pub(crate) fn thread_set_priority(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let value = match call.args.first() {
        Some(Slot::Int(value)) => *value,
        other => return Err(KvmError::Internal(format!("setPriority arg: {other:?}"))),
    };
    set_int_field(cs, heap, this_ref(call)?, "priority", "I", value.clamp(1, 10))?;
    Ok(None)
}

pub(crate) fn thread_get_priority(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Int(get_int_field(cs, heap, this_ref(call)?, "priority", "I")?)))
}

/// `interrupt()V`：置 interrupted 位。TODO T034+：打断 sleep/join 的
/// 唤醒面（当前 sleep 只认时钟到期，interrupt 不提前唤醒）。
pub(crate) fn thread_interrupt(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    set_int_field(cs, heap, this_ref(call)?, "interrupted", "Z", 1)?;
    Ok(None)
}

pub(crate) fn thread_is_interrupted(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Int(get_int_field(cs, heap, this_ref(call)?, "interrupted", "Z")?)))
}

// ---- 让出面（错误协议上抛） ---------------------------------------------

/// `sleep(J)V`：负毫秒 → IllegalArgumentException；否则让出协议上抛
/// Sleep{now+ms}（宿主时钟经 Host 轨取得）。
pub(crate) fn thread_sleep(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let ms = match call.args.first() {
        Some(Slot::Long(value)) => *value,
        other => return Err(KvmError::Internal(format!("sleep arg: {other:?}"))),
    };
    if ms < 0 {
        return Err(crate::cldc::lang::throw_java(
            cs,
            heap,
            "java/lang/IllegalArgumentException",
            Some("negative sleep time"),
        ));
    }
    let now = host.now_ms();
    Err(KvmError::Yield(YieldKind::Sleep {
        wake_at_ms: now + ms as u64,
    }))
}

/// `yield()V`：让出 CPU（保持 Runnable，天然轮转下个线程）。
pub(crate) fn thread_yield(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Err(KvmError::Yield(YieldKind::Cpu))
}

/// `join()V`（轮询版）：this 为目标线程对象；未入表（未 start）或
/// 已死（thread_alive==Some(false)）立即返回；存活或待 spawn 则
/// Sleep{now+2} 定时重入。TODO：BlockedJoin 精化 + interrupt 打断。
pub(crate) fn thread_join(
    cs: &mut ClassSpace,
    _heap: &mut Heap,
    _host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let target = this_ref(call)?;
    // 从未 start（started 位未置）→ 无可等待，立即返回（JVM 语义）。
    if get_int_field(cs, _heap, target, "started", "Z")? == 0 {
        return Ok(None);
    }
    // 目标已死 → 立即返回；未 spawn 或存活 → Join 让出（调度器登记
    // BlockedJoin，目标死亡时唤醒——T035 修复，见 YieldKind::Join 注释；
    // 未 spawn 的目标经调度器 pending_joins 逐轮重解析）。
    if let Some(&id) = cs.thread_ids.get(&target)
        && matches!(cs.thread_alive.get(&id), Some(false))
    {
        return Ok(None);
    }
    Err(KvmError::Yield(YieldKind::Join { target }))
}

// ---- spawn 消费（Vm.poll 调度段前） --------------------------------------

/// 登记线程存活（start_midlet 主线程用：Thread 对象懒建于 currentThread）。
pub fn register_thread_alive(cs: &mut ClassSpace, id: ThreadId) {
    cs.thread_alive.insert(id, true);
}

/// Thread 实例的名字字段文本（spawn 注册线程名用；读不出退 "thread"）。
fn thread_name_text(cs: &mut ClassSpace, heap: &mut Heap, obj: ObjRef) -> String {
    let Ok(name_ref) = get_ref_field(cs, heap, obj, "name", "Ljava/lang/String;") else {
        return "thread".into();
    };
    if name_ref.is_null() {
        return "thread".into();
    }
    heap.str_obj(name_ref)
        .map(|s| String::from_utf16_lossy(&s.chars))
        .unwrap_or_else(|_| "thread".into())
}

/// 解析 Thread 实例的 run 执行帧：
/// 1. 运行时类沿父链找 `run()V`——定义体有字节码（Thread 子类覆写）
///    → receiver=Thread 实例；
/// 2. 落到 bootstrap Thread 本体（body=None 桩）→ 读 target 字段，
///    receiver=target 实例（虚分发走 target 运行时类）；
/// 3. target 为 null 且无覆写 → 空帧序（无代码可跑，跳过 spawn）。
fn build_run_frames(
    machine: &mut Machine,
    thread_obj: ObjRef,
) -> Result<Vec<Frame>, KvmError> {
    let runtime_class = runtime_class_of(machine.cs, machine.heap, thread_obj)?;
    let name = machine.cs.syms.intern("run");
    let desc = machine.cs.syms.intern("()V");
    if let Some((defining, index)) = machine.cs.find_method(runtime_class, name, desc) {
        let has_body = machine.cs.class(defining).methods[index as usize]
            .body
            .is_some();
        if has_body {
            // 子类覆写 run：以 Thread 实例为 receiver。
            let mut frames = Vec::new();
            push_entry_frame(
                machine, &mut frames, runtime_class, "run", "()V", Some(thread_obj), Vec::new(),
            )?;
            return Ok(frames);
        }
    }
    // 默认路径：target.run()（虚分发——从 target 运行时类解析 run）。
    let target = get_ref_field(
        machine.cs,
        machine.heap,
        thread_obj,
        "target",
        "Ljava/lang/Runnable;",
    )?;
    if target.is_null() {
        // Runnable 无 body、Object 无 run：无代码可跑，跳过。
        return Ok(Vec::new());
    }
    let target_class = runtime_class_of(machine.cs, machine.heap, target)?;
    let mut frames = Vec::new();
    push_entry_frame(
        machine, &mut frames, target_class, "run", "()V", Some(target), Vec::new(),
    )?;
    Ok(frames)
}

/// 消费 `cs.pending_threads`：逐个解析 run 帧 → `scheduler.spawn` 真拉起
/// 绿线程 → 登记三表。单个 Thread 构帧失败（链接错误等）静默跳过——
/// started 已置位而三表未登记，对象生命周期视作"已启动即终止"
/// （isAlive 恒 false），不拖垮整个 VM。
pub fn spawn_pending(
    machine: &mut Machine,
    scheduler: &mut Scheduler,
) -> Result<(), KvmError> {
    let pending = std::mem::take(&mut machine.cs.pending_threads);
    for thread_obj in pending {
        let Ok(frames) = build_run_frames(machine, thread_obj) else {
            continue;
        };
        if frames.is_empty() {
            continue;
        }
        let name = thread_name_text(machine.cs, machine.heap, thread_obj);
        let id = scheduler.spawn(frames, name);
        machine.cs.thread_objects.insert(id, thread_obj);
        machine.cs.thread_ids.insert(thread_obj, id);
        machine.cs.thread_alive.insert(id, true);
    }
    Ok(())
}

