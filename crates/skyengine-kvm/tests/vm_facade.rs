//! T013 Vm 门面集成测试：install_jar 索引 / start_midlet 构帧执行 /
//! 状态机守卫 / 事件入队。夹具：gjxjwql.mrp 中的真实 jc.jar + 手构合成类。

mod common;

use std::fs;

use common::classbytes::{FieldSpec, MethodSpec, Pool, ROOT, build_class};
use common::{fixture_path, load_jar, mrp_read_entry};
use skyengine_kvm::host::mock::MockHostContext;
use skyengine_kvm::runtime::Slot;
use skyengine_kvm::{Vm, VmConfig, VmError, VmState};

fn mock_vm() -> Vm {
    Vm::new(
        Box::new(MockHostContext::new(240, 320)),
        VmConfig::default(),
    )
}

// ---- 手构 zip（store 法）：把合成类塞进 install_jar 通道 -------------------
// 注意 zip 为小端格式，所有多字节字段用 to_le_bytes。

/// 组装最小 store-method zip（CRC 不校验，仅结构完整）。
fn zip_store(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut offsets = Vec::new();
    for (name, data) in entries {
        offsets.push(out.len() as u32);
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&[0u8; 14]); // 版本/标志/方法/时间/日期/crc
        out.extend_from_slice(&(data.len() as u32).to_le_bytes()); // 压缩尺寸
        out.extend_from_slice(&(data.len() as u32).to_le_bytes()); // 原始尺寸
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // 扩展字段长
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
    }
    let cd_start = out.len() as u32;
    let mut central = Vec::new();
    for ((name, data), offset) in entries.iter().zip(&offsets) {
        central.extend_from_slice(b"PK\x01\x02");
        central.extend_from_slice(&[0u8; 16]); // 版本/标志/方法/时间/日期/crc
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0u8; 8]); // 扩展/注释/盘符/内部属性
        central.extend_from_slice(&0u32.to_le_bytes()); // 外部属性
        central.extend_from_slice(&offset.to_le_bytes()); // 局部头偏移
        central.extend_from_slice(name.as_bytes());
    }
    out.extend_from_slice(&central);
    // EOCD：盘符/计数/尺寸/偏移/注释长，逐字段按规范顺序写入。
    out.extend_from_slice(b"PK\x05\x06");
    out.extend_from_slice(&0u16.to_le_bytes()); // 本盘号
    out.extend_from_slice(&0u16.to_le_bytes()); // 中央目录起始盘号
    let count = entries.len() as u16;
    out.extend_from_slice(&count.to_le_bytes()); // 本盘条目数
    out.extend_from_slice(&count.to_le_bytes()); // 总条目数
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_start.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // 注释长
    out
}

fn m(access: u16, name: &'static str, code: Vec<u8>) -> MethodSpec {
    MethodSpec {
        access,
        name,
        desc: "()V",
        code: Some((8, 4, code)),
        handlers: Vec::new(),
    }
}

fn f(access: u16, name: &'static str, desc: &'static str) -> FieldSpec {
    FieldSpec {
        access,
        name,
        desc,
        constant: None,
    }
}

/// 真实 jc.jar 安装：全部类条目可查。
#[test]
fn install_jar_indexes_jc_classes() {
    let mrp = fs::read(fixture_path()).expect("read gjxjwql.mrp fixture");
    let jar = mrp_read_entry(&mrp, "jc.jar");
    let mut vm = mock_vm();
    vm.install_jar("jc.jar", &jar).unwrap();
    assert_eq!(vm.state(), VmState::JarReady);
    // 与测试侧独立解包的夹具对账：类条目数一致（T013 验收口径 89）。
    let fixture = load_jar();
    let expect_classes = fixture.keys().filter(|k| k.ends_with(".class")).count();
    assert_eq!(vm.jar_class_count(), expect_classes);
    assert_eq!(expect_classes, 89);
    // 任取一个类条目名（去 .class 后）可查。
    let some = fixture
        .keys()
        .find(|k| k.ends_with(".class"))
        .expect("fixture has classes");
    assert!(vm.has_jar_entry(some.strip_suffix(".class").unwrap()));
}

/// 坏数据 → BadJar，状态保持 Empty。
#[test]
fn install_jar_rejects_garbage() {
    let mut vm = mock_vm();
    let result = vm.install_jar("bad", &[1, 2, 3, 4, 5]);
    assert!(matches!(result, Err(VmError::BadJar(_))));
    assert_eq!(vm.state(), VmState::Empty);
}

/// 合成 MIDlet：<init> 置 mark=1，startApp 做 mark+=1；
/// poll 跑完后 mark==2 证明两者按序执行。
#[test]
fn start_midlet_runs_init_then_start_app() {
    let mut pool = Pool::new();
    let mark = pool.fieldref("M", "mark", "I");
    let hi = (mark >> 8) as u8;
    let lo = (mark & 0xff) as u8;
    // <init>: iconst_1; putstatic mark; return
    let init_code = vec![0x04, 0xb3, hi, lo, 0xb1];
    // startApp: getstatic mark; iconst_1; iadd; putstatic mark; return
    let app_code = vec![0xb2, hi, lo, 0x04, 0x60, 0xb3, hi, lo, 0xb1];
    let bytes = build_class(
        &mut pool,
        "M",
        Some(ROOT),
        &[],
        &[f(0x0009, "mark", "I")],
        &[
            m(0x0001, "<init>", init_code),
            m(0x0001, "startApp", app_code),
        ],
    );
    let zip = zip_store(&[("M.class", bytes)]);
    let mut vm = mock_vm();
    vm.install_jar("t", &zip).unwrap();
    vm.start_midlet("M").unwrap();
    assert_eq!(vm.state(), VmState::Running);
    assert!(vm.is_active());
    // 第一次 poll 驱动线程到跑尽（返回 Ran），随后一次见全 Dead → Idle。
    let first = vm.poll(10_000);
    assert_eq!(first.ran_threads, 1);
    let second = vm.poll(10_000);
    assert_eq!(second.state, VmState::Dead);
    assert!(!vm.is_active());
    assert_eq!(vm.static_field("M", "mark", "I"), Some(Slot::Int(2)));
}

/// 入口类不存在 → ClassNotFound；Empty 态 start_midlet → InvalidState。
#[test]
fn start_midlet_guards_state_and_class() {
    let mut vm = mock_vm();
    assert!(matches!(
        vm.start_midlet("M"),
        Err(VmError::InvalidState(_))
    ));
    // 空 jar（合法 zip）→ JarReady，但入口类查不到。
    let zip = zip_store(&[]);
    vm.install_jar("t", &zip).unwrap();
    assert!(matches!(
        vm.start_midlet("NoSuch"),
        Err(VmError::ClassNotFound(_))
    ));
}

/// 状态机：poll 非运行为 no-op；shutdown 可重入且 is_active 归假。
#[test]
fn shutdown_is_reentrant_and_poll_is_noop_when_not_running() {
    let mut vm = mock_vm();
    assert_eq!(vm.poll(100).state, VmState::Empty);
    vm.shutdown();
    assert!(!vm.is_active());
    assert_eq!(vm.state(), VmState::Dead);
    // 可重入：再次调用不 panic、状态不变。
    vm.shutdown();
    assert_eq!(vm.state(), VmState::Dead);
    assert_eq!(vm.poll(100).state, VmState::Dead);
}

/// 事件入队后 poll 不 panic；startApp 死循环时预算耗尽仍 Running。
#[test]
fn dispatch_events_enqueue_and_poll_survives() {
    let mut pool = Pool::new();
    // startApp: goto 自身（0xa7 0000）——永不返回的协作式忙循环。
    let bytes = build_class(
        &mut pool,
        "M",
        Some(ROOT),
        &[],
        &[],
        &[m(0x0001, "startApp", vec![0xa7, 0x00, 0x00])],
    );
    let zip = zip_store(&[("M.class", bytes)]);
    let mut vm = mock_vm();
    vm.install_jar("t", &zip).unwrap();
    vm.start_midlet("M").unwrap();
    vm.dispatch_key(0x21, true);
    vm.dispatch_pointer(10, 20, 0);
    let result = vm.poll(50);
    assert_eq!(result.ran_threads, 1);
    assert_eq!(result.state, VmState::Running);
    assert!(vm.is_active());
    // 再驱动若干轮仍稳定。
    for _ in 0..3 {
        assert_eq!(vm.poll(50).state, VmState::Running);
    }
}
