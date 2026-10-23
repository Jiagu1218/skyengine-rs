//! J2ME KVM 运行时宿主（T023~T026）。
//!
//! guest 通过 `platEx(0x70003)`（slot 38 的平台扩展命令）注册 J2ME 引擎：
//! 引擎在 output 槽写回一个 vtable 指针（8 字节表，output_len 同步写 8），
//! guest 的 sub80D04 随后读取 vtable 两槽并 `BLX` 调用——槽 0 = INIT、
//! 槽 1 = START（槽为空/0 时 guest 跳过且视为成功，不启动 KVM）。
//!
//! 两槽值是本模块保留的"魔数地址"：不指向真实指令，而是由执行循环在
//! 取指前识别（与 TRAP_BASE 平台槽同一套路，见 ext.rs 执行循环里的
//! `kvm.dispatch_magic` 拦截），把控制流转成本模块的状态迁移，结果写回
//! r0、PC 恢复 LR。
//!
//! 魔数窗口布局（映射一页可读内存，guest 需要先 ldr 读 vtable 槽值）：
//!
//! ```text
//! 0x0E00_0000  KVM_INIT_MAGIC   vtable 槽 0 指向 → KVM_INIT
//! 0x0E00_0004  KVM_START_MAGIC  vtable 槽 1 指向 → KVM_START
//! 0x0E00_0010  KVM_VTABLE       8 字节 {KVM_INIT_MAGIC, KVM_START_MAGIC}
//! ```
//!
//! 基址 0x0E00_0000 避开既有保留区（平台表/数据 0x0100_xxxx、trap 窗口
//! 0xff00_xxxx、guest 堆 0x2000_0000）。
//!
//! T025/T026 装配：platEx(0x70003) 成功时缓存 input 记录指针；KVM_INIT
//! 回读 guest 内存中的 KVM 对象（input+8 处：+0x00 包路径、+0x40 jar 名），
//! 经 `Package`（MRP 条目解析的唯一底层，同 mr/host/services.rs 的
//! read_package_file 嵌套分支）读取包内 mainclass 与 jar，装配 skyengine-kvm
//! 的 `Vm`；KVM_START 调 `start_midlet` 拉起 MIDlet 生命周期。

use super::*;
use skyengine_kvm::host::{
    FontHandle, FontMetrics, HostContext, HostNetError, HttpRequest, HttpResponse,
};
use skyengine_kvm::midp::lcdui::input as kvm_input;
use skyengine_kvm::vm::{Vm, VmConfig, VmState};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// 魔数窗口基址（一页，READ_WRITE，仅承载 vtable 与魔数地址字）。
const KVM_MAGIC_BASE: u32 = 0x0E00_0000;
/// vtable 槽 0：KVM 初始化入口（guest BLX 到此地址触发 INIT）。
pub(super) const KVM_INIT_MAGIC: u32 = KVM_MAGIC_BASE;
/// vtable 槽 1：KVM 启动入口（guest BLX 到此地址触发 START）。
pub(super) const KVM_START_MAGIC: u32 = KVM_MAGIC_BASE + 4;
/// guest 可读的 8 字节 vtable（内容 = 两个魔数地址）。
pub(super) const KVM_VTABLE: u32 = KVM_MAGIC_BASE + 0x10;

/// KVM 对象相对 input 记录的偏移：input 记录本身 8 字节（平台表指针 +
/// 31 号蹦床表指针），内联字符串区紧跟其后。
const KVM_OBJECT_OFFSET: u32 = 8;
/// KVM 对象 +0x00（input+0x08）起 0x40 字节：内联 char 包名 C 字符串
/// （gjxjwql sub80B20 实测 "gjxjwql.mrp"，T029 E2E hex dump 取证修正
/// ——T025 曾按二级 char* 指针假设，真实 guest 是内联字符串）。
const KVM_OBJECT_PACKAGE_PATH: u32 = 0x00;
/// 包名内联槽长度（input+0x08..input+0x48）。
const KVM_PACKAGE_NAME_MAX: usize = 0x40;
/// KVM 对象 +0x40（即 input+0x48）：内联 char jar 名（实测 "jc.jar"，
/// 空缺时回退 "jc.jar"）。
const KVM_OBJECT_JAR_NAME: u32 = 0x40;
/// jar 名内联槽读取上限（防未终止字符串无限读）。
const KVM_JAR_NAME_MAX: usize = 0x20;
/// 包路径/jar 名 guest C 字符串的读取上限（防未终止字符串无限读）。
/// guest 内存字符串的逐窗读取步长。
const KVM_GUEST_STRING_WINDOW: usize = 64;
/// 包内入口类名文件（内容 = MIDlet 类名）。
const KVM_MAINCLASS_ENTRY: &str = "mainclass";
/// jar 名缺省值（MRP 生态约定）。
const KVM_DEFAULT_JAR_NAME: &str = "jc.jar";

/// KVM 宿主状态机。guest 侧生命周期为 INIT →（INIT 可重入）→ START。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KvmState {
    /// 尚未收到 KVM_INIT。
    Uninitialized,
    /// KVM_INIT 已完成（Vm 已装配 jar，等待 START）。
    Initialized,
    /// KVM_START 已完成。
    Started,
    /// KVM 初始化/启动失败；后续 INIT 可重建。
    Crashed,
}

/// MR 键码 → KVM 引擎键名（`midp::lcdui::input` 的 ENGINE_KEY_* 值域）。
///
/// MR 直通键码表（skyengine-sdl key_code 先例）：0-9 数字、10='*'、
/// 11='#'、12-15 方向、16 左软、17 中键、18 右软、20 ENTER。KVM 引擎键
/// 名约定数字/星/井直接承载 ASCII 字符码（'0'=48..'9'=57），方向/FIRE/
/// 软键在 0x81-0x86 高位区（右软键沿用 18）。未覆盖的可打印 ASCII 直
/// 通；其余（负值/未知）返回 None 丢弃。
pub(super) fn mr_key_to_engine_key(code: i32) -> Option<u8> {
    match code {
        0..=9 => Some(b'0' + code as u8),
        10 => Some(b'*'),
        11 => Some(b'#'),
        12 => Some(kvm_input::ENGINE_KEY_UP),
        13 => Some(kvm_input::ENGINE_KEY_DOWN),
        14 => Some(kvm_input::ENGINE_KEY_LEFT),
        15 => Some(kvm_input::ENGINE_KEY_RIGHT),
        16 => Some(kvm_input::ENGINE_KEY_SOFT_LEFT),
        17 | 20 => Some(kvm_input::ENGINE_KEY_FIRE),
        18 => Some(kvm_input::ENGINE_KEY_SOFT_RIGHT),
        0x20..=0x7E => Some(code as u8),
        _ => None,
    }
}

/// KVM 帧 → 引擎屏幕的帧桥（T028）。
///
/// `present` 发生在 Vm 内部（poll 驱动中），那时 EngineHostContext 已整
/// 体移交 KVM、无法回访 ExtRuntime——present 把整帧 RGB565（引擎屏幕尺
/// 寸，letterbox 已做）写进本桥并置 fresh；执行循环的帧 pump 在 poll 后
/// 取走 fresh 帧刷 guest `screen_base` 并经 `draw_bitmap` 上屏（与 MRP
/// guest 绘制最终上屏同一通道，见 ext.rs `present_screen` 先例）。
#[derive(Debug, Default)]
struct KvmFrameBridge {
    /// 整帧 RGB565（LE u16/像素），长度恒为屏幕宽×高×2。
    rgb565: Vec<u8>,
    /// present 自上次取走后是否又写入新帧。
    fresh: bool,
    /// 音频命令队列（Player.start/stop 经 host 入队、帧泵 drain 交
    /// 引擎 NativeServices 播放——present 同款桥模式）。
    audio: std::collections::VecDeque<KvmAudioCmd>,
}

/// KVM→引擎音频命令（编码数据直交，解码合成归引擎 AudioPlayer）。
#[derive(Debug)]
pub(super) enum KvmAudioCmd {
    /// 运行中改循环标志（STARTED 态 setLoopCount 同步）。
    SetLoop { loop_forever: bool },
    Play {
        data: Vec<u8>,
        mime: String,
        loop_forever: bool,
    },
    Stop,
}

/// 单帧 pump 的 KVM 指令预算（默认 200k；SKYENGINE_KVM_BUDGET 可覆盖
/// ——实测 50k 下 RPG 主循环逻辑预算不足表现为卡顿）。
pub(super) fn kvm_poll_budget() -> u64 {
    std::env::var("SKYENGINE_KVM_BUDGET")
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(200_000)
}

/// 执行循环内两次帧泵的最小间隔（~60fps 驱动上限；poll 返回的
/// next_wake 不用管——时钟真实，阻塞线程按真实时间自然唤醒）。
/// 帧泵最小间隔：默认 15ms（~60fps 上限）；SKYENGINE_KVM_PUMP_MS 可调
///（gjxjwql 过场时序依赖实证：预算不足时异步等待饥饿死循环，提速后
/// 自愈——8ms 泵速配大预算明显改善卡顿）。
pub(super) fn kvm_frame_min_interval() -> std::time::Duration {
    std::time::Duration::from_millis(
        std::env::var("SKYENGINE_KVM_PUMP_MS")
            .ok()
            .and_then(|text| text.parse().ok())
            .unwrap_or(15),
    )
}

/// 挂在 ExtRuntime 上的 KVM 运行时宿主。
pub(crate) struct KvmRuntime {
    state: KvmState,
    /// skyengine-kvm 门面：INIT 成功时装配，START 拉起，重入 INIT 时
    /// 先 shutdown 再重建。
    vm: Option<Vm>,
    /// INIT 时从包内 mainclass 文件解析出的入口类名，START 消费。
    mainclass: Option<String>,
    /// platEx(0x70003) 成功路径缓存的 input 记录 guest 地址；INIT 回读
    /// 内联字符串区（input+8 起）用。
    pending_input: Option<u32>,
    /// 当前 MRP 的宿主路径（mr/host.rs 构造 ExtRuntime 时注入）：guest
    /// 内联包名是相对名（如 "gjxjwql.mrp"），与当前包文件名一致时解析
    /// 到此宿主路径——KVM 跑的就是当前 MRP 内嵌的 jar。
    package_path: Option<PathBuf>,
    /// SKYENGINE_TRACE_KVM：KVM_INIT/KVM_START 状态迁移日志。
    trace: bool,
    /// present 帧桥（INIT 装配时与 EngineHostContext 共享；pump 取帧）。
    frame_bridge: Option<Arc<Mutex<KvmFrameBridge>>>,
    /// INIT 时刻的屏幕尺寸快照（T045 错误画面栅格化的画布尺寸）。
    screen: (u16, u16),
    /// T045 错误画面已绘标记（一次性防重：fatal 停摆后每帧泵都会看到
    /// Dead + last_fatal，须只绘一帧）。
    fatal_screen_shown: bool,
}

// ExtRuntime 派生 Debug；Vm 内部不可打印，这里只呈现状态与装配位。
impl std::fmt::Debug for KvmRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KvmRuntime")
            .field("state", &self.state)
            .field("vm_attached", &self.vm.is_some())
            .finish()
    }
}

/// 引擎侧 HostContext（T025/T026 装配，T027 完整化：present 帧桥/
/// 合成字体/RMS 落盘；audio/http 维持占位待 US7/US8）。
///
/// 生命周期：INIT 时构造（持有包路径与屏幕尺寸快照），读包内 mainclass
/// 与 jar 后随 `Vm::new` 移交 KVM 侧；Vm 运行中的 `read_suite_file` 动态
/// 读取走同一份惰性打开的 `Package` 缓存（不重复解析 MRP）。
pub(super) struct EngineHostContext {
    /// 屏幕逻辑尺寸快照（取自 ExtRuntime.display_width/height 的真值）。
    screen: (u16, u16),
    /// 单调时钟原点：与 ext.rs ips 统计（std::time::Instant 先例）一致，
    /// 自持起点——移交 Vm 后无法回访 ExtRuntime 的 clock_origin。
    origin: std::time::Instant,
    /// 宿主侧 MRP 包路径（guest 包路径按相对宿主路径解释；根目录映射
    /// 待 T027 与引擎文件系统布局对齐）。
    package_path: PathBuf,
    /// 惰性打开的 MRP 包（条目解析复用 crate::package，唯一底层）。
    package: Option<Package>,
    /// present 首帧观测（SKYENGINE_TRACE_KVM 下打印一次尺寸后丢弃）。
    presented: bool,
    /// 斯凯 gb16.uc2 点阵字体（每 BMP 码位 16x16 字形于 `codepoint*32`，
    /// 每行 2 字节 MSB-first；ASCII 宽 8、全角宽 16——格式与 mr/host.rs
    /// draw_text 一致）。构造时按 MRP work-dir 布局探测加载；缺失时
    /// 回退 T027 内建合成字形。
    gb16: Option<Vec<u8>>,
    /// present 帧尺寸与屏幕不一致的 one-shot trace（letterbox 生效提示）。
    letterbox_traced: bool,
    /// present 帧桥：与 KvmRuntime 共享（移交 Vm 后仍可被 pump 侧取帧）。
    bridge: Arc<Mutex<KvmFrameBridge>>,
    /// RMS 持久化目录（`rms/<store>.rec`；取包文件同级的 `rms/` 子目录，
    /// 对齐 mr/host.rs 把 guest 数据落在工作目录的惯例——ExtRuntime 构
    /// 造无 work_dir 参数，包路径所在目录即引擎侧的工作目录）。
    rms_dir: PathBuf,
    trace: bool,
}

/// 探测加载斯凯 gb16.uc2 点阵字体。候选路径：SKYENGINE_KVM_FONT 显式
/// 指定 → 包同级 `mythroad/system/gb16.uc2`（MRP work-dir 布局，GUI 与
/// E2E fixture 目录均命中）→ cwd 布局。不足 65536×32 字节（平铺索引
/// 下限，runtime.rs GB16_BYTES 同口径）视为坏字体忽略。
fn load_gb16_font(package_path: &std::path::Path) -> Option<Vec<u8>> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(explicit) = std::env::var_os("SKYENGINE_KVM_FONT") {
        candidates.push(PathBuf::from(explicit));
    }
    if let Some(dir) = package_path.parent() {
        candidates.push(dir.join("mythroad/system/gb16.uc2"));
    }
    candidates.push(PathBuf::from("mythroad/system/gb16.uc2"));
    for path in candidates {
        if let Ok(bytes) = std::fs::read(&path) {
            if bytes.len() >= 65_536 * 32 {
                return Some(bytes);
            }
        }
    }
    None
}

/// gb16.uc2 位图文本渲染：与 mr/host.rs `draw_text` 同格式（每码位 32
/// 字节、每行 2 字节 MSB-first、ASCII 宽 8/全角 16），写入 RGBA i32
/// 目标并逐像素生效 clip；(x,y) 为基线参考点（kvm 侧已做锚点修正）。
fn render_gb16_text(
    font: &[u8],
    text_utf16: &[u16],
    color: u32,
    x: i32,
    y: i32,
    target: &mut [i32],
    target_w: u16,
    clip: (i32, i32, i32, i32),
) {
    let solid = (color | 0xFF00_0000) as i32;
    let (clip_x1, clip_y1, clip_x2, clip_y2) = clip;
    let target_w = i32::from(target_w.max(1)) as usize;
    let target_h = target.len() / target_w.max(1);
    let top = y - 14; // 基线：16 行字形，下 2 行为 descender。
    let mut cursor = x;
    for &unit in text_utf16 {
        let width: i32 = if unit < 128 { 8 } else { 16 };
        let start = unit as usize * 32;
        if let Some(glyph) = font.get(start..start + 32) {
            for row in 0..16_i32 {
                let bits = u16::from_be_bytes([
                    glyph[row as usize * 2],
                    glyph[row as usize * 2 + 1],
                ]);
                let py = top + row;
                if py < clip_y1 || py >= clip_y2 || py < 0 || py as usize >= target_h {
                    continue;
                }
                for column in 0..width {
                    if bits & (0x8000_u16 >> column) == 0 {
                        continue;
                    }
                    let px = cursor + column;
                    if px < clip_x1 || px >= clip_x2 || px < 0 {
                        continue;
                    }
                    let px = px as usize;
                    if px < target_w {
                        target[py as usize * target_w + px] = solid;
                    }
                }
            }
        }
        cursor += width;
    }
}

impl EngineHostContext {
    pub(super) fn new(screen: (u16, u16), package_path: PathBuf, trace: bool) -> Self {
        // RMS 目录 = 包文件同级 `rms/`（惰性创建；与 mr/host.rs 的
        // work_dir 相对路径解释一致——fixture 包在宿主路径上）。
        let rms_dir = package_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("rms");
        let gb16 = load_gb16_font(&package_path);
        Self {
            screen,
            origin: std::time::Instant::now(),
            package_path,
            package: None,
            presented: false,
            letterbox_traced: false,
            bridge: Arc::new(Mutex::new(KvmFrameBridge::default())),
            rms_dir,
            gb16,
            trace,
        }
    }

    /// 惰性打开并缓存 MRP 包；文件不存在/坏包返回 None（INIT 转失败）。
    fn open_package(&mut self) -> Option<&Package> {
        if self.package.is_none() {
            let limits = ResourceLimits::default();
            self.package = Package::open(&self.package_path, limits).ok();
        }
        self.package.as_ref()
    }

    /// 店名 → `rms/<store>.rec` 路径。店名做文件名净化：非
    /// `[A-Za-z0-9._-]` 字符替换 `_`（防路径穿越；MIDP 合法店名本身
    /// 落在安全字符集内，正常名字不受影响）。
    fn rms_store_path(&self, store: &str) -> PathBuf {
        let sanitized: String = store
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.rms_dir.join(format!("{sanitized}.rec"))
    }
}

// ---------------------------------------------------------------------------
// T027 内建合成字体（TODO uc2：真 uc2 点阵字体加载是 polish 项，替换本节）
//
// ASCII 0x20-0x7E 用经典公共域 5x7 点阵（每字符 5 列字节、bit0 = 顶行，
// 即 Adafruit GFX glcdfont 同族数据），按档位整数放大；CJK/非 ASCII 以
// 全角空心块占位。画质只求可辨识。
// ---------------------------------------------------------------------------

/// 小号档句柄（kvm font.rs 已归一化 size：0=小 1=中 2=大）。
const FONT_SMALL: u32 = 1;
/// 中号档句柄（默认字体，未识别句柄亦兜底到本档）。
const FONT_MEDIUM: u32 = 2;
/// 大号档句柄。
const FONT_LARGE: u32 = 3;

/// 单档字体的恒定度量与合成参数。
struct FontTier {
    /// 点阵放大倍数（1/2/2）。
    scale: u16,
    /// 伪粗体（右邻列重绘 + 间距 1px）。
    bold: bool,
    /// 字形顶相对 ascent 线的下移量（7 行点阵在行高中的内衬）。
    pad_top: i32,
    metrics: FontMetrics,
}

impl FontTier {
    /// 单字符 advance：ASCII 半角 6*scale（5 列 + 1 间距）；非 ASCII
    /// 全角翻倍（空心块宽 = advance - scale）。
    fn advance(&self, unit: u16) -> u16 {
        let half = 6_u16.saturating_mul(self.scale);
        if unit <= 0x7E {
            half
        } else {
            half.saturating_mul(2)
        }
    }
}

/// 句柄 → 档位参数（未识别句柄兜底中号，防御 kvm 侧传旧句柄）。
fn font_tier(FontHandle(handle): FontHandle) -> FontTier {
    let (scale, bold, pad_top, height, ascent, descent) = match handle {
        FONT_SMALL => (1, false, 2, 12, 9, 3),
        FONT_LARGE => (2, true, 3, 20, 16, 4),
        _ => (2, false, 1, 16, 13, 3),
    };
    FontTier {
        scale,
        bold,
        pad_top,
        metrics: FontMetrics {
            height,
            ascent,
            descent,
            leading: 0,
        },
    }
}

/// 取 ASCII 点阵（未知码位返回实心框，防 panic/防静默吞字）。
fn ascii_glyph(byte: u8) -> [u8; 5] {
    match byte {
        0x20..=0x7E => FONT_5X7[usize::from(byte - 0x20)],
        _ => [0x7E, 0x7E, 0x7E, 0x7E, 0x7E],
    }
}

/// 5x7 点阵文本栅格化到 RGB565 帧（T045 错误画面用）：白色像素
/// （0xFFFF 小端 u16）、黑底由调用方清好；列字节 bit0 = 顶行，与
/// FONT_5X7 约定一致。越界像素静默丢弃（屏小字长截断的兜底）。
fn blit_text_rgb565(
    frame: &mut [u8],
    frame_w: usize,
    frame_h: usize,
    text: &str,
    x0: usize,
    y0: usize,
    scale: usize,
) {
    let mut pen_x = x0;
    for byte in text.bytes() {
        let glyph = ascii_glyph(byte);
        for (column, column_bits) in glyph.iter().enumerate() {
            for row in 0..7_usize {
                if column_bits & (1 << row) == 0 {
                    continue;
                }
                for dy in 0..scale {
                    for dx in 0..scale {
                        let x = pen_x + column * scale + dx;
                        let y = y0 + row * scale + dy;
                        if x < frame_w && y < frame_h {
                            let index = (y * frame_w + x) * 2;
                            if let Some(slot) = frame.get_mut(index..index + 2) {
                                slot.copy_from_slice(&0xFFFF_u16.to_le_bytes());
                            }
                        }
                    }
                }
            }
        }
        pen_x += 6 * scale; // 5 列点阵 + 1 列字间隔
    }
}

/// 单像素写入（带 clip 矩形裁剪与缓冲越界防护）。
fn blit_pixel(
    target: &mut [i32],
    target_w: i32,
    x: i32,
    y: i32,
    color: i32,
    clip: (i32, i32, i32, i32),
) {
    let (clip_x1, clip_y1, clip_x2, clip_y2) = clip;
    if x < clip_x1 || y < clip_y1 || x >= clip_x2 || y >= clip_y2 {
        return;
    }
    if x < 0 || y < 0 {
        return;
    }
    let index = (y * target_w + x) as usize;
    if let Some(slot) = target.get_mut(index) {
        *slot = color;
    }
}

/// 点阵单元的 scale×scale 像素块写入（伪粗体再加右邻列）。
// 点阵写入原语参数组（目标/步长/坐标/缩放/粗体列标志）为栅格化域约束。
#[allow(clippy::too_many_arguments)]
fn blit_glyph_pixel(
    target: &mut [i32],
    target_w: i32,
    x: i32,
    y: i32,
    scale: i32,
    bold: bool,
    color: i32,
    clip: (i32, i32, i32, i32),
) {
    for dy in 0..scale {
        for dx in 0..scale {
            blit_pixel(target, target_w, x + dx, y + dy, color, clip);
        }
        if bold {
            blit_pixel(target, target_w, x + scale, y + dy, color, clip);
        }
    }
}

/// ASCII 0x20-0x7E 的 5x7 点阵（95 个码位；每字符 5 列、列字节
/// bit0 = 顶行；经典公共域字模，glcdfont 同族）。
const FONT_5X7: [[u8; 5]; 95] = [
    [0x00, 0x00, 0x00, 0x00, 0x00], // ' '
    [0x00, 0x00, 0x5F, 0x00, 0x00], // '!'
    [0x00, 0x07, 0x00, 0x07, 0x00], // '"'
    [0x14, 0x7F, 0x14, 0x7F, 0x14], // '#'
    [0x24, 0x2A, 0x7F, 0x2A, 0x12], // '$'
    [0x23, 0x13, 0x08, 0x64, 0x62], // '%'
    [0x36, 0x49, 0x55, 0x22, 0x50], // '&'
    [0x00, 0x05, 0x03, 0x00, 0x00], // '''
    [0x00, 0x1C, 0x22, 0x41, 0x00], // '('
    [0x00, 0x41, 0x22, 0x1C, 0x00], // ')'
    [0x14, 0x08, 0x3E, 0x08, 0x14], // '*'
    [0x08, 0x08, 0x3E, 0x08, 0x08], // '+'
    [0x00, 0x50, 0x30, 0x00, 0x00], // ','
    [0x08, 0x08, 0x08, 0x08, 0x08], // '-'
    [0x00, 0x60, 0x60, 0x00, 0x00], // '.'
    [0x20, 0x10, 0x08, 0x04, 0x02], // '/'
    [0x3E, 0x51, 0x49, 0x45, 0x3E], // '0'
    [0x00, 0x42, 0x7F, 0x40, 0x00], // '1'
    [0x42, 0x61, 0x51, 0x49, 0x46], // '2'
    [0x21, 0x41, 0x45, 0x4B, 0x31], // '3'
    [0x18, 0x14, 0x12, 0x7F, 0x10], // '4'
    [0x27, 0x45, 0x45, 0x45, 0x39], // '5'
    [0x3C, 0x4A, 0x49, 0x49, 0x30], // '6'
    [0x01, 0x71, 0x09, 0x05, 0x03], // '7'
    [0x36, 0x49, 0x49, 0x49, 0x36], // '8'
    [0x06, 0x49, 0x49, 0x29, 0x1E], // '9'
    [0x00, 0x36, 0x36, 0x00, 0x00], // ':'
    [0x00, 0x56, 0x36, 0x00, 0x00], // ';'
    [0x00, 0x08, 0x14, 0x22, 0x41], // '<'
    [0x14, 0x14, 0x14, 0x14, 0x14], // '='
    [0x41, 0x22, 0x14, 0x08, 0x00], // '>'
    [0x02, 0x01, 0x51, 0x09, 0x06], // '?'
    [0x32, 0x49, 0x79, 0x41, 0x3E], // '@'
    [0x7E, 0x11, 0x11, 0x11, 0x7E], // 'A'
    [0x7F, 0x49, 0x49, 0x49, 0x36], // 'B'
    [0x3E, 0x41, 0x41, 0x41, 0x22], // 'C'
    [0x7F, 0x41, 0x41, 0x22, 0x1C], // 'D'
    [0x7F, 0x49, 0x49, 0x49, 0x41], // 'E'
    [0x7F, 0x09, 0x09, 0x01, 0x01], // 'F'
    [0x3E, 0x41, 0x41, 0x51, 0x32], // 'G'
    [0x7F, 0x08, 0x08, 0x08, 0x7F], // 'H'
    [0x00, 0x41, 0x7F, 0x41, 0x00], // 'I'
    [0x20, 0x40, 0x41, 0x3F, 0x01], // 'J'
    [0x7F, 0x08, 0x14, 0x22, 0x41], // 'K'
    [0x7F, 0x40, 0x40, 0x40, 0x40], // 'L'
    [0x7F, 0x02, 0x04, 0x02, 0x7F], // 'M'
    [0x7F, 0x04, 0x08, 0x10, 0x7F], // 'N'
    [0x3E, 0x41, 0x41, 0x41, 0x3E], // 'O'
    [0x7F, 0x09, 0x09, 0x09, 0x06], // 'P'
    [0x3E, 0x41, 0x51, 0x32, 0x44], // 'Q'
    [0x7F, 0x09, 0x10, 0x10, 0x26], // 'R'
    [0x46, 0x49, 0x49, 0x49, 0x31], // 'S'
    [0x01, 0x01, 0x7F, 0x01, 0x01], // 'T'
    [0x3F, 0x40, 0x40, 0x40, 0x3F], // 'U'
    [0x1F, 0x20, 0x40, 0x20, 0x1F], // 'V'
    [0x7F, 0x20, 0x18, 0x20, 0x7F], // 'W'
    [0x63, 0x14, 0x08, 0x14, 0x63], // 'X'
    [0x03, 0x04, 0x78, 0x04, 0x03], // 'Y'
    [0x61, 0x51, 0x49, 0x45, 0x43], // 'Z'
    [0x00, 0x00, 0x7F, 0x41, 0x41], // '['
    [0x02, 0x04, 0x08, 0x10, 0x20], // '\'
    [0x41, 0x41, 0x7F, 0x00, 0x00], // ']'
    [0x04, 0x02, 0x01, 0x02, 0x04], // '^'
    [0x40, 0x40, 0x40, 0x40, 0x40], // '_'
    [0x00, 0x01, 0x02, 0x04, 0x00], // '`'
    [0x20, 0x54, 0x54, 0x54, 0x78], // 'a'
    [0x7F, 0x48, 0x44, 0x44, 0x38], // 'b'
    [0x38, 0x44, 0x44, 0x44, 0x20], // 'c'
    [0x38, 0x44, 0x44, 0x48, 0x7F], // 'd'
    [0x38, 0x54, 0x54, 0x54, 0x18], // 'e'
    [0x08, 0x7E, 0x09, 0x01, 0x02], // 'f'
    [0x0C, 0x52, 0x52, 0x52, 0x3E], // 'g'
    [0x7F, 0x08, 0x04, 0x04, 0x78], // 'h'
    [0x00, 0x44, 0x7D, 0x40, 0x00], // 'i'
    [0x20, 0x40, 0x44, 0x3D, 0x00], // 'j'
    [0x7F, 0x10, 0x28, 0x44, 0x00], // 'k'
    [0x00, 0x41, 0x7F, 0x40, 0x00], // 'l'
    [0x7C, 0x04, 0x18, 0x04, 0x78], // 'm'
    [0x7C, 0x08, 0x04, 0x04, 0x78], // 'n'
    [0x38, 0x44, 0x44, 0x44, 0x38], // 'o'
    [0x7C, 0x14, 0x14, 0x14, 0x08], // 'p'
    [0x08, 0x14, 0x14, 0x18, 0x7C], // 'q'
    [0x7C, 0x08, 0x04, 0x04, 0x08], // 'r'
    [0x48, 0x54, 0x54, 0x54, 0x20], // 's'
    [0x04, 0x3F, 0x44, 0x40, 0x20], // 't'
    [0x3C, 0x40, 0x40, 0x20, 0x7C], // 'u'
    [0x1C, 0x20, 0x40, 0x20, 0x1C], // 'v'
    [0x3C, 0x40, 0x30, 0x40, 0x3C], // 'w'
    [0x44, 0x28, 0x10, 0x28, 0x44], // 'x'
    [0x0C, 0x50, 0x50, 0x50, 0x3C], // 'y'
    [0x44, 0x64, 0x54, 0x4C, 0x44], // 'z'
    [0x00, 0x08, 0x36, 0x41, 0x00], // '{'
    [0x00, 0x00, 0x7F, 0x00, 0x00], // '|'
    [0x00, 0x41, 0x36, 0x08, 0x00], // '}'
    [0x08, 0x04, 0x08, 0x10, 0x08], // '~'
];

impl HostContext for EngineHostContext {
    fn screen_size(&self) -> (u16, u16) {
        self.screen
    }

    fn present(&mut self, rgba: &[u8], width: u16, height: u16) {
        // T027/T028：RGBA8 整帧转 RGB565 写帧桥（引擎屏幕通道 = guest
        // screen_base + draw_bitmap，由执行循环的帧 pump 刷出，见
        // ext.rs kvm_frame_pump）。尺寸一致直接逐像素；不一致 letterbox
        // 黑边居中（超出屏的部分裁剪），并 trace 一次。
        let (screen_w, screen_h) = self.screen;
        // 调试面：SKYENGINE_KVM_DUMP_FRAME=<路径> 每帧覆盖写 P3 PPM
        //（人工核对游戏画面；ASCII 格式免依赖）。写失败静默。
        if let Some(path) = std::env::var_os("SKYENGINE_KVM_DUMP_FRAME") {
            use std::fmt::Write as _;
            let mut ppm = String::with_capacity(rgba.len() * 4);
            let _ = writeln!(ppm, "P3\n{width} {height}\n255");
            for (i, px) in rgba.chunks(4).enumerate() {
                if i > 0 && i % usize::from(width) == 0 {
                    ppm.push('\n');
                }
                let _ = write!(ppm, "{} {} {}  ", px[0], px[1], px[2]);
            }
            let _ = std::fs::write(&path, ppm);
        }
        if self.trace && !self.presented {
            self.presented = true;
            eprintln!(
                "[kvm] present {width}x{height} rgba len {} -> bridge {screen_w}x{screen_h}",
                rgba.len()
            );
        }
        if (width, height) != self.screen && !self.letterbox_traced {
            self.letterbox_traced = true;
            if self.trace {
                eprintln!(
                    "[kvm] present size {width}x{height} != screen {screen_w}x{screen_h}: letterboxed"
                );
            }
        }
        let sw = usize::from(screen_w);
        let sh = usize::from(screen_h);
        let mut frame = vec![0_u8; sw.saturating_mul(sh).saturating_mul(2)];
        // 源/目的的可见交集（居中；任一方向超出即裁掉源的外侧）。
        let copy_w = usize::from(width).min(sw);
        let copy_h = usize::from(height).min(sh);
        let dst_x = sw.saturating_sub(copy_w) / 2;
        let dst_y = sh.saturating_sub(copy_h) / 2;
        let src_x = (usize::from(width).saturating_sub(sw)) / 2;
        let src_y = (usize::from(height).saturating_sub(sh)) / 2;
        for row in 0..copy_h {
            for column in 0..copy_w {
                let src = ((row + src_y) * usize::from(width) + column + src_x) * 4;
                let [r, g, b, _a] = match rgba.get(src..src + 4) {
                    Some(bytes) => <[u8; 4]>::try_from(bytes).unwrap_or([0; 4]),
                    None => continue,
                };
                // RGBA8 → RGB565（按 MR 屏幕缓冲的小端 u16 约定）。
                let rgb565 =
                    (u16::from(r >> 3) << 11) | (u16::from(g >> 2) << 5) | u16::from(b >> 3);
                let dst = ((row + dst_y) * sw + column + dst_x) * 2;
                frame[dst..dst + 2].copy_from_slice(&rgb565.to_le_bytes());
            }
        }
        if let Ok(mut bridge) = self.bridge.lock() {
            bridge.rgb565 = frame;
            bridge.fresh = true;
        }
    }

    fn font_query(&mut self, _face: u8, _style: u8, size: u8) -> FontHandle {
        // T027 合成字体：kvm 侧已归一化 size 为 0/1/2（小/中/大，见
        // font.rs to_host_key），三档句柄 1/2/3；越界按档位取模兜底。
        // TODO（uc2 polish）：真 uc2 点阵字体加载，替换内建合成字形。
        let handle = match size % 3 {
            0 => FONT_SMALL,
            1 => FONT_MEDIUM,
            _ => FONT_LARGE,
        };
        FontHandle(handle)
    }

    fn font_metrics(&mut self, handle: FontHandle) -> FontMetrics {
        // gb16.uc2 加载成功：三档统一 16px 度量（点阵 16 行，下 2 行
        // descender）；否则回退内建合成档位。
        if self.gb16.is_some() {
            return FontMetrics {
                height: 16,
                ascent: 14,
                descent: 2,
                leading: 0,
            };
        }
        font_tier(handle).metrics
    }

    fn text_width(&mut self, handle: FontHandle, text_utf16: &[u16]) -> u16 {
        if self.gb16.is_some() {
            // gb16 步进：ASCII 8 / 全角 16（与 mr/host.rs draw_text 一致）。
            let mut width = 0_u16;
            for &unit in text_utf16 {
                width = width.saturating_add(if unit < 128 { 8 } else { 16 });
            }
            return width;
        }
        let tier = font_tier(handle);
        let mut width = 0_u16;
        for &unit in text_utf16 {
            width = width.saturating_add(tier.advance(unit));
        }
        width
    }

    fn render_text(
        &mut self,
        handle: FontHandle,
        text_utf16: &[u16],
        color: u32,
        x: i32,
        y: i32,
        target: &mut [i32],
        target_w: u16,
        _clip: (i32, i32, i32, i32),
    ) {
        // gb16.uc2 真字形优先（中文点阵）；未加载时回退 T027 合成字形。
        // 调用方（kvm 侧 render_chars）已做锚点修正，(x, y) 为基线参考。
        if let Some(font) = self.gb16.as_deref() {
            render_gb16_text(font, text_utf16, color, x, y, target, target_w, _clip);
            return;
        }
        let tier = font_tier(handle);
        let solid = (color | 0xFF00_0000) as i32;
        let (clip_x1, clip_y1, clip_x2, clip_y2) = _clip;
        let target_w = i32::from(target_w.max(1));
        let mut cursor = x;
        for &unit in text_utf16 {
            let advance = i32::from(tier.advance(unit));
            // 字形顶 = 基线上方 ascent 减顶部内衬（点阵 7 行居中于行高）。
            let top = y - i32::from(tier.metrics.ascent) + tier.pad_top;
            if unit <= 0x7E {
                // ASCII：5x7 点阵放大（列字节 bit0 = 顶行）。
                let glyph = ascii_glyph(unit as u8);
                for (col_index, column) in glyph.iter().enumerate() {
                    for row in 0..7 {
                        if column & (1 << row) == 0 {
                            continue;
                        }
                        blit_glyph_pixel(
                            target,
                            target_w,
                            cursor + col_index as i32 * tier.scale as i32,
                            top + row * tier.scale as i32,
                            tier.scale as i32,
                            tier.bold,
                            solid,
                            (clip_x1, clip_y1, clip_x2, clip_y2),
                        );
                    }
                }
            } else {
                // 非 ASCII（CJK 区等）：全角空心块占位（可辨识的 TODO 字）。
                let box_w = advance - tier.scale as i32;
                let box_h = 7 * tier.scale as i32;
                for border in 0..tier.scale as i32 {
                    for column in 0..box_w {
                        for row in [border, box_h - 1 - border] {
                            blit_pixel(
                                target,
                                target_w,
                                cursor + column,
                                top + row,
                                solid,
                                (clip_x1, clip_y1, clip_x2, clip_y2),
                            );
                        }
                    }
                    for row in 0..box_h {
                        for column in [border, box_w - 1 - border] {
                            blit_pixel(
                                target,
                                target_w,
                                cursor + column,
                                top + row,
                                solid,
                                (clip_x1, clip_y1, clip_x2, clip_y2),
                            );
                        }
                    }
                }
            }
            cursor += advance;
        }
    }

    fn read_suite_file(&mut self, name: &str) -> Option<Vec<u8>> {
        let package = self.open_package()?;
        // 条目不存在与读失败对上层同义：拿不到就是 None。
        package.read_named(name.as_bytes()).ok()
    }

    fn rms_load(&mut self, store: &str) -> Option<Vec<u8>> {
        // T027：work-dir 下 `rms/<store>.rec` 落盘（目录取包文件同级
        // `rms/`，与 mr/host.rs 工作目录惯例一致）。不存在 → None。
        std::fs::read(self.rms_store_path(store)).ok()
    }

    fn rms_save(&mut self, store: &str, data: &[u8]) {
        // save 语义 = 整店覆盖写（MIDP RecordStore 持久化粒度）。
        // 目录惰性创建；写失败静默（IO 异常无上报通道，下次 load 为
        // None，与"无存储"同义）。
        if std::fs::create_dir_all(&self.rms_dir).is_err() {
            return;
        }
        let _ = std::fs::write(self.rms_store_path(store), data);
    }

    fn rms_delete(&mut self, store: &str) -> bool {
        std::fs::remove_file(self.rms_store_path(store)).is_ok()
    }

    fn rms_list(&mut self) -> Vec<String> {
        // 列出 rms/ 下全部 `.rec` 文件（去后缀即店名）。目录缺失 = 空表。
        let Ok(entries) = std::fs::read_dir(&self.rms_dir) else {
            return Vec::new();
        };
        let mut names = Vec::new();
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if let Some(store) = name.strip_suffix(".rec") {
                names.push(store.to_string());
            }
        }
        names.sort();
        names
    }

    fn now_ms(&mut self) -> u64 {
        u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(0)
    }

    fn audio_available(&self) -> bool {
        // 音频经帧桥命令队列交引擎 NativeServices（MIDI 由引擎
        // AudioPlayer 的 rustysynth 通道合成）——通道恒可用。
        true
    }

    fn audio_submit(&mut self, _samples: &[i16], _sample_rate: u32) -> bool {
        // PCM 直提交通道不使用（编码数据走 audio_play 桥）。
        false
    }

    fn audio_play(&mut self, data: &[u8], mime: &str, loop_forever: bool) -> bool {
        if let Ok(mut bridge) = self.bridge.lock() {
            bridge.audio.push_back(KvmAudioCmd::Play {
                data: data.to_vec(),
                mime: mime.to_string(),
                loop_forever,
            });
            return true;
        }
        false
    }

    fn audio_stop(&mut self) {
        if let Ok(mut bridge) = self.bridge.lock() {
            bridge.audio.push_back(KvmAudioCmd::Stop);
        }
    }

    fn audio_set_loop_forever(&mut self, loop_forever: bool) {
        if let Ok(mut bridge) = self.bridge.lock() {
            bridge.audio.push_back(KvmAudioCmd::SetLoop { loop_forever });
        }
    }

    fn http_request(&mut self, _req: HttpRequest) -> Result<HttpResponse, HostNetError> {
        // T042 架构结论（保持 Unsupported，绝不 panic）：
        //
        // 引擎既有的 HTTP 通道（ext/network.rs 的 native socket 族：
        // allocate/connect_native_socket + state/receive/send 轮询）是
        // MRW guest hostcall 语义——非阻塞 socket + WAP 代理路由 + DNS
        // 映射，挂在 `ExtRuntime` 上，由 32 位 guest 主动拨号并逐次
        // hostcall 轮询驱动。而 `EngineHostContext` 在 KVM_INIT 装配时
        // 构造后即移交 `Vm`（自持 screen/origin/rms_dir 等快照，无法
        // 回访 ExtRuntime——见上方 origin 注释），二者生命周期与执行
        // 模型（同步阻塞 http_request vs guest 轮询状态机）均不兼容，
        // 没有可直接复用的宿主侧"提交请求拿响应"执行面。
        //
        // 因此引擎侧按"平台无网络能力"上报 Unsupported（kvm GCF 层
        // 映射为 ConnectionNotFoundException）；桌面/测试环境经
        // MockHostContext 的脚本化响应覆盖全部 HTTP 语义（T043）。
        // 待后续任务把 socket 池抽出为独立宿主服务（脱离 ExtRuntime
        // 的轮询语境）后再在此接真通道。
        Err(HostNetError::Unsupported)
    }
}

/// 从 guest 内存读 C 字符串（按字节到 NUL，lossy 转 String）。
///
/// MRP 生态的包路径/jar 名实测为 ASCII；逐窗推进避免一次性大读越过
/// 缓冲尾页；未终止（到上限仍无 NUL）或起始指针非法视为失败。
fn read_guest_cstring(memory: &GuestMemory, ptr: GuestAddr, max: usize) -> Result<String> {
    let mut bytes: Vec<u8> = Vec::new();
    let mut offset: usize = 0;
    while offset < max {
        let window = KVM_GUEST_STRING_WINDOW.min(max - offset);
        let Some(window_base) = ptr.0.checked_add(offset as u32) else {
            return Err(Error::Package("KVM guest string pointer overflow".into()));
        };
        let chunk = memory.read(GuestAddr(window_base), window).map_err(|error| {
            Error::Package(format!(
                "KVM guest string read at {window_base:#010x} failed: {error}"
            ))
        })?;
        if let Some(nul) = chunk.iter().position(|byte| *byte == 0) {
            bytes.extend_from_slice(&chunk[..nul]);
            return Ok(String::from_utf8_lossy(&bytes).into_owned());
        }
        bytes.extend_from_slice(&chunk);
        offset += window;
    }
    Err(Error::Package(format!(
        "KVM guest string at {:#010x} is not NUL-terminated within {max} bytes",
        ptr.0
    )))
}

/// F8 剖析导出文件序号（进程内递增，工作目录下 kvm-profile-N.txt）。
fn prof_dump_seq() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed) + 1
}

impl KvmRuntime {
    pub(super) fn new() -> Self {
        Self {
            state: KvmState::Uninitialized,
            vm: None,
            mainclass: None,
            pending_input: None,
            package_path: None,
            trace: std::env::var_os("SKYENGINE_TRACE_KVM").is_some(),
            frame_bridge: None,
            screen: (240, 320),
            fatal_screen_shown: false,
        }
    }

    /// 注入当前 MRP 的宿主路径（mr/host.rs 构造 ExtRuntime 后调用）：
    /// INIT 解析 guest 内联相对包名时的落点。
    pub(crate) fn set_package_path(&mut self, path: PathBuf) {
        self.package_path = Some(path);
    }

    /// 当前状态（测试与装配层使用）。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn state(&self) -> KvmState {
        self.state
    }

    /// 观测面：Vm 已装配且仍存活（T028 帧 pump 挂主循环的驱动条件）。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn is_running(&self) -> bool {
        self.vm.as_ref().is_some_and(|vm| vm.is_active())
    }

    /// 观测面：INIT 时 install_jar 索引到的类条目数（测试断言用）。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn jar_class_count(&self) -> Option<usize> {
        self.vm.as_ref().map(|vm| vm.jar_class_count())
    }

    /// 观测面：包内 mainclass 文件解析出的入口类名。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn mainclass(&self) -> Option<&str> {
        self.mainclass.as_deref()
    }

    /// 观测面：待消费的 KVM 事件数（键/指针路由测试断言用）。
    #[cfg(test)]
    pub(crate) fn pending_event_count(&self) -> usize {
        self.vm
            .as_ref()
            .map_or(0, |vm| vm.pending_event_count())
    }

    /// T045 观测面：fatal 停摆待收尾（Vm 挂着 + 已停摆 + 带 fatal 根因 +
    /// 错误画面未绘）。帧泵的 is_running 闸外单放一次 poll 绘错误画面。
    pub(crate) fn needs_fatal_attention(&self) -> bool {
        !self.fatal_screen_shown
            && self
                .vm
                .as_ref()
                .is_some_and(|vm| !vm.is_active() && vm.last_fatal().is_some())
    }

    /// 观测面：MIDlet 已正常退出（Vm Dead 且无 fatal 根因；Paused 态
    /// 不算——hideNotify/notifyPaused 会短暂离开 Running）。
    pub(crate) fn exited_normally(&self) -> bool {
        self.vm
            .as_ref()
            .is_some_and(|vm| vm.state() == VmState::Dead && vm.last_fatal().is_none())
    }

    /// T028 帧 pump：驱动 KVM 调度器一个时间片（poll 自带事件泵 →
    /// 线程轮转 → repaint → present 写帧桥）。非运行态无害 no-op。
    ///
    /// T045 致命停摆检测（一次性防重）：Vm 已停摆（is_active()==false）
    /// 且带 fatal 根因（未捕获异常 / 内部故障 / OOM，`Vm::last_fatal`）
    /// 时绘制错误画面写帧桥 + 状态转 Crashed。此后 is_running()==false，
    /// 键路由自动回 guest 路径（既有逻辑），引擎继续可用——后续
    /// platEx(0x70003)+INIT 可重建新 Vm。
    pub(crate) fn poll(&mut self, budget: u64) {
        if let Some(vm) = self.vm.as_mut() {
            vm.poll(budget);
            // F8 取证：消费剖析导出请求（invoke_profile_top 落盘，
            // 文件名递增；SKYENGINE_KVM_PROFILE=1 才有数据）。
            if skyengine_kvm::debug_switch::take_profile_dump_request() {
                use std::fmt::Write as _;
                let mut text = String::new();
                for (index, (name, count)) in
                    vm.classspace().invoke_profile_top(400).into_iter().enumerate()
                {
                    let _ = writeln!(text, "{index:3} {count:10} {name}");
                }
                let path = format!("kvm-profile-{}.txt", prof_dump_seq());
                match std::fs::write(&path, text) {
                    Ok(()) => eprintln!("[kvm-profile] 已导出 {path}"),
                    Err(error) => eprintln!("[kvm-profile] 导出 {path} 失败: {error}"),
                }
            }
        }
        // T045 致命停摆检测（一次性防重）：Vm 已停摆且带 fatal 根因时
        // 绘制错误画面写帧桥 + 状态转 Crashed。此后 is_running()==false，
        // 键路由自动回 guest 路径（既有逻辑），引擎继续可用——后续
        // platEx(0x70003)+INIT 可重建新 Vm。
        let fatal = self
            .vm
            .as_ref()
            .filter(|vm| !vm.is_active())
            .and_then(|vm| vm.last_fatal().map(str::to_string));
        if !self.fatal_screen_shown
            && let Some(fatal) = fatal
        {
            self.fatal_screen_shown = true;
            self.render_fatal_screen(&fatal);
            self.state = KvmState::Crashed;
        }
    }

    /// T045 错误画面：黑底白字两行（"Application Error" / 根因文本），
    /// 复用 T027 内建 5x7 点阵直接栅格化到 RGB565 帧桥（KVM 侧无字体
    /// 渲染，错误画面归引擎侧职责；帧泵随后的 take_fresh_frame 照常
    /// 取走上屏）。根因文本超行宽截断；非 ASCII 字节以实心框字形呈现
    /// （ascii_glyph 防御语义）。
    fn render_fatal_screen(&mut self, fatal: &str) {
        // 完整根因进日志（画面受行宽截断）。
        eprintln!("[kvm] fatal: {fatal}");
        let Some(bridge) = self.frame_bridge.as_ref() else {
            return;
        };
        let Ok(mut bridge) = bridge.lock() else {
            return;
        };
        let (sw, sh) = (usize::from(self.screen.0), usize::from(self.screen.1));
        // 黑底整帧重建（不必与既有 present 帧尺寸耦合）。
        bridge.rgb565.clear();
        bridge.rgb565.resize(sw.saturating_mul(sh).saturating_mul(2), 0);
        // 宽屏 2 倍缩放（240 宽下 12px/字符），窄屏 1 倍。
        let scale = if sw >= 240 { 2 } else { 1 };
        let capacity = (sw / (6 * scale)).max(4);
        let title = "Application Error";
        let cause: String = fatal.chars().take(capacity).collect();
        let line_height = 7 * scale + 6;
        let y0 = sh.saturating_sub(line_height * 2) / 2;
        blit_text_rgb565(&mut bridge.rgb565, sw, sh, title, 4, y0, scale);
        blit_text_rgb565(&mut bridge.rgb565, sw, sh, &cause, 4, y0 + line_height, scale);
        bridge.fresh = true;
    }

    /// T028 键路由：引擎键名入队（poll 段前置转投 Canvas 回调）。
    pub(crate) fn dispatch_key(&mut self, key: u8, pressed: bool) {
        if let Some(vm) = self.vm.as_mut() {
            vm.dispatch_key(key, pressed);
        }
    }

    /// T028 指针路由：坐标 + 种类（0=按下 1=抬起）入队。
    pub(crate) fn dispatch_pointer(&mut self, x: i16, y: i16, kind: u8) {
        if let Some(vm) = self.vm.as_mut() {
            vm.dispatch_pointer(x, y, kind);
        }
    }

    /// 取走 present 写入帧桥的新帧（无新帧/桥失效返回 None；取走即清
    /// fresh，避免同一帧重复上屏）。调用方（ext.rs 帧泵）负责把它刷进
    /// guest 屏幕并上屏。
    pub(crate) fn take_fresh_frame(&mut self) -> Option<Vec<u8>> {
        let arc = self.frame_bridge.as_ref()?;
        let mut bridge = arc.lock().ok()?;
        if !bridge.fresh {
            return None;
        }
        bridge.fresh = false;
        Some(bridge.rgb565.clone())
    }

    /// 取走待处理音频命令（帧泵每帧 drain，交 NativeServices 播放）。
    pub(crate) fn take_audio_cmds(&mut self) -> Vec<KvmAudioCmd> {
        let Some(arc) = self.frame_bridge.as_ref() else {
            return Vec::new();
        };
        let Ok(mut bridge) = arc.lock() else {
            return Vec::new();
        };
        bridge.audio.drain(..).collect()
    }

    /// platEx(0x70003) 成功路径调用：缓存 input 记录地址，INIT 回读
    /// KVM 对象（input+8）用。
    pub(super) fn note_registration(&mut self, input: u32) {
        self.pending_input = Some(input);
    }

    fn trace(&self, entry: &str, result: i32) {
        if self.trace {
            eprintln!("[kvm] {entry} state={:?} -> {result}", self.state);
        }
    }

    /// ExtRuntime 构造时调用：映射魔数窗口并写入 vtable。guest sub80D04
    /// 会先 ldr 读 vtable 两槽再 BLX，所以窗口必须真实可读。
    pub(super) fn install_magic_window(memory: &mut GuestMemory) -> Result<()> {
        let mut window = vec![0_u8; 0x1000];
        window[0x10..0x14].copy_from_slice(&KVM_INIT_MAGIC.to_le_bytes());
        window[0x14..0x18].copy_from_slice(&KVM_START_MAGIC.to_le_bytes());
        memory.map_bytes(
            GuestAddr(KVM_MAGIC_BASE),
            window,
            Permissions::READ_WRITE,
            "kvm magic window",
        )?;
        Ok(())
    }

    /// 执行循环的魔数拦截入口：PC 命中魔数地址时执行对应状态迁移并返回
    /// 写给 r0 的结果；其余地址返回 None，交回常规执行路径。
    ///
    /// INIT 需要回读 guest 内存（KVM 对象与字符串指针），memory 由调用
    /// 方以分离字段借用传入；screen 为 ExtRuntime 的显示尺寸真值快照。
    pub(super) fn dispatch_magic(
        &mut self,
        pc: u32,
        cpu: &ArmCpu,
        memory: &GuestMemory,
        screen: (u16, u16),
    ) -> Option<i32> {
        match pc {
            KVM_INIT_MAGIC => Some(self.kvm_init(cpu, memory, screen)),
            KVM_START_MAGIC => Some(self.kvm_start(cpu)),
            _ => None,
        }
    }

    /// KVM_INIT（vtable 槽 0）：装配 Vm。
    ///
    /// 流程：读 pending_input → guest 内存读 KVM 对象（input+8）取包路径
    /// 与 jar 名 → EngineHostContext 读包内 mainclass 与 jar 字节 →
    /// `Vm::new` + `install_jar`。任一步失败（未注册/缺包/缺 mainclass/
    /// 坏 jar/guest 内存不可读）→ Crashed + 返回 -1 + trace。重入：已有
    /// Vm 先显式 shutdown 再重建。参数非零不迁移状态（guest 契约恒 (0,0)）。
    fn kvm_init(&mut self, cpu: &ArmCpu, memory: &GuestMemory, screen: (u16, u16)) -> i32 {
        // guest 契约按 (vt_x, 0) 调用，非零参数视为非法。
        if cpu.register(0) != 0 {
            self.trace("INIT(bad args)", -1);
            return -1;
        }
        // 重入：先 shutdown 旧 Vm（释放 KVM 侧调度器与堆）再重建。
        if let Some(mut old) = self.vm.take() {
            old.shutdown();
            self.mainclass = None;
        }
        match self.assemble_vm(memory, screen) {
            Ok(mainclass) => {
                self.mainclass = Some(mainclass);
                self.state = KvmState::Initialized;
                // T045：屏幕快照 + 错误画面防重标记复位（重入新 Vm 的
                // fatal 停摆须可再次绘制）。
                self.screen = screen;
                self.fatal_screen_shown = false;
                self.trace("INIT", 0);
                0
            }
            Err(reason) => {
                self.state = KvmState::Crashed;
                self.vm = None;
                self.mainclass = None;
                self.frame_bridge = None;
                self.trace(&format!("INIT(failed: {reason})"), -1);
                -1
            }
        }
    }

    /// INIT 的装配主体：所有失败以字符串原因返回（不 panic、不传染
    /// guest 内存错误到执行循环——统一收敛为 Crashed + -1）。
    fn assemble_vm(&mut self, memory: &GuestMemory, screen: (u16, u16)) -> Result<String> {
        let input = self.pending_input.ok_or_else(|| {
            Error::Package("KVM_INIT before platEx(0x70003) registration".into())
        })?;
        // SKYENGINE_TRACE_KVM 取证（T029）：INIT 现场回读 input 记录
        // 前 0x60 字节 hex。guest 布局与 T025 逆向假设有出入时（如
        // 内联字符串被当指针解引用），dump 是定位的第一手证据。
        if self.trace {
            let mut lines = Vec::new();
            for offset in (0..0x60).step_by(16) {
                let line = match memory.read(GuestAddr(input + offset), 16) {
                    Ok(bytes) => format!("{bytes:02x?}"),
                    Err(error) => format!("<unreadable: {error}>"),
                };
                lines.push(format!("  +{offset:03x}: {line}"));
            }
            eprintln!("[kvm] INIT input record @{input:#010x}:\n{}", lines.join("\n"));
        }
        let object_base = input
            .checked_add(KVM_OBJECT_OFFSET)
            .ok_or_else(|| Error::Package("KVM object address overflow".into()))?;
        // 内联字符串区（T029 E2E 实测布局）：input+0x08 包名、input+0x48
        // jar 名，均以 NUL 结尾的 C 字符串就地存放，不是二级指针。
        let package_name =
            read_guest_cstring(memory, GuestAddr(object_base + KVM_OBJECT_PACKAGE_PATH), KVM_PACKAGE_NAME_MAX)?;
        // jar 名槽读失败/空串回退默认 "jc.jar"（MRP 生态约定）。
        let jar_name = match read_guest_cstring(
            memory,
            GuestAddr(object_base + KVM_OBJECT_JAR_NAME),
            KVM_JAR_NAME_MAX,
        ) {
            Ok(name) if !name.is_empty() => name,
            _ => KVM_DEFAULT_JAR_NAME.to_string(),
        };
        if package_name.is_empty() {
            return Err(Error::Package("KVM object has a null package path".into()));
        }
        // 包路径解析：mr 层注入了当前 MRP 就无条件复用——stub 就是当前
        // 包的壳，KVM 跑的就是当前 MRP 内嵌的 jar；guest 内联名
        // （"gjxjwql.mrp"，不带 MRP 文件名的 id/分辨率前缀）仅作 trace
        // 记录。未注入（单测缺包路径）时按 guest 名原样解释（宿主绝对
        // 路径 / 相对引擎 cwd），由打开失败收敛为 Crashed。
        let package_path = match &self.package_path {
            Some(current) => {
                if self.trace
                    && current
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy() != package_name)
                {
                    eprintln!(
                        "[kvm] INIT inline package name {package_name:?} resolves to current MRP {}",
                        current.display()
                    );
                }
                current.clone()
            }
            None => PathBuf::from(&package_name),
        };

        // 先构造宿主上下文：mainclass/jar 均经其读包通道（同一 Package
        // 实例），随后整体移交 Vm 供运行中动态读套件文件。帧桥由本运行
        // 时与上下文共享（present 写 / 帧 pump 读），在移交前克隆一份。
        let mut host = EngineHostContext::new(screen, package_path, self.trace);
        let frame_bridge = Arc::clone(&host.bridge);
        let mainclass_bytes = host
            .read_suite_file(KVM_MAINCLASS_ENTRY)
            .ok_or_else(|| Error::Package("suite has no mainclass entry".into()))?;
        let mainclass = String::from_utf8_lossy(&mainclass_bytes)
            .trim()
            .trim_end_matches('\0')
            .to_string();
        if mainclass.is_empty() {
            return Err(Error::Package("mainclass entry is empty".into()));
        }
        let jar_bytes = host
            .read_suite_file(&jar_name)
            .ok_or_else(|| Error::Package(format!("suite has no {jar_name} entry")))?;

        // 托管堆默认 16MB；SKYENGINE_KVM_HEAP 可调（MB 单位）——
        // RPG 大量图集对象下小堆频繁 mark-sweep 全堆 GC 是卡顿主源
        // 之一（gjxjwql 实测加大后流畅度明显改善）。
        let heap_mb: usize = std::env::var("SKYENGINE_KVM_HEAP")
            .ok()
            .and_then(|text| text.parse().ok())
            .unwrap_or(16);
        let mut vm = Vm::new(Box::new(host), VmConfig { heap_bytes: heap_mb << 20 });
        vm.install_jar(&jar_name, &jar_bytes)
            .map_err(|error| Error::Package(format!("install_jar({jar_name}) failed: {error}")))?;
        // 平台文件资源并入（斯凯 jar 外资源语义）：包同级 mythroad/ 下
        // 的一层小文件（渠道数据如 1.link，jar 内不存在）对
        // getResourceAsStream 可见——gjxjwql 菜单数据实证依赖此通道。
        if let Some(parent) = self.package_path.as_ref().and_then(|p| p.parent()) {
            let mut files = Vec::new();
            if let Ok(entries) = std::fs::read_dir(parent.join("mythroad")) {
                for entry in entries.flatten() {
                    let Ok(meta) = entry.metadata() else { continue };
                    // 只收一层常规文件，防御异常体量（字体/子目录跳过）。
                    if !meta.is_file() || meta.len() > 256 * 1024 {
                        continue;
                    }
                    let Ok(name) = entry.file_name().into_string() else { continue };
                    if let Ok(bytes) = std::fs::read(entry.path()) {
                        files.push((name, bytes));
                    }
                }
            }
            vm.merge_platform_resources(files);
        }
        self.vm = Some(vm);
        self.frame_bridge = Some(frame_bridge);
        Ok(mainclass)
    }

    /// KVM_START（vtable 槽 1）：仅接受 Initialized 态；`start_midlet`
    /// 链接入口类 → 构造 → startApp。失败 → Crashed + -1；未初始化/
    /// 已启动/崩溃态一律拒绝返回 -1（guest 视为启动失败，不终止进程）。
    fn kvm_start(&mut self, cpu: &ArmCpu) -> i32 {
        if cpu.register(0) != 0 {
            self.trace("START(bad args)", -1);
            return -1;
        }
        if self.state != KvmState::Initialized {
            self.trace("START(rejected)", -1);
            return -1;
        }
        let Some(mainclass) = self.mainclass.clone() else {
            self.trace("START(no mainclass)", -1);
            return -1;
        };
        let Some(vm) = self.vm.as_mut() else {
            self.trace("START(no vm)", -1);
            return -1;
        };
        match vm.start_midlet(&mainclass) {
            Ok(()) => {
                self.state = KvmState::Started;
                self.trace("START", 0);
                0
            }
            Err(error) => {
                self.state = KvmState::Crashed;
                self.trace(&format!("START(failed: {error})"), -1);
                -1
            }
        }
    }
}
