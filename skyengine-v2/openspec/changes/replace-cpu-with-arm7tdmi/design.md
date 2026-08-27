# 设计：使用 vendored arm7tdmi 内核替换自研 ARM 解释器

## 上下文

skyengine-v2 通过自研解释器 `ArmCpu`（`crates/skyengine-core/src/arm/cpu.rs`）执行
MRP 原生扩展 chunk。该解释器采用"单步取指-译码-执行"结构，宿主主循环
（`ext.rs::run_guest_execution`）在每次 step 后检查 PC 是否落入陷阱窗口
（`0xff00_0000 + slot*4`）或 RETURN_SENTINEL，从而派发 EXT 服务。

候选替换内核 rustboyadvance-ng/arm7tdmi（MIT ©2019 Michel Heily）的关键事实：

- 指令执行走 `build.rs` 生成的 handler 函数指针跳转表（1024 Thumb + 4096 ARM 项），
  运行时零译码开销。
- CPU 核心对总线的唯一假设是 `trait MemoryInterface { load/store_8/16/32, idle_cycle }`；
  总线经 `Shared<I>`（= `Rc<UnsafeCell<T>>`）持有——这是 GBA 多设备共享总线的遗产，
  而 skyengine 的 guest 内存归 `ExtRuntime` 独占。
- gdb 存根（`src/gdb/`, 无条件依赖 `gdbstub*`）、彩色调试输出
  （`ansi_term`/`colored`）、反汇编器（`byteorder`）均不在执行热路径上。
- 调研阶段两个仓库均已克隆到本地：上游 skyengine-v2 与 rustboyadvance-ng。

## 目标 / 非目标

**目标：**

1. 客户机代码获得完整的 ARMv4T 语义（ARM 状态与 Thumb 状态），覆盖真实 MRP 扩展的指令长尾。
2. 宿主集成收敛到单一接触面；`arm/ext/` 的派发逻辑、ABI 校验与行为测试不动。
3. 保持既有可观测性保障：INSTRUCTION_BUDGET 预算、SKYENGINE_TRACE_ARM、
   错误上下文格式（module/slot/LR/r0-r3/sp）、确定性无头输出。
4. vendor 修改全部带机器可 grep 标记（`[skyengine] BEGIN/END`），保持未来同步上游的可能。

**非目标：**

- 不改变 EXT ABI、dispatch 表、容器格式、MR 字节码 VM。
- 不引入 JIT/缓存解释器等性能重写；性能只做基准对比记录，不做优化承诺。
- 不实现 ARMv5+ 指令（CLZ/BLX 等）；遇到即报错（两边现状一致）。
- 不做 GBA 外设总线语义（定时器/DMA/中断控制器）；guest 是裸计算二进制。
- 不保留 gdb 远程调试功能（gdb/ 目录删除；未来如需可用 SKYENGINE_TRACE_ARM 替代起步）。

## 决策

### D1 — 采用方案 B：vendor 进工作区并裁剪

直接依赖（git dep）会把 `gdbstub*` 等无关依赖拖进依赖图，且无法做 SWI 钩子这类
侵入性小改。完全重写自研（方案 C）成本已被指令长尾证伪。vendor 折中：
独立 crate `crates/arm7tdmi`、保留上游文件布局以利 diff 同步、附 VENDOR.md
记录来源 commit 与逐文件修改清单。

替代方案：A 直接依赖（被依赖洁癖否决）、C 继续自研（被正确性风险否决）。

### D2 — 总线所有权改为 Owned，废弃 Shared<UnsafeCell>

`Shared<T>` 存在的唯一理由是多设备共享总线；本场景内存独占。fork 后将核心字段改为
`pub bus: I`（约 5 行改动：new()/set_memory_interface() 两处签名），顺带消除
`UnsafeCell` 别名隐患。

替代方案：复制一个 5 行的迷你 Shared 保型少动——收益为零，纯为省两处调用点，不值得保留裸指针语义。

### D3 — SWI 宿主钩子做成 MemoryInterface 默认方法

```rust
fn software_interrupt(&mut self, comment: u32) -> bool { let _ = comment; false }
```

`exec_arm_swi` / `exec_thumb_swi` 开头一行询问总线：返回 true 则视作流水线冲刷继续，
否则按原生语义跳 SVC 异常向量。默认实现保证上游其余逻辑零改动，同步冲突面最小。
GuestBus 借此同时接管两类遗留触发机制：

| 机制 | 旧实现 | 新归属 |
|---|---|---|
| 陷阱槽（BL 0xff00_xxxx） | step 后检查 PC | 取指拦截返回 `0xE12FFF1E`(bx lr)，记 pending_slot |
| 半宿主 SWI 0x123456 | 仅 ARM 编码内联判断（cpu.rs:96） | hook 统一接收，r0==0x18 → semihosting 出口 |

附带修复 Thumb 态半字 SWI 编码未覆盖的一致性缺口。

### D4 — GuestCpu 门面镜像旧 API，调用点机械改名

新 facade 提供 `register/set_register/pc/set_pc/cpsr/is_thumb/take_semihosting_exit_reason`
等与旧 `ArmCpu` 同形的方法，使 ext.rs 约 30 处调用点基本是 sed 级改名。旧行为中
`set_pc` 清 thumb 位、PC 对齐检查由内核自身语义承担。

### D5 — 内存访问错误策略：fail-fast 记录后报错

GuestBus 将未映射/越权访问记入 pending fault 并让当前 step 返回 `Err(ArmFault)`
（携带与现格式一致的上下文）。不采用 TDMI 总线原生的静默回绕——运行时的安全
导向和确定性输出依赖显式失败。

### D6 — X 权限粒度收窄，靠模块元数据补位

MemoryInterface 无法区分取指与数据读，EXECUTE 页检查在数据读路径上放宽为允许读。
入口点完整性已由 ext.rs 的 `executable_image()` 元数据校验承担，攻击面可控。
R/W 权限维持精确执行。

### D7 — 反汇编/解码能力收进非默认 feature "disasm"

disass*.rs 与 InstructionDecoder 迁至 `#[cfg(feature = "disasm")]`；默认构建不编译。
byteorder、num 相关引用随之门控。目的：默认依赖图最小化，同时为将来恢复
SKYENGINE_TRACE_ARM 的人可读输出留口。

### D8 — num 枚举族第一刀保留

`num`/`num_traits`/`enum_primitive_derive` 仅服务枚举 FromPrimitive；保守起见随
初始 vendor 引入，待 vendor 稳定后另行 diet 变更处理（避免首刀混入无谓的语义风险）。

## 风险 / 权衡

- [内核簿记开销（流水线建模/idle_cycle）拉慢执行] → 建立 headless 吞吐基准对比旧核，
  只记录不优化；预算语义（200M 条）不变即可保证时序确定性不受影响。
- [内存错误策略分歧导致难排查的行为差异] → 差分冒烟测试：同一随机化指令流分别喂
  新旧核（旧核暂留于测试分支直至替换落地），比对寄存器/CPSR/访存序列终态。
- [condition-passed 判定、carry 语义在极端用例分歧] → arm7tdmi 经受 jsmolka gba-tests
  社区验证；ext/ 230KB 行为测试作为最终回归网，规格验收直接挂其上。
- [banked 寄存器引入后 sp/lr 语义漂移影响现有 guest] → guest 全程 User/System 态
  执行且无非 MaskedIR 中断源；SWI hook 不切模式，banking 对现有路径应为惰性；
  差分测试覆盖之。
- [供应商漂移/维护负担] → VENDOR.md 强制记录；所有修改 `[skyengine]` 标记包夹。
- [许可证合规遗漏] → vendor 目录头部保留 MIT 版权声明；VENDOR.md 注明来源仓库
  commit hash。

## 迁移计划

1. 引入 vendor crate（不改任何行为），workspace 构建与全量测试通过。
2. 新增 GuestBus/facade/runner，先以差分冒烟验证，再切换 run_guest_execution。
3. 删除自研 ArmCpu 及其测试；全量 `cargo test/clippy` 与 E2E 回归。
4. 回滚策略：步骤 2 前 commit 边界清晰，回滚仅需还原 facade 接线；vendor crate
   为纯增量，可直接留在树上。

## 开放问题

- Q3（反汇编 feature 门控）与 Q4（num diet 时机）按 D7/D8 倾向执行，
  如实现中发现门控成本高于预期可在 tasks 中降级为后续变更。
- 差分测试所用的指令流来源（随机生成 vs 从 mrpanalysize/*.disasm 提取真实热点）
  在任务展开时确定；后者更有代表性但需要一次性解析投入。
