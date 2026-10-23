//! 可恢复帧解释器（T009）：帧模型、clinit 链、异常展开、指令预算。
//!
//! ## 帧与调用协议
//!
//! - 普通调用：压帧前调用帧 `pc += 1`（指向 invoke 的下一条），被调帧
//!   记 `caller_pc = invoke 指令索引`。正常返回无需回调帧 pc；
//!   异常展开时把调用帧 pc 恢复到调用点再搜其处理器表。
//! - clinit 触发（new/getstatic/putstatic/invokestatic，JLS 12.4 主动使用）：
//!   触发指令**不推进 pc**；整条未初始化超类链一次性标 `Initializing`，
//!   从最根超类起逐个压 `<clinit>` 帧（余链存在 `FrameKind::Clinit::rest`）。
//!   链完成后触发指令重执行，此时全链已 `Initialized`。
//! - monitorenter 竞争失败：操作数恢复、pc 不推进，返回
//!   `BlockedOnMonitor` 交调度器（T011）稍后重试。
//!
//! ## 异常模型
//!
//! `Unwind` 区分 athrow 抛出的对象（`Thrown`）与运行时防御产生的故障
//! （`Fault`）。处理器搜索：`Thrown` 按对象运行时类的赋值兼容链；
//! `Fault` 按类名链（无需预载异常类层次）。`Fault` 被捕获时才物化为
//! 真实异常实例（加载异常类 → 模板分配 → 写 detailMessage）。
//! clinit 帧展开时整条余链粘滞标 `Failed`，后续主动使用报链接错误。
//!
//! ## GC 接线
//!
//! 分配走 `alloc_gc`：快路径直接分配，OOM 时以"全帧 locals/stack +
//! 静态区 + 调用方提供的临时引用"为根收集后重试一次（D2）。

pub(crate) mod opcodes;

/// bootstrap 声明式注册表与原生分派（T010）。
pub mod natives;

use std::rc::Rc;

use crate::classfile::code::{CodeBody, Kind};
use crate::host::HostContext;
use crate::runtime::classspace::{ClassSpace, InitState};
use crate::runtime::gc::{self, alloc_with_gc};
use crate::runtime::heap::Heap;
use crate::runtime::monitor::Monitors;
use crate::runtime::object::Payload;
use crate::runtime::{ClassId, ObjRef, Slot, Sym, ThreadId};
use crate::KvmError;

use opcodes::execute_insn;

/// 原生方法调用描述（T010 的 natives 注册表实现 `NativeDispatch`）。
#[derive(Debug)]
pub struct NativeCall {
    /// 声明原生方法的类。
    pub class: ClassId,
    pub name: Sym,
    pub desc: Sym,
    /// 实例方法的接收者；静态方法为 None。
    pub this: Option<ObjRef>,
    /// 实参数值槽（cat2 已剥 Pad；顺序与描述符一致）。
    pub args: Vec<Slot>,
    /// 发起调用的绿线程（T033）：`Thread.currentThread` 等经此查
    /// cs.thread_objects；由 exec_invoke 分派点自调度器传入的
    /// thread 参数填充（clinit native 通道无线程语境，填 ThreadId(0)）。
    pub thread: ThreadId,
}

/// 原生方法分派接口。`Ok(None)` = void 返回；返回值为单值槽
/// （long/double 不带 Pad，分派处补齐）。
pub trait NativeDispatch {
    fn call_native(
        &mut self,
        cs: &mut ClassSpace,
        heap: &mut Heap,
        host: &mut dyn HostContext,
        call: NativeCall,
    ) -> Result<Option<Slot>, KvmError>;
}

/// 解释器执行上下文：运行所需全局状态的独占引用集合。
pub struct Machine<'a> {
    pub cs: &'a mut ClassSpace,
    pub heap: &'a mut Heap,
    pub monitors: &'a mut Monitors,
    pub natives: &'a mut dyn NativeDispatch,
    /// 宿主服务通道（T019）：LCDUI 等平台相关 native 经此取字体/屏幕/时钟。
    pub host: &'a mut dyn HostContext,
}

/// 帧种类。
#[derive(Debug)]
pub enum FrameKind {
    /// 普通方法调用（含根帧）。
    Method,
    /// `<clinit>` 执行中：`class` 为当前初始化类，`rest` 为同链待初始化类。
    Clinit { class: ClassId, rest: Vec<ClassId> },
}

/// 调用帧。`pc` 为指令索引（`body.insns` 下标）。
#[derive(Debug)]
pub struct Frame {
    pub class: ClassId,
    pub method: u16,
    pub body: Rc<CodeBody>,
    pub locals: Vec<Slot>,
    pub stack: Vec<Slot>,
    pub pc: u32,
    /// 展开时恢复到的调用点指令索引；根帧与 clinit 帧为 None
    /// （clinit 完成后触发帧本就停在触发指令上）。
    pub caller_pc: Option<u32>,
    pub kind: FrameKind,
}

/// 异常展开值：athrow 抛出的对象或运行时防御产生的故障。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unwind {
    Thrown(ObjRef),
    Fault(Fault),
}

/// 运行时故障（JVM 防御性检查失败）。被捕获时按 `java_name` 物化。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    NullPointer,
    ArrayIndex { index: i32 },
    ClassCast,
    Arithmetic,
    NegativeArraySize { size: i32 },
    IllegalMonitor,
    ArrayStore,
}

impl Fault {
    /// 对应 Java 异常类的内部名。
    pub fn java_name(self) -> &'static str {
        match self {
            Fault::NullPointer => "java/lang/NullPointerException",
            Fault::ArrayIndex { .. } => "java/lang/ArrayIndexOutOfBoundsException",
            Fault::ClassCast => "java/lang/ClassCastException",
            Fault::Arithmetic => "java/lang/ArithmeticException",
            Fault::NegativeArraySize { .. } => "java/lang/NegativeArraySizeException",
            Fault::IllegalMonitor => "java/lang/IllegalMonitorStateException",
            Fault::ArrayStore => "java/lang/ArrayStoreException",
        }
    }

    /// 异常类名链（自身 → 祖先），处理器按名匹配（无需加载异常类层次）。
    pub fn chain(self) -> &'static [&'static str] {
        match self {
            Fault::NullPointer => &[
                "java/lang/NullPointerException",
                "java/lang/RuntimeException",
                "java/lang/Exception",
                "java/lang/Throwable",
            ],
            Fault::ArrayIndex { .. } => &[
                "java/lang/ArrayIndexOutOfBoundsException",
                "java/lang/IndexOutOfBoundsException",
                "java/lang/RuntimeException",
                "java/lang/Exception",
                "java/lang/Throwable",
            ],
            Fault::ClassCast => &[
                "java/lang/ClassCastException",
                "java/lang/RuntimeException",
                "java/lang/Exception",
                "java/lang/Throwable",
            ],
            Fault::Arithmetic => &[
                "java/lang/ArithmeticException",
                "java/lang/RuntimeException",
                "java/lang/Exception",
                "java/lang/Throwable",
            ],
            Fault::NegativeArraySize { .. } => &[
                "java/lang/NegativeArraySizeException",
                "java/lang/RuntimeException",
                "java/lang/Exception",
                "java/lang/Throwable",
            ],
            Fault::IllegalMonitor => &[
                "java/lang/IllegalMonitorStateException",
                "java/lang/RuntimeException",
                "java/lang/Exception",
                "java/lang/Throwable",
            ],
            Fault::ArrayStore => &[
                "java/lang/ArrayStoreException",
                "java/lang/RuntimeException",
                "java/lang/Exception",
                "java/lang/Throwable",
            ],
        }
    }

    /// detailMessage 文本。
    fn message(self) -> String {
        match self {
            Fault::NullPointer => "null reference".into(),
            Fault::ArrayIndex { index } => format!("array index out of bounds: {index}"),
            Fault::ClassCast => "class cast error".into(),
            Fault::Arithmetic => "arithmetic error".into(),
            Fault::NegativeArraySize { size } => format!("negative array size: {size}"),
            Fault::IllegalMonitor => "monitor not held".into(),
            Fault::ArrayStore => "array store type mismatch".into(),
        }
    }
}

/// run() 的终态。
#[derive(Debug)]
pub enum RunOutcome {
    /// 根帧返回（void 为 None）。
    Returned(Option<Slot>),
    /// 异常逃逸出根帧。
    Uncaught(Unwind),
    /// monitorenter 竞争失败：pc 未推进，调度器稍后重试。
    BlockedOnMonitor(ObjRef),
    /// 指令预算耗尽：状态可续跑（再次调用 run）。
    BudgetExhausted,
    /// 嵌套执行界（JIT invoke 助手）：被调帧全部返回，控制权交回
    /// JIT 生成码（返回值已压入其操作数栈）。
    FramePopped,
    /// 绿线程让出（T033）：sleep/yield/join 轮询版经 native 错误协议
    /// 上抛转此。调用指令 pc **已推进**（唤醒后从下一条指令续跑，
    /// 不重执行让出调用——否则 sleep 唤醒后再次 yield 死循环）；
    /// 调度器据此转 BlockedWake（Sleep）或保持 Runnable（Cpu）。
    Yielded(crate::YieldKind),
}

/// 单条指令的执行结果（run 循环据此推进 pc）。
#[derive(Debug)]
pub(crate) enum InsnOutcome {
    /// pc += 1。
    Next,
    /// pc 已就位（分支/压帧/触发重执行），不再推进。
    Jumped,
    /// 被调帧/clinit 链已压入，栈顶已切换。
    Pushed,
    /// 帧已弹空（仅根帧 return）。
    Returned(Option<Slot>),
    /// monitorenter 阻塞。
    Blocked(ObjRef),
    /// 绿线程让出（T033）：native 错误协议转正常控制流，run 循环
    /// 直接透传为 RunOutcome::Yielded（不走异常展开）。
    Yielded(crate::YieldKind),
    /// 异常展开（run 循环接手处理器搜索）。
    Unwind(Unwind),
}

// ---- 栈/局部表操作（以帧栈为参，便于与 Machine 调用交错） ---------------

pub(crate) fn push1(frames: &mut [Frame], slot: Slot) {
    frames
        .last_mut()
        .expect("frame stack empty")
        .stack
        .push(slot);
}

pub(crate) fn pop1(frames: &mut [Frame]) -> Result<Slot, KvmError> {
    frames
        .last_mut()
        .and_then(|frame| frame.stack.pop())
        .ok_or_else(|| KvmError::Internal("operand stack underflow".into()))
}

/// 按类别压栈：cat2 值补 Pad。
pub(crate) fn push_kind(frames: &mut [Frame], kind: Kind, slot: Slot) {
    push1(frames, slot);
    if matches!(kind, Kind::Long | Kind::Double) {
        push1(frames, Slot::Pad);
    }
}

/// 按类别弹栈：cat2 值先弹 Pad 再弹值。
pub(crate) fn pop_kind(frames: &mut [Frame], kind: Kind) -> Result<Slot, KvmError> {
    if matches!(kind, Kind::Long | Kind::Double) {
        let pad = pop1(frames)?;
        if pad != Slot::Pad {
            // 取证增强：栈顶实际槽位 + pc + 栈顶快照（反推压栈序列）。
            let pc = frames.last().map(|f| f.pc).unwrap_or(0);
            let top: Vec<String> = frames
                .last()
                .map(|f| {
                    f.stack
                        .iter()
                        .rev()
                        .take(6)
                        .map(|s| match s {
                            Slot::Int(v) => format!("I{v}"),
                            Slot::Long(v) => format!("L{v}"),
                            other => format!("{other:?}"),
                        })
                        .collect()
                })
                .unwrap_or_default();
            return Err(KvmError::Internal(format!(
                "expected category-2 pad slot, got {pad:?} @pc {pc} stack[{}]",
                top.join(", ")
            )));
        }
    }
    pop1(frames)
}

pub(crate) fn pop_int(frames: &mut [Frame]) -> Result<i32, KvmError> {
    match pop1(frames)? {
        Slot::Int(value) => Ok(value),
        slot => Err(KvmError::Internal(format!("expected int on stack, got {slot:?}"))),
    }
}

pub(crate) fn pop_ref(frames: &mut [Frame]) -> Result<ObjRef, KvmError> {
    match pop1(frames)? {
        Slot::Ref(reference) => Ok(reference),
        slot => Err(KvmError::Internal(format!(
            "expected reference on stack, got {slot:?}"
        ))),
    }
}

/// 探针开关快照（进程首读后缓存——热路径每指令级 env 查找开销归零；
/// 探针为排查工具，运行期不翻转语义）。
pub(crate) fn trace_flag(name: &str) -> bool {
    use std::sync::OnceLock;
    static UNWIND: OnceLock<bool> = OnceLock::new();
    if name == "SKYENGINE_KVM_TRACE_UNWIND" {
        return *UNWIND.get_or_init(|| std::env::var_os(name).is_some());
    }
    std::env::var_os(name).is_some()
}

/// 探针过滤值快照（首读后缓存）。`None` = 未设置；热路径每次调用前
/// 先查快照，避免逐调用 env 解析（含 UTF-8 校验与分配）。
pub(crate) fn trace_filter(name: &str) -> Option<&'static str> {
    use std::sync::OnceLock;
    static INVOKE: OnceLock<Option<String>> = OnceLock::new();
    debug_assert_eq!(name, "SKYENGINE_KVM_TRACE_INVOKE");
    INVOKE
        .get_or_init(|| std::env::var(name).ok())
        .as_deref()
}

/// 按槽自身类别压栈（原生返回值等无描述符语境）。
pub(crate) fn push_slot_value(frames: &mut [Frame], slot: Slot) {
    let cat2 = matches!(slot, Slot::Long(_) | Slot::Double(_));
    push1(frames, slot);
    if cat2 {
        push1(frames, Slot::Pad);
    }
}

// ---- 类型系统 ------------------------------------------------------------

/// 赋值兼容判定（checkcast/instanceof/处理器匹配）。
pub fn is_assignable(cs: &ClassSpace, from: ClassId, to: ClassId) -> bool {
    if from == to {
        return true;
    }
    // 目标是 java/lang/Object（object_class 未必已设置，按名兜底）。
    if Some(to) == cs.object_class || cs.name_of(to) == "java/lang/Object" {
        return true;
    }
    let from_class = cs.class(from);
    let to_class = cs.class(to);
    match (from_class.is_array, to_class.is_array) {
        // 数组：组件类型递归兼容（协变；原生类型精确相等）。
        (true, true) => match (from_class.array_elem, to_class.array_elem) {
            (Some(ArrayElem::Primitive(a)), Some(ArrayElem::Primitive(b))) => a == b,
            (Some(ArrayElem::Class(a)), Some(ArrayElem::Class(b))) => is_assignable(cs, a, b),
            _ => false,
        },
        // 普通类/接口：子类链 + 接口闭包。
        (false, false) => cs.is_subclass_of(from, to),
        _ => false,
    }
}

use crate::runtime::classspace::ArrayElem;

/// 对象的运行时类。
pub fn runtime_class_of(cs: &ClassSpace, heap: &Heap, obj: ObjRef) -> Result<ClassId, KvmError> {
    match heap.payload(obj)? {
        Payload::Instance(instance) => Ok(instance.class),
        Payload::Array(array) => Ok(array.class),
        // String 载荷的类需要引导期加载的 java/lang/String（T010）。
        Payload::Str(_) => cs.find("java/lang/String").ok_or_else(|| {
            KvmError::Internal("java/lang/String not loaded (bootstrap pending, T010)".into())
        }),
    }
}

/// 解析方法描述符：(参数类别序列, 返回类别；void 为 None)。
pub(crate) fn parse_method_desc(
    desc: &str,
) -> Result<(Vec<Kind>, Option<Kind>), KvmError> {
    let mut chars = desc.chars();
    if chars.next() != Some('(') {
        return Err(KvmError::Internal(format!("bad method descriptor {desc}")));
    }
    let mut args = Vec::new();
    loop {
        match chars.next() {
            Some(')') => break,
            Some(c) => args.push(parse_type(&mut chars, c)?),
            None => return Err(KvmError::Internal(format!("bad method descriptor {desc}"))),
        }
    }
    let ret = match chars.next() {
        Some('V') => None,
        Some(c) => Some(parse_type(&mut chars, c)?),
        None => return Err(KvmError::Internal(format!("bad method descriptor {desc}"))),
    };
    Ok((args, ret))
}

fn parse_type(chars: &mut std::str::Chars<'_>, first: char) -> Result<Kind, KvmError> {
    match first {
        'B' | 'C' | 'S' | 'Z' | 'I' => Ok(Kind::Int),
        'J' => Ok(Kind::Long),
        'F' => Ok(Kind::Float),
        'D' => Ok(Kind::Double),
        'L' => {
            for c in chars.by_ref() {
                if c == ';' {
                    return Ok(Kind::Ref);
                }
            }
            Err(KvmError::Internal("unterminated L type in descriptor".into()))
        }
        '[' => loop {
            match chars.next() {
                Some('[') => continue,
                Some(c) => {
                    parse_type(chars, c)?;
                    return Ok(Kind::Ref);
                }
                None => {
                    return Err(KvmError::Internal(
                        "unterminated array type in descriptor".into(),
                    ))
                }
            }
        },
        other => Err(KvmError::Internal(format!("bad type char {other:?}"))),
    }
}

// ---- GC 接线 -------------------------------------------------------------

/// 全部帧的局部表与操作数栈作为 GC 根。
fn frame_roots(frames: &[Frame]) -> Vec<Slot> {
    let mut roots = Vec::new();
    for frame in frames {
        roots.extend(frame.locals.iter().copied());
        roots.extend(frame.stack.iter().copied());
    }
    roots
}

/// 分配（带 GC 重试）：快路径直接分配；OOM 时以帧槽 + 静态区 +
/// `extra`（分配方持有的临时引用，如多维数组的父层）为根收集重试一次。
pub(crate) fn alloc_gc<T>(
    machine: &mut Machine,
    frames: &[Frame],
    extra: &[ObjRef],
    alloc: impl Fn(&mut Heap) -> Result<T, KvmError>,
) -> Result<T, KvmError> {
    match alloc(machine.heap) {
        Ok(value) => Ok(value),
        Err(KvmError::OutOfMemory) => {
            let mut roots = frame_roots(frames);
            roots.extend(extra.iter().map(|r| Slot::Ref(*r)));
            roots.extend(gc::static_roots(machine.cs));
            alloc_with_gc(machine.heap, &roots, alloc)
        }
        Err(error) => Err(error),
    }
}

// ---- clinit 链 -----------------------------------------------------------

/// 驻留类的静态字符串常量（初始化期执行；取空后幂等）。
fn intern_pending_str_consts(
    machine: &mut Machine,
    class: ClassId,
) -> Result<(), KvmError> {
    let pending = std::mem::take(&mut machine.cs.class_mut(class).pending_str_consts);
    for (slot, sym) in pending {
        let chars: Vec<u16> = machine
            .cs
            .syms
            .name(sym)
            .encode_utf16()
            .collect();
        let reference = machine.heap.alloc_string(chars)?;
        machine.cs.class_mut(class).statics[slot as usize] = Slot::Ref(reference);
    }
    Ok(())
}

/// 类的 `<clinit>` 方法索引（无则 None）。
fn clinit_index(cs: &mut ClassSpace, class: ClassId) -> Option<u16> {
    let name = cs.syms.intern("<clinit>");
    let desc = cs.syms.intern("()V");
    cs.class(class).declared_method(name, desc)
}

/// 待初始化链推进：取链首类，驻留其静态字符串常量并压 `<clinit>` 帧；
/// 无 `<clinit>` 的类就地标 Initialized 继续。返回是否压入了帧。
pub(crate) fn advance_clinit_chain(
    machine: &mut Machine,
    frames: &mut Vec<Frame>,
    chain: &[ClassId],
) -> Result<bool, KvmError> {
    let mut index = 0;
    while index < chain.len() {
        let class = chain[index];
        intern_pending_str_consts(machine, class)?;
        let Some(method_index) = clinit_index(machine.cs, class) else {
            machine.cs.class_mut(class).init_state = InitState::Initialized;
            index += 1;
            continue;
        };
        // 原生 <clinit>（bootstrap 注册表类，如 java/lang/System）：
        // 无字节码体，同步分派；成功就地标 Initialized 推进链，
        // 失败将余链全部标 Failed（与字节码 clinit 异常语义一致）。
        let has_body = machine.cs.class(class).methods[method_index as usize]
            .body
            .is_some();
        if !has_body {
            let name = machine.cs.syms.intern("<clinit>");
            let desc = machine.cs.syms.intern("()V");
            // clinit native 通道无线程语境（bootstrap <clinit> 不调
            // Thread.currentThread），填保留 id 0（T033）。
            let call = NativeCall {
                class,
                name,
                desc,
                this: None,
                args: Vec::new(),
                thread: ThreadId(0),
            };
            if let Err(error) =
                machine.natives.call_native(machine.cs, machine.heap, machine.host, call)
            {
                for failed in &chain[index..] {
                    machine.cs.class_mut(*failed).init_state = InitState::Failed;
                }
                return Err(error);
            }
            machine.cs.class_mut(class).init_state = InitState::Initialized;
            index += 1;
            continue;
        }
        let body = machine.cs.class(class).methods[method_index as usize]
            .body
            .clone()
            .ok_or_else(|| KvmError::Linkage("<clinit> has no code".into()))?;
        let max_locals = body.max_locals;
        let max_stack = body.max_stack;
        frames.push(Frame {
            class,
            method: method_index,
            body,
            locals: vec![Slot::Int(0); max_locals],
            stack: Vec::with_capacity(max_stack),
            pc: 0,
            caller_pc: None,
            kind: FrameKind::Clinit {
                class,
                rest: chain[index + 1..].to_vec(),
            },
        });
        return Ok(true);
    }
    Ok(false)
}

/// 主动使用触发类初始化（JLS 12.4）。返回 true 表示压入了 clinit 帧
/// （触发指令不得推进 pc，链完成后重执行）。
pub(crate) fn ensure_initialized(
    machine: &mut Machine,
    frames: &mut Vec<Frame>,
    trigger: ClassId,
) -> Result<bool, KvmError> {
    if machine.cs.class(trigger).is_array {
        return Ok(false);
    }
    match machine.cs.class(trigger).init_state {
        // 已初始化 / 初始化中（本线程递归触发：读默认值，JLS 12.4.2）。
        InitState::Initialized | InitState::Initializing => return Ok(false),
        InitState::Failed => {
            return Err(KvmError::Linkage(format!(
                "class {} failed initialization earlier",
                machine.cs.name_of(trigger)
            )))
        }
        InitState::Loaded => {}
    }
    // 收集未初始化超类链（触发类 → 根），翻转成根优先。
    let mut chain = Vec::new();
    let mut current = Some(trigger);
    while let Some(class) = current {
        match machine.cs.class(class).init_state {
            InitState::Loaded => chain.push(class),
            // 链上遇到已处理的类即停（其父链必然已处理）。
            InitState::Initializing | InitState::Initialized => break,
            InitState::Failed => {
                return Err(KvmError::Linkage(format!(
                    "class {} failed initialization earlier",
                    machine.cs.name_of(class)
                )))
            }
        }
        current = machine.cs.ensure_super(class)?;
    }
    chain.reverse();
    for class in &chain {
        machine.cs.class_mut(*class).init_state = InitState::Initializing;
    }
    advance_clinit_chain(machine, frames, &chain)
}

// ---- 帧构造 --------------------------------------------------------------

/// 组装调用帧：局部表 = [this] + 参数（cat2 占双槽），其余补零。
// 帧组装上下文（机器+帧栈+类+方法）参数宽度为域约束，非拆分可读。
#[allow(clippy::too_many_arguments)]
pub(crate) fn push_method_frame(
    machine: &mut Machine,
    frames: &mut Vec<Frame>,
    class: ClassId,
    method_index: u16,
    body: Rc<CodeBody>,
    this: Option<ObjRef>,
    args: Vec<Slot>,
    caller_pc: Option<u32>,
) -> Result<(), KvmError> {
    let desc_sym = machine.cs.class(class).methods[method_index as usize].desc;
    let arg_kinds = machine.cs.descriptor_kinds(desc_sym)?;
    let mut locals: Vec<Slot> = Vec::with_capacity(body.max_locals);
    if let Some(reference) = this {
        locals.push(Slot::Ref(reference));
    }
    let mut values = args.into_iter();
    for kind in arg_kinds.0.iter() {
        let value = values
            .next()
            .ok_or_else(|| KvmError::Internal("missing argument".into()))?;
        locals.push(value);
        if matches!(kind, Kind::Long | Kind::Double) {
            locals.push(Slot::Pad);
        }
    }
    locals.resize(body.max_locals, Slot::Int(0));
    let max_stack = body.max_stack;
    frames.push(Frame {
        class,
        method: method_index,
        body,
        locals,
        stack: Vec::with_capacity(max_stack),
        pc: 0,
        caller_pc,
        kind: FrameKind::Method,
    });
    Ok(())
}

/// 建立根帧（测试/调度器入口）：解析方法 → 压帧 → 触发类初始化。
/// 若压入 clinit 帧，run() 从栈顶执行，clinit 链先于方法体完成。
pub fn enter(
    machine: &mut Machine,
    frames: &mut Vec<Frame>,
    class: ClassId,
    name: &str,
    desc: &str,
    args: Vec<Slot>,
) -> Result<(), KvmError> {
    let defining = push_entry_frame(machine, frames, class, name, desc, None, args)?;
    ensure_initialized(machine, frames, defining)?;
    Ok(())
}

/// 入口构帧（T013 从 `enter` 抽出）：解析方法并压帧，**不**触发类初始化。
///
/// `Vm::start_midlet` 需要自定帧序（栈底→顶：startApp → `<init>` → clinit 链，
/// run 从栈顶执行即得"初始化 → 构造 → startApp"的语义顺序），故把
/// clinit 触发从构帧中剥离，由调用方在整栈就位后经 `ensure_initialized`
/// 统一压顶。返回方法定义类的 id。
pub(crate) fn push_entry_frame(
    machine: &mut Machine,
    frames: &mut Vec<Frame>,
    class: ClassId,
    name: &str,
    desc: &str,
    this: Option<ObjRef>,
    args: Vec<Slot>,
) -> Result<ClassId, KvmError> {
    let name_sym = machine.cs.syms.intern(name);
    let desc_sym = machine.cs.syms.intern(desc);
    let (defining, method_index) = machine
        .cs
        .find_method(class, name_sym, desc_sym)
        .ok_or_else(|| {
            KvmError::Linkage(format!(
                "method {name}{desc} not found on {}",
                machine.cs.name_of(class)
            ))
        })?;
    let body = machine.cs.class(defining).methods[method_index as usize]
        .body
        .clone()
        .ok_or_else(|| KvmError::Linkage("entry method is native/abstract".into()))?;
    push_method_frame(machine, frames, defining, method_index, body, this, args, None)?;
    Ok(defining)
}

// ---- 异常展开 ------------------------------------------------------------

/// 在帧的处理器表中按故障点 pc 匹配。
fn find_handler(
    cs: &ClassSpace,
    heap: &Heap,
    body: &CodeBody,
    pc: u32,
    unwind: &Unwind,
) -> Option<u32> {
    for handler in &body.handlers {
        if !(pc >= handler.start && pc < handler.end) {
            continue;
        }
        let matched = match (&handler.catch_class, unwind) {
            // catch_type 0 = finally 兜底。
            (None, _) => true,
            (Some(catch), Unwind::Thrown(exception)) => {
                match runtime_class_of(cs, heap, *exception) {
                    Ok(exception_class) => is_assignable(cs, exception_class, *catch),
                    Err(_) => false,
                }
            }
            (Some(catch), Unwind::Fault(fault)) => fault
                .chain()
                .iter()
                .any(|name| *name == cs.name_of(*catch)),
        };
        if matched {
            return Some(handler.handler);
        }
    }
    None
}

/// 故障 → 异常实例：加载异常类 → 模板分配 → 写 detailMessage（若有）。
fn materialize_fault(
    machine: &mut Machine,
    frames: &mut [Frame],
    fault: Fault,
) -> Result<ObjRef, KvmError> {
    let class = machine.cs.load_class(fault.java_name())?;
    // detailMessage（标准异常经继承持有；未声明则跳过）。
    let detail_slot = {
        let name = machine.cs.syms.intern("detailMessage");
        let desc = machine.cs.syms.intern("Ljava/lang/String;");
        machine.cs.find_field(class, name, desc).ok()
    };
    // 先驻留消息（其自身是临时引用，作为实例分配的附加根）。
    let message = if detail_slot.is_some() {
        let chars: Vec<u16> = fault.message().encode_utf16().collect();
        Some(alloc_gc(machine, frames, &[], |heap| {
            heap.alloc_string(chars.clone())
        })?)
    } else {
        None
    };
    let template = machine.cs.class(class).instance_template.clone();
    let extra: Vec<ObjRef> = message.into_iter().collect();
    let exception = alloc_gc(machine, frames, &extra, |heap| {
        heap.alloc_instance(class, template.clone())
    })?;
    if let (Some(slot), Some(message)) = (detail_slot, message) {
        let instance = machine.heap.instance_mut(exception)?;
        instance.fields[slot.slot as usize] = Slot::Ref(message);
    }
    Ok(exception)
}

/// unwind 调试文本（TRACE_UNWIND 打印面）：异常类名 + detailMessage，
/// 或故障类型——SP-1 排查斯凯链死点时从"哪类异常在哪个帧"直接定位
/// （如 anchor IllegalArgumentException / 资源 NPE 的具体消息）。
fn unwind_reason(machine: &mut Machine, unwind: Unwind) -> String {
    match unwind {
        Unwind::Thrown(obj) => {
            let class_name = runtime_class_of(machine.cs, machine.heap, obj)
                .map(|cid| machine.cs.name_of(cid).to_string())
                .unwrap_or_else(|_| "?".into());
            let message = (|| -> Option<String> {
                let class = runtime_class_of(machine.cs, machine.heap, obj).ok()?;
                let name = machine.cs.syms.intern("detailMessage");
                let desc = machine.cs.syms.intern("Ljava/lang/String;");
                let field = machine.cs.find_field(class, name, desc).ok()?;
                let slot = machine
                    .heap
                    .instance(obj)
                    .ok()?
                    .fields[field.slot as usize];
                match slot {
                    Slot::Ref(reference) if !reference.is_null() => Some(String::from_utf16_lossy(
                        &machine.heap.str_obj(reference).ok()?.chars,
                    )),
                    _ => None,
                }
            })()
            .unwrap_or_default();
            format!("thrown {class_name} {message}")
        }
        Unwind::Fault(fault) => format!("fault {fault:?}"),
    }
}

/// 异常展开：自栈顶帧向下搜索处理器。
/// 返回 `Some(Uncaught)` 表示逃逸；None 表示已捕获（栈顶帧已就位处理器入口）。
fn handle_unwind(
    machine: &mut Machine,
    frames: &mut Vec<Frame>,
    unwind: Unwind,
    stop: Option<usize>,
) -> Result<Option<RunOutcome>, KvmError> {
    loop {
        // 嵌套界（JIT 调用帧，无处理器）：不弹、交回外层继续搜索。
        if let Some(stop_len) = stop
            && frames.len() == stop_len
        {
            return Ok(Some(RunOutcome::Uncaught(unwind)));
        }
        let (pc, body) = {
            let Some(frame) = frames.last() else {
                return Ok(Some(RunOutcome::Uncaught(unwind)));
            };
            (frame.pc, Rc::clone(&frame.body))
        };
        let Some(handler_pc) = find_handler(machine.cs, machine.heap, &body, pc, &unwind) else {
            // 本帧不捕获：弹出。clinit 帧链整体粘滞标 Failed。
            let popped = frames.pop().expect("frame present");
            // 调试面：SKYENGINE_KVM_TRACE_UNWIND=1 时打印弹出的类.方法@pc。
            if trace_flag("SKYENGINE_KVM_TRACE_UNWIND") {
                let reason = unwind_reason(machine, unwind);
                let method = &machine.cs.class(popped.class).methods[popped.method as usize];
                eprintln!(
                    "[kvm] unwind pops {}.{:?}{:?} @pc {} [{reason}]",
                    machine.cs.name_of(popped.class),
                    machine.cs.syms.name(method.name),
                    machine.cs.syms.name(method.desc),
                    popped.pc
                );
            }
            if let FrameKind::Clinit { class, rest } = popped.kind {
                machine.cs.class_mut(class).init_state = InitState::Failed;
                for pending in rest {
                    machine.cs.class_mut(pending).init_state = InitState::Failed;
                }
            }
            // 恢复调用帧 pc 到调用点，继续搜其处理器表
            //（clinit 帧 caller_pc 为 None：触发帧本就停在触发指令上）。
            if let Some(caller_pc) = popped.caller_pc {
                frames.last_mut().expect("caller present").pc = caller_pc;
            }
            continue;
        };
        // 捕获：Fault 物化为异常实例；Thrown 直接用对象引用。
        let exception = match unwind {
            Unwind::Thrown(reference) => reference,
            Unwind::Fault(fault) => materialize_fault(machine, frames, fault)?,
        };
        let frame = frames.last_mut().expect("frame present");
        frame.stack.clear();
        frame.stack.push(Slot::Ref(exception));
        frame.pc = handler_pc;
        return Ok(None);
    }
}

// ---- 主循环 --------------------------------------------------------------

/// 执行直到根帧返回/异常逃逸/monitor 阻塞/预算耗尽。
///
/// `budget`：本次调用最多执行的指令数；耗尽返回 `BudgetExhausted`，
/// 状态可续跑。`None` = 不限。
pub fn run(
    machine: &mut Machine,
    frames: &mut Vec<Frame>,
    thread: ThreadId,
    budget: Option<u32>,
) -> Result<RunOutcome, KvmError> {
    run_inner(machine, frames, thread, budget, None)
}

/// 嵌套执行界（JIT invoke 助手用）：`Some((stop_len, stop_pc))` 时，
/// 帧栈回到 stop_len 且顶帧 pc != stop_pc（被调方返回、值已就位）即返
/// 回 `FramePopped`；pc == stop_pc 是 clinit 链完成后的重执行锚点，继续。
pub(crate) fn run_inner(
    machine: &mut Machine,
    frames: &mut Vec<Frame>,
    thread: ThreadId,
    budget: Option<u32>,
    stop: Option<(usize, u32)>,
) -> Result<RunOutcome, KvmError> {
    let mut remaining = budget;
    loop {
        if frames.is_empty() {
            return Err(KvmError::Internal("run with empty frame stack".into()));
        }
        if let Some(count) = remaining.as_mut() {
            if *count == 0 {
                return Ok(RunOutcome::BudgetExhausted);
            }
            *count -= 1;
        }
        if let Some((stop_len, stop_pc)) = stop
            && frames.len() == stop_len
            && frames.last().expect("checked non-empty").pc != stop_pc
        {
            return Ok(RunOutcome::FramePopped);
        }
        // 取当前指令：克隆 body 的 Rc（本地变量保活越过帧的可变借用）
        // 后按引用执行——消除逐指令的 Insn 深克隆。
        let (body, pc, class_id, method_index) = {
            let frame = frames.last_mut().expect("checked non-empty");
            (
                Rc::clone(&frame.body),
                frame.pc as usize,
                frame.class,
                frame.method,
            )
        };
        // JIT 接管（x86_64）：方法入口（pc==0）且有编译产物时整段执行；
        // 返回 → do_return 同路；退火（含除零重执行）→ 解释器继续步进。
        #[cfg(target_arch = "x86_64")]
        if pc == 0 && crate::jit::jit_enabled() {
            if let Some(code) = machine.cs.jit_cache_get(class_id, method_index) {
                let mut unlimited = u32::MAX;
                let budget_ptr = remaining.as_mut().unwrap_or(&mut unlimited);
                match crate::jit::execute_leaf(&code, machine, frames, thread, budget_ptr) {
                    crate::jit::JitExit::Returned(value) => {
                        match opcodes::do_return(machine, frames, value)? {
                            InsnOutcome::Returned(value) => {
                                return Ok(RunOutcome::Returned(value))
                            }
                            _ => {}
                        }
                        continue;
                    }
                    crate::jit::JitExit::Deopt => continue,
                    crate::jit::JitExit::Unwind(unwind) => {
                        match handle_unwind(machine, frames, unwind, None)? {
                            Some(outcome) => return Ok(outcome),
                            None => continue,
                        }
                    }
                    crate::jit::JitExit::Suspended(kind) => {
                        return Ok(RunOutcome::Yielded(kind))
                    }
                    crate::jit::JitExit::Blocked(object) => {
                        return Ok(RunOutcome::BlockedOnMonitor(object))
                    }
                }
            }
        }
        let Some(insn) = body.insns.get(pc) else {
            return Err(KvmError::Internal(format!("pc {pc} out of bounds")));
        };
        let executed = execute_insn(machine, frames, thread, insn);
        if let Err(KvmError::Internal(text)) = &executed {
            // cat2 栈错位取证：错误发生帧的类.方法（配合 pc/栈快照）。
            if text.starts_with("expected category-2") {
                if let Some(frame) = frames.last() {
                    let method = &machine.cs.class(frame.class).methods[frame.method as usize];
                    eprintln!(
                        "[kvm] cat2 fault in {}.{:?}{:?} @pc {}",
                        machine.cs.name_of(frame.class),
                        machine.cs.syms.name(method.name),
                        machine.cs.syms.name(method.desc),
                        frame.pc
                    );
                }
            }
        }
        match executed? {
            InsnOutcome::Next => {
                frames.last_mut().expect("frame present").pc += 1;
            }
            InsnOutcome::Jumped => {}
            InsnOutcome::Pushed => {}
            InsnOutcome::Returned(value) => return Ok(RunOutcome::Returned(value)),
            InsnOutcome::Blocked(object) => return Ok(RunOutcome::BlockedOnMonitor(object)),
            // T033 让出：正常控制流直达终态（pc 已推进，帧状态可续跑）。
            InsnOutcome::Yielded(kind) => return Ok(RunOutcome::Yielded(kind)),
            InsnOutcome::Unwind(unwind) => {
                // 调试面：SKYENGINE_KVM_TRACE_UNWIND=1 时异常展开开始即
                // 打一行（含随后被游戏 catch 吞掉的——"关于页空白"类
                // 问题需要看到被捕获的异常）。
                if trace_flag("SKYENGINE_KVM_TRACE_UNWIND") {
                    if let Some(frame) = frames.last() {
                        let reason = unwind_reason(machine, unwind);
                        let method = &machine.cs.class(frame.class).methods[frame.method as usize];
                        let insn_text = frame
                            .body
                            .insns
                            .get(frame.pc as usize)
                            .map(|i| match i {
                                crate::classfile::code::Insn::InvokeVirtual(mref)
                                | crate::classfile::code::Insn::InvokeSpecial(mref)
                                | crate::classfile::code::Insn::InvokeStatic(mref)
                                | crate::classfile::code::Insn::InvokeInterface(mref) => format!(
                                    "{i:?} {}.{:?}{:?}",
                                    machine.cs.name_of(mref.class),
                                    machine.cs.syms.name(mref.name),
                                    machine.cs.syms.name(mref.desc)
                                ),
                                _ => format!("{i:?}"),
                            })
                            .unwrap_or_else(|| "<past end>".into());
                        eprintln!(
                            "[kvm] unwind begins in {}.{:?}{:?} @pc {} {insn_text} [{reason}]",
                            machine.cs.name_of(frame.class),
                            machine.cs.syms.name(method.name),
                            machine.cs.syms.name(method.desc),
                            frame.pc
                        );
                    }
                }
                if let Some(outcome) =
                    handle_unwind(machine, frames, unwind, stop.map(|(len, _)| len))?
                {
                    return Ok(outcome);
                }
            }
        }
    }
}
