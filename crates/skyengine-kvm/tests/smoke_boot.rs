//! jc.jar 真字节码启动冒烟：Vm 门面完整链路
//! （install_jar 原始 zip → start_midlet(mainclass) → poll）。
//!
//! 与手构字节码的语义测试互补，验证解释器对真实编译产物的兼容性
//! （曾由此发现：mainclass 二进制名未转内部名、getAppProperty 空桩 NPE、
//! System.currentTimeMillis 墙钟分裂、getResourceAsStream 断流）。
//!
//! 宿主为 `Rc<RefCell<MockHostContext>>` 共享包装（模式取自
//! midp_display.rs）：poll 循环每轮 `advance_ms(16)` 模拟 60fps 引擎
//! 时钟，驱动游戏主循环的 sleep/定时唤醒。
//!
//! 画面推进史（虚拟 48s，BMP 人工核对）：
//! 51mrp Splash → Fishfly 开发商 Logo → 加载页（两行白字 3048 像素）
//! → **SP-1 斯凯 tlv 协议模拟后：全屏彩色游戏画面**（nonzero
//! 3048→76699/76800，主色棕 214667/深绿 265676/草绿 213964 + 白字；
//! 像素结构判读：顶部风景带 + 右上 UI 横条 + 中部白色面板 + 底部
//! 菜单文字行——游戏标题/菜单画面，state=Running、frames 38→756）。
//!
//! SP-1 斯凯平台 tlv 协议逆向结论（根级 f/d/e 类 + com/sky/netpay
//! 反汇编，实现见 natives.rs `sky_payment_*` 与 vm.rs 段零 a2）：
//! - tlv 元素 `[4B 大端 type][4B 大端 len][载荷]`；type 码 1=String
//!   （UTF-16LE 无 BOM，len=2*字符数）、2=Integer（len 恒 4）、3=byte[]。
//! - `f.a(II[Object)` 打包参数 → `SkyPayment.req_sync(
//!   moduleId<<16|serviceID, bytes, 0, len)`，响应同流解包回 Object[]；
//!   funcode==0 是通道打开（请求首元素 String 通道名，响应出参
//!   objs[1]=Integer(moduleId)）；空响应解包 Object[0]。
//! - 三处配合缺一不可（死点递进实证）：① 通道打开响应 objs[1] 缺失
//!   → objs[1] 越界；② strres 功能响应若空 → be.a(I)I 对缺省串
//!   "btye [] = null" parseInt 抛 NFE；③ be.e 尾部 e.f() 停主循环后
//!   **全靠 netpayCheck 的异步回调续命**（skyJNIResult → netpayCheckRet
//!   → netpayResultCB(-101) → e.g() 重启主循环线程），req_async 不
//!   回调则调度器空表 → VM Dead。
//! - 字符串编码：真机 getBytes("UNICODE") 产 `[FF FE BOM]+UTF-16LE`
//!   （KVM 原实现 Latin-1 截断会让 len 字段错位/奇数解析失败），
//!   cldc/lang.rs 已实现对称的 "UNICODE" charset。
//! - anchor=0 绘文本：斯凯 JVM 按 LEFT|TOP 兜底（严格 MIDP 校验会
//!   每帧杀画面 tick，graphics.rs anchor_delta 兼容）。
//!
//! 遗留（画面已大幅推进，如实记录）：游戏画面 tick 内仍有周期性
//! 被吞异常（f.a()V NPE、an.a(String) 资源流 NPE 等，e.run catch
//! Exception 吞掉不杀循环）——资源读取/绘制细节属后续专项。
mod common;

use std::cell::RefCell;
use std::rc::Rc;

use common::{fixture_path, mrp_read_entry};
use skyengine_kvm::host::mock::MockHostContext;
use skyengine_kvm::host::{
    FontHandle, FontMetrics, HostContext, HostNetError, HttpRequest, HttpResponse,
};
use skyengine_kvm::vm::{Vm, VmConfig, VmState};

// ---- 共享宿主（帧捕获与时钟推进需在 Vm 持有期间从测试侧观察） --------------

struct SharedHost(Rc<RefCell<MockHostContext>>);

impl HostContext for SharedHost {
    fn screen_size(&self) -> (u16, u16) {
        self.0.borrow().screen_size()
    }
    fn present(&mut self, rgba: &[u8], width: u16, height: u16) {
        self.0.borrow_mut().present(rgba, width, height)
    }
    fn font_query(&mut self, face: u8, style: u8, size: u8) -> FontHandle {
        self.0.borrow_mut().font_query(face, style, size)
    }
    fn font_metrics(&mut self, handle: FontHandle) -> FontMetrics {
        self.0.borrow_mut().font_metrics(handle)
    }
    fn text_width(&mut self, handle: FontHandle, text: &[u16]) -> u16 {
        self.0.borrow_mut().text_width(handle, text)
    }
    fn render_text(
        &mut self,
        handle: FontHandle,
        text: &[u16],
        color: u32,
        x: i32,
        y: i32,
        target: &mut [i32],
        target_w: u16,
        clip: (i32, i32, i32, i32),
    ) {
        self.0
            .borrow_mut()
            .render_text(handle, text, color, x, y, target, target_w, clip)
    }
    fn read_suite_file(&mut self, name: &str) -> Option<Vec<u8>> {
        self.0.borrow_mut().read_suite_file(name)
    }
    fn rms_load(&mut self, store: &str) -> Option<Vec<u8>> {
        self.0.borrow_mut().rms_load(store)
    }
    fn rms_save(&mut self, store: &str, data: &[u8]) {
        self.0.borrow_mut().rms_save(store, data)
    }
    fn rms_delete(&mut self, store: &str) -> bool {
        self.0.borrow_mut().rms_delete(store)
    }
    fn rms_list(&mut self) -> Vec<String> {
        self.0.borrow_mut().rms_list()
    }
    fn now_ms(&mut self) -> u64 {
        self.0.borrow_mut().now_ms()
    }
    fn audio_available(&self) -> bool {
        self.0.borrow().audio_available()
    }
    fn audio_submit(&mut self, samples: &[i16], sample_rate: u32) -> bool {
        self.0.borrow_mut().audio_submit(samples, sample_rate)
    }
    fn http_request(&mut self, req: HttpRequest) -> Result<HttpResponse, HostNetError> {
        self.0.borrow_mut().http_request(req)
    }
    fn audio_play(&mut self, data: &[u8], mime: &str, loop_forever: bool) -> bool {
        self.0.borrow_mut().audio_play(data, mime, loop_forever)
    }
    fn audio_stop(&mut self) {
        self.0.borrow_mut().audio_stop()
    }
}

#[test]
fn smoke_boot_real_jar() {
    let mrp = std::fs::read(fixture_path()).expect("read mrp fixture");
    let main = String::from_utf8_lossy(&mrp_read_entry(&mrp, "mainclass"))
        .trim()
        .to_string();
    // mainclass 为二进制类名（点号分隔）；start_midlet 内部转内部名。
    assert_eq!(main, "com.fs.arpg.GameMidlet");

    let host = Rc::new(RefCell::new(MockHostContext::new(240, 320)));
    let jar = mrp_read_entry(&mrp, "jc.jar");
    let mut vm = Vm::new(
        Box::new(SharedHost(Rc::clone(&host))),
        VmConfig::default(),
    );
    vm.install_jar("gjxjwql", &jar).expect("install jar");
    assert_eq!(vm.jar_class_count(), 89);

    vm.start_midlet(&main).expect("start midlet");
    assert_eq!(vm.state(), VmState::Running);

    // 首步执行：真字节码（clinit → <init> → startApp 链）至少跑满一次 poll。
    let first = vm.poll(200_000);
    assert_eq!(first.ran_threads, 1, "首 poll 应驱动主线程执行真字节码");

    // 引擎帧循环模拟：每轮推进 16ms（60fps）+ poll，观察驱动轮数、
    // 捕获帧数（repaint→paint→present 链）与终态。
    // T030 冒烟强化：第 60 轮（约 1s 虚拟时间）注入按键 '5'（按下+
    // 抬起）——实证按键事件链端到端（事件泵 → spawn lcdui-key 回调
    // 线程 → 执行完毕）。注入前后末帧像素统计对比（打印面）。
    //
    // 实证详录（T030 调查，jar 侧 com/fs/arpg/e.class 反汇编）：
    // - jar 中唯一输入回调类 e：keyPressed 在 `this.a==1` 时直接
    //   return；keyReleased 的 `a==1` 分支置 a=0 并调 bj.a().d()（下
    //   一画面切换）；a 的其他写入点：paint 尾部 a++（特定分支）、
    //   h() 的 `a==5→a=4`（疑似超时/Timer 推进路径）、pointerReleased。
    // - 注入 '5'/FIRE、时机第 3/40/60/120/180 轮、press+release 组合
    //   下：回调线程确认 spawn 且执行完毕（调度器 trace），无未捕获
    //   异常（SKYENGINE_KVM_TRACE_UNWIND 静默），但 canvas 实例 int
    //   字段在两次注入间零变化（a!=1，跳转分支不触发），末帧
    //   nonzero/主色分布恒定（15263/76800 不变）——游戏未进入"按键
    //   跳 splash"状态，splash 推进疑依赖 Timer/超时路径（h()，
    //   T034 Timer 队列范畴）。如实记录：按键后画面不变，按任务口径
    //   不硬造切换断言；pointer 输入（pointerPressed 同类）待 T031。
    //
    // 实证详录（T034 续）：300 轮（4.8s 虚拟）下画面仍不切换——
    // SKYENGINE_KVM_TRACE_TIMERS 下 **零 Timer 登记**（jar 的 splash
    // 段不用 java/util/Timer），真关键在 `System.currentTimeMillis`
    // 原取真实墙钟（与 mock/引擎时钟分裂）：h() 的超时轮询读它，
    // 虚拟时间快进 48s 而墙钟只走 ~1s，超时永不触发。T034 将其改
    // 宿主轨（与 sleep/Timer 同源）后，3000 轮（48s 虚拟）实测：
    // - 末帧 nonzero 15263 → 1810，主色从 splash 白/灰系变为荧光绿
    //   系（a3db00/97ca00/aae500），BMP 人工核对为 **Fishfly 开发商
    //   Logo**（黑底 + 荧光绿 "Fishfly" 字样 + 卡通鱼与气泡线条图，
    //   与 splash "我要冒泡" 气泡主题衔接）——splash 已切走；
    // - frames 43 → 429（新画面持续重绘），按键前后 delta=-13453
    //   (CHANGED)。轮数 300→3000（真实耗时 <1s），断言落"末帧与
    //   splash 基线不同"（结构化 nonzero 对比，不锁具体像素值）。
    let mut ran_rounds = 1u32;
    let mut nonzero_before_key: Option<usize> = None;
    for round in 0..3000 {
        if !vm.is_active() {
            break;
        }
        host.borrow_mut().advance_ms(16);
        if round == 60 {
            // 按键前末帧统计（画面是否切换的对比基线）。
            if let Some((rgba, _, _)) = host.borrow().captured_frames().last() {
                nonzero_before_key = Some(
                    rgba.chunks(4)
                        .filter(|px| px[..3] != [0, 0, 0])
                        .count(),
                );
            }
            vm.dispatch_key(b'5', true);
            vm.dispatch_key(b'5', false);
        }
        if matches!(round, 400 | 900 | 1500 | 2200 | 2900) {
            // 多点注入 FIRE+数字键+指针（触屏点击）：探 Logo 后的画面推进。
            vm.dispatch_key(0x85, true);
            vm.dispatch_key(0x85, false);
            vm.dispatch_key(b'5', true);
            vm.dispatch_key(b'5', false);
            vm.dispatch_pointer(120, 160, 0);
            vm.dispatch_pointer(120, 160, 1);
        }
        if vm.poll(200_000).ran_threads > 0 {
            ran_rounds += 1;
        }
    }
    let frames = host.borrow().captured_frames().len();
    println!(
        "smoke: ran_rounds={ran_rounds} frames={frames} state={:?}",
        vm.state()
    );
    // 外部依赖观测：网络/文本绘制调用面（画面停滞时定位等待源）。
    if let Some(req) = host.borrow().last_http() {
        println!("smoke: last_http = {} {}", req.method, req.url);
    } else {
        println!("smoke: last_http = none");
    }
    println!(
        "smoke: text_renders = {}",
        host.borrow().text_renders().len()
    );
    // 帧内容统计：非零像素数与出现最多的颜色（黑帧 vs 真画面）。
    if let Some((rgba, w, h)) = host.borrow().captured_frames().last() {
        let (w, h) = (*w as usize, *h as usize);
        let nonzero = rgba.chunks(4).filter(|px| px[..3] != [0, 0, 0]).count();
        let mut counts = std::collections::HashMap::new();
        for px in rgba.chunks(4) {
            *counts.entry([px[0], px[1], px[2]]).or_insert(0usize) += 1;
        }
        let mut top: Vec<_> = counts.iter().collect();
        top.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
        top.truncate(5);
        println!(
            "smoke: last frame {w}x{h} nonzero={nonzero}/{} colors={}",
            w * h,
            top.iter()
                .map(|(c, n)| format!("{:02x}{:02x}{:02x}x{n}", c[0], c[1], c[2]))
                .collect::<Vec<_>>()
                .join(" ")
        );
        // T030：按键前后末帧对比（splash 是否经 keyPressed 切走）。
        if let Some(before) = nonzero_before_key {
            println!(
                "smoke: key nonzero before={before} after={nonzero} delta={}{}",
                nonzero as i64 - before as i64,
                if nonzero != before { " (CHANGED)" } else { "" }
            );
            // T034：虚拟时间驱动超时路径（currentTimeMillis 宿主轨化）
            // 后，splash 必须切走——末帧非零像素数应偏离 splash 基线
            // （15263，白/灰系主色）进入下一画面（实测 Fishfly Logo，
            // 荧光绿系，nonzero=1810）。
            assert_ne!(
                nonzero, before,
                "T034 超时路径接通后末帧应切离 splash 基线"
            );
        }
        // SKYENGINE_SMOKE_DUMP=<路径>：末帧导出 24 位 BMP（人工核对画面）。
        if let Some(path) = std::env::var_os("SKYENGINE_SMOKE_DUMP") {
            let mut bmp = Vec::with_capacity(54 + w * h * 3);
            let stride = (w * 3 + 3) & !3;
            bmp.extend_from_slice(b"BM");
            bmp.extend_from_slice(&((54 + stride * h) as u32).to_le_bytes());
            bmp.extend_from_slice(&0u32.to_le_bytes());
            bmp.extend_from_slice(&54u32.to_le_bytes());
            bmp.extend_from_slice(&40u32.to_le_bytes());
            bmp.extend_from_slice(&(w as i32).to_le_bytes());
            bmp.extend_from_slice(&(h as i32).to_le_bytes());
            bmp.extend_from_slice(&1u16.to_le_bytes());
            bmp.extend_from_slice(&24u16.to_le_bytes());
            bmp.extend_from_slice(&0u32.to_le_bytes());
            bmp.extend_from_slice(&((stride * h) as u32).to_le_bytes());
            bmp.extend_from_slice(&0u32.to_le_bytes());
            bmp.extend_from_slice(&0u32.to_le_bytes());
            bmp.extend_from_slice(&0u32.to_le_bytes());
            bmp.extend_from_slice(&0u32.to_le_bytes());
            for row in (0..h).rev() {
                for col in 0..w {
                    let px = &rgba[(row * w + col) * 4..][..4];
                    bmp.extend_from_slice(&[px[2], px[1], px[0]]);
                }
                // 行尾 stride 填充（与「循环 push 0」等价；步长不足时不填充）
                bmp.resize(bmp.len() + stride.saturating_sub(w * 3), 0);
            }
            std::fs::write(&path, bmp).expect("dump bmp");
        }
    }
    assert!(
        matches!(vm.state(), VmState::Running | VmState::Dead),
        "终态应为 Running 或 Dead，实际 {:?}",
        vm.state()
    );
    assert!(ran_rounds >= 2, "主线程与工作线程应各被驱动至少一次");
    // 时钟驱动后（T033 + 本测试的 16ms/轮）：实测 43 帧、末帧为
    // 51mrp.com 渠道启动 Splash（"玩游戏 我要冒泡"，人工经 BMP 导出
    // 核对）——repaint→paint→present 全链出真画面，MVP 判据
    // "跑到标题/首画面"在 kvm 侧实质达成。
    assert!(
        frames >= 1,
        "时钟驱动后应捕获到渲染帧（实测 43），实际 {frames}"
    );
}
