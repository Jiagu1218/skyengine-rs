//! 取证用运行时开关：drawRegion transform 语义 A/B 实验变体。
//!
//! GUI 宿主（skyengine-sdl）按 F9 循环切换（经 skyengine-core 的 re-export
//! 写入本原子），`draw_region` 每次调用读取当前变体。变体仅重排
//! transform→(mirror, quarter) 映射，不动几何实现本身；全部变体下
//! 基准语义（变体 0）= 斯凯原生编号（6=ROT270 / 7=MIRROR_ROT90），
//! 其余变体用于实机双盲排除：
//!
//! - 0 `sky`：斯凯原生表（默认，VMRP mr_drawRegion 逐 case 对齐）；
//! - 1 `jsr`：JSR-118 编号（6/7 与斯凯互换，其余相同）；
//! - 2 `swap13`：1/3 互换（MIRROR_ROT180 ↔ ROT180，验证垂直族）；
//! - 3 `nomirror`：镜像族全部去镜像（2/4/6/7 视作无镜像旋转，
//!   排查"边框呈平移复制而非镜像"的形态假设）。

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

static TRANSFORM_VARIANT: AtomicU8 = AtomicU8::new(0);

/// 变体总数（F9 循环取模用）。
pub const TRANSFORM_VARIANT_COUNT: u8 = 4;

/// 变体短名（日志/文件名用）。
pub const TRANSFORM_VARIANT_NAMES: [&str; 4] = ["sky", "jsr", "swap13", "nomirror"];

/// 当前变体编号。
pub fn transform_variant() -> u8 {
    TRANSFORM_VARIANT.load(Ordering::Relaxed)
}

/// 设置变体（GUI F9 处理器调用）。
pub fn set_transform_variant(variant: u8) {
    TRANSFORM_VARIANT.store(variant % TRANSFORM_VARIANT_COUNT, Ordering::Relaxed);
}

/// 循环切换到下一个变体，返回切换后的编号。
pub fn cycle_transform_variant() -> u8 {
    let next = (TRANSFORM_VARIANT.load(Ordering::Relaxed) + 1) % TRANSFORM_VARIANT_COUNT;
    TRANSFORM_VARIANT.store(next, Ordering::Relaxed);
    next
}

static PROFILE_DUMP_REQUEST: AtomicBool = AtomicBool::new(false);
static PROFILE_EVER_REQUESTED: AtomicBool = AtomicBool::new(false);

/// 请求导出调用剖析（GUI F8 写，core 帧 pump 消费后写文件）。
/// 首次请求同时粘性激活调用计数（免 SKYENGINE_KVM_PROFILE=1 漏配
/// 导致空表——首按即从零计数）。差分用法：操作前/后各按一次，
/// 计数差即该操作路径的方法集。
pub fn request_profile_dump() {
    PROFILE_DUMP_REQUEST.store(true, Ordering::Relaxed);
    PROFILE_EVER_REQUESTED.store(true, Ordering::Relaxed);
}

/// 消费导出请求（返回 true 表示本次需要导出）。
pub fn take_profile_dump_request() -> bool {
    PROFILE_DUMP_REQUEST.swap(false, Ordering::Relaxed)
}

/// 是否曾经请求过剖析（粘性）：invoke 计数点的激活条件之一。
pub fn profile_ever_requested() -> bool {
    PROFILE_EVER_REQUESTED.load(Ordering::Relaxed)
}
