//! CLDC java.lang 语义测试（T014）：bootstrap 注册表真实现的端到端验证。
//!
//! 每个用例手构一个用户类 `T`（字节码见 `common/classbytes.rs`），经
//! `RegistryDispatch` 调用 java/lang 原生实现，断言 JVM 兼容语义：
//! String 哈希公式/越界异常、StringBuffer 扩容、Math.round 边界、
//! System.arraycopy 重叠、包装类解析与 NumberFormatException、
//! Throwable 消息链、Class.forName 反射等。
//!
//! 字节码约定（与 interp.rs 一致）：操作数池索引**先取号再构码**；
//! java/lang 类全部由 bootstrap 注册表解析（优先级高于类提供者）。

#[allow(dead_code)]
mod common;

use common::classbytes::{build_class, provider, HandlerSpec, MethodSpec, Pool};

use skyengine_kvm::interp::natives::RegistryDispatch;
use skyengine_kvm::host::mock::MockHostContext;
use skyengine_kvm::interp::{enter, run, runtime_class_of, Machine, RunOutcome, Unwind};
use skyengine_kvm::runtime::classspace::ClassSpace;
use skyengine_kvm::runtime::heap::Heap;
use skyengine_kvm::runtime::monitor::Monitors;
use skyengine_kvm::runtime::{ObjRef, Slot, ThreadId};
use skyengine_kvm::KvmError;

// ---- 操作码（仅本文件用到的） --------------------------------------------

const ACONST_NULL: u8 = 0x01;
const ICONST_0: u8 = 0x03;
const ICONST_1: u8 = 0x04;
const ICONST_2: u8 = 0x05;
const ICONST_3: u8 = 0x06;
const ICONST_4: u8 = 0x07;
const ICONST_5: u8 = 0x08;
const DCONST_0: u8 = 0x0e;
const DCONST_1: u8 = 0x0f;
const BIPUSH: u8 = 0x10;
const SIPUSH: u8 = 0x11;
const LDC_W: u8 = 0x13;
const LDC2_W: u8 = 0x14;
const ILOAD_2: u8 = 0x1c;
const ALOAD_1: u8 = 0x2b;
const ALOAD_2: u8 = 0x2c;
const ALOAD_3: u8 = 0x2d;
const IALOAD: u8 = 0x2e;
const BALOAD: u8 = 0x33;
const CALOAD: u8 = 0x34;
const ISTORE_2: u8 = 0x3d;
const ASTORE_1: u8 = 0x4c;
const ASTORE_2: u8 = 0x4d;
const ASTORE_3: u8 = 0x4e;
const IASTORE: u8 = 0x4f;
const POP: u8 = 0x57;
const DUP: u8 = 0x59;
const IADD: u8 = 0x60;
const IMUL: u8 = 0x68;
const DDIV: u8 = 0x6f;
const L2I: u8 = 0x88;
const F2D: u8 = 0x8d;
const IF_ACMPEQ: u8 = 0xa5;
const GOTO: u8 = 0xa7;
const IRETURN: u8 = 0xac;
const LRETURN: u8 = 0xad;
const DRETURN: u8 = 0xaf;
const GETSTATIC: u8 = 0xb2;
const INVOKEVIRTUAL: u8 = 0xb6;
const INVOKESPECIAL: u8 = 0xb7;
const INVOKESTATIC: u8 = 0xb8;
const NEW: u8 = 0xbb;
const NEWARRAY: u8 = 0xbc;
const ARRAYLENGTH: u8 = 0xbe;
const IFNULL: u8 = 0xc6;
const IFNONNULL: u8 = 0xc7;
// java.util 段补充
const ICONST_M1: u8 = 0x02;
const LCONST_0: u8 = 0x09;
const LCMP: u8 = 0x94;
const IFEQ: u8 = 0x99;
const IFNE: u8 = 0x9a;
const IF_ICMPNE: u8 = 0xa0;
const FRETURN: u8 = 0xae;
const ANEWARRAY: u8 = 0xbd;
const AALOAD: u8 = 0x32;
// java.io 段补充
const ILOAD: u8 = 0x15;
const ISTORE: u8 = 0x36;
const ALOAD: u8 = 0x19;
const ASTORE: u8 = 0x3a;
const BASTORE: u8 = 0x53;
const ISHL: u8 = 0x78;
const IF_ICMPGE: u8 = 0xa2;
const IINC: u8 = 0x84;
const ARETURN: u8 = 0xb0;

/// newarray 的 atype（byte）。
const T_BYTE: u8 = 8;

/// newarray 的 atype。
const T_CHAR: u8 = 5;
const T_INT: u8 = 10;

/// 主线程标识（与 interp.rs 夹具一致）。
const MAIN: ThreadId = ThreadId(1);

// ---- 夹具 ------------------------------------------------------------------

/// 走真注册表的测试 VM：java/lang 由 bootstrap 表解析，用户类走提供者。
struct Vm {
    cs: ClassSpace,
    heap: Heap,
    monitors: Monitors,
    natives: RegistryDispatch,
    /// HostContext 通道（T019）；语义测试用 Mock 兜底。
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
        // StrObj 载荷的运行时类按"已加载"查找，须预载 String。
        cs.load_class("java/lang/String").expect("load String");
        // 处理器表捕获类预载（展开路径按类名匹配）。
        for name in [
            "java/lang/Exception",
            "java/lang/StringIndexOutOfBoundsException",
            "java/lang/ArrayStoreException",
            "java/lang/ArrayIndexOutOfBoundsException",
            "java/lang/NumberFormatException",
            "java/lang/ClassNotFoundException",
            "java/lang/IllegalArgumentException",
            "java/util/NoSuchElementException",
            "java/util/EmptyStackException",
        ] {
            cs.load_class(name).unwrap_or_else(|error| panic!("load {name}: {error}"));
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

/// 静态 main 方法（带字节码）。
fn m_code(desc: &'static str, stack: u16, locals: u16, code: Vec<u8>) -> MethodSpec {
    MethodSpec {
        access: 0x0008,
        name: "main",
        desc,
        code: Some((stack, locals, code)),
        handlers: Vec::new(),
    }
}

/// 静态 main 方法（带字节码 + 异常表）。
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

/// 单常量池操作数指令（ldc/ldc2_w/invoke*/new/getstatic 均为 3 字节）。
fn op3(op: u8, index: u16) -> [u8; 3] {
    let [hi, lo] = index.to_be_bytes();
    [op, hi, lo]
}

/// 单操作数分支（goto/if*，3 字节，偏移相对指令首字节）。
fn branch(op: u8, offset: i16) -> [u8; 3] {
    let [hi, lo] = offset.to_be_bytes();
    [op, hi, lo]
}

fn expect_int(outcome: RunOutcome) -> i32 {
    match outcome {
        RunOutcome::Returned(Some(Slot::Int(value))) => value,
        other => panic!("expected int return, got {other:?}"),
    }
}

fn expect_long(outcome: RunOutcome) -> i64 {
    match outcome {
        RunOutcome::Returned(Some(Slot::Long(value))) => value,
        other => panic!("expected long return, got {other:?}"),
    }
}

fn expect_double(outcome: RunOutcome) -> f64 {
    match outcome {
        RunOutcome::Returned(Some(Slot::Double(value))) => value,
        other => panic!("expected double return, got {other:?}"),
    }
}

fn expect_float(outcome: RunOutcome) -> f32 {
    match outcome {
        RunOutcome::Returned(Some(Slot::Float(value))) => value,
        other => panic!("expected float return, got {other:?}"),
    }
}

fn str_text(heap: &Heap, reference: ObjRef) -> String {
    String::from_utf16(&heap.str_obj(reference).unwrap().chars).unwrap()
}

/// 以单方法类 `T` 建夹具并执行 main（断言 int 返回值）。
///
/// 池必须由调用方构造（构码取号与 build_class 序列化须同一实例）。
fn run_main(pool: &mut Pool, desc: &'static str, stack: u16, locals: u16, code: Vec<u8>) -> i32 {
    let bytes = build_class(
        pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_code(desc, stack, locals, code)],
    );
    let mut vm = Vm::new(vec![("T", bytes)]);
    expect_int(vm.exec(desc).expect("run main"))
}

/// 以单方法类 `T` 建夹具并执行 main（返回完整结果，用于 long/double/引用返回）。
fn run_main_outcome(
    pool: &mut Pool,
    desc: &'static str,
    stack: u16,
    locals: u16,
    code: Vec<u8>,
) -> RunOutcome {
    let bytes = build_class(
        pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_code(desc, stack, locals, code)],
    );
    let mut vm = Vm::new(vec![("T", bytes)]);
    vm.exec(desc).expect("run main")
}

/// Java String.hashCode 规范公式（int 回绕），用于独立交叉验证。
fn java_hash(text: &str) -> i32 {
    let mut hash: i32 = 0;
    for unit in text.encode_utf16() {
        hash = hash.wrapping_mul(31).wrapping_add(i32::from(unit));
    }
    hash
}

// ---- String ----------------------------------------------------------------

#[test]
fn string_length_and_char_at() {
    // "hello".length() + "hello".charAt(1) == 5 + 101
    let mut pool = Pool::new();
    let hello = pool.string("hello");
    let length = pool.methodref("java/lang/String", "length", "()I");
    let char_at = pool.methodref("java/lang/String", "charAt", "(I)C");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, hello));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, length));
    code.extend_from_slice(&op3(LDC_W, hello));
    code.push(ICONST_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, char_at));
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 4, 1, code), 106);
}

#[test]
fn string_hash_code_matches_java_formula() {
    let mut pool = Pool::new();
    let hello = pool.string("hello");
    let hash = pool.methodref("java/lang/String", "hashCode", "()I");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, hello));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, hash));
    code.push(IRETURN);
    // 手算交叉验证："hello" → 99162322（JVM 已知值）。
    assert_eq!(java_hash("hello"), 99162322);
    assert_eq!(run_main(&mut pool, "()I", 2, 1, code), 99162322);
}

#[test]
fn string_hash_code_wraps_on_long_text() {
    let text = "The quick brown fox jumps over the lazy dog";
    let mut pool = Pool::new();
    let constant = pool.string(text);
    let hash = pool.methodref("java/lang/String", "hashCode", "()I");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, constant));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, hash));
    code.push(IRETURN);
    // 长文本中间值溢出 int，必须回绕（与规范公式一致而非饱和）。
    assert_eq!(run_main(&mut pool, "()I", 2, 1, code), java_hash(text));
}

#[test]
fn string_equals_and_ignore_case() {
    // equals 大小写敏感 → 0；equalsIgnoreCase → 1。
    let mut pool = Pool::new();
    let upper = pool.string("Hello");
    let lower = pool.string("hello");
    let equals = pool.methodref("java/lang/String", "equals", "(Ljava/lang/Object;)Z");
    let ignore = pool.methodref("java/lang/String", "equalsIgnoreCase", "(Ljava/lang/String;)Z");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, upper));
    code.extend_from_slice(&op3(LDC_W, lower));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.push(BIPUSH);
    code.push(10);
    code.push(IMUL);
    code.extend_from_slice(&op3(LDC_W, upper));
    code.extend_from_slice(&op3(LDC_W, lower));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, ignore));
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 1, code), 1);
}

#[test]
fn string_substring_concat_chain() {
    // "hello".substring(1, 3).concat("lo").equals("ello") → 1
    let mut pool = Pool::new();
    let hello = pool.string("hello");
    let lo = pool.string("lo");
    let expect = pool.string("ello");
    let substring = pool.methodref("java/lang/String", "substring", "(II)Ljava/lang/String;");
    let concat = pool.methodref("java/lang/String", "concat", "(Ljava/lang/String;)Ljava/lang/String;");
    let equals = pool.methodref("java/lang/String", "equals", "(Ljava/lang/Object;)Z");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, hello));
    code.push(ICONST_1);
    code.push(ICONST_3);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, substring));
    code.extend_from_slice(&op3(LDC_W, lo));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, concat));
    code.extend_from_slice(&op3(LDC_W, expect));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 1, code), 1);
}

// ---- java.util（T015） ------------------------------------------------------

/// 测试内独立重写的 Java Random 48 位 LCG（交叉验证，不复用被测代码）。
struct RefLcg(i64);

impl RefLcg {
    fn new(seed: i64) -> Self {
        Self((seed ^ 0x5DEECE66D) & ((1i64 << 48) - 1))
    }
    fn next(&mut self, bits: u32) -> i64 {
        self.0 = self.0.wrapping_mul(0x5DEECE66D).wrapping_add(0xB) & ((1i64 << 48) - 1);
        self.0 >> (48 - bits)
    }
    fn next_int(&mut self) -> i32 {
        self.next(32) as i32
    }
    fn next_int_bound(&mut self, bound: i32) -> i32 {
        (self.next(31) % i64::from(bound)) as i32
    }
    fn next_long(&mut self) -> i64 {
        let hi = self.next(32) as i32 as i64;
        let lo = self.next(32) as i32 as i64;
        (hi << 32).wrapping_add(lo)
    }
    fn next_double(&mut self) -> f64 {
        ((self.next(26) << 27) + self.next(27)) as f64 / (1u64 << 53) as f64
    }
    fn next_float(&mut self) -> f32 {
        self.next(24) as f32 / (1u32 << 24) as f32
    }
}

/// 回填前向分支：sites 为各占位分支（branch() 三字节）操作码的 pc，
/// 目标统一取回填时的 code 末尾（miss 块首）。
fn patch_miss_branches(code: &mut [u8], sites: &[usize]) {
    let target = code.len() as i16;
    for site in sites {
        let rel = target - *site as i16;
        code[*site + 1..*site + 3].copy_from_slice(&rel.to_be_bytes());
    }
}

/// 压 int（SIPUSH 通路，避开 LDC_W 整数池差异）。
fn push_i32(code: &mut Vec<u8>, value: i32) {
    match value {
        -1 => code.push(ICONST_M1),
        0 => code.push(ICONST_0),
        1 => code.push(ICONST_1),
        2 => code.push(ICONST_2),
        3 => code.push(ICONST_3),
        4 => code.push(ICONST_4),
        5 => code.push(ICONST_5),
        _ => {
            code.push(SIPUSH);
            code.extend_from_slice(&(value as i16).to_be_bytes());
        }
    }
}

#[test]
fn random_next_int_matches_reference_lcg() {
    // new Random(42).nextInt() 必须与 Java 48 位 LCG 参考实现逐位一致。
    let mut pool = Pool::new();
    let new_random = pool.class("java/util/Random");
    let init = pool.methodref("java/util/Random", "<init>", "(J)V");
    let next_int = pool.methodref("java/util/Random", "nextInt", "()I");
    let seed = pool.long(42);
    let mut code = Vec::new();
    code.extend_from_slice(&op3(NEW, new_random));
    code.push(DUP);
    code.extend_from_slice(&op3(LDC2_W, seed));
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, next_int));
    code.push(IRETURN);
    let mut reference = RefLcg::new(42);
    assert_eq!(run_main(&mut pool, "()I", 4, 1, code), reference.next_int());
}

#[test]
fn random_next_int_bound_matches_reference_lcg() {
    // nextInt(100) = next(31) % 100，落在 [0,100)。
    let mut pool = Pool::new();
    let new_random = pool.class("java/util/Random");
    let init = pool.methodref("java/util/Random", "<init>", "(J)V");
    let next_bound = pool.methodref("java/util/Random", "nextInt", "(I)I");
    let seed = pool.long(42);
    let mut code = Vec::new();
    code.extend_from_slice(&op3(NEW, new_random));
    code.push(DUP);
    code.extend_from_slice(&op3(LDC2_W, seed));
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code.push(BIPUSH);
    code.push(100);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, next_bound));
    code.push(IRETURN);
    let mut reference = RefLcg::new(42);
    let expected = reference.next_int_bound(100);
    assert!((0..100).contains(&expected));
    assert_eq!(run_main(&mut pool, "()I", 5, 1, code), expected);
}

#[test]
fn random_next_long_matches_reference_lcg() {
    // nextLong：高 32 位按 int 符号扩展后移位相加。
    let mut pool = Pool::new();
    let new_random = pool.class("java/util/Random");
    let init = pool.methodref("java/util/Random", "<init>", "(J)V");
    let next_long = pool.methodref("java/util/Random", "nextLong", "()J");
    let seed = pool.long(42);
    let mut code = Vec::new();
    code.extend_from_slice(&op3(NEW, new_random));
    code.push(DUP);
    code.extend_from_slice(&op3(LDC2_W, seed));
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, next_long));
    code.push(LRETURN);
    let mut reference = RefLcg::new(42);
    let bytes = build_class(&mut pool, "T", Some("java/lang/Object"), &[], &[], &[m_code("()J", 4, 1, code)]);
    let mut vm = Vm::new(vec![("T", bytes)]);
    assert_eq!(expect_long(vm.exec("()J").unwrap()), reference.next_long());
}

#[test]
fn random_next_double_matches_reference_lcg() {
    let mut pool = Pool::new();
    let new_random = pool.class("java/util/Random");
    let init = pool.methodref("java/util/Random", "<init>", "(J)V");
    let next_double = pool.methodref("java/util/Random", "nextDouble", "()D");
    let seed = pool.long(42);
    let mut code = Vec::new();
    code.extend_from_slice(&op3(NEW, new_random));
    code.push(DUP);
    code.extend_from_slice(&op3(LDC2_W, seed));
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, next_double));
    code.push(DRETURN);
    let mut reference = RefLcg::new(42);
    let expected = reference.next_double();
    assert!((0.0..1.0).contains(&expected));
    assert_eq!(expect_double(run_main_outcome(&mut pool, "()D", 4, 1, code)), expected);
}

#[test]
fn random_next_float_matches_reference_lcg() {
    let mut pool = Pool::new();
    let new_random = pool.class("java/util/Random");
    let init = pool.methodref("java/util/Random", "<init>", "(J)V");
    let next_float = pool.methodref("java/util/Random", "nextFloat", "()F");
    let seed = pool.long(42);
    let mut code = Vec::new();
    code.extend_from_slice(&op3(NEW, new_random));
    code.push(DUP);
    code.extend_from_slice(&op3(LDC2_W, seed));
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, next_float));
    code.push(FRETURN);
    let mut reference = RefLcg::new(42);
    let expected = reference.next_float();
    assert!((0.0..1.0).contains(&expected));
    assert_eq!(expect_float(run_main_outcome(&mut pool, "()F", 4, 1, code)), expected);
}

#[test]
fn random_non_positive_bound_throws_illegal_argument() {
    // nextInt(0) → IllegalArgumentException，捕获返回 1。
    let mut pool = Pool::new();
    let new_random = pool.class("java/util/Random");
    let init = pool.methodref("java/util/Random", "<init>", "(J)V");
    let next_bound = pool.methodref("java/util/Random", "nextInt", "(I)I");
    let seed = pool.long(42);
    let mut code = Vec::new();
    code.extend_from_slice(&op3(NEW, new_random)); // 0-2
    code.push(DUP); // 3
    code.extend_from_slice(&op3(LDC2_W, seed)); // 4-6
    code.extend_from_slice(&op3(INVOKESPECIAL, init)); // 7-9
    code.push(ICONST_0); // 10
    code.extend_from_slice(&op3(INVOKEVIRTUAL, next_bound)); // 11-13
    code.push(POP); // 14
    code.push(ICONST_0); // 15
    code.extend_from_slice(&branch(GOTO, 4)); // 16-18 → 20
    code.push(ICONST_1); // 19
    code.push(IRETURN); // 20
    let bytes = build_class(
        &mut pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_catch(
            "()I",
            4,
            1,
            code,
            vec![handler(0, 14, 19, Some("java/lang/IllegalArgumentException"))],
        )],
    );
    let mut vm = Vm::new(vec![("T", bytes)]);
    assert_eq!(expect_int(vm.exec("()I").unwrap()), 1);
}

#[test]
fn date_get_set_time_roundtrip() {
    // new Date(1234).setTime(999).getTime() == 999。
    let mut pool = Pool::new();
    let new_date = pool.class("java/util/Date");
    let init = pool.methodref("java/util/Date", "<init>", "(J)V");
    let set_time = pool.methodref("java/util/Date", "setTime", "(J)V");
    let get_time = pool.methodref("java/util/Date", "getTime", "()J");
    let initial = pool.long(1234);
    let revised = pool.long(999);
    let mut code = Vec::new();
    code.extend_from_slice(&op3(NEW, new_date));
    code.push(DUP);
    code.extend_from_slice(&op3(LDC2_W, initial));
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code.push(DUP); // 保留接收者供 getTime
    code.extend_from_slice(&op3(LDC2_W, revised));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, set_time));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, get_time));
    code.push(LRETURN);
    assert_eq!(expect_long(run_main_outcome(&mut pool, "()J", 5, 1, code)), 999);
}

#[test]
fn date_after_before_compare_to() {
    // d1=1000, d2=2000：before/after/compareTo 全套。
    let mut pool = Pool::new();
    let new_date = pool.class("java/util/Date");
    let init = pool.methodref("java/util/Date", "<init>", "(J)V");
    let before = pool.methodref("java/util/Date", "before", "(Ljava/util/Date;)Z");
    let after = pool.methodref("java/util/Date", "after", "(Ljava/util/Date;)Z");
    let compare = pool.methodref("java/util/Date", "compareTo", "(Ljava/util/Date;)I");
    let t1 = pool.long(1000);
    let t2 = pool.long(2000);
    let mut code = Vec::new();
    let mut miss = Vec::new();
    // d1 → local1，d2 → local2
    code.extend_from_slice(&op3(NEW, new_date));
    code.push(DUP);
    code.extend_from_slice(&op3(LDC2_W, t1));
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code.push(ASTORE_1);
    code.extend_from_slice(&op3(NEW, new_date));
    code.push(DUP);
    code.extend_from_slice(&op3(LDC2_W, t2));
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code.push(ASTORE_2);
    // d1.before(d2)
    code.push(ALOAD_1);
    code.push(ALOAD_2);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, before));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    // d2.after(d1)
    code.push(ALOAD_2);
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, after));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    // d1.compareTo(d2) == -1
    code.push(ALOAD_1);
    code.push(ALOAD_2);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, compare));
    code.push(ICONST_M1);
    code.extend_from_slice(&branch(IF_ICMPNE, 0));
    miss.push(code.len() - 3);
    // d2.compareTo(d1) == 1
    code.push(ALOAD_2);
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, compare));
    code.push(ICONST_1);
    code.extend_from_slice(&branch(IF_ICMPNE, 0));
    miss.push(code.len() - 3);
    // d1.compareTo(d1) == 0
    code.push(ALOAD_1);
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, compare));
    code.extend_from_slice(&branch(IFNE, 0));
    miss.push(code.len() - 3);
    code.push(ICONST_1);
    code.push(IRETURN);
    patch_miss_branches(&mut code, &miss);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 6, 3, code), 1);
}

#[test]
fn date_to_string_epoch() {
    // new Date(0).toString() == "Thu Jan 01 00:00:00 GMT 1970"。
    let mut pool = Pool::new();
    let new_date = pool.class("java/util/Date");
    let init = pool.methodref("java/util/Date", "<init>", "(J)V");
    let to_string = pool.methodref("java/util/Date", "toString", "()Ljava/lang/String;");
    let expect = pool.string("Thu Jan 01 00:00:00 GMT 1970");
    let equals = pool.methodref("java/lang/String", "equals", "(Ljava/lang/Object;)Z");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(NEW, new_date));
    code.push(DUP);
    code.push(LCONST_0);
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, to_string));
    code.extend_from_slice(&op3(LDC_W, expect));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 1, code), 1);
}

#[test]
fn date_to_string_leap_day_2000() {
    // 2000-02-29 00:00:00 GMT（周二）：951782400000ms。
    let mut pool = Pool::new();
    let new_date = pool.class("java/util/Date");
    let init = pool.methodref("java/util/Date", "<init>", "(J)V");
    let to_string = pool.methodref("java/util/Date", "toString", "()Ljava/lang/String;");
    let millis = pool.long(951_782_400_000);
    let expect = pool.string("Tue Feb 29 00:00:00 GMT 2000");
    let equals = pool.methodref("java/lang/String", "equals", "(Ljava/lang/Object;)Z");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(NEW, new_date));
    code.push(DUP);
    code.extend_from_slice(&op3(LDC2_W, millis));
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, to_string));
    code.extend_from_slice(&op3(LDC_W, expect));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 1, code), 1);
}

#[test]
fn date_equals_and_hash_code() {
    // 同毫秒 Date equals/hashCode 一致；不同毫秒不等。
    let mut pool = Pool::new();
    let new_date = pool.class("java/util/Date");
    let init = pool.methodref("java/util/Date", "<init>", "(J)V");
    let equals = pool.methodref("java/util/Date", "equals", "(Ljava/lang/Object;)Z");
    let hash_code = pool.methodref("java/util/Date", "hashCode", "()I");
    let t = pool.long(500);
    let other = pool.long(501);
    let mut code = Vec::new();
    let mut miss = Vec::new();
    // local1=d1(500)，local2=d2(500)，local3=d3(501)
    for (store, constant) in [(ASTORE_1, t), (ASTORE_2, t), (ASTORE_3, other)] {
        code.extend_from_slice(&op3(NEW, new_date));
        code.push(DUP);
        code.extend_from_slice(&op3(LDC2_W, constant));
        code.extend_from_slice(&op3(INVOKESPECIAL, init));
        code.push(store);
    }
    // d1.equals(d2)
    code.push(ALOAD_1);
    code.push(ALOAD_2);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    // !d1.equals(d3)
    code.push(ALOAD_1);
    code.push(ALOAD_3);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.extend_from_slice(&branch(IFNE, 0));
    miss.push(code.len() - 3);
    // hashCode == 500（500 ^ (500>>>32) = 500），两实例一致
    for load in [ALOAD_1, ALOAD_2] {
        code.push(load);
        code.extend_from_slice(&op3(INVOKEVIRTUAL, hash_code));
        push_i32(&mut code, 500);
        code.extend_from_slice(&branch(IF_ICMPNE, 0));
        miss.push(code.len() - 3);
    }
    code.push(ICONST_1);
    code.push(IRETURN);
    patch_miss_branches(&mut code, &miss);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 7, 4, code), 1);
}

#[test]
fn calendar_epoch_field_decomposition() {
    // setTimeInMillis(0)：1970-01-01 周四 00:00:00.000。
    let mut pool = Pool::new();
    let get_instance = pool.methodref("java/util/Calendar", "getInstance", "()Ljava/util/Calendar;");
    let set_millis = pool.methodref("java/util/Calendar", "setTimeInMillis", "(J)V");
    let get = pool.methodref("java/util/Calendar", "get", "(I)I");
    let mut code = Vec::new();
    let mut miss = Vec::new();
    code.extend_from_slice(&op3(INVOKESTATIC, get_instance));
    code.push(ASTORE_1);
    code.push(ALOAD_1);
    code.push(LCONST_0);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, set_millis));
    // 逐字段断言：(field, expected)
    for (field, expected) in [(1, 1970), (2, 0), (5, 1), (7, 5), (11, 0), (14, 0)] {
        code.push(ALOAD_1);
        push_i32(&mut code, field);
        code.extend_from_slice(&op3(INVOKEVIRTUAL, get));
        if expected == 0 {
            code.extend_from_slice(&branch(IFNE, 0));
        } else {
            push_i32(&mut code, expected);
            code.extend_from_slice(&branch(IF_ICMPNE, 0));
        }
        miss.push(code.len() - 3);
    }
    code.push(ICONST_1);
    code.push(IRETURN);
    patch_miss_branches(&mut code, &miss);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 2, code), 1);
}

#[test]
fn calendar_year_later_field_decomposition() {
    // +365 天：1971-01-01 周五。
    let mut pool = Pool::new();
    let get_instance = pool.methodref("java/util/Calendar", "getInstance", "()Ljava/util/Calendar;");
    let set_millis = pool.methodref("java/util/Calendar", "setTimeInMillis", "(J)V");
    let get = pool.methodref("java/util/Calendar", "get", "(I)I");
    let year_millis = pool.long(365 * 86_400_000);
    let mut code = Vec::new();
    let mut miss = Vec::new();
    code.extend_from_slice(&op3(INVOKESTATIC, get_instance));
    code.push(ASTORE_1);
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(LDC2_W, year_millis));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, set_millis));
    for (field, expected) in [(1, 1971), (2, 0), (5, 1), (7, 6)] {
        code.push(ALOAD_1);
        push_i32(&mut code, field);
        code.extend_from_slice(&op3(INVOKEVIRTUAL, get));
        if expected == 0 {
            code.extend_from_slice(&branch(IFNE, 0));
        } else {
            push_i32(&mut code, expected);
            code.extend_from_slice(&branch(IF_ICMPNE, 0));
        }
        miss.push(code.len() - 3);
    }
    code.push(ICONST_1);
    code.push(IRETURN);
    patch_miss_branches(&mut code, &miss);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 2, code), 1);
}

#[test]
fn calendar_set_recomposes_to_leap_day() {
    // set(YEAR,2000)+set(MONTH,1)+set(DATE,29) → 2000-02-29 = 951782400000ms。
    let mut pool = Pool::new();
    let get_instance = pool.methodref("java/util/Calendar", "getInstance", "()Ljava/util/Calendar;");
    let set_millis = pool.methodref("java/util/Calendar", "setTimeInMillis", "(J)V");
    let set = pool.methodref("java/util/Calendar", "set", "(II)V");
    let get_millis = pool.methodref("java/util/Calendar", "getTimeInMillis", "()J");
    let expect = pool.long(951_782_400_000);
    let mut code = Vec::new();
    let mut miss = Vec::new();
    code.extend_from_slice(&op3(INVOKESTATIC, get_instance));
    code.push(ASTORE_1);
    code.push(ALOAD_1);
    code.push(LCONST_0);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, set_millis));
    for (field, value) in [(1, 2000), (2, 1), (5, 29)] {
        code.push(ALOAD_1);
        push_i32(&mut code, field);
        push_i32(&mut code, value);
        code.extend_from_slice(&op3(INVOKEVIRTUAL, set));
    }
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, get_millis));
    code.extend_from_slice(&op3(LDC2_W, expect));
    code.push(LCMP);
    code.extend_from_slice(&branch(IFNE, 0));
    miss.push(code.len() - 3);
    code.push(ICONST_1);
    code.push(IRETURN);
    patch_miss_branches(&mut code, &miss);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 6, 2, code), 1);
}

#[test]
fn calendar_get_time_roundtrip() {
    // cal.setTimeInMillis(12345678).getTime().getTime() == 12345678。
    let mut pool = Pool::new();
    let get_instance = pool.methodref("java/util/Calendar", "getInstance", "()Ljava/util/Calendar;");
    let set_millis = pool.methodref("java/util/Calendar", "setTimeInMillis", "(J)V");
    let get_time = pool.methodref("java/util/Calendar", "getTime", "()Ljava/util/Date;");
    let date_get_time = pool.methodref("java/util/Date", "getTime", "()J");
    let millis = pool.long(12_345_678);
    let mut code = Vec::new();
    code.extend_from_slice(&op3(INVOKESTATIC, get_instance));
    code.push(ASTORE_1);
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(LDC2_W, millis));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, set_millis));
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, get_time));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, date_get_time));
    code.push(LRETURN);
    assert_eq!(expect_long(run_main_outcome(&mut pool, "()J", 5, 2, code)), 12_345_678);
}

#[test]
fn timezone_gmt_defaults() {
    // 默认时区：GMT / 偏移 0 / 无夏令时。
    let mut pool = Pool::new();
    let get_default = pool.methodref("java/util/TimeZone", "getDefault", "()Ljava/util/TimeZone;");
    let get_id = pool.methodref("java/util/TimeZone", "getID", "()Ljava/lang/String;");
    let raw_offset = pool.methodref("java/util/TimeZone", "getRawOffset", "()I");
    let uses_dst = pool.methodref("java/util/TimeZone", "usesDaylightTime", "()Z");
    let gmt = pool.string("GMT");
    let equals = pool.methodref("java/lang/String", "equals", "(Ljava/lang/Object;)Z");
    let mut code = Vec::new();
    let mut miss = Vec::new();
    code.extend_from_slice(&op3(INVOKESTATIC, get_default));
    code.push(ASTORE_1);
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, get_id));
    code.extend_from_slice(&op3(LDC_W, gmt));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, raw_offset));
    code.extend_from_slice(&branch(IFNE, 0));
    miss.push(code.len() - 3);
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, uses_dst));
    code.extend_from_slice(&branch(IFNE, 0));
    miss.push(code.len() - 3);
    code.push(ICONST_1);
    code.push(IRETURN);
    patch_miss_branches(&mut code, &miss);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 4, 2, code), 1);
}

#[test]
fn hashtable_put_get_size() {
    let mut pool = Pool::new();
    let new_ht = pool.class("java/util/Hashtable");
    let init = pool.methodref("java/util/Hashtable", "<init>", "()V");
    let put = pool.methodref("java/util/Hashtable", "put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;");
    let get = pool.methodref("java/util/Hashtable", "get", "(Ljava/lang/Object;)Ljava/lang/Object;");
    let size = pool.methodref("java/util/Hashtable", "size", "()I");
    let equals = pool.methodref("java/lang/String", "equals", "(Ljava/lang/Object;)Z");
    let one = pool.string("one");
    let v1 = pool.string("1");
    let two = pool.string("two");
    let v2 = pool.string("2");
    let missing = pool.string("missing");
    let mut code = Vec::new();
    let mut miss = Vec::new();
    code.extend_from_slice(&op3(NEW, new_ht));
    code.push(DUP);
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code.push(ASTORE_1);
    for (key, value) in [(one, v1), (two, v2)] {
        code.push(ALOAD_1);
        code.extend_from_slice(&op3(LDC_W, key));
        code.extend_from_slice(&op3(LDC_W, value));
        code.extend_from_slice(&op3(INVOKEVIRTUAL, put));
        code.push(POP);
    }
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, size));
    code.push(ICONST_2);
    code.extend_from_slice(&branch(IF_ICMPNE, 0));
    miss.push(code.len() - 3);
    for (key, value) in [(one, v1), (two, v2)] {
        code.push(ALOAD_1);
        code.extend_from_slice(&op3(LDC_W, key));
        code.extend_from_slice(&op3(INVOKEVIRTUAL, get));
        code.extend_from_slice(&op3(LDC_W, value));
        code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
        code.extend_from_slice(&branch(IFEQ, 0));
        miss.push(code.len() - 3);
    }
    // get 缺键 → null
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(LDC_W, missing));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, get));
    code.extend_from_slice(&branch(IFNONNULL, 0));
    miss.push(code.len() - 3);
    code.push(ICONST_1);
    code.push(IRETURN);
    patch_miss_branches(&mut code, &miss);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 6, 2, code), 1);
}

#[test]
fn hashtable_integer_key_by_value() {
    // Integer 键按值相等（JDK equals/hashCode 语义）：put(new Integer(7))
    // 之后用**另一个** new Integer(7) 能 get 回值；equals/hashCode 配对。
    // gjxjwql 资源表实证：身份比较会令所有 Integer 键查询 miss。
    let mut pool = Pool::new();
    let new_ht = pool.class("java/util/Hashtable");
    let ht_init = pool.methodref("java/util/Hashtable", "<init>", "()V");
    let put = pool.methodref("java/util/Hashtable", "put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;");
    let get = pool.methodref("java/util/Hashtable", "get", "(Ljava/lang/Object;)Ljava/lang/Object;");
    let new_int = pool.class("java/lang/Integer");
    let int_init = pool.methodref("java/lang/Integer", "<init>", "(I)V");
    let int_equals = pool.methodref("java/lang/Integer", "equals", "(Ljava/lang/Object;)Z");
    let int_hash = pool.methodref("java/lang/Integer", "hashCode", "()I");
    let v = pool.string("hit");
    let mut code = Vec::new();
    let mut miss = Vec::new();
    // ht = new Hashtable()
    code.extend_from_slice(&op3(NEW, new_ht));
    code.push(DUP);
    code.extend_from_slice(&op3(INVOKESPECIAL, ht_init));
    code.push(ASTORE_1);
    // ht.put(new Integer(7), "hit")
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(NEW, new_int));
    code.push(DUP);
    code.push(BIPUSH);
    code.push(7);
    code.extend_from_slice(&op3(INVOKESPECIAL, int_init));
    code.extend_from_slice(&op3(LDC_W, v));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, put));
    code.push(POP);
    // 另一个 new Integer(7)：hashCode()==7
    code.extend_from_slice(&op3(NEW, new_int));
    code.push(DUP);
    code.push(BIPUSH);
    code.push(7);
    code.extend_from_slice(&op3(INVOKESPECIAL, int_init));
    code.push(ASTORE_2);
    code.push(ALOAD_2);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, int_hash));
    code.push(BIPUSH);
    code.push(7);
    code.extend_from_slice(&branch(IF_ICMPNE, 0));
    miss.push(code.len() - 3);
    // 键 equals 同值对象：a.equals(new Integer(7))
    code.push(ALOAD_2);
    code.extend_from_slice(&op3(NEW, new_int));
    code.push(DUP);
    code.push(BIPUSH);
    code.push(7);
    code.extend_from_slice(&op3(INVOKESPECIAL, int_init));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, int_equals));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    // ht.get(第三个 new Integer(7)) 非 null
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(NEW, new_int));
    code.push(DUP);
    code.push(BIPUSH);
    code.push(7);
    code.extend_from_slice(&op3(INVOKESPECIAL, int_init));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, get));
    code.extend_from_slice(&branch(IFNULL, 0));
    miss.push(code.len() - 3);
    code.push(ICONST_1);
    code.push(IRETURN);
    patch_miss_branches(&mut code, &miss);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 6, 3, code), 1);
}

#[test]
fn hashtable_overwrite_returns_old_value() {
    let mut pool = Pool::new();
    let new_ht = pool.class("java/util/Hashtable");
    let init = pool.methodref("java/util/Hashtable", "<init>", "()V");
    let put = pool.methodref("java/util/Hashtable", "put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;");
    let get = pool.methodref("java/util/Hashtable", "get", "(Ljava/lang/Object;)Ljava/lang/Object;");
    let size = pool.methodref("java/util/Hashtable", "size", "()I");
    let equals = pool.methodref("java/lang/String", "equals", "(Ljava/lang/Object;)Z");
    let key = pool.string("k");
    let a = pool.string("a");
    let b = pool.string("b");
    let mut code = Vec::new();
    let mut miss = Vec::new();
    code.extend_from_slice(&op3(NEW, new_ht));
    code.push(DUP);
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code.push(ASTORE_1);
    // put("k","a") 返回 null
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(LDC_W, key));
    code.extend_from_slice(&op3(LDC_W, a));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, put));
    code.extend_from_slice(&branch(IFNONNULL, 0));
    miss.push(code.len() - 3);
    // put("k","b") 返回旧值 "a"
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(LDC_W, key));
    code.extend_from_slice(&op3(LDC_W, b));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, put));
    code.extend_from_slice(&op3(LDC_W, a));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    // get("k") == "b"，size 仍 1
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(LDC_W, key));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, get));
    code.extend_from_slice(&op3(LDC_W, b));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, size));
    code.push(ICONST_1);
    code.extend_from_slice(&branch(IF_ICMPNE, 0));
    miss.push(code.len() - 3);
    code.push(ICONST_1);
    code.push(IRETURN);
    patch_miss_branches(&mut code, &miss);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 6, 2, code), 1);
}

#[test]
fn hashtable_remove_and_contains_key() {
    let mut pool = Pool::new();
    let new_ht = pool.class("java/util/Hashtable");
    let init = pool.methodref("java/util/Hashtable", "<init>", "()V");
    let put = pool.methodref("java/util/Hashtable", "put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;");
    let contains_key = pool.methodref("java/util/Hashtable", "containsKey", "(Ljava/lang/Object;)Z");
    let contains = pool.methodref("java/util/Hashtable", "contains", "(Ljava/lang/Object;)Z");
    let remove = pool.methodref("java/util/Hashtable", "remove", "(Ljava/lang/Object;)Ljava/lang/Object;");
    let size = pool.methodref("java/util/Hashtable", "size", "()I");
    let equals = pool.methodref("java/lang/String", "equals", "(Ljava/lang/Object;)Z");
    let key = pool.string("k");
    let value = pool.string("a");
    let mut code = Vec::new();
    let mut miss = Vec::new();
    code.extend_from_slice(&op3(NEW, new_ht));
    code.push(DUP);
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code.push(ASTORE_1);
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(LDC_W, key));
    code.extend_from_slice(&op3(LDC_W, value));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, put));
    code.push(POP);
    // contains("a") && containsKey("k")
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(LDC_W, value));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, contains));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(LDC_W, key));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, contains_key));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    // remove("k") 返回 "a"
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(LDC_W, key));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, remove));
    code.extend_from_slice(&op3(LDC_W, value));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    // 删除后 containsKey 假、size 0
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(LDC_W, key));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, contains_key));
    code.extend_from_slice(&branch(IFNE, 0));
    miss.push(code.len() - 3);
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, size));
    code.extend_from_slice(&branch(IFNE, 0));
    miss.push(code.len() - 3);
    code.push(ICONST_1);
    code.push(IRETURN);
    patch_miss_branches(&mut code, &miss);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 6, 2, code), 1);
}

#[test]
fn hashtable_clear_and_is_empty() {
    let mut pool = Pool::new();
    let new_ht = pool.class("java/util/Hashtable");
    let init = pool.methodref("java/util/Hashtable", "<init>", "()V");
    let put = pool.methodref("java/util/Hashtable", "put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;");
    let clear = pool.methodref("java/util/Hashtable", "clear", "()V");
    let is_empty = pool.methodref("java/util/Hashtable", "isEmpty", "()Z");
    let size = pool.methodref("java/util/Hashtable", "size", "()I");
    let key = pool.string("a");
    let value = pool.string("1");
    let mut code = Vec::new();
    let mut miss = Vec::new();
    code.extend_from_slice(&op3(NEW, new_ht));
    code.push(DUP);
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code.push(ASTORE_1);
    // 初始 isEmpty
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, is_empty));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(LDC_W, key));
    code.extend_from_slice(&op3(LDC_W, value));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, put));
    code.push(POP);
    // 非空
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, is_empty));
    code.extend_from_slice(&branch(IFNE, 0));
    miss.push(code.len() - 3);
    // clear 后 size 0 且 isEmpty
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, clear));
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, size));
    code.extend_from_slice(&branch(IFNE, 0));
    miss.push(code.len() - 3);
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, is_empty));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    code.push(ICONST_1);
    code.push(IRETURN);
    patch_miss_branches(&mut code, &miss);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 2, code), 1);
}

#[test]
fn hashtable_keys_enumeration_walks_all_entries() {
    // keys() 枚举按插入序走全部键并正常终止。
    let mut pool = Pool::new();
    let new_ht = pool.class("java/util/Hashtable");
    let init = pool.methodref("java/util/Hashtable", "<init>", "()V");
    let put = pool.methodref("java/util/Hashtable", "put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;");
    let keys = pool.methodref("java/util/Hashtable", "keys", "()Ljava/util/Enumeration;");
    let has_more = pool.methodref("java/util/Enumeration", "hasMoreElements", "()Z");
    let next = pool.methodref("java/util/Enumeration", "nextElement", "()Ljava/lang/Object;");
    let equals = pool.methodref("java/lang/String", "equals", "(Ljava/lang/Object;)Z");
    let ka = pool.string("a");
    let va = pool.string("1");
    let kb = pool.string("b");
    let vb = pool.string("2");
    let mut code = Vec::new();
    let mut miss = Vec::new();
    code.extend_from_slice(&op3(NEW, new_ht));
    code.push(DUP);
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code.push(ASTORE_1);
    for (key, value) in [(ka, va), (kb, vb)] {
        code.push(ALOAD_1);
        code.extend_from_slice(&op3(LDC_W, key));
        code.extend_from_slice(&op3(LDC_W, value));
        code.extend_from_slice(&op3(INVOKEVIRTUAL, put));
        code.push(POP);
    }
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, keys));
    code.push(ASTORE_2);
    // hasMore → "a" → hasMore → "b" → !hasMore
    code.push(ALOAD_2);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, has_more));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    for expect in [ka, kb] {
        code.push(ALOAD_2);
        code.extend_from_slice(&op3(INVOKEVIRTUAL, next));
        code.extend_from_slice(&op3(LDC_W, expect));
        code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
        code.extend_from_slice(&branch(IFEQ, 0));
        miss.push(code.len() - 3);
        code.push(ALOAD_2);
        code.extend_from_slice(&op3(INVOKEVIRTUAL, has_more));
        let op = if expect == kb { IFNE } else { IFEQ };
        code.extend_from_slice(&branch(op, 0));
        miss.push(code.len() - 3);
    }
    code.push(ICONST_1);
    code.push(IRETURN);
    patch_miss_branches(&mut code, &miss);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 6, 3, code), 1);
}

#[test]
fn vector_elements_snapshot_semantics() {
    // elements() 为快照：removeAllElements 后仍枚举出全部元素。
    let mut pool = Pool::new();
    let new_vec = pool.class("java/util/Vector");
    let init = pool.methodref("java/util/Vector", "<init>", "()V");
    let add = pool.methodref("java/util/Vector", "addElement", "(Ljava/lang/Object;)V");
    let elements = pool.methodref("java/util/Vector", "elements", "()Ljava/util/Enumeration;");
    let remove_all = pool.methodref("java/util/Vector", "removeAllElements", "()V");
    let size = pool.methodref("java/util/Vector", "size", "()I");
    let has_more = pool.methodref("java/util/Enumeration", "hasMoreElements", "()Z");
    let next = pool.methodref("java/util/Enumeration", "nextElement", "()Ljava/lang/Object;");
    let equals = pool.methodref("java/lang/String", "equals", "(Ljava/lang/Object;)Z");
    let x = pool.string("x");
    let y = pool.string("y");
    let mut code = Vec::new();
    let mut miss = Vec::new();
    code.extend_from_slice(&op3(NEW, new_vec));
    code.push(DUP);
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code.push(ASTORE_1);
    for item in [x, y] {
        code.push(ALOAD_1);
        code.extend_from_slice(&op3(LDC_W, item));
        code.extend_from_slice(&op3(INVOKEVIRTUAL, add));
    }
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, elements));
    code.push(ASTORE_2);
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, remove_all));
    // 快照仍可枚举
    code.push(ALOAD_2);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, has_more));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    for expect in [x, y] {
        code.push(ALOAD_2);
        code.extend_from_slice(&op3(INVOKEVIRTUAL, next));
        code.extend_from_slice(&op3(LDC_W, expect));
        code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
        code.extend_from_slice(&branch(IFEQ, 0));
        miss.push(code.len() - 3);
    }
    code.push(ALOAD_2);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, has_more));
    code.extend_from_slice(&branch(IFNE, 0));
    miss.push(code.len() - 3);
    // 原 Vector 已空
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, size));
    code.extend_from_slice(&branch(IFNE, 0));
    miss.push(code.len() - 3);
    code.push(ICONST_1);
    code.push(IRETURN);
    patch_miss_branches(&mut code, &miss);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 6, 3, code), 1);
}

#[test]
fn vector_add_size_capacity_growth() {
    // new Vector(2) 加 3 个元素：容量 2→4，size 3。
    let mut pool = Pool::new();
    let new_vec = pool.class("java/util/Vector");
    let init = pool.methodref("java/util/Vector", "<init>", "(I)V");
    let add = pool.methodref("java/util/Vector", "addElement", "(Ljava/lang/Object;)V");
    let size = pool.methodref("java/util/Vector", "size", "()I");
    let capacity = pool.methodref("java/util/Vector", "capacity", "()I");
    let is_empty = pool.methodref("java/util/Vector", "isEmpty", "()Z");
    let element_at = pool.methodref("java/util/Vector", "elementAt", "(I)Ljava/lang/Object;");
    let equals = pool.methodref("java/lang/String", "equals", "(Ljava/lang/Object;)Z");
    let a = pool.string("a");
    let b = pool.string("b");
    let c = pool.string("c");
    let mut code = Vec::new();
    let mut miss = Vec::new();
    code.extend_from_slice(&op3(NEW, new_vec));
    code.push(DUP);
    code.push(ICONST_2);
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code.push(ASTORE_1);
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, is_empty));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    for item in [a, b, c] {
        code.push(ALOAD_1);
        code.extend_from_slice(&op3(LDC_W, item));
        code.extend_from_slice(&op3(INVOKEVIRTUAL, add));
    }
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, size));
    code.push(ICONST_3);
    code.extend_from_slice(&branch(IF_ICMPNE, 0));
    miss.push(code.len() - 3);
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, capacity));
    code.push(ICONST_4);
    code.extend_from_slice(&branch(IF_ICMPNE, 0));
    miss.push(code.len() - 3);
    code.push(ALOAD_1);
    code.push(ICONST_2);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, element_at));
    code.extend_from_slice(&op3(LDC_W, c));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    code.push(ICONST_1);
    code.push(IRETURN);
    patch_miss_branches(&mut code, &miss);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 6, 2, code), 1);
}

#[test]
fn vector_index_of_contains_remove() {
    let mut pool = Pool::new();
    let new_vec = pool.class("java/util/Vector");
    let init = pool.methodref("java/util/Vector", "<init>", "()V");
    let add = pool.methodref("java/util/Vector", "addElement", "(Ljava/lang/Object;)V");
    let index_of = pool.methodref("java/util/Vector", "indexOf", "(Ljava/lang/Object;)I");
    let index_of_from = pool.methodref("java/util/Vector", "indexOf", "(Ljava/lang/Object;I)I");
    let contains = pool.methodref("java/util/Vector", "contains", "(Ljava/lang/Object;)Z");
    let remove = pool.methodref("java/util/Vector", "removeElement", "(Ljava/lang/Object;)Z");
    let size = pool.methodref("java/util/Vector", "size", "()I");
    let a = pool.string("a");
    let b = pool.string("b");
    let zz = pool.string("zz");
    let mut code = Vec::new();
    let mut miss = Vec::new();
    code.extend_from_slice(&op3(NEW, new_vec));
    code.push(DUP);
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code.push(ASTORE_1);
    for item in [a, b, a] {
        code.push(ALOAD_1);
        code.extend_from_slice(&op3(LDC_W, item));
        code.extend_from_slice(&op3(INVOKEVIRTUAL, add));
    }
    // indexOf("a") == 0
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(LDC_W, a));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, index_of));
    code.extend_from_slice(&branch(IFNE, 0));
    miss.push(code.len() - 3);
    // indexOf("a", 1) == 2
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(LDC_W, a));
    code.push(ICONST_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, index_of_from));
    code.push(ICONST_2);
    code.extend_from_slice(&branch(IF_ICMPNE, 0));
    miss.push(code.len() - 3);
    // contains("b")
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(LDC_W, b));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, contains));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    // removeElement("a") 删首个命中 → true，size 2
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(LDC_W, a));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, remove));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, size));
    code.push(ICONST_2);
    code.extend_from_slice(&branch(IF_ICMPNE, 0));
    miss.push(code.len() - 3);
    // indexOf("a") == 1（[b, a] 中 a 已在后位）
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(LDC_W, a));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, index_of));
    code.push(ICONST_1);
    code.extend_from_slice(&branch(IF_ICMPNE, 0));
    miss.push(code.len() - 3);
    // removeElement("zz") → false
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(LDC_W, zz));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, remove));
    code.extend_from_slice(&branch(IFNE, 0));
    miss.push(code.len() - 3);
    code.push(ICONST_1);
    code.push(IRETURN);
    patch_miss_branches(&mut code, &miss);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 6, 2, code), 1);
}

#[test]
fn vector_first_last_element() {
    let mut pool = Pool::new();
    let new_vec = pool.class("java/util/Vector");
    let init = pool.methodref("java/util/Vector", "<init>", "()V");
    let add = pool.methodref("java/util/Vector", "addElement", "(Ljava/lang/Object;)V");
    let first = pool.methodref("java/util/Vector", "firstElement", "()Ljava/lang/Object;");
    let last = pool.methodref("java/util/Vector", "lastElement", "()Ljava/lang/Object;");
    let equals = pool.methodref("java/lang/String", "equals", "(Ljava/lang/Object;)Z");
    let a = pool.string("a");
    let b = pool.string("b");
    let mut code = Vec::new();
    let mut miss = Vec::new();
    code.extend_from_slice(&op3(NEW, new_vec));
    code.push(DUP);
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code.push(ASTORE_1);
    for item in [a, b] {
        code.push(ALOAD_1);
        code.extend_from_slice(&op3(LDC_W, item));
        code.extend_from_slice(&op3(INVOKEVIRTUAL, add));
    }
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, first));
    code.extend_from_slice(&op3(LDC_W, a));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, last));
    code.extend_from_slice(&op3(LDC_W, b));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    code.push(ICONST_1);
    code.push(IRETURN);
    patch_miss_branches(&mut code, &miss);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 2, code), 1);
}

#[test]
fn vector_first_element_on_empty_throws_no_such_element() {
    let mut pool = Pool::new();
    let new_vec = pool.class("java/util/Vector");
    let init = pool.methodref("java/util/Vector", "<init>", "()V");
    let first = pool.methodref("java/util/Vector", "firstElement", "()Ljava/lang/Object;");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(NEW, new_vec)); // 0-2
    code.push(DUP); // 3
    code.extend_from_slice(&op3(INVOKESPECIAL, init)); // 4-6
    code.extend_from_slice(&op3(INVOKEVIRTUAL, first)); // 7-9
    code.push(POP); // 10
    code.push(ICONST_0); // 11
    code.extend_from_slice(&branch(GOTO, 4)); // 12-14 → 16
    code.push(ICONST_1); // 15
    code.push(IRETURN); // 16
    let bytes = build_class(
        &mut pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_catch(
            "()I",
            3,
            1,
            code,
            vec![handler(0, 10, 15, Some("java/util/NoSuchElementException"))],
        )],
    );
    let mut vm = Vm::new(vec![("T", bytes)]);
    assert_eq!(expect_int(vm.exec("()I").unwrap()), 1);
}

#[test]
fn vector_insert_remove_at_and_copy_into() {
    let mut pool = Pool::new();
    let new_vec = pool.class("java/util/Vector");
    let init = pool.methodref("java/util/Vector", "<init>", "()V");
    let add = pool.methodref("java/util/Vector", "addElement", "(Ljava/lang/Object;)V");
    let insert = pool.methodref("java/util/Vector", "insertElementAt", "(Ljava/lang/Object;I)V");
    let remove_at = pool.methodref("java/util/Vector", "removeElementAt", "(I)V");
    let element_at = pool.methodref("java/util/Vector", "elementAt", "(I)Ljava/lang/Object;");
    let copy_into = pool.methodref("java/util/Vector", "copyInto", "([Ljava/lang/Object;)V");
    let equals = pool.methodref("java/lang/String", "equals", "(Ljava/lang/Object;)Z");
    let object_class = pool.class("java/lang/Object");
    let a = pool.string("a");
    let b = pool.string("b");
    let mut code = Vec::new();
    let mut miss = Vec::new();
    code.extend_from_slice(&op3(NEW, new_vec));
    code.push(DUP);
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code.push(ASTORE_1);
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(LDC_W, b));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, add));
    // insertElementAt("a", 0) → [a, b]
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(LDC_W, a));
    code.push(ICONST_0);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, insert));
    code.push(ALOAD_1);
    code.push(ICONST_0);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, element_at));
    code.extend_from_slice(&op3(LDC_W, a));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    // removeElementAt(0) → [b]
    code.push(ALOAD_1);
    code.push(ICONST_0);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, remove_at));
    code.push(ALOAD_1);
    code.push(ICONST_0);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, element_at));
    code.extend_from_slice(&op3(LDC_W, b));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    // copyInto：new Object[1] 接收
    code.push(ICONST_1);
    code.extend_from_slice(&op3(ANEWARRAY, object_class));
    code.push(ASTORE_2);
    code.push(ALOAD_1);
    code.push(ALOAD_2);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, copy_into));
    code.push(ALOAD_2);
    code.push(ICONST_0);
    code.push(AALOAD);
    code.extend_from_slice(&op3(LDC_W, b));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    code.push(ICONST_1);
    code.push(IRETURN);
    patch_miss_branches(&mut code, &miss);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 6, 3, code), 1);
}

#[test]
fn vector_element_at_out_of_range_throws() {
    let mut pool = Pool::new();
    let new_vec = pool.class("java/util/Vector");
    let init = pool.methodref("java/util/Vector", "<init>", "()V");
    let add = pool.methodref("java/util/Vector", "addElement", "(Ljava/lang/Object;)V");
    let element_at = pool.methodref("java/util/Vector", "elementAt", "(I)Ljava/lang/Object;");
    let a = pool.string("a");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(NEW, new_vec)); // 0-2
    code.push(DUP); // 3
    code.extend_from_slice(&op3(INVOKESPECIAL, init)); // 4-6
    code.push(ASTORE_1); // 7
    code.push(ALOAD_1); // 8
    code.extend_from_slice(&op3(LDC_W, a)); // 9-11
    code.extend_from_slice(&op3(INVOKEVIRTUAL, add)); // 12-14
    code.push(ALOAD_1); // 15
    code.push(ICONST_5); // 16
    code.extend_from_slice(&op3(INVOKEVIRTUAL, element_at)); // 17-19
    code.push(POP); // 20
    code.push(ICONST_0); // 21
    code.extend_from_slice(&branch(GOTO, 4)); // 22-24 → 26
    code.push(ICONST_1); // 25（异常处理器入口）
    code.push(IRETURN); // 26
    let bytes = build_class(
        &mut pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_catch(
            "()I",
            4,
            2,
            code,
            vec![handler(0, 20, 25, Some("java/lang/ArrayIndexOutOfBoundsException"))],
        )],
    );
    let mut vm = Vm::new(vec![("T", bytes)]);
    assert_eq!(expect_int(vm.exec("()I").unwrap()), 1);
}

#[test]
fn vector_negative_capacity_throws_illegal_argument() {
    let mut pool = Pool::new();
    let new_vec = pool.class("java/util/Vector");
    let init = pool.methodref("java/util/Vector", "<init>", "(I)V");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(NEW, new_vec)); // 0-2
    code.push(DUP); // 3
    code.push(ICONST_M1); // 4
    code.extend_from_slice(&op3(INVOKESPECIAL, init)); // 5-7
    code.push(ICONST_0); // 8
    code.extend_from_slice(&branch(GOTO, 4)); // 9-11 → 13
    code.push(ICONST_1); // 12
    code.push(IRETURN); // 13
    let bytes = build_class(
        &mut pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_catch(
            "()I",
            3,
            1,
            code,
            vec![handler(0, 8, 12, Some("java/lang/IllegalArgumentException"))],
        )],
    );
    let mut vm = Vm::new(vec![("T", bytes)]);
    assert_eq!(expect_int(vm.exec("()I").unwrap()), 1);
}

#[test]
fn stack_push_pop_peek_search() {
    // Stack 继承 Vector 布局：自有方法与继承方法可混用。
    let mut pool = Pool::new();
    let new_stack = pool.class("java/util/Stack");
    let init = pool.methodref("java/util/Stack", "<init>", "()V");
    let push = pool.methodref("java/util/Stack", "push", "(Ljava/lang/Object;)Ljava/lang/Object;");
    let pop = pool.methodref("java/util/Stack", "pop", "()Ljava/lang/Object;");
    let peek = pool.methodref("java/util/Stack", "peek", "()Ljava/lang/Object;");
    let empty = pool.methodref("java/util/Stack", "empty", "()Z");
    let search = pool.methodref("java/util/Stack", "search", "(Ljava/lang/Object;)I");
    let size = pool.methodref("java/util/Vector", "size", "()I"); // 继承方法
    let equals = pool.methodref("java/lang/String", "equals", "(Ljava/lang/Object;)Z");
    let a = pool.string("a");
    let b = pool.string("b");
    let zz = pool.string("zz");
    let mut code = Vec::new();
    let mut miss = Vec::new();
    code.extend_from_slice(&op3(NEW, new_stack));
    code.push(DUP);
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code.push(ASTORE_1);
    // 空
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, empty));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    // push 返回 item
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(LDC_W, a));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, push));
    code.extend_from_slice(&op3(LDC_W, a));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(LDC_W, b));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, push));
    code.push(POP);
    // search：栈顶 b=1、次顶 a=2、未命中 -1
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(LDC_W, b));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, search));
    code.push(ICONST_1);
    code.extend_from_slice(&branch(IF_ICMPNE, 0));
    miss.push(code.len() - 3);
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(LDC_W, a));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, search));
    code.push(ICONST_2);
    code.extend_from_slice(&branch(IF_ICMPNE, 0));
    miss.push(code.len() - 3);
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(LDC_W, zz));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, search));
    code.push(ICONST_M1);
    code.extend_from_slice(&branch(IF_ICMPNE, 0));
    miss.push(code.len() - 3);
    // pop/peek 序列
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, pop));
    code.extend_from_slice(&op3(LDC_W, b));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, peek));
    code.extend_from_slice(&op3(LDC_W, a));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    // 继承 size == 1
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, size));
    code.push(ICONST_1);
    code.extend_from_slice(&branch(IF_ICMPNE, 0));
    miss.push(code.len() - 3);
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, pop));
    code.extend_from_slice(&op3(LDC_W, a));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    // 弹空后 empty
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, empty));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    code.push(ICONST_1);
    code.push(IRETURN);
    patch_miss_branches(&mut code, &miss);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 6, 2, code), 1);
}

#[test]
fn stack_pop_on_empty_throws_empty_stack() {
    let mut pool = Pool::new();
    let new_stack = pool.class("java/util/Stack");
    let init = pool.methodref("java/util/Stack", "<init>", "()V");
    let pop = pool.methodref("java/util/Stack", "pop", "()Ljava/lang/Object;");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(NEW, new_stack)); // 0-2
    code.push(DUP); // 3
    code.extend_from_slice(&op3(INVOKESPECIAL, init)); // 4-6
    code.extend_from_slice(&op3(INVOKEVIRTUAL, pop)); // 7-9
    code.push(POP); // 10
    code.push(ICONST_0); // 11
    code.extend_from_slice(&branch(GOTO, 4)); // 12-14 → 16
    code.push(ICONST_1); // 15
    code.push(IRETURN); // 16
    let bytes = build_class(
        &mut pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_catch(
            "()I",
            3,
            1,
            code,
            vec![handler(0, 10, 15, Some("java/util/EmptyStackException"))],
        )],
    );
    let mut vm = Vm::new(vec![("T", bytes)]);
    assert_eq!(expect_int(vm.exec("()I").unwrap()), 1);
}

#[test]
fn timer_task_cancel_and_scheduled_execution_time() {
    // 数据面：新任务 nextTime=0；cancel 首次 true、再次 false。
    let mut pool = Pool::new();
    let new_task = pool.class("java/util/TimerTask");
    let init = pool.methodref("java/util/TimerTask", "<init>", "()V");
    let scheduled = pool.methodref("java/util/TimerTask", "scheduledExecutionTime", "()J");
    let cancel = pool.methodref("java/util/TimerTask", "cancel", "()Z");
    let mut code = Vec::new();
    let mut miss = Vec::new();
    code.extend_from_slice(&op3(NEW, new_task));
    code.push(DUP);
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code.push(ASTORE_1);
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, scheduled));
    code.push(LCONST_0);
    code.push(LCMP);
    code.extend_from_slice(&branch(IFNE, 0));
    miss.push(code.len() - 3);
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, cancel));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, cancel));
    code.extend_from_slice(&branch(IFNE, 0));
    miss.push(code.len() - 3);
    code.push(ICONST_1);
    code.push(IRETURN);
    patch_miss_branches(&mut code, &miss);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 2, code), 1);
}

#[test]
fn timer_schedule_date_registers_task() {
    // schedule(task, Date(999999))：nextTime 落 999999；purge 恒 0。
    let mut pool = Pool::new();
    let new_timer = pool.class("java/util/Timer");
    let timer_init = pool.methodref("java/util/Timer", "<init>", "()V");
    let new_task = pool.class("java/util/TimerTask");
    let task_init = pool.methodref("java/util/TimerTask", "<init>", "()V");
    let new_date = pool.class("java/util/Date");
    let date_init = pool.methodref("java/util/Date", "<init>", "(J)V");
    let schedule =
        pool.methodref("java/util/Timer", "schedule", "(Ljava/util/TimerTask;Ljava/util/Date;)V");
    let scheduled = pool.methodref("java/util/TimerTask", "scheduledExecutionTime", "()J");
    let task_cancel = pool.methodref("java/util/TimerTask", "cancel", "()Z");
    let timer_cancel = pool.methodref("java/util/Timer", "cancel", "()V");
    let purge = pool.methodref("java/util/Timer", "purge", "()I");
    let when = pool.long(999_999);
    let mut code = Vec::new();
    let mut miss = Vec::new();
    code.extend_from_slice(&op3(NEW, new_timer));
    code.push(DUP);
    code.extend_from_slice(&op3(INVOKESPECIAL, timer_init));
    code.push(ASTORE_1);
    code.extend_from_slice(&op3(NEW, new_task));
    code.push(DUP);
    code.extend_from_slice(&op3(INVOKESPECIAL, task_init));
    code.push(ASTORE_2);
    code.extend_from_slice(&op3(NEW, new_date));
    code.push(DUP);
    code.extend_from_slice(&op3(LDC2_W, when));
    code.extend_from_slice(&op3(INVOKESPECIAL, date_init));
    code.push(ASTORE_3);
    code.push(ALOAD_1);
    code.push(ALOAD_2);
    code.push(ALOAD_3);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, schedule));
    // task.scheduledExecutionTime() == 999999
    code.push(ALOAD_2);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, scheduled));
    code.extend_from_slice(&op3(LDC2_W, when));
    code.push(LCMP);
    code.extend_from_slice(&branch(IFNE, 0));
    miss.push(code.len() - 3);
    // task.cancel() 首次生效
    code.push(ALOAD_2);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, task_cancel));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    // Timer.cancel / purge 数据面
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, timer_cancel));
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, purge));
    code.extend_from_slice(&branch(IFNE, 0));
    miss.push(code.len() - 3);
    // 已取消任务再 cancel → false
    code.push(ALOAD_2);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, task_cancel));
    code.extend_from_slice(&branch(IFNE, 0));
    miss.push(code.len() - 3);
    code.push(ICONST_1);
    code.push(IRETURN);
    patch_miss_branches(&mut code, &miss);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 6, 4, code), 1);
}

#[test]
fn timer_negative_delay_throws_illegal_argument() {
    let mut pool = Pool::new();
    let new_timer = pool.class("java/util/Timer");
    let timer_init = pool.methodref("java/util/Timer", "<init>", "()V");
    let new_task = pool.class("java/util/TimerTask");
    let task_init = pool.methodref("java/util/TimerTask", "<init>", "()V");
    let schedule = pool.methodref("java/util/Timer", "schedule", "(Ljava/util/TimerTask;J)V");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(NEW, new_timer)); // 0-2
    code.push(DUP); // 3
    code.extend_from_slice(&op3(INVOKESPECIAL, timer_init)); // 4-6
    code.push(ASTORE_1); // 7
    code.extend_from_slice(&op3(NEW, new_task)); // 8-10
    code.push(DUP); // 11
    code.extend_from_slice(&op3(INVOKESPECIAL, task_init)); // 12-14
    code.push(ASTORE_2); // 15
    code.push(ALOAD_1); // 16
    code.push(ALOAD_2); // 17
    let negative = pool.long(-1);
    code.extend_from_slice(&op3(LDC2_W, negative)); // 18-20
    code.extend_from_slice(&op3(INVOKEVIRTUAL, schedule)); // 21-23
    code.push(ICONST_0); // 24
    code.extend_from_slice(&branch(GOTO, 4)); // 25-27 → 29
    code.push(ICONST_1); // 28
    code.push(IRETURN); // 29
    let bytes = build_class(
        &mut pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_catch(
            "()I",
            5,
            3,
            code,
            vec![handler(0, 24, 28, Some("java/lang/IllegalArgumentException"))],
        )],
    );
    let mut vm = Vm::new(vec![("T", bytes)]);
    assert_eq!(expect_int(vm.exec("()I").unwrap()), 1);
}

#[test]
fn string_substring_out_of_range_throws_sioobe() {
    // "abc".substring(5) → StringIndexOutOfBoundsException，捕获返回 1。
    let mut pool = Pool::new();
    let abc = pool.string("abc");
    let substring = pool.methodref("java/lang/String", "substring", "(I)Ljava/lang/String;");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, abc)); // 0-2
    code.push(BIPUSH); // 3-4
    code.push(5);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, substring)); // 5-7
    code.push(POP); // 8：正常路径丢弃返回值
    code.push(ICONST_0); // 9
    code.extend_from_slice(&branch(GOTO, 4)); // 10-12 → 14
    code.push(ICONST_1); // 13：处理器
    code.push(IRETURN); // 14
    let bytes = build_class(
        &mut pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_catch(
            "()I",
            3,
            1,
            code,
            vec![handler(
                0,
                8,
                13,
                Some("java/lang/StringIndexOutOfBoundsException"),
            )],
        )],
    );
    let mut vm = Vm::new(vec![("T", bytes)]);
    assert_eq!(expect_int(vm.exec("()I").unwrap()), 1);
}

#[test]
fn native_thrown_exception_catchable_by_supertype() {
    // 同一 SIOOBE 用 java/lang/Exception 捕获：验证异常继承链。
    let mut pool = Pool::new();
    let abc = pool.string("abc");
    let char_at = pool.methodref("java/lang/String", "charAt", "(I)C");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, abc)); // 0-2
    code.push(ICONST_5); // 3
    code.extend_from_slice(&op3(INVOKEVIRTUAL, char_at)); // 4-6
    code.push(POP); // 7
    code.push(ICONST_0); // 8
    code.extend_from_slice(&branch(GOTO, 4)); // 9-11 → 13
    code.push(ICONST_1); // 12
    code.push(IRETURN); // 13
    let bytes = build_class(
        &mut pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_catch("()I", 3, 1, code, vec![handler(0, 7, 12, Some("java/lang/Exception"))])],
    );
    let mut vm = Vm::new(vec![("T", bytes)]);
    assert_eq!(expect_int(vm.exec("()I").unwrap()), 1);
}

#[test]
fn uncaught_native_exception_yields_thrown_unwind() {
    // 不捕获：substring(5) 的 SIOOBE 以 Thrown 逃逸根帧，类名与消息可查。
    let mut pool = Pool::new();
    let abc = pool.string("abc");
    let substring = pool.methodref("java/lang/String", "substring", "(I)Ljava/lang/String;");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, abc));
    code.push(BIPUSH);
    code.push(5);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, substring));
    code.push(POP);
    code.push(ICONST_0);
    code.push(IRETURN);
    let bytes = build_class(
        &mut pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_code("()I", 3, 1, code)],
    );
    let mut vm = Vm::new(vec![("T", bytes)]);
    let outcome = vm.exec("()I").unwrap();
    let RunOutcome::Uncaught(Unwind::Thrown(exception)) = outcome else {
        panic!("expected uncaught Thrown, got {outcome:?}")
    };
    let class = runtime_class_of(&vm.cs, &vm.heap, exception).unwrap();
    assert_eq!(vm.cs.name_of(class), "java/lang/StringIndexOutOfBoundsException");
    // detailMessage 经继承链可查（Throwable 字段）。
    let name = vm.cs.syms.intern("detailMessage");
    let desc = vm.cs.syms.intern("Ljava/lang/String;");
    let field = vm.cs.find_field(class, name, desc).unwrap();
    let Slot::Ref(message) = vm.heap.instance(exception).unwrap().fields[field.slot as usize]
    else {
        panic!("detailMessage 应为引用槽")
    };
    assert_eq!(str_text(&vm.heap, message), "String index out of range: 5");
}

#[test]
fn string_index_of_family() {
    // indexOf('l')*10 + indexOf('l',3) + lastIndexOf('l')*100 + indexOf("ll")*1000
    // = 2*10 + 3 + 3*100 + 2*1000 = 2323
    let mut pool = Pool::new();
    let hello = pool.string("hello");
    let ll = pool.string("ll");
    let index_of = pool.methodref("java/lang/String", "indexOf", "(I)I");
    let index_of_from = pool.methodref("java/lang/String", "indexOf", "(II)I");
    let last_index_of = pool.methodref("java/lang/String", "lastIndexOf", "(I)I");
    let index_of_str = pool.methodref("java/lang/String", "indexOf", "(Ljava/lang/String;)I");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, hello));
    code.push(BIPUSH);
    code.push(b'l' as i8 as u8);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, index_of));
    code.push(BIPUSH);
    code.push(10);
    code.push(IMUL);
    code.extend_from_slice(&op3(LDC_W, hello));
    code.push(BIPUSH);
    code.push(b'l' as i8 as u8);
    code.push(ICONST_3);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, index_of_from));
    code.push(IADD);
    code.extend_from_slice(&op3(LDC_W, hello));
    code.push(BIPUSH);
    code.push(b'l' as i8 as u8);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, last_index_of));
    code.push(BIPUSH);
    code.push(100);
    code.push(IMUL);
    code.push(IADD);
    code.extend_from_slice(&op3(LDC_W, hello));
    code.extend_from_slice(&op3(LDC_W, ll));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, index_of_str));
    code.push(SIPUSH);
    code.extend_from_slice(&1000i16.to_be_bytes());
    code.push(IMUL);
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 1, code), 2323);
}

#[test]
fn string_compare_to() {
    // "abc".compareTo("abd") == -1（首个不同码元差）。
    let mut pool = Pool::new();
    let abc = pool.string("abc");
    let abd = pool.string("abd");
    let compare = pool.methodref("java/lang/String", "compareTo", "(Ljava/lang/String;)I");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, abc));
    code.extend_from_slice(&op3(LDC_W, abd));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, compare));
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 3, 1, code), -1);
}

#[test]
fn string_starts_and_ends_with() {
    let mut pool = Pool::new();
    let hello = pool.string("hello");
    let he = pool.string("he");
    let lo = pool.string("lo");
    let starts = pool.methodref("java/lang/String", "startsWith", "(Ljava/lang/String;)Z");
    let ends = pool.methodref("java/lang/String", "endsWith", "(Ljava/lang/String;)Z");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, hello));
    code.extend_from_slice(&op3(LDC_W, he));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, starts));
    code.push(BIPUSH);
    code.push(10);
    code.push(IMUL);
    code.extend_from_slice(&op3(LDC_W, hello));
    code.extend_from_slice(&op3(LDC_W, lo));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, ends));
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 1, code), 11);
}

#[test]
fn string_trim_and_ascii_case() {
    // "  aB ".trim().equals("aB") *10 + "aB1".toUpperCase().equals("AB1")
    let mut pool = Pool::new();
    let padded = pool.string("  aB ");
    let ab = pool.string("aB");
    let mixed = pool.string("aB1");
    let upper = pool.string("AB1");
    let trim = pool.methodref("java/lang/String", "trim", "()Ljava/lang/String;");
    let to_upper = pool.methodref("java/lang/String", "toUpperCase", "()Ljava/lang/String;");
    let equals = pool.methodref("java/lang/String", "equals", "(Ljava/lang/Object;)Z");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, padded));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, trim));
    code.extend_from_slice(&op3(LDC_W, ab));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.push(BIPUSH);
    code.push(10);
    code.push(IMUL);
    code.extend_from_slice(&op3(LDC_W, mixed));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, to_upper));
    code.extend_from_slice(&op3(LDC_W, upper));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 1, code), 11);
}

#[test]
fn string_to_char_array_roundtrip() {
    // "hi".toCharArray()：长度 2，首元素 'h'。
    let mut pool = Pool::new();
    let hi = pool.string("hi");
    let to_chars = pool.methodref("java/lang/String", "toCharArray", "()[C");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, hi));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, to_chars));
    code.push(ASTORE_1);
    code.push(ALOAD_1);
    code.push(ARRAYLENGTH);
    code.push(BIPUSH);
    code.push(100);
    code.push(IMUL);
    code.push(ALOAD_1);
    code.push(ICONST_0);
    code.push(CALOAD);
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 4, 2, code), 2 * 100 + 'h' as i32);
}

#[test]
fn string_get_bytes_latin1() {
    // "AB".getBytes(null)：Latin-1 低 8 位 → [65, 66]。
    let mut pool = Pool::new();
    let ab = pool.string("AB");
    let get_bytes = pool.methodref("java/lang/String", "getBytes", "(Ljava/lang/String;)[B");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, ab));
    code.push(ACONST_NULL);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, get_bytes));
    code.push(ASTORE_1);
    code.push(ALOAD_1);
    code.push(ARRAYLENGTH);
    code.push(BIPUSH);
    code.push(100);
    code.push(IMUL);
    code.push(ALOAD_1);
    code.push(ICONST_0);
    code.push(BALOAD);
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 4, 2, code), 265);
}

#[test]
fn string_intern_returns_interned_identity() {
    // ldc 字面量与 new String("hi").intern() 应命中同一驻留对象。
    let mut pool = Pool::new();
    let hi = pool.string("hi");
    let new_string = pool.class("java/lang/String");
    let init = pool.methodref("java/lang/String", "<init>", "(Ljava/lang/String;)V");
    let intern = pool.methodref("java/lang/String", "intern", "()Ljava/lang/String;");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, hi)); // 0-1：驻留字面量
    code.push(ASTORE_1); // 2
    code.extend_from_slice(&op3(NEW, new_string)); // 3-5
    code.push(DUP); // 6
    code.extend_from_slice(&op3(LDC_W, hi)); // 7-8
    code.extend_from_slice(&op3(INVOKESPECIAL, init)); // 9-11
    code.push(ASTORE_2); // 12
    code.push(ALOAD_1); // 13
    code.extend_from_slice(&op3(INVOKEVIRTUAL, intern)); // 14-16
    code.push(ALOAD_2); // 17
    code.extend_from_slice(&op3(INVOKEVIRTUAL, intern)); // 18-20
    code.extend_from_slice(&branch(IF_ACMPEQ, 3)); // 21-23 → 24
    code.push(ICONST_1); // 24：相等
    code.push(IRETURN); // 25
    code.push(ICONST_0); // 26：不等（fallthrough）
    code.push(IRETURN); // 27
    assert_eq!(run_main(&mut pool, "()I", 4, 3, code), 1);
}

#[test]
fn string_value_of_int() {
    let mut pool = Pool::new();
    let value = pool.integer(12345);
    let expect = pool.string("12345");
    let value_of = pool.methodref("java/lang/String", "valueOf", "(I)Ljava/lang/String;");
    let equals = pool.methodref("java/lang/String", "equals", "(Ljava/lang/Object;)Z");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, value));
    code.extend_from_slice(&op3(INVOKESTATIC, value_of));
    code.extend_from_slice(&op3(LDC_W, expect));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 4, 1, code), 1);
}

// ---- StringBuffer ----------------------------------------------------------

#[test]
fn sb_append_chain_to_string() {
    // new StringBuffer().append("a").append(1).append(true) → "a1true"
    let mut pool = Pool::new();
    let a = pool.string("a");
    let expect = pool.string("a1true");
    let new_sb = pool.class("java/lang/StringBuffer");
    let init = pool.methodref("java/lang/StringBuffer", "<init>", "()V");
    let append_str = pool.methodref("java/lang/StringBuffer", "append", "(Ljava/lang/String;)Ljava/lang/StringBuffer;");
    let append_int = pool.methodref("java/lang/StringBuffer", "append", "(I)Ljava/lang/StringBuffer;");
    let append_bool = pool.methodref("java/lang/StringBuffer", "append", "(Z)Ljava/lang/StringBuffer;");
    let to_string = pool.methodref("java/lang/StringBuffer", "toString", "()Ljava/lang/String;");
    let equals = pool.methodref("java/lang/String", "equals", "(Ljava/lang/Object;)Z");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(NEW, new_sb)); // 0-2
    code.push(DUP); // 3
    code.extend_from_slice(&op3(INVOKESPECIAL, init)); // 4-6
    code.extend_from_slice(&op3(LDC_W, a)); // 7-8
    code.extend_from_slice(&op3(INVOKEVIRTUAL, append_str)); // 9-11
    code.push(BIPUSH); // 12-13
    code.push(1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, append_int)); // 14-16
    code.push(ICONST_1); // 17
    code.extend_from_slice(&op3(INVOKEVIRTUAL, append_bool)); // 18-20
    code.extend_from_slice(&op3(INVOKEVIRTUAL, to_string)); // 21-23
    code.extend_from_slice(&op3(LDC_W, expect)); // 24-25
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals)); // 26-28
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 1, code), 1);
}

#[test]
fn sb_append_null_string_is_literal_null() {
    // JDK 语义：append((String) null) 追加 "null" 字面量而非抛 NPE
    //（gjxjwql 关于页实证：sb.append(桌面快捷方式名) 非桌面模式传 null）。
    let mut pool = Pool::new();
    let pre = pool.string("x-");
    let expect = pool.string("x-null");
    let new_sb = pool.class("java/lang/StringBuffer");
    let init = pool.methodref("java/lang/StringBuffer", "<init>", "()V");
    let append_str = pool.methodref("java/lang/StringBuffer", "append", "(Ljava/lang/String;)Ljava/lang/StringBuffer;");
    let to_string = pool.methodref("java/lang/StringBuffer", "toString", "()Ljava/lang/String;");
    let equals = pool.methodref("java/lang/String", "equals", "(Ljava/lang/Object;)Z");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(NEW, new_sb)); // 0-2
    code.push(DUP); // 3
    code.extend_from_slice(&op3(INVOKESPECIAL, init)); // 4-6
    code.extend_from_slice(&op3(LDC_W, pre)); // 7-8
    code.extend_from_slice(&op3(INVOKEVIRTUAL, append_str)); // 9-11
    code.push(ACONST_NULL); // 12
    code.extend_from_slice(&op3(INVOKEVIRTUAL, append_str)); // 13-15
    code.extend_from_slice(&op3(INVOKEVIRTUAL, to_string)); // 16-18
    code.extend_from_slice(&op3(LDC_W, expect)); // 19-20
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals)); // 21-23
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 1, code), 1);
}

#[test]
fn sb_insert_delete_reverse() {
    // new SB("abc").insert(1,"XY").delete(1,3).reverse() → "cba"
    let mut pool = Pool::new();
    let abc = pool.string("abc");
    let xy = pool.string("XY");
    let expect = pool.string("cba");
    let new_sb = pool.class("java/lang/StringBuffer");
    let init = pool.methodref("java/lang/StringBuffer", "<init>", "(Ljava/lang/String;)V");
    let insert = pool.methodref("java/lang/StringBuffer", "insert", "(ILjava/lang/String;)Ljava/lang/StringBuffer;");
    let delete = pool.methodref("java/lang/StringBuffer", "delete", "(II)Ljava/lang/StringBuffer;");
    let reverse = pool.methodref("java/lang/StringBuffer", "reverse", "()Ljava/lang/StringBuffer;");
    let to_string = pool.methodref("java/lang/StringBuffer", "toString", "()Ljava/lang/String;");
    let equals = pool.methodref("java/lang/String", "equals", "(Ljava/lang/Object;)Z");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(NEW, new_sb)); // 0-2
    code.push(DUP); // 3
    code.extend_from_slice(&op3(LDC_W, abc)); // 4-5
    code.extend_from_slice(&op3(INVOKESPECIAL, init)); // 6-8
    code.push(ICONST_1); // 9
    code.extend_from_slice(&op3(LDC_W, xy)); // 10-11
    code.extend_from_slice(&op3(INVOKEVIRTUAL, insert)); // 12-14 → "aXYbc"
    code.push(ICONST_1); // 15
    code.push(ICONST_3); // 16
    code.extend_from_slice(&op3(INVOKEVIRTUAL, delete)); // 17-19 → "abc"
    code.extend_from_slice(&op3(INVOKEVIRTUAL, reverse)); // 20-22 → "cba"
    code.extend_from_slice(&op3(INVOKEVIRTUAL, to_string)); // 23-25
    code.extend_from_slice(&op3(LDC_W, expect)); // 26-27
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals)); // 28-30
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 1, code), 1);
}

#[test]
fn sb_char_at_and_set_length() {
    // new SB("abc")：charAt(1)*1000 + setLength(5) 后 length() = 98*1000 + 5
    let mut pool = Pool::new();
    let abc = pool.string("abc");
    let new_sb = pool.class("java/lang/StringBuffer");
    let init = pool.methodref("java/lang/StringBuffer", "<init>", "(Ljava/lang/String;)V");
    let char_at = pool.methodref("java/lang/StringBuffer", "charAt", "(I)C");
    let set_length = pool.methodref("java/lang/StringBuffer", "setLength", "(I)V");
    let length = pool.methodref("java/lang/StringBuffer", "length", "()I");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(NEW, new_sb)); // 0-2
    code.push(DUP); // 3
    code.extend_from_slice(&op3(LDC_W, abc)); // 4-5
    code.extend_from_slice(&op3(INVOKESPECIAL, init)); // 6-8
    code.push(ASTORE_1); // 9
    code.push(ALOAD_1); // 10
    code.push(ICONST_1); // 11
    code.extend_from_slice(&op3(INVOKEVIRTUAL, char_at)); // 12-14 → 98
    code.push(SIPUSH); // 15-17
    code.extend_from_slice(&1000i16.to_be_bytes());
    code.push(IMUL); // 18 → 98000
    code.push(ALOAD_1); // 19
    code.push(ICONST_5); // 20
    code.extend_from_slice(&op3(INVOKEVIRTUAL, set_length)); // 21-23
    code.push(ALOAD_1); // 24
    code.extend_from_slice(&op3(INVOKEVIRTUAL, length)); // 25-27 → 5
    code.push(IADD); // 28
    code.push(IRETURN); // 29
    assert_eq!(run_main(&mut pool, "()I", 4, 2, code), 98 * 1000 + 5);
}

#[test]
fn sb_set_length_pads_with_nul() {
    // new SB("ab").setLength(3) 后 charAt(2) == '\0'。
    let mut pool = Pool::new();
    let ab = pool.string("ab");
    let new_sb = pool.class("java/lang/StringBuffer");
    let init = pool.methodref("java/lang/StringBuffer", "<init>", "(Ljava/lang/String;)V");
    let set_length = pool.methodref("java/lang/StringBuffer", "setLength", "(I)V");
    let char_at = pool.methodref("java/lang/StringBuffer", "charAt", "(I)C");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(NEW, new_sb));
    code.push(DUP);
    code.extend_from_slice(&op3(LDC_W, ab));
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code.push(DUP); // 保留接收者：setLength 消耗 this，后续 charAt 仍需
    code.push(ICONST_3);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, set_length));
    code.push(ICONST_2);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, char_at));
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 4, 1, code), 0);
}

// ---- Math ------------------------------------------------------------------

#[test]
fn math_abs_min_max() {
    // abs(-5)*100 + min(2,3)*10 + max(2,3) = 500 + 20 + 3
    let mut pool = Pool::new();
    let minus5 = pool.integer(-5);
    let abs = pool.methodref("java/lang/Math", "abs", "(I)I");
    let min = pool.methodref("java/lang/Math", "min", "(II)I");
    let max = pool.methodref("java/lang/Math", "max", "(II)I");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, minus5));
    code.extend_from_slice(&op3(INVOKESTATIC, abs));
    code.push(BIPUSH);
    code.push(100);
    code.push(IMUL);
    code.push(ICONST_2);
    code.push(ICONST_3);
    code.extend_from_slice(&op3(INVOKESTATIC, min));
    code.push(BIPUSH);
    code.push(10);
    code.push(IMUL);
    code.push(IADD);
    code.push(ICONST_2);
    code.push(ICONST_3);
    code.extend_from_slice(&op3(INVOKESTATIC, max));
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 4, 1, code), 523);
}

#[test]
fn math_abs_int_min_wraps() {
    // JVM：abs(Integer.MIN_VALUE) == MIN_VALUE（负溢出回绕）。
    let mut pool = Pool::new();
    let min = pool.integer(i32::MIN);
    let abs = pool.methodref("java/lang/Math", "abs", "(I)I");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, min));
    code.extend_from_slice(&op3(INVOKESTATIC, abs));
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 2, 1, code), i32::MIN);
}

#[test]
fn math_round_semantics() {
    // round(-0.5)=0、round(0.5)=1（向正无穷取整）、round(2.5f)=3 → 0*100+1*10+3
    let mut pool = Pool::new();
    let minus_half = pool.double(-0.5);
    let half = pool.double(0.5);
    let two_half_f = pool.float(2.5);
    let round_d = pool.methodref("java/lang/Math", "round", "(D)J");
    let round_f = pool.methodref("java/lang/Math", "round", "(F)I");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC2_W, minus_half));
    code.extend_from_slice(&op3(INVOKESTATIC, round_d));
    code.push(L2I);
    code.push(BIPUSH);
    code.push(100);
    code.push(IMUL);
    code.extend_from_slice(&op3(LDC2_W, half));
    code.extend_from_slice(&op3(INVOKESTATIC, round_d));
    code.push(L2I);
    code.push(BIPUSH);
    code.push(10);
    code.push(IMUL);
    code.push(IADD);
    code.extend_from_slice(&op3(LDC_W, two_half_f));
    code.extend_from_slice(&op3(INVOKESTATIC, round_f));
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 6, 1, code), 13);
}

#[test]
fn math_floor_ceil_sqrt_pow() {
    // floor(-1.5)+ceil(-1.5)+sqrt(9)+pow(2,10) = -2 + -1 + 3 + 1024 = 1024
    let mut pool = Pool::new();
    let minus = pool.double(-1.5);
    let nine = pool.double(9.0);
    let two = pool.double(2.0);
    let ten = pool.double(10.0);
    let floor = pool.methodref("java/lang/Math", "floor", "(D)D");
    let ceil = pool.methodref("java/lang/Math", "ceil", "(D)D");
    let sqrt = pool.methodref("java/lang/Math", "sqrt", "(D)D");
    let pow = pool.methodref("java/lang/Math", "pow", "(DD)D");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC2_W, minus));
    code.extend_from_slice(&op3(INVOKESTATIC, floor));
    code.extend_from_slice(&op3(LDC2_W, minus));
    code.extend_from_slice(&op3(INVOKESTATIC, ceil));
    code.push(0x63); // dadd
    code.extend_from_slice(&op3(LDC2_W, nine));
    code.extend_from_slice(&op3(INVOKESTATIC, sqrt));
    code.push(0x63); // dadd
    code.extend_from_slice(&op3(LDC2_W, two));
    code.extend_from_slice(&op3(LDC2_W, ten));
    code.extend_from_slice(&op3(INVOKESTATIC, pow));
    code.push(0x63); // dadd
    code.push(DRETURN);
    assert_eq!(expect_double(run_main_outcome(&mut pool, "()D", 6, 1, code)), 1024.0);
}

#[test]
fn math_pi_static_constant() {
    // getstatic Math.PI：bootstrap 常量在布局期写入静态槽。
    let mut pool = Pool::new();
    let pi = pool.fieldref("java/lang/Math", "PI", "D");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(GETSTATIC, pi));
    code.push(DRETURN);
    let value = expect_double(run_main_outcome(&mut pool, "()D", 2, 1, code));
    assert!((value - std::f64::consts::PI).abs() < 1e-12);
}

#[test]
fn math_min_with_nan_yields_nan() {
    // JVM：min(1.0, NaN) == NaN。
    let mut pool = Pool::new();
    let min = pool.methodref("java/lang/Math", "min", "(DD)D");
    let mut code = vec![DCONST_1];
    code.push(DCONST_0);
    code.push(DCONST_0);
    code.push(DDIV); // 0.0/0.0 → NaN
    code.extend_from_slice(&op3(INVOKESTATIC, min));
    code.push(DRETURN);
    assert!(expect_double(run_main_outcome(&mut pool, "()D", 6, 1, code)).is_nan());
}

// ---- System ----------------------------------------------------------------

#[test]
fn system_arraycopy_handles_overlap() {
    // int[5]{1..5}，arraycopy(a,0,a,1,4) → {1,1,2,3,4}；a[4]*100+a[1] = 401。
    let mut pool = Pool::new();
    let arraycopy = pool.methodref(
        "java/lang/System",
        "arraycopy",
        "(Ljava/lang/Object;ILjava/lang/Object;II)V",
    );
    let mut code = vec![ICONST_5]; // 0
    code.push(NEWARRAY); // 1-2
    code.push(T_INT);
    code.push(ASTORE_1); // 3
    for index in 0u8..5 {
        code.push(ALOAD_1);
        code.push(ICONST_0 + index); // 下标 i
        code.push(ICONST_0 + index + 1); // a[i] = i + 1
        code.push(IASTORE);
    } // 4-23
    code.push(ALOAD_1); // 24
    code.push(ICONST_0); // 25
    code.push(ALOAD_1); // 26
    code.push(ICONST_1); // 27
    code.push(ICONST_4); // 28
    code.extend_from_slice(&op3(INVOKESTATIC, arraycopy)); // 29-31
    code.push(ALOAD_1); // 32
    code.push(ICONST_4); // 33
    code.push(IALOAD); // 34 → 4
    code.push(BIPUSH); // 35-36
    code.push(100);
    code.push(IMUL); // 37 → 400
    code.push(ALOAD_1); // 38
    code.push(ICONST_1); // 39
    code.push(IALOAD); // 40 → 1
    code.push(IADD); // 41
    code.push(IRETURN); // 42
    assert_eq!(run_main(&mut pool, "()I", 6, 2, code), 401);
}

#[test]
fn system_arraycopy_type_mismatch_throws() {
    // int[] → char[]：ArrayStoreException。
    let mut pool = Pool::new();
    let arraycopy = pool.methodref(
        "java/lang/System",
        "arraycopy",
        "(Ljava/lang/Object;ILjava/lang/Object;II)V",
    );
    let mut code = vec![ICONST_3]; // 0
    code.push(NEWARRAY); // 1-2
    code.push(T_INT);
    code.push(ASTORE_1); // 3
    code.push(ICONST_3); // 4
    code.push(NEWARRAY); // 5-6
    code.push(T_CHAR);
    code.push(ASTORE_2); // 7
    code.push(ALOAD_1); // 8
    code.push(ICONST_0); // 9
    code.push(ALOAD_2); // 10
    code.push(ICONST_0); // 11
    code.push(ICONST_2); // 12
    code.extend_from_slice(&op3(INVOKESTATIC, arraycopy)); // 13-15
    code.push(ICONST_0); // 16
    code.extend_from_slice(&branch(GOTO, 4)); // 17-19 → 21
    code.push(ICONST_1); // 20
    code.push(IRETURN); // 21
    let bytes = build_class(
        &mut pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_catch("()I", 6, 3, code, vec![handler(0, 16, 20, Some("java/lang/ArrayStoreException"))])],
    );
    let mut vm = Vm::new(vec![("T", bytes)]);
    assert_eq!(expect_int(vm.exec("()I").unwrap()), 1);
}

#[test]
fn system_arraycopy_out_of_bounds_throws() {
    // length 越过数组尾：ArrayIndexOutOfBoundsException。
    let mut pool = Pool::new();
    let arraycopy = pool.methodref(
        "java/lang/System",
        "arraycopy",
        "(Ljava/lang/Object;ILjava/lang/Object;II)V",
    );
    let mut code = vec![ICONST_3]; // 0
    code.push(NEWARRAY); // 1-2
    code.push(T_INT);
    code.push(ASTORE_1); // 3
    code.push(ALOAD_1); // 4
    code.push(ICONST_0); // 5
    code.push(ALOAD_1); // 6
    code.push(ICONST_0); // 7
    code.push(BIPUSH); // 8-9
    code.push(10);
    code.extend_from_slice(&op3(INVOKESTATIC, arraycopy)); // 10-12
    code.push(ICONST_0); // 13
    code.extend_from_slice(&branch(GOTO, 4)); // 14-16 → 18
    code.push(ICONST_1); // 17
    code.push(IRETURN); // 18
    let bytes = build_class(
        &mut pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_catch(
            "()I",
            6,
            2,
            code,
            vec![handler(
                0,
                13,
                17,
                Some("java/lang/ArrayIndexOutOfBoundsException"),
            )],
        )],
    );
    let mut vm = Vm::new(vec![("T", bytes)]);
    assert_eq!(expect_int(vm.exec("()I").unwrap()), 1);
}

#[test]
fn system_get_property_known_key() {
    // CLDC 固定键 microedition.platform 非空。
    let mut pool = Pool::new();
    let key = pool.string("microedition.platform");
    let get_property = pool.methodref(
        "java/lang/System",
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
    );
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, key)); // 0-1
    code.extend_from_slice(&op3(INVOKESTATIC, get_property)); // 2-4
    code.extend_from_slice(&branch(IFNONNULL, 7)); // 5-7 → 12
    code.push(ICONST_0); // 8：null
    code.extend_from_slice(&branch(GOTO, 4)); // 9-11 → 13
    code.push(ICONST_1); // 12：非空
    code.push(IRETURN); // 13
    assert_eq!(run_main(&mut pool, "()I", 3, 1, code), 1);
}

#[test]
fn system_get_property_unknown_key_is_null() {
    let mut pool = Pool::new();
    let key = pool.string("no.such.key");
    let get_property = pool.methodref(
        "java/lang/System",
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
    );
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, key)); // 0-1
    code.extend_from_slice(&op3(INVOKESTATIC, get_property)); // 2-4
    code.extend_from_slice(&branch(IFNULL, 7)); // 5-7 → 12
    code.push(ICONST_0); // 8：非空
    code.extend_from_slice(&branch(GOTO, 4)); // 9-11 → 13
    code.push(ICONST_1); // 12：null
    code.push(IRETURN); // 13
    assert_eq!(run_main(&mut pool, "()I", 3, 1, code), 1);
}

#[test]
fn system_current_time_millis_host_clock() {
    let mut pool = Pool::new();
    let millis = pool.methodref("java/lang/System", "currentTimeMillis", "()J");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(INVOKESTATIC, millis));
    code.push(LRETURN);
    // T034 改宿主轨：与 sleep/Timer 同源（mock 时钟 0 起步，虚拟时间
    // 可驱动超时轮询——51mrp splash 切换的实证关键）。本 harness 不
    // 推进时钟，故断言非负即可；时序真值面由 thread_semantics 的
    // Timer/wait 用例与 smoke_boot 的画面切换断言覆盖。
    assert!(expect_long(run_main_outcome(&mut pool, "()J", 2, 1, code)) >= 0);
}

#[test]
fn system_exit_enqueues_forced_destroy_request() {
    // T045：exit(0) 不再以内部错误上抛——置生命周期请求位
    // （DestroyApp{unconditional:true}）后正常返回，Vm.poll 消费段
    // 走 destroyApp 回调链收尾（生命周期语义真值面由
    // lifecycle_semantics 的 System.exit 用例覆盖）。
    let mut pool = Pool::new();
    let exit = pool.methodref("java/lang/System", "exit", "(I)V");
    let mut code = Vec::new();
    code.push(ICONST_0);
    code.extend_from_slice(&op3(INVOKESTATIC, exit));
    code.push(ICONST_0);
    code.push(IRETURN);
    let bytes = build_class(
        &mut pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_code("()I", 2, 1, code)],
    );
    let mut vm = Vm::new(vec![("T", bytes)]);
    match vm.exec("()I") {
        Ok(RunOutcome::Returned(Some(Slot::Int(0)))) => {}
        other => panic!("exit 后应正常返回 0，得到 {other:?}"),
    }
    assert_eq!(
        vm.cs.pending_lifecycle,
        vec![skyengine_kvm::runtime::classspace::MidletLifecycleOp::DestroyApp {
            unconditional: true
        }],
        "exit 应置强制销毁请求"
    );
}

// ---- 包装类 ----------------------------------------------------------------

#[test]
fn integer_parse_int_and_radix() {
    // parseInt("-42")*100 + parseInt("12",16) = -4200 + 18
    let mut pool = Pool::new();
    let minus = pool.string("-42");
    let hex = pool.string("12");
    let parse = pool.methodref("java/lang/Integer", "parseInt", "(Ljava/lang/String;)I");
    let parse_radix = pool.methodref("java/lang/Integer", "parseInt", "(Ljava/lang/String;I)I");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, minus));
    code.extend_from_slice(&op3(INVOKESTATIC, parse));
    code.push(BIPUSH);
    code.push(100);
    code.push(IMUL);
    code.extend_from_slice(&op3(LDC_W, hex));
    code.push(BIPUSH);
    code.push(16);
    code.extend_from_slice(&op3(INVOKESTATIC, parse_radix));
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 4, 1, code), -4182);
}

#[test]
fn integer_parse_bad_input_throws_nfe() {
    let mut pool = Pool::new();
    let bad = pool.string("abc");
    let parse = pool.methodref("java/lang/Integer", "parseInt", "(Ljava/lang/String;)I");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, bad)); // 0-2
    code.extend_from_slice(&op3(INVOKESTATIC, parse)); // 3-5
    code.push(POP); // 6
    code.push(ICONST_0); // 7
    code.extend_from_slice(&branch(GOTO, 4)); // 8-10 → 12
    code.push(ICONST_1); // 11
    code.push(IRETURN); // 12
    let bytes = build_class(
        &mut pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_catch("()I", 3, 1, code, vec![handler(0, 6, 11, Some("java/lang/NumberFormatException"))])],
    );
    let mut vm = Vm::new(vec![("T", bytes)]);
    assert_eq!(expect_int(vm.exec("()I").unwrap()), 1);
}

#[test]
fn integer_hex_and_radix_text() {
    // toHexString(-1)=="ffffffff" *10 + toString(255,16)=="ff" → 11
    let mut pool = Pool::new();
    let minus1 = pool.integer(-1);
    let hex_expect = pool.string("ffffffff");
    let ff = pool.string("ff");
    let to_hex = pool.methodref("java/lang/Integer", "toHexString", "(I)Ljava/lang/String;");
    let to_string_radix = pool.methodref("java/lang/Integer", "toString", "(II)Ljava/lang/String;");
    let equals = pool.methodref("java/lang/String", "equals", "(Ljava/lang/Object;)Z");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, minus1));
    code.extend_from_slice(&op3(INVOKESTATIC, to_hex));
    code.extend_from_slice(&op3(LDC_W, hex_expect));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.push(BIPUSH);
    code.push(10);
    code.push(IMUL);
    code.push(SIPUSH);
    code.extend_from_slice(&255i16.to_be_bytes());
    code.push(BIPUSH);
    code.push(16);
    code.extend_from_slice(&op3(INVOKESTATIC, to_string_radix));
    code.extend_from_slice(&op3(LDC_W, ff));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 1, code), 11);
}

#[test]
fn integer_wrapper_roundtrip() {
    // new Integer(7).intValue()*10 + Integer.valueOf(9).intValue() = 79
    let mut pool = Pool::new();
    let new_integer = pool.class("java/lang/Integer");
    let init = pool.methodref("java/lang/Integer", "<init>", "(I)V");
    let int_value = pool.methodref("java/lang/Integer", "intValue", "()I");
    let value_of = pool.methodref("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(NEW, new_integer));
    code.push(DUP);
    code.push(BIPUSH);
    code.push(7);
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, int_value));
    code.push(BIPUSH);
    code.push(10);
    code.push(IMUL);
    code.push(BIPUSH);
    code.push(9);
    code.extend_from_slice(&op3(INVOKESTATIC, value_of));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, int_value));
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 4, 1, code), 79);
}

#[test]
fn long_wrapper_roundtrip() {
    let mut pool = Pool::new();
    let big = pool.long(123456789012345);
    let new_long = pool.class("java/lang/Long");
    let init = pool.methodref("java/lang/Long", "<init>", "(J)V");
    let long_value = pool.methodref("java/lang/Long", "longValue", "()J");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(NEW, new_long));
    code.push(DUP);
    code.extend_from_slice(&op3(LDC2_W, big));
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, long_value));
    code.push(LRETURN);
    assert_eq!(expect_long(run_main_outcome(&mut pool, "()J", 4, 1, code)), 123456789012345);
}

#[test]
fn double_and_float_wrappers() {
    // new Double(2.5).doubleValue() + Float.parseFloat("1.5") = 4.0
    let mut pool = Pool::new();
    let two_five = pool.double(2.5);
    let one_five = pool.string("1.5");
    let new_double = pool.class("java/lang/Double");
    let init = pool.methodref("java/lang/Double", "<init>", "(D)V");
    let double_value = pool.methodref("java/lang/Double", "doubleValue", "()D");
    let parse_float = pool.methodref("java/lang/Float", "parseFloat", "(Ljava/lang/String;)F");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(NEW, new_double));
    code.push(DUP);
    code.extend_from_slice(&op3(LDC2_W, two_five));
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, double_value));
    code.extend_from_slice(&op3(LDC_W, one_five));
    code.extend_from_slice(&op3(INVOKESTATIC, parse_float));
    code.push(F2D);
    code.push(0x63); // dadd
    code.push(DRETURN);
    assert_eq!(expect_double(run_main_outcome(&mut pool, "()D", 6, 1, code)), 4.0);
}

#[test]
fn character_classification_and_digit() {
    // isDigit('5')*100 + isLetter('a')*10 + digit('f',16) = 100+10+15
    let mut pool = Pool::new();
    let is_digit = pool.methodref("java/lang/Character", "isDigit", "(C)Z");
    let is_letter = pool.methodref("java/lang/Character", "isLetter", "(C)Z");
    let digit = pool.methodref("java/lang/Character", "digit", "(CI)I");
    let mut code = Vec::new();
    code.push(BIPUSH);
    code.push(b'5');
    code.extend_from_slice(&op3(INVOKESTATIC, is_digit));
    code.push(BIPUSH);
    code.push(100);
    code.push(IMUL);
    code.push(BIPUSH);
    code.push(b'a');
    code.extend_from_slice(&op3(INVOKESTATIC, is_letter));
    code.push(BIPUSH);
    code.push(10);
    code.push(IMUL);
    code.push(IADD);
    code.push(BIPUSH);
    code.push(b'f');
    code.push(BIPUSH);
    code.push(16);
    code.extend_from_slice(&op3(INVOKESTATIC, digit));
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 4, 1, code), 125);
}

#[test]
fn character_to_upper_case() {
    let mut pool = Pool::new();
    let to_upper = pool.methodref("java/lang/Character", "toUpperCase", "(C)C");
    let mut code = Vec::new();
    code.push(BIPUSH);
    code.push(b'a');
    code.extend_from_slice(&op3(INVOKESTATIC, to_upper));
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 2, 1, code), b'A' as i32);
}

#[test]
fn boolean_wrapper_roundtrip() {
    // new Boolean(true).booleanValue()*10 + Boolean.valueOf(false).toString()=="false"
    let mut pool = Pool::new();
    let expect = pool.string("false");
    let new_boolean = pool.class("java/lang/Boolean");
    let init = pool.methodref("java/lang/Boolean", "<init>", "(Z)V");
    let bool_value = pool.methodref("java/lang/Boolean", "booleanValue", "()Z");
    let value_of = pool.methodref("java/lang/Boolean", "valueOf", "(Z)Ljava/lang/Boolean;");
    let to_string = pool.methodref("java/lang/Boolean", "toString", "()Ljava/lang/String;");
    let equals = pool.methodref("java/lang/String", "equals", "(Ljava/lang/Object;)Z");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(NEW, new_boolean));
    code.push(DUP);
    code.push(ICONST_1);
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, bool_value));
    code.push(BIPUSH);
    code.push(10);
    code.push(IMUL);
    code.push(ICONST_0);
    code.extend_from_slice(&op3(INVOKESTATIC, value_of));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, to_string));
    code.extend_from_slice(&op3(LDC_W, expect));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 1, code), 11);
}

// ---- Throwable / Class / Object --------------------------------------------

#[test]
fn throwable_message_and_to_string() {
    // new IllegalStateException("boom")：getMessage 与 toString（点分类名）。
    let mut pool = Pool::new();
    let boom = pool.string("boom");
    let expect_to_string = pool.string("java.lang.IllegalStateException: boom");
    let new_state = pool.class("java/lang/IllegalStateException");
    let init = pool.methodref("java/lang/IllegalStateException", "<init>", "(Ljava/lang/String;)V");
    let get_message = pool.methodref("java/lang/Throwable", "getMessage", "()Ljava/lang/String;");
    let to_string = pool.methodref("java/lang/Throwable", "toString", "()Ljava/lang/String;");
    let equals = pool.methodref("java/lang/String", "equals", "(Ljava/lang/Object;)Z");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(NEW, new_state)); // 0-2
    code.push(DUP); // 3
    code.extend_from_slice(&op3(LDC_W, boom)); // 4-5
    code.extend_from_slice(&op3(INVOKESPECIAL, init)); // 6-8
    code.push(ASTORE_1); // 9
    code.push(ALOAD_1); // 10
    code.extend_from_slice(&op3(INVOKEVIRTUAL, get_message)); // 11-13
    code.extend_from_slice(&op3(LDC_W, boom)); // 14-15
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals)); // 16-18 → 1
    code.push(BIPUSH); // 19-20
    code.push(10);
    code.push(IMUL); // 21 → 10
    code.push(ALOAD_1); // 22
    code.extend_from_slice(&op3(INVOKEVIRTUAL, to_string)); // 23-25
    code.extend_from_slice(&op3(LDC_W, expect_to_string)); // 26-27
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals)); // 28-30 → 1
    code.push(IADD); // 31
    code.push(IRETURN); // 32
    assert_eq!(run_main(&mut pool, "()I", 5, 2, code), 11);
}

#[test]
fn class_for_name_and_is_instance() {
    // Class.forName("java.lang.String")：isInstance("x")=1、isInstance(null)=0。
    let mut pool = Pool::new();
    let name = pool.string("java.lang.String");
    let x = pool.string("x");
    let for_name = pool.methodref("java/lang/Class", "forName", "(Ljava/lang/String;)Ljava/lang/Class;");
    let is_instance = pool.methodref("java/lang/Class", "isInstance", "(Ljava/lang/Object;)Z");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, name)); // 0-1
    code.extend_from_slice(&op3(INVOKESTATIC, for_name)); // 2-4
    code.push(ASTORE_1); // 5
    code.push(ALOAD_1); // 6
    code.extend_from_slice(&op3(LDC_W, x)); // 7-8
    code.extend_from_slice(&op3(INVOKEVIRTUAL, is_instance)); // 9-11 → 1
    code.push(ISTORE_2); // 12
    code.push(ALOAD_1); // 13
    code.push(ACONST_NULL); // 14
    code.extend_from_slice(&op3(INVOKEVIRTUAL, is_instance)); // 15-17 → 0
    code.push(ILOAD_2); // 18
    code.push(IADD); // 19 → 1
    code.push(IRETURN); // 20
    assert_eq!(run_main(&mut pool, "()I", 4, 3, code), 1);
}

#[test]
fn class_is_assignable_from() {
    // Object.class.isAssignableFrom(String.class) → 1（父链方向）。
    let mut pool = Pool::new();
    let object_name = pool.string("java.lang.Object");
    let string_name = pool.string("java.lang.String");
    let for_name = pool.methodref("java/lang/Class", "forName", "(Ljava/lang/String;)Ljava/lang/Class;");
    let assignable = pool.methodref("java/lang/Class", "isAssignableFrom", "(Ljava/lang/Class;)Z");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, object_name));
    code.extend_from_slice(&op3(INVOKESTATIC, for_name));
    code.push(ASTORE_1);
    code.extend_from_slice(&op3(LDC_W, string_name));
    code.extend_from_slice(&op3(INVOKESTATIC, for_name));
    code.push(ASTORE_2);
    code.push(ALOAD_1);
    code.push(ALOAD_2);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, assignable));
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 4, 3, code), 1);
}

#[test]
fn class_for_name_missing_throws_cnfe() {
    let mut pool = Pool::new();
    let missing = pool.string("no.such.Cls");
    let for_name = pool.methodref("java/lang/Class", "forName", "(Ljava/lang/String;)Ljava/lang/Class;");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, missing)); // 0-2
    code.extend_from_slice(&op3(INVOKESTATIC, for_name)); // 3-5
    code.push(POP); // 6
    code.push(ICONST_0); // 7
    code.extend_from_slice(&branch(GOTO, 4)); // 8-10 → 12
    code.push(ICONST_1); // 11
    code.push(IRETURN); // 12
    let bytes = build_class(
        &mut pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_catch(
            "()I",
            3,
            1,
            code,
            vec![handler(0, 6, 11, Some("java/lang/ClassNotFoundException"))],
        )],
    );
    let mut vm = Vm::new(vec![("T", bytes)]);
    assert_eq!(expect_int(vm.exec("()I").unwrap()), 1);
}

#[test]
fn object_get_class_name() {
    // new Object().getClass().getName() == "java.lang.Object"（点分）。
    let mut pool = Pool::new();
    let expect = pool.string("java.lang.Object");
    let new_object = pool.class("java/lang/Object");
    let init = pool.methodref("java/lang/Object", "<init>", "()V");
    let get_class = pool.methodref("java/lang/Object", "getClass", "()Ljava/lang/Class;");
    let get_name = pool.methodref("java/lang/Class", "getName", "()Ljava/lang/String;");
    let equals = pool.methodref("java/lang/String", "equals", "(Ljava/lang/Object;)Z");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(NEW, new_object));
    code.push(DUP);
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, get_class));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, get_name));
    code.extend_from_slice(&op3(LDC_W, expect));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 4, 1, code), 1);
}

// ---- java.io（T016） ---------------------------------------------------------

/// 双字节操作数指令（aload/astore/iload/istore/iinc 前两形态）。
fn op2(op: u8, index: u8) -> [u8; 2] {
    [op, index]
}

/// NEW + DUP + INVOKESPECIAL `<init>`（实参须已压在栈上）。
fn new_init(class: u16, init: u16) -> Vec<u8> {
    let mut code = op3(NEW, class).to_vec();
    code.push(DUP);
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code
}

/// 带参构造（javac 形态）：NEW + DUP + <参数码> + INVOKESPECIAL。
/// 参数码压参后参数在栈顶、receiver 副本在其下，调用后留构造好的引用。
/// jc.jar 实证：真字节码 525 处 new+dup、0 处 new+dup_x1 嵌套构造。
fn new_init_args(class: u16, init: u16, arg_code: &[u8]) -> Vec<u8> {
    let mut code = op3(NEW, class).to_vec();
    code.push(DUP);
    code.extend_from_slice(arg_code);
    code.extend_from_slice(&op3(INVOKESPECIAL, init));
    code
}

/// 构造并填充 `new byte[N]`（栈上留数组）。
fn push_bytes(values: &[u8]) -> Vec<u8> {
    let mut code = Vec::new();
    push_i32(&mut code, values.len() as i32);
    code.push(NEWARRAY);
    code.push(T_BYTE);
    for (index, value) in values.iter().enumerate() {
        code.push(DUP);
        push_i32(&mut code, index as i32);
        push_i32(&mut code, *value as i8 as i32);
        code.push(BASTORE);
    }
    code
}

/// 执行 main 并取 String 返回文本。
fn run_main_text(
    pool: &mut Pool,
    desc: &'static str,
    stack: u16,
    locals: u16,
    code: Vec<u8>,
) -> String {
    let bytes = build_class(
        pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_code(desc, stack, locals, code)],
    );
    let mut vm = Vm::new(vec![("T", bytes)]);
    match vm.exec(desc).expect("run main") {
        RunOutcome::Returned(Some(Slot::Ref(reference))) => str_text(&vm.heap, reference),
        other => panic!("expected String return, got {other:?}"),
    }
}

/// 执行 main 并断言未捕获异常，返回异常类内部名。
fn run_main_thrown(
    pool: &mut Pool,
    desc: &'static str,
    stack: u16,
    locals: u16,
    code: Vec<u8>,
) -> String {
    let bytes = build_class(
        pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_code(desc, stack, locals, code)],
    );
    let mut vm = Vm::new(vec![("T", bytes)]);
    let outcome = vm.exec(desc).expect("run main");
    let RunOutcome::Uncaught(Unwind::Thrown(exception)) = outcome else {
        panic!("expected uncaught Thrown, got {outcome:?}")
    };
    let class = runtime_class_of(&vm.cs, &vm.heap, exception).expect("exception class");
    vm.cs.name_of(class).to_string()
}

#[test]
fn dos_write_int_read_int_roundtrip() {
    // BAOS → DOS.writeInt(0x12345678) → toByteArray → BAIS → DIS.readInt。
    let mut pool = Pool::new();
    let baos_c = pool.class("java/io/ByteArrayOutputStream");
    let baos_i = pool.methodref("java/io/ByteArrayOutputStream", "<init>", "()V");
    let dos_c = pool.class("java/io/DataOutputStream");
    let dos_i = pool.methodref("java/io/DataOutputStream", "<init>", "(Ljava/io/OutputStream;)V");
    let write_int = pool.methodref("java/io/DataOutputStream", "writeInt", "(I)V");
    let to_bytes = pool.methodref("java/io/ByteArrayOutputStream", "toByteArray", "()[B");
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let dis_c = pool.class("java/io/DataInputStream");
    let dis_i = pool.methodref("java/io/DataInputStream", "<init>", "(Ljava/io/InputStream;)V");
    let read_int = pool.methodref("java/io/DataInputStream", "readInt", "()I");
    let value = pool.integer(0x12345678);
    let mut code = Vec::new();
    code.extend(new_init(baos_c, baos_i));
    code.extend_from_slice(&op2(ASTORE, 1));
        code.extend(new_init_args(dos_c, dos_i, &op2(ALOAD, 1)));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(LDC_W, value));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, write_int));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, to_bytes));
    code.extend_from_slice(&op2(ASTORE, 3));
        code.extend(new_init_args(bais_c, bais_i, &op2(ALOAD, 3)));
    code.extend_from_slice(&op2(ASTORE, 4));
        code.extend(new_init_args(dis_c, dis_i, &op2(ALOAD, 4)));
    code.extend_from_slice(&op2(ASTORE, 5));
    code.extend_from_slice(&op2(ALOAD, 5));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read_int));
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 6, code), 0x12345678);
}

#[test]
fn dos_write_int_is_big_endian() {
    // writeInt(1) 的字节序必须是 00 00 00 01（大端）。
    let mut pool = Pool::new();
    let baos_c = pool.class("java/io/ByteArrayOutputStream");
    let baos_i = pool.methodref("java/io/ByteArrayOutputStream", "<init>", "()V");
    let dos_c = pool.class("java/io/DataOutputStream");
    let dos_i = pool.methodref("java/io/DataOutputStream", "<init>", "(Ljava/io/OutputStream;)V");
    let write_int = pool.methodref("java/io/DataOutputStream", "writeInt", "(I)V");
    let to_bytes = pool.methodref("java/io/ByteArrayOutputStream", "toByteArray", "()[B");
    let mut code = Vec::new();
    code.extend(new_init(baos_c, baos_i));
    code.extend_from_slice(&op2(ASTORE, 1));
        code.extend(new_init_args(dos_c, dos_i, &op2(ALOAD, 1)));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.push(ICONST_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, write_int));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, to_bytes));
    code.extend_from_slice(&op2(ASTORE, 3));
    // b0*1000 + b1*100 + b2*10 + b3：大端 = 1，小端 = 1000。
    code.extend_from_slice(&op2(ALOAD, 3));
    code.push(ICONST_0);
    code.push(BALOAD);
    push_i32(&mut code, 1000);
    code.push(IMUL);
    code.extend_from_slice(&op2(ALOAD, 3));
    code.push(ICONST_1);
    code.push(BALOAD);
    push_i32(&mut code, 100);
    code.push(IMUL);
    code.push(IADD);
    code.extend_from_slice(&op2(ALOAD, 3));
    code.push(ICONST_2);
    code.push(BALOAD);
    code.push(BIPUSH);
    code.push(10);
    code.push(IMUL);
    code.push(IADD);
    code.extend_from_slice(&op2(ALOAD, 3));
    code.push(ICONST_3);
    code.push(BALOAD);
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 4, code), 1);
}

#[test]
fn dos_short_char_and_dis_unsigned_roundtrip() {
    // writeShort(0x1234) + writeChar(0x5678) →
    // readShort()<<16 | readUnsignedShort() == 0x12345678。
    let mut pool = Pool::new();
    let baos_c = pool.class("java/io/ByteArrayOutputStream");
    let baos_i = pool.methodref("java/io/ByteArrayOutputStream", "<init>", "()V");
    let dos_c = pool.class("java/io/DataOutputStream");
    let dos_i = pool.methodref("java/io/DataOutputStream", "<init>", "(Ljava/io/OutputStream;)V");
    let write_short = pool.methodref("java/io/DataOutputStream", "writeShort", "(I)V");
    let write_char = pool.methodref("java/io/DataOutputStream", "writeChar", "(I)V");
    let to_bytes = pool.methodref("java/io/ByteArrayOutputStream", "toByteArray", "()[B");
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let dis_c = pool.class("java/io/DataInputStream");
    let dis_i = pool.methodref("java/io/DataInputStream", "<init>", "(Ljava/io/InputStream;)V");
    let read_short = pool.methodref("java/io/DataInputStream", "readShort", "()S");
    let read_ushort = pool.methodref("java/io/DataInputStream", "readUnsignedShort", "()I");
    let mut code = Vec::new();
    code.extend(new_init(baos_c, baos_i));
    code.extend_from_slice(&op2(ASTORE, 1));
        code.extend(new_init_args(dos_c, dos_i, &op2(ALOAD, 1)));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 2));
    push_i32(&mut code, 0x1234);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, write_short));
    code.extend_from_slice(&op2(ALOAD, 2));
    push_i32(&mut code, 0x5678);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, write_char));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, to_bytes));
    code.extend_from_slice(&op2(ASTORE, 3));
        code.extend(new_init_args(bais_c, bais_i, &op2(ALOAD, 3)));
    code.extend_from_slice(&op2(ASTORE, 4));
        code.extend(new_init_args(dis_c, dis_i, &op2(ALOAD, 4)));
    code.extend_from_slice(&op2(ASTORE, 5));
    code.extend_from_slice(&op2(ALOAD, 5));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read_short));
    code.push(BIPUSH);
    code.push(16);
    code.push(ISHL);
    code.extend_from_slice(&op2(ALOAD, 5));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read_ushort));
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 6, code), 0x12345678);
}

#[test]
fn dos_write_long_read_long_roundtrip() {
    // writeLong(0x123456789ABCDEF) → readLong（JVM 大端 8 字节往返）。
    let mut pool = Pool::new();
    let baos_c = pool.class("java/io/ByteArrayOutputStream");
    let baos_i = pool.methodref("java/io/ByteArrayOutputStream", "<init>", "()V");
    let dos_c = pool.class("java/io/DataOutputStream");
    let dos_i = pool.methodref("java/io/DataOutputStream", "<init>", "(Ljava/io/OutputStream;)V");
    let write_long = pool.methodref("java/io/DataOutputStream", "writeLong", "(J)V");
    let to_bytes = pool.methodref("java/io/ByteArrayOutputStream", "toByteArray", "()[B");
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let dis_c = pool.class("java/io/DataInputStream");
    let dis_i = pool.methodref("java/io/DataInputStream", "<init>", "(Ljava/io/InputStream;)V");
    let read_long = pool.methodref("java/io/DataInputStream", "readLong", "()J");
    let value = pool.long(0x0123_4567_89AB_CDEF);
    let mut code = Vec::new();
    code.extend(new_init(baos_c, baos_i));
    code.extend_from_slice(&op2(ASTORE, 1));
        code.extend(new_init_args(dos_c, dos_i, &op2(ALOAD, 1)));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(LDC2_W, value));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, write_long));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, to_bytes));
    code.extend_from_slice(&op2(ASTORE, 3));
        code.extend(new_init_args(bais_c, bais_i, &op2(ALOAD, 3)));
    code.extend_from_slice(&op2(ASTORE, 4));
        code.extend(new_init_args(dis_c, dis_i, &op2(ALOAD, 4)));
    code.extend_from_slice(&op2(ASTORE, 5));
    code.extend_from_slice(&op2(ALOAD, 5));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read_long));
    code.push(LRETURN);
    let outcome = run_main_outcome(&mut pool, "()J", 5, 6, code);
    assert_eq!(expect_long(outcome), 0x0123_4567_89AB_CDEF);
}

#[test]
fn write_utf_read_utf_ascii_roundtrip() {
    let mut pool = Pool::new();
    let hello = pool.string("hello");
    let baos_c = pool.class("java/io/ByteArrayOutputStream");
    let baos_i = pool.methodref("java/io/ByteArrayOutputStream", "<init>", "()V");
    let dos_c = pool.class("java/io/DataOutputStream");
    let dos_i = pool.methodref("java/io/DataOutputStream", "<init>", "(Ljava/io/OutputStream;)V");
    let write_utf = pool.methodref("java/io/DataOutputStream", "writeUTF", "(Ljava/lang/String;)V");
    let to_bytes = pool.methodref("java/io/ByteArrayOutputStream", "toByteArray", "()[B");
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let dis_c = pool.class("java/io/DataInputStream");
    let dis_i = pool.methodref("java/io/DataInputStream", "<init>", "(Ljava/io/InputStream;)V");
    let read_utf = pool.methodref("java/io/DataInputStream", "readUTF", "()Ljava/lang/String;");
    let mut code = Vec::new();
    code.extend(new_init(baos_c, baos_i));
    code.extend_from_slice(&op2(ASTORE, 1));
        code.extend(new_init_args(dos_c, dos_i, &op2(ALOAD, 1)));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(LDC_W, hello));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, write_utf));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, to_bytes));
    code.extend_from_slice(&op2(ASTORE, 3));
        code.extend(new_init_args(bais_c, bais_i, &op2(ALOAD, 3)));
    code.extend_from_slice(&op2(ASTORE, 4));
        code.extend(new_init_args(dis_c, dis_i, &op2(ALOAD, 4)));
    code.extend_from_slice(&op2(ASTORE, 5));
    code.extend_from_slice(&op2(ALOAD, 5));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read_utf));
    code.push(ARETURN);
    assert_eq!(run_main_text(&mut pool, "()Ljava/lang/String;", 5, 6, code), "hello");
}

#[test]
fn write_utf_read_utf_cjk_and_nul_roundtrip() {
    // Modified UTF-8：BMP 三字节、U+0000 双字节（C0 80）。
    let mut pool = Pool::new();
    let text = pool.string("中\0A");
    let baos_c = pool.class("java/io/ByteArrayOutputStream");
    let baos_i = pool.methodref("java/io/ByteArrayOutputStream", "<init>", "()V");
    let dos_c = pool.class("java/io/DataOutputStream");
    let dos_i = pool.methodref("java/io/DataOutputStream", "<init>", "(Ljava/io/OutputStream;)V");
    let write_utf = pool.methodref("java/io/DataOutputStream", "writeUTF", "(Ljava/lang/String;)V");
    let to_bytes = pool.methodref("java/io/ByteArrayOutputStream", "toByteArray", "()[B");
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let dis_c = pool.class("java/io/DataInputStream");
    let dis_i = pool.methodref("java/io/DataInputStream", "<init>", "(Ljava/io/InputStream;)V");
    let read_utf = pool.methodref("java/io/DataInputStream", "readUTF", "()Ljava/lang/String;");
    let mut code = Vec::new();
    code.extend(new_init(baos_c, baos_i));
    code.extend_from_slice(&op2(ASTORE, 1));
        code.extend(new_init_args(dos_c, dos_i, &op2(ALOAD, 1)));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(LDC_W, text));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, write_utf));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, to_bytes));
    code.extend_from_slice(&op2(ASTORE, 3));
        code.extend(new_init_args(bais_c, bais_i, &op2(ALOAD, 3)));
    code.extend_from_slice(&op2(ASTORE, 4));
        code.extend(new_init_args(dis_c, dis_i, &op2(ALOAD, 4)));
    code.extend_from_slice(&op2(ASTORE, 5));
    code.extend_from_slice(&op2(ALOAD, 5));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read_utf));
    code.push(ARETURN);
    assert_eq!(run_main_text(&mut pool, "()Ljava/lang/String;", 5, 6, code), "中\0A");
}

#[test]
fn read_utf_truncated_throws_eof() {
    // 长度前缀声明 5 字节、实余 1 字节 → EOFException。
    let mut pool = Pool::new();
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let dis_c = pool.class("java/io/DataInputStream");
    let dis_i = pool.methodref("java/io/DataInputStream", "<init>", "(Ljava/io/InputStream;)V");
    let read_utf = pool.methodref("java/io/DataInputStream", "readUTF", "()Ljava/lang/String;");
    let mut code = Vec::new();
        code.extend(new_init_args(bais_c, bais_i, &push_bytes(&[0x00, 0x05, b'h'])));
    code.extend_from_slice(&op2(ASTORE, 1));
        code.extend(new_init_args(dis_c, dis_i, &op2(ALOAD, 1)));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read_utf));
    code.push(POP);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(
        run_main_thrown(&mut pool, "()I", 4, 3, code),
        "java/io/EOFException"
    );
}

#[test]
fn read_utf_malformed_throws_format() {
    // 0xFF 不是 Modified UTF-8 合法首字节 → UTFDataFormatException。
    let mut pool = Pool::new();
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let dis_c = pool.class("java/io/DataInputStream");
    let dis_i = pool.methodref("java/io/DataInputStream", "<init>", "(Ljava/io/InputStream;)V");
    let read_utf = pool.methodref("java/io/DataInputStream", "readUTF", "()Ljava/lang/String;");
    let mut code = Vec::new();
        code.extend(new_init_args(bais_c, bais_i, &push_bytes(&[0x00, 0x01, 0xFF])));
    code.extend_from_slice(&op2(ASTORE, 1));
        code.extend(new_init_args(dis_c, dis_i, &op2(ALOAD, 1)));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read_utf));
    code.push(POP);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(
        run_main_thrown(&mut pool, "()I", 4, 3, code),
        "java/io/UTFDataFormatException"
    );
}

#[test]
fn read_fully_eof_throws() {
    // 余 2 字节、请求 3 → EOFException。
    let mut pool = Pool::new();
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let dis_c = pool.class("java/io/DataInputStream");
    let dis_i = pool.methodref("java/io/DataInputStream", "<init>", "(Ljava/io/InputStream;)V");
    let read_fully = pool.methodref("java/io/DataInputStream", "readFully", "([B)V");
    let mut code = Vec::new();
        code.extend(new_init_args(bais_c, bais_i, &push_bytes(&[1, 2])));
    code.extend_from_slice(&op2(ASTORE, 1));
        code.extend(new_init_args(dis_c, dis_i, &op2(ALOAD, 1)));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.push(ICONST_3);
    code.push(NEWARRAY);
    code.push(T_BYTE);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read_fully));
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(
        run_main_thrown(&mut pool, "()I", 5, 3, code),
        "java/io/EOFException"
    );
}

#[test]
fn read_unsigned_byte_widens() {
    // 0xFF → 255（无符号扩展），非 -1。
    let mut pool = Pool::new();
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let dis_c = pool.class("java/io/DataInputStream");
    let dis_i = pool.methodref("java/io/DataInputStream", "<init>", "(Ljava/io/InputStream;)V");
    let read_ubyte = pool.methodref("java/io/DataInputStream", "readUnsignedByte", "()I");
    let mut code = Vec::new();
        code.extend(new_init_args(bais_c, bais_i, &push_bytes(&[0xFF])));
    code.extend_from_slice(&op2(ASTORE, 1));
        code.extend(new_init_args(dis_c, dis_i, &op2(ALOAD, 1)));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read_ubyte));
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 4, 3, code), 255);
}

#[test]
fn read_unsigned_byte_at_eof_throws() {
    let mut pool = Pool::new();
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let dis_c = pool.class("java/io/DataInputStream");
    let dis_i = pool.methodref("java/io/DataInputStream", "<init>", "(Ljava/io/InputStream;)V");
    let read_ubyte = pool.methodref("java/io/DataInputStream", "readUnsignedByte", "()I");
    let mut code = Vec::new();
        code.extend(new_init_args(bais_c, bais_i, &push_bytes(&[0x41])));
    code.extend_from_slice(&op2(ASTORE, 1));
        code.extend(new_init_args(dis_c, dis_i, &op2(ALOAD, 1)));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read_ubyte));
    code.push(POP);
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read_ubyte));
    code.push(IRETURN);
    assert_eq!(
        run_main_thrown(&mut pool, "()I", 4, 3, code),
        "java/io/EOFException"
    );
}

const POP2: u8 = 0x58;

#[test]
fn bais_skip_mark_reset() {
    // 99×'c' + 100×'d'：skip(99) → mark → read c,d → reset → 又读回 c。
    let mut data = vec![b'c'; 99];
    data.extend_from_slice(&[b'd'; 100]);
    let mut pool = Pool::new();
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let skip = pool.methodref("java/io/ByteArrayInputStream", "skip", "(J)J");
    let mark = pool.methodref("java/io/ByteArrayInputStream", "mark", "(I)V");
    let read = pool.methodref("java/io/ByteArrayInputStream", "read", "()I");
    let reset = pool.methodref("java/io/ByteArrayInputStream", "reset", "()V");
    let skip_arg = pool.long(99);
    let mut code = Vec::new();
    // mark 先于 skip（markPos=0）→ skip(99) → read 'c' → read 'd' → reset → read 'c'；
    // 99 + 100 + 99 = 298（mark/reset 回退验证）。
    code.extend(new_init_args(bais_c, bais_i, &push_bytes(&data)));
    code.extend_from_slice(&op2(ASTORE, 1));
    code.extend_from_slice(&op2(ALOAD, 1));
    push_i32(&mut code, 200);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, mark));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(LDC2_W, skip_arg));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, skip));
    code.push(POP2);
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read));
    code.push(IADD);
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, reset));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read));
    code.push(IADD);
    // 'd' + 'd' + 'c' = 100 + 100 + 99 = 299（mark 在 skip 前标记 pos=0，reset 回退）
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 6, 2, code), 299);
}

#[test]
fn bais_available() {
    let mut pool = Pool::new();
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let available = pool.methodref("java/io/ByteArrayInputStream", "available", "()I");
    let mut code = Vec::new();
    code.extend(new_init_args(bais_c, bais_i, &push_bytes(&[0xAA; 32])));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, available));
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 1, code), 32);
}

#[test]
fn bais_read_array_returns_count_and_fills() {
    // 3 字节源读入 4 字节缓冲 → n=3，buf[0]=97 → 3*1000+97。
    let mut pool = Pool::new();
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let read = pool.methodref("java/io/ByteArrayInputStream", "read", "([B)I");
    let mut code = Vec::new();
        code.extend(new_init_args(bais_c, bais_i, &push_bytes(&[97, 98, 99])));
    code.extend_from_slice(&op2(ASTORE, 1));
    code.push(ICONST_4);
    code.push(NEWARRAY);
    code.push(T_BYTE);
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read));
    push_i32(&mut code, 1000);
    code.push(IMUL);
    code.extend_from_slice(&op2(ALOAD, 2));
    code.push(ICONST_0);
    code.push(BALOAD);
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 3, code), 3097);
}

#[test]
fn bais_read_range_oob_throws() {
    // len=2 的缓冲，read(buf, 1, 4) → IndexOutOfBoundsException。
    let mut pool = Pool::new();
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let read = pool.methodref("java/io/ByteArrayInputStream", "read", "([BII)I");
    let mut code = Vec::new();
        code.extend(new_init_args(bais_c, bais_i, &push_bytes(&[1, 2])));
    code.extend_from_slice(&op2(ASTORE, 1));
    code.push(ICONST_2);
    code.push(NEWARRAY);
    code.push(T_BYTE);
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.push(ICONST_1);
    code.push(ICONST_4);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read));
    code.push(POP);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(
        run_main_thrown(&mut pool, "()I", 6, 3, code),
        "java/lang/IndexOutOfBoundsException"
    );
}

#[test]
fn baos_to_string() {
    // write('h'); write('i'); toString() == "hi"。
    let mut pool = Pool::new();
    let baos_c = pool.class("java/io/ByteArrayOutputStream");
    let baos_i = pool.methodref("java/io/ByteArrayOutputStream", "<init>", "()V");
    let write = pool.methodref("java/io/ByteArrayOutputStream", "write", "(I)V");
    let to_string = pool.methodref("java/io/ByteArrayOutputStream", "toString", "()Ljava/lang/String;");
    let mut code = Vec::new();
    code.extend(new_init(baos_c, baos_i));
    code.extend_from_slice(&op2(ASTORE, 1));
    code.extend_from_slice(&op2(ALOAD, 1));
    push_i32(&mut code, b'h' as i32);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, write));
    code.extend_from_slice(&op2(ALOAD, 1));
    push_i32(&mut code, b'i' as i32);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, write));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, to_string));
    code.push(ARETURN);
    assert_eq!(run_main_text(&mut pool, "()Ljava/lang/String;", 4, 2, code), "hi");
}

#[test]
fn baos_size_and_reset() {
    // 5 × write(byte[4]) → size 20；reset() → size 0 → 20*1000。
    let mut pool = Pool::new();
    let baos_c = pool.class("java/io/ByteArrayOutputStream");
    let baos_i = pool.methodref("java/io/ByteArrayOutputStream", "<init>", "()V");
    let write = pool.methodref("java/io/ByteArrayOutputStream", "write", "([B)V");
    let size = pool.methodref("java/io/ByteArrayOutputStream", "size", "()I");
    let reset = pool.methodref("java/io/ByteArrayOutputStream", "reset", "()V");
    let mut code = Vec::new();
    code.extend(new_init(baos_c, baos_i));
    code.extend_from_slice(&op2(ASTORE, 1));
    code.push(ICONST_4);
    code.push(NEWARRAY);
    code.push(T_BYTE);
    code.extend_from_slice(&op2(ASTORE, 2));
    for _ in 0..5 {
        code.extend_from_slice(&op2(ALOAD, 1));
        code.extend_from_slice(&op2(ALOAD, 2));
        code.extend_from_slice(&op3(INVOKEVIRTUAL, write));
    }
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, size));
    push_i32(&mut code, 1000);
    code.push(IMUL);
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, reset));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, size));
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 3, code), 20000);
}

#[test]
fn baos_growth_loop() {
    // 循环 write 40 字节（跨初始容量扩容）→ size == 40。
    let mut pool = Pool::new();
    let baos_c = pool.class("java/io/ByteArrayOutputStream");
    let baos_i = pool.methodref("java/io/ByteArrayOutputStream", "<init>", "()V");
    let write = pool.methodref("java/io/ByteArrayOutputStream", "write", "(I)V");
    let size = pool.methodref("java/io/ByteArrayOutputStream", "size", "()I");
    let mut code = Vec::new();
    code.extend(new_init(baos_c, baos_i));
    code.extend_from_slice(&op2(ASTORE, 1));
    code.push(ICONST_0);
    code.extend_from_slice(&op2(ISTORE, 3));
    let loop_start = code.len();
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op2(ILOAD, 3));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, write));
    code.push(IINC);
    code.push(3);
    code.push(1);
    code.extend_from_slice(&op2(ILOAD, 3));
    code.push(BIPUSH);
    code.push(40);
    let if_site = code.len();
    code.extend_from_slice(&branch(IF_ICMPGE, 0));
    code.extend_from_slice(&branch(GOTO, loop_start as i64 as i16 - code.len() as i64 as i16));
    let rel = code.len() as i64 as i16 - if_site as i64 as i16;
    code[if_site + 1] = (rel >> 8) as u8;
    code[if_site + 2] = rel as u8;
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, size));
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 4, code), 40);
}

#[test]
fn baos_negative_size_throws_iae() {
    let mut pool = Pool::new();
    let baos_c = pool.class("java/io/ByteArrayOutputStream");
    let baos_i = pool.methodref("java/io/ByteArrayOutputStream", "<init>", "(I)V");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(NEW, baos_c));
    code.push(DUP);
    code.push(ICONST_M1);
    code.extend_from_slice(&op3(INVOKESPECIAL, baos_i));
    code.push(POP);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(
        run_main_thrown(&mut pool, "()I", 4, 1, code),
        "java/lang/IllegalArgumentException"
    );
}

#[test]
fn dos_size_after_mixed_writes() {
    // writeInt(4) + writeBoolean(1) + writeUTF("AB")(2+2) → size 9。
    let mut pool = Pool::new();
    let baos_c = pool.class("java/io/ByteArrayOutputStream");
    let baos_i = pool.methodref("java/io/ByteArrayOutputStream", "<init>", "()V");
    let dos_c = pool.class("java/io/DataOutputStream");
    let dos_i = pool.methodref("java/io/DataOutputStream", "<init>", "(Ljava/io/OutputStream;)V");
    let write_int = pool.methodref("java/io/DataOutputStream", "writeInt", "(I)V");
    let write_bool = pool.methodref("java/io/DataOutputStream", "writeBoolean", "(Z)V");
    let write_utf = pool.methodref("java/io/DataOutputStream", "writeUTF", "(Ljava/lang/String;)V");
    let size = pool.methodref("java/io/ByteArrayOutputStream", "size", "()I");
    let text = pool.string("AB");
    let mut code = Vec::new();
    code.extend(new_init(baos_c, baos_i));
    code.extend_from_slice(&op2(ASTORE, 1));
        code.extend(new_init_args(dos_c, dos_i, &op2(ALOAD, 1)));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.push(ICONST_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, write_int));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.push(ICONST_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, write_bool));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(LDC_W, text));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, write_utf));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, size));
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 3, code), 9);
}

#[test]
fn system_clinit_initializes_streams() {
    // 触发 System.<clinit> 后 out/err 非 null、in 非 null。
    let mut pool = Pool::new();
    let out_f = pool.fieldref("java/lang/System", "out", "Ljava/io/PrintStream;");
    let err_f = pool.fieldref("java/lang/System", "err", "Ljava/io/PrintStream;");
    let in_f = pool.fieldref("java/lang/System", "in", "Ljava/io/InputStream;");
    let mut code = Vec::new();
    let mut miss = Vec::new();
    code.extend_from_slice(&op3(GETSTATIC, out_f));
    code.extend_from_slice(&branch(IFNULL, 0));
    miss.push(code.len() - 3);
    code.extend_from_slice(&op3(GETSTATIC, err_f));
    code.extend_from_slice(&branch(IFNULL, 0));
    miss.push(code.len() - 3);
    code.extend_from_slice(&op3(GETSTATIC, in_f));
    code.extend_from_slice(&branch(IFNONNULL, 0));
    miss.push(code.len() - 3);
    code.push(ICONST_1);
    code.push(IRETURN);
    patch_miss_branches(&mut code, &miss);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 3, 1, code), 1);
}

#[test]
fn ps_println_int() {
    // new PrintStream(baos).println(42) → "42\n"。
    let mut pool = Pool::new();
    let baos_c = pool.class("java/io/ByteArrayOutputStream");
    let baos_i = pool.methodref("java/io/ByteArrayOutputStream", "<init>", "()V");
    let ps_c = pool.class("java/io/PrintStream");
    let ps_i = pool.methodref("java/io/PrintStream", "<init>", "(Ljava/io/OutputStream;)V");
    let println = pool.methodref("java/io/PrintStream", "println", "(I)V");
    let to_string = pool.methodref("java/io/ByteArrayOutputStream", "toString", "()Ljava/lang/String;");
    let mut code = Vec::new();
    code.extend(new_init(baos_c, baos_i));
    code.extend_from_slice(&op2(ASTORE, 1));
        code.extend(new_init_args(ps_c, ps_i, &op2(ALOAD, 1)));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.push(BIPUSH);
    code.push(42);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, println));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, to_string));
    code.push(ARETURN);
    assert_eq!(run_main_text(&mut pool, "()Ljava/lang/String;", 5, 3, code), "42\n");
}

#[test]
fn ps_print_boolean_and_char() {
    // print(false) + print('X') → "falseX"。
    let mut pool = Pool::new();
    let baos_c = pool.class("java/io/ByteArrayOutputStream");
    let baos_i = pool.methodref("java/io/ByteArrayOutputStream", "<init>", "()V");
    let ps_c = pool.class("java/io/PrintStream");
    let ps_i = pool.methodref("java/io/PrintStream", "<init>", "(Ljava/io/OutputStream;)V");
    let print_bool = pool.methodref("java/io/PrintStream", "print", "(Z)V");
    let print_char = pool.methodref("java/io/PrintStream", "print", "(C)V");
    let to_string = pool.methodref("java/io/ByteArrayOutputStream", "toString", "()Ljava/lang/String;");
    let mut code = Vec::new();
    code.extend(new_init(baos_c, baos_i));
    code.extend_from_slice(&op2(ASTORE, 1));
        code.extend(new_init_args(ps_c, ps_i, &op2(ALOAD, 1)));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.push(ICONST_0);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, print_bool));
    code.extend_from_slice(&op2(ALOAD, 2));
    push_i32(&mut code, 'X' as i32);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, print_char));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, to_string));
    code.push(ARETURN);
    assert_eq!(run_main_text(&mut pool, "()Ljava/lang/String;", 5, 3, code), "falseX");
}

#[test]
fn ps_println_long() {
    // println(1234567890123L) → "1234567890123\n"。
    let mut pool = Pool::new();
    let baos_c = pool.class("java/io/ByteArrayOutputStream");
    let baos_i = pool.methodref("java/io/ByteArrayOutputStream", "<init>", "()V");
    let ps_c = pool.class("java/io/PrintStream");
    let ps_i = pool.methodref("java/io/PrintStream", "<init>", "(Ljava/io/OutputStream;)V");
    let println = pool.methodref("java/io/PrintStream", "println", "(J)V");
    let to_string = pool.methodref("java/io/ByteArrayOutputStream", "toString", "()Ljava/lang/String;");
    let value = pool.long(1_234_567_890_123);
    let mut code = Vec::new();
    code.extend(new_init(baos_c, baos_i));
    code.extend_from_slice(&op2(ASTORE, 1));
        code.extend(new_init_args(ps_c, ps_i, &op2(ALOAD, 1)));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(LDC2_W, value));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, println));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, to_string));
    code.push(ARETURN);
    assert_eq!(run_main_text(&mut pool, "()Ljava/lang/String;", 6, 3, code), "1234567890123\n");
}

#[test]
fn isr_utf8_read() {
    // UTF-8 三字节序列 E4 B8 AD → U+4E2D（20013）。
    let mut pool = Pool::new();
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let isr_c = pool.class("java/io/InputStreamReader");
    let isr_i = pool.methodref("java/io/InputStreamReader", "<init>", "(Ljava/io/InputStream;)V");
    let read = pool.methodref("java/io/InputStreamReader", "read", "()I");
    let mut code = Vec::new();
    code.extend(new_init_args(
        isr_c,
        isr_i,
        &new_init_args(bais_c, bais_i, &push_bytes(&[0xE4, 0xB8, 0xAD])),
    ));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read));
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 1, code), 20013);
}

#[test]
fn isr_latin1_read() {
    // ISO-8859-1 下 0xE9 → 233。
    let mut pool = Pool::new();
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let isr_c = pool.class("java/io/InputStreamReader");
    let isr_i =
        pool.methodref("java/io/InputStreamReader", "<init>", "(Ljava/io/InputStream;Ljava/lang/String;)V");
    let read = pool.methodref("java/io/InputStreamReader", "read", "()I");
    let encoding = pool.string("ISO-8859-1");
    let mut code = Vec::new();
    code.extend(new_init_args(isr_c, isr_i, &{
        let mut args = new_init_args(bais_c, bais_i, &push_bytes(&[0xE9]));
        args.extend_from_slice(&op3(LDC_W, encoding));
        args
    }));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read));
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 6, 1, code), 233);
}

#[test]
fn isr_get_encoding_default() {
    let mut pool = Pool::new();
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let isr_c = pool.class("java/io/InputStreamReader");
    let isr_i = pool.methodref("java/io/InputStreamReader", "<init>", "(Ljava/io/InputStream;)V");
    let get_encoding = pool.methodref("java/io/InputStreamReader", "getEncoding", "()Ljava/lang/String;");
    let mut code = Vec::new();
    code.extend(new_init_args(
        isr_c,
        isr_i,
        &new_init_args(bais_c, bais_i, &push_bytes(&[0x00])),
    ));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, get_encoding));
    code.push(ARETURN);
    assert_eq!(run_main_text(&mut pool, "()Ljava/lang/String;", 5, 1, code), "UTF-8");
}

#[test]
fn isr_unsupported_encoding_throws() {
    let mut pool = Pool::new();
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let isr_c = pool.class("java/io/InputStreamReader");
    let isr_i =
        pool.methodref("java/io/InputStreamReader", "<init>", "(Ljava/io/InputStream;Ljava/lang/String;)V");
    let encoding = pool.string("GBK");
    let mut code = Vec::new();
    code.extend(new_init_args(isr_c, isr_i, &{
        let mut args = new_init_args(bais_c, bais_i, &push_bytes(&[0x00]));
        args.extend_from_slice(&op3(LDC_W, encoding));
        args
    }));
    code.push(POP);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(
        run_main_thrown(&mut pool, "()I", 6, 1, code),
        "java/io/UnsupportedEncodingException"
    );
}

#[test]
fn osw_write_flush_roundtrip() {
    // OSW.write("中") + flush → BAOS → BAIS → ISR.read() == 20013。
    let mut pool = Pool::new();
    let baos_c = pool.class("java/io/ByteArrayOutputStream");
    let baos_i = pool.methodref("java/io/ByteArrayOutputStream", "<init>", "()V");
    let osw_c = pool.class("java/io/OutputStreamWriter");
    let osw_i = pool.methodref("java/io/OutputStreamWriter", "<init>", "(Ljava/io/OutputStream;)V");
    let write = pool.methodref("java/io/OutputStreamWriter", "write", "(Ljava/lang/String;)V");
    let flush = pool.methodref("java/io/OutputStreamWriter", "flush", "()V");
    let to_bytes = pool.methodref("java/io/ByteArrayOutputStream", "toByteArray", "()[B");
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let isr_c = pool.class("java/io/InputStreamReader");
    let isr_i = pool.methodref("java/io/InputStreamReader", "<init>", "(Ljava/io/InputStream;)V");
    let read = pool.methodref("java/io/InputStreamReader", "read", "()I");
    let text = pool.string("中");
    let mut code = Vec::new();
    code.extend(new_init(baos_c, baos_i));
    code.extend_from_slice(&op2(ASTORE, 1));
        code.extend(new_init_args(osw_c, osw_i, &op2(ALOAD, 1)));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(LDC_W, text));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, write));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, flush));
        code.extend(new_init_args(isr_c, isr_i, &{
        let mut args = op2(ALOAD, 1).to_vec();
        args.extend_from_slice(&op3(INVOKEVIRTUAL, to_bytes));
        args = new_init_args(bais_c, bais_i, &args);
        args
    }));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read));
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 3, code), 20013);
}

#[test]
fn dis_read_line() {
    // "ab\ncd" → readLine() == "ab"。
    let mut pool = Pool::new();
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let dis_c = pool.class("java/io/DataInputStream");
    let dis_i = pool.methodref("java/io/DataInputStream", "<init>", "(Ljava/io/InputStream;)V");
    let read_line = pool.methodref("java/io/DataInputStream", "readLine", "()Ljava/lang/String;");
    let mut code = Vec::new();    code.extend(new_init_args(dis_c, dis_i, &new_init_args(bais_c, bais_i, &push_bytes(b"ab\ncd"))));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read_line));
    code.push(ARETURN);
    assert_eq!(run_main_text(&mut pool, "()Ljava/lang/String;", 5, 1, code), "ab");
}

#[test]
fn dis_skip_bytes() {
    // skipBytes(2) → 2，再 readUnsignedByte → 99 → 2*1000+99。
    let mut pool = Pool::new();
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let dis_c = pool.class("java/io/DataInputStream");
    let dis_i = pool.methodref("java/io/DataInputStream", "<init>", "(Ljava/io/InputStream;)V");
    let skip_bytes = pool.methodref("java/io/DataInputStream", "skipBytes", "(I)I");
    let read_ubyte = pool.methodref("java/io/DataInputStream", "readUnsignedByte", "()I");
    let mut code = Vec::new();    code.extend(new_init_args(dis_c, dis_i, &new_init_args(bais_c, bais_i, &push_bytes(&[1, 2, 99, 4]))));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.push(ICONST_2);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, skip_bytes));
    push_i32(&mut code, 1000);
    code.push(IMUL);
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read_ubyte));
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 3, code), 2099);
}

#[test]
fn dos_write_float_dis_read_float() {
    let mut pool = Pool::new();
    let baos_c = pool.class("java/io/ByteArrayOutputStream");
    let baos_i = pool.methodref("java/io/ByteArrayOutputStream", "<init>", "()V");
    let dos_c = pool.class("java/io/DataOutputStream");
    let dos_i = pool.methodref("java/io/DataOutputStream", "<init>", "(Ljava/io/OutputStream;)V");
    let write_float = pool.methodref("java/io/DataOutputStream", "writeFloat", "(F)V");
    let to_bytes = pool.methodref("java/io/ByteArrayOutputStream", "toByteArray", "()[B");
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let dis_c = pool.class("java/io/DataInputStream");
    let dis_i = pool.methodref("java/io/DataInputStream", "<init>", "(Ljava/io/InputStream;)V");
    let read_float = pool.methodref("java/io/DataInputStream", "readFloat", "()F");
    let value = pool.float(1.5);
    let mut code = Vec::new();
    code.extend(new_init(baos_c, baos_i));
    code.extend_from_slice(&op2(ASTORE, 1));
        code.extend(new_init_args(dos_c, dos_i, &op2(ALOAD, 1)));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(LDC_W, value));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, write_float));
        code.extend(new_init_args(dis_c, dis_i, &{
        let mut args = op2(ALOAD, 1).to_vec();
        args.extend_from_slice(&op3(INVOKEVIRTUAL, to_bytes));
        args = new_init_args(bais_c, bais_i, &args);
        args
    }));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read_float));
    code.push(FRETURN);
    let outcome = run_main_outcome(&mut pool, "()F", 5, 3, code);
    assert_eq!(expect_float(outcome), 1.5);
}

#[test]
fn dos_write_double_dis_read_double() {
    let mut pool = Pool::new();
    let baos_c = pool.class("java/io/ByteArrayOutputStream");
    let baos_i = pool.methodref("java/io/ByteArrayOutputStream", "<init>", "()V");
    let dos_c = pool.class("java/io/DataOutputStream");
    let dos_i = pool.methodref("java/io/DataOutputStream", "<init>", "(Ljava/io/OutputStream;)V");
    let write_double = pool.methodref("java/io/DataOutputStream", "writeDouble", "(D)V");
    let to_bytes = pool.methodref("java/io/ByteArrayOutputStream", "toByteArray", "()[B");
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let dis_c = pool.class("java/io/DataInputStream");
    let dis_i = pool.methodref("java/io/DataInputStream", "<init>", "(Ljava/io/InputStream;)V");
    let read_double = pool.methodref("java/io/DataInputStream", "readDouble", "()D");
    let value = pool.double(2.5);
    let mut code = Vec::new();
    code.extend(new_init(baos_c, baos_i));
    code.extend_from_slice(&op2(ASTORE, 1));
        code.extend(new_init_args(dos_c, dos_i, &op2(ALOAD, 1)));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(LDC2_W, value));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, write_double));
        code.extend(new_init_args(dis_c, dis_i, &{
        let mut args = op2(ALOAD, 1).to_vec();
        args.extend_from_slice(&op3(INVOKEVIRTUAL, to_bytes));
        args = new_init_args(bais_c, bais_i, &args);
        args
    }));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read_double));
    code.push(DRETURN);
    let outcome = run_main_outcome(&mut pool, "()D", 6, 3, code);
    assert_eq!(expect_double(outcome), 2.5);
}

// ===========================================================================
// T017 补测段：jc.jar 高频调用面 + 已实现成员语义兜底。
// 覆盖 PrintStream 文本面、DataInputStream 单字节/数组读、包装类解析边界、
// Object/Class 形态、Math long 族与 atan2、Hashtable.contains、Vector 变体、
// Timer 数据面等先前未断言的公开成员。
// ===========================================================================

/// T017 段补充常量。
const CASTORE: u8 = 0x55;
const DMUL: u8 = 0x6b;
const D2I: u8 = 0x8e;

#[test]
fn ps_println_string_high_frequency() {
    // new PrintStream(baos).println("hi") → "hi\n"（jc.jar 15 处调用）。
    let mut pool = Pool::new();
    let baos_c = pool.class("java/io/ByteArrayOutputStream");
    let baos_i = pool.methodref("java/io/ByteArrayOutputStream", "<init>", "()V");
    let ps_c = pool.class("java/io/PrintStream");
    let ps_i = pool.methodref("java/io/PrintStream", "<init>", "(Ljava/io/OutputStream;)V");
    let println = pool.methodref("java/io/PrintStream", "println", "(Ljava/lang/String;)V");
    let to_string = pool.methodref("java/io/ByteArrayOutputStream", "toString", "()Ljava/lang/String;");
    let hi = pool.string("hi");
    let mut code = Vec::new();
    code.extend(new_init(baos_c, baos_i));
    code.extend_from_slice(&op2(ASTORE, 1));
    code.extend(new_init_args(ps_c, ps_i, &op2(ALOAD, 1)));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(LDC_W, hi));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, println));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, to_string));
    code.push(ARETURN);
    assert_eq!(run_main_text(&mut pool, "()Ljava/lang/String;", 3, 3, code), "hi\n");
}

#[test]
fn ps_print_string_then_int() {
    // print("a") + print(1) → "a1"（无换行拼接）。
    let mut pool = Pool::new();
    let baos_c = pool.class("java/io/ByteArrayOutputStream");
    let baos_i = pool.methodref("java/io/ByteArrayOutputStream", "<init>", "()V");
    let ps_c = pool.class("java/io/PrintStream");
    let ps_i = pool.methodref("java/io/PrintStream", "<init>", "(Ljava/io/OutputStream;)V");
    let print_str = pool.methodref("java/io/PrintStream", "print", "(Ljava/lang/String;)V");
    let print_int = pool.methodref("java/io/PrintStream", "print", "(I)V");
    let to_string = pool.methodref("java/io/ByteArrayOutputStream", "toString", "()Ljava/lang/String;");
    let a = pool.string("a");
    let mut code = Vec::new();
    code.extend(new_init(baos_c, baos_i));
    code.extend_from_slice(&op2(ASTORE, 1));
    code.extend(new_init_args(ps_c, ps_i, &op2(ALOAD, 1)));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(LDC_W, a));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, print_str));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.push(ICONST_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, print_int));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, to_string));
    code.push(ARETURN);
    assert_eq!(run_main_text(&mut pool, "()Ljava/lang/String;", 3, 3, code), "a1");
}

#[test]
fn ps_println_double_and_print_float() {
    // println(2.5d) → "2.5\n"，print(1.5f) → "1.5"（JDK Double/Float.toString 形态）。
    let mut pool = Pool::new();
    let baos_c = pool.class("java/io/ByteArrayOutputStream");
    let baos_i = pool.methodref("java/io/ByteArrayOutputStream", "<init>", "()V");
    let ps_c = pool.class("java/io/PrintStream");
    let ps_i = pool.methodref("java/io/PrintStream", "<init>", "(Ljava/io/OutputStream;)V");
    let println_d = pool.methodref("java/io/PrintStream", "println", "(D)V");
    let print_f = pool.methodref("java/io/PrintStream", "print", "(F)V");
    let to_string = pool.methodref("java/io/ByteArrayOutputStream", "toString", "()Ljava/lang/String;");
    let two_half = pool.double(2.5);
    let one_half = pool.float(1.5);
    let mut code = Vec::new();
    code.extend(new_init(baos_c, baos_i));
    code.extend_from_slice(&op2(ASTORE, 1));
    code.extend(new_init_args(ps_c, ps_i, &op2(ALOAD, 1)));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(LDC2_W, two_half));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, println_d));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(LDC_W, one_half));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, print_f));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, to_string));
    code.push(ARETURN);
    assert_eq!(run_main_text(&mut pool, "()Ljava/lang/String;", 3, 3, code), "2.5\n1.5");
}

#[test]
fn dis_read_byte_sign_and_boolean_roundtrip() {
    // writeBoolean(true) + writeByte(0xFF) → readBoolean==1、readByte==-1
    //（readByte 符号扩展；jc.jar 高频 19+17 处）。100*1 + (-1+2) == 101。
    let mut pool = Pool::new();
    let baos_c = pool.class("java/io/ByteArrayOutputStream");
    let baos_i = pool.methodref("java/io/ByteArrayOutputStream", "<init>", "()V");
    let dos_c = pool.class("java/io/DataOutputStream");
    let dos_i = pool.methodref("java/io/DataOutputStream", "<init>", "(Ljava/io/OutputStream;)V");
    let write_bool = pool.methodref("java/io/DataOutputStream", "writeBoolean", "(Z)V");
    let write_byte = pool.methodref("java/io/DataOutputStream", "writeByte", "(I)V");
    let to_bytes = pool.methodref("java/io/ByteArrayOutputStream", "toByteArray", "()[B");
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let dis_c = pool.class("java/io/DataInputStream");
    let dis_i = pool.methodref("java/io/DataInputStream", "<init>", "(Ljava/io/InputStream;)V");
    let read_bool = pool.methodref("java/io/DataInputStream", "readBoolean", "()Z");
    let read_byte = pool.methodref("java/io/DataInputStream", "readByte", "()B");
    let mut code = Vec::new();
    code.extend(new_init(baos_c, baos_i));
    code.extend_from_slice(&op2(ASTORE, 1));
    code.extend(new_init_args(dos_c, dos_i, &op2(ALOAD, 1)));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.push(ICONST_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, write_bool));
    code.extend_from_slice(&op2(ALOAD, 2));
    push_i32(&mut code, 0xFF);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, write_byte));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, to_bytes));
    code.extend_from_slice(&op2(ASTORE, 3));
    code.extend(new_init_args(bais_c, bais_i, &op2(ALOAD, 3)));
    code.extend_from_slice(&op2(ASTORE, 4));
    code.extend(new_init_args(dis_c, dis_i, &op2(ALOAD, 4)));
    code.extend_from_slice(&op2(ASTORE, 5));
    code.extend_from_slice(&op2(ALOAD, 5));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read_bool));
    code.push(BIPUSH);
    code.push(100);
    code.push(IMUL);
    code.extend_from_slice(&op2(ALOAD, 5));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read_byte));
    code.push(ICONST_2);
    code.push(IADD);
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 6, code), 101);
}

#[test]
fn dis_read_single_is_unsigned() {
    // DIS.read() 无符号读：{0xFF,0x01} → 255*100 + 1 == 25501。
    let mut pool = Pool::new();
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let dis_c = pool.class("java/io/DataInputStream");
    let dis_i = pool.methodref("java/io/DataInputStream", "<init>", "(Ljava/io/InputStream;)V");
    let read = pool.methodref("java/io/DataInputStream", "read", "()I");
    let mut code = Vec::new();
    code.extend(new_init_args(bais_c, bais_i, &push_bytes(&[0xFF, 0x01])));
    code.extend_from_slice(&op2(ASTORE, 1));
    code.extend(new_init_args(dis_c, dis_i, &op2(ALOAD, 1)));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read));
    code.push(BIPUSH);
    code.push(100);
    code.push(IMUL);
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read));
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 3, code), 25501);
}

#[test]
fn dis_read_array_fills_and_counts() {
    // DIS.read(byte[4]) 返回读取数 4 并填充：4*1000 + b[0]*10 + b[3] == 4140。
    let mut pool = Pool::new();
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let dis_c = pool.class("java/io/DataInputStream");
    let dis_i = pool.methodref("java/io/DataInputStream", "<init>", "(Ljava/io/InputStream;)V");
    let read_array = pool.methodref("java/io/DataInputStream", "read", "([B)I");
    let mut code = Vec::new();
    code.extend(new_init_args(bais_c, bais_i, &push_bytes(&[10, 20, 30, 40, 50])));
    code.extend_from_slice(&op2(ASTORE, 1));
    code.extend(new_init_args(dis_c, dis_i, &op2(ALOAD, 1)));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.push(ICONST_4);
    code.push(NEWARRAY);
    code.push(T_BYTE);
    code.extend_from_slice(&op2(ASTORE, 3));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op2(ALOAD, 3));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read_array));
    code.extend_from_slice(&op2(ISTORE, 4));
    code.extend_from_slice(&op2(ILOAD, 4));
    code.extend_from_slice(&op3(SIPUSH, 1000));
    code.push(IMUL);
    code.extend_from_slice(&op2(ALOAD, 3));
    code.push(ICONST_0);
    code.push(BALOAD);
    code.push(BIPUSH);
    code.push(10);
    code.push(IMUL);
    code.push(IADD);
    code.extend_from_slice(&op2(ALOAD, 3));
    code.push(ICONST_3);
    code.push(BALOAD);
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 5, code), 4140);
}

#[test]
fn dis_read_fully_range_partial() {
    // readFully(b,1,2) 从 {1,2,3,4} 读 2 字节落位：b[1]=1,b[2]=2,b[3]=0 → 1+20+0==21。
    let mut pool = Pool::new();
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let dis_c = pool.class("java/io/DataInputStream");
    let dis_i = pool.methodref("java/io/DataInputStream", "<init>", "(Ljava/io/InputStream;)V");
    let read_fully = pool.methodref("java/io/DataInputStream", "readFully", "([BII)V");
    let mut code = Vec::new();
    code.extend(new_init_args(bais_c, bais_i, &push_bytes(&[1, 2, 3, 4])));
    code.extend_from_slice(&op2(ASTORE, 1));
    code.extend(new_init_args(dis_c, dis_i, &op2(ALOAD, 1)));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.push(ICONST_5);
    code.push(NEWARRAY);
    code.push(T_BYTE);
    code.extend_from_slice(&op2(ASTORE, 3));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op2(ALOAD, 3));
    code.push(ICONST_1);
    code.push(ICONST_2);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read_fully));
    code.extend_from_slice(&op2(ALOAD, 3));
    code.push(ICONST_1);
    code.push(BALOAD);
    code.extend_from_slice(&op2(ALOAD, 3));
    code.push(ICONST_2);
    code.push(BALOAD);
    code.push(BIPUSH);
    code.push(10);
    code.push(IMUL);
    code.push(IADD);
    code.extend_from_slice(&op2(ALOAD, 3));
    code.push(ICONST_3);
    code.push(BALOAD);
    code.push(BIPUSH);
    code.push(100);
    code.push(IMUL);
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 4, code), 21);
}

#[test]
fn dis_skip_returns_count() {
    // DIS.skip(2L)==2（返回 long），随后 read()==7 → 2*100+7==207。
    let mut pool = Pool::new();
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let dis_c = pool.class("java/io/DataInputStream");
    let dis_i = pool.methodref("java/io/DataInputStream", "<init>", "(Ljava/io/InputStream;)V");
    let skip = pool.methodref("java/io/DataInputStream", "skip", "(J)J");
    let read = pool.methodref("java/io/DataInputStream", "read", "()I");
    let two = pool.long(2);
    let mut code = Vec::new();
    code.extend(new_init_args(bais_c, bais_i, &push_bytes(&[9, 8, 7])));
    code.extend_from_slice(&op2(ASTORE, 1));
    code.extend(new_init_args(dis_c, dis_i, &op2(ALOAD, 1)));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(LDC2_W, two));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, skip));
    code.push(L2I);
    code.extend_from_slice(&op2(ISTORE, 3));
    code.extend_from_slice(&op2(ILOAD, 3));
    code.push(BIPUSH);
    code.push(100);
    code.push(IMUL);
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, read));
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 4, code), 207);
}

#[test]
fn baos_write_array_and_range() {
    // write({1,2,3}) + write(b,1,1) → {1,2,3,2}：len*10 + b[3] == 42。
    let mut pool = Pool::new();
    let baos_c = pool.class("java/io/ByteArrayOutputStream");
    let baos_i = pool.methodref("java/io/ByteArrayOutputStream", "<init>", "()V");
    let write_all = pool.methodref("java/io/ByteArrayOutputStream", "write", "([B)V");
    let write_range = pool.methodref("java/io/ByteArrayOutputStream", "write", "([BII)V");
    let to_bytes = pool.methodref("java/io/ByteArrayOutputStream", "toByteArray", "()[B");
    let mut code = Vec::new();
    code.extend(push_bytes(&[1, 2, 3]));
    code.extend_from_slice(&op2(ASTORE, 1));
    code.extend(new_init(baos_c, baos_i));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, write_all));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.push(ICONST_1);
    code.push(ICONST_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, write_range));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, to_bytes));
    code.extend_from_slice(&op2(ASTORE, 3));
    code.extend_from_slice(&op2(ALOAD, 3));
    code.push(ARRAYLENGTH);
    code.push(BIPUSH);
    code.push(10);
    code.push(IMUL);
    code.extend_from_slice(&op2(ALOAD, 3));
    code.push(ICONST_3);
    code.push(BALOAD);
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 4, code), 42);
}

#[test]
fn io_close_and_flush_are_safe_noops() {
    // BAIS.close() / BAOS.flush() / BAOS.close() 不抛异常。
    let mut pool = Pool::new();
    let bais_c = pool.class("java/io/ByteArrayInputStream");
    let bais_i = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let bais_close = pool.methodref("java/io/ByteArrayInputStream", "close", "()V");
    let baos_c = pool.class("java/io/ByteArrayOutputStream");
    let baos_i = pool.methodref("java/io/ByteArrayOutputStream", "<init>", "()V");
    let baos_flush = pool.methodref("java/io/ByteArrayOutputStream", "flush", "()V");
    let baos_close = pool.methodref("java/io/ByteArrayOutputStream", "close", "()V");
    let mut code = Vec::new();
    let arr = push_bytes(&[1]);
    code.extend(new_init_args(bais_c, bais_i, &arr));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend(new_init(baos_c, baos_i));
    code.extend_from_slice(&op2(ASTORE, 3));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, bais_close));
    code.extend_from_slice(&op2(ALOAD, 3));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, baos_flush));
    code.extend_from_slice(&op2(ALOAD, 3));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, baos_close));
    code.push(ICONST_1);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 4, code), 1);
}

#[test]
fn string_init_chars_range_semantics() {
    // new String(char[]{'a','b','c','d'}, 1, 2) == "bc"。
    let mut pool = Pool::new();
    let string_c = pool.class("java/lang/String");
    let init = pool.methodref("java/lang/String", "<init>", "([CII)V");
    let to_string = pool.methodref("java/lang/String", "toString", "()Ljava/lang/String;");
    let mut chars = Vec::new();
    push_i32(&mut chars, 4);
    chars.push(NEWARRAY);
    chars.push(T_CHAR);
    for (index, value) in [97u16, 98, 99, 100].iter().enumerate() {
        chars.push(DUP);
        push_i32(&mut chars, index as i32);
        push_i32(&mut chars, *value as i32);
        chars.push(CASTORE);
    }
    let mut code = Vec::new();
    code.extend_from_slice(&chars);
    code.extend_from_slice(&op2(ASTORE, 1));
    let mut args = op2(ALOAD, 1).to_vec();
    args.push(ICONST_1);
    args.push(ICONST_2);
    code.extend(new_init_args(string_c, init, &args));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, to_string));
    code.push(ARETURN);
    assert_eq!(run_main_text(&mut pool, "()Ljava/lang/String;", 5, 2, code), "bc");
}

#[test]
fn string_value_of_family() {
    // valueOf('Z') + valueOf(true) + valueOf(7L) + valueOf(2.5) == "Ztrue72.5"。
    let mut pool = Pool::new();
    let value_char = pool.methodref("java/lang/String", "valueOf", "(C)Ljava/lang/String;");
    let value_bool = pool.methodref("java/lang/String", "valueOf", "(Z)Ljava/lang/String;");
    let value_long = pool.methodref("java/lang/String", "valueOf", "(J)Ljava/lang/String;");
    let value_double = pool.methodref("java/lang/String", "valueOf", "(D)Ljava/lang/String;");
    let concat = pool.methodref("java/lang/String", "concat", "(Ljava/lang/String;)Ljava/lang/String;");
    let seven = pool.long(7);
    let two_half = pool.double(2.5);
    let mut code = Vec::new();
    code.push(BIPUSH);
    code.push(b'Z' as i8 as u8);
    code.extend_from_slice(&op3(INVOKESTATIC, value_char));
    code.extend_from_slice(&op2(ASTORE, 1));
    code.push(ICONST_1);
    code.extend_from_slice(&op3(INVOKESTATIC, value_bool));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op3(LDC2_W, seven));
    code.extend_from_slice(&op3(INVOKESTATIC, value_long));
    code.extend_from_slice(&op2(ASTORE, 3));
    code.extend_from_slice(&op3(LDC2_W, two_half));
    code.extend_from_slice(&op3(INVOKESTATIC, value_double));
    code.extend_from_slice(&op2(ASTORE, 5));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, concat));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op2(ALOAD, 3));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, concat));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op2(ALOAD, 5));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, concat));
    code.push(ARETURN);
    assert_eq!(run_main_text(&mut pool, "()Ljava/lang/String;", 2, 6, code), "Ztrue72.5");
}

#[test]
fn string_value_of_null_object_is_null_text() {
    // String.valueOf((Object)null) == "null"（JDK 显式 null 文本）。
    let mut pool = Pool::new();
    let value_object = pool.methodref(
        "java/lang/String",
        "valueOf",
        "(Ljava/lang/Object;)Ljava/lang/String;",
    );
    let mut code = Vec::new();
    code.push(ACONST_NULL);
    code.extend_from_slice(&op3(INVOKESTATIC, value_object));
    code.push(ARETURN);
    assert_eq!(run_main_text(&mut pool, "()Ljava/lang/String;", 1, 1, code), "null");
}

#[test]
fn string_last_index_of_from_and_str() {
    // "hello"：lastIndexOf('l',2)==2、lastIndexOf('l',1)==-1、
    // lastIndexOf("l")==3、lastIndexOf("z")==-1 → (2+1)*1000+0+3*10+0 == 3030。
    let mut pool = Pool::new();
    let hello = pool.string("hello");
    let l = pool.string("l");
    let z = pool.string("z");
    let last_from = pool.methodref("java/lang/String", "lastIndexOf", "(II)I");
    let last_str = pool.methodref("java/lang/String", "lastIndexOf", "(Ljava/lang/String;)I");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, hello));
    code.push(BIPUSH);
    code.push(b'l' as i8 as u8);
    code.push(ICONST_2);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, last_from));
    code.push(ICONST_1);
    code.push(IADD);
    code.extend_from_slice(&op3(SIPUSH, 1000));
    code.push(IMUL);
    code.extend_from_slice(&op3(LDC_W, hello));
    code.push(BIPUSH);
    code.push(b'l' as i8 as u8);
    code.push(ICONST_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, last_from));
    code.push(ICONST_1);
    code.push(IADD);
    code.push(BIPUSH);
    code.push(100);
    code.push(IMUL);
    code.push(IADD);
    code.extend_from_slice(&op3(LDC_W, hello));
    code.extend_from_slice(&op3(LDC_W, l));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, last_str));
    code.push(BIPUSH);
    code.push(10);
    code.push(IMUL);
    code.push(IADD);
    code.extend_from_slice(&op3(LDC_W, hello));
    code.extend_from_slice(&op3(LDC_W, z));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, last_str));
    code.push(ICONST_1);
    code.push(IADD);
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 5, 1, code), 3030);
}

#[test]
fn object_equals_identity_and_stable_hash_code() {
    // Object.equals 同一性：o1.equals(o1)==1、o1.equals(o2)==0、
    // 同一对象两次 hashCode 相等 → 1*100 + 0*10 + 1 == 101。
    let mut pool = Pool::new();
    let object_c = pool.class("java/lang/Object");
    let object_i = pool.methodref("java/lang/Object", "<init>", "()V");
    let equals = pool.methodref("java/lang/Object", "equals", "(Ljava/lang/Object;)Z");
    let hash_code = pool.methodref("java/lang/Object", "hashCode", "()I");
    let mut code = Vec::new();
    code.extend(new_init(object_c, object_i));
    code.extend_from_slice(&op2(ASTORE, 1));
    code.extend(new_init(object_c, object_i));
    code.extend_from_slice(&op2(ASTORE, 2));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.extend_from_slice(&op2(ISTORE, 3));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op2(ALOAD, 2));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.extend_from_slice(&op2(ISTORE, 4));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, hash_code));
    code.extend_from_slice(&op2(ISTORE, 5));
    let mut miss = Vec::new();
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, hash_code));
    code.extend_from_slice(&op2(ILOAD, 5));
    code.extend_from_slice(&branch(IF_ICMPNE, 0));
    miss.push(code.len() - 3);
    code.extend_from_slice(&op2(ILOAD, 3));
    code.push(BIPUSH);
    code.push(100);
    code.push(IMUL);
    code.extend_from_slice(&op2(ILOAD, 4));
    code.push(BIPUSH);
    code.push(10);
    code.push(IMUL);
    code.push(IADD);
    code.push(ICONST_1);
    code.push(IADD);
    code.push(IRETURN);
    patch_miss_branches(&mut code, &miss);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 2, 6, code), 101);
}

#[test]
fn object_to_string_form() {
    // Object.toString() == getClass().getName() + "@" + hex（点分类名前缀）。
    let mut pool = Pool::new();
    let object_c = pool.class("java/lang/Object");
    let object_i = pool.methodref("java/lang/Object", "<init>", "()V");
    let to_string = pool.methodref("java/lang/Object", "toString", "()Ljava/lang/String;");
    let mut code = Vec::new();
    code.extend(new_init(object_c, object_i));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, to_string));
    code.push(ARETURN);
    let text = run_main_text(&mut pool, "()Ljava/lang/String;", 1, 1, code);
    assert!(
        text.starts_with("java.lang.Object@"),
        "unexpected toString form: {text}"
    );
}

#[test]
fn class_to_string_form() {
    // String 实例的 Class.toString() == "class java.lang.String"。
    let mut pool = Pool::new();
    let s = pool.string("s");
    let get_class = pool.methodref("java/lang/Object", "getClass", "()Ljava/lang/Class;");
    let to_string = pool.methodref("java/lang/Class", "toString", "()Ljava/lang/String;");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, s));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, get_class));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, to_string));
    code.push(ARETURN);
    assert_eq!(run_main_text(&mut pool, "()Ljava/lang/String;", 2, 1, code), "class java.lang.String");
}

#[test]
fn integer_parse_int_min_overflow_and_plus_sign() {
    // JDK：parseInt("-2147483648")==MIN、parseInt("+5")==5（JDK7+ 容许 '+'）；
    // "2147483648" 溢出与 "" 均抛 NumberFormatException。
    let plus = {
        let mut pool = Pool::new();
        let text = pool.string("+5");
        let parse = pool.methodref("java/lang/Integer", "parseInt", "(Ljava/lang/String;)I");
        let mut code = Vec::new();
        code.extend_from_slice(&op3(LDC_W, text));
        code.extend_from_slice(&op3(INVOKESTATIC, parse));
        code.push(IRETURN);
        run_main(&mut pool, "()I", 1, 1, code)
    };
    assert_eq!(plus, 5);
    let min = {
        let mut pool = Pool::new();
        let text = pool.string("-2147483648");
        let parse = pool.methodref("java/lang/Integer", "parseInt", "(Ljava/lang/String;)I");
        let mut code = Vec::new();
        code.extend_from_slice(&op3(LDC_W, text));
        code.extend_from_slice(&op3(INVOKESTATIC, parse));
        code.push(IRETURN);
        run_main(&mut pool, "()I", 1, 1, code)
    };
    assert_eq!(min, i32::MIN);
    for bad in ["2147483648", ""] {
        let mut pool = Pool::new();
        let text = pool.string(bad);
        let parse = pool.methodref("java/lang/Integer", "parseInt", "(Ljava/lang/String;)I");
        let mut code = Vec::new();
        code.extend_from_slice(&op3(LDC_W, text));
        code.extend_from_slice(&op3(INVOKESTATIC, parse));
        code.push(IRETURN);
        assert_eq!(
            run_main_thrown(&mut pool, "()I", 1, 1, code),
            "java/lang/NumberFormatException"
        );
    }
}

#[test]
fn long_parse_long_bounds() {
    // parseLong("9223372036854775807")==Long.MAX；溢出抛 NFE。
    let mut pool = Pool::new();
    let max_text = pool.string("9223372036854775807");
    // 溢出边界字符串仅入池（溢出抛 NFE 由后半段验证），构码不引用。
    let _overflow = pool.string("9223372036854775808");
    let parse = pool.methodref("java/lang/Long", "parseLong", "(Ljava/lang/String;)J");
    let max = pool.long(i64::MAX);
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, max_text));
    code.extend_from_slice(&op3(INVOKESTATIC, parse));
    code.extend_from_slice(&op3(LDC2_W, max));
    code.push(LCMP);
    let mut miss = Vec::new();
    code.extend_from_slice(&branch(IFNE, 0));
    miss.push(code.len() - 3);
    code.push(ICONST_1);
    code.push(IRETURN);
    patch_miss_branches(&mut code, &miss);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 3, 1, code), 1);

    let mut pool = Pool::new();
    let overflow = pool.string("9223372036854775808");
    let parse = pool.methodref("java/lang/Long", "parseLong", "(Ljava/lang/String;)J");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, overflow));
    code.extend_from_slice(&op3(INVOKESTATIC, parse));
    code.push(LRETURN);
    assert_eq!(
        run_main_thrown(&mut pool, "()J", 1, 1, code),
        "java/lang/NumberFormatException"
    );
}

#[test]
fn float_double_parse_and_static_classify() {
    // isNaN(parseDouble("NaN"))、isInfinite(1.0/0.0)、
    // isNaN(parseFloat("NaN") 转双精度) → 100 + 10 + 1 == 111。
    let mut pool = Pool::new();
    let nan = pool.string("NaN");
    let parse_double = pool.methodref("java/lang/Double", "parseDouble", "(Ljava/lang/String;)D");
    let parse_float = pool.methodref("java/lang/Float", "parseFloat", "(Ljava/lang/String;)F");
    let is_nan_d = pool.methodref("java/lang/Double", "isNaN", "(D)Z");
    let is_infinite_d = pool.methodref("java/lang/Double", "isInfinite", "(D)Z");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, nan));
    code.extend_from_slice(&op3(INVOKESTATIC, parse_double));
    code.extend_from_slice(&op3(INVOKESTATIC, is_nan_d));
    code.push(BIPUSH);
    code.push(100);
    code.push(IMUL);
    code.push(DCONST_1);
    code.push(DCONST_0);
    code.push(DDIV);
    code.extend_from_slice(&op3(INVOKESTATIC, is_infinite_d));
    code.push(BIPUSH);
    code.push(10);
    code.push(IMUL);
    code.push(IADD);
    code.extend_from_slice(&op3(LDC_W, nan));
    code.extend_from_slice(&op3(INVOKESTATIC, parse_float));
    code.push(F2D);
    code.extend_from_slice(&op3(INVOKESTATIC, is_nan_d));
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 3, 1, code), 111);
}

#[test]
fn character_wrapper_char_value() {
    // new Character('x').charValue() == 120。
    let mut pool = Pool::new();
    let char_c = pool.class("java/lang/Character");
    let init = pool.methodref("java/lang/Character", "<init>", "(C)V");
    let char_value = pool.methodref("java/lang/Character", "charValue", "()C");
    let mut code = Vec::new();
    code.push(BIPUSH);
    code.push(b'x' as i8 as u8);
    let arg = code.clone();
    code.clear();
    code.extend(new_init_args(char_c, init, &arg));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, char_value));
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 2, 1, code), 120);
}

#[test]
fn math_min_max_long() {
    // min(3L,2L)==2、max(3L,2L)==3 → 2*10 + 3 == 23。
    let mut pool = Pool::new();
    let min = pool.methodref("java/lang/Math", "min", "(JJ)J");
    let max = pool.methodref("java/lang/Math", "max", "(JJ)J");
    let three = pool.long(3);
    let two = pool.long(2);
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC2_W, three));
    code.extend_from_slice(&op3(LDC2_W, two));
    code.extend_from_slice(&op3(INVOKESTATIC, min));
    code.push(L2I);
    code.push(BIPUSH);
    code.push(10);
    code.push(IMUL);
    code.extend_from_slice(&op3(LDC2_W, three));
    code.extend_from_slice(&op3(LDC2_W, two));
    code.extend_from_slice(&op3(INVOKESTATIC, max));
    code.push(L2I);
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 4, 1, code), 23);
}

#[test]
fn math_atan2_diagonal_is_quarter_pi() {
    // atan2(1,1)==π/4 → (int)(π/4*4)==3。
    let mut pool = Pool::new();
    let atan2 = pool.methodref("java/lang/Math", "atan2", "(DD)D");
    let four = pool.double(4.0);
    let mut code = vec![DCONST_1];
    code.push(DCONST_1);
    code.extend_from_slice(&op3(INVOKESTATIC, atan2));
    code.extend_from_slice(&op3(LDC2_W, four));
    code.push(DMUL);
    code.push(D2I);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 4, 1, code), 3);
}

#[test]
fn system_gc_is_safe_noop() {
    // System.gc() 无返回、不抛异常。
    let mut pool = Pool::new();
    let gc = pool.methodref("java/lang/System", "gc", "()V");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(INVOKESTATIC, gc));
    code.push(ICONST_1);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 1, 1, code), 1);
}

#[test]
fn throwable_fill_in_stack_trace_keeps_message() {
    // fillInStackTrace() 返回同一异常（消息保持）。
    let mut pool = Pool::new();
    let throwable_c = pool.class("java/lang/Throwable");
    let init = pool.methodref("java/lang/Throwable", "<init>", "(Ljava/lang/String;)V");
    let fill = pool.methodref(
        "java/lang/Throwable",
        "fillInStackTrace",
        "()Ljava/lang/Throwable;",
    );
    let get_message = pool.methodref("java/lang/Throwable", "getMessage", "()Ljava/lang/String;");
    let message = pool.string("m");
    let mut code = Vec::new();
    let arg = op3(LDC_W, message).to_vec();
    code.extend(new_init_args(throwable_c, init, &arg));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, fill));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, get_message));
    code.push(ARETURN);
    assert_eq!(run_main_text(&mut pool, "()Ljava/lang/String;", 1, 1, code), "m");
}

#[test]
fn sb_set_char_at_and_append_variants() {
    // append(true) + append('c') → "truec"，setCharAt(0,'X') → "Xruec"。
    let mut pool = Pool::new();
    let sb_c = pool.class("java/lang/StringBuffer");
    let sb_i = pool.methodref("java/lang/StringBuffer", "<init>", "()V");
    let append_bool = pool.methodref("java/lang/StringBuffer", "append", "(Z)Ljava/lang/StringBuffer;");
    let append_char = pool.methodref("java/lang/StringBuffer", "append", "(C)Ljava/lang/StringBuffer;");
    let set_char_at = pool.methodref("java/lang/StringBuffer", "setCharAt", "(IC)V");
    let to_string = pool.methodref("java/lang/StringBuffer", "toString", "()Ljava/lang/String;");
    let mut code = Vec::new();
    code.extend(new_init(sb_c, sb_i));
    code.extend_from_slice(&op2(ASTORE, 1));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.push(ICONST_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, append_bool));
    code.push(POP);
    code.extend_from_slice(&op2(ALOAD, 1));
    code.push(BIPUSH);
    code.push(b'c' as i8 as u8);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, append_char));
    code.push(POP);
    code.extend_from_slice(&op2(ALOAD, 1));
    code.push(ICONST_0);
    code.push(BIPUSH);
    code.push(b'X' as i8 as u8);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, set_char_at));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, to_string));
    code.push(ARETURN);
    assert_eq!(run_main_text(&mut pool, "()Ljava/lang/String;", 2, 2, code), "Xruec");
}

#[test]
fn integer_value_of_string_int_value() {
    // Integer.valueOf("42").intValue() == 42。
    let mut pool = Pool::new();
    let text = pool.string("42");
    let value_of = pool.methodref(
        "java/lang/Integer",
        "valueOf",
        "(Ljava/lang/String;)Ljava/lang/Integer;",
    );
    let int_value = pool.methodref("java/lang/Integer", "intValue", "()I");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(LDC_W, text));
    code.extend_from_slice(&op3(INVOKESTATIC, value_of));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, int_value));
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 1, 1, code), 42);
}

#[test]
fn hashtable_contains_elements_and_absent_ops() {
    // contains 按值查找；get/remove 缺键返回 null；elements() 可取首个值。
    let mut pool = Pool::new();
    let ht_c = pool.class("java/util/Hashtable");
    let ht_i = pool.methodref("java/util/Hashtable", "<init>", "()V");
    let put = pool.methodref(
        "java/util/Hashtable",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    );
    let contains = pool.methodref("java/util/Hashtable", "contains", "(Ljava/lang/Object;)Z");
    let get = pool.methodref("java/util/Hashtable", "get", "(Ljava/lang/Object;)Ljava/lang/Object;");
    let remove = pool.methodref("java/util/Hashtable", "remove", "(Ljava/lang/Object;)Ljava/lang/Object;");
    let elements = pool.methodref("java/util/Hashtable", "elements", "()Ljava/util/Enumeration;");
    let next = pool.methodref("java/util/Enumeration", "nextElement", "()Ljava/lang/Object;");
    let equals = pool.methodref("java/lang/Object", "equals", "(Ljava/lang/Object;)Z");
    let key = pool.string("k");
    let value = pool.string("v");
    let absent = pool.string("no");
    let zz = pool.string("zz");
    let mut code = Vec::new();
    code.extend(new_init(ht_c, ht_i));
    code.extend_from_slice(&op2(ASTORE, 1));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(LDC_W, key));
    code.extend_from_slice(&op3(LDC_W, value));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, put));
    code.push(POP);
    let mut miss = Vec::new();
    // contains("v") == 1
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(LDC_W, value));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, contains));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    // contains("zz") == 0
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(LDC_W, zz));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, contains));
    code.extend_from_slice(&branch(IFNE, 0));
    miss.push(code.len() - 3);
    // get("no") == null
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(LDC_W, absent));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, get));
    code.extend_from_slice(&branch(IFNONNULL, 0));
    miss.push(code.len() - 3);
    // remove("no") == null（不误删现存键）
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(LDC_W, absent));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, remove));
    code.extend_from_slice(&branch(IFNONNULL, 0));
    miss.push(code.len() - 3);
    // elements().nextElement().equals("v")
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, elements));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, next));
    code.extend_from_slice(&op3(LDC_W, value));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    code.push(ICONST_1);
    code.push(IRETURN);
    patch_miss_branches(&mut code, &miss);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 3, 2, code), 1);
}

#[test]
fn vector_set_element_at_and_remove_all_elements() {
    // setElementAt 覆盖位 0；removeAllElements 后 size==0 且 isEmpty。
    let mut pool = Pool::new();
    let vec_c = pool.class("java/util/Vector");
    let vec_i = pool.methodref("java/util/Vector", "<init>", "()V");
    let add = pool.methodref("java/util/Vector", "addElement", "(Ljava/lang/Object;)V");
    let set_at = pool.methodref("java/util/Vector", "setElementAt", "(Ljava/lang/Object;I)V");
    let element_at = pool.methodref("java/util/Vector", "elementAt", "(I)Ljava/lang/Object;");
    let equals = pool.methodref("java/lang/Object", "equals", "(Ljava/lang/Object;)Z");
    let remove_all = pool.methodref("java/util/Vector", "removeAllElements", "()V");
    let size = pool.methodref("java/util/Vector", "size", "()I");
    let is_empty = pool.methodref("java/util/Vector", "isEmpty", "()Z");
    let a = pool.string("A");
    let b = pool.string("B");
    let c = pool.string("C");
    let x = pool.string("X");
    let mut code = Vec::new();
    code.extend(new_init(vec_c, vec_i));
    code.extend_from_slice(&op2(ASTORE, 1));
    for text in [a, b, c] {
        code.extend_from_slice(&op2(ALOAD, 1));
        code.extend_from_slice(&op3(LDC_W, text));
        code.extend_from_slice(&op3(INVOKEVIRTUAL, add));
    }
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(LDC_W, x));
    code.push(ICONST_0);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, set_at));
    let mut miss = Vec::new();
    // elementAt(0).equals("X")
    code.extend_from_slice(&op2(ALOAD, 1));
    code.push(ICONST_0);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, element_at));
    code.extend_from_slice(&op3(LDC_W, x));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, equals));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, remove_all));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, size));
    code.extend_from_slice(&branch(IFNE, 0));
    miss.push(code.len() - 3);
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, is_empty));
    code.extend_from_slice(&branch(IFEQ, 0));
    miss.push(code.len() - 3);
    code.push(ICONST_1);
    code.push(IRETURN);
    patch_miss_branches(&mut code, &miss);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 3, 2, code), 1);
}

#[test]
fn timezone_gmt_has_no_daylight_saving() {
    // GMT 时区 usesDaylightTime()==false。
    let mut pool = Pool::new();
    let get_default = pool.methodref("java/util/TimeZone", "getDefault", "()Ljava/util/TimeZone;");
    let uses_dst = pool.methodref("java/util/TimeZone", "usesDaylightTime", "()Z");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(INVOKESTATIC, get_default));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, uses_dst));
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 1, 1, code), 0);
}

#[test]
fn timer_cancel_then_purge_is_zero() {
    // Timer.cancel() 置消标志后 purge()==0（数据面无回收登记）。
    let mut pool = Pool::new();
    let timer_c = pool.class("java/util/Timer");
    let timer_i = pool.methodref("java/util/Timer", "<init>", "()V");
    let cancel = pool.methodref("java/util/Timer", "cancel", "()V");
    let purge = pool.methodref("java/util/Timer", "purge", "()I");
    let mut code = Vec::new();
    code.extend(new_init(timer_c, timer_i));
    code.extend_from_slice(&op2(ASTORE, 1));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, cancel));
    code.extend_from_slice(&op2(ALOAD, 1));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, purge));
    code.push(IRETURN);
    assert_eq!(run_main(&mut pool, "()I", 2, 2, code), 0);
}
