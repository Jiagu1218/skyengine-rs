# VENDOR — arm7tdmi

## 来源

- 上游仓库：https://github.com/michelhe/rustboyadvance-ng
- 目录：`arm7tdmi/`
- 来源 commit：`8fe45a8a7e634d11699083b789b7df583ecd6322`（2026-06-03）
- 许可证：MIT License, Copyright (c) 2019 Michel Heily（见上游仓库 LICENSE）

## 引入方式

方案 B：vendor 进本工作区并裁剪。保留上游文件布局以利 diff 同步。
所有对上游代码的修改以 `[skyengine] BEGIN` / `[skyengine] END` 注释标记包夹，
未来同步上游时可按标记重放。

## 修改清单

| 文件 | 修改 | 理由 |
|---|---|---|
| （引入时即裁剪） | `src/gdb/` 整目录未拷贝；Cargo.toml 不含 gdbstub/gdbstub_arch | gdb 远程调试不进入运行时 |
| `src/cpu.rs`、`src/psr.rs` | Debug/Display 纯文本化，去除 ansi_term/colored | 依赖洁癖；信息量保持 |
| `src/lib.rs`、disass 模块 | 反汇编/解码能力收进非默认 feature `"disasm"` | 默认依赖图最小化 |
| 内存所有权 | `Shared<UnsafeCell<T>>` → owned `pub bus: I` | skyengine guest 内存独占，无共享需求 |
| SWI 路径 | `MemoryInterface::software_interrupt(comment) -> bool` 默认方法钩子（exec_arm_swi / exec_thumb_swi） | 宿主接管半宿主 SWI（0x123456），统一 ARM/Thumb 两态语义 |

## 附注

### 差分测试结论（任务组 2）

差分夹具位于 `crates/skyengine-core/tests/differential.rs`，同一镜像与指令流分别驱动
旧自研 `ArmCpu` 与本 crate 的 `Arm7tdmiCore`（经 `DiffBus` 接到同一个 `GuestMemory`），
比对 r0-r14、NZCV/T 位、PC 与 64KB 内存映像。

- ARM 核心流（数据处理含移位进位/循环移位/乘法、LDR/STR、BL）：一致。
- 条件码全表（14 个条件字段逐一判定）：一致。
- Thumb 核心流（move-shifted/mul/ldr-str/push-pop/bne/bl）：一致。

实现注意：arm7tdmi 对 `shift_by_register` 使用 `amount = rs & 0xff`（逐字节截断），
与旧核的移位语义在极端寄存器移位量上有差异空间；差分首批未覆盖寄存器移位
长尾，真实应用回归由 ext/ 行为测试承担。
