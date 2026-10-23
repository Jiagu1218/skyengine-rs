//! javax.microedition.io（GCF / JSR-118 通用连接框架，T041）：
//! `Connector` 工厂 + `HttpConnection`/`HttpConnectionImpl` 懒连接实现。
//!
//! ## 基线签名核对（jar-analysis-full.txt 已随临时目录清理，按
//! MIDP-2.0 规范名注册；媒体侧 T038 已引用的
//! `javax/io/ConnectionNotFoundException` 沿用同一类名，避免并存
//! 两个"连接未找到"异常类）
//!
//! - `Connector`：`javax/microedition/io/Connector`（规范位置）；
//!   open(String) / open(String,int) / open(String,int,boolean) 三重载
//!   + openInputStream/openDataInputStream/openDataOutputStream(String)
//!   便捷法 + READ/WRITE/READ_WRITE 模式常量。
//! - `HttpConnection` 接口：`javax/microedition/io/HttpConnection`
//!   extends ContentConnection extends StreamConnection extends
//!   InputConnection + OutputConnection extends Connection；常量与方法
//!   签名按规范注册（GET/HEAD/POST、HTTP_* 状态码、默认端口）。
//! - 具体实现类：`javax/microedition/io/HttpConnectionImpl`
//!   （bootstrap 类兼作实现，Player/VolumnControlImpl 同模式）；
//!   请求体缓冲类 `javax/microedition/io/HttpOutputStream`。
//! - 不支持面：socket:// ssl:// comm:// 及未知协议 →
//!   `javax/io/ConnectionNotFoundException`（与 T038 媒体 locator
//!   分支同一异常类）。
//!
//! ## 状态机（MIDP HttpConnection javadoc）
//!
//! `state` 实例字段：0=setup / 1=connected / 2=closed。
//! - connect 懒触发：`openInputStream`/`getResponseCode`/
//!   `getResponseMessage`/响应头读取在 setup 态自动发起请求（经
//!   HostContext::http_request，D6 唯一通道）；
//! - setup 态才能 `setRequestMethod`/`setRequestProperty`（connected
//!   后抛 IllegalStateException——简化：规范对 connected 后改参数
//!   未定死异常类型，按项目 media 先例统一 ISE）；
//! - `openOutputStream` 返回 `HttpOutputStream`（写内部缓冲），
//!   flush/close 把缓冲固化进连接的 `bodyOut`；connect 时作请求体
//!   （GET 带体按实测宽容透传——斯凯网关见过非常规 GET 载荷）；
//! - connect 失败：连接置 closed，HostNetError 映射：
//!   Unsupported→ConnectionNotFoundException / Timeout→
//!   java/io/InterruptedIOException / Io(msg)→java/io/IOException。
//!
//! ## 头存储
//!
//! 请求/响应头各用平行 String 数组 + count 字段（`[Ljava/lang/String;`，
//! 满翻倍扩容——listeners 数组同模式），保持插入序供
//! getHeaderFieldKey/getHeaderField(int) 按序遍历。

use crate::classfile::member::acc;
use crate::cldc::io as jio;
use crate::cldc::lang as jl;
use crate::host::{HostContext, HostNetError, HttpRequest, HttpResponse};
use crate::interp::natives::{
    BootstrapConst, BootstrapDecl, NativeFn, field, m, mh, stub_null, stub_void,
};
use crate::interp::NativeCall;
use crate::runtime::classspace::ClassSpace;
use crate::runtime::heap::Heap;
use crate::runtime::object::{ArrayData, ElemType};
use crate::runtime::{ObjRef, Slot};
use crate::KvmError;

/// GCF 工厂类（规范位置）。
const CONNECTOR_CLASS: &str = "javax/microedition/io/Connector";
/// HttpConnection 接口。
const HTTP_CONNECTION_IFACE: &str = "javax/microedition/io/HttpConnection";
/// HttpConnection 具体实现（bootstrap 类兼作实现，同 Player 模式）。
const HCIMPL_CLASS: &str = "javax/microedition/io/HttpConnectionImpl";
/// 请求体缓冲输出流（bootstrap 类）。
const HOUT_CLASS: &str = "javax/microedition/io/HttpOutputStream";
/// 请求体内部缓冲复用 ByteArrayOutputStream（字段布局兼容）。
const BAOS_CLASS: &str = "java/io/ByteArrayOutputStream";

/// 连接状态（实例字段 state 的值域）。
const STATE_SETUP: i32 = 0;
const STATE_CONNECTED: i32 = 1;
const STATE_CLOSED: i32 = 2;

/// 默认请求超时（MIDP 无 per-request 超时设置面；取引擎侧惯例 30s）。
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// 协议默认端口（HttpConnection 常量同值）。
const HTTP_DEFAULT_PORT: i32 = 80;
const HTTPS_DEFAULT_PORT: i32 = 443;

// ---------------------------------------------------------------------------
// bootstrap 声明
// ---------------------------------------------------------------------------

/// Connection：GCF 根标记接口。
pub(crate) static CONNECTION_IFACE_DECL: BootstrapDecl = BootstrapDecl {
    name: "javax/microedition/io/Connection",
    access_flags: acc::PUBLIC | acc::INTERFACE,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[],
    methods: &[],
};

/// InputConnection：流打开面。
pub(crate) static INPUT_CONNECTION_IFACE_DECL: BootstrapDecl = BootstrapDecl {
    name: "javax/microedition/io/InputConnection",
    access_flags: acc::PUBLIC | acc::INTERFACE,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[],
    methods: &[
        m(
            "openDataInputStream",
            "()Ljava/io/DataInputStream;",
            acc::PUBLIC | acc::ABSTRACT,
            stub_null,
        ),
        m(
            "openInputStream",
            "()Ljava/io/InputStream;",
            acc::PUBLIC | acc::ABSTRACT,
            stub_null,
        ),
    ],
};

/// OutputConnection：流打开面。
pub(crate) static OUTPUT_CONNECTION_IFACE_DECL: BootstrapDecl = BootstrapDecl {
    name: "javax/microedition/io/OutputConnection",
    access_flags: acc::PUBLIC | acc::INTERFACE,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[],
    methods: &[
        m(
            "openDataOutputStream",
            "()Ljava/io/DataOutputStream;",
            acc::PUBLIC | acc::ABSTRACT,
            stub_null,
        ),
        m(
            "openOutputStream",
            "()Ljava/io/OutputStream;",
            acc::PUBLIC | acc::ABSTRACT,
            stub_null,
        ),
    ],
};

/// StreamConnection = Input + Output（规范继承面）。
pub(crate) static STREAM_CONNECTION_IFACE_DECL: BootstrapDecl = BootstrapDecl {
    name: "javax/microedition/io/StreamConnection",
    access_flags: acc::PUBLIC | acc::INTERFACE,
    super_name: Some("java/lang/Object"),
    interfaces: &[
        "javax/microedition/io/InputConnection",
        "javax/microedition/io/OutputConnection",
    ],
    fields: &[],
    methods: &[],
};

/// ContentConnection：内容元数据面。
pub(crate) static CONTENT_CONNECTION_IFACE_DECL: BootstrapDecl = BootstrapDecl {
    name: "javax/microedition/io/ContentConnection",
    access_flags: acc::PUBLIC | acc::INTERFACE,
    super_name: Some("java/lang/Object"),
    interfaces: &["javax/microedition/io/StreamConnection"],
    fields: &[],
    methods: &[
        m(
            "getEncoding",
            "()Ljava/lang/String;",
            acc::PUBLIC | acc::ABSTRACT,
            stub_null,
        ),
        m("getLength", "()J", acc::PUBLIC | acc::ABSTRACT, stub_null),
        m(
            "getType",
            "()Ljava/lang/String;",
            acc::PUBLIC | acc::ABSTRACT,
            stub_null,
        ),
    ],
};

/// HttpConnection 接口：方法全签名占位（真实现归 HttpConnectionImpl）；
/// 常量值按 MIDP-2.0 javadoc（GET=1/HEAD=2/POST=3 与 HTTP_* 状态码）。
pub(crate) static HTTP_CONNECTION_IFACE_DECL: BootstrapDecl = BootstrapDecl {
    name: HTTP_CONNECTION_IFACE,
    access_flags: acc::PUBLIC | acc::INTERFACE,
    super_name: Some("java/lang/Object"),
    interfaces: &["javax/microedition/io/ContentConnection"],
    fields: &[
        const_int_field("GET", 1),
        const_int_field("HEAD", 2),
        const_int_field("POST", 3),
        const_int_field("HTTP_DEFAULT_PORT", HTTP_DEFAULT_PORT),
        const_int_field("HTTPS_DEFAULT_PORT", HTTPS_DEFAULT_PORT),
        const_int_field("HTTP_OK", 200),
        const_int_field("HTTP_REDIRECT", 302),
        const_int_field("HTTP_BAD_REQUEST", 400),
        const_int_field("HTTP_UNAUTHORIZED", 401),
        const_int_field("HTTP_FORBIDDEN", 403),
        const_int_field("HTTP_NOT_FOUND", 404),
        const_int_field("HTTP_SERVER_ERROR", 500),
        const_int_field("HTTP_UNAVAILABLE", 503),
    ],
    methods: &[
        m(
            "getURL",
            "()Ljava/lang/String;",
            acc::PUBLIC | acc::ABSTRACT,
            stub_null,
        ),
        m(
            "getProtocol",
            "()Ljava/lang/String;",
            acc::PUBLIC | acc::ABSTRACT,
            stub_null,
        ),
        m(
            "getHost",
            "()Ljava/lang/String;",
            acc::PUBLIC | acc::ABSTRACT,
            stub_null,
        ),
        m("getFile", "()Ljava/lang/String;", acc::PUBLIC | acc::ABSTRACT, stub_null),
        m("getPort", "()I", acc::PUBLIC | acc::ABSTRACT, stub_null),
        m(
            "getQuery",
            "()Ljava/lang/String;",
            acc::PUBLIC | acc::ABSTRACT,
            stub_null,
        ),
        m("getRef", "()Ljava/lang/String;", acc::PUBLIC | acc::ABSTRACT, stub_null),
        m(
            "setRequestMethod",
            "(Ljava/lang/String;)V",
            acc::PUBLIC | acc::ABSTRACT,
            stub_void,
        ),
        m(
            "getRequestMethod",
            "()Ljava/lang/String;",
            acc::PUBLIC | acc::ABSTRACT,
            stub_null,
        ),
        m(
            "setRequestProperty",
            "(Ljava/lang/String;Ljava/lang/String;)V",
            acc::PUBLIC | acc::ABSTRACT,
            stub_void,
        ),
        m(
            "getRequestProperty",
            "(Ljava/lang/String;)Ljava/lang/String;",
            acc::PUBLIC | acc::ABSTRACT,
            stub_null,
        ),
        m(
            "getResponseCode",
            "()I",
            acc::PUBLIC | acc::ABSTRACT,
            stub_null,
        ),
        m(
            "getResponseMessage",
            "()Ljava/lang/String;",
            acc::PUBLIC | acc::ABSTRACT,
            stub_null,
        ),
        m(
            "getHeaderField",
            "(Ljava/lang/String;)Ljava/lang/String;",
            acc::PUBLIC | acc::ABSTRACT,
            stub_null,
        ),
        m(
            "getHeaderFieldInt",
            "(Ljava/lang/String;I)I",
            acc::PUBLIC | acc::ABSTRACT,
            stub_null,
        ),
        m(
            "getHeaderFieldDate",
            "(Ljava/lang/String;J)J",
            acc::PUBLIC | acc::ABSTRACT,
            stub_null,
        ),
        m(
            "getHeaderField",
            "(I)Ljava/lang/String;",
            acc::PUBLIC | acc::ABSTRACT,
            stub_null,
        ),
        m(
            "getHeaderFieldKey",
            "(I)Ljava/lang/String;",
            acc::PUBLIC | acc::ABSTRACT,
            stub_null,
        ),
        m(
            "getExpiration",
            "()J",
            acc::PUBLIC | acc::ABSTRACT,
            stub_null,
        ),
        m("getDate", "()J", acc::PUBLIC | acc::ABSTRACT, stub_null),
        m(
            "getLastModified",
            "()J",
            acc::PUBLIC | acc::ABSTRACT,
            stub_null,
        ),
    ],
};

/// 带静态 int 常量初值的字段速记（natives::const_field 私有，此处平替）。
const fn const_int_field(name: &'static str, value: i32) -> crate::interp::natives::BootstrapFieldDecl {
    crate::interp::natives::BootstrapFieldDecl {
        name,
        desc: "I",
        access_flags: acc::PUBLIC | acc::STATIC | acc::FINAL,
        constant: Some(BootstrapConst::Int(value)),
    }
}

/// Connector 工厂（全静态）。
pub(crate) static CONNECTOR_DECL: BootstrapDecl = BootstrapDecl {
    name: CONNECTOR_CLASS,
    access_flags: acc::PUBLIC | acc::FINAL,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[
        const_int_field("READ", 1),
        const_int_field("WRITE", 2),
        const_int_field("READ_WRITE", 3),
    ],
    methods: &[
        m(
            "open",
            "(Ljava/lang/String;)Ljavax/microedition/io/Connection;",
            acc::PUBLIC | acc::STATIC,
            connector_open,
        ),
        m(
            "open",
            "(Ljava/lang/String;I)Ljavax/microedition/io/Connection;",
            acc::PUBLIC | acc::STATIC,
            connector_open_mode,
        ),
        m(
            "open",
            "(Ljava/lang/String;IZ)Ljavax/microedition/io/Connection;",
            acc::PUBLIC | acc::STATIC,
            connector_open_mode,
        ),
        mh(
            "openInputStream",
            "(Ljava/lang/String;)Ljava/io/InputStream;",
            acc::PUBLIC | acc::STATIC,
            connector_open_input_stream,
        ),
        mh(
            "openDataInputStream",
            "(Ljava/lang/String;)Ljava/io/DataInputStream;",
            acc::PUBLIC | acc::STATIC,
            connector_open_data_input_stream,
        ),
        mh(
            "openDataOutputStream",
            "(Ljava/lang/String;)Ljava/io/DataOutputStream;",
            acc::PUBLIC | acc::STATIC,
            connector_open_data_output_stream,
        ),
    ],
};

/// HttpConnectionImpl：字段布局即状态机载体（slot 序 = 声明序）。
pub(crate) static HTTP_CONNECTION_IMPL_DECL: BootstrapDecl = BootstrapDecl {
    name: HCIMPL_CLASS,
    access_flags: acc::PUBLIC,
    super_name: Some("java/lang/Object"),
    interfaces: &[HTTP_CONNECTION_IFACE],
    fields: &[
        // setup 阶段可变
        field("url", "Ljava/lang/String;", acc::PRIVATE),
        field("method", "Ljava/lang/String;", acc::PRIVATE),
        field("reqNames", "[Ljava/lang/String;", acc::PRIVATE),
        field("reqValues", "[Ljava/lang/String;", acc::PRIVATE),
        field("reqCount", "I", acc::PRIVATE),
        field("bodyOut", "[B", acc::PRIVATE),
        // 状态与响应（connect 后只读）
        field("state", "I", acc::PRIVATE),
        field("status", "I", acc::PRIVATE),
        field("respMsg", "Ljava/lang/String;", acc::PRIVATE),
        field("respNames", "[Ljava/lang/String;", acc::PRIVATE),
        field("respValues", "[Ljava/lang/String;", acc::PRIVATE),
        field("respCount", "I", acc::PRIVATE),
        field("body", "[B", acc::PRIVATE),
    ],
    methods: &[
        m("<init>", "(Ljava/lang/String;)V", acc::PUBLIC, hc_init),
        m(
            "setRequestMethod",
            "(Ljava/lang/String;)V",
            acc::PUBLIC,
            hc_set_request_method,
        ),
        m(
            "getRequestMethod",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            hc_get_request_method,
        ),
        m(
            "setRequestProperty",
            "(Ljava/lang/String;Ljava/lang/String;)V",
            acc::PUBLIC,
            hc_set_request_property,
        ),
        m(
            "getRequestProperty",
            "(Ljava/lang/String;)Ljava/lang/String;",
            acc::PUBLIC,
            hc_get_request_property,
        ),
        m("getURL", "()Ljava/lang/String;", acc::PUBLIC, hc_get_url),
        m(
            "getProtocol",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            hc_get_protocol,
        ),
        m("getHost", "()Ljava/lang/String;", acc::PUBLIC, hc_get_host),
        m("getPort", "()I", acc::PUBLIC, hc_get_port),
        m("getFile", "()Ljava/lang/String;", acc::PUBLIC, hc_get_file),
        m("getQuery", "()Ljava/lang/String;", acc::PUBLIC, hc_get_query),
        m("getRef", "()Ljava/lang/String;", acc::PUBLIC, hc_get_ref),
        mh(
            "openOutputStream",
            "()Ljava/io/OutputStream;",
            acc::PUBLIC,
            hc_open_output_stream,
        ),
        mh(
            "openDataOutputStream",
            "()Ljava/io/DataOutputStream;",
            acc::PUBLIC,
            hc_open_data_output_stream,
        ),
        mh(
            "openInputStream",
            "()Ljava/io/InputStream;",
            acc::PUBLIC,
            hc_open_input_stream,
        ),
        mh(
            "openDataInputStream",
            "()Ljava/io/DataInputStream;",
            acc::PUBLIC,
            hc_open_data_input_stream,
        ),
        mh("getResponseCode", "()I", acc::PUBLIC, hc_get_response_code),
        mh(
            "getResponseMessage",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            hc_get_response_message,
        ),
        mh(
            "getHeaderField",
            "(Ljava/lang/String;)Ljava/lang/String;",
            acc::PUBLIC,
            hc_get_header_field_named,
        ),
        mh(
            "getHeaderFieldInt",
            "(Ljava/lang/String;I)I",
            acc::PUBLIC,
            hc_get_header_field_int,
        ),
        m(
            "getHeaderFieldDate",
            "(Ljava/lang/String;J)J",
            acc::PUBLIC,
            hc_get_header_field_date,
        ),
        mh(
            "getHeaderField",
            "(I)Ljava/lang/String;",
            acc::PUBLIC,
            hc_get_header_field_indexed,
        ),
        mh(
            "getHeaderFieldKey",
            "(I)Ljava/lang/String;",
            acc::PUBLIC,
            hc_get_header_field_key,
        ),
        mh("getLength", "()J", acc::PUBLIC, hc_get_length),
        mh("getType", "()Ljava/lang/String;", acc::PUBLIC, hc_get_type),
        mh(
            "getEncoding",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            hc_get_encoding,
        ),
        m("getExpiration", "()J", acc::PUBLIC, hc_get_expiration),
        m("getDate", "()J", acc::PUBLIC, hc_get_date),
        m("getLastModified", "()J", acc::PUBLIC, hc_get_last_modified),
        m("close", "()V", acc::PUBLIC, hc_close),
    ],
};

/// HttpOutputStream：写经内部 ByteArrayOutputStream 缓冲，
/// flush/close 把缓冲固化进所属连接的 bodyOut。
pub(crate) static HTTP_OUTPUT_STREAM_DECL: BootstrapDecl = BootstrapDecl {
    name: HOUT_CLASS,
    access_flags: acc::PUBLIC,
    super_name: Some("java/io/OutputStream"),
    interfaces: &[],
    fields: &[
        field(
            "conn",
            "Ljavax/microedition/io/HttpConnectionImpl;",
            acc::PRIVATE,
        ),
        field("baos", "Ljava/io/ByteArrayOutputStream;", acc::PRIVATE),
        field("flushed", "Z", acc::PRIVATE),
    ],
    methods: &[
        m(
            "<init>",
            "(Ljavax/microedition/io/HttpConnectionImpl;)V",
            acc::PUBLIC,
            ho_init,
        ),
        m("write", "(I)V", acc::PUBLIC, ho_write_int),
        m("write", "([B)V", acc::PUBLIC, ho_write_all),
        m("write", "([BII)V", acc::PUBLIC, ho_write_range),
        m("flush", "()V", acc::PUBLIC, ho_flush),
        m("close", "()V", acc::PUBLIC, ho_close),
    ],
};

// ---------------------------------------------------------------------------
// 私有辅助（与 media.rs 同款：各模块私有是项目惯例）
// ---------------------------------------------------------------------------

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

fn string_text(heap: &Heap, reference: ObjRef) -> Result<String, KvmError> {
    let chars = heap.str_obj(reference)?.chars.clone();
    Ok(String::from_utf16_lossy(&chars))
}

fn utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

fn this_ref(call: &NativeCall, who: &str) -> Result<ObjRef, KvmError> {
    call.this
        .ok_or_else(|| KvmError::Internal(format!("{who}: no receiver")))
}

fn alloc_string(heap: &mut Heap, text: &str) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Ref(heap.alloc_string_uninterned(utf16(text))?)))
}

fn throw_illegal_state(cs: &mut ClassSpace, heap: &mut Heap, message: &str) -> KvmError {
    jl::throw_java(cs, heap, "java/lang/IllegalStateException", Some(message))
}

fn throw_npe(cs: &mut ClassSpace, heap: &mut Heap, message: &str) -> KvmError {
    jl::throw_java(cs, heap, "java/lang/NullPointerException", Some(message))
}

/// HostNetError → Java 异常（connect 失败路径统一映射）。
fn throw_net_error(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    error: HostNetError,
) -> KvmError {
    match error {
        // 平台无网络能力/协议不受支持：GCF 语义归连接未找到。
        HostNetError::Unsupported => jl::throw_java(
            cs,
            heap,
            "javax/io/ConnectionNotFoundException",
            Some("network unsupported by host"),
        ),
        // 超时：CLDC 惯例 InterruptedIOException（extends IOException）。
        HostNetError::Timeout => {
            jl::throw_java(cs, heap, "java/io/InterruptedIOException", Some("timeout"))
        }
        // 传输/IO 故障：IOException。
        HostNetError::Io(message) => {
            jl::throw_java(cs, heap, "java/io/IOException", Some(&message))
        }
    }
}

/// byte[] 载荷 → Vec<u8>。
fn byte_vec(heap: &Heap, array: ObjRef) -> Result<Vec<u8>, KvmError> {
    match &heap.array(array)?.data {
        ArrayData::Byte(bytes) => Ok(bytes.iter().map(|byte| *byte as u8).collect()),
        other => Err(KvmError::Internal(format!(
            "expected byte array, got {other:?}"
        ))),
    }
}

/// 引用数组槽位克隆（元素为 ObjRef）。
fn ref_slots(heap: &Heap, array: ObjRef) -> Result<Vec<ObjRef>, KvmError> {
    match &heap.array(array)?.data {
        ArrayData::Refs(slots) => Ok(slots.clone()),
        other => Err(KvmError::Internal(format!(
            "expected ref array, got {other:?}"
        ))),
    }
}

/// String 数组元素文本（null 槽返回 None）。
fn string_at(heap: &Heap, refs: &[ObjRef], index: usize) -> Result<Option<String>, KvmError> {
    match refs.get(index) {
        Some(reference) if !reference.is_null() => Ok(Some(string_text(heap, *reference)?)),
        _ => Ok(None),
    }
}

/// 头查找（名字大小写不敏感；HTTP 头名语义不区分大小写）。
fn find_header(
    heap: &Heap,
    names: &[ObjRef],
    name: &str,
) -> Result<Option<usize>, KvmError> {
    for (index, reference) in names.iter().enumerate() {
        if !reference.is_null()
            && string_text(heap, *reference)?.eq_ignore_ascii_case(name)
        {
            return Ok(Some(index));
        }
    }
    Ok(None)
}

/// 追加/覆盖请求头：同名覆盖值（MIDP setRequestProperty 语义），
/// 新名则尾插（数组惰性分配 4 槽，满翻倍——listeners 同模式）。
fn push_header(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    this: ObjRef,
    name: &str,
    value: &str,
) -> Result<(), KvmError> {
    let names_slot = field_slot(cs, HCIMPL_CLASS, "reqNames", "[Ljava/lang/String;")?;
    let values_slot = field_slot(cs, HCIMPL_CLASS, "reqValues", "[Ljava/lang/String;")?;
    let count_slot = field_slot(cs, HCIMPL_CLASS, "reqCount", "I")?;
    let names_ref = field_ref(heap, this, names_slot)?;
    // 同名覆盖。
    if !names_ref.is_null() {
        let slots = ref_slots(heap, names_ref)?;
        if let Some(index) = find_header(heap, &slots, name)? {
            let value_ref = heap.alloc_string_uninterned(utf16(value))?;
            let values_ref = field_ref(heap, this, values_slot)?;
            if let ArrayData::Refs(values) = &mut heap.array_mut(values_ref)?.data {
                values[index] = value_ref;
            }
            return Ok(());
        }
    }
    // 尾插（含扩容）。
    let count = field_int(heap, this, count_slot)? as usize;
    let capacity = if names_ref.is_null() {
        0
    } else {
        heap.array(names_ref)?.data.len()
    };
    if count == capacity {
        let grown = if capacity == 0 { 4 } else { capacity * 2 };
        let class = cs.load_class("[Ljava/lang/String;")?;
        let new_names = heap.alloc_array(class, ElemType::Ref, grown)?;
        let new_values = heap.alloc_array(class, ElemType::Ref, grown)?;
        if !names_ref.is_null() {
            let old = ref_slots(heap, names_ref)?;
            if let ArrayData::Refs(target) = &mut heap.array_mut(new_names)?.data {
                target[..old.len()].clone_from_slice(&old);
            }
            let values_ref = field_ref(heap, this, values_slot)?;
            let old_values = ref_slots(heap, values_ref)?;
            if let ArrayData::Refs(target) = &mut heap.array_mut(new_values)?.data {
                target[..old_values.len()].clone_from_slice(&old_values);
            }
        }
        set_field(heap, this, names_slot, Slot::Ref(new_names))?;
        set_field(heap, this, values_slot, Slot::Ref(new_values))?;
    }
    let names_ref = field_ref(heap, this, names_slot)?;
    let values_ref = field_ref(heap, this, values_slot)?;
    let name_ref = heap.alloc_string_uninterned(utf16(name))?;
    let value_ref = heap.alloc_string_uninterned(utf16(value))?;
    if let ArrayData::Refs(names) = &mut heap.array_mut(names_ref)?.data {
        names[count] = name_ref;
    }
    if let ArrayData::Refs(values) = &mut heap.array_mut(values_ref)?.data {
        values[count] = value_ref;
    }
    set_field(heap, this, count_slot, Slot::Int(count as i32 + 1))?;
    Ok(())
}

/// URL 拆解结果（Rust 侧解析，供 URL 族 getter 共用）。
struct UrlParts {
    protocol: String,
    host: String,
    port: Option<i32>,
    /// path+query（含 '?'；空路径归 "/"）。
    file: String,
    query: Option<String>,
    reference: Option<String>,
}

/// "scheme://authority/path?query#ref" 拆解（宽容：畸形输入按缺省段处理）。
fn split_url(url: &str) -> Option<UrlParts> {
    let (protocol, rest) = url.split_once("://")?;
    let (authority, file) = match rest.find(['/', '?', '#']) {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, ""),
    };
    // authority 中最后一个 ':' 分 host/port（host 侧再含 ':' 视为畸形）。
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port.parse::<i32>().ok()),
        None => (authority, None),
    };
    let (file, reference) = match file.split_once('#') {
        Some((head, reference)) => (head, Some(reference.to_string())),
        None => (file, None),
    };
    let (file, query) = match file.split_once('?') {
        Some((path, query)) => (path, Some(query.to_string())),
        None => (file, None),
    };
    let file = if file.is_empty() { "/".to_string() } else { file.to_string() };
    Some(UrlParts {
        protocol: protocol.to_ascii_lowercase(),
        host: host.to_string(),
        port,
        file,
        query,
        reference,
    })
}

/// 读连接 url 字段的拆解结果。
fn url_parts_of(
    cs: &mut ClassSpace,
    heap: &Heap,
    this: ObjRef,
) -> Result<UrlParts, KvmError> {
    let slot = field_slot(cs, HCIMPL_CLASS, "url", "Ljava/lang/String;")?;
    let url = string_text(heap, field_ref(heap, this, slot)?)?;
    split_url(&url).ok_or_else(|| {
        KvmError::Internal(format!("HttpConnectionImpl url unparsable: {url}"))
    })
}

/// 标准理由短语（HttpResponse 不携带 message 字段，按状态码合成；
/// 未收录返回 None → getResponseMessage 返 null，javadoc 允许）。
fn reason_phrase(status: i32) -> Option<&'static str> {
    Some(match status {
        100 => "Continue",
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// 连接装配与 connect 核心
// ---------------------------------------------------------------------------

/// 构造 HttpConnectionImpl 并写齐字段初值（open 专用，不走 <init>，
/// 与 alloc_player 同模式）。调用方保证 url 非 null 的 http(s) 链接。
fn alloc_http_conn(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    url: &str,
) -> Result<ObjRef, KvmError> {
    let conn = alloc_instance_of(cs, heap, HCIMPL_CLASS)?;
    let url_slot = field_slot(cs, HCIMPL_CLASS, "url", "Ljava/lang/String;")?;
    let method_slot = field_slot(cs, HCIMPL_CLASS, "method", "Ljava/lang/String;")?;
    let state_slot = field_slot(cs, HCIMPL_CLASS, "state", "I")?;
    let status_slot = field_slot(cs, HCIMPL_CLASS, "status", "I")?;
    let url_ref = heap.alloc_string_uninterned(utf16(url))?;
    let method_ref = heap.alloc_string_uninterned(utf16("GET"))?;
    set_field(heap, conn, url_slot, Slot::Ref(url_ref))?;
    // MIDP 默认方法 GET。
    set_field(heap, conn, method_slot, Slot::Ref(method_ref))?;
    set_field(heap, conn, state_slot, Slot::Int(STATE_SETUP))?;
    // -1 = 未连接（与 status 0（保留码）区分）。
    set_field(heap, conn, status_slot, Slot::Int(-1))?;
    Ok(conn)
}

/// 懒 connect：setup 态组装 HttpRequest 经 host 轨发出并把响应固化进
/// 实例字段；connected 幂等直过；closed 抛 ISE。失败置 closed 并映射
/// HostNetError → Java 异常。
fn hc_connect(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    this: ObjRef,
) -> Result<(), KvmError> {
    let state_slot = field_slot(cs, HCIMPL_CLASS, "state", "I")?;
    match field_int(heap, this, state_slot)? {
        STATE_CONNECTED => return Ok(()),
        STATE_CLOSED => {
            return Err(throw_illegal_state(cs, heap, "connection is closed"));
        }
        _ => {}
    }
    // 组装请求（setup 态字段全只读快照，无重入风险）。
    let url = string_text(heap, field_ref(heap, this, field_slot(cs, HCIMPL_CLASS, "url", "Ljava/lang/String;")?)?)?;
    let method = string_text(heap, field_ref(heap, this, field_slot(cs, HCIMPL_CLASS, "method", "Ljava/lang/String;")?)?)?;
    let count = field_int(heap, this, field_slot(cs, HCIMPL_CLASS, "reqCount", "I")?)? as usize;
    let mut headers = Vec::with_capacity(count);
    let names_ref = field_ref(heap, this, field_slot(cs, HCIMPL_CLASS, "reqNames", "[Ljava/lang/String;")?)?;
    let values_ref = field_ref(heap, this, field_slot(cs, HCIMPL_CLASS, "reqValues", "[Ljava/lang/String;")?)?;
    if !names_ref.is_null() && !values_ref.is_null() {
        let names = ref_slots(heap, names_ref)?;
        let values = ref_slots(heap, values_ref)?;
        for index in 0..count {
            if let (Some(name), Some(value)) = (
                string_at(heap, &names, index)?,
                string_at(heap, &values, index)?,
            ) {
                headers.push((name, value));
            }
        }
    }
    let body = {
        let body_ref = field_ref(heap, this, field_slot(cs, HCIMPL_CLASS, "bodyOut", "[B")?)?;
        if body_ref.is_null() {
            None
        } else {
            // GET 带体宽容透传（斯凯网关见过非常规 GET 载荷）。
            Some(byte_vec(heap, body_ref)?)
        }
    };
    let request = HttpRequest {
        method,
        url,
        headers,
        body,
        // MIDP 无 per-request 超时面；引擎侧默认 30s。
        timeout_ms: DEFAULT_TIMEOUT_MS,
    };
    // 先置 connected 再发出（host 实现同步返回；异常路径下文置 closed）。
    set_field(heap, this, state_slot, Slot::Int(STATE_CONNECTED))?;
    match host.http_request(request) {
        Ok(response) => {
            let HttpResponse {
                status,
                headers,
                body,
            } = response;
            let status_slot = field_slot(cs, HCIMPL_CLASS, "status", "I")?;
            set_field(heap, this, status_slot, Slot::Int(status as i32))?;
            let msg_ref = heap.alloc_string_uninterned(utf16(
                reason_phrase(status as i32).unwrap_or(""),
            ))?;
            set_field(
                heap,
                this,
                field_slot(cs, HCIMPL_CLASS, "respMsg", "Ljava/lang/String;")?,
                Slot::Ref(msg_ref),
            )?;
            // 响应头写平行数组（一次性按量分配）。
            let class = cs.load_class("[Ljava/lang/String;")?;
            let names = heap.alloc_array(class, ElemType::Ref, headers.len())?;
            let values = heap.alloc_array(class, ElemType::Ref, headers.len())?;
            for (index, (name, value)) in headers.iter().enumerate() {
                let name_ref = heap.alloc_string_uninterned(utf16(name))?;
                let value_ref = heap.alloc_string_uninterned(utf16(value))?;
                if let ArrayData::Refs(slots) = &mut heap.array_mut(names)?.data {
                    slots[index] = name_ref;
                }
                if let ArrayData::Refs(slots) = &mut heap.array_mut(values)?.data {
                    slots[index] = value_ref;
                }
            }
            set_field(
                heap,
                this,
                field_slot(cs, HCIMPL_CLASS, "respNames", "[Ljava/lang/String;")?,
                Slot::Ref(names),
            )?;
            set_field(
                heap,
                this,
                field_slot(cs, HCIMPL_CLASS, "respValues", "[Ljava/lang/String;")?,
                Slot::Ref(values),
            )?;
            set_field(
                heap,
                this,
                field_slot(cs, HCIMPL_CLASS, "respCount", "I")?,
                Slot::Int(headers.len() as i32),
            )?;
            let body_ref = jio::alloc_byte_array_from(cs, heap, &body)?;
            set_field(
                heap,
                this,
                field_slot(cs, HCIMPL_CLASS, "body", "[B")?,
                Slot::Ref(body_ref),
            )?;
            Ok(())
        }
        Err(error) => {
            // 失败连接按 closed 处置（后续调用 ISE，不残留半连接态）。
            set_field(heap, this, state_slot, Slot::Int(STATE_CLOSED))?;
            Err(throw_net_error(cs, heap, error))
        }
    }
}

/// 响应读取前置：closed 拒绝 + 懒 connect（响应头/实体族共用）。
fn ensure_connected(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    this: ObjRef,
) -> Result<(), KvmError> {
    hc_connect(cs, heap, host, this)
}

/// 响应头值读取（ensure_connected 之后调用）。
fn header_value(
    cs: &mut ClassSpace,
    heap: &Heap,
    this: ObjRef,
    name: &str,
) -> Result<Option<String>, KvmError> {
    let count = field_int(heap, this, field_slot(cs, HCIMPL_CLASS, "respCount", "I")?)? as usize;
    let names_ref = field_ref(heap, this, field_slot(cs, HCIMPL_CLASS, "respNames", "[Ljava/lang/String;")?)?;
    if names_ref.is_null() || count == 0 {
        return Ok(None);
    }
    let names = ref_slots(heap, names_ref)?;
    let Some(index) = find_header(heap, &names, name)? else {
        return Ok(None);
    };
    let values_ref = field_ref(heap, this, field_slot(cs, HCIMPL_CLASS, "respValues", "[Ljava/lang/String;")?)?;
    let values = ref_slots(heap, values_ref)?;
    string_at(heap, &values, index)
}

// ---------------------------------------------------------------------------
// Connector 原生实现
// ---------------------------------------------------------------------------

/// Connector.open(String) / open(String,int[,boolean])：URL 分派。
/// http/https → HttpConnectionImpl；其余（socket/ssl/comm/未知）→
/// ConnectionNotFoundException（不支持面，与媒体 locator 分支同异常）。
fn connector_open_inner(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    url_ref: ObjRef,
) -> Result<Option<Slot>, KvmError> {
    if url_ref.is_null() {
        return Err(throw_npe(cs, heap, "name is null"));
    }
    let url = string_text(heap, url_ref)?;
    if url.starts_with("http://") || url.starts_with("https://") {
        let conn = alloc_http_conn(cs, heap, &url)?;
        return Ok(Some(Slot::Ref(conn)));
    }
    // socket:// ssl:// comm:// 等本引擎无 Connection 实现 → 规范的
    // ConnectionNotFoundException（提示协议不受支持）。
    Err(jl::throw_java(
        cs,
        heap,
        "javax/io/ConnectionNotFoundException",
        Some(&format!("protocol not supported: {url}")),
    ))
}

/// Connector.open(String)Connection。
pub(crate) fn connector_open(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    connector_open_inner(cs, heap, arg_ref(call, 0)?)
}

/// Connector.open(String,int[,boolean])：mode 值域 1..=3 校验
/// （javadoc：非法 mode IllegalArgumentException）；timeout 标志按
/// MIDP 仅对 stream 连接有意义，此处记录性接受（HttpConnection 忽略）。
pub(crate) fn connector_open_mode(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let mode = arg_int(call, 1)?;
    if !(1..=3).contains(&mode) {
        return Err(jl::throw_java(
            cs,
            heap,
            "java/lang/IllegalArgumentException",
            Some(&format!("invalid access mode: {mode}")),
        ));
    }
    connector_open_inner(cs, heap, arg_ref(call, 0)?)
}

/// 构造已 connect 的连接（便捷法共用）：分派 + 懒连接一步到位。
fn open_and_connect(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    url_ref: ObjRef,
) -> Result<ObjRef, KvmError> {
    let conn = match connector_open_inner(cs, heap, url_ref)? {
        Some(Slot::Ref(conn)) => conn,
        other => {
            return Err(KvmError::Internal(format!(
                "open returned {other:?}"
            )))
        }
    };
    hc_connect(cs, heap, host, conn)?;
    Ok(conn)
}

/// Connector.openInputStream(String)：open + GET + 实体流。
pub(crate) fn connector_open_input_stream(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let conn = open_and_connect(cs, heap, host, arg_ref(call, 0)?)?;
    open_input_stream_of(cs, heap, host, conn)
}

/// Connector.openDataInputStream(String)：openInputStream 包 DataInputStream。
pub(crate) fn connector_open_data_input_stream(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let conn = open_and_connect(cs, heap, host, arg_ref(call, 0)?)?;
    open_data_input_stream_of(cs, heap, host, conn)
}

/// Connector.openDataOutputStream(String)：open + 输出流（不 connect；
/// 后续 getResponseCode/openInputStream 才触发请求）。
pub(crate) fn connector_open_data_output_stream(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    _host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let conn = match connector_open_inner(cs, heap, arg_ref(call, 0)?)? {
        Some(Slot::Ref(conn)) => conn,
        other => {
            return Err(KvmError::Internal(format!("open returned {other:?}")))
        }
    };
    open_data_output_stream_of(cs, heap, conn)
}

// ---------------------------------------------------------------------------
// HttpConnectionImpl 原生实现
// ---------------------------------------------------------------------------

/// <init>(String)：NEW 由调用方完成，此处写齐初值（与 alloc_http_conn
/// 同字段集；测试与用户子类化路径走此入口）。
pub(crate) fn hc_init(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "HttpConnectionImpl.<init>")?;
    let url = arg_ref(call, 0)?;
    if url.is_null() {
        return Err(throw_npe(cs, heap, "url is null"));
    }
    let url_text = string_text(heap, url)?;
    if !(url_text.starts_with("http://") || url_text.starts_with("https://")) {
        return Err(jl::throw_java(
            cs,
            heap,
            "javax/io/ConnectionNotFoundException",
            Some(&format!("protocol not supported: {url_text}")),
        ));
    }
    let url_slot = field_slot(cs, HCIMPL_CLASS, "url", "Ljava/lang/String;")?;
    let method_slot = field_slot(cs, HCIMPL_CLASS, "method", "Ljava/lang/String;")?;
    let state_slot = field_slot(cs, HCIMPL_CLASS, "state", "I")?;
    let status_slot = field_slot(cs, HCIMPL_CLASS, "status", "I")?;
    set_field(heap, this, url_slot, Slot::Ref(url))?;
    let method_ref = heap.alloc_string_uninterned(utf16("GET"))?;
    set_field(heap, this, method_slot, Slot::Ref(method_ref))?;
    set_field(heap, this, state_slot, Slot::Int(STATE_SETUP))?;
    set_field(heap, this, status_slot, Slot::Int(-1))?;
    Ok(None)
}

/// setup 态守卫（setRequestMethod/setRequestProperty 共用）。
fn require_setup(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    this: ObjRef,
) -> Result<(), KvmError> {
    let state_slot = field_slot(cs, HCIMPL_CLASS, "state", "I")?;
    match field_int(heap, this, state_slot)? {
        STATE_SETUP => Ok(()),
        STATE_CONNECTED => Err(throw_illegal_state(
            cs,
            heap,
            "connection already connected",
        )),
        _ => Err(throw_illegal_state(cs, heap, "connection is closed")),
    }
}

/// setRequestMethod：null→NPE；connected 后 ISE；存原样（GET/POST/HEAD
/// 之外的自定义方法透传——网关方言宽容）。
pub(crate) fn hc_set_request_method(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "setRequestMethod")?;
    let method = arg_ref(call, 0)?;
    if method.is_null() {
        return Err(throw_npe(cs, heap, "method is null"));
    }
    require_setup(cs, heap, this)?;
    let slot = field_slot(cs, HCIMPL_CLASS, "method", "Ljava/lang/String;")?;
    set_field(heap, this, slot, Slot::Ref(method))?;
    Ok(None)
}

/// getRequestMethod。
pub(crate) fn hc_get_request_method(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "getRequestMethod")?;
    let slot = field_slot(cs, HCIMPL_CLASS, "method", "Ljava/lang/String;")?;
    Ok(Some(Slot::Ref(field_ref(heap, this, slot)?)))
}

/// setRequestProperty：null→NPE；setup 限定；同名覆盖。
pub(crate) fn hc_set_request_property(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "setRequestProperty")?;
    let name = arg_ref(call, 0)?;
    let value = arg_ref(call, 1)?;
    if name.is_null() || value.is_null() {
        return Err(throw_npe(cs, heap, "name/value is null"));
    }
    require_setup(cs, heap, this)?;
    push_header(
        cs,
        heap,
        this,
        &string_text(heap, name)?,
        &string_text(heap, value)?,
    )?;
    Ok(None)
}

/// getRequestProperty（未设置返 null；connected 后仍可读快照）。
pub(crate) fn hc_get_request_property(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "getRequestProperty")?;
    let name = string_text(heap, arg_ref(call, 0)?)?;
    let count = field_int(heap, this, field_slot(cs, HCIMPL_CLASS, "reqCount", "I")?)? as usize;
    let names_ref = field_ref(heap, this, field_slot(cs, HCIMPL_CLASS, "reqNames", "[Ljava/lang/String;")?)?;
    if names_ref.is_null() || count == 0 {
        return Ok(Some(Slot::Ref(ObjRef::NULL)));
    }
    let names = ref_slots(heap, names_ref)?;
    // 只遍历前 count 个有效头（扩容产生的尾槽为 null）。
    if let Some(index) = find_header(heap, &names[..count.min(names.len())], &name)? {
        let values_ref = field_ref(heap, this, field_slot(cs, HCIMPL_CLASS, "reqValues", "[Ljava/lang/String;")?)?;
        let values = ref_slots(heap, values_ref)?;
        if let Some(text) = string_at(heap, &values, index)? {
            return alloc_string(heap, &text);
        }
    }
    Ok(Some(Slot::Ref(ObjRef::NULL)))
}

/// URL 族 getter（pure：纯字符串拆解，无宿主依赖）。
pub(crate) fn hc_get_url(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "getURL")?;
    let slot = field_slot(cs, HCIMPL_CLASS, "url", "Ljava/lang/String;")?;
    Ok(Some(Slot::Ref(field_ref(heap, this, slot)?)))
}

pub(crate) fn hc_get_protocol(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "getProtocol")?;
    let parts = url_parts_of(cs, heap, this)?;
    alloc_string(heap, &parts.protocol)
}

pub(crate) fn hc_get_host(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "getHost")?;
    let parts = url_parts_of(cs, heap, this)?;
    alloc_string(heap, &parts.host)
}

/// getPort：显式端口或协议默认（http 80 / https 443；未知协议 80）。
pub(crate) fn hc_get_port(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "getPort")?;
    let parts = url_parts_of(cs, heap, this)?;
    let port = parts.port.unwrap_or(match parts.protocol.as_str() {
        "https" => HTTPS_DEFAULT_PORT,
        _ => HTTP_DEFAULT_PORT,
    });
    Ok(Some(Slot::Int(port)))
}

pub(crate) fn hc_get_file(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "getFile")?;
    let parts = url_parts_of(cs, heap, this)?;
    // MIDP getFile = path + query（含 '?'）。
    let file = match parts.query {
        Some(query) => format!("{}?{}", parts.file, query),
        None => parts.file,
    };
    alloc_string(heap, &file)
}

pub(crate) fn hc_get_query(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "getQuery")?;
    let parts = url_parts_of(cs, heap, this)?;
    match parts.query {
        Some(query) => alloc_string(heap, &query),
        None => Ok(Some(Slot::Ref(ObjRef::NULL))),
    }
}

pub(crate) fn hc_get_ref(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "getRef")?;
    let parts = url_parts_of(cs, heap, this)?;
    match parts.reference {
        Some(reference) => alloc_string(heap, &reference),
        None => Ok(Some(Slot::Ref(ObjRef::NULL))),
    }
}

/// openOutputStream：setup 态限定（connected 后按项目先例 ISE，
/// 简化规范未定死的异常类型）；返回 HttpOutputStream。
pub(crate) fn hc_open_output_stream(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    _host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "openOutputStream")?;
    require_setup(cs, heap, this)?;
    let stream = alloc_http_output(cs, heap, this)?;
    Ok(Some(Slot::Ref(stream)))
}

/// openDataOutputStream：openOutputStream 包 DataOutputStream。
pub(crate) fn hc_open_data_output_stream(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    _host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "openDataOutputStream")?;
    require_setup(cs, heap, this)?;
    open_data_output_stream_of(cs, heap, this)
}

/// openInputStream：懒 connect → body 的 ByteArrayInputStream。
pub(crate) fn hc_open_input_stream(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "openInputStream")?;
    open_input_stream_of(cs, heap, host, this)
}

/// openDataInputStream：openInputStream 包 DataInputStream。
pub(crate) fn hc_open_data_input_stream(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "openDataInputStream")?;
    open_data_input_stream_of(cs, heap, host, this)
}

/// openInputStream 主体（Connector 便捷法与实例方法共用）。
fn open_input_stream_of(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    conn: ObjRef,
) -> Result<Option<Slot>, KvmError> {
    ensure_connected(cs, heap, host, conn)?;
    let body_ref = field_ref(heap, conn, field_slot(cs, HCIMPL_CLASS, "body", "[B")?)?;
    // 响应体 ByteArrayInputStream（零拷贝视图语义：直接持 body 数组）。
    let bais = alloc_instance_of(cs, heap, "java/io/ByteArrayInputStream")?;
    let buf_slot = field_slot(cs, "java/io/ByteArrayInputStream", "buf", "[B")?;
    let pos_slot = field_slot(cs, "java/io/ByteArrayInputStream", "pos", "I")?;
    let count_slot = field_slot(cs, "java/io/ByteArrayInputStream", "count", "I")?;
    let mark_slot = field_slot(cs, "java/io/ByteArrayInputStream", "markPos", "I")?;
    set_field(heap, bais, buf_slot, Slot::Ref(body_ref))?;
    set_field(heap, bais, pos_slot, Slot::Int(0))?;
    set_field(
        heap,
        bais,
        count_slot,
        Slot::Int(heap.array(body_ref)?.data.len() as i32),
    )?;
    set_field(heap, bais, mark_slot, Slot::Int(0))?;
    Ok(Some(Slot::Ref(bais)))
}

/// openDataInputStream 主体。
fn open_data_input_stream_of(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    conn: ObjRef,
) -> Result<Option<Slot>, KvmError> {
    let Some(Slot::Ref(bais)) = open_input_stream_of(cs, heap, host, conn)? else {
        return Err(KvmError::Internal("openInputStream returned none".into()));
    };
    // DataInputStream(bais)：alloc + 写 FilterInputStream.in。
    let dis = alloc_instance_of(cs, heap, "java/io/DataInputStream")?;
    let in_slot = field_slot(
        cs,
        "java/io/FilterInputStream",
        "in",
        "Ljava/io/InputStream;",
    )?;
    set_field(heap, dis, in_slot, Slot::Ref(bais))?;
    Ok(Some(Slot::Ref(dis)))
}

/// openDataOutputStream 主体（HttpOutputStream 包 DataOutputStream）。
fn open_data_output_stream_of(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    conn: ObjRef,
) -> Result<Option<Slot>, KvmError> {
    let hout = alloc_http_output(cs, heap, conn)?;
    let dos = alloc_instance_of(cs, heap, "java/io/DataOutputStream")?;
    let out_slot = field_slot(
        cs,
        "java/io/FilterOutputStream",
        "out",
        "Ljava/io/OutputStream;",
    )?;
    set_field(heap, dos, out_slot, Slot::Ref(hout))?;
    Ok(Some(Slot::Ref(dos)))
}

/// getResponseCode：未连接自动 connect（javadoc 明文）。
pub(crate) fn hc_get_response_code(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "getResponseCode")?;
    ensure_connected(cs, heap, host, this)?;
    Ok(Some(Slot::Int(field_int(
        heap,
        this,
        field_slot(cs, HCIMPL_CLASS, "status", "I")?,
    )?)))
}

/// getResponseMessage：同 getResponseCode 自动 connect；未收录短语 null。
pub(crate) fn hc_get_response_message(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "getResponseMessage")?;
    ensure_connected(cs, heap, host, this)?;
    let status = field_int(heap, this, field_slot(cs, HCIMPL_CLASS, "status", "I")?)?;
    match reason_phrase(status) {
        Some(text) => alloc_string(heap, text),
        None => Ok(Some(Slot::Ref(ObjRef::NULL))),
    }
}

/// getHeaderField(String)：null 参数返回合成状态行（javadoc 语义）；
/// 名字大小写不敏感；未命中 null。
pub(crate) fn hc_get_header_field_named(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "getHeaderField")?;
    let name_ref = arg_ref(call, 0)?;
    if name_ref.is_null() {
        ensure_connected(cs, heap, host, this)?;
        let status = field_int(heap, this, field_slot(cs, HCIMPL_CLASS, "status", "I")?)?;
        let line = format!(
            "HTTP/1.1 {status} {}",
            reason_phrase(status).unwrap_or("")
        );
        return alloc_string(heap, &line);
    }
    let name = string_text(heap, name_ref)?;
    ensure_connected(cs, heap, host, this)?;
    match header_value(cs, heap, this, &name)? {
        Some(value) => alloc_string(heap, &value),
        None => Ok(Some(Slot::Ref(ObjRef::NULL))),
    }
}

/// getHeaderFieldInt(String,int)：未命中/解析失败返回默认值。
pub(crate) fn hc_get_header_field_int(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "getHeaderFieldInt")?;
    let name = string_text(heap, arg_ref(call, 0)?)?;
    let default = arg_int(call, 1)?;
    ensure_connected(cs, heap, host, this)?;
    let value = header_value(cs, heap, this, &name)?
        .and_then(|text| text.trim().parse::<i32>().ok())
        .unwrap_or(default);
    Ok(Some(Slot::Int(value)))
}

/// getHeaderFieldDate(String,long)：日期头解析归宿主层（引擎无
/// HTTP-date 语义面），此处直返默认值（javadoc 允许）。
pub(crate) fn hc_get_header_field_date(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let default = match call.args.get(1) {
        Some(Slot::Long(value)) => *value,
        other => {
            return Err(KvmError::Internal(format!(
                "getHeaderFieldDate arg 1 expected long, got {other:?}"
            )))
        }
    };
    Ok(Some(Slot::Long(default)))
}

/// getHeaderField(int)：按序取值（n 越界返 null；MIDP 序列不含状态行）。
pub(crate) fn hc_get_header_field_indexed(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "getHeaderField")?;
    let index = arg_int(call, 0)?;
    ensure_connected(cs, heap, host, this)?;
    match response_header_at(cs, heap, this, index)? {
        Some((_, value)) => alloc_string(heap, &value),
        None => Ok(Some(Slot::Ref(ObjRef::NULL))),
    }
}

/// getHeaderFieldKey(int)：按序取名（n 越界返 null）。
pub(crate) fn hc_get_header_field_key(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "getHeaderFieldKey")?;
    let index = arg_int(call, 0)?;
    ensure_connected(cs, heap, host, this)?;
    match response_header_at(cs, heap, this, index)? {
        Some((name, _)) => alloc_string(heap, &name),
        None => Ok(Some(Slot::Ref(ObjRef::NULL))),
    }
}

/// 响应头按序取（index 越界 None）。
fn response_header_at(
    cs: &mut ClassSpace,
    heap: &Heap,
    this: ObjRef,
    index: i32,
) -> Result<Option<(String, String)>, KvmError> {
    if index < 0 {
        return Ok(None);
    }
    let count = field_int(heap, this, field_slot(cs, HCIMPL_CLASS, "respCount", "I")?)? as usize;
    let index = index as usize;
    if index >= count {
        return Ok(None);
    }
    let names_ref = field_ref(heap, this, field_slot(cs, HCIMPL_CLASS, "respNames", "[Ljava/lang/String;")?)?;
    let values_ref = field_ref(heap, this, field_slot(cs, HCIMPL_CLASS, "respValues", "[Ljava/lang/String;")?)?;
    let names = ref_slots(heap, names_ref)?;
    let values = ref_slots(heap, values_ref)?;
    let name = string_at(heap, &names, index)?.unwrap_or_default();
    let value = string_at(heap, &values, index)?.unwrap_or_default();
    Ok(Some((name, value)))
}

/// getLength：Content-Length 或 -1（long 返回）。
pub(crate) fn hc_get_length(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "getLength")?;
    ensure_connected(cs, heap, host, this)?;
    let value = match header_value(cs, heap, this, "Content-Length")? {
        Some(text) => text.trim().parse::<i64>().unwrap_or(-1),
        None => -1,
    };
    Ok(Some(Slot::Long(value)))
}

/// getType：Content-Type（ContentConnection 面）。
pub(crate) fn hc_get_type(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "getType")?;
    ensure_connected(cs, heap, host, this)?;
    match header_value(cs, heap, this, "Content-Type")? {
        Some(text) => alloc_string(heap, &text),
        None => Ok(Some(Slot::Ref(ObjRef::NULL))),
    }
}

/// getEncoding：Content-Encoding（无则 null）。
pub(crate) fn hc_get_encoding(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "getEncoding")?;
    ensure_connected(cs, heap, host, this)?;
    match header_value(cs, heap, this, "Content-Encoding")? {
        Some(text) => alloc_string(heap, &text),
        None => Ok(Some(Slot::Ref(ObjRef::NULL))),
    }
}

/// 日期族桩：HTTP-date 语义归宿主层，直返 0（long）。
pub(crate) fn hc_get_expiration(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let _ = this_ref(_call, "getExpiration")?;
    Ok(Some(Slot::Long(0)))
}

pub(crate) fn hc_get_date(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let _ = this_ref(call, "getDate")?;
    Ok(Some(Slot::Long(0)))
}

pub(crate) fn hc_get_last_modified(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let _ = this_ref(call, "getLastModified")?;
    Ok(Some(Slot::Long(0)))
}

/// close：任何态幂等置 closed（javadoc：close 已关连接合法）。
pub(crate) fn hc_close(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "close")?;
    let state_slot = field_slot(cs, HCIMPL_CLASS, "state", "I")?;
    set_field(heap, this, state_slot, Slot::Int(STATE_CLOSED))?;
    Ok(None)
}

// ---------------------------------------------------------------------------
// HttpOutputStream 原生实现
// ---------------------------------------------------------------------------

/// 构造 HttpOutputStream（内嵌 BAOS 缓冲；不走 <init>，alloc 模式同上）。
fn alloc_http_output(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    conn: ObjRef,
) -> Result<ObjRef, KvmError> {
    let stream = alloc_instance_of(cs, heap, HOUT_CLASS)?;
    let conn_slot = field_slot(cs, HOUT_CLASS, "conn", "Ljavax/microedition/io/HttpConnectionImpl;")?;
    let baos_slot = field_slot(cs, HOUT_CLASS, "baos", "Ljava/io/ByteArrayOutputStream;")?;
    set_field(heap, stream, conn_slot, Slot::Ref(conn))?;
    // BAOS 零值实例（buf null/count 0，baos_append 惰性建缓冲）。
    let baos = alloc_instance_of(cs, heap, BAOS_CLASS)?;
    set_field(heap, stream, baos_slot, Slot::Ref(baos))?;
    Ok(stream)
}

/// <init>(HttpConnectionImpl)：NEW 由调用方完成，写 conn/baos 初值
///（与 alloc_http_output 同字段集；防御 null conn → NPE）。
pub(crate) fn ho_init(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "HttpOutputStream.<init>")?;
    let conn = arg_ref(call, 0)?;
    if conn.is_null() {
        return Err(throw_npe(cs, heap, "conn is null"));
    }
    let conn_slot = field_slot(cs, HOUT_CLASS, "conn", "Ljavax/microedition/io/HttpConnectionImpl;")?;
    let baos_slot = field_slot(cs, HOUT_CLASS, "baos", "Ljava/io/ByteArrayOutputStream;")?;
    set_field(heap, this, conn_slot, Slot::Ref(conn))?;
    let baos = alloc_instance_of(cs, heap, BAOS_CLASS)?;
    set_field(heap, this, baos_slot, Slot::Ref(baos))?;
    Ok(None)
}

/// 读 this 的内部 BAOS 引用。
fn baos_of(cs: &mut ClassSpace, heap: &Heap, this: ObjRef) -> Result<ObjRef, KvmError> {
    let slot = field_slot(cs, HOUT_CLASS, "baos", "Ljava/io/ByteArrayOutputStream;")?;
    Ok(field_ref(heap, this, slot)?)
}

/// 转发内部 BAOS 的 write（合成 NativeCall 桥，media toString 桥同法）。
fn forward_baos_write(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
    baos: ObjRef,
    func: NativeFn,
    desc: &str,
    args: Vec<Slot>,
) -> Result<Option<Slot>, KvmError> {
    let class = cs.load_class(BAOS_CLASS)?;
    let name = cs.syms.intern("write");
    let desc_sym = cs.syms.intern(desc);
    let inner = NativeCall {
        class,
        name,
        desc: desc_sym,
        this: Some(baos),
        args,
        thread: call.thread,
    };
    func(cs, heap, &inner)
}

/// write(int)。
pub(crate) fn ho_write_int(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "HttpOutputStream.write")?;
    let baos = baos_of(cs, heap, this)?;
    forward_baos_write(
        cs,
        heap,
        call,
        baos,
        jio::baos_write_int,
        "(I)V",
        vec![Slot::Int(arg_int(call, 0)?)],
    )
}

/// write(byte[])。
pub(crate) fn ho_write_all(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "HttpOutputStream.write")?;
    let baos = baos_of(cs, heap, this)?;
    forward_baos_write(
        cs,
        heap,
        call,
        baos,
        jio::baos_write_all,
        "([B)V",
        vec![Slot::Ref(arg_ref(call, 0)?)],
    )
}

/// write(byte[],int,int)。
pub(crate) fn ho_write_range(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "HttpOutputStream.write")?;
    let baos = baos_of(cs, heap, this)?;
    forward_baos_write(
        cs,
        heap,
        call,
        baos,
        jio::baos_write_range,
        "([BII)V",
        vec![
            Slot::Ref(arg_ref(call, 0)?),
            Slot::Int(arg_int(call, 1)?),
            Slot::Int(arg_int(call, 2)?),
        ],
    )
}

/// flush：把 BAOS 缓冲固化进所属连接的 bodyOut（幂等：flushed 置位后
/// 再 flush 仅同步增量——简化为每次全量快照覆盖，末次生效即请求体）。
pub(crate) fn ho_flush(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "HttpOutputStream.flush")?;
    let baos = baos_of(cs, heap, this)?;
    // 读 BAOS 的 buf/count（截断到 count 即有效载荷）。
    let buf_ref = field_ref(heap, baos, field_slot(cs, BAOS_CLASS, "buf", "[B")?)?;
    let count = field_ref_int_count(cs, heap, baos)?;
    let data = if buf_ref.is_null() {
        Vec::new()
    } else {
        let bytes = byte_vec(heap, buf_ref)?;
        bytes[..count.min(bytes.len())].to_vec()
    };
    let conn = field_ref(heap, this, field_slot(cs, HOUT_CLASS, "conn", "Ljavax/microedition/io/HttpConnectionImpl;")?)?;
    let body = jio::alloc_byte_array_from(cs, heap, &data)?;
    set_field(
        heap,
        conn,
        field_slot(cs, HCIMPL_CLASS, "bodyOut", "[B")?,
        Slot::Ref(body),
    )?;
    let flushed_slot = field_slot(cs, HOUT_CLASS, "flushed", "Z")?;
    set_field(heap, this, flushed_slot, Slot::Int(1))?;
    Ok(None)
}

/// BAOS count 字段读取。
fn field_ref_int_count(
    cs: &mut ClassSpace,
    heap: &Heap,
    baos: ObjRef,
) -> Result<usize, KvmError> {
    let slot = field_slot(cs, BAOS_CLASS, "count", "I")?;
    Ok(field_int(heap, baos, slot)?.max(0) as usize)
}

/// close：flush 语义（请求体固化）；已 close 再关幂等。
pub(crate) fn ho_close(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    ho_flush(cs, heap, call)
}
