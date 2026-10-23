use super::*;

struct StubServices;

std::thread_local! {
    static STUB_FRAMEBUFFER: std::cell::RefCell<Option<Vec<u8>>> = const {
        std::cell::RefCell::new(None)
    };
    static STUB_SOUND: std::cell::RefCell<Option<(SoundType, Vec<u8>, bool)>> = const {
        std::cell::RefCell::new(None)
    };
    static STUB_SHAKE: std::cell::RefCell<Option<u32>> = const {
        std::cell::RefCell::new(None)
    };
    static STUB_PLAYBACK_TIMES: std::cell::RefCell<(u64, u64)> = const {
        std::cell::RefCell::new((0, 0))
    };
    static STUB_SOUND_PAUSED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static STUB_PLAY_CALLS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static STUB_SOUND_SEEK_MS: std::cell::RefCell<Option<u64>> = const {
        std::cell::RefCell::new(None)
    };
    static STUB_CHANNEL_LOADS: std::cell::RefCell<Vec<(usize, Vec<u8>, bool)>> = const {
        std::cell::RefCell::new(Vec::new())
    };
    static STUB_CHANNEL_OPS: std::cell::RefCell<Vec<(&'static str, u32)>> = const {
        std::cell::RefCell::new(Vec::new())
    };
    static STUB_LIVE_CHANNEL_HANDLES: std::cell::RefCell<Vec<u32>> = const {
        std::cell::RefCell::new(Vec::new())
    };
    static STUB_NEXT_CHANNEL_HANDLE: std::cell::Cell<u32> = const { std::cell::Cell::new(1) };
}

struct StubFramebufferCapture;

impl Drop for StubFramebufferCapture {
    fn drop(&mut self) {
        STUB_FRAMEBUFFER.with(|framebuffer| *framebuffer.borrow_mut() = None);
    }
}

fn capture_stub_framebuffer(framebuffer: Vec<u8>) -> StubFramebufferCapture {
    STUB_FRAMEBUFFER.with(|captured| *captured.borrow_mut() = Some(framebuffer));
    StubFramebufferCapture
}

struct StubPlaybackTimesReset;

impl Drop for StubPlaybackTimesReset {
    fn drop(&mut self) {
        STUB_PLAYBACK_TIMES.with(|times| *times.borrow_mut() = (0, 0));
    }
}

fn set_stub_playback_times(position_ms: u64, duration_ms: u64) -> StubPlaybackTimesReset {
    STUB_PLAYBACK_TIMES.with(|times| *times.borrow_mut() = (position_ms, duration_ms));
    StubPlaybackTimesReset
}

struct StubChannelsReset;

impl Drop for StubChannelsReset {
    fn drop(&mut self) {
        STUB_CHANNEL_LOADS.with(|loads| loads.borrow_mut().clear());
        STUB_CHANNEL_OPS.with(|ops| ops.borrow_mut().clear());
        STUB_LIVE_CHANNEL_HANDLES.with(|handles| handles.borrow_mut().clear());
        STUB_NEXT_CHANNEL_HANDLE.with(|next| next.set(1));
    }
}

/// Clears the multi-channel stub state for the current test thread and
/// restores it when the test ends, mirroring the framebuffer/times guards.
fn reset_stub_channels() -> StubChannelsReset {
    STUB_CHANNEL_LOADS.with(|loads| loads.borrow_mut().clear());
    STUB_CHANNEL_OPS.with(|ops| ops.borrow_mut().clear());
    STUB_LIVE_CHANNEL_HANDLES.with(|handles| handles.borrow_mut().clear());
    STUB_NEXT_CHANNEL_HANDLE.with(|next| next.set(1));
    StubChannelsReset
}

fn load_test_module(runtime: &mut ExtRuntime) {
    let mut image = b"MRPGCMAP".to_vec();
    image.extend_from_slice(&0xe12f_ff1e_u32.to_le_bytes());
    runtime
        .load_and_call_entry(&image, 0, &mut StubServices)
        .unwrap();
}

fn motion_capture_module() -> (Vec<u8>, GuestAddr) {
    const MODULE_BASE: u32 = 0x1000_0000;
    const TRAP_BASE: u32 = 0xff00_0000;
    let helper = MODULE_BASE + 40;
    let captured_axes = GuestAddr(MODULE_BASE + 88);
    let instructions = [
        0xe92d_4000, // entry: push {lr}
        0xe59f_000c, // ldr r0, [pc, #12] (helper)
        0xe3a0_1014, // mov r1, #20
        0xe59f_c008, // ldr ip, [pc, #8] (slot 25)
        0xe12f_ff3c, // blx ip
        0xe8bd_8000, // pop {pc}
        helper,
        TRAP_BASE + 25 * 4,
        0xe351_0001, // helper: cmp r1, #1
        0x112f_ff1e, // bxne lr
        0xe592_1000, // ldr r1, [r2] (event)
        0xe351_0012, // cmp r1, #18
        0x112f_ff1e, // bxne lr
        0xe592_3008, // ldr r3, [r2, #8] (motion sample pointer)
        0xe893_0007, // ldm r3, {r0, r1, r2}
        0xe59f_3008, // ldr r3, [pc, #8] (capture address)
        0xe883_0007, // stm r3, {r0, r1, r2}
        0xe3a0_0000, // mov r0, #0
        0xe12f_ff1e, // bx lr
        captured_axes.0,
    ];
    let mut image = b"MRPGCMAP".to_vec();
    image.extend(instructions.into_iter().flat_map(u32::to_le_bytes));
    image.extend_from_slice(&[0; 12]);
    (image, captured_axes)
}

#[test]
fn motion_events_pass_a_guest_pointer_to_all_three_signed_axes() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let (module, captured_axes) = motion_capture_module();
    runtime
        .load_and_call_entry(&module, 0, &mut StubServices)
        .unwrap();

    runtime
        .call_active_motion_event(12, -34, 56, &mut StubServices)
        .unwrap();

    let axes = runtime.memory.read(captured_axes, 12).unwrap();
    assert_eq!(i32::from_le_bytes(axes[0..4].try_into().unwrap()), 12);
    assert_eq!(i32::from_le_bytes(axes[4..8].try_into().unwrap()), -34);
    assert_eq!(i32::from_le_bytes(axes[8..12].try_into().unwrap()), 56);
}

#[test]
fn semihosting_exit_unwinds_the_guest_call_and_requests_runtime_exit() {
    let mut image = b"MRPGCMAP".to_vec();
    image.extend_from_slice(&0xe3a0_0018_u32.to_le_bytes()); // mov r0, #24
    image.extend_from_slice(&0xe3a0_1026_u32.to_le_bytes()); // mov r1, #38
    image.extend_from_slice(&0xef12_3456_u32.to_le_bytes()); // svc #0x123456
    let mut runtime =
        ExtRuntime::new(240, 320, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();

    assert_eq!(
        runtime
            .load_and_call_entry(&image, 0, &mut StubServices)
            .unwrap(),
        0
    );
    assert_eq!(
        runtime.lifecycle_request().unwrap(),
        Some(ExtLifecycleRequest::Exit)
    );
}

#[test]
fn trap_window_entries_are_dereferenceable_like_vendor_wrapper_code() {
    // gghjt's game.ext polls a descriptor fetched through a near-NULL
    // pointer: loading [NULL + 8] lands on the low-table mirror slot for
    // platform slot 2 (the mr_realloc trap value 0xff00_0008), and vendor
    // firmware would then read the wrapper code that entry points at —
    // a harmless instruction word that fails the caller's state compare.
    // The trap window must therefore stay readable with code-shaped filler
    // instead of faulting on the second load.
    let mut image = b"MRPGCMAP".to_vec();
    image.extend_from_slice(&0xe3a0_0000_u32.to_le_bytes()); // mov r0, #0
    image.extend_from_slice(&0xe590_0008_u32.to_le_bytes()); // ldr r0, [r0, #8]
    image.extend_from_slice(&0xe590_0000_u32.to_le_bytes()); // ldr r0, [r0]
    image.extend_from_slice(&0xe12f_ff1e_u32.to_le_bytes()); // bx lr
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();

    assert_eq!(
        runtime
            .load_and_call_entry(&image, 0, &mut StubServices)
            .unwrap(),
        0xE92D_4FF0u32 as i32
    );
}

impl NativeServices for StubServices {
    fn resize_screen(&mut self, _width: u16, _height: u16) -> Result<()> {
        Ok(())
    }

    fn start_shake(&mut self, milliseconds: u32) -> Result<()> {
        STUB_SHAKE.with(|shake| *shake.borrow_mut() = Some(milliseconds));
        Ok(())
    }

    fn stop_shake(&mut self) -> Result<()> {
        STUB_SHAKE.with(|shake| *shake.borrow_mut() = None);
        Ok(())
    }

    fn capture_framebuffer(&mut self) -> Result<Option<Vec<u8>>> {
        Ok(STUB_FRAMEBUFFER.with(|framebuffer| framebuffer.borrow().clone()))
    }

    fn read_package_file(&mut self, _package_name: &[u8], name: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(match name {
            b"owned.bin" => Some(b"guest-owned".to_vec()),
            b"package.mp3" => Some(b"ID3".to_vec()),
            b"packaged.mid" => Some(b"MThd-packaged".to_vec()),
            _ => None,
        })
    }

    fn file_info(&mut self, _name: &[u8]) -> Result<i32> {
        Ok(-1)
    }

    fn remove_file(&mut self, _name: &[u8]) -> Result<i32> {
        Ok(0)
    }

    fn rename_file(&mut self, _source: &[u8], _destination: &[u8]) -> Result<i32> {
        Ok(0)
    }

    fn create_dir(&mut self, _name: &[u8]) -> Result<i32> {
        Ok(0)
    }

    fn remove_dir(&mut self, _name: &[u8]) -> Result<i32> {
        Ok(0)
    }

    fn open_file(&mut self, name: &[u8], _mode: u32) -> Result<i32> {
        Ok(if name == b"opened.bin" { 123 } else { -1 })
    }

    fn close_file(&mut self, _handle: i32) -> Result<i32> {
        Ok(0)
    }

    fn write_file(&mut self, _handle: i32, _bytes: &[u8]) -> Result<Option<usize>> {
        Ok(None)
    }

    fn read_file(&mut self, _handle: i32, _len: usize) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }

    fn seek_file(&mut self, handle: i32, _offset: i32, _origin: u32) -> Result<Option<u64>> {
        Ok((handle == 123).then_some(456))
    }

    fn file_len(&mut self, name: &[u8]) -> Result<Option<u64>> {
        Ok((name == b"media/clip.mp3").then_some(4))
    }

    fn read_sound_file(&mut self, name: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(match name {
            b"media/clip.mp3" => Some(b"ID3!".to_vec()),
            b"media/bgm.mid" => Some(b"MThd-soundfile".to_vec()),
            b"media/UPPER.MID" => Some(b"MThd-upper".to_vec()),
            b"media/intro.amr" => Some(b"#!AMR\n".to_vec()),
            _ => None,
        })
    }

    fn play_sound(&mut self, sound_type: SoundType, data: &[u8], looped: bool) -> Result<()> {
        STUB_PLAY_CALLS.with(|calls| calls.set(calls.get().saturating_add(1)));
        STUB_SOUND.with(|sound| {
            *sound.borrow_mut() = Some((sound_type, data.to_vec(), looped));
        });
        Ok(())
    }

    fn stop_sound(&mut self) -> Result<()> {
        STUB_SOUND.with(|sound| *sound.borrow_mut() = None);
        Ok(())
    }

    fn sound_is_active(&self) -> bool {
        STUB_SOUND.with(|sound| sound.borrow().is_some())
    }

    fn sound_playback_times(&self) -> (u64, u64) {
        STUB_PLAYBACK_TIMES.with(|times| *times.borrow())
    }

    fn sound_pause(&mut self) -> Result<()> {
        STUB_SOUND_PAUSED.with(|paused| paused.set(true));
        Ok(())
    }

    fn sound_resume(&mut self) -> Result<bool> {
        Ok(STUB_SOUND_PAUSED.with(|paused| paused.replace(false)))
    }

    fn sound_seek(&mut self, position_ms: u64) -> Result<bool> {
        STUB_SOUND_SEEK_MS.with(|seek| *seek.borrow_mut() = Some(position_ms));
        Ok(true)
    }

    fn load_sound_channel(
        &mut self,
        media_type: usize,
        data: &[u8],
        looped: bool,
    ) -> Result<u32> {
        // A marker payload lets dispatch tests exercise the MR_FAILED path
        // without decoding anything in the stub.
        if data == b"undecodable" {
            return Err(crate::Error::Platform("stub decode failure".into()));
        }
        STUB_CHANNEL_LOADS.with(|loads| {
            loads.borrow_mut().push((media_type, data.to_vec(), looped))
        });
        let handle = STUB_NEXT_CHANNEL_HANDLE.with(|next| next.replace(next.get() + 1));
        STUB_LIVE_CHANNEL_HANDLES.with(|handles| handles.borrow_mut().push(handle));
        Ok(handle)
    }

    fn play_sound_channel(&mut self, handle: u32) -> bool {
        let known = STUB_LIVE_CHANNEL_HANDLES.with(|handles| handles.borrow().contains(&handle));
        if known {
            STUB_CHANNEL_OPS.with(|ops| ops.borrow_mut().push(("play", handle)));
        }
        known
    }

    fn stop_sound_channel(&mut self, handle: u32) -> bool {
        let known = STUB_LIVE_CHANNEL_HANDLES.with(|handles| handles.borrow().contains(&handle));
        if known {
            STUB_CHANNEL_OPS.with(|ops| ops.borrow_mut().push(("stop", handle)));
        }
        known
    }

    fn release_sound_channel(&mut self, handle: u32) -> bool {
        let released = STUB_LIVE_CHANNEL_HANDLES.with(|handles| {
            let mut handles = handles.borrow_mut();
            handles
                .iter()
                .position(|live| *live == handle)
                .map(|index| {
                    handles.swap_remove(index);
                })
                .is_some()
        });
        if released {
            STUB_CHANNEL_OPS.with(|ops| ops.borrow_mut().push(("release", handle)));
        }
        released
    }

    fn find_start(&mut self, _directory: &[u8]) -> Result<Option<(i32, Vec<u8>)>> {
        Ok(Some((7, b"entry.dat".to_vec())))
    }

    fn find_next(&mut self, _handle: i32) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }

    fn find_stop(&mut self, handle: i32) -> Result<bool> {
        Ok(handle == 7)
    }

    fn char_bitmap(&mut self, codepoint: u32, font: u32) -> Result<Option<(Vec<u8>, u32, u32)>> {
        Ok(match (codepoint, font) {
            (0x2603, 0 | 2 | 7) | (0x786e | 0x5b9a, 1 | 2) => {
                Some((vec![0x01, 0x80, 0x96, 0x4b], 9, 2))
            }
            (0x25, 1) => Some((vec![0x80, 0x55, 0x40, 0xaa], 8, 2)),
            // Blank four-pixel space so DrawTextEx newline handling has a
            // bitmap to measure, like the real font service.
            (0x20, 1) => Some((vec![0x00, 0x00, 0x00, 0x00], 4, 2)),
            _ => None,
        })
    }

    fn draw_bitmap(
        &mut self,
        pixels: &[u8],
        _x: i32,
        _y: i32,
        _width: usize,
        _height: usize,
    ) -> Result<()> {
        STUB_FRAMEBUFFER.with(|framebuffer| {
            let mut framebuffer = framebuffer.borrow_mut();
            if framebuffer.is_some() {
                *framebuffer = Some(pixels.to_vec());
            }
        });
        Ok(())
    }
}

mod abi;
mod external_actions;
mod kvm;
mod lifecycle;
mod services;
mod state;
