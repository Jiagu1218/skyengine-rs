//! javax/microedition/lcdui 输入子系统（T030）：引擎键名 → MIDP
//! keyCode 映射 + Canvas 游戏动作 API（getGameAction/getKeyCode/
//! getKeyName natives）。事件泵（Vm.poll drain `events` → 绿线程
//! keyPressed/keyReleased 回调）在 vm.rs 落地，本模块只做纯映射与
//! 静态查询 native，不涉线程编排。

use crate::KvmError;
use crate::cldc::lang as jl;
use crate::interp::NativeCall;
use crate::runtime::Slot;
use crate::runtime::classspace::ClassSpace;
use crate::runtime::heap::Heap;

// ---------------------------------------------------------------------------
// MIDP keyCode 常量（Canvas public static final，负值区）
// ---------------------------------------------------------------------------

pub const KEY_UP: i32 = -1;
pub const KEY_DOWN: i32 = -2;
pub const KEY_LEFT: i32 = -3;
pub const KEY_RIGHT: i32 = -4;
/// 中键/确认键（FIRE）。
pub const KEY_FIRE: i32 = -5;
pub const KEY_SOFT_LEFT: i32 = -6;
pub const KEY_SOFT_RIGHT: i32 = -7;

// ---------------------------------------------------------------------------
// MIDP gameAction 常量（UP=1 LEFT=2 RIGHT=5 DOWN=6 FIRE=8；GAME_A-D=
// 9-12 值按规范注册供常量面/测试断言——键布局映射本 MVP 不提供，故
// getGameAction/getKeyCode 仍只认方向/FIRE 五键）
// ---------------------------------------------------------------------------

pub const GAME_UP: i32 = 1;
pub const GAME_LEFT: i32 = 2;
pub const GAME_RIGHT: i32 = 5;
pub const GAME_DOWN: i32 = 6;
pub const GAME_FIRE: i32 = 8;
pub const GAME_A: i32 = 9;
pub const GAME_B: i32 = 10;
pub const GAME_C: i32 = 11;
pub const GAME_D: i32 = 12;

// ---------------------------------------------------------------------------
// 引擎键名（Vm::dispatch_key 的 key 参数值域）
//
// 考证（仓库内既有事实）：skyengine-core `DisplayEvent::Key { code: i32 }`
// 在 MRP 直通路径上把宿主键码原样透传给 mrp runtime（mr/host.rs
// `route_key_event`：`(if pressed { 0 } else { 1 }, code, 0)`）；仓库内
// 唯一既定键名常量 `MR_KEY_SOFTRIGHT = 18`（skyengine-core/src/mr/
// host.rs 与 skyengine-sdl/src/lib.rs 同值，右软键=平台取消键）。
// 故引擎键名 u8 值域约定：
// - 可打印 ASCII 区（0x20-0x7E）直接承载字符码：数字 '0'-'9'、
//   '*'、'#'（与 MRP 直通语义一致，引擎接入时 as u8 截断即可）；
// - 右软键 = 18（对齐 MR_KEY_SOFTRIGHT）；
// - 方向/FIRE/左软键仓库内无既定值 → 本任务自定义高位区
//   0x81-0x86（避开 ASCII 与 18），引擎接入时对齐本枚举。
// ---------------------------------------------------------------------------

/// 右软键（= 引擎 MR_KEY_SOFTRIGHT，平台取消键）。
pub const ENGINE_KEY_SOFT_RIGHT: u8 = 18;
pub const ENGINE_KEY_UP: u8 = 0x81;
pub const ENGINE_KEY_DOWN: u8 = 0x82;
pub const ENGINE_KEY_LEFT: u8 = 0x83;
pub const ENGINE_KEY_RIGHT: u8 = 0x84;
/// 中键/ENTER。
pub const ENGINE_KEY_FIRE: u8 = 0x85;
pub const ENGINE_KEY_SOFT_LEFT: u8 = 0x86;

/// 按键回调描述符（keyPressed/keyReleased/keyRepeated 同形）。
pub const KEY_CB_DESC: &str = "(I)V";

/// 指针回调描述符（pointerPressed/pointerReleased/pointerDragged）。
pub const POINTER_CB_DESC: &str = "(II)V";

/// 引擎键名 → MIDP keyCode；未知键返回 None（事件泵丢弃）。
pub fn midp_keycode(engine_key: u8) -> Option<i32> {
    match engine_key {
        // 数字/星/井：ASCII 字符码即 MIDP keyCode（'0'=48 ... '9'=57、
        // '*'=42、'#'=35）。
        b'0'..=b'9' | b'*' | b'#' => Some(i32::from(engine_key)),
        ENGINE_KEY_UP => Some(KEY_UP),
        ENGINE_KEY_DOWN => Some(KEY_DOWN),
        ENGINE_KEY_LEFT => Some(KEY_LEFT),
        ENGINE_KEY_RIGHT => Some(KEY_RIGHT),
        ENGINE_KEY_FIRE => Some(KEY_FIRE),
        ENGINE_KEY_SOFT_LEFT => Some(KEY_SOFT_LEFT),
        ENGINE_KEY_SOFT_RIGHT => Some(KEY_SOFT_RIGHT),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 原生方法
// ---------------------------------------------------------------------------

fn arg_int(call: &NativeCall, index: usize) -> i32 {
    match call.args[index] {
        Slot::Int(v) => v,
        _ => 0,
    }
}

/// 抛 `java/lang/IllegalArgumentException`（MIDP 对非法 keyCode/
/// gameAction 的规定行为）。
fn throw_iae(cs: &mut ClassSpace, heap: &mut Heap, message: &str) -> KvmError {
    jl::throw_java(
        cs,
        heap,
        "java/lang/IllegalArgumentException",
        Some(message),
    )
}

/// `getGameAction(I)I`：keyCode → gameAction。仅方向/FIRE 五键有游戏
/// 动作语义；数字/软键/未知 → IllegalArgumentException（MIDP 规范）。
pub fn get_game_action(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let action = match arg_int(call, 0) {
        KEY_UP => GAME_UP,
        KEY_DOWN => GAME_DOWN,
        KEY_LEFT => GAME_LEFT,
        KEY_RIGHT => GAME_RIGHT,
        KEY_FIRE => GAME_FIRE,
        other => {
            return Err(throw_iae(
                cs,
                heap,
                &format!("keyCode {other} has no game action"),
            ));
        }
    };
    Ok(Some(Slot::Int(action)))
}

/// `getKeyCode(I)I`：gameAction → keyCode（-1..-5 反向）。GAME_A-D
/// （9-12）与未知值 → IllegalArgumentException（MVP 无既定键布局）。
pub fn get_key_code(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let key = match arg_int(call, 0) {
        GAME_UP => KEY_UP,
        GAME_DOWN => KEY_DOWN,
        GAME_LEFT => KEY_LEFT,
        GAME_RIGHT => KEY_RIGHT,
        GAME_FIRE => KEY_FIRE,
        other => return Err(throw_iae(cs, heap, &format!("unknown game action {other}"))),
    };
    Ok(Some(Slot::Int(key)))
}

/// `getKeyName(I)Ljava/lang/String;`：数字→字符本身、方向→"UP" 等简名。
/// 未知 keyCode → IllegalArgumentException。
pub fn get_key_name(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let name: Vec<u16> = match arg_int(call, 0) {
        key @ 0x20..=0x7E => vec![key as u16], // 可打印 ASCII（数字/星/井）
        KEY_UP => "UP".encode_utf16().collect(),
        KEY_DOWN => "DOWN".encode_utf16().collect(),
        KEY_LEFT => "LEFT".encode_utf16().collect(),
        KEY_RIGHT => "RIGHT".encode_utf16().collect(),
        KEY_FIRE => "SELECT".encode_utf16().collect(),
        KEY_SOFT_LEFT => "SOFT1".encode_utf16().collect(),
        KEY_SOFT_RIGHT => "SOFT2".encode_utf16().collect(),
        other => return Err(throw_iae(cs, heap, &format!("unknown keyCode {other}"))),
    };
    let reference = heap.alloc_string_uninterned(name)?;
    Ok(Some(Slot::Ref(reference)))
}
