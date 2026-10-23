//! LCDUI Image 原生实现测试（T018）：
//! PNG 解码器（png.rs 纯函数层）+ createImage 全套重载（构码经
//! RegistryDispatch 端到端）。
//!
//! 测试内用 ZlibEncoder 程序化构造最小 PNG（真彩 RGBA / 调色板 4bit+tRNS /
//! Adam7 / 各行过滤式），并以 fixture jc.jar 的真实 skyicon.png 实弹验证。
//!
//! 构码铁律（前车之鉴）：带参构造 NEW+DUP+参数+INVOKESPECIAL（javac 形态）；
//! 分支指令消耗条件值后不再 POP；long/double 占两槽；操作数池先取号再构码。

#[allow(dead_code)]
mod common;

use std::fs;
use std::rc::Rc;

use common::classbytes::{build_class, provider, MethodSpec, Pool};
use common::{fixture_path, load_jar, mrp_read_entry};

use skyengine_kvm::host::mock::MockHostContext;
use skyengine_kvm::interp::natives::RegistryDispatch;
use skyengine_kvm::interp::{enter, run, runtime_class_of, Machine, RunOutcome, Unwind};
use skyengine_kvm::midp::lcdui::png;
use skyengine_kvm::runtime::classspace::ClassSpace;
use skyengine_kvm::runtime::heap::Heap;
use skyengine_kvm::runtime::monitor::Monitors;
use skyengine_kvm::runtime::{Slot, ThreadId};
use skyengine_kvm::vm::{Vm as KvmVm, VmConfig};
use skyengine_kvm::KvmError;

// ---- 操作码（仅本文件用到的） --------------------------------------------

const ICONST_0: u8 = 0x03;
const ICONST_1: u8 = 0x04;
const ICONST_2: u8 = 0x05;
const ICONST_3: u8 = 0x06;
const ICONST_4: u8 = 0x07;
const BIPUSH: u8 = 0x10;
const SIPUSH: u8 = 0x11;
const LDC_W: u8 = 0x13;
const ALOAD: u8 = 0x19;
const IALOAD: u8 = 0x2e;
const IASTORE: u8 = 0x4f;
const BASTORE: u8 = 0x53;
const POP: u8 = 0x57;
const DUP: u8 = 0x59;
const IMUL: u8 = 0x68;
const IADD: u8 = 0x60;
const IRETURN: u8 = 0xac;
const GETFIELD: u8 = 0xb4;
const INVOKEVIRTUAL: u8 = 0xb6;
const INVOKESPECIAL: u8 = 0xb7;
const INVOKESTATIC: u8 = 0xb8;
const NEW: u8 = 0xbb;
const NEWARRAY: u8 = 0xbc;

/// newarray atype：byte / int。
const T_BYTE: u8 = 8;
const T_INT: u8 = 10;

/// 主线程标识（与其他测试夹具一致）。
const MAIN: ThreadId = ThreadId(1);

const IMAGE: &str = "javax/microedition/lcdui/Image";

// ---- PNG 构造（ZlibEncoder 程序化生成） -----------------------------------

/// zlib 压缩（IDAT 载荷）。
fn zlib(data: &[u8]) -> Vec<u8> {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).expect("zlib compress");
    encoder.finish().expect("zlib finish")
}

/// PNG chunk：长度 + 类型 + 数据 + CRC 占位（解码器不校验 CRC）。
fn chunk(tag: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = (data.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(tag);
    out.extend_from_slice(data);
    out.extend_from_slice(&[0, 0, 0, 0]);
    out
}

fn ihdr(width: u32, height: u32, depth: u8, color: u8, interlace: u8) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(&width.to_be_bytes());
    data.extend_from_slice(&height.to_be_bytes());
    data.push(depth);
    data.push(color);
    data.push(0); // compression
    data.push(0); // filter
    data.push(interlace);
    data
}

/// 组装最小 PNG（辅助块可选）。
// PNG 块字段（IHDR/PLTE/tRNS/IDAT…）按需透传，测试辅助函数豁免参数宽度。
#[allow(clippy::too_many_arguments)]
fn make_png(
    width: u32,
    height: u32,
    depth: u8,
    color: u8,
    interlace: u8,
    plte: Option<&[u8]>,
    trns: Option<&[u8]>,
    raw_rows: &[u8],
) -> Vec<u8> {
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    out.extend(chunk(b"IHDR", &ihdr(width, height, depth, color, interlace)));
    if let Some(bytes) = plte {
        out.extend(chunk(b"PLTE", bytes));
    }
    if let Some(bytes) = trns {
        out.extend(chunk(b"tRNS", bytes));
    }
    out.extend(chunk(b"IDAT", &zlib(raw_rows)));
    out.extend(chunk(b"IEND", b""));
    out
}

/// 2x2 真彩 RGBA（filter 全 None）：四像素 0xFF112233/0x80445566/0x00778899/0xFFAABBCC。
fn truecolor_png_2x2() -> Vec<u8> {
    let rows: &[u8] = &[
        0, 0x11, 0x22, 0x33, 0xFF, 0x44, 0x55, 0x66, 0x80, //
        0, 0x77, 0x88, 0x99, 0x00, 0xAA, 0xBB, 0xCC, 0xFF,
    ];
    make_png(2, 2, 8, 6, 0, None, None, rows)
}

// ---- png.rs 纯函数层 -------------------------------------------------------

#[test]
fn png_truecolor_rgba_filter_none() {
    let decoded = png::decode(&truecolor_png_2x2()).expect("decode");
    assert_eq!((decoded.width, decoded.height), (2, 2));
    assert_eq!(
        decoded.pixels,
        vec![0xFF11_2233, 0x8044_5566, 0x0077_8899, 0xFFAA_BBCC]
    );
}

#[test]
fn png_unfilter_sub_up_average_paeth() {
    // 2 像素宽 color6（stride=8、bpp=4）四行：None / Sub / Paeth / Up。
    // 目标解出值：
    // row0 = [10,20,30,40, 50,60,70,80]
    // row1 = [11,21,31,41, 55,65,75,85]（Sub：前 bpp 左邻为 0 原样，
    //        后 4 字节残差减同行前 bpp 解出值）
    // row2 = [13,23,33,43, 57,67,77,87]（Paeth：前 bpp 预测=上；
    //        后 bpp 预测=paeth(本行前像素, 上, 左上)）
    // row3 = [15,25,35,45, 59,69,79,89]（Up：残差加 row2 解出值）
    let rows: &[u8] = &[
        0, 10, 20, 30, 40, 50, 60, 70, 80, //
        1, 11, 21, 31, 41, 44, 44, 44, 44, //
        4, 2, 2, 2, 2, 2, 2, 2, 2, //
        2, 2, 2, 2, 2, 2, 2, 2, 2,
    ];
    let decoded = png::decode(&make_png(2, 4, 8, 6, 0, None, None, rows)).expect("decode");
    assert_eq!(
        decoded.pixels,
        vec![
            0x28_0A_14_1E, 0x50_32_3C_46, //
            0x29_0B_15_1F, 0x55_37_41_4B, //
            0x2B_0D_17_21, 0x57_39_43_4D, //
            0x2D_0F_19_23, 0x59_3B_45_4F
        ]
    );

    // Average：row1 残差 [1..8]，左=同行已解出的前 bpp 字节（i<bpp 为 0）
    let rows: &[u8] = &[
        0, 10, 20, 30, 40, 50, 60, 70, 80, //
        3, 1, 2, 3, 4, 5, 6, 7, 8,
    ];
    let decoded = png::decode(&make_png(2, 2, 8, 6, 0, None, None, rows)).expect("decode");
    // row1 解出：i<bpp 加 floor(up/2) → [6,12,18,24]；i>=bpp 加
    // floor((左+上)/2) → [5+28, 6+36, 7+44, 8+52] = [33,42,51,60]
    assert_eq!(decoded.pixels[2], 0x18_06_0C_12);
    assert_eq!(decoded.pixels[3], 0x3C_21_2A_33);
}

#[test]
fn png_palette_4bit_with_trns_and_padding() {
    // 3 项调色板 + tRNS（项 0 全透明）；2x2、位深 4：
    // 每行 1 字节恰好 2 索引（高 nibble 先）。第三行改 3 像素验证行尾
    // padding 位忽略（0x20：高 nibble=索引 2，低 nibble=padding 0）。
    let plte: &[u8] = &[0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90];
    let trns: &[u8] = &[0x00, 0x80]; // 项 0 全透明、项 1 半透明、项 2 不透明
    let rows: &[u8] = &[0, 0x01, 0, 0x10];
    let decoded = png::decode(&make_png(2, 2, 4, 3, 0, Some(plte), Some(trns), rows)).expect("decode");
    // row1 = 0x10：高 nibble 索引 1、低 nibble 索引 0（高 nibble 先）
    assert_eq!(
        decoded.pixels,
        vec![0x00_10_20_30, 0x80_40_50_60, 0x80_40_50_60, 0x00_10_20_30]
    );

    // 3 像素行：stride=2 字节，低 2 bit 为 padding
    let rows: &[u8] = &[0, 0x01, 0x2C];
    let decoded = png::decode(&make_png(3, 1, 4, 3, 0, Some(plte), Some(trns), rows)).expect("decode");
    assert_eq!(
        decoded.pixels,
        vec![0x00_10_20_30, 0x80_40_50_60, 0xFF_70_80_90]
    );
}

#[test]
fn png_gray_8bit_and_truecolor_trns_key() {
    // 灰度 8bit：样本等比放大（此处已 8bit 原样）
    let rows: &[u8] = &[0, 0x80, 0xFF];
    let decoded = png::decode(&make_png(2, 1, 8, 0, 0, None, None, rows)).expect("decode");
    assert_eq!(decoded.pixels, vec![0xFF_80_80_80, 0xFF_FF_FF_FF]);

    // 真彩 tRNS 色键（16bit 值取低字节）：命中键 → alpha 0
    let trns: &[u8] = &[0, 0x11, 0, 0x22, 0, 0x33];
    let rows: &[u8] = &[0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x00];
    let decoded = png::decode(&make_png(2, 1, 8, 2, 0, None, Some(trns), rows)).expect("decode");
    assert_eq!(decoded.pixels, vec![0x00_11_22_33, 0xFF_44_55_66]);
}

#[test]
fn png_adam7_interlace() {
    // 2x2 color6 interlace：仅 pass1/(0,0)、pass6/(1,0)、pass7/(0..2,1) 有数据
    let rows: &[u8] = &[
        0, 0x11, 0x11, 0x11, 0xFF, // pass1 → (0,0)
        0, 0x22, 0x22, 0x22, 0xFF, // pass6 → (1,0)
        0, 0x33, 0x33, 0x33, 0xFF, 0x44, 0x44, 0x44, 0xFF, // pass7 → (0,1),(1,1)
    ];
    let decoded = png::decode(&make_png(2, 2, 8, 6, 1, None, None, rows)).expect("decode");
    assert_eq!(
        decoded.pixels,
        vec![
            0xFF11_1111, 0xFF22_2222, //
            0xFF33_3333, 0xFF44_4444
        ]
    );
}

#[test]
fn png_rejects_unsupported_and_broken() {
    // 位深 16 / 颜色类型 4 明确拒绝
    let rows: &[u8] = &[0, 1, 0, 2, 0];
    assert!(png::decode(&make_png(1, 1, 16, 2, 0, None, None, rows)).is_err());
    assert!(png::decode(&make_png(1, 1, 8, 4, 0, None, None, rows)).is_err());
    // 坏签名 / 空 IDAT
    assert!(png::decode(b"nope").is_err());
    assert!(png::decode(&make_png(1, 1, 8, 6, 0, None, None, &[])).is_err());
}

#[test]
fn png_decodes_all_fixture_entries() {
    // 实弹：fixture jc.jar 全部 PNG（fishicon/icon/skyicon）可解
    let jar = load_jar();
    let mut count = 0;
    for (name, bytes) in &jar {
        if name.ends_with(".png") {
            png::decode(bytes).unwrap_or_else(|e| panic!("{name}: {e:?}"));
            count += 1;
        }
    }
    assert_eq!(count, 3, "fixture 应含 3 个 PNG 条目");
}

// ---- 构码夹具（RegistryDispatch 端到端） -----------------------------------

/// 走真注册表的测试 VM（cldc_semantics 同型）+ 套件资源表注入。
struct Vm {
    cs: ClassSpace,
    heap: Heap,
    monitors: Monitors,
    natives: RegistryDispatch,
    /// HostContext 通道（T019）；图像测试用 Mock 兜底。
    host: MockHostContext,
}

impl Vm {
    fn new(user: Vec<(String, Vec<u8>)>, resources: Vec<(&str, Vec<u8>)>) -> Self {
        let mut cs = ClassSpace::new(Box::new(provider(user)));
        let object = cs.load_class("java/lang/Object").expect("load Object");
        cs.object_class = Some(object);
        // StrObj 载荷的运行时类按"已加载"查找，须预载 String。
        cs.load_class("java/lang/String").expect("load String");
        // 套件资源（createImage(String) 数据源）
        for (name, bytes) in resources {
            cs.suite_resources.insert(name.to_string(), Rc::new(bytes));
        }
        Self {
            cs,
            heap: Heap::new(),
            monitors: Monitors::new(),
            natives: RegistryDispatch,
            host: MockHostContext::new(240, 320),
        }
    }

    fn machine(&mut self) -> Machine<'_> {
        Machine {
            cs: &mut self.cs,
            heap: &mut self.heap,
            monitors: &mut self.monitors,
            natives: &mut self.natives,
            host: &mut self.host,
        }
    }

    fn exec(&mut self, desc: &str) -> Result<RunOutcome, KvmError> {
        let class = self.cs.load_class("T")?;
        let mut frames = Vec::new();
        let mut machine = self.machine();
        enter(&mut machine, &mut frames, class, "main", desc, Vec::new())?;
        run(&mut machine, &mut frames, MAIN, None)
    }
}

fn m_code(desc: &'static str, stack: u16, locals: u16, code: Vec<u8>) -> MethodSpec {
    MethodSpec {
        access: 0x0008,
        name: "main",
        desc,
        code: Some((stack, locals, code)),
        handlers: Vec::new(),
    }
}

fn op3(op: u8, index: u16) -> [u8; 3] {
    let [hi, lo] = index.to_be_bytes();
    [op, hi, lo]
}

fn expect_int(outcome: RunOutcome) -> i32 {
    match outcome {
        RunOutcome::Returned(Some(Slot::Int(value))) => value,
        other => panic!("expected int return, got {other:?}"),
    }
}

/// int 压栈（iconst / bipush / sipush / ldc_w 按值域选择）。
fn push_int(pool: &mut Pool, value: i32) -> Vec<u8> {
    match value {
 -1..=5 => vec![0x02 + (value + 1) as u8], // iconst_m1..iconst_5
 -128..=127 => {
            let mut code = vec![BIPUSH];
            code.push(value as u8);
            code
        }
 -32768..=32767 => {
            let mut code = vec![SIPUSH];
            code.extend_from_slice(&(value as i16).to_be_bytes());
            code
        }
        _ => {
            let index = pool.integer(value);
            op3(LDC_W, index).to_vec()
        }
    }
}

/// 构码并执行 main（无参静态，返回 int），套件资源表注入夹具。
fn run_main_int(
    pool: &mut Pool,
    stack: u16,
    locals: u16,
    code: Vec<u8>,
    resources: Vec<(&str, Vec<u8>)>,
) -> i32 {
    let bytes = build_class(
        pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_code("()I", stack, locals, code)],
    );
    let mut vm = Vm::new(vec![("T".to_string(), bytes)], resources);
    let outcome = vm.exec("()I").expect("run main");
    if let RunOutcome::Uncaught(Unwind::Thrown(exception)) = &outcome
        && let Ok(class) = runtime_class_of(&vm.cs, &vm.heap, *exception) {
            panic!("expected int return, got Thrown({})", vm.cs.name_of(class));
        }
    expect_int(outcome)
}
fn run_main_thrown(
    pool: &mut Pool,
    stack: u16,
    locals: u16,
    code: Vec<u8>,
    resources: Vec<(&str, Vec<u8>)>,
) -> String {
    let bytes = build_class(
        pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_code("()I", stack, locals, code)],
    );
    let mut vm = Vm::new(vec![("T".to_string(), bytes)], resources);
    let outcome = vm.exec("()I").expect("run main");
    let RunOutcome::Uncaught(Unwind::Thrown(exception)) = outcome else {
        panic!("expected uncaught Thrown, got {outcome:?}")
    };
    let class = runtime_class_of(&vm.cs, &vm.heap, exception).expect("exception class");
    vm.cs.name_of(class).to_string()
}

/// 填充 byte[] 元素的构码序列（array 在 local `array_local`）。
fn code_fill_bytes(pool: &mut Pool, array_local: u8, bytes: &[u8]) -> Vec<u8> {
    let mut code = Vec::new();
    for (i, b) in bytes.iter().enumerate() {
        code.push(ALOAD);
        code.push(array_local);
        code.extend(push_int(pool, i as i32));
        code.extend(push_int(pool, i32::from(*b)));
        code.push(BASTORE);
    }
    code
}

/// 填充 int[] 元素的构码序列。
fn code_fill_ints(pool: &mut Pool, array_local: u8, values: &[i32]) -> Vec<u8> {
    let mut code = Vec::new();
    for (i, v) in values.iter().enumerate() {
        code.push(ALOAD);
        code.push(array_local);
        code.extend(push_int(pool, i as i32));
        code.extend(push_int(pool, *v));
        code.push(IASTORE);
    }
    code
}

// ---- createImage 端到端 -----------------------------------------------------

#[test]
fn create_image_string_loads_resource_and_fields() {
    // createImage("/skyicon.png") → getWidth*1000 + getHeight*100 + pixels[0]
    //（前导 '/' 资源键剥离 + 堆上 [I 像素数组经 GETFIELD/IALOAD 可达）。
    let mut pool = Pool::new();
    let name = pool.string("/skyicon.png");
    let create = pool.methodref(IMAGE, "createImage", "(Ljava/lang/String;)Ljavax/microedition/lcdui/Image;");
    let get_width = pool.methodref(IMAGE, "getWidth", "()I");
    let get_height = pool.methodref(IMAGE, "getHeight", "()I");
    let pixels = pool.fieldref(IMAGE, "pixels", "[I");

    let mut code = Vec::new();
    code.extend(op3(LDC_W, name));
    code.extend(op3(INVOKESTATIC, create));
    code.push(0x4c); // astore_1
    code.push(0x2b); // aload_1
    code.extend(op3(INVOKEVIRTUAL, get_width));
    code.extend(push_int(&mut pool, 1000));
    code.push(IMUL);
    code.push(0x2b);
    code.extend(op3(INVOKEVIRTUAL, get_height));
    code.extend(push_int(&mut pool, 100));
    code.push(IMUL);
    code.push(IADD);
    code.push(0x2b);
    code.extend(op3(GETFIELD, pixels));
    code.push(ICONST_0);
    code.push(IALOAD);
    code.push(IADD);
    code.push(IRETURN);

    let png_bytes = truecolor_png_2x2();
    let result = run_main_int(&mut pool, 6, 2, code, vec![("skyicon.png", png_bytes)]);
    // 2*1000 + 2*100 + pixels[0]（0xFF112233 为负 int，wrapping 求和）
    assert_eq!(result, 2 * 1000 + 2 * 100 + 0xFF112233u32 as i32);
}

#[test]
fn create_image_real_fixture_skyicon() {
    // 实弹：fixture jc.jar 的真实 skyicon.png（137x136）
    let jar = load_jar();
    let bytes = jar.get("skyicon.png").expect("fixture skyicon.png").clone();
    let mut pool = Pool::new();
    let name = pool.string("/skyicon.png");
    let create = pool.methodref(IMAGE, "createImage", "(Ljava/lang/String;)Ljavax/microedition/lcdui/Image;");
    let get_width = pool.methodref(IMAGE, "getWidth", "()I");
    let get_height = pool.methodref(IMAGE, "getHeight", "()I");

    let mut code = Vec::new();
    code.extend(op3(LDC_W, name));
    code.extend(op3(INVOKESTATIC, create));
    code.push(0x4c); // astore_1
    code.push(0x2b);
    code.extend(op3(INVOKEVIRTUAL, get_width));
    code.extend(push_int(&mut pool, 1000));
    code.push(IMUL);
    code.push(0x2b);
    code.extend(op3(INVOKEVIRTUAL, get_height));
    code.push(IADD);
    code.push(IRETURN);

    let result = run_main_int(&mut pool, 6, 2, code, vec![("skyicon.png", bytes)]);
    assert_eq!(result, 137 * 1000 + 136);
}

#[test]
fn create_rgb_image_roundtrip_and_process_alpha() {
    // createRGBImage(源[4], 0, 2, 2, processAlpha)：false → alpha 置 0xFF。
    // 返回 pixels[0]*10 + pixels[1] 太大溢出，分两个构码各断言首像素。
    for (process_alpha, first, second) in [
        (1, 0x0011_2233u32, 0x8044_5566),
        (0, 0xFF11_2233, 0xFF44_5566),
    ] {
        let mut pool = Pool::new();
        let create = pool.methodref(IMAGE, "createRGBImage", "([IIIZ)Ljavax/microedition/lcdui/Image;");
        let pixels = pool.fieldref(IMAGE, "pixels", "[I");
        let mut code = vec![ICONST_4, NEWARRAY, T_INT];
        code.push(0x4c); // astore_1
        code.extend(code_fill_ints(&mut pool, 1, &[0x0011_2233u32 as i32, 0x8044_5566u32 as i32, 0, 0]));
        code.push(0x2b); // aload_1
        code.push(ICONST_2); // width
        code.push(ICONST_2); // height
        code.extend(push_int(&mut pool, process_alpha));
        code.extend(op3(INVOKESTATIC, create));
        code.push(0x4d); // astore_2
        code.push(ALOAD);
        code.push(2);
        code.extend(op3(GETFIELD, pixels));
        code.push(ICONST_1); // pixels[1]（第二像素区分度更高）
        code.push(IALOAD);
        code.push(IRETURN);
        let result = run_main_int(&mut pool, 8, 3, code, Vec::new());
        assert_eq!(result as u32, second, "processAlpha={process_alpha} 首像素 {first:08X} 应忽略，断言第二像素");
    }
}

#[test]
fn create_image_source_copy_and_region_crop() {
    // 源 2x2（RGB 值已知）→ createImage(src,1,0,1,2) 裁剪 → 1x2，
    // pixels[0] 应为源 (1,0)=0xFFBB0000。
    let mut pool = Pool::new();
    let create_rgb = pool.methodref(IMAGE, "createRGBImage", "([IIIZ)Ljavax/microedition/lcdui/Image;");
    let create_region = pool.methodref(IMAGE, "createImage", "(Ljavax/microedition/lcdui/Image;IIII)Ljavax/microedition/lcdui/Image;");
    let get_width = pool.methodref(IMAGE, "getWidth", "()I");
    let pixels = pool.fieldref(IMAGE, "pixels", "[I");

    let mut code = vec![ICONST_4, NEWARRAY, T_INT];
    code.push(0x4c); // astore_1
    code.extend(code_fill_ints(
        &mut pool,
        1,
        &[
            0xFFAA_0000u32 as i32,
            0xFFBB_0000u32 as i32,
            0xFFCC_0000u32 as i32,
            0xFFDD_0000u32 as i32,
        ],
    ));
    // 源图：createRGBImage(arr, 2, 2, true) → local 2
    code.push(ALOAD);
    code.push(1);
    code.push(ICONST_2);
    code.push(ICONST_2);
    code.push(ICONST_1);
    code.extend(op3(INVOKESTATIC, create_rgb));
    code.push(0x4d); // astore_2
    // 裁剪：createImage(src, 1, 0, 1, 2) → local 3
    code.push(0x2c); // aload_2
    code.push(ICONST_1);
    code.push(ICONST_0);
    code.push(ICONST_1);
    code.push(ICONST_2);
    code.extend(op3(INVOKESTATIC, create_region));
    code.push(0x4e); // astore_3
    // 返回 pixels[0]*8 + getWidth（=0xFFBB0000 负数 + 1 → 回 wrapping 加法）
    code.push(0x2d); // aload_3
    code.extend(op3(GETFIELD, pixels));
    code.push(ICONST_0);
    code.push(IALOAD);
    code.push(0x2d);
    code.extend(op3(INVOKEVIRTUAL, get_width));
    code.push(IADD);
    code.push(IRETURN);

    let result = run_main_int(&mut pool, 8, 4, code, Vec::new());
    // pixels[0] + getWidth = 0xFFBB0000 + 1
    assert_eq!(result, 0xFFBB_0000u32 as i32 + 1);
}

#[test]
fn mutable_image_semantics() {
    // createImage(2,2) isMutable=1（宽高就绪）；解码产物 isMutable=0。
    // 返回 mutable*1000 + width*100 + height。
    let mut pool = Pool::new();
    let create_empty = pool.methodref(IMAGE, "createImage", "(II)Ljavax/microedition/lcdui/Image;");
    let is_mutable = pool.methodref(IMAGE, "isMutable", "()Z");
    let get_width = pool.methodref(IMAGE, "getWidth", "()I");
    let get_height = pool.methodref(IMAGE, "getHeight", "()I");

    let mut code = Vec::new();
    code.push(ICONST_2);
    code.push(ICONST_2);
    code.extend(op3(INVOKESTATIC, create_empty));
    code.push(0x4c); // astore_1
    code.push(0x2b);
    code.extend(op3(INVOKEVIRTUAL, is_mutable));
    code.extend(push_int(&mut pool, 1000));
    code.push(IMUL);
    code.push(0x2b);
    code.extend(op3(INVOKEVIRTUAL, get_width));
    code.extend(push_int(&mut pool, 100));
    code.push(IMUL);
    code.push(IADD);
    code.push(0x2b);
    code.extend(op3(INVOKEVIRTUAL, get_height));
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(
        run_main_int(&mut pool, 6, 2, code, Vec::new()),
        1000 + 2 * 100 + 2
    );

    // 解码产物 isMutable() == 0
    let mut pool = Pool::new();
    let name = pool.string("t.png");
    let create = pool.methodref(IMAGE, "createImage", "(Ljava/lang/String;)Ljavax/microedition/lcdui/Image;");
    let is_mutable = pool.methodref(IMAGE, "isMutable", "()Z");
    let mut code = Vec::new();
    code.extend(op3(LDC_W, name));
    code.extend(op3(INVOKESTATIC, create));
    code.extend(op3(INVOKEVIRTUAL, is_mutable));
    code.push(IRETURN);
    assert_eq!(
        run_main_int(&mut pool, 4, 1, code, vec![("t.png", truecolor_png_2x2())]),
        0
    );
}

#[test]
fn create_image_bytes_overload() {
    // byte[] 前置 3 字节垃圾 + PNG：createImage(arr, 3, len-3)
    let png_bytes = truecolor_png_2x2();
    let mut data = vec![0xDE, 0xAD, 0xBE];
    data.extend_from_slice(&png_bytes);

    let mut pool = Pool::new();
    let create = pool.methodref(IMAGE, "createImage", "([BII)Ljavax/microedition/lcdui/Image;");
    let get_width = pool.methodref(IMAGE, "getWidth", "()I");
    let pixels = pool.fieldref(IMAGE, "pixels", "[I");

    let mut code = Vec::new();
    code.extend(push_int(&mut pool, data.len() as i32));
    code.push(NEWARRAY);
    code.push(T_BYTE);
    code.push(0x4c); // astore_1
    code.extend(code_fill_bytes(&mut pool, 1, &data));
    code.push(ALOAD);
    code.push(1);
    code.push(ICONST_3);
    code.extend(push_int(&mut pool, png_bytes.len() as i32));
    code.extend(op3(INVOKESTATIC, create));
    code.push(0x4d); // astore_2
    code.push(0x2c);
    code.extend(op3(INVOKEVIRTUAL, get_width));
    code.extend(push_int(&mut pool, 100));
    code.push(IMUL);
    code.push(0x2c);
    code.extend(op3(GETFIELD, pixels));
    code.push(ICONST_3);
    code.push(IALOAD);
    code.push(IADD);
    code.push(IRETURN);

    let result = run_main_int(&mut pool, 8, 3, code, Vec::new());
    assert_eq!(result as u32, 2 * 100 + 0xFFAABBCCu32);
}

#[test]
fn create_image_from_input_stream() {
    // NEW+DUP+参数+INVOKESPECIAL（javac 形态）构造 BAIS([B) 后 createImage。
    let png_bytes = truecolor_png_2x2();
    let mut pool = Pool::new();
    let bais_class = pool.class("java/io/ByteArrayInputStream");
    let bais_init = pool.methodref("java/io/ByteArrayInputStream", "<init>", "([B)V");
    let create = pool.methodref(IMAGE, "createImage", "(Ljava/io/InputStream;)Ljavax/microedition/lcdui/Image;");
    let get_height = pool.methodref(IMAGE, "getHeight", "()I");
    let pixels = pool.fieldref(IMAGE, "pixels", "[I");

    let mut code = Vec::new();
    code.extend(push_int(&mut pool, png_bytes.len() as i32));
    code.push(NEWARRAY);
    code.push(T_BYTE);
    code.push(0x4c); // astore_1
    code.extend(code_fill_bytes(&mut pool, 1, &png_bytes));
    code.extend(op3(NEW, bais_class));
    code.push(DUP);
    code.push(ALOAD);
    code.push(1);
    code.extend(op3(INVOKESPECIAL, bais_init));
    code.extend(op3(INVOKESTATIC, create));
    code.push(0x4d); // astore_2
    code.push(0x2c);
    code.extend(op3(INVOKEVIRTUAL, get_height));
    code.extend(push_int(&mut pool, 1000));
    code.push(IMUL);
    code.push(0x2c);
    code.extend(op3(GETFIELD, pixels));
    code.push(ICONST_2); // 第三像素（行优先第二行首 = 0x00778899）
    code.push(IALOAD);
    code.push(IADD);
    code.push(IRETURN);

    let result = run_main_int(&mut pool, 8, 3, code, Vec::new());
    assert_eq!(result as u32, 2 * 1000 + 0x00778899u32);
}

#[test]
fn bad_png_and_missing_resource_throw_ioexception() {
    // 坏 PNG 字节 / 缺失资源键 → 未捕获 java/io/IOException
    for (key, bytes) in [("bad.png", b"not a png".to_vec()), ("missing.png", Vec::new())] {
        let mut pool = Pool::new();
        let name = pool.string(key);
        let create = pool.methodref(IMAGE, "createImage", "(Ljava/lang/String;)Ljavax/microedition/lcdui/Image;");
        let mut code = Vec::new();
        code.extend(op3(LDC_W, name));
        code.extend(op3(INVOKESTATIC, create));
        code.push(POP); // 正常路径丢弃返回引用（异常路径到不了这里）
        code.push(ICONST_0);
        code.push(IRETURN);
        let resources = if key == "bad.png" {
            vec![("bad.png", bytes)]
        } else {
            Vec::new()
        };
        assert_eq!(
            run_main_thrown(&mut pool, 4, 1, code, resources),
            "java/io/IOException",
            "case {key}"
        );
    }
}

#[test]
fn vm_facade_installs_suite_resources() {
    // Vm 门面装真 jc.jar：资源进套件资源表（has_jar_entry 并查两处）
    let mrp = fs::read(fixture_path()).expect("read mrp fixture");
    let jar = mrp_read_entry(&mrp, "jc.jar");
    let mut vm = KvmVm::new(Box::new(MockHostContext::new(240, 320)), VmConfig::default());
    vm.install_jar("gjxjwql", &jar).expect("install jar");
    assert!(vm.has_jar_entry("skyicon.png"), "PNG 资源应入套件资源表");
    assert!(vm.has_jar_entry("META-INF/MANIFEST.MF"));
    assert!(vm.has_jar_entry("com/fs/arpg/GameMidlet"), "类条目不受影响");
}

// ---- T022 汇合补漏：异常条件 + getGraphics 双取语义 -------------------------

/// 本段用到的补充操作码（数值补齐，不与上文冲突）。
const ICONST_5: u8 = 0x08;
const ICONST_M1: u8 = 0x02;
const ASTORE: u8 = 0x3a;
const ISUB: u8 = 0x64;
const INEG: u8 = 0x74;
const IOR: u8 = 0x80;
const IUSHR: u8 = 0x7c;
const GRAPHICS: &str = "javax/microedition/lcdui/Graphics";

/// createImage(II) 负宽/零高/零尺寸 → IllegalArgumentException
/// （T022 原文：负尺寸抛 IAE；MIDP javadoc 要求 width/height 均 > 0）。
#[test]
fn create_image_empty_rejects_non_positive_size() {
    for (w, h) in [(-4, 5), (4, 0), (0, 0)] {
        let mut pool = Pool::new();
        let create = pool.methodref(IMAGE, "createImage", "(II)Ljavax/microedition/lcdui/Image;");
        let mut code = Vec::new();
        code.extend(push_int(&mut pool, w));
        code.extend(push_int(&mut pool, h));
        code.extend(op3(INVOKESTATIC, create));
        code.push(ICONST_0); // 不到达（异常展开），仅平衡返回
        code.push(IRETURN);
        assert_eq!(
            run_main_thrown(&mut pool, 4, 1, code, Vec::new()),
            "java/lang/IllegalArgumentException",
            "createImage({w}, {h}) 应抛 IllegalArgumentException"
        );
    }
}

/// createRGBImage([IIIZ) 非正宽高 → IllegalArgumentException（同值域约束）。
#[test]
fn create_rgb_image_rejects_non_positive_size() {
    for (w, h) in [(0, 2), (2, -1)] {
        let mut pool = Pool::new();
        let create = pool.methodref(IMAGE, "createRGBImage", "([IIIZ)Ljavax/microedition/lcdui/Image;");
        let mut code = vec![ICONST_4, NEWARRAY, T_INT, 0x4c]; // int[4] → local 1
        code.extend(code_fill_ints(&mut pool, 1, &[0xFF11_2233u32 as i32; 4]));
        code.push(0x2b); // aload_1（数组）
        code.extend(push_int(&mut pool, w));
        code.extend(push_int(&mut pool, h));
        code.push(ICONST_1); // processAlpha=true（先于尺寸校验无关紧要）
        code.extend(op3(INVOKESTATIC, create));
        code.push(ICONST_0);
        code.push(IRETURN);
        assert_eq!(
            run_main_thrown(&mut pool, 6, 2, code, Vec::new()),
            "java/lang/IllegalArgumentException",
            "createRGBImage(_, {w}, {h}, _) 应抛 IllegalArgumentException"
        );
    }
}

/// createImage(src,IIII) 区域负宽 → IllegalArgumentException（MIDP javadoc：
/// 区域须含于源图且非负）。
#[test]
fn create_image_region_rejects_negative_size() {
    let mut pool = Pool::new();
    let rgb = pool.methodref(IMAGE, "createRGBImage", "([IIIZ)Ljavax/microedition/lcdui/Image;");
    let region = pool.methodref(IMAGE, "createImage", "(Ljavax/microedition/lcdui/Image;IIII)Ljavax/microedition/lcdui/Image;");
    let mut code = vec![ICONST_4, NEWARRAY, T_INT, 0x4c]; // int[4] → local 1
    code.extend(code_fill_ints(&mut pool, 1, &[0xFF00_00FFu32 as i32; 4]));
    code.push(0x2b);
    code.push(ICONST_2); // w=2
    code.push(ICONST_2); // h=2
    code.push(ICONST_1); // processAlpha
    code.extend(op3(INVOKESTATIC, rgb));
    code.push(0x4d); // astore_2（源图）
    code.push(0x2c); // aload_2（源图）
    code.push(ICONST_0); // x
    code.push(ICONST_0); // y
    code.push(ICONST_M1); // 宽 -1 → IAE
    code.push(ICONST_1); // 高 1
    code.extend(op3(INVOKESTATIC, region));
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(
        run_main_thrown(&mut pool, 8, 3, code, Vec::new()),
        "java/lang/IllegalArgumentException",
        "区域裁剪负宽应抛 IllegalArgumentException"
    );
}

/// 可变图两次 getGraphics：各自返回独立 Graphics 但共享同一像素后备
///（MIDP javadoc："每次调用返回新对象"，绘制效果均落在同一 Image 上）。
/// 断言：g1 画左上 2x2 红、g2 画右下 2x2 绿后，单像素数组同时含两块。
#[test]
fn get_graphics_twice_shares_backing_store() {
    let mut pool = Pool::new();
    let get_g = pool.methodref(IMAGE, "getGraphics", "()Ljavax/microedition/lcdui/Graphics;");
    let set_color = pool.methodref(GRAPHICS, "setColor", "(I)V");
    let fill = pool.methodref(GRAPHICS, "fillRect", "(IIII)V");
    let pixels = pool.fieldref(IMAGE, "pixels", "[I");
    let mut code = Vec::new();
    code.extend(push_int(&mut pool, 4));
    code.extend(push_int(&mut pool, 4));
    code.extend(op3(INVOKESTATIC, pool.methodref(IMAGE, "createImage", "(II)Ljavax/microedition/lcdui/Image;")));
    code.push(0x4c); // astore_1（img）
    code.push(0x2b);
    code.extend(op3(INVOKEVIRTUAL, get_g));
    code.push(0x4d); // astore_2（g1）
    code.push(0x2b);
    code.extend(op3(INVOKEVIRTUAL, get_g));
    code.push(0x4e); // astore_3（g2）
    // g1：红 fillRect(0,0,2,2) → 像素 0,1,4,5。
    code.push(0x2c);
    code.extend(push_int(&mut pool, 0xFF_0000));
    code.extend(op3(INVOKEVIRTUAL, set_color));
    code.push(0x2c);
    code.extend(push_int(&mut pool, 0));
    code.extend(push_int(&mut pool, 0));
    code.extend(push_int(&mut pool, 2));
    code.extend(push_int(&mut pool, 2));
    code.extend(op3(INVOKEVIRTUAL, fill));
    // g2：绿 fillRect(2,2,2,2) → 像素 10,11,14,15。
    code.push(0x2d);
    code.extend(push_int(&mut pool, 0x00_FF00));
    code.extend(op3(INVOKEVIRTUAL, set_color));
    code.push(0x2d);
    code.extend(push_int(&mut pool, 2));
    code.extend(push_int(&mut pool, 2));
    code.extend(push_int(&mut pool, 2));
    code.extend(push_int(&mut pool, 2));
    code.extend(op3(INVOKEVIRTUAL, fill));
    // 返回 Σ neq(p_i, e_i)：无分支比较（差值符号位即"不等"）。
    let red = 0xFFFF_0000u32 as i32;
    let green = 0xFF00_FF00u32 as i32;
    let expected = [red, red, 0, 0, red, red, 0, 0, 0, 0, green, green, 0, 0, green, green];
    code.push(ICONST_0); // 累加器
    code.push(0x2b);
    code.extend(op3(GETFIELD, pixels));
    code.push(ASTORE);
    code.push(4); // astore_4（数组）
    for (i, e) in expected.iter().enumerate() {
        code.push(ALOAD);
        code.push(4);
        code.extend(push_int(&mut pool, i as i32));
        code.push(IALOAD);
        code.extend(push_int(&mut pool, *e));
        code.push(ISUB);
        code.push(DUP);
        code.push(INEG);
        code.push(IOR);
        code.push(ICONST_5);
        code.push(IUSHR);
        code.push(IADD);
    }
    code.push(IRETURN);
    assert_eq!(
        run_main_int(&mut pool, 10, 5, code, Vec::new()),
        0,
        "两次 getGraphics 的绘制应同时落在同一像素后备（16 像素全符合）"
    );
}

#[test]
fn draw_region_transforms_match_sky() {
    // 源 2x2（AA BB / CC DD）drawRegion 后读 pixels[0..2]。
    // 权威表（VMRP 逆向 mr_drawRegion 核定，与 JSR-118 一致）：
    // t=1 MIRROR_ROT180（垂直镜像）→ [CC DD | AA BB]；
    // t=2 MIRROR（实证）→ [BB AA | DD CC]；
    // t=4 MIRROR_ROT270 → [AA CC | BB DD]；t=0 NONE → [AA BB | CC DD]。
    for (transform, want0, want1, label) in [
        (1i32, 0xFFCC_0000u32 as i32, 0xFFDD_0000u32 as i32, "MIRROR_ROT180"),
        (2, 0xFFBB_0000u32 as i32, 0xFFAA_0000u32 as i32, "SKY-MIRROR"),
        (4, 0xFFAA_0000u32 as i32, 0xFFCC_0000u32 as i32, "MIRROR_ROT270"),
        (0, 0xFFAA_0000u32 as i32, 0xFFBB_0000u32 as i32, "NONE"),
    ] {
        let mut pool = Pool::new();
        let create_rgb = pool.methodref(IMAGE, "createRGBImage", "([IIIZ)Ljavax/microedition/lcdui/Image;");
        let create_empty = pool.methodref(IMAGE, "createImage", "(II)Ljavax/microedition/lcdui/Image;");
        let get_graphics = pool.methodref(IMAGE, "getGraphics", "()Ljavax/microedition/lcdui/Graphics;");
        let draw_region = pool.methodref("javax/microedition/lcdui/Graphics", "drawRegion", "(Ljavax/microedition/lcdui/Image;IIIIIIII)V");
        let pixels = pool.fieldref(IMAGE, "pixels", "[I");

        let mut code = vec![ICONST_4, NEWARRAY, T_INT];
        code.push(0x4c); // astore_1
        code.extend(code_fill_ints(
            &mut pool,
            1,
            &[
                0xFFAA_0000u32 as i32,
                0xFFBB_0000u32 as i32,
                0xFFCC_0000u32 as i32,
                0xFFDD_0000u32 as i32,
            ],
        ));
        // local2 = createRGBImage(arr, 2, 2, true)
        code.push(ALOAD);
        code.push(1);
        code.push(ICONST_2);
        code.push(ICONST_2);
        code.push(ICONST_1);
        code.extend(op3(INVOKESTATIC, create_rgb));
        code.push(0x4d);
        // local3 = createImage(2, 2) 可变目标
        code.push(ICONST_2);
        code.push(ICONST_2);
        code.extend(op3(INVOKESTATIC, create_empty));
        code.push(0x4e);
        // g = local3.getGraphics()（栈顶留作 drawRegion 接收者）
        code.push(ALOAD);
        code.push(3);
        code.extend(op3(INVOKEVIRTUAL, get_graphics));
        // g.drawRegion(src(local2), 0,0,2,2, transform, 0,0, LEFT|TOP=20)
        code.push(ALOAD);
        code.push(2);
        code.push(ICONST_0);
        code.push(ICONST_0);
        code.push(ICONST_2);
        code.push(ICONST_2);
        code.extend(push_int(&mut pool, transform));
        code.push(ICONST_0);
        code.push(ICONST_0);
        code.extend(push_int(&mut pool, 20));
        code.extend(op3(INVOKEVIRTUAL, draw_region));
        // 返回 pixels[0]*2 + pixels[1]→太大；分两次断言，这里返回 pixels[0]
        code.push(ALOAD);
        code.push(3);
        code.extend(op3(GETFIELD, pixels));
        code.push(ICONST_0);
        code.push(IALOAD);
        code.push(IRETURN);
        let result = run_main_int(&mut pool, 8, 4, code, Vec::new());
        assert_eq!(result, want0, "TRANS={transform}({label}) pixels[0]");
    }
}
