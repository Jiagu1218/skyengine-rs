//! 解释器测试（T009）：帧执行、clinit 链、异常展开、monitor 阻塞、指令预算。
//!
//! 类字节全部经共享构造器手构（见 `common/classbytes.rs`）；
//! 标准异常层次由 `exception_classes` 夹具提供（Fault 物化依赖）。
//! 字节码中的操作数池索引一律**先取号再构码**（与 build_class 的
//! 预驻留约定一致）。

#[allow(dead_code)]
mod common;

use common::classbytes::{
    build_class, exception_classes, provider, FieldSpec, HandlerSpec, MethodSpec, Pool, ROOT,
};

use skyengine_kvm::host::mock::MockHostContext;
use skyengine_kvm::interp::{
    enter, run, runtime_class_of, Fault, Frame, Machine, NativeCall, NativeDispatch, RunOutcome,
    Unwind,
};
use skyengine_kvm::runtime::classspace::{ArrayElem, ClassSpace, InitState};
use skyengine_kvm::runtime::heap::Heap;
use skyengine_kvm::runtime::monitor::{EnterOutcome, Monitors};
use skyengine_kvm::runtime::{ObjRef, Slot, ThreadId};
use skyengine_kvm::KvmError;

// ---- 操作码助记（仅测试用到的） ------------------------------------------

const ACONST_NULL: u8 = 0x01;
const ICONST_M1: u8 = 0x02;
const ICONST_0: u8 = 0x03;
const ICONST_1: u8 = 0x04;
const ICONST_2: u8 = 0x05;
const ICONST_3: u8 = 0x06;
const ICONST_4: u8 = 0x07;
const ICONST_5: u8 = 0x08;
const LCONST_1: u8 = 0x0a;
const DCONST_1: u8 = 0x0f;
const BIPUSH: u8 = 0x10;
const SIPUSH: u8 = 0x11;
const LDC: u8 = 0x12;
const LDC2_W: u8 = 0x14;
const ILOAD_1: u8 = 0x1b;
const ILOAD_2: u8 = 0x1c;
const LLOAD_0: u8 = 0x1e;
const ALOAD_0: u8 = 0x2a;
const IALOAD: u8 = 0x2e;
const AALOAD: u8 = 0x32;
const ISTORE_1: u8 = 0x3c;
const ISTORE_2: u8 = 0x3d;
const ASTORE_0: u8 = 0x4b;
const ASTORE_1: u8 = 0x4c;
const IASTORE: u8 = 0x4f;
const POP: u8 = 0x57;
const DUP: u8 = 0x59;
const IADD: u8 = 0x60;
const LADD: u8 = 0x61;
const DMUL: u8 = 0x6b;
const IDIV: u8 = 0x6c;
const IINC: u8 = 0x84;
const I2L: u8 = 0x85;
const L2I: u8 = 0x88;
const I2B: u8 = 0x91;
const LCMP: u8 = 0x94;
const IF_ICMPGT: u8 = 0xa3;
const GOTO: u8 = 0xa7;
const JSR: u8 = 0xa8;
const RET: u8 = 0xa9;
const TABLESWITCH: u8 = 0xaa;
const LOOKUPSWITCH: u8 = 0xab;
const IRETURN: u8 = 0xac;
const LRETURN: u8 = 0xad;
const DRETURN: u8 = 0xaf;
const ARETURN: u8 = 0xb0;
const RETURN: u8 = 0xb1;
const GETSTATIC: u8 = 0xb2;
const PUTSTATIC: u8 = 0xb3;
const GETFIELD: u8 = 0xb4;
const PUTFIELD: u8 = 0xb5;
const INVOKEVIRTUAL: u8 = 0xb6;
const INVOKESTATIC: u8 = 0xb8;
const NEW: u8 = 0xbb;
const NEWARRAY: u8 = 0xbc;
const ANEWARRAY: u8 = 0xbd;
const ARRAYLENGTH: u8 = 0xbe;
const ATHROW: u8 = 0xbf;
const CHECKCAST: u8 = 0xc0;
const MONITORENTER: u8 = 0xc2;
const MONITOREXIT: u8 = 0xc3;
const MULTIANEWARRAY: u8 = 0xc5;

/// newarray 的 atype：T_INT。
const T_INT: u8 = 10;

/// 主线程（调度器 T011 前的固定标识）。
const MAIN: ThreadId = ThreadId(1);

// ---- 测试 VM 夹具 ---------------------------------------------------------

/// 记录调用并返回预置值的原生分派（native 路径测试）。
#[derive(Default)]
struct TestNatives {
    /// (类名, 方法名, this, 实参)。
    calls: Vec<(String, String, Option<ObjRef>, Vec<Slot>)>,
    /// 预置返回值。
    ret: Option<Slot>,
}

impl NativeDispatch for TestNatives {
    fn call_native(
        &mut self,
        cs: &mut ClassSpace,
        _heap: &mut Heap,
        _host: &mut dyn skyengine_kvm::host::HostContext,
        call: NativeCall,
    ) -> Result<Option<Slot>, KvmError> {
        self.calls.push((
            cs.name_of(call.class).to_string(),
            cs.syms.name(call.name).to_string(),
            call.this,
            call.args,
        ));
        Ok(self.ret)
    }
}

/// 测试 VM：类空间 + 堆 + monitor + 原生记录。
struct Vm {
    cs: ClassSpace,
    heap: Heap,
    monitors: Monitors,
    natives: TestNatives,
    /// HostContext 通道（T019）；纯 native 测试用 Mock 兜底。
    host: MockHostContext,
}

impl Vm {
    /// ROOT + 标准异常层次 + 用户类；object_class 回填给类空间。
    fn new(pool: &mut Pool, user: Vec<(String, Vec<u8>)>) -> Self {
        let mut entries = vec![(
            ROOT.to_string(),
            build_class(pool, ROOT, None, &[], &[], &[]),
        )];
        entries.extend(exception_classes(pool));
        entries.extend(user);
        let mut cs = ClassSpace::new(Box::new(provider(entries)));
        let root = cs.load_class(ROOT).unwrap();
        cs.object_class = Some(root);
        Self {
            cs,
            heap: Heap::new(),
            monitors: Monitors::new(),
            natives: TestNatives::default(),
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

    /// 执行静态入口方法（无参），全新帧栈。
    fn exec(&mut self, class: &str, name: &str, desc: &str) -> Result<RunOutcome, KvmError> {
        let mut frames = Vec::new();
        self.exec_frames(class, name, desc, &mut frames)
    }

    /// 压入入口方法并执行到帧栈中（帧栈由调用方持有，可续跑）。
    fn exec_frames(
        &mut self,
        class: &str,
        name: &str,
        desc: &str,
        frames: &mut Vec<Frame>,
    ) -> Result<RunOutcome, KvmError> {
        // 按需加载（find 不触发加载；exec 可能是首次使用该类）。
        let class_id = self.cs.load_class(class)?;
        let mut machine = self.machine();
        enter(&mut machine, frames, class_id, name, desc, Vec::new())?;
        run(&mut machine, frames, MAIN, None)
    }

    /// 续跑既有帧栈（预算测试 / monitor 阻塞恢复）。
    fn run_frames(
        &mut self,
        frames: &mut Vec<Frame>,
        budget: Option<u32>,
    ) -> Result<RunOutcome, KvmError> {
        let mut machine = self.machine();
        run(&mut machine, frames, MAIN, budget)
    }
}

// ---- 构造与断言小工具 -----------------------------------------------------

/// 带码方法。
fn m(
    access: u16,
    name: &'static str,
    desc: &'static str,
    stack: u16,
    locals: u16,
    code: Vec<u8>,
) -> MethodSpec {
    MethodSpec {
        access,
        name,
        desc,
        code: Some((stack, locals, code)),
        handlers: Vec::new(),
    }
}

/// 带码 + 异常表方法。
fn mh(
    access: u16,
    name: &'static str,
    desc: &'static str,
    stack: u16,
    locals: u16,
    code: Vec<u8>,
    handlers: Vec<HandlerSpec>,
) -> MethodSpec {
    MethodSpec {
        access,
        name,
        desc,
        code: Some((stack, locals, code)),
        handlers,
    }
}

/// native/abstract 方法（无码）。
fn native_m(access: u16, name: &'static str, desc: &'static str) -> MethodSpec {
    MethodSpec {
        access,
        name,
        desc,
        code: None,
        handlers: Vec::new(),
    }
}

fn handler(start: u16, end: u16, at: u16, catch: Option<&'static str>) -> HandlerSpec {
    HandlerSpec {
        start_pc: start,
        end_pc: end,
        handler_pc: at,
        catch_class: catch,
    }
}

fn field(access: u16, name: &'static str, desc: &'static str) -> FieldSpec {
    FieldSpec {
        access,
        name,
        desc,
        constant: None,
    }
}

fn field_const(access: u16, name: &'static str, desc: &'static str, constant: u16) -> FieldSpec {
    FieldSpec {
        access,
        name,
        desc,
        constant: Some(constant),
    }
}

/// 单操作数分支指令（3 字节）。
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

fn expect_ref(outcome: RunOutcome) -> ObjRef {
    match outcome {
        RunOutcome::Returned(Some(Slot::Ref(value))) => value,
        other => panic!("expected reference return, got {other:?}"),
    }
}

fn expect_uncaught_fault(outcome: RunOutcome, fault: Fault) {
    match outcome {
        RunOutcome::Uncaught(Unwind::Fault(actual)) => assert_eq!(actual, fault),
        other => panic!("expected uncaught fault {fault:?}, got {other:?}"),
    }
}

fn str_text(heap: &Heap, reference: ObjRef) -> String {
    String::from_utf16(&heap.str_obj(reference).unwrap().chars).unwrap()
}

// ---- 算术与转换 -----------------------------------------------------------

#[test]
fn int_arith_add_returns_sum() {
    let mut pool = Pool::new();
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[m(
            0x0008,
            "main",
            "()I",
            2,
            0,
            vec![ICONST_3, ICONST_4, IADD, IRETURN],
        )],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    assert_eq!(expect_int(vm.exec("A", "main", "()I").unwrap()), 7);
}

#[test]
fn branch_loop_sums_to_fifteen() {
    // int sum = 0; for (int i = 0; i <= 5; i++) sum += i; return sum;
    let mut code = vec![ICONST_0]; // 字节 0：sum = 0
    code.push(ISTORE_1);
    code.push(ICONST_0); // 字节 2：i = 0
    code.push(ISTORE_2);
    code.push(ILOAD_2); // 字节 4：循环头
    code.push(ICONST_5);
    code.extend_from_slice(&branch(IF_ICMPGT, 13)); // i > 5 → 字节 19
    code.push(ILOAD_1);
    code.push(ILOAD_2);
    code.push(IADD);
    code.push(ISTORE_1);
    code.push(IINC); // 字节 13：iinc 2, +1
    code.push(2);
    code.push(1);
    code.extend_from_slice(&branch(GOTO, -12)); // 字节 16 → 字节 4
    code.push(ILOAD_1); // 字节 19
    code.push(IRETURN);
    let mut pool = Pool::new();
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[m(0x0008, "main", "()I", 2, 3, code)],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    assert_eq!(expect_int(vm.exec("A", "main", "()I").unwrap()), 15);
}

#[test]
fn long_add_and_return() {
    // 1L + 2L：lconst 只有 _0/_1，常量 2 走 ldc2_w。
    let mut pool = Pool::new();
    let two = pool.long(2);
    let mut code = vec![LCONST_1, LDC2_W];
    code.extend_from_slice(&two.to_be_bytes());
    code.push(LADD);
    code.push(LRETURN);
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[m(0x0008, "main", "()J", 4, 0, code)],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    assert_eq!(expect_long(vm.exec("A", "main", "()J").unwrap()), 3);
}

#[test]
fn wide_long_int_arguments_pass_through_call() {
    // long wide(long a, int b) { return a + b; } —— cat2 参数占双局部槽。
    let mut pool = Pool::new();
    let wide_ref = pool.methodref("W", "wide", "(JI)J");
    let mut main_code = vec![BIPUSH];
    main_code.push(5);
    main_code.push(I2L);
    main_code.push(BIPUSH);
    main_code.push(2);
    main_code.push(INVOKESTATIC);
    main_code.extend_from_slice(&wide_ref.to_be_bytes());
    main_code.push(LRETURN);
    let w_bytes = build_class(
        &mut pool,
        "W",
        Some(ROOT),
        &[],
        &[],
        &[
            m(0x0008, "wide", "(JI)J", 4, 4, vec![LLOAD_0, ILOAD_2, I2L, LADD, LRETURN]),
            m(0x0008, "main", "()J", 3, 0, main_code),
        ],
    );
    let mut vm = Vm::new(&mut pool, vec![("W".into(), w_bytes)]);
    assert_eq!(expect_long(vm.exec("W", "main", "()J").unwrap()), 7);
}

#[test]
fn double_multiply_returns_one() {
    let mut pool = Pool::new();
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[m(
            0x0008,
            "main",
            "()D",
            4,
            0,
            vec![DCONST_1, DCONST_1, DMUL, DRETURN],
        )],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    assert_eq!(expect_double(vm.exec("A", "main", "()D").unwrap()), 1.0);
}

#[test]
fn long_compare_ordered_result() {
    // lcmp：1 < 5 → -1。
    let mut pool = Pool::new();
    let five = pool.long(5);
    let mut code = vec![LCONST_1, LDC2_W];
    code.extend_from_slice(&five.to_be_bytes());
    code.push(LCMP);
    code.push(IRETURN);
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[m(0x0008, "main", "()I", 4, 0, code)],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    assert_eq!(expect_int(vm.exec("A", "main", "()I").unwrap()), -1);
}

#[test]
fn int_long_roundtrip_conversion() {
    let mut pool = Pool::new();
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[m(0x0008, "main", "()I", 2, 0, vec![ICONST_5, I2L, L2I, IRETURN])],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    assert_eq!(expect_int(vm.exec("A", "main", "()I").unwrap()), 5);
}

#[test]
fn int_to_byte_truncates() {
    // sipush 200; i2b → -56。
    let mut pool = Pool::new();
    let mut code = vec![SIPUSH];
    code.extend_from_slice(&200i16.to_be_bytes());
    code.push(I2B);
    code.push(IRETURN);
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[m(0x0008, "main", "()I", 1, 0, code)],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    assert_eq!(expect_int(vm.exec("A", "main", "()I").unwrap()), -56);
}

// ---- clinit 链 -----------------------------------------------------------

#[test]
fn clinit_chain_runs_super_first() {
    // Base.<clinit>: v = 42；Sub.<clinit>: v = Base.v + 1 = 43。
    let mut pool = Pool::new();
    let base_v = pool.fieldref("Base", "v", "I");
    let sub_v = pool.fieldref("Sub", "v", "I");
    let mut base_clinit = Vec::new();
    base_clinit.push(BIPUSH);
    base_clinit.push(42);
    base_clinit.push(PUTSTATIC);
    base_clinit.extend_from_slice(&base_v.to_be_bytes());
    base_clinit.push(RETURN);
    let base_bytes = build_class(
        &mut pool,
        "Base",
        Some(ROOT),
        &[],
        &[field(0x0008, "v", "I")],
        &[m(0x0008, "<clinit>", "()V", 1, 0, base_clinit)],
    );
    let mut sub_clinit = Vec::new();
    sub_clinit.push(GETSTATIC);
    sub_clinit.extend_from_slice(&base_v.to_be_bytes());
    sub_clinit.push(ICONST_1);
    sub_clinit.push(IADD);
    sub_clinit.push(PUTSTATIC);
    sub_clinit.extend_from_slice(&sub_v.to_be_bytes());
    sub_clinit.push(RETURN);
    let mut sub_main = Vec::new();
    sub_main.push(GETSTATIC);
    sub_main.extend_from_slice(&sub_v.to_be_bytes());
    sub_main.push(IRETURN);
    let sub_bytes = build_class(
        &mut pool,
        "Sub",
        Some("Base"),
        &[],
        &[field(0x0008, "v", "I")],
        &[
            m(0x0008, "<clinit>", "()V", 1, 0, sub_clinit),
            m(0x0008, "main", "()I", 1, 0, sub_main),
        ],
    );
    let mut vm = Vm::new(
        &mut pool,
        vec![("Base".into(), base_bytes), ("Sub".into(), sub_bytes)],
    );
    // getstatic 触发链：ROOT → Base → Sub 逐个 clinit，随后重执行读 43。
    assert_eq!(expect_int(vm.exec("Sub", "main", "()I").unwrap()), 43);
    assert_eq!(
        vm.cs.class(vm.cs.find("Base").unwrap()).init_state,
        InitState::Initialized
    );
    assert_eq!(
        vm.cs.class(vm.cs.find("Sub").unwrap()).init_state,
        InitState::Initialized
    );
}

#[test]
fn clinit_interns_string_constant() {
    let mut pool = Pool::new();
    let msg_const = pool.string("hello");
    let msg_ref = pool.fieldref("Base", "msg", "Ljava/lang/String;");
    let get_ref = pool.methodref("Base", "getMsg", "()Ljava/lang/String;");
    let mut get_code = Vec::new();
    get_code.push(GETSTATIC);
    get_code.extend_from_slice(&msg_ref.to_be_bytes());
    get_code.push(ARETURN);
    let mut main_code = Vec::new();
    main_code.push(INVOKESTATIC);
    main_code.extend_from_slice(&get_ref.to_be_bytes());
    main_code.push(ARETURN);
    let base_bytes = build_class(
        &mut pool,
        "Base",
        Some(ROOT),
        &[],
        &[field_const(0x0009, "msg", "Ljava/lang/String;", msg_const)],
        &[
            m(0x0009, "getMsg", "()Ljava/lang/String;", 1, 0, get_code),
            m(0x0008, "main", "()Ljava/lang/String;", 1, 0, main_code),
        ],
    );
    let mut vm = Vm::new(&mut pool, vec![("Base".into(), base_bytes)]);
    let result = expect_ref(vm.exec("Base", "main", "()Ljava/lang/String;").unwrap());
    assert_eq!(str_text(&vm.heap, result), "hello");
    // 静态槽与驻留串是同一对象。
    let base_id = vm.cs.find("Base").unwrap();
    assert_eq!(vm.cs.class(base_id).statics[0], Slot::Ref(result));
}

#[test]
fn clinit_failure_is_sticky() {
    let mut pool = Pool::new();
    let x_ref = pool.fieldref("F", "x", "I");
    let mut main_code = Vec::new();
    main_code.push(GETSTATIC);
    main_code.extend_from_slice(&x_ref.to_be_bytes());
    main_code.push(IRETURN);
    let f_bytes = build_class(
        &mut pool,
        "F",
        Some(ROOT),
        &[],
        &[field(0x0008, "x", "I")],
        &[
            // <clinit> 除零 → Arithmetic 故障。
            m(0x0008, "<clinit>", "()V", 2, 0, vec![ICONST_1, ICONST_0, IDIV, RETURN]),
            m(0x0008, "main", "()I", 1, 0, main_code),
        ],
    );
    let mut vm = Vm::new(&mut pool, vec![("F".into(), f_bytes)]);
    // clinit 与触发帧都无处理器 → 异常逃逸，链粘滞标 Failed。
    expect_uncaught_fault(vm.exec("F", "main", "()I").unwrap(), Fault::Arithmetic);
    assert_eq!(
        vm.cs.class(vm.cs.find("F").unwrap()).init_state,
        InitState::Failed
    );
    // 粘滞：再次主动使用报链接错误。
    let error = vm.exec("F", "main", "()I").unwrap_err();
    assert!(matches!(error, KvmError::Linkage(_)), "{error}");
}

// ---- 字段与调用 -----------------------------------------------------------

#[test]
fn instance_field_store_and_virtual_getter() {
    let mut pool = Pool::new();
    let c_class = pool.class("C");
    let x_ref = pool.fieldref("C", "x", "I");
    let get_ref = pool.methodref("C", "get", "()I");
    let mut get_code = Vec::new();
    get_code.push(ALOAD_0);
    get_code.push(GETFIELD);
    get_code.extend_from_slice(&x_ref.to_be_bytes());
    get_code.push(IRETURN);
    let mut main_code = Vec::new();
    main_code.push(NEW);
    main_code.extend_from_slice(&c_class.to_be_bytes());
    main_code.push(DUP);
    main_code.push(BIPUSH);
    main_code.push(7);
    main_code.push(PUTFIELD);
    main_code.extend_from_slice(&x_ref.to_be_bytes());
    main_code.push(INVOKEVIRTUAL);
    main_code.extend_from_slice(&get_ref.to_be_bytes());
    main_code.push(IRETURN);
    let c_bytes = build_class(
        &mut pool,
        "C",
        Some(ROOT),
        &[],
        &[field(0x0002, "x", "I")],
        &[
            m(0x0001, "get", "()I", 1, 1, get_code),
            m(0x0008, "main", "()I", 3, 0, main_code),
        ],
    );
    let mut vm = Vm::new(&mut pool, vec![("C".into(), c_bytes)]);
    assert_eq!(expect_int(vm.exec("C", "main", "()I").unwrap()), 7);
}

#[test]
fn virtual_dispatch_selects_runtime_class() {
    let mut pool = Pool::new();
    let b_class = pool.class("B");
    let m_ref = pool.methodref("A", "m", "()I");
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[m(0x0001, "m", "()I", 1, 0, vec![ICONST_1, IRETURN])],
    );
    let b_bytes = build_class(
        &mut pool,
        "B",
        Some("A"),
        &[],
        &[],
        &[m(0x0001, "m", "()I", 1, 0, vec![ICONST_2, IRETURN])],
    );
    let mut main_code = Vec::new();
    main_code.push(NEW);
    main_code.extend_from_slice(&b_class.to_be_bytes());
    main_code.push(INVOKEVIRTUAL);
    main_code.extend_from_slice(&m_ref.to_be_bytes());
    main_code.push(IRETURN);
    let run_bytes = build_class(
        &mut pool,
        "Run",
        Some(ROOT),
        &[],
        &[],
        &[m(0x0008, "main", "()I", 1, 0, main_code)],
    );
    let mut vm = Vm::new(
        &mut pool,
        vec![
            ("A".into(), a_bytes),
            ("B".into(), b_bytes),
            ("Run".into(), run_bytes),
        ],
    );
    // 引用声明为 A.m，接收者是 B → 虚分派选 B.m。
    assert_eq!(expect_int(vm.exec("Run", "main", "()I").unwrap()), 2);
}

#[test]
fn native_dispatch_receives_args_and_returns_value() {
    let mut pool = Pool::new();
    let stub_ref = pool.methodref("N", "stub", "(I)I");
    let mut main_code = Vec::new();
    main_code.push(ICONST_3);
    main_code.push(INVOKESTATIC);
    main_code.extend_from_slice(&stub_ref.to_be_bytes());
    main_code.push(IRETURN);
    let n_bytes = build_class(
        &mut pool,
        "N",
        Some(ROOT),
        &[],
        &[],
        &[
            native_m(0x0109, "stub", "(I)I"), // public static native
            m(0x0008, "main", "()I", 2, 0, main_code),
        ],
    );
    let mut vm = Vm::new(&mut pool, vec![("N".into(), n_bytes)]);
    vm.natives.ret = Some(Slot::Int(99));
    assert_eq!(expect_int(vm.exec("N", "main", "()I").unwrap()), 99);
    assert_eq!(vm.natives.calls.len(), 1);
    let (class, name, this, args) = &vm.natives.calls[0];
    assert_eq!((class.as_str(), name.as_str(), *this), ("N", "stub", None));
    assert_eq!(args.as_slice(), [Slot::Int(3)]);
}

#[test]
fn uncaught_null_pointer_escapes() {
    let mut pool = Pool::new();
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[m(0x0008, "main", "()I", 1, 0, vec![ACONST_NULL, ARRAYLENGTH, IRETURN])],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    expect_uncaught_fault(vm.exec("A", "main", "()I").unwrap(), Fault::NullPointer);
}

// ---- 异常与故障 -------------------------------------------------------------

#[test]
fn caught_division_by_zero_returns_handler_value() {
    let mut pool = Pool::new();
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[mh(
            0x0008,
            "main",
            "()I",
            2,
            0,
            vec![ICONST_1, ICONST_0, IDIV, IRETURN, POP, BIPUSH, 9, IRETURN],
            vec![handler(0, 3, 4, Some("java/lang/ArithmeticException"))],
        )],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    // idiv 除零 → Arithmetic 故障，处理器返回 9。
    assert_eq!(expect_int(vm.exec("A", "main", "()I").unwrap()), 9);
}

#[test]
fn caught_null_pointer_materializes_exception() {
    let mut pool = Pool::new();
    let detail = pool.fieldref("java/lang/Throwable", "detailMessage", "Ljava/lang/String;");
    let mut code = Vec::new();
    code.extend_from_slice(&[ACONST_NULL, ARRAYLENGTH, ICONST_0, IRETURN]);
    code.push(ASTORE_0); // 字节 4：处理器入口
    code.push(ALOAD_0);
    code.push(GETFIELD);
    code.extend_from_slice(&detail.to_be_bytes());
    code.push(ARETURN);
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[mh(
            0x0008,
            "main",
            "()Ljava/lang/String;",
            1,
            1, // 处理器把异常实例存入局部槽 0
            code,
            vec![handler(0, 2, 4, Some("java/lang/NullPointerException"))],
        )],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    // Fault 捕获时物化为 NPE 实例，detailMessage 可 getfield 读出。
    let result = expect_ref(vm.exec("A", "main", "()Ljava/lang/String;").unwrap());
    assert_eq!(str_text(&vm.heap, result), "null reference");
}

/// athrow 测试夹具：MyEx extends Exception 携带 int f。
struct MyExFixture {
    bytes: Vec<u8>,
    class_index: u16,
    field_ref: u16,
}

fn my_ex_fixture(pool: &mut Pool) -> MyExFixture {
    let class_index = pool.class("MyEx");
    let field_ref = pool.fieldref("MyEx", "f", "I");
    let bytes = build_class(
        pool,
        "MyEx",
        Some("java/lang/Exception"),
        &[],
        &[field(0x0002, "f", "I")],
        &[],
    );
    MyExFixture {
        bytes,
        class_index,
        field_ref,
    }
}

/// new MyEx(); ex.f = 5; throw ex;（10 字节，处理器范围 [0, 10)）。
fn athrow_prefix(fixture: &MyExFixture) -> Vec<u8> {
    let mut code = Vec::new();
    code.push(NEW);
    code.extend_from_slice(&fixture.class_index.to_be_bytes());
    code.push(DUP);
    code.push(BIPUSH);
    code.push(5);
    code.push(PUTFIELD);
    code.extend_from_slice(&fixture.field_ref.to_be_bytes());
    code.push(ATHROW);
    code
}

#[test]
fn athrow_typed_catch_reads_exception_field() {
    let mut pool = Pool::new();
    let fixture = my_ex_fixture(&mut pool);
    let mut code = athrow_prefix(&fixture);
    code.push(ASTORE_1); // 字节 10：处理器入口
    code.push(ILOAD_1);
    code.push(GETFIELD);
    code.extend_from_slice(&fixture.field_ref.to_be_bytes());
    code.push(IRETURN);
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[mh(
            0x0008,
            "main",
            "()I",
            3,
            2,
            code,
            vec![handler(0, 10, 10, Some("MyEx"))],
        )],
    );
    let mut vm = Vm::new(
        &mut pool,
        vec![("A".into(), a_bytes), ("MyEx".into(), fixture.bytes)],
    );
    // 按运行时类 MyEx 匹配处理器，读出异常字段。
    assert_eq!(expect_int(vm.exec("A", "main", "()I").unwrap()), 5);
}

#[test]
fn athrow_catch_all_finally_block() {
    let mut pool = Pool::new();
    let fixture = my_ex_fixture(&mut pool);
    let mut code = athrow_prefix(&fixture);
    code.push(POP); // 字节 10：处理器入口（catch_type 0 兜底）
    code.push(BIPUSH);
    code.push(42);
    code.push(IRETURN);
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[mh(
            0x0008,
            "main",
            "()I",
            3,
            0,
            code,
            vec![handler(0, 10, 10, None)],
        )],
    );
    let mut vm = Vm::new(
        &mut pool,
        vec![("A".into(), a_bytes), ("MyEx".into(), fixture.bytes)],
    );
    assert_eq!(expect_int(vm.exec("A", "main", "()I").unwrap()), 42);
}

#[test]
fn athrow_escape_reports_runtime_class() {
    let mut pool = Pool::new();
    let fixture = my_ex_fixture(&mut pool);
    let mut code = athrow_prefix(&fixture);
    code.push(ICONST_0); // 不可达兜底
    code.push(IRETURN);
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[m(0x0008, "main", "()I", 3, 0, code)],
    );
    let mut vm = Vm::new(
        &mut pool,
        vec![("A".into(), a_bytes), ("MyEx".into(), fixture.bytes)],
    );
    match vm.exec("A", "main", "()I").unwrap() {
        RunOutcome::Uncaught(Unwind::Thrown(exception)) => {
            let class = runtime_class_of(&vm.cs, &vm.heap, exception).unwrap();
            assert_eq!(class, vm.cs.find("MyEx").unwrap());
        }
        other => panic!("expected uncaught thrown exception, got {other:?}"),
    }
}

// ---- 数组 -------------------------------------------------------------------

#[test]
fn int_array_store_and_load() {
    let mut pool = Pool::new();
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[m(
            0x0008,
            "main",
            "()I",
            4,
            0,
            vec![
                ICONST_4, NEWARRAY, T_INT, DUP, ICONST_0, BIPUSH, 7, IASTORE, DUP, ICONST_0,
                IALOAD, IRETURN,
            ],
        )],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    assert_eq!(expect_int(vm.exec("A", "main", "()I").unwrap()), 7);
}

#[test]
fn array_index_out_of_bounds_caught() {
    let mut pool = Pool::new();
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[mh(
            0x0008,
            "main",
            "()I",
            4,
            0,
            vec![
                ICONST_4, NEWARRAY, T_INT, DUP, ICONST_5, IALOAD, IRETURN, POP, BIPUSH, 77,
                IRETURN,
            ],
            vec![handler(
                0,
                6,
                7,
                Some("java/lang/ArrayIndexOutOfBoundsException"),
            )],
        )],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    // 长度 4 读下标 5 → AIOOBE，处理器返回 77。
    assert_eq!(expect_int(vm.exec("A", "main", "()I").unwrap()), 77);
}

#[test]
fn negative_array_size_caught() {
    let mut pool = Pool::new();
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[mh(
            0x0008,
            "main",
            "()I",
            2,
            0,
            vec![
                ICONST_M1, NEWARRAY, T_INT, POP, ICONST_0, IRETURN, POP, BIPUSH, 88, IRETURN,
            ],
            vec![handler(0, 3, 6, Some("java/lang/NegativeArraySizeException"))],
        )],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    assert_eq!(expect_int(vm.exec("A", "main", "()I").unwrap()), 88);
}

#[test]
fn anewarray_of_strings_has_class_component() {
    let mut pool = Pool::new();
    let string_class = pool.class("java/lang/String");
    let mut code = Vec::new();
    code.push(ICONST_2);
    code.push(ANEWARRAY);
    code.extend_from_slice(&string_class.to_be_bytes());
    code.push(ARRAYLENGTH);
    code.push(IRETURN);
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[m(0x0008, "main", "()I", 2, 0, code)],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    assert_eq!(expect_int(vm.exec("A", "main", "()I").unwrap()), 2);
    // 合成的数组类 [Ljava/lang/String; 组件是 String。
    let string_id = vm.cs.find("java/lang/String").unwrap();
    let array_id = vm.cs.find("[Ljava/lang/String;").unwrap();
    assert_eq!(
        vm.cs.class(array_id).array_elem,
        Some(ArrayElem::Class(string_id))
    );
}

// ---- checkcast ---------------------------------------------------------------

#[test]
fn checkcast_upcast_passes() {
    let mut pool = Pool::new();
    let a_class = pool.class("A");
    let b_class = pool.class("B");
    let a_bytes = build_class(&mut pool, "A", Some(ROOT), &[], &[], &[]);
    let b_bytes = build_class(&mut pool, "B", Some("A"), &[], &[], &[]);
    let mut main_code = Vec::new();
    main_code.push(NEW);
    main_code.extend_from_slice(&b_class.to_be_bytes());
    main_code.push(CHECKCAST);
    main_code.extend_from_slice(&a_class.to_be_bytes());
    main_code.push(POP);
    main_code.push(ICONST_1);
    main_code.push(IRETURN);
    let run_bytes = build_class(
        &mut pool,
        "Run",
        Some(ROOT),
        &[],
        &[],
        &[m(0x0008, "main", "()I", 2, 0, main_code)],
    );
    let mut vm = Vm::new(
        &mut pool,
        vec![
            ("A".into(), a_bytes),
            ("B".into(), b_bytes),
            ("Run".into(), run_bytes),
        ],
    );
    // B 实例向上转型 A：通过。
    assert_eq!(expect_int(vm.exec("Run", "main", "()I").unwrap()), 1);
}

#[test]
fn checkcast_incompatible_sibling_throws_class_cast() {
    let mut pool = Pool::new();
    let b_class = pool.class("B");
    let c_class = pool.class("C");
    let a_bytes = build_class(&mut pool, "A", Some(ROOT), &[], &[], &[]);
    let b_bytes = build_class(&mut pool, "B", Some("A"), &[], &[], &[]);
    let c_bytes = build_class(&mut pool, "C", Some("A"), &[], &[], &[]);
    let mut main_code = Vec::new();
    main_code.push(NEW);
    main_code.extend_from_slice(&b_class.to_be_bytes());
    main_code.push(CHECKCAST); // B → C：不兼容兄弟 → CCE
    main_code.extend_from_slice(&c_class.to_be_bytes());
    main_code.push(POP);
    main_code.push(ICONST_0);
    main_code.push(IRETURN);
    main_code.push(POP); // 字节 9：处理器入口
    main_code.push(BIPUSH);
    main_code.push(66);
    main_code.push(IRETURN);
    let run_bytes = build_class(
        &mut pool,
        "Run",
        Some(ROOT),
        &[],
        &[],
        &[mh(
            0x0008,
            "main",
            "()I",
            2,
            0,
            main_code,
            vec![handler(0, 6, 9, Some("java/lang/ClassCastException"))],
        )],
    );
    let mut vm = Vm::new(
        &mut pool,
        vec![
            ("A".into(), a_bytes),
            ("B".into(), b_bytes),
            ("C".into(), c_bytes),
            ("Run".into(), run_bytes),
        ],
    );
    assert_eq!(expect_int(vm.exec("Run", "main", "()I").unwrap()), 66);
}

// ---- monitor ------------------------------------------------------------------

#[test]
fn monitor_enter_exit_roundtrip() {
    let mut pool = Pool::new();
    let root_class = pool.class(ROOT);
    let mut code = Vec::new();
    code.push(NEW);
    code.extend_from_slice(&root_class.to_be_bytes());
    code.push(DUP);
    code.push(MONITORENTER);
    code.push(MONITOREXIT);
    code.push(ICONST_1);
    code.push(IRETURN);
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[m(0x0008, "main", "()I", 2, 0, code)],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    assert_eq!(expect_int(vm.exec("A", "main", "()I").unwrap()), 1);
}

#[test]
fn monitor_exit_without_hold_faults() {
    let mut pool = Pool::new();
    let root_class = pool.class(ROOT);
    let mut code = Vec::new();
    code.push(NEW);
    code.extend_from_slice(&root_class.to_be_bytes());
    code.push(MONITOREXIT); // 未持有 → IllegalMonitorState
    code.push(ICONST_0);
    code.push(IRETURN);
    code.push(POP); // 字节 6：处理器入口
    code.push(BIPUSH);
    code.push(55);
    code.push(IRETURN);
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[mh(
            0x0008,
            "main",
            "()I",
            2,
            0,
            code,
            vec![handler(
                0,
                4,
                6,
                Some("java/lang/IllegalMonitorStateException"),
            )],
        )],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    assert_eq!(expect_int(vm.exec("A", "main", "()I").unwrap()), 55);
}

#[test]
fn monitor_contention_blocks_and_resumes() {
    let mut pool = Pool::new();
    let root_class = pool.class(ROOT);
    let holder_ref = pool.fieldref("T", "holder", "Ljava/lang/Object;");
    let mut setup = Vec::new();
    setup.push(NEW);
    setup.extend_from_slice(&root_class.to_be_bytes());
    setup.push(PUTSTATIC);
    setup.extend_from_slice(&holder_ref.to_be_bytes());
    setup.push(RETURN);
    let mut grab = Vec::new();
    grab.push(GETSTATIC);
    grab.extend_from_slice(&holder_ref.to_be_bytes());
    grab.push(MONITORENTER);
    grab.push(ICONST_1);
    grab.push(IRETURN);
    let t_bytes = build_class(
        &mut pool,
        "T",
        Some(ROOT),
        &[],
        &[field(0x0008, "holder", "Ljava/lang/Object;")],
        &[
            m(0x0008, "setup", "()V", 1, 0, setup),
            m(0x0008, "grab", "()I", 1, 0, grab),
        ],
    );
    let mut vm = Vm::new(&mut pool, vec![("T".into(), t_bytes)]);
    vm.exec("T", "setup", "()V").unwrap();
    let Slot::Ref(holder) = vm.cs.class(vm.cs.find("T").unwrap()).statics[0] else {
        panic!("holder not populated");
    };
    // 他线程持有 → grab 阻塞在 monitorenter（pc 停在指令上）。
    let rival = ThreadId(2);
    assert_eq!(vm.monitors.enter(holder, rival), EnterOutcome::Acquired(1));
    let mut frames = Vec::new();
    match vm.exec_frames("T", "grab", "()I", &mut frames).unwrap() {
        RunOutcome::BlockedOnMonitor(blocked) => assert_eq!(blocked, holder),
        other => panic!("expected monitor blocking, got {other:?}"),
    }
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].pc, 1);
    // 释放后同帧续跑：monitorenter 重试成功。
    vm.monitors.exit(holder, rival).unwrap();
    assert_eq!(expect_int(vm.run_frames(&mut frames, None).unwrap()), 1);
}

// ---- jsr/ret 与 switch --------------------------------------------------------

#[test]
fn jsr_ret_subroutine_returns() {
    // jsr 压返回地址（指令索引），子程序 astore + ret 跳回主流程。
    let mut code = Vec::new();
    code.extend_from_slice(&branch(JSR, 7)); // 字节 0 → 字节 7
    code.push(ICONST_1); // 字节 3
    code.extend_from_slice(&branch(GOTO, 6)); // 字节 4 → 字节 10
    code.push(ASTORE_1); // 字节 7：子程序
    code.push(RET);
    code.push(1); // 字节 8-9
    code.push(IRETURN); // 字节 10
    let mut pool = Pool::new();
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[m(0x0008, "main", "()I", 2, 2, code)],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    assert_eq!(expect_int(vm.exec("A", "main", "()I").unwrap()), 1);
}

/// tableswitch 字节：opcode 位于字节 1（前置 1 字节压栈指令），
/// 2 字节对齐填充后 base=4；目标偏移相对 opcode 起点。
fn tableswitch_bytes(low: i32, high: i32, default: i32, targets: &[i32]) -> Vec<u8> {
    let mut out = vec![TABLESWITCH, 0, 0]; // opcode + 填充（字节 2-3）
    out.extend_from_slice(&default.to_be_bytes());
    out.extend_from_slice(&low.to_be_bytes());
    out.extend_from_slice(&high.to_be_bytes());
    for target in targets {
        out.extend_from_slice(&target.to_be_bytes());
    }
    out
}

/// switch 公共体（字节 24 起）：default/命中 1/命中 2 → 0/1/2。
fn switch_body() -> [u8; 6] {
    [ICONST_0, IRETURN, ICONST_1, IRETURN, ICONST_2, IRETURN]
}

#[test]
fn tableswitch_dispatches_and_defaults() {
    // low=1, high=2：值 2 命中第二项；值 5 越界走 default。
    let mut pool = Pool::new();
    let mut two = Vec::new();
    two.push(ICONST_2);
    two.extend_from_slice(&tableswitch_bytes(1, 2, 23, &[25, 27]));
    two.extend_from_slice(&switch_body());
    let mut five = Vec::new();
    five.push(ICONST_5);
    five.extend_from_slice(&tableswitch_bytes(1, 2, 23, &[25, 27]));
    five.extend_from_slice(&switch_body());
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[
            m(0x0008, "two", "()I", 1, 0, two),
            m(0x0008, "five", "()I", 1, 0, five),
        ],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    assert_eq!(expect_int(vm.exec("A", "two", "()I").unwrap()), 2);
    assert_eq!(expect_int(vm.exec("A", "five", "()I").unwrap()), 0);
}

/// lookupswitch 字节：opcode 位于字节 2（前置 2 字节 bipush），
/// 1 字节填充后 base=4；目标偏移相对 opcode 起点。
fn lookupswitch_bytes(default: i32, pairs: &[(i32, i32)]) -> Vec<u8> {
    let mut out = vec![LOOKUPSWITCH, 0]; // opcode + 填充（字节 3）
    out.extend_from_slice(&default.to_be_bytes());
    out.extend_from_slice(&(pairs.len() as i32).to_be_bytes());
    for (match_value, target) in pairs {
        out.extend_from_slice(&match_value.to_be_bytes());
        out.extend_from_slice(&target.to_be_bytes());
    }
    out
}

/// lookupswitch 公共体（字节 28 起）：default/命中 10/命中 20 → -1/1/2。
fn lookup_body() -> [u8; 6] {
    [ICONST_M1, IRETURN, ICONST_1, IRETURN, ICONST_2, IRETURN]
}

#[test]
fn lookupswitch_dispatches_and_defaults() {
    // 匹配表 {10→1, 20→2}：键 20 命中；键 7 走 default -1。
    let mut pool = Pool::new();
    let mut key20 = Vec::new();
    key20.push(BIPUSH);
    key20.push(20);
    key20.extend_from_slice(&lookupswitch_bytes(26, &[(10, 28), (20, 30)]));
    key20.extend_from_slice(&lookup_body());
    let mut key7 = Vec::new();
    key7.push(BIPUSH);
    key7.push(7);
    key7.extend_from_slice(&lookupswitch_bytes(26, &[(10, 28), (20, 30)]));
    key7.extend_from_slice(&lookup_body());
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[
            m(0x0008, "key20", "()I", 1, 0, key20),
            m(0x0008, "key7", "()I", 1, 0, key7),
        ],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    assert_eq!(expect_int(vm.exec("A", "key20", "()I").unwrap()), 2);
    assert_eq!(expect_int(vm.exec("A", "key7", "()I").unwrap()), -1);
}

#[test]
fn multianewarray_allocates_and_stores() {
    // new int[2][3]：arr[0][1] = 42 后读回；外层长度 2。
    let mut pool = Pool::new();
    let int_int = pool.class("[[I");
    let mut store = Vec::new();
    store.push(ICONST_2);
    store.push(ICONST_3);
    store.push(MULTIANEWARRAY);
    store.extend_from_slice(&int_int.to_be_bytes());
    store.push(2); // dims
    store.push(DUP);
    store.push(ICONST_0);
    store.push(AALOAD); // arr[0]
    store.push(DUP);
    store.push(ICONST_1);
    store.push(BIPUSH);
    store.push(42);
    store.push(IASTORE); // arr[0][1] = 42
    store.push(ICONST_1);
    store.push(IALOAD);
    store.push(IRETURN);
    let mut length = Vec::new();
    length.push(ICONST_2);
    length.push(ICONST_3);
    length.push(MULTIANEWARRAY);
    length.extend_from_slice(&int_int.to_be_bytes());
    length.push(2);
    length.push(ARRAYLENGTH);
    length.push(IRETURN);
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[
            m(0x0008, "store", "()I", 5, 0, store),
            m(0x0008, "length", "()I", 2, 0, length),
        ],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    assert_eq!(expect_int(vm.exec("A", "store", "()I").unwrap()), 42);
    assert_eq!(expect_int(vm.exec("A", "length", "()I").unwrap()), 2);
}

// ---- 预算与驻留 -------------------------------------------------------------

#[test]
fn budget_exhaustion_is_resumable() {
    let mut pool = Pool::new();
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[m(0x0008, "main", "()I", 1, 0, vec![GOTO, 0, 0])], // 自旋
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    let mut frames = Vec::new();
    {
        let class_id = vm.cs.load_class("A").unwrap();
        let mut machine = vm.machine();
        enter(&mut machine, &mut frames, class_id, "main", "()I", Vec::new()).unwrap();
    }
    // 两轮限额：均耗尽且状态可续跑（pc 归零、帧保留）。
    for _ in 0..2 {
        match vm.run_frames(&mut frames, Some(50)).unwrap() {
            RunOutcome::BudgetExhausted => {}
            other => panic!("expected budget exhaustion, got {other:?}"),
        }
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].pc, 0);
    }
}

#[test]
fn ldc_string_interned_across_runs() {
    let mut pool = Pool::new();
    let abc = pool.string("abc");
    assert!(abc <= 0xff, "ldc 池索引须 fits u8");
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[m(
            0x0008,
            "main",
            "()Ljava/lang/String;",
            1,
            0,
            vec![LDC, abc as u8, ARETURN],
        )],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    let first = expect_ref(vm.exec("A", "main", "()Ljava/lang/String;").unwrap());
    let second = expect_ref(vm.exec("A", "main", "()Ljava/lang/String;").unwrap());
    // 同内容字面量驻留为同一 String 对象。
    assert_eq!(first, second);
    assert_eq!(str_text(&vm.heap, first), "abc");
}
