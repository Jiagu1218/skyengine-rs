//! 进程级虚拟机时序缩放（speed multiplier / speed hack）。
//!
//! MRP 游戏的主循环由宿主定时器驱动：guest 通过 mr_timer / ext timer
//! 注册回调间隔，宿主到点触发一次游戏逻辑帧。本模块把 guest 申请的
//! 定时器间隔按倍速压缩（interval / scale），使游戏逻辑以 N 倍速推进。
//!
//! 作用域（仅游戏时序）：
//! - `mr/host.rs` 的 mr_timer deadline
//! - `arm/ext/dispatch.rs` 的原生扩展 timer deadline
//! - FFI worker 循环的 tick 间隔（避免高倍速时被 10ms 量化拖慢）
//!
//! 刻意不缩放（保持真实时间）：
//! - 音频：AudioPlayer 解码后由宿主线程按真实速率拉流，BGM/音效不变速
//! - 设备时钟（device_clock_origin）：guest 读取的日期时间不变
//! - 网络超时：真实网络等待不加速

use std::fmt::Arguments;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

/// 最大允许倍速，防止极端值把定时器间隔压成零。
pub const MAX_TIME_SCALE: u32 = 8;

static TIME_SCALE: AtomicU32 = AtomicU32::new(1);

/// 设置倍速（1 = 原速）。小于 1 的值按 1 处理。
pub fn set_time_scale(mult: u32) {
    TIME_SCALE.store(mult.clamp(1, MAX_TIME_SCALE), Ordering::Release);
}

/// 当前倍速。
pub fn time_scale() -> u32 {
    TIME_SCALE.load(Ordering::Acquire)
}

/// 把 guest 申请的时长压缩为宿主实际等待时长。
pub fn scale_duration(duration: Duration) -> Duration {
    let scale = time_scale();
    if scale == 1 {
        return duration;
    }
    duration / scale
}

/// 调试追踪:SKYENGINE_TRACE_TIMER 开启时输出定时器注册/到期/停止事件,
/// 每行带 UNIX 毫秒时间戳,便于离线计算事件节奏(如 mpmusic 锁屏排查)。
/// 首次调用缓存开关,之后零开销(单次 env 读取)。
pub fn trace_timer(message: Arguments) {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    if !*ENABLED.get_or_init(|| std::env::var_os("SKYENGINE_TRACE_TIMER").is_some()) {
        return;
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    eprintln!("[timer {timestamp}] {message}");
}
