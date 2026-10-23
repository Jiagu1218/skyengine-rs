# skyengine-rs

用 Rust 实现的斯凯 Mythroad（MRP）应用运行时：在一个内核里同时跑三类 MRP 应用——
原生 ARM 应用（LLE，真 dsm_gm 内核）、DSR/MR 脚本应用，以及内嵌 J2ME（jar）应用。

本项目衍生自 [open-skyengine/skyengine-v2](https://github.com/open-skyengine/skyengine-v2)
（内核上游），此后已大幅偏离：J2ME KVM、Thumb/Java 双 JIT、渲染零拷贝热路径、
v2060 应用列表适配等均为本仓库独立演进。

## 执行 lanes

| Lane | 路线 | 说明 |
| --- | --- | --- |
| 原生 ARM MRP | LLE | 每个 MRP 包内嵌的 ARM EXT 镜像（logo.ext/游戏代码等）在 ARM/Thumb 模拟层中真实执行，调用宿主实现的服务表（文件/绘图/文本/音频/网络/定时器/RMS），与真机语义 1:1 |
| DSM 脚本 MRP | 宿主 VM | `\x1bMRP` 预编译脚本字节码由内置寄存器机（MrVm）解释执行 |
| J2ME MRP | KVM | 内核经 `platEx(0x70003)` 注册 J2ME 引擎后，MIDlet 套件（jc.jar）在自带 KVM 中运行：CLDC/MIDP 子集 + Canvas/Game API + RMS + HTTP + 基础多媒体，Java 侧热路径有 baseline JIT 加速 |

## 架构

```text
crates/
  skyengine-arm    ARM/Thumb CPU 模拟（寄存器、指令、guest 内存、权限）——可独立使用
  skyengine-jit    Thumb 块级 JIT（x86_64 后端；SKYENGINE_JIT=1 选通）
  skyengine-core   引擎核心：MR/EXT 运行时、dsm 内核装载、DSM 脚本 VM、
                   文件/资源/图形/字体/音频/网络/定时器服务
  skyengine-kvm    J2ME KVM：类装载、解释器 + superopcode、绿线程调度、
                   mark-sweep GC、CLDC/MIDP 原生类、Java baseline JIT
  skyengine-sdl    SDL2 桌面前端（窗口/输入/音频）
  skyengine-ffi    cdylib 封装（HarmonyOS NAPI 宿主使用）
apps/
  skyengine        CLI（inspect / run）
  skyengine-arm    ARM 模拟独立示例
  skydbg           调试器
test/
  fixtures         MRP 样本与平台文件（mythroad 布局、字体）
  e2e              vitest 端到端（真实启动 → 像素/行为断言）
```

## 构建与测试

需要：当前稳定版 Rust 工具链；SDL2 开发库（Debian/Ubuntu 为 `libsdl2-dev`，
Windows 为 SDL2 开发包，MSYS2 的 `mingw-w64-x86_64-SDL2` 即可）。

```bash
cargo build --workspace
cargo test  --workspace        # 940+ 项单元/集成测试
cargo clippy --workspace --all-targets -- -D warnings
```

端到端测试（Node + vitest）：

```bash
pnpm install
npx vitest run test/e2e/kvm    # 示例：J2ME 全链启动断言
```

## 运行

```bash
cargo run --release -p skyengine -- run \
  --work-dir <设备根目录> <应用.mrp>
```

`--work-dir` 是设备文件系统根，沿用真机目录布局：

```text
<work-dir>/
  mythroad/
    app.mrp             # 应用（各自的 start.mr 自举，直接运行即可）
    system/gb16.uc2     # 字体
    plugins/*.mrp       # 插件（netpay 等）
    dsm_gm.mrp          # 平台应用（应用列表/启动器，可选）：
                        #   真机上常驻并负责启动其它 MRP；本引擎中它只是
                        #   一个普通 MRP，运行它可获得列表 UI，并经应用栈
                        #   启动其它应用。直接运行单个 MRP 时无需它。
```

常用运行时开关（环境变量，默认全部关闭）：

| 变量 | 作用 |
| --- | --- |
| `SKYENGINE_JIT=1` | 启用 Thumb JIT |
| `SKYENGINE_KVM_JIT=1` | 启用 Java 方法 baseline JIT |
| `SKYENGINE_KVM_BUDGET` / `SKYENGINE_KVM_PUMP_MS` | KVM 单帧指令预算 / 帧泵最小间隔 |
| `SKYENGINE_TRACE_KVM` | KVM INIT/START/present 轨迹 |
| `SKYENGINE_TRACE_SLOTS` / `SKYENGINE_TRACE_FS` / `SKYENGINE_TRACE_MR_CALLS` | EXT 槽位 / 文件服务 / 脚本调用取证 |
| `SKYENGINE_KVM_TRACE_TEXT` / `SKYENGINE_KVM_TRACE_RMS` / `SKYENGINE_KVM_TRACE_RES` | Java 文本绘制 / RMS 落盘 / 资源查询取证 |

CLI 细节见 `skyengine --help`；`inspect` 子命令可列出 MRP 包条目。

## 设计文档

- [`docs/design.md`](docs/design.md) —— 总体设计
- 各 crate 模块文档（`crates/*/src`）含行为契约与真机语义取证注释

## 状态

开发中。兼容性按测试语料驱动增长：原生 lane 覆盖平台列表/常见游戏，
脚本 lane 覆盖 V50/V80 字节码，J2ME lane 以 RPG 类游戏实机验证
（绘图 transform 语义、媒体循环、RMS、退出传播等均有专项测试）。

## 声明

本项目与 Sky-mobi 及任何原始 Mythroad / MRP / 手机厂商 / 运营商无关。
Mythroad、MRP、Sky-mobi 及相关名称与商标归其各自所有者。
`test/fixtures` 中的样本仅用于兼容性研究与回归测试，请仅配合合法获得的软件使用本项目。
