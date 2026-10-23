//! java.lang 原生实现（T014）。
//!
//! 函数签名与 `interp::natives::NativeFn` 一致，由 bootstrap 注册表
//! （natives.rs 的 java/lang 段）按 类名.方法名+描述符 绑定。
//!
//! 关键形态：
//! - **字符串**：沿用堆内 `Payload::Str`（UTF-16 码元 + 惰性哈希）。
//!   `new String(...)` 的 `<init>` 把 `new` 分配的实例载荷原地替换成
//!   Str，与 ldc 驻留字面量共用一套表示；计算结果（substring/concat/
//!   toString 等）走 `alloc_string_uninterned` 保持 JVM "新对象" 语义，
//!   仅 `intern` 与 ldc 走驻留表。
//! - **异常抛出**：原生方法无返回异常的通道，借道 `KvmError::Throw`
//!   （T014 新增），解释器 invoke 原生分派点拦截并转 `Unwind::Thrown`。
//! - **Class 对象**：java/lang/Class 实例以 int 字段 `classId` 持类表
//!   下标（最小表示；按类单例缓存留待后续任务）。
//! - **StringBuffer**：实例字段 `value: [C` + `count: I`（对齐 JDK 布局），
//!   缓冲不足按 2×+2 扩容。
//!
//! 已知边界（保守实现，注释逐处标注）：
//! - toUpperCase/toLowerCase/Character.isXXX 仅覆盖 ASCII（CLDC 游戏
//!   语料以 ASCII 为主，Unicode 表留待需要时补）。
//! - Object.wait/notify/notifyAll 保持桩：桥接需要 NativeFn 携带
//!   Monitors/ThreadId 与调度器等待停车，随调度整合任务落实。
//! - fillInStackTrace 为空实现：帧栈对原生方法不可达（TODO T045）。


use crate::host::HostContext;
use crate::interp::runtime_class_of;
use crate::interp::NativeCall;
use crate::runtime::classspace::ClassSpace;
use crate::runtime::heap::Heap;
use crate::runtime::object::{ArrayData, ElemType, Payload, StrObj};
use crate::runtime::{ClassId, FieldRef, ObjRef, Slot};
use crate::{KvmError, YieldKind};

// ---------------------------------------------------------------------------
// 通用小工具
// ---------------------------------------------------------------------------

/// 实例方法接收者（解释器已保证非 null）。
fn this(call: &NativeCall) -> Result<ObjRef, KvmError> {
    call.this
        .ok_or_else(|| KvmError::Internal("instance native without receiver".into()))
}

fn arg_i32(call: &NativeCall, index: usize) -> Result<i32, KvmError> {
    match call.args.get(index) {
        Some(Slot::Int(value)) => Ok(*value),
        other => Err(KvmError::Internal(format!(
            "native arg {index} expected int, got {other:?}"
        ))),
    }
}

fn arg_i64(call: &NativeCall, index: usize) -> Result<i64, KvmError> {
    match call.args.get(index) {
        Some(Slot::Long(value)) => Ok(*value),
        other => Err(KvmError::Internal(format!(
            "native arg {index} expected long, got {other:?}"
        ))),
    }
}

fn arg_f32(call: &NativeCall, index: usize) -> Result<f32, KvmError> {
    match call.args.get(index) {
        Some(Slot::Float(value)) => Ok(*value),
        other => Err(KvmError::Internal(format!(
            "native arg {index} expected float, got {other:?}"
        ))),
    }
}

fn arg_f64(call: &NativeCall, index: usize) -> Result<f64, KvmError> {
    match call.args.get(index) {
        Some(Slot::Double(value)) => Ok(*value),
        other => Err(KvmError::Internal(format!(
            "native arg {index} expected double, got {other:?}"
        ))),
    }
}

fn arg_ref(call: &NativeCall, index: usize) -> Result<ObjRef, KvmError> {
    match call.args.get(index) {
        Some(Slot::Ref(value)) => Ok(*value),
        other => Err(KvmError::Internal(format!(
            "native arg {index} expected ref, got {other:?}"
        ))),
    }
}

fn utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

/// 读字符串内容（载荷必须是 Str）。
fn string_chars(heap: &Heap, reference: ObjRef) -> Result<Vec<u16>, KvmError> {
    Ok(heap.str_obj(reference)?.chars.clone())
}

fn new_string(heap: &mut Heap, chars: Vec<u16>) -> Result<ObjRef, KvmError> {
    heap.alloc_string_uninterned(chars)
}

fn ret_i32(value: i32) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Int(value)))
}

fn ret_bool(value: bool) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Int(i32::from(value))))
}

fn ret_ref(reference: ObjRef) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Ref(reference)))
}

fn ret_string(heap: &mut Heap, chars: Vec<u16>) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Ref(new_string(heap, chars)?)))
}

/// 内部名 → 点分名（Class.getName / toString 用）。
fn dotted_name(cs: &ClassSpace, class: ClassId) -> String {
    cs.name_of(class).replace('/', ".")
}

/// 引用兼容判定：父类链 + 接口闭包（instanceof/checkcast 语义）。
fn is_assignable(cs: &ClassSpace, from: ClassId, to: ClassId) -> bool {
    cs.is_subclass_of(from, to) || cs.interface_closure(from).contains(&to)
}

// ---------------------------------------------------------------------------
// 异常抛出通道
// ---------------------------------------------------------------------------

/// 物化异常实例（可带消息），失败时退回原错误（异常无法构造按 VM 故障）。
fn alloc_throwable(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    class_name: &str,
    message: Option<&str>,
) -> Result<ObjRef, KvmError> {
    let class = cs.load_class(class_name)?;
    let detail = detail_message_field(cs, class)?;
    let message_ref = match (detail, message) {
        (Some(_), Some(text)) => Some(heap.alloc_string(utf16(text))?),
        _ => None,
    };
    let template = cs.class(class).instance_template.clone();
    let exception = heap.alloc_instance(class, template)?;
    if let (Some(field), Some(message_ref)) = (detail, message_ref) {
        heap.instance_mut(exception)?.fields[field.slot as usize] = Slot::Ref(message_ref);
    }
    Ok(exception)
}

/// 抛 Java 异常：封装成 `KvmError::Throw`，由解释器 invoke 点转展开。
pub(crate) fn throw_java(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    class_name: &str,
    message: Option<&str>,
) -> KvmError {
    match alloc_throwable(cs, heap, class_name, message) {
        Ok(exception) => KvmError::Throw(exception),
        Err(error) => error,
    }
}

fn throw_npe(cs: &mut ClassSpace, heap: &mut Heap) -> KvmError {
    throw_java(cs, heap, "java/lang/NullPointerException", None)
}

/// Throwable.detailMessage 字段槽位（继承链查找；未声明返回 None）。
fn detail_message_field(
    cs: &mut ClassSpace,
    class: ClassId,
) -> Result<Option<FieldRef>, KvmError> {
    let name = cs.syms.intern("detailMessage");
    let desc = cs.syms.intern("Ljava/lang/String;");
    Ok(cs.find_field(class, name, desc).ok())
}

// ---------------------------------------------------------------------------
// Class 对象（int 字段 classId 持类表下标）
// ---------------------------------------------------------------------------

fn class_object_new(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    class: ClassId,
) -> Result<ObjRef, KvmError> {
    let cls = cs.load_class("java/lang/Class")?;
    let name = cs.syms.intern("classId");
    let desc = cs.syms.intern("I");
    let field = cs.find_field(cls, name, desc)?;
    let template = cs.class(cls).instance_template.clone();
    let object = heap.alloc_instance(cls, template)?;
    heap.instance_mut(object)?.fields[field.slot as usize] = Slot::Int(class.0 as i32);
    Ok(object)
}

fn class_object_id(cs: &mut ClassSpace, heap: &mut Heap, object: ObjRef) -> Result<ClassId, KvmError> {
    let class = heap.instance(object)?.class;
    let name = cs.syms.intern("classId");
    let desc = cs.syms.intern("I");
    let field = cs.find_field(class, name, desc)?;
    match heap.instance(object)?.fields[field.slot as usize] {
        Slot::Int(value) => Ok(ClassId(value as u32)),
        other => Err(KvmError::Internal(format!(
            "Class.classId expected int, got {other:?}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// 数值/文本格式化
// ---------------------------------------------------------------------------

/// Java 风格 double 文本（最小实现）：NaN/Infinity 精确；整值带 ".0"；
/// 其余取 Rust 最短往返表示（Java 科学计数 1.0E7 形态未对齐，对 CLDC
/// 游戏显示无影响）。
fn java_double_text(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if x == x.trunc() && x.abs() < 1e7 {
        return format!("{x:.1}");
    }
    format!("{x}")
}

/// 按基数转正整数文本（负数带符号；Long.MIN_VALUE 经 unsigned_abs 安全）。
fn magnitude_text(mut value: u64, radix: u32, negative: bool) -> String {
    let mut digits = Vec::new();
    if value == 0 {
        digits.push(b'0');
    }
    while value > 0 {
        let digit = (value % u64::from(radix)) as u32;
        digits.push(if digit < 10 {
            b'0' + digit as u8
        } else {
            b'a' + (digit - 10) as u8
        });
        value /= u64::from(radix);
    }
    if negative {
        digits.push(b'-');
    }
    digits.reverse();
    String::from_utf8(digits).expect("digit bytes are ascii")
}

fn long_text(value: i64) -> String {
    magnitude_text(value.unsigned_abs(), 10, value < 0)
}

fn long_text_radix(value: i64, radix: u32) -> String {
    magnitude_text(value.unsigned_abs(), radix, value < 0)
}

/// 按 Java 语义解析整数文本：可选 +/-、至少一位数字、溢出失败。
/// None = NumberFormatException。
fn parse_java_long(chars: &[u16], radix: u32) -> Option<i64> {
    if chars.is_empty() || !(2..=36).contains(&radix) {
        return None;
    }
    let mut index = 0usize;
    let negative = match chars[0] {
        0x2d => {
            index = 1;
            true
        }
        0x2b => {
            index = 1;
            false
        }
        _ => false,
    };
    if index >= chars.len() {
        return None;
    }
    let mut acc: i128 = 0;
    for &c in &chars[index..] {
        let digit = char_digit(c, radix)? as i128;
        acc = acc * radix as i128 + digit;
        if acc > i64::MAX as i128 + 1 {
            return None;
        }
    }
    if negative {
        if acc > i64::MAX as i128 + 1 {
            return None;
        }
        Some(-(acc as i64))
    } else if acc > i64::MAX as i128 {
        None
    } else {
        Some(acc as i64)
    }
}

fn parse_java_int(chars: &[u16], radix: u32) -> Option<i32> {
    parse_java_long(chars, radix).and_then(|value| i32::try_from(value).ok())
}

/// 字符数字值（0-9/a-z/A-Z），超出基数返回 None。
fn char_digit(c: u16, radix: u32) -> Option<u32> {
    let digit = match c {
        0x30..=0x39 => (c - 0x30) as u32,
        0x61..=0x7a => (c - 0x61) as u32 + 10,
        0x41..=0x5a => (c - 0x41) as u32 + 10,
        _ => return None,
    };
    if digit < radix {
        Some(digit)
    } else {
        None
    }
}

/// Java parseFloat/parseDouble 语义（最小实现）：首尾空白可容忍、
/// 尾缀 f/F/d/D 可选、NaN/Infinity 精确匹配；其余交给 Rust 解析
/// （Rust 对 "inf" 等形态比 Java 宽容，边界差异可忽略）。
fn parse_java_double(chars: &[u16]) -> Option<f64> {
    let text = String::from_utf16_lossy(chars);
    let trimmed = text.trim_matches(|c: char| c <= ' ');
    if trimmed.is_empty() {
        return None;
    }
    let mut core = trimmed;
    if let Some(last) = core.chars().last()
        && matches!(last, 'f' | 'F' | 'd' | 'D')
    {
        core = &core[..core.len() - last.len_utf8()];
    }
    match core {
        "NaN" => Some(f64::NAN),
        "Infinity" => Some(f64::INFINITY),
        "-Infinity" => Some(f64::NEG_INFINITY),
        "+Infinity" => Some(f64::INFINITY),
        _ => core.parse::<f64>().ok(),
    }
}

// ---------------------------------------------------------------------------
// Object
// ---------------------------------------------------------------------------

pub fn object_init(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(None)
}

/// 身份哈希：沿用 T010 形态（引用值即哈希，稳定且满足唯一性）。
pub fn object_hash_code(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Int(call.this.map(|r| r.0 as i32).unwrap_or(0))))
}

pub fn object_equals(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let same = match (call.this, call.args.first()) {
        (Some(a), Some(Slot::Ref(b))) => a == *b,
        _ => false,
    };
    ret_bool(same)
}

/// "ClassName@hex"（对齐 Object.toString；十六进制小写无符号）。
pub fn object_to_string(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let reference = this(call)?;
    let class = runtime_class_of(cs, heap, reference)?;
    let text = format!("{}@{:x}", dotted_name(cs, class), reference.0);
    ret_string(heap, utf16(&text))
}

pub fn object_get_class(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let reference = this(call)?;
    let class = runtime_class_of(cs, heap, reference)?;
    ret_ref(class_object_new(cs, heap, class)?)
}

// ---- Object.wait/notify/notifyAll（T034）-----------------------------------
//
// native 语境无 Monitors/调度器通道，故分两段：
// - **wait**（宿主轨）：解析超时参数后上抛 `Yield(YieldKind::Wait)`；
//   监视器的"完全释放 + 入等待队列"与持锁检查（未持锁 →
//   IllegalMonitorStateException）由解释器 native 分派点经
//   `Monitors::wait` 桥接（opcodes.rs exec_invoke）；线程状态转移
//   （BlockedWait）与唤醒/移交由调度器 poll 处理（scheduler.rs）。
// - **notify/notifyAll**：置 `cs.notify_requests` 请求位（锁对象、
//   请求线程、是否 all），同样由分派点**同步消费**——notify 的
//   IllegalMonitorStateException 必须在调用点立即抛出（JLS 17.14），
//   不能等 Vm.poll；实际唤醒在调用者释放锁后由调度器的
//   reacquire_ready 移交完成。

/// `Object.wait()V` / `wait(J)V`：无限等待（timeout=0 或缺参）或
/// 定时等待（wake_at = now + ms）。负超时 → IllegalArgumentException。
pub fn object_wait(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let obj = this(call)?;
    let (timeout_ms, wake_at_ms) = match call.args.first() {
        // wait()V 无参 / wait(0)：无限等待（不登 next_wake）。
        None => (0, None),
        Some(Slot::Long(0)) => (0, None),
        Some(Slot::Long(ms)) if *ms < 0 => {
            return Err(throw_java(
                cs,
                heap,
                "java/lang/IllegalArgumentException",
                Some("negative wait timeout"),
            ));
        }
        Some(Slot::Long(ms)) => {
            let ms = *ms as u64;
            (ms, Some(host.now_ms() + ms))
        }
        other => return Err(KvmError::Internal(format!("wait arg: {other:?}"))),
    };
    Err(KvmError::Yield(YieldKind::Wait { obj, timeout_ms, wake_at_ms }))
}

/// `Object.notify()V`：置请求位（单唤醒）；分派点桥接见模块注释。
pub fn object_notify(
    cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let obj = this(call)?;
    cs.notify_requests.push((obj, call.thread, false));
    Ok(None)
}

/// `Object.notifyAll()V`：置请求位（全唤醒）。
pub fn object_notify_all(
    cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let obj = this(call)?;
    cs.notify_requests.push((obj, call.thread, true));
    Ok(None)
}

// ---------------------------------------------------------------------------
// Class
// ---------------------------------------------------------------------------

pub fn class_get_name(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let class = class_object_id(cs, heap, this(call)?)?;
    let name = dotted_name(cs, class);
    ret_string(heap, utf16(&name))
}

pub fn class_to_string(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let class = class_object_id(cs, heap, this(call)?)?;
    let prefix = if cs.class(class).is_interface() {
        "interface "
    } else {
        "class "
    };
    let text = format!("{prefix}{}", dotted_name(cs, class));
    ret_string(heap, utf16(&text))
}

/// 按点分名加载类；未命中抛 ClassNotFoundException（CLDC 语义）。
/// 数组类形态（"[Ljava.lang.String;"）暂不支持，留待需要时补。
pub fn class_for_name(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let name_ref = arg_ref(call, 0)?;
    if name_ref.is_null() {
        return Err(throw_npe(cs, heap));
    }
    let dotted = String::from_utf16_lossy(&string_chars(heap, name_ref)?);
    let slashed = dotted.replace('.', "/");
    match cs.load_class(&slashed) {
        Ok(class) => ret_ref(class_object_new(cs, heap, class)?),
        Err(KvmError::ClassNotFound(_)) => Err(throw_java(
            cs,
            heap,
            "java/lang/ClassNotFoundException",
            Some(&dotted),
        )),
        Err(error) => Err(error),
    }
}

pub fn class_is_instance(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let target = class_object_id(cs, heap, this(call)?)?;
    let object = arg_ref(call, 0)?;
    if object.is_null() {
        return ret_bool(false);
    }
    let runtime_class = runtime_class_of(cs, heap, object)?;
    ret_bool(is_assignable(cs, runtime_class, target))
}

pub fn class_is_assignable_from(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let target = class_object_id(cs, heap, this(call)?)?;
    let source_ref = arg_ref(call, 0)?;
    if source_ref.is_null() {
        return Err(throw_npe(cs, heap));
    }
    let source = class_object_id(cs, heap, source_ref)?;
    ret_bool(is_assignable(cs, source, target))
}

// ---------------------------------------------------------------------------
// String（载荷：Payload::Str，UTF-16 码元）
// ---------------------------------------------------------------------------

/// `new String(...)`：把 `new` 分配的实例载荷原地替换为 Str
/// （与 ldc 驻留字面量共用一套表示，见模块头注释）。
fn string_become(heap: &mut Heap, reference: ObjRef, chars: Vec<u16>) -> Result<(), KvmError> {
    *heap.payload_mut(reference)? = Payload::Str(StrObj { chars, hash: 0 });
    Ok(())
}

fn throw_sioobe(cs: &mut ClassSpace, heap: &mut Heap, index: i32) -> KvmError {
    throw_java(
        cs,
        heap,
        "java/lang/StringIndexOutOfBoundsException",
        Some(&format!("String index out of range: {index}")),
    )
}

fn chars_of_this(heap: &Heap, call: &NativeCall) -> Result<Vec<u16>, KvmError> {
    string_chars(heap, this(call)?)
}

pub fn string_init(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    string_become(heap, this(call)?, Vec::new())?;
    Ok(None)
}

pub fn string_init_string(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let source = arg_ref(call, 0)?;
    if source.is_null() {
        return Err(throw_npe(cs, heap));
    }
    let chars = string_chars(heap, source)?;
    string_become(heap, this(call)?, chars)?;
    Ok(None)
}

pub fn string_init_chars(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let source = arg_ref(call, 0)?;
    if source.is_null() {
        return Err(throw_npe(cs, heap));
    }
    let chars = char_array_chars(heap, source)?;
    string_become(heap, this(call)?, chars)?;
    Ok(None)
}

pub fn string_init_chars_range(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let source = arg_ref(call, 0)?;
    if source.is_null() {
        return Err(throw_npe(cs, heap));
    }
    let offset = arg_i32(call, 1)?;
    let count = arg_i32(call, 2)?;
    let all = char_array_chars(heap, source)?;
    let length = all.len() as i32;
    if offset < 0 || count < 0 || offset as i64 + count as i64 > length as i64 {
        return Err(throw_sioobe(cs, heap, if offset < 0 { offset } else { offset + count }));
    }
    let chars = all[offset as usize..(offset + count) as usize].to_vec();
    string_become(heap, this(call)?, chars)?;
    Ok(None)
}

/// byte[] 构造：按 Latin-1 解码（CLDC 默认编码语义；enc 参数忽略）。
pub fn string_init_bytes(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let source = arg_ref(call, 0)?;
    let bytes = byte_array_bytes(heap, source)?;
    let chars: Vec<u16> = bytes.into_iter().map(|b| u16::from(b as u8)).collect();
    string_become(heap, this(call)?, chars)?;
    Ok(None)
}

pub fn string_init_bytes_enc(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    // 斯凯平台 "UNICODE" charset（SP-1）：UTF-16LE 语义。jar 侧 f 类的
    // tlv 字符串流是「去 BOM 的裸 UTF-16LE 载荷」（f.tlv_getString 用
    // 本构造器还原），故无 BOM 按 LE；带 BOM（FF FE/FE FF）时按 BOM
    // 端序并跳过（d.getUnicodeByte 把 getBytes 结果手工去 BOM，真机
    // getBytes 产 BOM——见 string_get_bytes 的对称实现）。
    if call.args.get(1).map(|slot| is_unicode_enc(heap, slot)) == Some(true) {
        let source = arg_ref(call, 0)?;
        let raw: Vec<u8> = byte_array_bytes(heap, source)?
            .into_iter()
            .map(|b| b as u8)
            .collect();
        let (skip, little_endian) = match raw.get(..2) {
            Some([0xFF, 0xFE]) => (2usize, true),
            Some([0xFE, 0xFF]) => (2usize, false),
            _ => (0usize, true),
        };
        let body = &raw[skip..];
        let chars: Vec<u16> = body[..body.len() & !1]
            .chunks(2)
            .map(|pair| {
                if little_endian {
                    u16::from_le_bytes([pair[0], pair[1]])
                } else {
                    u16::from_be_bytes([pair[0], pair[1]])
                }
            })
            .collect();
        string_become(heap, this(call)?, chars)?;
        return Ok(None);
    }
    string_init_bytes(cs, heap, call)
}

/// 参数槽是否为字符串 "UNICODE"（null/其他编码返回 false，走 Latin-1）。
fn is_unicode_enc(heap: &Heap, slot: &Slot) -> bool {
    match slot {
        Slot::Ref(reference) if !reference.is_null() => heap
            .str_obj(*reference)
            .map(|obj| obj.chars == utf16("UNICODE"))
            .unwrap_or(false),
        _ => false,
    }
}

pub fn string_length(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_i32(heap.str_obj(this(call)?)?.chars.len() as i32)
}

pub fn string_char_at(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let chars = chars_of_this(heap, call)?;
    let index = arg_i32(call, 0)?;
    if index < 0 || index as usize as i64 >= chars.len() as i64 {
        return Err(throw_sioobe(cs, heap, index));
    }
    ret_i32(chars[index as usize] as i32)
}

pub fn string_equals(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let chars = chars_of_this(heap, call)?;
    let same = match call.args.first() {
        Some(Slot::Ref(other)) if !other.is_null() && heap.is_string(*other) => {
            heap.str_obj(*other)?.chars == chars
        }
        _ => false,
    };
    ret_bool(same)
}

pub fn string_equals_ignore_case(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let chars = chars_of_this(heap, call)?;
    let other = arg_ref(call, 0)?;
    if other.is_null() || !heap.is_string(other) {
        return ret_bool(false);
    }
    let other_chars = &heap.str_obj(other)?.chars;
    let same = chars.len() == other_chars.len()
        && chars
            .iter()
            .zip(other_chars)
            .all(|(a, b)| ascii_lower(*a) == ascii_lower(*b));
    ret_bool(same)
}

/// JVM 公式：s[0]*31^(n-1)+…，int 溢出回绕；结果惰性缓存进 StrObj.hash。
pub fn string_hash_code(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let reference = this(call)?;
    let string = heap.str_obj_mut(reference)?;
    if string.hash == 0 {
        let mut hash: i32 = 0;
        for &c in &string.chars {
            hash = hash.wrapping_mul(31).wrapping_add(c as i32);
        }
        string.hash = hash;
    }
    ret_i32(string.hash)
}

/// char 检索：from 负值取 0；超 BMP 码点按不存在处理。
fn index_of_char(chars: &[u16], ch: i32, from: i32) -> i32 {
    if !(0..=0xFFFF).contains(&ch) {
        return -1;
    }
    let needle = ch as u16;
    let start = from.max(0) as usize;
    for (index, &c) in chars.iter().enumerate().skip(start) {
        if c == needle {
            return index as i32;
        }
    }
    -1
}

fn last_index_of_char(chars: &[u16], ch: i32, from: i32) -> i32 {
    if !(0..=0xFFFF).contains(&ch) {
        return -1;
    }
    let needle = ch as u16;
    let end = (from.min(chars.len() as i32 - 1)).max(-1);
    let mut index = end;
    while index >= 0 {
        if chars[index as usize] == needle {
            return index;
        }
        index -= 1;
    }
    -1
}

pub fn string_index_of(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let chars = chars_of_this(heap, call)?;
    ret_i32(index_of_char(&chars, arg_i32(call, 0)?, 0))
}

pub fn string_index_of_from(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let chars = chars_of_this(heap, call)?;
    ret_i32(index_of_char(&chars, arg_i32(call, 0)?, arg_i32(call, 1)?))
}

pub fn string_last_index_of(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let chars = chars_of_this(heap, call)?;
    ret_i32(last_index_of_char(&chars, arg_i32(call, 0)?, chars.len() as i32 - 1))
}

pub fn string_last_index_of_from(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let chars = chars_of_this(heap, call)?;
    ret_i32(last_index_of_char(&chars, arg_i32(call, 0)?, arg_i32(call, 1)?))
}

fn index_of_str(haystack: &[u16], needle: &[u16], from: i32) -> i32 {
    if needle.is_empty() {
        return from.clamp(0, haystack.len() as i32);
    }
    if needle.len() > haystack.len() {
        return -1;
    }
    let start = from.max(0) as usize;
    let last = haystack.len() - needle.len();
    let mut index = start;
    while index <= last {
        if haystack[index..index + needle.len()] == *needle {
            return index as i32;
        }
        index += 1;
    }
    -1
}

pub fn string_index_of_str(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let chars = chars_of_this(heap, call)?;
    let needle = string_arg_or_npe(cs, heap, call, 0)?;
    ret_i32(index_of_str(&chars, &needle, 0))
}

pub fn string_index_of_str_from(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let chars = chars_of_this(heap, call)?;
    let needle = string_arg_or_npe(cs, heap, call, 0)?;
    ret_i32(index_of_str(&chars, &needle, arg_i32(call, 1)?))
}

pub fn string_last_index_of_str(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let chars = chars_of_this(heap, call)?;
    let needle = string_arg_or_npe(cs, heap, call, 0)?;
    if needle.len() > chars.len() {
        return ret_i32(-1);
    }
    let last = chars.len() - needle.len();
    let mut index = last as i32;
    while index >= 0 {
        if chars[index as usize..index as usize + needle.len()] == *needle {
            return ret_i32(index);
        }
        index -= 1;
    }
    ret_i32(-1)
}

fn string_arg_or_npe(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
    index: usize,
) -> Result<Vec<u16>, KvmError> {
    let reference = arg_ref(call, index)?;
    if reference.is_null() {
        return Err(throw_npe(cs, heap));
    }
    string_chars(heap, reference)
}

/// JDK null 宽容语义的字符串参数（append/insert 族）：null → "null" 字面量
/// （AbstractStringBuilder.appendNull）。concat/indexOf 等严格族仍走
/// `string_arg_or_npe`。
fn nullable_string_arg(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
    index: usize,
) -> Result<Vec<u16>, KvmError> {
    let reference = arg_ref(call, index)?;
    if reference.is_null() {
        return Ok(utf16("null"));
    }
    string_chars(heap, reference)
}

pub fn string_substring_from(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let chars = chars_of_this(heap, call)?;
    let begin = arg_i32(call, 0)?;
    let length = chars.len() as i32;
    if begin < 0 || begin > length {
        return Err(throw_sioobe(cs, heap, begin));
    }
    ret_string(heap, chars[begin as usize..].to_vec())
}

pub fn string_substring_range(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let chars = chars_of_this(heap, call)?;
    let begin = arg_i32(call, 0)?;
    let end = arg_i32(call, 1)?;
    let length = chars.len() as i32;
    if begin < 0 || end > length || begin > end {
        return Err(throw_sioobe(cs, heap, if begin < 0 { begin } else { end }));
    }
    ret_string(heap, chars[begin as usize..end as usize].to_vec())
}

pub fn string_concat(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let mut chars = chars_of_this(heap, call)?;
    let tail = string_arg_or_npe(cs, heap, call, 0)?;
    chars.extend_from_slice(&tail);
    ret_string(heap, chars)
}

/// 字典序：逐码元差，再比长度。
pub fn string_compare_to(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let chars = chars_of_this(heap, call)?;
    let other = string_arg_or_npe(cs, heap, call, 0)?;
    let common = chars.len().min(other.len());
    for index in 0..common {
        let diff = chars[index] as i32 - other[index] as i32;
        if diff != 0 {
            return ret_i32(diff);
        }
    }
    ret_i32(chars.len() as i32 - other.len() as i32)
}

pub fn string_starts_with(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let chars = chars_of_this(heap, call)?;
    let prefix = string_arg_or_npe(cs, heap, call, 0)?;
    let matches = chars.len() >= prefix.len() && chars[..prefix.len()] == *prefix;
    ret_bool(matches)
}

pub fn string_ends_with(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let chars = chars_of_this(heap, call)?;
    let suffix = string_arg_or_npe(cs, heap, call, 0)?;
    let matches = chars.len() >= suffix.len() && chars[chars.len() - suffix.len()..] == *suffix;
    ret_bool(matches)
}

fn ascii_upper(c: u16) -> u16 {
    match c {
        0x61..=0x7a => c - 0x20,
        other => other,
    }
}

fn ascii_lower(c: u16) -> u16 {
    match c {
        0x41..=0x5a => c + 0x20,
        other => other,
    }
}

/// 大小写转换仅覆盖 ASCII（Unicode 表留待需要时补，见模块头）。
pub fn string_to_upper(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let chars: Vec<u16> = chars_of_this(heap, call)?.iter().map(|&c| ascii_upper(c)).collect();
    ret_string(heap, chars)
}

pub fn string_to_lower(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let chars: Vec<u16> = chars_of_this(heap, call)?.iter().map(|&c| ascii_lower(c)).collect();
    ret_string(heap, chars)
}

pub fn string_to_char_array(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let chars = chars_of_this(heap, call)?;
    let array = alloc_char_array(cs, heap, chars.len())?;
    write_char_array(heap, array, 0, &chars)?;
    ret_ref(array)
}

/// 去除两端 <= ' ' 的码元（JVM trim 语义）。
pub fn string_trim(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let chars = chars_of_this(heap, call)?;
    let start = chars.iter().position(|&c| c > 0x20).unwrap_or(chars.len());
    let end = chars
        .iter()
        .rposition(|&c| c > 0x20)
        .map(|index| index + 1)
        .unwrap_or(start);
    ret_string(heap, chars[start..end].to_vec())
}

pub fn string_to_string(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_ref(this(call)?)
}

/// 驻留：同内容幂等返回同一对象（ldc 字面量同表）。
pub fn string_intern(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let chars = chars_of_this(heap, call)?;
    ret_ref(heap.alloc_string(chars)?)
}

/// getBytes(enc)：码元低 8 位（Latin-1；enc 参数忽略）。
/// 例外：enc=="UNICODE" 产 `[FF FE BOM] + UTF-16LE`（斯凯平台语义，
/// SP-1——f.tlv_appendString 依赖「跳 2 字节 BOM、len-2」的带 BOM 产物，
/// Latin-1 截断会让载荷长度与 len 字段错位、奇数长度解析失败）。
pub fn string_get_bytes(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let chars = chars_of_this(heap, call)?;
    if call.args.first().map(|slot| is_unicode_enc(heap, slot)) == Some(true) {
        let mut bytes: Vec<i8> = vec![0xFF_u8 as i8, 0xFE_u8 as i8];
        for &unit in &chars {
            bytes.push((unit & 0xFF) as u8 as i8);
            bytes.push((unit >> 8) as u8 as i8);
        }
        let class = cs.load_class("[B")?;
        let array = heap.alloc_array(class, ElemType::Byte, bytes.len())?;
        {
            let arr = heap.array_mut(array)?;
            for (index, byte) in bytes.into_iter().enumerate() {
                arr.data.set(index, Slot::Int(byte as i32))?;
            }
        }
        return ret_ref(array);
    }
    let bytes: Vec<i8> = chars.iter().map(|&c| c as u8 as i8).collect();
    let class = cs.load_class("[B")?;
    let array = heap.alloc_array(class, ElemType::Byte, bytes.len())?;
    {
        let arr = heap.array_mut(array)?;
        for (index, byte) in bytes.into_iter().enumerate() {
            arr.data.set(index, Slot::Int(byte as i32))?;
        }
    }
    ret_ref(array)
}

// ---- String.valueOf 静态族 -------------------------------------------------

pub fn string_value_of_bool(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let value = arg_i32(call, 0)? != 0;
    ret_string(heap, utf16(if value { "true" } else { "false" }))
}

pub fn string_value_of_char(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_string(heap, vec![arg_i32(call, 0)? as u16])
}

pub fn string_value_of_int(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_string(heap, utf16(&arg_i32(call, 0)?.to_string()))
}

pub fn string_value_of_long(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_string(heap, utf16(&long_text(arg_i64(call, 0)?)))
}

pub fn string_value_of_float(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_string(heap, utf16(&java_double_text(arg_f32(call, 0)? as f64)))
}

pub fn string_value_of_double(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_string(heap, utf16(&java_double_text(arg_f64(call, 0)?)))
}

/// Object 参数：null → "null"；String 原样；其余取 Object.toString 形态
/// （原生方法无法回调 guest 虚方法，"ClassName@hash" 为最接近近似）。
pub fn string_value_of_object(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let reference = arg_ref(call, 0)?;
    if reference.is_null() {
        return ret_string(heap, utf16("null"));
    }
    if heap.is_string(reference) {
        return ret_ref(reference);
    }
    let class = runtime_class_of(cs, heap, reference)?;
    let text = format!("{}@{:x}", dotted_name(cs, class), reference.0);
    ret_string(heap, utf16(&text))
}

pub fn string_value_of_chars(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let source = arg_ref(call, 0)?;
    let chars = char_array_chars(heap, source)?;
    ret_string(heap, chars)
}

// ---------------------------------------------------------------------------
// 数组小工具（String/StringBuffer 共用）
// ---------------------------------------------------------------------------

fn alloc_char_array(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    len: usize,
) -> Result<ObjRef, KvmError> {
    let class = cs.load_class("[C")?;
    heap.alloc_array(class, ElemType::Char, len)
}

fn char_array_chars(heap: &Heap, array: ObjRef) -> Result<Vec<u16>, KvmError> {
    match &heap.array(array)?.data {
        ArrayData::Char(data) => Ok(data.clone()),
        other => Err(KvmError::Internal(format!("expected char[], got {other:?}"))),
    }
}

fn byte_array_bytes(heap: &Heap, array: ObjRef) -> Result<Vec<i8>, KvmError> {
    match &heap.array(array)?.data {
        ArrayData::Byte(data) => Ok(data.clone()),
        other => Err(KvmError::Internal(format!("expected byte[], got {other:?}"))),
    }
}

fn write_char_array(
    heap: &mut Heap,
    array: ObjRef,
    offset: usize,
    chars: &[u16],
) -> Result<(), KvmError> {
    match &mut heap.array_mut(array)?.data {
        ArrayData::Char(data) => {
            data[offset..offset + chars.len()].copy_from_slice(chars);
            Ok(())
        }
        other => Err(KvmError::Internal(format!("expected char[], got {other:?}"))),
    }
}

// ---------------------------------------------------------------------------
// StringBuffer（实例字段 value:[C + count:I）
// ---------------------------------------------------------------------------

/// value/count 字段槽位（走 find_field 保守取位）。
fn sb_field_slots(cs: &mut ClassSpace) -> Result<(usize, usize), KvmError> {
    let class = cs.load_class("java/lang/StringBuffer")?;
    let value_name = cs.syms.intern("value");
    let value_desc = cs.syms.intern("[C");
    let count_name = cs.syms.intern("count");
    let count_desc = cs.syms.intern("I");
    let value = cs.find_field(class, value_name, value_desc)?;
    let count = cs.find_field(class, count_name, count_desc)?;
    Ok((value.slot as usize, count.slot as usize))
}

fn sb_state(heap: &Heap, this: ObjRef, slots: (usize, usize)) -> Result<(ObjRef, i32), KvmError> {
    let instance = heap.instance(this)?;
    match (&instance.fields[slots.0], instance.fields[slots.1]) {
        (Slot::Ref(value), Slot::Int(count)) => Ok((*value, count)),
        other => Err(KvmError::Internal(format!(
            "StringBuffer state expected (ref,int), got {other:?}"
        ))),
    }
}

fn sb_set_field(
    heap: &mut Heap,
    this: ObjRef,
    slot: usize,
    value: Slot,
) -> Result<(), KvmError> {
    heap.instance_mut(this)?.fields[slot] = value;
    Ok(())
}

/// 追加并按需扩容（2×+2），更新 count。
fn sb_append_text(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    this: ObjRef,
    extra: &[u16],
) -> Result<(), KvmError> {
    let slots = sb_field_slots(cs)?;
    let (mut value, count) = sb_state(heap, this, slots)?;
    if value.is_null() {
        return Err(KvmError::Internal("StringBuffer not initialized".into()));
    }
    let new_count = count + extra.len() as i32;
    let capacity = heap.array(value)?.data.len() as i32;
    if new_count > capacity {
        let new_capacity = (new_count.max(capacity * 2 + 2)) as usize;
        let fresh = alloc_char_array(cs, heap, new_capacity)?;
        let old: Vec<u16> = char_array_chars(heap, value)?
            .into_iter()
            .take(count as usize)
            .collect();
        write_char_array(heap, fresh, 0, &old)?;
        sb_set_field(heap, this, slots.0, Slot::Ref(fresh))?;
        value = fresh;
    }
    write_char_array(heap, value, count as usize, extra)?;
    sb_set_field(heap, this, slots.1, Slot::Int(new_count))?;
    Ok(())
}

/// 在 offset 处插入（右移原有内容）。
fn sb_insert_chars(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    this: ObjRef,
    offset: i32,
    extra: &[u16],
) -> Result<(), KvmError> {
    let slots = sb_field_slots(cs)?;
    let (value, count) = sb_state(heap, this, slots)?;
    if offset < 0 || offset > count {
        return Err(throw_java(
            cs,
            heap,
            "java/lang/StringIndexOutOfBoundsException",
            Some(&format!("String index out of range: {offset}")),
        ));
    }
    let chars = char_array_chars(heap, value)?.into_iter().take(count as usize).collect::<Vec<_>>();
    let mut joined = chars[..offset as usize].to_vec();
    joined.extend_from_slice(extra);
    joined.extend_from_slice(&chars[offset as usize..]);
    // 重置 count 后整体重写（走 append 的扩容路径最省事）。
    sb_set_field(heap, this, slots.1, Slot::Int(0))?;
    sb_append_text(cs, heap, this, &joined)
}

fn sb_throw_sioobe(cs: &mut ClassSpace, heap: &mut Heap, index: i32) -> KvmError {
    throw_java(
        cs,
        heap,
        "java/lang/StringIndexOutOfBoundsException",
        Some(&format!("String index out of range: {index}")),
    )
}

pub fn sb_init(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let slots = sb_field_slots(cs)?;
    let buffer = alloc_char_array(cs, heap, 16)?;
    sb_set_field(heap, this, slots.0, Slot::Ref(buffer))?;
    sb_set_field(heap, this, slots.1, Slot::Int(0))?;
    Ok(None)
}

pub fn sb_init_capacity(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let capacity = arg_i32(call, 0)?;
    if capacity < 0 {
        return Err(throw_java(
            cs,
            heap,
            "java/lang/NegativeArraySizeException",
            Some(&capacity.to_string()),
        ));
    }
    let slots = sb_field_slots(cs)?;
    let buffer = alloc_char_array(cs, heap, capacity as usize)?;
    sb_set_field(heap, this, slots.0, Slot::Ref(buffer))?;
    sb_set_field(heap, this, slots.1, Slot::Int(0))?;
    Ok(None)
}

pub fn sb_init_string(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let source = string_arg_or_npe(cs, heap, call, 0)?;
    let slots = sb_field_slots(cs)?;
    let buffer = alloc_char_array(cs, heap, source.len() + 16)?;
    write_char_array(heap, buffer, 0, &source)?;
    sb_set_field(heap, this, slots.0, Slot::Ref(buffer))?;
    sb_set_field(heap, this, slots.1, Slot::Int(source.len() as i32))?;
    Ok(None)
}

// ---- append 族（链式返回 this）---------------------------------------------

pub fn sb_append_bool(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let text = if arg_i32(call, 0)? != 0 { "true" } else { "false" };
    sb_append_text(cs, heap, this(call)?, &utf16(text))?;
    ret_ref(this(call)?)
}

pub fn sb_append_char(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let c = arg_i32(call, 0)? as u16;
    sb_append_text(cs, heap, this(call)?, &[c])?;
    ret_ref(this(call)?)
}

pub fn sb_append_int(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let text = arg_i32(call, 0)?.to_string();
    sb_append_text(cs, heap, this(call)?, &utf16(&text))?;
    ret_ref(this(call)?)
}

pub fn sb_append_long(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let text = long_text(arg_i64(call, 0)?);
    sb_append_text(cs, heap, this(call)?, &utf16(&text))?;
    ret_ref(this(call)?)
}

pub fn sb_append_float(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let text = java_double_text(arg_f32(call, 0)? as f64);
    sb_append_text(cs, heap, this(call)?, &utf16(&text))?;
    ret_ref(this(call)?)
}

pub fn sb_append_double(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let text = java_double_text(arg_f64(call, 0)?);
    sb_append_text(cs, heap, this(call)?, &utf16(&text))?;
    ret_ref(this(call)?)
}

pub fn sb_append_string(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    // JDK 语义：append((String) null) 追加字面量 "null"
    //（AbstractStringBuilder.appendNull）——gjxjwql 关于页
    // sb.append(桌面快捷方式名) 非桌面模式传 null，抛 NPE 会令整页
    // 文字生成中断（实证：f.a()V 关于分支每帧 NPE）。
    let text = nullable_string_arg(cs, heap, call, 0)?;
    sb_append_text(cs, heap, this(call)?, &text)?;
    ret_ref(this(call)?)
}

pub fn sb_append_string_buffer(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    // JDK 语义：append((StringBuffer) null) 同样追加 "null"。
    let source = arg_ref(call, 0)?;
    if source.is_null() {
        sb_append_text(cs, heap, this(call)?, &utf16("null"))?;
        return ret_ref(this(call)?);
    }
    let text = sb_content(heap, source)?;
    sb_append_text(cs, heap, this(call)?, &text)?;
    ret_ref(this(call)?)
}

pub fn sb_append_object(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let reference = arg_ref(call, 0)?;
    let text = if reference.is_null() {
        utf16("null")
    } else if heap.is_string(reference) {
        string_chars(heap, reference)?
    } else {
        // 与 String.valueOf(Object) 同近似：无 guest 虚方法回调通道。
        let class = runtime_class_of(cs, heap, reference)?;
        utf16(&format!("{}@{:x}", dotted_name(cs, class), reference.0))
    };
    sb_append_text(cs, heap, this(call)?, &text)?;
    ret_ref(this(call)?)
}

pub fn sb_append_chars(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let source = arg_ref(call, 0)?;
    if source.is_null() {
        return Err(throw_npe(cs, heap));
    }
    let text = char_array_chars(heap, source)?;
    sb_append_text(cs, heap, this(call)?, &text)?;
    ret_ref(this(call)?)
}

// ---- insert 族 --------------------------------------------------------------

pub fn sb_insert_char(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let offset = arg_i32(call, 0)?;
    let c = arg_i32(call, 1)? as u16;
    sb_insert_chars(cs, heap, this(call)?, offset, &[c])?;
    ret_ref(this(call)?)
}

pub fn sb_insert_int(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let offset = arg_i32(call, 0)?;
    let text = arg_i32(call, 1)?.to_string();
    sb_insert_chars(cs, heap, this(call)?, offset, &utf16(&text))?;
    ret_ref(this(call)?)
}

pub fn sb_insert_long(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let offset = arg_i32(call, 0)?;
    let text = long_text(arg_i64(call, 1)?);
    sb_insert_chars(cs, heap, this(call)?, offset, &utf16(&text))?;
    ret_ref(this(call)?)
}

pub fn sb_insert_string(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let offset = arg_i32(call, 0)?;
    // JDK 语义：insert(offset, (String) null) 插入 "null" 字面量。
    let text = nullable_string_arg(cs, heap, call, 1)?;
    sb_insert_chars(cs, heap, this(call)?, offset, &text)?;
    ret_ref(this(call)?)
}

pub fn sb_insert_object(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let offset = arg_i32(call, 0)?;
    let reference = arg_ref(call, 1)?;
    let text = if reference.is_null() {
        utf16("null")
    } else if heap.is_string(reference) {
        string_chars(heap, reference)?
    } else {
        let class = runtime_class_of(cs, heap, reference)?;
        utf16(&format!("{}@{:x}", dotted_name(cs, class), reference.0))
    };
    sb_insert_chars(cs, heap, this(call)?, offset, &text)?;
    ret_ref(this(call)?)
}

// ---- 其余实例方法 -----------------------------------------------------------

/// 当前内容（value 前 count 个码元）。
fn sb_content(heap: &Heap, this: ObjRef) -> Result<Vec<u16>, KvmError> {
    let slots = sb_field_slots_ro(heap, this)?;
    let (value, count) = sb_state(heap, this, slots)?;
    if value.is_null() {
        return Err(KvmError::Internal("StringBuffer not initialized".into()));
    }
    Ok(char_array_chars(heap, value)?
        .into_iter()
        .take(count.max(0) as usize)
        .collect())
}

/// 只读路径的字段槽位（测试/内部读取时 cs 未必可变；StringBuffer 布局
/// 恒为 value=0/count=1——Object 无实例字段且声明序固定）。
fn sb_field_slots_ro(_heap: &Heap, _this: ObjRef) -> Result<(usize, usize), KvmError> {
    Ok((0, 1))
}

pub fn sb_length(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let slots = sb_field_slots_ro(heap, this)?;
    let (_, count) = sb_state(heap, this, slots)?;
    ret_i32(count)
}

pub fn sb_char_at(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let content = sb_content(heap, this)?;
    let index = arg_i32(call, 0)?;
    if index < 0 || index as usize >= content.len() {
        return Err(sb_throw_sioobe(cs, heap, index));
    }
    ret_i32(content[index as usize] as i32)
}

pub fn sb_set_char_at(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let index = arg_i32(call, 0)?;
    let c = arg_i32(call, 1)? as u16;
    let slots = sb_field_slots(cs)?;
    let (value, count) = sb_state(heap, this, slots)?;
    if index < 0 || index >= count {
        return Err(sb_throw_sioobe(cs, heap, index));
    }
    match &mut heap.array_mut(value)?.data {
        ArrayData::Char(data) => data[index as usize] = c,
        other => {
            return Err(KvmError::Internal(format!("expected char[], got {other:?}")));
        }
    }
    Ok(None)
}

pub fn sb_set_length(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let new_length = arg_i32(call, 0)?;
    if new_length < 0 {
        return Err(sb_throw_sioobe(cs, heap, new_length));
    }
    let content = sb_content(heap, this)?;
    let mut joined = content;
    while joined.len() < new_length as usize {
        joined.push(0); // '\0' 填充（JVM setLength 语义）
    }
    joined.truncate(new_length as usize);
    let slots = sb_field_slots(cs)?;
    sb_set_field(heap, this, slots.1, Slot::Int(0))?;
    sb_append_text(cs, heap, this, &joined)?;
    Ok(None)
}

pub fn sb_delete_char_at(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let index = arg_i32(call, 0)?;
    let content = sb_content(heap, this)?;
    if index < 0 || index as usize >= content.len() {
        return Err(sb_throw_sioobe(cs, heap, index));
    }
    let mut joined = content;
    joined.remove(index as usize);
    let slots = sb_field_slots(cs)?;
    sb_set_field(heap, this, slots.1, Slot::Int(0))?;
    sb_append_text(cs, heap, this, &joined)?;
    ret_ref(this)
}

pub fn sb_delete(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let start = arg_i32(call, 0)?;
    let end = arg_i32(call, 1)?;
    let content = sb_content(heap, this)?;
    let length = content.len() as i32;
    if start < 0 || start > length || start > end {
        return Err(sb_throw_sioobe(cs, heap, start));
    }
    let end = end.min(length);
    let mut joined = content[..start as usize].to_vec();
    joined.extend_from_slice(&content[end as usize..]);
    let slots = sb_field_slots(cs)?;
    sb_set_field(heap, this, slots.1, Slot::Int(0))?;
    sb_append_text(cs, heap, this, &joined)?;
    ret_ref(this)
}

pub fn sb_reverse(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let mut content = sb_content(heap, this)?;
    content.reverse();
    let slots = sb_field_slots(cs)?;
    sb_set_field(heap, this, slots.1, Slot::Int(0))?;
    sb_append_text(cs, heap, this, &content)?;
    ret_ref(this)
}

pub fn sb_to_string(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let content = sb_content(heap, this(call)?)?;
    ret_string(heap, content)
}

// ---------------------------------------------------------------------------
// Math
// ---------------------------------------------------------------------------

/// Java Math.min NaN 语义：任一 NaN → NaN；±0 取 -0。
fn jmin_f64(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a == 0.0 && b == 0.0 {
        if a.is_sign_negative() || b.is_sign_negative() { -0.0 } else { 0.0 }
    } else if a <= b {
        a
    } else {
        b
    }
}

fn jmax_f64(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a == 0.0 && b == 0.0 {
        if a.is_sign_positive() || b.is_sign_positive() { 0.0 } else { -0.0 }
    } else if a >= b {
        a
    } else {
        b
    }
}

pub fn math_abs_int(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_i32(arg_i32(call, 0)?.wrapping_abs())
}

pub fn math_abs_long(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Long(arg_i64(call, 0)?.wrapping_abs())))
}

pub fn math_abs_float(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Float(arg_f32(call, 0)?.abs())))
}

pub fn math_abs_double(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Double(arg_f64(call, 0)?.abs())))
}

pub fn math_min_int(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_i32(arg_i32(call, 0)?.min(arg_i32(call, 1)?))
}

pub fn math_max_int(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_i32(arg_i32(call, 0)?.max(arg_i32(call, 1)?))
}

pub fn math_min_long(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Long(arg_i64(call, 0)?.min(arg_i64(call, 1)?))))
}

pub fn math_max_long(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Long(arg_i64(call, 0)?.max(arg_i64(call, 1)?))))
}

pub fn math_min_float(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Float(jmin_f64(
        arg_f32(call, 0)? as f64,
        arg_f32(call, 1)? as f64,
    ) as f32)))
}

pub fn math_max_float(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Float(jmax_f64(
        arg_f32(call, 0)? as f64,
        arg_f32(call, 1)? as f64,
    ) as f32)))
}

pub fn math_min_double(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Double(jmin_f64(arg_f64(call, 0)?, arg_f64(call, 1)?))))
}

pub fn math_max_double(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Double(jmax_f64(arg_f64(call, 0)?, arg_f64(call, 1)?))))
}

macro_rules! math_f64 {
    ($name:ident, $body:expr) => {
        pub fn $name(
            _cs: &mut ClassSpace,
            _heap: &mut Heap,
            call: &NativeCall,
        ) -> Result<Option<Slot>, KvmError> {
            let x = arg_f64(call, 0)?;
            let f: fn(f64) -> f64 = $body;
            Ok(Some(Slot::Double(f(x))))
        }
    };
}

math_f64!(math_sqrt, |x| x.sqrt());
math_f64!(math_sin, |x| x.sin());
math_f64!(math_cos, |x| x.cos());
math_f64!(math_tan, |x| x.tan());
math_f64!(math_asin, |x| x.asin());
math_f64!(math_acos, |x| x.acos());
math_f64!(math_atan, |x| x.atan());
math_f64!(math_floor, |x| x.floor());
math_f64!(math_ceil, |x| x.ceil());
math_f64!(math_to_radians, |x| x * std::f64::consts::PI / 180.0);
math_f64!(math_to_degrees, |x| x * 180.0 / std::f64::consts::PI);

pub fn math_atan2(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Double(arg_f64(call, 0)?.atan2(arg_f64(call, 1)?))))
}

pub fn math_pow(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Double(arg_f64(call, 0)?.powf(arg_f64(call, 1)?))))
}

/// Java round 语义：floor(x+0.5)；NaN → 0，越界饱和（Rust as 语义）。
pub fn math_round_double(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let x = arg_f64(call, 0)?;
    Ok(Some(Slot::Long(if x.is_nan() { 0 } else { (x + 0.5).floor() as i64 })))
}

pub fn math_round_float(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let x = arg_f32(call, 0)?;
    ret_i32(if x.is_nan() { 0 } else { (x + 0.5).floor() as i32 })
}

// ---------------------------------------------------------------------------
// System
// ---------------------------------------------------------------------------

/// 元素级复制（先快照源区间，天然处理重叠与同数组）。
pub fn system_arraycopy(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let src = arg_ref(call, 0)?;
    let src_pos = arg_i32(call, 1)?;
    let dst = arg_ref(call, 2)?;
    let dst_pos = arg_i32(call, 3)?;
    let length = arg_i32(call, 4)?;
    if src.is_null() || dst.is_null() {
        return Err(throw_npe(cs, heap));
    }
    let (src_elem, src_len) = match heap.payload(src)? {
        Payload::Array(array) => (array.elem, array.data.len() as i32),
        _ => {
            return Err(throw_java(
                cs,
                heap,
                "java/lang/ArrayStoreException",
                Some("arraycopy: source is not an array"),
            ));
        }
    };
    let (dst_elem, dst_len) = match heap.payload(dst)? {
        Payload::Array(array) => (array.elem, array.data.len() as i32),
        _ => {
            return Err(throw_java(
                cs,
                heap,
                "java/lang/ArrayStoreException",
                Some("arraycopy: destination is not an array"),
            ));
        }
    };
    // 元素类型精确相等；引用数组不做逐元素 store 检查（最小实现）。
    if src_elem != dst_elem {
        return Err(throw_java(
            cs,
            heap,
            "java/lang/ArrayStoreException",
            Some("arraycopy: array element type mismatch"),
        ));
    }
    if src_pos < 0 || dst_pos < 0 || length < 0
        || src_pos as i64 + length as i64 > src_len as i64
        || dst_pos as i64 + length as i64 > dst_len as i64
    {
        return Err(throw_java(
            cs,
            heap,
            "java/lang/ArrayIndexOutOfBoundsException",
            Some(&format!(
                "arraycopy: src[{src_pos}, {}/{length}] dst[{dst_pos}, {}]",
                src_len, dst_len
            )),
        ));
    }
    if length == 0 {
        return Ok(None);
    }
    let mut chunk: Vec<Slot> = Vec::with_capacity(length as usize);
    {
        let array = heap.array(src)?;
        for index in 0..length as usize {
            chunk.push(array.data.get(src_pos as usize + index));
        }
    }
    {
        let array = heap.array_mut(dst)?;
        for (index, value) in chunk.into_iter().enumerate() {
            array.data.set(dst_pos as usize + index, value)?;
        }
    }
    Ok(None)
}

/// 宿主时钟（T034 改宿主轨）：与 sleep/Timer 同源——原实现取真实
/// 墙钟（"确定性时钟注入留待 T045"），引擎/mock 时钟快进对
/// `currentTimeMillis` 轮询型超时逻辑（实证：51mrp splash 的 h()
/// 超时切换路径）完全无效，虚拟时间推进后仍卡首画面。
pub fn system_current_time_millis(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    host: &mut dyn HostContext,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Long(host.now_ms() as i64)))
}

/// CLDC 固定属性面；未声明键返回 null。
pub fn system_get_property(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let key_ref = arg_ref(call, 0)?;
    let key = String::from_utf16_lossy(&string_chars(heap, key_ref)?);
    let value = match key.as_str() {
        "microedition.platform" => Some("jSky"),
        "microedition.configuration" => Some("CLDC-1.1"),
        "microedition.profiles" => Some("MIDP-2.0"),
        "microedition.encoding" => Some("ISO-8859-1"),
        _ => None,
    };
    match value {
        Some(text) => Ok(Some(Slot::Ref(heap.alloc_string(utf16(text))?))),
        None => ret_ref(ObjRef::NULL),
    }
}

/// TODO(T045)：Vm shutdown 请求标志桥接（NativeFn 无状态通道，先以
/// 内部错误显式中断；VmError 层识别后转 shutdown 语义）。
pub fn system_exit(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let code = arg_i32(call, 0)?;
    Err(KvmError::Internal(format!(
        "System.exit({code}) requested (shutdown bridge pending T045)"
    )))
}

/// 主动 GC 需根集（帧栈 + 静态区），NativeFn 无访问通道；既有 GC 已在
/// 分配失败路径自动触发（gc::alloc_with_gc），此处空实现。
pub fn system_gc(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(None)
}

// ---------------------------------------------------------------------------
// Throwable 及异常体系
// ---------------------------------------------------------------------------

pub fn throwable_init_empty(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    // detailMessage 保持 null（Object.<init> 无操作语义）。
    Ok(None)
}

pub fn throwable_init_message(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let message = arg_ref(call, 0)?;
    let class = heap.instance(this)?.class;
    let name = cs.syms.intern("detailMessage");
    let desc = cs.syms.intern("Ljava/lang/String;");
    if let Ok(field) = cs.find_field(class, name, desc) {
        heap.instance_mut(this)?.fields[field.slot as usize] = Slot::Ref(message);
    }
    Ok(None)
}

/// (int) 重载：按异常类拼 "Array/String index out of range: n"。
pub fn throwable_init_index(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let index = arg_i32(call, 0)?;
    let class_name = cs.name_of(call.class);
    let text = if class_name.contains("Array") {
        format!("Array index out of range: {index}")
    } else {
        format!("String index out of range: {index}")
    };
    let message = heap.alloc_string(utf16(&text))?;
    let class = heap.instance(this)?.class;
    let name = cs.syms.intern("detailMessage");
    let desc = cs.syms.intern("Ljava/lang/String;");
    if let Ok(field) = cs.find_field(class, name, desc) {
        heap.instance_mut(this)?.fields[field.slot as usize] = Slot::Ref(message);
    }
    Ok(None)
}

pub fn throwable_get_message(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let class = heap.instance(this)?.class;
    let name = cs.syms.intern("detailMessage");
    let desc = cs.syms.intern("Ljava/lang/String;");
    match cs.find_field(class, name, desc) {
        Ok(field) => {
            let message = heap.instance(this)?.fields[field.slot as usize];
            match message {
                Slot::Ref(reference) => ret_ref(reference),
                Slot::Int(0) => ret_ref(ObjRef::NULL),
                other => Err(KvmError::Internal(format!(
                    "detailMessage expected ref, got {other:?}"
                ))),
            }
        }
        Err(_) => ret_ref(ObjRef::NULL),
    }
}

/// "ClassName[: message]"（JVM Throwable.toString 形态，空消息保留冒号）。
pub fn throwable_to_string(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let class = heap.instance(this)?.class;
    let name = dotted_name(cs, class);
    let name_sym = cs.syms.intern("detailMessage");
    let desc_sym = cs.syms.intern("Ljava/lang/String;");
    let text = match cs.find_field(class, name_sym, desc_sym) {
        Ok(field) => match heap.instance(this)?.fields[field.slot as usize] {
            Slot::Ref(reference) if !reference.is_null() => {
                let message = String::from_utf16_lossy(&string_chars(heap, reference)?);
                format!("{name}: {message}")
            }
            _ => name,
        },
        Err(_) => name,
    };
    ret_string(heap, utf16(&text))
}

/// 写 stderr；TODO(T045)：错误画面呈现。
pub fn throwable_print_stack_trace(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let class = heap.instance(this)?.class;
    let name = dotted_name(cs, class);
    let name_sym = cs.syms.intern("detailMessage");
    let desc_sym = cs.syms.intern("Ljava/lang/String;");
    let message = match cs.find_field(class, name_sym, desc_sym) {
        Ok(field) => match heap.instance(this)?.fields[field.slot as usize] {
            Slot::Ref(reference) if !reference.is_null() => {
                Some(String::from_utf16_lossy(&string_chars(heap, reference)?))
            }
            _ => None,
        },
        Err(_) => None,
    };
    match message {
        Some(message) => eprintln!("{name}: {message}"),
        None => eprintln!("{name}"),
    }
    Ok(None)
}

/// JVM 返回 this；帧栈方法名序列对原生方法不可达，栈轨迹留空
/// （TODO T045：经 Machine 侧补记）。
pub fn throwable_fill_in_stack_trace(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_ref(this(call)?)
}

// ---------------------------------------------------------------------------
// 包装类（Boolean/Character/Integer/Long/Float/Double）
// ---------------------------------------------------------------------------

fn wrapper_field(
    cs: &mut ClassSpace,
    heap: &Heap,
    this: ObjRef,
    desc: &str,
) -> Result<FieldRef, KvmError> {
    let class = heap.instance(this)?.class;
    let name = cs.syms.intern("value");
    let desc_sym = cs.syms.intern(desc);
    cs.find_field(class, name, desc_sym)
}

fn wrapper_value(
    cs: &mut ClassSpace,
    heap: &Heap,
    this: ObjRef,
    desc: &str,
) -> Result<Slot, KvmError> {
    let field = wrapper_field(cs, heap, this, desc)?;
    Ok(heap.instance(this)?.fields[field.slot as usize])
}

fn wrapper_new(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    class_name: &str,
    desc: &str,
    value: Slot,
) -> Result<ObjRef, KvmError> {
    let class = cs.load_class(class_name)?;
    let name = cs.syms.intern("value");
    let desc_sym = cs.syms.intern(desc);
    let field = cs.find_field(class, name, desc_sym)?;
    let template = cs.class(class).instance_template.clone();
    let object = heap.alloc_instance(class, template)?;
    heap.instance_mut(object)?.fields[field.slot as usize] = value;
    Ok(object)
}

fn throw_nfe(cs: &mut ClassSpace, heap: &mut Heap, text: &str) -> KvmError {
    throw_java(
        cs,
        heap,
        "java/lang/NumberFormatException",
        Some(&format!("For input string: \"{text}\"")),
    )
}

// ---- Boolean ----------------------------------------------------------------

pub fn boolean_init(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let field = wrapper_field(_cs, heap, this, "Z")?;
    heap.instance_mut(this)?.fields[field.slot as usize] = Slot::Int(arg_i32(call, 0)?);
    Ok(None)
}

pub fn boolean_value(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_i32(match wrapper_value(cs, heap, this(call)?, "Z")? {
        Slot::Int(value) => value,
        other => {
            return Err(KvmError::Internal(format!(
                "Boolean.value expected int, got {other:?}"
            )));
        }
    })
}

pub fn boolean_to_string(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let value = match wrapper_value(cs, heap, this(call)?, "Z")? {
        Slot::Int(value) => value != 0,
        other => {
            return Err(KvmError::Internal(format!(
                "Boolean.value expected int, got {other:?}"
            )));
        }
    };
    ret_string(heap, utf16(if value { "true" } else { "false" }))
}

pub fn boolean_value_of(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let value = arg_i32(call, 0)?;
    let object = wrapper_new(_cs, heap, "java/lang/Boolean", "Z", Slot::Int(value))?;
    ret_ref(object)
}

// ---- Character 静态族 --------------------------------------------------------

fn is_ascii_digit(c: u16) -> bool {
    (0x30..=0x39).contains(&c)
}

fn is_ascii_letter(c: u16) -> bool {
    (0x61..=0x7a).contains(&c) || (0x41..=0x5a).contains(&c)
}

fn is_ascii_lower(c: u16) -> bool {
    (0x61..=0x7a).contains(&c)
}

fn is_ascii_upper(c: u16) -> bool {
    (0x41..=0x5a).contains(&c)
}

/// ASCII 分类（Unicode 表留待需要时补，见模块头）。
pub fn character_is_digit(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_bool(is_ascii_digit(arg_i32(call, 0)? as u16))
}

pub fn character_is_letter(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_bool(is_ascii_letter(arg_i32(call, 0)? as u16))
}

pub fn character_is_letter_or_digit(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let c = arg_i32(call, 0)? as u16;
    ret_bool(is_ascii_letter(c) || is_ascii_digit(c))
}

pub fn character_is_whitespace(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let c = arg_i32(call, 0)? as u16;
    ret_bool(c == 0x20 || (0x09..=0x0d).contains(&c))
}

pub fn character_is_lower(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_bool(is_ascii_lower(arg_i32(call, 0)? as u16))
}

pub fn character_is_upper(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_bool(is_ascii_upper(arg_i32(call, 0)? as u16))
}

pub fn character_to_lower(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_i32(ascii_lower(arg_i32(call, 0)? as u16) as i32)
}

pub fn character_to_upper(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_i32(ascii_upper(arg_i32(call, 0)? as u16) as i32)
}

/// digit(char, radix)：超出基数/非数字字符返回 -1。
pub fn character_digit(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let c = arg_i32(call, 0)? as u16;
    let radix = arg_i32(call, 1)?;
    if !(2..=36).contains(&radix) {
        return ret_i32(-1);
    }
    ret_i32(char_digit(c, radix as u32).map(|d| d as i32).unwrap_or(-1))
}

pub fn character_init(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let field = wrapper_field(_cs, heap, this, "C")?;
    heap.instance_mut(this)?.fields[field.slot as usize] = Slot::Int(arg_i32(call, 0)?);
    Ok(None)
}

pub fn character_char_value(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_i32(match wrapper_value(cs, heap, this(call)?, "C")? {
        Slot::Int(value) => value,
        other => {
            return Err(KvmError::Internal(format!(
                "Character.value expected int, got {other:?}"
            )));
        }
    })
}

pub fn character_to_string(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let value = match wrapper_value(cs, heap, this(call)?, "C")? {
        Slot::Int(value) => value as u16,
        other => {
            return Err(KvmError::Internal(format!(
                "Character.value expected int, got {other:?}"
            )));
        }
    };
    ret_string(heap, vec![value])
}

// ---- Integer -----------------------------------------------------------------

pub fn integer_init(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let field = wrapper_field(_cs, heap, this, "I")?;
    heap.instance_mut(this)?.fields[field.slot as usize] = Slot::Int(arg_i32(call, 0)?);
    Ok(None)
}

/// Integer.equals：按值比较（JDK 语义）。缺省走 Object.equals 身份比较
/// 会令 `Hashtable.get(new Integer(id))` 永远 miss——gjxjwql 资源表
/// ByteArrayInputStream(null) NPE 实证（h.b 表 Integer 键全查空）。
pub fn integer_equals(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let other = arg_ref(call, 0)?;
    if other.is_null() {
        return ret_i32(0);
    }
    // Integer 为 final：运行时类相同即同类（JDK equals 的类型判定）。
    let self_class = runtime_class_of(cs, heap, receiver)?;
    if runtime_class_of(cs, heap, other)? != self_class {
        return ret_i32(0);
    }
    let (Slot::Int(a), Slot::Int(b)) = (
        wrapper_value(cs, heap, receiver, "I")?,
        wrapper_value(cs, heap, other, "I")?,
    ) else {
        return Err(KvmError::Internal("Integer.value slot kind".into()));
    };
    ret_i32(i32::from(a == b))
}

/// Integer.hashCode() == value（JDK 语义；与 equals 配对，Hashtable 桶位
/// 按值分布）。
pub fn integer_hash_code(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    match wrapper_value(cs, heap, this(call)?, "I")? {
        Slot::Int(value) => ret_i32(value),
        other => Err(KvmError::Internal(format!(
            "Integer.value expected int, got {other:?}"
        ))),
    }
}

pub fn integer_int_value(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_i32(match wrapper_value(cs, heap, this(call)?, "I")? {
        Slot::Int(value) => value,
        other => {
            return Err(KvmError::Internal(format!(
                "Integer.value expected int, got {other:?}"
            )));
        }
    })
}

pub fn integer_parse_int(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let text_ref = arg_ref(call, 0)?;
    if text_ref.is_null() {
        return Err(throw_nfe(cs, heap, "null"));
    }
    let chars = string_chars(heap, text_ref)?;
    match parse_java_int(&chars, 10) {
        Some(value) => ret_i32(value),
        None => Err(throw_nfe(cs, heap, &String::from_utf16_lossy(&chars))),
    }
}

pub fn integer_parse_int_radix(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let text_ref = arg_ref(call, 0)?;
    if text_ref.is_null() {
        return Err(throw_nfe(cs, heap, "null"));
    }
    let chars = string_chars(heap, text_ref)?;
    let radix = arg_i32(call, 1)?;
    match parse_java_int(&chars, radix as u32) {
        Some(value) => ret_i32(value),
        None => Err(throw_nfe(cs, heap, &String::from_utf16_lossy(&chars))),
    }
}

pub fn integer_to_string(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let value = match wrapper_value(cs, heap, this(call)?, "I")? {
        Slot::Int(value) => value,
        other => {
            return Err(KvmError::Internal(format!(
                "Integer.value expected int, got {other:?}"
            )));
        }
    };
    ret_string(heap, utf16(&value.to_string()))
}

pub fn integer_to_string_static(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_string(heap, utf16(&arg_i32(call, 0)?.to_string()))
}

pub fn integer_to_string_radix(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let radix = arg_i32(call, 1)? as u32;
    ret_string(heap, utf16(&long_text_radix(arg_i32(call, 0)? as i64, radix)))
}

/// 十六进制（无符号语义：-1 → "ffffffff"）。
pub fn integer_to_hex_string(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_string(heap, utf16(&format!("{:x}", arg_i32(call, 0)? as u32)))
}

pub fn integer_value_of(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let object = wrapper_new(_cs, heap, "java/lang/Integer", "I", Slot::Int(arg_i32(call, 0)?))?;
    ret_ref(object)
}

pub fn integer_value_of_string(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let text_ref = arg_ref(call, 0)?;
    if text_ref.is_null() {
        return Err(throw_nfe(cs, heap, "null"));
    }
    let chars = string_chars(heap, text_ref)?;
    match parse_java_int(&chars, 10) {
        Some(value) => {
            let object = wrapper_new(cs, heap, "java/lang/Integer", "I", Slot::Int(value))?;
            ret_ref(object)
        }
        None => Err(throw_nfe(cs, heap, &String::from_utf16_lossy(&chars))),
    }
}

// ---- Long --------------------------------------------------------------------

pub fn long_init(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let field = wrapper_field(_cs, heap, this, "J")?;
    heap.instance_mut(this)?.fields[field.slot as usize] = Slot::Long(arg_i64(call, 0)?);
    Ok(None)
}

pub fn long_value(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(Some(match wrapper_value(cs, heap, this(call)?, "J")? {
        Slot::Long(value) => Slot::Long(value),
        other => {
            return Err(KvmError::Internal(format!(
                "Long.value expected long, got {other:?}"
            )));
        }
    }))
}

pub fn long_parse_long(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let text_ref = arg_ref(call, 0)?;
    if text_ref.is_null() {
        return Err(throw_nfe(cs, heap, "null"));
    }
    let chars = string_chars(heap, text_ref)?;
    match parse_java_long(&chars, 10) {
        Some(value) => Ok(Some(Slot::Long(value))),
        None => Err(throw_nfe(cs, heap, &String::from_utf16_lossy(&chars))),
    }
}

pub fn long_to_string(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let value = match wrapper_value(cs, heap, this(call)?, "J")? {
        Slot::Long(value) => value,
        other => {
            return Err(KvmError::Internal(format!(
                "Long.value expected long, got {other:?}"
            )));
        }
    };
    ret_string(heap, utf16(&long_text(value)))
}

pub fn long_to_string_static(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_string(heap, utf16(&long_text(arg_i64(call, 0)?)))
}

pub fn long_value_of(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let object = wrapper_new(_cs, heap, "java/lang/Long", "J", Slot::Long(arg_i64(call, 0)?))?;
    ret_ref(object)
}

// ---- Float -------------------------------------------------------------------

pub fn float_init(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let field = wrapper_field(_cs, heap, this, "F")?;
    heap.instance_mut(this)?.fields[field.slot as usize] = Slot::Float(arg_f32(call, 0)?);
    Ok(None)
}

pub fn float_value(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(Some(match wrapper_value(cs, heap, this(call)?, "F")? {
        Slot::Float(value) => Slot::Float(value),
        other => {
            return Err(KvmError::Internal(format!(
                "Float.value expected float, got {other:?}"
            )));
        }
    }))
}

pub fn float_parse_float(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let text_ref = arg_ref(call, 0)?;
    if text_ref.is_null() {
        return Err(throw_nfe(cs, heap, "null"));
    }
    let chars = string_chars(heap, text_ref)?;
    match parse_java_double(&chars) {
        Some(value) => Ok(Some(Slot::Float(value as f32))),
        None => Err(throw_nfe(cs, heap, &String::from_utf16_lossy(&chars))),
    }
}

pub fn float_to_string(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let value = match wrapper_value(cs, heap, this(call)?, "F")? {
        Slot::Float(value) => value,
        other => {
            return Err(KvmError::Internal(format!(
                "Float.value expected float, got {other:?}"
            )));
        }
    };
    ret_string(heap, utf16(&java_double_text(value as f64)))
}

pub fn float_to_string_static(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_string(heap, utf16(&java_double_text(arg_f32(call, 0)? as f64)))
}

pub fn float_value_of(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let object = wrapper_new(_cs, heap, "java/lang/Float", "F", Slot::Float(arg_f32(call, 0)?))?;
    ret_ref(object)
}

pub fn float_is_nan(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_bool(arg_f32(call, 0)?.is_nan())
}

pub fn float_is_infinite(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_bool(arg_f32(call, 0)?.is_infinite())
}

// ---- Double ------------------------------------------------------------------

pub fn double_init(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let field = wrapper_field(_cs, heap, this, "D")?;
    heap.instance_mut(this)?.fields[field.slot as usize] = Slot::Double(arg_f64(call, 0)?);
    Ok(None)
}

pub fn double_value(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(Some(match wrapper_value(cs, heap, this(call)?, "D")? {
        Slot::Double(value) => Slot::Double(value),
        other => {
            return Err(KvmError::Internal(format!(
                "Double.value expected double, got {other:?}"
            )));
        }
    }))
}

pub fn double_parse_double(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let text_ref = arg_ref(call, 0)?;
    if text_ref.is_null() {
        return Err(throw_nfe(cs, heap, "null"));
    }
    let chars = string_chars(heap, text_ref)?;
    match parse_java_double(&chars) {
        Some(value) => Ok(Some(Slot::Double(value))),
        None => Err(throw_nfe(cs, heap, &String::from_utf16_lossy(&chars))),
    }
}

pub fn double_to_string(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let value = match wrapper_value(cs, heap, this(call)?, "D")? {
        Slot::Double(value) => value,
        other => {
            return Err(KvmError::Internal(format!(
                "Double.value expected double, got {other:?}"
            )));
        }
    };
    ret_string(heap, utf16(&java_double_text(value)))
}

pub fn double_to_string_static(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_string(heap, utf16(&java_double_text(arg_f64(call, 0)?)))
}

pub fn double_value_of(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let object =
        wrapper_new(_cs, heap, "java/lang/Double", "D", Slot::Double(arg_f64(call, 0)?))?;
    ret_ref(object)
}

pub fn double_is_nan(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_bool(arg_f64(call, 0)?.is_nan())
}

pub fn double_is_infinite(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_bool(arg_f64(call, 0)?.is_infinite())
}
