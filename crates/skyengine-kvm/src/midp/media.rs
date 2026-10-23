//! javax.microedition.media（MMAPI / JSR-135）Manager/Player 真实现
//! （T038 状态机；解码后端归 T039 不做）。
//!
//! ## 状态机口径（JSR-135 javadoc，迁移表定死）
//!
//! `state` 实例字段直接存 Player 常量值：UNREALIZED=0 / REALIZED=100 /
//! PREFETCHED=200 / STARTED=300 / CLOSED=400（静态常量同值注册于
//! natives.rs DECL，Java 侧 `getState() == Player.STARTED` 直接成立，
//! 与真机一致）。迁移规则：
//!
//! - `realize`：UNREALIZED→REALIZED；REALIZED 及以上（含 PREFETCHED/
//!   STARTED）**幂等 no-op**（javadoc："realize 在 REALIZED/PREFETCHED/
//!   STARTED 是 no-op"）；CLOSED→IllegalStateException。
//! - `prefetch`：UNREALIZED/REALIZED→PREFETCHED；PREFETCHED 幂等；
//!   STARTED 保持（prefetch 只前进到 PREFETCHED，已越过即 no-op）；
//!   CLOSED→ISE。
//! - `start`：任何未关闭态→STARTED（内部蕴含 realize+prefetch）；
//!   STARTED no-op；CLOSED→ISE。
//! - `stop`：STARTED→PREFETCHED；其余未关闭态 no-op；CLOSED→ISE。
//! - `deallocate`：STARTED/PREFETCHED→REALIZED；其余未关闭态 no-op；
//!   CLOSED→ISE。
//! - `close`：任何态→CLOSED；CLOSED 再 close **幂等 no-op**（javadoc
//!   明文）。CLOSED 后除 close/getState 外一律 ISE；getState 返回
//!   CLOSED（400）以保持可观测（javadoc 未禁止，实现侧惯例）。
//!
//! ## 音频降级（D7 决策）
//!
//! HostContext 已双实现 audio_available/audio_submit；无音频设备环境
//! （Mock/Engine 均 false）下媒体调用**全链正确降级**：start 照常推进
//! 状态机、playTone 直过返回，首次触发打一条降级 trace，采样合成/
//! 提交与解码后端一并归 T039。
//!
//! ## 内容类型嗅探（供 T039 解码定 type）
//!
//! createPlayer(流, null/空 mime) 时按魔数嗅探：WAV（"RIFF"+8 处
//! "WAVE"）→ audio/x-wav；MIDI（"MThd"）→ audio/midi；MP3（"ID3" 或
//! 0xFF Ex 帧头）→ audio/mpeg；嗅探不出抛 MediaException。
//!
//! ## 监听器（降级）
//!
//! addPlayerListener/removePlayerListener 存取 `listeners [Ljava/lang/
//! Object;` 数组（null 起始，满则翻倍扩容，remove 置空槽幂等）。
//! **事件不触发**——回调需解释器重入，T039 定时事件（TODO）。
//!
//! ## VolumeControl
//!
//! getControl("VolumeControl") 返回内部实现类 `javax/microedition/
//! media/control/VolumeControlImpl`（字段 `owner LPlayer;`，方法转发
//! owner 的 level/muted 实例字段；单例缓存在 Player 的 `volumeImpl`
//! 字段）。MIDP 游戏常见的直转型因此不会 ClassCastException。
//! getControl 其它名（ToneControl/MIDIControl 等）返回 null（T039）。

use std::sync::atomic::{AtomicBool, Ordering};

use crate::cldc::io::{alloc_byte_array_from, input_read_to_vec};
use crate::cldc::lang as jl;
use crate::host::HostContext;
use crate::interp::NativeCall;
use crate::runtime::classspace::ClassSpace;
use crate::runtime::heap::Heap;
use crate::runtime::object::{ArrayData, ElemType};
use crate::runtime::{ObjRef, Slot};
use crate::KvmError;

/// Player bootstrap 实现类名（实例字段的声明类）。
const PLAYER_CLASS: &str = "javax/microedition/media/Player";
/// VolumeControl 内部实现类名（本模块注册的 bootstrap 类）。
const VOLUME_IMPL_CLASS: &str = "javax/microedition/media/control/VolumeControlImpl";

/// Player 状态值（JSR-135 常量；静态常量字段同值注册于 natives.rs）。
const STATE_UNREALIZED: i32 = 0;
const STATE_REALIZED: i32 = 100;
const STATE_PREFETCHED: i32 = 200;
const STATE_STARTED: i32 = 300;
const STATE_CLOSED: i32 = 400;

/// 音频降级提示只打一次（避免刷屏；对齐 rms 的 DEGRADE_NOTIFIED）。
static AUDIO_DEGRADE_NOTIFIED: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// 小工具（对齐 midp/rms.rs 的实例区访问模式）
// ---------------------------------------------------------------------------

/// 字段槽位（find_field 保守取位）。
fn field_slot(
    cs: &mut ClassSpace,
    class_name: &str,
    name: &str,
    desc: &str,
) -> Result<usize, KvmError> {
    let class = cs.load_class(class_name)?;
    let name = cs.syms.intern(name);
    let desc = cs.syms.intern(desc);
    Ok(cs.find_field(class, name, desc)?.slot as usize)
}

fn field_int(heap: &Heap, this: ObjRef, slot: usize) -> Result<i32, KvmError> {
    match &heap.instance(this)?.fields[slot] {
        Slot::Int(value) => Ok(*value),
        other => Err(KvmError::Internal(format!(
            "field slot {slot} expected int, got {other:?}"
        ))),
    }
}

fn field_ref(heap: &Heap, this: ObjRef, slot: usize) -> Result<ObjRef, KvmError> {
    match &heap.instance(this)?.fields[slot] {
        Slot::Ref(r) => Ok(*r),
        other => Err(KvmError::Internal(format!(
            "field slot {slot} expected ref, got {other:?}"
        ))),
    }
}

/// 读 String 字段文本（slot 由调用方经 field_slot 查好；null 返回 None）。
fn read_string_field(heap: &Heap, this: ObjRef, slot: usize) -> Option<String> {
    let reference = field_ref(heap, this, slot).ok()?;
    if reference == ObjRef::NULL {
        return None;
    }
    match &heap.payload(reference) {
        Ok(crate::runtime::object::Payload::Str(s)) => {
            Some(String::from_utf16_lossy(&s.chars))
        }
        _ => None,
    }
}

fn set_field(heap: &mut Heap, this: ObjRef, slot: usize, value: Slot) -> Result<(), KvmError> {
    heap.instance_mut(this)?.fields[slot] = value;
    Ok(())
}

/// 分配指定 bootstrap 类的实例（模板零值）。
fn alloc_instance_of(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    class_name: &str,
) -> Result<ObjRef, KvmError> {
    let class = cs.load_class(class_name)?;
    let template = cs.class(class).instance_template.clone();
    heap.alloc_instance(class, template)
}

fn arg_int(call: &NativeCall, index: usize) -> Result<i32, KvmError> {
    match call.args.get(index) {
        Some(Slot::Int(value)) => Ok(*value),
        other => Err(KvmError::Internal(format!(
            "arg {index} expected int, got {other:?}"
        ))),
    }
}

fn arg_ref(call: &NativeCall, index: usize) -> Result<ObjRef, KvmError> {
    match call.args.get(index) {
        Some(Slot::Ref(value)) => Ok(*value),
        other => Err(KvmError::Internal(format!(
            "arg {index} expected ref, got {other:?}"
        ))),
    }
}

/// String 载荷 → Rust 字符串（UTF-16 有损转换）。
fn string_text(heap: &Heap, reference: ObjRef) -> Result<String, KvmError> {
    let chars = heap.str_obj(reference)?.chars.clone();
    Ok(String::from_utf16_lossy(&chars))
}

fn utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

/// 接收者提取（native 分派保证非 static 调用带 this）。
fn this_ref(call: &NativeCall, who: &str) -> Result<ObjRef, KvmError> {
    call.this
        .ok_or_else(|| KvmError::Internal(format!("{who}: no receiver")))
}

/// 抛 MediaException 的速记。
fn throw_media(cs: &mut ClassSpace, heap: &mut Heap, message: &str) -> KvmError {
    jl::throw_java(
        cs,
        heap,
        "javax/microedition/media/MediaException",
        Some(message),
    )
}

/// 抛 IllegalStateException 的速记（CLOSED/STARTED 态非法调用）。
fn throw_illegal_state(cs: &mut ClassSpace, heap: &mut Heap, message: &str) -> KvmError {
    jl::throw_java(cs, heap, "java/lang/IllegalStateException", Some(message))
}

/// 抛 NullPointerException 的速记（规范要求的 null 参数）。
fn throw_npe(cs: &mut ClassSpace, heap: &mut Heap, message: &str) -> KvmError {
    jl::throw_java(cs, heap, "java/lang/NullPointerException", Some(message))
}

// ---------------------------------------------------------------------------
// 音频降级（D7：无音频设备环境全链照常）
// ---------------------------------------------------------------------------

/// 无音频设备时打一条降级 trace（仅首次）；状态机照常推进（D7 决策），
/// 采样合成/提交归 T039。
fn note_audio_degrade(host: &dyn HostContext) {
    if !host.audio_available() && !AUDIO_DEGRADE_NOTIFIED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "[media] 音频设备不可用：媒体调用降级为静默（状态机照常，采样合成/提交归 T039）"
        );
    }
}

// ---------------------------------------------------------------------------
// 内容类型嗅探（T038 定 type 供 T039 解码；嗅探不出 → MediaException）
// ---------------------------------------------------------------------------

/// 按魔数嗅探内容类型；命中返回 MMAPI 内容类型串，嗅探不出 None。
fn sniff_content_type(data: &[u8]) -> Option<&'static str> {
    // WAV：RIFF <len:4> WAVE
    if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WAVE" {
        return Some("audio/x-wav");
    }
    // MIDI：MThd 头
    if data.len() >= 4 && &data[0..4] == b"MThd" {
        return Some("audio/midi");
    }
    // MP3：ID3v2 标签或 MPEG 帧同步（0xFF Ex）
    if data.len() >= 3 && &data[0..3] == b"ID3" {
        return Some("audio/mpeg");
    }
    if data.len() >= 2 && data[0] == 0xFF && data[1] & 0xE0 == 0xE0 {
        return Some("audio/mpeg");
    }
    None
}

// ---------------------------------------------------------------------------
// Player 装配
// ---------------------------------------------------------------------------

/// 构造 Player 实例并写齐实例字段初值（Manager.createPlayer 专用，
/// 不走 <init>；真机 Player 为接口，本引擎以 bootstrap 类兼作实现）。
fn alloc_player(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    data: Option<&[u8]>,
    mime: Option<&str>,
) -> Result<Slot, KvmError> {
    let player = alloc_instance_of(cs, heap, PLAYER_CLASS)?;
    let state = field_slot(cs, PLAYER_CLASS, "state", "I")?;
    let loop_count = field_slot(cs, PLAYER_CLASS, "loopCount", "I")?;
    let level = field_slot(cs, PLAYER_CLASS, "level", "I")?;
    // data/mime/listeners/volumeImpl 模板零值已是 null（Slot::Ref(NULL)）。
    set_field(heap, player, state, Slot::Int(STATE_UNREALIZED))?;
    // MMAPI 默认循环一次（getLoopCount 不在接口面，行为验证归 T039）。
    set_field(heap, player, loop_count, Slot::Int(1))?;
    // VolumeControl 默认音量 100（规范默认最大）。
    set_field(heap, player, level, Slot::Int(100))?;
    if let Some(bytes) = data {
        let slot = field_slot(cs, PLAYER_CLASS, "data", "[B")?;
        let reference = alloc_byte_array_from(cs, heap, bytes)?;
        set_field(heap, player, slot, Slot::Ref(reference))?;
    }
    if let Some(text) = mime {
        let slot = field_slot(cs, PLAYER_CLASS, "mime", "Ljava/lang/String;")?;
        let reference = heap.alloc_string_uninterned(utf16(text))?;
        set_field(heap, player, slot, Slot::Ref(reference))?;
    }
    Ok(Slot::Ref(player))
}

// ---------------------------------------------------------------------------
// Manager 原生实现
// ---------------------------------------------------------------------------

/// Manager.createPlayer(InputStream, String)：读尽流（复用 T018 的
/// input_read_to_vec 读取通道）存字节与 mime 到新 Player；mime 为 null
/// 或空串时按魔数嗅探，嗅探不出抛 MediaException。
pub(crate) fn manager_create_player_stream(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let stream = arg_ref(call, 0)?;
    if stream.is_null() {
        return Err(throw_npe(cs, heap, "input stream is null"));
    }
    let data = input_read_to_vec(cs, heap, stream)?;
    // mime 显式给值优先；null/空串走嗅探。
    let mime_ref = arg_ref(call, 1)?;
    let explicit = if mime_ref.is_null() {
        None
    } else {
        let text = string_text(heap, mime_ref)?;
        Some(text).filter(|t| !t.is_empty())
    };
    let mime = match explicit {
        Some(text) => text,
        None => sniff_content_type(&data)
            .ok_or_else(|| throw_media(cs, heap, "unable to determine content type"))?
            .to_string(),
    };
    Ok(Some(alloc_player(cs, heap, Some(&data), Some(&mime))?))
}

/// Manager.createPlayer(String locator)："device://tone" → 空 Player
/// （ToneControl 目标，T039）；http/file 等网络定位符 →
/// ConnectionNotFoundException（GCF 归 US8）；未知 → MediaException。
pub(crate) fn manager_create_player_locator(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let locator_ref = arg_ref(call, 0)?;
    if locator_ref.is_null() {
        return Err(throw_npe(cs, heap, "locator is null"));
    }
    let locator = string_text(heap, locator_ref)?;
    if locator == "device://tone" {
        // 空 Player：无数据无类型（ToneControl 序列驱动，T039）。
        return Ok(Some(alloc_player(cs, heap, None, None)?));
    }
    // 网络/文件定位符需要 GCF 通道（US8 范围），按规范抛
    // ConnectionNotFoundException 提示上层走Connector重试。
    if locator.starts_with("http://")
        || locator.starts_with("https://")
        || locator.starts_with("file:")
        || locator.starts_with("socket:")
    {
        return Err(jl::throw_java(
            cs,
            heap,
            "javax/io/ConnectionNotFoundException",
            Some(&format!("locator requires GCF (US8): {locator}")),
        ));
    }
    Err(throw_media(cs, heap, &format!("unsupported locator: {locator}")))
}

/// Manager.playTone(int note, int duration, int volume)：无音频设备
/// 直过返回（D7 降级；T039 接合成器）。
pub(crate) fn manager_play_tone(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    host: &mut dyn HostContext,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    note_audio_degrade(host);
    Ok(None)
}

// ---------------------------------------------------------------------------
// Player 状态机
// ---------------------------------------------------------------------------

/// realize()：UNREALIZED→REALIZED；REALIZED 及以上幂等 no-op；CLOSED→ISE。
pub(crate) fn player_realize(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "realize")?;
    let slot = field_slot(cs, PLAYER_CLASS, "state", "I")?;
    match field_int(heap, this, slot)? {
        STATE_CLOSED => Err(throw_illegal_state(cs, heap, "player is closed")),
        STATE_UNREALIZED => {
            set_field(heap, this, slot, Slot::Int(STATE_REALIZED))?;
            Ok(None)
        }
        // REALIZED/PREFETCHED/STARTED：幂等 no-op（JSR-135 javadoc 口径）。
        _ => Ok(None),
    }
}

/// prefetch()：≤PREFETCHED 前进/幂等到 PREFETCHED；STARTED 保持；CLOSED→ISE。
pub(crate) fn player_prefetch(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "prefetch")?;
    let slot = field_slot(cs, PLAYER_CLASS, "state", "I")?;
    match field_int(heap, this, slot)? {
        STATE_CLOSED => Err(throw_illegal_state(cs, heap, "player is closed")),
        // STARTED 已越过 PREFETCHED：no-op（任务口径幂等优先）。
        STATE_STARTED => Ok(None),
        _ => {
            set_field(heap, this, slot, Slot::Int(STATE_PREFETCHED))?;
            Ok(None)
        }
    }
}

/// start()：任何未关闭态→STARTED（内部蕴含 realize+prefetch）；
/// STARTED no-op；CLOSED→ISE。无音频设备照常推进（D7，trace 降级一次）。
pub(crate) fn player_start(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "start")?;
    let slot = field_slot(cs, PLAYER_CLASS, "state", "I")?;
    match field_int(heap, this, slot)? {
        STATE_CLOSED => Err(throw_illegal_state(cs, heap, "player is closed")),
        STATE_STARTED => Ok(None),
        _ => {
            // 编码数据直交宿主引擎解码合成（MIDI 走 rustysynth 通道）；
            // 受理与否状态机照常推进（无设备降级 D7）。loopCount==0 为
            // 无限循环（MIDP 语义，T038 口径）。
            let data_ref =
                field_ref(heap, this, field_slot(cs, PLAYER_CLASS, "data", "[B")?)
                    .unwrap_or(ObjRef::NULL);
            if data_ref != ObjRef::NULL {
                let bytes: Vec<u8> = match &heap.array(data_ref)?.data {
                    ArrayData::Byte(data) => data.iter().map(|&b| b as u8).collect(),
                    _ => Vec::new(),
                };
                let mime = read_string_field(
                    heap,
                    this,
                    field_slot(cs, PLAYER_CLASS, "mime", "Ljava/lang/String;")?,
                )
                .unwrap_or_default();
                let loop_count =
                    field_int(heap, this, field_slot(cs, PLAYER_CLASS, "loopCount", "I")?)
                        .unwrap_or(1);
                host.audio_play(&bytes, &mime, loop_count == 0);
            } else {
                note_audio_degrade(host);
            }
            set_field(heap, this, slot, Slot::Int(STATE_STARTED))?;
            Ok(None)
        }
    }
}

/// stop()：STARTED→PREFETCHED（停播）；其余未关闭态 no-op；CLOSED→ISE。
pub(crate) fn player_stop(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "stop")?;
    let slot = field_slot(cs, PLAYER_CLASS, "state", "I")?;
    match field_int(heap, this, slot)? {
        STATE_CLOSED => Err(throw_illegal_state(cs, heap, "player is closed")),
        STATE_STARTED => {
            host.audio_stop();
            set_field(heap, this, slot, Slot::Int(STATE_PREFETCHED))?;
            Ok(None)
        }
        _ => Ok(None),
    }
}

/// deallocate()：STARTED/PREFETCHED→REALIZED；其余未关闭态 no-op；
/// CLOSED→ISE。
pub(crate) fn player_deallocate(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "deallocate")?;
    let slot = field_slot(cs, PLAYER_CLASS, "state", "I")?;
    match field_int(heap, this, slot)? {
        STATE_CLOSED => Err(throw_illegal_state(cs, heap, "player is closed")),
        STATE_STARTED | STATE_PREFETCHED => {
            set_field(heap, this, slot, Slot::Int(STATE_REALIZED))?;
            Ok(None)
        }
        _ => Ok(None),
    }
}

/// close()：任何态→CLOSED（停播）；CLOSED 再 close 幂等 no-op。
pub(crate) fn player_close(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "close")?;
    let slot = field_slot(cs, PLAYER_CLASS, "state", "I")?;
    if field_int(heap, this, slot)? != STATE_CLOSED {
        if field_int(heap, this, slot)? == STATE_STARTED {
            host.audio_stop();
        }
        set_field(heap, this, slot, Slot::Int(STATE_CLOSED))?;
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Player 查询/属性
// ---------------------------------------------------------------------------

/// getState()：返回常量值（CLOSED 也返回 400 保持可观测）。
pub(crate) fn player_get_state(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "getState")?;
    let slot = field_slot(cs, PLAYER_CLASS, "state", "I")?;
    Ok(Some(Slot::Int(field_int(heap, this, slot)?)))
}

/// setLoopCount(int)：CLOSED→ISE；-1/负值=无限循环（MIDP javadoc：
/// "-1 indicates infinite looping"——gjxjwql 传负值设 BGM 无限，抛 IAE
/// 会令游戏 catch 分支弹画面栈）；0 同为无限。STARTED 态直接生效
///（javadoc "may be called at any time"，对已提交宿主的播放无实时
/// 改环效果，仅记字段）。
pub(crate) fn player_set_loop_count(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn crate::host::HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "setLoopCount")?;
    let count = arg_int(call, 0)?;
    let state_slot = field_slot(cs, PLAYER_CLASS, "state", "I")?;
    if field_int(heap, this, state_slot)? == STATE_CLOSED {
        return Err(throw_illegal_state(cs, heap, "player is closed"));
    }
    // 负值=无限（与 loopCount==0 同义——player_start 以 0 判无限）。
    let stored = if count < 0 { 0 } else { count };
    let slot = field_slot(cs, PLAYER_CLASS, "loopCount", "I")?;
    set_field(heap, this, slot, Slot::Int(stored))?;
    // STARTED 态改循环实时下发（gjxjwql BGM 时序：start 后才
    // setLoopCount(-1)——音频命令仅在 start 时提交一次，后设的循环
    // 标志需运行中通道同步，否则一遍即停）。
    if stored == 0
        && field_int(heap, this, state_slot)? == STATE_STARTED
    {
        host.audio_set_loop_forever(true);
    }
    Ok(None)
}

/// getDuration()：无解码返回 TIME_UNKNOWN(-1)（T039 按媒体头解析时长）。
pub(crate) fn player_get_duration(
    _cs: &mut ClassSpace,
    _heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Long(-1)))
}

/// getContentType()：createPlayer 确定的类型（locator 版 null）。
pub(crate) fn player_get_content_type(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "getContentType")?;
    let slot = field_slot(cs, PLAYER_CLASS, "mime", "Ljava/lang/String;")?;
    Ok(Some(Slot::Ref(field_ref(heap, this, slot)?)))
}

/// getMediaTime()：简化口径——未到 PREFETCHED 返回 -1（未知），否则 0
/// （播放时钟归 T039 定时事件）。
pub(crate) fn player_get_media_time(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "getMediaTime")?;
    let slot = field_slot(cs, PLAYER_CLASS, "state", "I")?;
    let state = field_int(heap, this, slot)?;
    let now = if state >= STATE_PREFETCHED && state != STATE_CLOSED {
        0
    } else {
        -1
    };
    Ok(Some(Slot::Long(now)))
}

/// setMediaTime(long)：CLOSED→ISE；简化接受请求并原值返回（T039）。
pub(crate) fn player_set_media_time(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "setMediaTime")?;
    let slot = field_slot(cs, PLAYER_CLASS, "state", "I")?;
    if field_int(heap, this, slot)? == STATE_CLOSED {
        return Err(throw_illegal_state(cs, heap, "player is closed"));
    }
    match call.args.first() {
        Some(Slot::Long(now)) => Ok(Some(Slot::Long(*now))),
        other => Err(KvmError::Internal(format!(
            "setMediaTime arg 0 expected long, got {other:?}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// PlayerListener（注册接受、事件不触发——T039 降级口径）
// ---------------------------------------------------------------------------

/// addPlayerListener：null→NPE；CLOSED→ISE；存入 listeners 数组
/// （null 起始长度 4，满则翻倍扩容；同引用幂等跳过）。
pub(crate) fn player_add_listener(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "addPlayerListener")?;
    let listener = arg_ref(call, 0)?;
    if listener.is_null() {
        return Err(throw_npe(cs, heap, "listener is null"));
    }
    let state_slot = field_slot(cs, PLAYER_CLASS, "state", "I")?;
    if field_int(heap, this, state_slot)? == STATE_CLOSED {
        return Err(throw_illegal_state(cs, heap, "player is closed"));
    }
    let listeners_slot = field_slot(cs, PLAYER_CLASS, "listeners", "[Ljava/lang/Object;")?;
    let listeners = field_ref(heap, this, listeners_slot)?;
    if listeners.is_null() {
        // 首个监听器：分配 4 槽数组。
        let class = cs.load_class("[Ljava/lang/Object;")?;
        let array = heap.alloc_array(class, ElemType::Ref, 4)?;
        if let ArrayData::Refs(slots) = &mut heap.array_mut(array)?.data {
            slots[0] = listener;
        }
        set_field(heap, this, listeners_slot, Slot::Ref(array))?;
        return Ok(None);
    }
    // 已有数组：查重幂等，满则翻倍扩容后追加。
    let existing: Vec<ObjRef> = match &heap.array(listeners)?.data {
        ArrayData::Refs(slots) => slots.clone(),
        other => {
            return Err(KvmError::Internal(format!(
                "listeners array expected refs, got {other:?}"
            )))
        }
    };
    if existing.contains(&listener) {
        return Ok(None);
    }
    match existing.iter().position(|r| r.is_null()) {
        Some(index) => {
            if let ArrayData::Refs(slots) = &mut heap.array_mut(listeners)?.data {
                slots[index] = listener;
            }
        }
        None => {
            let class = cs.load_class("[Ljava/lang/Object;")?;
            let grown = existing.len() * 2;
            let array = heap.alloc_array(class, ElemType::Ref, grown)?;
            let mut next = existing.clone();
            next.push(listener);
            if let ArrayData::Refs(slots) = &mut heap.array_mut(array)?.data {
                slots.copy_from_slice(&next);
            }
            set_field(heap, this, listeners_slot, Slot::Ref(array))?;
        }
    }
    Ok(None)
}

/// removePlayerListener：null→NPE；CLOSED→ISE；置空匹配槽（幂等）。
pub(crate) fn player_remove_listener(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "removePlayerListener")?;
    let listener = arg_ref(call, 0)?;
    if listener.is_null() {
        return Err(throw_npe(cs, heap, "listener is null"));
    }
    let state_slot = field_slot(cs, PLAYER_CLASS, "state", "I")?;
    if field_int(heap, this, state_slot)? == STATE_CLOSED {
        return Err(throw_illegal_state(cs, heap, "player is closed"));
    }
    let listeners_slot = field_slot(cs, PLAYER_CLASS, "listeners", "[Ljava/lang/Object;")?;
    let listeners = field_ref(heap, this, listeners_slot)?;
    if !listeners.is_null()
        && let ArrayData::Refs(slots) = &mut heap.array_mut(listeners)?.data {
            for slot in slots.iter_mut() {
                if *slot == listener {
                    *slot = ObjRef::NULL;
                }
            }
        }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Control 门面（VolumeControl 真实现；ToneControl/MIDIControl 归 T039）
// ---------------------------------------------------------------------------

/// getControl(String)：null→NPE；CLOSED→ISE；"VolumeControl" 返回
/// VolumeControlImpl 单例（懒建并缓存在 Player 的 volumeImpl 字段，
/// 方法转发 owner 的 level/muted）；其它名返回 null（T039）。
pub(crate) fn player_get_control(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "getControl")?;
    let name_ref = arg_ref(call, 0)?;
    if name_ref.is_null() {
        return Err(throw_npe(cs, heap, "control type is null"));
    }
    let state_slot = field_slot(cs, PLAYER_CLASS, "state", "I")?;
    if field_int(heap, this, state_slot)? == STATE_CLOSED {
        return Err(throw_illegal_state(cs, heap, "player is closed"));
    }
    if string_text(heap, name_ref)? != "VolumeControl" {
        // ToneControl/MIDIControl 等归 T039；占位返回 null。
        return Ok(Some(Slot::Ref(ObjRef::NULL)));
    }
    let cache_slot = field_slot(cs, PLAYER_CLASS, "volumeImpl", "Ljavax/microedition/media/control/VolumeControlImpl;")?;
    let cached = field_ref(heap, this, cache_slot)?;
    if !cached.is_null() {
        return Ok(Some(Slot::Ref(cached)));
    }
    let control = alloc_instance_of(cs, heap, VOLUME_IMPL_CLASS)?;
    let owner_slot = field_slot(cs, VOLUME_IMPL_CLASS, "owner", "Ljavax/microedition/media/Player;")?;
    set_field(heap, control, owner_slot, Slot::Ref(this))?;
    set_field(heap, this, cache_slot, Slot::Ref(control))?;
    Ok(Some(Slot::Ref(control)))
}

/// getControls()：CLOSED→ISE；返回单元素 [VolumeControlImpl]
/// （与 getControl("VolumeControl") 同一单例；T039 增补 ToneControl）。
pub(crate) fn player_get_controls(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "getControls")?;
    let state_slot = field_slot(cs, PLAYER_CLASS, "state", "I")?;
    if field_int(heap, this, state_slot)? == STATE_CLOSED {
        return Err(throw_illegal_state(cs, heap, "player is closed"));
    }
    // 与 getControl("VolumeControl") 同一单例：命中缓存或懒建。
    let cache_slot = field_slot(cs, PLAYER_CLASS, "volumeImpl", "Ljavax/microedition/media/control/VolumeControlImpl;")?;
    let control = {
        let cached = field_ref(heap, this, cache_slot)?;
        if !cached.is_null() {
            cached
        } else {
            let control = alloc_instance_of(cs, heap, VOLUME_IMPL_CLASS)?;
            let owner_slot = field_slot(cs, VOLUME_IMPL_CLASS, "owner", "Ljavax/microedition/media/Player;")?;
            set_field(heap, control, owner_slot, Slot::Ref(this))?;
            set_field(heap, this, cache_slot, Slot::Ref(control))?;
            control
        }
    };
    let class = cs.load_class("[Ljavax/microedition/media/Control;")?;
    let array = heap.alloc_array(class, ElemType::Ref, 1)?;
    if let ArrayData::Refs(slots) = &mut heap.array_mut(array)?.data {
        slots[0] = control;
    }
    Ok(Some(Slot::Ref(array)))
}

// ---------------------------------------------------------------------------
// VolumeControlImpl（转发 owner 的 level/muted 字段）
// ---------------------------------------------------------------------------

/// 控制对象的 owner Player。
fn control_owner(heap: &Heap, this: ObjRef, cs: &mut ClassSpace) -> Result<ObjRef, KvmError> {
    let slot = field_slot(cs, VOLUME_IMPL_CLASS, "owner", "Ljavax/microedition/media/Player;")?;
    field_ref(heap, this, slot)
}

/// setLevel(int)：钳 0-100 写 owner.level，返回生效值。
pub(crate) fn vc_set_level(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "VolumeControl.setLevel")?;
    let value = arg_int(call, 0)?.clamp(0, 100);
    let owner = control_owner(heap, this, cs)?;
    let slot = field_slot(cs, PLAYER_CLASS, "level", "I")?;
    set_field(heap, owner, slot, Slot::Int(value))?;
    Ok(Some(Slot::Int(value)))
}

/// getLevel()：读 owner.level。
pub(crate) fn vc_get_level(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "VolumeControl.getLevel")?;
    let owner = control_owner(heap, this, cs)?;
    let slot = field_slot(cs, PLAYER_CLASS, "level", "I")?;
    Ok(Some(Slot::Int(field_int(heap, owner, slot)?)))
}

/// setMute(boolean)：写 owner.muted（音量字段不动，规范语义）。
pub(crate) fn vc_set_mute(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "VolumeControl.setMute")?;
    let value = arg_int(call, 0)?;
    let owner = control_owner(heap, this, cs)?;
    let slot = field_slot(cs, PLAYER_CLASS, "muted", "Z")?;
    set_field(heap, owner, slot, Slot::Int(i32::from(value != 0)))?;
    Ok(None)
}

/// isMuted()：读 owner.muted。
pub(crate) fn vc_is_muted(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call, "VolumeControl.isMuted")?;
    let owner = control_owner(heap, this, cs)?;
    let slot = field_slot(cs, PLAYER_CLASS, "muted", "Z")?;
    Ok(Some(Slot::Int(field_int(heap, owner, slot)?)))
}
