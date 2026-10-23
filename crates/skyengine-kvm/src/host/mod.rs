//! HostContext：kvm → 宿主引擎的唯一服务通道（plan.md 决策 D6/D7/D8）。
//!
//! J2ME 层（LCDUI/media/io/RMS）不直接触碰平台 API，全部经由本 trait
//! 获取屏幕提交、字体光栅化、包文件读取、RMS 持久化、时钟、音频与
//! 网络能力。引擎侧（HarmonyOS / 桌面模拟器 / 测试）提供各自实现；
//! `mock::MockHostContext` 为内存实现，供测试与无宿主场景使用。

pub mod mock;

/// 字体句柄：`font_query` 返回，供度量/绘字接口引用。
/// 非零；0 保留为"无效句柄"哨兵。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FontHandle(pub u32);

/// 字体度量（像素）。MIDP Font.getHeight/getBaselinePosition 等映射于此。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FontMetrics {
    /// 总行高。
    pub height: u16,
    /// 基线以上高度（ascent）。
    pub ascent: u16,
    /// 基线以下深度（descent）。
    pub descent: u16,
    /// 行间-leading。
    pub leading: u16,
}

/// HTTP 请求描述（Generic Connection Framework HttpConnection 映射）。
#[derive(Debug, Clone)]
pub struct HttpRequest {
    /// 方法名（"GET"/"POST"/...，按 MIDP 传入原样）。
    pub method: String,
    /// 完整 URL。
    pub url: String,
    /// 请求头（名值对，保持插入顺序）。
    pub headers: Vec<(String, String)>,
    /// 请求体（GET/HEAD 通常为 None）。
    pub body: Option<Vec<u8>>,
    /// 超时毫秒数。
    pub timeout_ms: u64,
}

/// HTTP 响应。
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// 状态码（200/404/...）。
    pub status: u16,
    /// 响应头。
    pub headers: Vec<(String, String)>,
    /// 响应体。
    pub body: Vec<u8>,
}

/// 网络错误：后续 io 层映射为 ConnectionNotFoundException / IOException。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostNetError {
    /// 平台无网络能力或该 URL 不受支持。
    #[error("network unsupported")]
    Unsupported,
    /// 请求超时。
    #[error("timeout")]
    Timeout,
    /// 传输/IO 故障。
    #[error("io error: {0}")]
    Io(String),
}

/// kvm → 引擎的服务抽象。
///
/// 所有方法均要求可变借用（`&mut self`）：宿主实现内部通常带有
/// 自身状态（帧队列、音频缓冲等），且 kvm 侧为单线程轮转调度，
/// 可变借用即够用，无需内部同步。
pub trait HostContext {
    /// 当前屏幕逻辑尺寸（宽, 高），像素。
    fn screen_size(&self) -> (u16, u16);

    /// 整帧提交（RGBA8，自上而下逐行）。`rgba.len() == w*h*4` 由调用方保证。
    fn present(&mut self, rgba: &[u8], width: u16, height: u16);

    /// 按 MIDP Font 三元组查询/创建字体，返回非零句柄。
    /// face 0-2 / style 0-16 / size 0-2，值域宽松不强校验（越界值按实现自定策略兜底）。
    fn font_query(&mut self, face: u8, style: u8, size: u8) -> FontHandle;

    /// 查询字体度量。
    fn font_metrics(&mut self, handle: FontHandle) -> FontMetrics;

    /// 计算 UTF-16 文本的显示宽度（像素）。
    fn text_width(&mut self, handle: FontHandle, text_utf16: &[u16]) -> u16;

    /// 将文本以指定颜色 blit 进目标缓冲 `target`（行宽 `target_w`）。
    ///
    /// `target` 为堆上 `[I` 数组视图（位模式 0xAARRGGBB 以 i32 传递，
    /// u32/i32 位模式等价，T020 Graphics 直通无转换）。
    /// (x, y) 为基线左上角参考点；`clip` 为 (x1, y1, x2, y2) 剪裁矩形。
    /// 越界与裁剪均由宿主实现处理，调用方（T020 Graphics）不做预剪。
    #[allow(clippy::too_many_arguments)]
    fn render_text(
        &mut self,
        handle: FontHandle,
        text_utf16: &[u16],
        color: u32,
        x: i32,
        y: i32,
        target: &mut [i32],
        target_w: u16,
        clip: (i32, i32, i32, i32),
    );

    /// 读取 MRP 包内文件（如 mainclass 配置等），返回原始字节。
    /// jar 内资源不走此通道。
    fn read_suite_file(&mut self, name: &str) -> Option<Vec<u8>>;

    /// RMS：读取记录存储；不存在返回 None。
    fn rms_load(&mut self, store: &str) -> Option<Vec<u8>>;
    /// RMS：写入（存在则覆盖）。
    fn rms_save(&mut self, store: &str, data: &[u8]);
    /// RMS：删除；返回是否确实删除了存在的存储。
    fn rms_delete(&mut self, store: &str) -> bool;
    /// RMS：列出现有存储名。
    fn rms_list(&mut self) -> Vec<String>;

    /// 单调时钟（毫秒）。
    fn now_ms(&mut self) -> u64;

    /// 是否有可用音频输出设备。
    fn audio_available(&self) -> bool;
    /// 提交一段 PCM（交织立体声时按实现约定）。无设备时返回 false 且丢弃样本。
    fn audio_submit(&mut self, samples: &[i16], sample_rate: u32) -> bool;

    /// 启动音频播放（编码数据直交宿主解码合成——MIDI/WAV/MP3 按宿主
    /// 引擎能力；mime 如 "audio/midi"）。`loop_forever` 循环播放。
    /// 返回是否受理（无设备/不支持的格式 false，Player 状态机照常）。
    /// 冒烟实证：jc.jar 的 BGM 为 /music.pkg 内 MIDI，经此通道交引擎。
    fn audio_play(&mut self, data: &[u8], mime: &str, loop_forever: bool) -> bool;
    /// 停止 audio_play 启动的播放。
    fn audio_stop(&mut self);
    /// 运行中改循环标志（STARTED 态 setLoopCount(-1) 同步——gjxjwql
    /// BGM 时序：音频命令仅在 start 提交一次，后设循环需此通道）。
    fn audio_set_loop_forever(&mut self, _loop_forever: bool) {}

    /// 执行 HTTP 请求（阻塞语义；kvm 侧在 Io 阻塞状态下轮询驱动）。
    fn http_request(&mut self, req: HttpRequest) -> Result<HttpResponse, HostNetError>;
}
