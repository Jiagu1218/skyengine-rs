//! T012：HostContext trait + MockHostContext 行为测试。

use skyengine_kvm::host::mock::MockHostContext;
use skyengine_kvm::host::{FontHandle, HostContext, HostNetError, HttpRequest, HttpResponse};

/// 构造一个简单响应。
fn resp(status: u16, body: &[u8]) -> HttpResponse {
    HttpResponse {
        status,
        headers: vec![],
        body: body.to_vec(),
    }
}

#[test]
fn present_captures_two_frames() {
    let mut host = MockHostContext::new(240, 320);
    assert_eq!(host.screen_size(), (240, 320));

    let f1 = vec![1u8; 240 * 320 * 4];
    let f2 = vec![2u8; 240 * 320 * 4];
    host.present(&f1, 240, 320);
    host.present(&f2, 240, 320);

    let frames = host.captured_frames();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0], (f1.clone(), 240, 320));
    assert_eq!(frames[1], (f2.clone(), 240, 320));
}

#[test]
fn font_query_three_sizes_and_linear_text_width() {
    let mut host = MockHostContext::new(240, 320);
    let h_small = host.font_query(0, 0, 0);
    let h_med = host.font_query(0, 0, 1);
    let h_large = host.font_query(0, 0, 2);

    // 句柄非零且互异。
    assert_ne!(FontHandle(0), h_small);
    assert_ne!(h_small, h_med);
    assert_ne!(h_med, h_large);

    // 三档合成度量：height = px + 4, ascent = px, descent = 4, leading = 2。
    let check = |host: &mut MockHostContext, h: FontHandle, px: u16| {
        let m = host.font_metrics(h);
        assert_eq!(
            m,
            skyengine_kvm::host::FontMetrics {
                height: px + 4,
                ascent: px,
                descent: 4,
                leading: 2,
            }
        );
    };
    check(&mut host, h_small, 12);
    check(&mut host, h_med, 16);
    check(&mut host, h_large, 20);

    // text_width = 码元数 × (px/2)，线性。
    let text: Vec<u16> = vec![0x41, 0x42, 0x43, 0x44];
    assert_eq!(host.text_width(h_small, &text), 4 * 6);
    assert_eq!(host.text_width(h_med, &text), 4 * 8);
    assert_eq!(host.text_width(h_large, &text), 4 * 10);
    assert_eq!(host.text_width(h_large, &text[..2]), 2 * 10);
}

#[test]
fn render_text_writes_inside_clip_only() {
    let mut host = MockHostContext::new(100, 100);
    let h = host.font_query(0, 0, 2); // 20px 档：width/2=10/码元

    let w = 100u16;
    // 全屏缓冲初始化为 0（i32 像素位模式，与堆上 [I 同构）。
    let mut target = vec![0i32; (w as usize) * 100];
    // 文本 "AB"：宽 20px，高 24px，落点 (10, 10)；clip 限制到 (15, 15, 100, 100)。
    host.render_text(
        h,
        &[0x41, 0x42],
        0x00FF_0000,
        10,
        10,
        &mut target,
        w,
        (0, 0, 100, 100),
    );
    // 第二次：完全在 clip 外，不应写入。
    host.render_text(
        h,
        &[0x43],
        0x0000_FF00,
        10,
        10,
        &mut target,
        w,
        (200, 200, 300, 300),
    );

    // 调用记录两条。
    assert_eq!(host.text_renders().len(), 2);
    let (rh, rtext, rcolor, rx, ry) = &host.text_renders()[0];
    assert_eq!(*rh, h);
    assert_eq!(rtext, &[0x41, 0x42]);
    assert_eq!(*rcolor, 0x00FF_0000);
    assert_eq!((*rx, *ry), (10, 10));

    let px = |x: usize, y: usize| target[y * (w as usize) + x];
    // 落点区域内（矩形 x∈[10,30), y∈[10,34) ）非零且带不透明位（u32 位模式比较）。
    let red = 0xFFFF_0000u32 as i32;
    let inside = (10..30).all(|x| (10..34).all(|y| px(x, y) == red));
    assert!(inside, "落点矩形应全部写入不透明红色");
    // 矩形外未被写。
    assert_eq!(px(9, 10), 0);
    assert_eq!(px(30, 10), 0);
    assert_eq!(px(10, 9), 0);
    assert_eq!(px(10, 34), 0);
    assert_eq!(px(50, 50), 0);
}

#[test]
fn rms_roundtrip() {
    let mut host = MockHostContext::new(240, 320);

    // 初始为空。
    assert_eq!(host.rms_list(), Vec::<String>::new());
    assert_eq!(host.rms_load("s1"), None);

    // save → load 往返。
    host.rms_save("s1", b"hello");
    host.rms_save("s2", b"world");
    assert_eq!(host.rms_load("s1"), Some(b"hello".to_vec()));
    assert_eq!(host.rms_load("s2"), Some(b"world".to_vec()));
    assert_eq!(host.rms_list(), vec!["s1".to_string(), "s2".to_string()]);

    // 覆盖写。
    host.rms_save("s1", b"HELLO");
    assert_eq!(host.rms_load("s1"), Some(b"HELLO".to_vec()));

    // delete：存在的返回 true，不存在的返回 false。
    assert!(host.rms_delete("s1"));
    assert!(!host.rms_delete("s1"));
    assert_eq!(host.rms_load("s1"), None);
    assert_eq!(host.rms_list(), vec!["s2".to_string()]);
}

#[test]
fn clock_advance_and_now_ms() {
    let mut host = MockHostContext::new(240, 320);
    assert_eq!(host.now_ms(), 0);
    host.advance_ms(500);
    assert_eq!(host.now_ms(), 500);
    host.advance_ms(250);
    assert_eq!(host.now_ms(), 750);
}

#[test]
fn http_queue_and_last_http() {
    let mut host = MockHostContext::new(240, 320);

    let req = HttpRequest {
        method: "GET".to_string(),
        url: "http://example.com/a".to_string(),
        headers: vec![("X-Test".to_string(), "1".to_string())],
        body: None,
        timeout_ms: 3000,
    };

    // 空队列：Unsupported。
    let r = host.http_request(req.clone());
    assert_eq!(r.unwrap_err(), HostNetError::Unsupported);
    assert_eq!(host.last_http().unwrap().url, "http://example.com/a");

    // 脚本队列出队。
    host.push_http(Ok(resp(200, b"ok")));
    host.push_http(Err(HostNetError::Timeout));
    host.push_http(Ok(resp(404, b"nf")));

    let r1 = host.http_request(req.clone()).unwrap();
    assert_eq!(r1.status, 200);
    assert_eq!(r1.body, b"ok");
    assert_eq!(
        host.http_request(req.clone()).unwrap_err(),
        HostNetError::Timeout
    );
    let r3 = host.http_request(req.clone()).unwrap();
    assert_eq!(r3.status, 404);

    // last_http 始终记录最近一次请求。
    assert_eq!(host.last_http().unwrap().method, "GET");
    assert_eq!(host.last_http().unwrap().timeout_ms, 3000);

    // 队列耗尽后再次 Unsupported。
    assert_eq!(
        host.http_request(req).unwrap_err(),
        HostNetError::Unsupported
    );
}

#[test]
fn audio_and_suite_file_unavailable() {
    let mut host = MockHostContext::new(240, 320);
    // Mock 无音频设备：提交被丢弃。
    assert!(!host.audio_available());
    assert!(!host.audio_submit(&[0i16, 100, -100], 8000));
    // Mock 不挂 MRP 包。
    assert_eq!(host.read_suite_file("mainclass"), None);
}
