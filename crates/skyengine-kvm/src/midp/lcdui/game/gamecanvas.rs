//! javax/microedition/lcdui/game/GameCanvas 真实现（T031）。
//!
//! 对象模型：`extends Canvas` 的 bootstrap 具体类。实例字段仅
//! `suppress Z`（构造 `GameCanvas(boolean)` 存入；Canvas 父类字段
//! 零值即初态——bootstrap native 构造不经 super 链，Canvas `<init>`
//! 本就是无副作用桩）。
//!
//! **键状态位图**（getKeyStates 数据面）存在 `ClassSpace.key_states`
//! （cs 侧全局 u32，"请求位 + Vm.poll 消费"协议族的新成员）：T030
//! 事件泵在 Key press 时按"物理键 → MIDP keyCode → gameAction →
//! 状态位"换算置位；release **不清位**（MIDP javadoc：状态位反映
//! "自上次读取以来曾被按下"，物理释放不影响锁存值）。
//! `getKeyStates()` 读取即清零（latched-and-clear 语义）。
//!
//! **suppressKeyEvents 语义**：suppress=true 时事件泵不派发
//! keyPressed/keyReleased 回调（事件只进状态位）；suppress=false 时
//! 按规范"两者都发生"——回调照派、状态位照置（分流逻辑在 vm.rs
//! `pump_input_events`，本模块提供判定辅助）。
//!
//! **flushGraphics 取舍**：MIDP 双缓冲模型（离屏 Image → 提交显存）
//! 在本引擎退化为单屏图直通——离屏缓冲已就是 Canvas 屏图（T021），
//! present 通道唯一（poll 段三）。故 flush = 复用 repaint 请求位走
//! "poll 段一 paint 回调 + 段三 present"链；带参版本先区域标脏
//! （与屏求交在 maybe_spawn_paint 收敛）。GameCanvas 子类必然覆写
//! paint（Canvas.paint 抽象，javac 强制），present 链可达。

use crate::KvmError;
use crate::cldc::lang as jl;
use crate::host::HostContext;
use crate::interp::NativeCall;
use crate::midp::lcdui::{canvas, font, graphics, image, input};
use crate::runtime::classspace::ClassSpace;
use crate::runtime::heap::Heap;
use crate::runtime::{ClassId, ObjRef, Slot};

/// GameCanvas 内部名。
pub const CLASS: &str = "javax/microedition/lcdui/game/GameCanvas";

// ---------------------------------------------------------------------------
// 键状态常量（MIDP-2.0 javadoc 考证：GameCanvas 九个 *_PRESSED 位——
// UP=bit1、LEFT=bit2、DOWN=bit6、RIGHT=bit7、FIRE=bit8、GAME_A-D=
// bit9-12；bit0/3/4/5 保留未用）
// ---------------------------------------------------------------------------

/// 上键位（1<<1）。
pub const UP_PRESSED: i32 = 1 << 1; // 2
/// 下键位（1<<6）。
pub const DOWN_PRESSED: i32 = 1 << 6; // 64
/// 左键位（1<<2）。
pub const LEFT_PRESSED: i32 = 1 << 2; // 4
/// 右键位（1<<7）。
pub const RIGHT_PRESSED: i32 = 1 << 7; // 128
/// 发射键位（1<<8）。
pub const FIRE_PRESSED: i32 = 1 << 8; // 256
/// GAME_A 键位（1<<9）。
pub const GAME_A_PRESSED: i32 = 1 << 9; // 512
/// GAME_B 键位（1<<10）。
pub const GAME_B_PRESSED: i32 = 1 << 10; // 1024
/// GAME_C 键位（1<<11）。
pub const GAME_C_PRESSED: i32 = 1 << 11; // 2048
/// GAME_D 键位（1<<12）。
pub const GAME_D_PRESSED: i32 = 1 << 12; // 4096

/// MIDP keyCode → 键状态位：仅方向/FIRE 五键产生状态位（数字/软键无
/// gameAction 语义，按规范不进位图——GAME_A-D 因 MVP 无设备键布局
/// 同样无来源键）。事件泵 press 置位用。
pub fn state_bit(keycode: i32) -> Option<u32> {
    match keycode {
        input::KEY_UP => Some(UP_PRESSED as u32),
        input::KEY_DOWN => Some(DOWN_PRESSED as u32),
        input::KEY_LEFT => Some(LEFT_PRESSED as u32),
        input::KEY_RIGHT => Some(RIGHT_PRESSED as u32),
        input::KEY_FIRE => Some(FIRE_PRESSED as u32),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 辅助（槽位/类判定）
// ---------------------------------------------------------------------------

fn arg_int(call: &NativeCall, index: usize) -> i32 {
    match call.args[index] {
        Slot::Int(v) => v,
        _ => 0,
    }
}

/// 抛 `java/lang/NullPointerException`（this 为 null）。
fn npe(cs: &mut ClassSpace, heap: &mut Heap) -> KvmError {
    jl::throw_java(cs, heap, "java/lang/NullPointerException", None)
}

/// suppress 字段槽位（本类声明，每次调用现查——与 canvas_slots 同模式）。
fn suppress_slot(cs: &mut ClassSpace) -> Result<usize, KvmError> {
    let class = cs.load_class(CLASS)?;
    let name = cs.syms.intern("suppress");
    let desc = cs.syms.intern("Z");
    Ok(cs.find_field(class, name, desc)?.slot as usize)
}

/// 对象运行时类是否为 GameCanvas 或其子类（沿父链；事件泵 suppress
/// 判定用，与 `canvas::is_canvas_class` 同型）。
pub fn is_game_canvas_class(cs: &ClassSpace, mut class: ClassId) -> bool {
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

/// current displayable 是否抑制键事件回调（事件泵分流判定）：
/// 运行时类为 GameCanvas 子类且其实例字段 suppress=true。
/// 类未注册/字段损坏/非 GameCanvas 一律 false（走常规回调路径）。
pub fn suppresses_key_events(cs: &mut ClassSpace, heap: &Heap, displayable: ObjRef) -> bool {
    let Ok(instance) = heap.instance(displayable) else {
        return false;
    };
    let runtime_class = instance.class;
    if !is_game_canvas_class(cs, runtime_class) {
        return false;
    }
    let Ok(slot) = suppress_slot(cs) else {
        return false;
    };
    matches!(
        heap.instance(displayable).map(|i| i.fields[slot]),
        Ok(Slot::Int(v)) if v != 0
    )
}

// ---------------------------------------------------------------------------
// 原生方法
// ---------------------------------------------------------------------------

/// `<init>(Z)V`：super 语义（Canvas 无参桩，无副作用）+ 存
/// suppressKeyEvents 标志。
pub fn init(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = call.this.unwrap_or(ObjRef::NULL);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let slot = suppress_slot(cs)?;
    heap.instance_mut(this)?.fields[slot] = Slot::Int(i32::from(arg_int(call, 0) != 0));
    Ok(None)
}

/// `getKeyStates()I`：读当前键状态位图并**自动清零**（MIDP latched
/// 语义：位自置位起锁存，首次读取返回全量并复位）。
pub fn get_key_states(
    cs: &mut ClassSpace,
    _heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let states = cs.key_states;
    cs.key_states = 0;
    Ok(Some(Slot::Int(states as i32)))
}

/// `flushGraphics()V`：全屏直通提交——置全局重绘请求位，走 poll 段一
/// paint 回调 + 段三 present 链（单屏图直通模型取舍，见模块注释）。
pub fn flush_graphics(
    cs: &mut ClassSpace,
    _heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    cs.repaint_requested = true;
    Ok(None)
}

/// `flushGraphics(IIII)V`：区域提交——先复用 `Canvas.repaint(IIII)V`
/// 的区域标脏（并集合并、与屏求交在段一收敛），再置重绘请求位。
pub fn flush_graphics_rect(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    canvas::repaint_rect(cs, heap, call)?;
    cs.repaint_requested = true;
    Ok(None)
}

/// `getGraphics()Ljavax/microedition/lcdui/Graphics;`：分配绑 Canvas
/// 屏图（懒分配，继承 T021 单一数据源）的 Graphics——复用 poll 段一
/// paint 链同款分配序列（像素数组 + 默认 Font）。
pub fn get_graphics(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = call.this.unwrap_or(ObjRef::NULL);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let screen = canvas::ensure_screen(cs, heap, host, this)?;
    if screen == ObjRef::NULL {
        // 宿主屏不可用（防御路径，与 getWidth/getHeight 同归）：无屏
        // 图可绑，返回 null 引用由游戏侧自然暴露。
        return Ok(Some(Slot::Ref(ObjRef::NULL)));
    }
    let w = image::image_width(cs, heap, screen)?;
    let h = image::image_height(cs, heap, screen)?;
    let target = image::pixels_array_ref(cs, heap, screen)?;
    let default_font = font::default_font(cs, heap, host)?;
    let g = graphics::alloc_graphics(cs, heap, target, w, h, default_font)?;
    Ok(Some(Slot::Ref(g)))
}
