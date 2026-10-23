//! 运行时基座测试：类空间加载-布局-解析（T006）与堆（字符串驻留/数组/实例）。
//!
//! class 字节经共享构造器手构（见 `common/classbytes.rs`），
//! 聚焦布局/防环/解析逻辑本身；语料级验证（jc.jar 全量加载）
//! 在 bootstrap 类（T010）就绪后进行。

// 共享夹具：本文件仅用 classbytes 构造器，其余（MRP/zip）允许未用。
#[allow(dead_code)]
mod common;

use common::classbytes::{build_class, provider, FieldSpec, MethodSpec, Pool, ROOT};

use skyengine_kvm::classfile::code::Linker;
use skyengine_kvm::runtime::classspace::{
    ArrayElem, ClassSpace, InitState,
};
use skyengine_kvm::runtime::heap::Heap;
use skyengine_kvm::runtime::object::{ArrayData, ElemType, Payload};
use skyengine_kvm::runtime::{ClassId, ObjRef, Slot};
use skyengine_kvm::KvmError;

// 操作码助记（仅测试用到的）。
const GETSTATIC: u8 = 0xb2;
const GETFIELD: u8 = 0xb4;
const ANEWARRAY: u8 = 0xbd;
const POP: u8 = 0x57;
const IRETURN: u8 = 0xac;
const RETURN: u8 = 0xb1;

/// 解码产物首逻辑指令：superopcode 融合后首槽可能是
/// `Fused { a: 原首指令, .. }`，断言目标指令时经此下钻。
fn head_insn(insn: &skyengine_kvm::classfile::code::Insn) -> &skyengine_kvm::classfile::code::Insn {
    match insn {
        skyengine_kvm::classfile::code::Insn::Fused { a, .. } => head_insn(a),
        other => other,
    }
}

// ---- 类空间：布局与字段解析 -------------------------------------------

/// 继承链字段布局：绝对槽位、long 双槽、静态存储、ConstantValue。
#[test]
fn field_layout_and_resolution() {
    let mut pool = Pool::new();
    let r_const = pool.integer(7);
    // T010 起 java/lang/Object 由 bootstrap 注册表提供（无测试字段），
    // 夹具静态字段 ri 放在中间类 R 上。
    let r_bytes = build_class(
        &mut pool,
        "R",
        Some(ROOT),
        &[],
        &[FieldSpec {
            access: 0x0008, // static
            name: "ri",
            desc: "I",
            constant: Some(r_const),
        }],
        &[],
    );
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some("R"),
        &[],
        &[
            FieldSpec { access: 0, name: "aI", desc: "I", constant: None },
            FieldSpec { access: 0, name: "aL", desc: "J", constant: None },
            FieldSpec { access: 0x0008, name: "aS", desc: "I", constant: None },
        ],
        &[],
    );
    let b_const = pool.long(0x1122_3344_5566_7788);
    let b_bytes = build_class(
        &mut pool,
        "B",
        Some("A"),
        &[],
        &[
            FieldSpec { access: 0x0008, name: "bL", desc: "J", constant: Some(b_const) },
            FieldSpec { access: 0, name: "bI", desc: "I", constant: None },
        ],
        &[],
    );
    // C 的字节码引用父链各层字段（触发解码期链接）。
    let mut code = Vec::new();
    code.push(GETSTATIC);
    code.extend_from_slice(&pool.fieldref("R", "ri", "I").to_be_bytes());
    code.push(POP);
    code.push(GETSTATIC);
    code.extend_from_slice(&pool.fieldref("A", "aS", "I").to_be_bytes());
    code.push(POP);
    code.push(GETSTATIC);
    code.extend_from_slice(&pool.fieldref("B", "bL", "J").to_be_bytes());
    code.push(POP);
    code.push(GETFIELD);
    code.extend_from_slice(&pool.fieldref("A", "aL", "J").to_be_bytes());
    code.push(RETURN);
    let c_bytes = build_class(
        &mut pool,
        "C",
        Some("B"),
        &[],
        &[],
        &[MethodSpec {
            access: 0,
            name: "m",
            desc: "()V",
            code: Some((4, 1, code)),
            handlers: Vec::new(),
        }],
    );

    let mut cs = ClassSpace::new(Box::new(provider(vec![
        ("R".into(), r_bytes),
        ("A".into(), a_bytes),
        ("B".into(), b_bytes),
        ("C".into(), c_bytes),
    ])));
    let c_id = cs.load_class("C").expect("C loads");
    let r_id = cs.load_class("R").unwrap();
    let a_id = cs.load_class("A").unwrap();
    let b_id = cs.load_class("B").unwrap();

    // 实例布局：A.aI@0, A.aL@1-2, B.bI@3；C 无字段，总数 4。
    let c = cs.class(c_id);
    assert_eq!(c.instance_slot_count, 4);
    assert_eq!(
        c.instance_template,
        vec![Slot::Int(0), Slot::Long(0), Slot::Pad, Slot::Int(0)]
    );
    assert_eq!(c.init_state, InitState::Loaded);

    // 静态存储按类各自独立：R.ri = 7；A.aS 无常量 → 0；B.bL = 常量 + Pad。
    assert_eq!(cs.class(r_id).statics, vec![Slot::Int(7)]);
    assert_eq!(cs.class(a_id).statics, vec![Slot::Int(0)]);
    let b = cs.class(b_id);
    assert_eq!(
        b.statics,
        vec![Slot::Long(0x1122_3344_5566_7788), Slot::Pad]
    );

    // 解析：从 C 找 A.aI → owner A slot 0；A.aL → slot 1（双槽首槽）。
    let mut cs_mut = cs;
    let name = |cs: &ClassSpace, text: &str| skyengine_kvm::runtime::Sym(cs.syms.lookup(text).unwrap());
    let field = cs_mut
        .find_field(c_id, name(&cs_mut, "aI"), name(&cs_mut, "I"))
        .unwrap();
    assert_eq!(field.owner, a_id);
    assert_eq!(field.slot, 0);
    assert!(!field.is_static);
    let field = cs_mut
        .find_field(c_id, name(&cs_mut, "aL"), name(&cs_mut, "J"))
        .unwrap();
    assert_eq!(field.slot, 1);

    // 描述符不匹配 → 解析失败（同名不同类型是合法遮蔽，按 (name, desc) 匹配）。
    let error = cs_mut
        .find_field(c_id, name(&cs_mut, "aI"), name(&cs_mut, "J"))
        .unwrap_err();
    assert!(matches!(error, KvmError::Linkage(_)), "{error}");

    // 指令类别不匹配（getfield 用在静态字段上）→ Linkage。
    let error = cs_mut
        .link_field(a_id, "aS", "I", false)
        .unwrap_err();
    assert!(matches!(error, KvmError::Linkage(_)), "{error}");
}

/// 相互引用：A 的方法引用 B 的静态字段，B 的方法引用 A 的静态字段。
/// A 加载中注册占位 → B 递归加载 → B 的解码引用 A（布局已完成）→ 成功。
#[test]
fn mutual_reference_resolves_via_placeholder() {
    let mut pool = Pool::new();
    let r_bytes = build_class(&mut pool, ROOT, None, &[], &[], &[]);
    let a_ref = pool.fieldref("B", "bS", "I");
    let b_ref = pool.fieldref("A", "aS", "I");
    let mut a_code = Vec::new();
    a_code.push(GETSTATIC);
    a_code.extend_from_slice(&a_ref.to_be_bytes());
    a_code.push(POP);
    a_code.push(RETURN);
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[FieldSpec { access: 0x0008, name: "aS", desc: "I", constant: None }],
        &[MethodSpec { access: 0, name: "m", desc: "()V", code: Some((1, 0, a_code)), handlers: Vec::new() }],
    );
    let mut b_code = Vec::new();
    b_code.push(GETSTATIC);
    b_code.extend_from_slice(&b_ref.to_be_bytes());
    b_code.push(IRETURN);
    let b_const = pool.integer(3);
    let b_bytes = build_class(
        &mut pool,
        "B",
        Some(ROOT),
        &[],
        &[FieldSpec { access: 0x0008, name: "bS", desc: "I", constant: Some(b_const) }],
        &[MethodSpec { access: 0, name: "n", desc: "()I", code: Some((1, 0, b_code)), handlers: Vec::new() }],
    );

    let mut cs = ClassSpace::new(Box::new(provider(vec![
        (ROOT.into(), r_bytes),
        ("A".into(), a_bytes),
        ("B".into(), b_bytes),
    ])));
    let a_id = cs.load_class("A").expect("A loads (B pulled in transitively)");
    let b_id = cs.find("B").expect("B loaded transitively");

    // A.m 的 getstatic 已解析到 B.bS（owner B, slot 0, static）。
    let body = cs.class(a_id).methods[0].body.as_ref().unwrap();
    match head_insn(&body.insns[0]) {
        skyengine_kvm::classfile::code::Insn::GetStatic(field) => {
            assert_eq!(field.owner, b_id);
            assert_eq!(field.slot, 0);
            assert!(field.is_static);
        }
        other => panic!("expected GetStatic, got {other:?}"),
    }
    // B.n → A.aS。
    let body = cs.class(b_id).methods[0].body.as_ref().unwrap();
    match head_insn(&body.insns[0]) {
        skyengine_kvm::classfile::code::Insn::GetStatic(field) => {
            assert_eq!(field.owner, a_id);
            assert_eq!(field.slot, 0);
        }
        other => panic!("expected GetStatic, got {other:?}"),
    }

    // 方法查找：B.n 在 B 上；兄弟类方法不可达。
    let sym = |cs: &ClassSpace, text: &str| skyengine_kvm::runtime::Sym(cs.syms.lookup(text).unwrap());
    assert!(cs.find_method(b_id, sym(&cs, "n"), sym(&cs, "()I")).is_some());
    let error = cs
        .link_method(b_id, "m", "()V")
        .unwrap_err();
    assert!(matches!(error, KvmError::Linkage(_)), "{error}");
}

/// 继承环：X extends Y、Y extends X → 加载失败且失败粘滞。
#[test]
fn circular_hierarchy_is_rejected_stickily() {
    let mut pool = Pool::new();
    let x_bytes = build_class(&mut pool, "X", Some("Y"), &[], &[], &[]);
    let y_bytes = build_class(&mut pool, "Y", Some("X"), &[], &[], &[]);
    let mut cs = ClassSpace::new(Box::new(provider(vec![
        ("X".into(), x_bytes),
        ("Y".into(), y_bytes),
    ])));
    let error = cs.load_class("X").unwrap_err();
    assert!(
        error.to_string().contains("circular"),
        "{error}"
    );
    // 重试命中占位的粘滞失败标记。
    let error = cs.load_class("X").unwrap_err();
    assert!(
        error.to_string().contains("earlier"),
        "{error}"
    );
}

/// 数组类合成：`[I`、`[LA;`、`[[I`；非法描述符被拒。
#[test]
fn array_classes_are_synthesized() {
    let mut pool = Pool::new();
    let r_bytes = build_class(&mut pool, ROOT, None, &[], &[], &[]);
    let mut cs = ClassSpace::new(Box::new(provider(vec![(ROOT.into(), r_bytes)])));

    let int_arr = cs.load_class("[I").unwrap();
    assert!(cs.class(int_arr).is_array);
    assert_eq!(
        cs.class(int_arr).array_elem,
        Some(ArrayElem::Primitive(ElemType::Int))
    );

    let r_id = cs.load_class(ROOT).unwrap();
    let obj_arr = cs.load_class(&format!("[L{ROOT};")).unwrap();
    assert_eq!(cs.class(obj_arr).array_elem, Some(ArrayElem::Class(r_id)));

    // 嵌套数组：`[[I` 的组件是 `[I`。
    let nested = cs.load_class("[[I").unwrap();
    let ArrayElem::Class(component) = cs.class(nested).array_elem.unwrap() else {
        panic!("[[I must have class component");
    };
    assert_eq!(component, int_arr);

    // 非法描述符。
    assert!(matches!(
        cs.load_class("[x").unwrap_err(),
        KvmError::ClassFormat(_)
    ));
    // 未知类。
    assert!(matches!(
        cs.load_class("Nope").unwrap_err(),
        KvmError::ClassNotFound(_)
    ));
}

/// anewarray 指令经 Linker 合成数组类。
#[test]
fn anewarray_links_array_class() {
    let mut pool = Pool::new();
    let r_bytes = build_class(&mut pool, ROOT, None, &[], &[], &[]);
    let arr_class = pool.class(&format!("[L{ROOT};"));
    let mut code = Vec::new();
    code.push(ANEWARRAY);
    code.extend_from_slice(&arr_class.to_be_bytes());
    code.push(RETURN);
    let a_bytes = build_class(
        &mut pool,
        "A",
        Some(ROOT),
        &[],
        &[],
        &[MethodSpec { access: 0, name: "m", desc: "()V", code: Some((1, 0, code)), handlers: Vec::new() }],
    );
    let mut cs = ClassSpace::new(Box::new(provider(vec![
        (ROOT.into(), r_bytes),
        ("A".into(), a_bytes),
    ])));
    let a_id = cs.load_class("A").unwrap();
    let body = cs.class(a_id).methods[0].body.as_ref().unwrap();
    match head_insn(&body.insns[0]) {
        skyengine_kvm::classfile::code::Insn::ANewArray(class) => {
            assert!(cs.class(*class).is_array);
        }
        other => panic!("expected ANewArray, got {other:?}"),
    }
}

/// 静态字符串常量延后驻留：statics 槽为零值 null，字面量记录在 pending。
#[test]
fn string_constants_are_deferred() {
    let mut pool = Pool::new();
    let si_const = pool.integer(9);
    let ss_const = pool.string("hello");
    let sl_const = pool.long(7);
    // java/lang/Object 已由 bootstrap 提供（T010），常量字段放普通类 T 上。
    let r_bytes = build_class(
        &mut pool,
        "T",
        Some(ROOT),
        &[],
        &[
            FieldSpec { access: 0x0008, name: "si", desc: "I", constant: Some(si_const) },
            FieldSpec { access: 0x0008, name: "ss", desc: "Ljava/lang/String;", constant: Some(ss_const) },
            FieldSpec { access: 0x0008, name: "sl", desc: "J", constant: Some(sl_const) },
        ],
        &[],
    );
    let mut cs = ClassSpace::new(Box::new(provider(vec![("T".into(), r_bytes)])));
    let r_id = cs.load_class("T").unwrap();
    let r = cs.class(r_id);
    assert_eq!(
        r.statics,
        vec![
            Slot::Int(9),
            Slot::Ref(ObjRef::NULL),
            Slot::Long(7),
            Slot::Pad,
        ]
    );
    assert_eq!(r.pending_str_consts.len(), 1);
    let (slot, sym) = r.pending_str_consts[0];
    assert_eq!(slot, 1);
    assert_eq!(cs.syms.name(sym), "hello");
}

/// native/abstract 方法允许无 Code；具体方法缺 Code 是格式错误。
#[test]
fn method_code_presence_rules() {
    let mut pool = Pool::new();
    let r_bytes = build_class(
        &mut pool,
        ROOT,
        None,
        &[],
        &[],
        &[
            MethodSpec { access: 0x0001 | 0x0100, name: "nativeM", desc: "()I", code: None, handlers: Vec::new() },
            MethodSpec { access: 0x0400, name: "absM", desc: "()V", code: None, handlers: Vec::new() },
        ],
    );
    let mut cs = ClassSpace::new(Box::new(provider(vec![(ROOT.into(), r_bytes)])));
    let r_id = cs.load_class(ROOT).unwrap();
    assert!(cs.class(r_id).methods.iter().all(|m| m.body.is_none()));

    let bad_bytes = build_class(
        &mut pool,
        "Bad",
        Some(ROOT),
        &[],
        &[],
        &[MethodSpec { access: 0x0001, name: "m", desc: "()V", code: None, handlers: Vec::new() }],
    );
    // Bad 的父类是 ROOT：第二份空间同样要能提供 ROOT 字节。
    let root_bytes2 = build_class(&mut pool, ROOT, None, &[], &[], &[]);
    let mut cs2 = ClassSpace::new(Box::new(provider(vec![
        (ROOT.into(), root_bytes2),
        ("Bad".into(), bad_bytes),
    ])));
    let error = cs2.load_class("Bad").unwrap_err();
    assert!(error.to_string().contains("no Code"), "{error}");
}

/// 接口：方法经闭包解析，接口常量字段（隐式 static）可解析。
#[test]
fn interface_members_resolve() {
    let mut pool = Pool::new();
    let r_bytes = build_class(&mut pool, ROOT, None, &[], &[], &[]);
    // 接口 I：常量字段（public static final，javac 对接口常量的实际编码）+ 抽象方法。
    let i_const = pool.integer(42);
    let i_bytes = build_class(
        &mut pool,
        "I",
        Some(ROOT),
        &[],
        &[FieldSpec { access: 0x0019, name: "CONST", desc: "I", constant: Some(i_const) }],
        &[MethodSpec { access: 0x0401, name: "run", desc: "()V", code: None, handlers: Vec::new() }],
    );
    // C implements I，调用 I.run 并读取 I.CONST。
    let mut code = Vec::new();
    code.push(GETSTATIC);
    code.extend_from_slice(&pool.fieldref("I", "CONST", "I").to_be_bytes());
    code.push(POP);
    code.push(RETURN);
    let c_bytes = build_class(
        &mut pool,
        "C",
        Some(ROOT),
        &["I"],
        &[],
        &[MethodSpec { access: 0, name: "go", desc: "()V", code: Some((1, 0, code)), handlers: Vec::new() }],
    );
    let mut cs = ClassSpace::new(Box::new(provider(vec![
        (ROOT.into(), r_bytes),
        ("I".into(), i_bytes),
        ("C".into(), c_bytes),
    ])));
    let c_id = cs.load_class("C").unwrap();
    let i_id = cs.find("I").unwrap();

    // C 实现了 I（接口闭包包含）。
    assert!(cs.is_subclass_of(c_id, i_id));
    // 从 C 解析接口方法与常量。
    let sym = |cs: &ClassSpace, text: &str| skyengine_kvm::runtime::Sym(cs.syms.lookup(text).unwrap());
    assert!(cs.find_method(c_id, sym(&cs, "run"), sym(&cs, "()V")).is_some());
    let field = cs.find_field(c_id, sym(&cs, "CONST"), sym(&cs, "I")).unwrap();
    assert!(field.is_static);
    assert_eq!(field.owner, i_id);
    assert_eq!(cs.class(i_id).statics, vec![Slot::Int(42)]);
}

// ---- 堆 ----------------------------------------------------------------

/// 字符串驻留幂等。
#[test]
fn heap_interns_strings() {
    let mut heap = Heap::new();
    let chars: Vec<u16> = "héllo".encode_utf16().collect();
    let first = heap.alloc_string(chars.clone()).unwrap();
    let second = heap.alloc_string(chars.clone()).unwrap();
    assert_eq!(first, second);
    assert_eq!(heap.interned_string(&chars), Some(first));
    // 内容不同 → 新对象。
    let other = heap.alloc_string(vec![0x41, 0x42]).unwrap();
    assert_ne!(other, first);
    // 载荷可读。
    let string = heap.str_obj(first).unwrap();
    assert_eq!(string.chars, chars);
    assert_eq!(string.hash, 0);
}

/// 数组：零值、截断语义、类型不匹配拒绝、null 元素。
#[test]
fn heap_arrays_store_typed_elements() {
    let mut heap = Heap::new();
    let array = heap.alloc_array(ClassId(0), ElemType::Int, 3).unwrap();
    {
        let obj = heap.array_mut(array).unwrap();
        assert_eq!(obj.data.len(), 3);
        assert_eq!(obj.data.get(1), Slot::Int(0));
        obj.data.set(1, Slot::Int(77)).unwrap();
        assert_eq!(obj.data.get(1), Slot::Int(77));
    }
    // 引用数组默认 null。
    let refs = heap.alloc_array(ClassId(0), ElemType::Ref, 2).unwrap();
    assert_eq!(heap.array(refs).unwrap().data.get(0), Slot::Ref(ObjRef::NULL));
    // char 截断语义。
    let chars = heap.alloc_array(ClassId(0), ElemType::Char, 1).unwrap();
    heap.array_mut(chars).unwrap().data.set(0, Slot::Int(0x1_2345)).unwrap();
    assert_eq!(heap.array(chars).unwrap().data.get(0), Slot::Int(0x2345));
    // boolean 规约。
    let bools = heap.alloc_array(ClassId(0), ElemType::Boolean, 1).unwrap();
    heap.array_mut(bools).unwrap().data.set(0, Slot::Int(5)).unwrap();
    assert_eq!(heap.array(bools).unwrap().data.get(0), Slot::Int(1));
    // 类型不匹配 → 内部错误。
    let error = heap
        .array_mut(array)
        .unwrap()
        .data
        .set(0, Slot::Long(1))
        .unwrap_err();
    assert!(error.to_string().contains("mismatch"), "{error}");
}

/// 实例分配：模板字段即存储；null/空引用拒绝。
#[test]
fn heap_instances_use_template_slots() {
    let mut heap = Heap::new();
    let template = vec![Slot::Ref(ObjRef::NULL), Slot::Long(5), Slot::Pad];
    let reference = heap
        .alloc_instance(skyengine_kvm::runtime::ClassId(3), template)
        .unwrap();
    let instance = heap.instance(reference).unwrap();
    assert_eq!(instance.fields[1], Slot::Long(5));
    // 载荷类型不符。
    assert!(heap.array(reference).is_err());
    // null 引用（槽 0）不可访问。
    assert!(heap.payload(ObjRef::NULL).is_err());
    // 软上限。
    let mut tiny = Heap::with_cap(2);
    assert!(tiny.alloc_array(ClassId(0), ElemType::Int, 1).is_ok());
    assert!(tiny.alloc_array(ClassId(0), ElemType::Int, 1).is_ok());
    assert!(matches!(
        tiny.alloc_array(ClassId(0), ElemType::Int, 1).unwrap_err(),
        KvmError::OutOfMemory
    ));
    // ArrayData 直接构造（长数组上限防御）。
    assert_eq!(ArrayData::zeroed(ElemType::Long, 2).len(), 2);
    assert!(heap.alloc_array(ClassId(0), ElemType::Int, 1 << 29).is_err());
}

/// Payload 形态覆盖：三类载荷各得其所。
#[test]
fn heap_payload_kinds() {
    let mut heap = Heap::new();
    let reference = heap.alloc_array(ClassId(0), ElemType::Byte, 2).unwrap();
    assert!(matches!(
        heap.payload(reference).unwrap(),
        Payload::Array(_)
    ));
    let string = heap.alloc_string(vec![0x61]).unwrap();
    assert!(heap.is_string(string));
    assert!(matches!(heap.payload(string).unwrap(), Payload::Str(_)));
    let instance = heap
        .alloc_instance(skyengine_kvm::runtime::ClassId(0), vec![])
        .unwrap();
    assert!(matches!(
        heap.payload(instance).unwrap(),
        Payload::Instance(_)
    ));
    assert_eq!(heap.object_count(), 3);
}
