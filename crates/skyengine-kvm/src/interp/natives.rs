//! natives 框架（T010）：bootstrap 类声明式注册表 + `NativeDispatch` 实现。
//!
//! ## 接入点
//!
//! 解释器 `invoke*` 系列在 `VmMethod::body == None` 时走原生分派
//! （见 `opcodes.rs` 的 `call_native`），本模块提供：
//! - **声明结构**：`BootstrapDecl` 系列描述无字节码的框架类
//!   （类名/父类/接口/字段布局/方法签名 → Rust 函数绑定）。
//! - **注册表**：`bootstrap_registry()` 返回静态表；类空间
//!   `load_class` 在 jar 提供者之前优先查表（见 classspace `define`）。
//! - **分派器**：`RegistryDispatch` 按 类名.方法名+描述符 查表调用 `NativeFn`。
//!
//! ## 注册表范围
//!
//! jc.jar 字节码饥饿链接（decode 期解析全部指令级类/方法/字段引用），
//! 因此注册表须覆盖 jar 引用的全部 CLDC/MIDP API 面：
//! java/lang 核心、java/io、java/util、javax/microedition（lcdui/media/
//! midlet/rms）。方法体本任务以桩值占位，T014 起填真实现
//! （String/字符串缓冲、System.arraycopy、monitor 桥接等）。
//! jar 引用的**不存在类**（com/jblend、com/sky、cost 等）不进表，
//! 加载时按 NoClassDefFoundError 语义报错。

use crate::KvmError;
use crate::classfile::member::acc;
use crate::cldc::io as jio;
use crate::cldc::lang as jl;
use crate::cldc::thread as jt;
use crate::cldc::util as ju;
use crate::host::HostContext;
use crate::midp::lcdui::canvas;
use crate::midp::lcdui::display;
use crate::midp::lcdui::font;
use crate::midp::lcdui::game::gamecanvas;
use crate::midp::lcdui::game::layer;
use crate::midp::lcdui::graphics;
use crate::midp::lcdui::image;
use crate::midp::lcdui::input;
use crate::midp::io as jgcf;
use crate::midp::media as jmedia;
use crate::midp::midlet as jmidlet;
use crate::midp::rms as jrms;
use crate::runtime::classspace::{ClassSpace, StaticConst};
use crate::runtime::heap::Heap;
use crate::runtime::{ObjRef, Slot};

use super::{NativeCall, NativeDispatch};

/// 原生方法函数指针：与解释器调用约定一致。
/// `Ok(None)` = void 返回；返回值为单值槽（long/double 不带 Pad，分派处补齐）。
pub type NativeFn = fn(&mut ClassSpace, &mut Heap, &NativeCall) -> Result<Option<Slot>, KvmError>;

/// 宿主通道原生函数（T019 双轨）：在纯函数轨之上透传 `HostContext`，
/// LCDUI Font/Display 等平台相关实现经此访问字体/屏幕/时钟服务。
pub type HostNativeFn = fn(
    &mut ClassSpace,
    &mut Heap,
    &mut dyn HostContext,
    &NativeCall,
) -> Result<Option<Slot>, KvmError>;

/// 注册表条目的实现轨：Pure = 纯 KVM 侧（旧 native 全量沿用），
/// Host = 需要宿主服务的实现（T019 起接入）。
#[derive(Clone, Copy)]
pub enum NativeImpl {
    Pure(NativeFn),
    Host(HostNativeFn),
}

/// bootstrap 静态字段常量初值（字符串在注册期驻留成符号）。
#[derive(Clone, Copy, Debug)]
pub enum BootstrapConst {
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Str(&'static str),
}

/// bootstrap 字段声明（实例/静态共用；`access_flags` 区分）。
pub struct BootstrapFieldDecl {
    pub name: &'static str,
    pub desc: &'static str,
    pub access_flags: u16,
    pub constant: Option<BootstrapConst>,
}

/// bootstrap 方法声明（native 绑定 Rust 函数；接口抽象方法仅占签名）。
pub struct BootstrapMethodDecl {
    pub name: &'static str,
    pub desc: &'static str,
    /// 静态方法须含 `acc::STATIC`；注册时非 abstract 统一补 `acc::NATIVE`。
    pub access_flags: u16,
    pub func: NativeImpl,
}

/// bootstrap 类声明：无字节码，由类空间按声明直接构造 `VmClass`。
pub struct BootstrapDecl {
    pub name: &'static str,
    pub access_flags: u16,
    /// 父类内部名；`java/lang/Object` 为 `None`。
    pub super_name: Option<&'static str>,
    pub interfaces: &'static [&'static str],
    pub fields: &'static [BootstrapFieldDecl],
    pub methods: &'static [BootstrapMethodDecl],
}

/// 方法声明速记（静态上下文可用的 const 构造器）。
/// pub(crate)：midp::lcdui::game::layer 占位声明跨模块复用（T031）。
pub(crate) const fn m(
    name: &'static str,
    desc: &'static str,
    access_flags: u16,
    func: NativeFn,
) -> BootstrapMethodDecl {
    BootstrapMethodDecl {
        name,
        desc,
        access_flags,
        func: NativeImpl::Pure(func),
    }
}

/// 宿主轨方法声明速记（T019）：实现签名带 `HostContext` 参数。
/// pub(crate)：midp::io（GCF，T041）跨模块声明复用。
pub(crate) const fn mh(
    name: &'static str,
    desc: &'static str,
    access_flags: u16,
    func: HostNativeFn,
) -> BootstrapMethodDecl {
    BootstrapMethodDecl {
        name,
        desc,
        access_flags,
        func: NativeImpl::Host(func),
    }
}

/// 普通骨架类声明（无字段；方法列表可为空）。
const fn klass(
    name: &'static str,
    super_name: &'static str,
    methods: &'static [BootstrapMethodDecl],
) -> BootstrapDecl {
    BootstrapDecl {
        name,
        access_flags: acc::PUBLIC,
        super_name: Some(super_name),
        interfaces: &[],
        fields: &[],
        methods,
    }
}

/// 接口骨架声明（方法占签名，`acc::ABSTRACT` 不可调用，T014 落实实现类）。
const fn iface(name: &'static str, methods: &'static [BootstrapMethodDecl]) -> BootstrapDecl {
    BootstrapDecl {
        name,
        access_flags: acc::PUBLIC | acc::INTERFACE,
        super_name: Some("java/lang/Object"),
        interfaces: &[],
        fields: &[],
        methods,
    }
}

/// 实例/静态字段声明速记（无常量初值）。
/// pub(crate)：同 `m`（layer 占位复用）。
pub(crate) const fn field(
    name: &'static str,
    desc: &'static str,
    access_flags: u16,
) -> BootstrapFieldDecl {
    BootstrapFieldDecl {
        name,
        desc,
        access_flags,
        constant: None,
    }
}

/// 带常量初值的静态字段声明速记。
const fn const_field(
    name: &'static str,
    desc: &'static str,
    constant: BootstrapConst,
) -> BootstrapFieldDecl {
    BootstrapFieldDecl {
        name,
        desc,
        access_flags: acc::PUBLIC | acc::STATIC | acc::FINAL,
        constant: Some(constant),
    }
}

// ---------------------------------------------------------------------------
// 桩实现（T014 替换为真实现）
// ---------------------------------------------------------------------------

/// void 桩：什么都不做。pub(crate)：layer 占位复用（T031）。
pub(crate) fn stub_void(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(None)
}

/// 引用返回桩：暂返 null（T014 实现）。pub(crate)：同上。
pub(crate) fn stub_null(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Ref(ObjRef::NULL)))
}

/// int/boolean 桩：暂返 0（T014 实现）。pub(crate)：同上。
pub(crate) fn stub_zero(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Int(0)))
}

/// 审计面（T047 基线审计）：绑定是否为通用桩（void/null/zero 之一）。
/// 基线测试据此把"可链接"细分为 已实现 / 占位 两类；
/// FR-016 允许长尾冷门成员以桩占位（调用返回默认值语义）。
pub fn is_stub(func: NativeImpl) -> bool {
    let NativeImpl::Pure(f) = func else {
        // 宿主轨绑定视为已实现（平台服务接入点，非空壳）。
        return false;
    };
    // fn_addr_eq：同一编译会话内 fn 项地址可比（三个桩函数体互异，
    // 链接器不会折叠合并）；fn 项先 as 强转 fn 指针以满足 FnPtr 约束。
    let (v, n, z): (NativeFn, NativeFn, NativeFn) = (stub_void, stub_null, stub_zero);
    std::ptr::fn_addr_eq(f, v) || std::ptr::fn_addr_eq(f, n) || std::ptr::fn_addr_eq(f, z)
}

// ---------------------------------------------------------------------------
// java/lang 核心（T014：真实现，绑定 crate::cldc::lang）
// ---------------------------------------------------------------------------

static OBJECT_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/lang/Object",
    access_flags: acc::PUBLIC,
    super_name: None,
    interfaces: &[],
    fields: &[],
    methods: &[
        m("<init>", "()V", acc::PUBLIC, jl::object_init),
        m("hashCode", "()I", acc::PUBLIC, jl::object_hash_code),
        m(
            "equals",
            "(Ljava/lang/Object;)Z",
            acc::PUBLIC,
            jl::object_equals,
        ),
        m(
            "toString",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            jl::object_to_string,
        ),
        m(
            "getClass",
            "()Ljava/lang/Class;",
            acc::PUBLIC | acc::FINAL,
            jl::object_get_class,
        ),
        // wait/notify/notifyAll（T034）：native 置请求位/上抛 Yield，
        // 解释器 native 分派点经 Monitors 桥接（持锁检查 + 释放/标记）
        // ——见 opcodes.rs exec_invoke 尾部与 cldc/lang.rs 注释。
        m("notify", "()V", acc::PUBLIC | acc::FINAL, jl::object_notify),
        m(
            "notifyAll",
            "()V",
            acc::PUBLIC | acc::FINAL,
            jl::object_notify_all,
        ),
        mh("wait", "()V", acc::PUBLIC | acc::FINAL, jl::object_wait),
        mh("wait", "(J)V", acc::PUBLIC | acc::FINAL, jl::object_wait),
    ],
};

static CLASS_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/lang/Class",
    access_flags: acc::PUBLIC | acc::FINAL,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    // int classId 持类表下标（见 cldc::lang::class_object_new）。
    fields: &[field("classId", "I", acc::PRIVATE)],
    methods: &[
        m(
            "getName",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            jl::class_get_name,
        ),
        m(
            "toString",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            jl::class_to_string,
        ),
        m(
            "forName",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            acc::PUBLIC | acc::STATIC,
            jl::class_for_name,
        ),
        m(
            "isInstance",
            "(Ljava/lang/Object;)Z",
            acc::PUBLIC,
            jl::class_is_instance,
        ),
        m(
            "isAssignableFrom",
            "(Ljava/lang/Class;)Z",
            acc::PUBLIC,
            jl::class_is_assignable_from,
        ),
        m(
            "getResourceAsStream",
            "(Ljava/lang/String;)Ljava/io/InputStream;",
            acc::PUBLIC,
            class_get_resource_as_stream,
        ),
        m(
            "newInstance",
            "()Ljava/lang/Object;",
            acc::PUBLIC,
            stub_null,
        ),
    ],
};

/// `Class.getResourceAsStream(String)`：套件资源 → ByteArrayInputStream。
/// 资源键与 Image.createImage(String) 同型（先去前导 '/' 再试原键）；
/// 未命中按 JDK 语义返回 null（调用方自行判 NPE）。
/// 冒烟实证：jc.jar 资源加载线程经此读 jar 数据文件，空桩断流。
fn class_get_resource_as_stream(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let name_ref = match call.args.first() {
        Some(Slot::Ref(reference)) => *reference,
        other => {
            return Err(KvmError::Internal(format!(
                "getResourceAsStream arg 0 expected ref, got {other:?}"
            )));
        }
    };
    if name_ref.is_null() {
        return Err(jl::throw_java(
            cs,
            heap,
            "java/lang/NullPointerException",
            None,
        ));
    }
    let chars = heap.str_obj(name_ref)?.chars.clone();
    let name = String::from_utf16_lossy(&chars);
    let stripped = name.strip_prefix('/').unwrap_or(&name).to_string();
    // Rc 浅拷贝（数据本体不复制），构流期不占 cs/heap 借用。
    // 第三跳 basename 兜底：gjxjwql 的 images.dat 表统一拼 /images/ 前缀
    // 查询，而渠道 jar 把 Huo.act 等动作数据放在根（.act 缺失令精灵
    // 拼合错乱）——按文件名末段再查一次（斯凯宽松查找语义）。
    let basename = stripped.rsplit('/').next().map(str::to_string);
    let data = cs
        .suite_resources
        .get(&stripped)
        .or_else(|| cs.suite_resources.get(&name))
        .or_else(|| basename.as_deref().and_then(|b| cs.suite_resources.get(b)))
        .cloned();
    // 调试面：SKYENGINE_KVM_TRACE_RES=1 打印资源查询与命中（缺资源
    // 断流取证——h.a 装载链 ByteArrayInputStream(null) NPE 的上游）。
    if std::env::var_os("SKYENGINE_KVM_TRACE_RES").is_some() {
        eprintln!(
            "[kvm-res] getResourceAsStream({name:?}) -> {}",
            data.as_ref().map_or(-1, |d| d.len() as i64)
        );
    }
    match data {
        Some(data) => Ok(Some(Slot::Ref(jio::alloc_byte_array_stream(
            cs, heap, &data,
        )?))),
        None => Ok(Some(Slot::Ref(ObjRef::NULL))),
    }
}

/// Throwable 构造对（()V / (String)V），全部异常子类共用。
static INIT_PAIR: &[BootstrapMethodDecl] = &[
    m("<init>", "()V", acc::PUBLIC, jl::throwable_init_empty),
    m(
        "<init>",
        "(Ljava/lang/String;)V",
        acc::PUBLIC,
        jl::throwable_init_message,
    ),
];

/// 索引异常构造（(I)V → "Array/String index out of range: N"）。
static INIT_PAIR_INDEX: &[BootstrapMethodDecl] = &[
    m("<init>", "()V", acc::PUBLIC, jl::throwable_init_empty),
    m(
        "<init>",
        "(Ljava/lang/String;)V",
        acc::PUBLIC,
        jl::throwable_init_message,
    ),
    m("<init>", "(I)V", acc::PUBLIC, jl::throwable_init_index),
];

static THROWABLE_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/lang/Throwable",
    access_flags: acc::PUBLIC,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[field("detailMessage", "Ljava/lang/String;", acc::PRIVATE)],
    methods: &[
        m("<init>", "()V", acc::PUBLIC, jl::throwable_init_empty),
        m(
            "<init>",
            "(Ljava/lang/String;)V",
            acc::PUBLIC,
            jl::throwable_init_message,
        ),
        m(
            "getMessage",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            jl::throwable_get_message,
        ),
        m(
            "toString",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            jl::throwable_to_string,
        ),
        m(
            "printStackTrace",
            "()V",
            acc::PUBLIC,
            jl::throwable_print_stack_trace,
        ),
        m(
            "fillInStackTrace",
            "()Ljava/lang/Throwable;",
            acc::PUBLIC,
            jl::throwable_fill_in_stack_trace,
        ),
    ],
};

static EXCEPTION_DECL: BootstrapDecl =
    klass("java/lang/Exception", "java/lang/Throwable", INIT_PAIR);
static RUNTIME_EXCEPTION_DECL: BootstrapDecl = klass(
    "java/lang/RuntimeException",
    "java/lang/Exception",
    INIT_PAIR,
);
static ILLEGAL_ARGUMENT_DECL: BootstrapDecl = klass(
    "java/lang/IllegalArgumentException",
    "java/lang/RuntimeException",
    INIT_PAIR,
);
/// Thread.start 重复调用抛出（T033；JVM 层次：IllegalArgumentException 子类）。
static ILLEGAL_THREAD_STATE_DECL: BootstrapDecl = klass(
    "java/lang/IllegalThreadStateException",
    "java/lang/IllegalArgumentException",
    INIT_PAIR,
);
static ILLEGAL_STATE_DECL: BootstrapDecl = klass(
    "java/lang/IllegalStateException",
    "java/lang/RuntimeException",
    INIT_PAIR,
);
static ILLEGAL_ACCESS_DECL: BootstrapDecl = klass(
    "java/lang/IllegalAccessException",
    "java/lang/Exception",
    INIT_PAIR,
);
static INSTANTIATION_DECL: BootstrapDecl = klass(
    "java/lang/InstantiationException",
    "java/lang/Exception",
    INIT_PAIR,
);
static CLASS_NOT_FOUND_DECL: BootstrapDecl = klass(
    "java/lang/ClassNotFoundException",
    "java/lang/Exception",
    INIT_PAIR,
);
static INTERRUPTED_DECL: BootstrapDecl = klass(
    "java/lang/InterruptedException",
    "java/lang/Exception",
    INIT_PAIR,
);
static CLONE_NOT_SUPPORTED_DECL: BootstrapDecl = klass(
    "java/lang/CloneNotSupportedException",
    "java/lang/Exception",
    INIT_PAIR,
);
static INDEX_OOB_DECL: BootstrapDecl = klass(
    "java/lang/IndexOutOfBoundsException",
    "java/lang/RuntimeException",
    INIT_PAIR,
);
static ARRAY_INDEX_OOB_DECL: BootstrapDecl = klass(
    "java/lang/ArrayIndexOutOfBoundsException",
    "java/lang/IndexOutOfBoundsException",
    INIT_PAIR_INDEX,
);
static STRING_INDEX_OOB_DECL: BootstrapDecl = klass(
    "java/lang/StringIndexOutOfBoundsException",
    "java/lang/IndexOutOfBoundsException",
    INIT_PAIR_INDEX,
);
static NUMBER_FORMAT_DECL: BootstrapDecl = klass(
    "java/lang/NumberFormatException",
    "java/lang/IllegalArgumentException",
    INIT_PAIR,
);
static CLASS_CAST_DECL: BootstrapDecl = klass(
    "java/lang/ClassCastException",
    "java/lang/RuntimeException",
    INIT_PAIR,
);
static ARITHMETIC_DECL: BootstrapDecl = klass(
    "java/lang/ArithmeticException",
    "java/lang/RuntimeException",
    INIT_PAIR,
);
static NEG_ARRAY_SIZE_DECL: BootstrapDecl = klass(
    "java/lang/NegativeArraySizeException",
    "java/lang/RuntimeException",
    INIT_PAIR,
);
static ILLEGAL_MONITOR_DECL: BootstrapDecl = klass(
    "java/lang/IllegalMonitorStateException",
    "java/lang/RuntimeException",
    INIT_PAIR,
);
static ARRAY_STORE_DECL: BootstrapDecl = klass(
    "java/lang/ArrayStoreException",
    "java/lang/RuntimeException",
    INIT_PAIR,
);
static NPE_DECL: BootstrapDecl = klass(
    "java/lang/NullPointerException",
    "java/lang/RuntimeException",
    INIT_PAIR,
);
static UNSUPPORTED_OP_DECL: BootstrapDecl = klass(
    "java/lang/UnsupportedOperationException",
    "java/lang/RuntimeException",
    INIT_PAIR,
);
static LINKAGE_ERROR_DECL: BootstrapDecl =
    klass("java/lang/LinkageError", "java/lang/Error", INIT_PAIR);
static NO_CLASS_DEF_DECL: BootstrapDecl = klass(
    "java/lang/NoClassDefFoundError",
    "java/lang/LinkageError",
    INIT_PAIR,
);
static OOM_DECL: BootstrapDecl = klass("java/lang/OutOfMemoryError", "java/lang/Error", INIT_PAIR);
static ERROR_DECL: BootstrapDecl = klass("java/lang/Error", "java/lang/Throwable", INIT_PAIR);

static STRING_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/lang/String",
    access_flags: acc::PUBLIC | acc::FINAL,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[],
    methods: &[
        m("<init>", "()V", acc::PUBLIC, jl::string_init),
        m(
            "<init>",
            "(Ljava/lang/String;)V",
            acc::PUBLIC,
            jl::string_init_string,
        ),
        m("<init>", "([C)V", acc::PUBLIC, jl::string_init_chars),
        m(
            "<init>",
            "([CII)V",
            acc::PUBLIC,
            jl::string_init_chars_range,
        ),
        m("<init>", "([B)V", acc::PUBLIC, jl::string_init_bytes),
        m(
            "<init>",
            "([BLjava/lang/String;)V",
            acc::PUBLIC,
            jl::string_init_bytes_enc,
        ),
        m("length", "()I", acc::PUBLIC, jl::string_length),
        m("charAt", "(I)C", acc::PUBLIC, jl::string_char_at),
        m(
            "equals",
            "(Ljava/lang/Object;)Z",
            acc::PUBLIC,
            jl::string_equals,
        ),
        m(
            "equalsIgnoreCase",
            "(Ljava/lang/String;)Z",
            acc::PUBLIC,
            jl::string_equals_ignore_case,
        ),
        m("hashCode", "()I", acc::PUBLIC, jl::string_hash_code),
        m("indexOf", "(I)I", acc::PUBLIC, jl::string_index_of),
        m("indexOf", "(II)I", acc::PUBLIC, jl::string_index_of_from),
        m("lastIndexOf", "(I)I", acc::PUBLIC, jl::string_last_index_of),
        m(
            "lastIndexOf",
            "(II)I",
            acc::PUBLIC,
            jl::string_last_index_of_from,
        ),
        m(
            "indexOf",
            "(Ljava/lang/String;)I",
            acc::PUBLIC,
            jl::string_index_of_str,
        ),
        m(
            "indexOf",
            "(Ljava/lang/String;I)I",
            acc::PUBLIC,
            jl::string_index_of_str_from,
        ),
        m(
            "lastIndexOf",
            "(Ljava/lang/String;)I",
            acc::PUBLIC,
            jl::string_last_index_of_str,
        ),
        m(
            "substring",
            "(I)Ljava/lang/String;",
            acc::PUBLIC,
            jl::string_substring_from,
        ),
        m(
            "substring",
            "(II)Ljava/lang/String;",
            acc::PUBLIC,
            jl::string_substring_range,
        ),
        m(
            "concat",
            "(Ljava/lang/String;)Ljava/lang/String;",
            acc::PUBLIC,
            jl::string_concat,
        ),
        m(
            "compareTo",
            "(Ljava/lang/String;)I",
            acc::PUBLIC,
            jl::string_compare_to,
        ),
        m(
            "startsWith",
            "(Ljava/lang/String;)Z",
            acc::PUBLIC,
            jl::string_starts_with,
        ),
        m(
            "endsWith",
            "(Ljava/lang/String;)Z",
            acc::PUBLIC,
            jl::string_ends_with,
        ),
        m(
            "toUpperCase",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            jl::string_to_upper,
        ),
        m(
            "toLowerCase",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            jl::string_to_lower,
        ),
        m("toCharArray", "()[C", acc::PUBLIC, jl::string_to_char_array),
        m("trim", "()Ljava/lang/String;", acc::PUBLIC, jl::string_trim),
        m(
            "toString",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            jl::string_to_string,
        ),
        m(
            "intern",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            jl::string_intern,
        ),
        m(
            "getBytes",
            "(Ljava/lang/String;)[B",
            acc::PUBLIC,
            jl::string_get_bytes,
        ),
        m(
            "valueOf",
            "(Z)Ljava/lang/String;",
            acc::PUBLIC | acc::STATIC,
            jl::string_value_of_bool,
        ),
        m(
            "valueOf",
            "(C)Ljava/lang/String;",
            acc::PUBLIC | acc::STATIC,
            jl::string_value_of_char,
        ),
        m(
            "valueOf",
            "(I)Ljava/lang/String;",
            acc::PUBLIC | acc::STATIC,
            jl::string_value_of_int,
        ),
        m(
            "valueOf",
            "(J)Ljava/lang/String;",
            acc::PUBLIC | acc::STATIC,
            jl::string_value_of_long,
        ),
        m(
            "valueOf",
            "(F)Ljava/lang/String;",
            acc::PUBLIC | acc::STATIC,
            jl::string_value_of_float,
        ),
        m(
            "valueOf",
            "(D)Ljava/lang/String;",
            acc::PUBLIC | acc::STATIC,
            jl::string_value_of_double,
        ),
        m(
            "valueOf",
            "(Ljava/lang/Object;)Ljava/lang/String;",
            acc::PUBLIC | acc::STATIC,
            jl::string_value_of_object,
        ),
        m(
            "valueOf",
            "([C)Ljava/lang/String;",
            acc::PUBLIC | acc::STATIC,
            jl::string_value_of_chars,
        ),
    ],
};

static STRING_BUFFER_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/lang/StringBuffer",
    access_flags: acc::PUBLIC | acc::FINAL,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[
        field("value", "[C", acc::PRIVATE),
        field("count", "I", acc::PRIVATE),
    ],
    methods: &[
        m("<init>", "()V", acc::PUBLIC, jl::sb_init),
        m("<init>", "(I)V", acc::PUBLIC, jl::sb_init_capacity),
        m(
            "<init>",
            "(Ljava/lang/String;)V",
            acc::PUBLIC,
            jl::sb_init_string,
        ),
        m(
            "append",
            "(Z)Ljava/lang/StringBuffer;",
            acc::PUBLIC,
            jl::sb_append_bool,
        ),
        m(
            "append",
            "(C)Ljava/lang/StringBuffer;",
            acc::PUBLIC,
            jl::sb_append_char,
        ),
        m(
            "append",
            "(I)Ljava/lang/StringBuffer;",
            acc::PUBLIC,
            jl::sb_append_int,
        ),
        m(
            "append",
            "(J)Ljava/lang/StringBuffer;",
            acc::PUBLIC,
            jl::sb_append_long,
        ),
        m(
            "append",
            "(F)Ljava/lang/StringBuffer;",
            acc::PUBLIC,
            jl::sb_append_float,
        ),
        m(
            "append",
            "(D)Ljava/lang/StringBuffer;",
            acc::PUBLIC,
            jl::sb_append_double,
        ),
        m(
            "append",
            "(Ljava/lang/String;)Ljava/lang/StringBuffer;",
            acc::PUBLIC,
            jl::sb_append_string,
        ),
        m(
            "append",
            "(Ljava/lang/StringBuffer;)Ljava/lang/StringBuffer;",
            acc::PUBLIC,
            jl::sb_append_string_buffer,
        ),
        m(
            "append",
            "(Ljava/lang/Object;)Ljava/lang/StringBuffer;",
            acc::PUBLIC,
            jl::sb_append_object,
        ),
        m(
            "append",
            "([C)Ljava/lang/StringBuffer;",
            acc::PUBLIC,
            jl::sb_append_chars,
        ),
        m(
            "insert",
            "(IC)Ljava/lang/StringBuffer;",
            acc::PUBLIC,
            jl::sb_insert_char,
        ),
        m(
            "insert",
            "(II)Ljava/lang/StringBuffer;",
            acc::PUBLIC,
            jl::sb_insert_int,
        ),
        m(
            "insert",
            "(IJ)Ljava/lang/StringBuffer;",
            acc::PUBLIC,
            jl::sb_insert_long,
        ),
        m(
            "insert",
            "(ILjava/lang/String;)Ljava/lang/StringBuffer;",
            acc::PUBLIC,
            jl::sb_insert_string,
        ),
        m(
            "insert",
            "(ILjava/lang/Object;)Ljava/lang/StringBuffer;",
            acc::PUBLIC,
            jl::sb_insert_object,
        ),
        m("length", "()I", acc::PUBLIC, jl::sb_length),
        m("charAt", "(I)C", acc::PUBLIC, jl::sb_char_at),
        m("setCharAt", "(IC)V", acc::PUBLIC, jl::sb_set_char_at),
        m("setLength", "(I)V", acc::PUBLIC, jl::sb_set_length),
        m(
            "deleteCharAt",
            "(I)Ljava/lang/StringBuffer;",
            acc::PUBLIC,
            jl::sb_delete_char_at,
        ),
        m(
            "delete",
            "(II)Ljava/lang/StringBuffer;",
            acc::PUBLIC,
            jl::sb_delete,
        ),
        m(
            "reverse",
            "()Ljava/lang/StringBuffer;",
            acc::PUBLIC,
            jl::sb_reverse,
        ),
        m(
            "toString",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            jl::sb_to_string,
        ),
    ],
};

static SYSTEM_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/lang/System",
    access_flags: acc::PUBLIC | acc::FINAL,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[
        field("out", "Ljava/io/PrintStream;", acc::PUBLIC | acc::STATIC),
        field("err", "Ljava/io/PrintStream;", acc::PUBLIC | acc::STATIC),
        // in 零值即 null（宿主 stdin 桥接 TODO T045）。
        field("in", "Ljava/io/InputStream;", acc::PUBLIC | acc::STATIC),
    ],
    methods: &[
        // 原生 <clinit>：构造 out/err（fd=1 → 宿主 stderr；解释器
        // 原生 clinit 分派见 interp::advance_clinit_chain）。
        m("<clinit>", "()V", acc::STATIC, jio::system_clinit),
        m(
            "arraycopy",
            "(Ljava/lang/Object;ILjava/lang/Object;II)V",
            acc::PUBLIC | acc::STATIC,
            jl::system_arraycopy,
        ),
        mh(
            "currentTimeMillis",
            "()J",
            acc::PUBLIC | acc::STATIC,
            jl::system_current_time_millis,
        ),
        m(
            "getProperty",
            "(Ljava/lang/String;)Ljava/lang/String;",
            acc::PUBLIC | acc::STATIC,
            jl::system_get_property,
        ),
        // exit(N)：T045 桥接生命周期通道——置强制销毁请求（回调链收尾
        // 而非进程级终止，语义取舍见 midp::midlet 模块注释）。
        m(
            "exit",
            "(I)V",
            acc::PUBLIC | acc::STATIC,
            jmidlet::system_exit_bridge,
        ),
        m("gc", "()V", acc::PUBLIC | acc::STATIC, jl::system_gc),
    ],
};

static MATH_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/lang/Math",
    access_flags: acc::PUBLIC | acc::FINAL,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[
        const_field("PI", "D", BootstrapConst::Double(std::f64::consts::PI)),
        const_field("E", "D", BootstrapConst::Double(std::f64::consts::E)),
    ],
    methods: &[
        m("abs", "(I)I", acc::PUBLIC | acc::STATIC, jl::math_abs_int),
        m("abs", "(J)J", acc::PUBLIC | acc::STATIC, jl::math_abs_long),
        m("abs", "(F)F", acc::PUBLIC | acc::STATIC, jl::math_abs_float),
        m(
            "abs",
            "(D)D",
            acc::PUBLIC | acc::STATIC,
            jl::math_abs_double,
        ),
        m("min", "(II)I", acc::PUBLIC | acc::STATIC, jl::math_min_int),
        m("max", "(II)I", acc::PUBLIC | acc::STATIC, jl::math_max_int),
        m("min", "(JJ)J", acc::PUBLIC | acc::STATIC, jl::math_min_long),
        m("max", "(JJ)J", acc::PUBLIC | acc::STATIC, jl::math_max_long),
        m(
            "min",
            "(FF)F",
            acc::PUBLIC | acc::STATIC,
            jl::math_min_float,
        ),
        m(
            "max",
            "(FF)F",
            acc::PUBLIC | acc::STATIC,
            jl::math_max_float,
        ),
        m(
            "min",
            "(DD)D",
            acc::PUBLIC | acc::STATIC,
            jl::math_min_double,
        ),
        m(
            "max",
            "(DD)D",
            acc::PUBLIC | acc::STATIC,
            jl::math_max_double,
        ),
        m("sqrt", "(D)D", acc::PUBLIC | acc::STATIC, jl::math_sqrt),
        m("sin", "(D)D", acc::PUBLIC | acc::STATIC, jl::math_sin),
        m("cos", "(D)D", acc::PUBLIC | acc::STATIC, jl::math_cos),
        m("tan", "(D)D", acc::PUBLIC | acc::STATIC, jl::math_tan),
        m("asin", "(D)D", acc::PUBLIC | acc::STATIC, jl::math_asin),
        m("acos", "(D)D", acc::PUBLIC | acc::STATIC, jl::math_acos),
        m("atan", "(D)D", acc::PUBLIC | acc::STATIC, jl::math_atan),
        m("atan2", "(DD)D", acc::PUBLIC | acc::STATIC, jl::math_atan2),
        m("pow", "(DD)D", acc::PUBLIC | acc::STATIC, jl::math_pow),
        m("floor", "(D)D", acc::PUBLIC | acc::STATIC, jl::math_floor),
        m("ceil", "(D)D", acc::PUBLIC | acc::STATIC, jl::math_ceil),
        m(
            "round",
            "(D)J",
            acc::PUBLIC | acc::STATIC,
            jl::math_round_double,
        ),
        m(
            "round",
            "(F)I",
            acc::PUBLIC | acc::STATIC,
            jl::math_round_float,
        ),
        m(
            "toRadians",
            "(D)D",
            acc::PUBLIC | acc::STATIC,
            jl::math_to_radians,
        ),
        m(
            "toDegrees",
            "(D)D",
            acc::PUBLIC | acc::STATIC,
            jl::math_to_degrees,
        ),
    ],
};

/// java/lang/Thread（T033 真实现，cldc::thread）：
/// 实例字段 target/name/priority/interrupted/started；`start()` 把
/// this 压 cs.pending_threads 请求队列（真 spawn 由 Vm.poll 的
/// spawn_pending 消费）；sleep/yield/join 经 KvmError::Yield 让出
/// 协议上抛（join 为 2ms 轮询版）。
static THREAD_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/lang/Thread",
    access_flags: acc::PUBLIC,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[
        field("target", "Ljava/lang/Runnable;", acc::PRIVATE),
        field("name", "Ljava/lang/String;", acc::PRIVATE),
        field("priority", "I", acc::PRIVATE),
        field("interrupted", "Z", acc::PRIVATE),
        // start 幂等守卫：已 start 再 start → IllegalThreadStateException。
        field("started", "Z", acc::PRIVATE),
    ],
    methods: &[
        m("<init>", "()V", acc::PUBLIC, jt::thread_init),
        m(
            "<init>",
            "(Ljava/lang/Runnable;)V",
            acc::PUBLIC,
            jt::thread_init_runnable,
        ),
        m(
            "<init>",
            "(Ljava/lang/Runnable;Ljava/lang/String;)V",
            acc::PUBLIC,
            jt::thread_init_runnable_name,
        ),
        m(
            "<init>",
            "(Ljava/lang/String;)V",
            acc::PUBLIC,
            jt::thread_init_name,
        ),
        m(
            "currentThread",
            "()Ljava/lang/Thread;",
            acc::PUBLIC | acc::STATIC,
            jt::thread_current_thread,
        ),
        m(
            "getName",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            jt::thread_get_name,
        ),
        m("setPriority", "(I)V", acc::PUBLIC, jt::thread_set_priority),
        m("getPriority", "()I", acc::PUBLIC, jt::thread_get_priority),
        m("interrupt", "()V", acc::PUBLIC, jt::thread_interrupt),
        m(
            "isInterrupted",
            "()Z",
            acc::PUBLIC,
            jt::thread_is_interrupted,
        ),
        m("isAlive", "()Z", acc::PUBLIC, jt::thread_is_alive),
        // join/sleep 需宿主时钟（now_ms），走 Host 轨（mh 速记）。
        mh("join", "()V", acc::PUBLIC | acc::FINAL, jt::thread_join),
        mh("sleep", "(J)V", acc::PUBLIC | acc::STATIC, jt::thread_sleep),
        m("start", "()V", acc::PUBLIC, jt::thread_start),
        m("yield", "()V", acc::PUBLIC | acc::STATIC, jt::thread_yield),
    ],
};

static RUNNABLE_DECL: BootstrapDecl = iface("java/lang/Runnable", &[]);

static BOOLEAN_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/lang/Boolean",
    access_flags: acc::PUBLIC | acc::FINAL,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[field("value", "Z", acc::PRIVATE)],
    methods: &[
        m("<init>", "(Z)V", acc::PUBLIC, jl::boolean_init),
        m("booleanValue", "()Z", acc::PUBLIC, jl::boolean_value),
        m(
            "toString",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            jl::boolean_to_string,
        ),
        m(
            "valueOf",
            "(Z)Ljava/lang/Boolean;",
            acc::PUBLIC | acc::STATIC,
            jl::boolean_value_of,
        ),
    ],
};

static CHARACTER_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/lang/Character",
    access_flags: acc::PUBLIC | acc::FINAL,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[field("value", "C", acc::PRIVATE)],
    methods: &[
        m(
            "isDigit",
            "(C)Z",
            acc::PUBLIC | acc::STATIC,
            jl::character_is_digit,
        ),
        m(
            "isLetter",
            "(C)Z",
            acc::PUBLIC | acc::STATIC,
            jl::character_is_letter,
        ),
        m(
            "isLetterOrDigit",
            "(C)Z",
            acc::PUBLIC | acc::STATIC,
            jl::character_is_letter_or_digit,
        ),
        m(
            "isWhitespace",
            "(C)Z",
            acc::PUBLIC | acc::STATIC,
            jl::character_is_whitespace,
        ),
        m(
            "isLowerCase",
            "(C)Z",
            acc::PUBLIC | acc::STATIC,
            jl::character_is_lower,
        ),
        m(
            "isUpperCase",
            "(C)Z",
            acc::PUBLIC | acc::STATIC,
            jl::character_is_upper,
        ),
        m(
            "toLowerCase",
            "(C)C",
            acc::PUBLIC | acc::STATIC,
            jl::character_to_lower,
        ),
        m(
            "toUpperCase",
            "(C)C",
            acc::PUBLIC | acc::STATIC,
            jl::character_to_upper,
        ),
        m(
            "digit",
            "(CI)I",
            acc::PUBLIC | acc::STATIC,
            jl::character_digit,
        ),
        m("<init>", "(C)V", acc::PUBLIC, jl::character_init),
        m("charValue", "()C", acc::PUBLIC, jl::character_char_value),
        m(
            "toString",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            jl::character_to_string,
        ),
    ],
};

static INTEGER_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/lang/Integer",
    access_flags: acc::PUBLIC | acc::FINAL,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[field("value", "I", acc::PRIVATE)],
    methods: &[
        m("<init>", "(I)V", acc::PUBLIC, jl::integer_init),
        m("intValue", "()I", acc::PUBLIC, jl::integer_int_value),
        m(
            "equals",
            "(Ljava/lang/Object;)Z",
            acc::PUBLIC,
            jl::integer_equals,
        ),
        m("hashCode", "()I", acc::PUBLIC, jl::integer_hash_code),
        m(
            "parseInt",
            "(Ljava/lang/String;)I",
            acc::PUBLIC | acc::STATIC,
            jl::integer_parse_int,
        ),
        m(
            "parseInt",
            "(Ljava/lang/String;I)I",
            acc::PUBLIC | acc::STATIC,
            jl::integer_parse_int_radix,
        ),
        m(
            "toString",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            jl::integer_to_string,
        ),
        m(
            "toString",
            "(I)Ljava/lang/String;",
            acc::PUBLIC | acc::STATIC,
            jl::integer_to_string_static,
        ),
        m(
            "toString",
            "(II)Ljava/lang/String;",
            acc::PUBLIC | acc::STATIC,
            jl::integer_to_string_radix,
        ),
        m(
            "toHexString",
            "(I)Ljava/lang/String;",
            acc::PUBLIC | acc::STATIC,
            jl::integer_to_hex_string,
        ),
        m(
            "valueOf",
            "(I)Ljava/lang/Integer;",
            acc::PUBLIC | acc::STATIC,
            jl::integer_value_of,
        ),
        m(
            "valueOf",
            "(Ljava/lang/String;)Ljava/lang/Integer;",
            acc::PUBLIC | acc::STATIC,
            jl::integer_value_of_string,
        ),
    ],
};

static LONG_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/lang/Long",
    access_flags: acc::PUBLIC | acc::FINAL,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[field("value", "J", acc::PRIVATE)],
    methods: &[
        m("<init>", "(J)V", acc::PUBLIC, jl::long_init),
        m("longValue", "()J", acc::PUBLIC, jl::long_value),
        m(
            "parseLong",
            "(Ljava/lang/String;)J",
            acc::PUBLIC | acc::STATIC,
            jl::long_parse_long,
        ),
        m(
            "toString",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            jl::long_to_string,
        ),
        m(
            "toString",
            "(J)Ljava/lang/String;",
            acc::PUBLIC | acc::STATIC,
            jl::long_to_string_static,
        ),
        m(
            "valueOf",
            "(J)Ljava/lang/Long;",
            acc::PUBLIC | acc::STATIC,
            jl::long_value_of,
        ),
    ],
};

static FLOAT_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/lang/Float",
    access_flags: acc::PUBLIC | acc::FINAL,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[field("value", "F", acc::PRIVATE)],
    methods: &[
        m("<init>", "(F)V", acc::PUBLIC, jl::float_init),
        m("floatValue", "()F", acc::PUBLIC, jl::float_value),
        m(
            "parseFloat",
            "(Ljava/lang/String;)F",
            acc::PUBLIC | acc::STATIC,
            jl::float_parse_float,
        ),
        m(
            "toString",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            jl::float_to_string,
        ),
        m(
            "toString",
            "(F)Ljava/lang/String;",
            acc::PUBLIC | acc::STATIC,
            jl::float_to_string_static,
        ),
        m(
            "valueOf",
            "(F)Ljava/lang/Float;",
            acc::PUBLIC | acc::STATIC,
            jl::float_value_of,
        ),
        m("isNaN", "(F)Z", acc::PUBLIC | acc::STATIC, jl::float_is_nan),
        m(
            "isInfinite",
            "(F)Z",
            acc::PUBLIC | acc::STATIC,
            jl::float_is_infinite,
        ),
    ],
};

static DOUBLE_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/lang/Double",
    access_flags: acc::PUBLIC | acc::FINAL,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[field("value", "D", acc::PRIVATE)],
    methods: &[
        m("<init>", "(D)V", acc::PUBLIC, jl::double_init),
        m("doubleValue", "()D", acc::PUBLIC, jl::double_value),
        m(
            "parseDouble",
            "(Ljava/lang/String;)D",
            acc::PUBLIC | acc::STATIC,
            jl::double_parse_double,
        ),
        m(
            "toString",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            jl::double_to_string,
        ),
        m(
            "toString",
            "(D)Ljava/lang/String;",
            acc::PUBLIC | acc::STATIC,
            jl::double_to_string_static,
        ),
        m(
            "valueOf",
            "(D)Ljava/lang/Double;",
            acc::PUBLIC | acc::STATIC,
            jl::double_value_of,
        ),
        m(
            "isNaN",
            "(D)Z",
            acc::PUBLIC | acc::STATIC,
            jl::double_is_nan,
        ),
        m(
            "isInfinite",
            "(D)Z",
            acc::PUBLIC | acc::STATIC,
            jl::double_is_infinite,
        ),
    ],
};

// ---------------------------------------------------------------------------
// java/io（T016：真实现，绑定 crate::cldc::io）
// ---------------------------------------------------------------------------

/// DataInput 接口（DataInputStream 实现；占签名供层次/链接解析）。
static DATA_INPUT_IFACE_DECL: BootstrapDecl = iface(
    "java/io/DataInput",
    &[
        m(
            "readBoolean",
            "()Z",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "readByte",
            "()B",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "readChar",
            "()C",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "readDouble",
            "()D",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "readFloat",
            "()F",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "readFully",
            "([B)V",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "readFully",
            "([BII)V",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "readInt",
            "()I",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "readLine",
            "()Ljava/lang/String;",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "readLong",
            "()J",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "readShort",
            "()S",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "readUTF",
            "()Ljava/lang/String;",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "readUnsignedByte",
            "()I",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "readUnsignedShort",
            "()I",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "skipBytes",
            "(I)I",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
    ],
);

/// DataOutput 接口（DataOutputStream 实现）。
static DATA_OUTPUT_IFACE_DECL: BootstrapDecl = iface(
    "java/io/DataOutput",
    &[
        m(
            "write",
            "([B)V",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "write",
            "([BII)V",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "write",
            "(I)V",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "writeBoolean",
            "(Z)V",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "writeByte",
            "(I)V",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "writeChar",
            "(I)V",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "writeDouble",
            "(D)V",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "writeFloat",
            "(F)V",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "writeInt",
            "(I)V",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "writeLong",
            "(J)V",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "writeShort",
            "(I)V",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "writeUTF",
            "(Ljava/lang/String;)V",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
    ],
);

/// InputStream 骨架：读侧抽象；close/skip/available 提供缺省语义
///（虚分派按接收者运行时类解析，具体子类声明优先）。
static INPUT_STREAM_DECL: BootstrapDecl = klass(
    "java/io/InputStream",
    "java/lang/Object",
    &[
        m("available", "()I", acc::PUBLIC, jio::input_available_stub),
        m("close", "()V", acc::PUBLIC, jio::io_close),
        m(
            "read",
            "()I",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "read",
            "([B)I",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "read",
            "([BII)I",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m("skip", "(J)J", acc::PUBLIC, jio::input_skip_stub),
    ],
);

/// OutputStream 骨架：write 抽象；close/flush 空操作（内存流无资源）。
static OUTPUT_STREAM_DECL: BootstrapDecl = klass(
    "java/io/OutputStream",
    "java/lang/Object",
    &[
        m("close", "()V", acc::PUBLIC, jio::io_close),
        m("flush", "()V", acc::PUBLIC, jio::io_flush),
        m(
            "write",
            "(I)V",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "write",
            "([B)V",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "write",
            "([BII)V",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
    ],
);

/// Reader 骨架（CLDC：close + read 两形态）。
static READER_DECL: BootstrapDecl = klass(
    "java/io/Reader",
    "java/lang/Object",
    &[
        m(
            "close",
            "()V",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "read",
            "()I",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "read",
            "([CII)I",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
    ],
);

/// Writer 骨架（CLDC：close/flush + write 全族）。
static WRITER_DECL: BootstrapDecl = klass(
    "java/io/Writer",
    "java/lang/Object",
    &[
        m(
            "close",
            "()V",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "flush",
            "()V",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "write",
            "(I)V",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "write",
            "([C)V",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "write",
            "([CII)V",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "write",
            "(Ljava/lang/String;)V",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "write",
            "(Ljava/lang/String;II)V",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
    ],
);

static FILTER_INPUT_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/io/FilterInputStream",
    access_flags: acc::PUBLIC,
    super_name: Some("java/io/InputStream"),
    interfaces: &[],
    fields: &[field("in", "Ljava/io/InputStream;", acc::PRIVATE)],
    methods: &[
        // 读侧共享 DIS 实现（仅依赖 in 字段，与接收者类无关）。
        m("read", "()I", acc::PUBLIC, jio::dis_read_single),
        m("read", "([B)I", acc::PUBLIC, jio::dis_read_array),
        m("read", "([BII)I", acc::PUBLIC, jio::dis_read_range),
        m("skip", "(J)J", acc::PUBLIC, jio::dis_skip),
        m("available", "()I", acc::PUBLIC, jio::dis_available),
    ],
};

static FILTER_OUTPUT_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/io/FilterOutputStream",
    access_flags: acc::PUBLIC,
    super_name: Some("java/io/OutputStream"),
    interfaces: &[],
    fields: &[field("out", "Ljava/io/OutputStream;", acc::PRIVATE)],
    methods: &[
        m("write", "(I)V", acc::PUBLIC, jio::fos_write_int),
        m("write", "([B)V", acc::PUBLIC, jio::fos_write_all),
        m("write", "([BII)V", acc::PUBLIC, jio::fos_write_range),
        // flush 经底层递归对内存流等价空操作（宿主桥接 TODO T045）。
        m("flush", "()V", acc::PUBLIC, jio::io_flush),
    ],
};

static BYTE_ARRAY_INPUT_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/io/ByteArrayInputStream",
    access_flags: acc::PUBLIC,
    super_name: Some("java/io/InputStream"),
    interfaces: &[],
    fields: &[
        field("buf", "[B", acc::PRIVATE),
        field("pos", "I", acc::PRIVATE),
        field("count", "I", acc::PRIVATE),
        field("markPos", "I", acc::PRIVATE),
    ],
    methods: &[
        m("<init>", "([B)V", acc::PUBLIC, jio::bais_init),
        m("available", "()I", acc::PUBLIC, jio::bais_available),
        m("mark", "(I)V", acc::PUBLIC, jio::bais_mark),
        m("read", "()I", acc::PUBLIC, jio::bais_read_single),
        m("read", "([B)I", acc::PUBLIC, jio::bais_read_array),
        m("read", "([BII)I", acc::PUBLIC, jio::bais_read_range),
        m("reset", "()V", acc::PUBLIC, jio::bais_reset),
        m("skip", "(J)J", acc::PUBLIC, jio::bais_skip),
    ],
};

static BYTE_ARRAY_OUTPUT_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/io/ByteArrayOutputStream",
    access_flags: acc::PUBLIC,
    super_name: Some("java/io/OutputStream"),
    interfaces: &[],
    fields: &[
        field("buf", "[B", acc::PRIVATE),
        field("count", "I", acc::PRIVATE),
    ],
    methods: &[
        m("<init>", "()V", acc::PUBLIC, jio::baos_init_empty),
        m("<init>", "(I)V", acc::PUBLIC, jio::baos_init_size),
        m("reset", "()V", acc::PUBLIC, jio::baos_reset),
        m("size", "()I", acc::PUBLIC, jio::baos_size),
        m("toByteArray", "()[B", acc::PUBLIC, jio::baos_to_byte_array),
        m(
            "toString",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            jio::baos_to_string,
        ),
        m("write", "(I)V", acc::PUBLIC, jio::baos_write_int),
        m("write", "([B)V", acc::PUBLIC, jio::baos_write_all),
        m("write", "([BII)V", acc::PUBLIC, jio::baos_write_range),
    ],
};

static DATA_INPUT_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/io/DataInputStream",
    access_flags: acc::PUBLIC,
    super_name: Some("java/io/FilterInputStream"),
    interfaces: &["java/io/DataInput"],
    fields: &[],
    methods: &[
        m(
            "<init>",
            "(Ljava/io/InputStream;)V",
            acc::PUBLIC,
            jio::dis_init,
        ),
        m("available", "()I", acc::PUBLIC, jio::dis_available),
        m("read", "()I", acc::PUBLIC, jio::dis_read_single),
        m("read", "([B)I", acc::PUBLIC, jio::dis_read_array),
        m("read", "([BII)I", acc::PUBLIC, jio::dis_read_range),
        m("readBoolean", "()Z", acc::PUBLIC, jio::dis_read_boolean),
        m("readByte", "()B", acc::PUBLIC, jio::dis_read_byte),
        m("readChar", "()C", acc::PUBLIC, jio::dis_read_char),
        m("readDouble", "()D", acc::PUBLIC, jio::dis_read_double),
        m("readFloat", "()F", acc::PUBLIC, jio::dis_read_float),
        m("readFully", "([B)V", acc::PUBLIC, jio::dis_read_fully_all),
        m(
            "readFully",
            "([BII)V",
            acc::PUBLIC,
            jio::dis_read_fully_range,
        ),
        m("readInt", "()I", acc::PUBLIC, jio::dis_read_int),
        m(
            "readLine",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            jio::dis_read_line,
        ),
        m("readLong", "()J", acc::PUBLIC, jio::dis_read_long),
        m("readShort", "()S", acc::PUBLIC, jio::dis_read_short),
        m(
            "readUTF",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            jio::dis_read_utf,
        ),
        m(
            "readUnsignedByte",
            "()I",
            acc::PUBLIC,
            jio::dis_read_unsigned_byte,
        ),
        m(
            "readUnsignedShort",
            "()I",
            acc::PUBLIC,
            jio::dis_read_unsigned_short,
        ),
        m("skip", "(J)J", acc::PUBLIC, jio::dis_skip),
        m("skipBytes", "(I)I", acc::PUBLIC, jio::dis_skip_bytes),
    ],
};

static DATA_OUTPUT_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/io/DataOutputStream",
    access_flags: acc::PUBLIC,
    super_name: Some("java/io/FilterOutputStream"),
    interfaces: &["java/io/DataOutput"],
    fields: &[field("written", "I", acc::PRIVATE)],
    methods: &[
        m(
            "<init>",
            "(Ljava/io/OutputStream;)V",
            acc::PUBLIC,
            jio::dos_init,
        ),
        m("flush", "()V", acc::PUBLIC, jio::io_flush),
        m("size", "()I", acc::PUBLIC, jio::dos_size),
        m("write", "(I)V", acc::PUBLIC, jio::dos_write_int),
        m("write", "([B)V", acc::PUBLIC, jio::dos_write_all),
        m("write", "([BII)V", acc::PUBLIC, jio::dos_write_range),
        m("writeBoolean", "(Z)V", acc::PUBLIC, jio::dos_write_boolean),
        // writeByte = 单字节截断写（与 write(I) 同实现）。
        m("writeByte", "(I)V", acc::PUBLIC, jio::dos_write_int),
        m("writeChar", "(I)V", acc::PUBLIC, jio::dos_write_char),
        m("writeDouble", "(D)V", acc::PUBLIC, jio::dos_write_double),
        m("writeFloat", "(F)V", acc::PUBLIC, jio::dos_write_float),
        m("writeInt", "(I)V", acc::PUBLIC, jio::dos_write_int_value),
        m("writeLong", "(J)V", acc::PUBLIC, jio::dos_write_long),
        m("writeShort", "(I)V", acc::PUBLIC, jio::dos_write_short),
        m(
            "writeUTF",
            "(Ljava/lang/String;)V",
            acc::PUBLIC,
            jio::dos_write_utf,
        ),
    ],
};

static PRINT_STREAM_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/io/PrintStream",
    access_flags: acc::PUBLIC,
    super_name: Some("java/io/OutputStream"),
    interfaces: &[],
    fields: &[
        field("out", "Ljava/io/OutputStream;", acc::PRIVATE),
        field("fd", "I", acc::PRIVATE),
    ],
    methods: &[
        m(
            "<init>",
            "(Ljava/io/OutputStream;)V",
            acc::PUBLIC,
            jio::ps_init_stream,
        ),
        m("print", "(C)V", acc::PUBLIC, jio::ps_print_char),
        m("print", "(D)V", acc::PUBLIC, jio::ps_print_double),
        m("print", "(F)V", acc::PUBLIC, jio::ps_print_float),
        m("print", "(I)V", acc::PUBLIC, jio::ps_print_int),
        m("print", "(J)V", acc::PUBLIC, jio::ps_print_long),
        m(
            "print",
            "(Ljava/lang/Object;)V",
            acc::PUBLIC,
            jio::ps_print_object,
        ),
        m(
            "print",
            "(Ljava/lang/String;)V",
            acc::PUBLIC,
            jio::ps_print_string,
        ),
        m("print", "(Z)V", acc::PUBLIC, jio::ps_print_boolean),
        m("println", "()V", acc::PUBLIC, jio::ps_println_empty),
        m("println", "(C)V", acc::PUBLIC, jio::ps_println_char),
        m("println", "(D)V", acc::PUBLIC, jio::ps_println_double),
        m("println", "(F)V", acc::PUBLIC, jio::ps_println_float),
        m("println", "(I)V", acc::PUBLIC, jio::ps_println_int),
        m("println", "(J)V", acc::PUBLIC, jio::ps_println_long),
        m(
            "println",
            "(Ljava/lang/Object;)V",
            acc::PUBLIC,
            jio::ps_println_object,
        ),
        m(
            "println",
            "(Ljava/lang/String;)V",
            acc::PUBLIC,
            jio::ps_println_string,
        ),
        m("println", "(Z)V", acc::PUBLIC, jio::ps_println_boolean),
        m("write", "(I)V", acc::PUBLIC, jio::ps_write_int),
        m("write", "([BII)V", acc::PUBLIC, jio::ps_write_range),
    ],
};

static INPUT_STREAM_READER_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/io/InputStreamReader",
    access_flags: acc::PUBLIC,
    super_name: Some("java/io/Reader"),
    interfaces: &[],
    fields: &[
        field("in", "Ljava/io/InputStream;", acc::PRIVATE),
        field("encoding", "I", acc::PRIVATE),
        field("pending", "I", acc::PRIVATE),
    ],
    methods: &[
        m(
            "<init>",
            "(Ljava/io/InputStream;)V",
            acc::PUBLIC,
            jio::isr_init_default,
        ),
        m(
            "<init>",
            "(Ljava/io/InputStream;Ljava/lang/String;)V",
            acc::PUBLIC,
            jio::isr_init_named,
        ),
        m("close", "()V", acc::PUBLIC, jio::io_close),
        m(
            "getEncoding",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            jio::isr_get_encoding,
        ),
        m("read", "()I", acc::PUBLIC, jio::isr_read_single),
        m("read", "([CII)I", acc::PUBLIC, jio::isr_read_range),
    ],
};

static OUTPUT_STREAM_WRITER_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/io/OutputStreamWriter",
    access_flags: acc::PUBLIC,
    super_name: Some("java/io/Writer"),
    interfaces: &[],
    fields: &[
        field("out", "Ljava/io/OutputStream;", acc::PRIVATE),
        field("encoding", "I", acc::PRIVATE),
    ],
    methods: &[
        m(
            "<init>",
            "(Ljava/io/OutputStream;)V",
            acc::PUBLIC,
            jio::osw_init_default,
        ),
        m(
            "<init>",
            "(Ljava/io/OutputStream;Ljava/lang/String;)V",
            acc::PUBLIC,
            jio::osw_init_named,
        ),
        m("close", "()V", acc::PUBLIC, jio::io_close),
        m("flush", "()V", acc::PUBLIC, jio::io_flush),
        m(
            "getEncoding",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            jio::osw_get_encoding,
        ),
        m("write", "(I)V", acc::PUBLIC, jio::osw_write_int),
        m("write", "([C)V", acc::PUBLIC, jio::osw_write_chars_all),
        m("write", "([CII)V", acc::PUBLIC, jio::osw_write_chars_range),
        m(
            "write",
            "(Ljava/lang/String;)V",
            acc::PUBLIC,
            jio::osw_write_string,
        ),
        m(
            "write",
            "(Ljava/lang/String;II)V",
            acc::PUBLIC,
            jio::osw_write_string_range,
        ),
    ],
};

static IO_EXCEPTION_DECL: BootstrapDecl =
    klass("java/io/IOException", "java/lang/Exception", INIT_PAIR);
static UNSUPPORTED_ENCODING_DECL: BootstrapDecl = klass(
    "java/io/UnsupportedEncodingException",
    "java/io/IOException",
    INIT_PAIR,
);
static EOF_DECL: BootstrapDecl = klass("java/io/EOFException", "java/io/IOException", INIT_PAIR);
static UTF_DATA_FORMAT_DECL: BootstrapDecl = klass(
    "java/io/UTFDataFormatException",
    "java/io/IOException",
    INIT_PAIR,
);

/// InterruptedIOException：携带 bytesTransferred（JDK 语义，本引擎
/// 无阻塞 IO，暂无原生路径写入）。
static INTERRUPTED_IO_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/io/InterruptedIOException",
    access_flags: acc::PUBLIC,
    super_name: Some("java/io/IOException"),
    interfaces: &[],
    fields: &[field("bytesTransferred", "I", acc::PUBLIC)],
    methods: INIT_PAIR,
};

// ---------------------------------------------------------------------------
// java/util（T015：真实现，绑定 crate::cldc::util）
// ---------------------------------------------------------------------------

static CALENDAR_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/util/Calendar",
    access_flags: acc::PUBLIC,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[field("time", "J", acc::PRIVATE)],
    methods: &[
        m(
            "getInstance",
            "()Ljava/util/Calendar;",
            acc::PUBLIC | acc::STATIC,
            ju::calendar_get_instance,
        ),
        m("get", "(I)I", acc::PUBLIC, ju::calendar_get),
        m("set", "(II)V", acc::PUBLIC, ju::calendar_set),
        m(
            "getTime",
            "()Ljava/util/Date;",
            acc::PUBLIC,
            ju::calendar_get_time,
        ),
        m(
            "setTime",
            "(Ljava/util/Date;)V",
            acc::PUBLIC,
            ju::calendar_set_time,
        ),
        m(
            "getTimeInMillis",
            "()J",
            acc::PUBLIC,
            ju::calendar_get_time_in_millis,
        ),
        m(
            "setTimeInMillis",
            "(J)V",
            acc::PUBLIC,
            ju::calendar_set_time_in_millis,
        ),
    ],
};

static DATE_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/util/Date",
    access_flags: acc::PUBLIC,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[field("time", "J", acc::PRIVATE)],
    methods: &[
        m("<init>", "()V", acc::PUBLIC, ju::date_init),
        m("<init>", "(J)V", acc::PUBLIC, ju::date_init_long),
        m("getTime", "()J", acc::PUBLIC, ju::date_get_time),
        m("setTime", "(J)V", acc::PUBLIC, ju::date_set_time),
        m(
            "toString",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            ju::date_to_string,
        ),
        m("after", "(Ljava/util/Date;)Z", acc::PUBLIC, ju::date_after),
        m(
            "before",
            "(Ljava/util/Date;)Z",
            acc::PUBLIC,
            ju::date_before,
        ),
        m(
            "equals",
            "(Ljava/lang/Object;)Z",
            acc::PUBLIC,
            ju::date_equals,
        ),
        m(
            "compareTo",
            "(Ljava/util/Date;)I",
            acc::PUBLIC,
            ju::date_compare_to,
        ),
        m("hashCode", "()I", acc::PUBLIC, ju::date_hash_code),
    ],
};

static TIMEZONE_DECL: BootstrapDecl = klass(
    "java/util/TimeZone",
    "java/lang/Object",
    &[
        m(
            "getDefault",
            "()Ljava/util/TimeZone;",
            acc::PUBLIC | acc::STATIC,
            ju::timezone_get_default,
        ),
        m(
            "getID",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            ju::timezone_get_id,
        ),
        m(
            "getRawOffset",
            "()I",
            acc::PUBLIC,
            ju::timezone_get_raw_offset,
        ),
        m(
            "getOffset",
            "(IIIIII)I",
            acc::PUBLIC,
            ju::timezone_get_offset,
        ),
        m(
            "usesDaylightTime",
            "()Z",
            acc::PUBLIC,
            ju::timezone_uses_daylight_time,
        ),
    ],
);

static HASHTABLE_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/util/Hashtable",
    access_flags: acc::PUBLIC,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[
        field("keys", "[Ljava/lang/Object;", acc::PRIVATE),
        field("values", "[Ljava/lang/Object;", acc::PRIVATE),
        field("count", "I", acc::PRIVATE),
    ],
    methods: &[
        m("<init>", "()V", acc::PUBLIC, ju::ht_init),
        m("<init>", "(I)V", acc::PUBLIC, ju::ht_init_capacity),
        m(
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            acc::PUBLIC,
            ju::ht_get,
        ),
        m(
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            acc::PUBLIC,
            ju::ht_put,
        ),
        m(
            "remove",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            acc::PUBLIC,
            ju::ht_remove,
        ),
        m(
            "containsKey",
            "(Ljava/lang/Object;)Z",
            acc::PUBLIC,
            ju::ht_contains_key,
        ),
        m(
            "contains",
            "(Ljava/lang/Object;)Z",
            acc::PUBLIC,
            ju::ht_contains,
        ),
        m("size", "()I", acc::PUBLIC, ju::ht_size),
        m("isEmpty", "()Z", acc::PUBLIC, ju::ht_is_empty),
        m("clear", "()V", acc::PUBLIC, ju::ht_clear),
        m(
            "keys",
            "()Ljava/util/Enumeration;",
            acc::PUBLIC,
            ju::ht_keys,
        ),
        m(
            "elements",
            "()Ljava/util/Enumeration;",
            acc::PUBLIC,
            ju::ht_elements,
        ),
    ],
};

static ENUMERATION_DECL: BootstrapDecl = iface(
    "java/util/Enumeration",
    &[
        m(
            "hasMoreElements",
            "()Z",
            acc::PUBLIC | acc::ABSTRACT,
            stub_zero,
        ),
        m(
            "nextElement",
            "()Ljava/lang/Object;",
            acc::PUBLIC | acc::ABSTRACT,
            stub_null,
        ),
    ],
);

/// Enumeration 游标实现（VM 内部引导类；快照语义）。
static ENUMERATOR_DECL: BootstrapDecl = BootstrapDecl {
    name: "com/skyengine/kvm/util/Enumerator",
    access_flags: acc::PUBLIC,
    super_name: Some("java/lang/Object"),
    interfaces: &["java/util/Enumeration"],
    fields: &[
        field("data", "[Ljava/lang/Object;", acc::PRIVATE),
        field("cursor", "I", acc::PRIVATE),
    ],
    methods: &[
        m(
            "hasMoreElements",
            "()Z",
            acc::PUBLIC,
            ju::enumerator_has_more,
        ),
        m(
            "nextElement",
            "()Ljava/lang/Object;",
            acc::PUBLIC,
            ju::enumerator_next,
        ),
    ],
};

static NO_SUCH_ELEMENT_DECL: BootstrapDecl = klass(
    "java/util/NoSuchElementException",
    "java/lang/RuntimeException",
    INIT_PAIR,
);

static EMPTY_STACK_DECL: BootstrapDecl = klass(
    "java/util/EmptyStackException",
    "java/lang/RuntimeException",
    INIT_PAIR,
);

static RANDOM_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/util/Random",
    access_flags: acc::PUBLIC,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[field("seed", "J", acc::PRIVATE)],
    methods: &[
        m("<init>", "()V", acc::PUBLIC, ju::random_init_default),
        m("<init>", "(J)V", acc::PUBLIC, ju::random_init_seed),
        m("nextInt", "()I", acc::PUBLIC, ju::random_next_int),
        m("nextInt", "(I)I", acc::PUBLIC, ju::random_next_int_bound),
        m("nextLong", "()J", acc::PUBLIC, ju::random_next_long),
        m("nextFloat", "()F", acc::PUBLIC, ju::random_next_float),
        m("nextDouble", "()D", acc::PUBLIC, ju::random_next_double),
    ],
};

static VECTOR_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/util/Vector",
    access_flags: acc::PUBLIC,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[
        field("data", "[Ljava/lang/Object;", acc::PRIVATE),
        field("count", "I", acc::PRIVATE),
    ],
    methods: &[
        m("<init>", "()V", acc::PUBLIC, ju::vec_init),
        m("<init>", "(I)V", acc::PUBLIC, ju::vec_init_capacity),
        m("size", "()I", acc::PUBLIC, ju::vec_size),
        m("capacity", "()I", acc::PUBLIC, ju::vec_capacity),
        m("isEmpty", "()Z", acc::PUBLIC, ju::vec_is_empty),
        m(
            "addElement",
            "(Ljava/lang/Object;)V",
            acc::PUBLIC,
            ju::vec_add_element,
        ),
        m(
            "elementAt",
            "(I)Ljava/lang/Object;",
            acc::PUBLIC,
            ju::vec_element_at,
        ),
        m(
            "setElementAt",
            "(Ljava/lang/Object;I)V",
            acc::PUBLIC,
            ju::vec_set_element_at,
        ),
        m(
            "insertElementAt",
            "(Ljava/lang/Object;I)V",
            acc::PUBLIC,
            ju::vec_insert_element_at,
        ),
        m(
            "removeElementAt",
            "(I)V",
            acc::PUBLIC,
            ju::vec_remove_element_at,
        ),
        m(
            "removeElement",
            "(Ljava/lang/Object;)Z",
            acc::PUBLIC,
            ju::vec_remove_element,
        ),
        m(
            "contains",
            "(Ljava/lang/Object;)Z",
            acc::PUBLIC,
            ju::vec_contains,
        ),
        m(
            "indexOf",
            "(Ljava/lang/Object;)I",
            acc::PUBLIC,
            ju::vec_index_of,
        ),
        m(
            "indexOf",
            "(Ljava/lang/Object;I)I",
            acc::PUBLIC,
            ju::vec_index_of_from,
        ),
        m(
            "removeAllElements",
            "()V",
            acc::PUBLIC,
            ju::vec_remove_all_elements,
        ),
        m(
            "firstElement",
            "()Ljava/lang/Object;",
            acc::PUBLIC,
            ju::vec_first_element,
        ),
        m(
            "lastElement",
            "()Ljava/lang/Object;",
            acc::PUBLIC,
            ju::vec_last_element,
        ),
        m(
            "elements",
            "()Ljava/util/Enumeration;",
            acc::PUBLIC,
            ju::vec_elements,
        ),
        m(
            "copyInto",
            "([Ljava/lang/Object;)V",
            acc::PUBLIC,
            ju::vec_copy_into,
        ),
    ],
};

/// Stack 无自有字段，复用 Vector 布局（分派键=定义类，继承方法自动落 Vector）。
static STACK_DECL: BootstrapDecl = klass(
    "java/util/Stack",
    "java/util/Vector",
    &[
        m(
            "push",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            acc::PUBLIC,
            ju::stack_push,
        ),
        m("pop", "()Ljava/lang/Object;", acc::PUBLIC, ju::stack_pop),
        m("peek", "()Ljava/lang/Object;", acc::PUBLIC, ju::stack_peek),
        m("empty", "()Z", acc::PUBLIC, ju::stack_empty),
        m(
            "search",
            "(Ljava/lang/Object;)I",
            acc::PUBLIC,
            ju::stack_search,
        ),
    ],
);

/// Timer/TimerTask（T034）：数据面（nextTime/period/cancelled 字段）+
/// 触发面（schedule 类 native 置 `cs.pending_timers` 请求位，Vm.poll
/// 段零消费转调度器定时队列，到期 spawn run 线程）。delay 版走宿主轨
/// （nextTime 取宿主时钟，mock 时钟可驱动测试）。
static TIMER_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/util/Timer",
    access_flags: acc::PUBLIC,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[
        field("tasks", "[Ljava/util/TimerTask;", acc::PRIVATE),
        field("count", "I", acc::PRIVATE),
        field("cancelled", "Z", acc::PRIVATE),
    ],
    methods: &[
        m("<init>", "()V", acc::PUBLIC, ju::timer_init),
        mh(
            "schedule",
            "(Ljava/util/TimerTask;J)V",
            acc::PUBLIC,
            ju::timer_schedule_delay,
        ),
        mh(
            "schedule",
            "(Ljava/util/TimerTask;JJ)V",
            acc::PUBLIC,
            ju::timer_schedule_delay_period,
        ),
        m(
            "schedule",
            "(Ljava/util/TimerTask;Ljava/util/Date;)V",
            acc::PUBLIC,
            ju::timer_schedule_date,
        ),
        m(
            "schedule",
            "(Ljava/util/TimerTask;Ljava/util/Date;J)V",
            acc::PUBLIC,
            ju::timer_schedule_date_period,
        ),
        mh(
            "scheduleAtFixedRate",
            "(Ljava/util/TimerTask;JJ)V",
            acc::PUBLIC,
            ju::timer_fixed_rate_delay,
        ),
        m(
            "scheduleAtFixedRate",
            "(Ljava/util/TimerTask;Ljava/util/Date;J)V",
            acc::PUBLIC,
            ju::timer_fixed_rate_date,
        ),
        m("cancel", "()V", acc::PUBLIC, ju::timer_cancel),
        m("purge", "()I", acc::PUBLIC, ju::timer_purge),
    ],
};

static TIMER_TASK_DECL: BootstrapDecl = BootstrapDecl {
    name: "java/util/TimerTask",
    access_flags: acc::PUBLIC,
    super_name: Some("java/lang/Object"),
    interfaces: &["java/lang/Runnable"],
    fields: &[
        field("nextTime", "J", acc::PRIVATE),
        field("period", "J", acc::PRIVATE),
        field("cancelled", "Z", acc::PRIVATE),
        field("timer", "Ljava/util/Timer;", acc::PRIVATE),
    ],
    methods: &[
        m("<init>", "()V", acc::PUBLIC, ju::timer_task_init),
        // JDK 中 TimerTask 为抽象类；VM 侧保持具体类 + 空 run，
        // 用户子类覆写后分派自然命中子类实现。
        m("run", "()V", acc::PUBLIC, stub_void),
        m("cancel", "()Z", acc::PUBLIC, ju::timer_task_cancel),
        m(
            "scheduledExecutionTime",
            "()J",
            acc::PUBLIC,
            ju::timer_task_scheduled_execution_time,
        ),
    ],
};

// ---------------------------------------------------------------------------
// javax/microedition
// ---------------------------------------------------------------------------

/// Canvas（T021 真实现，绑定 `midp::lcdui::canvas`）：实例字段
/// screen（懒分配可变屏图）/dirty+dx/dy/dw/dh（脏矩形，repaint 并集
/// 合并，同帧多次 repaint 一次 paint）/fullScreen（MVP 仅记录）。
/// `paint` 声明为**抽象**——jar 子类 override 后由 Vm.poll 三段式的
/// paint 回调链以绿线程回调（见 vm.rs）；keyPressed/keyReleased/
/// keyRepeated 声明保持桩（body=None），子类覆写后由 poll 段前置
/// 事件泵以绿线程回调（T030，同 paint 模式）。
static CANVAS_DECL: BootstrapDecl = BootstrapDecl {
    name: "javax/microedition/lcdui/Canvas",
    access_flags: acc::PUBLIC,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[
        // MIDP-2.0 gameAction 常量（T031 补注册：UP=1/LEFT=2/RIGHT=5/
        // DOWN=6/FIRE=8/GAME_A-D=9-12——与 input 模块 Rust 常量同源）。
        const_field("UP", "I", BootstrapConst::Int(1)),
        const_field("LEFT", "I", BootstrapConst::Int(2)),
        const_field("RIGHT", "I", BootstrapConst::Int(5)),
        const_field("DOWN", "I", BootstrapConst::Int(6)),
        const_field("FIRE", "I", BootstrapConst::Int(8)),
        const_field("GAME_A", "I", BootstrapConst::Int(9)),
        const_field("GAME_B", "I", BootstrapConst::Int(10)),
        const_field("GAME_C", "I", BootstrapConst::Int(11)),
        const_field("GAME_D", "I", BootstrapConst::Int(12)),
        field("screen", "Ljavax/microedition/lcdui/Image;", acc::PRIVATE),
        field("dirty", "Z", acc::PRIVATE),
        field("dx", "I", acc::PRIVATE),
        field("dy", "I", acc::PRIVATE),
        field("dw", "I", acc::PRIVATE),
        field("dh", "I", acc::PRIVATE),
        field("fullScreen", "Z", acc::PRIVATE),
    ],
    methods: &[
        m("<init>", "()V", acc::PUBLIC, stub_void),
        // 抽象回调：子类未覆写时不可分派（Vm.poll 段一探测 body 跳过）。
        m(
            "paint",
            canvas::PAINT_DESC,
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
        m(
            "setFullScreenMode",
            "(Z)V",
            acc::PUBLIC,
            canvas::set_full_screen_mode,
        ),
        m("repaint", "()V", acc::PUBLIC, canvas::repaint),
        m("repaint", "(IIII)V", acc::PUBLIC, canvas::repaint_rect),
        m(
            "serviceRepaints",
            "()V",
            acc::PUBLIC,
            canvas::service_repaints,
        ),
        mh("getWidth", "()I", acc::PUBLIC, canvas::get_width),
        mh("getHeight", "()I", acc::PUBLIC, canvas::get_height),
        // 输入静态查询（T030 真实现，绑定 `midp::lcdui::input`）；非法
        // 参数按 MIDP 语义抛 IllegalArgumentException。
        m("getGameAction", "(I)I", acc::PUBLIC, input::get_game_action),
        m("getKeyCode", "(I)I", acc::PUBLIC, input::get_key_code),
        m(
            "getKeyName",
            "(I)Ljava/lang/String;",
            acc::PUBLIC,
            input::get_key_name,
        ),
        m("hasPointerEvents", "()Z", acc::PUBLIC, stub_zero),
        m("hasRepeatEvents", "()Z", acc::PUBLIC, stub_zero),
        m("showNotify", "()V", acc::PUBLIC, stub_void),
        m("hideNotify", "()V", acc::PUBLIC, stub_void),
        // keyPressed/keyRepeated/keyReleased/pointer 系声明保持桩
        // （body=None）：jar 子类覆写后由 Vm.poll 事件泵以绿线程回调
        // （T030/T031，同 paint 模式）；未覆写即游戏未实现该回调，
        // 事件泵探测跳过。
        m("keyPressed", "(I)V", acc::PUBLIC, stub_void),
        m("keyReleased", "(I)V", acc::PUBLIC, stub_void),
        m("keyRepeated", "(I)V", acc::PUBLIC, stub_void),
        m("pointerPressed", "(II)V", acc::PUBLIC, stub_void),
        m("pointerReleased", "(II)V", acc::PUBLIC, stub_void),
        m("pointerDragged", "(II)V", acc::PUBLIC, stub_void),
    ],
};

/// GameCanvas（T031 真实现，绑定 `midp::lcdui::game::gamecanvas`）：
/// extends Canvas 的 bootstrap 具体类。实例字段仅 `suppress Z`
/// （构造 `(Z)V` 存入——suppressKeyEvents）；键状态位图在
/// ClassSpace.key_states（事件泵维护，getKeyStates 读取清零——MIDP
/// 锁存语义）；flushGraphics 走 repaint 请求位直通 paint+present 链
/// （单屏图直通模型取舍）；getGraphics 分配绑屏图 Graphics（复用
/// T021 懒分配屏图）。九个 *_PRESSED 位常量按 MIDP-2.0 javadoc
/// 考证值注册（UP=2/DOWN=64/LEFT=4/RIGHT=128/FIRE=256/GAME_A-D=
/// 512/1024/2048/4096）。
static GAMECANVAS_DECL: BootstrapDecl = BootstrapDecl {
    name: "javax/microedition/lcdui/game/GameCanvas",
    access_flags: acc::PUBLIC,
    super_name: Some("javax/microedition/lcdui/Canvas"),
    interfaces: &[],
    fields: &[
        const_field("UP_PRESSED", "I", BootstrapConst::Int(1 << 1)),
        const_field("DOWN_PRESSED", "I", BootstrapConst::Int(1 << 6)),
        const_field("LEFT_PRESSED", "I", BootstrapConst::Int(1 << 2)),
        const_field("RIGHT_PRESSED", "I", BootstrapConst::Int(1 << 7)),
        const_field("FIRE_PRESSED", "I", BootstrapConst::Int(1 << 8)),
        const_field("GAME_A_PRESSED", "I", BootstrapConst::Int(1 << 9)),
        const_field("GAME_B_PRESSED", "I", BootstrapConst::Int(1 << 10)),
        const_field("GAME_C_PRESSED", "I", BootstrapConst::Int(1 << 11)),
        const_field("GAME_D_PRESSED", "I", BootstrapConst::Int(1 << 12)),
        field("suppress", "Z", acc::PRIVATE),
    ],
    methods: &[
        m("<init>", "(Z)V", acc::PUBLIC, gamecanvas::init),
        m(
            "getKeyStates",
            "()I",
            acc::PUBLIC,
            gamecanvas::get_key_states,
        ),
        m(
            "flushGraphics",
            "()V",
            acc::PUBLIC,
            gamecanvas::flush_graphics,
        ),
        m(
            "flushGraphics",
            "(IIII)V",
            acc::PUBLIC,
            gamecanvas::flush_graphics_rect,
        ),
        mh(
            "getGraphics",
            "()Ljavax/microedition/lcdui/Graphics;",
            acc::PUBLIC,
            gamecanvas::get_graphics,
        ),
    ],
};

/// Display（T021 真实现，绑定 `midp::lcdui::display`）：`getDisplay`
/// 单例（ClassSpace.display_cache，同 font_cache 模式）；current 为
/// 实例字段（`setCurrent`/`getCurrent` 回读同一引用）。
/// 次要方法（isShown 等）与 Screen 子体系（Form/List/TextBox）留桩。
static DISPLAY_DECL: BootstrapDecl = BootstrapDecl {
    name: "javax/microedition/lcdui/Display",
    access_flags: acc::PUBLIC,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[field(
        "current",
        "Ljavax/microedition/lcdui/Displayable;",
        acc::PRIVATE,
    )],
    methods: &[
        m(
            "getDisplay",
            "(Ljavax/microedition/midlet/MIDlet;)Ljavax/microedition/lcdui/Display;",
            acc::PUBLIC | acc::STATIC,
            display::get_display,
        ),
        m(
            "setCurrent",
            "(Ljavax/microedition/lcdui/Displayable;)V",
            acc::PUBLIC,
            display::set_current,
        ),
        m(
            "getCurrent",
            "()Ljavax/microedition/lcdui/Displayable;",
            acc::PUBLIC,
            display::get_current,
        ),
    ],
};

/// Font（T019 真实现，绑定 `midp::lcdui::font`）：实例字段
/// face/style/size（MIDP 常量原值）+ handle（HostContext FontHandle 快照，
/// u32 窄化 i32）；类常量按 MIDP-2.0 值注册（const_field 静态常量机制）。
static FONT_DECL: BootstrapDecl = BootstrapDecl {
    name: "javax/microedition/lcdui/Font",
    access_flags: acc::PUBLIC,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[
        const_field("FACE_SYSTEM", "I", BootstrapConst::Int(0)),
        const_field("FACE_PROPORTIONAL", "I", BootstrapConst::Int(16)),
        const_field("FACE_MONOSPACE", "I", BootstrapConst::Int(32)),
        const_field("STYLE_PLAIN", "I", BootstrapConst::Int(0)),
        const_field("STYLE_BOLD", "I", BootstrapConst::Int(1)),
        const_field("STYLE_ITALIC", "I", BootstrapConst::Int(2)),
        const_field("STYLE_UNDERLINED", "I", BootstrapConst::Int(4)),
        const_field("SIZE_SMALL", "I", BootstrapConst::Int(8)),
        const_field("SIZE_MEDIUM", "I", BootstrapConst::Int(16)),
        const_field("SIZE_LARGE", "I", BootstrapConst::Int(32)),
        field("face", "I", acc::PRIVATE),
        field("style", "I", acc::PRIVATE),
        field("size", "I", acc::PRIVATE),
        field("handle", "I", acc::PRIVATE),
    ],
    methods: &[
        mh("charWidth", "(C)I", acc::PUBLIC, font::char_width),
        mh("charsWidth", "([CII)I", acc::PUBLIC, font::chars_width),
        mh(
            "getBaselinePosition",
            "()I",
            acc::PUBLIC,
            font::get_baseline_position,
        ),
        mh(
            "getDefaultFont",
            "()Ljavax/microedition/lcdui/Font;",
            acc::PUBLIC | acc::STATIC,
            font::get_default_font,
        ),
        mh("getFace", "()I", acc::PUBLIC, font::get_face),
        mh(
            "getFont",
            "(III)Ljavax/microedition/lcdui/Font;",
            acc::PUBLIC | acc::STATIC,
            font::get_font,
        ),
        mh("getHeight", "()I", acc::PUBLIC, font::get_height),
        mh("getSize", "()I", acc::PUBLIC, font::get_size),
        mh("getStyle", "()I", acc::PUBLIC, font::get_style),
        mh("isBold", "()Z", acc::PUBLIC, font::is_bold),
        mh("isItalic", "()Z", acc::PUBLIC, font::is_italic),
        mh("isPlain", "()Z", acc::PUBLIC, font::is_plain),
        mh("isUnderlined", "()Z", acc::PUBLIC, font::is_underlined),
        mh(
            "stringWidth",
            "(Ljava/lang/String;)I",
            acc::PUBLIC,
            font::string_width,
        ),
        mh(
            "substringWidth",
            "(Ljava/lang/String;II)I",
            acc::PUBLIC,
            font::substring_width,
        ),
    ],
};

/// Graphics（T020 真实现，绑定 `midp::lcdui::graphics`）：实例字段
/// target `[I`/尺寸/clip(设备坐标)/translate/color/font/stroke；
/// 锚点与线型静态常量（const_field）。文本四法与 setFont 走宿主轨
///（render_text/font 句柄）；其余为纯光栅。
static GRAPHICS_DECL: BootstrapDecl = BootstrapDecl {
    name: "javax/microedition/lcdui/Graphics",
    access_flags: acc::PUBLIC,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[
        const_field("BASELINE", "I", BootstrapConst::Int(64)),
        const_field("BOTTOM", "I", BootstrapConst::Int(32)),
        const_field("DOTTED", "I", BootstrapConst::Int(1)),
        const_field("HCENTER", "I", BootstrapConst::Int(1)),
        const_field("LEFT", "I", BootstrapConst::Int(4)),
        const_field("RIGHT", "I", BootstrapConst::Int(8)),
        const_field("SOLID", "I", BootstrapConst::Int(0)),
        const_field("TOP", "I", BootstrapConst::Int(16)),
        const_field("VCENTER", "I", BootstrapConst::Int(2)),
        field("target", "[I", acc::PRIVATE),
        field("target_w", "I", acc::PRIVATE),
        field("target_h", "I", acc::PRIVATE),
        field("clip_x1", "I", acc::PRIVATE),
        field("clip_y1", "I", acc::PRIVATE),
        field("clip_x2", "I", acc::PRIVATE),
        field("clip_y2", "I", acc::PRIVATE),
        field("trans_x", "I", acc::PRIVATE),
        field("trans_y", "I", acc::PRIVATE),
        field("color", "I", acc::PRIVATE),
        field("font", "Ljavax/microedition/lcdui/Font;", acc::PRIVATE),
        field("stroke", "I", acc::PRIVATE),
    ],
    methods: &[
        m("clipRect", "(IIII)V", acc::PUBLIC, graphics::clip_rect),
        m("drawArc", "(IIIIII)V", acc::PUBLIC, graphics::draw_arc),
        mh("drawChar", "(CIII)V", acc::PUBLIC, graphics::draw_char),
        mh("drawChars", "([CIIIII)V", acc::PUBLIC, graphics::draw_chars),
        m(
            "drawImage",
            "(Ljavax/microedition/lcdui/Image;III)V",
            acc::PUBLIC,
            graphics::draw_image,
        ),
        m("drawLine", "(IIII)V", acc::PUBLIC, graphics::draw_line),
        m("drawRGB", "([IIIIIIIZ)V", acc::PUBLIC, graphics::draw_rgb),
        m("drawRect", "(IIII)V", acc::PUBLIC, graphics::draw_rect),
        m(
            "drawRegion",
            "(Ljavax/microedition/lcdui/Image;IIIIIIII)V",
            acc::PUBLIC,
            graphics::draw_region,
        ),
        m(
            "drawRoundRect",
            "(IIIIII)V",
            acc::PUBLIC,
            graphics::draw_round_rect,
        ),
        mh(
            "drawString",
            "(Ljava/lang/String;III)V",
            acc::PUBLIC,
            graphics::draw_string,
        ),
        mh(
            "drawSubstring",
            "(Ljava/lang/String;IIIII)V",
            acc::PUBLIC,
            graphics::draw_substring,
        ),
        m("fillArc", "(IIIIII)V", acc::PUBLIC, graphics::fill_arc),
        m("fillRect", "(IIII)V", acc::PUBLIC, graphics::fill_rect),
        m(
            "fillRoundRect",
            "(IIIIII)V",
            acc::PUBLIC,
            graphics::fill_round_rect,
        ),
        m(
            "fillTriangle",
            "(IIIIII)V",
            acc::PUBLIC,
            graphics::fill_triangle,
        ),
        m(
            "getClipHeight",
            "()I",
            acc::PUBLIC,
            graphics::get_clip_height,
        ),
        m("getClipWidth", "()I", acc::PUBLIC, graphics::get_clip_width),
        m("getClipX", "()I", acc::PUBLIC, graphics::get_clip_x),
        m("getClipY", "()I", acc::PUBLIC, graphics::get_clip_y),
        m("getColor", "()I", acc::PUBLIC, graphics::get_color),
        m(
            "getBlueComponent",
            "()I",
            acc::PUBLIC,
            graphics::get_blue_component,
        ),
        m(
            "getFont",
            "()Ljavax/microedition/lcdui/Font;",
            acc::PUBLIC,
            graphics::get_font,
        ),
        m("getGrayScale", "()I", acc::PUBLIC, graphics::get_gray_scale),
        m(
            "getGreenComponent",
            "()I",
            acc::PUBLIC,
            graphics::get_green_component,
        ),
        m(
            "getRedComponent",
            "()I",
            acc::PUBLIC,
            graphics::get_red_component,
        ),
        m(
            "getStrokeStyle",
            "()I",
            acc::PUBLIC,
            graphics::get_stroke_style,
        ),
        m(
            "getTranslateX",
            "()I",
            acc::PUBLIC,
            graphics::get_translate_x,
        ),
        m(
            "getTranslateY",
            "()I",
            acc::PUBLIC,
            graphics::get_translate_y,
        ),
        m("setClip", "(IIII)V", acc::PUBLIC, graphics::set_clip),
        m("setColor", "(I)V", acc::PUBLIC, graphics::set_color_int),
        m("setColor", "(III)V", acc::PUBLIC, graphics::set_color_iii),
        mh(
            "setFont",
            "(Ljavax/microedition/lcdui/Font;)V",
            acc::PUBLIC,
            graphics::set_font,
        ),
        m(
            "setGrayScale",
            "(I)V",
            acc::PUBLIC,
            graphics::set_gray_scale,
        ),
        m(
            "setStrokeStyle",
            "(I)V",
            acc::PUBLIC,
            graphics::set_stroke_style,
        ),
        m("translate", "(II)V", acc::PUBLIC, graphics::translate),
    ],
};

/// Image（T018 真实现，绑定 `midp::lcdui::image`）：实例字段
/// width/height/pixels `[I`/mutable（像素堆上 int 数组，GC 可达）。
static IMAGE_DECL: BootstrapDecl = BootstrapDecl {
    name: "javax/microedition/lcdui/Image",
    access_flags: acc::PUBLIC,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[
        field("width", "I", acc::PRIVATE),
        field("height", "I", acc::PRIVATE),
        field("pixels", "[I", acc::PRIVATE),
        field("mutable", "Z", acc::PRIVATE),
    ],
    methods: &[
        m(
            "createImage",
            "(II)Ljavax/microedition/lcdui/Image;",
            acc::PUBLIC | acc::STATIC,
            image::create_image_empty,
        ),
        m(
            "createImage",
            "(Ljava/lang/String;)Ljavax/microedition/lcdui/Image;",
            acc::PUBLIC | acc::STATIC,
            image::create_image_string,
        ),
        m(
            "createImage",
            "([BII)Ljavax/microedition/lcdui/Image;",
            acc::PUBLIC | acc::STATIC,
            image::create_image_bytes,
        ),
        m(
            "createImage",
            "(Ljavax/microedition/lcdui/Image;)Ljavax/microedition/lcdui/Image;",
            acc::PUBLIC | acc::STATIC,
            image::create_image_source,
        ),
        m(
            "createImage",
            "(Ljavax/microedition/lcdui/Image;IIII)Ljavax/microedition/lcdui/Image;",
            acc::PUBLIC | acc::STATIC,
            image::create_image_region,
        ),
        m(
            "createImage",
            "(Ljava/io/InputStream;)Ljavax/microedition/lcdui/Image;",
            acc::PUBLIC | acc::STATIC,
            image::create_image_stream,
        ),
        m(
            "createRGBImage",
            "([IIIZ)Ljavax/microedition/lcdui/Image;",
            acc::PUBLIC | acc::STATIC,
            image::create_rgb_image,
        ),
        mh(
            "getGraphics",
            "()Ljavax/microedition/lcdui/Graphics;",
            acc::PUBLIC,
            image::get_graphics,
        ),
        m("getHeight", "()I", acc::PUBLIC, image::get_height),
        m("getRGB", "([IIIIIII)V", acc::PUBLIC, image::image_get_rgb),
        m("getWidth", "()I", acc::PUBLIC, image::get_width),
        m("isMutable", "()Z", acc::PUBLIC, image::is_mutable),
    ],
};

/// Manager 真实现（T038）：createPlayer 流+类型/定位符两形态 +
/// playTone；定位符静态常量按 JSR-135 真机值注册。
static MANAGER_DECL: BootstrapDecl = BootstrapDecl {
    name: "javax/microedition/media/Manager",
    access_flags: acc::PUBLIC,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[
        const_field(
            "TONE_DEVICE_LOCATOR",
            "Ljava/lang/String;",
            BootstrapConst::Str("device://tone"),
        ),
        const_field(
            "MIDI_DEVICE_LOCATOR",
            "Ljava/lang/String;",
            BootstrapConst::Str("device://midi"),
        ),
    ],
    methods: &[
        m(
            "createPlayer",
            "(Ljava/io/InputStream;Ljava/lang/String;)Ljavax/microedition/media/Player;",
            acc::PUBLIC | acc::STATIC,
            jmedia::manager_create_player_stream,
        ),
        m(
            "createPlayer",
            "(Ljava/lang/String;)Ljavax/microedition/media/Player;",
            acc::PUBLIC | acc::STATIC,
            jmedia::manager_create_player_locator,
        ),
        mh(
            "playTone",
            "(III)V",
            acc::PUBLIC | acc::STATIC,
            jmedia::manager_play_tone,
        ),
    ],
};

/// Player 真实现（T038）：bootstrap 类替换接口桩——真机 Player 是接口，
/// 本引擎以带实例字段的 bootstrap 类兼作唯一实现（Manager.createPlayer
/// 内部装配实例，jar 侧 invokeinterface 分派按接收者实际类命中本表；
/// 实现详录见 midp/media.rs 模块文档：状态机口径/嗅探/降级决策）。
/// state 字段直接存 JSR-135 常量值（0/100/200/300/400）。
static PLAYER_DECL: BootstrapDecl = BootstrapDecl {
    name: "javax/microedition/media/Player",
    access_flags: acc::PUBLIC | acc::FINAL,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[
        field("state", "I", acc::PRIVATE),
        field("loopCount", "I", acc::PRIVATE),
        field("level", "I", acc::PRIVATE),
        field("muted", "Z", acc::PRIVATE),
        field("data", "[B", acc::PRIVATE),
        field("mime", "Ljava/lang/String;", acc::PRIVATE),
        field("listeners", "[Ljava/lang/Object;", acc::PRIVATE),
        field(
            "volumeImpl",
            "Ljavax/microedition/media/control/VolumeControlImpl;",
            acc::PRIVATE,
        ),
        const_field("UNREALIZED", "I", BootstrapConst::Int(0)),
        const_field("REALIZED", "I", BootstrapConst::Int(100)),
        const_field("PREFETCHED", "I", BootstrapConst::Int(200)),
        const_field("STARTED", "I", BootstrapConst::Int(300)),
        const_field("CLOSED", "I", BootstrapConst::Int(400)),
        const_field("TIME_UNKNOWN", "I", BootstrapConst::Int(-1)),
    ],
    methods: &[
        m(
            "addPlayerListener",
            "(Ljavax/microedition/media/PlayerListener;)V",
            acc::PUBLIC,
            jmedia::player_add_listener,
        ),
        mh("close", "()V", acc::PUBLIC, jmedia::player_close),
        m("deallocate", "()V", acc::PUBLIC, jmedia::player_deallocate),
        m(
            "getContentType",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            jmedia::player_get_content_type,
        ),
        m(
            "getControl",
            "(Ljava/lang/String;)Ljavax/microedition/media/Control;",
            acc::PUBLIC,
            jmedia::player_get_control,
        ),
        m(
            "getControls",
            "()[Ljavax/microedition/media/Control;",
            acc::PUBLIC,
            jmedia::player_get_controls,
        ),
        m(
            "getDuration",
            "()J",
            acc::PUBLIC,
            jmedia::player_get_duration,
        ),
        m(
            "getMediaTime",
            "()J",
            acc::PUBLIC,
            jmedia::player_get_media_time,
        ),
        m("getState", "()I", acc::PUBLIC, jmedia::player_get_state),
        m("prefetch", "()V", acc::PUBLIC, jmedia::player_prefetch),
        m("realize", "()V", acc::PUBLIC, jmedia::player_realize),
        m(
            "removePlayerListener",
            "(Ljavax/microedition/media/PlayerListener;)V",
            acc::PUBLIC,
            jmedia::player_remove_listener,
        ),
        mh(
            "setLoopCount",
            "(I)V",
            acc::PUBLIC,
            jmedia::player_set_loop_count,
        ),
        m(
            "setMediaTime",
            "(J)J",
            acc::PUBLIC,
            jmedia::player_set_media_time,
        ),
        mh("start", "()V", acc::PUBLIC, jmedia::player_start),
        mh("stop", "()V", acc::PUBLIC, jmedia::player_stop),
    ],
};

/// MediaException（MMAPI 异常体系唯一成员：extends Exception）。
static MEDIA_EXCEPTION_DECL: BootstrapDecl = klass(
    "javax/microedition/media/MediaException",
    "java/lang/Exception",
    INIT_PAIR,
);

/// GCF ConnectionNotFoundException（媒体定位符走网络时抛；GCF 本体 US8）。
static CONNECTION_NOT_FOUND_DECL: BootstrapDecl = klass(
    "javax/io/ConnectionNotFoundException",
    "java/io/IOException",
    INIT_PAIR,
);

/// PlayerListener 接口（T038 注册接受、事件不触发——重入回调归 T039）。
static PLAYER_LISTENER_IFACE_DECL: BootstrapDecl = iface(
    "javax/microedition/media/PlayerListener",
    &[m(
        "playerUpdate",
        "(Ljavax/microedition/media/Player;Ljava/lang/String;Ljava/lang/Object;)V",
        acc::PUBLIC | acc::ABSTRACT,
        stub_void,
    )],
);

/// Control 标记接口（getControl(s) 返回类型的饿链接目标）。
static CONTROL_IFACE_DECL: BootstrapDecl = iface("javax/microedition/media/Control", &[]);

/// VolumeControl 接口（JSR-135 四方法；实现归 VolumeControlImpl）。
static VOLUME_CONTROL_IFACE_DECL: BootstrapDecl = iface(
    "javax/microedition/media/control/VolumeControl",
    &[
        m("setLevel", "(I)I", acc::PUBLIC | acc::ABSTRACT, stub_zero),
        m("getLevel", "()I", acc::PUBLIC | acc::ABSTRACT, stub_zero),
        m("setMute", "(Z)V", acc::PUBLIC | acc::ABSTRACT, stub_void),
        m("isMuted", "()Z", acc::PUBLIC | acc::ABSTRACT, stub_zero),
    ],
);

/// VolumeControlImpl 内部实现类（T038）：字段 `owner LPlayer;`，方法
/// 转发 owner 的 level/muted 字段——getControl("VolumeControl") 返回
/// 其单例，游戏侧直转型不会 ClassCastException。
static VOLUME_CONTROL_IMPL_DECL: BootstrapDecl = BootstrapDecl {
    name: "javax/microedition/media/control/VolumeControlImpl",
    access_flags: acc::PUBLIC | acc::FINAL,
    super_name: Some("java/lang/Object"),
    interfaces: &["javax/microedition/media/control/VolumeControl"],
    fields: &[field(
        "owner",
        "Ljavax/microedition/media/Player;",
        acc::PRIVATE,
    )],
    methods: &[
        m("setLevel", "(I)I", acc::PUBLIC, jmedia::vc_set_level),
        m("getLevel", "()I", acc::PUBLIC, jmedia::vc_get_level),
        m("setMute", "(Z)V", acc::PUBLIC, jmedia::vc_set_mute),
        m("isMuted", "()Z", acc::PUBLIC, jmedia::vc_is_muted),
    ],
};

/// ToneControl 接口占位（T039 接序列与合成；extends Control）。
static TONE_CONTROL_IFACE_DECL: BootstrapDecl = BootstrapDecl {
    name: "javax/microedition/media/control/ToneControl",
    access_flags: acc::PUBLIC | acc::INTERFACE,
    super_name: Some("java/lang/Object"),
    interfaces: &["javax/microedition/media/Control"],
    fields: &[],
    methods: &[m(
        "setSequence",
        "([B)V",
        acc::PUBLIC | acc::ABSTRACT,
        stub_void,
    )],
};

/// MIDIControl 接口占位（T039；extends Control，签名按 JSR-135）。
static MIDI_CONTROL_IFACE_DECL: BootstrapDecl = BootstrapDecl {
    name: "javax/microedition/media/control/MIDIControl",
    access_flags: acc::PUBLIC | acc::INTERFACE,
    super_name: Some("java/lang/Object"),
    interfaces: &["javax/microedition/media/Control"],
    fields: &[],
    methods: &[
        m(
            "isBankQuerySupported",
            "()Z",
            acc::PUBLIC | acc::ABSTRACT,
            stub_zero,
        ),
        m("getProgram", "(I)I", acc::PUBLIC | acc::ABSTRACT, stub_zero),
        m(
            "setProgram",
            "(IIIZ)V",
            acc::PUBLIC | acc::ABSTRACT,
            stub_void,
        ),
        m(
            "isPolyphonySupported",
            "(I)Z",
            acc::PUBLIC | acc::ABSTRACT,
            stub_zero,
        ),
        m(
            "getChannelVolume",
            "(I)I",
            acc::PUBLIC | acc::ABSTRACT,
            stub_zero,
        ),
        m(
            "setChannelVolume",
            "(II)V",
            acc::PUBLIC | acc::ABSTRACT,
            stub_void,
        ),
        m(
            "getProgramName",
            "(I)Ljava/lang/String;",
            acc::PUBLIC | acc::ABSTRACT,
            stub_null,
        ),
        m(
            "getKeyName",
            "(I)Ljava/lang/String;",
            acc::PUBLIC | acc::ABSTRACT,
            stub_null,
        ),
        m(
            "getChannels",
            "()[I",
            acc::PUBLIC | acc::ABSTRACT,
            stub_null,
        ),
    ],
};

/// MIDletStateChangeException（T044）：destroyApp/pauseApp 回调可抛出
/// 的受检异常——destroyApp 非强制（unconditional=false）时抛出即拒绝
/// 销毁（Vm.poll 生命周期收尾段按 last_uncaught 判定）。INIT_PAIR 沿
/// Exception 构造惯例（String 消息/无参双轨）。
static MIDLET_STATE_CHANGE_DECL: BootstrapDecl = klass(
    "javax/microedition/midlet/MIDletStateChangeException",
    "java/lang/Exception",
    INIT_PAIR,
);

static MIDLET_DECL: BootstrapDecl = klass(
    "javax/microedition/midlet/MIDlet",
    "java/lang/Object",
    &[
        m("<init>", "()V", acc::PUBLIC, stub_void),
        // jar 内子类经继承链解析到的生命周期方法（T044：真实现置
        // cs 侧请求位，Vm.poll 生命周期段消费构帧回调）。
        m(
            "notifyDestroyed",
            "()V",
            acc::PUBLIC,
            jmidlet::notify_destroyed,
        ),
        m("notifyPaused", "()V", acc::PUBLIC, jmidlet::notify_paused),
        m("resumeRequest", "()V", acc::PUBLIC, jmidlet::resume_request),
        m(
            "getAppProperty",
            "(Ljava/lang/String;)Ljava/lang/String;",
            acc::PUBLIC,
            midlet_get_app_property,
        ),
        m(
            "platformRequest",
            "(Ljava/lang/String;)Z",
            acc::PUBLIC,
            stub_zero,
        ),
    ],
);

/// MIDlet.getAppProperty：套件属性表查询（install_jar 从
/// META-INF/MANIFEST.MF 解析入 `ClassSpace::suite_properties`；
/// T025 扩展 JAD 来源）。未命中返回 null（MIDP 规范）。
fn midlet_get_app_property(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let key_ref = match call.args.first() {
        Some(Slot::Ref(reference)) => *reference,
        other => {
            return Err(KvmError::Internal(format!(
                "getAppProperty arg 0 expected ref, got {other:?}"
            )));
        }
    };
    if key_ref.is_null() {
        return Err(jl::throw_java(
            cs,
            heap,
            "java/lang/NullPointerException",
            None,
        ));
    }
    let chars = heap.str_obj(key_ref)?.chars.clone();
    let key = String::from_utf16_lossy(&chars);
    match cs.suite_properties.get(&key) {
        Some(value) => {
            let out: Vec<u16> = value.encode_utf16().collect();
            Ok(Some(Slot::Ref(heap.alloc_string_uninterned(out)?)))
        }
        // 未定义属性：MIDP 规范返回 null（非异常）。
        None => Ok(Some(Slot::Ref(ObjRef::NULL))),
    }
}

static RECORD_ENUMERATION_DECL: BootstrapDecl = iface(
    "javax/microedition/rms/RecordEnumeration",
    &[
        m(
            "hasNextElement",
            "()Z",
            acc::PUBLIC | acc::ABSTRACT,
            stub_zero,
        ),
        m(
            "nextRecordId",
            "()I",
            acc::PUBLIC | acc::ABSTRACT,
            stub_zero,
        ),
    ],
);

/// RMS 异常体系（MIDP-2.0 继承链；RecordStoreException : Exception，
/// 其余三个 : RecordStoreException——catch 基类可兜住全部 RMS 故障）。
static RECORD_STORE_EXCEPTION_DECL: BootstrapDecl = klass(
    "javax/microedition/rms/RecordStoreException",
    "java/lang/Exception",
    INIT_PAIR,
);
static RECORD_STORE_NOT_FOUND_DECL: BootstrapDecl = klass(
    "javax/microedition/rms/RecordStoreNotFoundException",
    "javax/microedition/rms/RecordStoreException",
    INIT_PAIR,
);
/// RecordStoreFullException：预留继承链；店满语义不做（宿主盘容量之外）。
static RECORD_STORE_FULL_DECL: BootstrapDecl = klass(
    "javax/microedition/rms/RecordStoreFullException",
    "javax/microedition/rms/RecordStoreException",
    INIT_PAIR,
);
static INVALID_RECORD_ID_DECL: BootstrapDecl = klass(
    "javax/microedition/rms/InvalidRecordIDException",
    "javax/microedition/rms/RecordStoreException",
    INIT_PAIR,
);

/// RecordStore 真实现（T036）：实例字段只存 `name` + `open`，记录数据
/// 整店 blob 经 host 通道即时装载/回存（实现详录见 midp/rms.rs 模块
/// 文档：blob 格式、无驻留缓存的取舍、降级决策）。
static RECORD_STORE_DECL: BootstrapDecl = BootstrapDecl {
    name: "javax/microedition/rms/RecordStore",
    access_flags: acc::PUBLIC | acc::FINAL,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[
        field("name", "Ljava/lang/String;", acc::PRIVATE),
        field("open", "Z", acc::PRIVATE),
    ],
    methods: &[
        mh("addRecord", "([BII)I", acc::PUBLIC, jrms::rs_add_record),
        // 监听器桩保持（T036 范围外：需事件分发管线）。
        m(
            "addRecordListener",
            "(Ljavax/microedition/rms/RecordListener;)V",
            acc::PUBLIC,
            stub_void,
        ),
        mh(
            "closeRecordStore",
            "()V",
            acc::PUBLIC,
            jrms::rs_close_record_store,
        ),
        mh(
            "enumerateRecords",
            "(Ljavax/microedition/rms/RecordFilter;Ljavax/microedition/rms/RecordComparator;Z)Ljavax/microedition/rms/RecordEnumeration;",
            acc::PUBLIC,
            jrms::rs_enumerate_records,
        ),
        mh(
            "getName",
            "()Ljava/lang/String;",
            acc::PUBLIC,
            jrms::rs_get_name,
        ),
        mh(
            "getNextRecordID",
            "()I",
            acc::PUBLIC,
            jrms::rs_get_next_record_id,
        ),
        mh(
            "getNumRecords",
            "()I",
            acc::PUBLIC,
            jrms::rs_get_num_records,
        ),
        mh("getRecord", "(I)[B", acc::PUBLIC, jrms::rs_get_record),
        mh("getRecord", "(I[B)I", acc::PUBLIC, jrms::rs_get_record_into),
        mh("getSize", "()I", acc::PUBLIC, jrms::rs_get_size),
        mh(
            "getLastModified",
            "()J",
            acc::PUBLIC,
            jrms::rs_get_last_modified,
        ),
        mh("getVersion", "()I", acc::PUBLIC, jrms::rs_get_version),
        mh(
            "openRecordStore",
            "(Ljava/lang/String;Z)Ljavax/microedition/rms/RecordStore;",
            acc::PUBLIC | acc::STATIC,
            jrms::rs_open_record_store,
        ),
        mh(
            "deleteRecordStore",
            "(Ljava/lang/String;)V",
            acc::PUBLIC | acc::STATIC,
            jrms::rs_delete_record_store,
        ),
        mh(
            "listRecordStores",
            "()[Ljava/lang/String;",
            acc::PUBLIC | acc::STATIC,
            jrms::rs_list_record_stores,
        ),
        mh("deleteRecord", "(I)V", acc::PUBLIC, jrms::rs_delete_record),
        m(
            "removeRecordListener",
            "(Ljavax/microedition/rms/RecordListener;)V",
            acc::PUBLIC,
            stub_void,
        ),
        mh("setRecord", "(I[BII)V", acc::PUBLIC, jrms::rs_set_record),
        mh("setMode", "(IZ)V", acc::PUBLIC, jrms::rs_set_mode),
        mh("getMode", "()I", acc::PUBLIC, jrms::rs_get_mode),
    ],
};

/// RecordEnumeration 内部实现类（T036）：字段 `storeName`（查店 blob 的
/// 钥匙）+ `ids [I`（构造时的记录 id 快照）+ `pos I`（游标）；接口
/// RECORD_ENUMERATION_DECL 保持占签名（jar 侧饿链接目标）。
static RECORD_ENUM_DECL: BootstrapDecl = BootstrapDecl {
    name: "javax/microedition/rms/RecordEnum",
    access_flags: acc::PUBLIC | acc::FINAL,
    super_name: Some("java/lang/Object"),
    interfaces: &["javax/microedition/rms/RecordEnumeration"],
    fields: &[
        field("storeName", "Ljava/lang/String;", acc::PRIVATE),
        field("ids", "[I", acc::PRIVATE),
        field("pos", "I", acc::PRIVATE),
    ],
    methods: &[
        mh("hasNextElement", "()Z", acc::PUBLIC, jrms::enum_has_next),
        mh(
            "nextRecordId",
            "()I",
            acc::PUBLIC,
            jrms::enum_next_record_id,
        ),
        mh("nextRecord", "()[B", acc::PUBLIC, jrms::enum_next_record),
        mh("reset", "()V", acc::PUBLIC, jrms::enum_reset),
        mh("destroy", "()V", acc::PUBLIC, jrms::enum_destroy),
        mh(
            "hasPreviousElement",
            "()Z",
            acc::PUBLIC,
            jrms::enum_has_previous,
        ),
        mh(
            "previousRecord",
            "()[B",
            acc::PUBLIC,
            jrms::enum_previous_record,
        ),
    ],
};

// ---------------------------------------------------------------------------
// 注册表
// ---------------------------------------------------------------------------

/// bootstrap 注册表：覆盖 jc.jar 字节码引用的全部 CLDC/MIDP API 面 +
/// 异常体系骨架（`Fault::java_name` 全部目标 + Error 侧常用链）。
/// jar 引用的不存在类不进表 → NoClassDefFoundError 语义。
static REGISTRY: &[&BootstrapDecl] = &[
    // java/lang 核心
    &OBJECT_DECL,
    &CLASS_DECL,
    &THROWABLE_DECL,
    &EXCEPTION_DECL,
    &ERROR_DECL,
    &RUNTIME_EXCEPTION_DECL,
    &ILLEGAL_ARGUMENT_DECL,
    &ILLEGAL_THREAD_STATE_DECL,
    &ILLEGAL_STATE_DECL,
    &ILLEGAL_ACCESS_DECL,
    &INSTANTIATION_DECL,
    &CLASS_NOT_FOUND_DECL,
    &INTERRUPTED_DECL,
    &CLONE_NOT_SUPPORTED_DECL,
    &INDEX_OOB_DECL,
    &ARRAY_INDEX_OOB_DECL,
    &STRING_INDEX_OOB_DECL,
    &NUMBER_FORMAT_DECL,
    &CLASS_CAST_DECL,
    &ARITHMETIC_DECL,
    &NEG_ARRAY_SIZE_DECL,
    &ILLEGAL_MONITOR_DECL,
    &ARRAY_STORE_DECL,
    &NPE_DECL,
    &UNSUPPORTED_OP_DECL,
    &LINKAGE_ERROR_DECL,
    &NO_CLASS_DEF_DECL,
    &OOM_DECL,
    &STRING_DECL,
    &STRING_BUFFER_DECL,
    &SYSTEM_DECL,
    &MATH_DECL,
    &THREAD_DECL,
    &RUNNABLE_DECL,
    &BOOLEAN_DECL,
    &CHARACTER_DECL,
    &INTEGER_DECL,
    &LONG_DECL,
    &FLOAT_DECL,
    &DOUBLE_DECL,
    // java/io（T016）
    &DATA_INPUT_IFACE_DECL,
    &DATA_OUTPUT_IFACE_DECL,
    &INPUT_STREAM_DECL,
    &OUTPUT_STREAM_DECL,
    &READER_DECL,
    &WRITER_DECL,
    &FILTER_INPUT_DECL,
    &FILTER_OUTPUT_DECL,
    &BYTE_ARRAY_INPUT_DECL,
    &BYTE_ARRAY_OUTPUT_DECL,
    &DATA_INPUT_DECL,
    &DATA_OUTPUT_DECL,
    &PRINT_STREAM_DECL,
    &INPUT_STREAM_READER_DECL,
    &OUTPUT_STREAM_WRITER_DECL,
    &IO_EXCEPTION_DECL,
    &UNSUPPORTED_ENCODING_DECL,
    &EOF_DECL,
    &UTF_DATA_FORMAT_DECL,
    &INTERRUPTED_IO_DECL,
    // java/util
    &CALENDAR_DECL,
    &DATE_DECL,
    &EMPTY_STACK_DECL,
    &ENUMERATION_DECL,
    &ENUMERATOR_DECL,
    &HASHTABLE_DECL,
    &NO_SUCH_ELEMENT_DECL,
    &RANDOM_DECL,
    &STACK_DECL,
    &TIMER_DECL,
    &TIMER_TASK_DECL,
    &TIMEZONE_DECL,
    &VECTOR_DECL,
    // javax/microedition
    &CANVAS_DECL,
    // lcdui/game（T031）：GameCanvas 真实现 + Layer 族占位（FR-016，
    // jc.jar 基线 0 引用 game 包——见 layer.rs 考证注释）
    &GAMECANVAS_DECL,
    &layer::LAYER_DECL,
    &layer::LAYER_MANAGER_DECL,
    &layer::SPRITE_DECL,
    &layer::TILED_LAYER_DECL,
    &DISPLAY_DECL,
    &FONT_DECL,
    &GRAPHICS_DECL,
    &IMAGE_DECL,
    &MANAGER_DECL,
    &PLAYER_DECL,
    &MEDIA_EXCEPTION_DECL,
    &CONNECTION_NOT_FOUND_DECL,
    // javax/microedition/io（GCF，T041）：接口占位 + Connector 工厂 +
    // HttpConnectionImpl 懒连接实现 + HttpOutputStream 请求体缓冲。
    &jgcf::CONNECTION_IFACE_DECL,
    &jgcf::INPUT_CONNECTION_IFACE_DECL,
    &jgcf::OUTPUT_CONNECTION_IFACE_DECL,
    &jgcf::STREAM_CONNECTION_IFACE_DECL,
    &jgcf::CONTENT_CONNECTION_IFACE_DECL,
    &jgcf::HTTP_CONNECTION_IFACE_DECL,
    &jgcf::CONNECTOR_DECL,
    &jgcf::HTTP_CONNECTION_IMPL_DECL,
    &jgcf::HTTP_OUTPUT_STREAM_DECL,
    &PLAYER_LISTENER_IFACE_DECL,
    &CONTROL_IFACE_DECL,
    &VOLUME_CONTROL_IFACE_DECL,
    &VOLUME_CONTROL_IMPL_DECL,
    &TONE_CONTROL_IFACE_DECL,
    &MIDI_CONTROL_IFACE_DECL,
    &MIDLET_DECL,
    &MIDLET_STATE_CHANGE_DECL,
    &RECORD_ENUMERATION_DECL,
    &RECORD_STORE_DECL,
    &RECORD_ENUM_DECL,
    &RECORD_STORE_EXCEPTION_DECL,
    &RECORD_STORE_NOT_FOUND_DECL,
    &RECORD_STORE_FULL_DECL,
    &INVALID_RECORD_ID_DECL,
    &PAYMENT_LISTENER_DECL,
    &SKY_PAYMENT_DECL,
];

/// 斯凯计费 SDK 平台桩（真机由斯凯平台注入 jar 外类；jc.jar 的 f.<clinit>
/// 直接 `new SkyPayment`，类缺失 → clinit 失败 → 静态字段全 null → 计费
/// 初始化 NPE 杀加载线程——冒烟实证）。req_sync 按 tlv 协议分型响应、
/// req_async 受理并模拟平台异步完成通知（SP-1，详录见函数文档与
/// smoke_boot 注释），游戏按无计费环境路径推进。
static PAYMENT_LISTENER_DECL: BootstrapDecl = iface("com/jblend/payment/PaymentListener", &[]);

static SKY_PAYMENT_DECL: BootstrapDecl = klass(
    "com/jblend/payment/SkyPayment",
    "java/lang/Object",
    &[
        m("<init>", "()V", acc::PUBLIC, stub_void),
        // 同步请求：斯凯平台函数 tlv 协议模拟（SP-1，格式逆向详录见
        // smoke_boot 注释）——按 funcode 区分通道打开/功能调用，返回
        // 手工构造的合法最小 tlv 响应。
        m("req_sync", "(I[BII)[B", acc::PUBLIC, sky_payment_req_sync),
        // 异步请求：受理并模拟平台异步完成通知（见 sky_payment_req_async）。
        m("req_async", "(I[BII)I", acc::PUBLIC, sky_payment_req_async),
        m(
            "setPaymentListener",
            "(Lcom/jblend/payment/PaymentListener;)V",
            acc::PUBLIC,
            stub_void,
        ),
    ],
);

/// 斯凯平台函数 tlv 协议 req_sync 模拟（SP-1）。
///
/// jar 侧逆向结论（f/d/NetpayJAgent/NetpayJar/be 反汇编，详录见
/// smoke_boot 注释）：tlv 元素统一 `[4B 大端 type][4B 大端 len][载荷]`，
/// type 码 1=String（UNICODE/UTF-16LE 无 BOM，len=2*字符数）、2=Integer
/// （len 恒 4）、3=byte[]。`f.a(II[Object)` 打包参数后调
/// `req_sync(moduleId<<16|serviceID, bytes, 0, len)`，响应按同一 tlv 流
/// 解包回 Object[]。调用分型（按 funcode）：
///
/// - **funcode==0（通道打开）**：请求首个 tlv 是通道名 String。响应
///   `[Integer(0)][Integer(moduleId)]`（出参 objs[1] 必须是 Integer）：
///   - `"strres"` → moduleId=2（字符串资源通道，功能调用见下）；
///   - 其他（`"ntpstub"` 等）→ moduleId=1：NetpayJar 初始化链继续。
/// - **funcode 高 16 位==2（strres 功能调用）**：响应
///   `[Integer(0)][String("0")]`——出参 objs[1] 是 String，d.getUnicodeByte
///   经 String(bytes,"UNICODE") 还原 "0"，上游 be.a(I)I 的
///   Integer.parseInt 得 0（零计费/零计数语义）。不能走 null/空表降级：
///   e.a(I) 的缺省串是 "btye [] = null"，parseInt 抛 NumberFormatException
///   杀调用线程（冒烟实证的第二死点）。
/// - **其余 funcode!=0（netpay 等同步功能调用）**：返回空 byte[]（解包
///   Object[0]）。实证调用点 netpayJAgent_req 拿到返回值直接 `pop`
///   （丢弃），"已受理无结果"即合法。
fn sky_payment_req_sync(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    // 实参序：funcode(I) / 请求字节流([B) / offset(I) / length(I)。
    let funcode = match call.args.first() {
        Some(Slot::Int(value)) => *value,
        other => {
            return Err(KvmError::Internal(format!(
                "req_sync funcode expected int, got {other:?}"
            )));
        }
    };
    let request_bytes = || -> Option<Vec<u8>> {
        call.args
            .get(1)
            .and_then(|slot| match slot {
                Slot::Ref(reference) => Some(*reference),
                _ => None,
            })
            .and_then(|array| heap.array(array).ok())
            .map(|array| match &array.data {
                crate::runtime::object::ArrayData::Byte(data) => {
                    data.iter().map(|&b| b as u8).collect::<Vec<_>>()
                }
                _ => Vec::new(),
            })
    };
    let mut resp: Vec<u8> = Vec::new();
    if funcode == 0 {
        // 通道打开：strres 分配 moduleId=2，其余（ntpstub）为 1。
        let module_id = if request_bytes()
            .and_then(|data| sky_tlv_first_string(&data))
            .as_deref()
            == Some("strres")
        {
            2
        } else {
            1
        };
        sky_tlv_append_int(&mut resp, 0);
        sky_tlv_append_int(&mut resp, module_id);
    } else if ((funcode as u32) >> 16) == 2 {
        // strres 功能调用：[占位 Integer][String("0")] → parseInt 得 0。
        sky_tlv_append_int(&mut resp, 0);
        sky_tlv_append_string(&mut resp, "0");
    }
    Ok(Some(Slot::Ref(jio::alloc_byte_array_from(
        cs, heap, &resp,
    )?)))
}

/// 斯凯平台 req_async 模拟（SP-1）：受理（返回 0）并把异步完成响应
/// 入 `cs.pending_sky_results`——`Vm.poll` 段零 spawn 根级类 f 的静态
/// `a([B)V`（dispatchJNI）帧，按真机语义分发到通道回调
/// `c.skyJNIResult`。响应 tlv `[Integer(funcode 原样)][Integer(-101)]`：
/// - objs[0] Integer：dispatchJNI 解回 moduleId/serviceId 定位回调；
/// - objs[1] Integer(-101)：无计费环境错误码。下游实证链（逆向）：
///   NetpayJAgent.skyJNIResult（objs.length<3 无 byte[] 分支）→
///   netpayCheckRet(-101, 0, null)（payCheck!=1 直通分支）→
///   netpayResultCB(-101,...) → **e.g() 重启游戏主循环线程** + 清计费
///   UI 标志——be.e 尾部 e.f() 停主循环后，游戏的续命全靠这次回调
///   （无回调则调度器空表 → VM Dead，冒烟实证）。
fn sky_payment_req_async(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let funcode = match call.args.first() {
        Some(Slot::Int(value)) => *value,
        other => {
            return Err(KvmError::Internal(format!(
                "req_async funcode expected int, got {other:?}"
            )));
        }
    };
    let mut resp: Vec<u8> = Vec::new();
    sky_tlv_append_int(&mut resp, funcode);
    sky_tlv_append_int(&mut resp, -101);
    let bytes = jio::alloc_byte_array_from(cs, heap, &resp)?;
    cs.pending_sky_results.push(bytes);
    Ok(Some(Slot::Int(0)))
}

/// tlv 流追加一个 Integer 元素（type=2，len=4，大端值）。
fn sky_tlv_append_int(out: &mut Vec<u8>, value: i32) {
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&4u32.to_be_bytes());
    out.extend_from_slice(&(value as u32).to_be_bytes());
}

/// tlv 流追加一个 String 元素（type=1，UNICODE/UTF-16LE 无 BOM 载荷）。
fn sky_tlv_append_string(out: &mut Vec<u8>, text: &str) {
    let units = text.encode_utf16().collect::<Vec<u16>>();
    out.extend_from_slice(&1u32.to_be_bytes());
    out.extend_from_slice(&(units.len() as u32 * 2).to_be_bytes());
    for unit in units {
        out.extend_from_slice(&unit.to_le_bytes());
    }
}

/// 解析 tlv 流首个 String 元素（type=1，UNICODE/UTF-16LE 无 BOM 载荷）。
/// 非法流（长度不足/type 不符）返回 None，调用方按保守语义处理。
fn sky_tlv_first_string(raw: &[u8]) -> Option<String> {
    if raw.len() < 8 {
        return None;
    }
    let elem_type = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
    let len = u32::from_be_bytes([raw[4], raw[5], raw[6], raw[7]]) as usize;
    if elem_type != 1 || !len.is_multiple_of(2) || raw.len() < 8 + len {
        return None;
    }
    let units = raw[8..8 + len]
        .chunks(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    Some(String::from_utf16_lossy(&units))
}

/// bootstrap 声明表（进程内唯一）。
pub fn bootstrap_registry() -> &'static [&'static BootstrapDecl] {
    REGISTRY
}

/// 按类内部名查声明。
pub fn bootstrap_lookup(name: &str) -> Option<&'static BootstrapDecl> {
    REGISTRY.iter().find(|decl| decl.name == name).copied()
}

// ---------------------------------------------------------------------------
// 分派器
// ---------------------------------------------------------------------------

/// 按声明式注册表分派原生方法的 `NativeDispatch` 实现。
///
/// 分派键 = 声明类内部名 + 方法名 + 描述符（运行期从 `NativeCall` 的
/// ClassId/Sym 还原文本查表），`VmClass` 本身无需携带函数指针。
pub struct RegistryDispatch;

impl NativeDispatch for RegistryDispatch {
    fn call_native(
        &mut self,
        cs: &mut ClassSpace,
        heap: &mut Heap,
        host: &mut dyn HostContext,
        call: NativeCall,
    ) -> Result<Option<Slot>, KvmError> {
        // 先拷出符号文本再查表，避免与 `&mut cs` 的后续借用冲突。
        let class_name = cs.name_of(call.class).to_string();
        let method_name = cs.syms.name(call.name).to_string();
        let desc = cs.syms.name(call.desc).to_string();
        // 调试面：SKYENGINE_KVM_TRACE_NATIVES=1 打印 native 调用
        //（类.方法+描述符+线程）——冒烟停滞时统计高频调用定位等待源。
        if std::env::var_os("SKYENGINE_KVM_TRACE_NATIVES").is_some() {
            eprintln!(
                "[kvm-native] {}.{method_name}{desc} t={:?}",
                class_name, call.thread
            );
        }
        let Some(decl) = bootstrap_lookup(&class_name) else {
            return Err(KvmError::Linkage(format!(
                "native {method_name}{desc} on {class_name} has no bootstrap binding"
            )));
        };
        let Some(method) = decl
            .methods
            .iter()
            .find(|m| m.name == method_name && m.desc == desc)
        else {
            return Err(KvmError::Linkage(format!(
                "bootstrap native {class_name}.{method_name}{desc} not registered"
            )));
        };
        match method.func {
            NativeImpl::Pure(func) => func(cs, heap, &call),
            NativeImpl::Host(func) => func(cs, heap, host, &call),
        }
    }
}

/// 声明常量 → 类空间静态常量（字符串在此驻留成符号）。
pub(crate) fn to_static_const(cs: &mut ClassSpace, value: &BootstrapConst) -> StaticConst {
    match value {
        BootstrapConst::Int(v) => StaticConst::Int(*v),
        BootstrapConst::Long(v) => StaticConst::Long(*v),
        BootstrapConst::Float(v) => StaticConst::Float(*v),
        BootstrapConst::Double(v) => StaticConst::Double(*v),
        BootstrapConst::Str(text) => StaticConst::Str(cs.syms.intern(text)),
    }
}
