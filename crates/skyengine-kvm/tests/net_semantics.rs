//! GCF/HttpConnection 语义测试（T043）：T041 bootstrap 真实现的端到端
//! 验证（经 MockHostContext 的 http_request 脚本化响应）。
//!
//! 覆盖面：
//! - Connector.open 分派：http/https 返回非 null；socket:// 与未知协议
//!   → ConnectionNotFoundException（Java catch 断言）；
//! - GET 全流程：setRequestProperty → openInputStream（触发请求）→
//!   读 body 字节 → getResponseCode/getHeaderField/getLength/
//!   getHeaderFieldInt 未命中默认值；exec 后 Rust 侧断言 mock 收到的
//!   method/url/headers；
//! - POST：openOutputStream 写体 → flush → getResponseCode 触发
//!   connect；mock 收到 body 字节与方法；
//! - 懒 connect：getResponseCode 自动发起（不 openInputStream）；
//!   404/500 透传 + getResponseMessage + getHeaderField(null) 状态行；
//! - 响应头按序遍历：getHeaderFieldKey(0)/getHeaderField(0)/(1)、
//!   越界 null、getHeaderFieldInt 命中；
//! - 异常映射：HostNetError::Timeout → InterruptedIOException、
//!   Unsupported（脚本耗尽/引擎离线）→ ConnectionNotFoundException、
//!   Io(msg) → IOException（三段精确 catch，handler 只列目标类，
//!   未命中即冒泡失败——实现精确类型断言）；
//! - 状态机：connected 后 setRequestProperty ISE、close 后
//!   getResponseCode ISE、setup 态设头正常；
//! - URL 拆解：getProtocol/getHost/getPort（显式+默认）/getFile/
//!   getQuery/getRef（pure 轨不触发请求）；
//! - Connector 便捷法：openInputStream/openDataInputStream(String)
//!   即连即读、openDataOutputStream 返回非 null 且不触发请求。
//!
//! 断言策略与 media_semantics 一致：多指标折算一个 int 返回（组合公式
//! 在用例注释给出），Rust 侧独立算期望值比对；请求侧断言经
//! MockHostContext::last_http 在 exec 后核验。

#[allow(dead_code)]
mod common;

use common::classbytes::{build_class, provider, HandlerSpec, MethodSpec, Pool};
use skyengine_kvm::host::mock::MockHostContext;
use skyengine_kvm::host::{HttpRequest, HttpResponse};
use skyengine_kvm::interp::natives::RegistryDispatch;
use skyengine_kvm::interp::{enter, run, Machine, RunOutcome};
use skyengine_kvm::runtime::classspace::ClassSpace;
use skyengine_kvm::runtime::heap::Heap;
use skyengine_kvm::runtime::monitor::Monitors;
use skyengine_kvm::runtime::{Slot, ThreadId};
use skyengine_kvm::KvmError;

// ---- 操作码 ---------------------------------------------------------------

const ICONST_0: u8 = 0x03;
const ICONST_1: u8 = 0x04;
const BIPUSH: u8 = 0x10;
const SIPUSH: u8 = 0x11;
const LDC_W: u8 = 0x13;
const ILOAD: u8 = 0x15;
const ALOAD: u8 = 0x19;
const ISTORE: u8 = 0x36;
const ASTORE: u8 = 0x3a;
const IINC: u8 = 0x84;
const L2I: u8 = 0x88;
const IADD: u8 = 0x60;
const IFEQ: u8 = 0x99;
const IF_ICMPNE: u8 = 0xa0;
const IFNULL: u8 = 0xc6;
const GOTO: u8 = 0xa7;
const IRETURN: u8 = 0xac;
const POP: u8 = 0x57;
const ACONST_NULL: u8 = 0x01;
const INVOKEVIRTUAL: u8 = 0xb6;
const INVOKESTATIC: u8 = 0xb8;

/// 主线程标识（与 media_semantics 夹具一致）。
const MAIN: ThreadId = ThreadId(1);

const CONNECTOR: &str = "javax/microedition/io/Connector";
const HCIMPL: &str = "javax/microedition/io/HttpConnectionImpl";
const HOUT: &str = "javax/microedition/io/HttpOutputStream";
const BAIS: &str = "java/io/ByteArrayInputStream";
const DIS: &str = "java/io/DataInputStream";

// ---- 夹具 ------------------------------------------------------------------

/// 走真注册表的测试 VM；host 留在字段供脚本注入与请求核验。
struct Vm {
    cs: ClassSpace,
    heap: Heap,
    monitors: Monitors,
    natives: RegistryDispatch,
    host: MockHostContext,
}

impl Vm {
    fn new(user: Vec<(&'static str, Vec<u8>)>) -> Self {
        let entries = user
            .into_iter()
            .map(|(name, bytes)| (name.to_string(), bytes))
            .collect();
        let mut cs = ClassSpace::new(Box::new(provider(entries)));
        let object = cs.load_class("java/lang/Object").expect("load Object");
        cs.object_class = Some(object);
        cs.load_class("java/lang/String").expect("load String");
        // 异常处理器捕获类预载（展开路径按类名匹配，与 media 夹具一致）。
        for name in [
            "javax/microedition/io/Connector",
            "javax/microedition/io/HttpConnectionImpl",
            "javax/io/ConnectionNotFoundException",
            "java/io/InterruptedIOException",
            "java/io/IOException",
            "java/lang/IllegalStateException",
            BAIS,
            DIS,
        ] {
            cs.load_class(name).unwrap_or_else(|e| panic!("load {name}: {e}"));
        }
        Self {
            cs,
            heap: Heap::new(),
            monitors: Monitors::new(),
            natives: RegistryDispatch,
            host: MockHostContext::new(240, 320),
        }
    }

    fn machine(&mut self) -> Machine<'_> {
        Machine {
            cs: &mut self.cs,
            heap: &mut self.heap,
            monitors: &mut self.monitors,
            natives: &mut self.natives,
            host: &mut self.host,
        }
    }

    /// 执行用户类 `T` 的静态 main（描述符由用例指定）。
    fn exec(&mut self, desc: &str) -> Result<RunOutcome, KvmError> {
        let class = self.cs.load_class("T")?;
        let mut frames = Vec::new();
        let mut machine = self.machine();
        enter(&mut machine, &mut frames, class, "main", desc, Vec::new())?;
        run(&mut machine, &mut frames, MAIN, None)
    }
}

// ---- 构码小工具 ------------------------------------------------------------

fn m_main(
    desc: &'static str,
    stack: u16,
    locals: u16,
    code: Vec<u8>,
    handlers: Vec<HandlerSpec>,
) -> MethodSpec {
    MethodSpec {
        access: 0x0008,
        name: "main",
        desc,
        code: Some((stack, locals, code)),
        handlers,
    }
}

fn handler(start: u16, end: u16, at: u16, catch: Option<&'static str>) -> HandlerSpec {
    HandlerSpec { start_pc: start, end_pc: end, handler_pc: at, catch_class: catch }
}

fn op3(op: u8, index: u16) -> Vec<u8> {
    let [hi, lo] = index.to_be_bytes();
    vec![op, hi, lo]
}

fn push_int(value: i32) -> Vec<u8> {
    match value {
        0 => vec![ICONST_0],
        1 => vec![ICONST_1],
        -128..=127 => vec![BIPUSH, value as i8 as u8],
        -32768..=32767 => {
            let [hi, lo] = (value as i16).to_be_bytes();
            vec![SIPUSH, hi, lo]
        }
        _ => panic!("测试常量超界: {value}"),
    }
}

/// store/load（int 与 ref 共用编号；本文件用 3=score、1/2=对象槽、4=临时）。
fn store_int(local: u8) -> Vec<u8> {
    vec![ISTORE, local]
}
fn load_int(local: u8) -> Vec<u8> {
    vec![ILOAD, local]
}
fn store_ref(local: u8) -> Vec<u8> {
    vec![ASTORE, local]
}
fn load_ref(local: u8) -> Vec<u8> {
    vec![ALOAD, local]
}

/// score(local) += delta（ILOAD+PUSH+IADD+ISTORE 通用序列，
/// 避免 IINC 单字节 const 的量程限制）。
fn bump(local: u8, delta: i16) -> Vec<u8> {
    let mut code = load_int(local);
    code.extend(push_int(delta as i32));
    code.push(IADD);
    code.extend(store_int(local));
    code
}

/// 回填 3 字节分支偏移（相对指令首字节；对齐 media_semantics 的 patch）。
fn patch(code: &mut [u8], at: u16, target: u16) {
    let offset = target as i16 - at as i16;
    code[at as usize + 1..at as usize + 3].copy_from_slice(&offset.to_be_bytes());
}

/// 构类 + 建 VM + 执行 + 取 int 返回（无需 host 交互的用例）。
fn run_int(
    pool: &mut Pool,
    stack: u16,
    locals: u16,
    code: Vec<u8>,
    handlers: Vec<HandlerSpec>,
) -> i32 {
    vm_int(pool, stack, locals, code, handlers).0
}

/// 构类 + 建 VM（不执行）：返回 (结果, VM) 供 host 预注入。
fn build_vm(
    pool: &mut Pool,
    stack: u16,
    locals: u16,
    code: Vec<u8>,
    handlers: Vec<HandlerSpec>,
) -> Vm {
    let bytes = build_class(
        pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_main("()I", stack, locals, code, handlers)],
    );
    Vm::new(vec![("T", bytes)])
}

/// 执行并取 int 返回（exec 前已注入脚本的 VM）。
fn vm_exec(vm: &mut Vm) -> i32 {
    match vm.exec("()I").expect("run main") {
        RunOutcome::Returned(Some(Slot::Int(value))) => value,
        other => panic!("expected int return, got {other:?}"),
    }
}

fn vm_int(
    pool: &mut Pool,
    stack: u16,
    locals: u16,
    code: Vec<u8>,
    handlers: Vec<HandlerSpec>,
) -> (i32, Vm) {
    let mut vm = build_vm(pool, stack, locals, code, handlers);
    let value = vm_exec(&mut vm);
    (value, vm)
}

// ---- 方法索引速记 ----------------------------------------------------------

/// Connector.open(String)。
fn conn_open(pool: &mut Pool) -> u16 {
    pool.methodref(
        CONNECTOR,
        "open",
        "(Ljava/lang/String;)Ljavax/microedition/io/Connection;",
    )
}

/// HttpConnectionImpl 实例方法。
fn hcm(pool: &mut Pool, name: &str, desc: &str) -> u16 {
    pool.methodref(HCIMPL, name, desc)
}

/// String.equals(Object)Z。
fn str_equals(pool: &mut Pool) -> u16 {
    pool.methodref("java/lang/String", "equals", "(Ljava/lang/Object;)Z")
}

/// 压字符串常量。
fn ldc(pool: &mut Pool, text: &str) -> Vec<u8> {
    op3(LDC_W, pool.string(text))
}

/// open(url) → astore(local)。
fn open_code(pool: &mut Pool, url: &str, local: u8) -> Vec<u8> {
    let mut code = ldc(pool, url);
    code.extend(op3(INVOKESTATIC, conn_open(pool)));
    code.extend(store_ref(local));
    code
}

/// equals 断言段：obj 在 local，命中 score(local 3) 加 weight；
/// IFEQ 偏移内部回填（false 跳过加分）。
fn equals_check(
    code: &mut Vec<u8>,
    pool: &mut Pool,
    local: u8,
    expect: &str,
    weight: i16,
) {
    let eq = str_equals(pool);
    code.extend(load_ref(local));
    code.extend(ldc(pool, expect));
    code.extend(op3(INVOKEVIRTUAL, eq));
    let at = code.len() as u16;
    code.extend(vec![IFEQ, 0, 0]);
    code.extend(bump(3, weight));
    let after = code.len() as u16;
    patch(code, at, after);
}

/// 响应速记。
fn resp(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Result<HttpResponse, skyengine_kvm::host::HostNetError> {
    Ok(HttpResponse {
        status,
        headers: headers.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect(),
        body: body.to_vec(),
    })
}

// ---- 用例 ------------------------------------------------------------------

/// open 分派：http/https 非 null（+1/+1）；socket:// 与未知协议 →
/// ConnectionNotFoundException（+10/+100）。返回 = 1+1+10+100 = 112。
#[test]
fn open_dispatch_supported_and_rejected() {
    let mut pool = Pool::new();
    let mut code = Vec::new();
    code.push(ICONST_0);
    code.extend(store_int(3));
    // http:// 非 null → +1
    code.extend(open_code(&mut pool, "http://a.example/", 1));
    let n1 = code.len() as u16;
    code.extend(load_ref(1));
    code.extend(vec![IFNULL, 0, 0]);
    code.extend(bump(3, 1));
    let e1 = code.len() as u16;
    // https:// 非 null → +1
    code.extend(open_code(&mut pool, "https://b.example/", 1));
    let n2 = code.len() as u16;
    code.extend(load_ref(1));
    code.extend(vec![IFNULL, 0, 0]);
    code.extend(bump(3, 1));
    let e2 = code.len() as u16;
    // socket:// → CNFE（+10）
    let t1 = code.len() as u16;
    code.extend(ldc(&mut pool, "socket://h.example:80"));
    code.extend(op3(INVOKESTATIC, conn_open(&mut pool)));
    let t1e = code.len() as u16;
    code.push(POP);
    let h1 = code.len() as u16;
    code.extend(store_ref(5)); // 异常对象入专用槽（不覆盖连接引用）
    code.extend(bump(3, 10));
    // 未知协议 ftp:// → CNFE（+100）
    let t2 = code.len() as u16;
    code.extend(ldc(&mut pool, "ftp://c.example/x"));
    code.extend(op3(INVOKESTATIC, conn_open(&mut pool)));
    let t2e = code.len() as u16;
    code.push(POP);
    let h2 = code.len() as u16;
    code.extend(store_ref(5));
    code.extend(bump(3, 100));
    code.extend(load_int(3));
    code.push(IRETURN);

    patch(&mut code, n1 + 1, e1); // IFNULL（ALOAD_1 后 1 字节处）跳过 bump
    patch(&mut code, n2 + 1, e2);
    let handlers = vec![
        handler(t1, t1e, h1, Some("javax/io/ConnectionNotFoundException")),
        handler(t2, t2e, h2, Some("javax/io/ConnectionNotFoundException")),
    ];
    assert_eq!(run_int(&mut pool, 6, 6, code, handlers), 112);
}

/// GET 全流程：读 body 字节和(532) + 状态码(200) + content-type 头命中
/// (1000) + getLength(5) + getHeaderFieldInt 未命中默认(77) = 1814；
/// Rust 侧断言 mock 收到 GET/url/X-Test 头/无 body。
#[test]
fn get_flow_request_and_response() {
    let mut pool = Pool::new();
    let url = "http://h.example/p?a=1";
    let set_prop = hcm(&mut pool, "setRequestProperty", "(Ljava/lang/String;Ljava/lang/String;)V");
    let open_in = hcm(&mut pool, "openInputStream", "()Ljava/io/InputStream;");
    let read = pool.methodref(BAIS, "read", "()I");
    let code_of = hcm(&mut pool, "getResponseCode", "()I");
    let header = hcm(&mut pool, "getHeaderField", "(Ljava/lang/String;)Ljava/lang/String;");
    let length = hcm(&mut pool, "getLength", "()J");
    let header_int = hcm(&mut pool, "getHeaderFieldInt", "(Ljava/lang/String;I)I");

    let mut code = Vec::new();
    code.push(ICONST_0);
    code.extend(store_int(3));
    code.extend(open_code(&mut pool, url, 1));
    // setRequestProperty("X-Test","v1")
    code.extend(load_ref(1));
    code.extend(ldc(&mut pool, "X-Test"));
    code.extend(ldc(&mut pool, "v1"));
    code.extend(op3(INVOKEVIRTUAL, set_prop));
    // openInputStream（触发请求）
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, open_in));
    code.extend(store_ref(2));
    // 读 5 字节求和：h+e+l+l+o = 532
    for _ in 0..5 {
        code.extend(load_ref(2));
        code.extend(op3(INVOKEVIRTUAL, read));
        code.extend(load_int(3));
        code.push(IADD);
        code.extend(store_int(3));
    }
    // getResponseCode → +200
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, code_of));
    code.extend(load_int(3));
    code.push(IADD);
    code.extend(store_int(3));
    // getHeaderField("content-type") == "text/plain"（大小写不敏感）→ +1000
    code.extend(load_ref(1));
    code.extend(ldc(&mut pool, "content-type"));
    code.extend(op3(INVOKEVIRTUAL, header));
    code.extend(store_ref(4));
    equals_check(&mut code, &mut pool, 4, "text/plain", 1000);
    // getLength() → 5（long → L2I）
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, length));
    code.push(L2I);
    code.extend(load_int(3));
    code.push(IADD);
    code.extend(store_int(3));
    // getHeaderFieldInt("X-Miss", 77) 未命中 → +77
    code.extend(load_ref(1));
    code.extend(ldc(&mut pool, "X-Miss"));
    code.extend(push_int(77));
    code.extend(op3(INVOKEVIRTUAL, header_int));
    code.extend(load_int(3));
    code.push(IADD);
    code.extend(store_int(3));
    code.extend(load_int(3));
    code.push(IRETURN);

    let mut vm = build_vm(&mut pool, 8, 5, code, Vec::new());
    vm.host.push_http(resp(
        200,
        &[("Content-Type", "text/plain"), ("Content-Length", "5")],
        b"hello",
    ));
    assert_eq!(vm_exec(&mut vm), 1814);
    let request = vm.host.last_http().expect("mock received request");
    assert_eq!(request.method, "GET");
    assert_eq!(request.url, url);
    assert!(
        request
            .headers
            .iter()
            .any(|(name, value)| name == "X-Test" && value == "v1"),
        "headers should carry X-Test: {:?}",
        request.headers
    );
    assert_eq!(request.body, None);
}

/// POST：openOutputStream 写 abc → flush → getResponseCode 触发 connect
/// （返回 200）；mock 收到 method POST + body [97,98,99]。
#[test]
fn post_body_reaches_host() {
    let mut pool = Pool::new();
    let set_method = hcm(&mut pool, "setRequestMethod", "(Ljava/lang/String;)V");
    let open_out = hcm(&mut pool, "openOutputStream", "()Ljava/io/OutputStream;");
    let write = pool.methodref(HOUT, "write", "(I)V");
    let flush = pool.methodref(HOUT, "flush", "()V");
    let code_of = hcm(&mut pool, "getResponseCode", "()I");

    let mut code = Vec::new();
    code.push(ICONST_0);
    code.extend(store_int(3));
    code.extend(open_code(&mut pool, "http://h.example/submit", 1));
    code.extend(load_ref(1));
    code.extend(ldc(&mut pool, "POST"));
    code.extend(op3(INVOKEVIRTUAL, set_method));
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, open_out));
    code.extend(store_ref(2));
    for byte in [97u8, 98, 99] {
        code.extend(load_ref(2));
        code.extend(push_int(byte as i32));
        code.extend(op3(INVOKEVIRTUAL, write));
    }
    code.extend(load_ref(2));
    code.extend(op3(INVOKEVIRTUAL, flush));
    // getResponseCode 触发 connect；返回状态码
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, code_of));
    code.push(IRETURN);

    let mut vm = build_vm(&mut pool, 8, 4, code, Vec::new());
    vm.host.push_http(resp(200, &[], b""));
    assert_eq!(vm_exec(&mut vm), 200);
    let request = vm.host.last_http().expect("mock received request");
    assert_eq!(request.method, "POST");
    assert_eq!(request.body, Some(vec![97, 98, 99]));
}

/// 懒 connect：不 openInputStream，getResponseCode 自动发起。
/// 404(+0 折算基数) + "Not Found" 消息(1000) + 状态行(100) +
/// 第二连接 500 = 404+1000+100+500 = 2004。
#[test]
fn auto_connect_status_codes_and_message() {
    let mut pool = Pool::new();
    let code_of = hcm(&mut pool, "getResponseCode", "()I");
    let message = hcm(&mut pool, "getResponseMessage", "()Ljava/lang/String;");
    let header = hcm(&mut pool, "getHeaderField", "(Ljava/lang/String;)Ljava/lang/String;");

    let mut code = Vec::new();
    code.push(ICONST_0);
    code.extend(store_int(3));
    code.extend(open_code(&mut pool, "http://h.example/missing", 1));
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, code_of));
    code.extend(store_int(3)); // 404
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, message));
    code.extend(store_ref(4));
    equals_check(&mut code, &mut pool, 4, "Not Found", 1000);
    // getHeaderField(null) = 状态行
    code.extend(load_ref(1));
    code.push(ACONST_NULL);
    code.extend(op3(INVOKEVIRTUAL, header));
    code.extend(store_ref(4));
    equals_check(&mut code, &mut pool, 4, "HTTP/1.1 404 Not Found", 100);
    // 第二连接 500 透传
    code.extend(open_code(&mut pool, "http://h.example/boom", 1));
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, code_of));
    code.extend(load_int(3));
    code.push(IADD);
    code.extend(store_int(3));
    code.extend(load_int(3));
    code.push(IRETURN);

    let mut vm = build_vm(&mut pool, 8, 5, code, Vec::new());
    vm.host.push_http(resp(404, &[("X-A", "1")], b""));
    vm.host.push_http(resp(500, &[], b""));
    assert_eq!(vm_exec(&mut vm), 2004);
}

/// 响应头按序遍历：Key(0)/Field(0)/Field(1) 命中(1/10/100) + 越界
/// Key(5) null(1000) + getHeaderFieldInt 命中(123) = 1234。
#[test]
fn header_enumeration_by_index() {
    let mut pool = Pool::new();
    let code_of = hcm(&mut pool, "getResponseCode", "()I");
    let key_at = hcm(&mut pool, "getHeaderFieldKey", "(I)Ljava/lang/String;");
    let field_at = hcm(&mut pool, "getHeaderField", "(I)Ljava/lang/String;");
    let header_int = hcm(&mut pool, "getHeaderFieldInt", "(Ljava/lang/String;I)I");

    let mut code = Vec::new();
    code.push(ICONST_0);
    code.extend(store_int(3));
    code.extend(open_code(&mut pool, "http://h.example/list", 1));
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, code_of));
    code.push(POP); // connect（200）
    // Key(0) == "Content-Type" → +1
    code.extend(load_ref(1));
    code.push(ICONST_0);
    code.extend(op3(INVOKEVIRTUAL, key_at));
    code.extend(store_ref(4));
    equals_check(&mut code, &mut pool, 4, "Content-Type", 1);
    // Field(0) == "a/b" → +10
    code.extend(load_ref(1));
    code.push(ICONST_0);
    code.extend(op3(INVOKEVIRTUAL, field_at));
    code.extend(store_ref(4));
    equals_check(&mut code, &mut pool, 4, "a/b", 10);
    // Field(1) == "123" → +100
    code.extend(load_ref(1));
    code.push(ICONST_1);
    code.extend(op3(INVOKEVIRTUAL, field_at));
    code.extend(store_ref(4));
    equals_check(&mut code, &mut pool, 4, "123", 100);
    // Key(5) 越界 → null → +1000（IFNULL：null 时落入加分段）
    code.extend(load_ref(1));
    code.extend(push_int(5));
    code.extend(op3(INVOKEVIRTUAL, key_at));
    code.extend(store_ref(4));
    code.extend(load_ref(4));
    let not_null = code.len() as u16;
    code.extend(vec![IFNULL, 0, 0]);
    code.extend(vec![GOTO, 0, 0]);
    let hit = code.len() as u16;
    code.extend(bump(3, 1000));
    let after = code.len() as u16;
    patch(&mut code, not_null, hit);
    patch(&mut code, not_null + 3, after);
    // getHeaderFieldInt("X-Num", -1) == 123 → +123
    code.extend(load_ref(1));
    code.extend(ldc(&mut pool, "X-Num"));
    code.extend(push_int(-1));
    code.extend(op3(INVOKEVIRTUAL, header_int));
    code.extend(load_int(3));
    code.push(IADD);
    code.extend(store_int(3));
    code.extend(load_int(3));
    code.push(IRETURN);

    let mut vm = build_vm(&mut pool, 8, 5, code, Vec::new());
    vm.host.push_http(resp(200, &[("Content-Type", "a/b"), ("X-Num", "123")], b""));
    assert_eq!(vm_exec(&mut vm), 1234);
}

/// 异常映射三段精确 catch：Timeout → InterruptedIOException(+1)、
/// Unsupported（平台无网络能力）→ ConnectionNotFoundException(+10)、
/// Io("boom") → IOException(+100)。返回 111。
#[test]
fn host_errors_map_to_exception_types() {
    let mut pool = Pool::new();
    let code_of = hcm(&mut pool, "getResponseCode", "()I");

    let mut code = Vec::new();
    code.push(ICONST_0);
    code.extend(store_int(3));
    // 段 1：Timeout
    let t1 = code.len() as u16;
    code.extend(open_code(&mut pool, "http://h.example/slow", 1));
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, code_of));
    let t1e = code.len() as u16;
    code.push(POP);
    let h1 = code.len() as u16;
    code.extend(store_ref(5));
    code.extend(bump(3, 1));
    // 段 2：空队列 Unsupported
    let t2 = code.len() as u16;
    code.extend(open_code(&mut pool, "http://h.example/offline", 1));
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, code_of));
    let t2e = code.len() as u16;
    code.push(POP);
    let h2 = code.len() as u16;
    code.extend(store_ref(5));
    code.extend(bump(3, 10));
    // 段 3：Io
    let t3 = code.len() as u16;
    code.extend(open_code(&mut pool, "http://h.example/broken", 1));
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, code_of));
    let t3e = code.len() as u16;
    code.push(POP);
    let h3 = code.len() as u16;
    code.extend(store_ref(5));
    code.extend(bump(3, 100));
    code.extend(load_int(3));
    code.push(IRETURN);

    let handlers = vec![
        handler(t1, t1e, h1, Some("java/io/InterruptedIOException")),
        handler(t2, t2e, h2, Some("javax/io/ConnectionNotFoundException")),
        handler(t3, t3e, h3, Some("java/io/IOException")),
    ];
    let mut vm = build_vm(&mut pool, 6, 6, code, handlers);
    vm.host
        .push_http(Err(skyengine_kvm::host::HostNetError::Timeout));
    vm.host
        .push_http(Err(skyengine_kvm::host::HostNetError::Unsupported));
    vm.host
        .push_http(Err(skyengine_kvm::host::HostNetError::Io("boom".into())));
    assert_eq!(vm_exec(&mut vm), 111);
}

/// 状态机：connected 后 setRequestProperty ISE(+1)、close 后
/// getResponseCode ISE(+10)、新连接 setup 态设头正常(+100)。返回 111。
#[test]
fn state_machine_guards() {
    let mut pool = Pool::new();
    let set_prop = hcm(&mut pool, "setRequestProperty", "(Ljava/lang/String;Ljava/lang/String;)V");
    let open_in = hcm(&mut pool, "openInputStream", "()Ljava/io/InputStream;");
    let close = hcm(&mut pool, "close", "()V");
    let code_of = hcm(&mut pool, "getResponseCode", "()I");

    let mut code = Vec::new();
    code.push(ICONST_0);
    code.extend(store_int(3));
    code.extend(open_code(&mut pool, "http://h.example/a", 1));
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, open_in));
    code.extend(store_ref(2));
    // connected 后 setRequestProperty → ISE
    let t1 = code.len() as u16;
    code.extend(load_ref(1));
    code.extend(ldc(&mut pool, "X-Late"));
    code.extend(ldc(&mut pool, "v"));
    code.extend(op3(INVOKEVIRTUAL, set_prop));
    let t1e = code.len() as u16;
    let h1 = code.len() as u16;
    code.extend(store_ref(5));
    code.extend(bump(3, 1));
    // close 后 getResponseCode → ISE
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, close));
    let t2 = code.len() as u16;
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, code_of));
    let t2e = code.len() as u16;
    code.push(POP);
    let h2 = code.len() as u16;
    code.extend(store_ref(5));
    code.extend(bump(3, 10));
    // 新连接 setup 态设头正常 → +100
    code.extend(open_code(&mut pool, "http://h.example/b", 1));
    code.extend(load_ref(1));
    code.extend(ldc(&mut pool, "X-Ok"));
    code.extend(ldc(&mut pool, "v"));
    code.extend(op3(INVOKEVIRTUAL, set_prop));
    code.extend(bump(3, 100));
    code.extend(load_int(3));
    code.push(IRETURN);

    let handlers = vec![
        handler(t1, t1e, h1, Some("java/lang/IllegalStateException")),
        handler(t2, t2e, h2, Some("java/lang/IllegalStateException")),
    ];
    let mut vm = build_vm(&mut pool, 6, 6, code, handlers);
    vm.host.push_http(resp(200, &[], b""));
    assert_eq!(vm_exec(&mut vm), 111);
}

/// URL 拆解（pure 轨，不触发请求）："http://h.example:8080/p?q=1#f"：
/// protocol(1)+host(2)+port 8080(4)+file "/p?q=1"(8)+query "q=1"(16)+
/// ref "f"(32)；默认端口 80(64)。返回 127；last_http 为 None。
#[test]
fn url_part_getters() {
    let mut pool = Pool::new();
    let get_protocol = hcm(&mut pool, "getProtocol", "()Ljava/lang/String;");
    let get_host = hcm(&mut pool, "getHost", "()Ljava/lang/String;");
    let get_port = hcm(&mut pool, "getPort", "()I");
    let get_file = hcm(&mut pool, "getFile", "()Ljava/lang/String;");
    let get_query = hcm(&mut pool, "getQuery", "()Ljava/lang/String;");
    let get_ref = hcm(&mut pool, "getRef", "()Ljava/lang/String;");

    let mut code = Vec::new();
    code.push(ICONST_0);
    code.extend(store_int(3));
    code.extend(open_code(&mut pool, "http://h.example:8080/p?q=1#f", 1));
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, get_protocol));
    code.extend(store_ref(4));
    equals_check(&mut code, &mut pool, 4, "http", 1);
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, get_host));
    code.extend(store_ref(4));
    equals_check(&mut code, &mut pool, 4, "h.example", 2);
    // getPort == 8080
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, get_port));
    code.extend(push_int(8080));
    let c1 = code.len() as u16;
    code.extend(vec![IF_ICMPNE, 0, 0]);
    code.extend(bump(3, 4));
    let c1_end = code.len() as u16;
    patch(&mut code, c1, c1_end);
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, get_file));
    code.extend(store_ref(4));
    equals_check(&mut code, &mut pool, 4, "/p?q=1", 8);
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, get_query));
    code.extend(store_ref(4));
    equals_check(&mut code, &mut pool, 4, "q=1", 16);
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, get_ref));
    code.extend(store_ref(4));
    equals_check(&mut code, &mut pool, 4, "f", 32);
    // 默认端口
    code.extend(open_code(&mut pool, "http://h2.example/", 1));
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, get_port));
    code.extend(push_int(80));
    let c2 = code.len() as u16;
    code.extend(vec![IF_ICMPNE, 0, 0]);
    code.extend(bump(3, 64));
    let c2_end = code.len() as u16;
    patch(&mut code, c2, c2_end);
    code.extend(load_int(3));
    code.push(IRETURN);

    let (value, vm) = vm_int(&mut pool, 8, 5, code, Vec::new());
    assert_eq!(value, 127);
    assert!(vm.host.last_http().is_none(), "pure getters must not connect");
}

/// Connector 便捷法：openInputStream(String) 即连即读（65+66）、
/// openDataInputStream(String) 读 1 字节(67)+1000、
/// openDataOutputStream(String) 非 null(10000) 且不触发请求。
/// 返回 65+66+67+1000+10000 = 11198。
#[test]
fn connector_convenience_methods() {
    let mut pool = Pool::new();
    let open_is = pool.methodref(
        CONNECTOR,
        "openInputStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;",
    );
    let open_dis = pool.methodref(
        CONNECTOR,
        "openDataInputStream",
        "(Ljava/lang/String;)Ljava/io/DataInputStream;",
    );
    let open_dos = pool.methodref(
        CONNECTOR,
        "openDataOutputStream",
        "(Ljava/lang/String;)Ljava/io/DataOutputStream;",
    );
    let read_bais = pool.methodref(BAIS, "read", "()I");
    let read_dis = pool.methodref(DIS, "read", "()I");

    let mut code = Vec::new();
    code.push(ICONST_0);
    code.extend(store_int(3));
    // openInputStream("http://c.example/1") → 读 2 字节
    code.extend(ldc(&mut pool, "http://c.example/1"));
    code.extend(op3(INVOKESTATIC, open_is));
    code.extend(store_ref(2));
    for _ in 0..2 {
        code.extend(load_ref(2));
        code.extend(op3(INVOKEVIRTUAL, read_bais));
        code.extend(load_int(3));
        code.push(IADD);
        code.extend(store_int(3));
    }
    // openDataInputStream("http://c.example/2") → 读 1 字节 +1000
    code.extend(ldc(&mut pool, "http://c.example/2"));
    code.extend(op3(INVOKESTATIC, open_dis));
    code.extend(store_ref(2));
    code.extend(load_ref(2));
    code.extend(op3(INVOKEVIRTUAL, read_dis));
    code.extend(load_int(3));
    code.push(IADD);
    code.extend(store_int(3));
    code.extend(bump(3, 1000));
    // openDataOutputStream 非 null → +10000（不触发请求）
    code.extend(ldc(&mut pool, "http://c.example/3"));
    code.extend(op3(INVOKESTATIC, open_dos));
    code.extend(store_ref(2));
    let n1 = code.len() as u16;
    code.extend(load_ref(2));
    code.extend(vec![IFNULL, 0, 0]);
    code.extend(bump(3, 10000));
    let skip_to = code.len() as u16;
    patch(&mut code, n1 + 1, skip_to);
    code.extend(load_int(3));
    code.push(IRETURN);

    let mut vm = build_vm(&mut pool, 8, 4, code, Vec::new());
    vm.host.push_http(resp(200, &[("Content-Length", "2")], b"AB"));
    vm.host.push_http(resp(200, &[], b"C"));
    assert_eq!(vm_exec(&mut vm), 11198);
    // 最后一次请求来自 openDataInputStream（u2）；openDataOutputStream 不发。
    let request = vm.host.last_http().expect("mock received request");
    assert_eq!(request.url, "http://c.example/2");
}
