import { afterEach, describe, expect, it } from "vitest";
import { readFile } from "node:fs/promises";
import { SkyEngineE2e, type PpmImage, SkyEngineWorkspace } from "../engine-e2e.js";

/**
 * US1 T029：J2ME KVM 全链 E2E 验收（gjxjwql.mrp 内嵌 jc.jar）。
 *
 * 链路：VMR 启动器（guest MRP 壳）→ stub 加载画面 → 字体下载确认/完成
 * 两轮按键 → platEx(0x70003) 注册（input 记录内联 "gjxjwql.mrp"/"jc.jar"）
 * → guest BLX vtable（INIT/START 魔数窗口）→ KVM 接管 jc.jar → MIDlet
 * 生命周期 → present 帧桥 → 引擎屏幕 = 游戏 Splash 画面。
 *
 * 运行方式（宿主 Windows，需先带 SDL2 RUSTFLAGS 构建 release 二进制）：
 *   SKYENGINE_BIN=target/release/skyengine.exe \
 *   SKYENGINE_JIT=1 SKYENGINE_TRACE_KVM=1 npx vitest run test/e2e/kvm
 * SKYENGINE_BIN 必须显式给出：global-setup 否则会裸跑 cargo build，
 * 缺 SDL2 库路径的链接在宿主上必然失败。SKYENGINE_JIT=1 打开 JIT 执行，
 * SKYENGINE_TRACE_KVM=1 让引擎向 stderr 打 [kvm] INIT/START/present
 * trace，用例据此断言 KVM 真正接管（而非停留在 stub 加载画面）。
 */

/** 统计非黑像素（RGB 任一分量非 0）。Splash 基线：黑底 + 白/灰主色。 */
function countNonBlack(screen: PpmImage): number {
  let count = 0;
  for (let y = 0; y < screen.height; y++) {
    for (let x = 0; x < screen.width; x++) {
      const [r, g, b] = screen.pixel(x, y);
      if (r !== 0 || g !== 0 || b !== 0) count++;
    }
  }
  return count;
}

describe("US1 T029 KVM 全链 E2E：gjxjwql", () => {
  let engine: SkyEngineE2e | undefined;
  let ws: SkyEngineWorkspace | undefined;

  afterEach(async () => {
    await engine?.close();
    engine = undefined;
    await ws?.dispose();
    ws = undefined;
  });

  it(
    "platEx(0x70003) → KVM 接管 → 引擎屏幕呈现游戏 Splash（非黑帧 + KVM 帧桥出帧）",
    async () => {
      ws = await SkyEngineWorkspace.create();
      engine = await SkyEngineE2e.start(
        "test/fixtures/395021_1002_320480_gjxjwql.mrp",
        {
          workDir: ws.dir,
          screen: "240x320",
          memory: "16M",
          timeoutMs: 60_000,
        },
      );

      // stub 前置流程（真实时钟，自适应推进）：加载画面 ~2-4s 弹出
      // "运行此应用需要下载字体文件"确认框；左软键确认后字体下载 +
      // "下载已完成"页；再放行后 stub 走完配置解析，platEx(0x70003)
      // 注册并 BLX INIT/START，KVM 接管。各页出现时刻有秒级抖动，
      // 这里轮询按键直到引擎 stderr 出现 KVM START 成功 trace。
      await engine.delay(4_000);
      let kvmStarted = false;
      for (let round = 1; round <= 8 && !kvmStarted; round++) {
        await engine.key("SOFTLEFT", { timeoutMs: 4_000, waitForDraw: false });
        await engine.delay(5_000);
        const stderr = await readFile(engine.stderrPath, "utf8");
        kvmStarted = stderr.includes("[kvm] START state=Started -> 0");
        console.info(`[t029] softleft round=${round} kvm_started=${kvmStarted}`);
      }
      expect(kvmStarted).toBe(true);

      // KVM 启动后真实时钟等待：MIDlet 构造 → startApp → Splash/Fishfly
      // Logo 首帧经 present 帧桥上屏（T028 fixture 级基线约 5-8 秒）。
      await engine.delay(8_000);

      // KVM 帧泵在持续出帧（stub 画面切换 + KVM present 均计入绘图计数）。
      const draws = await engine.drawCount();
      console.info(`[t029] draw_count=${draws}`);
      expect(draws).toBeGreaterThan(0);

      // 引擎屏幕输出 = 游戏画面：非黑像素远超黑屏（T028 fixture 级基线
      // 15263 非黑像素；全链实测见 README，阈值取 >1000 防回归）。
      const screen = await engine.screen("splash");
      const nonBlack = countNonBlack(screen);
      console.info(
        `[t029] screen=${screen.width}x${screen.height} non_black=${nonBlack} colors=${screen.uniqueColorCount()}`,
      );
      expect(nonBlack).toBeGreaterThan(1_000);

      // KVM 接管的直接证据：SKYENGINE_TRACE_KVM=1 下引擎 stderr 应包含
      // INIT/START 成功迁移与 present 帧桥 trace。仅 stub 加载画面不会出现。
      const stderr = await readFile(engine.stderrPath, "utf8");
      expect(stderr).toContain("[kvm] INIT state=Initialized -> 0");
      expect(stderr).toContain("[kvm] START state=Started -> 0");
      expect(stderr).toContain("[kvm] present");
    },
    120_000,
  );
});
