//! GC（T007）测试：标记-清除、循环引用回收、可达图存活、
//! 驻留表清理、回收槽复用、分配失败→收集→重试、静态区根。

// 共享夹具：本文件仅用 classbytes 构造器，其余（MRP/zip）允许未用。
#[allow(dead_code)]
mod common;

use common::classbytes::{build_class, provider, FieldSpec, Pool, ROOT};use skyengine_kvm::runtime::classspace::ClassSpace;
use skyengine_kvm::runtime::gc::{alloc_with_gc, collect, static_roots};
use skyengine_kvm::runtime::heap::Heap;
use skyengine_kvm::runtime::object::ElemType;
use skyengine_kvm::runtime::{ClassId, ObjRef, Slot};
use skyengine_kvm::KvmError;

fn utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

/// 不可达对象被清除；根直接持有的对象存活。
#[test]
fn unreachable_objects_are_swept() {
    let mut heap = Heap::new();
    let live = heap.alloc_array(ClassId(0), ElemType::Int, 4).unwrap();
    let garbage = heap.alloc_array(ClassId(0), ElemType::Int, 8).unwrap();
    assert_eq!(heap.object_count(), 2);

    let freed = collect(&mut heap, &[Slot::Ref(live)]);

    assert_eq!(freed, 1);
    assert_eq!(heap.object_count(), 1);
    assert_eq!(heap.array(live).unwrap().data.len(), 4);
    assert!(heap.payload(garbage).is_err(), "被清除对象应视为空槽");
}

/// 实例字段 → 引用数组 → 字符串 的可达图整体存活。
#[test]
fn reachable_graph_survives() {
    let mut heap = Heap::new();
    let leaf = heap.alloc_string(utf16("leaf")).unwrap();
    let elements = heap.alloc_array(ClassId(0), ElemType::Ref, 2).unwrap();
    heap.array_mut(elements).unwrap().data.set(0, Slot::Ref(leaf)).unwrap();
    let instance = heap
        .alloc_instance(ClassId(0), vec![Slot::Int(7), Slot::Ref(elements)])
        .unwrap();

    let freed = collect(&mut heap, &[Slot::Ref(instance)]);

    assert_eq!(freed, 0);
    assert_eq!(heap.object_count(), 3);
    let array = match heap.instance(instance).unwrap().fields[1] {
        Slot::Ref(reference) => reference,
        other => panic!("实例字段应为引用，got {other:?}"),
    };
    let text = match heap.array(array).unwrap().data.get(0) {
        Slot::Ref(reference) => reference,
        other => panic!("数组元素应为引用，got {other:?}"),
    };
    assert_eq!(heap.str_obj(text).unwrap().chars, utf16("leaf"));
}

/// 互引环不可达时整体回收（工作列表标记天然终止）。
#[test]
fn cycles_are_collected() {
    let mut heap = Heap::new();
    let a = heap
        .alloc_instance(ClassId(0), vec![Slot::Ref(ObjRef::NULL)])
        .unwrap();
    let b = heap
        .alloc_instance(ClassId(0), vec![Slot::Ref(ObjRef::NULL)])
        .unwrap();
    heap.instance_mut(a).unwrap().fields[0] = Slot::Ref(b);
    heap.instance_mut(b).unwrap().fields[0] = Slot::Ref(a);

    let freed = collect(&mut heap, &[]);

    assert_eq!(freed, 2);
    assert_eq!(heap.object_count(), 0);
}

/// 清除产生的空洞被后续分配复用（对象不移动，槽位数值回绕）。
#[test]
fn freed_slots_are_reused() {
    let mut heap = Heap::new();
    let garbage = heap.alloc_array(ClassId(0), ElemType::Int, 1).unwrap();
    collect(&mut heap, &[]);

    let reused = heap.alloc_array(ClassId(0), ElemType::Byte, 2).unwrap();

    assert_eq!(reused, garbage, "回收槽应被复用而非永久丢失");
    assert_eq!(heap.object_count(), 1);
    assert_eq!(heap.array(reused).unwrap().data.len(), 2);
}

/// 驻留表随清除同步清理：死串可重新驻留，活串保持幂等。
#[test]
fn interned_table_tracks_liveness() {
    let mut heap = Heap::new();
    let _dead = heap.alloc_string(utf16("dead")).unwrap();
    let live = heap.alloc_string(utf16("live")).unwrap();
    assert_eq!(
        heap.alloc_string(utf16("live")).unwrap(),
        live,
        "驻留幂等：同内容返回同一对象"
    );

    collect(&mut heap, &[Slot::Ref(live)]);

    assert!(heap.interned_string(&utf16("dead")).is_none(), "死串应移出驻留表");
    assert_eq!(heap.interned_string(&utf16("live")), Some(live));
    // 死串重新驻留得到可用对象（回收槽可能复用，数值比较无意义）。
    let revived = heap.alloc_string(utf16("dead")).unwrap();
    assert_eq!(heap.str_obj(revived).unwrap().chars, utf16("dead"));
    assert_eq!(heap.interned_string(&utf16("dead")), Some(revived));
}

/// 分配失败 → 收集 → 重试成功；全部可达时仍失败返回 OutOfMemory。
#[test]
fn alloc_with_gc_collects_then_retries() {
    // cap=2：对象表最多 2 个实槽（null 槽不计）。
    let mut heap = Heap::with_cap(2);
    let held = heap.alloc_array(ClassId(0), ElemType::Int, 1).unwrap();
    let _garbage = heap.alloc_array(ClassId(0), ElemType::Int, 1).unwrap();
    let roots = [Slot::Ref(held)];

    // 第三次分配超限 → 触发 GC 释放垃圾 → 重试成功。
    let third = alloc_with_gc(&mut heap, &roots, |heap| {
        heap.alloc_array(ClassId(0), ElemType::Int, 2)
    })
    .unwrap();
    assert_eq!(heap.object_count(), 2);
    assert_eq!(heap.array(third).unwrap().data.len(), 2);

    // 此时 held + third 均可达，再分配已无垃圾可收 → OutOfMemory。
    let roots = [Slot::Ref(held), Slot::Ref(third)];
    let error = alloc_with_gc(&mut heap, &roots, |heap| {
        heap.alloc_array(ClassId(0), ElemType::Int, 1)
    })
    .unwrap_err();
    assert!(matches!(error, KvmError::OutOfMemory));
}

/// 静态区根：静态引用字段持有的对象在收集中存活。
#[test]
fn static_roots_keep_static_references_alive() {
    let mut pool = Pool::new();
    let root_bytes = build_class(&mut pool, ROOT, None, &[], &[], &[]);
    let holder_bytes = build_class(
        &mut pool,
        "Holder",
        Some(ROOT),
        &[],
        &[FieldSpec {
            access: 0x0008, // static
            name: "s",
            desc: "Ljava/lang/Object;",
            constant: None,
        }],
        &[],
    );
    let mut cs = ClassSpace::new(Box::new(provider(vec![
        (ROOT.to_string(), root_bytes),
        ("Holder".to_string(), holder_bytes),
    ])));
    let holder = cs.load_class("Holder").unwrap();

    let mut heap = Heap::new();
    let kept = heap.alloc_string(utf16("static")).unwrap();
    cs.class_mut(holder).statics[0] = Slot::Ref(kept);
    let _dropped = heap.alloc_array(ClassId(0), ElemType::Int, 1).unwrap();

    let roots = static_roots(&cs);
    let freed = collect(&mut heap, &roots);

    assert_eq!(freed, 1);
    assert!(matches!(
        cs.class(holder).statics[0],
        Slot::Ref(reference) if heap.str_obj(reference).is_ok()
    ));
}
