# 任务：replace-cpu-with-arm7tdmi

## 1. Vendor 引入（行为零变化）

- [x] 1.1 复制 rustboyadvance-ng/arm7tdmi 源树到 `crates/arm7tdmi/`，创建独立 crate 与 workspace 成员注册；记录来源 commit 至 VENDOR.md
- [x] 1.2 移除 `src/gdb/` 目录与 lib.rs 的 gdb re-export；从 Cargo.toml 删除 gdbstub、gdbstub_arch、ansi_term、colored、cfg_if
- [x] 1.3 纯文本化 Debug/Display 格式化实现（cpu.rs、psr.rs），保持输出信息不丢失
- [x] 1.4 将 disass.rs / arm/disass.rs / thumb/disass.rs 及 InstructionDecoder 迁入非默认 feature "disasm"；byteorder、相关 num 引用随 feature 门控
- [x] 1.5 总线所有权改为 `pub bus: I`：调整 new()/set_memory_interface() 签名并清理 Shared<UnsafeCell> 遗留（保留 utils 无需进入 vendor）
- [x] 1.6 SWI 路径接入默认方法钩子 `MemoryInterface::software_interrupt(comment) -> bool`（exec_arm_swi / exec_thumb_swi 各一行询问），所有修改以 `[skyengine] BEGIN/END` 标记包夹并登记 VENDOR.md
- [ ] 1.7 验收：`cargo build --workspace` 与既有全量测试通过（新 crate 尚未被 skyengine-core 引用）；`cargo clippy --workspace --all-targets -- -D warnings` 干净

## 2. 差分冒烟基线

- [x] 2.1 设计差分夹具：同一指令流与镜像内存分别喂旧 ArmCpu 与新 Arm7tdmiCore（SimpleMemory 变体），比对寄存器/CPSR 终态与访存序列
- [x] 2.2 构造覆盖核心类的指令流（数据处理含移位进位边界、乘累加/长乘、LDR/STR 含回旋对齐、LDM/STM 用户态变体、BX/BLX 切换、条件码全表）
- [x] 2.3 运行差分并对分歧逐条裁决：修正预期或确认语义等价性；结论记入 VENDOR.md 附注
- [ ] 2.4 决定差分指令流补充来源（随机生成 vs mrpanalysize/*.disasm 真实热点提取）并实施第二批次

## 3. GuestBus / GuestCpu 门面

- [ ] 3.1 新增 `arm/bus.rs`：GuestBus 实现 MemoryInterface——load/store 经 GuestMemory 权限检查、未映射/越权访问记 pending fault 并在 step 出口转为 Err；陷阱窗口取指返回 0xE12FFF1E 并记录 pending_slot
- [ ] 3.2 实现软件中断钩子：comment==0x123456 时置 semihosting_exit_reason=r0==0x18 的 r1 并返回 true，其余返回 false 走原生异常
- [ ] 3.3 新增 facade：镜像旧 ArmCpu API 形状（register/set_register/pc/set_pc/cpsr/is_thumb/take_semihosting_exit_reason），内部驱动 Arm7tdmiCore
- [ ] 3.4 为 GuestMemory 补充总线所需的只读内省能力（find_region 等），不改变现有写路径权限语义

## 4. 执行循环切换

- [ ] 4.1 迁移 run_guest_execution 主循环至门面：RETURN_SENTINEL 终止、try_dispatch_legacy_external_action 边界、错误上下文格式（module/slot/LR/r0-r3/sp）逐项保持
- [ ] 4.2 ext.rs 全部调用点切换到门面 API；trap_slot(pc) 直接检查改为读取 pending_slot；确认 INSTRUCTION_BUDGET 计数点唯一且一致
- [ ] 4.3 SKYENGINE_TRACE_ARM 轨迹在新内核下恢复可用（初期可经 "disasm" feature + 条件编译或简化为 PC/指令字十六进制行）

## 5. 退役自研解释器

- [ ] 5.1 将 arm/cpu/tests.rs 中仍有价值的行为断言迁移为新内核单元测试或合入差分夹具后，删除 arm/cpu.rs 与 arm/cpu/tests.rs 及其 mod 声明
- [ ] 5.2 清理残余引用（imports、文档注释中的旧类型名），`cargo build/clippy/test --workspace` 全绿

## 6. 规格验收与回归

- [ ] 6.1 对照 specs/arm-cpu 逐条需求编写/关联验收测试：状态切换与报错、条件副作用、未映射/越权访问、陷阱弹床往返、未知槽位拒绝、semihosting 双态一致、预算耗尽、TRACE 默认静默
- [ ] 6.2 以 dsm_gm.mrp 样本运行 headless 与 E2E 回归，对比替换前帧缓冲输出的确定性一致性
- [ ] 6.3 建立 headless 吞吐基准（每秒执行指令数）并记录新旧内核数据到 VENDOR.md；只记录不优化
- [ ] 6.4 收尾：VENDOR.md 最终校对（commit hash、修改清单完整）、README"当前实现范围"措辞更新（EXT 执行的架构说明）
