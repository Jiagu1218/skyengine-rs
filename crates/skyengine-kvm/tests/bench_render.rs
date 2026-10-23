//! 渲染热路径微基准（ignored；`cargo test -p skyengine-kvm --release
//! --test bench_render -- --ignored --nocapture` 手动跑）：
//! drawRegion t=2（游戏 99% 路径）/ t=6（旋转族）/ drawImage 全图 /
//! drawRGB 全屏，各 N 次展开调用报耗时与每千次均值。优化前后对比用
//! （解释器分派开销两侧恒定，差值即 native 渲染本体变化）。
//! 每个用例带回读断言（目标中心像素 = 源色），防止静默 no-op 假提速。

#[allow(dead_code)]
mod common;

use std::time::Instant;

use common::classbytes::{build_class, provider, MethodSpec, Pool};
use skyengine_kvm::host::mock::MockHostContext;
use skyengine_kvm::interp::natives::RegistryDispatch;
use skyengine_kvm::interp::{enter, run, runtime_class_of, Machine, RunOutcome, Unwind};
use skyengine_kvm::runtime::classspace::ClassSpace;
use skyengine_kvm::runtime::heap::Heap;
use skyengine_kvm::runtime::monitor::Monitors;
use skyengine_kvm::runtime::{Slot, ThreadId};
use skyengine_kvm::KvmError;

const MAIN: ThreadId = ThreadId(1);
const IMAGE: &str = "javax/microedition/lcdui/Image";
const GRAPHICS: &str = "javax/microedition/lcdui/Graphics";

const ASTORE_1: u8 = 0x4c;
const ASTORE_2: u8 = 0x4d;
const ASTORE_3: u8 = 0x4e;
const ALOAD_1: u8 = 0x2b;
const ALOAD_2: u8 = 0x2c;
const ALOAD_3: u8 = 0x2d;
const ALOAD: u8 = 0x19;
const ASTORE: u8 = 0x3a;
const ILOAD: u8 = 0x15;
const SIPUSH: u8 = 0x11;
const LDC_W: u8 = 0x13;
const IASTORE: u8 = 0x4f;
const IALOAD: u8 = 0x2e;
const IINC: u8 = 0x84;
const IF_ICMPGE: u8 = 0xa2;
const GOTO: u8 = 0xa7;
const NEWARRAY: u8 = 0xbc;
const INVOKEVIRTUAL: u8 = 0xb6;
const INVOKESTATIC: u8 = 0xb8;
const GETFIELD: u8 = 0xb4;
const IRETURN: u8 = 0xac;
const T_INT: u8 = 10;

const ATLAS_COLOR: i32 = 0x33_66CC; // setColor 实参（alpha 0，fill 时补 OPAQUE）

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
        cs.load_class("java/lang/String").expect("load String");
        Self {
            cs,
            heap: Heap::new(),
            monitors: Monitors::new(),
            natives: RegistryDispatch,
            host: MockHostContext::new(240, 320),
        }
    }

    fn exec(&mut self, desc: &str) -> Result<RunOutcome, KvmError> {
        let class = self.cs.load_class("T")?;
        let mut frames = Vec::new();
        let mut machine = Machine {
            cs: &mut self.cs,
            heap: &mut self.heap,
            monitors: &mut self.monitors,
            natives: &mut self.natives,
            host: &mut self.host,
        };
        enter(&mut machine, &mut frames, class, "main", desc, Vec::new())?;
        run(&mut machine, &mut frames, MAIN, None)
    }
}

fn m_code(stack: u16, locals: u16, code: Vec<u8>) -> MethodSpec {
    MethodSpec {
        access: 0x0008,
        name: "main",
        desc: "()I",
        code: Some((stack, locals, code)),
        handlers: Vec::new(),
    }
}

fn op3(op: u8, index: u16) -> [u8; 3] {
    let [hi, lo] = index.to_be_bytes();
    [op, hi, lo]
}

fn push_int(pool: &mut Pool, value: i32) -> Vec<u8> {
    match value {
        -1..=5 => vec![0x02 + (value + 1) as u8],
        -128..=127 => vec![0x10, value as u8],
        -32768..=32767 => {
            let mut code = vec![SIPUSH];
            code.extend_from_slice(&(value as i16).to_be_bytes());
            code
        }
        _ => {
            let index = pool.integer(value);
            let [hi, lo] = index.to_be_bytes();
            vec![LDC_W, hi, lo]
        }
    }
}

fn expect_int(outcome: RunOutcome) -> i32 {
    match outcome {
        RunOutcome::Returned(Some(Slot::Int(value))) => value,
        other => panic!("expected int return, got {other:?}"),
    }
}

/// 公共前序：atlas(256x512 实心) + screen(320x480) + g。
/// locals: 1=atlas, 2=atlasG, 3=screen, 4=g。
fn setup(pool: &mut Pool, code: &mut Vec<u8>) {
    let create_img = pool.methodref(IMAGE, "createImage", "(II)Ljavax/microedition/lcdui/Image;");
    let get_g = pool.methodref(IMAGE, "getGraphics", "()Ljavax/microedition/lcdui/Graphics;");
    let set_color = pool.methodref(GRAPHICS, "setColor", "(I)V");
    let fill = pool.methodref(GRAPHICS, "fillRect", "(IIII)V");
    code.extend(push_int(pool, 256));
    code.extend(push_int(pool, 512));
    code.extend(op3(INVOKESTATIC, create_img));
    code.push(ASTORE_1);
    code.push(ALOAD_1);
    code.extend(op3(INVOKEVIRTUAL, get_g));
    code.push(ASTORE_2);
    code.push(ALOAD_2);
    code.extend(push_int(pool, ATLAS_COLOR));
    code.extend(op3(INVOKEVIRTUAL, set_color));
    code.push(ALOAD_2);
    code.extend(push_int(pool, 0));
    code.extend(push_int(pool, 0));
    code.extend(push_int(pool, 256));
    code.extend(push_int(pool, 512));
    code.extend(op3(INVOKEVIRTUAL, fill));
    code.extend(push_int(pool, 320));
    code.extend(push_int(pool, 480));
    code.extend(op3(INVOKESTATIC, create_img));
    code.push(ASTORE_3);
    code.push(ALOAD_3);
    code.extend(op3(INVOKEVIRTUAL, get_g));
    code.push(ASTORE);
    code.push(4);
}

/// 回读断言收尾：return screen.pixels[idx]。
fn readback(pool: &mut Pool, code: &mut Vec<u8>, idx: i32) {
    let pixels = pool.fieldref(IMAGE, "pixels", "[I");
    code.push(ALOAD_3);
    code.extend(op3(GETFIELD, pixels));
    code.extend(push_int(pool, idx));
    code.push(IALOAD);
    code.push(IRETURN);
}

fn run_bench(label: &str, n: usize, build: impl Fn(&mut Pool, &mut Vec<u8>), expect: i32) {
    let mut pool = Pool::new();
    let mut code = Vec::new();
    setup(&mut pool, &mut code);
    build(&mut pool, &mut code);
    // n 次循环体已在 build 内展开（或本函数展开简单序列）
    let bytes = build_class(&mut pool, "T", Some("java/lang/Object"), &[], &[], &[m_code(
        16, 8, code,
    )]);
    let mut vm = Vm::new(vec![("T".to_string(), bytes)]);
    let start = Instant::now();
    let outcome = vm.exec("()I").expect("run main");
    if let RunOutcome::Uncaught(Unwind::Thrown(exception)) = &outcome
        && let Ok(class) = runtime_class_of(&vm.cs, &vm.heap, *exception)
    {
        panic!("{label}: thrown {}", vm.cs.name_of(class));
    }
    let got = expect_int(outcome);
    let elapsed = start.elapsed();
    assert_eq!(
        got, expect,
        "{label}: 目标中心像素不符（got {got:#010x} want {expect:#010x}）——存在静默 no-op"
    );
    println!(
        "{label:<26} {n:>5} 次  {:>10.1?}  ({:8.1} µs/千次)",
        elapsed,
        elapsed.as_micros() as f64 / n as f64
    );
}

/// drawRegion t=2 16x16（游戏主导路径）× n。
#[test]
#[ignore]
fn bench_draw_region_mirror_tile() {
    let n = 4000;
    run_bench("drawRegion t=2 16x16", n, |pool, code| {
        let draw_region =
            pool.methodref(GRAPHICS, "drawRegion", "(Ljavax/microedition/lcdui/Image;IIIIIIII)V");
        for _ in 0..n {
            code.extend([ALOAD, 4]);
            code.push(ALOAD_1);
            code.extend(push_int(pool, 32));
            code.extend(push_int(pool, 48));
            code.extend(push_int(pool, 16));
            code.extend(push_int(pool, 16));
            code.extend(push_int(pool, 2));
            code.extend(push_int(pool, 40));
            code.extend(push_int(pool, 96));
            code.extend(push_int(pool, 20));
            code.extend(op3(INVOKEVIRTUAL, draw_region));
        }
        readback(pool, code, 104 * 320 + 48);
    }, (0xFF00_0000u32 | ATLAS_COLOR as u32 & 0xFF_FFFF) as i32);
}

/// drawRegion t=6 16x16（旋转族）× n。
#[test]
#[ignore]
fn bench_draw_region_rotated_tile() {
    let n = 2000;
    run_bench("drawRegion t=6 16x16", n, |pool, code| {
        let draw_region =
            pool.methodref(GRAPHICS, "drawRegion", "(Ljavax/microedition/lcdui/Image;IIIIIIII)V");
        for _ in 0..n {
            code.extend([ALOAD, 4]);
            code.push(ALOAD_1);
            code.extend(push_int(pool, 32));
            code.extend(push_int(pool, 48));
            code.extend(push_int(pool, 16));
            code.extend(push_int(pool, 16));
            code.extend(push_int(pool, 6));
            code.extend(push_int(pool, 40));
            code.extend(push_int(pool, 96));
            code.extend(push_int(pool, 36));
            code.extend(op3(INVOKEVIRTUAL, draw_region));
        }
        // anchor=36（LEFT|BOTTOM）→ 输出占 (40..56, 80..96)，取中心 (48, 88)。
        readback(pool, code, 88 * 320 + 48);
    }, (0xFF00_0000u32 | ATLAS_COLOR as u32 & 0xFF_FFFF) as i32);
}

/// drawImage 整图（256x512 → 320x480 屏幕）× n。
#[test]
#[ignore]
fn bench_draw_image_full() {
    let n = 60;
    run_bench("drawImage 256x512", n, |pool, code| {
        let draw_image = pool.methodref(GRAPHICS, "drawImage", "(Ljavax/microedition/lcdui/Image;III)V");
        for _ in 0..n {
            code.extend([ALOAD, 4]);
            code.push(ALOAD_1);
            code.extend(push_int(pool, 8));
            code.extend(push_int(pool, 8));
            code.extend(push_int(pool, 20));
            code.extend(op3(INVOKEVIRTUAL, draw_image));
        }
        readback(pool, code, 104 * 320 + 48);
    }, (0xFF00_0000u32 | ATLAS_COLOR as u32 & 0xFF_FFFF) as i32);
}

/// drawRGB 320x480 全屏 × n（源数组 Java 填充循环构造）。
#[test]
#[ignore]
fn bench_draw_rgb_full() {
    let n = 60;
    let rgb_color = 0xFF40_80C0u32 as i32;
    let expected = rgb_color;
    run_bench("drawRGB 320x480", n, |pool, code| {
        let draw_rgb = pool.methodref(GRAPHICS, "drawRGB", "([IIIIIIIZ)V");
        // local 5 = rgb[153600]，Java 循环填充：i=0; while (i<len) a[i]=c;
        let total = 320 * 480;
        code.extend(push_int(pool, total));
        code.push(NEWARRAY);
        code.push(T_INT);
        code.extend([ASTORE, 5]);
        code.push(0x03); // iconst_0
        code.extend([0x3a, 6]); // istore 6
        let loop_start = code.len() as i32;
        // L: iload 6; total; if_icmpge E
        code.extend([ILOAD, 6]);
        code.extend(push_int(pool, total));
        let branch_at = code.len() as i32;
        code.extend([IF_ICMPGE, 0, 0]); // 回填
        // a[i] = color
        code.extend([ALOAD, 5]);
        code.extend([ILOAD, 6]);
        code.extend(push_int(pool, rgb_color));
        code.push(IASTORE);
        code.extend([IINC, 6, 1]);
        let goto_at = code.len() as i32;
        code.extend([GOTO, 0, 0]); // 回填
        let after = code.len() as i32;
        // E:
        let e_at = after as i16;
        let [bh, bl] = (e_at - branch_at as i16).to_be_bytes(); // JVM：相对指令自身地址
        code[branch_at as usize + 1] = bh;
        code[branch_at as usize + 2] = bl;
        let back = (loop_start - goto_at) as i16;
        let [gh, gl] = back.to_be_bytes();
        code[goto_at as usize + 1] = gh;
        code[goto_at as usize + 2] = gl;
        for _ in 0..n {
            code.extend([ALOAD, 4]);
            code.extend([ALOAD, 5]);
            code.extend(push_int(pool, 0)); // offset
            code.extend(push_int(pool, 320)); // scanlength
            code.extend(push_int(pool, 0)); // x
            code.extend(push_int(pool, 0)); // y
            code.extend(push_int(pool, 320));
            code.extend(push_int(pool, 480));
            code.extend(push_int(pool, 0)); // processAlpha=false
            code.extend(op3(INVOKEVIRTUAL, draw_rgb));
        }
        readback(pool, code, 104 * 320 + 48);
    }, expected);
}

#[test]
#[ignore]
fn bench_all() {
    bench_draw_region_mirror_tile();
    bench_draw_region_rotated_tile();
    bench_draw_image_full();
    bench_draw_rgb_full();
}
