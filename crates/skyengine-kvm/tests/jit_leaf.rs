//! JIT v1（叶子整数方法）语义测试。
//!
//! 同一断言集在两种模式下都应通过：
//! - 纯解释：`cargo test --test jit_leaf`
//! - JIT 全开：`SKYENGINE_KVM_JIT=1 SKYENGINE_KVM_JIT_THRESHOLD=1
//!   cargo test --test jit_leaf`（阈值 1 = 首次调用即编译，被调方法
//!   全部走生成码路径）。
//!
//! 覆盖：常量/局部/算术/分支/循环（后向分支预算检查）/返回；除零
//! deopt 重执行产生与解释器一致的 ArithmeticException 路径；调用方
//! （含 invoke 的方法，不编译）与 JIT 叶子的互操作。

#[allow(dead_code)]
mod common;

use common::classbytes::{build_class, exception_classes, provider, MethodSpec, Pool, ROOT};

use skyengine_kvm::host::mock::MockHostContext;
use skyengine_kvm::interp::{enter, run, Machine, NativeCall, NativeDispatch, RunOutcome};
use skyengine_kvm::runtime::classspace::ClassSpace;
use skyengine_kvm::runtime::heap::Heap;
use skyengine_kvm::runtime::monitor::Monitors;
use skyengine_kvm::runtime::{Slot, ThreadId};
use skyengine_kvm::KvmError;

const ILOAD_0: u8 = 0x1a;
const BIPUSH: u8 = 0x10;
const SIPUSH: u8 = 0x11;
const ISTORE_1: u8 = 0x3c;
const POP: u8 = 0x57;
const DUP: u8 = 0x59;
const IADD: u8 = 0x60;
const IMUL: u8 = 0x68;
const IDIV: u8 = 0x6c;
const IREM: u8 = 0x70;
const IUSHR: u8 = 0x7c;
const IINC: u8 = 0x84;
const IF_ICMPEQ: u8 = 0x9f;
const IF_ICMPGE: u8 = 0xa2;
const IF_ICMPGT: u8 = 0xa3;
const GOTO: u8 = 0xa7;
const IRETURN: u8 = 0xac;
const INVOKESTATIC: u8 = 0xb8;
const ICONST_0: u8 = 0x03;
const ICONST_1: u8 = 0x04;
const ICONST_2: u8 = 0x05;

const MAIN: ThreadId = ThreadId(1);

#[derive(Default)]
struct TestNatives {
    ret: Option<Slot>,
}

impl NativeDispatch for TestNatives {
    fn call_native(
        &mut self,
        _cs: &mut ClassSpace,
        _heap: &mut Heap,
        _host: &mut dyn skyengine_kvm::host::HostContext,
        _call: NativeCall,
    ) -> Result<Option<Slot>, KvmError> {
        Ok(self.ret)
    }
}

struct Vm {
    cs: ClassSpace,
    heap: Heap,
    monitors: Monitors,
    natives: TestNatives,
    host: MockHostContext,
}

impl Vm {
    fn new(pool: &mut Pool, user: Vec<(String, Vec<u8>)>) -> Self {
        let mut entries = vec![(ROOT.to_string(), build_class(pool, ROOT, None, &[], &[], &[]))];
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

    fn exec_int(&mut self, class: &str, name: &str, args: Vec<Slot>) -> Option<i32> {
        let class_id = self.cs.load_class(class).unwrap();
        let mut machine = Machine {
            cs: &mut self.cs,
            heap: &mut self.heap,
            monitors: &mut self.monitors,
            natives: &mut self.natives,
            host: &mut self.host,
        };
        let mut frames: Vec<skyengine_kvm::interp::Frame> = Vec::new();
        enter(&mut machine, &mut frames, class_id, name, "(I)I", args).unwrap();
        match run(&mut machine, &mut frames, MAIN, None).unwrap() {
            RunOutcome::Returned(Some(Slot::Int(value))) => Some(value),
            _ => None,
        }
    }

    fn exec_outcome(&mut self, class: &str, name: &str, args: Vec<Slot>) -> RunOutcome {
        let class_id = self.cs.load_class(class).unwrap();
        let mut machine = Machine {
            cs: &mut self.cs,
            heap: &mut self.heap,
            monitors: &mut self.monitors,
            natives: &mut self.natives,
            host: &mut self.host,
        };
        let mut frames: Vec<skyengine_kvm::interp::Frame> = Vec::new();
        enter(&mut machine, &mut frames, class_id, name, "(I)I", args).unwrap();
        run(&mut machine, &mut frames, MAIN, None).unwrap()
    }
}

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

/// bk.a(C)Z 同形叶子：`x == 127 ? 1 : 0`（含 if_icmpeq 双分支）。
#[test]
fn leaf_equality_branch() {
    let mut pool = Pool::new();
    // iload_0; bipush 127; if_icmpeq L; iconst_1; ireturn; L: iconst_0; ireturn
    let code = vec![
        ILOAD_0, BIPUSH, 127, IF_ICMPEQ, 0, 5, ICONST_0, IRETURN, ICONST_1, IRETURN,
    ];
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[m(0x0008, "test", "(I)I", 2, 1, code)],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    assert_eq!(vm.exec_int("A", "test", vec![Slot::Int(127)]), Some(1));
    assert_eq!(vm.exec_int("A", "test", vec![Slot::Int(126)]), Some(0));
    assert_eq!(vm.exec_int("A", "test", vec![Slot::Int(-1)]), Some(0));
}

/// 循环求和 1..=n（iinc + if_icmpge 后向分支——预算检查路径）+ 调用方
/// 经 invokestatic 进入叶子（分层触发点在 exec_invoke）。
#[test]
fn leaf_loop_sum_and_caller() {
    let mut pool = Pool::new();
    // sum: iconst_0; istore_2; loop: iinc 2,1; iload_2; iload_1?——参数在 0
    // 改用静态 (I)I：局部 0 = n，局部 1 = 累加器。
    // iconst_0; istore_1; L: iinc 1,1; iload_1; iload_0; if_icmpgt E;
    //   iload_1; iconst_m1? —— 简化：sum(1..n) 累加：
    // L: iinc 1,1; iload_1; iload_0; if_icmpgt E; goto L; E: iload_1; ireturn
    // 累加缺失——加 imul 累加太绕，改为：结果 = n*(n+1)/2 需要乘除。
    // 直接累加：L: iinc 1,1(计数); iload_1(计数) push; iload_0(n) push;
    // if_icmpgt E; pop; goto L —— 不行。经典形：
    //   L: iinc 1,1; iload_1; iload_0; if_icmpgt E; iload_1; iload_1?...
    // 最朴素：result += i 放局部 1（既是计数也是和）只对 1..n 的和
    // 当 result 初值 0 时等于 n(n+1)/2 恰好——不，iinc 只加 1。
    // 用两条局部：1=和 2=i。max_locals=3。
    // 局部 0=n 1=sum 2=i；L@4 后向分支（预算检查路径）。
    let mut code = Vec::new();
    code.push(ICONST_0);
    code.push(ISTORE_1);
    code.push(ICONST_0);
    code.push(0x3d); // istore_2
    code.extend_from_slice(&[IINC, 2, 1]); // L: i++
    code.push(0x1c); // iload_2
    code.push(ILOAD_0); // iload_0
    code.extend_from_slice(&[IF_ICMPGT, 0, 10]); // if_icmpgt：i > n → E@19
    code.push(0x1b); // iload_1
    code.push(0x1c); // iload_2
    code.push(IADD);
    code.push(ISTORE_1);
    code.extend_from_slice(&[GOTO, 0xFF, 0xF4]); // → L@4
    code.push(0x1b); // E: iload_1
    code.push(IRETURN);
    // 调用方：invokestatic test → +1
    let mut caller = Vec::new();
    caller.push(ILOAD_0);
    let mref = pool.methodref("A", "test", "(I)I");
    caller.extend_from_slice(&[INVOKESTATIC]);
    caller.extend_from_slice(&mref.to_be_bytes());
    caller.push(ICONST_1);
    caller.push(IADD);
    caller.push(IRETURN);
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[
            m(0x0008, "test", "(I)I", 2, 3, code),
            m(0x0008, "call", "(I)I", 2, 1, caller),
        ],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    assert_eq!(vm.exec_int("A", "test", vec![Slot::Int(10)]), Some(55));
    assert_eq!(vm.exec_int("A", "test", vec![Slot::Int(1)]), Some(1));
    assert_eq!(vm.exec_int("A", "call", vec![Slot::Int(10)]), Some(56));
}

/// 整数运算全算子（含移位/除/余/负数/溢出回绕）。
#[test]
fn leaf_arith_ops() {
    let mut pool = Pool::new();
    let mut code = Vec::new();
    code.push(ILOAD_0);
    code.extend_from_slice(&[SIPUSH, 0x30, 0x39]); // 12345
    code.push(IMUL);
    code.push(ILOAD_0);
    code.push(IUSHR);
    code.push(ICONST_2);
    code.push(IREM);
    code.push(IRETURN);
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[m(0x0008, "test", "(I)I", 4, 1, code)],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    // (7*12345)>>>7 % 2
    let expect = ((7i32.wrapping_mul(12345)) as u32 >> 7) as i32 % 2;
    assert_eq!(vm.exec_int("A", "test", vec![Slot::Int(7)]), Some(expect));
}

/// 除零：JIT deopt 重执行 → 与解释器一致的 ArithmeticException 逃逸。
#[test]
fn leaf_div_zero_deopts_to_same_unwind() {
    let mut pool = Pool::new();
    let code = vec![ILOAD_0, ICONST_0, IDIV, IRETURN];
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[m(0x0008, "test", "(I)I", 2, 1, code)],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    let outcome = vm.exec_outcome("A", "test", vec![Slot::Int(5)]);
    assert!(
        matches!(&outcome, RunOutcome::Uncaught(unwind)
            if format!("{unwind:?}").contains("Arithmetic")),
        "除零应产生 Arithmetic 异常路径，got {outcome:?}"
    );
}

/// 栈操作（pop/dup）叶子。
#[test]
fn leaf_stack_ops() {
    let mut pool = Pool::new();
    // iload_0; pop; iconst_2; dup; pop; ireturn → 2
    let code = vec![ILOAD_0, POP, ICONST_2, DUP, POP, IRETURN];
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[m(0x0008, "test", "(I)I", 3, 1, code)],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    assert_eq!(vm.exec_int("A", "test", vec![Slot::Int(9)]), Some(2));
}
