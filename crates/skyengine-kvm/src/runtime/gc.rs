//! 标记-清除 GC（D2）：非移动、全停顿。
//!
//! 根集 = 调用方提供的槽位切片（解释器帧的局部变量与操作数栈）
//! **加上** 类空间静态区（`static_roots`）。分配失败时经
//! `alloc_with_gc` 触发收集并重试一次，仍失败向上传播
//! `OutOfMemory`（由上层转译为 Java 的 OutOfMemoryError）。
//!
//! 标记阶段使用显式工作列表而非递归：深引用链不耗 Rust 栈，
//! 循环引用因"已标记即跳过"天然终止。对象不移动，回收槽由
//! 堆的自由表复用，`ObjRef` 数值在对象存活期间保持稳定。

use std::collections::HashSet;

use crate::runtime::classspace::ClassSpace;
use crate::runtime::heap::Heap;
use crate::runtime::object::{ArrayData, Payload};
use crate::runtime::{ClassId, ObjRef, Slot};
use crate::KvmError;

/// 收集：标记从根可达的对象并清除其余，返回释放的对象数。
pub fn collect(heap: &mut Heap, roots: &[Slot]) -> usize {
    let marked = mark(heap, roots);
    heap.sweep(&marked)
}

/// 标记阶段：工作列表遍历，返回可达对象集合。
fn mark(heap: &Heap, roots: &[Slot]) -> HashSet<ObjRef> {
    let mut marked: HashSet<ObjRef> = HashSet::new();
    let mut work: Vec<ObjRef> = Vec::new();
    push_ref_slots(roots, &mut work);
    while let Some(reference) = work.pop() {
        if reference.is_null() || !marked.insert(reference) {
            continue; // null 或已标记（循环引用在此终止）
        }
        match heap.payload(reference) {
            Ok(Payload::Instance(instance)) => push_ref_slots(&instance.fields, &mut work),
            Ok(Payload::Array(array)) => {
                if let ArrayData::Refs(elements) = &array.data {
                    work.extend(elements.iter().copied().filter(|r| !r.is_null()));
                }
            }
            Ok(Payload::Str(_)) => {} // 字符串无出边
            // 空槽混入根集（调用方持有过期引用）：容忍，跳过展开。
            Err(_) => {}
        }
    }
    marked
}

/// 从槽位切片中收集引用压入工作列表（null 跳过；类别 2 第二槽为 Pad 无引用）。
fn push_ref_slots(slots: &[Slot], work: &mut Vec<ObjRef>) {
    for slot in slots {
        if let Slot::Ref(reference) = slot
            && !reference.is_null() {
                work.push(*reference);
            }
    }
}

/// 类空间全部静态槽作为根。
///
/// 静态引用字段（含初始化期驻留的字符串常量）经由静态区保持可达。
pub fn static_roots(cs: &ClassSpace) -> Vec<Slot> {
    let mut roots = Vec::new();
    for index in 0..cs.class_count() {
        roots.extend(cs.class(ClassId(index as u32)).statics.iter().copied());
    }
    roots
}

/// 分配辅助：`OutOfMemory` 时先收集再重试一次，仍失败返回原错误。
///
/// 根集必须覆盖调用现场全部活跃引用（帧槽 + 静态区），否则可达
/// 对象会被误回收；闭包内的中间垃圾正因不在根集中而被释放。
/// 闭包须为 `Fn`（重试会再次调用）。
pub fn alloc_with_gc<T>(
    heap: &mut Heap,
    roots: &[Slot],
    alloc: impl Fn(&mut Heap) -> Result<T, KvmError>,
) -> Result<T, KvmError> {
    match alloc(heap) {
        Ok(value) => Ok(value),
        Err(KvmError::OutOfMemory) => {
            collect(heap, roots);
            alloc(heap)
        }
        Err(error) => Err(error),
    }
}
