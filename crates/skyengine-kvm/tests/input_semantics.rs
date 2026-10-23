//! T030 按键事件端到端测试：Vm.dispatch_key（引擎键名）→ poll 段前置
//! 事件泵 → Canvas keyPressed/keyReleased 绿线程回调 + 游戏动作 API
//! （getGameAction/getKeyCode/getKeyName）。
//!
//! 夹具：手构 `T extends javax/microedition/lcdui/Canvas`（startApp 里
//! getDisplay → setCurrent(this)），静态字段 pKey/pCnt/rKey/rCnt 由
//! 回调体写入，Rust 侧经 `Vm::static_field` 回读。
//!
//! 时序要点：事件泵在 poll 段前置执行，current 未设置（startApp 未跑）
//! 时事件被丢弃——故先 poll 一轮跑完 startApp，再 dispatch_key + poll
//! 驱动回调（第二次 poll 的 pump 先于段二 Idle→Dead 判定，回调线程
//! 得以 spawn 与执行）。
//!
//! 构码铁律（前车之鉴）：带参构造 NEW+DUP+参数+INVOKESPECIAL；分支
//! 指令消耗条件值后不再 POP；池索引先取号再构码。

mod common;

use common::classbytes::{FieldSpec, HandlerSpec, MethodSpec, Pool, build_class};
use skyengine_kvm::Vm;
use skyengine_kvm::host::mock::MockHostContext;
use skyengine_kvm::midp::lcdui::game::gamecanvas::{
    self, DOWN_PRESSED, FIRE_PRESSED, GAME_A_PRESSED, GAME_B_PRESSED, GAME_C_PRESSED,
    GAME_D_PRESSED, LEFT_PRESSED, RIGHT_PRESSED, UP_PRESSED,
};
use skyengine_kvm::midp::lcdui::input::{
    self, ENGINE_KEY_FIRE, ENGINE_KEY_SOFT_RIGHT, GAME_A, GAME_B, GAME_C, GAME_D, GAME_DOWN,
    GAME_FIRE, GAME_LEFT, GAME_RIGHT, GAME_UP, KEY_FIRE, KEY_SOFT_RIGHT, KEY_UP,
};
use skyengine_kvm::runtime::Slot;
use skyengine_kvm::{VmConfig, VmState};

// ---- 操作码（仅本文件用到的） --------------------------------------------

const ICONST_1: u8 = 0x04;
const BIPUSH: u8 = 0x10;
const ALOAD_0: u8 = 0x2a;
const ALOAD_1: u8 = 0x2b;
const ILOAD_1: u8 = 0x1b;
const ILOAD_2: u8 = 0x1c;
const ACONST_NULL: u8 = 0x01;
const ASTORE_1: u8 = 0x4c;
const POP: u8 = 0x57;
const IADD: u8 = 0x60;
const GOTO: u8 = 0xa7;
const IFNULL: u8 = 0xc6;
const PUTSTATIC: u8 = 0xb3;
const GETSTATIC: u8 = 0xb2;
const INVOKEVIRTUAL: u8 = 0xb6;
const INVOKESPECIAL: u8 = 0xb7;
const INVOKESTATIC: u8 = 0xb8;
const RETURN: u8 = 0xb1;

const CANVAS: &str = "javax/microedition/lcdui/Canvas";
const DISPLAY: &str = "javax/microedition/lcdui/Display";
const GAMECANVAS: &str = "javax/microedition/lcdui/game/GameCanvas";

// ---- 最小 stored zip（同 vm_facade.rs 的 zip_store） ----------------------

fn zip_store(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut offsets = Vec::new();
    for (name, data) in entries {
        offsets.push(out.len() as u32);
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&[0u8; 14]);
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
    }
    let cd_start = out.len() as u32;
    let mut central = Vec::new();
    for ((name, data), offset) in entries.iter().zip(&offsets) {
        central.extend_from_slice(b"PK\x01\x02");
        central.extend_from_slice(&[0u8; 16]);
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0u8; 8]);
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    out.extend_from_slice(&central);
    out.extend_from_slice(b"PK\x05\x06");
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    let count = entries.len() as u16;
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_start.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

// ---- 迷你汇编 --------------------------------------------------------------

fn op3(op: u8, index: u16) -> [u8; 3] {
    let [hi, lo] = index.to_be_bytes();
    [op, hi, lo]
}

/// int 压栈（iconst_m1..iconst_5/bipush 按值域选择；本文件测试值都在
/// bipush 值域内）。
fn push_int(value: i32) -> Vec<u8> {
    match value {
        -1..=5 => vec![0x02 + (value + 1) as u8],
        _ => vec![BIPUSH, value as u8],
    }
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

/// 回调体骨架（keyPressed/keyReleased 同形）：key 存 *Key 字段、
/// *Cnt 自增。
fn code_key_counter(pool: &mut Pool, key_field: &str, cnt_field: &str) -> Vec<u8> {
    let key = pool.fieldref("T", key_field, "I");
    let cnt = pool.fieldref("T", cnt_field, "I");
    let mut code = Vec::new();
    code.push(ILOAD_1);
    code.extend_from_slice(&op3(PUTSTATIC, key));
    code.extend_from_slice(&op3(GETSTATIC, cnt));
    code.push(ICONST_1);
    code.push(IADD);
    code.extend_from_slice(&op3(PUTSTATIC, cnt));
    code.push(RETURN);
    code
}

/// 静态 int 字段速记。
fn static_i_field(name: &'static str) -> FieldSpec {
    FieldSpec {
        access: 0x0009, // public static
        name,
        desc: "I",
        constant: None,
    }
}

/// 组装 T：4 个静态计数字段 + startApp(setDisplay) + 空 paint +
/// keyPressed/keyReleased（覆写计数）。
fn build_t(pool: &mut Pool) -> Vec<u8> {
    let mut sa = code_show(pool);
    sa.push(RETURN);
    // 构码需先行（构码借 pool 取池号，build_class 再借 pool 写类文件，
    // 内嵌表达式会二次可变借用）。
    let pressed = code_key_counter(pool, "pKey", "pCnt");
    let released = code_key_counter(pool, "rKey", "rCnt");
    build_class(
        pool,
        "T",
        Some(CANVAS),
        &[],
        &[
            static_i_field("pKey"),
            static_i_field("pCnt"),
            static_i_field("rKey"),
            static_i_field("rCnt"),
        ],
        &[
            MethodSpec {
                access: 0x0001,
                name: "startApp",
                desc: "()V",
                code: Some((4, 2, sa)),
                handlers: Vec::new(),
            },
            MethodSpec {
                access: 0x0001,
                name: "paint",
                desc: "(Ljavax/microedition/lcdui/Graphics;)V",
                code: Some((1, 2, vec![RETURN])),
                handlers: Vec::new(),
            },
            MethodSpec {
                access: 0x0001,
                name: "keyPressed",
                desc: "(I)V",
                code: Some((3, 2, pressed)),
                handlers: Vec::new(),
            },
            MethodSpec {
                access: 0x0001,
                name: "keyReleased",
                desc: "(I)V",
                code: Some((3, 2, released)),
                handlers: Vec::new(),
            },
        ],
    )
}

/// 装配 Vm（Mock 宿主 240x320）并启动 T。
fn boot(class_bytes: Vec<u8>) -> Vm {
    let zip = zip_store(&[("T.class", class_bytes)]);
    let mut vm = Vm::new(
        Box::new(MockHostContext::new(240, 320)),
        VmConfig::default(),
    );
    vm.install_jar("t", &zip).expect("install jar");
    vm.start_midlet("T").expect("start midlet");
    vm
}

/// T 静态 int 字段回读。
fn static_i(vm: &Vm, name: &str) -> i32 {
    match vm.static_field("T", name, "I") {
        Some(Slot::Int(v)) => v,
        other => panic!("T.{name} corrupt: {other:?}"),
    }
}

// ---- 用例 ------------------------------------------------------------------

/// 引擎键名 '5'（ASCII 53）按下 → keyPressed 收到 MIDP keyCode 53，
/// 计数恰 1；keyReleased 未触发。
#[test]
fn key_press_maps_digit_to_midp_keycode() {
    let mut pool = Pool::new();
    let bytes = build_t(&mut pool);
    let mut vm = boot(bytes);
    // 第一轮：跑 startApp（setDisplay），此时事件泵尚无事件。
    assert_eq!(vm.poll(200_000).ran_threads, 1);
    // dispatch '5' 按下 → 下一轮 pump spawn 回调 + 段二驱动。
    vm.dispatch_key(b'5', true);
    assert_eq!(vm.poll(200_000).ran_threads, 1, "回调线程应被驱动");
    assert_eq!(static_i(&vm, "pKey"), 53, "'5' 的 MIDP keyCode 为 53");
    assert_eq!(static_i(&vm, "pCnt"), 1);
    assert_eq!(static_i(&vm, "rCnt"), 0, "未 release 不触发 keyReleased");
}

/// press + release 两事件依次投递 → 两回调各 1 次，键码一致。
#[test]
fn press_and_release_both_delivered() {
    let mut pool = Pool::new();
    let bytes = build_t(&mut pool);
    let mut vm = boot(bytes);
    assert_eq!(vm.poll(200_000).ran_threads, 1);
    vm.dispatch_key(b'5', true);
    vm.dispatch_key(b'5', false);
    // 两事件 drain 出两回调线程；poll 每轮驱动一个到让出点。
    assert_eq!(vm.poll(200_000).ran_threads, 1);
    assert_eq!(vm.poll(200_000).ran_threads, 1);
    assert_eq!(static_i(&vm, "pKey"), 53);
    assert_eq!(static_i(&vm, "rKey"), 53);
    assert_eq!(static_i(&vm, "pCnt"), 1);
    assert_eq!(static_i(&vm, "rCnt"), 1);
}

/// 高位区引擎键名映射：FIRE → -5、右软键（=18 MR_KEY_SOFTRIGHT）→ -7。
#[test]
fn fire_and_soft_keys_map_to_negative_keycodes() {
    let mut pool = Pool::new();
    let bytes = build_t(&mut pool);
    let mut vm = boot(bytes);
    assert_eq!(vm.poll(200_000).ran_threads, 1);
    vm.dispatch_key(ENGINE_KEY_FIRE, true);
    assert_eq!(vm.poll(200_000).ran_threads, 1);
    assert_eq!(static_i(&vm, "pKey"), KEY_FIRE, "FIRE 应映射 -5");
    vm.dispatch_key(ENGINE_KEY_SOFT_RIGHT, true);
    assert_eq!(vm.poll(200_000).ran_threads, 1);
    assert_eq!(static_i(&vm, "pKey"), KEY_SOFT_RIGHT, "右软键应映射 -7");
    assert_eq!(static_i(&vm, "pCnt"), 2);
}

/// 未覆写 keyPressed/keyReleased 的 Canvas 子类：事件泵探测到
/// bootstrap 桩（body=None）→ 静默丢弃，poll 后 VM 正常终态不炸不挂。
#[test]
fn canvas_without_key_overrides_survives() {
    let mut pool = Pool::new();
    let mut sa = code_show(&mut pool);
    sa.push(RETURN);
    let bytes = build_class(
        &mut pool,
        "T",
        Some(CANVAS),
        &[],
        &[],
        &[
            MethodSpec {
                access: 0x0001,
                name: "startApp",
                desc: "()V",
                code: Some((4, 2, sa)),
                handlers: Vec::new(),
            },
            MethodSpec {
                access: 0x0001,
                name: "paint",
                desc: "(Ljavax/microedition/lcdui/Graphics;)V",
                code: Some((1, 2, vec![RETURN])),
                handlers: Vec::new(),
            },
        ],
    );
    let mut vm = boot(bytes);
    assert_eq!(vm.poll(200_000).ran_threads, 1, "startApp 应跑完");
    vm.dispatch_key(b'5', true);
    vm.dispatch_key(ENGINE_KEY_FIRE, false);
    // pump 丢弃两事件后调度器空表 → 正常 Dead（非 panic/挂起）。
    let result = vm.poll(200_000);
    assert_eq!(result.state, VmState::Dead, "无回调实现时 VM 应正常终止");
    assert!(!vm.is_active());
}

/// 游戏动作 API：getGameAction(-1)==UP(1)、getKeyCode(UP)==-1、
/// getKeyName(53).charAt(0)=='5'；getGameAction(53)（数字键无游戏
/// 动作）抛 IllegalArgumentException 且被 Java 侧 catch 记标志。
#[test]
fn game_action_roundtrip_and_iae_on_non_game_key() {
    let mut pool = Pool::new();
    let ga = pool.methodref(CANVAS, "getGameAction", "(I)I");
    let kc = pool.methodref(CANVAS, "getKeyCode", "(I)I");
    let kn = pool.methodref(CANVAS, "getKeyName", "(I)Ljava/lang/String;");
    let char_at = pool.methodref("java/lang/String", "charAt", "(I)C");
    let f_ga = pool.fieldref("T", "ga", "I");
    let f_kc = pool.fieldref("T", "kc", "I");
    let f_name = pool.fieldref("T", "nameChar", "I");
    let f_iae = pool.fieldref("T", "iaeFlag", "I");

    let mut code = code_show(&mut pool);
    // ga = getGameAction(-1)
    code.push(ALOAD_0);
    code.extend_from_slice(&push_int(KEY_UP));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, ga));
    code.extend_from_slice(&op3(PUTSTATIC, f_ga));
    // kc = getKeyCode(GAME_UP)
    code.push(ALOAD_0);
    code.extend_from_slice(&push_int(GAME_UP));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, kc));
    code.extend_from_slice(&op3(PUTSTATIC, f_kc));
    // nameChar = getKeyName('5').charAt(0)
    code.push(ALOAD_0);
    code.extend_from_slice(&push_int(53));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, kn));
    code.extend_from_slice(&push_int(0));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, char_at));
    code.extend_from_slice(&op3(PUTSTATIC, f_name));
    // try { getGameAction(53); } catch (IllegalArgumentException) { iaeFlag=1 }
    let try_start = code.len() as u16;
    code.push(ALOAD_0);
    code.extend_from_slice(&push_int(53));
    code.extend_from_slice(&op3(INVOKEVIRTUAL, ga));
    code.push(POP); // 消耗返回值
    let goto_at = code.len();
    code.extend_from_slice(&[GOTO, 0, 0]); // 占位，稍后回填跳过 handler
    let handler_pc = code.len() as u16;
    let try_end = handler_pc;
    code.push(POP); // 消耗异常引用
    code.push(ICONST_1);
    code.extend_from_slice(&op3(PUTSTATIC, f_iae));
    let end_pc = code.len() as u16;
    // 回填 GOTO 偏移（自指令起算）。
    let offset = (end_pc as i64 - goto_at as i64) as i16;
    code[goto_at + 1..goto_at + 3].copy_from_slice(&offset.to_be_bytes());
    code.push(RETURN);

    let bytes = build_class(
        &mut pool,
        "T",
        Some(CANVAS),
        &[],
        &[
            static_i_field("ga"),
            static_i_field("kc"),
            static_i_field("nameChar"),
            static_i_field("iaeFlag"),
        ],
        &[
            MethodSpec {
                access: 0x0001,
                name: "startApp",
                desc: "()V",
                code: Some((6, 3, code)),
                handlers: vec![HandlerSpec {
                    start_pc: try_start,
                    end_pc: try_end,
                    handler_pc,
                    catch_class: Some("java/lang/IllegalArgumentException"),
                }],
            },
            MethodSpec {
                access: 0x0001,
                name: "paint",
                desc: "(Ljavax/microedition/lcdui/Graphics;)V",
                code: Some((1, 2, vec![RETURN])),
                handlers: Vec::new(),
            },
        ],
    );
    let mut vm = boot(bytes);
    assert_eq!(vm.poll(200_000).ran_threads, 1);
    assert_eq!(static_i(&vm, "ga"), GAME_UP, "getGameAction(-1) 应为 UP=1");
    assert_eq!(static_i(&vm, "kc"), KEY_UP, "getKeyCode(1) 应为 -1");
    assert_eq!(static_i(&vm, "nameChar"), 53, "getKeyName(53) 应为 \"5\"");
    assert_eq!(
        static_i(&vm, "iaeFlag"),
        1,
        "getGameAction(53) 应抛 IllegalArgumentException"
    );
}

/// 映射表纯 Rust 面快测（无 VM）：数字/星/井 ASCII 直通、未知键 None。
#[test]
fn midp_keycode_table_edges() {
    assert_eq!(input::midp_keycode(b'0'), Some(48));
    assert_eq!(input::midp_keycode(b'9'), Some(57));
    assert_eq!(input::midp_keycode(b'*'), Some(42));
    assert_eq!(input::midp_keycode(b'#'), Some(35));
    assert_eq!(input::midp_keycode(input::ENGINE_KEY_UP), Some(-1));
    assert_eq!(input::midp_keycode(input::ENGINE_KEY_LEFT), Some(-3));
    assert_eq!(input::midp_keycode(input::ENGINE_KEY_SOFT_LEFT), Some(-6));
    assert_eq!(input::midp_keycode(0x7F), None, "未知键丢弃");
    assert_eq!(input::midp_keycode(0x00), None, "零值丢弃");
}

// ===========================================================================
// T031+T032：GameCanvas 体系 + 输入测试汇合
// ===========================================================================

/// pointer 回调体骨架（三回调同形）：两参坐标存静态字段 + 计数自增。
fn code_pointer_counter(pool: &mut Pool, x_field: &str, y_field: &str, cnt_field: &str) -> Vec<u8> {
    let x = pool.fieldref("T", x_field, "I");
    let y = pool.fieldref("T", y_field, "I");
    let cnt = pool.fieldref("T", cnt_field, "I");
    let mut code = Vec::new();
    code.push(ILOAD_1);
    code.extend_from_slice(&op3(PUTSTATIC, x));
    code.push(ILOAD_2);
    code.extend_from_slice(&op3(PUTSTATIC, y));
    code.extend_from_slice(&op3(GETSTATIC, cnt));
    code.push(ICONST_1);
    code.push(IADD);
    code.extend_from_slice(&op3(PUTSTATIC, cnt));
    code.push(RETURN);
    code
}

/// 组装 pointer 版 T：extends Canvas，覆写 pointerPressed/Released/
/// Dragged 记录坐标与计数（9 静态字段）。
fn build_pointer_t(pool: &mut Pool) -> Vec<u8> {
    let mut sa = code_show(pool);
    sa.push(RETURN);
    let pressed = code_pointer_counter(pool, "ppX", "ppY", "ppCnt");
    let released = code_pointer_counter(pool, "rX", "rY", "rCnt");
    let dragged = code_pointer_counter(pool, "dX", "dY", "dCnt");
    build_class(
        pool,
        "T",
        Some(CANVAS),
        &[],
        &[
            static_i_field("ppX"),
            static_i_field("ppY"),
            static_i_field("ppCnt"),
            static_i_field("rX"),
            static_i_field("rY"),
            static_i_field("rCnt"),
            static_i_field("dX"),
            static_i_field("dY"),
            static_i_field("dCnt"),
        ],
        &[
            MethodSpec {
                access: 0x0001,
                name: "startApp",
                desc: "()V",
                code: Some((4, 2, sa)),
                handlers: Vec::new(),
            },
            MethodSpec {
                access: 0x0001,
                name: "paint",
                desc: "(Ljavax/microedition/lcdui/Graphics;)V",
                code: Some((1, 2, vec![RETURN])),
                handlers: Vec::new(),
            },
            MethodSpec {
                access: 0x0001,
                name: "pointerPressed",
                desc: "(II)V",
                code: Some((3, 3, pressed)),
                handlers: Vec::new(),
            },
            MethodSpec {
                access: 0x0001,
                name: "pointerReleased",
                desc: "(II)V",
                code: Some((3, 3, released)),
                handlers: Vec::new(),
            },
            MethodSpec {
                access: 0x0001,
                name: "pointerDragged",
                desc: "(II)V",
                code: Some((3, 3, dragged)),
                handlers: Vec::new(),
            },
        ],
    )
}

/// bootstrap 类静态 int 常量回读（const_field 常量面断言用）。
fn const_i(vm: &Vm, class: &str, name: &str) -> i32 {
    match vm.static_field(class, name, "I") {
        Some(Slot::Int(v)) => v,
        other => panic!("{class}.{name} corrupt: {other:?}"),
    }
}

/// 组装 GameCanvas 版 T：`<init>` 带 suppress 参构造（构造体内上溯
/// super：aload_0 + 参数 + INVOKESPECIAL 铁律）；startApp 额外调
/// getGraphics 判 null 记 gOk（顺带实证屏图 Graphics 分配）；
/// read_in_pressed=true 时 keyPressed 内两次读 getKeyStates 存 s1/s2
///（读后清零断言）；read_in_released=true 时 keyReleased 内存 rs1
///（多键组合位断言——keyPressed 不读防位图被过早清零）。
fn build_game_t(
    pool: &mut Pool,
    suppress: i32,
    read_in_pressed: bool,
    read_in_released: bool,
) -> Vec<u8> {
    // 池索引先取号再构码（铁律）。
    let gc_init = pool.methodref(GAMECANVAS, "<init>", "(Z)V");
    let get_g = pool.methodref(
        GAMECANVAS,
        "getGraphics",
        "()Ljavax/microedition/lcdui/Graphics;",
    );
    let get_states = pool.methodref(GAMECANVAS, "getKeyStates", "()I");
    let g_ok = pool.fieldref("T", "gOk", "I");
    // T.<init>: aload_0; push suppress; invokespecial GameCanvas 构造
    let mut init_code = Vec::new();
    init_code.push(ALOAD_0);
    init_code.extend_from_slice(&push_int(suppress));
    init_code.extend_from_slice(&op3(INVOKESPECIAL, gc_init));
    init_code.push(RETURN);
    // startApp：setDisplay + getGraphics 判 null（IFNULL 消耗条件值，
    // 跳过 gOk=1 直达共用的 RETURN——分支后不再 POP）。
    let mut sa = code_show(pool);
    sa.push(ALOAD_0);
    sa.extend_from_slice(&op3(INVOKEVIRTUAL, get_g));
    // ifnull 目标必须是指令起点：跳过 iconst_1(1)+putstatic(3) 直达
    // return（ifnull@14 → 目标@21，偏移 7；偏移须按指令边界对齐）。
    sa.extend_from_slice(&[IFNULL, 0, 7]);
    sa.push(ICONST_1);
    sa.extend_from_slice(&op3(PUTSTATIC, g_ok));
    sa.push(RETURN);
    // keyPressed：pKey/pCnt [+ s1/s2 两次 getKeyStates]
    let p_key = pool.fieldref("T", "pKey", "I");
    let p_cnt = pool.fieldref("T", "pCnt", "I");
    let mut pressed = Vec::new();
    pressed.push(ILOAD_1);
    pressed.extend_from_slice(&op3(PUTSTATIC, p_key));
    pressed.extend_from_slice(&op3(GETSTATIC, p_cnt));
    pressed.push(ICONST_1);
    pressed.push(IADD);
    pressed.extend_from_slice(&op3(PUTSTATIC, p_cnt));
    if read_in_pressed {
        let s1 = pool.fieldref("T", "s1", "I");
        let s2 = pool.fieldref("T", "s2", "I");
        pressed.push(ALOAD_0);
        pressed.extend_from_slice(&op3(INVOKEVIRTUAL, get_states));
        pressed.extend_from_slice(&op3(PUTSTATIC, s1));
        pressed.push(ALOAD_0);
        pressed.extend_from_slice(&op3(INVOKEVIRTUAL, get_states));
        pressed.extend_from_slice(&op3(PUTSTATIC, s2));
    }
    pressed.push(RETURN);
    // keyReleased：rKey/rCnt [+ rs1 一次 getKeyStates]
    let r_key = pool.fieldref("T", "rKey", "I");
    let r_cnt = pool.fieldref("T", "rCnt", "I");
    let mut released = Vec::new();
    released.push(ILOAD_1);
    released.extend_from_slice(&op3(PUTSTATIC, r_key));
    released.extend_from_slice(&op3(GETSTATIC, r_cnt));
    released.push(ICONST_1);
    released.push(IADD);
    released.extend_from_slice(&op3(PUTSTATIC, r_cnt));
    if read_in_released {
        let rs1 = pool.fieldref("T", "rs1", "I");
        released.push(ALOAD_0);
        released.extend_from_slice(&op3(INVOKEVIRTUAL, get_states));
        released.extend_from_slice(&op3(PUTSTATIC, rs1));
    }
    released.push(RETURN);
    let mut fields = vec![
        static_i_field("pKey"),
        static_i_field("pCnt"),
        static_i_field("rKey"),
        static_i_field("rCnt"),
        static_i_field("gOk"),
    ];
    if read_in_pressed {
        fields.push(static_i_field("s1"));
        fields.push(static_i_field("s2"));
    }
    if read_in_released {
        fields.push(static_i_field("rs1"));
    }
    build_class(
        pool,
        "T",
        Some(GAMECANVAS),
        &[],
        &fields,
        &[
            MethodSpec {
                access: 0x0001,
                name: "<init>",
                desc: "()V",
                code: Some((2, 2, init_code)),
                handlers: Vec::new(),
            },
            MethodSpec {
                access: 0x0001,
                name: "startApp",
                desc: "()V",
                code: Some((6, 2, sa)),
                handlers: Vec::new(),
            },
            MethodSpec {
                access: 0x0001,
                name: "paint",
                desc: "(Ljavax/microedition/lcdui/Graphics;)V",
                code: Some((1, 2, vec![RETURN])),
                handlers: Vec::new(),
            },
            MethodSpec {
                access: 0x0001,
                name: "keyPressed",
                desc: "(I)V",
                code: Some((3, 2, pressed)),
                handlers: Vec::new(),
            },
            MethodSpec {
                access: 0x0001,
                name: "keyReleased",
                desc: "(I)V",
                code: Some((3, 2, released)),
                handlers: Vec::new(),
            },
        ],
    )
}

/// getKeyStates 锁存与读后清零：press UP + release UP 都投递后，
/// keyPressed 回调内首读仍得 UP_PRESSED（物理释放不清位），次读得 0
/// （读取自动清零）。顺带实证 getGraphics 返回非 null 屏图 Graphics。
#[test]
fn get_key_states_latched_and_cleared_on_read() {
    let mut pool = Pool::new();
    let bytes = build_game_t(&mut pool, 0, true, false);
    let mut vm = boot(bytes);
    assert_eq!(vm.poll(200_000).ran_threads, 1); // startApp
    assert_eq!(
        static_i(&vm, "gOk"),
        1,
        "getGraphics 应返回非 null Graphics"
    );
    vm.dispatch_key(input::ENGINE_KEY_UP, true);
    vm.dispatch_key(input::ENGINE_KEY_UP, false);
    assert_eq!(vm.poll(200_000).ran_threads, 1); // keyPressed 线程
    assert_eq!(vm.poll(200_000).ran_threads, 1); // keyReleased 线程
    assert_eq!(static_i(&vm, "pCnt"), 1);
    assert_eq!(static_i(&vm, "rCnt"), 1);
    assert_eq!(
        static_i(&vm, "s1"),
        UP_PRESSED,
        "press+release 后首读仍含 UP 位（锁存语义）"
    );
    assert_eq!(static_i(&vm, "s2"), 0, "第二次读取应已清零");
}

/// 多键组合位或 + release 不清位：press UP、press FIRE、release UP
/// 三事件后，keyReleased 回调读得 UP|FIRE 组合位（keyPressed 只计数
/// 不读，保证位图完整置位后才被消费）。
#[test]
fn key_states_combine_and_survive_release() {
    let mut pool = Pool::new();
    let bytes = build_game_t(&mut pool, 0, false, true);
    let mut vm = boot(bytes);
    assert_eq!(vm.poll(200_000).ran_threads, 1);
    vm.dispatch_key(input::ENGINE_KEY_UP, true);
    vm.dispatch_key(ENGINE_KEY_FIRE, true);
    vm.dispatch_key(input::ENGINE_KEY_UP, false);
    assert_eq!(vm.poll(200_000).ran_threads, 1); // keyPressed(UP)
    assert_eq!(vm.poll(200_000).ran_threads, 1); // keyPressed(FIRE)
    assert_eq!(vm.poll(200_000).ran_threads, 1); // keyReleased(UP)
    assert_eq!(static_i(&vm, "pCnt"), 2);
    assert_eq!(static_i(&vm, "rCnt"), 1);
    assert_eq!(
        static_i(&vm, "rs1"),
        UP_PRESSED | FIRE_PRESSED,
        "组合位 UP|FIRE 按位或"
    );
}

/// suppressKeyEvents=true：dispatch 后无键回调派发（计数不增，该轮
/// poll 无线程可跑即 Idle→Dead），但键状态位图照常置位（引擎观测面
/// 快照验证——Java 侧 getKeyStates 读取才清零）。
#[test]
fn suppress_key_events_blocks_callbacks_but_sets_states() {
    let mut pool = Pool::new();
    let bytes = build_game_t(&mut pool, 1, false, false);
    let mut vm = boot(bytes);
    assert_eq!(vm.poll(200_000).ran_threads, 1);
    vm.dispatch_key(input::ENGINE_KEY_UP, true);
    vm.dispatch_key(input::ENGINE_KEY_UP, false);
    vm.dispatch_key(ENGINE_KEY_FIRE, true);
    let result = vm.poll(200_000);
    assert_eq!(result.ran_threads, 0, "suppress 下不应派发键回调线程");
    assert_eq!(static_i(&vm, "pCnt"), 0);
    assert_eq!(static_i(&vm, "rCnt"), 0);
    assert_eq!(
        vm.key_states(),
        (UP_PRESSED | FIRE_PRESSED) as u32,
        "事件只进状态位（观测面不清零快照）"
    );
}

/// pointer 拖拽三连：pressed(10,10) → dragged(50,60) → released(50,60)，
/// 三回调各恰 1 次且坐标逐一正确（T extends Canvas 覆写三方法）。
#[test]
fn pointer_drag_sequence_three_callbacks() {
    let mut pool = Pool::new();
    let bytes = build_pointer_t(&mut pool);
    let mut vm = boot(bytes);
    assert_eq!(vm.poll(200_000).ran_threads, 1);
    vm.dispatch_pointer(10, 10, 0);
    vm.dispatch_pointer(50, 60, 2);
    vm.dispatch_pointer(50, 60, 1);
    assert_eq!(vm.poll(200_000).ran_threads, 1); // pointerPressed
    assert_eq!(vm.poll(200_000).ran_threads, 1); // pointerDragged
    assert_eq!(vm.poll(200_000).ran_threads, 1); // pointerReleased
    assert_eq!(static_i(&vm, "ppCnt"), 1);
    assert_eq!(static_i(&vm, "ppX"), 10);
    assert_eq!(static_i(&vm, "ppY"), 10);
    assert_eq!(static_i(&vm, "dCnt"), 1);
    assert_eq!(static_i(&vm, "dX"), 50);
    assert_eq!(static_i(&vm, "dY"), 60);
    assert_eq!(static_i(&vm, "rCnt"), 1);
    assert_eq!(static_i(&vm, "rX"), 50);
    assert_eq!(static_i(&vm, "rY"), 60);
}

/// 事件不丢不重：一次 poll 前排 5 个不同键 press → 逐 poll 计数恰 5
///（每轮恰驱动一个回调线程，无丢失无重复）；同键 press press release
/// → press 计 2、release 计 1。
#[test]
fn five_key_events_exact_no_loss_no_dup() {
    let mut pool = Pool::new();
    let bytes = build_t(&mut pool);
    let mut vm = boot(bytes);
    assert_eq!(vm.poll(200_000).ran_threads, 1);
    for key in b'1'..=b'5' {
        vm.dispatch_key(key, true);
    }
    for round in 1..=5 {
        assert_eq!(
            vm.poll(200_000).ran_threads,
            1,
            "第 {round} 轮应恰驱动一个回调线程"
        );
    }
    assert_eq!(static_i(&vm, "pCnt"), 5, "5 个键事件不丢不重");
    assert_eq!(static_i(&vm, "rCnt"), 0);
    // 重复键：同键 press×2 + release×1（引擎层连发即重复按下语义）。
    vm.dispatch_key(b'7', true);
    vm.dispatch_key(b'7', true);
    vm.dispatch_key(b'7', false);
    for _ in 0..3 {
        assert_eq!(vm.poll(200_000).ran_threads, 1);
    }
    assert_eq!(static_i(&vm, "pCnt"), 7, "重复 press 计 2");
    assert_eq!(static_i(&vm, "rCnt"), 1, "release 计 1");
}

/// 常量面：Canvas 九个 gameAction 常量（UP=1..GAME_D=12）与
/// GameCanvas 九个 *_PRESSED 位常量按 MIDP-2.0 考证值注册可读；
/// Rust 侧 keyCode→位换算对数字键返回 None（无 gameAction 无位）。
#[test]
fn game_constants_canvas_and_gamecanvas_bits() {
    let mut pool = Pool::new();
    // 用 GameCanvas 子类夹具：T extends GameCanvas 触发 game 包类加载
    //（static_field 观测面按需查找，未加载类返回 None）。
    let bytes = build_game_t(&mut pool, 0, false, true);
    let mut vm = boot(bytes);
    assert_eq!(vm.poll(200_000).ran_threads, 1);
    // Canvas gameAction 常量（MIDP-2.0：UP/LEFT/RIGHT/DOWN/FIRE=
    // 1/2/5/6/8，GAME_A-D=9-12）
    assert_eq!(const_i(&vm, CANVAS, "UP"), GAME_UP);
    assert_eq!(const_i(&vm, CANVAS, "LEFT"), GAME_LEFT);
    assert_eq!(const_i(&vm, CANVAS, "RIGHT"), GAME_RIGHT);
    assert_eq!(const_i(&vm, CANVAS, "DOWN"), GAME_DOWN);
    assert_eq!(const_i(&vm, CANVAS, "FIRE"), GAME_FIRE);
    assert_eq!(const_i(&vm, CANVAS, "GAME_A"), GAME_A);
    assert_eq!(const_i(&vm, CANVAS, "GAME_B"), GAME_B);
    assert_eq!(const_i(&vm, CANVAS, "GAME_C"), GAME_C);
    assert_eq!(const_i(&vm, CANVAS, "GAME_D"), GAME_D);
    // GameCanvas 位常量（javadoc 考证：UP=2/DOWN=64/LEFT=4/RIGHT=128/
    // FIRE=256/GAME_A-D=512/1024/2048/4096）
    assert_eq!(const_i(&vm, GAMECANVAS, "UP_PRESSED"), UP_PRESSED);
    assert_eq!(const_i(&vm, GAMECANVAS, "DOWN_PRESSED"), DOWN_PRESSED);
    assert_eq!(const_i(&vm, GAMECANVAS, "LEFT_PRESSED"), LEFT_PRESSED);
    assert_eq!(const_i(&vm, GAMECANVAS, "RIGHT_PRESSED"), RIGHT_PRESSED);
    assert_eq!(const_i(&vm, GAMECANVAS, "FIRE_PRESSED"), FIRE_PRESSED);
    assert_eq!(const_i(&vm, GAMECANVAS, "GAME_A_PRESSED"), GAME_A_PRESSED);
    assert_eq!(const_i(&vm, GAMECANVAS, "GAME_B_PRESSED"), GAME_B_PRESSED);
    assert_eq!(const_i(&vm, GAMECANVAS, "GAME_C_PRESSED"), GAME_C_PRESSED);
    assert_eq!(const_i(&vm, GAMECANVAS, "GAME_D_PRESSED"), GAME_D_PRESSED);
    // Rust 纯面：位换算表（数字键无位；五方向各归各位）
    assert_eq!(
        gamecanvas::state_bit(input::KEY_UP),
        Some(UP_PRESSED as u32)
    );
    assert_eq!(
        gamecanvas::state_bit(input::KEY_DOWN),
        Some(DOWN_PRESSED as u32)
    );
    assert_eq!(
        gamecanvas::state_bit(input::KEY_LEFT),
        Some(LEFT_PRESSED as u32)
    );
    assert_eq!(
        gamecanvas::state_bit(input::KEY_RIGHT),
        Some(RIGHT_PRESSED as u32)
    );
    assert_eq!(
        gamecanvas::state_bit(input::KEY_FIRE),
        Some(FIRE_PRESSED as u32)
    );
    assert_eq!(gamecanvas::state_bit(53), None, "数字键不产生状态位");
}

/// getGameAction/getKeyCode 五方向全往返（既有用例仅覆盖 UP 单向）：
/// 每方向 keyCode→gameAction→keyCode 双射成立。
#[test]
fn game_action_roundtrip_all_five_directions() {
    // (key_code, game_action, 正向结果字段, 反向结果字段)
    let pairs: [(i32, i32, &'static str, &'static str); 5] = [
        (input::KEY_UP, GAME_UP, "gaU", "kcU"),
        (input::KEY_DOWN, GAME_DOWN, "gaD", "kcD"),
        (input::KEY_LEFT, GAME_LEFT, "gaL", "kcL"),
        (input::KEY_RIGHT, GAME_RIGHT, "gaR", "kcR"),
        (input::KEY_FIRE, GAME_FIRE, "gaF", "kcF"),
    ];
    let mut pool = Pool::new();
    let ga = pool.methodref(CANVAS, "getGameAction", "(I)I");
    let kc = pool.methodref(CANVAS, "getKeyCode", "(I)I");
    // 池索引先取号再构码。
    let mut ga_fields = Vec::new();
    let mut kc_fields = Vec::new();
    for (_, _, ga_name, kc_name) in &pairs {
        ga_fields.push(pool.fieldref("T", ga_name, "I"));
        kc_fields.push(pool.fieldref("T", kc_name, "I"));
    }
    let mut code = code_show(&mut pool);
    for (index, (key, action, _, _)) in pairs.iter().enumerate() {
        // ga_i = getGameAction(key_i)
        code.push(ALOAD_0);
        code.extend_from_slice(&push_int(*key));
        code.extend_from_slice(&op3(INVOKEVIRTUAL, ga));
        code.extend_from_slice(&op3(PUTSTATIC, ga_fields[index]));
        // kc_i = getKeyCode(action_i)
        code.push(ALOAD_0);
        code.extend_from_slice(&push_int(*action));
        code.extend_from_slice(&op3(INVOKEVIRTUAL, kc));
        code.extend_from_slice(&op3(PUTSTATIC, kc_fields[index]));
    }
    code.push(RETURN);
    let mut fields = Vec::new();
    for (_, _, ga_name, kc_name) in &pairs {
        fields.push(static_i_field(ga_name));
        fields.push(static_i_field(kc_name));
    }
    let bytes = build_class(
        &mut pool,
        "T",
        Some(CANVAS),
        &[],
        &fields,
        &[
            MethodSpec {
                access: 0x0001,
                name: "startApp",
                desc: "()V",
                code: Some((6, 2, code)),
                handlers: Vec::new(),
            },
            MethodSpec {
                access: 0x0001,
                name: "paint",
                desc: "(Ljavax/microedition/lcdui/Graphics;)V",
                code: Some((1, 2, vec![RETURN])),
                handlers: Vec::new(),
            },
        ],
    );
    let mut vm = boot(bytes);
    assert_eq!(vm.poll(200_000).ran_threads, 1);
    for (key, action, ga_name, kc_name) in &pairs {
        assert_eq!(
            static_i(&vm, ga_name),
            *action,
            "getGameAction({key}) 应为 {action}"
        );
        assert_eq!(
            static_i(&vm, kc_name),
            *key,
            "getKeyCode({action}) 应为 {key}"
        );
    }
}
