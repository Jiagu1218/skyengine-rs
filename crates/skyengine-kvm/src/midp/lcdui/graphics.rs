//! `javax/microedition/lcdui/Graphics` 光栅化（T020）。
//!
//! 对象模型（BootstrapDecl 实例字段，与 Image.pixels 同构）：
//! - `target [I`：目标像素数组引用（0xAARRGGBB 行优先）
//! - `target_w I` / `target_h I`：目标尺寸
//! - `clip_x1/y1/x2/y2 I`：规范合并后的裁剪矩形（设备坐标，半开区间）
//! - `trans_x/trans_y I`：translate 累积
//! - `color I`：当前颜色（24 位 RGB；setGrayScale 后为灰度三通道复制）
//! - `font LFont;`：当前字体（初始默认字体；setFont(null) 恢复默认）
//! - `stroke I`：线型（SOLID/DOTTED，存储无视觉差异——Canvas 全 FILL 语义）
//!
//! 构造唯一路径：`Image.getGraphics()`（仅可变图，绑定 Image 像素数组；
//! T021 Canvas 屏缓冲同型复用 `alloc_graphics`）。
//!
//! MIDP-2.0 语义要点（经典坑，按 javadoc 落实）：
//! - drawRect 画 w+1×h+1（边框落 x..x+w）；fillRect 填 x..x+w-1（w×h）
//! - clipRect 与当前裁剪求交（坐标经 translate 解释）；getClip* 返回设备坐标
//! - translate 累积；drawLine 两端点都画
//! - 角度：0 度 = 3 点钟方向、顺时针增大（屏幕 y 向下坐标系自然方向）
//! - drawString 经 `host.render_text` blit（宿主处理字形与裁剪）
//!
//! 本模块绘制原语签名镜像 MIDP Graphics.draw* API（x/y/w/h/arcW/arcH…），
//! 参数宽度是域约束，整体豁免 too_many_arguments。
#![allow(clippy::too_many_arguments)]

use crate::cldc::lang as jl;
use crate::host::HostContext;
use crate::interp::NativeCall;
use crate::runtime::classspace::ClassSpace;
use crate::runtime::heap::Heap;
use crate::runtime::object::ArrayData;
use crate::runtime::{ObjRef, Slot};
use crate::KvmError;

use super::font;
use super::image;

/// Graphics 内部名。
const CLASS: &str = "javax/microedition/lcdui/Graphics";

// ---- MIDP 锚点/线型常量（与 DECL 静态常量一致） ---------------------------

const HCENTER: i32 = 1;
const VCENTER: i32 = 2;
const LEFT: i32 = 4;
const RIGHT: i32 = 8;
const TOP: i32 = 16;
const BOTTOM: i32 = 32;
const BASELINE: i32 = 64;
const SOLID: i32 = 0;
/// 不透明位（0xFF000000 以 i32 位模式传递）。
const OPAQUE: i32 = 0xFF00_0000u32 as i32;
const DOTTED: i32 = 1;

/// 水平锚点掩码。
const H_MASK: i32 = LEFT | RIGHT | HCENTER;
/// 垂直锚点掩码。
const V_MASK: i32 = TOP | BOTTOM | VCENTER | BASELINE;

/// Graphics 实例字段槽位（每次调用现查；与 image.rs 同模式）。
struct GraphicsSlots {
    target: usize,
    target_w: usize,
    target_h: usize,
    clip_x1: usize,
    clip_y1: usize,
    clip_x2: usize,
    clip_y2: usize,
    trans_x: usize,
    trans_y: usize,
    color: usize,
    font: usize,
    stroke: usize,
}

fn graphics_slots(cs: &mut ClassSpace) -> Result<GraphicsSlots, KvmError> {
    let class = cs.load_class(CLASS)?;
    let slot = |cs: &mut ClassSpace, name: &str, desc: &str| -> Result<usize, KvmError> {
        let name = cs.syms.intern(name);
        let desc = cs.syms.intern(desc);
        Ok(cs.find_field(class, name, desc)?.slot as usize)
    };
    Ok(GraphicsSlots {
        target: slot(cs, "target", "[I")?,
        target_w: slot(cs, "target_w", "I")?,
        target_h: slot(cs, "target_h", "I")?,
        clip_x1: slot(cs, "clip_x1", "I")?,
        clip_y1: slot(cs, "clip_y1", "I")?,
        clip_x2: slot(cs, "clip_x2", "I")?,
        clip_y2: slot(cs, "clip_y2", "I")?,
        trans_x: slot(cs, "trans_x", "I")?,
        trans_y: slot(cs, "trans_y", "I")?,
        color: slot(cs, "color", "I")?,
        font: slot(cs, "font", "Ljavax/microedition/lcdui/Font;")?,
        stroke: slot(cs, "stroke", "I")?,
    })
}

fn npe(cs: &mut ClassSpace, heap: &mut Heap) -> KvmError {
    jl::throw_java(cs, heap, "java/lang/NullPointerException", None)
}

fn illegal_arg(cs: &mut ClassSpace, heap: &mut Heap, message: &str) -> KvmError {
    jl::throw_java(
        cs,
        heap,
        "java/lang/IllegalArgumentException",
        Some(message),
    )
}

fn arg_int(call: &NativeCall, index: usize) -> i32 {
    match call.args[index] {
        Slot::Int(v) => v,
        _ => 0,
    }
}

fn arg_ref(call: &NativeCall, index: usize) -> ObjRef {
    match call.args[index] {
        Slot::Ref(r) => r,
        _ => ObjRef::NULL,
    }
}

fn this_ref(call: &NativeCall) -> ObjRef {
    call.this.unwrap_or(ObjRef::NULL)
}

/// 绘制上下文快照（先算后借：标量全部 copy 出，借用即刻释放）。
struct Gfx {
    target: ObjRef,
    target_w: i32,
    clip: (i32, i32, i32, i32),
    trans: (i32, i32),
    color: i32,
    font: ObjRef,
}

fn snapshot(cs: &mut ClassSpace, heap: &Heap, this: ObjRef) -> Result<Gfx, KvmError> {
    let s = graphics_slots(cs)?;
    let fields = &heap.instance(this)?.fields;
    let ints = [
        s.target_w,
        s.target_h,
        s.clip_x1,
        s.clip_y1,
        s.clip_x2,
        s.clip_y2,
        s.trans_x,
        s.trans_y,
        s.color,
    ]
    .map(|slot| match fields[slot] {
        Slot::Int(v) => v,
        _ => 0,
    });
    let target = match fields[s.target] {
        Slot::Ref(r) => r,
        _ => return Err(KvmError::Internal("Graphics.target corrupt".into())),
    };
    let font = match fields[s.font] {
        Slot::Ref(r) => r,
        _ => ObjRef::NULL,
    };
    Ok(Gfx {
        target,
        target_w: ints[0],
        clip: (ints[2], ints[3], ints[4], ints[5]),
        trans: (ints[6], ints[7]),
        color: ints[8],
        font,
    })
}

// ---------------------------------------------------------------------------
// 光栅原语（纯 &mut [i32] 操作；坐标均设备坐标、经 clip 与边界双重钳制。
// 独立子模块避免与 native 层方法实现同名。）
// ---------------------------------------------------------------------------

/// 取证开关：SKYENGINE_KVM_NO_FASTBLIT=1 时 drawImage/drawRegion/drawRGB
/// 全部走回退臂（等价旧实现路径），用于二分直通路径回归。
fn fastblit_disabled() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISABLED.get_or_init(|| std::env::var_os("SKYENGINE_KVM_NO_FASTBLIT").is_some())
}

mod raster {
    // ---------------------------------------------------------------------------
    // 光栅原语（纯 &mut [i32] 操作；坐标均设备坐标、经 clip 与边界双重钳制）
    // ---------------------------------------------------------------------------

/// 写单像素：越界/裁剪外静默丢弃。像素恒带不透明位 0xFF000000|rgb。
#[inline]
    pub(super) fn put(
        buf: &mut [i32],
        tw: i32,
        clip: (i32, i32, i32, i32),
        x: i32,
        y: i32,
        color: i32,
    ) {
    let (cx1, cy1, cx2, cy2) = clip;
    if x < cx1 || x >= cx2 || y < cy1 || y >= cy2 || x < 0 || y < 0 || tw <= 0 {
        return;
    }
    let idx = y as usize * tw as usize + x as usize;
    if idx < buf.len() {
        buf[idx] = color;
    }
}

/// 水平段填充（含两端 x1..=x2，y 固定）。
pub(super) fn hspan(buf: &mut [i32], tw: i32, clip: (i32, i32, i32, i32), x1: i32, x2: i32, y: i32, color: i32) {
    let mut xx = x1.max(clip.0).max(0);
    let end = x2.min(clip.2 - 1).min(tw - 1);
    while xx <= end {
        if y >= clip.1 && y < clip.3 && y >= 0 {
            let idx = y as usize * tw as usize + xx as usize;
            if idx < buf.len() {
                buf[idx] = color;
            }
        }
        xx += 1;
    }
}

/// 垂直段（含两端 y1..=y2）。
pub(super) fn vspan(buf: &mut [i32], tw: i32, clip: (i32, i32, i32, i32), x: i32, y1: i32, y2: i32, color: i32) {
    let mut yy = y1.max(clip.1).max(0);
    let end = y2.min(clip.3 - 1);
    while yy <= end {
        if x >= clip.0 && x < clip.2 && x >= 0 && x < tw {
            let idx = yy as usize * tw as usize + x as usize;
            if idx < buf.len() {
                buf[idx] = color;
            }
        }
        yy += 1;
    }
}

/// Bresenham 斜线（两端点都画；含水平/垂直退化）。
pub(super) fn line(
    buf: &mut [i32],
    tw: i32,
    clip: (i32, i32, i32, i32),
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
    color: i32,
) {
    let dx = (x2 - x1).abs();
    let dy = (y2 - y1).abs();
    let sx = if x1 < x2 { 1 } else { -1 };
    let sy = if y1 < y2 { 1 } else { -1 };
    let mut err = dx - dy;
    let (mut x, mut y) = (x1, y1);
    loop {
        put(buf, tw, clip, x, y, color);
        if x == x2 && y == y2 {
            break;
        }
        let e2 = err * 2;
        if e2 > -dy {
            err -= dy;
            x += sx;
        }
        if e2 < dx {
            err += dx;
            y += sy;
        }
    }
}

/// 矩形填充 x..x+w-1（w×h；w/h<=0 no-op；与 clip 求交）。
pub(super) fn fill_rect(
    buf: &mut [i32],
    tw: i32,
    clip: (i32, i32, i32, i32),
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: i32,
) {
    if w <= 0 || h <= 0 {
        return;
    }
    let y1 = y.max(clip.1).max(0);
    let y2 = (y + h).min(clip.3);
    let x1 = x.max(clip.0).max(0);
    let x2 = (x + w).min(clip.2).min(tw);
    for yy in y1..y2 {
        let base = yy as usize * tw as usize;
        for xx in x1..x2 {
            let idx = base + xx as usize;
            if idx < buf.len() {
                buf[idx] = color;
            }
        }
    }
}

/// 矩形边框：w+1×h+1（边落 x..x+w、y..y+h）；w/h<0 no-op；
/// w==0 或 h==0 退化画线（MIDP javadoc 允许）。
pub(super) fn draw_rect(
    buf: &mut [i32],
    tw: i32,
    clip: (i32, i32, i32, i32),
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: i32,
) {
    if w < 0 || h < 0 {
        return;
    }
    if w == 0 || h == 0 {
        // 退化：w==0 画竖线 (x, y..y+h)；h==0 画横线（两者皆零画点）。
        line(buf, tw, clip, x, y, x + w, y + h, color);
        return;
    }
    hspan(buf, tw, clip, x, x + w, y, color);
    hspan(buf, tw, clip, x, x + w, y + h, color);
    vspan(buf, tw, clip, x, y, y + h, color);
    vspan(buf, tw, clip, x + w, y, y + h, color);
}

/// 椭圆参数点遍历：中心 (cx,cy)、半径 (rx,ry)，角度 start 起、
/// sweep 度（正 = 顺时针，屏幕 y 向下）；步数随周长自适应。
pub(super) fn arc_points(
    cx: f64,
    cy: f64,
    rx: f64,
    ry: f64,
    start: i32,
    sweep: i32,
    mut f: impl FnMut(i32, i32),
) {
    if sweep == 0 {
        return;
    }
    let circumference = (rx.max(ry) * std::f64::consts::PI * 2.0).max(8.0);
    let steps = ((sweep.abs() as f64 / 360.0) * circumference).ceil() as i32 + 8;
    let steps = steps.min(4000);
    for i in 0..=steps {
        let deg = start as f64 + sweep as f64 * (i as f64 / steps as f64);
        let rad = deg.to_radians();
        let px = cx + rx * rad.cos();
        let py = cy + ry * rad.sin();
        f(px.round() as i32, py.round() as i32);
    }
}

/// 圆弧边线：外接矩形 (x..x+w, y..y+h)。
pub(super) fn draw_arc(
    buf: &mut [i32],
    tw: i32,
    clip: (i32, i32, i32, i32),
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    start: i32,
    sweep: i32,
    color: i32,
) {
    if w <= 0 || h <= 0 {
        return;
    }
    let (cx, cy, rx, ry) = (x as f64 + w as f64 / 2.0, y as f64 + h as f64 / 2.0,
        w as f64 / 2.0, h as f64 / 2.0);
    arc_points(cx, cy, rx, ry, start, sweep, |px, py| {
        put(buf, tw, clip, px, py, color)
    });
}

/// 扇形填充（fillArc 含中心连线成饼）：包围盒逐像素中心测试
/// （椭圆内 + 角度落在顺时针 sweep 区间），中精度。
pub(super) fn fill_arc(
    buf: &mut [i32],
    tw: i32,
    clip: (i32, i32, i32, i32),
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    start: i32,
    sweep: i32,
    color: i32,
) {
    if w <= 0 || h <= 0 || sweep == 0 {
        return;
    }
    let (cx, cy, rx, ry) = (x as f64 + w as f64 / 2.0, y as f64 + h as f64 / 2.0,
        w as f64 / 2.0, h as f64 / 2.0);
    let start_norm = start.rem_euclid(360);
    let abs_sweep = sweep.abs().min(360) as f64;
    let in_sweep = |deg: f64| -> bool {
        let rel = (deg - start_norm as f64).rem_euclid(360.0);
        // sweep<0 为逆时针：覆盖条件改为反向距离。
        let rel = if sweep < 0 { 360.0 - rel } else { rel };
        rel <= abs_sweep
    };
    for py in y..y + h {
        for px in x..x + w {
            let nx = (px as f64 + 0.5 - cx) / rx;
            let ny = (py as f64 + 0.5 - cy) / ry;
            if nx * nx + ny * ny > 1.0 {
                continue;
            }
            let mut deg = ny.atan2(nx).to_degrees();
            if deg < 0.0 {
                deg += 360.0;
            }
            if in_sweep(deg) {
                put(buf, tw, clip, px, py, color);
            }
        }
    }
}

/// 圆角矩形填充：中部全宽、顶/底行按四分之一圆半弦收缩。
pub(super) fn fill_round_rect(
    buf: &mut [i32],
    tw: i32,
    clip: (i32, i32, i32, i32),
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    aw: i32,
    ah: i32,
    color: i32,
) {
    if w <= 0 || h <= 0 {
        return;
    }
    let aw = aw.clamp(0, w);
    let ah = ah.clamp(0, h);
    for row in 0..h {
        let d = if row < ah {
            ah - row // 顶部：圆心行距离（圆心在 y+ah）
        } else if row >= h - ah {
            ah - (h - 1 - row) // 底部：圆心在 y+h-1-ah
        } else {
            0
        };
        let inset = if d <= 0 || ah == 0 {
            0
        } else {
            let t = 1.0 - (d as f64 / ah as f64) * (d as f64 / ah as f64);
            if t <= 0.0 {
                aw
            } else {
                (aw as f64 * (1.0 - t.sqrt())).round() as i32
            }
        };
        hspan(buf, tw, clip, x + inset, x + w - 1 - inset, y + row, color);
    }
}

/// 圆角矩形边框（w+1×h+1 边 + 四角四分之一圆弧）。
pub(super) fn draw_round_rect(
    buf: &mut [i32],
    tw: i32,
    clip: (i32, i32, i32, i32),
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    aw: i32,
    ah: i32,
    color: i32,
) {
    if w < 0 || h < 0 {
        return;
    }
    if w == 0 || h == 0 {
        line(buf, tw, clip, x, y, x + w, y + h, color);
        return;
    }
    let aw = aw.clamp(0, w);
    let ah = ah.clamp(0, h);
    // 直边段（圆角让位）。
    hspan(buf, tw, clip, x + aw, x + w - aw, y, color);
    hspan(buf, tw, clip, x + aw, x + w - aw, y + h, color);
    vspan(buf, tw, clip, x, y + ah, y + h - ah, color);
    vspan(buf, tw, clip, x + w, y + ah, y + h - ah, color);
    if aw == 0 || ah == 0 {
        return;
    }
    // 四角弧（屏幕顺时针角：0=右 90=下 180=左 270=上）。
    let corners = [
        (x as f64 + aw as f64, y as f64 + ah as f64, 180, 90), // 左上 180→270
        (x as f64 + w as f64 - aw as f64, y as f64 + ah as f64, 270, 90), // 右上 270→360
        (x as f64 + w as f64 - aw as f64, y as f64 + h as f64 - ah as f64, 0, 90), // 右下
        (x as f64 + aw as f64, y as f64 + h as f64 - ah as f64, 90, 90), // 左下
    ];
    for (ccx, ccy, start, sweep) in corners {
        arc_points(ccx, ccy, aw as f64, ah as f64, start, sweep, |px, py| {
            put(buf, tw, clip, px, py, color)
        });
    }
}

/// 三角形填充：边函数半平面测试（含边）；退化（共线）画三边线。
pub(super) fn fill_triangle(
    buf: &mut [i32],
    tw: i32,
    clip: (i32, i32, i32, i32),
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
    x3: i32,
    y3: i32,
    color: i32,
) {
    let edge = |ax: i32, ay: i32, bx: i32, by: i32, px: f64, py: f64| -> f64 {
        (bx as f64 - ax as f64) * (py - ay as f64) - (by as f64 - ay as f64) * (px - ax as f64)
    };
    let area = edge(x1, y1, x2, y2, x3 as f64, y3 as f64);
    if area == 0.0 {
        line(buf, tw, clip, x1, y1, x2, y2, color);
        line(buf, tw, clip, x2, y2, x3, y3, color);
        line(buf, tw, clip, x3, y3, x1, y1, color);
        return;
    }
    let minx = x1.min(x2).min(x3);
    let maxx = x1.max(x2).max(x3);
    let miny = y1.min(y2).min(y3);
    let maxy = y1.max(y2).max(y3);
    for py in miny..=maxy {
        for px in minx..=maxx {
            let (fx, fy) = (px as f64 + 0.5, py as f64 + 0.5);
            let (w0, w1, w2) = (
                edge(x2, y2, x3, y3, fx, fy),
                edge(x3, y3, x1, y1, fx, fy),
                edge(x1, y1, x2, y2, fx, fy),
            );
            let inside = (w0 >= 0.0 && w1 >= 0.0 && w2 >= 0.0)
                || (w0 <= 0.0 && w1 <= 0.0 && w2 <= 0.0);
            if inside {
                put(buf, tw, clip, px, py, color);
            }
        }
    }
}

/// 目标行/列范围一次性求交：clip ∩ 屏幕矩形 ∩ 绘制矩形。
/// 返回 None 表示完全不可见。行/列循环内不再做逐像素边界判断
/// （旧实现每像素 6 分支 + 越界查表，渲染热点）。
#[inline]
fn clip_span(
    buf_len: usize,
    tw: i32,
    clip: (i32, i32, i32, i32),
    dx: i32,
    dy: i32,
    sw: i32,
    sh: i32,
) -> Option<(i32, i32, i32, i32)> {
    let screen_h = (buf_len / tw.max(1) as usize) as i32;
    let x0 = dx.max(clip.0).max(0);
    let x1 = (dx + sw).min(clip.2).min(tw);
    let y0 = dy.max(clip.1).max(0);
    let y1 = (dy + sh).min(clip.3).min(screen_h);
    (x0 < x1 && y0 < y1).then_some((x0, x1, y0, y1))
}

/// 像素写入：不透明直拷，全透明跳过，中间值 source-over 混合。
#[inline]
fn composite_pixel(dst: &mut i32, s: i32) {
    let alpha = (s >> 24) & 0xFF;
    if alpha == 0 {
        return;
    }
    if alpha == 0xFF {
        *dst = s;
        return;
    }
    let inv = 255 - alpha;
    let d = *dst;
    let mix = |sc: i32, dc: i32| -> i32 { (sc * alpha + dc * inv) / 255 };
    let r = mix((s >> 16) & 0xFF, (d >> 16) & 0xFF);
    let g = mix((s >> 8) & 0xFF, (d >> 8) & 0xFF);
    let b = mix(s & 0xFF, d & 0xFF);
    let out_a = (alpha + ((d >> 24) & 0xFF) * inv / 255).min(0xFF);
    *dst = (out_a << 24) | (r << 16) | (g << 8) | b;
}

/// 行距扫描 blit（drawRGB 直通路径）：源是 int[] 的 (offset, scan) 行距
/// 视图；opaque=true 时按 processAlpha=false 语义强制不透明。
pub(super) fn blit_scan(
    buf: &mut [i32],
    tw: i32,
    clip: (i32, i32, i32, i32),
    src: &[i32],
    offset: usize,
    scan: usize,
    w: i32,
    h: i32,
    dx: i32,
    dy: i32,
    opaque: bool,
) {
    let Some((x0, x1, y0, y1)) = clip_span(buf.len(), tw, clip, dx, dy, w, h) else {
        return;
    };
    for ty in y0..y1 {
        let src_row = offset + (ty - dy) as usize * scan;
        let dst_row = ty as usize * tw as usize;
        for tx in x0..x1 {
            let raw = src[src_row + (tx - dx) as usize];
            let s = if opaque {
                super::OPAQUE | (raw & 0xFF_FFFF)
            } else {
                raw
            };
            composite_pixel(&mut buf[dst_row + tx as usize], s);
        }
    }
}

/// 整幅 blit（source-over alpha 混合）：源 alpha=0 不碰目标（字库
/// 位图字为透明底，直拷会把整块擦黑——冒烟实证的"方块"根因）、
/// 255 直拷、中间值逐通道混合；目标行/列范围预先与 clip/边界求交。
pub(super) fn blit(
    buf: &mut [i32],
    tw: i32,
    clip: (i32, i32, i32, i32),
    src: &[i32],
    sw: i32,
    sh: i32,
    dx: i32,
    dy: i32,
) {
    let Some((x0, x1, y0, y1)) = clip_span(buf.len(), tw, clip, dx, dy, sw, sh) else {
        return;
    };
    for ty in y0..y1 {
        let src_row = (ty - dy) as usize * sw as usize;
        let dst_row = ty as usize * tw as usize;
        for tx in x0..x1 {
            let s = src[src_row + (tx - dx) as usize];
            composite_pixel(&mut buf[dst_row + tx as usize], s);
        }
    }
}

/// 跨步 blit（drawRegion t=0/t=2 直通路径）：源是大图（图集）里的
/// 子矩形 (sx, sy, sw, sh)，行距 src_stride；hflip 时行内列序倒放
/// （t=2 水平镜像）。语义与"先物化镜像缓冲再 blit"逐像素一致，
/// 省去中间缓冲分配。
pub(super) fn blit_strided(
    buf: &mut [i32],
    tw: i32,
    clip: (i32, i32, i32, i32),
    src: &[i32],
    src_stride: usize,
    sx: usize,
    sy: usize,
    sw: i32,
    sh: i32,
    dx: i32,
    dy: i32,
    hflip: bool,
) {
    let Some((x0, x1, y0, y1)) = clip_span(buf.len(), tw, clip, dx, dy, sw, sh) else {
        return;
    };
    for ty in y0..y1 {
        let src_row = (sy + (ty - dy) as usize) * src_stride + sx;
        let dst_row = ty as usize * tw as usize;
        if hflip {
            for tx in x0..x1 {
                let col = sw as usize - 1 - (tx - dx) as usize;
                let s = src[src_row + col];
                composite_pixel(&mut buf[dst_row + tx as usize], s);
            }
        } else {
            for tx in x0..x1 {
                let s = src[src_row + (tx - dx) as usize];
                composite_pixel(&mut buf[dst_row + tx as usize], s);
            }
        }
    }
}

} // mod raster

/// 锚点修正量 (dx, dy)：以被绘对象自身度量 (w, h, baseline) 平移 (x,y)。
/// 文本禁 VCENTER、图像禁 BASELINE；水平/垂直位各须恰一个（MIDP javadoc）。
fn anchor_delta(
    anchor: i32,
    w: i32,
    h: i32,
    baseline: i32,
    text: bool,
) -> Result<(i32, i32), &'static str> {
    if anchor & !(H_MASK | V_MASK) != 0 {
        return Err("invalid anchor");
    }
    // 斯凯真机兼容（SP-1 冒烟实证）：jc.jar 以 anchor=0 绘制文本
    // （规范上非法——水平/垂直位各须恰一个），MTK/斯凯 JVM 按
    // LEFT|TOP 兜底；严格抛 IllegalArgumentException 会杀游戏的
    // 画面 tick（f.a()V 每帧抛、e.run 主循环 catch 后画面停滞）。
    let anchor = if anchor == 0 { LEFT | TOP } else { anchor };
    let (hx, vy) = (anchor & H_MASK, anchor & V_MASK);
    if hx.count_ones() != 1 || vy.count_ones() != 1 {
        return Err("anchor needs exactly one horizontal and one vertical bit");
    }
    if text && (anchor & VCENTER) != 0 {
        return Err("VCENTER invalid for text");
    }
    if !text && (anchor & BASELINE) != 0 {
        return Err("BASELINE invalid for images");
    }
    let dx = match hx {
        x if x == RIGHT => w,
        x if x == HCENTER => w / 2,
        _ => 0,
    };
    let dy = match vy {
        y if y == BOTTOM => h,
        y if y == VCENTER => h / 2,
        y if y == BASELINE => baseline,
        _ => 0,
    };
    Ok((dx, dy))
}

// ---------------------------------------------------------------------------
// 构造（Image.getGraphics 唯一路径；T021 Canvas 复用）
// ---------------------------------------------------------------------------

/// 分配 Graphics 实例：绑定目标数组、clip=全图、translate=(0,0)、
/// color=0（黑）、字体=传入实例（默认字体）。
pub fn alloc_graphics(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    target: ObjRef,
    width: i32,
    height: i32,
    font: ObjRef,
) -> Result<ObjRef, KvmError> {
    let s = graphics_slots(cs)?;
    let class = cs.load_class(CLASS)?;
    let template = cs.class(class).instance_template.clone();
    let g = heap.alloc_instance(class, template)?;
    let f = &mut heap.instance_mut(g)?.fields;
    f[s.target] = Slot::Ref(target);
    f[s.target_w] = Slot::Int(width);
    f[s.target_h] = Slot::Int(height);
    f[s.clip_x1] = Slot::Int(0);
    f[s.clip_y1] = Slot::Int(0);
    f[s.clip_x2] = Slot::Int(width);
    f[s.clip_y2] = Slot::Int(height);
    f[s.trans_x] = Slot::Int(0);
    f[s.trans_y] = Slot::Int(0);
    f[s.color] = Slot::Int(0);
    f[s.font] = Slot::Ref(font);
    f[s.stroke] = Slot::Int(SOLID);
    Ok(g)
}

/// 直写 Graphics 裁剪框（T021 Canvas paint 回调链：分配后把
/// clip 从全图收窄为 脏矩形∩屏；坐标为设备坐标角点 x1/y1/x2/y2）。
pub fn set_clip_to(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    graphics: ObjRef,
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
) -> Result<(), KvmError> {
    let s = graphics_slots(cs)?;
    let f = &mut heap.instance_mut(graphics)?.fields;
    f[s.clip_x1] = Slot::Int(x1);
    f[s.clip_y1] = Slot::Int(y1);
    f[s.clip_x2] = Slot::Int(x2);
    f[s.clip_y2] = Slot::Int(y2);
    Ok(())
}

// ---------------------------------------------------------------------------
// 状态读（纯轨）
// ---------------------------------------------------------------------------

macro_rules! state_get {
    ($name:ident, $slot:ident, $doc:expr) => {
        #[doc = $doc]
        pub fn $name(
            cs: &mut ClassSpace,
            heap: &mut Heap,
            call: &NativeCall,
        ) -> Result<Option<Slot>, KvmError> {
            let this = this_ref(call);
            if this == ObjRef::NULL {
                return Err(npe(cs, heap));
            }
            let s = graphics_slots(cs)?;
            match heap.instance(this)?.fields[s.$slot] {
                Slot::Int(v) => Ok(Some(Slot::Int(v))),
                _ => Err(KvmError::Internal(concat!("Graphics.", stringify!($slot), " corrupt").into())),
            }
        }
    };
}

state_get!(get_clip_x, clip_x1, "`getClipX()I`：裁剪左上 X（设备坐标）。");
state_get!(get_clip_y, clip_y1, "`getClipY()I`：裁剪左上 Y（设备坐标）。");

/// `getClipWidth()I`：max(0, x2-x1)（空裁剪规范为 0 宽）。
pub fn get_clip_width(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let s = graphics_slots(cs)?;
    let fields = &heap.instance(this)?.fields;
    let (x1, x2) = match (fields[s.clip_x1], fields[s.clip_x2]) {
        (Slot::Int(a), Slot::Int(b)) => (a, b),
        _ => return Err(KvmError::Internal("Graphics.clip corrupt".into())),
    };
    Ok(Some(Slot::Int((x2 - x1).max(0))))
}

/// `getClipHeight()I`：max(0, y2-y1)。
pub fn get_clip_height(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let s = graphics_slots(cs)?;
    let fields = &heap.instance(this)?.fields;
    let (y1, y2) = match (fields[s.clip_y1], fields[s.clip_y2]) {
        (Slot::Int(a), Slot::Int(b)) => (a, b),
        _ => return Err(KvmError::Internal("Graphics.clip corrupt".into())),
    };
    Ok(Some(Slot::Int((y2 - y1).max(0))))
}

state_get!(get_translate_x, trans_x, "`getTranslateX()I`：translate 累积 X。");
state_get!(get_translate_y, trans_y, "`getTranslateY()I`：translate 累积 Y。");
state_get!(get_color, color, "`getColor()I`：当前颜色（24 位）。");
state_get!(get_stroke_style, stroke, "`getStrokeStyle()I`：SOLID/DOTTED。");

/// `getRedComponent()I`。
pub fn get_red_component(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let s = graphics_slots(cs)?;
    match heap.instance(this)?.fields[s.color] {
        Slot::Int(v) => Ok(Some(Slot::Int((v >> 16) & 0xFF))),
        _ => Err(KvmError::Internal("Graphics.color corrupt".into())),
    }
}

/// `getGreenComponent()I`。
pub fn get_green_component(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let s = graphics_slots(cs)?;
    match heap.instance(this)?.fields[s.color] {
        Slot::Int(v) => Ok(Some(Slot::Int((v >> 8) & 0xFF))),
        _ => Err(KvmError::Internal("Graphics.color corrupt".into())),
    }
}

/// `getBlueComponent()I`。
pub fn get_blue_component(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let s = graphics_slots(cs)?;
    match heap.instance(this)?.fields[s.color] {
        Slot::Int(v) => Ok(Some(Slot::Int(v & 0xFF))),
        _ => Err(KvmError::Internal("Graphics.color corrupt".into())),
    }
}

/// `getGrayScale()I`：等价回读（灰度时三通道相同）。
pub fn get_gray_scale(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let s = graphics_slots(cs)?;
    match heap.instance(this)?.fields[s.color] {
        Slot::Int(v) => Ok(Some(Slot::Int((v >> 16) & 0xFF))),
        _ => Err(KvmError::Internal("Graphics.color corrupt".into())),
    }
}

/// `getFont()LFont;`：回读当前字体（永不 null——构造/置空均落默认字体）。
pub fn get_font(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let s = graphics_slots(cs)?;
    match heap.instance(this)?.fields[s.font] {
        Slot::Ref(r) => Ok(Some(Slot::Ref(r))),
        _ => Err(KvmError::Internal("Graphics.font corrupt".into())),
    }
}

// ---------------------------------------------------------------------------
// 状态写（纯轨）
// ---------------------------------------------------------------------------

/// `translate(II)V`：累积。
pub fn translate(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let (x, y) = (arg_int(call, 0), arg_int(call, 1));
    let s = graphics_slots(cs)?;
    let fields = &mut heap.instance_mut(this)?.fields;
    for (slot, delta) in [(s.trans_x, x), (s.trans_y, y)] {
        if let Slot::Int(v) = fields[slot] {
            fields[slot] = Slot::Int(v.wrapping_add(delta));
        }
    }
    Ok(None)
}

/// `clipRect(IIII)V`：当前 translate 解释坐标后与当前裁剪求交（非替换）；
/// 空交规范为退化矩形（宽/高回读为 0）。
pub fn clip_rect(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let (x, y, w, h) = (arg_int(call, 0), arg_int(call, 1), arg_int(call, 2), arg_int(call, 3));
    let s = graphics_slots(cs)?;
    let fields = &mut heap.instance_mut(this)?.fields;
    let (tx, ty) = match (fields[s.trans_x], fields[s.trans_y]) {
        (Slot::Int(a), Slot::Int(b)) => (a, b),
        _ => (0, 0),
    };
    let (ox1, oy1, ox2, oy2) = match (
        fields[s.clip_x1],
        fields[s.clip_y1],
        fields[s.clip_x2],
        fields[s.clip_y2],
    ) {
        (Slot::Int(a), Slot::Int(b), Slot::Int(c), Slot::Int(d)) => (a, b, c, d),
        _ => return Err(KvmError::Internal("Graphics.clip corrupt".into())),
    };
    // 空裁剪保持空（x1 > x2 形态下任何求交仍空）。
    let nx1 = (x + tx).max(ox1);
    let ny1 = (y + ty).max(oy1);
    let nx2 = (x + tx + w).min(ox2);
    let ny2 = (y + ty + h).min(oy2);
    fields[s.clip_x1] = Slot::Int(nx1);
    fields[s.clip_y1] = Slot::Int(ny1);
    fields[s.clip_x2] = Slot::Int(nx2);
    fields[s.clip_y2] = Slot::Int(ny2);
    Ok(None)
}

/// `setClip(IIII)V`：替换裁剪（当前 translate 解释，MIDP javadoc）。
pub fn set_clip(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let (x, y, w, h) = (arg_int(call, 0), arg_int(call, 1), arg_int(call, 2), arg_int(call, 3));
    let s = graphics_slots(cs)?;
    let fields = &mut heap.instance_mut(this)?.fields;
    let (tx, ty) = match (fields[s.trans_x], fields[s.trans_y]) {
        (Slot::Int(a), Slot::Int(b)) => (a, b),
        _ => (0, 0),
    };
    fields[s.clip_x1] = Slot::Int(x + tx);
    fields[s.clip_y1] = Slot::Int(y + ty);
    fields[s.clip_x2] = Slot::Int(x + tx + w);
    fields[s.clip_y2] = Slot::Int(y + ty + h);
    Ok(None)
}

/// `setColor(I)V`：取低 24 位（alpha 忽略）。
pub fn set_color_int(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let rgb = arg_int(call, 0) & 0x00FF_FFFF;
    let s = graphics_slots(cs)?;
    heap.instance_mut(this)?.fields[s.color] = Slot::Int(rgb);
    Ok(None)
}

/// `setColor(III)V`：分量 0..255，越界抛 IllegalArgumentException。
pub fn set_color_iii(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let (r, g, b) = (arg_int(call, 0), arg_int(call, 1), arg_int(call, 2));
    if !(0..=255).contains(&r) || !(0..=255).contains(&g) || !(0..=255).contains(&b) {
        return Err(illegal_arg(cs, heap, "color component out of range"));
    }
    let rgb = (r << 16) | (g << 8) | b;
    let s = graphics_slots(cs)?;
    heap.instance_mut(this)?.fields[s.color] = Slot::Int(rgb);
    Ok(None)
}

/// `setGrayScale(I)V`：灰度复制到三通道；越界抛 IllegalArgumentException。
pub fn set_gray_scale(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let v = arg_int(call, 0);
    if !(0..=255).contains(&v) {
        return Err(illegal_arg(cs, heap, "gray scale out of range"));
    }
    let rgb = (v << 16) | (v << 8) | v;
    let s = graphics_slots(cs)?;
    heap.instance_mut(this)?.fields[s.color] = Slot::Int(rgb);
    Ok(None)
}

/// `setStrokeStyle(I)V`：SOLID/DOTTED 之外抛 IllegalArgumentException
///（存储无视觉差异，Canvas 全 FILL 语义）。
pub fn set_stroke_style(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let style = arg_int(call, 0);
    if style != SOLID && style != DOTTED {
        return Err(illegal_arg(cs, heap, "bad stroke style"));
    }
    let s = graphics_slots(cs)?;
    heap.instance_mut(this)?.fields[s.stroke] = Slot::Int(style);
    Ok(None)
}

/// 目标数组可变借用统一入口（非 `[I` 视为内部腐坏）。
fn target_buf(heap: &mut Heap, arr: ObjRef) -> Result<&mut Vec<i32>, KvmError> {
    match &mut heap.array_mut(arr)?.data {
        ArrayData::Int(data) => Ok(data),
        _ => Err(KvmError::Internal("Graphics.target not int[]".into())),
    }
}

// ---------------------------------------------------------------------------
// 光栅绘制（纯轨）
// ---------------------------------------------------------------------------

/// `drawLine(IIII)V`：两端点都画（Bresenham）。
pub fn draw_line(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let (x1, y1, x2, y2) = (
        arg_int(call, 0),
        arg_int(call, 1),
        arg_int(call, 2),
        arg_int(call, 3),
    );
    let g = snapshot(cs, heap, this)?;
    let color = OPAQUE | g.color;
    let buf = target_buf(heap, g.target)?;
    raster::line(buf, g.target_w, g.clip, x1 + g.trans.0, y1 + g.trans.1, x2 + g.trans.0, y2 + g.trans.1, color);
    Ok(None)
}

/// `drawRect(IIII)V`：w+1×h+1 边框（w/h<0 no-op、w==0/h==0 退化画线）。
pub fn draw_rect(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let (x, y, w, h) = (arg_int(call, 0), arg_int(call, 1), arg_int(call, 2), arg_int(call, 3));
    let g = snapshot(cs, heap, this)?;
    let color = OPAQUE | g.color;
    let buf = target_buf(heap, g.target)?;
    raster::draw_rect(buf, g.target_w, g.clip, x + g.trans.0, y + g.trans.1, w, h, color);
    Ok(None)
}

/// `fillRect(IIII)V`：w×h（w/h<=0 no-op）。
pub fn fill_rect(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let (x, y, w, h) = (arg_int(call, 0), arg_int(call, 1), arg_int(call, 2), arg_int(call, 3));
    let g = snapshot(cs, heap, this)?;
    let color = OPAQUE | g.color;
    let buf = target_buf(heap, g.target)?;
    raster::fill_rect(buf, g.target_w, g.clip, x + g.trans.0, y + g.trans.1, w, h, color);
    Ok(None)
}

/// `drawRoundRect(IIIIII)V`。
pub fn draw_round_rect(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let (x, y, w, h, aw, ah) = (
        arg_int(call, 0),
        arg_int(call, 1),
        arg_int(call, 2),
        arg_int(call, 3),
        arg_int(call, 4),
        arg_int(call, 5),
    );
    let g = snapshot(cs, heap, this)?;
    let color = OPAQUE | g.color;
    let buf = target_buf(heap, g.target)?;
    raster::draw_round_rect(buf, g.target_w, g.clip, x + g.trans.0, y + g.trans.1, w, h, aw, ah, color);
    Ok(None)
}

/// `fillRoundRect(IIIIII)V`：圆角半径钳制到 w/h。
pub fn fill_round_rect(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let (x, y, w, h, aw, ah) = (
        arg_int(call, 0),
        arg_int(call, 1),
        arg_int(call, 2),
        arg_int(call, 3),
        arg_int(call, 4),
        arg_int(call, 5),
    );
    let g = snapshot(cs, heap, this)?;
    let color = OPAQUE | g.color;
    let buf = target_buf(heap, g.target)?;
    raster::fill_round_rect(buf, g.target_w, g.clip, x + g.trans.0, y + g.trans.1, w, h, aw, ah, color);
    Ok(None)
}

/// `drawArc(IIIIII)V`：角度制，0=3 点钟、顺时针增大。
pub fn draw_arc(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let (x, y, w, h, start, sweep) = (
        arg_int(call, 0),
        arg_int(call, 1),
        arg_int(call, 2),
        arg_int(call, 3),
        arg_int(call, 4),
        arg_int(call, 5),
    );
    let g = snapshot(cs, heap, this)?;
    let color = OPAQUE | g.color;
    let buf = target_buf(heap, g.target)?;
    raster::draw_arc(buf, g.target_w, g.clip, x + g.trans.0, y + g.trans.1, w, h, start, sweep, color);
    Ok(None)
}

/// `fillArc(IIIIII)V`：含中心连线成饼。
pub fn fill_arc(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let (x, y, w, h, start, sweep) = (
        arg_int(call, 0),
        arg_int(call, 1),
        arg_int(call, 2),
        arg_int(call, 3),
        arg_int(call, 4),
        arg_int(call, 5),
    );
    let g = snapshot(cs, heap, this)?;
    let color = OPAQUE | g.color;
    let buf = target_buf(heap, g.target)?;
    raster::fill_arc(buf, g.target_w, g.clip, x + g.trans.0, y + g.trans.1, w, h, start, sweep, color);
    Ok(None)
}

/// `fillTriangle(IIIIII)V`：扫描填充（含边；退化画三边线）。
pub fn fill_triangle(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let pts = [
        arg_int(call, 0),
        arg_int(call, 1),
        arg_int(call, 2),
        arg_int(call, 3),
        arg_int(call, 4),
        arg_int(call, 5),
    ];
    let g = snapshot(cs, heap, this)?;
    let color = OPAQUE | g.color;
    let buf = target_buf(heap, g.target)?;
    raster::fill_triangle(
        buf,
        g.target_w,
        g.clip,
        pts[0] + g.trans.0,
        pts[1] + g.trans.1,
        pts[2] + g.trans.0,
        pts[3] + g.trans.1,
        pts[4] + g.trans.0,
        pts[5] + g.trans.1,
        color,
    );
    Ok(None)
}

/// `drawImage(LImage;III)V`：整幅 blit（不可变/可变源均可），
/// 锚点按源图宽高修正；源像素含 alpha 直拷。
pub fn draw_image(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let img = arg_ref(call, 0);
    if img == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let (x, y, anchor) = (arg_int(call, 1), arg_int(call, 2), arg_int(call, 3));
    // 先快照 Graphics、再取源图尺寸/数组引用（不借堆、不拷贝像素）。
    let g = snapshot(cs, heap, this)?;
    let (src_w, src_h, src_arr) = image::source_dims_array(cs, heap, img)?;
    let (dx, dy) = anchor_delta(anchor, src_w, src_h, 0, false)
        .map_err(|e| illegal_arg(cs, heap, e))?;
    // 直通：拆分借用图集（只读）与目标（可写），零中间拷贝——旧路径
    // 每次调用整图深拷贝两遍（read_source u32 转换 + 回转 i32），为
    // 渲染第一热点。源==目标（图自绘，罕见）时回退快照路径。
    let (fx, fy) = (x - dx + g.trans.0, y - dy + g.trans.1);
    match if fastblit_disabled() {
        Err(KvmError::Internal("fastblit disabled".into()))
    } else {
        heap.int_arrays_src_dst(src_arr, g.target)
    } {
        Ok((src, dst)) => {
            raster::blit(dst, g.target_w, g.clip, src, src_w, src_h, fx, fy);
        }
        Err(_) => {
            let pixels = image::read_source(cs, heap, img)?;
            let copied: Vec<i32> = pixels.pixels.iter().map(|&p| p as i32).collect();
            let buf = target_buf(heap, g.target)?;
            raster::blit(
                buf,
                g.target_w,
                g.clip,
                &copied,
                src_w,
                src_h,
                fx,
                fy,
            );
        }
    }
    Ok(None)
}

/// `drawRegion(Ljavax/microedition/lcdui/Image;IIIIIIII)V`：源区域按
/// transform 变换后经 anchor 画到目标。变换用 mirror（垂直轴镜像）
/// + rot90cw 两个基元组合。**斯凯原生编号**（gjxjwql 实机定案：
/// 0-5 与 JSR-118 相同，6=ROT270 / 7=MIRROR_ROT90 与 JSR 相反；
/// 证据链 = VMRP mr_drawRegion 逐 case 对齐 + 游戏 r.a anchor 补偿表
/// 自洽性 + be.a 边框四角 0/2/1/3 标准镜像）。映射表见函数体注释。
/// 冒烟实证：游戏位图字库切字经此路径，空桩致方块。
pub fn draw_region(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let img = arg_ref(call, 0);
    if img == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let (sx, sy, sw, sh, transform, dx, dy, anchor) = (
        arg_int(call, 1),
        arg_int(call, 2),
        arg_int(call, 3),
        arg_int(call, 4),
        arg_int(call, 5),
        arg_int(call, 6),
        arg_int(call, 7),
        arg_int(call, 8),
    );
    if !(0..8).contains(&transform) {
        return Err(illegal_arg(cs, heap, "drawRegion transform out of range"));
    }
    // 调试面：SKYENGINE_KVM_TRACE_REGION=1 打印参数（精灵碎片错位取证：
    // 对比正常/异常实例的源区与变换差异）。
    if std::env::var_os("SKYENGINE_KVM_TRACE_REGION").is_some() {
        eprintln!(
            "[kvm-region] v={} src({sx},{sy},{sw}x{sh}) t={transform} dst({dx},{dy}) anchor={anchor}",
            crate::debug_switch::transform_variant()
        );
    }
    let g = snapshot(cs, heap, this)?;
    let (src_w, src_h, src_arr) = image::source_dims_array(cs, heap, img)?;
    // 源区域快照（负尺寸 IAE；区域钳到图像内，空区域 no-op）。
    if sw < 0 || sh < 0 {
        return Err(illegal_arg(cs, heap, "drawRegion negative size"));
    }
    let sx = sx.max(0) as usize;
    let sy = sy.max(0) as usize;
    let sw = (sw as usize).min(src_w.max(0) as usize - sx.min(src_w.max(0) as usize));
    let sh = (sh as usize).min(src_h.max(0) as usize - sy.min(src_h.max(0) as usize));
    if sw == 0 || sh == 0 {
        return Ok(None);
    }
    // 斯凯 KVM transform 语义（t=2 经 gjxjwql 双盲实验实证为水平镜像；
    // 全表对照 VMRP 逆向的 mr_drawRegion 原生实现核定）：
    // 0 NONE / 1 MIRROR_ROT180（垂直镜像）/ 2 MIRROR（水平镜像）/
    // 3 ROT180 / 4 MIRROR_ROT270 / 5 ROT90 / 6 ROT270 / 7 MIRROR_ROT90。
    // 本表以 (mirror=水平镜像, quarter=顺时针 90° 次数) 组合表达，
    // 由 tools 级指针步进模拟对齐（trans_check）。
    // 变体 1-3 为取证实验开关（debug_switch，GUI F9 循环）：jsr=6/7
    // 按编号互换；swap13=1/3 互换；nomirror=镜像族全部去镜像。定案后
    // 应保持变体 0。
    let variant = crate::debug_switch::transform_variant();
    let (mirror, quarter): (bool, u32) = match (transform, variant) {
        // 变体 1（jsr）：仅 6/7 与基准互换。
        (6, 1) => (true, 1),
        (7, 1) => (false, 3),
        // 变体 2（swap13）：仅 1/3 互换。
        (1, 2) => (false, 2),
        (3, 2) => (true, 2),
        // 变体 3（nomirror）：镜像族去镜像（2/4/6/7）。
        (2, 3) => (false, 0),
        (4, 3) => (false, 3),
        (6, 3) => (false, 3),
        (7, 3) => (false, 1),
        _ => match transform {
            0 => (false, 0),
            1 => (true, 2),
            2 => (true, 0),
            3 => (false, 2),
            4 => (true, 3),
            5 => (false, 1),
            6 => (false, 3),
            _ => (true, 1),
        },
    };
    // 直通路径：t=0 / t=2（quarter==0）输出与源区同尺寸，直接跨步
    // blit 图集子矩形（t=2 行内倒放），零中间缓冲——占实际调用 99%+
    // （gjxjwql 场景 6.2 万次里 t=2 占 6.1 万）。旋转族走小尺寸
    // 变换缓冲（sw×sh 级），再常规 blit。
    if quarter == 0 {
        let (adx, ady) = anchor_delta(anchor, sw as i32, sh as i32, 0, false)
            .map_err(|e| illegal_arg(cs, heap, e))?;
        let (fx, fy) = (dx - adx + g.trans.0, dy - ady + g.trans.1);
        return match if fastblit_disabled() {
            Err(KvmError::Internal("fastblit disabled".into()))
        } else {
            heap.int_arrays_src_dst(src_arr, g.target)
        } {
            Ok((src, dst)) => {
                raster::blit_strided(
                    dst,
                    g.target_w,
                    g.clip,
                    src,
                    src_w.max(0) as usize,
                    sx,
                    sy,
                    sw as i32,
                    sh as i32,
                    fx,
                    fy,
                    mirror,
                );
                Ok(None)
            }
            Err(_) => {
                let pixels = image::read_source(cs, heap, img)?;
                let out: Vec<i32> = (0..sh * sw)
                    .map(|i| {
                        let (y, x) = (i / sw, i % sw);
                        pixels.pixels[(sy + y) * src_w.max(0) as usize + sx
                            + if mirror { sw - 1 - x } else { x }]
                            as i32
                    })
                    .collect();
                let buf = target_buf(heap, g.target)?;
                raster::blit(
                    buf,
                    g.target_w,
                    g.clip,
                    &out,
                    sw as i32,
                    sh as i32,
                    fx,
                    fy,
                );
                Ok(None)
            }
        };
    }
    // 旋转族：小缓冲变换（mirror → 逐次 rot90cw：(x,y)→(h_cur-1-y, x)，
    // 尺寸同步互换）。源像素直接借堆数组读取（不整图拷贝）。
    // 输出尺寸按奇偶次旋转取（quarter=2 即 t=1/t=3 为 180°，不换维）。
    let (out_w, out_h) = if quarter % 2 == 0 { (sw, sh) } else { (sh, sw) };
    let out = {
        let pixels = match &heap.array(src_arr)?.data {
            ArrayData::Int(data) => data,
            _ => return Err(KvmError::Internal("Image pixels array corrupt".into())),
        };
        let stride = src_w.max(0) as usize;
        let mut out = vec![0_i32; out_w * out_h];
        for y in 0..sh {
            for x in 0..sw {
                let (mut px, mut py) = (x as i32, y as i32);
                if mirror {
                    px = sw as i32 - 1 - px;
                }
                let (mut w_cur, mut h_cur) = (sw as i32, sh as i32);
                for _ in 0..quarter {
                    let (nx, ny) = (h_cur - 1 - py, px);
                    px = nx;
                    py = ny;
                    std::mem::swap(&mut w_cur, &mut h_cur);
                }
                debug_assert!((0..out_w as i32).contains(&px) && (0..out_h as i32).contains(&py));
                out[py as usize * out_w + px as usize] = pixels[(sy + y) * stride + sx + x];
            }
        }
        out
    };
    let (adx, ady) = anchor_delta(anchor, out_w as i32, out_h as i32, 0, false)
        .map_err(|e| illegal_arg(cs, heap, e))?;
    let buf = target_buf(heap, g.target)?;
    raster::blit(
        buf,
        g.target_w,
        g.clip,
        &out,
        out_w as i32,
        out_h as i32,
        dx - adx + g.trans.0,
        dy - ady + g.trans.1,
    );
    Ok(None)
}

/// `drawRGB([IIIIIIIZ)V`：rgbData 直绘（processAlpha=false 时统一不透明）。
pub fn draw_rgb(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let array = arg_ref(call, 0);
    if array == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let (offset, scanlength, x, y, w, h) = (
        arg_int(call, 1),
        arg_int(call, 2),
        arg_int(call, 3),
        arg_int(call, 4),
        arg_int(call, 5),
        arg_int(call, 6),
    );
    let process_alpha = arg_int(call, 7) != 0;
    let g = snapshot(cs, heap, this)?;
    if w <= 0 || h <= 0 {
        return Ok(None);
    }
    // 行距语义：scanlength 为源行距（非正值按行宽兜底）。
    let scan = if scanlength > 0 { scanlength } else { w };
    let need = offset + scan * (h - 1) + w;
    // 范围/类型检查先行（不可变借用内取长度；越界按规范抛 AIOOBE）。
    let src_len = match &heap.array(array)?.data {
        ArrayData::Int(data) => data.len() as i32,
        _ => return Err(illegal_arg(cs, heap, "rgbData must be int[]")),
    };
    if offset < 0 || need > src_len {
        return Err(jl::throw_java(
            cs,
            heap,
            "java/lang/ArrayIndexOutOfBoundsException",
            Some("rgbData range"),
        ));
    }
    // 直通：拆分借用 rgbData（只读）与目标（可写），行距扫描直绘
    // （旧路径 clone 整组 + 重排 w×h 中转，全屏调用为主热点之一）。
    // rgbData 与目标为同一数组（理论边角）时回退重排路径。
    let (fx, fy) = (x + g.trans.0, y + g.trans.1);
    match if fastblit_disabled() {
        Err(KvmError::Internal("fastblit disabled".into()))
    } else {
        heap.int_arrays_src_dst(array, g.target)
    } {
        Ok((src, dst)) => {
            raster::blit_scan(
                dst,
                g.target_w,
                g.clip,
                src,
                offset as usize,
                scan as usize,
                w,
                h,
                fx,
                fy,
                !process_alpha,
            );
        }
        Err(_) => {
            let source: Vec<i32> = match &heap.array(array)?.data {
                ArrayData::Int(data) => data.clone(),
                _ => return Err(illegal_arg(cs, heap, "rgbData must be int[]")),
            };
            let mut pixels = Vec::with_capacity((w * h) as usize);
            for row in 0..h {
                let base = (offset + scan * row) as usize;
                for col in 0..w {
                    let raw = source[base + col as usize];
                    pixels.push(if process_alpha { raw } else { OPAQUE | (raw & 0xFF_FFFF) });
                }
            }
            let buf = target_buf(heap, g.target)?;
            raster::blit(buf, g.target_w, g.clip, &pixels, w, h, fx, fy);
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// 文本绘制（Host 轨：度量/锚点先行，render_text blit 收尾）
// ---------------------------------------------------------------------------

/// 文本绘制公共路径：锚点修正（Font 度量）→ 目标数组借用 → 宿主 blit。
fn render_chars(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    this: ObjRef,
    chars: &[u16],
    x: i32,
    y: i32,
    anchor: i32,
) -> Result<(), KvmError> {
    // 取证面：SKYENGINE_KVM_TRACE_TEXT=1 打印绘制文本（文字消失排查：
    // 确认标签是否走宿主字体轨、绘制范围是否被 clip/坐标推出屏）。
    if std::env::var_os("SKYENGINE_KVM_TRACE_TEXT").is_some() {
        let text = String::from_utf16_lossy(chars);
        eprintln!("[kvm-text] ({x},{y}) anchor={anchor} len={} {text:?}", chars.len());
    }
    let g = snapshot(cs, heap, this)?;
    let handle = font::handle_of(cs, heap, g.font)?;
    // 先算后借：host 度量在此阶段完成，数组借用期间只走 render_text。
    let metrics = host.font_metrics(handle);
    let width = host.text_width(handle, chars) as i32;
    let (dx, dy) = anchor_delta(anchor, width, metrics.height as i32, metrics.ascent as i32, true)
        .map_err(|e| illegal_arg(cs, heap, e))?;
    let buf = target_buf(heap, g.target)?;
    host.render_text(
        handle,
        chars,
        g.color as u32,
        x - dx + g.trans.0,
        y - dy + g.trans.1,
        buf,
        g.target_w.max(0) as u16,
        g.clip,
    );
    Ok(())
}

/// `drawString(LString;III)V`：null 抛 NPE。
pub fn draw_string(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let text = arg_ref(call, 0);
    if text == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let (x, y, anchor) = (arg_int(call, 1), arg_int(call, 2), arg_int(call, 3));
    let chars = heap.str_obj(text)?.chars.clone();
    render_chars(cs, heap, host, this, &chars, x, y, anchor)?;
    Ok(None)
}

/// `drawSubstring(LString;IIIII)V`：越界抛 StringIndexOutOfBoundsException。
pub fn draw_substring(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let text = arg_ref(call, 0);
    if text == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let (offset, len, x, y, anchor) = (
        arg_int(call, 1),
        arg_int(call, 2),
        arg_int(call, 3),
        arg_int(call, 4),
        arg_int(call, 5),
    );
    let chars = heap.str_obj(text)?.chars.clone();
    if offset < 0 || len < 0 || offset + len > chars.len() as i32 {
        return Err(jl::throw_java(
            cs,
            heap,
            "java/lang/StringIndexOutOfBoundsException",
            Some("substring range"),
        ));
    }
    render_chars(
        cs,
        heap,
        host,
        this,
        &chars[offset as usize..(offset + len) as usize],
        x,
        y,
        anchor,
    )?;
    Ok(None)
}

/// `drawChar(CIII)V`。
pub fn draw_char(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let (ch, x, y, anchor) = (arg_int(call, 0), arg_int(call, 1), arg_int(call, 2), arg_int(call, 3));
    render_chars(cs, heap, host, this, &[ch as u16], x, y, anchor)?;
    Ok(None)
}

/// `drawChars([CIIIII)V`：越界抛 ArrayIndexOutOfBoundsException。
pub fn draw_chars(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let array = arg_ref(call, 0);
    if array == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let (offset, len, x, y, anchor) = (
        arg_int(call, 1),
        arg_int(call, 2),
        arg_int(call, 3),
        arg_int(call, 4),
        arg_int(call, 5),
    );
    let chars: Vec<u16> = match &heap.array(array)?.data {
        ArrayData::Char(data) => {
            if offset < 0 || len < 0 || offset + len > data.len() as i32 {
                return Err(jl::throw_java(
                    cs,
                    heap,
                    "java/lang/ArrayIndexOutOfBoundsException",
                    Some("chars range"),
                ));
            }
            data[offset as usize..(offset + len) as usize].to_vec()
        }
        _ => return Err(illegal_arg(cs, heap, "data must be char[]")),
    };
    render_chars(cs, heap, host, this, &chars, x, y, anchor)?;
    Ok(None)
}

/// `setFont(LFont;)V`：null 表示恢复默认字体（MIDP javadoc）。
pub fn set_font(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = this_ref(call);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let font_ref = match call.args[0] {
        Slot::Ref(r) if r != ObjRef::NULL => r,
        _ => font::default_font(cs, heap, host)?,
    };
    let s = graphics_slots(cs)?;
    heap.instance_mut(this)?.fields[s.font] = Slot::Ref(font_ref);
    Ok(None)
}
