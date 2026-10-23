//! RMS RecordStore 语义测试（T037）：bootstrap 真实现的端到端验证。
//!
//! 每个用例手构用户类 `T`（字节码见 `common/classbytes.rs`），经真
//! 注册表调用 javax/microedition/rms natives，断言 MIDP-2.0 语义：
//! add/get 字节精确往返（含中文 UTF-8）、set/delete + InvalidRecordID、
//! 元数据（num/next/version/lastModified）、跨 VM 重启持久（同一
//! MockHostContext 重建 Vm）、open 不存在/删除后 NotFound、枚举遍历、
//! close 后操作 RecordStoreException。
//!
//! 断言策略：Java 侧把多指标折算成一个 int 返回（组合公式在用例
//! 注释里给出），Rust 侧独立计算期望值比对——两侧算法独立，等值
//! 才能证明往返正确。

#[allow(dead_code)]
mod common;

use common::classbytes::{build_class, provider, HandlerSpec, MethodSpec, Pool};

use skyengine_kvm::host::mock::MockHostContext;
use skyengine_kvm::interp::natives::RegistryDispatch;
use skyengine_kvm::interp::{enter, run, Machine, RunOutcome};
use skyengine_kvm::runtime::classspace::ClassSpace;
use skyengine_kvm::runtime::heap::Heap;
use skyengine_kvm::runtime::monitor::Monitors;
use skyengine_kvm::runtime::{Slot, ThreadId};
use skyengine_kvm::KvmError;

// ---- 操作码 ---------------------------------------------------------------

const ACONST_NULL: u8 = 0x01;
const ICONST_0: u8 = 0x03;
const ICONST_1: u8 = 0x04;
const ICONST_2: u8 = 0x05;
const BIPUSH: u8 = 0x10;
const SIPUSH: u8 = 0x11;
const LDC_W: u8 = 0x13;
const LCONST_0: u8 = 0x09;
const LCMP: u8 = 0x94;
const ILOAD_2: u8 = 0x1c;
const ISTORE_2: u8 = 0x3d;
const ALOAD_1: u8 = 0x2b;
const ALOAD_2: u8 = 0x2c;
const ASTORE_1: u8 = 0x4c;
const BALOAD: u8 = 0x33;
const BASTORE: u8 = 0x53;
const POP: u8 = 0x57;
const IMUL: u8 = 0x68;
const IADD: u8 = 0x60;
const IAND: u8 = 0x7e;
const IINC: u8 = 0x84;
const IFEQ: u8 = 0x99;
const IF_ICMPGE: u8 = 0xa2;
const GOTO: u8 = 0xa7;
const IRETURN: u8 = 0xac;
const INVOKEVIRTUAL: u8 = 0xb6;
const INVOKESTATIC: u8 = 0xb8;
const NEWARRAY: u8 = 0xbc;
const ARRAYLENGTH: u8 = 0xbe;

/// newarray 的 atype（byte）。
const T_BYTE: u8 = 8;

/// 主线程标识（与 cldc_semantics 夹具一致）。
const MAIN: ThreadId = ThreadId(1);

const STORE: &str = "javax/microedition/rms/RecordStore";

// ---- 夹具 ------------------------------------------------------------------

/// 走真注册表的测试 VM；host 注入式构造（跨重启持久用例复用同一
/// MockHostContext 重建 VM）。
struct Vm {
    cs: ClassSpace,
    heap: Heap,
    monitors: Monitors,
    natives: RegistryDispatch,
    host: MockHostContext,
}

impl Vm {
    fn new(user: Vec<(&'static str, Vec<u8>)>, host: MockHostContext) -> Self {
        let entries = user
            .into_iter()
            .map(|(name, bytes)| (name.to_string(), bytes))
            .collect();
        let mut cs = ClassSpace::new(Box::new(provider(entries)));
        let object = cs.load_class("java/lang/Object").expect("load Object");
        cs.object_class = Some(object);
        // StrObj 载荷运行时类按"已加载"查找。
        cs.load_class("java/lang/String").expect("load String");
        // 处理器表捕获类预载（展开路径按类名匹配）。
        for name in [
            "javax/microedition/rms/RecordStoreException",
            "javax/microedition/rms/RecordStoreNotFoundException",
            "javax/microedition/rms/RecordStoreFullException",
            "javax/microedition/rms/InvalidRecordIDException",
        ] {
            cs.load_class(name).unwrap_or_else(|e| panic!("load {name}: {e}"));
        }
        Self {
            cs,
            heap: Heap::new(),
            monitors: Monitors::new(),
            natives: RegistryDispatch,
            host,
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

    /// 取回 host（跨重启用例：host 移交下一台 VM）。
    fn into_host(self) -> MockHostContext {
        self.host
    }
}

// ---- 构码小工具 ------------------------------------------------------------

fn m_code(desc: &'static str, stack: u16, locals: u16, code: Vec<u8>) -> MethodSpec {
    MethodSpec {
        access: 0x0008,
        name: "main",
        desc,
        code: Some((stack, locals, code)),
        handlers: Vec::new(),
    }
}

fn m_catch(
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

/// 单常量池操作数指令（ldc/invoke* 均 3 字节）。
fn op3(op: u8, index: u16) -> [u8; 3] {
    let [hi, lo] = index.to_be_bytes();
    [op, hi, lo]
}

/// int 字面压栈（覆盖本文件用到的量程）。
fn push_int(value: i32) -> Vec<u8> {
    match value {
        0 => vec![ICONST_0],
        1 => vec![ICONST_1],
        2 => vec![ICONST_2],
        -128..=127 => vec![BIPUSH, value as i8 as u8],
        -32768..=32767 => {
            let [hi, lo] = (value as i16).to_be_bytes();
            vec![SIPUSH, hi, lo]
        }
        _ => panic!("测试常量超界: {value}"),
    }
}

/// 压 int 局部变量。
fn load_int(local: u8) -> Vec<u8> {
    match local {
        2 => vec![ILOAD_2],
        n => vec![0x15, n],
    }
}

/// 存 int 局部变量。
fn store_int(local: u8) -> Vec<u8> {
    match local {
        2 => vec![ISTORE_2],
        n => vec![0x36, n],
    }
}

/// 压引用局部变量。
fn load_ref(local: u8) -> Vec<u8> {
    match local {
        1 => vec![ALOAD_1],
        2 => vec![ALOAD_2],
        n => vec![0x19, n],
    }
}

/// 构造 byte[] 到指定局部变量（NEWARRAY + 逐字节 BASTORE）。
fn bytes_into_local(data: &[u8], local: u8) -> Vec<u8> {
    let mut code = push_int(data.len() as i32);
    code.push(NEWARRAY);
    code.push(T_BYTE);
    code.extend(store_int(local)); // 引用槽同 int 槽位编码
    for (index, &byte) in data.iter().enumerate() {
        // BASTORE 栈序：arrayref, index, value。
        code.extend(load_ref(local));
        code.extend(push_int(index as i32));
        code.extend(push_int(byte as i32));
        code.push(BASTORE);
    }
    code
}

/// 回填 3 字节分支偏移（相对指令首字节）。
fn patch(code: &mut [u8], at: u16, target: u16) {
    let offset = target as i16 - at as i16;
    code[at as usize + 1..at as usize + 3].copy_from_slice(&offset.to_be_bytes());
}

/// 回填到"当前末尾"的分支（先取长度再借用，避开借用冲突）。
fn patch_to_end(code: &mut [u8], at: u16) {
    let end = code.len() as u16;
    patch(code, at, end);
}

/// arr[0..] 的无符号字节校验和 → sum 局部变量：
/// `sum = 0; for i: sum += arr[i] & 0xFF`。
fn checksum_code(arr: u8, sum: u8, i: u8) -> Vec<u8> {
    let mut code = Vec::new();
    code.push(ICONST_0);
    code.extend(store_int(sum));
    code.push(ICONST_0);
    code.extend(store_int(i));
    let loop_start = code.len() as u16;
    // 条件：i >= arr.length 退出。
    code.extend(load_int(i));
    code.extend(load_ref(arr));
    code.push(ARRAYLENGTH);
    let if_pc = code.len() as u16;
    code.extend(vec![IF_ICMPGE, 0, 0]);
    // 累加体：sum += arr[i] & 0xFF。
    code.extend(load_int(sum));
    code.extend(load_ref(arr));
    code.extend(load_int(i));
    code.push(BALOAD);
    code.extend(push_int(0xFF));
    code.push(IAND);
    code.push(IADD);
    code.extend(store_int(sum));
    code.push(IINC);
    code.push(i);
    code.push(1);
    let goto_pc = code.len() as u16;
    code.extend(vec![GOTO, 0, 0]);
    let end = code.len() as u16;
    patch(&mut code, if_pc, end);
    patch(&mut code, goto_pc, loop_start);
    code
}

fn expect_int(outcome: RunOutcome) -> i32 {
    match outcome {
        RunOutcome::Returned(Some(Slot::Int(value))) => value,
        other => panic!("expected int return, got {other:?}"),
    }
}

fn run_int(
    pool: &mut Pool,
    desc: &'static str,
    stack: u16,
    locals: u16,
    code: Vec<u8>,
) -> i32 {
    let bytes = build_class(
        pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_code(desc, stack, locals, code)],
    );
    let mut vm = Vm::new(vec![("T", bytes)], MockHostContext::new(240, 320));
    expect_int(vm.exec(desc).expect("run main"))
}

// ---- 用例 ------------------------------------------------------------------

/// add×3（id 1/2/3）+ getRecord(2) 中文 UTF-8 字节精确往返。
/// 返回 = id 和 ×1000 + "你好" 的无符号校验和。
#[test]
fn add_and_get_roundtrip_with_utf8() {
    let mut pool = Pool::new();
    let name = pool.string("rms_t1");
    let open = pool.methodref(STORE, "openRecordStore", "(Ljava/lang/String;Z)Ljavax/microedition/rms/RecordStore;");
    let add = pool.methodref(STORE, "addRecord", "([BII)I");
    let get = pool.methodref(STORE, "getRecord", "(I)[B");

    let record2 = "你好".as_bytes();
    let mut code = Vec::new();
    code.extend(op3(LDC_W, name)); // 0-2
    code.push(ICONST_1); // 3 create=true
    code.extend(op3(INVOKESTATIC, open)); // 4-6
    code.extend(vec![ASTORE_1]); // 7 ASTORE_1
    code.push(ICONST_0); // 8
    code.extend(store_int(5)); // 9 ISTORE_5（id 累加器）

    // 记录1 {0x11,0x22} → id 1
    code.extend(bytes_into_local(&[0x11, 0x22], 2));
    code.extend(load_int(5)); // ILOAD_5
    code.extend(load_ref(1));
    code.extend(load_ref(2));
    code.push(ICONST_0);
    code.push(ICONST_2);
    code.extend(op3(INVOKEVIRTUAL, add));
    code.push(IADD);
    code.extend(store_int(5)); // ISTORE_5

    // 记录2 "你好"（6 字节 UTF-8）→ id 2
    code.extend(bytes_into_local(record2, 2));
    code.extend(load_int(5));
    code.extend(load_ref(1));
    code.extend(load_ref(2));
    code.push(ICONST_0);
    code.extend(push_int(record2.len() as i32));
    code.extend(op3(INVOKEVIRTUAL, add));
    code.push(IADD);
    code.extend(store_int(5));

    // 记录3 {0x7F} → id 3
    code.extend(bytes_into_local(&[0x7F], 2));
    code.extend(load_int(5));
    code.extend(load_ref(1));
    code.extend(load_ref(2));
    code.push(ICONST_0);
    code.push(ICONST_1);
    code.extend(op3(INVOKEVIRTUAL, add));
    code.push(IADD);
    code.extend(store_int(5));

    // getRecord(2) → checksum(local3, i=4)
    code.extend(load_ref(1));
    code.push(ICONST_2);
    code.extend(op3(INVOKEVIRTUAL, get));
    code.extend(store_int(2)); // getRecord 结果覆盖记录数组槽
    code.extend(checksum_code(2, 3, 4));

    // 返回 id_sum×1000 + checksum
    code.extend(load_int(5));
    code.extend(push_int(1000));
    code.push(IMUL);
    code.extend(load_int(3));
    code.push(IADD);
    code.push(IRETURN);

    let id_sum = 1 + 2 + 3;
    let checksum: i32 = record2.iter().map(|&b| b as i32).sum();
    let want = id_sum * 1000 + checksum;
    assert_eq!(run_int(&mut pool, "()I", 12, 8, code), want);
}

/// setRecord 覆写生效；deleteRecord 后 getRecord → InvalidRecordIDException
/// （Java catch 断言）。返回 = 覆写后校验和 + 100000。
#[test]
fn set_overwrites_and_deleted_id_is_invalid() {
    let mut pool = Pool::new();
    let name = pool.string("rms_t2");
    let open = pool.methodref(STORE, "openRecordStore", "(Ljava/lang/String;Z)Ljavax/microedition/rms/RecordStore;");
    let add = pool.methodref(STORE, "addRecord", "([BII)I");
    let set = pool.methodref(STORE, "setRecord", "(I[BII)V");
    let get = pool.methodref(STORE, "getRecord", "(I)[B");
    let delete = pool.methodref(STORE, "deleteRecord", "(I)V");

    let big = pool.integer(100_000); // SIPUSH 量程外，走常量池
    let mut code = Vec::new();
    code.extend(op3(LDC_W, name));
    code.push(ICONST_1);
    code.extend(op3(INVOKESTATIC, open));
    code.extend(vec![ASTORE_1]); // ASTORE_1 store

    // addRecord({0x01}) → id 1
    code.extend(bytes_into_local(&[0x01], 2));
    code.extend(load_ref(1));
    code.extend(load_ref(2));
    code.push(ICONST_0);
    code.push(ICONST_1);
    code.extend(op3(INVOKEVIRTUAL, add));
    code.push(POP);

    // setRecord(1, {0x42})
    code.extend(bytes_into_local(&[0x42], 2));
    code.extend(load_ref(1));
    code.push(ICONST_1);
    code.extend(load_ref(2));
    code.push(ICONST_0);
    code.push(ICONST_1);
    code.extend(op3(INVOKEVIRTUAL, set));

    // checksum(getRecord(1)) → local3
    code.extend(load_ref(1));
    code.push(ICONST_1);
    code.extend(op3(INVOKEVIRTUAL, get));
    code.extend(store_int(2)); // getRecord 结果覆盖记录数组槽
    code.extend(checksum_code(2, 3, 4));

    // deleteRecord(1)
    code.extend(load_ref(1));
    code.push(ICONST_1);
    code.extend(op3(INVOKEVIRTUAL, delete));

    // try { getRecord(1) } catch InvalidRecordIDException → checksum+100000
    let try_start = code.len() as u16;
    code.extend(load_ref(1));
    code.push(ICONST_1);
    code.extend(op3(INVOKEVIRTUAL, get));
    code.push(POP);
    let try_end = code.len() as u16;
    code.extend(vec![GOTO, 0, 0]); // 跳过 handler → 正常返回（占位回填）
    let goto_pc = (code.len() - 3) as u16;
    let handler_pc = code.len() as u16;
    let mut h: Vec<u8> = Vec::new();
    h.extend(load_int(3));
    h.extend(op3(LDC_W, big));
    h.push(IADD);
    h.push(IRETURN);
    code.extend(h);
    let normal_pc = code.len() as u16;
    patch(&mut code, goto_pc, normal_pc);
    // 正常路径（不该走到，走到则期望不匹配即失败）：checksum+7。
    code.extend(load_int(3));
    code.extend(push_int(7));
    code.push(IADD);
    code.push(IRETURN);

    let bytes = build_class(
        &mut pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_catch(
            "()I",
            12,
            8,
            code,
            vec![handler(
                try_start,
                try_end,
                handler_pc,
                Some("javax/microedition/rms/InvalidRecordIDException"),
            )],
        )],
    );
    let mut vm = Vm::new(vec![("T", bytes)], MockHostContext::new(240, 320));
    // 覆写后记录 = {0x42} → 66；异常路径 +100000。
    assert_eq!(expect_int(vm.exec("()I").unwrap()), 66 + 100000);
}

/// 元数据：numRecords×1000000 + nextRecordID×10000 + version×100 +
/// 非负标志（LCMP+1 编码：lm<0→0 / ==0→1 / >0→2）。两次 add 后
/// num/next/version = 2/3/2；MockHostContext 时钟恒 0 → lastModified
/// 更新为 0 → 标志 1 → 2030201。
#[test]
fn stats_version_and_last_modified() {
    let mut pool = Pool::new();
    let name = pool.string("rms_t3");
    let open = pool.methodref(STORE, "openRecordStore", "(Ljava/lang/String;Z)Ljavax/microedition/rms/RecordStore;");
    let add = pool.methodref(STORE, "addRecord", "([BII)I");
    let num = pool.methodref(STORE, "getNumRecords", "()I");
    let next = pool.methodref(STORE, "getNextRecordID", "()I");
    let version = pool.methodref(STORE, "getVersion", "()I");
    let last = pool.methodref(STORE, "getLastModified", "()J");

    let million = pool.integer(1_000_000); // SIPUSH 量程外，走常量池
    let mut code = Vec::new();
    code.extend(op3(LDC_W, name));
    code.push(ICONST_1);
    code.extend(op3(INVOKESTATIC, open));
    code.extend(vec![ASTORE_1]);
    for payload in [&[0x09u8][..], &[0x08u8, 0x07u8]] {
        code.extend(bytes_into_local(payload, 2));
        code.extend(load_ref(1));
        code.extend(load_ref(2));
        code.push(ICONST_0);
        code.extend(push_int(payload.len() as i32));
        code.extend(op3(INVOKEVIRTUAL, add));
        code.push(POP);
    }
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, num));
    code.extend(store_int(2)); // 2 num
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, next));
    code.extend(store_int(3)); // 3 next
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, version));
    code.extend(store_int(4)); // 4 version
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, last));
    code.push(LCONST_0);
    code.push(LCMP); // lastModified 与 0 比较：-1/0/1
    code.push(ICONST_1);
    code.push(IADD); // 编码非负性：lm<0→0，lm==0→1，lm>0→2
    code.extend(store_int(5)); // 5 flag
    code.extend(load_int(2));
    code.extend(op3(LDC_W, million));
    code.push(IMUL);
    code.extend(load_int(3));
    code.extend(push_int(10_000));
    code.push(IMUL);
    code.push(IADD);
    code.extend(load_int(4));
    code.extend(push_int(100));
    code.push(IMUL);
    code.push(IADD);
    code.extend(load_int(5));
    code.push(IADD);
    code.push(IRETURN);

    assert_eq!(run_int(&mut pool, "()I", 12, 8, code), 2_030_201);
}

/// 跨"进程重启"持久：VM A 写入 → 同一 MockHostContext 重建 VM B →
/// open(false)+getRecord 数据在（host 通道持久性）。
#[test]
fn data_survives_vm_restart_via_host_channel() {
    // --- VM A：open(create)+addRecord({0x5A})，返回新 id。
    let mut pool = Pool::new();
    let name = pool.string("rms_keep");
    let open = pool.methodref(STORE, "openRecordStore", "(Ljava/lang/String;Z)Ljavax/microedition/rms/RecordStore;");
    let add = pool.methodref(STORE, "addRecord", "([BII)I");
    let mut code_a = Vec::new();
    code_a.extend(op3(LDC_W, name));
    code_a.push(ICONST_1);
    code_a.extend(op3(INVOKESTATIC, open));
    code_a.extend(vec![ASTORE_1]);
    code_a.extend(bytes_into_local(&[0x5A], 2));
    code_a.extend(load_ref(1));
    code_a.extend(load_ref(2));
    code_a.push(ICONST_0);
    code_a.push(ICONST_1);
    code_a.extend(op3(INVOKEVIRTUAL, add));
    code_a.push(IRETURN);
    let class_a = build_class(
        &mut pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_code("()I", 12, 8, code_a)],
    );
    let mut vm_a = Vm::new(vec![("T", class_a)], MockHostContext::new(240, 320));
    assert_eq!(expect_int(vm_a.exec("()I").unwrap()), 1);
    let host = vm_a.into_host();

    // --- VM B：同一 host 重建 → open(create=false)+getRecord(1) 校验和。
    let mut pool = Pool::new();
    let name = pool.string("rms_keep");
    let open = pool.methodref(STORE, "openRecordStore", "(Ljava/lang/String;Z)Ljavax/microedition/rms/RecordStore;");
    let get = pool.methodref(STORE, "getRecord", "(I)[B");
    let mut code_b = Vec::new();
    code_b.extend(op3(LDC_W, name));
    code_b.push(ICONST_0); // create=false：必须命中已存店
    code_b.extend(op3(INVOKESTATIC, open));
    code_b.extend(vec![ASTORE_1]);
    code_b.extend(load_ref(1));
    code_b.push(ICONST_1);
    code_b.extend(op3(INVOKEVIRTUAL, get));
    code_b.extend(store_int(2));
    code_b.extend(checksum_code(2, 3, 4));
    code_b.extend(load_int(3));
    code_b.push(IRETURN);
    let class_b = build_class(
        &mut pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_code("()I", 12, 8, code_b)],
    );
    let mut vm_b = Vm::new(vec![("T", class_b)], host);
    assert_eq!(expect_int(vm_b.exec("()I").unwrap()), 0x5A);
}

/// open 不存在不创建 → NotFound；listRecordStores 计数；删除后 open →
/// NotFound。返回 = 1 + 店数×10 + 100。
#[test]
fn not_found_list_and_delete_store() {
    let mut pool = Pool::new();
    let no_such = pool.string("rms_no_such");
    let name5 = pool.string("rms_t5");
    let open = pool.methodref(STORE, "openRecordStore", "(Ljava/lang/String;Z)Ljavax/microedition/rms/RecordStore;");
    let add = pool.methodref(STORE, "addRecord", "([BII)I");
    let list = pool.methodref(STORE, "listRecordStores", "()[Ljava/lang/String;");
    let delete_store = pool.methodref(STORE, "deleteRecordStore", "(Ljava/lang/String;)V");

    let mut code = Vec::new();
    code.push(ICONST_0);
    code.extend(store_int(6)); // 6 = r（store 在 1、数组在 2 避开）

    // try1: open("rms_no_such", false) → catch NotFound → r += 1
    let t1s = code.len() as u16;
    code.extend(op3(LDC_W, no_such));
    code.push(ICONST_0);
    code.extend(op3(INVOKESTATIC, open));
    code.push(POP);
    let t1e = code.len() as u16;
    let mut h1: Vec<u8> = Vec::new();
    h1.extend(load_int(6));
    h1.push(ICONST_1);
    h1.push(IADD);
    h1.extend(store_int(6));
    code.extend(vec![GOTO, 0, 0]);
    let g1 = (code.len() - 3) as u16;
    let h1_pc = code.len() as u16;
    code.extend(h1);
    patch_to_end(&mut code, g1);

    // 建店 rms_t5 + 一条记录。
    code.extend(op3(LDC_W, name5));
    code.push(ICONST_1);
    code.extend(op3(INVOKESTATIC, open));
    code.extend(vec![ASTORE_1]);
    code.extend(bytes_into_local(&[0x01], 2));
    code.extend(load_ref(1));
    code.extend(load_ref(2));
    code.push(ICONST_0);
    code.push(ICONST_1);
    code.extend(op3(INVOKEVIRTUAL, add));
    code.push(POP);

    // r += listRecordStores().length × 10（本夹具此前仅此一店）。
    code.extend(op3(INVOKESTATIC, list));
    code.push(ARRAYLENGTH);
    code.extend(push_int(10));
    code.push(IMUL);
    code.extend(load_int(6));
    code.push(IADD);
    code.extend(store_int(6));

    // deleteRecordStore("rms_t5")。
    code.extend(op3(LDC_W, name5));
    code.extend(op3(INVOKESTATIC, delete_store));

    // try2: open("rms_t5", false) → catch NotFound → r += 100
    let t2s = code.len() as u16;
    code.extend(op3(LDC_W, name5));
    code.push(ICONST_0);
    code.extend(op3(INVOKESTATIC, open));
    code.push(POP);
    let t2e = code.len() as u16;
    let mut h2: Vec<u8> = Vec::new();
    h2.extend(load_int(6));
    h2.extend(push_int(100));
    h2.push(IADD);
    h2.extend(store_int(6));
    code.extend(vec![GOTO, 0, 0]);
    let g2 = (code.len() - 3) as u16;
    let h2_pc = code.len() as u16;
    code.extend(h2);
    patch_to_end(&mut code, g2);

    code.extend(load_int(6));
    code.push(IRETURN);

    let bytes = build_class(
        &mut pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_catch(
            "()I",
            12,
            10,
            code,
            vec![
                handler(t1s, t1e, h1_pc, Some("javax/microedition/rms/RecordStoreNotFoundException")),
                handler(t2s, t2e, h2_pc, Some("javax/microedition/rms/RecordStoreNotFoundException")),
            ],
        )],
    );
    let mut vm = Vm::new(vec![("T", bytes)], MockHostContext::new(240, 320));
    assert_eq!(expect_int(vm.exec("()I").unwrap()), 1 + 10 + 100);
}

/// 枚举：无 filter 遍历 id 集（1+2+3）+ reset 后 nextRecord 首条往返 +
/// 耗尽一步后 hasPreviousElement。返回 = id 和×1000 + 首条校验和×10 +
/// hasPrevious。
#[test]
fn enumeration_walks_ids_and_records() {
    let mut pool = Pool::new();
    let name = pool.string("rms_t6");
    let open = pool.methodref(STORE, "openRecordStore", "(Ljava/lang/String;Z)Ljavax/microedition/rms/RecordStore;");
    let add = pool.methodref(STORE, "addRecord", "([BII)I");
    let enumerate = pool.methodref(STORE, "enumerateRecords", "(Ljavax/microedition/rms/RecordFilter;Ljavax/microedition/rms/RecordComparator;Z)Ljavax/microedition/rms/RecordEnumeration;");
    let has_next = pool.methodref("javax/microedition/rms/RecordEnum", "hasNextElement", "()Z");
    let next_id = pool.methodref("javax/microedition/rms/RecordEnum", "nextRecordId", "()I");
    let next_record = pool.methodref("javax/microedition/rms/RecordEnum", "nextRecord", "()[B");
    let reset = pool.methodref("javax/microedition/rms/RecordEnum", "reset", "()V");
    let has_prev = pool.methodref("javax/microedition/rms/RecordEnum", "hasPreviousElement", "()Z");

    let mut code = Vec::new();
    code.extend(op3(LDC_W, name));
    code.push(ICONST_1);
    code.extend(op3(INVOKESTATIC, open));
    code.extend(vec![ASTORE_1]); // 1 store
    for payload in [&[0x0Au8][..], &[0x14u8][..], &[0x1Eu8][..]] {
        code.extend(bytes_into_local(payload, 2));
        code.extend(load_ref(1));
        code.extend(load_ref(2));
        code.push(ICONST_0);
        code.extend(push_int(payload.len() as i32));
        code.extend(op3(INVOKEVIRTUAL, add));
        code.push(POP);
    }

    // enumerateRecords(null, null, false) → local2（RecordEnum）。
    code.extend(load_ref(1)); // INVOKEVIRTUAL 需先压 receiver
    code.push(ACONST_NULL);
    code.push(ACONST_NULL);
    code.push(ICONST_0);
    code.extend(op3(INVOKEVIRTUAL, enumerate));
    code.extend(store_int(2)); // 引用存槽

    // 循环累加 nextRecordId → local5。
    code.push(ICONST_0);
    code.extend(store_int(5));
    let loop_start = code.len() as u16;
    code.extend(load_ref(2));
    code.extend(op3(INVOKEVIRTUAL, has_next));
    let if_pc = code.len() as u16;
    code.extend(vec![IFEQ, 0, 0]);
    code.extend(load_int(5));
    code.extend(load_ref(2));
    code.extend(op3(INVOKEVIRTUAL, next_id));
    code.push(IADD);
    code.extend(store_int(5));
    let goto_pc = code.len() as u16;
    code.extend(vec![GOTO, 0, 0]);
    patch_to_end(&mut code, if_pc);
    patch(&mut code, goto_pc, loop_start);

    // reset → nextRecord() 首条 checksum（=10）→ local3（i 用 4）。
    code.extend(load_ref(2));
    code.extend(op3(INVOKEVIRTUAL, reset));
    code.extend(load_ref(2));
    code.extend(op3(INVOKEVIRTUAL, next_record));
    code.extend(store_int(3));
    code.extend(checksum_code(3, 6, 4)); // sum 在 6，数组 3，i 在 4

    // hasPreviousElement()（游标已前进 1）→ 栈顶。
    code.extend(load_ref(2));
    code.extend(op3(INVOKEVIRTUAL, has_prev));

    // 返回 id_sum×1000 + checksum×10 + hasPrev。
    code.extend(load_int(5));
    code.extend(push_int(1000));
    code.push(IMUL);
    code.extend(load_int(6));
    code.extend(push_int(10));
    code.push(IMUL);
    code.push(IADD);
    code.push(IADD);
    code.push(IRETURN);

    // id 和=6 → 6000；首条 {0x0A} → 10×10=100；hasPrev=1。
    assert_eq!(run_int(&mut pool, "()I", 12, 10, code), 6101);
}

/// close 后 addRecord / 重复 close → RecordStoreException（catch 断言）。
/// 返回 = 1 + 10。
#[test]
fn closed_store_operations_throw() {
    let mut pool = Pool::new();
    let name = pool.string("rms_t7");
    let open = pool.methodref(STORE, "openRecordStore", "(Ljava/lang/String;Z)Ljavax/microedition/rms/RecordStore;");
    let add = pool.methodref(STORE, "addRecord", "([BII)I");
    let close = pool.methodref(STORE, "closeRecordStore", "()V");

    let mut code = Vec::new();
    code.extend(op3(LDC_W, name));
    code.push(ICONST_1);
    code.extend(op3(INVOKESTATIC, open));
    code.extend(vec![ASTORE_1]);
    code.extend(bytes_into_local(&[0x01], 2));
    code.push(ICONST_0);
    code.extend(store_int(6)); // r
    // 正常 close。
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, close));

    // try1: addRecord after close → catch RSE → r += 1
    let t1s = code.len() as u16;
    code.extend(load_ref(1));
    code.extend(load_ref(2));
    code.push(ICONST_0);
    code.push(ICONST_1);
    code.extend(op3(INVOKEVIRTUAL, add));
    code.push(POP);
    let t1e = code.len() as u16;
    let mut h1: Vec<u8> = Vec::new();
    h1.extend(load_int(6));
    h1.push(ICONST_1);
    h1.push(IADD);
    h1.extend(store_int(6));
    code.extend(vec![GOTO, 0, 0]);
    let g1 = (code.len() - 3) as u16;
    let h1_pc = code.len() as u16;
    code.extend(h1);
    patch_to_end(&mut code, g1);

    // try2: 重复 close → catch RSE → r += 10
    let t2s = code.len() as u16;
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, close));
    let t2e = code.len() as u16;
    let mut h2: Vec<u8> = Vec::new();
    h2.extend(load_int(6));
    h2.extend(push_int(10));
    h2.push(IADD);
    h2.extend(store_int(6));
    code.extend(vec![GOTO, 0, 0]);
    let g2 = (code.len() - 3) as u16;
    let h2_pc = code.len() as u16;
    code.extend(h2);
    patch_to_end(&mut code, g2);

    code.extend(load_int(6));
    code.push(IRETURN);

    let bytes = build_class(
        &mut pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_catch(
            "()I",
            12,
            10,
            code,
            vec![
                handler(t1s, t1e, h1_pc, Some("javax/microedition/rms/RecordStoreException")),
                handler(t2s, t2e, h2_pc, Some("javax/microedition/rms/RecordStoreException")),
            ],
        )],
    );
    let mut vm = Vm::new(vec![("T", bytes)], MockHostContext::new(240, 320));
    assert_eq!(expect_int(vm.exec("()I").unwrap()), 11);
}
