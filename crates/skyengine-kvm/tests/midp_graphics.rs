//! LCDUI Graphics 光栅化测试（T020）：真注册表 + MockHostContext 端到端。
//!
//! 像素断言直接读堆上 `[I`（main 返回 image.pixels 引用，Rust 侧
//! `heap.array` 逐点核对）。Mock 合成字体（host/mock.rs）：默认档
//! (0,0,0) px=12 → height=16、ascent=12、text_width = 码元数×6；
//! render_text 写不透明单色矩形（x..x+w, y..y+height）。
//!
//! 构码铁律（前车之鉴）：带参构造 NEW+DUP+参数+INVOKESPECIAL；
//! 分支指令消耗条件值后不再 POP；池索引先取号再构码。

#[allow(dead_code)]
mod common;

use common::classbytes::{build_class, provider, MethodSpec, Pool};
use skyengine_kvm::host::mock::MockHostContext;
use skyengine_kvm::interp::natives::RegistryDispatch;
use skyengine_kvm::interp::{enter, run, runtime_class_of, Machine, RunOutcome, Unwind};
use skyengine_kvm::runtime::classspace::ClassSpace;
use skyengine_kvm::runtime::heap::Heap;
use skyengine_kvm::runtime::monitor::Monitors;
use skyengine_kvm::runtime::object::ArrayData;
use skyengine_kvm::runtime::{ObjRef, Slot, ThreadId};
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
const ACONST_NULL: u8 = 0x01;
const POP: u8 = 0x57;
const IMUL: u8 = 0x68;
const IADD: u8 = 0x60;
const NEWARRAY: u8 = 0xbc;
const T_INT: u8 = 10;
const IASTORE: u8 = 0x4f;
const ALOAD: u8 = 0x19;
const ASTORE: u8 = 0x3a;
const GETFIELD: u8 = 0xb4;
const INVOKEVIRTUAL: u8 = 0xb6;
const INVOKESTATIC: u8 = 0xb8;
const IRETURN: u8 = 0xac;
const ARETURN: u8 = 0xb0;

const MAIN: ThreadId = ThreadId(1);
const IMAGE: &str = "javax/microedition/lcdui/Image";
const GRAPHICS: &str = "javax/microedition/lcdui/Graphics";

// ---- 夹具（midp_image 同型） -----------------------------------------------

struct Vm {
    cs: ClassSpace,
    heap: Heap,
    monitors: Monitors,
    natives: RegistryDispatch,
    host: MockHostContext,
}

impl Vm {
    fn new(user: Vec<(String, Vec<u8>)>) -> Self {
        let mut cs = ClassSpace::new(Box::new(provider(user)));
        let object = cs.load_class("java/lang/Object").expect("load Object");
        cs.object_class = Some(object);
        // StrObj 载荷的运行时类按"已加载"查找，须预载 String。
        cs.load_class("java/lang/String").expect("load String");
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
        -128..=127 => vec![BIPUSH, value as u8],
        -32768..=32767 => {
            let mut code = vec![SIPUSH];
            code.extend_from_slice(&(value as i16).to_be_bytes());
            code
        }
        _ => op3(LDC_W, pool.integer(value)).to_vec(),
    }
}

/// 构码并执行 main（无参静态，返回 int）。
fn run_main_int(pool: &mut Pool, stack: u16, locals: u16, code: Vec<u8>) -> i32 {
    let bytes = build_class(
        pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_code("()I", stack, locals, code)],
    );
    let mut vm = Vm::new(vec![("T".to_string(), bytes)]);
    let outcome = vm.exec("()I").expect("run main");
    if let RunOutcome::Uncaught(Unwind::Thrown(exception)) = &outcome
        && let Ok(class) = runtime_class_of(&vm.cs, &vm.heap, *exception) {
            panic!("expected int return, got Thrown({})", vm.cs.name_of(class));
        }
    expect_int(outcome)
}

/// 构码执行并保留 VM（main 返回引用——像素数组/Font 实例），
/// 供 Rust 侧直接读堆断言。
fn run_main_ref(pool: &mut Pool, stack: u16, locals: u16, code: Vec<u8>) -> (Vm, ObjRef) {
    let bytes = build_class(
        pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_code("()[I", stack, locals, code)],
    );
    let mut vm = Vm::new(vec![("T".to_string(), bytes)]);
    let outcome = vm.exec("()[I").expect("run main");
    match outcome {
        RunOutcome::Returned(Some(Slot::Ref(r))) => (vm, r),
        other => panic!("expected ref return, got {other:?}"),
    }
}

/// 期望未捕获异常，返回异常类名。
fn run_main_thrown(pool: &mut Pool, stack: u16, locals: u16, code: Vec<u8>) -> String {
    let bytes = build_class(
        pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_code("()I", stack, locals, code)],
    );
    let mut vm = Vm::new(vec![("T".to_string(), bytes)]);
    let outcome = vm.exec("()I").expect("run main");
    let RunOutcome::Uncaught(Unwind::Thrown(exception)) = outcome else {
        panic!("expected uncaught Thrown, got {outcome:?}")
    };
    let class = runtime_class_of(&vm.cs, &vm.heap, exception).expect("exception class");
    vm.cs.name_of(class).to_string()
}

/// 像素读取（堆上 `[I，行宽 w）。
fn px(vm: &Vm, arr: ObjRef, w: i32, x: i32, y: i32) -> i32 {
    match &vm.heap.array(arr).expect("pixels array").data {
        ArrayData::Int(d) => d[(y * w + x) as usize],
        _ => panic!("not int[]"),
    }
}

// ---- 构码片段速写 ----------------------------------------------------------

/// createImage(w,h)→local1；getGraphics()→local2。
fn code_new_graphics(pool: &mut Pool, w: i32, h: i32) -> Vec<u8> {
    let create = pool.methodref(IMAGE, "createImage", "(II)Ljavax/microedition/lcdui/Image;");
    let get_g = pool.methodref(IMAGE, "getGraphics", "()Ljavax/microedition/lcdui/Graphics;");
    let mut code = Vec::new();
    code.extend(push_int(pool, w));
    code.extend(push_int(pool, h));
    code.extend(op3(INVOKESTATIC, create));
    code.push(0x4c); // astore_1
    code.push(0x2b); // aload_1
    code.extend(op3(INVOKEVIRTUAL, get_g));
    code.push(0x4d); // astore_2
    code
}

/// g.setColor(color)（local 2）。
fn code_set_color(pool: &mut Pool, color: i32) -> Vec<u8> {
    let set = pool.methodref(GRAPHICS, "setColor", "(I)V");
    let mut code = vec![0x2c]; // aload_2（g）
    code.extend(push_int(pool, color));
    code.extend(op3(INVOKEVIRTUAL, set));
    code
}

/// g.invoke(IIII)（四参 void 方法：fillRect/drawRect/clipRect/setClip）。
fn code_call4(pool: &mut Pool, name: &str, a: i32, b: i32, c: i32, d: i32) -> Vec<u8> {
    let mref = pool.methodref(GRAPHICS, name, "(IIII)V");
    let mut code = vec![0x2c]; // aload_2（g）
    code.extend(push_int(pool, a));
    code.extend(push_int(pool, b));
    code.extend(push_int(pool, c));
    code.extend(push_int(pool, d));
    code.extend(op3(INVOKEVIRTUAL, mref));
    code
}

/// 尾码：返回 image.pixels（local 1）。
fn code_return_pixels(pool: &mut Pool) -> Vec<u8> {
    let pixels = pool.fieldref(IMAGE, "pixels", "[I");
    let mut code = vec![0x2b]; // aload_1
    code.extend(op3(GETFIELD, pixels));
    code.push(ARETURN);
    code
}

/// 填充 int[] 元素构码（array 在 local `al`）。
fn code_fill_ints(pool: &mut Pool, al: u8, values: &[i32]) -> Vec<u8> {
    let mut code = Vec::new();
    for (i, v) in values.iter().enumerate() {
        code.push(ALOAD);
        code.push(al);
        code.extend(push_int(pool, i as i32));
        code.extend(push_int(pool, *v));
        code.push(IASTORE);
    }
    code
}

/// 造 2x2 不可变源图（像素 0xFF112233/0xFF445566/0xFF778899/0xFFAABBCC）
/// → local `dst`。
fn code_new_rgb_source(pool: &mut Pool, dst: u8) -> Vec<u8> {
    let create = pool.methodref(IMAGE, "createRGBImage", "([IIIZ)Ljavax/microedition/lcdui/Image;");
    let mut code = vec![ICONST_4, NEWARRAY, T_INT];
    code.push(ASTORE);
    code.push(dst);
    let vals: &[i32] = &[
        0xFF11_2233u32 as i32,
        0xFF44_5566u32 as i32,
        0xFF77_8899u32 as i32,
        0xFFAA_BBCCu32 as i32,
    ];
    code.extend(code_fill_ints(pool, dst, vals));
    code.push(ALOAD);
    code.push(dst);
    code.push(ICONST_2); // width
    code.push(ICONST_2); // height
    code.push(ICONST_1); // processAlpha
    code.extend(op3(INVOKESTATIC, create));
    code.push(ASTORE);
    code.push(dst);
    code
}

const WHITE: i32 = 0xFFFF_FFFFu32 as i32;

// ---- Image.getGraphics -----------------------------------------------------

#[test]
fn get_graphics_on_immutable_throws() {
    // createRGBImage 产物不可变 → IllegalStateException。
    let mut pool = Pool::new();
    let get_g = pool.methodref(IMAGE, "getGraphics", "()Ljavax/microedition/lcdui/Graphics;");
    let mut code = code_new_rgb_source(&mut pool, 1);
    code.push(0x2b); // aload_1
    code.extend(op3(INVOKEVIRTUAL, get_g));
    code.push(POP);
    code.push(ICONST_0);
    code.push(IRETURN);
    let thrown = run_main_thrown(&mut pool, 6, 3, code);
    assert_eq!(thrown, "java/lang/IllegalStateException");
}

#[test]
fn get_graphics_initial_state() {
    // clip=全图、translate=(0,0)、color=0、stroke=SOLID、font 非 null。
    let mut pool = Pool::new();
    let mut code = code_new_graphics(&mut pool, 8, 6);
    // getClipX + getClipY*10 + getClipWidth*100 + getClipHeight*1000
    // + getTranslateX*10000 + getTranslateY*100000 + getColor*10^6 + stroke*10^7
    let combos: &[(&str, &str, i32)] = &[
        ("getClipX", "()I", 1),
        ("getClipY", "()I", 10),
        ("getClipWidth", "()I", 100),
        ("getClipHeight", "()I", 1000),
        ("getTranslateX", "()I", 10000),
        ("getTranslateY", "()I", 100000),
        ("getColor", "()I", 1000000),
        ("getStrokeStyle", "()I", 10000000),
    ];
    for (i, (name, desc, mul)) in combos.iter().enumerate() {
        let mref = pool.methodref(GRAPHICS, name, desc);
        code.push(0x2c); // aload_2（g）
        code.extend(op3(INVOKEVIRTUAL, mref));
        code.extend(push_int(&mut pool, *mul));
        code.push(IMUL);
        if i > 0 {
            code.push(IADD);
        }
    }
    code.push(IRETURN);
    let result = run_main_int(&mut pool, 8, 3, code);
    assert_eq!(result, 8 * 100 + 6 * 1000, "clip=(0,0,8,6)、其余全 0");

    // font 非 null：返回 getFont() 引用断言。
    let mut pool = Pool::new();
    let get_font = pool.methodref(GRAPHICS, "getFont", "()Ljavax/microedition/lcdui/Font;");
    let mut code = code_new_graphics(&mut pool, 4, 4);
    code.push(0x2c); // aload_2（g）
    code.extend(op3(INVOKEVIRTUAL, get_font));
    code.push(ARETURN);
    let (_vm, font) = run_main_ref(&mut pool, 6, 3, code);
    assert_ne!(font, ObjRef::NULL, "初始字体应为默认字体实例");
}

// ---- 颜色 ------------------------------------------------------------------

#[test]
fn set_color_and_components_roundtrip() {
    // setColor(0xAB123456)：alpha 忽略 → getColor=0x123456；
    // 分量回读 R=0x12 G=0x34 B=0x56。
    let mut pool = Pool::new();
    let mut code = code_new_graphics(&mut pool, 4, 4);
    code.extend(code_set_color(&mut pool, 0xAB12_3456u32 as i32));
    let combos: &[(&str, i32)] = &[
        ("getColor", 1),
        ("getRedComponent", 256),
        ("getGreenComponent", 65536),
        ("getBlueComponent", 16777216),
    ];
    for (i, (name, mul)) in combos.iter().enumerate() {
        let mref = pool.methodref(GRAPHICS, name, "()I");
        code.push(0x2c); // aload_2（g）
        code.extend(op3(INVOKEVIRTUAL, mref));
        code.extend(push_int(&mut pool, *mul));
        code.push(IMUL);
        if i > 0 {
            code.push(IADD);
        }
    }
    code.push(IRETURN);
    let result = run_main_int(&mut pool, 8, 3, code) as u32;
    assert_eq!(
        result,
        0x0012_3456 + 0x12 * 256 + 0x34 * 65536 + 0x56 * 16777216
    );

    // setColor(r,g,b) 组合路径。
    let mut pool = Pool::new();
    let set = pool.methodref(GRAPHICS, "setColor", "(III)V");
    let get = pool.methodref(GRAPHICS, "getColor", "()I");
    let mut code = code_new_graphics(&mut pool, 4, 4);
    code.push(0x2c); // aload_2（g）
    code.extend(push_int(&mut pool, 0x11));
    code.extend(push_int(&mut pool, 0x22));
    code.extend(push_int(&mut pool, 0x33));
    code.extend(op3(INVOKEVIRTUAL, set));
    code.push(0x2c); // aload_2（g）
    code.extend(op3(INVOKEVIRTUAL, get));
    code.push(IRETURN);
    assert_eq!(run_main_int(&mut pool, 6, 3, code), 0x112233);
}

#[test]
fn set_gray_scale_roundtrip() {
    // setGrayScale(0x80) → getColor = 0x808080、getGrayScale = 0x80。
    let mut pool = Pool::new();
    let set = pool.methodref(GRAPHICS, "setGrayScale", "(I)V");
    let get = pool.methodref(GRAPHICS, "getColor", "()I");
    let get_gray = pool.methodref(GRAPHICS, "getGrayScale", "()I");
    let mut code = code_new_graphics(&mut pool, 4, 4);
    code.push(0x2c); // aload_2（g）
    code.extend(push_int(&mut pool, 0x80));
    code.extend(op3(INVOKEVIRTUAL, set));
    code.push(0x2c); // aload_2（g）
    code.extend(op3(INVOKEVIRTUAL, get));
    code.push(0x2c); // aload_2（g）
    code.extend(op3(INVOKEVIRTUAL, get_gray));
    code.extend(push_int(&mut pool, 0x10000));
    code.push(IMUL);
    code.push(IADD);
    code.push(IRETURN);
    // 0x808080 + 0x80*0x10000
    assert_eq!(run_main_int(&mut pool, 6, 3, code), 0x808080 + 0x80 * 0x10000);
}

#[test]
fn color_and_stroke_validation_throws() {
    // setColor(256,0,0) / setGrayScale(300) / setStrokeStyle(2) → IAE。
    let bad: &[(&str, &str, [i32; 3])] = &[
        ("setColor", "(III)V", [256, 0, 0]),
        ("setGrayScale", "(I)V", [300, 0, 0]),
        ("setStrokeStyle", "(I)V", [2, 0, 0]),
    ];
    for (name, desc, args) in bad {
        let mut pool = Pool::new();
        let mref = pool.methodref(GRAPHICS, name, desc);
        let mut code = code_new_graphics(&mut pool, 4, 4);
        code.push(0x2c); // aload_2（g）
        for a in args.iter().take(if *desc == "(I)V" { 1 } else { 3 }) {
            code.extend(push_int(&mut pool, *a));
        }
        code.extend(op3(INVOKEVIRTUAL, mref));
        code.push(ICONST_0);
        code.push(IRETURN);
        let thrown = run_main_thrown(&mut pool, 6, 3, code);
        assert_eq!(thrown, "java/lang/IllegalArgumentException", "{name}");
    }
}

// ---- fillRect / clipRect / translate ---------------------------------------

#[test]
fn fill_rect_exact_pixels() {
    let mut pool = Pool::new();
    let mut code = code_new_graphics(&mut pool, 8, 6);
    code.extend(code_set_color(&mut pool, 0xFF_FFFF));
    code.extend(code_call4(&mut pool, "fillRect", 1, 1, 2, 2));
    code.extend(code_return_pixels(&mut pool));
    let (vm, arr) = run_main_ref(&mut pool, 6, 3, code);
    // 2x2 精确落点（x..x+w-1）。
    for (x, y) in [(1, 1), (2, 1), (1, 2), (2, 2)] {
        assert_eq!(px(&vm, arr, 8, x, y), WHITE, "({x},{y}) 应白");
    }
    for (x, y) in [(0, 0), (3, 3), (1, 0), (0, 1), (3, 2), (2, 3)] {
        assert_eq!(px(&vm, arr, 8, x, y), 0, "({x},{y}) 应保持 0");
    }
}

#[test]
fn fill_rect_clip_intersect() {
    // 全图基色 → clipRect(2,2,4,4) → 重填全图：仅交内变白。
    let mut pool = Pool::new();
    // 基色落屏带不透明位（0xAARRGGBB）。
    let base = 0xFFFF00FFu32 as i32;
    let mut code = code_new_graphics(&mut pool, 8, 6);
    code.extend(code_set_color(&mut pool, base));
    code.extend(code_call4(&mut pool, "fillRect", 0, 0, 8, 6));
    code.extend(code_call4(&mut pool, "clipRect", 2, 2, 4, 4));
    code.extend(code_set_color(&mut pool, 0xFF_FFFF));
    code.extend(code_call4(&mut pool, "fillRect", 0, 0, 8, 6));
    code.extend(code_return_pixels(&mut pool));
    let (vm, arr) = run_main_ref(&mut pool, 6, 3, code);
    assert_eq!(px(&vm, arr, 8, 2, 2), WHITE, "交内左上");
    assert_eq!(px(&vm, arr, 8, 5, 5), WHITE, "交内右下（x2/y2 排他）");
    assert_eq!(px(&vm, arr, 8, 6, 5), base, "交外保持基色");
    assert_eq!(px(&vm, arr, 8, 0, 0), base, "角外保持基色");
}

#[test]
fn fill_rect_translate_offset() {
    // translate 后 fillRect(0,0,2,2) 实际落 (2,1)..
    let mut pool = Pool::new();
    let tr = pool.methodref(GRAPHICS, "translate", "(II)V");
    let mut code = code_new_graphics(&mut pool, 8, 6);
    code.push(0x2c); // aload_2（g）
    code.extend(push_int(&mut pool, 2));
    code.push(ICONST_1);
    code.extend(op3(INVOKEVIRTUAL, tr));
    code.extend(code_set_color(&mut pool, 0xFF_FFFF));
    code.extend(code_call4(&mut pool, "fillRect", 0, 0, 2, 2));
    code.extend(code_return_pixels(&mut pool));
    let (vm, arr) = run_main_ref(&mut pool, 6, 3, code);
    assert_eq!(px(&vm, arr, 8, 2, 1), WHITE);
    assert_eq!(px(&vm, arr, 8, 3, 2), WHITE);
    assert_eq!(px(&vm, arr, 8, 1, 1), 0);
    assert_eq!(px(&vm, arr, 8, 2, 0), 0);
}

#[test]
fn translate_accumulates() {
    let mut pool = Pool::new();
    let tr = pool.methodref(GRAPHICS, "translate", "(II)V");
    let tx = pool.methodref(GRAPHICS, "getTranslateX", "()I");
    let ty = pool.methodref(GRAPHICS, "getTranslateY", "()I");
    let mut code = code_new_graphics(&mut pool, 4, 4);
    for (x, y) in [(2, 3), (1, 1)] {
        code.push(0x2c); // aload_2（g）
        code.extend(push_int(&mut pool, x));
        code.extend(push_int(&mut pool, y));
        code.extend(op3(INVOKEVIRTUAL, tr));
    }
    code.push(0x2c); // aload_2（g）
    code.extend(op3(INVOKEVIRTUAL, tx));
    code.push(0x2c); // aload_2（g）
    code.extend(op3(INVOKEVIRTUAL, ty));
    code.extend(push_int(&mut pool, 10));
    code.push(IMUL);
    code.push(IADD);
    code.push(IRETURN);
    assert_eq!(run_main_int(&mut pool, 6, 3, code), 3 + 4 * 10);
}

#[test]
fn clip_rect_intersect_and_getters() {
    // clipRect(2,2,8,8) 与全图 (0,0,8,6) 求交 → (2,2,6,4)。
    let mut pool = Pool::new();
    let mut code = code_new_graphics(&mut pool, 8, 6);
    code.extend(code_call4(&mut pool, "clipRect", 2, 2, 8, 8));
    let combos: &[(&str, i32)] = &[
        ("getClipX", 1),
        ("getClipY", 10),
        ("getClipWidth", 100),
        ("getClipHeight", 1000),
    ];
    for (i, (name, mul)) in combos.iter().enumerate() {
        let mref = pool.methodref(GRAPHICS, name, "()I");
        code.push(0x2c); // aload_2（g）
        code.extend(op3(INVOKEVIRTUAL, mref));
        code.extend(push_int(&mut pool, *mul));
        code.push(IMUL);
        if i > 0 {
            code.push(IADD);
        }
    }
    code.push(IRETURN);
    assert_eq!(run_main_int(&mut pool, 6, 3, code), 2 + 2 * 10 + 6 * 100 + 4 * 1000);

    // 再交不相交矩形 → 空裁剪（宽 0）。
    let mut pool = Pool::new();
    let w_ref = pool.methodref(GRAPHICS, "getClipWidth", "()I");
    let mut code = code_new_graphics(&mut pool, 8, 6);
    code.extend(code_call4(&mut pool, "clipRect", 2, 2, 8, 8));
    code.extend(code_call4(&mut pool, "clipRect", 0, 0, 1, 6));
    code.push(0x2c); // aload_2（g）
    code.extend(op3(INVOKEVIRTUAL, w_ref));
    code.push(IRETURN);
    assert_eq!(run_main_int(&mut pool, 6, 3, code), 0);
}

// ---- drawLine / drawRect ---------------------------------------------------

#[test]
fn draw_line_endpoints() {
    let mut pool = Pool::new();
    let line = pool.methodref(GRAPHICS, "drawLine", "(IIII)V");
    let mut code = code_new_graphics(&mut pool, 8, 6);
    code.extend(code_set_color(&mut pool, 0xFF_FFFF));
    code.push(0x2c); // aload_2（g）
    code.extend(push_int(&mut pool, 1));
    code.extend(push_int(&mut pool, 1));
    code.extend(push_int(&mut pool, 4));
    code.extend(push_int(&mut pool, 3));
    code.extend(op3(INVOKEVIRTUAL, line));
    code.extend(code_return_pixels(&mut pool));
    let (vm, arr) = run_main_ref(&mut pool, 6, 3, code);
    assert_eq!(px(&vm, arr, 8, 1, 1), WHITE, "端点 1");
    assert_eq!(px(&vm, arr, 8, 4, 3), WHITE, "端点 2");
    assert_eq!(px(&vm, arr, 8, 0, 0), 0);
    assert_eq!(px(&vm, arr, 8, 5, 4), 0);
}

#[test]
fn draw_rect_wider_than_fill() {
    // 同参 4x3：drawRect 占 5x4 边框（w+1×h+1），fillRect 占 4x3。
    let mut pool = Pool::new();
    let mut code = code_new_graphics(&mut pool, 6, 5);
    code.extend(code_set_color(&mut pool, 0xFF_FFFF));
    code.extend(code_call4(&mut pool, "drawRect", 0, 0, 4, 3));
    code.extend(code_return_pixels(&mut pool));
    let (vm, arr) = run_main_ref(&mut pool, 6, 3, code);
    for (x, y) in [(0, 0), (2, 0), (4, 0), (0, 3), (4, 3), (2, 3), (0, 1), (4, 2)] {
        assert_eq!(px(&vm, arr, 6, x, y), WHITE, "边框 ({x},{y})");
    }
    for (x, y) in [(1, 1), (3, 2)] {
        assert_eq!(px(&vm, arr, 6, x, y), 0, "内部 ({x},{y})");
    }

    let mut pool = Pool::new();
    let mut code = code_new_graphics(&mut pool, 6, 5);
    code.extend(code_set_color(&mut pool, 0xFF_FFFF));
    code.extend(code_call4(&mut pool, "fillRect", 0, 0, 4, 3));
    code.extend(code_return_pixels(&mut pool));
    let (vm, arr) = run_main_ref(&mut pool, 6, 3, code);
    assert_eq!(px(&vm, arr, 6, 0, 0), WHITE);
    assert_eq!(px(&vm, arr, 6, 3, 2), WHITE, "x+w-1/h-1 右下含");
    assert_eq!(px(&vm, arr, 6, 4, 3), 0, "fillRect 不含 (4,3)");
}

// ---- 文本（mock 合成矩形） --------------------------------------------------

#[test]
fn draw_string_top_baseline_and_hcenter() {
    // 默认字体：height=16、ascent=12、text_width("AB")=12。
    let magenta = 0xFFFF00FFu32 as i32;
    // TOP：矩形 (2,5)-(14,21)。
    let mut pool = Pool::new();
    let draw = pool.methodref(GRAPHICS, "drawString", "(Ljava/lang/String;III)V");
    let text = pool.string("AB");
    let mut code = code_new_graphics(&mut pool, 32, 32);
    code.extend(code_set_color(&mut pool, 0xFF00FF));
    code.push(0x2c); // aload_2（g）
    code.extend(op3(LDC_W, text));
    code.extend(push_int(&mut pool, 2));
    code.extend(push_int(&mut pool, 5));
    code.extend(push_int(&mut pool, 20)); // LEFT|TOP
    code.extend(op3(INVOKEVIRTUAL, draw));
    code.extend(code_return_pixels(&mut pool));
    let (vm, arr) = run_main_ref(&mut pool, 6, 3, code);
    assert_eq!(px(&vm, arr, 32, 2, 5), magenta, "左上");
    assert_eq!(px(&vm, arr, 32, 13, 20), magenta, "右下（w=12,h=16 排他端）");
    assert_eq!(px(&vm, arr, 32, 14, 5), 0, "宽外");
    assert_eq!(px(&vm, arr, 32, 2, 21), 0, "高外");
    assert_eq!(px(&vm, arr, 32, 1, 5), 0, "左外");

    // BASELINE：y 上移 ascent=12 → 矩形 (0,8)-(12,24)；
    // 与 TOP(y=20) 的 y 差恰为 ascent。
    let mut pool = Pool::new();
    let text = pool.string("AB");
    let draw = pool.methodref(GRAPHICS, "drawString", "(Ljava/lang/String;III)V");
    let mut code = code_new_graphics(&mut pool, 32, 32);
    code.extend(code_set_color(&mut pool, 0xFF00FF));
    code.push(0x2c); // aload_2（g）
    code.extend(op3(LDC_W, text));
    code.push(ICONST_0);
    code.extend(push_int(&mut pool, 20));
    code.extend(push_int(&mut pool, 68)); // LEFT|BASELINE
    code.extend(op3(INVOKEVIRTUAL, draw));
    code.extend(code_return_pixels(&mut pool));
    let (vm, arr) = run_main_ref(&mut pool, 6, 3, code);
    assert_eq!(px(&vm, arr, 32, 0, 8), magenta, "BASELINE 顶 = y-ascent");
    assert_eq!(px(&vm, arr, 32, 0, 23), magenta, "底部含 23");
    assert_eq!(px(&vm, arr, 32, 0, 24), 0, "高外");
    assert_eq!(px(&vm, arr, 32, 0, 19), magenta, "TOP 情形下 19 未写、BASELINE 下已写");

    // HCENTER：x=6 修正 -6 → 左上 (0,20)。
    let mut pool = Pool::new();
    let text = pool.string("AB");
    let draw = pool.methodref(GRAPHICS, "drawString", "(Ljava/lang/String;III)V");
    let mut code = code_new_graphics(&mut pool, 32, 32);
    code.extend(code_set_color(&mut pool, 0xFF00FF));
    code.push(0x2c); // aload_2（g）
    code.extend(op3(LDC_W, text));
    code.extend(push_int(&mut pool, 6));
    code.extend(push_int(&mut pool, 20));
    code.extend(push_int(&mut pool, 17)); // HCENTER|TOP
    code.extend(op3(INVOKEVIRTUAL, draw));
    code.extend(code_return_pixels(&mut pool));
    let (vm, arr) = run_main_ref(&mut pool, 6, 3, code);
    assert_eq!(px(&vm, arr, 32, 0, 20), magenta, "HCENTER x 修正");
    assert_eq!(px(&vm, arr, 32, 12, 20), 0);
}

#[test]
fn draw_string_bad_anchor_and_null_throw() {
    // 文本锚点 VCENTER 非法 → IAE；null 文本 → NPE。
    let bad: &[(&str, i32, &str)] = &[
        ("anchor", 6, "java/lang/IllegalArgumentException"),
        ("null", 20, "java/lang/NullPointerException"),
    ];
    for (case, anchor, expected) in bad {
        let mut pool = Pool::new();
        let draw = pool.methodref(GRAPHICS, "drawString", "(Ljava/lang/String;III)V");
        let mut code = code_new_graphics(&mut pool, 8, 8);
        code.push(0x2c); // aload_2（g）
        if *case == "null" {
            code.push(ACONST_NULL);
        } else {
            let text = pool.string("A");
            code.extend(op3(LDC_W, text));
        }
        code.push(ICONST_0);
        code.push(ICONST_0);
        code.extend(push_int(&mut pool, *anchor));
        code.extend(op3(INVOKEVIRTUAL, draw));
        code.push(ICONST_0);
        code.push(IRETURN);
        let thrown = run_main_thrown(&mut pool, 6, 3, code);
        assert_eq!(&thrown, expected, "case {case}");
    }
}

#[test]
fn draw_char_and_substring() {
    // drawChar：宽 6、高 16 的单码元矩形。
    let mut pool = Pool::new();
    let draw = pool.methodref(GRAPHICS, "drawChar", "(CIII)V");
    let mut code = code_new_graphics(&mut pool, 16, 20);
    code.extend(code_set_color(&mut pool, 0xFF_FFFF));
    code.push(0x2c); // aload_2（g）
    code.push(BIPUSH);
    code.push(b'A');
    code.extend(push_int(&mut pool, 3));
    code.extend(push_int(&mut pool, 4));
    code.extend(push_int(&mut pool, 20));
    code.extend(op3(INVOKEVIRTUAL, draw));
    code.extend(code_return_pixels(&mut pool));
    let (vm, arr) = run_main_ref(&mut pool, 6, 3, code);
    assert_eq!(px(&vm, arr, 16, 3, 4), WHITE);
    assert_eq!(px(&vm, arr, 16, 8, 19), WHITE);
    assert_eq!(px(&vm, arr, 16, 9, 4), 0);

    // drawSubstring("AB",0,1,...)：等价单字符 "A"。
    let mut pool = Pool::new();
    let draw = pool.methodref(GRAPHICS, "drawSubstring", "(Ljava/lang/String;IIIII)V");
    let text = pool.string("AB");
    let mut code = code_new_graphics(&mut pool, 16, 20);
    code.extend(code_set_color(&mut pool, 0xFF_FFFF));
    code.push(0x2c); // aload_2（g）
    code.extend(op3(LDC_W, text));
    code.push(ICONST_0);
    code.push(ICONST_1);
    code.push(ICONST_0);
    code.push(ICONST_0);
    code.extend(push_int(&mut pool, 20));
    code.extend(op3(INVOKEVIRTUAL, draw));
    code.extend(code_return_pixels(&mut pool));
    let (vm, arr) = run_main_ref(&mut pool, 6, 3, code);
    assert_eq!(px(&vm, arr, 16, 0, 0), WHITE);
    assert_eq!(px(&vm, arr, 16, 5, 15), WHITE);
    assert_eq!(px(&vm, arr, 16, 6, 0), 0);
}

#[test]
fn set_font_null_restores_default() {
    // setFont(null) 合法（恢复默认字体）→ getFont() 非 null。
    let mut pool = Pool::new();
    let set = pool.methodref(GRAPHICS, "setFont", "(Ljavax/microedition/lcdui/Font;)V");
    let get = pool.methodref(GRAPHICS, "getFont", "()Ljavax/microedition/lcdui/Font;");
    let mut code = code_new_graphics(&mut pool, 4, 4);
    code.push(0x2c); // aload_2（g）
    code.push(ACONST_NULL);
    code.extend(op3(INVOKEVIRTUAL, set));
    code.push(0x2c); // aload_2（g）
    code.extend(op3(INVOKEVIRTUAL, get));
    code.push(ARETURN);
    let (_vm, font) = run_main_ref(&mut pool, 6, 3, code);
    assert_ne!(font, ObjRef::NULL);
}

// ---- drawImage / drawRGB ---------------------------------------------------

#[test]
fn draw_image_blit_anchor_and_clip() {
    // 源 2x2 不可变图 blit 到左上角：整幅直拷。
    let mut pool = Pool::new();
    let draw = pool.methodref(GRAPHICS, "drawImage", "(Ljavax/microedition/lcdui/Image;III)V");
    let mut code = code_new_graphics(&mut pool, 8, 8);
    code.extend(code_new_rgb_source(&mut pool, 3)); // 源图 local 3
    code.push(0x2c); // aload_2（g） // g
    code.push(ALOAD);
    code.push(3);
    code.push(ICONST_0);
    code.push(ICONST_0);
    code.extend(push_int(&mut pool, 20)); // LEFT|TOP
    code.extend(op3(INVOKEVIRTUAL, draw));
    code.extend(code_return_pixels(&mut pool));
    let (vm, arr) = run_main_ref(&mut pool, 8, 4, code);
    assert_eq!(px(&vm, arr, 8, 0, 0), 0xFF112233u32 as i32, "首像素");
    assert_eq!(px(&vm, arr, 8, 1, 0), 0xFF445566u32 as i32);
    assert_eq!(px(&vm, arr, 8, 0, 1), 0xFF778899u32 as i32);
    assert_eq!(px(&vm, arr, 8, 1, 1), 0xFFAABBCCu32 as i32);
    assert_eq!(px(&vm, arr, 8, 3, 3), 0, "blit 区外未写");

    // anchor HCENTER|VCENTER：(4,4) 修正 -1,-1 → (3,3)。
    let mut pool = Pool::new();
    let draw = pool.methodref(GRAPHICS, "drawImage", "(Ljavax/microedition/lcdui/Image;III)V");
    let mut code = code_new_graphics(&mut pool, 8, 8);
    code.extend(code_new_rgb_source(&mut pool, 3));
    code.push(0x2c); // aload_2（g）
    code.push(ALOAD);
    code.push(3);
    code.extend(push_int(&mut pool, 4));
    code.extend(push_int(&mut pool, 4));
    code.push(ICONST_3); // HCENTER|VCENTER
    code.extend(op3(INVOKEVIRTUAL, draw));
    code.extend(code_return_pixels(&mut pool));
    let (vm, arr) = run_main_ref(&mut pool, 8, 4, code);
    assert_eq!(px(&vm, arr, 8, 3, 3), 0xFF112233u32 as i32, "中心锚点修正");
    assert_eq!(px(&vm, arr, 8, 2, 3), 0);

    // 越界裁剪：x=7 处 2 宽 blit 仅首列落界内，不越界不 panic。
    let mut pool = Pool::new();
    let draw = pool.methodref(GRAPHICS, "drawImage", "(Ljavax/microedition/lcdui/Image;III)V");
    let mut code = code_new_graphics(&mut pool, 8, 8);
    code.extend(code_new_rgb_source(&mut pool, 3));
    code.push(0x2c); // aload_2（g）
    code.push(ALOAD);
    code.push(3);
    code.extend(push_int(&mut pool, 7));
    code.push(ICONST_0);
    code.extend(push_int(&mut pool, 20));
    code.extend(op3(INVOKEVIRTUAL, draw));
    code.extend(code_return_pixels(&mut pool));
    let (vm, arr) = run_main_ref(&mut pool, 8, 4, code);
    assert_eq!(px(&vm, arr, 8, 7, 0), 0xFF112233u32 as i32, "界内首列");
    assert_eq!(px(&vm, arr, 8, 6, 0), 0);
}

#[test]
fn draw_image_anchor_and_null_throw() {
    // 图像锚点 BASELINE 非法 → IAE；null 图 → NPE。
    for (anchor, use_null, expected) in
        [(64i32, false, "java/lang/IllegalArgumentException"), (20, true, "java/lang/NullPointerException")]
    {
        let mut pool = Pool::new();
        let draw = pool.methodref(GRAPHICS, "drawImage", "(Ljavax/microedition/lcdui/Image;III)V");
        let mut code = code_new_graphics(&mut pool, 8, 8);
        code.push(0x2c); // aload_2（g）
        if use_null {
            code.push(ACONST_NULL);
        } else {
            code.extend(code_new_rgb_source(&mut pool, 3));
            code.push(ALOAD);
            code.push(3);
        }
        code.push(ICONST_0);
        code.push(ICONST_0);
        code.extend(push_int(&mut pool, anchor));
        code.extend(op3(INVOKEVIRTUAL, draw));
        code.push(ICONST_0);
        code.push(IRETURN);
        let thrown = run_main_thrown(&mut pool, 8, 4, code);
        assert_eq!(thrown, expected);
    }
}

#[test]
fn draw_rgb_blit_respects_alpha_flag() {
    // drawRGB(rgb[4], 0, 2, 0, 0, 2, 2, false)：alpha 忽略 → 全不透明。
    let mut pool = Pool::new();
    let draw = pool.methodref(GRAPHICS, "drawRGB", "([IIIIIIIZ)V");
    let mut code = code_new_graphics(&mut pool, 4, 4);
    code.push(ICONST_4);
    code.push(NEWARRAY);
    code.push(T_INT);
    code.push(ASTORE);
    code.push(3);
    code.extend(code_fill_ints(
        &mut pool,
        3,
        &[0x0011_2233, 0x0044_5566, 0x0077_8899, 0x00AA_BBCC],
    ));
    code.push(0x2c); // aload_2（g）
    code.push(ALOAD);
    code.push(3);
    code.push(ICONST_0); // offset
    code.push(ICONST_2); // scanlength
    code.push(ICONST_0); // x
    code.push(ICONST_0); // y
    code.push(ICONST_2); // w
    code.push(ICONST_2); // h
    code.push(ICONST_0); // processAlpha=false
    code.extend(op3(INVOKEVIRTUAL, draw));
    code.extend(code_return_pixels(&mut pool));
    let (vm, arr) = run_main_ref(&mut pool, 8, 4, code);
    assert_eq!(px(&vm, arr, 4, 0, 0), 0xFF112233u32 as i32);
    assert_eq!(px(&vm, arr, 4, 1, 1), 0xFFAABBCCu32 as i32);
}

// ---- 弧/圆角/三角形 ---------------------------------------------------------

#[test]
fn arc_fill_center_and_draw_edge() {
    let mut pool = Pool::new();
    let fill = pool.methodref(GRAPHICS, "fillArc", "(IIIIII)V");
    let mut code = code_new_graphics(&mut pool, 16, 16);
    code.extend(code_set_color(&mut pool, 0xFF_FFFF));
    code.push(0x2c); // aload_2（g）
    for v in [0, 0, 10, 10, 0, 180] {
        code.extend(push_int(&mut pool, v));
    }
    code.extend(op3(INVOKEVIRTUAL, fill));
    code.extend(code_return_pixels(&mut pool));
    let (vm, arr) = run_main_ref(&mut pool, 6, 3, code);
    // 0→180 顺时针经 6 点钟方向：覆盖下半（含中心）。
    assert_eq!(px(&vm, arr, 16, 5, 5), WHITE, "中心");
    assert_eq!(px(&vm, arr, 16, 8, 7), WHITE, "右下象限");
    assert_eq!(px(&vm, arr, 16, 5, 0), 0, "上半中心不填");
    assert_eq!(px(&vm, arr, 16, 2, 2), 0, "左上象限不填");

    // drawArc 边线：0°/90°/180° 三端点着色、内部不着。
    let mut pool = Pool::new();
    let draw = pool.methodref(GRAPHICS, "drawArc", "(IIIIII)V");
    let mut code = code_new_graphics(&mut pool, 16, 16);
    code.extend(code_set_color(&mut pool, 0xFF_FFFF));
    code.push(0x2c); // aload_2（g）
    for v in [0, 0, 10, 10, 0, 180] {
        code.extend(push_int(&mut pool, v));
    }
    code.extend(op3(INVOKEVIRTUAL, draw));
    code.extend(code_return_pixels(&mut pool));
    let (vm, arr) = run_main_ref(&mut pool, 6, 3, code);
    assert_eq!(px(&vm, arr, 16, 10, 5), WHITE, "0° 端点（3 点钟）");
    assert_eq!(px(&vm, arr, 16, 5, 10), WHITE, "90° 端点（6 点钟）");
    assert_eq!(px(&vm, arr, 16, 0, 5), WHITE, "180° 端点（9 点钟）");
    assert_eq!(px(&vm, arr, 16, 5, 5), 0, "内部不着色");
}

#[test]
fn round_rect_fill_and_draw_corners() {
    let mut pool = Pool::new();
    let fill = pool.methodref(GRAPHICS, "fillRoundRect", "(IIIIII)V");
    let mut code = code_new_graphics(&mut pool, 16, 16);
    code.extend(code_set_color(&mut pool, 0xFF_FFFF));
    code.push(0x2c); // aload_2（g）
    for v in [0, 0, 10, 10, 4, 4] {
        code.extend(push_int(&mut pool, v));
    }
    code.extend(op3(INVOKEVIRTUAL, fill));
    code.extend(code_return_pixels(&mut pool));
    let (vm, arr) = run_main_ref(&mut pool, 6, 3, code);
    assert_eq!(px(&vm, arr, 16, 5, 5), WHITE, "中心");
    assert_eq!(px(&vm, arr, 16, 5, 0), WHITE, "顶边中段");
    assert_eq!(px(&vm, arr, 16, 0, 5), WHITE, "左边中段");
    assert_eq!(px(&vm, arr, 16, 0, 0), 0, "左上圆角切除");
    assert_eq!(px(&vm, arr, 16, 0, 1), 0, "近角行收缩");

    // drawRoundRect：完全圆角（8x8 半径 4）退化为圆。
    let mut pool = Pool::new();
    let draw = pool.methodref(GRAPHICS, "drawRoundRect", "(IIIIII)V");
    let mut code = code_new_graphics(&mut pool, 16, 16);
    code.extend(code_set_color(&mut pool, 0xFF_FFFF));
    code.push(0x2c); // aload_2（g）
    for v in [0, 0, 8, 8, 4, 4] {
        code.extend(push_int(&mut pool, v));
    }
    code.extend(op3(INVOKEVIRTUAL, draw));
    code.extend(code_return_pixels(&mut pool));
    let (vm, arr) = run_main_ref(&mut pool, 6, 3, code);
    assert_eq!(px(&vm, arr, 16, 4, 0), WHITE, "顶弧中点");
    assert_eq!(px(&vm, arr, 16, 0, 4), WHITE, "左弧中点");
    assert_eq!(px(&vm, arr, 16, 4, 4), 0, "内部");
    assert_eq!(px(&vm, arr, 16, 0, 0), 0, "角内缩");
    assert_eq!(px(&vm, arr, 16, 1, 1), WHITE, "对角弧上（225°）");
}

#[test]
fn fill_triangle_smoke() {
    let mut pool = Pool::new();
    let fill = pool.methodref(GRAPHICS, "fillTriangle", "(IIIIII)V");
    let mut code = code_new_graphics(&mut pool, 8, 8);
    code.extend(code_set_color(&mut pool, 0xFF_FFFF));
    code.push(0x2c); // aload_2（g）
    for v in [0, 0, 6, 0, 0, 6] {
        code.extend(push_int(&mut pool, v));
    }
    code.extend(op3(INVOKEVIRTUAL, fill));
    code.extend(code_return_pixels(&mut pool));
    let (vm, arr) = run_main_ref(&mut pool, 6, 3, code);
    assert_eq!(px(&vm, arr, 8, 0, 0), WHITE, "顶点");
    assert_eq!(px(&vm, arr, 8, 1, 1), WHITE, "内部");
    assert_eq!(px(&vm, arr, 8, 4, 1), WHITE, "斜边上（中心恰在边线）");
    assert_eq!(px(&vm, arr, 8, 5, 1), 0, "中心越边为形外");
    assert_eq!(px(&vm, arr, 8, 6, 0), 0, "顶点像素中心越边（形外）");
    assert_eq!(px(&vm, arr, 8, 6, 6), 0, "形外");
    assert_eq!(px(&vm, arr, 8, 3, 5), 0, "形外 2");
}

// ---- T022 汇合补漏：颜色掩码 / 零长线 / 平移后裁剪 / 自绘 -------------------

/// setColor 负值：int 按 24 位 RGB 掩码解释（-1 = 0xFFFFFFFF → 0xFFFFFF），
/// 与 MIDP javadoc "24 位 color 值" 一致（不抛异常、高位丢弃）。
#[test]
fn set_color_negative_masks_to_24bit() {
    let mut pool = Pool::new();
    let get = pool.methodref(GRAPHICS, "getColor", "()I");
    let mut code = code_new_graphics(&mut pool, 2, 2);
    code.extend(code_set_color(&mut pool, -1));
    code.push(0x2c); // aload_2（g）
    code.extend(op3(INVOKEVIRTUAL, get));
    code.push(IRETURN);
    assert_eq!(
        run_main_int(&mut pool, 6, 3, code),
        0xFF_FFFF,
        "setColor(-1) 应掩码为 0x00FFFFFF"
    );
}

/// drawLine 零长（x1==x2 且 y1==y2）：恰好落一个点（Bresenham 首点即终点）。
#[test]
fn draw_line_zero_length_plots_single_point() {
    let mut pool = Pool::new();
    let mut code = code_new_graphics(&mut pool, 4, 4);
    code.extend(code_set_color(&mut pool, 0xFF_0000));
    code.extend(code_call4(&mut pool, "drawLine", 1, 2, 1, 2));
    code.extend(code_return_pixels(&mut pool));
    let (vm, arr) = run_main_ref(&mut pool, 6, 3, code);
    let red = 0xFFFF_0000u32 as i32;
    assert_eq!(px(&vm, arr, 4, 1, 2), red, "(1,2) 应落单点");
    for y in 0..4 {
        for x in 0..4 {
            if (x, y) != (1, 2) {
                assert_eq!(px(&vm, arr, 4, x, y), 0, "({x},{y}) 不应有像素");
            }
        }
    }
}

/// translate 后 clipRect：裁剪矩形按当前平移解释（MIDP javadoc——
/// "矩形在当前坐标系统中"）→ translate(10,10)+clipRect(2,2,4,4) 的
/// 有效裁剪为设备坐标 [12,16)²；随后的全幅 fillRect 仅在该区域落墨。
#[test]
fn clip_rect_in_translated_space_bounds_later_fill() {
    let mut pool = Pool::new();
    let translate = pool.methodref(GRAPHICS, "translate", "(II)V");
    let mut code = code_new_graphics(&mut pool, 20, 20);
    code.push(0x2c); // aload_2（g）
    code.extend(push_int(&mut pool, 10));
    code.extend(push_int(&mut pool, 10));
    code.extend(op3(INVOKEVIRTUAL, translate));
    code.extend(code_call4(&mut pool, "clipRect", 2, 2, 4, 4));
    code.extend(code_set_color(&mut pool, 0xFF_0000));
    code.extend(code_call4(&mut pool, "fillRect", 0, 0, 20, 20));
    code.extend(code_return_pixels(&mut pool));
    let (vm, arr) = run_main_ref(&mut pool, 8, 3, code);
    let red = 0xFFFF_0000u32 as i32;
    // 有效区域四角与内部。
    for (x, y) in [(12, 12), (15, 15), (13, 14), (15, 12)] {
        assert_eq!(px(&vm, arr, 20, x, y), red, "({x},{y}) 应在裁剪内变红");
    }
    // 裁剪外一律未写（含平移原点 10,10 与裁剪边界外 11/16）。
    for (x, y) in [(10, 10), (11, 12), (16, 12), (12, 11), (12, 16), (0, 0), (19, 19)] {
        assert_eq!(px(&vm, arr, 20, x, y), 0, "({x},{y}) 应被裁剪挡住");
    }
}

/// drawImage 源=目标（可变图画到自身）：实现先整幅快照源像素再 blit，
/// 自绘应为恒等操作（无撕裂、无半程覆盖），全图保持填充色。
#[test]
fn draw_image_onto_itself_is_identity_snapshot() {
    let mut pool = Pool::new();
    let draw = pool.methodref(GRAPHICS, "drawImage", "(Ljavax/microedition/lcdui/Image;III)V");
    let mut code = code_new_graphics(&mut pool, 4, 4); // img=local1, g=local2（目标即 img）
    code.extend(code_set_color(&mut pool, 0xFF_0000));
    code.extend(code_call4(&mut pool, "fillRect", 0, 0, 4, 4));
    code.push(0x2c); // aload_2（g）
    code.push(0x2b); // aload_1（img，源==目标）
    code.push(ICONST_0);
    code.push(ICONST_0);
    code.extend(push_int(&mut pool, 20)); // LEFT|TOP
    code.extend(op3(INVOKEVIRTUAL, draw));
    code.extend(code_return_pixels(&mut pool));
    let (vm, arr) = run_main_ref(&mut pool, 6, 3, code);
    let red = 0xFFFF_0000u32 as i32;
    for y in 0..4 {
        for x in 0..4 {
            assert_eq!(px(&vm, arr, 4, x, y), red, "({x},{y}) 自绘后应保持红（快照语义）");
        }
    }
}
