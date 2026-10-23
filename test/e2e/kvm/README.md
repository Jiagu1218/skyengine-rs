# US1 T029：J2ME KVM 全链 E2E 验收（gjxjwql）

US1 的正式验收形态：引擎完整跑 `395021_1002_320480_gjxjwql.mrp` →
VMR 启动器（guest MRP 壳）→ stub 加载/字体下载流程 →
platEx(0x70003) 注册 → guest BLX vtable（INIT/START 魔数窗口）→
KVM 接管 jc.jar → MIDlet 生命周期 → present 帧桥 → 引擎屏幕 = 游戏
Splash 画面（KVM 合成字体渲染的黑底白字 Splash 文字页）。

## 运行方式

宿主 Windows（其它平台二进制名去 `.exe`）：

```bash
# 1. 构建 release 二进制（必须带 SDL2 库路径，否则链接失败）
RUSTFLAGS='-L C:\Users\12859\AppData\Local\Temp\deveco\sdl2\SDL2-2.32.10\x86_64-w64-mingw32\lib' \
  cargo build --release -p skyengine

# 2. 跑 E2E（SKYENGINE_BIN 必须显式给出：global-setup 否则会裸跑
#    cargo build，缺 SDL2 路径的链接必然失败）
SKYENGINE_BIN=target/release/skyengine.exe \
SKYENGINE_JIT=1 SKYENGINE_TRACE_KVM=1 \
  npx vitest run test/e2e/kvm
```

Rust 侧配套占位：`skyengine-core/src/arm/ext/tests/kvm.rs` 的
`kvm_e2e_smoke_spawns_release_binary_when_present`（`#[ignore]`，
二进制存在才跑，`cargo test -p skyengine-core --lib -- --ignored`）。

## 用例序列（gjxjwql.test.ts）

复用既有 vitest 驱动 `test/e2e/engine-e2e.ts`（`SkyEngineE2e`：
named pipe 命令协议 DRAW_COUNT/SCREEN/KEY/QUIT；`SkyEngineWorkspace`：
mythroad 模板隔离副本作 --work-dir）。

1. 启动：`--screen 240x320 --memory 16M`，fixture 驱动自动复制进 work-dir。
2. 等 4s → stub 弹"运行此应用需要下载字体文件"确认框。
3. 自适应推进：每轮 `KEY SOFTLEFT`（waitForDraw=false）→ 等 5s → 读
   stderr 查 `[kvm] START state=Started -> 0`；实测第 3 轮命中
   （确认下载 → 下载完成页 → 放行 → stub 走完配置解析后注册并启动）。
4. 再等 8s（MIDlet 构造 → startApp → Splash 首帧经 present 帧桥上屏）。
5. 断言：
   - `DRAW_COUNT` > 0（实测 154，KVM 帧泵持续出帧）；
   - `SCREEN` 非黑像素 > 1000（实测 2844，2 色：黑底 + 白色合成字形；
     T028 fixture 级基线 15263，全链实测时点画面为 Splash 文字页）；
   - stderr 含 `[kvm] INIT state=Initialized -> 0`、
     `[kvm] START state=Started -> 0`、`[kvm] present 240x320 ... ->
     bridge 240x320`（KVM 接管的直接证据，stub 加载画面不会出现）。
6. `QUIT`（afterEach close）。

## T029 期间的全链修复（按 E2E 实测证据）

T025 对 platEx(0x70003) input 记录的"二级指针"假设与真实 guest 不符。
`SKYENGINE_TRACE_KVM=1` 下新增的 INIT input 记录 hex dump 取证
（`[kvm] INIT input record @0x2006e1dc`）显示真实布局：

```
+000: 78 dc 06 20  64 e2 06 20      平台表指针 / 蹦床表指针
+008: 67 6a 78 6a 77 71 6c 2e 6d 72 70 00   "gjxjwql.mrp"（内联 C 字符串）
+048: 6a 63 2e 6a 61 72 00                   "jc.jar"（内联 C 字符串）
```

即包名/jar 名为**就地内联字符串**（包名槽 input+0x08..0x48，jar 名
input+0x48），非 char* 指针。据此修复（见提交）：

- `kvm.rs assemble_vm`：按内联 C 字符串读取（原实现把
  "gjxj\0\0\0" 误当指针解引用，INIT 报 unmapped guest access）。
- `KvmRuntime::set_package_path`：mr/host.rs 构造 ExtRuntime 时注入
  当前 MRP 宿主路径——guest 内联的是不带 id/分辨率前缀的相对名
  （"gjxjwql.mrp" vs 实际文件 "395021_1002_320480_gjxjwql.mrp"），
  KVM 无条件复用当前包（stub 就是当前包的壳）。
- 单测 `stage_kvm_input` 同步改为真实内联布局，并新增
  `kvm_init_resolves_inline_relative_name_via_current_mrp`。

## 遗留

- Splash 文字页为合成字体（白字黑底"豆腐块"形态）；图形 logo 帧的
  像素基线（对照 T028 的 15263）待 US 后续字体/渲染任务打磨后再固化。
- 字体下载确认页前的 stub 流程含真实网络行为（159.75.119.124），
  本机实测可走通；CI 若需完全离线可加 --dns-map 约束。
