//! MIDlet 生命周期语义测试（T044/T045/T046 故障注入）：
//! notifyDestroyed/notifyPaused/System.exit 的"native 置位 → poll 消费
//! → 回调绿线程 → 线程死后收尾"全链路，以及未捕获异常的 fatal 通道
//! （`Vm::last_fatal`）。
//!
//! 夹具：手构 class（`T extends javax/microedition/midlet/MIDlet`，
//! bootstrap 桩可被 jar 子类 extends）打成最小 stored zip 喂
//! `install_jar`；回调执行证据经 T 静态字段（dcount/pcount）由
//! `Vm::static_field` 回读。
//!
//! 构码铁律（前车之鉴）：带参构造 NEW+DUP+参数+INVOKESPECIAL；
//! 分支指令消耗条件值后不再 POP；池索引先取号再构码。

mod common;

use common::classbytes::{FieldSpec, MethodSpec, Pool, build_class};
use skyengine_kvm::host::mock::MockHostContext;
use skyengine_kvm::vm::{Vm, VmConfig, VmState};

// ---- 操作码（仅本文件用到的） --------------------------------------------

const ICONST_0: u8 = 0x03;
const ICONST_1: u8 = 0x04;
const ALOAD_0: u8 = 0x2a;
const NEW: u8 = 0xbb;
const DUP: u8 = 0x59;
const GETSTATIC: u8 = 0xb2;
const PUTSTATIC: u8 = 0xb3;
const INVOKESPECIAL: u8 = 0xb7;
const INVOKEVIRTUAL: u8 = 0xb6;
const INVOKESTATIC: u8 = 0xb8;
const ATHROW: u8 = 0xbf;
const RETURN: u8 = 0xb1;
const GOTO: u8 = 0xa7;

/// 跳转偏移的字节码补丁（GOTO 短跳，自分支指令起算的 i16）。
fn goto_jump(offset: i16) -> [u8; 3] {
    let [hi, lo] = offset.to_be_bytes();
    [GOTO, hi, lo]
}

const MIDLET: &str = "javax/microedition/midlet/MIDlet";
const MSC_EXCEPTION: &str = "javax/microedition/midlet/MIDletStateChangeException";

// ---- 夹具 ----------------------------------------------------------------

/// startApp 行为脚本（构码前池条目已就位）。
enum StartApp {
    /// 调 this.notifyDestroyed() 后返回（主线程自然死亡）。
    NotifyDestroyed,
    /// 自旋循环反复调 this.notifyDestroyed()（主线程保持存活，反复制造
    /// destroy 请求——验证拒绝销毁后的"可再次 destroy"）。
    SpinNotifyDestroyed,
    /// 自旋循环反复调 System.exit(0)（强制销毁路径）。
    SpinSystemExit,
    /// 调 this.notifyPaused() 后返回。
    NotifyPaused,
    /// NEW RuntimeException; DUP; INVOKESPECIAL <init>; ATHROW。
    Throw,
}

/// destroyApp(Z)V 行为脚本。
enum DestroyApp {
    /// dcount++ 后正常返回。
    Count,
    /// dcount++ 后抛 MIDletStateChangeException（拒绝销毁）。
    CountAndThrow,
}

/// 组装 T：静态 int dcount/pcount + startApp（脚本）+ destroyApp（脚本，
/// None 则不声明）+ pauseApp（need_pause 时 pcount++）。
fn build_t(
    pool: &mut Pool,
    start_app: StartApp,
    destroy_app: Option<DestroyApp>,
    need_pause: bool,
) -> Vec<u8> {
    let dcount = pool.fieldref("T", "dcount", "I");
    let pcount = pool.fieldref("T", "pcount", "I");
    let notify_destroyed = pool.methodref(MIDLET, "notifyDestroyed", "()V");
    let notify_paused = pool.methodref(MIDLET, "notifyPaused", "()V");
    let system_exit = pool.methodref("java/lang/System", "exit", "(I)V");
    let runtime_init = pool.methodref("java/lang/RuntimeException", "<init>", "()V");
    let msc_init = pool.methodref(MSC_EXCEPTION, "<init>", "()V");

    let mut start = Vec::new();
    match start_app {
        StartApp::NotifyDestroyed => {
            start.extend_from_slice(&[ALOAD_0]);
            let [hi, lo] = notify_destroyed.to_be_bytes();
            start.extend_from_slice(&[INVOKEVIRTUAL, hi, lo]);
            start.push(RETURN);
        }
        StartApp::SpinNotifyDestroyed => {
            // L: aload_0; invokevirtual notifyDestroyed; goto L
            start.extend_from_slice(&[ALOAD_0]);
            let [hi, lo] = notify_destroyed.to_be_bytes();
            start.extend_from_slice(&[INVOKEVIRTUAL, hi, lo]);
            // 分支自身 3 字节，自跳转偏移 = -3。
            start.extend_from_slice(&goto_jump(-4));
        }
        StartApp::SpinSystemExit => {
            // L: iconst_0; invokestatic System.exit(I)V; goto L
            start.push(ICONST_0);
            let [hi, lo] = system_exit.to_be_bytes();
            start.extend_from_slice(&[INVOKESTATIC, hi, lo]);
            start.extend_from_slice(&goto_jump(-4));
        }
        StartApp::NotifyPaused => {
            start.extend_from_slice(&[ALOAD_0]);
            let [hi, lo] = notify_paused.to_be_bytes();
            start.extend_from_slice(&[INVOKEVIRTUAL, hi, lo]);
            start.push(RETURN);
        }
        StartApp::Throw => {
            let [chi, clo] = pool.class("java/lang/RuntimeException").to_be_bytes();
            start.extend_from_slice(&[NEW, chi, clo, DUP]);
            let [hi, lo] = runtime_init.to_be_bytes();
            start.extend_from_slice(&[INVOKESPECIAL, hi, lo]);
            start.push(ATHROW);
        }
    }

    let mut methods = Vec::new();
    methods.push(MethodSpec {
        access: 0x0001, // public 实例（start_midlet 帧序约定）
        name: "startApp",
        desc: "()V",
        code: Some((3, 1, start)),
        handlers: Vec::new(),
    });
    if let Some(script) = destroy_app {
        let mut code = Vec::new();
        // dcount++（回调执行证据，含拒绝路径——计数先于抛出）。
        let [hi, lo] = dcount.to_be_bytes();
        code.extend_from_slice(&[GETSTATIC, hi, lo, ICONST_1]);
        code.push(0x60); // iadd
        code.extend_from_slice(&[PUTSTATIC, hi, lo]);
        match script {
            DestroyApp::Count => code.push(RETURN),
            DestroyApp::CountAndThrow => {
                let [chi, clo] = pool.class(MSC_EXCEPTION).to_be_bytes();
                code.extend_from_slice(&[NEW, chi, clo, DUP]);
                let [hi, lo] = msc_init.to_be_bytes();
                code.extend_from_slice(&[INVOKESPECIAL, hi, lo]);
                code.push(ATHROW);
            }
        }
        methods.push(MethodSpec {
            access: 0x0004, // protected（MIDP 规范声明；引擎只查方法体）
            name: "destroyApp",
            desc: "(Z)V",
            code: Some((3, 2, code)),
            handlers: Vec::new(),
        });
    }
    if need_pause {
        let mut code = Vec::new();
        let [hi, lo] = pcount.to_be_bytes();
        code.extend_from_slice(&[GETSTATIC, hi, lo, ICONST_1]);
        code.push(0x60); // iadd
        code.extend_from_slice(&[PUTSTATIC, hi, lo, RETURN]);
        methods.push(MethodSpec {
            access: 0x0004,
            name: "pauseApp",
            desc: "()V",
            code: Some((3, 1, code)),
            handlers: Vec::new(),
        });
    }

    build_class(
        pool,
        "T",
        Some(MIDLET),
        &[],
        &[
            FieldSpec {
                access: 0x0008, // static
                name: "dcount",
                desc: "I",
                constant: None,
            },
            FieldSpec {
                access: 0x0008,
                name: "pcount",
                desc: "I",
                constant: None,
            },
        ],
        &methods,
    )
}

/// 最小 stored zip（jar::unpack 只认 EOCD/中央目录/局部头三件套）。
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
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
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

/// 装配 Vm（Mock 宿主 240x320）并启动 T。
fn boot(bytes: Vec<u8>) -> Vm {
    let host = MockHostContext::new(240, 320);
    let zip = stored_zip(&[("T.class", &bytes)]);
    let mut vm = Vm::new(Box::new(host), VmConfig::default());
    vm.install_jar("t", &zip).expect("install jar");
    vm.start_midlet("T").expect("start midlet");
    vm
}

/// 轮询驱动至停摆或轮数上限（自旋夹具靠上限截断）。
fn drive(vm: &mut Vm, rounds: usize) {
    for _ in 0..rounds {
        if !vm.is_active() {
            break;
        }
        vm.poll(200_000);
    }
}

/// T 静态 int 字段回读。
fn static_int(vm: &Vm, field: &str) -> i32 {
    match vm.static_field("T", field, "I") {
        Some(skyengine_kvm::runtime::Slot::Int(value)) => value,
        other => panic!("T.{field} corrupt: {other:?}"),
    }
}

// ---- 用例 ----------------------------------------------------------------

/// startApp 抛 RuntimeException 未捕获 → 主线程异常死亡 → 全线程死尽后
/// Vm 转 Dead 且 fatal 通道带类名（T045 错误画面根因）。
#[test]
fn uncaught_in_startapp_kills_vm_with_fatal() {
    let mut pool = Pool::new();
    let bytes = build_t(&mut pool, StartApp::Throw, Some(DestroyApp::Count), false);
    let mut vm = boot(bytes);
    drive(&mut vm, 10);
    assert_eq!(vm.state(), VmState::Dead, "全线程死尽后应转 Dead");
    let fatal = vm.last_fatal().expect("未捕获异常应升格 fatal");
    assert!(
        fatal.contains("RuntimeException"),
        "fatal 应含异常类名，got {fatal:?}"
    );
}

/// notifyDestroyed → destroyApp(false) 回调执行（静态计数）→ 回调线程
/// 正常返回死后 Vm 转 Dead，且无 fatal（正常退出，非崩溃）。
#[test]
fn notify_destroyed_runs_destroyapp_then_dies() {
    let mut pool = Pool::new();
    let bytes = build_t(&mut pool, StartApp::NotifyDestroyed, Some(DestroyApp::Count), false);
    let mut vm = boot(bytes);
    drive(&mut vm, 10);
    assert_eq!(vm.state(), VmState::Dead);
    assert_eq!(static_int(&vm, "dcount"), 1, "destroyApp 回调应执行一次");
    assert_eq!(vm.last_fatal(), None, "正常退出不应有 fatal");
}

/// destroyApp 回调抛 MIDletStateChangeException 且非强制（notifyDestroyed
/// 路径）→ 拒绝销毁：Vm 保持 Running，且主循环可再次 destroy（回调重复
/// 执行，dcount 累计）。
#[test]
fn destroyapp_reject_keeps_running_and_allows_retry() {
    let mut pool = Pool::new();
    let bytes = build_t(
        &mut pool,
        StartApp::SpinNotifyDestroyed,
        Some(DestroyApp::CountAndThrow),
        false,
    );
    let mut vm = boot(bytes);
    drive(&mut vm, 40);
    assert_eq!(vm.state(), VmState::Running, "非强制销毁被拒绝后应保持 Running");
    assert!(
        static_int(&vm, "dcount") >= 2,
        "拒绝后应可再次 destroy（回调重复执行）"
    );
    assert_eq!(vm.last_fatal(), None, "拒绝销毁不是崩溃");
    vm.shutdown();
}

/// System.exit → 强制销毁（unconditional=true）：destroyApp 回调即使抛
/// MIDletStateChangeException 也照销（异常吞掉），Vm 转 Dead 无 fatal。
#[test]
fn forced_destroy_via_system_exit_kills_despite_reject() {
    let mut pool = Pool::new();
    let bytes = build_t(
        &mut pool,
        StartApp::SpinSystemExit,
        Some(DestroyApp::CountAndThrow),
        false,
    );
    let mut vm = boot(bytes);
    drive(&mut vm, 40);
    assert_eq!(vm.state(), VmState::Dead, "强制销毁不可拒绝");
    assert!(
        static_int(&vm, "dcount") >= 1,
        "destroyApp 回调应已执行"
    );
    assert_eq!(vm.last_fatal(), None, "强制销毁吞异常，不算崩溃");
}

/// notifyPaused → pauseApp 回调执行（pcount 证据）；暂停后主线程自然
/// 死亡，全线程死尽 → Dead（无 fatal）。
#[test]
fn notify_paused_invokes_pauseapp_callback() {
    let mut pool = Pool::new();
    let bytes = build_t(&mut pool, StartApp::NotifyPaused, None, true);
    let mut vm = boot(bytes);
    drive(&mut vm, 10);
    assert_eq!(static_int(&vm, "pcount"), 1, "pauseApp 回调应执行一次");
    assert_eq!(vm.state(), VmState::Dead, "主线程死后全线程死尽转 Dead");
    assert_eq!(vm.last_fatal(), None);
}
