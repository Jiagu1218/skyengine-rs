//! MMAPI（JSR-135）Manager/Player 语义测试（T040）：T038 bootstrap 真
//! 实现的端到端验证。每个用例手构用户类 `T`（字节码见 common/
//! classbytes.rs），经真注册表调用 javax/microedition/media natives。
//!
//! 覆盖面：
//! - createPlayer(流+mime)：显式类型优先、null 走魔数嗅探（WAV 头）、
//!   嗅探不出抛 MediaException（Java catch）；
//! - 状态机全迁移：realize/prefetch/start/getState 逐步（100/200/300）、
//!   UNREALIZED 直 start、STARTED 幂等 no-op、stop→PREFETCHED 再 start、
//!   高态 realize/prefetch 幂等、getDuration()J == TIME_UNKNOWN(-1)；
//! - close 语义：CLOSED 后 getState()==400、close 幂等、后续调用
//!   IllegalStateException（catch）；
//! - setLoopCount：任意未关闭态接受、负值=无限（MIDP 规范）、CLOSED ISE；
//! - VolumeControl（经 VolumeControlImpl）：setLevel 钳 0-100 往返、
//!   mute 往返、getControl/getControls 单例同引用；
//! - locator：device://tone 建 Player 可 start、ToneControl 桩 null、
//!   未知 locator MediaException、http 定位符 ConnectionNotFoundException；
//! - playTone(III) 静默直过（Host 轨降级，Mock 即无音频设备）；
//! - PlayerListener：注册/重复注册/移除不炸、null 注册 NPE（catch）。
//!
//! 断言策略与 rms_semantics 一致：多指标折算一个 int 返回（组合公式
//! 在用例注释给出），Rust 侧独立算期望值比对。

#[allow(dead_code)]
mod common;

use common::classbytes::{build_class, provider, HandlerSpec, MethodSpec, Pool};

use skyengine_kvm::host::mock::MockHostContext;
use skyengine_kvm::interp::natives::RegistryDispatch;
use skyengine_kvm::interp::{enter, run, Machine, RunOutcome};
use skyengine_kvm::runtime::classspace::ClassSpace;
use skyengine_kvm::runtime::heap::Heap;
use skyengine_kvm::runtime::monitor::Monitors;
use skyengine_kvm::runtime::{Slot, ThreadId};
use skyengine_kvm::KvmError;

// ---- 操作码 ---------------------------------------------------------------

const ACONST_NULL: u8 = 0x01;
const ICONST_0: u8 = 0x03;
const ICONST_1: u8 = 0x04;
const ICONST_5: u8 = 0x08;
const BIPUSH: u8 = 0x10;
const SIPUSH: u8 = 0x11;
const LDC_W: u8 = 0x13;
const ILOAD_3: u8 = 0x1d;
const ISTORE_3: u8 = 0x3e;
const ALOAD_1: u8 = 0x2b;
const ALOAD_2: u8 = 0x2c;
const ASTORE_1: u8 = 0x4c;
const ASTORE_2: u8 = 0x4d;
const AALOAD: u8 = 0x32;
const POP: u8 = 0x57;
const DUP: u8 = 0x59;
const IMUL: u8 = 0x68;
const IADD: u8 = 0x60;
const IFEQ: u8 = 0x99;
const GOTO: u8 = 0xa7;
const IF_ACMPNE: u8 = 0xa6;
const IFNULL: u8 = 0xc6;
const IFNONNULL: u8 = 0xc7;
const IRETURN: u8 = 0xac;
const RETURN: u8 = 0xb1;
const INVOKESPECIAL: u8 = 0xb7;
const INVOKESTATIC: u8 = 0xb8;
const INVOKEVIRTUAL: u8 = 0xb6;
const NEW: u8 = 0xbb;
const NEWARRAY: u8 = 0xbc;
const BASTORE: u8 = 0x53;

/// newarray 的 atype（byte）。
const T_BYTE: u8 = 8;

/// 主线程标识（与 rms_semantics 夹具一致）。
const MAIN: ThreadId = ThreadId(1);

const MANAGER: &str = "javax/microedition/media/Manager";
const PLAYER: &str = "javax/microedition/media/Player";
const BAIS: &str = "java/io/ByteArrayInputStream";
const VCIMPL: &str = "javax/microedition/media/control/VolumeControlImpl";

// ---- 夹具 ------------------------------------------------------------------

/// 走真注册表的测试 VM（MockHostContext 即无音频设备环境：D7 降级前提）。
struct Vm {
    cs: ClassSpace,
    heap: Heap,
    monitors: Monitors,
    natives: RegistryDispatch,
    #[allow(dead_code)] // 与 rms 夹具对齐保留；host 由 machine 可变借用
    host: MockHostContext,
}

impl Vm {
    fn new(user: Vec<(&'static str, Vec<u8>)>) -> Self {
        let entries = user
            .into_iter()
            .map(|(name, bytes)| (name.to_string(), bytes))
            .collect();
        let mut cs = ClassSpace::new(Box::new(provider(entries)));
        let object = cs.load_class("java/lang/Object").expect("load Object");
        cs.object_class = Some(object);
        cs.load_class("java/lang/String").expect("load String");
        // 异常处理器捕获类预载（展开路径按类名匹配，与 rms 夹具一致）。
        for name in [
            "javax/microedition/media/MediaException",
            "javax/io/ConnectionNotFoundException",
            "java/lang/IllegalStateException",
            "java/lang/IllegalArgumentException",
            "java/lang/NullPointerException",
        ] {
            cs.load_class(name).unwrap_or_else(|e| panic!("load {name}: {e}"));
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

    /// 执行用户类 `T` 的静态 main（描述符由用例指定）。
    fn exec(&mut self, desc: &str) -> Result<RunOutcome, KvmError> {
        let class = self.cs.load_class("T")?;
        let mut frames = Vec::new();
        let mut machine = self.machine();
        enter(&mut machine, &mut frames, class, "main", desc, Vec::new())?;
        run(&mut machine, &mut frames, MAIN, None)
    }
}

// ---- 构码小工具 ------------------------------------------------------------

fn m_catch(
    desc: &'static str,
    stack: u16,
    locals: u16,
    code: Vec<u8>,
    handlers: Vec<HandlerSpec>,
) -> MethodSpec {
    MethodSpec {
        access: 0x0008,
        name: "main",
        desc,
        code: Some((stack, locals, code)),
        handlers,
    }
}

fn handler(start: u16, end: u16, at: u16, catch: Option<&'static str>) -> HandlerSpec {
    HandlerSpec { start_pc: start, end_pc: end, handler_pc: at, catch_class: catch }
}

/// 单常量池操作数指令（ldc/invoke* 均 3 字节）。
fn op3(op: u8, index: u16) -> [u8; 3] {
    let [hi, lo] = index.to_be_bytes();
    [op, hi, lo]
}

/// int 字面压栈（覆盖本文件用到的量程；大常量走池由调用方自理）。
fn push_int(value: i32) -> Vec<u8> {
    match value {
        0 => vec![ICONST_0],
        1 => vec![ICONST_1],
        5 => vec![ICONST_5],
        -128..=127 => vec![BIPUSH, value as i8 as u8],
        -32768..=32767 => {
            let [hi, lo] = (value as i16).to_be_bytes();
            vec![SIPUSH, hi, lo]
        }
        _ => panic!("测试常量超界: {value}"),
    }
}

fn store_int(local: u8) -> Vec<u8> {
    match local {
        1 => vec![ASTORE_1],
        2 => vec![ASTORE_2],
        3 => vec![ISTORE_3],
        n => vec![0x36, n],
    }
}

fn load_int(local: u8) -> Vec<u8> {
    match local {
        3 => vec![ILOAD_3],
        n => vec![0x15, n],
    }
}

fn load_ref(local: u8) -> Vec<u8> {
    match local {
        1 => vec![ALOAD_1],
        2 => vec![ALOAD_2],
        n => vec![0x19, n],
    }
}

/// 存引用局部变量（异常对象/数组引用等）。注意 ASTORE 系列速记仅
/// 覆盖 0x4b..0x4e（ASTORE_0..3）：0x50/0x51 是 LASTORE/FASTORE，
/// 槽位 >=4 必须走 wide 形式 [0x3a, n]。
fn store_ref(local: u8) -> Vec<u8> {
    match local {
        1 => vec![ASTORE_1],
        2 => vec![ASTORE_2],
        3 => vec![0x4e],
        n => vec![0x3a, n],
    }
}

/// 构造 byte[] 到指定局部变量（NEWARRAY + 逐字节 BASTORE）。
fn bytes_into_local(data: &[u8], local: u8) -> Vec<u8> {
    let mut code = push_int(data.len() as i32);
    code.push(NEWARRAY);
    code.push(T_BYTE);
    code.extend(store_int(local));
    for (index, &byte) in data.iter().enumerate() {
        // BASTORE 栈序：arrayref, index, value。
        code.extend(load_ref(local));
        code.extend(push_int(index as i32));
        code.extend(push_int(byte as i32));
        code.push(BASTORE);
    }
    code
}

/// createPlayer(流字节, mime 常量索引) 的调用码：BAIS 装配 + INVOKESTATIC，
/// Player 引用留在栈顶（调用方自行 ASTORE/POP）。
fn create_stream_code(
    pool: &mut Pool,
    bytes: &[u8],
    mime: Option<u16>,
) -> Vec<u8> {
    let mut code = bytes_into_local(bytes, 2);
    let init = pool.methodref(BAIS, "<init>", "([B)V");
    let create = pool.methodref(
        MANAGER,
        "createPlayer",
        "(Ljava/io/InputStream;Ljava/lang/String;)Ljavax/microedition/media/Player;",
    );
    code.extend(op3(NEW, pool.class(BAIS)));
    code.push(DUP);
    code.extend(load_ref(2));
    code.extend(op3(INVOKESPECIAL, init));
    match mime {
        Some(index) => code.extend(op3(LDC_W, index)),
        None => code.push(ACONST_NULL),
    }
    code.extend(op3(INVOKESTATIC, create));
    code
}

/// Player 实例方法的 invokevirtual 索引速记。
fn pmethod(pool: &mut Pool, name: &str, desc: &str) -> u16 {
    pool.methodref(PLAYER, name, desc)
}

/// 回填 3 字节分支偏移（相对指令首字节；对齐 rms_semantics 的 patch）。
fn patch(code: &mut [u8], at: u16, target: u16) {
    let offset = target as i16 - at as i16;
    code[at as usize + 1..at as usize + 3].copy_from_slice(&offset.to_be_bytes());
}

fn expect_int(outcome: RunOutcome) -> i32 {
    match outcome {
        RunOutcome::Returned(Some(Slot::Int(value))) => value,
        other => panic!("expected int return, got {other:?}"),
    }
}

fn run_int(
    pool: &mut Pool,
    desc: &'static str,
    stack: u16,
    locals: u16,
    code: Vec<u8>,
) -> i32 {
    run_int_with(pool, desc, stack, locals, code, Vec::new())
}

fn run_int_with(
    pool: &mut Pool,
    desc: &'static str,
    stack: u16,
    locals: u16,
    code: Vec<u8>,
    handlers: Vec<HandlerSpec>,
) -> i32 {
    let bytes = build_class(
        pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_catch(desc, stack, locals, code, handlers)],
    );
    let mut vm = Vm::new(vec![("T", bytes)]);
    expect_int(vm.exec(desc).expect("run main"))
}

/// 简化 WAV 头（RIFF + len + WAVE + 2 字节载荷）：嗅探命中 audio/x-wav。
fn wav_bytes() -> Vec<u8> {
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&[0x24, 0x00, 0x00, 0x00]);
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(&[0xF0, 0xFF]);
    out
}

// ---- 用例 ------------------------------------------------------------------

/// createPlayer(WAV 流, null)：嗅探 RIFF/WAVE → audio/x-wav；
/// 初始态 UNREALIZED(0)。返回 = state + equals("audio/x-wav")*1000。
#[test]
fn create_stream_sniffs_wav() {
    let mut pool = Pool::new();
    let wav_type = pool.string("audio/x-wav");
    let equals = pool.methodref("java/lang/String", "equals", "(Ljava/lang/Object;)Z");
    let get_state = pmethod(&mut pool, "getState", "()I");
    let get_type = pmethod(&mut pool, "getContentType", "()Ljava/lang/String;");

    let mut code = create_stream_code(&mut pool, &wav_bytes(), None);
    code.extend(store_int(1)); // p
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, get_state));
    code.extend(store_int(3)); // s = 0
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, get_type));
    code.extend(store_int(4)); // type
    // 返回 s + equals*1000
    code.extend(load_ref(4));
    code.extend(op3(LDC_W, wav_type));
    code.extend(op3(INVOKEVIRTUAL, equals));
    code.extend(push_int(1000));
    code.push(IMUL);
    code.extend(load_int(3));
    code.push(IADD);
    code.push(IRETURN);

    let want = 1000;
    assert_eq!(run_int(&mut pool, "()I", 8, 6, code), want);
}

/// 显式 mime 覆盖嗅探：MIDI 魔数字节 + mime "audio/x-wav" →
/// getContentType 等于显式值。返回 equals ? 1 : 0。
#[test]
fn explicit_mime_wins_over_sniff() {
    let mut pool = Pool::new();
    let wav_type = pool.string("audio/x-wav");
    let equals = pool.methodref("java/lang/String", "equals", "(Ljava/lang/Object;)Z");
    let get_type = pmethod(&mut pool, "getContentType", "()Ljava/lang/String;");

    let midi_magic = b"MThd\x00\x00\x00\x06\x00\x01".to_vec();
    let mut code = create_stream_code(&mut pool, &midi_magic, Some(wav_type));
    code.extend(store_int(1));
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, get_type));
    code.extend(store_int(2));
    code.extend(load_ref(2));
    code.extend(op3(LDC_W, wav_type));
    code.extend(op3(INVOKEVIRTUAL, equals));
    code.push(IRETURN);

    assert_eq!(run_int(&mut pool, "()I", 8, 4, code), 1);
}

/// 嗅探不出（"XXXXXXXX" + null mime）→ MediaException（Java catch）。
/// 返回 caught ? 1 : 0。
#[test]
fn unknown_bytes_throw_media_exception() {
    let mut pool = Pool::new();
    let mut code = Vec::new();
    code.push(ICONST_0);
    code.extend(store_int(1)); // caught
    code.extend(create_stream_code(&mut pool, b"XXXXXXXX", None));
    // createPlayer 正常返回的引用（不会发生）也要有 POP 兜底。
    let try_start = (code.len() - 3) as u16;
    let try_end = code.len() as u16;
    code.push(POP);
    // handler：ASTORE_2; ICONST_1; ISTORE_1
    let handler_pc = code.len() as u16;
    code.extend(store_int(2));
    code.push(ICONST_1);
    code.extend(store_int(1));
    code.extend(load_int(1));
    code.push(IRETURN);

    let handlers = vec![handler(
        try_start,
        try_end,
        handler_pc,
        Some("javax/microedition/media/MediaException"),
    )];
    assert_eq!(
        run_int_with(&mut pool, "()I", 8, 4, code, handlers),
        1
    );
}

/// 状态机全迁移：realize→100, prefetch→200, start→300, stop→200,
/// 再 start→300。返回逐步 getState 之和 = 1100。
#[test]
fn state_machine_full_walk() {
    let mut pool = Pool::new();
    let realize = pmethod(&mut pool, "realize", "()V");
    let prefetch = pmethod(&mut pool, "prefetch", "()V");
    let start = pmethod(&mut pool, "start", "()V");
    let stop = pmethod(&mut pool, "stop", "()V");
    let get_state = pmethod(&mut pool, "getState", "()I");

    let mut code = create_stream_code(&mut pool, &wav_bytes(), None);
    code.extend(store_int(1));
    code.push(ICONST_0);
    code.extend(store_int(3)); // sum
    for method in [realize, prefetch, start, stop, start] {
        code.extend(load_ref(1));
        code.extend(op3(INVOKEVIRTUAL, method));
        code.extend(load_ref(1));
        code.extend(op3(INVOKEVIRTUAL, get_state));
        code.extend(load_int(3));
        code.push(IADD);
        code.extend(store_int(3));
    }
    code.extend(load_int(3));
    code.push(IRETURN);

    let want = 100 + 200 + 300 + 200 + 300;
    assert_eq!(run_int(&mut pool, "()I", 8, 5, code), want);
}

/// UNREALIZED 直 start→300；STARTED 再 start no-op→300；stop→200；
/// getDuration() == TIME_UNKNOWN(-1) 加 7。返回 = 300+300+200+7 = 807。
#[test]
fn start_from_unrealized_and_duration_unknown() {
    let mut pool = Pool::new();
    let start = pmethod(&mut pool, "start", "()V");
    let stop = pmethod(&mut pool, "stop", "()V");
    let get_state = pmethod(&mut pool, "getState", "()I");
    let duration = pmethod(&mut pool, "getDuration", "()J");
    let minus_one = pool.long(-1);

    let mut code = create_stream_code(&mut pool, &wav_bytes(), None);
    code.extend(store_int(1));
    code.push(ICONST_0);
    code.extend(store_int(3)); // sum
    for method in [start, start, stop] {
        code.extend(load_ref(1));
        code.extend(op3(INVOKEVIRTUAL, method));
        code.extend(load_ref(1));
        code.extend(op3(INVOKEVIRTUAL, get_state));
        code.extend(load_int(3));
        code.push(IADD);
        code.extend(store_int(3));
    }
    // duration == -1 ? +7 : +0（LCMP 结果 0 即相等；long 常量走 LDC2_W）
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, duration));
    let [hi, lo] = minus_one.to_be_bytes();
    code.extend(vec![0x14, hi, lo]); // LDC2_W
    code.push(0x94); // LCMP
    let ifeq = code.len() as u16;
    code.extend(vec![IFEQ, 0, 0]);
    code.push(ICONST_0);
    code.extend(store_int(4));
    let goto_pc = code.len() as u16;
    code.extend(vec![GOTO, 0, 0]);
    let eq_pc = code.len() as u16;
    code.push(BIPUSH);
    code.push(7);
    code.extend(store_int(4));
    let end = code.len() as u16;
    patch(&mut code, ifeq, eq_pc);
    patch(&mut code, goto_pc, end);
    code.extend(load_int(3));
    code.extend(load_int(4));
    code.push(IADD);
    code.push(IRETURN);

    let want = 300 + 300 + 200 + 7;
    assert_eq!(run_int(&mut pool, "()I", 10, 6, code), want);
}

/// 高态幂等：start 直上 300 → realize no-op 300 → stop 200 →
/// realize no-op 200 → prefetch no-op 200。返回和 = 1200。
#[test]
fn realize_prefetch_idempotent_in_high_states() {
    let mut pool = Pool::new();
    let realize = pmethod(&mut pool, "realize", "()V");
    let prefetch = pmethod(&mut pool, "prefetch", "()V");
    let start = pmethod(&mut pool, "start", "()V");
    let stop = pmethod(&mut pool, "stop", "()V");
    let get_state = pmethod(&mut pool, "getState", "()I");

    let mut code = create_stream_code(&mut pool, &wav_bytes(), None);
    code.extend(store_int(1));
    code.push(ICONST_0);
    code.extend(store_int(3));
    for method in [start, realize, stop, realize, prefetch] {
        code.extend(load_ref(1));
        code.extend(op3(INVOKEVIRTUAL, method));
        code.extend(load_ref(1));
        code.extend(op3(INVOKEVIRTUAL, get_state));
        code.extend(load_int(3));
        code.push(IADD);
        code.extend(store_int(3));
    }
    code.extend(load_int(3));
    code.push(IRETURN);

    let want = 300 + 300 + 200 + 200 + 200;
    assert_eq!(run_int(&mut pool, "()I", 8, 5, code), want);
}

/// CLOSED 语义：close 后 getState==400、close 幂等再调不炸、
/// start 抛 IllegalStateException（catch）。返回 = 400 + caught*2000。
#[test]
fn closed_player_rejects_calls() {
    let mut pool = Pool::new();
    let close = pmethod(&mut pool, "close", "()V");
    let get_state = pmethod(&mut pool, "getState", "()I");
    let start = pmethod(&mut pool, "start", "()V");

    let mut code = create_stream_code(&mut pool, &wav_bytes(), None);
    code.extend(store_int(1));
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, close));
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, get_state));
    code.extend(store_int(3)); // 400
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, close)); // 幂等
    code.push(ICONST_0);
    code.extend(store_int(5)); // caught
    // try { p.start(); }
    let try_start = code.len() as u16;
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, start));
    let try_end = code.len() as u16;
    let handler_pc = code.len() as u16;
    code.extend(store_int(6)); // ASTORE_6 ex
    code.push(ICONST_1);
    code.extend(store_int(5));
    // 返回 state + caught*2000
    code.extend(load_int(3));
    code.extend(load_int(5));
    code.extend(push_int(2000));
    code.push(IMUL);
    code.push(IADD);
    code.push(IRETURN);

    let handlers = vec![handler(
        try_start,
        try_end,
        handler_pc,
        Some("java/lang/IllegalStateException"),
    )];
    let want = 400 + 2000;
    assert_eq!(run_int_with(&mut pool, "()I", 8, 8, code, handlers), want);
}

/// setLoopCount 规则：任意未关闭态接受（MIDP "may be called at any
/// time"）；-1 及更负值=无限循环（javadoc "-1 indicates infinite
/// looping"——gjxjwql 传负值设 BGM 无限，抛异常会令游戏弹画面栈）；
/// CLOSED → IllegalStateException。返回 ise（0=全程无异常）。
#[test]
fn loop_count_rules() {
    let mut pool = Pool::new();
    let set_loop = pmethod(&mut pool, "setLoopCount", "(I)V");
    let start = pmethod(&mut pool, "start", "()V");

    let mut code = create_stream_code(&mut pool, &wav_bytes(), None);
    code.extend(store_int(1));
    code.push(ICONST_0);
    code.extend(store_int(5)); // ise
    // 合法存取（3 / 0 无限）
    code.extend(load_ref(1));
    code.extend(push_int(3));
    code.extend(op3(INVOKEVIRTUAL, set_loop));
    code.extend(load_ref(1));
    code.push(ICONST_0);
    code.extend(op3(INVOKEVIRTUAL, set_loop));
    // 负值 = 无限循环，不抛
    let t1 = code.len() as u16;
    code.extend(load_ref(1));
    code.extend(push_int(-1));
    code.extend(op3(INVOKEVIRTUAL, set_loop));
    let t1_end = code.len() as u16;
    // start 后 setLoopCount(5) 同样接受
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, start));
    let t2 = code.len() as u16;
    code.extend(load_ref(1));
    code.extend(push_int(5));
    code.extend(op3(INVOKEVIRTUAL, set_loop));
    let t2_end = code.len() as u16;
    // 正常路径跳过 handler 区；异常时 ise=1 后同路返回。
    let skip = code.len() as u16;
    code.extend(vec![GOTO, 0, 0]);
    let h_any = code.len() as u16;
    code.extend(store_ref(6)); // 异常引用出栈
    code.push(ICONST_1);
    code.extend(store_int(5));
    let merge = code.len() as u16;
    patch(&mut code, skip, merge);
    code.extend(load_int(5));
    code.push(IRETURN);

    let handlers = vec![
        handler(t1, t1_end, h_any, Some("java/lang/IllegalArgumentException")),
        handler(t2, t2_end, h_any, Some("java/lang/IllegalStateException")),
    ];
    assert_eq!(run_int_with(&mut pool, "()I", 8, 8, code, handlers), 0);
}

/// VolumeControl（经 VolumeControlImpl）：setLevel(150)→100、
/// setLevel(-30)→0、setLevel(80)+getLevel→80、mute 往返 1/0、
/// getControls()[0] 与 getControl 单例同引用。
/// 返回 = 100*1000000 + 0*100000 + 80*1000 + 1*100 + 0*10 + eq。
#[test]
fn volume_control_clamps_and_singleton() {
    let mut pool = Pool::new();
    let volume_name = pool.string("VolumeControl");
    let get_control = pmethod(&mut pool, "getControl", "(Ljava/lang/String;)Ljavax/microedition/media/Control;");
    let get_controls = pmethod(&mut pool, "getControls", "()[Ljavax/microedition/media/Control;");
    let set_level = pool.methodref(VCIMPL, "setLevel", "(I)I");
    let get_level = pool.methodref(VCIMPL, "getLevel", "()I");
    let set_mute = pool.methodref(VCIMPL, "setMute", "(Z)V");
    let is_muted = pool.methodref(VCIMPL, "isMuted", "()Z");
    let m1m = pool.integer(1_000_000);
    let m100k = pool.integer(100_000);
    let m1k = pool.integer(1_000);

    let mut code = create_stream_code(&mut pool, &wav_bytes(), None);
    code.extend(store_int(1)); // p
    // impl = p.getControl("VolumeControl")
    code.extend(load_ref(1));
    code.extend(op3(LDC_W, volume_name));
    code.extend(op3(INVOKEVIRTUAL, get_control));
    code.extend(store_int(2));
    // r100 = impl.setLevel(150)
    code.extend(load_ref(2));
    code.extend(push_int(150));
    code.extend(op3(INVOKEVIRTUAL, set_level));
    code.extend(store_int(3));
    // r0 = impl.setLevel(-30)
    code.extend(load_ref(2));
    code.extend(push_int(-30));
    code.extend(op3(INVOKEVIRTUAL, set_level));
    code.extend(store_int(4));
    // impl.setLevel(80)（返回值丢弃）；lvl = impl.getLevel()
    code.extend(load_ref(2));
    code.extend(push_int(80));
    code.extend(op3(INVOKEVIRTUAL, set_level));
    code.push(POP);
    code.extend(load_ref(2));
    code.extend(op3(INVOKEVIRTUAL, get_level));
    code.extend(store_int(5));
    // m1 = impl.isMuted()（setMute(true) 后）
    code.extend(load_ref(2));
    code.push(ICONST_1);
    code.extend(op3(INVOKEVIRTUAL, set_mute));
    code.extend(load_ref(2));
    code.extend(op3(INVOKEVIRTUAL, is_muted));
    code.extend(store_int(6));
    // m0 = impl.isMuted()（setMute(false) 后）
    code.extend(load_ref(2));
    code.push(ICONST_0);
    code.extend(op3(INVOKEVIRTUAL, set_mute));
    code.extend(load_ref(2));
    code.extend(op3(INVOKEVIRTUAL, is_muted));
    code.extend(store_int(7));
    // eq = (p.getControl("VolumeControl") == p.getControls()[0])
    code.extend(load_ref(1));
    code.extend(op3(LDC_W, volume_name));
    code.extend(op3(INVOKEVIRTUAL, get_control));
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, get_controls));
    code.push(ICONST_0);
    code.push(AALOAD);
    let branch = code.len() as u16;
    code.extend(vec![IF_ACMPNE, 0, 0]);
    code.push(ICONST_1);
    code.extend(store_int(8));
    let goto_pc = code.len() as u16;
    code.extend(vec![GOTO, 0, 0]);
    let ne_pc = code.len() as u16;
    code.push(ICONST_0);
    code.extend(store_int(8));
    let end = code.len() as u16;
    patch(&mut code, branch, ne_pc);
    patch(&mut code, goto_pc, end);
    // 返回折算
    code.extend(load_int(3));
    code.extend(op3(LDC_W, m1m));
    code.push(IMUL);
    code.extend(load_int(4));
    code.extend(op3(LDC_W, m100k));
    code.push(IMUL);
    code.push(IADD);
    code.extend(load_int(5));
    code.extend(op3(LDC_W, m1k));
    code.push(IMUL);
    code.push(IADD);
    code.extend(load_int(6));
    code.extend(push_int(100));
    code.push(IMUL);
    code.push(IADD);
    code.extend(load_int(7));
    code.extend(push_int(10));
    code.push(IMUL);
    code.push(IADD);
    code.extend(load_int(8));
    code.push(IADD);
    code.push(IRETURN);

    let want = ((100 * 1_000_000) + 80 * 1_000 + 100) + 1;
    assert_eq!(run_int(&mut pool, "()I", 10, 10, code), want);
}

/// locator："device://tone" 建 Player（非 null）可 start→300，
/// getControl("ToneControl") 桩 null。返回 = 300 + nullTone。
#[test]
fn tone_locator_player() {
    let mut pool = Pool::new();
    let tone_locator = pool.string("device://tone");
    let tone_name = pool.string("ToneControl");
    let create = pool.methodref(
        MANAGER,
        "createPlayer",
        "(Ljava/lang/String;)Ljavax/microedition/media/Player;",
    );
    let start = pmethod(&mut pool, "start", "()V");
    let get_state = pmethod(&mut pool, "getState", "()I");
    let get_control = pmethod(&mut pool, "getControl", "(Ljava/lang/String;)Ljavax/microedition/media/Control;");

    let mut code = Vec::new();
    code.extend(op3(LDC_W, tone_locator));
    code.extend(op3(INVOKESTATIC, create));
    code.extend(store_int(1));
    // p null → 直接返回 0
    code.extend(load_ref(1));
    let null_check = code.len() as u16;
    code.extend(vec![IFNULL, 0, 0]);
    // s = p.start(); p.getState()
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, start));
    code.extend(load_ref(1));
    code.extend(op3(INVOKEVIRTUAL, get_state));
    code.extend(store_int(3));
    // toneNull = (p.getControl("ToneControl") == null) ? 1 : 0
    code.push(ICONST_1);
    code.extend(store_int(4));
    code.extend(load_ref(1));
    code.extend(op3(LDC_W, tone_name));
    code.extend(op3(INVOKEVIRTUAL, get_control));
    let nn = code.len() as u16;
    code.extend(vec![IFNONNULL, 0, 0]);
    let goto_pc = code.len() as u16;
    code.extend(vec![GOTO, 0, 0]);
    let clear_pc = code.len() as u16;
    code.push(ICONST_0);
    code.extend(store_int(4));
    let end = code.len() as u16;
    patch(&mut code, nn, clear_pc);
    patch(&mut code, goto_pc, end);
    // 返回 s + toneNull
    code.extend(load_int(3));
    code.extend(load_int(4));
    code.push(IADD);
    code.push(IRETURN);
    // p null 分支：返回 0
    let zero_pc = code.len() as u16;
    code.push(ICONST_0);
    code.push(IRETURN);
    patch(&mut code, null_check, zero_pc);

    assert_eq!(run_int(&mut pool, "()I", 8, 6, code), 300 + 1);
}

/// locator 异常：未知 → MediaException；http →
/// ConnectionNotFoundException。返回 = media*10 + cnf = 11。
#[test]
fn locators_throw() {
    let mut pool = Pool::new();
    let bogus = pool.string("bogus://x");
    let http = pool.string("http://media.example/x.mid");
    let create = pool.methodref(
        MANAGER,
        "createPlayer",
        "(Ljava/lang/String;)Ljavax/microedition/media/Player;",
    );

    let mut code = Vec::new();
    code.push(ICONST_0);
    code.extend(store_int(3)); // media
    code.push(ICONST_0);
    code.extend(store_int(4)); // cnf
    // try createPlayer("bogus://x") → MediaException
    let t1 = code.len() as u16;
    code.extend(op3(LDC_W, bogus));
    code.extend(op3(INVOKESTATIC, create));
    let t1_end = code.len() as u16;
    code.push(POP);
    let h1 = code.len() as u16;
    code.extend(store_ref(5));
    code.push(ICONST_1);
    code.extend(store_int(3));
    // try createPlayer("http://...") → ConnectionNotFoundException
    let t2 = code.len() as u16;
    code.extend(op3(LDC_W, http));
    code.extend(op3(INVOKESTATIC, create));
    let t2_end = code.len() as u16;
    code.push(POP);
    let h2 = code.len() as u16;
    code.extend(store_ref(5));
    code.push(ICONST_1);
    code.extend(store_int(4));
    // 返回 media*10 + cnf
    code.extend(load_int(3));
    code.extend(push_int(10));
    code.push(IMUL);
    code.extend(load_int(4));
    code.push(IADD);
    code.push(IRETURN);

    let handlers = vec![
        handler(t1, t1_end, h1, Some("javax/microedition/media/MediaException")),
        handler(t2, t2_end, h2, Some("javax/io/ConnectionNotFoundException")),
    ];
    assert_eq!(run_int_with(&mut pool, "()I", 8, 6, code, handlers), 11);
}

/// playTone(60, 50, 100) 静默直过（无音频设备降级路径）。返回 1。
#[test]
fn play_tone_passes_silently() {
    let mut pool = Pool::new();
    let play_tone = pool.methodref(MANAGER, "playTone", "(III)V");
    let mut code = Vec::new();
    code.extend(push_int(60));
    code.extend(push_int(50));
    code.extend(push_int(100));
    code.extend(op3(INVOKESTATIC, play_tone));
    code.push(ICONST_1);
    code.push(IRETURN);
    assert_eq!(run_int(&mut pool, "()I", 8, 4, code), 1);
}

/// PlayerListener：注册/重复注册/移除不炸；null 注册 → NPE（catch）。
/// 返回 = 1（流程走通） + npe_caught。
#[test]
fn player_listener_register_and_remove() {
    let mut pool = Pool::new();
    let add = pmethod(&mut pool, "addPlayerListener", "(Ljavax/microedition/media/PlayerListener;)V");
    let remove = pmethod(&mut pool, "removePlayerListener", "(Ljavax/microedition/media/PlayerListener;)V");
    let t2_init = pool.methodref("T2", "<init>", "()V");

    // 监听器类 T2 implements PlayerListener（playerUpdate 空实现）
    let object_init = pool.methodref("java/lang/Object", "<init>", "()V");
    let t2 = build_class(
        &mut pool,
        "T2",
        Some("java/lang/Object"),
        &["javax/microedition/media/PlayerListener"],
        &[],
        &[
            MethodSpec {
                access: 0x0001,
                name: "<init>",
                desc: "()V",
                code: Some((2, 1, {
                    let mut c = vec![0x2a]; // ALOAD_0
                    c.extend(op3(INVOKESPECIAL, object_init));
                    c.push(RETURN);
                    c
                })),
                handlers: Vec::new(),
            },
            MethodSpec {
                access: 0x0001,
                name: "playerUpdate",
                desc: "(Ljavax/microedition/media/Player;Ljava/lang/String;Ljava/lang/Object;)V",
                code: Some((1, 4, vec![RETURN])),
                handlers: Vec::new(),
            },
        ],
    );

    let mut code = create_stream_code(&mut pool, &wav_bytes(), None);
    code.extend(store_int(1)); // p
    code.push(ICONST_0);
    code.extend(store_int(4)); // npe
    // T2 l = new T2()
    code.extend(op3(NEW, pool.class("T2")));
    code.push(DUP);
    code.extend(op3(INVOKESPECIAL, t2_init));
    code.extend(store_int(2));
    // p.addPlayerListener(l) 两次（幂等）+ remove 一次
    code.extend(load_ref(1));
    code.extend(load_ref(2));
    code.extend(op3(INVOKEVIRTUAL, add));
    code.extend(load_ref(1));
    code.extend(load_ref(2));
    code.extend(op3(INVOKEVIRTUAL, add));
    code.extend(load_ref(1));
    code.extend(load_ref(2));
    code.extend(op3(INVOKEVIRTUAL, remove));
    // try p.addPlayerListener(null) → NullPointerException
    let t1 = code.len() as u16;
    code.extend(load_ref(1));
    code.push(ACONST_NULL);
    code.extend(op3(INVOKEVIRTUAL, add));
    let t1_end = code.len() as u16;
    let h1 = code.len() as u16;
    code.extend(store_ref(5));
    code.push(ICONST_1);
    code.extend(store_int(4));
    // 返回 1 + npe
    code.push(ICONST_1);
    code.extend(load_int(4));
    code.push(IADD);
    code.push(IRETURN);

    let handlers = vec![handler(t1, t1_end, h1, Some("java/lang/NullPointerException"))];
    let bytes = build_class(
        &mut pool,
        "T",
        Some("java/lang/Object"),
        &[],
        &[],
        &[m_catch("()I", 8, 7, code, handlers)],
    );
    let mut vm = Vm::new(vec![("T", bytes), ("T2", t2)]);
    assert_eq!(expect_int(vm.exec("()I").expect("run main")), 2);
}
