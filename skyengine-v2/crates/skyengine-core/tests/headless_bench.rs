//! Headless throughput benchmark for the vendored arm7tdmi core.
//!
//! Measures wall-clock time for running the dsm_gm.mrp start chain
//! through its first N ticks. Reports approximate throughput.

use std::path::PathBuf;
use std::time::Instant;

use skyengine_core::{Framebuffer, PlatformDisplay, Result, Runtime, RuntimeConfig, RuntimeState};

struct HeadlessDisplay;

impl PlatformDisplay for HeadlessDisplay {
    fn present(&mut self, _framebuffer: &Framebuffer) -> Result<()> {
        Ok(())
    }
    fn poll_event(&mut self) -> Result<Option<skyengine_core::DisplayEvent>> {
        Ok(None)
    }
    fn wait_timeout(&mut self, _milliseconds: u32) {}
}

/// Run the dsm_gm.mrp start chain and measure wall-clock time for N ticks.
/// Reports ticks-per-second as an approximate throughput metric.
#[test]
#[ignore] // Benchmark: run with `cargo test -- --ignored`
fn headless_throughput_benchmark() {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut config = RuntimeConfig::for_app(workspace.join("test/fixtures/mythroad/dsm_gm.mrp"));
    config.font_path = workspace.join("test/fixtures/fonts/gb16.uc2");
    config.work_dir = std::env::temp_dir().join(format!(
        "skyengine-bench-{}",
        std::process::id()
    ));

    let mut runtime = Runtime::load(config, Box::new(HeadlessDisplay)).unwrap();
    runtime.start().unwrap();
    assert_eq!(runtime.state(), RuntimeState::Running);

    // Warm up: 5 ticks
    for _ in 0..5 {
        runtime.tick().unwrap();
    }

    // Measure: 100 ticks
    let tick_count = 100u32;
    let start = Instant::now();
    for _ in 0..tick_count {
        runtime.tick().unwrap();
    }
    let elapsed = start.elapsed();

    let tps = tick_count as f64 / elapsed.as_secs_f64();
    eprintln!(
        "[bench] {} ticks in {:.3}s → {:.1} ticks/s",
        tick_count,
        elapsed.as_secs_f64(),
        tps,
    );

    // Sanity: at least 30 ticks/s in debug mode
    assert!(tps > 30.0, "Throughput too low: {tps} ticks/s");
}
