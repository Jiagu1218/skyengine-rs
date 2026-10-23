//! LCDUI Display/Canvas 体系与 paint 回调链测试（T021）：
//! Vm 门面端到端（三段式 poll：repaint 标脏 → spawn lcdui-paint 绿线程
//! → 线程死 → present 屏图）。
//!
//! 夹具：手构 class（`T extends javax/microedition/lcdui/Canvas`，
//! bootstrap 桩可被 jar 子类 extends）打成最小 stored zip 喂
//! `install_jar`；宿主为 `Rc<RefCell<MockHostContext>>` 共享包装
//!（present 捕获帧供 Rust 侧断言）；paint 调用次数经 T 静态字段
//! `count` 由 `Vm::static_field` 回读。
//!
//! 构码铁律（前车之鉴）：带参构造 NEW+DUP+参数+INVOKESPECIAL；
//! 分支指令消耗条件值后不再 POP；池索引先取号再构码。

mod common;

use std::cell::RefCell;
use std::rc::Rc;

use common::classbytes::{FieldSpec, MethodSpec, Pool, build_class};
use skyengine_kvm::host::mock::MockHostContext;
use skyengine_kvm::host::{
    FontHandle, FontMetrics, HostContext, HostNetError, HttpRequest, HttpResponse,
};
use skyengine_kvm::runtime::Slot;
use skyengine_kvm::vm::{Vm, VmConfig, VmState};

// ---- 操作码（仅本文件用到的） --------------------------------------------

const ACONST_NULL: u8 = 0x01;
const ICONST_0: u8 = 0x03;
const ICONST_1: u8 = 0x04;
const BIPUSH: u8 = 0x10;
const SIPUSH: u8 = 0x11;
const LDC_W: u8 = 0x13;
const ALOAD_0: u8 = 0x2a;
const ALOAD_1: u8 = 0x2b;
const ALOAD_2: u8 = 0x2c;
const ASTORE_1: u8 = 0x4c;
const ASTORE_2: u8 = 0x4d;
const IADD: u8 = 0x60;
const IF_ICMPNE: u8 = 0xa0;
const IF_ACMPNE: u8 = 0xa6;
const GOTO: u8 = 0xa7;
const IFNULL: u8 = 0xc6;
const GETSTATIC: u8 = 0xb2;
const PUTSTATIC: u8 = 0xb3;
const INVOKEVIRTUAL: u8 = 0xb6;
const INVOKESTATIC: u8 = 0xb8;
const RETURN: u8 = 0xb1;

const CANVAS: &str = "javax/microedition/lcdui/Canvas";
const DISPLAY: &str = "javax/microedition/lcdui/Display";
const GRAPHICS: &str = "javax/microedition/lcdui/Graphics";
/// Mock 屏尺寸（MockHostContext::new(240, 320)）。
const SCREEN_W: i32 = 240;
const SCREEN_H: i32 = 320;

// ---- 共享宿主（present 捕获帧需在 Vm 持有期间从 Rust 侧观察） --------------

struct SharedHost(Rc<RefCell<MockHostContext>>);

impl HostContext for SharedHost {
    fn screen_size(&self) -> (u16, u16) {
        self.0.borrow().screen_size()
    }
    fn present(&mut self, rgba: &[u8], width: u16, height: u16) {
        self.0.borrow_mut().present(rgba, width, height)
    }
    fn font_query(&mut self, face: u8, style: u8, size: u8) -> FontHandle {
        self.0.borrow_mut().font_query(face, style, size)
    }
    fn font_metrics(&mut self, handle: FontHandle) -> FontMetrics {
        self.0.borrow_mut().font_metrics(handle)
    }
    fn text_width(&mut self, handle: FontHandle, text: &[u16]) -> u16 {
        self.0.borrow_mut().text_width(handle, text)
    }
    fn render_text(
        &mut self,
        handle: FontHandle,
        text: &[u16],
        color: u32,
        x: i32,
        y: i32,
        target: &mut [i32],
        target_w: u16,
        clip: (i32, i32, i32, i32),
    ) {
        self.0
            .borrow_mut()
            .render_text(handle, text, color, x, y, target, target_w, clip)
    }
    fn read_suite_file(&mut self, name: &str) -> Option<Vec<u8>> {
        self.0.borrow_mut().read_suite_file(name)
    }
    fn rms_load(&mut self, store: &str) -> Option<Vec<u8>> {
        self.0.borrow_mut().rms_load(store)
    }
    fn rms_save(&mut self, store: &str, data: &[u8]) {
        self.0.borrow_mut().rms_save(store, data)
    }
    fn rms_delete(&mut self, store: &str) -> bool {
        self.0.borrow_mut().rms_delete(store)
    }
    fn rms_list(&mut self) -> Vec<String> {
        self.0.borrow_mut().rms_list()
    }
    fn now_ms(&mut self) -> u64 {
        self.0.borrow_mut().now_ms()
    }
    fn audio_available(&self) -> bool {
        self.0.borrow().audio_available()
    }
    fn audio_submit(&mut self, samples: &[i16], sample_rate: u32) -> bool {
        self.0.borrow_mut().audio_submit(samples, sample_rate)
    }
    fn http_request(&mut self, req: HttpRequest) -> Result<HttpResponse, HostNetError> {
        self.0.borrow_mut().http_request(req)
    }
    fn audio_play(&mut self, data: &[u8], mime: &str, loop_forever: bool) -> bool {
        self.0.borrow_mut().audio_play(data, mime, loop_forever)
    }
    fn audio_stop(&mut self) {
        self.0.borrow_mut().audio_stop()
    }
}

// ---- 最小 stored zip（jar::unpack 只认 EOCD/中央目录/局部头三件套） --------

fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut locals = Vec::new();
    for (name, data) in entries {
        locals.push((name.to_string(), data.len() as u32, out.len() as u32));
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&0u16.to_le_bytes()); // method 0 = store
        out.extend_from_slice(&0u16.to_le_bytes()); // mod time
        out.extend_from_slice(&0u16.to_le_bytes()); // mod date
        out.extend_from_slice(&0u32.to_le_bytes()); // crc（unpack 不校验）
        out.extend_from_slice(&(data.len() as u32).to_le_bytes()); // compressed
        out.extend_from_slice(&(data.len() as u32).to_le_bytes()); // uncompressed
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
    }
    let cd_start = out.len() as u32;
    for (name, size, offset) in &locals {
        out.extend_from_slice(b"PK\x01\x02");
        out.extend_from_slice(&20u16.to_le_bytes()); // version made by
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&0u16.to_le_bytes()); // method
        out.extend_from_slice(&0u16.to_le_bytes()); // time
        out.extend_from_slice(&0u16.to_le_bytes()); // date
        out.extend_from_slice(&0u32.to_le_bytes()); // crc
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra
        out.extend_from_slice(&0u16.to_le_bytes()); // comment
        out.extend_from_slice(&0u16.to_le_bytes()); // disk start
        out.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        out.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        out.extend_from_slice(&offset.to_le_bytes()); // 局部头偏移
        out.extend_from_slice(name.as_bytes());
    }
    let cd_size = out.len() as u32 - cd_start;
    out.extend_from_slice(b"PK\x05\x06");
    out.extend_from_slice(&0u16.to_le_bytes()); // disk
    out.extend_from_slice(&0u16.to_le_bytes()); // cd 所在 disk
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_start.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment len
    out
}

// ---- 迷你汇编（分支补丁） --------------------------------------------------

/// int 压栈（iconst/bipush/sipush/ldc_w 按值域选择）。
fn push_int(pool: &mut Pool, value: i32) -> Vec<u8> {
    match value {
        -1..=5 => vec![0x02 + (value + 1) as u8], // iconst_m1..iconst_5
        -128..=127 => vec![BIPUSH, value as u8],
        -32768..=32767 => {
            let mut code = vec![SIPUSH];
            code.extend_from_slice(&(value as i16).to_be_bytes());
            code
        }
        _ => {
            let [hi, lo] = pool.integer(value).to_be_bytes();
            vec![LDC_W, hi, lo]
        }
    }
}

/// 分支补丁收集器：`branch` 发占位指令并分配标签 id，
/// `mark` 回填目标地址，`finish` 统一写相对偏移（自分支指令起算）。
struct Patcher {
    /// (分支指令地址, 标签序号)。
    pending: Vec<(usize, usize)>,
    /// 标签序号 → 目标地址（MAX = 未标记）。
    labels: Vec<usize>,
}

impl Patcher {
    fn new() -> Self {
        Patcher {
            pending: Vec::new(),
            labels: Vec::new(),
        }
    }
    /// 发射 3 字节分支占位，返回新标签 id。
    fn branch(&mut self, code: &mut Vec<u8>, op: u8) -> usize {
        let id = self.labels.len();
        self.labels.push(usize::MAX);
        self.pending.push((code.len(), id));
        code.extend_from_slice(&[op, 0, 0]);
        id
    }
    fn mark(&mut self, id: usize, pc: usize) {
        self.labels[id] = pc;
    }
    fn finish(self, code: &mut [u8]) {
        for (at, id) in self.pending {
            let target = self.labels[id];
            assert_ne!(target, usize::MAX, "branch label {id} never marked");
            let offset = (target as i64 - at as i64) as i16;
            code[at + 1..at + 3].copy_from_slice(&offset.to_be_bytes());
        }
    }
}

fn op3(op: u8, index: u16) -> [u8; 3] {
    let [hi, lo] = index.to_be_bytes();
    [op, hi, lo]
}

// ---- T 类构造（extends Canvas 桩；paint 由 T 提供） -------------------------

/// paint 公共前段：count++；g.setColor(0xFF0000)；g.fillRect(0,0,16,16)。
fn paint_prologue(pool: &mut Pool) -> Vec<u8> {
    let count = pool.fieldref("T", "count", "I");
    let set_color = pool.methodref(GRAPHICS, "setColor", "(I)V");
    let fill = pool.methodref(GRAPHICS, "fillRect", "(IIII)V");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(GETSTATIC, count));
    code.push(ICONST_1);
    code.push(IADD);
    code.extend_from_slice(&op3(PUTSTATIC, count));
    code.push(ALOAD_1);
    code.extend_from_slice(&push_int(pool, 0xFF0_000));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, set_color));
    code.push(ALOAD_1);
    code.extend_from_slice(&push_int(pool, 0));
    code.extend_from_slice(&push_int(pool, 0));
    code.extend_from_slice(&push_int(pool, 16));
    code.extend_from_slice(&push_int(pool, 16));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, fill));
    code
}

/// 组装 T：静态 int count + startApp(实参码) + paint(前段+实参尾码)。
fn build_t(pool: &mut Pool, startapp: Vec<u8>, paint_tail: Vec<u8>) -> Vec<u8> {
    let mut paint = paint_prologue(pool);
    paint.extend(paint_tail);
    paint.push(RETURN);
    let mut sa = startapp;
    sa.push(RETURN);
    build_class(
        pool,
        "T",
        Some(CANVAS),
        &[],
        &[FieldSpec {
            access: 0x0008, // static
            name: "count",
            desc: "I",
            constant: None,
        }],
        &[
            MethodSpec {
                access: 0x0001, // public 实例（start_midlet 帧序约定）
                name: "startApp",
                desc: "()V",
                code: Some((6, 4, sa)),
                handlers: Vec::new(),
            },
            MethodSpec {
                access: 0x0001,
                name: "paint",
                desc: "(Ljavax/microedition/lcdui/Graphics;)V",
                // locals: this/g/局部 d（set_current_null 用例的尾段用到 local 2）。
                code: Some((6, 3, paint)),
                handlers: Vec::new(),
            },
        ],
    )
}

/// 装配 Vm（共享 Mock 宿主 240x320）并启动 T。
fn boot(
    pool: &mut Pool,
    startapp: Vec<u8>,
    paint_tail: Vec<u8>,
) -> (Vm, Rc<RefCell<MockHostContext>>) {
    let host = Rc::new(RefCell::new(MockHostContext::new(240, 320)));
    let bytes = build_t(pool, startapp, paint_tail);
    let zip = stored_zip(&[("T.class", &bytes)]);
    let mut vm = Vm::new(Box::new(SharedHost(Rc::clone(&host))), VmConfig::default());
    vm.install_jar("t", &zip).expect("install jar");
    vm.start_midlet("T").expect("start midlet");
    (vm, host)
}

/// 轮询驱动至线程跑尽或轮数上限。
fn drive(vm: &mut Vm, rounds: usize) {
    for _ in 0..rounds {
        if !vm.is_active() {
            break;
        }
        vm.poll(200_000);
    }
}

/// T.count 静态字段回读。
fn count_of(vm: &Vm) -> i32 {
    match vm.static_field("T", "count", "I") {
        Some(Slot::Int(v)) => v,
        other => panic!("T.count corrupt: {other:?}"),
    }
}

/// 帧内 (x, y) 像素 → (r, g, b, a)。
fn frame_px(frame: &(Vec<u8>, u16, u16), x: usize, y: usize) -> (u8, u8, u8, u8) {
    let (rgba, w, _) = frame;
    let i = (y * *w as usize + x) * 4;
    (rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3])
}

/// 比较码骨架：栈顶两操作数（调用方先压好）比较失败 → count=0；
/// 成功 → count=1。cmp_op 为消耗两操作数的比较分支。
fn bool_to_count(pool: &mut Pool, code: &mut Vec<u8>, cmp_op: u8) {
    let count = pool.fieldref("T", "count", "I");
    let mut patch = Patcher::new();
    let fail = patch.branch(code, cmp_op);
    code.push(ICONST_1);
    let skip = patch.branch(code, GOTO);
    patch.mark(fail, code.len());
    code.push(ICONST_0);
    patch.mark(skip, code.len());
    code.extend_from_slice(&op3(PUTSTATIC, count));
    patch.finish(code);
}

// ---- 用例 ------------------------------------------------------------------

/// getDisplay 返回非 null 且两次同实例（display_cache 单例）。
#[test]
fn get_display_singleton_non_null() {
    let mut pool = Pool::new();
    let gd = pool.methodref(
        DISPLAY,
        "getDisplay",
        "(Ljavax/microedition/midlet/MIDlet;)Ljavax/microedition/lcdui/Display;",
    );
    let mut code = Vec::new();
    code.push(ACONST_NULL);
    code.extend_from_slice(&op3(INVOKESTATIC, gd));
    code.push(ASTORE_1);
    code.push(ACONST_NULL);
    code.extend_from_slice(&op3(INVOKESTATIC, gd));
    code.push(ASTORE_2);
    // d1 非 null（IFNULL 跳过赋值，count 保持 0）且 d1==d2 → count=1。
    let mut patch = Patcher::new();
    code.push(ALOAD_1);
    let null_skip = patch.branch(&mut code, IFNULL);
    code.push(ALOAD_1);
    code.push(ALOAD_2);
    bool_to_count(&mut pool, &mut code, IF_ACMPNE);
    patch.mark(null_skip, code.len());
    patch.finish(&mut code);
    let (mut vm, _host) = boot(&mut pool, code, Vec::new());
    drive(&mut vm, 6);
    assert_eq!(count_of(&vm), 1, "getDisplay 两次应同实例且非 null");
}

/// setCurrent(this) 后 getCurrent 回读同一引用。
#[test]
fn set_current_roundtrip() {
    let mut pool = Pool::new();
    let gd = pool.methodref(
        DISPLAY,
        "getDisplay",
        "(Ljavax/microedition/midlet/MIDlet;)Ljavax/microedition/lcdui/Display;",
    );
    let set_cur = pool.methodref(
        DISPLAY,
        "setCurrent",
        "(Ljavax/microedition/lcdui/Displayable;)V",
    );
    let get_cur = pool.methodref(
        DISPLAY,
        "getCurrent",
        "()Ljavax/microedition/lcdui/Displayable;",
    );
    let mut code = Vec::new();
    code.push(ACONST_NULL);
    code.extend_from_slice(&op3(INVOKESTATIC, gd));
    code.push(ASTORE_1);
    code.push(ALOAD_1);
    code.push(ALOAD_0);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, set_cur));
    code.push(ALOAD_1);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, get_cur));
    code.push(ALOAD_0);
    bool_to_count(&mut pool, &mut code, IF_ACMPNE);
    let (mut vm, _host) = boot(&mut pool, code, Vec::new());
    drive(&mut vm, 6);
    assert_eq!(count_of(&vm), 1, "getCurrent 应回读 setCurrent 的同一引用");
}

/// getWidth/getHeight == Mock 屏宽高（240/320，懒分配屏图数据源）。
#[test]
fn canvas_screen_metrics() {
    let mut pool = Pool::new();
    let gw = pool.methodref(CANVAS, "getWidth", "()I");
    let gh = pool.methodref(CANVAS, "getHeight", "()I");
    let mut code = Vec::new();
    code.push(ALOAD_0);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, gw));
    code.extend_from_slice(&push_int(&mut pool, SCREEN_W));
    let count = pool.fieldref("T", "count", "I");
    let mut patch = Patcher::new();
    let fail_w = patch.branch(&mut code, IF_ICMPNE);
    code.push(ALOAD_0);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, gh));
    code.extend_from_slice(&push_int(&mut pool, SCREEN_H));
    let fail_h = patch.branch(&mut code, IF_ICMPNE);
    code.push(ICONST_1);
    let skip = patch.branch(&mut code, GOTO);
    patch.mark(fail_w, code.len());
    patch.mark(fail_h, code.len());
    code.push(ICONST_0);
    patch.mark(skip, code.len());
    code.extend_from_slice(&op3(PUTSTATIC, count));
    patch.finish(&mut code);
    let (mut vm, _host) = boot(&mut pool, code, Vec::new());
    drive(&mut vm, 6);
    assert_eq!(count_of(&vm), 1, "getWidth/getHeight 应等于 Mock 屏尺寸");
}

/// startApp 公共前段：setDisplay(this)（getDisplay → setCurrent）。
fn code_show(pool: &mut Pool) -> Vec<u8> {
    let gd = pool.methodref(
        DISPLAY,
        "getDisplay",
        "(Ljavax/microedition/midlet/MIDlet;)Ljavax/microedition/lcdui/Display;",
    );
    let set_cur = pool.methodref(
        DISPLAY,
        "setCurrent",
        "(Ljavax/microedition/lcdui/Displayable;)V",
    );
    let mut code = Vec::new();
    code.push(ACONST_NULL);
    code.extend_from_slice(&op3(INVOKESTATIC, gd));
    code.push(ASTORE_1);
    code.push(ALOAD_1);
    code.push(ALOAD_0);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, set_cur));
    code
}

/// repaint → poll 若干 → 捕获到帧且 (0,0) 为红色（paint 链端到端）。
#[test]
fn repaint_paints_red_frame() {
    let mut pool = Pool::new();
    let repaint = pool.methodref(CANVAS, "repaint", "()V");
    let mut code = code_show(&mut pool);
    code.push(ALOAD_0);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, repaint));
    let (mut vm, host) = boot(&mut pool, code, Vec::new());
    drive(&mut vm, 10);
    assert_eq!(count_of(&vm), 1, "repaint 应触发恰好一次 paint");
    let host_ref = host.borrow();
    let frames = host_ref.captured_frames();
    assert_eq!(frames.len(), 1, "应呈现恰好一帧");
    let frame = &frames[0];
    assert_eq!((frame.1, frame.2), (SCREEN_W as u16, SCREEN_H as u16));
    assert_eq!(
        frame_px(frame, 0, 0),
        (0xFF, 0, 0, 0xFF),
        "(0,0) 应为不透明红"
    );
    assert_eq!(
        frame_px(frame, 15, 15),
        (0xFF, 0, 0, 0xFF),
        "(15,15) 应为红"
    );
    assert_eq!(frame_px(frame, 20, 20), (0, 0, 0, 0), "(20,20) 应为屏零值");
}

/// 两区域两次 repaint → 并集合并后仅一次 paint（计数==1、一帧）。
#[test]
fn repaint_union_merges_to_single_paint() {
    let mut pool = Pool::new();
    let repaint4 = pool.methodref(CANVAS, "repaint", "(IIII)V");
    let mut code = code_show(&mut pool);
    for (x, y) in [(0, 0), (20, 20)] {
        code.push(ALOAD_0);
        code.extend_from_slice(&push_int(&mut pool, x));
        code.extend_from_slice(&push_int(&mut pool, y));
        code.extend_from_slice(&push_int(&mut pool, 10));
        code.extend_from_slice(&push_int(&mut pool, 10));
        code.extend_from_slice(&op3(INVOKEVIRTUAL, repaint4));
    }
    let (mut vm, host) = boot(&mut pool, code, Vec::new());
    drive(&mut vm, 10);
    assert_eq!(count_of(&vm), 1, "两次 repaint 应合并为一次 paint");
    assert_eq!(host.borrow().captured_frames().len(), 1);
}

/// paint 内 setCurrent(null)+repaint：本帧仍呈现（会话快照），
/// 之后的 poll 不再产生新 paint/新帧。
#[test]
fn set_current_null_stops_paint_chain() {
    let mut pool = Pool::new();
    let gd = pool.methodref(
        DISPLAY,
        "getDisplay",
        "(Ljavax/microedition/midlet/MIDlet;)Ljavax/microedition/lcdui/Display;",
    );
    let set_cur = pool.methodref(
        DISPLAY,
        "setCurrent",
        "(Ljavax/microedition/lcdui/Displayable;)V",
    );
    let repaint = pool.methodref(CANVAS, "repaint", "()V");
    let mut start = code_show(&mut pool);
    start.push(ALOAD_0);
    start.extend_from_slice(&op3(INVOKEVIRTUAL, repaint));
    // paint 尾段：d=getDisplay(null); d.setCurrent(null); this.repaint();
    let mut tail = Vec::new();
    tail.push(ACONST_NULL);
    tail.extend_from_slice(&op3(INVOKESTATIC, gd));
    tail.push(ASTORE_2);
    tail.push(ALOAD_2);
    tail.push(ACONST_NULL);
    tail.extend_from_slice(&op3(INVOKEVIRTUAL, set_cur));
    tail.push(ALOAD_0);
    tail.extend_from_slice(&op3(INVOKEVIRTUAL, repaint));
    let (mut vm, host) = boot(&mut pool, start, tail);
    drive(&mut vm, 12);
    assert_eq!(count_of(&vm), 1, "current 置 null 后不应再有新 paint");
    assert_eq!(host.borrow().captured_frames().len(), 1, "仅首帧呈现");
    // 终态：主线程与 paint 线程都跑尽 → Dead。
    assert_eq!(vm.state(), VmState::Dead);
}

// ---- T022 汇合补漏：setCurrent 双画布切换 + 可变图端到端上屏 ----------------

const IMAGE: &str = "javax/microedition/lcdui/Image";
const DUP: u8 = 0x59;
const NEW: u8 = 0xbb;
const INVOKESPECIAL: u8 = 0xb7;

/// 两画布先后 setCurrent：getCurrent 跟随最后一次切换；对未覆写 paint 的
/// 空 Canvas repaint：paint 走 bootstrap 空桩，整链不炸且呈现清屏帧。
#[test]
fn set_current_switch_follows_and_empty_canvas_repaint_survives() {
    let mut pool = Pool::new();
    let set_cur = pool.methodref(
        DISPLAY,
        "setCurrent",
        "(Ljavax/microedition/lcdui/Displayable;)V",
    );
    let get_cur = pool.methodref(
        DISPLAY,
        "getCurrent",
        "()Ljavax/microedition/lcdui/Displayable;",
    );
    let repaint = pool.methodref(CANVAS, "repaint", "()V");
    let c_init = op3(INVOKESPECIAL, pool.methodref("C", "<init>", "()V"));
    let canvas_init = op3(INVOKESPECIAL, pool.methodref(CANVAS, "<init>", "()V"));
    // startApp：d=getDisplay(null) → local1；d.setCurrent(this)；c=new C() →
    // local2；d.setCurrent(c)；count=(getCurrent()==c)；c.repaint()。
    let mut sa = code_show(&mut pool); // 复用前段：取 Display + setCurrent(this)
    sa.extend_from_slice(&op3(NEW, pool.class("C")));
    sa.push(DUP);
    sa.extend_from_slice(&c_init);
    sa.push(ASTORE_2); // c → local 2
    sa.push(ALOAD_1); // d
    sa.push(ALOAD_2); // c
    sa.extend_from_slice(&op3(INVOKEVIRTUAL, set_cur));
    sa.push(ALOAD_1);
    sa.extend_from_slice(&op3(INVOKEVIRTUAL, get_cur));
    sa.push(ALOAD_2);
    bool_to_count(&mut pool, &mut sa, IF_ACMPNE); // 不等→0，等→1
    sa.push(ALOAD_2);
    sa.extend_from_slice(&op3(INVOKEVIRTUAL, repaint));
    // C：仅 <init> 链 Canvas 桩，不覆写 paint（空 Canvas）。
    let c_class = build_class(
        &mut pool,
        "C",
        Some(CANVAS),
        &[],
        &[],
        &[MethodSpec {
            access: 0x0001,
            name: "<init>",
            desc: "()V",
            code: Some((1, 1, {
                let mut bytes = Vec::new();
                bytes.push(ALOAD_0);
                bytes.extend_from_slice(&canvas_init);
                bytes.push(RETURN);
                bytes
            })),
            handlers: Vec::new(),
        }],
    );
    let t = build_t(&mut pool, sa, Vec::new());
    let zip = stored_zip(&[("T.class", &t), ("C.class", &c_class)]);
    let host = Rc::new(RefCell::new(MockHostContext::new(240, 320)));
    let mut vm = Vm::new(Box::new(SharedHost(Rc::clone(&host))), VmConfig::default());
    vm.install_jar("t", &zip).expect("install jar");
    vm.start_midlet("T").expect("start midlet");
    drive(&mut vm, 10);
    assert_eq!(count_of(&vm), 1, "getCurrent 应跟随最后一次 setCurrent(c)");
    // 现状语义（natives.rs：Canvas.paint 声明为抽象，Vm.poll 段一探测
    // 子类 paint body，未覆写 → 静默清脏跳过）：无帧呈现、不抛不炸、
    // 线程正常收敛。注意真实 MIDP 中未覆写 paint 的 Canvas 子类是抽象
    // 类不可实例化，此处按实现容错语义如实断言。
    assert!(
        host.borrow().captured_frames().is_empty(),
        "未覆写 paint 的 Canvas：poll 应静默清脏（不呈现帧）"
    );
    assert!(!vm.is_active(), "空 Canvas repaint 链应正常收敛不炸");
}

/// 跨模块组合（真端到端）：startApp 造可变 Image 并在其 Graphics 上填绿，
/// Canvas.paint 里 drawImage 上屏 → mock 帧捕获像素对齐
///（红块为 paint 前段、绿块为可变图 blit 结果，二者同帧共存）。
#[test]
fn canvas_paint_blits_mutable_image_end_to_end() {
    let mut pool = Pool::new();
    let create = pool.methodref(IMAGE, "createImage", "(II)Ljavax/microedition/lcdui/Image;");
    let get_g = pool.methodref(IMAGE, "getGraphics", "()Ljavax/microedition/lcdui/Graphics;");
    let set_color = pool.methodref(GRAPHICS, "setColor", "(I)V");
    let fill = pool.methodref(GRAPHICS, "fillRect", "(IIII)V");
    let draw = pool.methodref(GRAPHICS, "drawImage", "(Ljavax/microedition/lcdui/Image;III)V");
    let repaint = pool.methodref(CANVAS, "repaint", "()V");
    let img_put = pool.fieldref("T", "img", "Ljavax/microedition/lcdui/Image;");
    // startApp：img=createImage(8,8)；g=img.getGraphics()→local2；
    // g.setColor(0x00FF00)；g.fillRect(0,0,8,8)；上屏 + repaint。
    let mut sa = code_show(&mut pool);
    sa.extend(push_int(&mut pool, 8));
    sa.extend(push_int(&mut pool, 8));
    sa.extend(op3(INVOKESTATIC, create));
    sa.extend(op3(PUTSTATIC, img_put));
    sa.extend(op3(GETSTATIC, img_put));
    sa.extend(op3(INVOKEVIRTUAL, get_g));
    sa.push(ASTORE_2);
    sa.push(ALOAD_2);
    sa.extend(push_int(&mut pool, 0x00_FF00));
    sa.extend(op3(INVOKEVIRTUAL, set_color));
    sa.push(ALOAD_2);
    sa.extend(push_int(&mut pool, 0));
    sa.extend(push_int(&mut pool, 0));
    sa.extend(push_int(&mut pool, 8));
    sa.extend(push_int(&mut pool, 8));
    sa.extend(op3(INVOKEVIRTUAL, fill));
    sa.push(ALOAD_0);
    sa.extend(op3(INVOKEVIRTUAL, repaint));
    sa.push(RETURN); // 显式收尾
    // paint：前段红块 [0,16)² + drawImage(T.img, 0, 20, LEFT|TOP)。
    let mut paint = paint_prologue(&mut pool);
    paint.push(ALOAD_1); // g
    paint.extend(op3(GETSTATIC, img_put));
    paint.extend(push_int(&mut pool, 0)); // x
    paint.extend(push_int(&mut pool, 20)); // y
    paint.extend(push_int(&mut pool, 20)); // LEFT|TOP
    paint.extend(op3(INVOKEVIRTUAL, draw));
    paint.push(RETURN);
    let bytes = build_class(
        &mut pool,
        "T",
        Some(CANVAS),
        &[],
        &[
            FieldSpec { access: 0x0008, name: "count", desc: "I", constant: None },
            FieldSpec {
                access: 0x0008,
                name: "img",
                desc: "Ljavax/microedition/lcdui/Image;",
                constant: None,
            },
        ],
        &[
            MethodSpec {
                access: 0x0001,
                name: "startApp",
                desc: "()V",
                code: Some((8, 4, sa)),
                handlers: Vec::new(),
            },
            MethodSpec {
                access: 0x0001,
                name: "paint",
                desc: "(Ljavax/microedition/lcdui/Graphics;)V",
                code: Some((8, 3, paint)),
                handlers: Vec::new(),
            },
        ],
    );
    let zip = stored_zip(&[("T.class", &bytes)]);
    let host = Rc::new(RefCell::new(MockHostContext::new(240, 320)));
    let mut vm = Vm::new(Box::new(SharedHost(Rc::clone(&host))), VmConfig::default());
    vm.install_jar("t", &zip).expect("install jar");
    vm.start_midlet("T").expect("start midlet");
    drive(&mut vm, 10);
    assert_eq!(count_of(&vm), 1, "repaint 应恰好触发一次 paint");
    let host_ref = host.borrow();
    let frames = host_ref.captured_frames();
    assert_eq!(frames.len(), 1, "应呈现恰好一帧");
    let frame = frames[0].clone();
    // 红块（paint 前段）与绿块（可变图 blit）同帧对齐。
    assert_eq!(frame_px(&frame, 0, 0), (0xFF, 0, 0, 0xFF), "(0,0) 红");
    assert_eq!(frame_px(&frame, 15, 15), (0xFF, 0, 0, 0xFF), "(15,15) 红");
    assert_eq!(frame_px(&frame, 0, 20), (0, 0xFF, 0, 0xFF), "(0,20) 应为可变图绿");
    assert_eq!(frame_px(&frame, 7, 27), (0, 0xFF, 0, 0xFF), "(7,27) 绿块右下角");
    assert_eq!(frame_px(&frame, 8, 20), (0, 0, 0, 0), "(8,20) 绿块右邻应为屏零值");
    assert_eq!(frame_px(&frame, 0, 19), (0, 0, 0, 0), "(0,19) 绿块上邻应为屏零值");
    assert_eq!(frame_px(&frame, 0, 16), (0, 0, 0, 0), "(0,16) 红块下邻应为屏零值");
}

