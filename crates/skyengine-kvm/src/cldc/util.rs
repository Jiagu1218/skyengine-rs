//! java.util 原生实现（T015）。
//!
//! 函数签名与 `interp::natives::NativeFn` 一致，由 bootstrap 注册表
//! （natives.rs 的 java/util 段）按 类名.方法名+描述符 绑定。
//!
//! ## 实例存储选型（对齐 T014 StringBuffer：能存实例字段就存实例字段）
//!
//! 全部集合类状态存对象实例区字段，无 peer 结构：
//! - **Random**：`seed J`（Java 48 位 LCG，规范常量 0x5DEECE66D/0xB）。
//! - **Date / Calendar**：`time J` 毫秒数单一事实源；Calendar 的 get/set
//!   现场分解/重组（无字段缓存与脏标记，行为等价且无失效路径）。
//! - **Vector / Stack**：`data [Ljava/lang/Object;` + `count I`，
//!   容量翻倍扩容；Stack 无自有字段（继承 Vector 布局，方法分派经
//!   find_method 走父类链，实例模板由类空间按父类模板克隆）。
//! - **Hashtable**：`keys`/`values` 两个平行对象数组 + `count I`，
//!   **顺序表线性扫描**（设计允许"选最省事"：MIDlet 规模的表 O(n)
//!   查找可忽略，且避开开链/墓碑清理的正确性负担；哈希桶留待性能
//!   需要时替换）。key/value 相等性桥接：双方均为 Str 载荷时按
//!   UTF-16 内容比较（String.equals 语义），否则 ObjRef 身份比较
//!   （Object.equals 缺省语义）。
//! - **Enumeration 游标**：注册表内部引导类
//!   `com/skyengine/kvm/util/Enumerator`（声明 implements
//!   java/util/Enumeration），字段 `data [Ljava/lang/Object;` +
//!   `cursor I`；keys()/elements()/elements() 返回**快照**（保守：
//!   不随底层集合后续增删变化）。
//! - **Timer/TimerTask**：仅数据面——schedule* 把触发时刻/周期写入
//!   task 字段（period>0=固定延迟、period<0=固定速率，对齐 JDK 内部
//!   表示）并登记进 Timer 的任务数组；**实际触发接调度器定时器 =
//!   TODO T034**（Scheduler 已有 next_wake BTreeMap 与
//!   TimerCb::Placeholder）。Timer.cancel 仅置标志；purge 恒返 0。
//!
//! ## 已知边界（保守实现，注释逐处标注）
//!
//! - **TimeZone**：getDefault 返回固定 "GMT" 实例（raw offset 0、
//!   无夏令时）；宿主时区接入待引擎侧（TODO）。
//! - Calendar.set 对越界值**钳制**到合法域（Java lenient 模式的溢出
//!   归一化不实现）；set(DAY_OF_WEEK) 为派生字段，忽略。
//! - Random 无参构造的缺省种子用宿主单调时钟纳秒（Java 的原子递增
//!   序号对单线程 MIDlet 语义无影响）。
//! - native 内 `Heap::alloc*` 不经 `alloc_with_gc`（仅解释器分配路径
//!   触发 GC），Rust 局部引用在原生执行期安全。

use std::time::{SystemTime, UNIX_EPOCH};

use crate::cldc::lang::throw_java;
use crate::host::HostContext;
use crate::interp::NativeCall;
use crate::runtime::classspace::ClassSpace;
use crate::runtime::heap::Heap;
use crate::runtime::object::{ArrayData, ElemType, Payload};
use crate::runtime::{ObjRef, Slot};
use crate::scheduler::{Scheduler, TimerCb};
use crate::KvmError;

// ---------------------------------------------------------------------------
// 通用小工具（与 lang.rs 私有版重复；体量小，跨模块抽象不划算）
// ---------------------------------------------------------------------------

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

fn arg_ref(call: &NativeCall, index: usize) -> Result<ObjRef, KvmError> {
    match call.args.get(index) {
        Some(Slot::Ref(value)) => Ok(*value),
        other => Err(KvmError::Internal(format!(
            "native arg {index} expected ref, got {other:?}"
        ))),
    }
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

fn ret_long(value: i64) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Long(value)))
}

fn ret_float(value: f32) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Float(value)))
}

fn ret_double(value: f64) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Double(value)))
}

fn ret_string(heap: &mut Heap, chars: Vec<u16>) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Ref(heap.alloc_string_uninterned(chars)?)))
}

/// 宿主当前时刻（毫秒）：与 T014 System.currentTimeMillis 同源同式。
fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

// ---------------------------------------------------------------------------
// 实例字段 / 对象数组小工具
// ---------------------------------------------------------------------------

/// 字段槽位（走 find_field 保守取位；沿父类链解析，子类实例同槽位）。
fn field_slot(
    cs: &mut ClassSpace,
    class_name: &str,
    name: &str,
    desc: &str,
) -> Result<usize, KvmError> {
    let class = cs.load_class(class_name)?;
    let name = cs.syms.intern(name);
    let desc = cs.syms.intern(desc);
    Ok(cs.find_field(class, name, desc)?.slot as usize)
}

fn field_int(heap: &Heap, this: ObjRef, slot: usize) -> Result<i32, KvmError> {
    match &heap.instance(this)?.fields[slot] {
        Slot::Int(value) => Ok(*value),
        other => Err(KvmError::Internal(format!(
            "field slot {slot} expected int, got {other:?}"
        ))),
    }
}

fn field_long(heap: &Heap, this: ObjRef, slot: usize) -> Result<i64, KvmError> {
    match &heap.instance(this)?.fields[slot] {
        Slot::Long(value) => Ok(*value),
        other => Err(KvmError::Internal(format!(
            "field slot {slot} expected long, got {other:?}"
        ))),
    }
}

fn field_ref(heap: &Heap, this: ObjRef, slot: usize) -> Result<ObjRef, KvmError> {
    match &heap.instance(this)?.fields[slot] {
        Slot::Ref(value) => Ok(*value),
        other => Err(KvmError::Internal(format!(
            "field slot {slot} expected ref, got {other:?}"
        ))),
    }
}

fn set_field(
    heap: &mut Heap,
    this: ObjRef,
    slot: usize,
    value: Slot,
) -> Result<(), KvmError> {
    heap.instance_mut(this)?.fields[slot] = value;
    Ok(())
}

/// 分配指定 bootstrap 类的实例（模板零值）。
fn alloc_instance_of(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    class_name: &str,
) -> Result<ObjRef, KvmError> {
    let class = cs.load_class(class_name)?;
    let template = cs.class(class).instance_template.clone();
    heap.alloc_instance(class, template)
}

fn alloc_ref_array(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    len: usize,
) -> Result<ObjRef, KvmError> {
    let class = cs.load_class("[Ljava/lang/Object;")?;
    heap.alloc_array(class, ElemType::Ref, len)
}

fn ref_array_len(heap: &Heap, array: ObjRef) -> Result<usize, KvmError> {
    Ok(heap.array(array)?.data.len())
}

fn ref_array_get(heap: &Heap, array: ObjRef, index: usize) -> Result<ObjRef, KvmError> {
    match &heap.array(array)?.data {
        ArrayData::Refs(data) => Ok(data[index]),
        other => Err(KvmError::Internal(format!("expected ref[], got {other:?}"))),
    }
}

fn ref_array_set(
    heap: &mut Heap,
    array: ObjRef,
    index: usize,
    value: ObjRef,
) -> Result<(), KvmError> {
    match &mut heap.array_mut(array)?.data {
        ArrayData::Refs(data) => {
            data[index] = value;
            Ok(())
        }
        other => Err(KvmError::Internal(format!("expected ref[], got {other:?}"))),
    }
}

/// 引用相等性桥接：双 Str 按内容（String.equals），否则身份（Object.equals）。
fn refs_equal(heap: &Heap, a: ObjRef, b: ObjRef) -> bool {
    if a == b {
        return true;
    }
    if a.is_null() || b.is_null() {
        return false;
    }
    match (heap.payload(a), heap.payload(b)) {
        (Ok(Payload::Str(x)), Ok(Payload::Str(y))) => x.chars == y.chars,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// 历法换算（GMT 固定时区）
// ---------------------------------------------------------------------------

/// days → (y, m, d)：Howard Hinnant civil_from_days 公式照抄（月份 1 起）。
fn civil_from_days(z: i64) -> (i64, i32, i32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (y + i64::from(m <= 2), m as i32, d as i32)
}

/// (y, m, d) → days：Howard Hinnant days_from_civil 公式照抄（civil 逆）。
fn days_from_civil(y: i64, m: i32, d: i32) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 毫秒数分解结果（Calendar/Date 共用；month 1 起，day_of_week 1=周日）。
struct CivilDateTime {
    year: i64,
    month: i32,
    day: i32,
    hour: i32,
    minute: i32,
    second: i32,
    millis: i32,
    day_of_week: i32,
}

fn civil_date_time(ms: i64) -> CivilDateTime {
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    // 1970-01-01 为周四（DAY_OF_WEEK=5）：days 偏移 +4 对齐周日为 1 的编号，mod 7 回绕。
    let day_of_week = (days.rem_euclid(7) as i32 + 4) % 7 + 1;
    CivilDateTime {
        year,
        month,
        day,
        hour: (rem / 3_600_000) as i32,
        minute: (rem / 60_000 % 60) as i32,
        second: (rem / 1_000 % 60) as i32,
        millis: (rem % 1_000) as i32,
        day_of_week,
    }
}

/// 字段 → 毫秒数（civil_date_time 的逆；各分量须已在合法域内）。
fn millis_of(
    year: i64,
    month: i32,
    day: i32,
    hour: i32,
    minute: i32,
    second: i32,
    millis: i32,
) -> i64 {
    days_from_civil(year, month, day) * 86_400_000
        + hour as i64 * 3_600_000
        + minute as i64 * 60_000
        + second as i64 * 1_000
        + millis as i64
}

fn clamp_i32(value: i32, lo: i32, hi: i32) -> i32 {
    value.max(lo).min(hi)
}

const MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
/// 下标 = DAY_OF_WEEK - 1（1=周日）。
const WEEKDAY_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

// ---------------------------------------------------------------------------
// Random（Java 48 位 LCG：seed = (seed*0x5DEECE66D + 0xB) & (2^48-1)）
// ---------------------------------------------------------------------------

const RAND_MULTIPLIER: i64 = 0x5DEECE66D;
const RAND_ADDEND: i64 = 0xB;
const RAND_MASK: i64 = (1 << 48) - 1;

fn random_seed_slot(cs: &mut ClassSpace) -> Result<usize, KvmError> {
    field_slot(cs, "java/util/Random", "seed", "J")
}

/// 规范 next(bits)：推进种子并返回高 bits 位（[0, 2^bits)）。
fn random_next(
    heap: &mut Heap,
    this: ObjRef,
    slot: usize,
    bits: u32,
) -> Result<i64, KvmError> {
    let seed = field_long(heap, this, slot)?;
    let seed = seed
        .wrapping_mul(RAND_MULTIPLIER)
        .wrapping_add(RAND_ADDEND)
        & RAND_MASK;
    set_field(heap, this, slot, Slot::Long(seed))?;
    Ok(seed >> (48 - bits))
}

/// `Random()`：缺省种子 = 宿主单调时钟纳秒（近似唯一；
/// Java 的原子递增序号对单线程 MIDlet 语义无影响）。
pub fn random_init_default(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let slot = random_seed_slot(cs)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    set_field(heap, this, slot, Slot::Long((nanos ^ RAND_MULTIPLIER) & RAND_MASK))?;
    Ok(None)
}

/// `Random(long)`：规范扰动 `seed = (seed ^ 0x5DEECE66D) & (2^48-1)`。
pub fn random_init_seed(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let seed = arg_i64(call, 0)?;
    let slot = random_seed_slot(cs)?;
    set_field(heap, this, slot, Slot::Long((seed ^ RAND_MULTIPLIER) & RAND_MASK))?;
    Ok(None)
}

/// `nextInt()` = next(32)（含符号：高 32 位按 int 解释）。
pub fn random_next_int(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let slot = random_seed_slot(cs)?;
    ret_i32(random_next(heap, this, slot, 32)? as i32)
}

/// `nextInt(n)`（CLDC 语义：`next(31) % n`；n<=0 → IllegalArgumentException）。
pub fn random_next_int_bound(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let bound = arg_i32(call, 0)?;
    if bound <= 0 {
        return Err(throw_java(
            cs,
            heap,
            "java/lang/IllegalArgumentException",
            Some("n must be positive"),
        ));
    }
    let slot = random_seed_slot(cs)?;
    // next(31) ∈ [0, 2^31) 恒非负，% n 落在 [0, n)。
    ret_i32((random_next(heap, this, slot, 31)? % bound as i64) as i32)
}

/// `nextLong()` = `((long)next(32) << 32) + next(32)`：Java 的 next 返回
/// 带符号 int，高 32 位按 long **符号扩展**后移位，低 32 位按加法回绕。
pub fn random_next_long(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let slot = random_seed_slot(cs)?;
    let hi = random_next(heap, this, slot, 32)? as i32 as i64; // 符号扩展
    let lo = random_next(heap, this, slot, 32)? as i32 as i64;
    ret_long((hi << 32).wrapping_add(lo))
}

/// `nextFloat()` = `next(24) / 2^24`。
pub fn random_next_float(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let slot = random_seed_slot(cs)?;
    let value = random_next(heap, this, slot, 24)? as f32 / (1u32 << 24) as f32;
    ret_float(value)
}

/// `nextDouble()` = `((next(26) << 27) + next(27)) / 2^53`。
pub fn random_next_double(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let slot = random_seed_slot(cs)?;
    let hi = random_next(heap, this, slot, 26)?;
    let lo = random_next(heap, this, slot, 27)?;
    ret_double(((hi << 27) + lo) as f64 / (1u64 << 53) as f64)
}

// ---------------------------------------------------------------------------
// Date（实例字段 time J；时区恒 GMT）
// ---------------------------------------------------------------------------

fn date_slot(cs: &mut ClassSpace) -> Result<usize, KvmError> {
    field_slot(cs, "java/util/Date", "time", "J")
}

fn alloc_date(cs: &mut ClassSpace, heap: &mut Heap, ms: i64) -> Result<ObjRef, KvmError> {
    let date = alloc_instance_of(cs, heap, "java/util/Date")?;
    let slot = date_slot(cs)?;
    set_field(heap, date, slot, Slot::Long(ms))?;
    Ok(date)
}

fn date_ms_of(cs: &mut ClassSpace, heap: &Heap, date: ObjRef) -> Result<i64, KvmError> {
    let slot = date_slot(cs)?;
    field_long(heap, date, slot)
}

/// `Date()`：当前时刻（与 System.currentTimeMillis 同源）。
pub fn date_init(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let slot = date_slot(cs)?;
    set_field(heap, this, slot, Slot::Long(now_millis()))?;
    Ok(None)
}

pub fn date_init_long(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let ms = arg_i64(call, 0)?;
    let slot = date_slot(cs)?;
    set_field(heap, this, slot, Slot::Long(ms))?;
    Ok(None)
}

pub fn date_get_time(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_long(date_ms_of(cs, heap, this(call)?)?)
}

pub fn date_set_time(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let ms = arg_i64(call, 0)?;
    let slot = date_slot(cs)?;
    set_field(heap, this, slot, Slot::Long(ms))?;
    Ok(None)
}

/// `toString()`："EEE MMM dd HH:mm:ss zzz yyyy"（GMT），
/// 如 "Thu Jan 01 00:00:00 GMT 1970"（日月时分秒补零）。
pub fn date_to_string(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let ms = date_ms_of(cs, heap, this)?;
    let dt = civil_date_time(ms);
    let text = format!(
        "{} {} {:02} {:02}:{:02}:{:02} GMT {}",
        WEEKDAY_NAMES[(dt.day_of_week - 1) as usize],
        MONTH_NAMES[(dt.month - 1) as usize],
        dt.day,
        dt.hour,
        dt.minute,
        dt.second,
        dt.year
    );
    ret_string(heap, utf16(&text))
}

pub fn date_after(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let self_ms = date_ms_of(cs, heap, this(call)?)?;
    let other = arg_ref(call, 0)?;
    if other.is_null() {
        return Err(throw_java(cs, heap, "java/lang/NullPointerException", None));
    }
    ret_bool(self_ms > date_ms_of(cs, heap, other)?)
}

pub fn date_before(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let self_ms = date_ms_of(cs, heap, this(call)?)?;
    let other = arg_ref(call, 0)?;
    if other.is_null() {
        return Err(throw_java(cs, heap, "java/lang/NullPointerException", None));
    }
    ret_bool(self_ms < date_ms_of(cs, heap, other)?)
}

/// `equals(Object)`：运行时类相同且毫秒数相等。
pub fn date_equals(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let other = arg_ref(call, 0)?;
    if other.is_null() {
        return ret_bool(false);
    }
    let same_class = match (heap.payload(receiver)?, heap.payload(other)?) {
        (Payload::Instance(a), Payload::Instance(b)) => a.class == b.class,
        _ => false,
    };
    if !same_class {
        return ret_bool(false);
    }
    ret_bool(date_ms_of(cs, heap, receiver)? == date_ms_of(cs, heap, other)?)
}

pub fn date_compare_to(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let self_ms = date_ms_of(cs, heap, this(call)?)?;
    let other = arg_ref(call, 0)?;
    if other.is_null() {
        return Err(throw_java(cs, heap, "java/lang/NullPointerException", None));
    }
    let other_ms = date_ms_of(cs, heap, other)?;
    ret_i32(if self_ms < other_ms {
        -1
    } else if self_ms == other_ms {
        0
    } else {
        1
    })
}

/// `hashCode()` = `(int)(ms ^ (ms >>> 32))`。
pub fn date_hash_code(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let ms = date_ms_of(cs, heap, this(call)?)?;
    ret_i32((ms ^ ((ms as u64 >> 32) as i64)) as i32)
}

// ---------------------------------------------------------------------------
// Calendar（实例字段 time J 单一事实源；CLDC 规范形态为抽象类，
// getInstance 直接分配 java/util/Calendar 实例——规范抽象类的最小可
// 运行具体形态，jar 侧不会 new Calendar，无需独立子类）
// ---------------------------------------------------------------------------

/// CLDC Calendar 字段常量（与 JDK 编号一致）。
const CALENDAR_YEAR: i32 = 1;
const CALENDAR_MONTH: i32 = 2; // 0 起
const CALENDAR_DATE: i32 = 5; // 同 DAY_OF_MONTH
const CALENDAR_DAY_OF_WEEK: i32 = 7; // 1=周日
const CALENDAR_HOUR_OF_DAY: i32 = 11;
const CALENDAR_MINUTE: i32 = 12;
const CALENDAR_SECOND: i32 = 13;
const CALENDAR_MILLISECOND: i32 = 14;

fn calendar_slot(cs: &mut ClassSpace) -> Result<usize, KvmError> {
    field_slot(cs, "java/util/Calendar", "time", "J")
}

fn calendar_time_of(cs: &mut ClassSpace, heap: &Heap, this: ObjRef) -> Result<i64, KvmError> {
    let slot = calendar_slot(cs)?;
    field_long(heap, this, slot)
}

/// `getInstance()`：以宿主当前时间构造。
pub fn calendar_get_instance(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let instance = alloc_instance_of(cs, heap, "java/util/Calendar")?;
    let slot = calendar_slot(cs)?;
    set_field(heap, instance, slot, Slot::Long(now_millis()))?;
    ret_ref(instance)
}

pub fn calendar_get(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let field_id = arg_i32(call, 0)?;
    let dt = civil_date_time(calendar_time_of(cs, heap, this)?);
    let value = match field_id {
        CALENDAR_YEAR => dt.year as i32,
        CALENDAR_MONTH => dt.month - 1,
        CALENDAR_DATE => dt.day,
        CALENDAR_DAY_OF_WEEK => dt.day_of_week,
        CALENDAR_HOUR_OF_DAY => dt.hour,
        CALENDAR_MINUTE => dt.minute,
        CALENDAR_SECOND => dt.second,
        CALENDAR_MILLISECOND => dt.millis,
        _ => {
            return Err(throw_java(
                cs,
                heap,
                "java/lang/IllegalArgumentException",
                Some(&format!("bad Calendar field {field_id}")),
            ))
        }
    };
    ret_i32(value)
}

/// `set(field, value)`：分解 → 替换 → 重组（保守：越界值钳制到合法域，
/// Java lenient 溢出归一化不实现；DAY_OF_WEEK 为派生字段，忽略）。
pub fn calendar_set(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let field_id = arg_i32(call, 0)?;
    let value = arg_i32(call, 1)?;
    let dt = civil_date_time(calendar_time_of(cs, heap, this)?);
    let (year, month, day, hour, minute, second, millis) = match field_id {
        CALENDAR_YEAR => {
            (value as i64, dt.month, dt.day, dt.hour, dt.minute, dt.second, dt.millis)
        }
        CALENDAR_MONTH => (
            dt.year,
            clamp_i32(value, 0, 11) + 1,
            dt.day,
            dt.hour,
            dt.minute,
            dt.second,
            dt.millis,
        ),
        CALENDAR_DATE => (
            dt.year,
            dt.month,
            clamp_i32(value, 1, 31),
            dt.hour,
            dt.minute,
            dt.second,
            dt.millis,
        ),
        CALENDAR_DAY_OF_WEEK => return Ok(None), // 派生字段：不落地
        CALENDAR_HOUR_OF_DAY => (
            dt.year,
            dt.month,
            dt.day,
            clamp_i32(value, 0, 23),
            dt.minute,
            dt.second,
            dt.millis,
        ),
        CALENDAR_MINUTE => (
            dt.year,
            dt.month,
            dt.day,
            dt.hour,
            clamp_i32(value, 0, 59),
            dt.second,
            dt.millis,
        ),
        CALENDAR_SECOND => (
            dt.year,
            dt.month,
            dt.day,
            dt.hour,
            dt.minute,
            clamp_i32(value, 0, 59),
            dt.millis,
        ),
        CALENDAR_MILLISECOND => (
            dt.year,
            dt.month,
            dt.day,
            dt.hour,
            dt.minute,
            dt.second,
            clamp_i32(value, 0, 999),
        ),
        _ => {
            return Err(throw_java(
                cs,
                heap,
                "java/lang/IllegalArgumentException",
                Some(&format!("bad Calendar field {field_id}")),
            ))
        }
    };
    let slot = calendar_slot(cs)?;
    set_field(
        heap,
        this,
        slot,
        Slot::Long(millis_of(year, month, day, hour, minute, second, millis)),
    )?;
    Ok(None)
}

/// `getTime()`：按当前毫秒数物化 Date。
pub fn calendar_get_time(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let ms = calendar_time_of(cs, heap, this(call)?)?;
    ret_ref(alloc_date(cs, heap, ms)?)
}

/// `setTime(Date)`：Date 为 null → NPE。
pub fn calendar_set_time(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let date = arg_ref(call, 0)?;
    if date.is_null() {
        return Err(throw_java(cs, heap, "java/lang/NullPointerException", None));
    }
    let ms = date_ms_of(cs, heap, date)?;
    let slot = calendar_slot(cs)?;
    set_field(heap, this, slot, Slot::Long(ms))?;
    Ok(None)
}

pub fn calendar_get_time_in_millis(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_long(calendar_time_of(cs, heap, this(call)?)?)
}

pub fn calendar_set_time_in_millis(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let ms = arg_i64(call, 0)?;
    let slot = calendar_slot(cs)?;
    set_field(heap, this, slot, Slot::Long(ms))?;
    Ok(None)
}

// ---------------------------------------------------------------------------
// TimeZone（固定 "GMT"：raw offset 0、无夏令时）
// ---------------------------------------------------------------------------

/// `getDefault()`：固定返回 GMT 形态实例（无状态，每次新分配）。
/// TODO：宿主时区接入待引擎侧（需要引擎注入时区表 + 随系统切换）。
pub fn timezone_get_default(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_ref(alloc_instance_of(cs, heap, "java/util/TimeZone")?)
}

pub fn timezone_get_id(
    _cs: &mut ClassSpace,
    heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_string(heap, utf16("GMT"))
}

pub fn timezone_get_raw_offset(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_i32(0)
}

/// `getOffset(era, year, month, day, dayOfWeek, millis)`：GMT 恒 0。
pub fn timezone_get_offset(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_i32(0)
}

pub fn timezone_uses_daylight_time(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_bool(false)
}

// ---------------------------------------------------------------------------
// Hashtable（平行数组顺序表：keys/values + count；线性扫描）
// ---------------------------------------------------------------------------

fn ht_slots(cs: &mut ClassSpace) -> Result<(usize, usize, usize), KvmError> {
    let keys = field_slot(cs, "java/util/Hashtable", "keys", "[Ljava/lang/Object;")?;
    let values = field_slot(cs, "java/util/Hashtable", "values", "[Ljava/lang/Object;")?;
    let count = field_slot(cs, "java/util/Hashtable", "count", "I")?;
    Ok((keys, values, count))
}

fn ht_init_with(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    this: ObjRef,
    capacity: usize,
) -> Result<(), KvmError> {
    let slots = ht_slots(cs)?;
    let keys = alloc_ref_array(cs, heap, capacity)?;
    let values = alloc_ref_array(cs, heap, capacity)?;
    set_field(heap, this, slots.0, Slot::Ref(keys))?;
    set_field(heap, this, slots.1, Slot::Ref(values))?;
    set_field(heap, this, slots.2, Slot::Int(0))?;
    Ok(())
}

/// `Hashtable()`：缺省容量 11（对齐 JDK）。
pub fn ht_init(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ht_init_with(cs, heap, this(call)?, 11)?;
    Ok(None)
}

/// `Hashtable(int)`：容量 <0 → IllegalArgumentException。
pub fn ht_init_capacity(
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
            "java/lang/IllegalArgumentException",
            Some(&capacity.to_string()),
        ));
    }
    ht_init_with(cs, heap, this, capacity.max(1) as usize)?;
    Ok(None)
}

/// Hashtable 键语义等价：引用快路径后按运行时类分派——String 内容、
/// bootstrap 包装类按 value（唯一实例字段 slot 0）、其余身份。
/// gjxjwql 资源表实证：Integer 键若按身份比较，`get(new Integer(id))`
/// 永远 miss → ByteArrayInputStream(null) NPE（equals 语义在集合侧）。
fn keys_equal(
    cs: &ClassSpace,
    heap: &Heap,
    a: ObjRef,
    b: ObjRef,
) -> Result<bool, KvmError> {
    if a == b {
        return Ok(true);
    }
    if a.is_null() || b.is_null() {
        return Ok(false);
    }
    if let (Ok(Payload::Str(x)), Ok(Payload::Str(y))) = (heap.payload(a), heap.payload(b)) {
        return Ok(x.chars == y.chars);
    }
    let class_a = crate::interp::runtime_class_of(cs, heap, a)?;
    let class_b = crate::interp::runtime_class_of(cs, heap, b)?;
    if class_a != class_b {
        return Ok(false);
    }
    if !matches!(
        cs.name_of(class_a),
        "java/lang/Integer"
            | "java/lang/Long"
            | "java/lang/Short"
            | "java/lang/Byte"
            | "java/lang/Boolean"
            | "java/lang/Character"
            | "java/lang/Float"
            | "java/lang/Double"
    ) {
        return Ok(false);
    }
    let (Some(value_a), Some(value_b)) = (
        heap.instance(a)?.fields.first(),
        heap.instance(b)?.fields.first(),
    ) else {
        return Ok(false);
    };
    Ok(value_a == value_b)
}

/// 顺序表定位：返回命中下标（未命中 -1）。key 为 null 由调用方先拦截。
fn ht_find_index(
    cs: &ClassSpace,
    heap: &Heap,
    keys: ObjRef,
    count: usize,
    key: ObjRef,
) -> Result<i32, KvmError> {
    for index in 0..count {
        if keys_equal(cs, heap, ref_array_get(heap, keys, index)?, key)? {
            return Ok(index as i32);
        }
    }
    Ok(-1)
}

/// 扩容（2n+1，对齐 JDK rehash 步长）并搬迁既有条目。
fn ht_grow(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    this: ObjRef,
    slots: (usize, usize, usize),
) -> Result<(), KvmError> {
    let (keys, values) =
        (field_ref(heap, this, slots.0)?, field_ref(heap, this, slots.1)?);
    let count = field_int(heap, this, slots.2)? as usize;
    let capacity = ref_array_len(heap, keys)?;
    let fresh_keys = alloc_ref_array(cs, heap, capacity * 2 + 1)?;
    let fresh_values = alloc_ref_array(cs, heap, capacity * 2 + 1)?;
    for index in 0..count {
        ref_array_set(heap, fresh_keys, index, ref_array_get(heap, keys, index)?)?;
        ref_array_set(heap, fresh_values, index, ref_array_get(heap, values, index)?)?;
    }
    set_field(heap, this, slots.0, Slot::Ref(fresh_keys))?;
    set_field(heap, this, slots.1, Slot::Ref(fresh_values))?;
    Ok(())
}

/// `put(key, value)`：null key/value → NPE；同 key 覆盖返回旧值。
pub fn ht_put(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let key = arg_ref(call, 0)?;
    let value = arg_ref(call, 1)?;
    if key.is_null() || value.is_null() {
        return Err(throw_java(cs, heap, "java/lang/NullPointerException", None));
    }
    let slots = ht_slots(cs)?;
    let keys = field_ref(heap, this, slots.0)?;
    let count = field_int(heap, this, slots.2)? as usize;
    let existing = ht_find_index(cs, heap, keys, count, key)?;
    if existing >= 0 {
        let values = field_ref(heap, this, slots.1)?;
        let old = ref_array_get(heap, values, existing as usize)?;
        ref_array_set(heap, values, existing as usize, value)?;
        return ret_ref(old);
    }
    let count = if count == ref_array_len(heap, keys)? {
        ht_grow(cs, heap, this, slots)?;
        field_int(heap, this, slots.2)? as usize
    } else {
        count
    };
    let keys = field_ref(heap, this, slots.0)?;
    let values = field_ref(heap, this, slots.1)?;
    ref_array_set(heap, keys, count, key)?;
    ref_array_set(heap, values, count, value)?;
    set_field(heap, this, slots.2, Slot::Int(count as i32 + 1))?;
    ret_ref(ObjRef::NULL)
}

/// `get(key)`：null key → NPE；未命中返回 null。
pub fn ht_get(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let key = arg_ref(call, 0)?;
    if key.is_null() {
        return Err(throw_java(cs, heap, "java/lang/NullPointerException", None));
    }
    let slots = ht_slots(cs)?;
    let keys = field_ref(heap, this, slots.0)?;
    let count = field_int(heap, this, slots.2)? as usize;
    let existing = ht_find_index(cs, heap, keys, count, key)?;
    if existing < 0 {
        return ret_ref(ObjRef::NULL);
    }
    let values = field_ref(heap, this, slots.1)?;
    ret_ref(ref_array_get(heap, values, existing as usize)?)
}

/// `remove(key)`：删除并返回旧值（后继条目前移；Hashtable 无序保证）。
pub fn ht_remove(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let key = arg_ref(call, 0)?;
    if key.is_null() {
        return Err(throw_java(cs, heap, "java/lang/NullPointerException", None));
    }
    let slots = ht_slots(cs)?;
    let keys = field_ref(heap, this, slots.0)?;
    let values = field_ref(heap, this, slots.1)?;
    let count = field_int(heap, this, slots.2)? as usize;
    let existing = ht_find_index(cs, heap, keys, count, key)?;
    if existing < 0 {
        return ret_ref(ObjRef::NULL);
    }
    let index = existing as usize;
    let old = ref_array_get(heap, values, index)?;
    // 前移填补空洞并清空尾部（旧引用不滞留，利于 GC）。
    for i in index..count - 1 {
        ref_array_set(heap, keys, i, ref_array_get(heap, keys, i + 1)?)?;
        ref_array_set(heap, values, i, ref_array_get(heap, values, i + 1)?)?;
    }
    ref_array_set(heap, keys, count - 1, ObjRef::NULL)?;
    ref_array_set(heap, values, count - 1, ObjRef::NULL)?;
    set_field(heap, this, slots.2, Slot::Int(count as i32 - 1))?;
    ret_ref(old)
}

pub fn ht_contains_key(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let key = arg_ref(call, 0)?;
    if key.is_null() {
        return Err(throw_java(cs, heap, "java/lang/NullPointerException", None));
    }
    let slots = ht_slots(cs)?;
    let keys = field_ref(heap, this, slots.0)?;
    let count = field_int(heap, this, slots.2)? as usize;
    ret_bool(ht_find_index(cs, heap, keys, count, key)? >= 0)
}

/// `contains(value)`：值侧扫描（JDK 语义：contains 与 containsValue 同义）。
pub fn ht_contains(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let value = arg_ref(call, 0)?;
    if value.is_null() {
        return Err(throw_java(cs, heap, "java/lang/NullPointerException", None));
    }
    let slots = ht_slots(cs)?;
    let values = field_ref(heap, this, slots.1)?;
    let count = field_int(heap, this, slots.2)? as usize;
    for index in 0..count {
        if refs_equal(heap, ref_array_get(heap, values, index)?, value) {
            return ret_bool(true);
        }
    }
    ret_bool(false)
}

pub fn ht_size(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let slots = ht_slots(cs)?;
    ret_i32(field_int(heap, this(call)?, slots.2)?)
}

pub fn ht_is_empty(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let slots = ht_slots(cs)?;
    ret_bool(field_int(heap, this(call)?, slots.2)? == 0)
}

/// `clear()`：条目置 null（旧引用不滞留）。
pub fn ht_clear(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let slots = ht_slots(cs)?;
    let keys = field_ref(heap, this, slots.0)?;
    let values = field_ref(heap, this, slots.1)?;
    let count = field_int(heap, this, slots.2)? as usize;
    for index in 0..count {
        ref_array_set(heap, keys, index, ObjRef::NULL)?;
        ref_array_set(heap, values, index, ObjRef::NULL)?;
    }
    set_field(heap, this, slots.2, Slot::Int(0))?;
    Ok(None)
}

/// 把 [0, count) 的 keys 或 values 快照成枚举游标。
fn ht_snapshot_enum(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    this: ObjRef,
    keys_side: bool,
) -> Result<ObjRef, KvmError> {
    let slots = ht_slots(cs)?;
    let source = field_ref(heap, this, if keys_side { slots.0 } else { slots.1 })?;
    let count = field_int(heap, this, slots.2)? as usize;
    let snapshot = alloc_ref_array(cs, heap, count)?;
    for index in 0..count {
        ref_array_set(heap, snapshot, index, ref_array_get(heap, source, index)?)?;
    }
    new_enumerator(cs, heap, snapshot)
}

/// `keys()`：键快照枚举（不随后续增删变化，保守快照语义）。
pub fn ht_keys(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_ref(ht_snapshot_enum(cs, heap, this(call)?, true)?)
}

/// `elements()`：值快照枚举。
pub fn ht_elements(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_ref(ht_snapshot_enum(cs, heap, this(call)?, false)?)
}

// ---------------------------------------------------------------------------
// Enumeration 游标（引导类 com/skyengine/kvm/util/Enumerator）
// ---------------------------------------------------------------------------

fn enumerator_slots(cs: &mut ClassSpace) -> Result<(usize, usize), KvmError> {
    let data =
        field_slot(cs, "com/skyengine/kvm/util/Enumerator", "data", "[Ljava/lang/Object;")?;
    let cursor = field_slot(cs, "com/skyengine/kvm/util/Enumerator", "cursor", "I")?;
    Ok((data, cursor))
}

fn new_enumerator(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    data: ObjRef,
) -> Result<ObjRef, KvmError> {
    let instance = alloc_instance_of(cs, heap, "com/skyengine/kvm/util/Enumerator")?;
    let slots = enumerator_slots(cs)?;
    set_field(heap, instance, slots.0, Slot::Ref(data))?;
    set_field(heap, instance, slots.1, Slot::Int(0))?;
    Ok(instance)
}

pub fn enumerator_has_more(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let slots = enumerator_slots(cs)?;
    let data = field_ref(heap, this, slots.0)?;
    if data.is_null() {
        return ret_bool(false);
    }
    let cursor = field_int(heap, this, slots.1)?;
    ret_bool(cursor >= 0 && (cursor as usize) < ref_array_len(heap, data)?)
}

/// `nextElement()`：游标越界 → NoSuchElementException。
pub fn enumerator_next(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let slots = enumerator_slots(cs)?;
    let data = field_ref(heap, this, slots.0)?;
    let cursor = field_int(heap, this, slots.1)?;
    if data.is_null() || cursor < 0 || cursor as usize >= ref_array_len(heap, data)? {
        return Err(throw_java(cs, heap, "java/util/NoSuchElementException", None));
    }
    let element = ref_array_get(heap, data, cursor as usize)?;
    set_field(heap, this, slots.1, Slot::Int(cursor + 1))?;
    ret_ref(element)
}

// ---------------------------------------------------------------------------
// Vector（data + count；容量翻倍扩容；允许 null 元素）
// ---------------------------------------------------------------------------

fn vec_slots(cs: &mut ClassSpace) -> Result<(usize, usize), KvmError> {
    let data = field_slot(cs, "java/util/Vector", "data", "[Ljava/lang/Object;")?;
    let count = field_slot(cs, "java/util/Vector", "count", "I")?;
    Ok((data, count))
}

/// 容量保障：不足时翻倍（needed 为最小要求）并搬迁 [0, count)。
fn vec_ensure(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    this: ObjRef,
    slots: (usize, usize),
    needed: i32,
) -> Result<(), KvmError> {
    let data = field_ref(heap, this, slots.0)?;
    if data.is_null() {
        return Err(KvmError::Internal("Vector not initialized".into()));
    }
    let capacity = ref_array_len(heap, data)? as i32;
    if needed <= capacity {
        return Ok(());
    }
    let count = field_int(heap, this, slots.1)? as usize;
    let fresh = alloc_ref_array(cs, heap, (needed.max(capacity * 2)).max(1) as usize)?;
    for index in 0..count {
        ref_array_set(heap, fresh, index, ref_array_get(heap, data, index)?)?;
    }
    set_field(heap, this, slots.0, Slot::Ref(fresh))?;
    Ok(())
}

fn vec_throw_aioobe(cs: &mut ClassSpace, heap: &mut Heap, index: i32) -> KvmError {
    throw_java(
        cs,
        heap,
        "java/lang/ArrayIndexOutOfBoundsException",
        Some(&format!("Array index out of range: {index}")),
    )
}

/// `Vector()`：缺省容量 10（对齐 JDK）。
pub fn vec_init(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let slots = vec_slots(cs)?;
    let data = alloc_ref_array(cs, heap, 10)?;
    set_field(heap, this, slots.0, Slot::Ref(data))?;
    set_field(heap, this, slots.1, Slot::Int(0))?;
    Ok(None)
}

/// `Vector(int)`：容量 <0 → IllegalArgumentException。
pub fn vec_init_capacity(
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
            "java/lang/IllegalArgumentException",
            Some(&capacity.to_string()),
        ));
    }
    let slots = vec_slots(cs)?;
    let data = alloc_ref_array(cs, heap, capacity.max(1) as usize)?;
    set_field(heap, this, slots.0, Slot::Ref(data))?;
    set_field(heap, this, slots.1, Slot::Int(0))?;
    Ok(None)
}

pub fn vec_size(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let slots = vec_slots(cs)?;
    ret_i32(field_int(heap, this(call)?, slots.1)?)
}

pub fn vec_capacity(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let slots = vec_slots(cs)?;
    let data = field_ref(heap, this, slots.0)?;
    ret_i32(ref_array_len(heap, data)? as i32)
}

pub fn vec_is_empty(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let slots = vec_slots(cs)?;
    ret_bool(field_int(heap, this(call)?, slots.1)? == 0)
}

pub fn vec_add_element(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let element = arg_ref(call, 0)?;
    let slots = vec_slots(cs)?;
    let count = field_int(heap, this, slots.1)?;
    vec_ensure(cs, heap, this, slots, count + 1)?;
    let data = field_ref(heap, this, slots.0)?;
    ref_array_set(heap, data, count as usize, element)?;
    set_field(heap, this, slots.1, Slot::Int(count + 1))?;
    Ok(None)
}

pub fn vec_element_at(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let index = arg_i32(call, 0)?;
    let slots = vec_slots(cs)?;
    let count = field_int(heap, this, slots.1)?;
    if index < 0 || index >= count {
        return Err(vec_throw_aioobe(cs, heap, index));
    }
    let data = field_ref(heap, this, slots.0)?;
    ret_ref(ref_array_get(heap, data, index as usize)?)
}

pub fn vec_set_element_at(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let element = arg_ref(call, 0)?;
    let index = arg_i32(call, 1)?;
    let slots = vec_slots(cs)?;
    let count = field_int(heap, this, slots.1)?;
    if index < 0 || index >= count {
        return Err(vec_throw_aioobe(cs, heap, index));
    }
    let data = field_ref(heap, this, slots.0)?;
    ref_array_set(heap, data, index as usize, element)?;
    Ok(None)
}

pub fn vec_insert_element_at(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let element = arg_ref(call, 0)?;
    let index = arg_i32(call, 1)?;
    let slots = vec_slots(cs)?;
    let count = field_int(heap, this, slots.1)?;
    if index < 0 || index > count {
        return Err(vec_throw_aioobe(cs, heap, index));
    }
    vec_ensure(cs, heap, this, slots, count + 1)?;
    let data = field_ref(heap, this, slots.0)?;
    // 自尾向 index 右移一格。
    let mut i = count;
    while i > index {
        ref_array_set(heap, data, i as usize, ref_array_get(heap, data, (i - 1) as usize)?)?;
        i -= 1;
    }
    ref_array_set(heap, data, index as usize, element)?;
    set_field(heap, this, slots.1, Slot::Int(count + 1))?;
    Ok(None)
}

pub fn vec_remove_element_at(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let index = arg_i32(call, 0)?;
    let slots = vec_slots(cs)?;
    let count = field_int(heap, this, slots.1)?;
    if index < 0 || index >= count {
        return Err(vec_throw_aioobe(cs, heap, index));
    }
    let data = field_ref(heap, this, slots.0)?;
    // index 后左移一格并清空尾部。
    let mut i = index;
    while i + 1 < count {
        ref_array_set(heap, data, i as usize, ref_array_get(heap, data, (i + 1) as usize)?)?;
        i += 1;
    }
    ref_array_set(heap, data, (count - 1) as usize, ObjRef::NULL)?;
    set_field(heap, this, slots.1, Slot::Int(count - 1))?;
    Ok(None)
}

/// `removeElement(obj)`：命中删除返回 true（顺序表扫描）。
pub fn vec_remove_element(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let element = arg_ref(call, 0)?;
    let slots = vec_slots(cs)?;
    let data = field_ref(heap, this, slots.0)?;
    let count = field_int(heap, this, slots.1)?;
    for index in 0..count {
        if refs_equal(heap, ref_array_get(heap, data, index as usize)?, element) {
            // 命中：内联 removeElementAt 的左移逻辑（下标已验合法）。
            let mut i = index;
            while i + 1 < count {
                ref_array_set(
                    heap,
                    data,
                    i as usize,
                    ref_array_get(heap, data, (i + 1) as usize)?,
                )?;
                i += 1;
            }
            ref_array_set(heap, data, (count - 1) as usize, ObjRef::NULL)?;
            set_field(heap, this, slots.1, Slot::Int(count - 1))?;
            return ret_bool(true);
        }
    }
    ret_bool(false)
}

pub fn vec_contains(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let element = arg_ref(call, 0)?;
    let slots = vec_slots(cs)?;
    let data = field_ref(heap, this, slots.0)?;
    let count = field_int(heap, this, slots.1)?;
    for index in 0..count {
        if refs_equal(heap, ref_array_get(heap, data, index as usize)?, element) {
            return ret_bool(true);
        }
    }
    ret_bool(false)
}

pub fn vec_index_of(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let element = arg_ref(call, 0)?;
    let slots = vec_slots(cs)?;
    let data = field_ref(heap, this, slots.0)?;
    let count = field_int(heap, this, slots.1)?;
    for index in 0..count {
        if refs_equal(heap, ref_array_get(heap, data, index as usize)?, element) {
            return ret_i32(index);
        }
    }
    ret_i32(-1)
}

/// `indexOf(obj, fromIndex)`：fromIndex <0 → IndexOutOfBoundsException（JDK 语义）。
pub fn vec_index_of_from(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let element = arg_ref(call, 0)?;
    let from = arg_i32(call, 1)?;
    if from < 0 {
        return Err(vec_throw_aioobe(cs, heap, from));
    }
    let slots = vec_slots(cs)?;
    let data = field_ref(heap, this, slots.0)?;
    let count = field_int(heap, this, slots.1)?;
    let mut index = from;
    while index < count {
        if refs_equal(heap, ref_array_get(heap, data, index as usize)?, element) {
            return ret_i32(index);
        }
        index += 1;
    }
    ret_i32(-1)
}

/// `removeAllElements()`：元素置 null（旧引用不滞留）。
pub fn vec_remove_all_elements(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let slots = vec_slots(cs)?;
    let data = field_ref(heap, this, slots.0)?;
    let count = field_int(heap, this, slots.1)?;
    for index in 0..count {
        ref_array_set(heap, data, index as usize, ObjRef::NULL)?;
    }
    set_field(heap, this, slots.1, Slot::Int(0))?;
    Ok(None)
}

/// `firstElement()`：空表 → NoSuchElementException。
pub fn vec_first_element(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let slots = vec_slots(cs)?;
    let count = field_int(heap, this, slots.1)?;
    if count == 0 {
        return Err(throw_java(cs, heap, "java/util/NoSuchElementException", None));
    }
    let data = field_ref(heap, this, slots.0)?;
    ret_ref(ref_array_get(heap, data, 0)?)
}

/// `lastElement()`：空表 → NoSuchElementException。
pub fn vec_last_element(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let slots = vec_slots(cs)?;
    let count = field_int(heap, this, slots.1)?;
    if count == 0 {
        return Err(throw_java(cs, heap, "java/util/NoSuchElementException", None));
    }
    let data = field_ref(heap, this, slots.0)?;
    ret_ref(ref_array_get(heap, data, (count - 1) as usize)?)
}

/// `elements()`：快照枚举。
pub fn vec_elements(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let slots = vec_slots(cs)?;
    let data = field_ref(heap, this, slots.0)?;
    let count = field_int(heap, this, slots.1)? as usize;
    let snapshot = alloc_ref_array(cs, heap, count)?;
    for index in 0..count {
        ref_array_set(heap, snapshot, index, ref_array_get(heap, data, index)?)?;
    }
    ret_ref(new_enumerator(cs, heap, snapshot)?)
}

/// `copyInto(Object[])`：目标过短 → ArrayIndexOutOfBoundsException。
pub fn vec_copy_into(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let target = arg_ref(call, 0)?;
    if target.is_null() {
        return Err(throw_java(cs, heap, "java/lang/NullPointerException", None));
    }
    let slots = vec_slots(cs)?;
    let data = field_ref(heap, this, slots.0)?;
    let count = field_int(heap, this, slots.1)? as usize;
    if ref_array_len(heap, target)? < count {
        return Err(vec_throw_aioobe(cs, heap, count as i32));
    }
    for index in 0..count {
        let element = ref_array_get(heap, data, index)?;
        ref_array_set(heap, target, index, element)?;
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Stack extends Vector（无自有字段，复用 Vector 布局/工具）
// ---------------------------------------------------------------------------

/// `push(item)`：压栈并返回 item。
pub fn stack_push(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let item = arg_ref(call, 0)?;
    let slots = vec_slots(cs)?;
    let count = field_int(heap, this, slots.1)?;
    vec_ensure(cs, heap, this, slots, count + 1)?;
    let data = field_ref(heap, this, slots.0)?;
    ref_array_set(heap, data, count as usize, item)?;
    set_field(heap, this, slots.1, Slot::Int(count + 1))?;
    ret_ref(item)
}

/// `pop()`：空栈 → EmptyStackException。
pub fn stack_pop(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let slots = vec_slots(cs)?;
    let count = field_int(heap, this, slots.1)?;
    if count == 0 {
        return Err(throw_java(cs, heap, "java/util/EmptyStackException", None));
    }
    let data = field_ref(heap, this, slots.0)?;
    let top = ref_array_get(heap, data, (count - 1) as usize)?;
    ref_array_set(heap, data, (count - 1) as usize, ObjRef::NULL)?;
    set_field(heap, this, slots.1, Slot::Int(count - 1))?;
    ret_ref(top)
}

/// `peek()`：空栈 → EmptyStackException；不弹栈。
pub fn stack_peek(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let slots = vec_slots(cs)?;
    let count = field_int(heap, this, slots.1)?;
    if count == 0 {
        return Err(throw_java(cs, heap, "java/util/EmptyStackException", None));
    }
    let data = field_ref(heap, this, slots.0)?;
    ret_ref(ref_array_get(heap, data, (count - 1) as usize)?)
}

pub fn stack_empty(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let slots = vec_slots(cs)?;
    ret_bool(field_int(heap, this(call)?, slots.1)? == 0)
}

/// `search(obj)`：自顶向下 1 距离编号（栈顶=1）；未命中 -1。
pub fn stack_search(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let element = arg_ref(call, 0)?;
    let slots = vec_slots(cs)?;
    let data = field_ref(heap, this, slots.0)?;
    let count = field_int(heap, this, slots.1)?;
    let mut index = count - 1;
    while index >= 0 {
        if refs_equal(heap, ref_array_get(heap, data, index as usize)?, element) {
            return ret_i32(count - index);
        }
        index -= 1;
    }
    ret_i32(-1)
}

// ---------------------------------------------------------------------------
// Timer / TimerTask（T034：数据面 + 触发面）
//
// - 数据面：nextTime/period/cancelled/timer 实例字段（JDK 内部表示：
//   period>0=固定延迟、period<0=固定速率）+ Timer 的任务表；
// - 触发面：schedule 类 native 置 `cs.pending_timers` 请求位（native
//   无调度器通道），`Vm.poll` 段零消费 → `scheduler.schedule_timer`；
//   到期由调度器 `run_due_timers` 处理（惰性查取消、fixed-rate 重排、
//   spawn run 线程——见 scheduler.rs）。
// ---------------------------------------------------------------------------

/// TimerTask 字段槽位（nextTime/period/cancelled/timer）。
fn timer_task_slots(
    cs: &mut ClassSpace,
) -> Result<(usize, usize, usize, usize), KvmError> {
    let next = field_slot(cs, "java/util/TimerTask", "nextTime", "J")?;
    let period = field_slot(cs, "java/util/TimerTask", "period", "J")?;
    let cancelled = field_slot(cs, "java/util/TimerTask", "cancelled", "Z")?;
    let timer = field_slot(cs, "java/util/TimerTask", "timer", "Ljava/util/Timer;")?;
    Ok((next, period, cancelled, timer))
}

/// Timer 字段槽位（tasks/count/cancelled）。
fn timer_slots(cs: &mut ClassSpace) -> Result<(usize, usize, usize), KvmError> {
    let tasks = field_slot(cs, "java/util/Timer", "tasks", "[Ljava/util/TimerTask;")?;
    let count = field_slot(cs, "java/util/Timer", "count", "I")?;
    let cancelled = field_slot(cs, "java/util/Timer", "cancelled", "Z")?;
    Ok((tasks, count, cancelled))
}

/// `TimerTask()`：字段显式归零（模板已零值，防御式重复置位）。
pub fn timer_task_init(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let slots = timer_task_slots(cs)?;
    set_field(heap, this, slots.0, Slot::Long(0))?;
    set_field(heap, this, slots.1, Slot::Long(0))?;
    set_field(heap, this, slots.2, Slot::Int(0))?;
    set_field(heap, this, slots.3, Slot::Ref(ObjRef::NULL))?;
    Ok(None)
}

/// `TimerTask.cancel()`：置取消标志；返回本次取消是否生效
/// （近似 JDK：此前未取消即视为生效）。
pub fn timer_task_cancel(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let slots = timer_task_slots(cs)?;
    let was_cancelled = field_int(heap, this, slots.2)? != 0;
    set_field(heap, this, slots.2, Slot::Int(1))?;
    ret_bool(!was_cancelled)
}

/// `scheduledExecutionTime()`：下次触发时刻（T034 调度器读取）。
pub fn timer_task_scheduled_execution_time(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let slots = timer_task_slots(cs)?;
    ret_long(field_long(heap, this, slots.0)?)
}

/// `Timer()`：空任务表（初始容量 4）。
pub fn timer_init(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let slots = timer_slots(cs)?;
    let task_class = cs.load_class("[Ljava/util/TimerTask;")?;
    let tasks = heap.alloc_array(task_class, ElemType::Ref, 4)?;
    set_field(heap, this, slots.0, Slot::Ref(tasks))?;
    set_field(heap, this, slots.1, Slot::Int(0))?;
    set_field(heap, this, slots.2, Slot::Int(0))?;
    Ok(None)
}

/// 登记公共路径：写 task 数据字段（period>0=固定延迟、period<0=固定速率，
/// 对齐 JDK 内部表示）并追加进 Timer 任务表（满则翻倍）；同时入
/// `pending_timers` 请求位（T034 触发面），`Vm.poll` 段零消费转
/// `scheduler.schedule_timer`，到期回调 task.run()。
fn timer_register(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    timer: ObjRef,
    task: ObjRef,
    next_time: i64,
    period: i64,
) -> Result<(), KvmError> {
    let task_slots = timer_task_slots(cs)?;
    set_field(heap, task, task_slots.0, Slot::Long(next_time))?;
    set_field(heap, task, task_slots.1, Slot::Long(period))?;
    set_field(heap, task, task_slots.2, Slot::Int(0))?;
    set_field(heap, task, task_slots.3, Slot::Ref(timer))?;
    let slots = timer_slots(cs)?;
    let mut count = field_int(heap, timer, slots.1)?;
    let tasks = field_ref(heap, timer, slots.0)?;
    if count as usize >= ref_array_len(heap, tasks)? {
        // 翻倍扩容并搬迁。
        let fresh = heap.alloc_array(
            cs.load_class("[Ljava/util/TimerTask;")?,
            ElemType::Ref,
            ref_array_len(heap, tasks)? * 2,
        )?;
        for index in 0..count as usize {
            ref_array_set(heap, fresh, index, ref_array_get(heap, tasks, index)?)?;
        }
        set_field(heap, timer, slots.0, Slot::Ref(fresh))?;
    }
    let tasks = field_ref(heap, timer, slots.0)?;
    ref_array_set(heap, tasks, count as usize, task)?;
    count += 1;
    set_field(heap, timer, slots.1, Slot::Int(count))?;
    // 触发面登记（T034）：past 时刻（date 版已过）立即到期的语义
    // 由 fire_due(<=now) 自然覆盖。
    cs.pending_timers.push((next_time.max(0) as u64, task));
    Ok(())
}

/// Timer 请求位消费（T034）：drain `cs.pending_timers` → 调度器定时
/// 队列登记。`Vm.poll` 段零与测试驱动循环调用（须在 scheduler.poll
/// 前，到期触发在 poll 唤醒段内做）。
pub fn pump_pending_timers(cs: &mut ClassSpace, scheduler: &mut Scheduler) {
    for (at_ms, task) in std::mem::take(&mut cs.pending_timers) {
        // 调试面：SKYENGINE_KVM_TRACE_TIMERS=1 时打印登记（实证 Timer 面）。
        if std::env::var_os("SKYENGINE_KVM_TRACE_TIMERS").is_some() {
            eprintln!("[kvm] timer scheduled: at={at_ms} task={task:?}");
        }
        scheduler.schedule_timer(at_ms, TimerCb::RunTask(task));
    }
}

/// TimerTask 触发前置检查（T034，调度器 run_due_timers 调用）：
/// task.cancelled 或所属 Timer.cancelled 任一置位即应跳过（惰性删除
/// ——`TimerTask.cancel()`/`Timer.cancel()` 只置标志，不回摘定时队列）。
pub(crate) fn timer_task_is_cancelled(
    cs: &mut ClassSpace,
    heap: &Heap,
    task: ObjRef,
) -> Result<bool, KvmError> {
    let (_, _, cancelled, timer) = timer_task_slots(cs)?;
    if field_int(heap, task, cancelled)? != 0 {
        return Ok(true);
    }
    let owner = field_ref(heap, task, timer)?;
    if !owner.is_null() {
        let (_, _, timer_cancelled) = timer_slots(cs)?;
        if field_int(heap, owner, timer_cancelled)? != 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

/// fixed-rate 重排（T034，调度器 run_due_timers 调用）：period != 0 →
/// nextTime += |period| 写回并返回新时刻（重新入队）；一次性任务
/// （period=0）返回 None。固定延迟与固定速率的差异对游戏无感，
/// 统一取 fixed-rate（见 scheduler.rs 注释）。
pub(crate) fn timer_task_requeue(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    task: ObjRef,
) -> Result<Option<u64>, KvmError> {
    let (next, period, _, _) = timer_task_slots(cs)?;
    let period = field_long(heap, task, period)?;
    if period == 0 {
        return Ok(None);
    }
    let mut next_time = field_long(heap, task, next)?;
    next_time += period.abs();
    set_field(heap, task, next, Slot::Long(next_time))?;
    Ok(Some(next_time.max(0) as u64))
}

/// `schedule(task, delay)`：一次性；delay<0 → IllegalArgumentException。
/// 宿主轨（T034）：nextTime 取宿主时钟（mock 时钟可驱动测试）。
pub fn timer_schedule_delay(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let timer = this(call)?;
    let task = arg_ref(call, 0)?;
    let delay = arg_i64(call, 1)?;
    if task.is_null() {
        return Err(throw_java(cs, heap, "java/lang/NullPointerException", None));
    }
    if delay < 0 {
        return Err(throw_java(
            cs,
            heap,
            "java/lang/IllegalArgumentException",
            Some("Negative delay"),
        ));
    }
    let now = host.now_ms() as i64;
    timer_register(cs, heap, timer, task, now + delay, 0).map(|_| None)
}

/// `schedule(task, delay, period)`：固定延迟；period<=0 → IAE。
pub fn timer_schedule_delay_period(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let timer = this(call)?;
    let task = arg_ref(call, 0)?;
    let delay = arg_i64(call, 1)?;
    let period = arg_i64(call, 2)?;
    if task.is_null() {
        return Err(throw_java(cs, heap, "java/lang/NullPointerException", None));
    }
    if delay < 0 || period <= 0 {
        return Err(throw_java(
            cs,
            heap,
            "java/lang/IllegalArgumentException",
            Some("Non-positive period"),
        ));
    }
    let now = host.now_ms() as i64;
    timer_register(cs, heap, timer, task, now + delay, period).map(|_| None)
}

/// `schedule(task, date)`：一次性，时刻取 date.getTime()。
pub fn timer_schedule_date(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let timer = this(call)?;
    let task = arg_ref(call, 0)?;
    let date = arg_ref(call, 1)?;
    if task.is_null() || date.is_null() {
        return Err(throw_java(cs, heap, "java/lang/NullPointerException", None));
    }
    let time = date_ms_of(cs, heap, date)?;
    timer_register(cs, heap, timer, task, time, 0).map(|_| None)
}

/// `schedule(task, date, period)`：固定延迟；period<=0 → IAE。
pub fn timer_schedule_date_period(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let timer = this(call)?;
    let task = arg_ref(call, 0)?;
    let date = arg_ref(call, 1)?;
    let period = arg_i64(call, 2)?;
    if task.is_null() || date.is_null() {
        return Err(throw_java(cs, heap, "java/lang/NullPointerException", None));
    }
    if period <= 0 {
        return Err(throw_java(
            cs,
            heap,
            "java/lang/IllegalArgumentException",
            Some("Non-positive period"),
        ));
    }
    let time = date_ms_of(cs, heap, date)?;
    timer_register(cs, heap, timer, task, time, period).map(|_| None)
}

/// `scheduleAtFixedRate(task, delay, period)`：固定速率（period 存负值）。
pub fn timer_fixed_rate_delay(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let timer = this(call)?;
    let task = arg_ref(call, 0)?;
    let delay = arg_i64(call, 1)?;
    let period = arg_i64(call, 2)?;
    if task.is_null() {
        return Err(throw_java(cs, heap, "java/lang/NullPointerException", None));
    }
    if delay < 0 || period <= 0 {
        return Err(throw_java(
            cs,
            heap,
            "java/lang/IllegalArgumentException",
            Some("Non-positive period"),
        ));
    }
    let now = host.now_ms() as i64;
    timer_register(cs, heap, timer, task, now + delay, -period).map(|_| None)
}

/// `scheduleAtFixedRate(task, date, period)`：固定速率（period 存负值）。
pub fn timer_fixed_rate_date(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let timer = this(call)?;
    let task = arg_ref(call, 0)?;
    let date = arg_ref(call, 1)?;
    let period = arg_i64(call, 2)?;
    if task.is_null() || date.is_null() {
        return Err(throw_java(cs, heap, "java/lang/NullPointerException", None));
    }
    if period <= 0 {
        return Err(throw_java(
            cs,
            heap,
            "java/lang/IllegalArgumentException",
            Some("Non-positive period"),
        ));
    }
    let time = date_ms_of(cs, heap, date)?;
    timer_register(cs, heap, timer, task, time, -period).map(|_| None)
}

/// `Timer.cancel()`：置取消标志（数据面；任务字段不动，
/// T034 调度接入时按标志剔除唤醒项）。
pub fn timer_cancel(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this(call)?;
    let slots = timer_slots(cs)?;
    set_field(heap, this, slots.2, Slot::Int(1))?;
    Ok(None)
}

/// `purge()`：数据面恒返 0（无已取消任务的登记回收；T034 落实）。
pub fn timer_purge(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ret_i32(0)
}
