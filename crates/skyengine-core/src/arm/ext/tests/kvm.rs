//! T023~T026：platEx(0x70003) KVM 引擎注册、魔数窗口拦截与 INIT/START
//! 装配测试（INIT 经真 fixture MRP 驱动完整 Vm 装配链）。
//! T027/T028：宿主接线（present 帧桥/字体/RMS）与帧泵/输入路由。
//!
//! SC-003 核对结论（T049，2026-10-02）——0x70003 未激活路径与历史一致：
//! - 注册拒绝面：`plat_ex_kvm_registration_rejects_null_pointers`（空指针
//!   → MR_FAILED(1)，不落 guest 终止分支）、
//!   `plat_ex_kvm_registration_ignores_nonzero_input_len`（r2≠0 → platEx
//!   catch-all MR_IGNORE(1)，output 槽/output_len 不被写、KVM 不注册）。
//! - 注册成功但永不 INIT：`kvm_registered_but_never_init_leaves_mr_flow_
//!   intact`（本任务新增显式用例——无关 platEx 命令行为与注册前逐位
//!   一致，KVM 停留 Uninitialized、观测面零事件）。
//! - INIT 顺序防线：`kvm_init_without_registration_crashes`（未注册 INIT
//!   → 引擎自身 Crashed，MR 宿主不殃及）、`kvm_start_before_init_fails`。
//! - 仅运行态分流：`kvm_key_routing_diverts_to_engine_when_running` /
//!   `kvm_pointer_routing_diverts_to_engine_when_running`（非运行态输入
//!   走原 MR 通道）；帧泵 `kvm_pump_paints_engine_screen_from_game`
//!   （kvm_frame_pump 以 kvm.is_running() 为闸，未激活即 no-op）。
//! - 无 KVM 基线：ext 的 services/state/lifecycle/abi 测试套件全程零
//!   KVM 注册，构成 MR 流程历史行为回归面（core --lib 433→434 全绿）。

use super::*;
use super::super::kvm::{KvmState, KVM_INIT_MAGIC, KVM_START_MAGIC, KVM_VTABLE};
use skyengine_kvm::host::{FontHandle, HostContext as _};

/// gjxjwql fixture 的宿主路径（包内 mainclass = com.fs.arpg.GameMidlet，
/// jc.jar 89 个类条目）。单测以 mr 层注入方式传给 set_kvm_package_path，
/// input 记录内联的是与之一致的相对包名（照真实 guest 链路）。
fn kvm_fixture_path() -> String {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../test/fixtures/395021_1002_320480_gjxjwql.mrp")
        .to_string_lossy()
        .into_owned()
}

/// gjxjwql fixture 的文件名（input 记录内联包名用）。
fn kvm_fixture_name() -> &'static str {
    "395021_1002_320480_gjxjwql.mrp"
}

/// talkcat fixture 的宿主路径（无 mainclass/jc.jar 条目，坏 jar 用）。
fn talkcat_fixture_path() -> String {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../test/fixtures/talkcat.mrp")
        .to_string_lossy()
        .into_owned()
}

/// 构造一个最小 ExtRuntime（240x320 屏，照 lifecycle 用例的装配方式）。
fn new_runtime() -> ExtRuntime {
    ExtRuntime::new(240, 320, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap()
}

/// 在 guest 内存装配 platEx(0x70003) 的 input 记录（T029 E2E 实测布局：
/// +0x00 平台表指针、+0x04 蹦床表指针、+0x08 起 0x40 字节内联包名 C
/// 字符串、+0x48 起内联 jar 名——均为就地内联，不是二级指针），返回
/// input 记录地址。
fn stage_kvm_input(runtime: &mut ExtRuntime, package_path: &str, jar_name: &str) -> u32 {
    let mut name = package_path.as_bytes().to_vec();
    name.push(0);
    assert!(name.len() <= 0x40, "内联包名槽 0x40 字节");
    let mut jar = jar_name.as_bytes().to_vec();
    jar.push(0);
    assert!(jar.len() <= 0x20, "内联 jar 名槽 0x20 字节读取上限");
    let input = runtime.allocate(0x48 + 0x20, 4).unwrap();
    runtime.memory.write_u32(input, 0x0100_0000).unwrap();
    runtime
        .memory
        .write_u32(input.checked_add(4).unwrap(), 0x0100_0004)
        .unwrap();
    runtime
        .memory
        .write(GuestAddr(input.0 + 8), &name)
        .unwrap();
    runtime
        .memory
        .write(GuestAddr(input.0 + 0x48), &jar)
        .unwrap();
    input.0
}

/// 以 guest sub80B20 的 platEx ABI 现场调用 slot 38 的 0x70003 命令：
/// r0=0x70003、r1=input、r2=0、r3=output、[r13]=output_len 指针。
/// 返回 (r0, output 槽值, output_len 值)。
fn register_kvm(runtime: &mut ExtRuntime, input: u32, output: u32) -> (u32, u32, u32) {
    let output_len = runtime.allocate(4, 4).unwrap();
    let stack = runtime.allocate(4, 4).unwrap();
    runtime.memory.write_u32(stack, output_len.0).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 0x0007_0003);
    cpu.set_register(1, input);
    cpu.set_register(2, 0);
    cpu.set_register(3, output);
    cpu.set_register(13, stack.0);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();
    (
        cpu.register(0),
        if output == 0 {
            0
        } else {
            runtime.memory.read_u32(GuestAddr(output)).unwrap()
        },
        runtime.memory.read_u32(output_len).unwrap(),
    )
}

/// 完整注册流：注入当前 MRP 宿主路径（照 mr/host.rs）、装配内联相对包
/// 名（fixture 包 + jc.jar）并 platEx 注册。断言注册成功，返回可 INIT
/// 的 runtime。
fn runtime_with_kvm_fixture() -> ExtRuntime {
    let mut runtime = new_runtime();
    runtime.set_kvm_package_path(std::path::PathBuf::from(kvm_fixture_path()));
    let input = stage_kvm_input(&mut runtime, kvm_fixture_name(), "jc.jar");
    let output = runtime.allocate(4, 4).unwrap();
    let (result, vtable, len) = register_kvm(&mut runtime, input, output.0);
    assert_eq!((result, vtable, len), (0, KVM_VTABLE, 8));
    runtime
}

/// BLX 魔数地址的现场拦截（INIT 成功路径断言见各用例）。
fn call_magic(runtime: &mut ExtRuntime, magic: u32) -> Option<i32> {
    let cpu = ArmCpu::new();
    runtime
        .kvm
        .dispatch_magic(magic, &cpu, &runtime.memory, (240, 320))
}

#[test]
fn plat_ex_kvm_registration_writes_magic_vtable() {
    let mut runtime = new_runtime();
    let output = runtime.allocate(4, 4).unwrap();
    let (result, vtable, len) = register_kvm(&mut runtime, 0x0100_0000, output.0);
    // 引擎成功路径：返回 0，output 槽 = vtable 地址，output_len = 8。
    assert_eq!(result, 0);
    assert_eq!(vtable, KVM_VTABLE);
    assert_eq!(len, 8);
    // guest sub80D04 读 vtable 两槽，得到 KVM_INIT/KVM_START 魔数地址。
    let init = runtime.memory.read_u32(GuestAddr(vtable)).unwrap();
    let start = runtime.memory.read_u32(GuestAddr(vtable + 4)).unwrap();
    assert_eq!(init, KVM_INIT_MAGIC);
    assert_eq!(start, KVM_START_MAGIC);
}

#[test]
fn plat_ex_kvm_registration_rejects_null_pointers() {
    let mut runtime = new_runtime();
    // output 为空：按 MR_FAILED(1) 拒绝，不得写入、不得崩溃。
    let output_len = runtime.allocate(4, 4).unwrap();
    runtime.memory.write_u32(output_len, 0).unwrap();
    let (result, _, len) = register_kvm(&mut runtime, 0x0100_0000, 0);
    assert_eq!(result, 1);
    assert_eq!(len, 0);
    // input 为空但 output 有效：能力查询形态（v2060 应用列表探测 jsky
    // 支持）——回写 vtable + MR_SUCCESS(0)，且不缓存注册（后续 INIT
    // 仍要求带 input 的注册先行）。
    let output = runtime.allocate(4, 4).unwrap();
    let (result, vtable, len) = register_kvm(&mut runtime, 0, output.0);
    assert_eq!(result, 0);
    assert_eq!(vtable, KVM_VTABLE);
    assert_eq!(len, 8);
}

#[test]
fn plat_ex_kvm_registration_ignores_nonzero_input_len() {
    // D5 契约（T029 查漏）：guest sub80B20 固定以 input_len=0（r2）调用
    // 0x70003。r2 非 0 不是已识别的调用形态，guard 不命中专用分支，落入
    // platEx catch-all：MR_IGNORE(1) 拒绝、output 槽与 output_len 单元均
    // 不被写入、KVM 不注册——guest 走自身 fallback 分支而非引擎崩溃。
    let mut runtime = new_runtime();
    runtime.set_kvm_package_path(std::path::PathBuf::from(kvm_fixture_path()));
    let input = stage_kvm_input(&mut runtime, kvm_fixture_name(), "jc.jar");
    let output = runtime.allocate(4, 4).unwrap();
    runtime.memory.write_u32(output, 0).unwrap();
    let output_len = runtime.allocate(4, 4).unwrap();
    runtime.memory.write_u32(output_len, 0).unwrap();
    let stack = runtime.allocate(4, 4).unwrap();
    runtime.memory.write_u32(stack, output_len.0).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 0x0007_0003);
    cpu.set_register(1, input);
    // 与 register_kvm 唯一差异：input_len（r2）= 8 而非 0。
    cpu.set_register(2, 8);
    cpu.set_register(3, output.0);
    cpu.set_register(13, stack.0);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();
    // 拒绝值 1（MR_IGNORE），output 槽/output_len 保持原样。
    assert_eq!(cpu.register(0), 1);
    assert_eq!(runtime.memory.read_u32(output).unwrap(), 0);
    assert_eq!(runtime.memory.read_u32(output_len).unwrap(), 0);
    // 未注册：后续 BLX INIT 按未注册失败进 Crashed，引擎不崩。
    assert_eq!(call_magic(&mut runtime, KVM_INIT_MAGIC), Some(-1));
    assert_eq!(runtime.kvm.state(), KvmState::Crashed);
    assert_eq!(runtime.kvm.jar_class_count(), None);
}

/// SC-003 显式用例（T049）：platEx(0x70003) 以真实调用形态（input_len=0）
/// 注册成功、但 guest 从不 BLX KVM_INIT——KVM 停留 Idle（pending_input 已
/// 缓存、vm 未建）。此间 MR 侧 platEx 行为必须与注册前的历史逐位一致。
fn dispatch_unrelated_plat_ex(runtime: &mut ExtRuntime) -> (u32, u32) {
    // slot 38 的无关命令（0x0007_00FF）：走 platEx catch-all 历史分支。
    let scratch = runtime.allocate(4, 4).unwrap();
    runtime.memory.write_u32(scratch, 0).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 0x0007_00FF);
    cpu.set_register(1, 0);
    cpu.set_register(2, 0);
    cpu.set_register(3, scratch.0);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();
    (cpu.register(0), runtime.memory.read_u32(scratch).unwrap())
}

#[test]
fn kvm_registered_but_never_init_leaves_mr_flow_intact() {
    // 注册前基线：无关命令的历史行为。
    let mut runtime = new_runtime();
    let baseline = dispatch_unrelated_plat_ex(&mut runtime);
    // 真实调用形态（r2=0）注册成功，随后永不 INIT。
    let input = stage_kvm_input(&mut runtime, kvm_fixture_name(), "jc.jar");
    let output = runtime.allocate(4, 4).unwrap();
    let (result, vtable, len) = register_kvm(&mut runtime, input, output.0);
    assert_eq!((result, vtable, len), (0, KVM_VTABLE, 8));
    assert_eq!(runtime.kvm.state(), KvmState::Uninitialized);
    assert_eq!(runtime.kvm.jar_class_count(), None);
    // MR 流程照旧：同一无关命令行为与注册前逐位一致，KVM 状态零漂移。
    assert_eq!(dispatch_unrelated_plat_ex(&mut runtime), baseline);
    assert_eq!(runtime.kvm.state(), KvmState::Uninitialized);
    assert_eq!(runtime.kvm.jar_class_count(), None);
    assert_eq!(runtime.kvm.pending_event_count(), 0);
}

#[test]
fn kvm_init_without_registration_crashes() {
    let mut runtime = new_runtime();
    // 未 platEx 注册（无 pending_input）：INIT 失败进 Crashed，不崩溃。
    assert_eq!(call_magic(&mut runtime, KVM_INIT_MAGIC), Some(-1));
    assert_eq!(runtime.kvm.state(), KvmState::Crashed);
    assert!(!runtime.kvm.is_running());
    assert_eq!(runtime.kvm.jar_class_count(), None);
    // 非魔数地址不被拦截。
    assert_eq!(call_magic(&mut runtime, 0x1234_5678), None);
}

#[test]
fn kvm_start_before_init_fails() {
    let mut runtime = new_runtime();
    // 未初始化直接 BLX START：失败返回非 0，且不崩溃、状态不变。
    let result = call_magic(&mut runtime, KVM_START_MAGIC).unwrap();
    assert_ne!(result, 0);
    assert_eq!(runtime.kvm.state(), KvmState::Uninitialized);
    // 非零参数按非法拒绝（不迁移状态）。
    let mut cpu_bad = ArmCpu::new();
    cpu_bad.set_register(0, 1);
    assert_eq!(
        runtime
            .kvm
            .dispatch_magic(KVM_INIT_MAGIC, &cpu_bad, &runtime.memory, (240, 320)),
        Some(-1)
    );
    assert_eq!(runtime.kvm.state(), KvmState::Uninitialized);
}

#[test]
fn kvm_init_loads_jar_and_mainclass_from_fixture() {
    let mut runtime = runtime_with_kvm_fixture();
    assert_eq!(call_magic(&mut runtime, KVM_INIT_MAGIC), Some(0));
    assert_eq!(runtime.kvm.state(), KvmState::Initialized);
    // 包内 mainclass 文件内容 = 入口类名；jc.jar 索引到 89 个类条目。
    assert_eq!(runtime.kvm.mainclass(), Some("com.fs.arpg.GameMidlet"));
    assert_eq!(runtime.kvm.jar_class_count(), Some(89));
    // 已装配未启动：Vm 尚未运行。
    assert!(!runtime.kvm.is_running());
}

#[test]
fn kvm_init_resolves_inline_relative_name_via_current_mrp() {
    // T029 E2E 实测形态：guest 内联的是相对包名（"gjxjwql.mrp"），mr
    // 层注入当前 MRP 宿主路径后应解析复用当前包——KVM 跑的就是当前
    // MRP 内嵌的 jc.jar，而非按引擎 cwd 相对解释（必然落空）。
    let mut runtime = new_runtime();
    runtime.set_kvm_package_path(std::path::PathBuf::from(kvm_fixture_path()));
    let input = stage_kvm_input(&mut runtime, "395021_1002_320480_gjxjwql.mrp", "jc.jar");
    let output = runtime.allocate(4, 4).unwrap();
    register_kvm(&mut runtime, input, output.0);
    assert_eq!(call_magic(&mut runtime, KVM_INIT_MAGIC), Some(0));
    assert_eq!(runtime.kvm.state(), KvmState::Initialized);
    assert_eq!(runtime.kvm.jar_class_count(), Some(89));
}

#[test]
#[ignore = "E2E 冒烟占位：宿主需先带 SDL2 RUSTFLAGS 构建 release 二进制，二进制存在才跑（--ignored 显式运行）"]
fn kvm_e2e_smoke_spawns_release_binary_when_present() {
    // T029 E2E 全链验收的 Rust 侧占位：正式验收在 test/e2e/kvm/
    // （vitest 管道驱动，DRAW_COUNT/SCREEN 拿像素与 [kvm] trace 断言），
    // 此处只验证 release 二进制存在时可执行（帮助文本可打出），避免
    // 资产悄悄失效；二进制不存在时跳过（不失败）。
    let bin = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/release/skyengine.exe");
    if !bin.exists() {
        eprintln!("skip: {} 不存在（未构建 release）", bin.display());
        return;
    }
    let output = std::process::Command::new(&bin).arg("--help").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("SkyEngine"), "帮助文本异常：{stdout}");
}

#[test]
fn kvm_start_runs_midlet() {
    let mut runtime = runtime_with_kvm_fixture();
    assert_eq!(call_magic(&mut runtime, KVM_INIT_MAGIC), Some(0));
    assert_eq!(call_magic(&mut runtime, KVM_START_MAGIC), Some(0));
    assert_eq!(runtime.kvm.state(), KvmState::Started);
    // start_midlet 链（链接入口类 → 构造 → startApp）已执行，Vm 存活。
    assert!(runtime.kvm.is_running());
}

#[test]
fn kvm_init_with_missing_package_crashes_without_taking_the_engine_down() {
    let mut runtime = new_runtime();
    // 缺包：路径不存在 → INIT 失败进 Crashed，引擎继续可用。
    let input = stage_kvm_input(&mut runtime, "Z:/no/such/package.mrp", "jc.jar");
    let output = runtime.allocate(4, 4).unwrap();
    register_kvm(&mut runtime, input, output.0);
    assert_eq!(call_magic(&mut runtime, KVM_INIT_MAGIC), Some(-1));
    assert_eq!(runtime.kvm.state(), KvmState::Crashed);
    assert!(!runtime.kvm.is_running());
    assert_eq!(runtime.kvm.jar_class_count(), None);

    // 坏 jar：真 MRP 但无 mainclass/jc.jar 条目（talkcat 只有资源图）。
    runtime.set_kvm_package_path(std::path::PathBuf::from(talkcat_fixture_path()));
    let input = stage_kvm_input(&mut runtime, "talkcat.mrp", "jc.jar");
    let output = runtime.allocate(4, 4).unwrap();
    register_kvm(&mut runtime, input, output.0);
    assert_eq!(call_magic(&mut runtime, KVM_INIT_MAGIC), Some(-1));
    assert_eq!(runtime.kvm.state(), KvmState::Crashed);
    assert_eq!(runtime.kvm.jar_class_count(), None);

    // Crashed 可恢复：同一 runtime 上重新注册 fixture 并 INIT 成功。
    runtime.set_kvm_package_path(std::path::PathBuf::from(kvm_fixture_path()));
    let input = stage_kvm_input(&mut runtime, kvm_fixture_name(), "jc.jar");
    let output = runtime.allocate(4, 4).unwrap();
    register_kvm(&mut runtime, input, output.0);
    assert_eq!(call_magic(&mut runtime, KVM_INIT_MAGIC), Some(0));
    assert_eq!(runtime.kvm.state(), KvmState::Initialized);
    assert_eq!(runtime.kvm.jar_class_count(), Some(89));
}

#[test]
fn kvm_reinit_after_start_rebuilds_a_running_vm() {
    let mut runtime = runtime_with_kvm_fixture();
    assert_eq!(call_magic(&mut runtime, KVM_INIT_MAGIC), Some(0));
    assert_eq!(call_magic(&mut runtime, KVM_START_MAGIC), Some(0));
    assert!(runtime.kvm.is_running());
    // 重入 INIT：旧 Vm 显式 shutdown 后重建，回到 Initialized（未启动）。
    assert_eq!(call_magic(&mut runtime, KVM_INIT_MAGIC), Some(0));
    assert_eq!(runtime.kvm.state(), KvmState::Initialized);
    assert_eq!(runtime.kvm.jar_class_count(), Some(89));
    assert!(!runtime.kvm.is_running());
    // 新 Vm 可再次 START 并跑起来。
    assert_eq!(call_magic(&mut runtime, KVM_START_MAGIC), Some(0));
    assert!(runtime.kvm.is_running());
}

#[test]
fn guest_blx_to_magic_address_is_intercepted() {
    // 端到端：platEx 注册后，guest 代码 BLX 到 KVM_INIT_MAGIC，执行循环
    // 在取指前拦截，状态迁移后结果写 r0、PC 恢复 LR，helper 正常返回。
    let mut image = b"MRPGCMAP".to_vec();
    // 无栈方案（call_active_helper 会预构造调用帧，不能 push/pop）：
    image.extend_from_slice(&0xe3a0_0000_u32.to_le_bytes()); // mov r0, #0（INIT 参数）
    image.extend_from_slice(&0xe3a0_140e_u32.to_le_bytes()); // mov r1, #0x0E000000 (ror #8)
    image.extend_from_slice(&0xe12f_ff31_u32.to_le_bytes()); // blx r1 (LR=+8)
    image.extend_from_slice(&0xe51f_f004_u32.to_le_bytes()); // ldr pc, [pc, #-4] → sentinel
    image.extend_from_slice(&0xffff_ff00_u32.to_le_bytes()); // RETURN_SENTINEL 字面量
    let mut runtime = runtime_with_kvm_fixture();
    runtime
        .load_and_call_entry(&image, 0, &mut StubServices)
        .unwrap();
    let helper = runtime.modules[0].base.checked_add(8).unwrap();
    // 注册 guest helper（照 lifecycle 用例：slot 25 传地址与代号 20）。
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, helper.0);
    cpu.set_register(1, 20);
    runtime
        .dispatch(25, 0, &mut cpu, &mut StubServices)
        .unwrap();
    let (result, _) = runtime
        .call_active_helper_raw(6, [1, 2_000], &mut StubServices)
        .unwrap();
    // BLX 现场完成整个 INIT：真包装配成功（mainclass/jar 类数就位）。
    assert_eq!(result, 0);
    assert_eq!(runtime.kvm.state(), KvmState::Initialized);
    assert_eq!(runtime.kvm.mainclass(), Some("com.fs.arpg.GameMidlet"));
    assert_eq!(runtime.kvm.jar_class_count(), Some(89));
}

// ---------------------------------------------------------------------------
// T028：US1 实质验收——INIT→START→帧泵驱动后引擎屏幕捕获到游戏画面。
// ---------------------------------------------------------------------------

// chunks_exact_to_as_chunks：as_chunks 在当前工具链未稳定，豁免此测试的像素对遍历。
#[test]
#[allow(clippy::chunks_exact_to_as_chunks)]
fn kvm_pump_paints_engine_screen_from_game() {
    let mut runtime = runtime_with_kvm_fixture();
    assert_eq!(call_magic(&mut runtime, KVM_INIT_MAGIC), Some(0));
    assert_eq!(call_magic(&mut runtime, KVM_START_MAGIC), Some(0));
    assert!(runtime.kvm.is_running());
    // force 泵 ~300 帧（真实时钟：每帧留 2ms 墙钟让 sleep/Timer 类阻塞
    // 自然推进；50k 指令/帧预算），足够游戏跑完 startApp → repaint →
    // paint → present 链并刷进 guest 屏幕缓冲。
    let expected = 240_usize * 320 * 2;
    for _ in 0..300 {
        runtime.kvm_frame_pump(&mut StubServices, true);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let screen = runtime.memory.read(runtime.screen_base, expected).unwrap();
    // RGB565（LE u16/像素）非黑像素统计（黑 = 0x0000）。
    let lit = screen
        .chunks_exact(2)
        .filter(|pair| pair[0] != 0 || pair[1] != 0)
        .count();
    eprintln!("[kvm-us1] 非黑像素 {lit}/76800");
    assert!(
        lit > 1000,
        "KVM 帧泵后引擎屏幕应捕获到非黑游戏画面（非黑像素 {lit} <= 1000）"
    );
}

// ---------------------------------------------------------------------------
// T028：键/指针路由——KVM 激活时改投 KVM，不进 guest 分发。
// ---------------------------------------------------------------------------

#[test]
fn kvm_key_routing_diverts_to_engine_when_running() {
    let mut runtime = runtime_with_kvm_fixture();
    assert_eq!(call_magic(&mut runtime, KVM_INIT_MAGIC), Some(0));
    assert_eq!(call_magic(&mut runtime, KVM_START_MAGIC), Some(0));
    // 方向键（MR 12=UP → 引擎键 0x81）入队 KVM 事件，且不再产生 guest
    // 三元组（None = 宿主已消费）。
    let before = runtime.kvm.pending_event_count();
    assert_eq!(
        runtime.route_key_event(12, true, &mut StubServices).unwrap(),
        None
    );
    assert_eq!(runtime.kvm.pending_event_count(), before + 1);
    // 数字键 0（MR 0 → 引擎键 '0'=48）同样入队。
    assert_eq!(
        runtime.route_key_event(0, true, &mut StubServices).unwrap(),
        None
    );
    assert_eq!(runtime.kvm.pending_event_count(), before + 2);
    // 抬起事件成对入队（keyReleased）。
    assert_eq!(
        runtime.route_key_event(12, false, &mut StubServices).unwrap(),
        None
    );
    assert_eq!(runtime.kvm.pending_event_count(), before + 3);

    // KVM 未激活（新 runtime 未注册未 INIT）：原路径零回归——无平台 UI
    // 时返回直通三元组 (按下=0, code, 0)。
    let mut plain = new_runtime();
    assert_eq!(
        plain.route_key_event(12, true, &mut StubServices).unwrap(),
        Some((0, 12, 0))
    );
    assert_eq!(plain.kvm.pending_event_count(), 0);
}

#[test]
fn kvm_pointer_routing_diverts_to_engine_when_running() {
    let mut runtime = runtime_with_kvm_fixture();
    assert_eq!(call_magic(&mut runtime, KVM_INIT_MAGIC), Some(0));
    assert_eq!(call_magic(&mut runtime, KVM_START_MAGIC), Some(0));
    let before = runtime.kvm.pending_event_count();
    // 指针按下（kind=0）/抬起（kind=1）成对入队，不进 guest 分发。
    assert_eq!(
        runtime
            .route_pointer_event(30, 40, true, &mut StubServices)
            .unwrap(),
        None
    );
    assert_eq!(
        runtime
            .route_pointer_event(30, 40, false, &mut StubServices)
            .unwrap(),
        None
    );
    assert_eq!(runtime.kvm.pending_event_count(), before + 2);
    // 未激活：原路径直通 (按下=2, x, y)。
    let mut plain = new_runtime();
    assert_eq!(
        plain
            .route_pointer_event(1, 2, true, &mut StubServices)
            .unwrap(),
        Some((2, 1, 2))
    );
}

// ---------------------------------------------------------------------------
// T027：RMS 四件套（直接对 EngineHostContext 单测文件行为）。
// ---------------------------------------------------------------------------

#[test]
fn kvm_rms_roundtrip_through_engine_host() {
    let dir = std::env::temp_dir().join(format!("skyengine-kvm-rms-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // RMS 目录取包路径同级的 rms/（包文件本身不存在也没关系——RMS 落盘
    // 只依赖目录推导）。
    let mut host = super::super::kvm::EngineHostContext::new(
        (240, 320),
        dir.join("pkg.mrp"),
        false,
    );
    // 空态：load None、list 空、delete false。
    assert_eq!(host.rms_load("scores"), None);
    assert!(host.rms_list().is_empty());
    assert!(!host.rms_delete("scores"));
    // save → load/list roundtrip；覆盖写生效。
    host.rms_save("scores", b"hello");
    assert_eq!(host.rms_load("scores"), Some(b"hello".to_vec()));
    assert_eq!(host.rms_list(), vec!["scores".to_string()]);
    host.rms_save("scores", b"world!");
    assert_eq!(host.rms_load("scores"), Some(b"world!".to_vec()));
    // 多店共存 + 删除。
    host.rms_save("settings", &[1, 2, 3]);
    assert_eq!(host.rms_list(), vec!["scores".to_string(), "settings".to_string()]);
    assert!(host.rms_delete("scores"));
    assert_eq!(host.rms_load("scores"), None);
    assert_eq!(host.rms_list(), vec!["settings".to_string()]);
    // 磁盘上确为 rms/<store>.rec。
    let rec = dir.join("rms").join("settings.rec");
    assert!(rec.is_file());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// T027：内建合成字体（三档句柄/度量/宽度/blit 与 clip）。
// ---------------------------------------------------------------------------

#[test]
fn kvm_synthetic_font_tiers_metrics_and_blit() {
    let mut host = super::super::kvm::EngineHostContext::new(
        (240, 320),
        PathBuf::from("nonexistent-pkg.mrp"),
        false,
    );
    // size 已由 kvm 侧归一化：0/1/2 → 句柄 1/2/3，行高 12/16/20。
    assert_eq!(host.font_query(0, 0, 0), FontHandle(1));
    assert_eq!(host.font_query(0, 0, 1), FontHandle(2));
    assert_eq!(host.font_query(0, 0, 2), FontHandle(3));
    assert_eq!(host.font_metrics(FontHandle(1)).height, 12);
    assert_eq!(host.font_metrics(FontHandle(2)).height, 16);
    assert_eq!(host.font_metrics(FontHandle(3)).height, 20);
    // 半角恒定 advance（小档 6px），CJK 全角翻倍。
    let ab: Vec<u16> = "AB".encode_utf16().collect();
    assert_eq!(host.text_width(FontHandle(1), &ab), 12);
    let cjk: Vec<u16> = "试".encode_utf16().collect();
    assert_eq!(host.text_width(FontHandle(1), &cjk), 12);

    // blit：64x32 目标缓冲，基线 (2, 12) 画 "A"（白色不透明）。
    let mut target = vec![0_i32; 64 * 32];
    let text: Vec<u16> = "A".encode_utf16().collect();
    host.render_text(
        FontHandle(1),
        &text,
        0x00FF_FFFF,
        2,
        12,
        &mut target,
        64,
        (0, 0, 64, 32),
    );
    let lit = target.iter().filter(|&&pixel| pixel != 0).count();
    assert!(lit > 0, "render_text 应把字形 blit 进目标缓冲");
    assert!(
        target.iter().all(|&pixel| pixel == 0 || pixel == -1),
        "字形像素应为不透明白（0xFFFFFFFF）"
    );

    // clip 全外：一个像素都不落。
    let mut clipped = vec![0_i32; 64 * 32];
    host.render_text(
        FontHandle(1),
        &text,
        0x00FF_FFFF,
        2,
        12,
        &mut clipped,
        64,
        (100, 100, 101, 101),
    );
    assert!(clipped.iter().all(|&pixel| pixel == 0), "clip 外不落像素");
}

// ---------------------------------------------------------------------------
// T045/T046：fatal 停摆错误画面与引擎存活（故障注入：手构坏 jar 的 MRP）。
// ---------------------------------------------------------------------------

/// 最小 class 文件手构（T045 坏 jar 夹具）：`T extends
/// javax/microedition/midlet/MIDlet`，唯一方法 startApp()V =
/// `NEW RuntimeException; DUP; INVOKESPECIAL <init>()V; ATHROW`
/// （启动即未捕获异常）。常量池手工编号（池索引先取号再构码）。
fn throwing_midlet_class() -> Vec<u8> {
    fn cp_utf8(out: &mut Vec<u8>, text: &str) {
        out.push(0x01);
        out.extend_from_slice(&(text.len() as u16).to_be_bytes());
        out.extend_from_slice(text.as_bytes());
    }
    fn cp_class(out: &mut Vec<u8>, name_index: u16) {
        out.push(0x07);
        out.extend_from_slice(&name_index.to_be_bytes());
    }
    fn cp_name_and_type(out: &mut Vec<u8>, name: u16, desc: u16) {
        out.push(0x0c);
        out.extend_from_slice(&name.to_be_bytes());
        out.extend_from_slice(&desc.to_be_bytes());
    }
    fn cp_methodref(out: &mut Vec<u8>, class: u16, nat: u16) {
        out.push(0x0a);
        out.extend_from_slice(&class.to_be_bytes());
        out.extend_from_slice(&nat.to_be_bytes());
    }
    // 池布局（count=13）：1 "T"；2 class T；3 MIDLET；4 class MIDLET；
    // 5 RTE；6 class RTE；7 "<init>"；8 "()V"；9 nat；10 methodref；
    // 11 "startApp"；12 "Code"。
    let mut cp = Vec::new();
    cp_utf8(&mut cp, "T");
    cp_class(&mut cp, 1);
    cp_utf8(&mut cp, "javax/microedition/midlet/MIDlet");
    cp_class(&mut cp, 3);
    cp_utf8(&mut cp, "java/lang/RuntimeException");
    cp_class(&mut cp, 5);
    cp_utf8(&mut cp, "<init>");
    cp_utf8(&mut cp, "()V");
    cp_name_and_type(&mut cp, 7, 8);
    cp_methodref(&mut cp, 6, 9);
    cp_utf8(&mut cp, "startApp");
    cp_utf8(&mut cp, "Code");
    // startApp 字节码：NEW #6; DUP; INVOKESPECIAL #10; ATHROW（8 字节）。
    let code: [u8; 8] = [0xbb, 0x00, 0x06, 0x59, 0xb7, 0x00, 0x0a, 0xbf];
    // Code 属性：max_stack=2 max_locals=1 code_len=8 异常表 0 属性 0。
    let mut attr = Vec::new();
    attr.extend_from_slice(&2u16.to_be_bytes());
    attr.extend_from_slice(&1u16.to_be_bytes());
    attr.extend_from_slice(&8u32.to_be_bytes());
    attr.extend_from_slice(&code);
    attr.extend_from_slice(&0u16.to_be_bytes());
    attr.extend_from_slice(&0u16.to_be_bytes());

    let mut out = Vec::new();
    out.extend_from_slice(&[0xca, 0xfe, 0xba, 0xbe]);
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&48u16.to_be_bytes()); // major（解释器支持域 45-48）
    out.extend_from_slice(&13u16.to_be_bytes());
    out.extend_from_slice(&cp);
    out.extend_from_slice(&0x0021u16.to_be_bytes()); // public super
    out.extend_from_slice(&2u16.to_be_bytes()); // this = T
    out.extend_from_slice(&4u16.to_be_bytes()); // super = MIDlet
    out.extend_from_slice(&0u16.to_be_bytes()); // interfaces
    out.extend_from_slice(&0u16.to_be_bytes()); // fields
    out.extend_from_slice(&1u16.to_be_bytes()); // methods
    out.extend_from_slice(&0x0001u16.to_be_bytes()); // public
    out.extend_from_slice(&11u16.to_be_bytes()); // name = startApp
    out.extend_from_slice(&8u16.to_be_bytes()); // desc = ()V
    out.extend_from_slice(&1u16.to_be_bytes()); // 属性数
    out.extend_from_slice(&12u16.to_be_bytes()); // 属性名 = Code
    out.extend_from_slice(&(attr.len() as u32).to_be_bytes());
    out.extend_from_slice(&attr);
    out.extend_from_slice(&0u16.to_be_bytes()); // 类属性
    out
}

/// 最小 stored zip（kvm 侧 jar::unpack 只认 EOCD/中央目录/局部头三件套）。
fn stored_zip_core(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut locals = Vec::new();
    for (name, data) in entries {
        locals.push((name.to_string(), data.len() as u32, out.len() as u32));
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&[0u8; 12].as_slice()); // flags..crc
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
    }
    let cd_start = out.len() as u32;
    for (name, size, offset) in &locals {
        out.extend_from_slice(b"PK\x01\x02");
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&[0u8; 14].as_slice()); // verneed..crc
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&[0u8; 12].as_slice()); // extra..external attrs
        out.extend_from_slice(&offset.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
    }
    let cd_size = out.len() as u32 - cd_start;
    out.extend_from_slice(b"PK\x05\x06");
    out.extend_from_slice(&[0u8; 4].as_slice());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_start.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// 最小 MRP 打包（T045 坏包夹具，stored 条目——zip 以 PK 开头不触 gzip
/// 魔数判定）：0xd0 头 + 数据区 + 目录（u32 名长 + 名 + NUL + offset +
/// stored_len + reserved）。parse 契约：list_start..file_start+8 为目录
/// （file_start = 目录末尾-8）、declared_len = 全长。
fn build_mrp(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = vec![0_u8; 0xd0];
    out[0..4].copy_from_slice(b"MRPG");
    let mut directory = Vec::new();
    let mut offset = out.len() as u32;
    for (name, data) in entries {
        out.extend_from_slice(data);
        let name_bytes = name.as_bytes();
        directory.extend_from_slice(&((name_bytes.len() + 1) as u32).to_le_bytes());
        directory.extend_from_slice(name_bytes);
        directory.push(0); // 目录名 NUL 收尾（parse 侧剥除）
        directory.extend_from_slice(&offset.to_le_bytes());
        directory.extend_from_slice(&(data.len() as u32).to_le_bytes());
        directory.extend_from_slice(&0u32.to_le_bytes());
        offset += data.len() as u32;
    }
    let list_start = out.len() as u32;
    out.extend_from_slice(&directory);
    let file_start = list_start + directory.len() as u32 - 8;
    let declared_len = out.len() as u32;
    out[4..8].copy_from_slice(&file_start.to_le_bytes());
    out[8..12].copy_from_slice(&declared_len.to_le_bytes());
    out[12..16].copy_from_slice(&list_start.to_le_bytes());
    out
}

/// 故障注入全链路（T045/T046）：坏 jar（startApp 未捕获异常）→ INIT/
/// START 成功 → 帧泵驱动 → 主线程异常死尽 → Vm Dead + fatal → 引擎绘
/// 错误画面进帧桥（黑底白字）→ KvmRuntime Crashed；错误画面一次性防
/// 重；引擎存活——is_running false（键路由天然回 guest）且重入
/// 0x70003+INIT+START 换正常 fixture 后正常运行。
#[test]
fn kvm_fatal_midlet_paints_error_screen_and_engine_survives() {
    let dir = std::env::temp_dir().join(format!("skyengine-t045-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let jar = stored_zip_core(&[("T.class", &throwing_midlet_class())]);
    let mrp_path = dir.join("bad.mrp");
    std::fs::write(&mrp_path, build_mrp(&[("mainclass", b"T\n"), ("jc.jar", &jar)])).unwrap();

    let mut runtime = new_runtime();
    runtime.set_kvm_package_path(mrp_path.clone());
    let input = stage_kvm_input(&mut runtime, "bad.mrp", "jc.jar");
    let output = runtime.allocate(4, 4).unwrap();
    let (result, _, _) = register_kvm(&mut runtime, input, output.0);
    assert_eq!(result, 0);
    assert_eq!(call_magic(&mut runtime, KVM_INIT_MAGIC), Some(0));
    assert_eq!(call_magic(&mut runtime, KVM_START_MAGIC), Some(0));
    assert!(runtime.kvm.is_running());

    // 泵驱动：startApp 抛 RuntimeException → 线程死尽 → Dead+fatal →
    // 错误画面帧进桥并经同一泵上屏（kvm_present_frame 消费帧桥）+
    // 状态 Crashed。
    for _ in 0..10 {
        runtime.kvm_frame_pump(&mut StubServices, true);
    }
    assert_eq!(
        runtime.kvm.state(),
        KvmState::Crashed,
        "fatal 停摆转 Crashed"
    );
    assert!(
        !runtime.kvm.is_running(),
        "停摆后 is_running false（键路由回 guest）"
    );
    // 错误画面已刷进 guest 屏幕缓冲：黑底白字（非黑像素显著）。
    let expected = 240_usize * 320 * 2;
    let screen = runtime
        .memory
        .read(runtime.screen_base, expected)
        .unwrap();
    let lit = screen
        .chunks_exact(2)
        .filter(|pair| pair[0] != 0 || pair[1] != 0)
        .count();
    assert!(lit > 100, "错误画面黑底白字应上屏（非黑像素 {lit}）");
    // 一次性防重：后续泵不再产生新错误画面帧（帧桥无 fresh）。
    runtime.kvm_frame_pump(&mut StubServices, true);
    assert!(
        runtime.kvm.take_fresh_frame().is_none(),
        "错误画面只绘一次"
    );

    // 引擎存活（重入序列）：换正常 fixture 重新 0x70003+INIT+START →
    // 正常运行——坏启动不终结引擎进程。
    runtime.set_kvm_package_path(std::path::PathBuf::from(kvm_fixture_path()));
    let input2 = stage_kvm_input(&mut runtime, kvm_fixture_name(), "jc.jar");
    let output2 = runtime.allocate(4, 4).unwrap();
    let (result2, _, _) = register_kvm(&mut runtime, input2, output2.0);
    assert_eq!(result2, 0);
    assert_eq!(
        call_magic(&mut runtime, KVM_INIT_MAGIC),
        Some(0),
        "Crashed 后 INIT 可重建"
    );
    assert_eq!(call_magic(&mut runtime, KVM_START_MAGIC), Some(0));
    assert!(runtime.kvm.is_running(), "重入后新 MIDlet 正常运行");
}
