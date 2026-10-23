//! java.io 原生实现（T016）：替换 T010 桩为真实现。
//!
//! 函数签名与 `interp::natives::NativeFn` 一致，由 bootstrap 注册表
//! （natives.rs 的 java/io 段）按 类名.方法名+描述符 绑定。
//!
//! ## 实例存储选型（对齐 T014/T015：状态存实例区字段，无 peer 结构）
//!
//! - **ByteArrayInputStream**：`buf [B`（不拷贝）+ `pos/count/markPos I`。
//! - **ByteArrayOutputStream**：`buf [B`（容量翻倍，缺省 32）+ `count I`。
//! - **FilterInputStream/FilterOutputStream**：底层引用存 `in`/`out`
//!   实例字段；DataInputStream 无自有数据，DataOutputStream 另有
//!   `written I` 计数。
//! - **PrintStream**：`out Ljava/io/OutputStream;`（包装模式）+ `fd I`
//!   （fd≠0 = 宿主标准错误直写，TODO T045 桥接宿主错误画面）。
//! - **InputStreamReader**：`in` + `encoding I`（0=UTF-8，1=Latin-1）+
//!   `pending I`（代理对低半/回退单元暂存，跨 read 调用保序）。
//! - **OutputStreamWriter**：`out` + `encoding I`（同上编码表）。
//!
//! ## 原生互操作（不经解释器重入）
//!
//! write/read 按接收者**运行时类名**在原生实现间递归直达：
//! DataOutputStream → FilterOutputStream.out → ByteArrayOutputStream 等。
//! 用户字节码子类覆盖的 write/read 无法参与（需解释器重入），按
//! KvmError::Internal 上报（TODO 注释）。
//!
//! ## 已知边界（保守实现，注释逐处标注）
//!
//! - **readUTF**：长度前缀或数据中途 EOF → EOFException；非法序列 /
//!   孤代理 → UTFDataFormatException（对齐 JDK 语义）。
//! - **readLine**：读到 \n 或 \r 即止，**不预读吞掉 \r 后的 \n**（JDK
//!   会预读吞掉；MIDlet 语料无 \r\n 行尾，偏差无害）。
//! - **InputStreamReader（UTF-8）**：尾部截断按流耗尽（-1）处理；非法
//!   序列替换 U+FFFD；代理对可跨 read 调用返回（高代理占末槽、低代理
//!   留 pending 待下次读出，UTF-16 码元语义正确，无需字节回退）。
//! - **OutputStreamWriter**：孤代理（不成对）写 '?'（对齐 JDK
//!   CharsetEncoder 缺省 REPLACE 动作）。
//! - **PrintStream 包装模式**：文本按 ISO-8859-1 逐码元（>0xFF 截断
//!   低字节）；fd 模式整段 Unicode 直写宿主标准错误。
//! - native 内 `Heap::alloc*` 不经 `alloc_with_gc`（T015 约定，仅解释
//!   器分配路径触发 GC），Rust 局部引用在原生执行期安全。

use crate::cldc::lang as jl;
use crate::cldc::util as ju;
use crate::interp::natives::NativeFn;
use crate::interp::NativeCall;
use crate::runtime::classspace::ClassSpace;
use crate::runtime::heap::Heap;
use crate::runtime::object::{ArrayData, ElemType, Payload};
use crate::runtime::{ObjRef, Slot, ThreadId};
use crate::KvmError;

// ---------------------------------------------------------------------------
// 通用小工具（与 util.rs 私有版重复；体量小，跨模块抽象不划算）
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

fn ret_string(heap: &mut Heap, chars: Vec<u16>) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Ref(heap.alloc_string_uninterned(chars)?)))
}

fn utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

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

/// 字节数组数据（`[B` 载荷）。
fn byte_data(heap: &Heap, array: ObjRef) -> Result<&Vec<i8>, KvmError> {
    match &heap.array(array)?.data {
        ArrayData::Byte(data) => Ok(data),
        other => Err(KvmError::Internal(format!("expected byte[], got {other:?}"))),
    }
}

fn byte_data_mut(heap: &mut Heap, array: ObjRef) -> Result<&mut Vec<i8>, KvmError> {
    match &mut heap.array_mut(array)?.data {
        ArrayData::Byte(data) => Ok(data),
        other => Err(KvmError::Internal(format!("expected byte[], got {other:?}"))),
    }
}

/// 字符数组数据（`[C` 载荷）。
fn char_data(heap: &Heap, array: ObjRef) -> Result<&Vec<u16>, KvmError> {
    match &heap.array(array)?.data {
        ArrayData::Char(data) => Ok(data),
        other => Err(KvmError::Internal(format!("expected char[], got {other:?}"))),
    }
}

fn char_data_mut(heap: &mut Heap, array: ObjRef) -> Result<&mut Vec<u16>, KvmError> {
    match &mut heap.array_mut(array)?.data {
        ArrayData::Char(data) => Ok(data),
        other => Err(KvmError::Internal(format!("expected char[], got {other:?}"))),
    }
}

/// 分配字节数组并填充内容。
pub(crate) fn alloc_byte_array_from(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    data: &[u8],
) -> Result<ObjRef, KvmError> {
    let class = cs.load_class("[B")?;
    let reference = heap.alloc_array(class, ElemType::Byte, data.len())?;
    if let ArrayData::Byte(target) = &mut heap.array_mut(reference)?.data {
        for (i, &byte) in data.iter().enumerate() {
            target[i] = byte as i8;
        }
    }
    Ok(reference)
}

/// String 载荷码元（调用方已完成 null 检查）。
fn string_chars(heap: &Heap, reference: ObjRef) -> Result<Vec<u16>, KvmError> {
    match heap.payload(reference)? {
        Payload::Str(s) => Ok(s.chars.clone()),
        _ => Err(KvmError::Internal("expected String payload".into())),
    }
}

/// 区间边界检查（off/len ≥ 0 且 off+len ≤ 数组长度），越界抛
/// IndexOutOfBoundsException（JDK java.io 数组区间检查语义）。
fn range_or_throw(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    len: usize,
    off: i32,
    count: i32,
) -> Result<(usize, usize), KvmError> {
    if off < 0 || count < 0 || i64::from(off) + i64::from(count) > len as i64 {
        return Err(jl::throw_java(
            cs,
            heap,
            "java/lang/IndexOutOfBoundsException",
            None,
        ));
    }
    Ok((off as usize, count as usize))
}

/// 抛 NullPointerException。
fn throw_npe(cs: &mut ClassSpace, heap: &mut Heap) -> KvmError {
    jl::throw_java(cs, heap, "java/lang/NullPointerException", None)
}

/// 抛 EOFException。
fn throw_eof(cs: &mut ClassSpace, heap: &mut Heap) -> KvmError {
    jl::throw_java(cs, heap, "java/io/EOFException", None)
}

// ---------------------------------------------------------------------------
// Modified UTF-8 编解码（DataOutputStream.writeUTF / readUTF 专用）
// ---------------------------------------------------------------------------

/// 解码故障：尾部截断（对应 EOFException）与非法序列（对应
/// UTFDataFormatException）。
enum Utf8Fault {
    Truncated,
    Malformed,
}

/// 解码 Modified UTF-8（DataInputStream.readUTF 语义）：
/// - 0x00 单字节按 JDK 宽容为 U+0000（规范形态应为 C0 80）；
/// - 高代理必须紧跟一个 3 字节编码的低代理（6 字节代理对）；
/// - 孤高代理 / 孤低代理 / 错误的续字节形态均按 Malformed。
fn decode_modified_utf8(bytes: &[u8]) -> Result<Vec<u16>, Utf8Fault> {
    let mut chars = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b1 = bytes[i];
        i += 1;
        let unit = match b1 >> 4 {
            0..=7 => u16::from(b1),
            12 | 13 => {
                if i >= bytes.len() {
                    return Err(Utf8Fault::Truncated);
                }
                let b2 = bytes[i];
                i += 1;
                if b2 & 0xC0 != 0x80 {
                    return Err(Utf8Fault::Malformed);
                }
                (u16::from(b1 & 0x1F) << 6) | u16::from(b2 & 0x3F)
            }
            14 => {
                if i + 1 >= bytes.len() {
                    return Err(Utf8Fault::Truncated);
                }
                let (b2, b3) = (bytes[i], bytes[i + 1]);
                i += 2;
                if b2 & 0xC0 != 0x80 || b3 & 0xC0 != 0x80 {
                    return Err(Utf8Fault::Malformed);
                }
                (u16::from(b1 & 0x0F) << 12) | (u16::from(b2 & 0x3F) << 6)
                    | u16::from(b3 & 0x3F)
            }
            // 0xF0..=0xFF：Modified UTF-8 不存在 4 字节形态。
            _ => return Err(Utf8Fault::Malformed),
        };
        if (0xD800..=0xDBFF).contains(&unit) {
            // 高代理：必须紧跟 3 字节编码的低代理。
            if i + 3 > bytes.len() {
                return Err(Utf8Fault::Truncated);
            }
            let (c1, c2, c3) = (bytes[i], bytes[i + 1], bytes[i + 2]);
            if c1 >> 4 != 14 || c2 & 0xC0 != 0x80 || c3 & 0xC0 != 0x80 {
                return Err(Utf8Fault::Malformed);
            }
            let low = (u16::from(c1 & 0x0F) << 12) | (u16::from(c2 & 0x3F) << 6)
                | u16::from(c3 & 0x3F);
            if !(0xDC00..=0xDFFF).contains(&low) {
                return Err(Utf8Fault::Malformed);
            }
            chars.push(unit);
            chars.push(low);
            i += 3;
            continue;
        }
        if (0xDC00..=0xDFFF).contains(&unit) {
            // 孤低代理。
            return Err(Utf8Fault::Malformed);
        }
        chars.push(unit);
    }
    Ok(chars)
}

/// 编码 Modified UTF-8：U+0000 → C0 80；代理（成对或孤生）逐码元各
/// 3 字节（代理对合计 6 字节，与 JDK writeUTF 一致，不做校验）。
fn encode_modified_utf8(chars: &[u16]) -> Vec<u8> {
    let mut out = Vec::new();
    for &unit in chars {
        match unit {
            0x0000 => out.extend_from_slice(&[0xC0, 0x80]),
            0x0001..=0x007F => out.push(unit as u8),
            0x0080..=0x07FF => {
                out.push(0xC0 | (unit >> 6) as u8);
                out.push(0x80 | (unit & 0x3F) as u8);
            }
            _ => {
                out.push(0xE0 | (unit >> 12) as u8);
                out.push(0x80 | ((unit >> 6) & 0x3F) as u8);
                out.push(0x80 | (unit & 0x3F) as u8);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 输出通道：按运行时类递归写字节
// ---------------------------------------------------------------------------

/// 向输出流写字节：按接收者运行时类名在原生实现间递归直达。
///
/// 用户字节码子类的覆盖 write 无法参与（需解释器重入，TODO），
/// 按 KvmError::Internal 上报。
fn output_write(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    out: ObjRef,
    bytes: &[u8],
) -> Result<(), KvmError> {
    if out.is_null() {
        return Err(throw_npe(cs, heap));
    }
    let class_name = cs.name_of(heap.instance(out)?.class).to_string();
    match class_name.as_str() {
        "java/io/ByteArrayOutputStream" => baos_append(cs, heap, out, bytes),
        "java/io/DataOutputStream" => {
            let underlying = filter_out(cs, heap, out)?;
            dos_add_written(cs, heap, out, bytes.len() as i32)?;
            output_write(cs, heap, underlying, bytes)
        }
        "java/io/FilterOutputStream" => {
            let underlying = filter_out(cs, heap, out)?;
            output_write(cs, heap, underlying, bytes)
        }
        "java/io/PrintStream" => {
            // PrintStream 被上层流包装的罕见形态：fd 直写或转包装流。
            let fd_slot = field_slot(cs, "java/io/PrintStream", "fd", "I")?;
            if field_int(heap, out, fd_slot)? != 0 {
                let text: String = bytes.iter().map(|b| char::from(*b)).collect();
                eprint!("{text}");
                Ok(())
            } else {
                let slot = field_slot(cs, "java/io/PrintStream", "out", "Ljava/io/OutputStream;")?;
                let underlying = field_ref(heap, out, slot)?;
                output_write(cs, heap, underlying, bytes)
            }
        }
        other => Err(KvmError::Internal(format!(
            "java.io: write on user class {other} unsupported (interpreter reentry TODO)"
        ))),
    }
}

/// FilterOutputStream.out 字段引用。
fn filter_out(cs: &mut ClassSpace, heap: &Heap, this: ObjRef) -> Result<ObjRef, KvmError> {
    let slot = field_slot(cs, "java/io/FilterOutputStream", "out", "Ljava/io/OutputStream;")?;
    field_ref(heap, this, slot)
}

/// DataOutputStream.written 计数累加（饱和加法防御溢出）。
fn dos_add_written(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    this: ObjRef,
    delta: i32,
) -> Result<(), KvmError> {
    let slot = field_slot(cs, "java/io/DataOutputStream", "written", "I")?;
    let written = field_int(heap, this, slot)?;
    set_field(heap, this, slot, Slot::Int(written.saturating_add(delta)))
}

/// BAOS 追加字节：容量不足时翻倍扩容（JDK Arrays.copyOf 语义，
/// 0 容量 → 32）；buf 为 null 时懒分配（防御用户子类绕过构造器）。
fn baos_append(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    out: ObjRef,
    bytes: &[u8],
) -> Result<(), KvmError> {
    let buf_slot = field_slot(cs, "java/io/ByteArrayOutputStream", "buf", "[B")?;
    let count_slot = field_slot(cs, "java/io/ByteArrayOutputStream", "count", "I")?;
    if field_ref(heap, out, buf_slot)?.is_null() {
        let buf = alloc_byte_array_from(cs, heap, &[])?;
        set_field(heap, out, buf_slot, Slot::Ref(buf))?;
    }
    for &byte in bytes {
        let mut buf_ref = field_ref(heap, out, buf_slot)?;
        let count = field_int(heap, out, count_slot)? as usize;
        let capacity = heap.array(buf_ref)?.data.len();
        if count == capacity {
            let new_capacity = if capacity == 0 { 32 } else { capacity * 2 };
            let mut data: Vec<u8> = byte_data(heap, buf_ref)?.iter().map(|b| *b as u8).collect();
            data.resize(new_capacity, 0);
            let new_ref = alloc_byte_array_from(cs, heap, &data)?;
            set_field(heap, out, buf_slot, Slot::Ref(new_ref))?;
            buf_ref = new_ref;
        }
        byte_data_mut(heap, buf_ref)?[count] = byte as i8;
        set_field(heap, out, count_slot, Slot::Int(count as i32 + 1))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 输入通道：按运行时类递归读字节
// ---------------------------------------------------------------------------

/// 从输入流读字节到缓冲：`Ok(Some(n))` = 读到 n 字节（0 仅当请求 0），
/// `Ok(None)` = EOF。按接收者运行时类名递归直达。
fn input_read(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    input: ObjRef,
    out: &mut [u8],
) -> Result<Option<usize>, KvmError> {
    if input.is_null() {
        return Err(throw_npe(cs, heap));
    }
    let class_name = cs.name_of(heap.instance(input)?.class).to_string();
    match class_name.as_str() {
        "java/io/ByteArrayInputStream" => bais_read(cs, heap, input, out),
        "java/io/DataInputStream" | "java/io/FilterInputStream" => {
            let underlying = filter_in(cs, heap, input)?;
            input_read(cs, heap, underlying, out)
        }
        // 未知子类：InputStream.read 抽象方法无法直达，按内部错误上报
        //（解释器重入 TODO）。
        other => Err(KvmError::Internal(format!(
            "java.io: read on user class {other} unsupported (interpreter reentry TODO)"
        ))),
    }
}

/// FilterInputStream.in 字段引用。
fn filter_in(cs: &mut ClassSpace, heap: &Heap, this: ObjRef) -> Result<ObjRef, KvmError> {
    let slot = field_slot(cs, "java/io/FilterInputStream", "in", "Ljava/io/InputStream;")?;
    field_ref(heap, this, slot)
}

/// BAIS 读：pos..count 区间拷贝。
fn bais_read(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    input: ObjRef,
    out: &mut [u8],
) -> Result<Option<usize>, KvmError> {
    if out.is_empty() {
        return Ok(Some(0));
    }
    let pos_slot = field_slot(cs, "java/io/ByteArrayInputStream", "pos", "I")?;
    let count_slot = field_slot(cs, "java/io/ByteArrayInputStream", "count", "I")?;
    let pos = field_int(heap, input, pos_slot)? as usize;
    let count = field_int(heap, input, count_slot)? as usize;
    if pos >= count {
        return Ok(None);
    }
    let buf_slot = field_slot(cs, "java/io/ByteArrayInputStream", "buf", "[B")?;
    let buf_ref = field_ref(heap, input, buf_slot)?;
    let n = out.len().min(count - pos);
    let data = byte_data(heap, buf_ref)?;
    for i in 0..n {
        out[i] = data[pos + i] as u8;
    }
    set_field(heap, input, pos_slot, Slot::Int((pos + n) as i32))?;
    Ok(Some(n))
}

/// 读单个字节：`Ok(None)` = EOF。
fn input_read_byte(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    input: ObjRef,
) -> Result<Option<u8>, KvmError> {
    let mut buf = [0u8; 1];
    match input_read(cs, heap, input, &mut buf)? {
        None | Some(0) => Ok(None),
        Some(_) => Ok(Some(buf[0])),
    }
}

/// 读尽输入流（`Image.createImage(InputStream)` 用，T018）：
/// 按输入通道循环读到 EOF，返回全部字节。
pub(crate) fn input_read_to_vec(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    input: ObjRef,
) -> Result<Vec<u8>, KvmError> {
    let mut out = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        match input_read(cs, heap, input, &mut buf)? {
            // 通道契约：请求非 0 不会返回 0；防御无进展直接终止
            Some(0) | None => return Ok(out),
            Some(n) => out.extend_from_slice(&buf[..n]),
        }
    }
}

/// 读满整个缓冲；中途 EOF → EOFException（readFully / 原语读语义）。
fn input_read_exact(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    input: ObjRef,
    out: &mut [u8],
) -> Result<(), KvmError> {
    let mut filled = 0;
    while filled < out.len() {
        match input_read(cs, heap, input, &mut out[filled..])? {
            None => return Err(throw_eof(cs, heap)),
            // 防御：非空请求的零长读若放行会死循环，按内部错误上报。
            Some(0) => {
                return Err(KvmError::Internal(
                    "zero-length read against non-empty request".into(),
                ))
            }
            Some(n) => filled += n,
        }
    }
    Ok(())
}

/// 跳过 n 字节：返回实际跳过数（BAIS 游标推进；DIS/FIS 递归底层）。
fn input_skip(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    input: ObjRef,
    n: i64,
) -> Result<i64, KvmError> {
    if n <= 0 {
        return Ok(0);
    }
    if input.is_null() {
        return Err(throw_npe(cs, heap));
    }
    let class_name = cs.name_of(heap.instance(input)?.class).to_string();
    match class_name.as_str() {
        "java/io/ByteArrayInputStream" => {
            let pos_slot = field_slot(cs, "java/io/ByteArrayInputStream", "pos", "I")?;
            let count_slot = field_slot(cs, "java/io/ByteArrayInputStream", "count", "I")?;
            let pos = field_int(heap, input, pos_slot)? as i64;
            let count = field_int(heap, input, count_slot)? as i64;
            let skipped = n.min(count - pos).max(0);
            set_field(heap, input, pos_slot, Slot::Int((pos + skipped) as i32))?;
            Ok(skipped)
        }
        "java/io/DataInputStream" | "java/io/FilterInputStream" => {
            let underlying = filter_in(cs, heap, input)?;
            input_skip(cs, heap, underlying, n)
        }
        other => Err(KvmError::Internal(format!(
            "java.io: skip on user class {other} unsupported (interpreter reentry TODO)"
        ))),
    }
}

/// 可读字节数（BAIS 剩余量；DIS/FIS 递归底层；未知按 InputStream
/// 缺省语义返回 0）。
fn input_available(cs: &mut ClassSpace, heap: &mut Heap, input: ObjRef) -> Result<i32, KvmError> {
    if input.is_null() {
        return Err(throw_npe(cs, heap));
    }
    let class_name = cs.name_of(heap.instance(input)?.class).to_string();
    match class_name.as_str() {
        "java/io/ByteArrayInputStream" => {
            let pos_slot = field_slot(cs, "java/io/ByteArrayInputStream", "pos", "I")?;
            let count_slot = field_slot(cs, "java/io/ByteArrayInputStream", "count", "I")?;
            let pos = field_int(heap, input, pos_slot)?;
            let count = field_int(heap, input, count_slot)?;
            Ok((count - pos).max(0))
        }
        "java/io/DataInputStream" | "java/io/FilterInputStream" => {
            let underlying = filter_in(cs, heap, input)?;
            input_available(cs, heap, underlying)
        }
        // InputStream.available 缺省返回 0（无阻塞语义，保守）。
        _ => Ok(0),
    }
}

// ---------------------------------------------------------------------------
// ByteArrayInputStream
// ---------------------------------------------------------------------------

/// 分配并初始化 ByteArrayInputStream（buf=数据副本、pos=0、count=len、
/// markPos=0）。`Class.getResourceAsStream` 等构造流返回值的宿主侧入口
/// （字段语义与 `bais_init` 一致）。
pub(crate) fn alloc_byte_array_stream(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    data: &[u8],
) -> Result<ObjRef, KvmError> {
    let stream = alloc_instance_of(cs, heap, "java/io/ByteArrayInputStream")?;
    let buf = alloc_byte_array_from(cs, heap, data)?;
    set_field(
        heap,
        stream,
        field_slot(cs, "java/io/ByteArrayInputStream", "buf", "[B")?,
        Slot::Ref(buf),
    )?;
    set_field(
        heap,
        stream,
        field_slot(cs, "java/io/ByteArrayInputStream", "pos", "I")?,
        Slot::Int(0),
    )?;
    set_field(
        heap,
        stream,
        field_slot(cs, "java/io/ByteArrayInputStream", "count", "I")?,
        Slot::Int(data.len() as i32),
    )?;
    set_field(
        heap,
        stream,
        field_slot(cs, "java/io/ByteArrayInputStream", "markPos", "I")?,
        Slot::Int(0),
    )?;
    Ok(stream)
}

pub fn bais_init(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let buf = arg_ref(call, 0)?;
    if buf.is_null() {
        return Err(throw_npe(cs, heap));
    }
    let len = heap.array(buf)?.data.len();
    let buf_slot = field_slot(cs, "java/io/ByteArrayInputStream", "buf", "[B")?;
    let pos_slot = field_slot(cs, "java/io/ByteArrayInputStream", "pos", "I")?;
    let count_slot = field_slot(cs, "java/io/ByteArrayInputStream", "count", "I")?;
    let mark_slot = field_slot(cs, "java/io/ByteArrayInputStream", "markPos", "I")?;
    set_field(heap, receiver, buf_slot, Slot::Ref(buf))?;
    set_field(heap, receiver, pos_slot, Slot::Int(0))?;
    set_field(heap, receiver, count_slot, Slot::Int(len as i32))?;
    set_field(heap, receiver, mark_slot, Slot::Int(0))?;
    Ok(None)
}

pub fn bais_read_single(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let pos_slot = field_slot(cs, "java/io/ByteArrayInputStream", "pos", "I")?;
    let count_slot = field_slot(cs, "java/io/ByteArrayInputStream", "count", "I")?;
    let pos = field_int(heap, receiver, pos_slot)? as usize;
    let count = field_int(heap, receiver, count_slot)? as usize;
    if pos >= count {
        return ret_i32(-1);
    }
    let buf_slot = field_slot(cs, "java/io/ByteArrayInputStream", "buf", "[B")?;
    let buf_ref = field_ref(heap, receiver, buf_slot)?;
    let byte = byte_data(heap, buf_ref)?[pos];
    set_field(heap, receiver, pos_slot, Slot::Int(pos as i32 + 1))?;
    // JDK read() 返回 0..=255（零扩展）。
    ret_i32(byte as u8 as i32)
}

pub fn bais_read_range(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let buf = arg_ref(call, 0)?;
    let off = arg_i32(call, 1)?;
    let len = arg_i32(call, 2)?;
    let (off, len) = range_or_throw(cs, heap, heap.array(buf)?.data.len(), off, len)?;
    if len == 0 {
        return ret_i32(0);
    }
    let mut tmp = vec![0u8; len];
    match bais_read(cs, heap, receiver, &mut tmp)? {
        None => ret_i32(-1),
        Some(n) => {
            let data = byte_data_mut(heap, buf)?;
            for i in 0..n {
                data[off + i] = tmp[i] as i8;
            }
            ret_i32(n as i32)
        }
    }
}

pub fn bais_mark(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let pos_slot = field_slot(cs, "java/io/ByteArrayInputStream", "pos", "I")?;
    let mark_slot = field_slot(cs, "java/io/ByteArrayInputStream", "markPos", "I")?;
    let pos = field_int(heap, receiver, pos_slot)?;
    // readLimit 忽略（BAIS 环形缓冲淘汰语义不适用，游标永不失效）。
    set_field(heap, receiver, mark_slot, Slot::Int(pos))?;
    Ok(None)
}

pub fn bais_reset(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let pos_slot = field_slot(cs, "java/io/ByteArrayInputStream", "pos", "I")?;
    let mark_slot = field_slot(cs, "java/io/ByteArrayInputStream", "markPos", "I")?;
    let mark = field_int(heap, receiver, mark_slot)?;
    set_field(heap, receiver, pos_slot, Slot::Int(mark))?;
    Ok(None)
}

/// read([B)：off=0、len=buf.length 全量读（InputStream.read([B) 语义）。
pub fn bais_read_array(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let buf = arg_ref(call, 0)?;
    let cap = heap.array(buf)?.data.len();
    let (off, len) = range_or_throw(cs, heap, cap, 0, cap as i32)?;
    if len == 0 {
        return ret_i32(0);
    }
    let mut tmp = vec![0u8; len];
    match bais_read(cs, heap, receiver, &mut tmp)? {
        None => ret_i32(-1),
        Some(n) => {
            let data = byte_data_mut(heap, buf)?;
            for i in 0..n {
                data[off + i] = tmp[i] as i8;
            }
            ret_i32(n as i32)
        }
    }
}

/// skip(J)：BAIS 游标跳跃（按剩余量截断）。
pub fn bais_skip(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let n = arg_i64(call, 0)?;
    ret_long(input_skip(cs, heap, receiver, n)?)
}

/// available()：剩余可读字节数（count - pos）。
pub fn bais_available(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    ret_i32(input_available(cs, heap, receiver)?)
}

/// BAOS.write([B)：全量追加（off=0、len=buf.length）。
pub fn baos_write_all(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let buf = arg_ref(call, 0)?;
    let src: Vec<u8> = byte_data(heap, buf)?.iter().map(|b| *b as u8).collect();
    baos_append(cs, heap, receiver, &src)?;
    Ok(None)
}

// ---------------------------------------------------------------------------
// ByteArrayOutputStream
// ---------------------------------------------------------------------------

pub fn baos_init_empty(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let buf = alloc_byte_array_from(cs, heap, &[0u8; 32])?;
    let buf_slot = field_slot(cs, "java/io/ByteArrayOutputStream", "buf", "[B")?;
    let count_slot = field_slot(cs, "java/io/ByteArrayOutputStream", "count", "I")?;
    set_field(heap, receiver, buf_slot, Slot::Ref(buf))?;
    set_field(heap, receiver, count_slot, Slot::Int(0))?;
    Ok(None)
}

pub fn baos_init_size(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let size = arg_i32(call, 0)?;
    if size < 0 {
        return Err(jl::throw_java(
            cs,
            heap,
            "java/lang/IllegalArgumentException",
            Some(&format!("Negative initial size: {size}")),
        ));
    }
    let buf = alloc_byte_array_from(cs, heap, &vec![0u8; size as usize])?;
    let buf_slot = field_slot(cs, "java/io/ByteArrayOutputStream", "buf", "[B")?;
    let count_slot = field_slot(cs, "java/io/ByteArrayOutputStream", "count", "I")?;
    set_field(heap, receiver, buf_slot, Slot::Ref(buf))?;
    set_field(heap, receiver, count_slot, Slot::Int(0))?;
    Ok(None)
}

pub fn baos_write_int(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let value = arg_i32(call, 0)?;
    baos_append(cs, heap, receiver, &[value as u8])?;
    Ok(None)
}

pub fn baos_write_range(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let buf = arg_ref(call, 0)?;
    let off = arg_i32(call, 1)?;
    let len = arg_i32(call, 2)?;
    let (off, len) = range_or_throw(cs, heap, heap.array(buf)?.data.len(), off, len)?;
    let src: Vec<u8> = byte_data(heap, buf)?[off..off + len]
        .iter()
        .map(|b| *b as u8)
        .collect();
    baos_append(cs, heap, receiver, &src)?;
    Ok(None)
}

pub fn baos_to_byte_array(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let buf_slot = field_slot(cs, "java/io/ByteArrayOutputStream", "buf", "[B")?;
    let count_slot = field_slot(cs, "java/io/ByteArrayOutputStream", "count", "I")?;
    let buf_ref = field_ref(heap, receiver, buf_slot)?;
    let count = field_int(heap, receiver, count_slot)? as usize;
    let src: Vec<u8> = if buf_ref.is_null() {
        Vec::new()
    } else {
        byte_data(heap, buf_ref)?[..count]
            .iter()
            .map(|b| *b as u8)
            .collect()
    };
    let copy = alloc_byte_array_from(cs, heap, &src)?;
    ret_ref(copy)
}

pub fn baos_size(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let count_slot = field_slot(cs, "java/io/ByteArrayOutputStream", "count", "I")?;
    ret_i32(field_int(heap, receiver, count_slot)?)
}

pub fn baos_reset(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let count_slot = field_slot(cs, "java/io/ByteArrayOutputStream", "count", "I")?;
    set_field(heap, receiver, count_slot, Slot::Int(0))?;
    Ok(None)
}

pub fn baos_to_string(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let buf_slot = field_slot(cs, "java/io/ByteArrayOutputStream", "buf", "[B")?;
    let count_slot = field_slot(cs, "java/io/ByteArrayOutputStream", "count", "I")?;
    let buf_ref = field_ref(heap, receiver, buf_slot)?;
    let count = field_int(heap, receiver, count_slot)? as usize;
    // CLDC toString 按平台缺省编码；此处固定 Latin-1（逐字节 → 码元），
    // 与 PrintStream 包装模式一致（宿主编码接入 TODO）。
    let chars: Vec<u16> = if buf_ref.is_null() {
        Vec::new()
    } else {
        byte_data(heap, buf_ref)?[..count].iter().map(|b| *b as u8 as u16).collect()
    };
    ret_string(heap, chars)
}

// ---------------------------------------------------------------------------
// DataInputStream（底层经 FilterInputStream.in）
// ---------------------------------------------------------------------------

/// 通用原语读：n 字节大端读满（EOF → EOFException）。
fn dis_read_exact(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    receiver: ObjRef,
    n: usize,
) -> Result<Vec<u8>, KvmError> {
    let underlying = filter_in(cs, heap, receiver)?;
    let mut buf = vec![0u8; n];
    input_read_exact(cs, heap, underlying, &mut buf)?;
    Ok(buf)
}

/// DIS <init>(InputStream)：底层流装入继承的 FIS.in 字段。
pub fn dis_init(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let input = arg_ref(call, 0)?;
    let in_slot = field_slot(cs, "java/io/FilterInputStream", "in", "Ljava/io/InputStream;")?;
    set_field(heap, receiver, in_slot, Slot::Ref(input))?;
    Ok(None)
}

pub fn dis_read_single(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let underlying = filter_in(cs, heap, receiver)?;
    match input_read_byte(cs, heap, underlying)? {
        Some(byte) => ret_i32(byte as i32),
        None => ret_i32(-1),
    }
}

pub fn dis_read_range(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let buf = arg_ref(call, 0)?;
    let off = arg_i32(call, 1)?;
    let len = arg_i32(call, 2)?;
    let (off, len) = range_or_throw(cs, heap, heap.array(buf)?.data.len(), off, len)?;
    if len == 0 {
        return ret_i32(0);
    }
    let underlying = filter_in(cs, heap, receiver)?;
    let mut tmp = vec![0u8; len];
    match input_read(cs, heap, underlying, &mut tmp)? {
        None => ret_i32(-1),
        Some(n) => {
            let data = byte_data_mut(heap, buf)?;
            for i in 0..n {
                data[off + i] = tmp[i] as i8;
            }
            ret_i32(n as i32)
        }
    }
}

/// read([B)：off=0、len=buf.length 全量读（DIS/FIS 共享）。
pub fn dis_read_array(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let buf = arg_ref(call, 0)?;
    let cap = heap.array(buf)?.data.len();
    let (off, len) = range_or_throw(cs, heap, cap, 0, cap as i32)?;
    if len == 0 {
        return ret_i32(0);
    }
    let underlying = filter_in(cs, heap, receiver)?;
    let mut tmp = vec![0u8; len];
    match input_read(cs, heap, underlying, &mut tmp)? {
        None => ret_i32(-1),
        Some(n) => {
            let data = byte_data_mut(heap, buf)?;
            for i in 0..n {
                data[off + i] = tmp[i] as i8;
            }
            ret_i32(n as i32)
        }
    }
}

/// readFully 实现（共享于 ([B)V 与 ([BII)V）。
fn dis_read_fully_impl(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    receiver: ObjRef,
    buf: ObjRef,
    off: i32,
    len: i32,
) -> Result<Option<Slot>, KvmError> {
    let (off, len) = range_or_throw(cs, heap, heap.array(buf)?.data.len(), off, len)?;
    let underlying = filter_in(cs, heap, receiver)?;
    let mut tmp = vec![0u8; len];
    input_read_exact(cs, heap, underlying, &mut tmp)?;
    let data = byte_data_mut(heap, buf)?;
    for i in 0..len {
        data[off + i] = tmp[i] as i8;
    }
    Ok(None)
}

pub fn dis_read_fully_all(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let buf = arg_ref(call, 0)?;
    let len = heap.array(buf)?.data.len() as i32;
    dis_read_fully_impl(cs, heap, receiver, buf, 0, len)
}

pub fn dis_read_fully_range(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let buf = arg_ref(call, 0)?;
    let off = arg_i32(call, 1)?;
    let len = arg_i32(call, 2)?;
    dis_read_fully_impl(cs, heap, receiver, buf, off, len)
}

pub fn dis_read_boolean(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let bytes = dis_read_exact(cs, heap, receiver, 1)?;
    ret_bool(bytes[0] != 0)
}

pub fn dis_read_byte(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let bytes = dis_read_exact(cs, heap, receiver, 1)?;
    ret_i32(bytes[0] as i8 as i32)
}

pub fn dis_read_unsigned_byte(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let bytes = dis_read_exact(cs, heap, receiver, 1)?;
    ret_i32(bytes[0] as i32)
}

/// 调试面：SKYENGINE_KVM_TRACE_IO=1 时打印 DataInputStream 读原语的
/// 值序列（.ani 等自定义格式取证——与离线模拟对比找分叉）。
fn trace_io(op: &str, value: u64) {
    if std::env::var_os("SKYENGINE_KVM_TRACE_IO").is_some() {
        eprintln!("[kvm-io] {op} {value}");
    }
}

pub fn dis_read_short(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let bytes = dis_read_exact(cs, heap, receiver, 2)?;
    let value = i16::from_be_bytes([bytes[0], bytes[1]]) as i32;
    trace_io("readShort", value as u64);
    ret_i32(value)
}

pub fn dis_read_unsigned_short(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let bytes = dis_read_exact(cs, heap, receiver, 2)?;
    ret_i32(u16::from_be_bytes([bytes[0], bytes[1]]) as i32)
}

pub fn dis_read_char(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let bytes = dis_read_exact(cs, heap, receiver, 2)?;
    ret_i32(u16::from_be_bytes([bytes[0], bytes[1]]) as i32)
}

pub fn dis_read_int(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let bytes = dis_read_exact(cs, heap, receiver, 4)?;
    ret_i32(i32::from_be_bytes([
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
    ]))
}

pub fn dis_read_long(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let bytes = dis_read_exact(cs, heap, receiver, 8)?;
    ret_long(i64::from_be_bytes([
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
    ]))
}

/// JDK skipBytes 循环补齐；此处单次 skip（BAIS 单层即可到位，
/// 包装流语义偏差无害，注释标注）。
pub fn dis_skip_bytes(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let n = arg_i32(call, 0)?;
    if n <= 0 {
        return ret_i32(0);
    }
    let underlying = filter_in(cs, heap, receiver)?;
    let skipped = input_skip(cs, heap, underlying, i64::from(n))?;
    ret_i32(skipped as i32)
}

pub fn dis_skip(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let n = arg_i64(call, 0)?;
    let underlying = filter_in(cs, heap, receiver)?;
    ret_long(input_skip(cs, heap, underlying, n)?)
}

pub fn dis_available(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let underlying = filter_in(cs, heap, receiver)?;
    ret_i32(input_available(cs, heap, underlying)?)
}

pub fn dis_read_utf(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    // 长度前缀（u16 大端）；不足 2 字节 → EOFException。
    let prefix = dis_read_exact(cs, heap, receiver, 2)?;
    let utflen = u16::from_be_bytes([prefix[0], prefix[1]]) as usize;
    trace_io("readUTF.len", utflen as u64);
    let underlying = filter_in(cs, heap, receiver)?;
    let mut data = vec![0u8; utflen];
    // 数据段中途 EOF → EOFException（JDK 语义）。
    input_read_exact(cs, heap, underlying, &mut data)?;
    match decode_modified_utf8(&data) {
        Ok(chars) => ret_string(heap, chars),
        Err(Utf8Fault::Truncated) => Err(throw_eof(cs, heap)),
        Err(Utf8Fault::Malformed) => Err(jl::throw_java(
            cs,
            heap,
            "java/io/UTFDataFormatException",
            Some("malformed UTF-8"),
        )),
    }
}

/// readLine：Latin-1 累积，读到 \n 或 \r 即止。
/// 偏差：**不预读吞掉 \r 后的 \n**（JDK 会预读；MIDlet 语料无
/// \r\n 行尾，无害）；立即 EOF → null。
pub fn dis_read_line(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let underlying = filter_in(cs, heap, receiver)?;
    let first = input_read_byte(cs, heap, underlying)?;
    match first {
        None => Ok(Some(Slot::Ref(ObjRef::NULL))),
        Some(b'\n') | Some(b'\r') => ret_string(heap, Vec::new()),
        Some(byte) => {
            let mut chars = vec![u16::from(byte)];
            loop {
                match input_read_byte(cs, heap, underlying)? {
                    None | Some(b'\n') | Some(b'\r') => break,
                    Some(byte) => chars.push(u16::from(byte)),
                }
            }
            ret_string(heap, chars)
        }
    }
}

// ---------------------------------------------------------------------------
// DataOutputStream（底层经 FilterOutputStream.out，written 计数）
// ---------------------------------------------------------------------------

pub fn dos_init(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let out = arg_ref(call, 0)?;
    let out_slot = field_slot(cs, "java/io/FilterOutputStream", "out", "Ljava/io/OutputStream;")?;
    let written_slot = field_slot(cs, "java/io/DataOutputStream", "written", "I")?;
    set_field(heap, receiver, out_slot, Slot::Ref(out))?;
    set_field(heap, receiver, written_slot, Slot::Int(0))?;
    Ok(None)
}

/// 原语写：字节直写底层 + written 累加（字节数 = 各原语编码长度）。
fn dos_write_raw(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    receiver: ObjRef,
    bytes: &[u8],
) -> Result<(), KvmError> {
    let underlying = filter_out(cs, heap, receiver)?;
    output_write(cs, heap, underlying, bytes)?;
    dos_add_written(cs, heap, receiver, bytes.len() as i32)
}

pub fn dos_write_int(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let value = arg_i32(call, 0)?;
    dos_write_raw(cs, heap, receiver, &[value as u8])?;
    Ok(None)
}

pub fn dos_write_range(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let buf = arg_ref(call, 0)?;
    let off = arg_i32(call, 1)?;
    let len = arg_i32(call, 2)?;
    let (off, len) = range_or_throw(cs, heap, heap.array(buf)?.data.len(), off, len)?;
    let src: Vec<u8> = byte_data(heap, buf)?[off..off + len]
        .iter()
        .map(|b| *b as u8)
        .collect();
    dos_write_raw(cs, heap, receiver, &src)?;
    Ok(None)
}

pub fn dos_write_boolean(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let value = arg_i32(call, 0)?;
    dos_write_raw(cs, heap, receiver, &[if value != 0 { 1 } else { 0 }])?;
    Ok(None)
}

pub fn dos_write_short(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let value = arg_i32(call, 0)?;
    dos_write_raw(cs, heap, receiver, &(value as u16).to_be_bytes())?;
    Ok(None)
}

pub fn dos_write_char(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let value = arg_i32(call, 0)?;
    dos_write_raw(cs, heap, receiver, &(value as u16).to_be_bytes())?;
    Ok(None)
}

pub fn dos_write_int_value(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let value = arg_i32(call, 0)?;
    dos_write_raw(cs, heap, receiver, &value.to_be_bytes())?;
    Ok(None)
}

pub fn dos_write_long(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let value = arg_i64(call, 0)?;
    dos_write_raw(cs, heap, receiver, &value.to_be_bytes())?;
    Ok(None)
}

pub fn dos_write_float(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let value = arg_f32(call, 0)?;
    dos_write_raw(cs, heap, receiver, &value.to_bits().to_be_bytes())?;
    Ok(None)
}

pub fn dos_write_double(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let value = arg_f64(call, 0)?;
    dos_write_raw(cs, heap, receiver, &value.to_bits().to_be_bytes())?;
    Ok(None)
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

/// readFloat：4 字节大端 IEEE-754 位型；尾部截断 → EOFException。
pub fn dis_read_float(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let buf = dis_read_exact(cs, heap, receiver, 4)?;
    let bits = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    Ok(Some(Slot::Float(f32::from_bits(bits))))
}

/// readDouble：8 字节大端 IEEE-754 位型；尾部截断 → EOFException。
pub fn dis_read_double(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let buf = dis_read_exact(cs, heap, receiver, 8)?;
    let bits = u64::from_be_bytes([
        buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
    ]);
    Ok(Some(Slot::Double(f64::from_bits(bits))))
}

pub fn dos_write_utf(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let value = arg_ref(call, 0)?;
    if value.is_null() {
        return Err(throw_npe(cs, heap));
    }
    let chars = string_chars(heap, value)?;
    let encoded = encode_modified_utf8(&chars);
    if encoded.len() > 65535 {
        return Err(jl::throw_java(
            cs,
            heap,
            "java/io/UTFDataFormatException",
            Some("encoded string too long"),
        ));
    }
    let mut out = Vec::with_capacity(2 + encoded.len());
    out.extend_from_slice(&(encoded.len() as u16).to_be_bytes());
    out.extend_from_slice(&encoded);
    dos_write_raw(cs, heap, receiver, &out)?;
    Ok(None)
}

pub fn dos_size(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let written_slot = field_slot(cs, "java/io/DataOutputStream", "written", "I")?;
    ret_i32(field_int(heap, receiver, written_slot)?)
}

pub fn dos_write_all(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let buf = arg_ref(call, 0)?;
    let len = heap.array(buf)?.data.len();
    dis_write_all_impl(cs, heap, receiver, buf, 0, len as i32)
}

/// write([B) 共享实现（DOS 计数 / FIS/FOS 直通）。
fn dis_write_all_impl(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    receiver: ObjRef,
    buf: ObjRef,
    off: i32,
    len: i32,
) -> Result<Option<Slot>, KvmError> {
    let (off, len) = range_or_throw(cs, heap, heap.array(buf)?.data.len(), off, len)?;
    let src: Vec<u8> = byte_data(heap, buf)?[off..off + len]
        .iter()
        .map(|b| *b as u8)
        .collect();
    let underlying = filter_out(cs, heap, receiver)?;
    output_write(cs, heap, underlying, &src)?;
    Ok(None)
}

// ---------------------------------------------------------------------------
// FilterOutputStream / FilterInputStream（直通底层；与 DIS/DOS 共用
// in/out 字段布局，读侧直接复用 dis_* 系列）
// ---------------------------------------------------------------------------

pub fn fos_write_int(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let value = arg_i32(call, 0)?;
    let underlying = filter_out(cs, heap, receiver)?;
    output_write(cs, heap, underlying, &[value as u8])?;
    Ok(None)
}

pub fn fos_write_all(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let buf = arg_ref(call, 0)?;
    let len = heap.array(buf)?.data.len();
    dis_write_all_impl(cs, heap, receiver, buf, 0, len as i32)
}

pub fn fos_write_range(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let buf = arg_ref(call, 0)?;
    let off = arg_i32(call, 1)?;
    let len = arg_i32(call, 2)?;
    dis_write_all_impl(cs, heap, receiver, buf, off, len)
}

// ---------------------------------------------------------------------------
// PrintStream
// ---------------------------------------------------------------------------

pub fn ps_init_stream(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let out = arg_ref(call, 0)?;
    let out_slot = field_slot(cs, "java/io/PrintStream", "out", "Ljava/io/OutputStream;")?;
    let fd_slot = field_slot(cs, "java/io/PrintStream", "fd", "I")?;
    set_field(heap, receiver, out_slot, Slot::Ref(out))?;
    set_field(heap, receiver, fd_slot, Slot::Int(0))?;
    Ok(None)
}

/// 文本落点：fd≠0 → 宿主标准错误（Unicode 直写，TODO T045 桥接
/// 宿主错误画面）；否则包装模式按 ISO-8859-1 逐码元（>0xFF 截断
/// 低字节）写底层流。
fn ps_emit(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    ps: ObjRef,
    text: &str,
) -> Result<(), KvmError> {
    let fd_slot = field_slot(cs, "java/io/PrintStream", "fd", "I")?;
    if field_int(heap, ps, fd_slot)? != 0 {
        eprint!("{text}");
        Ok(())
    } else {
        let out_slot = field_slot(cs, "java/io/PrintStream", "out", "Ljava/io/OutputStream;")?;
        let underlying = field_ref(heap, ps, out_slot)?;
        let bytes: Vec<u8> = text.encode_utf16().map(|unit| unit as u8).collect();
        output_write(cs, heap, underlying, &bytes)
    }
}

fn ps_print_text(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    ps: ObjRef,
    text: &str,
    newline: bool,
) -> Result<(), KvmError> {
    ps_emit(cs, heap, ps, text)?;
    if newline {
        ps_emit(cs, heap, ps, "\n")?;
    }
    Ok(())
}

pub fn ps_print_string(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let ps = this(call)?;
    let value = arg_ref(call, 0)?;
    let text = if value.is_null() {
        "null".to_string()
    } else {
        String::from_utf16_lossy(&string_chars(heap, value)?)
    };
    ps_print_text(cs, heap, ps, &text, false)?;
    Ok(None)
}

pub fn ps_println_string(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let ps = this(call)?;
    let value = arg_ref(call, 0)?;
    let text = if value.is_null() {
        "null".to_string()
    } else {
        String::from_utf16_lossy(&string_chars(heap, value)?)
    };
    ps_print_text(cs, heap, ps, &text, true)?;
    Ok(None)
}

pub fn ps_print_int(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let ps = this(call)?;
    let text = format!("{}", arg_i32(call, 0)?);
    ps_print_text(cs, heap, ps, &text, false)?;
    Ok(None)
}

pub fn ps_println_int(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let ps = this(call)?;
    let text = format!("{}", arg_i32(call, 0)?);
    ps_print_text(cs, heap, ps, &text, true)?;
    Ok(None)
}

pub fn ps_print_long(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let ps = this(call)?;
    let text = format!("{}", arg_i64(call, 0)?);
    ps_print_text(cs, heap, ps, &text, false)?;
    Ok(None)
}

pub fn ps_println_long(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let ps = this(call)?;
    let text = format!("{}", arg_i64(call, 0)?);
    ps_print_text(cs, heap, ps, &text, true)?;
    Ok(None)
}

pub fn ps_print_boolean(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let ps = this(call)?;
    let text = if arg_i32(call, 0)? != 0 { "true" } else { "false" };
    ps_print_text(cs, heap, ps, text, false)?;
    Ok(None)
}

pub fn ps_println_boolean(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let ps = this(call)?;
    let text = if arg_i32(call, 0)? != 0 { "true" } else { "false" };
    ps_print_text(cs, heap, ps, text, true)?;
    Ok(None)
}

pub fn ps_print_char(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let ps = this(call)?;
    let text = String::from_utf16_lossy(&[arg_i32(call, 0)? as u16]);
    ps_print_text(cs, heap, ps, &text, false)?;
    Ok(None)
}

pub fn ps_println_char(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let ps = this(call)?;
    let text = String::from_utf16_lossy(&[arg_i32(call, 0)? as u16]);
    ps_print_text(cs, heap, ps, &text, true)?;
    Ok(None)
}

/// float/double 文本桥接 jl 静态 toString 原生（合成 NativeCall）。
fn static_to_string_text(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    func: NativeFn,
    class_name: &str,
    desc: &str,
    arg: Slot,
) -> Result<String, KvmError> {
    let class = cs.load_class(class_name)?;
    let name = cs.syms.intern("toString");
    let desc_sym = cs.syms.intern(desc);
    let call = NativeCall {
        class,
        name,
        desc: desc_sym,
        this: None,
        args: vec![arg],
        // toString 桥接无线程语境（T033 保留 id 0）。
        thread: ThreadId(0),
    };
    match func(cs, heap, &call)? {
        Some(Slot::Ref(text)) => Ok(String::from_utf16_lossy(&heap.str_obj(text)?.chars)),
        other => Err(KvmError::Internal(format!("toString bridge returned {other:?}"))),
    }
}

pub fn ps_print_float(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let ps = this(call)?;
    let value = match call.args.first() {
        Some(Slot::Float(value)) => *value,
        other => {
            return Err(KvmError::Internal(format!(
                "native arg 0 expected float, got {other:?}"
            )))
        }
    };
    let text = static_to_string_text(
        cs,
        heap,
        jl::float_to_string_static,
        "java/lang/Float",
        "(F)Ljava/lang/String;",
        Slot::Float(value),
    )?;
    ps_print_text(cs, heap, ps, &text, false)?;
    Ok(None)
}

pub fn ps_print_double(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let ps = this(call)?;
    let value = match call.args.first() {
        Some(Slot::Double(value)) => *value,
        other => {
            return Err(KvmError::Internal(format!(
                "native arg 0 expected double, got {other:?}"
            )))
        }
    };
    let text = static_to_string_text(
        cs,
        heap,
        jl::double_to_string_static,
        "java/lang/Double",
        "(D)Ljava/lang/String;",
        Slot::Double(value),
    )?;
    ps_print_text(cs, heap, ps, &text, false)?;
    Ok(None)
}

pub fn ps_println_float(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let ps = this(call)?;
    let value = match call.args.first() {
        Some(Slot::Float(value)) => *value,
        other => {
            return Err(KvmError::Internal(format!(
                "native arg 0 expected float, got {other:?}"
            )))
        }
    };
    let text = static_to_string_text(
        cs,
        heap,
        jl::float_to_string_static,
        "java/lang/Float",
        "(F)Ljava/lang/String;",
        Slot::Float(value),
    )?;
    ps_print_text(cs, heap, ps, &text, true)?;
    Ok(None)
}

pub fn ps_println_double(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let ps = this(call)?;
    let value = match call.args.first() {
        Some(Slot::Double(value)) => *value,
        other => {
            return Err(KvmError::Internal(format!(
                "native arg 0 expected double, got {other:?}"
            )))
        }
    };
    let text = static_to_string_text(
        cs,
        heap,
        jl::double_to_string_static,
        "java/lang/Double",
        "(D)Ljava/lang/String;",
        Slot::Double(value),
    )?;
    ps_print_text(cs, heap, ps, &text, true)?;
    Ok(None)
}

/// print(Object) 文本：null → "null"；String 载荷直取；Throwable 族 /
/// 包装类 / StringBuffer / Date 桥接各自 toString 原生（合成
/// NativeCall，this = 实例）；其余按 Object.toString 缺省
/// （点分类名@对象地址十六进制）。
fn object_text(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    object: ObjRef,
) -> Result<String, KvmError> {
    if object.is_null() {
        return Ok("null".into());
    }
    if let Payload::Str(s) = heap.payload(object)? {
        return Ok(String::from_utf16_lossy(&s.chars));
    }
    let class_name = cs.name_of(heap.instance(object)?.class).to_string();
    // Throwable 族按子类关系匹配（异常 println 常用）。
    let throwable = cs.load_class("java/lang/Throwable")?;
    let func: Option<NativeFn> = match class_name.as_str() {
        "java/lang/Integer" => Some(jl::integer_to_string),
        "java/lang/Long" => Some(jl::long_to_string),
        "java/lang/Boolean" => Some(jl::boolean_to_string),
        "java/lang/Character" => Some(jl::character_to_string),
        "java/lang/Float" => Some(jl::float_to_string),
        "java/lang/Double" => Some(jl::double_to_string),
        "java/lang/StringBuffer" => Some(jl::sb_to_string),
        "java/util/Date" => Some(ju::date_to_string),
        _ if cs.is_subclass_of(heap.instance(object)?.class, throwable) => {
            Some(jl::throwable_to_string)
        }
        _ => None,
    };
    match func {
        Some(func) => {
            let class = cs.load_class(&class_name)?;
            let name = cs.syms.intern("toString");
            let desc = cs.syms.intern("()Ljava/lang/String;");
            let call = NativeCall {
                class,
                name,
                desc,
                this: Some(object),
                args: Vec::new(),
                // toString 桥接无线程语境（T033 保留 id 0）。
                thread: ThreadId(0),
            };
            match func(cs, heap, &call)? {
                Some(Slot::Ref(text)) => {
                    Ok(String::from_utf16_lossy(&heap.str_obj(text)?.chars))
                }
                other => Err(KvmError::Internal(format!(
                    "toString bridge returned {other:?}"
                ))),
            }
        }
        None => Ok(format!("{}@{:x}", class_name.replace('/', "."), object.0)),
    }
}

pub fn ps_print_object(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let ps = this(call)?;
    let text = object_text(cs, heap, arg_ref(call, 0)?)?;
    ps_print_text(cs, heap, ps, &text, false)?;
    Ok(None)
}

pub fn ps_println_object(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let ps = this(call)?;
    let text = object_text(cs, heap, arg_ref(call, 0)?)?;
    ps_print_text(cs, heap, ps, &text, true)?;
    Ok(None)
}

pub fn ps_println_empty(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let ps = this(call)?;
    ps_emit(cs, heap, ps, "\n")?;
    Ok(None)
}

pub fn ps_write_int(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let ps = this(call)?;
    let text = String::from_utf16_lossy(&[arg_i32(call, 0)? as u16]);
    ps_emit(cs, heap, ps, &text)?;
    Ok(None)
}

pub fn ps_write_range(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let ps = this(call)?;
    let buf = arg_ref(call, 0)?;
    let off = arg_i32(call, 1)?;
    let len = arg_i32(call, 2)?;
    let (off, len) = range_or_throw(cs, heap, heap.array(buf)?.data.len(), off, len)?;
    let bytes: Vec<u8> = byte_data(heap, buf)?[off..off + len]
        .iter()
        .map(|b| *b as u8)
        .collect();
    let text: String = bytes.iter().map(|b| char::from(*b)).collect();
    ps_emit(cs, heap, ps, &text)?;
    Ok(None)
}

// ---------------------------------------------------------------------------
// System.<clinit>（原生类初始化：构造 out/err）
// ---------------------------------------------------------------------------

/// System.<clinit>：构造 out/err 两个 PrintStream（fd=1 → 宿主标准
/// 错误，TODO T045 桥接错误画面）；in 保持静态区零值 null
///（标准输入接入 = TODO）。
pub fn system_clinit(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let system = cs.load_class("java/lang/System")?;
    let fd_slot = field_slot(cs, "java/io/PrintStream", "fd", "I")?;
    for name in ["out", "err"] {
        let slot = field_slot(cs, "java/lang/System", name, "Ljava/io/PrintStream;")?;
        let ps = alloc_instance_of(cs, heap, "java/io/PrintStream")?;
        heap.instance_mut(ps)?.fields[fd_slot] = Slot::Int(1);
        cs.class_mut(system).statics[slot] = Slot::Ref(ps);
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// InputStreamReader（Reader 实现；encoding 字段 0=UTF-8，1=Latin-1）
// ---------------------------------------------------------------------------

/// 编码名 → 代号（大小写不敏感；支持的形态保守取 CLDC 常用集）。
fn encoding_code(name: &str) -> Option<i32> {
    match name.to_ascii_lowercase().as_str() {
        "utf-8" | "utf8" => Some(0),
        "iso-8859-1" | "iso8859-1" | "iso8859_1" => Some(1),
        _ => None,
    }
}

/// 代号 → 规范编码名（getEncoding 返回）。
fn encoding_name(code: i32) -> &'static str {
    if code == 1 {
        "ISO-8859-1"
    } else {
        "UTF-8"
    }
}

/// UTF-8 续字节读取结果。
enum Utf8Next {
    /// 流耗尽（尾部截断）。
    Exhausted,
    /// 非法续字节形态。
    Malformed,
    /// 合法续字节（低 6 位）。
    Cont(u8),
}

fn utf8_next(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    underlying: ObjRef,
) -> Result<Utf8Next, KvmError> {
    match input_read_byte(cs, heap, underlying)? {
        None => Ok(Utf8Next::Exhausted),
        Some(byte) if byte & 0xC0 == 0x80 => Ok(Utf8Next::Cont(byte & 0x3F)),
        Some(_) => Ok(Utf8Next::Malformed),
    }
}

/// 标准 UTF-8 单序列解码（首字节已取出）。
/// `Ok(None)` = 流耗尽（尾部截断按耗尽处理）；非法形态 → U+FFFD
///（已消费字节不回退）。
fn utf8_seq(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    underlying: ObjRef,
    first: u8,
) -> Result<Option<u16>, KvmError> {
    match first {
        0x00..=0x7F => Ok(Some(u16::from(first))),
        0xC2..=0xDF => match utf8_next(cs, heap, underlying)? {
            Utf8Next::Cont(b2) => Ok(Some((u16::from(first & 0x1F) << 6) | u16::from(b2))),
            Utf8Next::Exhausted => Ok(None),
            Utf8Next::Malformed => Ok(Some(0xFFFD)),
        },
        0xE0..=0xEF => {
            let b2 = utf8_next(cs, heap, underlying)?;
            let b3 = utf8_next(cs, heap, underlying)?;
            match (b2, b3) {
                (Utf8Next::Cont(x2), Utf8Next::Cont(x3)) => Ok(Some(
                    (u16::from(first & 0x0F) << 12) | (u16::from(x2) << 6) | u16::from(x3),
                )),
                (Utf8Next::Exhausted, _) | (_, Utf8Next::Exhausted) => Ok(None),
                _ => Ok(Some(0xFFFD)),
            }
        }
        // 孤续字节 / 超长形态（C0、C1）/ 超出 BMP 序列（F0+）：U+FFFD。
        _ => Ok(Some(0xFFFD)),
    }
}

/// 读一个 UTF-16 码元：pending（低代理/回退单元）优先；UTF-8 高代理
/// 时预读下一序列，低代理或回退单元入 pending（跨 read 调用保序，
/// 无需字节回退——UTF-16 码元语义下代理对跨两次 read 返回合法）；
/// 尾部截断丢字节按流耗尽（宽松处理）。
fn isr_read_unit(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    receiver: ObjRef,
) -> Result<Option<u16>, KvmError> {
    let pending_slot = field_slot(cs, "java/io/InputStreamReader", "pending", "I")?;
    let pending = field_int(heap, receiver, pending_slot)?;
    if pending != 0 {
        set_field(heap, receiver, pending_slot, Slot::Int(0))?;
        return Ok(Some(pending as u16));
    }
    let in_slot = field_slot(cs, "java/io/InputStreamReader", "in", "Ljava/io/InputStream;")?;
    let underlying = field_ref(heap, receiver, in_slot)?;
    if underlying.is_null() {
        return Err(throw_npe(cs, heap));
    }
    let encoding_slot = field_slot(cs, "java/io/InputStreamReader", "encoding", "I")?;
    let encoding = field_int(heap, receiver, encoding_slot)?;
    if encoding == 1 {
        // Latin-1：字节零扩展即码元。
        return Ok(input_read_byte(cs, heap, underlying)?.map(u16::from));
    }
    let Some(first) = input_read_byte(cs, heap, underlying)? else {
        return Ok(None);
    };
    let Some(unit) = utf8_seq(cs, heap, underlying, first)? else {
        return Ok(None);
    };
    if (0xD800..=0xDBFF).contains(&unit) {
        let next = match input_read_byte(cs, heap, underlying)? {
            None => None,
            Some(byte) => utf8_seq(cs, heap, underlying, byte)?,
        };
        if let Some(next_unit) = next {
            set_field(heap, receiver, pending_slot, Slot::Int(next_unit as i32))?;
        }
        return Ok(Some(unit));
    }
    Ok(Some(unit))
}

fn isr_init_common(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    receiver: ObjRef,
    input: ObjRef,
    code: i32,
) -> Result<(), KvmError> {
    let in_slot = field_slot(cs, "java/io/InputStreamReader", "in", "Ljava/io/InputStream;")?;
    let encoding_slot = field_slot(cs, "java/io/InputStreamReader", "encoding", "I")?;
    let pending_slot = field_slot(cs, "java/io/InputStreamReader", "pending", "I")?;
    set_field(heap, receiver, in_slot, Slot::Ref(input))?;
    set_field(heap, receiver, encoding_slot, Slot::Int(code))?;
    set_field(heap, receiver, pending_slot, Slot::Int(0))
}

pub fn isr_init_default(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let input = arg_ref(call, 0)?;
    // 无编码名：平台缺省编码，此处固定 UTF-8（宿主时区/编码接入 TODO）。
    isr_init_common(cs, heap, receiver, input, 0)?;
    Ok(None)
}

pub fn isr_init_named(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let input = arg_ref(call, 0)?;
    let name_ref = arg_ref(call, 1)?;
    if name_ref.is_null() {
        return Err(throw_npe(cs, heap));
    }
    let name = String::from_utf16_lossy(&string_chars(heap, name_ref)?);
    let Some(code) = encoding_code(&name) else {
        return Err(jl::throw_java(
            cs,
            heap,
            "java/io/UnsupportedEncodingException",
            Some(name.as_str()),
        ));
    };
    isr_init_common(cs, heap, receiver, input, code)?;
    Ok(None)
}

pub fn isr_get_encoding(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let encoding_slot = field_slot(cs, "java/io/InputStreamReader", "encoding", "I")?;
    let code = field_int(heap, receiver, encoding_slot)?;
    ret_string(heap, utf16(encoding_name(code)))
}

pub fn isr_read_single(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    match isr_read_unit(cs, heap, receiver)? {
        Some(unit) => ret_i32(i32::from(unit)),
        None => ret_i32(-1),
    }
}

pub fn isr_read_range(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let buf = arg_ref(call, 0)?;
    let off = arg_i32(call, 1)?;
    let len = arg_i32(call, 2)?;
    let (off, len) = range_or_throw(cs, heap, heap.array(buf)?.data.len(), off, len)?;
    let mut filled = 0usize;
    while filled < len {
        match isr_read_unit(cs, heap, receiver)? {
            None => break,
            Some(unit) => {
                char_data_mut(heap, buf)?[off + filled] = unit;
                filled += 1;
            }
        }
    }
    if filled == 0 && len > 0 {
        ret_i32(-1)
    } else {
        ret_i32(filled as i32)
    }
}

// ---------------------------------------------------------------------------
// OutputStreamWriter（Writer 实现）
// ---------------------------------------------------------------------------

fn osw_out(cs: &mut ClassSpace, heap: &Heap, receiver: ObjRef) -> Result<ObjRef, KvmError> {
    let slot = field_slot(cs, "java/io/OutputStreamWriter", "out", "Ljava/io/OutputStream;")?;
    field_ref(heap, receiver, slot)
}

fn osw_encoding(
    cs: &mut ClassSpace,
    heap: &Heap,
    receiver: ObjRef,
) -> Result<i32, KvmError> {
    let slot = field_slot(cs, "java/io/OutputStreamWriter", "encoding", "I")?;
    field_int(heap, receiver, slot)
}

/// 标准 UTF-8 单码元编码追加（区别于 Modified UTF-8：U+0000 按
/// 单字节 0x00）。
fn utf8_push_unit(unit: u16, out: &mut Vec<u8>) {
    if unit < 0x80 {
        out.push(unit as u8);
    } else if unit < 0x800 {
        out.push(0xC0 | (unit >> 6) as u8);
        out.push(0x80 | (unit & 0x3F) as u8);
    } else {
        out.push(0xE0 | (unit >> 12) as u8);
        out.push(0x80 | ((unit >> 6) & 0x3F) as u8);
        out.push(0x80 | (unit & 0x3F) as u8);
    }
}

/// 码元序列编码：Latin-1 逐码元截断；UTF-8 代理对各自 3 字节
///（合计 6 字节），孤代理 → '?'（对齐 JDK CharsetEncoder 缺省
/// REPLACE 动作）。
fn osw_encode(
    cs: &mut ClassSpace,
    heap: &Heap,
    receiver: ObjRef,
    units: &[u16],
) -> Result<Vec<u8>, KvmError> {
    if osw_encoding(cs, heap, receiver)? == 1 {
        return Ok(units.iter().map(|unit| *unit as u8).collect());
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < units.len() {
        let unit = units[i];
        if (0xD800..=0xDBFF).contains(&unit)
            && i + 1 < units.len()
            && (0xDC00..=0xDFFF).contains(&units[i + 1])
        {
            utf8_push_unit(unit, &mut out);
            utf8_push_unit(units[i + 1], &mut out);
            i += 2;
        } else if (0xD800..=0xDFFF).contains(&unit) {
            out.push(b'?');
            i += 1;
        } else {
            utf8_push_unit(unit, &mut out);
            i += 1;
        }
    }
    Ok(out)
}

fn osw_write_units(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    receiver: ObjRef,
    units: &[u16],
) -> Result<(), KvmError> {
    let bytes = osw_encode(cs, heap, receiver, units)?;
    let underlying = osw_out(cs, heap, receiver)?;
    output_write(cs, heap, underlying, &bytes)
}

fn osw_init_common(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    receiver: ObjRef,
    out: ObjRef,
    code: i32,
) -> Result<(), KvmError> {
    let out_slot = field_slot(cs, "java/io/OutputStreamWriter", "out", "Ljava/io/OutputStream;")?;
    let encoding_slot = field_slot(cs, "java/io/OutputStreamWriter", "encoding", "I")?;
    set_field(heap, receiver, out_slot, Slot::Ref(out))?;
    set_field(heap, receiver, encoding_slot, Slot::Int(code))
}

pub fn osw_init_default(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let out = arg_ref(call, 0)?;
    // 平台缺省编码固定 UTF-8（与 ISR 对称）。
    osw_init_common(cs, heap, receiver, out, 0)?;
    Ok(None)
}

pub fn osw_init_named(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let out = arg_ref(call, 0)?;
    let name_ref = arg_ref(call, 1)?;
    if name_ref.is_null() {
        return Err(throw_npe(cs, heap));
    }
    let name = String::from_utf16_lossy(&string_chars(heap, name_ref)?);
    let Some(code) = encoding_code(&name) else {
        return Err(jl::throw_java(
            cs,
            heap,
            "java/io/UnsupportedEncodingException",
            Some(name.as_str()),
        ));
    };
    osw_init_common(cs, heap, receiver, out, code)?;
    Ok(None)
}

pub fn osw_get_encoding(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let code = osw_encoding(cs, heap, receiver)?;
    ret_string(heap, utf16(encoding_name(code)))
}

pub fn osw_write_int(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let unit = arg_i32(call, 0)? as u16;
    osw_write_units(cs, heap, receiver, &[unit])?;
    Ok(None)
}

pub fn osw_write_chars_all(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let buf = arg_ref(call, 0)?;
    let len = heap.array(buf)?.data.len();
    osw_write_chars_impl(cs, heap, receiver, buf, 0, len as i32)?;
    Ok(None)
}

fn osw_write_chars_impl(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    receiver: ObjRef,
    buf: ObjRef,
    off: i32,
    len: i32,
) -> Result<(), KvmError> {
    let (off, len) = range_or_throw(cs, heap, heap.array(buf)?.data.len(), off, len)?;
    let units = char_data(heap, buf)?[off..off + len].to_vec();
    osw_write_units(cs, heap, receiver, &units)
}

pub fn osw_write_chars_range(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let buf = arg_ref(call, 0)?;
    let off = arg_i32(call, 1)?;
    let len = arg_i32(call, 2)?;
    osw_write_chars_impl(cs, heap, receiver, buf, off, len)?;
    Ok(None)
}

pub fn osw_write_string(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let value = arg_ref(call, 0)?;
    if value.is_null() {
        return Err(throw_npe(cs, heap));
    }
    let units = string_chars(heap, value)?;
    osw_write_units(cs, heap, receiver, &units)?;
    Ok(None)
}

pub fn osw_write_string_range(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let receiver = this(call)?;
    let value = arg_ref(call, 0)?;
    let off = arg_i32(call, 1)?;
    let len = arg_i32(call, 2)?;
    if value.is_null() {
        return Err(throw_npe(cs, heap));
    }
    let units = string_chars(heap, value)?;
    // JDK Writer.write(String,int,int) 越界 → StringIndexOutOfBounds。
    if off < 0 || len < 0 || i64::from(off) + i64::from(len) > units.len() as i64 {
        return Err(jl::throw_java(
            cs,
            heap,
            "java/lang/StringIndexOutOfBoundsException",
            None,
        ));
    }
    osw_write_units(cs, heap, receiver, &units[off as usize..off as usize + len as usize])?;
    Ok(None)
}

// ---------------------------------------------------------------------------
// 占位实现（抽象方法 / 空操作）
// ---------------------------------------------------------------------------

/// close 空操作：内存流无资源；宿主文件句柄桥接 = TODO。
pub fn io_close(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(None)
}

/// flush 空操作：内存流无缓冲；宿主桥接 = TODO。
pub fn io_flush(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(None)
}

/// 抽象方法占位（acc::ABSTRACT 不可分派；达此即注册/分派配置错误）。
pub fn abstract_stub(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Err(KvmError::Internal(format!(
        "abstract method {}.{} reached dispatch",
        call.class.0,
        call.name .0
    )))
}

/// InputStream.available 缺省（未知子类 0；JDK 无阻塞语义保守值）。
pub fn input_available_stub(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Int(0)))
}

/// InputStream.skip 缺省（未知子类 0；JDK 经缓冲读取实现，此处
/// 保守不消费——调用方按返回值感知）。
pub fn input_skip_stub(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Long(0)))
}
