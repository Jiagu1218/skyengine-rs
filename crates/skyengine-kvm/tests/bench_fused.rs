//! superopcode/取指优化基准（`--ignored` 手动跑）：
//! 真实 jc.jar（gjxjwql 夹具）全链路启动 + 固定 3000 轮帧泵，
//! 测墙钟耗时。A/B 用法：
//!
//! ```text
//! # 当前构建（融合开启）
//! cargo test --release -p skyengine-kvm --test bench_fused -- --ignored --nocapture
//! # 仅取指优化（禁融合）
//! SKYENGINE_KVM_NO_FUSE=1 cargo test ...同上
//! ```
//!
//! 负载确定性：MockHostContext 虚拟时钟（每轮 +16ms）、无按键注入、
//! RMS 为进程内存储——同二进制同环境多次运行方差 <5% 量级。

#[allow(dead_code)]
mod common;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use common::{fixture_path, mrp_read_entry};
use skyengine_kvm::host::mock::MockHostContext;
use skyengine_kvm::host::{
    FontHandle, FontMetrics, HostContext, HostNetError, HttpRequest, HttpResponse,
};
use skyengine_kvm::vm::{Vm, VmConfig, VmState};

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
#[ignore = "基准（手动跑）：真实 jc.jar 3000 轮帧泵墙钟"]
fn bench_frame_pump_3000_rounds() {
    let mrp = std::fs::read(fixture_path()).expect("read mrp fixture");
    let main = String::from_utf8_lossy(&mrp_read_entry(&mrp, "mainclass"))
        .trim()
        .to_string();
    let host = Rc::new(RefCell::new(MockHostContext::new(240, 320)));
    let jar = mrp_read_entry(&mrp, "jc.jar");
    let mut vm = Vm::new(
        Box::new(SharedHost(Rc::clone(&host))),
        VmConfig::default(),
    );
    vm.install_jar("gjxjwql", &jar).expect("install jar");
    vm.start_midlet(&main).expect("start midlet");
    assert_eq!(vm.state(), VmState::Running);

    let start = Instant::now();
    let mut rounds = 0u32;
    for _ in 0..3000 {
        if !vm.is_active() {
            break;
        }
        host.borrow_mut().advance_ms(16);
        vm.poll(200_000);
        rounds += 1;
    }
    let elapsed = start.elapsed();
    let state = vm.state();
    let frames = host.borrow().captured_frames().len();
    let profile = vm.classspace().invoke_profile_top(30);
    drop(vm);
    println!(
        "[bench] rounds={rounds} frames={frames} state={state:?} elapsed={elapsed:?} ({:.0} rounds/s)",
        rounds as f64 / elapsed.as_secs_f64()
    );
    // SKYENGINE_KVM_PROFILE=1 时顺带打印热点方法表（JIT 选型数据）。
    for (text, count) in profile {
        println!("[prof] {count:9}  {text}");
    }
}
