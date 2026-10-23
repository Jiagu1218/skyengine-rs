//! javax/microedition/lcdui/Display 真实现（T021）。
//!
//! 对象模型：`getDisplay(MIDlet)` 单例——实例缓存在
//! `ClassSpace.display_cache`（与 font_cache 同型的运行态挂靠，
//! MIDP 语义每 MIDlet 一个 Display，MVP 单 MIDlet 即进程单例），
//! 保证两次调用返回同实例。`current` 为 Display 实例字段
//! （`setCurrent`/`getCurrent` 回读同一引用）。
//!
//! paint 派发的"current 是否为待绘 Canvas"查询也走本模块
//! （`current_displayable`，Vm.poll 段一）。次要方法（isShown 等）
//! 与 Screen 子体系（Form/List/TextBox）留桩。

use crate::KvmError;
use crate::cldc::lang as jl;
use crate::interp::NativeCall;
use crate::runtime::classspace::ClassSpace;
use crate::runtime::heap::Heap;
use crate::runtime::{ObjRef, Slot};

/// Display 内部名。
const CLASS: &str = "javax/microedition/lcdui/Display";

/// Display 实例字段槽位（每次调用现查；与 font.rs 同模式）。
fn current_slot(cs: &mut ClassSpace) -> Result<usize, KvmError> {
    let class = cs.load_class(CLASS)?;
    let name = cs.syms.intern("current");
    let desc = cs.syms.intern("Ljavax/microedition/lcdui/Displayable;");
    Ok(cs.find_field(class, name, desc)?.slot as usize)
}

/// 抛 `java/lang/NullPointerException`。
fn npe(cs: &mut ClassSpace, heap: &mut Heap) -> KvmError {
    jl::throw_java(cs, heap, "java/lang/NullPointerException", None)
}

// ---------------------------------------------------------------------------
// 原生方法
// ---------------------------------------------------------------------------

/// `getDisplay(Ljavax/microedition/midlet/MIDlet;)LDisplay;`（静态）：
/// 单例（display_cache），首次分配后固定。midlet 实参 MVP 不参与
///（单 MIDlet 进程）。
pub fn get_display(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    if let Some(existing) = cs.display_cache
        && existing != ObjRef::NULL {
            return Ok(Some(Slot::Ref(existing)));
        }
    let class = cs.load_class(CLASS)?;
    let template = cs.class(class).instance_template.clone();
    let display = heap.alloc_instance(class, template)?;
    cs.display_cache = Some(display);
    Ok(Some(Slot::Ref(display)))
}

/// `setCurrent(Ljavax/microedition/lcdui/Displayable;)V`：写入实例字段
/// `current`（null 合法 = 清屏）。
pub fn set_current(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = call.this.unwrap_or(ObjRef::NULL);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let target = match call.args[0] {
        Slot::Ref(r) => r,
        _ => ObjRef::NULL,
    };
    let slot = current_slot(cs)?;
    heap.instance_mut(this)?.fields[slot] = Slot::Ref(target);
    Ok(None)
}

/// `getCurrent()Ljavax/microedition/lcdui/Displayable;`：回读 `current`。
pub fn get_current(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = call.this.unwrap_or(ObjRef::NULL);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let slot = current_slot(cs)?;
    match heap.instance(this)?.fields[slot] {
        Slot::Ref(r) => Ok(Some(Slot::Ref(r))),
        _ => Err(KvmError::Internal("Display.current corrupt".into())),
    }
}

// ---------------------------------------------------------------------------
// Vm.poll 段一复用
// ---------------------------------------------------------------------------

/// 当前 Displayable（display_cache → Display.current）；未建 Display、
/// 未 setCurrent 或已置 null 均返回 None。
pub fn current_displayable(cs: &mut ClassSpace, heap: &Heap) -> Option<ObjRef> {
    let display = *cs.display_cache.as_ref()?;
    let slot = current_slot(cs).ok()?;
    match heap.instance(display).ok()?.fields[slot] {
        Slot::Ref(r) if r != ObjRef::NULL => Some(r),
        _ => None,
    }
}
