//! MockHostContext：HostContext 的纯内存实现，供测试与无宿主场景。
//!
//! 特性：present 捕获帧副本、可控时钟（advance_ms）、按 size 三档的
//! 合成字体度量、内存 RMS（BTreeMap）、脚本化 HTTP 队列、无音频。

use std::collections::{BTreeMap, VecDeque};

use super::{FontHandle, FontMetrics, HostContext, HostNetError, HttpRequest, HttpResponse};

/// 内存 Mock 宿主。
#[derive(Debug, Default)]
pub struct MockHostContext {
    /// screen_size 返回值。
    size: (u16, u16),
    /// present 捕获的帧（rgba, w, h）。
    frames: Vec<(Vec<u8>, u16, u16)>,
    /// 可控时钟（毫秒）。
    clock: u64,
    /// 简单固定字体表：handle → (size 档, metrics)。handle 从 1 递增。
    fonts: BTreeMap<u32, (u8, FontMetrics)>,
    /// 下一个字体句柄。
    next_font: u32,
    /// 内存 RMS。
    rms: BTreeMap<String, Vec<u8>>,
    /// HTTP 应答脚本队列（逐次出队）。
    http_queue: VecDeque<Result<HttpResponse, HostNetError>>,
    /// 最近一次 http_request 收到的请求。
    last_http: Option<HttpRequest>,
    /// render_text 调用记录 (handle, text, color, x, y)。
    text_renders: Vec<(FontHandle, Vec<u16>, u32, i32, i32)>,
    /// 最近一次 audio_play (mime, data, loop_forever)。
    last_audio: Option<(String, Vec<u8>, bool)>,
    /// audio_play 是否受理。
    audio_enabled: bool,
    /// audio_stop 次数。
    audio_stops: u32,
}

impl MockHostContext {
    /// 创建指定屏幕尺寸的 Mock。
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            size: (width, height),
            next_font: 1,
            ..Default::default()
        }
    }

    /// 推进时钟 n 毫秒。
    pub fn advance_ms(&mut self, n: u64) {
        self.clock += n;
    }

    /// 已捕获的帧列表 (rgba, w, h)。
    pub fn captured_frames(&self) -> &[(Vec<u8>, u16, u16)] {
        &self.frames
    }

    /// 最近一次 HTTP 请求。
    pub fn last_http(&self) -> Option<&HttpRequest> {
        self.last_http.as_ref()
    }

    /// 最近一次 audio_play (mime, data, loop_forever)。
    pub fn last_audio(&self) -> Option<&(String, Vec<u8>, bool)> {
        self.last_audio.as_ref()
    }

    /// audio_stop 调用次数。
    pub fn audio_stops(&self) -> u32 {
        self.audio_stops
    }

    /// render_text 调用记录 (handle, text, color, x, y)。
    pub fn text_renders(&self) -> &[(FontHandle, Vec<u16>, u32, i32, i32)] {
        &self.text_renders
    }

    /// 追加 HTTP 应答脚本（按出队顺序消费）。
    pub fn push_http(&mut self, resp: Result<HttpResponse, HostNetError>) {
        self.http_queue.push_back(resp);
    }

    /// 按 MIDP size 档位合成度量：height=size+4, ascent=size, descent=4, leading=2。
    fn synth_metrics(size: u8) -> FontMetrics {
        let s = size.min(2); // 值域宽松：越界按最大档兜底
        let px: u16 = match s {
            0 => 12,
            1 => 16,
            _ => 20,
        };
        FontMetrics {
            height: px + 4,
            ascent: px,
            descent: 4,
            leading: 2,
        }
    }
}

impl HostContext for MockHostContext {
    fn screen_size(&self) -> (u16, u16) {
        self.size
    }

    fn present(&mut self, rgba: &[u8], width: u16, height: u16) {
        self.frames.push((rgba.to_vec(), width, height));
    }

    fn font_query(&mut self, _face: u8, _style: u8, size: u8) -> FontHandle {
        let m = Self::synth_metrics(size);
        let handle = self.next_font;
        self.next_font += 1;
        self.fonts.insert(handle, (size.min(2), m));
        FontHandle(handle)
    }

    fn font_metrics(&mut self, handle: FontHandle) -> FontMetrics {
        // 无效句柄兜底为最小档度量（宽松策略）。
        self.fonts
            .get(&handle.0)
            .map(|(_, m)| *m)
            .unwrap_or_else(|| Self::synth_metrics(0))
    }

    fn text_width(&mut self, handle: FontHandle, text_utf16: &[u16]) -> u16 {
        let size = self
            .fonts
            .get(&handle.0)
            .map(|(s, _)| *s)
            .unwrap_or(0);
        let px: u16 = match size.min(2) {
            0 => 12,
            1 => 16,
            _ => 20,
        };
        // 合成规则：码元数 × (size/2)。
        (text_utf16.len() as u64 * (px / 2) as u64).min(u16::MAX as u64) as u16
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
        clip: (i32, i32, i32, i32),
    ) {
        // 记录调用供测试断言。
        self.text_renders
            .push((handle, text_utf16.to_vec(), color, x, y));

        // 合成"字形"：不透明单色矩形，宽 = 文本宽度，高 = 字体 height。
        let metrics = self.font_metrics(handle);
        let w = self.text_width(handle, text_utf16) as i32;
        let h = metrics.height as i32;
        // 屏幕坐标矩形 (x, y, x+w, y+h) 与 clip 相交后逐行填充。
        let (cx1, cy1, cx2, cy2) = clip;
        let x1 = x.max(cx1);
        let y1 = y.max(cy1);
        let x2 = (x + w).min(cx2);
        let y2 = (y + h).min(cy2);
        let tw = target_w as i32;
        let th = (target.len() / target_w.max(1) as usize) as i32;
        if x1 >= x2 || y1 >= y2 {
            return; // 完全裁掉
        }
        let px = (color | 0xFF00_0000) as i32; // 不透明（u32/i32 位模式等价）
        let row = |target: &mut [i32], yy: i32| {
            for xx in x1..x2 {
                if xx >= 0 && xx < tw {
                    let idx = (yy as usize) * (tw as usize) + (xx as usize);
                    if idx < target.len() {
                        target[idx] = px;
                    }
                }
            }
        };
        for yy in y1..y2 {
            if yy >= 0 && yy < th {
                row(target, yy);
            }
        }
    }

    fn read_suite_file(&mut self, _name: &str) -> Option<Vec<u8>> {
        // Mock 不挂 MRP 包：一律未找到。
        None
    }

    fn rms_load(&mut self, store: &str) -> Option<Vec<u8>> {
        self.rms.get(store).cloned()
    }

    fn rms_save(&mut self, store: &str, data: &[u8]) {
        self.rms.insert(store.to_string(), data.to_vec());
    }

    fn rms_delete(&mut self, store: &str) -> bool {
        self.rms.remove(store).is_some()
    }

    fn rms_list(&mut self) -> Vec<String> {
        self.rms.keys().cloned().collect()
    }

    fn now_ms(&mut self) -> u64 {
        self.clock
    }

    fn audio_available(&self) -> bool {
        false
    }

    fn audio_submit(&mut self, _samples: &[i16], _sample_rate: u32) -> bool {
        // 无音频设备：返回 false 且丢弃样本。
        false
    }

    fn audio_play(&mut self, data: &[u8], mime: &str, loop_forever: bool) -> bool {
        // 记录观测面（测试断言 Player→host 音频通道）；受理与否由
        // audio_enabled 控制，默认 true（状态机推进语义不依赖受理结果）。
        self.last_audio = Some((mime.to_string(), data.to_vec(), loop_forever));
        self.audio_enabled
    }

    fn audio_stop(&mut self) {
        self.audio_stops += 1;
    }

    fn http_request(&mut self, req: HttpRequest) -> Result<HttpResponse, HostNetError> {
        self.last_http = Some(req);
        match self.http_queue.pop_front() {
            Some(r) => r,
            // 脚本耗尽：Unsupported。
            None => Err(HostNetError::Unsupported),
        }
    }
}
