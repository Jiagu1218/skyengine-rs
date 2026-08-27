# 提案：使用 vendored arm7tdmi 内核替换自研 ARM 解释器

## 为什么

skyengine-v2 当前用于执行 MRP 原生扩展（EXT）的自研 ARM 解释器
（`crates/skyengine-core/src/arm/cpu.rs`，约 1078 行）只实现了 ARM 指令集的一个子集，
缺少寄存器 bank、CPU 模式、SPSR、异常模型和集中式条件执行等 ARM7TDMI 语义。
随着更多真实 MRP 应用的原生代码路径被启用，指令覆盖的长尾将持续侵蚀正确性，
而逐条补齐的成本高于直接采用一个久经 GBA 模拟社区验证的完整内核。

rustboyadvance-ng 的 `arm7tdmi` crate（MIT，与本仓库许可证兼容）提供了完整的
ARMv4T 语义、经构建脚本生成的零解码开销跳转表，以及干净的 `MemoryInterface`
总线抽象。供应商化（vendor）它并替换自研解释器，可以一次性消除指令覆盖率风险，
并将宿主集成收敛到单一接触面。

## 变更内容

- **新增** workspace 成员 `crates/arm7tdmi`：vendor 自 rustboyadvance-ng 的 arm7tdmi 内核，
  按 VENDOR.md 记录来源 commit 与修改清单；裁剪依赖集（删除 gdbstub/ansi_term/colored，
  disass 收进非默认 feature），SWI 处理增加可被总线消费的宿主钩子。
- **新增** `GuestBus` 内存适配器与 `GuestCpu` 门面：把现有 `GuestMemory` 的分区权限
  语义接到 `MemoryInterface` 上；陷阱窗口（0xff00_0000 起）的取指返回编码的 `bx lr`
  弹床并记录待派发 slot，替代“step 后检查 PC”的旧机制。
- **修改** `arm/ext.rs` 主执行循环（run_guest_execution）：改由门面驱动新内核，
  RETURN_SENTINEL、外部动作、异常上下文格式保持不变；约 30 处 API 调用点等价改名。
- **修改** `arm/memory.rs`：为总线补充最小只读内省能力（find_region），权限检查语义不变。
- **移除** 自研解释器 `ArmCpu` 的取指-译码-执行实现及其单元测试
  （`arm/cpu.rs` 与 `arm/cpu/tests.rs`）；其行为期望由 ext/ 下现有测试继续保障。

对 guest 二进制与 MRP 包完全无影响：容器格式、MR 字节码 VM、EXT ABI、确定性无头输出
均不改变。

## 功能 (Capabilities)

### 新增功能

- `arm-cpu`: 客户机 ARM 原生代码执行的内核级需求——完整的 ARMv4T 指令语义
  （ARM 与 Thumb 状态）、CPU 状态模型（GPR/CPSR/bank/模式）、经由带权限检查的
  客户机内存的总线访问语义、未映射访问的错误报告、主机扩展陷阱弹床的外部可见行为、
  SWI 半宿主出口语义，以及固定指令预算下的确定性执行。

### 修改功能

（无——openspec/specs 尚无既有能力，本变更是首个针对该能力的规格定义；
被替换的解释器在规格层面没有已发布的行为文档。）

## 影响

- **代码**：
  - 新增 `crates/arm7tdmi/**`（vendor + 裁剪）、`crates/skyengine-core/src/arm/{bus,facade}.rs`
  - 重写 `crates/skyengine-core/src/arm/ext.rs` 的执行循环挂接层
  - 删除 `crates/skyengine-core/src/arm/cpu.rs`、`arm/cpu/tests.rs`
- **依赖**：workspace 新增 `bit`、`num` 系、`enum_primitive_derive`、`log`（部分经
  feature 门控）；明确排除 `gdbstub*`、`ansi_term`、`colored`。
- **测试**：`ext/` 下约 230KB 行为测试成为新内核的回归网；自研 cpu 单测退役；
  补充一个双核差分冒烟用例（同一指令流喂两个内核比对终态）。
- **许可证**：引入第三方 MIT 代码需在 vendor 目录保留版权声明（VENDOR.md/源文件头）。
- **风险载体**：内存访问错误策略（fail-fast vs 静默回绕）、X 权限粒度收窄、
  Thumb 态半宿主 SWI 的行为统一化（现为顺带修复的一致性缺口）。
