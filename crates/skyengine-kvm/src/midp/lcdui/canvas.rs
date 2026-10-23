//! javax/microedition/lcdui/Canvas 真实现（T021）。
//!
//! 对象模型：实例字段 `screen`（懒分配的可变屏图——首次使用时按
//! `host.screen_size()` 经 `image::alloc_mutable_image` 建立，复用
//! T018 的可变空图分配路径）+ 脏矩形 `dirty Z`/`dx/dy/dw/dh I`
//!（repaint 标脏并与已有脏矩形**并集合并**，同帧多次 repaint 只触发
//! 一次 paint）。
//!
//! paint 回调链：本模块只负责"标脏/读脏/清脏/屏图与像素快照"数据面；
//! 线程编排（找子类 paint → push_entry_frame → spawn → present）在
//! `Vm::poll` 三段式中落地（见 vm.rs 段一/段三）。
//!
//! 输入事件回调（keyPressed/keyReleased）的线程编排在 `Vm::poll`
//! 段前置事件泵（T030）；键码映射与游戏动作 API 见 `input` 模块。

use crate::KvmError;
use crate::cldc::lang as jl;
use crate::host::HostContext;
use crate::interp::NativeCall;
use crate::midp::lcdui::image;
use crate::runtime::classspace::ClassSpace;
use crate::runtime::heap::Heap;
use crate::runtime::{ClassId, ObjRef, Slot};

/// Canvas 内部名。
pub const CLASS: &str = "javax/microedition/lcdui/Canvas";

/// paint 回调描述符（Vm.poll 段一按此解析子类覆写）。
pub const PAINT_DESC: &str = "(Ljavax/microedition/lcdui/Graphics;)V";

/// Canvas 实例字段槽位（每次调用现查；与 image.rs 的 image_slots 同模式）。
struct CanvasSlots {
    screen: usize,
    dirty: usize,
    dx: usize,
    dy: usize,
    dw: usize,
    dh: usize,
    full_screen: usize,
}

fn canvas_slots(cs: &mut ClassSpace) -> Result<CanvasSlots, KvmError> {
    let class = cs.load_class(CLASS)?;
    let slot = |cs: &mut ClassSpace, name: &str, desc: &str| -> Result<usize, KvmError> {
        let name = cs.syms.intern(name);
        let desc = cs.syms.intern(desc);
        Ok(cs.find_field(class, name, desc)?.slot as usize)
    };
    Ok(CanvasSlots {
        screen: slot(cs, "screen", "Ljavax/microedition/lcdui/Image;")?,
        dirty: slot(cs, "dirty", "Z")?,
        dx: slot(cs, "dx", "I")?,
        dy: slot(cs, "dy", "I")?,
        dw: slot(cs, "dw", "I")?,
        dh: slot(cs, "dh", "I")?,
        full_screen: slot(cs, "fullScreen", "Z")?,
    })
}

/// 抛 `java/lang/NullPointerException`。
fn npe(cs: &mut ClassSpace, heap: &mut Heap) -> KvmError {
    jl::throw_java(cs, heap, "java/lang/NullPointerException", None)
}

fn arg_int(call: &NativeCall, index: usize) -> i32 {
    match call.args[index] {
        Slot::Int(v) => v,
        _ => 0,
    }
}

/// 标脏并与已有脏矩形并集合并（同帧多次 repaint 一次 paint 的关键）。
/// 空区域（w/h <= 0，MIDP javadoc 语义）静默忽略。
fn mark_dirty(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    canvas: ObjRef,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> Result<(), KvmError> {
    if w <= 0 || h <= 0 {
        return Ok(());
    }
    let s = canvas_slots(cs)?;
    let f = &mut heap.instance_mut(canvas)?.fields;
    let dirty = matches!(f[s.dirty], Slot::Int(v) if v != 0);
    if !dirty {
        f[s.dirty] = Slot::Int(1);
        f[s.dx] = Slot::Int(x);
        f[s.dy] = Slot::Int(y);
        f[s.dw] = Slot::Int(w);
        f[s.dh] = Slot::Int(h);
        return Ok(());
    }
    // 并集：先还原为角点求包络，再存回 x/y/w/h。
    let (ox, oy, ow, oh) = match (f[s.dx], f[s.dy], f[s.dw], f[s.dh]) {
        (Slot::Int(a), Slot::Int(b), Slot::Int(c), Slot::Int(d)) => (a, b, c, d),
        _ => return Err(KvmError::Internal("Canvas.dirty rect corrupt".into())),
    };
    let x1 = ox.min(x);
    let y1 = oy.min(y);
    let x2 = (ox + ow).max(x + w);
    let y2 = (oy + oh).max(y + h);
    f[s.dx] = Slot::Int(x1);
    f[s.dy] = Slot::Int(y1);
    f[s.dw] = Slot::Int(x2 - x1);
    f[s.dh] = Slot::Int(y2 - y1);
    Ok(())
}

// ---------------------------------------------------------------------------
// 原生方法
// ---------------------------------------------------------------------------

/// `repaint()V`：全屏标脏（并集合并）。全屏以 i32::MAX 宽高占位，
/// paint 派发时与屏尺寸求交。
pub fn repaint(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = call.this.unwrap_or(ObjRef::NULL);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    mark_dirty(cs, heap, this, 0, 0, i32::MAX, i32::MAX)?;
    Ok(None)
}

/// `repaint(IIII)V`：区域标脏（并集合并）；空区域忽略。
pub fn repaint_rect(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = call.this.unwrap_or(ObjRef::NULL);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let (x, y, w, h) = (
        arg_int(call, 0),
        arg_int(call, 1),
        arg_int(call, 2),
        arg_int(call, 3),
    );
    mark_dirty(cs, heap, this, x, y, w, h)?;
    Ok(None)
}

/// `serviceRepaints()V`：置全局 repaint 请求位，下次 poll 立即触发。
/// 绿线程模型下暂为异步（TODO T034：MIDP 规范为同步阻塞到 paint 完成）。
pub fn service_repaints(
    cs: &mut ClassSpace,
    _heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    cs.repaint_requested = true;
    Ok(None)
}

/// `setFullScreenMode(Z)V`：MVP 仅记录标志（尺寸语义不变）。
pub fn set_full_screen_mode(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = call.this.unwrap_or(ObjRef::NULL);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let s = canvas_slots(cs)?;
    heap.instance_mut(this)?.fields[s.full_screen] = Slot::Int(i32::from(arg_int(call, 0) != 0));
    Ok(None)
}

/// `getWidth()I`：屏图宽（懒分配后读取，单一数据源）。
pub fn get_width(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = call.this.unwrap_or(ObjRef::NULL);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let screen = ensure_screen(cs, heap, host, this)?;
    let w = image::image_width(cs, heap, screen)?;
    Ok(Some(Slot::Int(w)))
}

/// `getHeight()I`：屏图高。
pub fn get_height(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = call.this.unwrap_or(ObjRef::NULL);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let screen = ensure_screen(cs, heap, host, this)?;
    let h = image::image_height(cs, heap, screen)?;
    Ok(Some(Slot::Int(h)))
}

// ---------------------------------------------------------------------------
// Vm.poll 段一/段三复用的数据面
// ---------------------------------------------------------------------------

/// 屏图懒分配：首次调用按 `host.screen_size()` 建可变空图并挂到
/// `screen` 字段；之后直接返回既有引用。
pub fn ensure_screen(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    canvas: ObjRef,
) -> Result<ObjRef, KvmError> {
    let s = canvas_slots(cs)?;
    let existing = match heap.instance(canvas)?.fields[s.screen] {
        Slot::Ref(r) => r,
        _ => return Err(KvmError::Internal("Canvas.screen corrupt".into())),
    };
    if existing != ObjRef::NULL {
        return Ok(existing);
    }
    let (w, h) = host.screen_size();
    if w == 0 || h == 0 {
        // 宿主屏不可用（防御）：不分配，调用方按无屏跳过。
        return Ok(ObjRef::NULL);
    }
    let screen = image::alloc_mutable_image(cs, heap, i32::from(w), i32::from(h))?;
    heap.instance_mut(canvas)?.fields[s.screen] = Slot::Ref(screen);
    Ok(screen)
}

/// 读脏矩形：Some((x, y, w, h)) = 有待绘区域。
pub fn dirty_rect(
    cs: &mut ClassSpace,
    heap: &Heap,
    canvas: ObjRef,
) -> Result<Option<(i32, i32, i32, i32)>, KvmError> {
    let s = canvas_slots(cs)?;
    let f = &heap.instance(canvas)?.fields;
    if !matches!(f[s.dirty], Slot::Int(v) if v != 0) {
        return Ok(None);
    }
    match (f[s.dx], f[s.dy], f[s.dw], f[s.dh]) {
        (Slot::Int(x), Slot::Int(y), Slot::Int(w), Slot::Int(h)) => Ok(Some((x, y, w, h))),
        _ => Err(KvmError::Internal("Canvas.dirty rect corrupt".into())),
    }
}

/// 清脏（paint 已派发或跳过后调用，防同帧重触发）。
pub fn clear_dirty(cs: &mut ClassSpace, heap: &mut Heap, canvas: ObjRef) -> Result<(), KvmError> {
    let s = canvas_slots(cs)?;
    heap.instance_mut(canvas)?.fields[s.dirty] = Slot::Int(0);
    Ok(())
}

/// 屏图像素快照转 RGBA8（自上而下逐行，供 `host.present`）。
/// 像素位模式 0xAARRGGBB（与 Graphics/Image 一致）→ 每像素 4 字节。
/// 无屏图（尚未懒分配）返回 None。
pub fn screen_rgba(
    cs: &mut ClassSpace,
    heap: &Heap,
    canvas: ObjRef,
) -> Result<Option<(Vec<u8>, u16, u16)>, KvmError> {
    let s = canvas_slots(cs)?;
    let screen = match heap.instance(canvas)?.fields[s.screen] {
        Slot::Ref(r) if r != ObjRef::NULL => r,
        _ => return Ok(None),
    };
    let (w, h) = (
        image::image_width(cs, heap, screen)?,
        image::image_height(cs, heap, screen)?,
    );
    let pixels = image::pixels_of(cs, heap, screen)?;
    if pixels.len() != (w as usize) * (h as usize) {
        return Err(KvmError::Internal("Canvas.screen size mismatch".into()));
    }
    let mut rgba = Vec::with_capacity(pixels.len() * 4);
    for &raw in pixels {
        let p = raw as u32; // i32 位模式等价转 u32
        rgba.push(((p >> 16) & 0xFF) as u8); // R
        rgba.push(((p >> 8) & 0xFF) as u8); // G
        rgba.push((p & 0xFF) as u8); // B
        rgba.push(((p >> 24) & 0xFF) as u8); // A
    }
    Ok(Some((rgba, w as u16, h as u16)))
}

/// 对象运行时类是否为 Canvas 或其子类（沿父链；Vm.poll 段一过滤
/// current 非 Canvas 的 Displayable——Screen 子体系留桩）。
pub fn is_canvas_class(cs: &ClassSpace, mut class: ClassId) -> bool {
    loop {
        if cs.name_of(class) == CLASS {
            return true;
        }
        match cs.class(class).super_id {
            Some(next) if next != class => class = next,
            _ => return false,
        }
    }
}
