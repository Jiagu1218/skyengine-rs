//! javax.microedition.rms RecordStore 真实现（T036）：整店 blob 经
//! host 通道（rms_load/rms_save/rms_delete/rms_list，T027 落盘
//! work-dir/rms/<store>.rec）持久化，替换 T010 空桩。
//!
//! ## 存储模型（设计定死：简单可靠优先）
//!
//! - RecordStore **实例字段**只存 `name Ljava/lang/String;` + `open Z`；
//!   记录数据不驻实例——每次操作从 host 装载整店 blob、改后回存。
//!   **不做打开缓存**：MIDlet 语料存档频度低（每帧零次、关卡节点一
//!   次），load+save 的字节数在 KB 量级，即时回存换取"无脏态、无
//!   失效协议"的实现简单性；高频存档游戏若现瓶颈再引驻留缓存。
//! - blob 格式（kvm 侧自定义，host 只当不透明字节；大端）：
//!   `[4B magic "RMS1"][4B version][8B last_modified_ms]`
//!   `[4B next_record_id][4B count]` + 每记录 `[4B id][4B len][bytes]`。
//!   id 从 1 递增不回收；version 每次 add/set/delete +1；
//!   last_modified 在 add/set/delete 时刷新为 host.now_ms()。
//! - RecordEnumeration：enumerateRecords 返回内部实现类
//!   `javax/microedition/rms/RecordEnum`（字段 `storeName`/`ids [I`
//!   快照/`pos I` 游标）；nextRecord* 按游标取 id 后即时查店 blob。
//!   **filter/comparator 非 null 时按无过滤降级**（native 内构造回调
//!   需解释器重入，T036 范围外，首次降级 trace 一条 + TODO 注释）；
//!   keepUpdated 忽略（快照语义）。
//!
//! ## 异常体系（MIDP-2.0 继承链，DECL 见 natives.rs）
//!
//! RecordStoreException : Exception；RecordStoreNotFoundException /
//! RecordStoreFullException / InvalidRecordIDException :
//! RecordStoreException。店满语义不做（宿主盘容量之外的概念）。

use std::sync::atomic::{AtomicBool, Ordering};

use crate::cldc::io::alloc_byte_array_from;
use crate::cldc::lang as jl;
use crate::host::HostContext;
use crate::interp::NativeCall;
use crate::runtime::classspace::ClassSpace;
use crate::runtime::heap::Heap;
use crate::runtime::object::{ArrayData, ElemType};
use crate::runtime::{ObjRef, Slot};
use crate::KvmError;

/// RecordStore 类名（实例字段 name/open 的声明类）。
const STORE_CLASS: &str = "javax/microedition/rms/RecordStore";
/// RecordEnumeration 内部实现类名（本模块注册的 bootstrap 类）。
const ENUM_CLASS: &str = "javax/microedition/rms/RecordEnum";
/// blob 魔数 "RMS1"。
const MAGIC: [u8; 4] = [0x52, 0x4D, 0x53, 0x31];

/// filter/comparator 降级提示只打一次（避免刷屏）。
static DEGRADE_NOTIFIED: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// blob 编解码（大端）
// ---------------------------------------------------------------------------

/// 店 blob 的内存形态。
struct StoreBlob {
    /// 每次结构变更 +1（add/set/delete）。
    version: i32,
    /// 最近一次结构变更的宿主毫秒时钟（新店 0）。
    last_modified_ms: i64,
    /// 下一记录 id（从 1 递增，不回收）。
    next_record_id: i32,
    /// 有序记录表 (id, data)。
    records: Vec<(i32, Vec<u8>)>,
}

impl StoreBlob {
    fn empty() -> Self {
        StoreBlob {
            version: 0,
            last_modified_ms: 0,
            next_record_id: 1,
            records: Vec::new(),
        }
    }

    /// 解析整店 blob；坏档（魔数/截断）报错由调用方转 RecordStoreException。
    fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut p = 0usize;
        let rd_u32 = |p: &mut usize| -> Result<u32, String> {
            if *p + 4 > bytes.len() {
                return Err("blob truncated (u32)".into());
            }
            let v = u32::from_be_bytes([
                bytes[*p],
                bytes[*p + 1],
                bytes[*p + 2],
                bytes[*p + 3],
            ]);
            *p += 4;
            Ok(v)
        };
        if rd_u32(&mut p)? != u32::from_be_bytes(MAGIC) {
            return Err("bad magic".into());
        }
        let version = rd_u32(&mut p)? as i32;
        let hi = rd_u32(&mut p)? as u64;
        let lo = rd_u32(&mut p)? as u64;
        let last_modified_ms = ((hi << 32) | lo) as i64;
        let next_record_id = rd_u32(&mut p)? as i32;
        let count = rd_u32(&mut p)? as usize;
        let mut records = Vec::with_capacity(count.min(65536));
        let mut max_id = 0i32;
        for _ in 0..count {
            let id = rd_u32(&mut p)? as i32;
            let len = rd_u32(&mut p)? as usize;
            if p + len > bytes.len() {
                return Err("record payload truncated".into());
            }
            records.push((id, bytes[p..p + len].to_vec()));
            p += len;
            max_id = max_id.max(id);
        }
        // 尾部多余字节忽略（前向兼容：旧 kvm 读新格式头部+已解析记录）。
        let mut blob = StoreBlob {
            version,
            last_modified_ms,
            next_record_id,
            records,
        };
        // 坏档防御：next 至少越过现存最大 id。
        blob.next_record_id = blob.next_record_id.max(max_id + 1).max(1);
        Ok(blob)
    }

    /// 序列化整店 blob（大端，格式见模块文档）。
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(24 + self.records.iter().map(|(_, d)| d.len() + 8).sum::<usize>());
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&self.version.to_be_bytes());
        out.extend_from_slice(&self.last_modified_ms.to_be_bytes());
        out.extend_from_slice(&self.next_record_id.to_be_bytes());
        out.extend_from_slice(&(self.records.len() as u32).to_be_bytes());
        for (id, data) in &self.records {
            out.extend_from_slice(&id.to_be_bytes());
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(data);
        }
        out
    }

    /// 记录数据字节和（MIDP getSize 为实现定义，取数据净荷）。
    fn data_size(&self) -> i32 {
        self.records.iter().map(|(_, d)| d.len() as i32).sum()
    }
}

// ---------------------------------------------------------------------------
// 小工具（对齐 cldc/io.rs、cldc/util.rs 的实例区访问模式）
// ---------------------------------------------------------------------------

/// 字段槽位（find_field 保守取位）。
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

fn set_field(heap: &mut Heap, this: ObjRef, slot: usize, value: Slot) -> Result<(), KvmError> {
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

fn arg_int(call: &NativeCall, index: usize) -> Result<i32, KvmError> {
    match call.args.get(index) {
        Some(Slot::Int(value)) => Ok(*value),
        other => Err(KvmError::Internal(format!(
            "arg {index} expected int, got {other:?}"
        ))),
    }
}

fn arg_ref(call: &NativeCall, index: usize) -> Result<ObjRef, KvmError> {
    match call.args.get(index) {
        Some(Slot::Ref(value)) => Ok(*value),
        other => Err(KvmError::Internal(format!(
            "arg {index} expected ref, got {other:?}"
        ))),
    }
}

/// String 载荷 → Rust 字符串（UTF-16 有损转换）。
fn string_text(heap: &Heap, reference: ObjRef) -> Result<String, KvmError> {
    let chars = heap.str_obj(reference)?.chars.clone();
    Ok(String::from_utf16_lossy(&chars))
}

fn utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

/// 分配 int[] 并填充。
fn alloc_int_array_from(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    data: &[i32],
) -> Result<ObjRef, KvmError> {
    let class = cs.load_class("[I")?;
    let reference = heap.alloc_array(class, ElemType::Int, data.len())?;
    if let ArrayData::Int(target) = &mut heap.array_mut(reference)?.data {
        target.copy_from_slice(data);
    }
    Ok(reference)
}

/// 抛 RMS 异常的速记。
fn throw_rms(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    class_name: &str,
    message: &str,
) -> KvmError {
    jl::throw_java(cs, heap, class_name, Some(message))
}

// ---------------------------------------------------------------------------
// 店句柄（RecordStore 实例）与 host 装载/回存
// ---------------------------------------------------------------------------

/// 构造 RecordStore 实例（name + open 字段；真机为 final 类禁构造器，
/// 本引擎静态 open 直接内部装配）。
fn alloc_store_handle(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    name: &str,
) -> Result<ObjRef, KvmError> {
    let handle = alloc_instance_of(cs, heap, STORE_CLASS)?;
    let name_slot = field_slot(cs, STORE_CLASS, "name", "Ljava/lang/String;")?;
    let open_slot = field_slot(cs, STORE_CLASS, "open", "Z")?;
    let name_ref = heap.alloc_string_uninterned(utf16(name))?;
    set_field(heap, handle, name_slot, Slot::Ref(name_ref))?;
    set_field(heap, handle, open_slot, Slot::Int(1))?;
    Ok(handle)
}

/// 读取 this 的店名 + 打开检查；关闭后操作 → RecordStoreException。
fn open_store_name(cs: &mut ClassSpace, heap: &mut Heap, this: ObjRef) -> Result<String, KvmError> {
    let name_slot = field_slot(cs, STORE_CLASS, "name", "Ljava/lang/String;")?;
    let open_slot = field_slot(cs, STORE_CLASS, "open", "Z")?;
    if field_int(heap, this, open_slot)? == 0 {
        return Err(throw_rms(
            cs,
            heap,
            "javax/microedition/rms/RecordStoreException",
            "record store is closed",
        ));
    }
    let name_ref = field_ref(heap, this, name_slot)?;
    string_text(heap, name_ref)
}

/// 从 host 装载整店 blob；缺失/坏档 → RecordStoreException（调用方
/// 语义上确保店存在；坏档升级为显式异常而非静默空店）。
fn load_store(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    name: &str,
) -> Result<StoreBlob, KvmError> {
    match host.rms_load(name) {
        Some(bytes) => StoreBlob::decode(&bytes).map_err(|reason| {
            throw_rms(
                cs,
                heap,
                "javax/microedition/rms/RecordStoreException",
                &format!("corrupt store blob: {reason}"),
            )
        }),
        None => Err(throw_rms(
            cs,
            heap,
            "javax/microedition/rms/RecordStoreException",
            "store vanished on host",
        )),
    }
}

/// 回存整店 blob（add/set/delete 路径即时落 host）。
/// 取证面：SKYENGINE_KVM_TRACE_RMS=1 打印每次落盘的记录载荷摘要
/// （全零载荷 = 游戏侧状态清空——菜单文字/战斗伤害消失的根因链）。
fn save_store(host: &mut dyn HostContext, name: &str, blob: &StoreBlob) {
    if std::env::var_os("SKYENGINE_KVM_TRACE_RMS").is_some() {
        for (id, data) in &blob.records {
            let head: Vec<String> = data.iter().take(16).map(|b| format!("{b:02x}")).collect();
            eprintln!(
                "[kvm-rms] save {name} rec#{id} len={} head=[{}] all_zero={}",
                data.len(),
                head.join(" "),
                data.iter().all(|&b| b == 0)
            );
        }
    }
    host.rms_save(name, &blob.encode());
}

/// 参数切片校验 + 取记录载荷（addRecord/setRecord 共用）。
/// javadoc：data null → NPE；off/len 非法 → ArrayIndexOutOfBoundsException。
fn record_slice(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    data_ref: ObjRef,
    off: i32,
    len: i32,
) -> Result<Vec<u8>, KvmError> {
    if data_ref.is_null() {
        return Err(jl::throw_java(
            cs,
            heap,
            "java/lang/NullPointerException",
            Some("record data is null"),
        ));
    }
    let source = match &heap.array(data_ref)?.data {
        ArrayData::Byte(data) => data,
        other => {
            return Err(KvmError::Internal(format!(
                "record data expected byte[], got {other:?}"
            )))
        }
    };
    let start = off as usize;
    let length = len as usize;
    if off < 0 || len < 0 || start + length > source.len() {
        return Err(jl::throw_java(
            cs,
            heap,
            "java/lang/ArrayIndexOutOfBoundsException",
            Some(&format!("off={off} len={len} size={}", source.len())),
        ));
    }
    Ok(source[start..start + length].iter().map(|&b| b as u8).collect())
}

// ---------------------------------------------------------------------------
// 静态 API
// ---------------------------------------------------------------------------

/// openRecordStore(String, boolean)：host 命中 → 句柄；未命中且
/// create → 新空店落 host；未命中且 !create → RecordStoreNotFoundException。
pub(crate) fn rs_open_record_store(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let name_ref = arg_ref(call, 0)?;
    if name_ref.is_null() {
        return Err(jl::throw_java(
            cs,
            heap,
            "java/lang/NullPointerException",
            Some("store name is null"),
        ));
    }
    let name = string_text(heap, name_ref)?;
    let create = arg_int(call, 1)? != 0;
    match host.rms_load(&name) {
        Some(bytes) => {
            // 预读校验：坏档在 open 即抛（而非推迟到首个操作）。
            StoreBlob::decode(&bytes).map_err(|reason| {
                throw_rms(
                    cs,
                    heap,
                    "javax/microedition/rms/RecordStoreException",
                    &format!("corrupt store blob: {reason}"),
                )
            })?;
            Ok(Some(Slot::Ref(alloc_store_handle(cs, heap, &name)?)))
        }
        None if create => {
            // 新空店即时落 host（version=0 / next_id=1 / 无记录）。
            let blob = StoreBlob::empty();
            save_store(host, &name, &blob);
            Ok(Some(Slot::Ref(alloc_store_handle(cs, heap, &name)?)))
        }
        None => Err(throw_rms(
            cs,
            heap,
            "javax/microedition/rms/RecordStoreNotFoundException",
            &name,
        )),
    }
}

/// deleteRecordStore(String)：不存在 → RecordStoreNotFoundException。
/// 注：已打开句柄不受影响（本引擎无强制关闭协议，后续操作按店缺失
/// 报 RecordStoreException，MIDP 对此本就是实现定义行为）。
pub(crate) fn rs_delete_record_store(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let name_ref = arg_ref(call, 0)?;
    if name_ref.is_null() {
        return Err(jl::throw_java(
            cs,
            heap,
            "java/lang/NullPointerException",
            Some("store name is null"),
        ));
    }
    let name = string_text(heap, name_ref)?;
    if host.rms_delete(&name) {
        Ok(None)
    } else {
        Err(throw_rms(
            cs,
            heap,
            "javax/microedition/rms/RecordStoreNotFoundException",
            &name,
        ))
    }
}

/// listRecordStores()：host 列表 → String[]；空表返回 null（MIDP 语义）。
pub(crate) fn rs_list_record_stores(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let names = host.rms_list();
    if names.is_empty() {
        return Ok(Some(Slot::Ref(ObjRef::NULL)));
    }
    let class = cs.load_class("[Ljava/lang/String;")?;
    let array = heap.alloc_array(class, ElemType::Ref, names.len())?;
    // 先分配全部字符串再写入槽位（避免 heap 双重可变借用）。
    let refs: Vec<ObjRef> = names
        .iter()
        .map(|name| heap.alloc_string_uninterned(utf16(name)))
        .collect::<Result<_, _>>()?;
    if let ArrayData::Refs(slots) = &mut heap.array_mut(array)?.data {
        slots.copy_from_slice(&refs);
    }
    Ok(Some(Slot::Ref(array)))
}

// ---------------------------------------------------------------------------
// 实例 API：记录增删改查
// ---------------------------------------------------------------------------

/// addRecord(byte[], int, int)：追加记录，返回新 id（不回收）。
pub(crate) fn rs_add_record(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = call.this.ok_or_else(|| KvmError::Internal("addRecord: no receiver".into()))?;
    let name = open_store_name(cs, heap, this)?;
    let data_ref = arg_ref(call, 0)?;
    let off = arg_int(call, 1)?;
    let len = arg_int(call, 2)?;
    let payload = record_slice(cs, heap, data_ref, off, len)?;
    let mut blob = load_store(cs, heap, host, &name)?;
    let id = blob.next_record_id;
    blob.next_record_id += 1;
    blob.version += 1;
    blob.last_modified_ms = host.now_ms() as i64;
    blob.records.push((id, payload));
    save_store(host, &name, &blob);
    Ok(Some(Slot::Int(id)))
}

/// setRecord(int, byte[], int, int)：整条覆写；id 不存在 →
/// InvalidRecordIDException。
pub(crate) fn rs_set_record(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = call.this.ok_or_else(|| KvmError::Internal("setRecord: no receiver".into()))?;
    let name = open_store_name(cs, heap, this)?;
    let id = arg_int(call, 0)?;
    let data_ref = arg_ref(call, 1)?;
    let off = arg_int(call, 2)?;
    let len = arg_int(call, 3)?;
    let payload = record_slice(cs, heap, data_ref, off, len)?;
    let mut blob = load_store(cs, heap, host, &name)?;
    let slot = blob.records.iter().position(|(rid, _)| *rid == id);
    match slot {
        Some(index) => {
            blob.records[index].1 = payload;
            blob.version += 1;
            blob.last_modified_ms = host.now_ms() as i64;
            save_store(host, &name, &blob);
            Ok(None)
        }
        None => Err(throw_rms(
            cs,
            heap,
            "javax/microedition/rms/InvalidRecordIDException",
            &format!("record {id}"),
        )),
    }
}

/// getRecord(int) → byte[]（新分配）；id 不存在 → InvalidRecordIDException。
pub(crate) fn rs_get_record(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = call.this.ok_or_else(|| KvmError::Internal("getRecord: no receiver".into()))?;
    let name = open_store_name(cs, heap, this)?;
    let id = arg_int(call, 0)?;
    let blob = load_store(cs, heap, host, &name)?;
    match blob.records.iter().find(|(rid, _)| *rid == id) {
        Some((_, data)) => Ok(Some(Slot::Ref(alloc_byte_array_from(cs, heap, data)?))),
        None => Err(throw_rms(
            cs,
            heap,
            "javax/microedition/rms/InvalidRecordIDException",
            &format!("record {id}"),
        )),
    }
}

/// getRecord(int, byte[]) 填充版：返回写入长度；buffer 太小 →
/// ArrayIndexOutOfBoundsException（javadoc 语义）。
pub(crate) fn rs_get_record_into(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = call.this.ok_or_else(|| KvmError::Internal("getRecord: no receiver".into()))?;
    let name = open_store_name(cs, heap, this)?;
    let id = arg_int(call, 0)?;
    let buffer_ref = arg_ref(call, 1)?;
    if buffer_ref.is_null() {
        return Err(jl::throw_java(
            cs,
            heap,
            "java/lang/NullPointerException",
            Some("buffer is null"),
        ));
    }
    let blob = load_store(cs, heap, host, &name)?;
    match blob.records.iter().find(|(rid, _)| *rid == id) {
        Some((_, data)) => {
            let size = heap.array(buffer_ref)?.data.len();
            if data.len() > size {
                return Err(jl::throw_java(
                    cs,
                    heap,
                    "java/lang/ArrayIndexOutOfBoundsException",
                    Some(&format!("record {} len {} > buffer {}", id, data.len(), size)),
                ));
            }
            if let ArrayData::Byte(target) = &mut heap.array_mut(buffer_ref)?.data {
                target[..data.len()].copy_from_slice(
                    &data.iter().map(|&b| b as i8).collect::<Vec<_>>(),
                );
            }
            Ok(Some(Slot::Int(data.len() as i32)))
        }
        None => Err(throw_rms(
            cs,
            heap,
            "javax/microedition/rms/InvalidRecordIDException",
            &format!("record {id}"),
        )),
    }
}

/// deleteRecord(int)：id 不存在 → InvalidRecordIDException。
pub(crate) fn rs_delete_record(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = call.this.ok_or_else(|| KvmError::Internal("deleteRecord: no receiver".into()))?;
    let name = open_store_name(cs, heap, this)?;
    let id = arg_int(call, 0)?;
    let mut blob = load_store(cs, heap, host, &name)?;
    let before = blob.records.len();
    blob.records.retain(|(rid, _)| *rid != id);
    if blob.records.len() == before {
        return Err(throw_rms(
            cs,
            heap,
            "javax/microedition/rms/InvalidRecordIDException",
            &format!("record {id}"),
        ));
    }
    blob.version += 1;
    blob.last_modified_ms = host.now_ms() as i64;
    save_store(host, &name, &blob);
    Ok(None)
}

// ---------------------------------------------------------------------------
// 实例 API：元数据
// ---------------------------------------------------------------------------

pub(crate) fn rs_get_num_records(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = call.this.ok_or_else(|| KvmError::Internal("getNumRecords: no receiver".into()))?;
    let name = open_store_name(cs, heap, this)?;
    let blob = load_store(cs, heap, host, &name)?;
    Ok(Some(Slot::Int(blob.records.len() as i32)))
}

/// getSize()：MIDP 实现定义，取记录数据字节和（不含 blob 开销）。
pub(crate) fn rs_get_size(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = call.this.ok_or_else(|| KvmError::Internal("getSize: no receiver".into()))?;
    let name = open_store_name(cs, heap, this)?;
    let blob = load_store(cs, heap, host, &name)?;
    Ok(Some(Slot::Int(blob.data_size())))
}

pub(crate) fn rs_get_next_record_id(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = call.this.ok_or_else(|| KvmError::Internal("getNextRecordID: no receiver".into()))?;
    let name = open_store_name(cs, heap, this)?;
    let blob = load_store(cs, heap, host, &name)?;
    Ok(Some(Slot::Int(blob.next_record_id)))
}

pub(crate) fn rs_get_last_modified(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this =
        call.this.ok_or_else(|| KvmError::Internal("getLastModified: no receiver".into()))?;
    let name = open_store_name(cs, heap, this)?;
    let blob = load_store(cs, heap, host, &name)?;
    Ok(Some(Slot::Long(blob.last_modified_ms)))
}

/// closeRecordStore()：清 open 标志（blob 已即时保存，无 flush 语义）；
/// 重复 close → RecordStoreException（MIDP 语义）。
pub(crate) fn rs_close_record_store(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    _host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this =
        call.this.ok_or_else(|| KvmError::Internal("closeRecordStore: no receiver".into()))?;
    let open_slot = field_slot(cs, STORE_CLASS, "open", "Z")?;
    if field_int(heap, this, open_slot)? == 0 {
        return Err(throw_rms(
            cs,
            heap,
            "javax/microedition/rms/RecordStoreException",
            "record store already closed",
        ));
    }
    set_field(heap, this, open_slot, Slot::Int(0))?;
    Ok(None)
}

pub(crate) fn rs_get_name(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    _host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = call.this.ok_or_else(|| KvmError::Internal("getName: no receiver".into()))?;
    let name = open_store_name(cs, heap, this)?;
    Ok(Some(Slot::Ref(heap.alloc_string_uninterned(utf16(&name))?)))
}

pub(crate) fn rs_get_version(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = call.this.ok_or_else(|| KvmError::Internal("getVersion: no receiver".into()))?;
    let name = open_store_name(cs, heap, this)?;
    let blob = load_store(cs, heap, host, &name)?;
    Ok(Some(Slot::Int(blob.version)))
}

/// setMode(int, boolean)：授权模式改写为 no-op（单租户引擎无 ACL 语义）。
pub(crate) fn rs_set_mode(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    _host: &mut dyn HostContext,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(None)
}

/// getMode()：恒返回 AUTHMODE_PRIVATE(0)（setMode no-op 的对偶）。
pub(crate) fn rs_get_mode(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    _host: &mut dyn HostContext,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Int(0)))
}

/// enumerateRecords(filter, comparator, keepUpdated)：返回 RecordEnum
/// （ids 快照 + 游标）。filter/comparator 非 null 按无过滤降级
/// （回调需解释器重入，TODO 后续任务）；keepUpdated 忽略（快照语义）。
pub(crate) fn rs_enumerate_records(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this =
        call.this.ok_or_else(|| KvmError::Internal("enumerateRecords: no receiver".into()))?;
    let name = open_store_name(cs, heap, this)?;
    let filter_ref = arg_ref(call, 0)?;
    let comparator_ref = arg_ref(call, 1)?;
    if (!filter_ref.is_null() || !comparator_ref.is_null())
        && !DEGRADE_NOTIFIED.swap(true, Ordering::Relaxed)
    {
        eprintln!(
            "[rms] enumerateRecords: filter/comparator 非 null，降级为无过滤/原始序（重入回调待后续任务）"
        );
    }
    let blob = load_store(cs, heap, host, &name)?;
    let ids: Vec<i32> = blob.records.iter().map(|(id, _)| *id).collect();
    let handle = alloc_instance_of(cs, heap, ENUM_CLASS)?;
    let name_slot = field_slot(cs, ENUM_CLASS, "storeName", "Ljava/lang/String;")?;
    let ids_slot = field_slot(cs, ENUM_CLASS, "ids", "[I")?;
    let pos_slot = field_slot(cs, ENUM_CLASS, "pos", "I")?;
    let name_ref = heap.alloc_string_uninterned(utf16(&name))?;
    let ids_ref = alloc_int_array_from(cs, heap, &ids)?;
    set_field(heap, handle, name_slot, Slot::Ref(name_ref))?;
    set_field(heap, handle, ids_slot, Slot::Ref(ids_ref))?;
    set_field(heap, handle, pos_slot, Slot::Int(0))?;
    Ok(Some(Slot::Ref(handle)))
}

// ---------------------------------------------------------------------------
// RecordEnum natives（接口 RecordEnumeration 的内部实现类）
// ---------------------------------------------------------------------------

/// 读 RecordEnum 的店名 / ids / pos 三元组。
fn enum_state(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    this: ObjRef,
) -> Result<(String, Vec<i32>, i32), KvmError> {
    let name_slot = field_slot(cs, ENUM_CLASS, "storeName", "Ljava/lang/String;")?;
    let ids_slot = field_slot(cs, ENUM_CLASS, "ids", "[I")?;
    let pos_slot = field_slot(cs, ENUM_CLASS, "pos", "I")?;
    let name = string_text(heap, field_ref(heap, this, name_slot)?)?;
    let ids = match &heap.array(field_ref(heap, this, ids_slot)?)?.data {
        ArrayData::Int(data) => data.clone(),
        other => {
            return Err(KvmError::Internal(format!(
                "RecordEnum.ids expected int[], got {other:?}"
            )))
        }
    };
    let pos = field_int(heap, this, pos_slot)?;
    Ok((name, ids, pos))
}

/// nextRecord/previousRecord 共用：按 id 即时查店 blob 取记录字节。
fn record_by_id(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    store: &str,
    id: i32,
) -> Result<ObjRef, KvmError> {
    // 店可能在快照后被删/记录被删：按无此 id 处理。
    let blob = load_store(cs, heap, host, store)?;
    match blob.records.iter().find(|(rid, _)| *rid == id) {
        Some((_, data)) => alloc_byte_array_from(cs, heap, data),
        None => Err(throw_rms(
            cs,
            heap,
            "javax/microedition/rms/InvalidRecordIDException",
            &format!("record {id} gone"),
        )),
    }
}

pub(crate) fn enum_has_next(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    _host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = call.this.ok_or_else(|| KvmError::Internal("hasNextElement: no receiver".into()))?;
    let (_, ids, pos) = enum_state(cs, heap, this)?;
    Ok(Some(Slot::Int((pos >= 0 && (pos as usize) < ids.len()) as i32)))
}

pub(crate) fn enum_next_record_id(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    _host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this =
        call.this.ok_or_else(|| KvmError::Internal("nextRecordId: no receiver".into()))?;
    let (_, ids, pos) = enum_state(cs, heap, this)?;
    if pos < 0 || pos as usize >= ids.len() {
        return Err(throw_rms(
            cs,
            heap,
            "javax/microedition/rms/InvalidRecordIDException",
            "no more records",
        ));
    }
    let id = ids[pos as usize];
    // 推进游标（nextRecordId 也前进，对齐 KVM 参考实现）。
    let pos_slot = field_slot(cs, ENUM_CLASS, "pos", "I")?;
    set_field(heap, this, pos_slot, Slot::Int(pos + 1))?;
    Ok(Some(Slot::Int(id)))
}

pub(crate) fn enum_next_record(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = call.this.ok_or_else(|| KvmError::Internal("nextRecord: no receiver".into()))?;
    let (store, ids, pos) = enum_state(cs, heap, this)?;
    if pos < 0 || pos as usize >= ids.len() {
        return Err(throw_rms(
            cs,
            heap,
            "javax/microedition/rms/InvalidRecordIDException",
            "no more records",
        ));
    }
    let id = ids[pos as usize];
    let data = record_by_id(cs, heap, host, &store, id)?;
    let pos_slot = field_slot(cs, ENUM_CLASS, "pos", "I")?;
    set_field(heap, this, pos_slot, Slot::Int(pos + 1))?;
    Ok(Some(Slot::Ref(data)))
}

pub(crate) fn enum_reset(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    _host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = call.this.ok_or_else(|| KvmError::Internal("reset: no receiver".into()))?;
    let pos_slot = field_slot(cs, ENUM_CLASS, "pos", "I")?;
    set_field(heap, this, pos_slot, Slot::Int(0))?;
    Ok(None)
}

/// destroy()：清空 ids 快照 + 归零游标（之后的 hasNext 恒 false）。
pub(crate) fn enum_destroy(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    _host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = call.this.ok_or_else(|| KvmError::Internal("destroy: no receiver".into()))?;
    let ids_slot = field_slot(cs, ENUM_CLASS, "ids", "[I")?;
    let pos_slot = field_slot(cs, ENUM_CLASS, "pos", "I")?;
    let empty = alloc_int_array_from(cs, heap, &[])?;
    set_field(heap, this, ids_slot, Slot::Ref(empty))?;
    set_field(heap, this, pos_slot, Slot::Int(0))?;
    Ok(None)
}

pub(crate) fn enum_has_previous(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    _host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this =
        call.this.ok_or_else(|| KvmError::Internal("hasPreviousElement: no receiver".into()))?;
    let (_, _, pos) = enum_state(cs, heap, this)?;
    Ok(Some(Slot::Int((pos > 0) as i32)))
}

pub(crate) fn enum_previous_record(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this =
        call.this.ok_or_else(|| KvmError::Internal("previousRecord: no receiver".into()))?;
    let (store, ids, pos) = enum_state(cs, heap, this)?;
    if pos <= 0 {
        return Err(throw_rms(
            cs,
            heap,
            "javax/microedition/rms/InvalidRecordIDException",
            "no previous record",
        ));
    }
    let id = ids[pos as usize - 1];
    let data = record_by_id(cs, heap, host, &store, id)?;
    let pos_slot = field_slot(cs, ENUM_CLASS, "pos", "I")?;
    set_field(heap, this, pos_slot, Slot::Int(pos - 1))?;
    Ok(Some(Slot::Ref(data)))
}
