//! LCDUI Font 原生实现测试（T019）：Host 轨端到端——构码经
//! RegistryDispatch 分发到 `midp::lcdui::font`，FontHandle/度量/测宽
//! 全部取自 MockHostContext 合成字体。
//!
//! Mock 合成规则（host/mock.rs）：size 档 0/1/2 → 基准 px 12/16/20；
//! height=px+4、ascent=px；text_width = 码元数 × (px/2)。
//!
//! 构码铁律（前车之鉴）：带参构造 NEW+DUP+参数+INVOKESPECIAL（javac 形态）；
//! 分支指令消耗条件值后不再 POP；long/double 占两槽；操作数池先取号再构码。

#[allow(dead_code)]
mod common;

use common::classbytes::{build_class, provider, MethodSpec, Pool};
use skyengine_kvm::host::mock::MockHostContext;
use skyengine_kvm::interp::natives::RegistryDispatch;
use skyengine_kvm::interp::{enter, run, runtime_class_of, Machine, RunOutcome, Unwind};
use skyengine_kvm::runtime::classspace::ClassSpace;
use skyengine_kvm::runtime::heap::Heap;
use skyengine_kvm::runtime::monitor::Monitors;
use skyengine_kvm::runtime::{Slot, ThreadId};
use skyengine_kvm::KvmError;

// ---- 操作码（仅本文件用到的） --------------------------------------------

const ICONST_0: u8 = 0x03;
const ICONST_1: u8 = 0x04;
const ICONST_3: u8 = 0x06;
const ICONST_5: u8 = 0x08;
const ICONST_2: u8 = 0x05;
const BIPUSH: u8 = 0x10;
const LDC_W: u8 = 0x13;
const IF_ACMPNE: u8 = 0xa6;
const IRETURN: u8 = 0xac;
const GETSTATIC: u8 = 0xb2;
const INVOKEVIRTUAL: u8 = 0xb6;
const INVOKESTATIC: u8 = 0xb8;

/// 主线程标识（与其他测试夹具一致）。
const MAIN: ThreadId = ThreadId(1);

const FONT: &str = "javax/microedition/lcdui/Font";

// ---- 夹具（midp_image 同型：真注册表 + Mock 宿主） -------------------------

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

/// 构码并执行 main，断言未捕获异常并返回类名。
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

/// 字体方法引用速记。
fn font_static(pool: &mut Pool, name: &str, desc: &str) -> u16 {
    pool.methodref(FONT, name, desc)
}

// ---- 默认字体与缓存 -------------------------------------------------------

#[test]
fn default_font_metrics_match_mock_synth() {
    // 默认档（归一化 size 0）：height = 12+4 = 16、baseline = 12。
    let mut pool = Pool::new();
    let get_default = font_static(&mut pool, "getDefaultFont", "()Ljavax/microedition/lcdui/Font;");
    let get_height = font_static(&mut pool, "getHeight", "()I");
    let get_baseline = font_static(&mut pool, "getBaselinePosition", "()I");

    let mut code = Vec::new();
    code.extend_from_slice(&op3(INVOKESTATIC, get_default));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, get_height));
    code.push(IRETURN);
    assert_eq!(run_main_int(&mut pool, 2, 1, code), 16);

    let mut code = Vec::new();
    code.extend_from_slice(&op3(INVOKESTATIC, get_default));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, get_baseline));
    code.push(IRETURN);
    assert_eq!(run_main_int(&mut pool, 2, 1, code), 12);
}

#[test]
fn default_font_returns_cached_instance() {
    // 两次 getDefaultFont 同实例（font_cache 命中）：IF_ACMPNE 消耗两引用。
    let mut pool = Pool::new();
    let get_default = font_static(&mut pool, "getDefaultFont", "()Ljavax/microedition/lcdui/Font;");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(INVOKESTATIC, get_default));
    code.extend_from_slice(&op3(INVOKESTATIC, get_default));
    // 布局：0..5 两次调用；6 分支；9 ICONST_1；10 IRETURN；11 miss。
    code.extend_from_slice(&[IF_ACMPNE, 0x00, 0x05]);
    code.push(ICONST_1);
    code.push(IRETURN);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(run_main_int(&mut pool, 2, 1, code), 1);
}

#[test]
fn get_font_triple_returns_cached_instance() {
    // 同三元组 getFont(SYSTEM, PLAIN, LARGE) 两次同实例。
    let mut pool = Pool::new();
    let get_font = font_static(&mut pool, "getFont", "(III)Ljavax/microedition/lcdui/Font;");
    let mut code = Vec::new();
    for _ in 0..2 {
        code.push(ICONST_0); // FACE_SYSTEM
        code.push(ICONST_0); // STYLE_PLAIN
        code.push(BIPUSH);
        code.push(32); // SIZE_LARGE
        code.extend_from_slice(&op3(INVOKESTATIC, get_font));
    }
    // 布局：每次 1+1+2+3=7 字节，两次调用占 0..13；分支 14..16；17/18；miss 19。
    code.extend_from_slice(&[IF_ACMPNE, 0x00, 0x05]);
    code.push(ICONST_1);
    code.push(IRETURN);
    code.push(ICONST_0);
    code.push(IRETURN);
    assert_eq!(run_main_int(&mut pool, 4, 1, code), 1);
}

// ---- 三元组回读与度量 -----------------------------------------------------

#[test]
fn get_font_triple_readback_and_large_metrics() {
    let mut pool = Pool::new();
    let get_font = font_static(&mut pool, "getFont", "(III)Ljavax/microedition/lcdui/Font;");
    let get_face = font_static(&mut pool, "getFace", "()I");
    let get_style = font_static(&mut pool, "getStyle", "()I");
    let get_size = font_static(&mut pool, "getSize", "()I");
    let get_height = font_static(&mut pool, "getHeight", "()I");
    let get_baseline = font_static(&mut pool, "getBaselinePosition", "()I");

    // getFont(SYSTEM, BOLD, LARGE) → face 0 / style 1 / size 32（MIDP 原值回读）。
    let build = |tail: u16| -> Vec<u8> {
        let mut code = vec![ICONST_0, ICONST_1];
        code.push(BIPUSH);
        code.push(32);
        code.extend_from_slice(&op3(INVOKESTATIC, get_font));
        code.extend_from_slice(&op3(INVOKEVIRTUAL, tail));
        code.push(IRETURN);
        code
    };
    let face = build(get_face);
    assert_eq!(run_main_int(&mut pool, 4, 1, face), 0);
    let style = build(get_style);
    assert_eq!(run_main_int(&mut pool, 4, 1, style), 1);
    let size = build(get_size);
    assert_eq!(run_main_int(&mut pool, 4, 1, size), 32);

    // LARGE 档（size 2 → px 20）：height = 24、baseline = 20。
    let height = build(get_height);
    assert_eq!(run_main_int(&mut pool, 4, 1, height), 24);
    let baseline = build(get_baseline);
    assert_eq!(run_main_int(&mut pool, 4, 1, baseline), 20);
}

#[test]
fn get_font_rejects_invalid_triple() {
    // face=5 非法 → IllegalArgumentException（MIDP javadoc）。
    let mut pool = Pool::new();
    let get_font = font_static(&mut pool, "getFont", "(III)Ljavax/microedition/lcdui/Font;");
    let mut code = Vec::new();
    code.push(ICONST_1);
    code.push(ICONST_1);
    code.push(ICONST_1); // size=1 非 SIZE 常量同样非法，此处 face=1 亦非法
    code.extend_from_slice(&op3(INVOKESTATIC, get_font));
    code.push(IRETURN); // 不到达（异常展开）
    assert_eq!(
        run_main_thrown(&mut pool, 4, 1, code),
        "java/lang/IllegalArgumentException"
    );
}

// ---- 测宽（mock 规则：码元数 × px/2） -------------------------------------

#[test]
fn string_substring_char_width_follow_mock() {
    // 默认档 px=12：每码元 6px。
    let mut pool = Pool::new();
    let get_default = font_static(&mut pool, "getDefaultFont", "()Ljavax/microedition/lcdui/Font;");
    let string_width = font_static(&mut pool, "stringWidth", "(Ljava/lang/String;)I");
    let substring_width =
        font_static(&mut pool, "substringWidth", "(Ljava/lang/String;II)I");
    let char_width = font_static(&mut pool, "charWidth", "(C)I");

    // stringWidth("hello") = 5 × 6 = 30。
    let hello = pool.string("hello");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(INVOKESTATIC, get_default));
    code.extend_from_slice(&op3(LDC_W, hello));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, string_width));
    code.push(IRETURN);
    assert_eq!(run_main_int(&mut pool, 3, 1, code), 30);

    // substringWidth("hello", 1, 3) = "ell" → 18。
    let mut code = Vec::new();
    code.extend_from_slice(&op3(INVOKESTATIC, get_default));
    code.extend_from_slice(&op3(LDC_W, hello));
    code.push(ICONST_1);
    code.push(ICONST_3);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, substring_width));
    code.push(IRETURN);
    assert_eq!(run_main_int(&mut pool, 5, 1, code), 18);

    // charWidth('A') = 6。
    let mut code = Vec::new();
    code.extend_from_slice(&op3(INVOKESTATIC, get_default));
    code.push(BIPUSH);
    code.push(65);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, char_width));
    code.push(IRETURN);
    assert_eq!(run_main_int(&mut pool, 3, 1, code), 6);
}

#[test]
fn substring_width_out_of_bounds_throws() {
    // substringWidth("hello", 0, 6)：end=6 > len=5 → StringIndexOutOfBoundsException。
    let mut pool = Pool::new();
    let get_default = font_static(&mut pool, "getDefaultFont", "()Ljavax/microedition/lcdui/Font;");
    let substring_width =
        font_static(&mut pool, "substringWidth", "(Ljava/lang/String;II)I");
    let hello = pool.string("hello");
    let mut code = Vec::new();
    code.extend_from_slice(&op3(INVOKESTATIC, get_default));
    code.extend_from_slice(&op3(LDC_W, hello));
    code.push(ICONST_0);
    code.push(BIPUSH);
    code.push(6);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, substring_width));
    code.push(IRETURN);
    assert_eq!(
        run_main_thrown(&mut pool, 5, 1, code),
        "java/lang/StringIndexOutOfBoundsException"
    );
}

#[test]
fn large_font_string_width_uses_large_tier() {
    // LARGE 档 px=20：每码元 10px，"abc" → 30（与默认档 5 码元同值可区分来源）。
    let mut pool = Pool::new();
    let get_font = font_static(&mut pool, "getFont", "(III)Ljavax/microedition/lcdui/Font;");
    let string_width = font_static(&mut pool, "stringWidth", "(Ljava/lang/String;)I");
    let abc = pool.string("abc");
    let mut code = vec![ICONST_0, ICONST_0];
    code.push(BIPUSH);
    code.push(32);
    code.extend_from_slice(&op3(INVOKESTATIC, get_font));
    code.extend_from_slice(&op3(LDC_W, abc));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, string_width));
    code.push(IRETURN);
    assert_eq!(run_main_int(&mut pool, 4, 1, code), 30);
}

// ---- style 位判定与类常量 -------------------------------------------------

#[test]
fn style_bits_reported_correctly() {
    // getFont(SYSTEM, BOLD|ITALIC, SMALL)：isBold/isItalic=1，isPlain/isUnderlined=0。
    let mut pool = Pool::new();
    let get_font = font_static(&mut pool, "getFont", "(III)Ljavax/microedition/lcdui/Font;");
    let is_bold = font_static(&mut pool, "isBold", "()Z");
    let is_italic = font_static(&mut pool, "isItalic", "()Z");
    let is_plain = font_static(&mut pool, "isPlain", "()Z");
    let is_underlined = font_static(&mut pool, "isUnderlined", "()Z");

    let build = |tail: u16| -> Vec<u8> {
        let mut code = vec![ICONST_0 /* FACE_SYSTEM */, ICONST_3 /* STYLE_BOLD|STYLE_ITALIC */];
        code.push(BIPUSH);
        code.push(8); // SIZE_SMALL
        code.extend_from_slice(&op3(INVOKESTATIC, get_font));
        code.extend_from_slice(&op3(INVOKEVIRTUAL, tail));
        code.push(IRETURN);
        code
    };
    let bold = build(is_bold);
    assert_eq!(run_main_int(&mut pool, 4, 1, bold), 1);
    let italic = build(is_italic);
    assert_eq!(run_main_int(&mut pool, 4, 1, italic), 1);
    let plain = build(is_plain);
    assert_eq!(run_main_int(&mut pool, 4, 1, plain), 0);
    let underlined = build(is_underlined);
    assert_eq!(run_main_int(&mut pool, 4, 1, underlined), 0);
}

#[test]
fn font_constants_registered() {
    // MIDP-2.0 类常量经静态字段可读（BootstrapConst::Int 注册路径）。
    let cases: Vec<(&str, i32)> = vec![
        ("FACE_SYSTEM", 0),
        ("FACE_PROPORTIONAL", 16),
        ("FACE_MONOSPACE", 32),
        ("STYLE_PLAIN", 0),
        ("STYLE_BOLD", 1),
        ("STYLE_ITALIC", 2),
        ("STYLE_UNDERLINED", 4),
        ("SIZE_SMALL", 8),
        ("SIZE_MEDIUM", 16),
        ("SIZE_LARGE", 32),
    ];
    for (name, want) in cases {
        let mut pool = Pool::new();
        let index = pool.fieldref(FONT, name, "I");
        let mut code = Vec::new();
        code.extend_from_slice(&op3(GETSTATIC, index));
        code.push(IRETURN);
        assert_eq!(run_main_int(&mut pool, 1, 1, code), want, "{name}");
    }
}

// ---- T022 汇合补漏：三档度量切换 + 子串测宽一致性 ---------------------------

/// 三档（SMALL/MEDIUM/LARGE）度量与测宽联动切换：height 与每码元宽度
/// 随档位单调放大，getSize 回读 MIDP 原值（mock 合成规则 px/2 每码元）。
#[test]
fn size_three_tiers_metrics_and_width_scale() {
    // (SIZE 常量值, 期望 height, 每码元宽度)——mock：档位 px 12/16/20 → 6/8/10。
    for (size, height, char_px) in [(8, 16, 6), (16, 20, 8), (32, 24, 10)] {
        // getHeight
        let mut pool = Pool::new();
        let get_font = font_static(&mut pool, "getFont", "(III)Ljavax/microedition/lcdui/Font;");
        let get_height = font_static(&mut pool, "getHeight", "()I");
        let mut code = vec![ICONST_0, ICONST_0, BIPUSH, size as u8];
        code.extend(op3(INVOKESTATIC, get_font));
        code.extend(op3(INVOKEVIRTUAL, get_height));
        code.push(IRETURN);
        assert_eq!(
            run_main_int(&mut pool, 4, 1, code),
            height,
            "size={size} 档 height 应为 {height}"
        );
        // getSize 回读
        let mut pool = Pool::new();
        let get_font = font_static(&mut pool, "getFont", "(III)Ljavax/microedition/lcdui/Font;");
        let get_size = font_static(&mut pool, "getSize", "()I");
        let mut code = vec![ICONST_0, ICONST_0, BIPUSH, size as u8];
        code.extend(op3(INVOKESTATIC, get_font));
        code.extend(op3(INVOKEVIRTUAL, get_size));
        code.push(IRETURN);
        assert_eq!(run_main_int(&mut pool, 4, 1, code), size, "getSize 应回读 {size}");
        // stringWidth("hi") = 2 × 每码元宽
        let mut pool = Pool::new();
        let get_font = font_static(&mut pool, "getFont", "(III)Ljavax/microedition/lcdui/Font;");
        let string_width = font_static(&mut pool, "stringWidth", "(Ljava/lang/String;)I");
        let hi = pool.string("hi");
        let mut code = vec![ICONST_0, ICONST_0, BIPUSH, size as u8];
        code.extend(op3(INVOKESTATIC, get_font));
        code.extend(op3(LDC_W, hi));
        code.extend(op3(INVOKEVIRTUAL, string_width));
        code.push(IRETURN);
        assert_eq!(
            run_main_int(&mut pool, 4, 1, code),
            2 * char_px,
            "size={size} 档 stringWidth(\"hi\") 应为 {}",
            2 * char_px
        );
    }
}

/// substringWidth 与 stringWidth 一致性：子串 [0,len) 即全串前缀——
/// substringWidth(s,0,len) == stringWidth(s)；中段/前缀也按码元数线性。
/// 三档各自验证（宽度规则随档切换后一致性仍成立）。
#[test]
fn substring_width_prefix_matches_string_width() {
    for (size, char_px) in [(8, 6), (16, 8), (32, 10)] {
        // substringWidth("hello",0,5) == stringWidth("hello") == 5×每码元。
        let mut pool = Pool::new();
        let get_font = font_static(&mut pool, "getFont", "(III)Ljavax/microedition/lcdui/Font;");
        let substring_width = font_static(&mut pool, "substringWidth", "(Ljava/lang/String;II)I");
        let hello = pool.string("hello");
        let mut code = vec![ICONST_0, ICONST_0, BIPUSH, size as u8];
        code.extend(op3(INVOKESTATIC, get_font));
        code.extend(op3(LDC_W, hello));
        code.push(ICONST_0);
        code.push(ICONST_5);
        code.extend(op3(INVOKEVIRTUAL, substring_width));
        code.push(IRETURN);
        assert_eq!(
            run_main_int(&mut pool, 5, 1, code),
            5 * char_px,
            "size={size}：全串子串宽度应等于 stringWidth"
        );
        // 前缀 substringWidth("hello",0,2) == 2×每码元。
        let mut pool = Pool::new();
        let get_font = font_static(&mut pool, "getFont", "(III)Ljavax/microedition/lcdui/Font;");
        let substring_width = font_static(&mut pool, "substringWidth", "(Ljava/lang/String;II)I");
        let hello = pool.string("hello");
        let mut code = vec![ICONST_0, ICONST_0, BIPUSH, size as u8];
        code.extend(op3(INVOKESTATIC, get_font));
        code.extend(op3(LDC_W, hello));
        code.push(ICONST_0);
        code.push(ICONST_2);
        code.extend(op3(INVOKEVIRTUAL, substring_width));
        code.push(IRETURN);
        assert_eq!(
            run_main_int(&mut pool, 5, 1, code),
            2 * char_px,
            "size={size}：前缀子串宽度应为 2 码元"
        );
        // 中段 substringWidth("hello",2,3) == 3×每码元（一致性线性推进）。
        let mut pool = Pool::new();
        let get_font = font_static(&mut pool, "getFont", "(III)Ljavax/microedition/lcdui/Font;");
        let substring_width = font_static(&mut pool, "substringWidth", "(Ljava/lang/String;II)I");
        let hello = pool.string("hello");
        let mut code = vec![ICONST_0, ICONST_0, BIPUSH, size as u8];
        code.extend(op3(INVOKESTATIC, get_font));
        code.extend(op3(LDC_W, hello));
        code.push(ICONST_2);
        code.push(ICONST_3);
        code.extend(op3(INVOKEVIRTUAL, substring_width));
        code.push(IRETURN);
        assert_eq!(
            run_main_int(&mut pool, 5, 1, code),
            3 * char_px,
            "size={size}：中段子串宽度应为 3 码元"
        );
    }
}
