//! T010：bootstrap 声明式注册表接入类空间 + jc.jar 89 类链接首测。
//!
//! - 89 个 jar 类逐个 `load_class`（含布局/方法体解码）全部成功；
//!   jar 外部缺失引用按惰性加载策略不在此时触发。
//! - 不存在类返回 `KvmError::ClassNotFound`（NoClassDefFoundError 语义）
//!   而非 panic，且重试幂等。
//! - `java/lang/Object` 从 bootstrap 表解析，异常体系骨架链可达 Object。
//! - `RegistryDispatch` 按 类名.方法名+描述符 绑定 `NativeFn`。

mod common;

use skyengine_kvm::host::mock::MockHostContext;
use skyengine_kvm::interp::natives::RegistryDispatch;
use skyengine_kvm::interp::{NativeCall, NativeDispatch};
use skyengine_kvm::runtime::classspace::{ClassSpace, MapClassProvider};
use skyengine_kvm::runtime::heap::Heap;
use skyengine_kvm::runtime::{ObjRef, Slot, ThreadId};
use skyengine_kvm::KvmError;

fn classspace() -> (ClassSpace, Heap) {
    let jar = common::load_jar();
    (
        ClassSpace::new(Box::new(MapClassProvider::from_jar(&jar))),
        Heap::new(),
    )
}

/// jc.jar 全部 89 类：加载 + 布局 + 方法体解码全部成功。
#[test]
fn jar_classes_link_after_bootstrap() {
    let jar = common::load_jar();
    let mut names: Vec<String> = jar
        .keys()
        .filter(|key| key.ends_with(".class"))
        .map(|key| key.trim_end_matches(".class").replace('\\', "/"))
        .collect();
    names.sort();
    assert_eq!(names.len(), 89, "fixture jar 应含 89 个类");

    let (mut cs, _) = classspace();
    for name in &names {
        let id = cs
            .load_class(name)
            .unwrap_or_else(|error| panic!("load {name}: {error}"));
        cs.ensure_layout(id)
            .unwrap_or_else(|error| panic!("layout {name}: {error}"));
    }
    // 89 个 jar 类 + 按需拉入的 bootstrap 父类。
    assert!(cs.class_count() >= names.len());
}

/// 不存在类：NoClassDefFoundError 语义错误而非 panic，重试幂等。
#[test]
fn missing_class_yields_not_found() {
    let (mut cs, _) = classspace();
    // com/jblend/payment/* 不在 jc.jar 也不在 bootstrap 表 → 真缺失类。
    match cs.load_class("com/jblend/payment/Payment") {
        Err(KvmError::ClassNotFound(name)) => {
            assert_eq!(name, "com/jblend/payment/Payment");
        }
        other => panic!("期望 ClassNotFound，得到 {other:?}"),
    }
    assert!(matches!(
        cs.load_class("com/jblend/payment/Payment"),
        Err(KvmError::ClassNotFound(_))
    ));
}

/// java/lang/Object 从 bootstrap 表解析：native 桩方法 + 异常骨架链。
#[test]
fn object_resolves_from_bootstrap() {
    let (mut cs, _) = classspace();
    let object = cs.load_class("java/lang/Object").expect("load Object");
    assert_eq!(cs.name_of(object), "java/lang/Object");
    // Object 无父类；布局后仍无 super_id。
    cs.ensure_layout(object).unwrap();
    assert!(cs.class(object).super_id.is_none());

    // hashCode 桩已注册：native（body=None）。
    let hash_name = cs.syms.intern("hashCode");
    let hash_desc = cs.syms.intern("()I");
    let index = cs
        .class(object)
        .declared_method(hash_name, hash_desc)
        .expect("hashCode 已声明");
    let method = &cs.class(object).methods[index as usize];
    assert!(method.body.is_none(), "bootstrap 方法不应有字节码体");

    // 异常体系骨架：子类链可达 Object；Throwable 携带 detailMessage。
    for name in [
        "java/lang/Throwable",
        "java/lang/Exception",
        "java/lang/RuntimeException",
        "java/lang/NullPointerException",
        "java/lang/ArrayIndexOutOfBoundsException",
        "java/lang/ClassCastException",
        "java/lang/ArithmeticException",
        "java/lang/NegativeArraySizeException",
        "java/lang/IllegalMonitorStateException",
        "java/lang/ArrayStoreException",
        "java/lang/IndexOutOfBoundsException",
        "java/lang/UnsupportedOperationException",
        "java/lang/Error",
        "java/lang/LinkageError",
        "java/lang/NoClassDefFoundError",
        "java/lang/OutOfMemoryError",
        "java/lang/Class",
    ] {
        let id = cs.load_class(name).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert!(
            cs.is_subclass_of(id, object),
            "{name} 应可达 java/lang/Object"
        );
    }
    let throwable = cs.load_class("java/lang/Throwable").unwrap();
    let msg_name = cs.syms.intern("detailMessage");
    let msg_desc = cs.syms.intern("Ljava/lang/String;");
    assert!(cs.class(throwable).field_index.contains_key(&(msg_name, msg_desc)));
}

/// RegistryDispatch：按 类名.方法名+描述符 绑定；未注册方法报链接错误。
#[test]
fn registry_dispatch_binds_native_fn() {
    let (mut cs, mut heap) = classspace();
    let object = cs.load_class("java/lang/Object").unwrap();
    let mut dispatch = RegistryDispatch;

    // hashCode 桩：无接收者 → 0。
    let hash = NativeCall {
        class: object,
        name: cs.syms.intern("hashCode"),
        desc: cs.syms.intern("()I"),
        this: None,
        args: Vec::new(),
        thread: ThreadId(0),
    };
    assert_eq!(
        dispatch.call_native(&mut cs, &mut heap, &mut MockHostContext::new(240, 320), hash).unwrap(),
        Some(Slot::Int(0))
    );

    // equals 桩：引用相等比较。
    let equals = NativeCall {
        class: object,
        name: cs.syms.intern("equals"),
        desc: cs.syms.intern("(Ljava/lang/Object;)Z"),
        this: Some(ObjRef(7)),
        args: vec![Slot::Ref(ObjRef(7))],
        thread: ThreadId(0),
    };
    assert_eq!(
        dispatch.call_native(&mut cs, &mut heap, &mut MockHostContext::new(240, 320), equals).unwrap(),
        Some(Slot::Int(1))
    );

    // notify 桩：void 返回。
    let notify = NativeCall {
        class: object,
        name: cs.syms.intern("notify"),
        desc: cs.syms.intern("()V"),
        this: Some(ObjRef(7)),
        args: Vec::new(),
        thread: ThreadId(0),
    };
    assert_eq!(dispatch.call_native(&mut cs, &mut heap, &mut MockHostContext::new(240, 320), notify).unwrap(), None);

    // 未注册方法 → 链接错误（非 panic）。
    let bogus = NativeCall {
        class: object,
        name: cs.syms.intern("nope"),
        desc: cs.syms.intern("()V"),
        this: None,
        args: Vec::new(),
        thread: ThreadId(0),
    };
    assert!(matches!(
        dispatch.call_native(&mut cs, &mut heap, &mut MockHostContext::new(240, 320), bogus),
        Err(KvmError::Linkage(_))
    ));
}
