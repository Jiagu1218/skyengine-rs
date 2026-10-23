//! superopcode 融合测试：解码期相邻指令对融合（`Insn::Fused` + 蹦床）
//! 的结构约束与关键控制流路径。
//!
//! 覆盖面：
//! - 结构：融合对的产生、蹦床槽形态、跳转目标/处理器边界不被融合；
//! - 执行：复合尾故障的处理器匹配（故障 pc 折叠到复合槽）、invoke
//!   压帧经蹦床续跑、clinit 触发复合首的无副作用重执行、jsr 返回
//!   地址落在蹦床上。

#[allow(dead_code)]
mod common;

use common::classbytes::{
    build_class, exception_classes, provider, FieldSpec, MethodSpec, Pool, ROOT,
};

use skyengine_kvm::classfile::code::Insn;
use skyengine_kvm::host::mock::MockHostContext;
use skyengine_kvm::interp::{enter, run, Machine, NativeCall, NativeDispatch, RunOutcome};
use skyengine_kvm::runtime::classspace::ClassSpace;
use skyengine_kvm::runtime::heap::Heap;
use skyengine_kvm::runtime::monitor::Monitors;
use skyengine_kvm::runtime::{Slot, ThreadId};
use skyengine_kvm::KvmError;

// 操作码助记（仅测试用到的）。
const ICONST_0: u8 = 0x03;
const BIPUSH: u8 = 0x10;
const ILOAD_1: u8 = 0x1b;
const ASTORE_1: u8 = 0x4c;
const IADD: u8 = 0x60;

const IINC: u8 = 0x84;
const IF_ICMPGE: u8 = 0xa2;
const GOTO: u8 = 0xa7;
const JSR: u8 = 0xa8;
const RET: u8 = 0xa9;
const IRETURN: u8 = 0xac;
const RETURN: u8 = 0xb1;
const GETSTATIC: u8 = 0xb2;
const PUTSTATIC: u8 = 0xb3;
const INVOKEVIRTUAL: u8 = 0xb6;
const NEW: u8 = 0xbb;
const NEWARRAY: u8 = 0xbc;
const ARRAYLENGTH: u8 = 0xbe;

/// newarray 的 atype：T_INT。
const T_INT: u8 = 10;

const MAIN: ThreadId = ThreadId(1);

// ---- 测试 VM 夹具（与 interp.rs 同构的最小集） ---------------------------

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

    fn exec(&mut self, class: &str, name: &str, desc: &str) -> Result<RunOutcome, KvmError> {
        let class_id = self.cs.load_class(class)?;
        let mut machine = Machine {
            cs: &mut self.cs,
            heap: &mut self.heap,
            monitors: &mut self.monitors,
            natives: &mut self.natives,
            host: &mut self.host,
        };
        let mut frames: Vec<skyengine_kvm::interp::Frame> = Vec::new();
        enter(&mut machine, &mut frames, class_id, name, desc, Vec::new())?;
        run(&mut machine, &mut frames, MAIN, None)
    }
}

fn expect_int(outcome: RunOutcome) -> i32 {
    match outcome {
        RunOutcome::Returned(Some(Slot::Int(value))) => value,
        other => panic!("expected int return, got {other:?}"),
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

// ---- 结构 -----------------------------------------------------------------

/// 线性序列全量融合：`iconst_1; iconst_2; iadd; ireturn` →
/// 两个复合槽 + 两个蹦床槽（末对蹦床目标越界但 ireturn 永不落入），
/// 数组长度不变。
#[test]
fn linear_sequence_fuses_into_compound_slots() {
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
            vec![0x04, 0x05, IADD, IRETURN], // iconst_1; iconst_2; iadd; ireturn
        )],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    let a_id = vm.cs.load_class("A").unwrap();
    let body = vm.cs.class(a_id).methods[0].body.as_ref().unwrap();
    assert_eq!(body.insns.len(), 4, "数组长度不变（索引引用保持有效）");
    assert!(matches!(
        &body.insns[0],
        Insn::Fused { a, b } if matches!(**a, Insn::PushInt(1)) && matches!(**b, Insn::PushInt(2))
    ));
    assert!(matches!(body.insns[1], Insn::Goto(2)), "蹦床跳到复合后继");
    assert!(matches!(
        &body.insns[2],
        Insn::Fused { a, b } if matches!(**a, Insn::Bin { .. }) && matches!(**b, Insn::Return(Some(_)))
    ));
    assert!(matches!(body.insns[3], Insn::Goto(4)), "死蹦床（ireturn 永不落入）");
    let _ = &body;
    assert_eq!(expect_int(vm.exec("A", "main", "()I").unwrap()), 3);
}

/// 跳转目标不被融合为复合尾：循环中 if_icmpge 落点（exit）与 goto 落点
///（loop 头 iinc）都不作复合尾；两者作复合头合法（跳入即整对执行）。
#[test]
fn branch_targets_stay_standalone() {
    let mut pool = Pool::new();
    // loop: iinc 1,1; iload_1; bipush 10; if_icmpge exit; goto loop;
    // exit: iload_1; ireturn
    // 字节布局：iinc@0(3B) iload@3 bipush@4(2B) if@6(3B, +6→12)
    //           goto@9(3B, -9→0) iload@12 ireturn@13
    let mut code = Vec::new();
    code.extend_from_slice(&[IINC, 1, 1]);
    code.push(ILOAD_1);
    code.extend_from_slice(&[BIPUSH, 10]);
    code.extend_from_slice(&[IF_ICMPGE, 0, 6]);
    code.extend_from_slice(&[GOTO, 0xff, (-9i8) as u8]);
    code.push(ILOAD_1);
    code.push(IRETURN);
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[m(0x0008, "main", "()I", 2, 2, code)],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    let a_id = vm.cs.load_class("A").unwrap();
    let body = vm.cs.class(a_id).methods[0].body.as_ref().unwrap();
    // 指令索引：0=iinc 1=iload 2=bipush 3=if 4=goto 5=iload 6=ireturn。
    // 跳转目标：goto→idx0（loop 头）、if→idx5（exit）→ 保护 {0,5}。
    // (0,1) 融合（goto 落点作复合头）；(2,3) 融合；(4,5) goto 非复合首
    // → 不融合；(5,6) 末对融合（if 落点作复合头）。
    assert!(matches!(&body.insns[0], Insn::Fused { .. }), "goto 落点作复合头（整对执行）");
    assert!(matches!(&body.insns[2], Insn::Fused { .. }), "bipush+if 融合");
    assert!(matches!(body.insns[4], Insn::Goto(0)));
    assert!(matches!(&body.insns[5], Insn::Fused { .. }), "if 落点作复合头（整对执行）");
    let _ = &body;
    assert_eq!(expect_int(vm.exec("A", "main", "()I").unwrap()), 10);
}

/// 复合尾故障的处理器匹配：newarray 负长度故障发生在复合对的 `b` 中，
/// 故障 pc 折叠到复合槽。两个对照场景：
/// - 处理器区间覆盖两指令 → 融合发生，故障按折叠 pc 正确捕获；
/// - 区间只覆盖首指令（end 边界落在对中间）→ 融合被边界保护阻止，
///   故障按原语义不被捕获（逃逸出方法）。
#[test]
fn fault_in_fused_tail_matches_handler() {
    // 码体：bipush -3; newarray int; arraylength; ireturn;
    //       handler 入口：bipush 7; ireturn
    let build = |pool: &mut Pool, end_pc: u16| {
        let mut code = Vec::new();
        code.extend_from_slice(&[BIPUSH, (-3i8) as u8]); // 0..2
        code.extend_from_slice(&[NEWARRAY, T_INT]); // 2..4
        code.push(ARRAYLENGTH); // 4
        code.push(IRETURN); // 5
        code.extend_from_slice(&[BIPUSH, 7]); // 6..8（handler 入口）
        code.push(IRETURN); // 8
        build_class(
            pool,
            "A",
            Some(ROOT),
            &[],
            &[],
            &[MethodSpec {
                access: 0x0008,
                name: "main",
                desc: "()I",
                code: Some((2, 0, code)),
                handlers: vec![common::classbytes::HandlerSpec {
                    start_pc: 0,
                    end_pc,
                    handler_pc: 6,
                    catch_class: Some("java/lang/NegativeArraySizeException"),
                }],
            }],
        )
    };

    // 场景一：区间 [0,4) 覆盖 bipush+newarray 两指令 → 融合，捕获 → 7。
    let mut pool = Pool::new();
    let a_bytes = build(&mut pool, 4);
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    let a_id = vm.cs.load_class("A").unwrap();
    let body = vm.cs.class(a_id).methods[0].body.as_ref().unwrap();
    // 指令索引：0=bipush 1=newarray 2=arraylength 3=ireturn 4=bipush 5=ireturn。
    assert!(
        matches!(&body.insns[0], Insn::Fused { b, .. } if matches!(**b, Insn::NewArray(_))),
        "区间覆盖两指令 → bipush+newarray 融合"
    );
    let _ = &body;
    assert_eq!(expect_int(vm.exec("A", "main", "()I").unwrap()), 7);

    // 场景二：区间 [0,2) 只覆盖 bipush → end 边界（idx1）保护，
    // 不融合；故障发生在 newarray（原 idx1）不被捕获 → Uncaught。
    let mut pool = Pool::new();
    let a_bytes = build(&mut pool, 2);
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    let a_id = vm.cs.load_class("A").unwrap();
    let body = vm.cs.class(a_id).methods[0].body.as_ref().unwrap();
    assert!(
        !matches!(&body.insns[0], Insn::Fused { .. }),
        "处理器 end 边界槽不作复合尾 → 对不融合"
    );
    let _ = &body;
    assert!(matches!(
        vm.exec("A", "main", "()I").unwrap(),
        RunOutcome::Uncaught(_)
    ));
}

// ---- 控制流路径 -------------------------------------------------------------

/// invoke 不作复合尾（性能取舍，见解码期 fuse_second_ok 注释）：
/// getstatic+invokevirtual 保持独立执行，行为不受融合影响。
#[test]
fn invoke_pairs_stay_unfused() {
    let mut pool = Pool::new();
    let b_class = pool.class("B");
    let s_ref = pool.fieldref("A", "s", "LB;");
    let g_ref = pool.methodref("B", "g", "()I");
    let g_body = vec![0x08, IRETURN]; // iconst_5; ireturn
    let b_bytes = build_class(
        &mut pool,
        "B",
        Some(ROOT),
        &[],
        &[],
        &[m(0x0001, "g", "()I", 1, 1, g_body)],
    );
    // A.<clinit>: new B; dup; putstatic s; return
    let mut clinit = Vec::new();
    clinit.push(NEW);
    clinit.extend_from_slice(&b_class.to_be_bytes());
    clinit.push(0x59); // dup
    clinit.extend_from_slice(&[PUTSTATIC]);
    clinit.extend_from_slice(&s_ref.to_be_bytes());
    clinit.push(RETURN);
    // A.main: getstatic s; invokevirtual g; ireturn
    let mut main = Vec::new();
    main.extend_from_slice(&[GETSTATIC]);
    main.extend_from_slice(&s_ref.to_be_bytes());
    main.extend_from_slice(&[INVOKEVIRTUAL]);
    main.extend_from_slice(&g_ref.to_be_bytes());
    main.push(IRETURN);
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[FieldSpec { access: 0x0008, name: "s", desc: "LB;", constant: None }],
        &[
            m(0x0008, "<clinit>", "()V", 3, 0, clinit),
            m(0x0008, "main", "()I", 1, 0, main),
        ],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes), ("B".into(), b_bytes)]);
    let a_id = vm.cs.load_class("A").unwrap();
    let body = vm.cs.class(a_id).methods[1].body.as_ref().unwrap();
    assert!(
        !matches!(&body.insns[0], Insn::Fused { .. }),
        "invoke 作复合尾的配对不融合（蹦床开销取舍）"
    );
    let _ = &body;
    assert_eq!(expect_int(vm.exec("A", "main", "()I").unwrap()), 5);
}

/// 复合首是 getstatic 且目标类未初始化：Pushed 早退（无栈副作用）→
/// clinit 执行 → 复合重执行 → 正常取值续跑复合尾。
#[test]
fn clinit_on_fused_getstatic_head_reexecutes_compound() {
    let mut pool = Pool::new();
    let v_ref = pool.fieldref("B", "v", "I");
    // B.<clinit>: bipush 9; putstatic v; return
    let mut clinit = Vec::new();
    clinit.extend_from_slice(&[BIPUSH, 9]);
    clinit.extend_from_slice(&[PUTSTATIC]);
    clinit.extend_from_slice(&v_ref.to_be_bytes());
    clinit.push(RETURN);
    let b_bytes = build_class(
        &mut pool,
        "B",
        Some(ROOT),
        &[],
        &[FieldSpec { access: 0x0008, name: "v", desc: "I", constant: None }],
        &[m(0x0008, "<clinit>", "()V", 1, 0, clinit)],
    );
    // A.main: getstatic v; ireturn（融合对：getstatic 首可、ireturn 尾可）。
    let mut main = Vec::new();
    main.extend_from_slice(&[GETSTATIC]);
    main.extend_from_slice(&v_ref.to_be_bytes());
    main.push(IRETURN);
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[m(0x0008, "main", "()I", 1, 0, main)],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes), ("B".into(), b_bytes)]);
    assert_eq!(expect_int(vm.exec("A", "main", "()I").unwrap()), 9);
}

/// jsr 作复合尾：返回地址 = 蹦床槽，ret 经蹦床落到复合后继；
/// jsr 目标（子程序入口）保持独立可入。
#[test]
fn jsr_in_fused_tail_returns_via_trampoline() {
    let mut pool = Pool::new();
    // iconst_0; jsr sub; bipush 33; ireturn; sub: astore_1; ret 1
    // 字节布局：iconst@0 jsr@1(+6→7) bipush@4(2B) ireturn@6
    //           astore_1@7 ret@8(2B)
    let mut code = Vec::new();
    code.push(ICONST_0);
    code.extend_from_slice(&[JSR, 0, 6]);
    code.extend_from_slice(&[BIPUSH, 33]);
    code.push(IRETURN);
    code.push(ASTORE_1);
    code.extend_from_slice(&[RET, 1]);
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[m(0x0008, "main", "()I", 1, 2, code)],
    );
    let mut vm = Vm::new(&mut pool, vec![("A".into(), a_bytes)]);
    let a_id = vm.cs.load_class("A").unwrap();
    let body = vm.cs.class(a_id).methods[0].body.as_ref().unwrap();
    // 指令索引：0=iconst 1=jsr 2=bipush 3=ireturn 4=astore 5=ret。
    // jsr 目标 idx4 受保护；(0,1) 融合（jsr 尾可）、(2,3) 融合、
    // (4,5) 因目标保护不融合（ret 也非"永不落入蹦床"族）。
    assert!(matches!(&body.insns[0], Insn::Fused { b, .. } if matches!(**b, Insn::Jsr(_))));
    assert!(matches!(body.insns[1], Insn::Goto(2)), "返回地址落点为蹦床");
    assert!(!matches!(&body.insns[4], Insn::Fused { .. }), "jsr 目标保持独立");
    let _ = &body;
    assert_eq!(expect_int(vm.exec("A", "main", "()I").unwrap()), 33);
}
