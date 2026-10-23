use std::{
    io::{Read, Write},
    net::{Ipv4Addr, TcpListener},
    thread,
    time::{Duration, Instant},
};

use super::*;

fn verified_platform_jpeg() -> Vec<u8> {
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test/fixtures/talkcat.mrp");
    Package::open(fixture, ResourceLimits::default())
        .unwrap()
        .read_named(b"startw.jpg")
        .unwrap()
}

fn dispatch_platform_extension(
    runtime: &mut ExtRuntime,
    command: u32,
    input: GuestAddr,
    input_len: u32,
    output: GuestAddr,
    output_len: GuestAddr,
) -> ArmCpu {
    let stack = runtime.allocate(8, 4).unwrap();
    runtime.memory.write_u32(stack, output_len.0).unwrap();
    runtime
        .memory
        .write_u32(stack.checked_add(4).unwrap(), 0xfeed_face)
        .unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, command);
    cpu.set_register(1, input.0);
    cpu.set_register(2, input_len);
    cpu.set_register(3, output.0);
    cpu.set_register(13, stack.0);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(
        runtime
            .memory
            .read_u32(stack.checked_add(4).unwrap())
            .unwrap(),
        0xfeed_face
    );
    cpu
}

#[test]
fn platform_jpeg_info_returns_verified_dimensions() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let jpeg = verified_platform_jpeg();
    let source = runtime.allocate(jpeg.len(), 4).unwrap();
    runtime.memory.write(source, &jpeg).unwrap();
    let input = runtime.allocate(12, 4).unwrap();
    for (index, value) in [source.0, jpeg.len() as u32, 1].into_iter().enumerate() {
        runtime
            .memory
            .write_u32(input.checked_add((index * 4) as u32).unwrap(), value)
            .unwrap();
    }
    let output = runtime.allocate(4, 4).unwrap();
    let output_len = runtime.allocate(4, 4).unwrap();

    let cpu = dispatch_platform_extension(&mut runtime, 3_001, input, 12, output, output_len);

    assert_eq!(cpu.register(0), 0);
    assert_eq!(runtime.memory.read_u32(output_len).unwrap(), 8);
    let info = GuestAddr(runtime.memory.read_u32(output).unwrap());
    assert_eq!(runtime.memory.read_u32(info).unwrap(), 240);
    assert_eq!(
        runtime
            .memory
            .read_u32(info.checked_add(4).unwrap())
            .unwrap(),
        320
    );
}

#[test]
fn platform_jpeg_info_failure_clears_output_fields() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let source = runtime.allocate(4, 4).unwrap();
    runtime
        .memory
        .write(source, &[0xff, 0xd8, 0xff, 0xd9])
        .unwrap();
    let input = runtime.allocate(12, 4).unwrap();
    for (index, value) in [source.0, 4, 1].into_iter().enumerate() {
        runtime
            .memory
            .write_u32(input.checked_add((index * 4) as u32).unwrap(), value)
            .unwrap();
    }
    let output = runtime.allocate(4, 4).unwrap();
    let output_len = runtime.allocate(4, 4).unwrap();
    runtime.memory.write_u32(output, 0xaaaa_aaaa).unwrap();
    runtime.memory.write_u32(output_len, 0xbbbb_bbbb).unwrap();

    let cpu = dispatch_platform_extension(&mut runtime, 3_001, input, 12, output, output_len);

    assert_eq!(cpu.register(0), u32::MAX);
    assert_eq!(runtime.memory.read_u32(output).unwrap(), 0);
    assert_eq!(runtime.memory.read_u32(output_len).unwrap(), 0);
}

#[test]
fn unknown_platform_extension_returns_mr_ignore_and_preserves_output_cells() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let input = runtime.allocate(4, 4).unwrap();
    runtime.memory.write_u32(input, 0).unwrap();
    let output = runtime.allocate(4, 4).unwrap();
    let output_len = runtime.allocate(4, 4).unwrap();
    runtime.memory.write_u32(output, 0xaaaa_aaaa).unwrap();
    runtime.memory.write_u32(output_len, 0xbbbb_bbbb).unwrap();

    let cpu = dispatch_platform_extension(&mut runtime, 4_200, input, 4, output, output_len);

    assert_eq!(cpu.register(0), 1);
    assert_eq!(runtime.memory.read_u32(output).unwrap(), 0xaaaa_aaaa);
    assert_eq!(runtime.memory.read_u32(output_len).unwrap(), 0xbbbb_bbbb);
}

#[test]
fn platform_jpeg_decode_writes_little_endian_rgb565() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let jpeg = verified_platform_jpeg();
    let source = runtime.allocate(jpeg.len(), 4).unwrap();
    runtime.memory.write(source, &jpeg).unwrap();
    let destination = runtime.allocate(240 * 320 * 2, 2).unwrap();
    let input = runtime.allocate(24, 4).unwrap();
    for (index, value) in [source.0, jpeg.len() as u32, 240, 320, 1, destination.0]
        .into_iter()
        .enumerate()
    {
        runtime
            .memory
            .write_u32(input.checked_add((index * 4) as u32).unwrap(), value)
            .unwrap();
    }

    let cpu =
        dispatch_platform_extension(&mut runtime, 3_002, input, 24, GuestAddr(0), GuestAddr(0));

    assert_eq!(cpu.register(0), 0);
    assert_eq!(
        runtime.memory.read(destination, 16).unwrap(),
        [
            0x62, 0x00, 0x62, 0x00, 0x62, 0x00, 0x62, 0x00, 0x62, 0x00, 0x62, 0x00, 0x62, 0x00,
            0x62, 0x00,
        ]
    );
}

#[test]
fn platform_jpeg_decode_rejects_dimension_mismatches_without_writing() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let jpeg = verified_platform_jpeg();
    let source = runtime.allocate(jpeg.len(), 4).unwrap();
    runtime.memory.write(source, &jpeg).unwrap();
    let destination = runtime.allocate(8, 2).unwrap();
    runtime.memory.write(destination, &[0xaa; 8]).unwrap();
    let input = runtime.allocate(24, 4).unwrap();
    for (index, value) in [source.0, jpeg.len() as u32, 1, 1, 1, destination.0]
        .into_iter()
        .enumerate()
    {
        runtime
            .memory
            .write_u32(input.checked_add((index * 4) as u32).unwrap(), value)
            .unwrap();
    }

    let cpu =
        dispatch_platform_extension(&mut runtime, 3_002, input, 24, GuestAddr(0), GuestAddr(0));

    assert_eq!(cpu.register(0), u32::MAX);
    assert_eq!(runtime.memory.read(destination, 8).unwrap(), [0xaa; 8]);
}

#[test]
fn platform_jpeg_decode_rejects_a_tracked_destination_that_is_too_small() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let jpeg = verified_platform_jpeg();
    let source = runtime.allocate(jpeg.len(), 4).unwrap();
    runtime.memory.write(source, &jpeg).unwrap();
    let destination = runtime.allocate(8, 2).unwrap();
    runtime.memory.write(destination, &[0xaa; 8]).unwrap();
    let input = runtime.allocate(24, 4).unwrap();
    for (index, value) in [source.0, jpeg.len() as u32, 240, 320, 1, destination.0]
        .into_iter()
        .enumerate()
    {
        runtime
            .memory
            .write_u32(input.checked_add((index * 4) as u32).unwrap(), value)
            .unwrap();
    }

    let cpu =
        dispatch_platform_extension(&mut runtime, 3_002, input, 24, GuestAddr(0), GuestAddr(0));

    assert_eq!(cpu.register(0), u32::MAX);
    assert_eq!(runtime.memory.read(destination, 8).unwrap(), [0xaa; 8]);
}

#[test]
fn platform_jpeg_handle_commands_remain_unsupported() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let mut cpu = ArmCpu::new();

    for command in [3_004, 3_005] {
        cpu.set_register(0, command);
        assert!(matches!(
            runtime.dispatch(38, 0, &mut cpu, &mut StubServices),
            Err(Error::Abi(message))
                if message.contains(&format!("JPEG handle command {command}"))
        ));
    }
}

#[test]
fn platform_command_2013_parameterless_form_initializes_the_mp3_device() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let stack = runtime.allocate(8, 4).unwrap();
    runtime.memory.write(stack, &[0; 8]).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 2_013);
    cpu.set_register(13, stack.0);

    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();

    // dsm.c dsm_media_init returns MR_SUCCESS unconditionally, with no
    // device-3 special case.
    assert_eq!(cpu.register(0), 0);

    for register in 1..=3 {
        cpu.set_register(0, 2_013);
        cpu.set_register(register, 1);
        assert!(matches!(
            runtime.dispatch(38, 0, &mut cpu, &mut StubServices),
            Err(Error::Abi(message)) if message.contains("command 2013")
        ));
        cpu.set_register(register, 0);
    }

    runtime.memory.write_u32(stack, 1).unwrap();
    cpu.set_register(0, 2_013);
    assert!(matches!(
        runtime.dispatch(38, 0, &mut cpu, &mut StubServices),
        Err(Error::Abi(message)) if message.contains("command 2013")
    ));

    runtime.memory.write_u32(stack, 0).unwrap();
    runtime
        .memory
        .write_u32(stack.checked_add(4).unwrap(), 1)
        .unwrap();
    cpu.set_register(0, 2_013);
    assert!(matches!(
        runtime.dispatch(38, 0, &mut cpu, &mut StubServices),
        Err(Error::Abi(message)) if message.contains("command 2013")
    ));
}

#[test]
fn platform_command_2023_uses_a_bounded_existing_mp3_path() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let stack = runtime.allocate(8, 4).unwrap();
    runtime.memory.write(stack, &[0; 8]).unwrap();
    let path = runtime.allocate(32, 1).unwrap();
    runtime.memory.write(path, b"media/clip.mp3").unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 2_023);
    cpu.set_register(1, path.0);
    cpu.set_register(2, 14);
    cpu.set_register(13, stack.0);

    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();

    assert_eq!(cpu.register(0), 0);

    runtime.memory.write(path, b"media/missing.mp3").unwrap();
    cpu.set_register(0, 2_023);
    cpu.set_register(2, 17);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), u32::MAX);

    runtime.memory.write(path, b"media/package.mp3").unwrap();
    cpu.set_register(0, 2_023);
    cpu.set_register(2, 17);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 0);
}

#[test]
fn platform_command_2023_keeps_the_resource_for_the_resume_request() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let stack = runtime.allocate(8, 4).unwrap();
    runtime.memory.write(stack, &[0; 8]).unwrap();
    let path = runtime.allocate(32, 1).unwrap();
    runtime.memory.write(path, b"media/clip.mp3").unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 2_023);
    cpu.set_register(1, path.0);
    cpu.set_register(2, 14);
    cpu.set_register(13, stack.0);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 0);

    // STOP (2073) stops playback but keeps the loaded resource, so the
    // RESUME (2063) request replays it from the start like the C++ kernel
    // (dsm_media_play on the stored data).
    let cpu = dispatch_midi_media(&mut runtime, 2_073, None);
    assert_eq!(cpu.register(0), 0);
    STUB_SOUND.with(|sound| assert!(sound.borrow().is_none()));
    let cpu = dispatch_midi_media(&mut runtime, 2_063, None);
    assert_eq!(cpu.register(0), 0);
    STUB_SOUND.with(|sound| {
        let (sound_type, data, looped) = sound.borrow().clone().unwrap();
        assert_eq!(sound_type, SoundType::Mp3);
        assert_eq!(data, b"ID3!".to_vec());
        assert!(!looped);
    });

    // CLOSE (2083) releases the resource: a further resume fails.
    let cpu = dispatch_midi_media(&mut runtime, 2_083, None);
    assert_eq!(cpu.register(0), 0);
    let cpu = dispatch_midi_media(&mut runtime, 2_063, None);
    assert_eq!(cpu.register(0), u32::MAX);
}

#[test]
fn platform_command_2023_rejects_unverified_request_shapes() {
    let invalid_paths = [
        b"media/clip.wav".as_slice(),
        b"media/clip.mp3\0suffix",
        b".mp3",
        b"../clip.mp3",
        b"media//clip.mp3",
    ];
    for invalid_path in invalid_paths {
        let mut runtime =
            ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
        let stack = runtime.allocate(8, 4).unwrap();
        runtime.memory.write(stack, &[0; 8]).unwrap();
        let path = runtime.allocate(invalid_path.len(), 1).unwrap();
        runtime.memory.write(path, invalid_path).unwrap();
        let mut cpu = ArmCpu::new();
        cpu.set_register(0, 2_023);
        cpu.set_register(1, path.0);
        cpu.set_register(2, invalid_path.len() as u32);
        cpu.set_register(13, stack.0);

        assert!(matches!(
            runtime.dispatch(38, 0, &mut cpu, &mut StubServices),
            Err(Error::Abi(message)) if message.contains("platform MP3 path")
        ));
    }

    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let stack = runtime.allocate(8, 4).unwrap();
    runtime.memory.write(stack, &[0; 8]).unwrap();
    let path = runtime.allocate(4_097, 1).unwrap();
    runtime.memory.write(path, &vec![b'a'; 4_097]).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 2_023);
    cpu.set_register(1, path.0);
    cpu.set_register(2, 4_097);
    cpu.set_register(13, stack.0);
    assert!(matches!(
        runtime.dispatch(38, 0, &mut cpu, &mut StubServices),
        Err(Error::Abi(message)) if message.contains("platform MP3 request")
    ));

    cpu.set_register(0, 2_023);
    cpu.set_register(2, 14);
    cpu.set_register(3, 1);
    assert!(matches!(
        runtime.dispatch(38, 0, &mut cpu, &mut StubServices),
        Err(Error::Abi(message)) if message.contains("platform MP3 request")
    ));
}

#[test]
fn platform_command_2043_preserves_playback_for_verified_start_forms() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let stack = runtime.allocate(8, 4).unwrap();
    runtime.memory.write(stack, &[0; 8]).unwrap();
    let path = runtime.allocate(14, 1).unwrap();
    runtime.memory.write(path, b"media/clip.mp3").unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 2_023);
    cpu.set_register(1, path.0);
    cpu.set_register(2, 14);
    cpu.set_register(13, stack.0);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();

    cpu.set_register(0, 2_043);
    cpu.set_register(1, 0);
    cpu.set_register(2, 0);
    cpu.set_register(13, stack.0);

    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 0);
    STUB_SOUND.with(|sound| {
        assert_eq!(
            sound.borrow().as_ref(),
            Some(&(SoundType::Mp3, b"ID3!".to_vec(), false))
        );
    });

    let options = runtime.allocate(12, 4).unwrap();
    runtime.memory.write(options, &[0; 12]).unwrap();
    cpu.set_register(0, 2_043);
    cpu.set_register(1, options.0);
    cpu.set_register(2, 12);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 0);
    STUB_SOUND.with(|sound| {
        assert_eq!(
            sound.borrow().as_ref(),
            Some(&(SoundType::Mp3, b"ID3!".to_vec(), false))
        );
    });

    runtime.memory.write_u32(options, 1).unwrap();
    cpu.set_register(0, 2_043);
    assert!(matches!(
        runtime.dispatch(38, 0, &mut cpu, &mut StubServices),
        Err(Error::Abi(message)) if message.contains("player-start")
    ));
    runtime.memory.write_u32(options, 0).unwrap();
    cpu.set_register(1, 0);
    cpu.set_register(2, 0);

    cpu.set_register(0, 2_093);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();
    // The 209x contract reports MR_PLAT_VALUE_BASE + state: the 2023 load
    // left the device PLAYING with an active stub sink, so 1004.
    assert_eq!(cpu.register(0), 1_004);

    for register in 1..=3 {
        cpu.set_register(0, 2_043);
        cpu.set_register(register, 1);
        assert!(matches!(
            runtime.dispatch(38, 0, &mut cpu, &mut StubServices),
            Err(Error::Abi(message)) if message.contains("player-start")
        ));
        cpu.set_register(register, 0);
    }

    for offset in [0, 4] {
        runtime
            .memory
            .write_u32(stack.checked_add(offset).unwrap(), 1)
            .unwrap();
        cpu.set_register(0, 2_043);
        assert!(matches!(
            runtime.dispatch(38, 0, &mut cpu, &mut StubServices),
            Err(Error::Abi(message)) if message.contains("player-start")
        ));
        runtime
            .memory
            .write_u32(stack.checked_add(offset).unwrap(), 0)
            .unwrap();
    }
}

fn assert_platform_player_stop_command(command: u32) {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let stack = runtime.allocate(8, 4).unwrap();
    runtime.memory.write(stack, &[0; 8]).unwrap();
    let sound = runtime.allocate(4, 1).unwrap();
    runtime.memory.write(sound, b"ID3!").unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 2);
    cpu.set_register(1, sound.0);
    cpu.set_register(2, 4);
    runtime
        .dispatch(57, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert!(STUB_SOUND.with(|active| active.borrow().is_some()));

    cpu.set_register(0, command);
    cpu.set_register(1, 0);
    cpu.set_register(2, 0);
    cpu.set_register(3, 0);
    cpu.set_register(13, stack.0);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 0);
    assert!(STUB_SOUND.with(|active| active.borrow().is_none()));

    cpu.set_register(0, 2_093);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();
    // The 209x contract reports 1000 + state: STOP keeps the loaded
    // resource (1003) while CLOSE releases it back to idle (1001).
    let expected_status = if command == 2_073 { 1_003 } else { 1_001 };
    assert_eq!(cpu.register(0), expected_status);

    for register in 1..=3 {
        cpu.set_register(0, command);
        cpu.set_register(register, 1);
        assert!(matches!(
            runtime.dispatch(38, 0, &mut cpu, &mut StubServices),
            Err(Error::Abi(message)) if message.contains(&format!("command {command}"))
        ));
        cpu.set_register(register, 0);
    }

    for offset in [0, 4] {
        runtime
            .memory
            .write_u32(stack.checked_add(offset).unwrap(), 1)
            .unwrap();
        cpu.set_register(0, command);
        assert!(matches!(
            runtime.dispatch(38, 0, &mut cpu, &mut StubServices),
            Err(Error::Abi(message)) if message.contains(&format!("command {command}"))
        ));
        runtime
            .memory
            .write_u32(stack.checked_add(offset).unwrap(), 0)
            .unwrap();
    }
}

#[test]
fn platform_command_2073_stops_playback_and_whitelists_only_the_parameterless_form() {
    assert_platform_player_stop_command(2_073);
}

#[test]
fn platform_command_2083_releases_playback_and_whitelists_only_the_parameterless_form() {
    assert_platform_player_stop_command(2_083);
}

#[test]
fn platform_command_2093_whitelists_only_the_parameterless_idle_query() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let stack = runtime.allocate(8, 4).unwrap();
    runtime.memory.write(stack, &[0; 8]).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 2_093);
    cpu.set_register(13, stack.0);

    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();
    // A fresh device is IDLE: the 209x contract reports 1000 + 1.
    assert_eq!(cpu.register(0), 1_001);

    for register in 1..=3 {
        cpu.set_register(0, 2_093);
        cpu.set_register(register, 1);
        assert!(matches!(
            runtime.dispatch(38, 0, &mut cpu, &mut StubServices),
            Err(Error::Abi(message)) if message.contains("command 2093")
        ));
        cpu.set_register(register, 0);
    }

    for offset in [0, 4] {
        runtime
            .memory
            .write_u32(stack.checked_add(offset).unwrap(), 1)
            .unwrap();
        cpu.set_register(0, 2_093);
        assert!(matches!(
            runtime.dispatch(38, 0, &mut cpu, &mut StubServices),
            Err(Error::Abi(message)) if message.contains("command 2093")
        ));
        runtime
            .memory
            .write_u32(stack.checked_add(offset).unwrap(), 0)
            .unwrap();
    }
}

#[test]
fn platform_command_2093_reports_a_finished_playback_as_loaded() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let stack = runtime.allocate(8, 4).unwrap();
    runtime.memory.write(stack, &[0; 8]).unwrap();
    let path = runtime.allocate(14, 1).unwrap();
    runtime.memory.write(path, b"media/clip.mp3").unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 2_023);
    cpu.set_register(1, path.0);
    cpu.set_register(2, 14);
    cpu.set_register(13, stack.0);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 0);

    // The decoded stream reached its end (the stub sink went silent): the
    // status must degrade from PLAYING to LOADED ("play finished" in the
    // PLAY_STATE enum) instead of reporting 1004 forever.
    STUB_SOUND.with(|sound| *sound.borrow_mut() = None);
    let cpu = dispatch_midi_media(&mut runtime, 2_093, None);
    assert_eq!(cpu.register(0), 1_003);
}

fn load_mp3_for_player_tests(runtime: &mut ExtRuntime) {
    let stack = runtime.allocate(8, 4).unwrap();
    runtime.memory.write(stack, &[0; 8]).unwrap();
    let path = runtime.allocate(14, 1).unwrap();
    runtime.memory.write(path, b"media/clip.mp3").unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 2_023);
    cpu.set_register(1, path.0);
    cpu.set_register(2, 14);
    cpu.set_register(13, stack.0);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 0);
}

#[test]
fn platform_command_2053_pauses_and_2063_resumes_from_the_pause_point() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    load_mp3_for_player_tests(&mut runtime);
    let play_calls = STUB_PLAY_CALLS.with(|calls| calls.get());

    // PAUSE keeps the resource and reports the paused state (1005).
    let cpu = dispatch_midi_media(&mut runtime, 2_053, None);
    assert_eq!(cpu.register(0), 0);
    STUB_SOUND.with(|sound| assert!(sound.borrow().is_some()));
    let cpu = dispatch_midi_media(&mut runtime, 2_093, None);
    assert_eq!(cpu.register(0), 1_005);

    // RESUME continues the paused stream instead of replaying it: the
    // play counter must not move.
    let cpu = dispatch_midi_media(&mut runtime, 2_063, None);
    assert_eq!(cpu.register(0), 0);
    assert_eq!(STUB_PLAY_CALLS.with(|calls| calls.get()), play_calls);
    let cpu = dispatch_midi_media(&mut runtime, 2_093, None);
    assert_eq!(cpu.register(0), 1_004);
}

#[test]
fn platform_command_2053_without_playback_keeps_the_current_state() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();

    // Pausing an idle device is idempotent: success without inventing a
    // paused state out of nothing.
    let cpu = dispatch_midi_media(&mut runtime, 2_053, None);
    assert_eq!(cpu.register(0), 0);
    let cpu = dispatch_midi_media(&mut runtime, 2_093, None);
    assert_eq!(cpu.register(0), 1_001);
}

#[test]
fn platform_command_2063_replays_when_nothing_is_paused() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();

    // No loaded resource: RESUME falls back to the replay path and fails
    // like a PLAY without data.
    let cpu = dispatch_midi_media(&mut runtime, 2_063, None);
    assert_eq!(cpu.register(0), u32::MAX);
}

#[test]
fn platform_command_2103_seeks_the_playback_in_seconds() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    load_mp3_for_player_tests(&mut runtime);

    // T_SET_PLAY_POS carries an int32 in seconds; the audio service
    // receives milliseconds.
    let position = runtime.allocate(4, 4).unwrap();
    runtime.memory.write_u32(position, 61).unwrap();
    let cpu = dispatch_midi_media(&mut runtime, 2_103, Some((position, 4)));
    assert_eq!(cpu.register(0), 0);
    STUB_SOUND_SEEK_MS.with(|seek| assert_eq!(*seek.borrow(), Some(61_000)));

    // A seek without an input record is the legacy no-op success.
    let cpu = dispatch_midi_media(&mut runtime, 2_103, None);
    assert_eq!(cpu.register(0), 0);
    STUB_SOUND_SEEK_MS.with(|seek| assert_eq!(*seek.borrow(), Some(61_000)));
}

#[test]
fn platform_command_2103_rejects_a_truncated_position_record() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let position = runtime.allocate(4, 4).unwrap();
    runtime.memory.write_u32(position, 61).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 2_103);
    cpu.set_register(1, position.0);
    cpu.set_register(2, 2);
    let stack = runtime.allocate(8, 4).unwrap();
    runtime.memory.write(stack, &[0; 8]).unwrap();
    cpu.set_register(13, stack.0);
    assert!(matches!(
        runtime.dispatch(38, 0, &mut cpu, &mut StubServices),
        Err(Error::Abi(message)) if message.contains("seek request shape")
    ));
}

#[test]
fn platform_command_2043_replays_a_stopped_resource() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    load_mp3_for_player_tests(&mut runtime);

    // STOP keeps the resource; a following start must actually replay it.
    let cpu = dispatch_midi_media(&mut runtime, 2_073, None);
    assert_eq!(cpu.register(0), 0);
    STUB_SOUND.with(|sound| assert!(sound.borrow().is_none()));

    let cpu = dispatch_midi_media(&mut runtime, 2_043, None);
    assert_eq!(cpu.register(0), 0);
    STUB_SOUND.with(|sound| {
        assert_eq!(
            sound.borrow().as_ref(),
            Some(&(SoundType::Mp3, b"ID3!".to_vec(), false))
        );
    });
    let cpu = dispatch_midi_media(&mut runtime, 2_093, None);
    assert_eq!(cpu.register(0), 1_004);
}

#[test]
fn platform_command_2700_answers_mr_ignore_like_the_vendor_default() {
    // The C++ mr_platEx has no case 2700, so the default answers MR_IGNORE
    // without touching the input/output buffers and without faulting. The
    // DSM v2060 extension probes 2700 during startup and must survive it.
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let path = runtime.allocate(17, 1).unwrap();
    runtime.memory.write(path, b"media/record.WAV\0").unwrap();
    let input = runtime.allocate(16, 4).unwrap();
    let stack = runtime.allocate(8, 4).unwrap();
    runtime.memory.write(stack, &[0; 8]).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(13, stack.0);

    let configure_request = |runtime: &mut ExtRuntime, cpu: &mut ArmCpu| {
        for (index, value) in [path.0, 0, 0, 1].into_iter().enumerate() {
            runtime
                .memory
                .write_u32(input.checked_add((index * 4) as u32).unwrap(), value)
                .unwrap();
        }
        cpu.set_register(0, 2_700);
        cpu.set_register(1, input.0);
        cpu.set_register(2, 16);
        cpu.set_register(3, 0);
    };

    // The previously "verified" WAV recording shape is ignored like any
    // other shape: r0 = MR_IGNORE (1).
    configure_request(&mut runtime, &mut cpu);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 1);

    // So are degraded shapes (the extension may probe with NULL pointers,
    // short buffers, or stack arguments).
    for (register, value) in [(1, 0), (2, 15), (3, 1)] {
        configure_request(&mut runtime, &mut cpu);
        cpu.set_register(register, value);
        runtime
            .dispatch(38, 0, &mut cpu, &mut StubServices)
            .unwrap();
        assert_eq!(cpu.register(0), 1);
    }

    for offset in [0, 4] {
        configure_request(&mut runtime, &mut cpu);
        runtime
            .memory
            .write_u32(stack.checked_add(offset).unwrap(), 1)
            .unwrap();
        runtime
            .dispatch(38, 0, &mut cpu, &mut StubServices)
            .unwrap();
        assert_eq!(cpu.register(0), 1);
        runtime
            .memory
            .write_u32(stack.checked_add(offset).unwrap(), 0)
            .unwrap();
    }

    for (offset, value) in [(0, 0), (4, 1), (8, 1), (12, 0), (12, 2)] {
        configure_request(&mut runtime, &mut cpu);
        runtime
            .memory
            .write_u32(input.checked_add(offset).unwrap(), value)
            .unwrap();
        runtime
            .dispatch(38, 0, &mut cpu, &mut StubServices)
            .unwrap();
        assert_eq!(cpu.register(0), 1);
    }
}

#[test]
fn datetime_uses_the_deterministic_headless_baseline() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let output = runtime.allocate(8, 2).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, output.0);

    runtime
        .dispatch(34, 0, &mut cpu, &mut StubServices)
        .unwrap();

    assert_eq!(cpu.register(0), 0);
    assert_eq!(
        runtime.memory.read(output, 8).unwrap(),
        [0xdc, 0x07, 6, 20, 0, 0, 0, 3]
    );
}

#[test]
fn datetime_uses_the_configured_device_date() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    runtime.set_device_date(DeviceDate::new(2000, 2, 29).unwrap());
    let output = runtime.allocate(8, 2).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, output.0);

    runtime
        .dispatch(34, 0, &mut cpu, &mut StubServices)
        .unwrap();

    assert_eq!(
        runtime.memory.read(output, 8).unwrap(),
        [0xd0, 0x07, 2, 29, 0, 0, 0, 2]
    );
}

#[test]
fn datetime_writes_the_configured_time_of_day() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    runtime.set_device_date(DeviceDate::with_time_of_day(2000, 2, 29, 23, 59, 59).unwrap());
    let output = runtime.allocate(8, 2).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, output.0);

    runtime
        .dispatch(34, 0, &mut cpu, &mut StubServices)
        .unwrap();

    assert_eq!(
        runtime.memory.read(output, 8).unwrap(),
        [0xd0, 0x07, 2, 29, 23, 59, 59, 2]
    );
}

#[test]
fn guest_character_bitmap_uses_lsb_first_bytes() {
    let mut runtime =
        ExtRuntime::new(16, 16, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let width_out = runtime.allocate(4, 4).unwrap();
    let height_out = runtime.allocate(4, 4).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 0x2603);
    cpu.set_register(1, 7);
    cpu.set_register(2, width_out.0);
    cpu.set_register(3, height_out.0);

    runtime
        .dispatch(30, 0, &mut cpu, &mut StubServices)
        .unwrap();

    let bitmap = GuestAddr(cpu.register(0));
    assert_ne!(bitmap.0, 0);
    assert_eq!(
        runtime.memory.read(bitmap, 4).unwrap(),
        [0x80, 0x01, 0x69, 0xd2]
    );
    assert_eq!(runtime.memory.read_u32(width_out).unwrap(), 9);
    assert_eq!(runtime.memory.read_u32(height_out).unwrap(), 2);
}

#[test]
fn guest_ascii_character_bitmap_uses_one_byte_per_scanline() {
    let mut runtime =
        ExtRuntime::new(16, 16, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let width_out = runtime.allocate(4, 4).unwrap();
    let height_out = runtime.allocate(4, 4).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, u32::from(b'%'));
    cpu.set_register(1, 1);
    cpu.set_register(2, width_out.0);
    cpu.set_register(3, height_out.0);

    runtime
        .dispatch(30, 0, &mut cpu, &mut StubServices)
        .unwrap();

    let bitmap = GuestAddr(cpu.register(0));
    assert_ne!(bitmap.0, 0);
    assert_eq!(runtime.memory.read(bitmap, 2).unwrap(), [0x01, 0x02]);
    assert_eq!(runtime.memory.read_u32(width_out).unwrap(), 8);
    assert_eq!(runtime.memory.read_u32(height_out).unwrap(), 2);
}

#[test]
fn character_bitmaps_stay_out_of_the_guest_heap() {
    let mut runtime =
        ExtRuntime::new(16, 16, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let width_out = runtime.allocate(4, 4).unwrap();
    let height_out = runtime.allocate(4, 4).unwrap();
    let heap_before = runtime.guest_heap_state().unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 0x2603);
    cpu.set_register(1, 7);
    cpu.set_register(2, width_out.0);
    cpu.set_register(3, height_out.0);

    runtime
        .dispatch(30, 0, &mut cpu, &mut StubServices)
        .unwrap();

    let bitmap = GuestAddr(cpu.register(0));
    assert!(bitmap.0 >= GLYPH_BITMAP_ARENA_BASE.0);
    assert!(
        bitmap.0 < GLYPH_BITMAP_ARENA_BASE.0 + GLYPH_BITMAP_ARENA_MAX_LEN as u32
    );
    assert_eq!(
        runtime.memory.read(bitmap, 4).unwrap(),
        [0x80, 0x01, 0x69, 0xd2]
    );
    // 缓存命中时复用同一块 arena 内存，guest 堆空闲量完全不变。
    let mut cached_cpu = ArmCpu::new();
    cached_cpu.set_register(0, 0x2603);
    cached_cpu.set_register(1, 7);
    cached_cpu.set_register(2, 0);
    cached_cpu.set_register(3, 0);
    runtime
        .dispatch(30, 0, &mut cached_cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cached_cpu.register(0), bitmap.0);
    let heap_after = runtime.guest_heap_state().unwrap();
    assert_eq!(heap_after.free_left, heap_before.free_left);
}

#[test]
fn character_bitmaps_fall_back_to_the_guest_heap_when_the_arena_is_full() {
    let mut runtime =
        ExtRuntime::new(16, 16, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let width_out = runtime.allocate(4, 4).unwrap();
    let height_out = runtime.allocate(4, 4).unwrap();
    runtime.glyph_bitmap_arena_cursor = GLYPH_BITMAP_ARENA_MAX_LEN as u32;
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 0x2603);
    cpu.set_register(1, 7);
    cpu.set_register(2, width_out.0);
    cpu.set_register(3, height_out.0);

    runtime
        .dispatch(30, 0, &mut cpu, &mut StubServices)
        .unwrap();

    let bitmap = GuestAddr(cpu.register(0));
    assert!(bitmap.0 >= HEAP_BASE.0);
    assert!(bitmap.0 < HEAP_BASE.0 + DEFAULT_HEAP_LEN as u32);
    assert_eq!(
        runtime.memory.read(bitmap, 4).unwrap(),
        [0x80, 0x01, 0x69, 0xd2]
    );
}

#[test]
fn host_text_drawing_keeps_msb_first_glyph_bytes() {
    let mut runtime =
        ExtRuntime::new(16, 16, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();

    runtime
        .draw_text_to_screen(&[0x2603], 0, 0, 0xffff, 7, &mut StubServices)
        .unwrap();

    assert_eq!(
        runtime
            .memory
            .read_u16(runtime.screen_address(7, 0, 16).unwrap())
            .unwrap(),
        0xffff
    );
    assert_eq!(
        runtime
            .memory
            .read_u16(runtime.screen_address(8, 0, 16).unwrap())
            .unwrap(),
        0xffff
    );
    assert_eq!(
        runtime
            .memory
            .read_u16(runtime.screen_address(0, 0, 16).unwrap())
            .unwrap(),
        0
    );
}

#[test]
fn gray_bitmap_mode_preserves_the_key_and_clips_the_source_region() {
    let mut runtime =
        ExtRuntime::new(3, 2, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let transparent = 0xf81f_u16;
    let red = Framebuffer::rgb565(255, 0, 0);
    let blue = Framebuffer::rgb565(0, 0, 255);
    let source = runtime.allocate(12, 2).unwrap();
    let source_pixels = [0_u16, transparent, red, 0, blue, transparent];
    runtime
        .memory
        .write(
            source,
            &source_pixels
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .unwrap();
    let preserved = 0x1234;
    runtime
        .memory
        .write_u16(runtime.screen_address(0, 1, 3).unwrap(), preserved)
        .unwrap();
    let stack = runtime.allocate(24, 4).unwrap();
    for (offset, value) in [2_u32, 8, u32::from(transparent), 1, 0, 3]
        .into_iter()
        .enumerate()
    {
        runtime
            .memory
            .write_u32(stack.checked_add((offset * 4) as u32).unwrap(), value)
            .unwrap();
    }
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, source.0);
    cpu.set_register(1, u32::MAX);
    cpu.set_register(2, 0);
    cpu.set_register(3, 2);
    cpu.set_register(13, stack.0);

    runtime
        .dispatch(120, 0, &mut cpu, &mut StubServices)
        .unwrap();

    assert_eq!(cpu.register(0), 0);
    assert_eq!(
        runtime
            .memory
            .read_u16(runtime.screen_address(0, 0, 3).unwrap())
            .unwrap(),
        Framebuffer::rgb565(77, 77, 77)
    );
    assert_eq!(
        runtime
            .memory
            .read_u16(runtime.screen_address(0, 1, 3).unwrap())
            .unwrap(),
        preserved
    );
}

#[test]
fn user_info_returns_the_deterministic_headless_baseline() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let output = runtime.allocate(PLATFORM_USER_INFO_LEN, 4).unwrap();
    runtime
        .memory
        .write(output, &[0xaa; PLATFORM_USER_INFO_LEN])
        .unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, output.0);

    runtime
        .dispatch(35, 0, &mut cpu, &mut StubServices)
        .unwrap();

    assert_eq!(cpu.register(0), 0);
    assert_eq!(
        runtime.memory.read(output, PLATFORM_USER_INFO_LEN).unwrap(),
        platform_user_info()
    );
    assert_eq!(
        &runtime.memory.read(output, PLATFORM_USER_INFO_LEN).unwrap()[16..31],
        VIRTUAL_IMSI
    );
    assert_eq!(
        runtime
            .memory
            .read_u32(output.checked_add(48).unwrap())
            .unwrap(),
        PLATFORM_USER_INFO_VERSION
    );
}

#[test]
fn user_info_rejects_a_null_output() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 0);

    runtime
        .dispatch(35, 0, &mut cpu, &mut StubServices)
        .unwrap();

    assert_eq!(cpu.register(0) as i32, -1);
}

#[test]
fn mtk_profile_reserves_an_extension_window_until_slot_131_marks_code_executable() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    runtime
        .set_native_extension_profile(NativeExtensionProfile::Mtk)
        .unwrap();
    let mut module = b"MRPGCMAP".to_vec();
    module.extend_from_slice(&0xe12f_ff1e_u32.to_le_bytes());
    runtime
        .load_and_call_entry(&module, 0, &mut StubServices)
        .unwrap();
    runtime
        .memory
        .write(MTK_NATIVE_EXTENSION_BASE, &[0x70, 0x47])
        .unwrap();

    assert!(runtime.memory.fetch_u16(MTK_NATIVE_EXTENSION_BASE).is_err());

    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 0);
    cpu.set_register(1, 9);
    cpu.set_register(2, MTK_NATIVE_EXTENSION_BASE.0);
    cpu.set_register(3, 2);
    runtime
        .dispatch(131, 0, &mut cpu, &mut StubServices)
        .unwrap();

    assert_eq!(cpu.register(0), 0);
    assert_eq!(
        runtime.memory.fetch_u16(MTK_NATIVE_EXTENSION_BASE).unwrap(),
        0x4770
    );
    assert_eq!(
        runtime.modules[0].dynamic_executable_ranges,
        [Some(ExecutableRange {
            base: MTK_NATIVE_EXTENSION_BASE,
            len: 2,
        })]
    );

    cpu.set_register(0, 0);
    cpu.set_register(3, 4);
    runtime
        .dispatch(131, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(
        runtime.modules[0].dynamic_executable_ranges,
        [Some(ExecutableRange {
            base: MTK_NATIVE_EXTENSION_BASE,
            len: 4,
        })]
    );

    runtime
        .load_and_call_entry(&module, 0, &mut StubServices)
        .unwrap();
    cpu.set_register(0, 0);
    cpu.set_register(3, 2);
    assert!(matches!(
        runtime.dispatch(131, 1, &mut cpu, &mut StubServices),
        Err(Error::Abi(message)) if message.contains("another module")
    ));
    assert!(runtime.modules[1].dynamic_executable_ranges.is_empty());
}

#[test]
fn optional_device_metric_reports_unavailable() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 1_101);
    cpu.set_register(1, 2);

    runtime
        .dispatch(37, 0, &mut cpu, &mut StubServices)
        .unwrap();

    assert_eq!(cpu.register(0) as i32, -1);
}

#[test]
fn optional_runtime_profile_reports_unavailable() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 1_100);
    cpu.set_register(1, 0);

    runtime
        .dispatch(37, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), u32::MAX);

    cpu.set_register(0, 1_100);
    cpu.set_register(1, 1);
    assert!(matches!(
        runtime.dispatch(37, 0, &mut cpu, &mut StubServices),
        Err(Error::Abi(message)) if message.contains("command (1100, 1)")
    ));
}

#[test]
fn platform_screen_mode_updates_dimensions_and_screen_bitmap() {
    let mut runtime =
        ExtRuntime::new(320, 480, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let bitmap_table = GuestAddr(runtime.memory.read_u32(table_slot_address(95)).unwrap());
    let screen_bitmap = bitmap_table
        .checked_add(SCREEN_BITMAP_ID * BITMAP_ENTRY_SIZE)
        .unwrap();
    let mut cpu = ArmCpu::new();

    cpu.set_register(0, 101);
    cpu.set_register(1, 3);
    runtime
        .dispatch(37, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 0);
    assert_eq!(runtime.screen_dimensions().unwrap(), (480, 320));
    assert_eq!((runtime.display_width, runtime.display_height), (480, 320));
    assert_eq!(runtime.memory.read_u16(screen_bitmap).unwrap(), 480);
    assert_eq!(
        runtime
            .memory
            .read_u16(screen_bitmap.checked_add(2).unwrap())
            .unwrap(),
        320
    );
    assert_eq!(
        runtime
            .memory
            .read_u32(screen_bitmap.checked_add(4).unwrap())
            .unwrap(),
        320 * 480 * 2
    );

    // Some legacy apps reuse the guest-visible dimension slots for layout state.
    // Reasserting the current orientation must use the host display geometry and
    // repair the ABI slots instead of resizing the host to those transient values.
    runtime.memory.write_u32(data_slot_address(92), 19).unwrap();
    runtime.memory.write_u32(data_slot_address(93), 21).unwrap();
    runtime.memory.write_u16(screen_bitmap, 19).unwrap();
    runtime
        .memory
        .write_u16(screen_bitmap.checked_add(2).unwrap(), 21)
        .unwrap();
    cpu.set_register(0, 101);
    cpu.set_register(1, 3);
    runtime
        .dispatch(37, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(runtime.screen_dimensions().unwrap(), (480, 320));
    assert_eq!((runtime.display_width, runtime.display_height), (480, 320));
    assert_eq!(runtime.memory.read_u16(screen_bitmap).unwrap(), 480);
    assert_eq!(
        runtime
            .memory
            .read_u16(screen_bitmap.checked_add(2).unwrap())
            .unwrap(),
        320
    );

    cpu.set_register(0, 101);
    cpu.set_register(1, 0);
    runtime
        .dispatch(37, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 0);
    assert_eq!(runtime.screen_dimensions().unwrap(), (320, 480));
    assert_eq!((runtime.display_width, runtime.display_height), (320, 480));
    assert_eq!(runtime.memory.read_u16(screen_bitmap).unwrap(), 320);
    assert_eq!(
        runtime
            .memory
            .read_u16(screen_bitmap.checked_add(2).unwrap())
            .unwrap(),
        480
    );
}

#[test]
fn platform_screen_mode_accepts_180_and_ignores_out_of_contract_values() {
    let mut runtime =
        ExtRuntime::new(320, 480, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let mut cpu = ArmCpu::new();

    // MR_LCD_ROTATE_180 (2) keeps the portrait orientation, like every
    // other 0..3 value the reference kernel's mr_plat case 101 accepts.
    cpu.set_register(0, 101);
    cpu.set_register(1, 2);
    runtime
        .dispatch(37, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 0);
    assert_eq!(runtime.screen_dimensions().unwrap(), (320, 480));

    // Values outside the 0..3 contract answer MR_IGNORE without touching
    // the screen state instead of faulting the guest.
    cpu.set_register(1, 7);
    runtime
        .dispatch(37, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 1);
    assert_eq!(runtime.screen_dimensions().unwrap(), (320, 480));
}

#[test]
fn platform_payment_foreground_notification_is_accepted() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    for mode in 0..=1 {
        let mut cpu = ArmCpu::new();
        cpu.set_register(0, 1_011);
        cpu.set_register(1, mode);

        runtime
            .dispatch(37, 0, &mut cpu, &mut StubServices)
            .unwrap();

        assert_eq!(cpu.register(0), 0);
    }
}

#[test]
fn parameterless_runtime_service_notifications_are_strictly_accepted() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let mut cpu = ArmCpu::new();

    for command in [1_016, 1_020, 1_215] {
        cpu.set_register(0, command);
        cpu.set_register(1, 0);
        runtime
            .dispatch(37, 0, &mut cpu, &mut StubServices)
            .unwrap();
        assert_eq!(
            cpu.register(0),
            if command == 1_020 { 1 } else { 0 },
            "command {command}"
        );

        cpu.set_register(0, command);
        cpu.set_register(1, 1);
        assert!(matches!(
            runtime.dispatch(37, 0, &mut cpu, &mut StubServices),
            Err(Error::Abi(message)) if message.contains(&format!("command ({command}, 1)"))
        ));
    }
}

#[test]
fn platform_command_2703_whitelists_only_the_parameterless_form() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 2_703);
    cpu.set_register(1, 0);

    runtime
        .dispatch(37, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 0);

    cpu.set_register(0, 2_703);
    cpu.set_register(1, 1);
    assert!(matches!(
        runtime.dispatch(37, 0, &mut cpu, &mut StubServices),
        Err(Error::Abi(message)) if message.contains("command (2703, 1)")
    ));
}

#[test]
fn unavailable_buffered_capabilities_clear_outputs_and_reject_inputs() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let output = runtime.allocate(4, 4).unwrap();
    let output_len = runtime.allocate(4, 4).unwrap();
    let stack = runtime.allocate(4, 4).unwrap();
    runtime.memory.write_u32(stack, output_len.0).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(3, output.0);
    cpu.set_register(13, stack.0);

    for command in [1_017, 1_324] {
        runtime.memory.write_u32(output, 0xaaaa_aaaa).unwrap();
        runtime.memory.write_u32(output_len, 0xbbbb_bbbb).unwrap();
        cpu.set_register(0, command);
        cpu.set_register(1, 0);
        cpu.set_register(2, 0);
        runtime
            .dispatch(38, 0, &mut cpu, &mut StubServices)
            .unwrap();
        assert_eq!(cpu.register(0), u32::MAX, "command {command}");
        assert_eq!(runtime.memory.read_u32(output).unwrap(), 0);
        assert_eq!(runtime.memory.read_u32(output_len).unwrap(), 0);

        cpu.set_register(0, command);
        cpu.set_register(1, 1);
        assert!(matches!(
            runtime.dispatch(38, 0, &mut cpu, &mut StubServices),
            Err(Error::Abi(message)) if message.contains(&format!("command {command}"))
        ));
    }
}

#[test]
fn optional_runtime_profile_query_uses_the_unavailable_fallback() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let stack = runtime.allocate(4, 4).unwrap();
    runtime.memory.write_u32(stack, 0).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 4_033);
    cpu.set_register(13, stack.0);

    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0) as i32, -1);

    cpu.set_register(0, 4_033);
    cpu.set_register(1, 1);
    assert!(matches!(
        runtime.dispatch(38, 0, &mut cpu, &mut StubServices),
        Err(Error::Abi(message)) if message.contains("command 4033")
    ));
}

#[test]
fn platform_file_position_uses_the_legacy_bias_and_rejects_invalid_handles() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 1_231);
    cpu.set_register(1, 123);

    runtime
        .dispatch(37, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 1_456);

    cpu.set_register(0, 1_231);
    cpu.set_register(1, u32::MAX);
    runtime
        .dispatch(37, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), u32::MAX);
}

#[test]
fn runtime_shutdown_notification_does_not_change_lifecycle_state() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 1_018);
    cpu.set_register(1, 0);
    cpu.set_register(2, 0xff00_0094);
    cpu.set_register(3, 0xff00_0038);

    runtime
        .dispatch(37, 0, &mut cpu, &mut StubServices)
        .unwrap();

    assert_eq!(cpu.register(0), 0);
    assert_eq!(runtime.lifecycle_request().unwrap(), None);
    assert!(runtime.pending_external_action_completions.is_empty());
    assert!(!runtime.exit_requested);

    cpu.set_register(0, 1_018);
    cpu.set_register(1, 1);
    assert!(matches!(
        runtime.dispatch(37, 0, &mut cpu, &mut StubServices),
        Err(Error::Abi(message)) if message.contains("command (1018, 1)")
    ));
}

#[test]
fn platform_runtime_profile_returns_a_stable_twelve_byte_record() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let output = runtime.allocate(4, 4).unwrap();
    let output_len = runtime.allocate(4, 4).unwrap();
    let stack = runtime.allocate(8, 4).unwrap();
    runtime.memory.write_u32(stack, output_len.0).unwrap();
    runtime
        .memory
        .write_u32(stack.checked_add(4).unwrap(), 0xfeed_face)
        .unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 1_224);
    cpu.set_register(1, 0);
    cpu.set_register(2, 0);
    cpu.set_register(3, output.0);
    cpu.set_register(13, stack.0);

    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();

    assert_eq!(cpu.register(0), 0);
    assert_eq!(
        runtime.memory.read_u32(output).unwrap(),
        PLATFORM_RUNTIME_PROFILE_DATA.0
    );
    assert_eq!(runtime.memory.read_u32(output_len).unwrap(), 12);
    assert_eq!(
        runtime
            .memory
            .read(PLATFORM_RUNTIME_PROFILE_DATA, PLATFORM_RUNTIME_PROFILE_LEN)
            .unwrap(),
        [0; PLATFORM_RUNTIME_PROFILE_LEN]
    );
    assert_eq!(
        runtime
            .memory
            .read_u32(stack.checked_add(4).unwrap())
            .unwrap(),
        0xfeed_face
    );
}

#[test]
fn headless_native_windows_use_nonzero_owned_handles() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    load_test_module(&mut runtime);
    let mut cpu = ArmCpu::new();

    runtime
        .dispatch(78, 0, &mut cpu, &mut StubServices)
        .unwrap();
    let handle = cpu.register(0);
    assert_ne!(handle, 0);
    assert!(runtime.native_windows.contains_key(&handle));

    cpu.set_register(0, handle);
    runtime
        .dispatch(79, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 0);
    assert!(!runtime.native_windows.contains_key(&handle));

    cpu.set_register(0, handle);
    runtime
        .dispatch(79, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), u32::MAX);
}

#[test]
fn headless_audio_stop_is_idempotent() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 7);

    runtime
        .dispatch(58, 0, &mut cpu, &mut StubServices)
        .unwrap();

    assert_eq!(cpu.register(0), 0);
}

#[test]
fn headless_audio_forwards_verified_in_memory_formats() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let midi = runtime.allocate(14, 4).unwrap();
    runtime
        .memory
        .write(midi, b"MThd\0\0\0\x06\0\0\0\x01\0\x78")
        .unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 0);
    cpu.set_register(1, midi.0);
    cpu.set_register(2, 14);
    cpu.set_register(3, 1);

    runtime
        .dispatch(57, 0, &mut cpu, &mut StubServices)
        .unwrap();

    assert_eq!(cpu.register(0), 0);
    STUB_SOUND.with(|sound| {
        let sound = sound.borrow();
        let (sound_type, data, looped) = sound.as_ref().unwrap();
        assert_eq!(*sound_type, SoundType::Midi);
        assert_eq!(data, b"MThd\0\0\0\x06\0\0\0\x01\0\x78");
        assert!(*looped);
    });

    cpu.set_register(3, u32::MAX);
    runtime
        .dispatch(57, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 0);
    STUB_SOUND.with(|sound| assert!(sound.borrow().as_ref().unwrap().2));

    cpu.set_register(0, 2);
    cpu.set_register(3, 0);
    runtime
        .dispatch(57, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 0);
    STUB_SOUND.with(|sound| {
        let sound = sound.borrow();
        assert_eq!(sound.as_ref().unwrap().0, SoundType::Mp3);
        assert!(!sound.as_ref().unwrap().2);
    });

    cpu.set_register(0, 1);
    runtime
        .dispatch(57, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 0);
    STUB_SOUND.with(|sound| {
        let sound = sound.borrow();
        assert_eq!(sound.as_ref().unwrap().0, SoundType::Wav);
        assert!(!sound.as_ref().unwrap().2);
    });

    runtime
        .dispatch(58, 0, &mut cpu, &mut StubServices)
        .unwrap();
    STUB_SOUND.with(|sound| assert!(sound.borrow().is_none()));
}

#[test]
fn headless_audio_rejects_unverified_request_forms() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let sound = runtime.allocate(4, 4).unwrap();
    runtime.memory.write(sound, b"data").unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(1, sound.0);
    cpu.set_register(2, 4);

    for (sound_type, looped) in [(3, 0), (0, 2)] {
        cpu.set_register(0, sound_type);
        cpu.set_register(3, looped);
        assert!(matches!(
            runtime.dispatch(57, 0, &mut cpu, &mut StubServices),
            Err(Error::Abi(message)) if message.contains("unsupported headless sound request")
        ));
    }
}

#[test]
fn native_file_write_rejects_invalid_arguments_before_reading_input() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let input = runtime.allocate(4, 1).unwrap();
    let mut cpu = ArmCpu::new();
    for (handle, source, len) in [(u32::MAX, u32::MAX, 4), (7, 0, 4), (7, input.0, u32::MAX)] {
        cpu.set_register(0, handle);
        cpu.set_register(1, source);
        cpu.set_register(2, len);

        runtime
            .dispatch(43, 0, &mut cpu, &mut StubServices)
            .unwrap();

        assert_eq!(cpu.register(0), u32::MAX);
    }
}

#[test]
fn native_file_open_translates_every_host_failure_to_a_null_handle() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let name = runtime.allocate(32, 1).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, name.0);
    cpu.set_register(1, 1);

    for missing in [
        b"missing.bin\0".as_slice(),
        b"plugins/missing.mrp\0".as_slice(),
        b"C:\\data\\missing\0".as_slice(),
    ] {
        runtime.memory.write(name, missing).unwrap();
        cpu.set_register(0, name.0);
        runtime
            .dispatch(40, 0, &mut cpu, &mut StubServices)
            .unwrap();
        assert_eq!(cpu.register(0), 0);
    }

    runtime.memory.write(name, b"opened.bin\0").unwrap();
    cpu.set_register(0, name.0);
    runtime
        .dispatch(40, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 123);
}

#[test]
fn dns_fails_deterministically_without_a_resolver_provider() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let hostname = runtime.allocate(16, 1).unwrap();
    runtime
        .memory
        .write(hostname, b"missing.invalid\0")
        .unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, hostname.0);

    runtime
        .dispatch(83, 0, &mut cpu, &mut StubServices)
        .unwrap();

    assert_eq!(cpu.register(0) as i32, -1);
}

#[test]
fn dns_mapping_returns_a_network_order_ipv4_address() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    runtime.set_dns_mappings(Arc::from([DnsMapping {
        source: "spd.skymobiapp.com".into(),
        address: Ipv4Addr::new(159, 75, 119, 124),
        port: None,
    }]));
    let hostname = runtime.allocate(21, 1).unwrap();
    runtime
        .memory
        .write(hostname, b"SPD.SkyMobiApp.com.\0")
        .unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, hostname.0);
    cpu.set_register(1, 0x1000_0009);

    runtime
        .dispatch(83, 0, &mut cpu, &mut StubServices)
        .unwrap();

    assert_eq!(cpu.register(0), u32::from_be_bytes([159, 75, 119, 124]));
}

#[test]
fn endpoint_mapping_can_redirect_an_ip_and_port() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    runtime.set_dns_mappings(Arc::from([DnsMapping {
        source: "211.155.236.18".into(),
        address: Ipv4Addr::LOCALHOST,
        port: Some(8088),
    }]));

    assert_eq!(
        runtime.route_mapped_endpoint(u32::from_be_bytes([211, 155, 236, 18]), 6009),
        (u32::from_be_bytes([127, 0, 0, 1]), 8088)
    );
}

#[test]
fn wap_gateway_routes_to_the_internal_proxy_unless_explicitly_mapped() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let proxy = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 32_123);
    runtime.set_wap_proxy_endpoint(Some(proxy));

    assert_eq!(
        runtime.route_mapped_endpoint(u32::from_be_bytes([10, 0, 0, 172]), 80),
        (
            u32::from_be_bytes(Ipv4Addr::LOCALHOST.octets()),
            u32::from(proxy.port()),
        )
    );

    runtime.set_dns_mappings(Arc::from([DnsMapping {
        source: "10.0.0.172".into(),
        address: Ipv4Addr::new(192, 0, 2, 10),
        port: Some(8080),
    }]));
    assert_eq!(
        runtime.route_mapped_endpoint(u32::from_be_bytes([10, 0, 0, 172]), 80),
        (u32::from_be_bytes([192, 0, 2, 10]), 8080)
    );
}

#[test]
fn native_stream_socket_connects_polls_and_transfers_on_loopback() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 4];
        stream.read_exact(&mut request).unwrap();
        assert_eq!(&request, b"ping");
        thread::sleep(Duration::from_millis(50));
        stream.write_all(b"pong").unwrap();
        thread::sleep(Duration::from_millis(250));
        stream.write_all(b"done").unwrap();
    });
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 0x1000_0009);

    runtime
        .dispatch(81, 0, &mut cpu, &mut StubServices)
        .unwrap();

    assert_eq!(cpu.register(0), 0);

    cpu.set_register(0, 0);
    cpu.set_register(1, 0);
    runtime
        .dispatch(84, 0, &mut cpu, &mut StubServices)
        .unwrap();
    let handle = cpu.register(0);
    assert_ne!(handle as i32, -1);

    cpu.set_register(0, handle);
    cpu.set_register(1, u32::from_be_bytes(Ipv4Addr::LOCALHOST.octets()));
    cpu.set_register(2, u32::from(port));
    cpu.set_register(3, 1);
    runtime
        .dispatch(85, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 2);

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        cpu.set_register(0, 1_001);
        cpu.set_register(1, handle);
        runtime
            .dispatch(37, 0, &mut cpu, &mut StubServices)
            .unwrap();
        if cpu.register(0) == 0 {
            break;
        }
        assert_eq!(cpu.register(0), 2);
        assert!(Instant::now() < deadline, "loopback connect timed out");
        thread::sleep(Duration::from_millis(1));
    }

    let payload = runtime.allocate(4, 1).unwrap();
    runtime.memory.write(payload, b"ping").unwrap();
    cpu.set_register(0, handle);
    cpu.set_register(1, payload.0);
    cpu.set_register(2, 4);
    runtime
        .dispatch(89, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 4);

    let output = runtime.allocate(4, 1).unwrap();
    loop {
        cpu.set_register(0, handle);
        cpu.set_register(1, output.0);
        cpu.set_register(2, 4);
        runtime
            .dispatch(87, 0, &mut cpu, &mut StubServices)
            .unwrap();
        if cpu.register(0) == 4 {
            break;
        }
        assert_eq!(cpu.register(0) as i32, -1);
        assert!(Instant::now() < deadline, "loopback receive timed out");
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(runtime.memory.read(output, 4).unwrap(), b"pong");

    let poll_started = Instant::now();
    cpu.set_register(0, handle);
    cpu.set_register(1, output.0);
    cpu.set_register(2, 4);
    runtime
        .dispatch(87, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0) as i32, -1);
    assert!(
        poll_started.elapsed() < Duration::from_millis(100),
        "receive stopped polling after the initial response"
    );

    thread::sleep(Duration::from_millis(300));
    cpu.set_register(0, handle);
    cpu.set_register(1, output.0);
    cpu.set_register(2, 4);
    runtime
        .dispatch(87, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 4);
    assert_eq!(runtime.memory.read(output, 4).unwrap(), b"done");

    cpu.set_register(0, handle);
    runtime
        .dispatch(86, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 0);
    server.join().unwrap();

    runtime
        .dispatch(82, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 0);
}

#[test]
fn http_host_mapping_reroutes_a_proxy_connection_after_a_split_header() {
    let proxy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let proxy_port = proxy.local_addr().unwrap().port();
    let proxy_server = thread::spawn(move || {
        let (mut stream, _) = proxy.accept().unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let target = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let target_port = target.local_addr().unwrap().port();
    let expected = b"POST /resource HTTP/1.1\r\nHost: service.test\r\nContent-Length: 0\r\n\r\n";
    let target_server = thread::spawn(move || {
        let (mut stream, _) = target.accept().unwrap();
        let mut request = vec![0; expected.len()];
        stream.read_exact(&mut request).unwrap();
        stream.write_all(b"ok").unwrap();
        request
    });

    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    runtime.set_dns_mappings(Arc::from([DnsMapping {
        source: "service.test".into(),
        address: Ipv4Addr::LOCALHOST,
        port: Some(target_port),
    }]));
    let handle = runtime.allocate_native_socket_handle().unwrap().unwrap();
    assert_eq!(
        runtime.connect_native_socket(
            handle,
            u32::from_be_bytes(Ipv4Addr::LOCALHOST.octets()),
            u32::from(proxy_port),
            1,
        ),
        2
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while runtime.native_socket_state(handle) == 2 {
        assert!(Instant::now() < deadline, "proxy connect timed out");
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(runtime.native_socket_state(handle), 0);

    let split = expected
        .windows(b"Host".len())
        .position(|window| window == b"Host")
        .unwrap();
    assert_eq!(
        runtime.send_native_socket(handle, &expected[..split]),
        Some(split)
    );
    assert_eq!(
        runtime.send_native_socket(handle, &expected[split..]),
        Some(expected.len() - split)
    );

    let response = loop {
        if let Some(response) = runtime.receive_native_socket(handle, 2) {
            break response;
        }
        assert!(Instant::now() < deadline, "mapped response timed out");
        thread::sleep(Duration::from_millis(1));
    };
    assert_eq!(response, b"ok");
    assert_eq!(target_server.join().unwrap(), expected);
    assert!(proxy_server.join().unwrap().is_empty());
}

#[test]
fn failed_http_host_mapping_does_not_fall_back_to_the_proxy() {
    let proxy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let proxy_port = proxy.local_addr().unwrap().port();
    let proxy_server = thread::spawn(move || {
        let (mut stream, _) = proxy.accept().unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let unavailable = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let unavailable_port = unavailable.local_addr().unwrap().port();
    drop(unavailable);

    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    runtime.set_dns_mappings(Arc::from([DnsMapping {
        source: "unavailable.test".into(),
        address: Ipv4Addr::LOCALHOST,
        port: Some(unavailable_port),
    }]));
    let handle = runtime.allocate_native_socket_handle().unwrap().unwrap();
    assert_eq!(
        runtime.connect_native_socket(
            handle,
            u32::from_be_bytes(Ipv4Addr::LOCALHOST.octets()),
            u32::from(proxy_port),
            1,
        ),
        2
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while runtime.native_socket_state(handle) == 2 {
        assert!(Instant::now() < deadline, "proxy connect timed out");
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(runtime.native_socket_state(handle), 0);

    let request = b"POST /pay HTTP/1.1\r\nHost: unavailable.test\r\nContent-Length: 0\r\n\r\n";
    assert_eq!(runtime.send_native_socket(handle, request), None);
    assert_eq!(runtime.native_socket_state(handle), -1);
    assert_eq!(runtime.receive_native_socket(handle, 64), None);
    assert!(proxy_server.join().unwrap().is_empty());
}

#[test]
fn nonblocking_connect_reports_waiting_and_parks_send_recv() {
    // A non-blocking connect reports MR_WAITING right away; while the dial is
    // in flight, state polls, sends, and receives all report the C++
    // "pending, not failed" answers (2 / 0 / empty) instead of MR_FAILED.
    let unavailable = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let unavailable_port = unavailable.local_addr().unwrap().port();
    drop(unavailable);

    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let handle = runtime.allocate_native_socket_handle().unwrap().unwrap();
    // A freshly created socket still reports MR_WAITING like the C++ my_socket.
    assert_eq!(runtime.native_socket_state(handle), 2);
    assert_eq!(
        runtime.connect_native_socket(
            handle,
            u32::from_be_bytes(Ipv4Addr::LOCALHOST.octets()),
            u32::from(unavailable_port),
            1,
        ),
        2
    );
    assert_eq!(runtime.native_socket_state(handle), 2);
    assert_eq!(runtime.send_native_socket(handle, b"GET / HTTP/1.1\r\n"), Some(0));
    assert_eq!(runtime.receive_native_socket(handle, 64), Some(Vec::new()));
}

#[test]
fn blocking_connect_dials_synchronously() {
    // MR_SOCKET_BLOCK dials synchronously like the C++ my_connectSync path
    // and reports the final result directly instead of MR_WAITING.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let listener_port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0u8; 4];
        stream.read_exact(&mut request).unwrap();
        stream.write_all(b"pong").unwrap();
    });

    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let handle = runtime.allocate_native_socket_handle().unwrap().unwrap();
    assert_eq!(
        runtime.connect_native_socket(
            handle,
            u32::from_be_bytes(Ipv4Addr::LOCALHOST.octets()),
            u32::from(listener_port),
            0,
        ),
        0
    );
    assert_eq!(runtime.native_socket_state(handle), 0);
    assert_eq!(runtime.send_native_socket(handle, b"ping"), Some(4));
    assert_eq!(runtime.receive_native_socket(handle, 4).as_deref(), Some(&b"pong"[..]));
    server.join().unwrap();
}

#[test]
fn platform_storage_query_reports_normal_mode() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 1_218);

    runtime
        .dispatch(37, 0, &mut cpu, &mut StubServices)
        .unwrap();

    assert_eq!(cpu.register(0), 1_001);
}

#[test]
fn platform_vibration_requests_reach_the_host_service() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let mut cpu = ArmCpu::new();
    STUB_SHAKE.with(|shake| *shake.borrow_mut() = None);

    cpu.set_register(0, 500);
    runtime
        .dispatch(55, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 0);
    assert_eq!(STUB_SHAKE.with(|shake| *shake.borrow()), Some(500));

    runtime
        .dispatch(56, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 0);
    assert_eq!(STUB_SHAKE.with(|shake| *shake.borrow()), None);
}

#[test]
fn platform_rx_initialization_accepts_the_default_mode() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let mut cpu = ArmCpu::new();
    for mode in 0..=1 {
        cpu.set_register(0, 1_214);
        cpu.set_register(1, mode);
        runtime
            .dispatch(37, 0, &mut cpu, &mut StubServices)
            .unwrap();
        assert_eq!(cpu.register(0), 0, "mode {mode}");
    }
}

#[test]
fn platform_motion_provider_tracks_event_subscription_lifecycle() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 1_206);
    cpu.set_register(1, 0);

    runtime
        .dispatch(37, 0, &mut cpu, &mut StubServices)
        .unwrap();

    assert_eq!(cpu.register(0), 0);
    assert!(!runtime.motion_active());

    cpu.set_register(0, 1_206);
    cpu.set_register(1, 1);
    assert!(matches!(
        runtime.dispatch(37, 0, &mut cpu, &mut StubServices),
        Err(Error::Abi(message)) if message.contains("command (1206, 1)")
    ));

    for (command, argument) in [(4_002, 0), (4_005, 2)] {
        cpu.set_register(0, command);
        cpu.set_register(1, argument);
        runtime
            .dispatch(37, 0, &mut cpu, &mut StubServices)
            .unwrap();
        assert_eq!(cpu.register(0), 0, "command {command}, argument {argument}");
    }
    assert!(runtime.motion_active());

    cpu.set_register(0, 4_003);
    cpu.set_register(1, 0);
    runtime
        .dispatch(37, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 0);
    assert!(!runtime.motion_active());

    cpu.set_register(0, 4_005);
    cpu.set_register(1, 1);
    assert!(matches!(
        runtime.dispatch(37, 0, &mut cpu, &mut StubServices),
        Err(Error::Abi(message)) if message.contains("command (4005, 1)")
    ));

    cpu.set_register(0, 4_003);
    cpu.set_register(1, 1);
    assert!(matches!(
        runtime.dispatch(37, 0, &mut cpu, &mut StubServices),
        Err(Error::Abi(message)) if message.contains("command (4003, 1)")
    ));
}

#[test]
fn platform_talkcat_device_effect_accepts_only_the_observed_modes() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let mut cpu = ArmCpu::new();

    for mode in [2, 3] {
        cpu.set_register(0, 1_211);
        cpu.set_register(1, mode);
        runtime
            .dispatch(37, 0, &mut cpu, &mut StubServices)
            .unwrap();
        assert_eq!(cpu.register(0), 0, "mode {mode}");
    }

    for mode in [0, 1, 4] {
        cpu.set_register(0, 1_211);
        cpu.set_register(1, mode);
        assert!(matches!(
            runtime.dispatch(37, 0, &mut cpu, &mut StubServices),
            Err(Error::Abi(message)) if message.contains(&format!("command (1211, {mode})"))
        ));
    }
}

#[test]
fn platform_audio_volume_accepts_supported_levels() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let mut cpu = ArmCpu::new();

    for volume in 0..=5 {
        cpu.set_register(0, 1_302);
        cpu.set_register(1, volume);
        runtime
            .dispatch(37, 0, &mut cpu, &mut StubServices)
            .unwrap();
        assert_eq!(cpu.register(0), 0, "volume {volume}");
    }

    cpu.set_register(0, 1_302);
    cpu.set_register(1, 6);
    runtime
        .dispatch(37, 0, &mut cpu, &mut StubServices)
        .unwrap();
    // The C++ mr_plat switch has no 1302 case and answers every volume
    // call with MR_IGNORE, so out-of-range values mirror that result.
    assert_eq!(cpu.register(0), 1);
}

#[test]
fn unknown_plat_command_returns_mr_ignore() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let mut cpu = ArmCpu::new();

    // A code absent from both the Rust arms and the C++ mr_plat switch
    // falls through the C++ default, which logs a warning and answers
    // MR_IGNORE (1) without failing the dispatch.
    cpu.set_register(0, 1_502);
    cpu.set_register(1, 0);
    runtime
        .dispatch(37, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 1);

    // Known commands keep the loud verified contract for malformed shapes.
    cpu.set_register(0, 2_703);
    cpu.set_register(1, 1);
    assert!(matches!(
        runtime.dispatch(37, 0, &mut cpu, &mut StubServices),
        Err(Error::Abi(message)) if message.contains("command (2703, 1)")
    ));
}

#[test]
fn platform_sms_uses_a_bounded_headless_sink() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    load_test_module(&mut runtime);
    let mut register = ArmCpu::new();
    register.set_register(0, runtime.modules[0].base.0 + 8);
    register.set_register(1, 20);
    runtime
        .dispatch(25, 0, &mut register, &mut StubServices)
        .unwrap();
    let number = runtime.allocate(9, 1).unwrap();
    let message = runtime.allocate(24, 1).unwrap();
    runtime.memory.write(number, b"10668001\0").unwrap();
    runtime
        .memory
        .write(message, b"bounded-sms-payload-data")
        .unwrap();

    let mut cpu = ArmCpu::new();
    cpu.set_register(0, number.0);
    cpu.set_register(1, message.0);
    cpu.set_register(2, 24);
    runtime
        .dispatch(59, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 0);
    let completion = runtime.pending_sms_results.front().unwrap();
    assert_eq!(completion.owner_generation, runtime.modules[0].generation);
    let helper = runtime.modules[0].helper.unwrap();
    assert_eq!(completion.helper.module, helper.module);
    assert_eq!(completion.helper.address, helper.address);
    assert_eq!(completion.result, 0);

    assert!(
        runtime
            .dispatch_pending_platform_event(&mut StubServices)
            .unwrap()
    );
    assert!(runtime.pending_sms_results.is_empty());
    assert!(
        !runtime
            .dispatch_pending_platform_event(&mut StubServices)
            .unwrap()
    );

    cpu.set_register(0, number.0);
    cpu.set_register(1, message.0);
    cpu.set_register(2, 0);
    runtime
        .dispatch(59, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0) as i32, -1);
    assert!(runtime.pending_sms_results.is_empty());
}

#[test]
fn platform_storage_info_reports_the_vendor_geometry_per_drive() {
    // Mirrors the vendor mr_platEx case MR_GET_FREE_SPACE (1305): drives
    // A/B/C answer the real-machine T_DSM_FREE_SAPCE {total, tunit, account,
    // unit} geometry, and any other drive answers MR_IGNORE.
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let drive = runtime.allocate(4, 1).unwrap();
    let output = runtime.allocate(4, 4).unwrap();
    let output_len = runtime.allocate(4, 4).unwrap();
    let stack = runtime.allocate(4, 4).unwrap();
    runtime.memory.write_u32(stack, output_len.0).unwrap();

    let mut cpu = ArmCpu::new();
    cpu.set_register(1, drive.0);
    cpu.set_register(3, output.0);
    cpu.set_register(13, stack.0);

    for (selector, geometry) in [
        (&b"A"[..], [1722, 1024, 1271, 1024]),
        (&b"a\0"[..], [1722, 1024, 1271, 1024]),
        (&b"B"[..], [95, 1024, 77, 1024]),
        (&b"b:"[..], [95, 1024, 77, 1024]),
        (&b"C"[..], [1874, 1024 * 1024, 1873, 1024 * 1024]),
        (&b"c:/mythroad/"[..], [1874, 1024 * 1024, 1873, 1024 * 1024]),
    ] {
        runtime.memory.write(drive, selector).unwrap();
        cpu.set_register(0, 1_305);
        cpu.set_register(2, selector.len() as u32);
        runtime
            .dispatch(38, 0, &mut cpu, &mut StubServices)
            .unwrap();
        assert_eq!(cpu.register(0), 0, "selector {selector:?}");

        let info = GuestAddr(runtime.memory.read_u32(output).unwrap());
        let reported = (0..4)
            .map(|index| {
                runtime
                    .memory
                    .read_u32(info.checked_add(index * 4).unwrap())
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            runtime.memory.read_u32(output_len).unwrap(),
            PLATFORM_STORAGE_INFO_LEN as u32,
            "selector {selector:?}"
        );
        assert_eq!(&reported, &geometry, "selector {selector:?}");
        assert!(
            u64::from(reported[2]) * u64::from(reported[3])
                <= u64::from(reported[0]) * u64::from(reported[1]),
            "free space must not exceed total for {selector:?}"
        );
    }

    // Drives outside the vendor set answer MR_IGNORE (1).
    for selector in [&b"D\0"[..], &b"X"[..], &b"Y:"[..], &b"Z:\0"[..], &b"\0"[..]] {
        runtime.memory.write(drive, selector).unwrap();
        cpu.set_register(0, 1_305);
        cpu.set_register(2, selector.len() as u32);
        runtime
            .dispatch(38, 0, &mut cpu, &mut StubServices)
            .unwrap();
        assert_eq!(cpu.register(0), 1, "selector {selector:?}");
    }
}

#[test]
fn sprintf_honors_numeric_width_and_zero_padding() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let destination = runtime.allocate(32, 1).unwrap();
    let format = runtime.allocate(16, 1).unwrap();
    runtime.memory.write(format, b"%04d%02d%02d\0").unwrap();
    let stack = runtime.allocate(4, 4).unwrap();
    runtime.memory.write_u32(stack, 20).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, destination.0);
    cpu.set_register(1, format.0);
    cpu.set_register(2, 2012);
    cpu.set_register(3, 6);
    cpu.set_register(13, stack.0);

    runtime
        .dispatch(17, 0, &mut cpu, &mut StubServices)
        .unwrap();

    assert_eq!(cpu.register(0), 8);
    assert_eq!(runtime.memory.read(destination, 9).unwrap(), b"20120620\0");
}

#[test]
fn sprintf_combines_flags_width_and_precision() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let destination = runtime.allocate(64, 1).unwrap();
    let format = runtime.allocate(40, 1).unwrap();
    runtime
        .memory
        .write(format, b"%+05d|% 4d|%-4u|%#06x|%08.5d\0")
        .unwrap();
    let stack = runtime.allocate(12, 4).unwrap();
    runtime.memory.write_u32(stack, 3).unwrap();
    runtime
        .memory
        .write_u32(stack.checked_add(4).unwrap(), 0x2a)
        .unwrap();
    runtime
        .memory
        .write_u32(stack.checked_add(8).unwrap(), 12)
        .unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, destination.0);
    cpu.set_register(1, format.0);
    cpu.set_register(2, (-12_i32) as u32);
    cpu.set_register(3, 7);
    cpu.set_register(13, stack.0);

    runtime
        .dispatch(17, 0, &mut cpu, &mut StubServices)
        .unwrap();

    let expected = b"-0012|   7|3   |0x002a|   00012\0";
    assert_eq!(cpu.register(0), (expected.len() - 1) as u32);
    assert_eq!(
        runtime.memory.read(destination, expected.len()).unwrap(),
        expected
    );
}

#[test]
fn sprintf_formats_double_speed_from_register_pair() {
    // UCweb's download manager calls `sprintf(buf, "%6.1fk/s", speed)` with
    // the speed double in the aligned r2:r3 pair (low word first).
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let destination = runtime.allocate(24, 1).unwrap();
    let format = runtime.allocate(16, 1).unwrap();
    runtime.memory.write(format, b"%6.1fk/s\0").unwrap();
    let speed = 3.75_f64;
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, destination.0);
    cpu.set_register(1, format.0);
    cpu.set_register(2, speed.to_bits() as u32);
    cpu.set_register(3, (speed.to_bits() >> 32) as u32);

    runtime
        .dispatch(17, 0, &mut cpu, &mut StubServices)
        .unwrap();

    let expected = b"   3.8k/s\0";
    assert_eq!(cpu.register(0), (expected.len() - 1) as u32);
    assert_eq!(
        runtime.memory.read(destination, expected.len()).unwrap(),
        expected
    );
}

#[test]
fn sprintf_double_after_word_argument_lands_on_aligned_stack() {
    // "%d %.1f": the integer consumes r2, so the double cannot start at r3
    // and moves to the 8-byte-aligned stack area (words 0 and 1).
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let destination = runtime.allocate(24, 1).unwrap();
    let format = runtime.allocate(16, 1).unwrap();
    runtime.memory.write(format, b"%d %.1f\0").unwrap();
    let stack = runtime.allocate(8, 4).unwrap();
    let value = 2.5_f64;
    let high_word = (value.to_bits() >> 32) as u32;
    runtime
        .memory
        .write_u32(stack, value.to_bits() as u32)
        .unwrap();
    runtime
        .memory
        .write_u32(stack.checked_add(4).unwrap(), high_word)
        .unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, destination.0);
    cpu.set_register(1, format.0);
    cpu.set_register(2, 42);
    cpu.set_register(3, 0xdead_beef);
    cpu.set_register(13, stack.0);

    runtime
        .dispatch(17, 0, &mut cpu, &mut StubServices)
        .unwrap();

    let expected = b"42 2.5\0";
    assert_eq!(cpu.register(0), (expected.len() - 1) as u32);
    assert_eq!(
        runtime.memory.read(destination, expected.len()).unwrap(),
        expected
    );
}

#[test]
fn sprintf_negative_double_defaults_to_six_decimals() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let destination = runtime.allocate(24, 1).unwrap();
    let format = runtime.allocate(8, 1).unwrap();
    runtime.memory.write(format, b"%f\0").unwrap();
    let value = -2.5_f64;
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, destination.0);
    cpu.set_register(1, format.0);
    cpu.set_register(2, value.to_bits() as u32);
    cpu.set_register(3, (value.to_bits() >> 32) as u32);

    runtime
        .dispatch(17, 0, &mut cpu, &mut StubServices)
        .unwrap();

    let expected = b"-2.500000\0";
    assert_eq!(cpu.register(0), (expected.len() - 1) as u32);
    assert_eq!(
        runtime.memory.read(destination, expected.len()).unwrap(),
        expected
    );
}

#[test]
fn sprintf_word_after_double_reads_next_stack_word() {
    // "%f/%d": the double consumes r2:r3, so the integer comes from stack
    // word 0.
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let destination = runtime.allocate(24, 1).unwrap();
    let format = runtime.allocate(16, 1).unwrap();
    runtime.memory.write(format, b"%f/%d\0").unwrap();
    let stack = runtime.allocate(4, 4).unwrap();
    runtime.memory.write_u32(stack, 7).unwrap();
    let value = 1.0_f64;
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, destination.0);
    cpu.set_register(1, format.0);
    cpu.set_register(2, value.to_bits() as u32);
    cpu.set_register(3, (value.to_bits() >> 32) as u32);
    cpu.set_register(13, stack.0);

    runtime
        .dispatch(17, 0, &mut cpu, &mut StubServices)
        .unwrap();

    let expected = b"1.000000/7\0";
    assert_eq!(cpu.register(0), (expected.len() - 1) as u32);
    assert_eq!(
        runtime.memory.read(destination, expected.len()).unwrap(),
        expected
    );
}

#[test]
fn sprintf_sdk_m_is_literal_and_does_not_consume_an_argument() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let destination = runtime.allocate(32, 1).unwrap();
    let format = runtime.allocate(24, 1).unwrap();
    let suffix = runtime.allocate(8, 1).unwrap();
    runtime
        .memory
        .write(format, b"%mexit%s|%m%d.jpg\0")
        .unwrap();
    runtime.memory.write(suffix, b".slg\0").unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, destination.0);
    cpu.set_register(1, format.0);
    cpu.set_register(2, suffix.0);
    cpu.set_register(3, 7);

    runtime
        .dispatch(17, 0, &mut cpu, &mut StubServices)
        .unwrap();

    let expected = b"mexit.slg|m7.jpg\0";
    assert_eq!(cpu.register(0), (expected.len() - 1) as u32);
    assert_eq!(
        runtime.memory.read(destination, expected.len()).unwrap(),
        expected
    );
}

#[test]
fn sprintf_sdk_m_rejects_unverified_modifiers_without_writing_output() {
    for unverified_format in [b"%1m\0".as_slice(), b"%-m\0", b"%.m\0", b"%lm\0"] {
        let mut runtime =
            ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
        let destination = runtime.allocate(9, 1).unwrap();
        let format = runtime.allocate(unverified_format.len(), 1).unwrap();
        runtime.memory.write(destination, b"unchanged").unwrap();
        runtime.memory.write(format, unverified_format).unwrap();
        let mut cpu = ArmCpu::new();
        cpu.set_register(0, destination.0);
        cpu.set_register(1, format.0);

        let error = runtime
            .dispatch(17, 0, &mut cpu, &mut StubServices)
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("unsupported sprintf modifiers for '%m'"),
            "format {unverified_format:?}: {error}"
        );
        assert_eq!(runtime.memory.read(destination, 9).unwrap(), b"unchanged");
    }
}

#[test]
fn platform_storage_drive_query_resolves_supported_volumes() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let volume = runtime.allocate(1, 1).unwrap();
    let output = runtime.allocate(4, 4).unwrap();
    let output_len = runtime.allocate(4, 4).unwrap();
    let stack = runtime.allocate(4, 4).unwrap();
    runtime.memory.write_u32(stack, output_len.0).unwrap();

    let mut cpu = ArmCpu::new();
    cpu.set_register(1, volume.0);
    cpu.set_register(2, 1);
    cpu.set_register(3, output.0);
    cpu.set_register(13, stack.0);

    for supported_volume in *b"CXYZ" {
        runtime.memory.write(volume, &[supported_volume]).unwrap();
        cpu.set_register(0, 1_204);
        runtime
            .dispatch(38, 0, &mut cpu, &mut StubServices)
            .unwrap();

        let drive = GuestAddr(runtime.memory.read_u32(output).unwrap());
        assert_eq!(cpu.register(0), 0, "volume {}", supported_volume as char);
        assert_eq!(
            runtime.memory.read_u32(output_len).unwrap(),
            PLATFORM_STORAGE_DRIVE_LEN as u32
        );
        assert_eq!(
            runtime
                .memory
                .read(drive, PLATFORM_STORAGE_DRIVE_LEN)
                .unwrap(),
            PLATFORM_STORAGE_DRIVE
        );
    }

    runtime.memory.write(volume, b"D").unwrap();
    cpu.set_register(0, 1_204);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0) as i32, -1);

    runtime.memory.write(volume, b"C").unwrap();
    runtime.memory.write_u32(stack, 0).unwrap();
    cpu.set_register(0, 1_204);
    cpu.set_register(3, 0);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0) as i32, -1);
}

#[test]
fn text_drawing_decodes_legacy_gbk_and_uses_the_font_argument() {
    let mut runtime =
        ExtRuntime::new(24, 16, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let text = runtime.allocate(5, 1).unwrap();
    runtime
        .memory
        .write(text, &[0xc8, 0xb7, 0xb6, 0xa8, 0])
        .unwrap();
    let stack = runtime.allocate(16, 4).unwrap();
    runtime
        .memory
        .write_u32(stack.checked_add(8).unwrap(), 0)
        .unwrap();
    runtime
        .memory
        .write_u32(stack.checked_add(12).unwrap(), 1)
        .unwrap();

    let mut cpu = ArmCpu::new();
    cpu.set_register(0, text.0);
    cpu.set_register(3, 255);
    cpu.set_register(13, stack.0);

    runtime
        .dispatch(123, 0, &mut cpu, &mut StubServices)
        .unwrap();

    assert_eq!(cpu.register(0), 0);
    assert_eq!(
        runtime
            .memory
            .read_u16(runtime.screen_address(7, 0, 24).unwrap())
            .unwrap(),
        0xf800
    );
    assert_eq!(
        runtime
            .memory
            .read_u16(runtime.screen_address(16, 0, 24).unwrap())
            .unwrap(),
        0xf800
    );
}

#[test]
fn text_drawing_reads_ucs2_be_when_requested() {
    let mut runtime =
        ExtRuntime::new(24, 16, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let text = runtime.allocate(6, 2).unwrap();
    runtime
        .memory
        .write(text, &[0x78, 0x6e, 0x5b, 0x9a, 0, 0])
        .unwrap();
    let stack = runtime.allocate(16, 4).unwrap();
    runtime
        .memory
        .write_u32(stack.checked_add(8).unwrap(), 1)
        .unwrap();
    runtime
        .memory
        .write_u32(stack.checked_add(12).unwrap(), 2)
        .unwrap();

    let mut cpu = ArmCpu::new();
    cpu.set_register(0, text.0);
    cpu.set_register(3, 255);
    cpu.set_register(13, stack.0);

    runtime
        .dispatch(123, 0, &mut cpu, &mut StubServices)
        .unwrap();

    assert_eq!(cpu.register(0), 0);
    assert_eq!(
        runtime
            .memory
            .read_u16(runtime.screen_address(7, 0, 24).unwrap())
            .unwrap(),
        0xf800
    );
    assert_eq!(
        runtime
            .memory
            .read_u16(runtime.screen_address(16, 0, 24).unwrap())
            .unwrap(),
        0xf800
    );
}

#[test]
fn text_drawing_rejects_invalid_font_arguments() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let text = runtime.allocate(4, 2).unwrap();
    runtime.memory.write(text, &[0, b'A', 0, 0]).unwrap();
    let stack = runtime.allocate(16, 4).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, text.0);
    cpu.set_register(13, stack.0);

    runtime
        .memory
        .write_u32(stack.checked_add(8).unwrap(), 1)
        .unwrap();
    runtime
        .memory
        .write_u32(stack.checked_add(12).unwrap(), 3)
        .unwrap();
    assert!(matches!(
        runtime.dispatch(123, 0, &mut cpu, &mut StubServices),
        Err(Error::Abi(message))
            if message == "unsupported text drawing font 3 called by module 0"
    ));
}

#[test]
fn text_drawing_treats_a_null_text_pointer_as_empty_text() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let stack = runtime.allocate(16, 4).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 0);
    cpu.set_register(13, stack.0);

    runtime
        .dispatch(123, 0, &mut cpu, &mut StubServices)
        .unwrap();

    assert_eq!(cpu.register(0), 0);
}

/// Dispatches a DrawTextEx (slot 128) call with the vendor ABI: the rect
/// tail, colour, flag and font ride on the stack behind r0-r3.
fn dispatch_draw_text_ex(
    runtime: &mut ExtRuntime,
    text: GuestAddr,
    x: u32,
    y: u32,
    rect_w: u32,
    rect_h: u32,
    flag: u32,
    font: u32,
) -> ArmCpu {
    let stack = runtime.allocate(16, 4).unwrap();
    runtime.memory.write(stack, &[0; 16]).unwrap();
    runtime
        .memory
        .write_u32(stack, rect_w | (rect_h << 16))
        .unwrap();
    // mr_colourSt {r, g, b} packs red into the low byte.
    runtime
        .memory
        .write_u32(stack.checked_add(4).unwrap(), 0xff)
        .unwrap();
    runtime
        .memory
        .write_u32(stack.checked_add(8).unwrap(), flag)
        .unwrap();
    runtime
        .memory
        .write_u32(stack.checked_add(12).unwrap(), font)
        .unwrap();

    let mut cpu = ArmCpu::new();
    cpu.set_register(0, text.0);
    cpu.set_register(1, x);
    cpu.set_register(2, y);
    cpu.set_register(3, 0);
    cpu.set_register(13, stack.0);
    runtime
        .dispatch(128, 0, &mut cpu, &mut StubServices)
        .unwrap();
    cpu
}

#[test]
fn draw_text_ex_renders_unicode_text_and_clips_to_the_rect() {
    let mut runtime =
        ExtRuntime::new(24, 16, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let text = runtime.allocate(6, 2).unwrap();
    runtime
        .memory
        .write(text, &[0x78, 0x6e, 0x5b, 0x9a, 0, 0])
        .unwrap();

    // The clip window is nine pixels wide, so the second nine-pixel glyph
    // overflows: it records its byte offset as the stop position and its
    // pixels stay clipped away.
    let cpu = dispatch_draw_text_ex(&mut runtime, text, 0, 0, 9, 2, 1, 2);
    assert_eq!(cpu.register(0), 2);
    // Row zero of the stub glyph sets columns 7 and 8.
    assert_eq!(
        runtime
            .memory
            .read_u16(runtime.screen_address(7, 0, 24).unwrap())
            .unwrap(),
        0xf800
    );
    // Everything past the clip edge is untouched.
    assert_eq!(
        runtime
            .memory
            .read_u16(runtime.screen_address(9, 0, 24).unwrap())
            .unwrap(),
        0
    );
}

#[test]
fn draw_text_ex_auto_newline_wraps_and_reports_the_wrap_offset() {
    let mut runtime =
        ExtRuntime::new(24, 16, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let text = runtime.allocate(10, 2).unwrap();
    runtime
        .memory
        .write(text, &[0, b'%', 0, b'%', 0, b'%', 0, b'%', 0, 0])
        .unwrap();

    // Each '%' is eight pixels wide; a twenty-pixel clip fits two per line.
    // The third character wraps, so the stop offset is its byte position
    // and the wrapped line starts two pixels below the two-pixel glyphs.
    let cpu = dispatch_draw_text_ex(&mut runtime, text, 0, 0, 20, 6, 3, 1);
    assert_eq!(cpu.register(0), 4);
    // Row zero of '%' sets column zero.
    for (x, y) in [(0, 0), (8, 0), (0, 4), (8, 4)] {
        assert_eq!(
            runtime
                .memory
                .read_u16(runtime.screen_address(x, y, 24).unwrap())
                .unwrap(),
            0xf800,
            "pixel ({x},{y}) should be set"
        );
    }
    // The two-pixel gap row between the wrapped lines stays clean.
    assert_eq!(
        runtime
            .memory
            .read_u16(runtime.screen_address(0, 2, 24).unwrap())
            .unwrap(),
        0
    );
    assert_eq!(
        runtime
            .memory
            .read_u16(runtime.screen_address(16, 0, 24).unwrap())
            .unwrap(),
        0
    );
}

#[test]
fn draw_text_ex_auto_newline_stops_once_lines_leave_the_rect() {
    let mut runtime =
        ExtRuntime::new(24, 16, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let text = runtime.allocate(10, 2).unwrap();
    runtime
        .memory
        .write(text, &[0, b'%', 0, b'%', 0, b'%', 0, b'%', 0, 0])
        .unwrap();

    // A three-pixel-tall rect shows the first line only: the wrapped line
    // lands at y=4, past the clip bottom, so drawing stops there with the
    // wrap offset and no pixel below the rect is touched.
    let cpu = dispatch_draw_text_ex(&mut runtime, text, 0, 0, 20, 3, 3, 1);
    assert_eq!(cpu.register(0), 4);
    assert_eq!(
        runtime
            .memory
            .read_u16(runtime.screen_address(0, 0, 24).unwrap())
            .unwrap(),
        0xf800
    );
    assert_eq!(
        runtime
            .memory
            .read_u16(runtime.screen_address(0, 4, 24).unwrap())
            .unwrap(),
        0
    );
}

#[test]
fn draw_text_ex_decodes_gbk_text_when_the_unicode_flag_is_clear() {
    let mut runtime =
        ExtRuntime::new(24, 16, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let text = runtime.allocate(5, 1).unwrap();
    // GBK for U+786E (确) and U+5B9A (定).
    runtime
        .memory
        .write(text, &[0xc8, 0xb7, 0xb6, 0xa8, 0])
        .unwrap();

    // Without DRAW_TEXT_EX_IS_UNICODE the bytes are GBK; both nine-pixel
    // glyphs fit an eighteen-pixel clip, so the full byte length returns.
    let cpu = dispatch_draw_text_ex(&mut runtime, text, 0, 0, 18, 4, 2, 1);
    assert_eq!(cpu.register(0), 4);
    assert_eq!(
        runtime
            .memory
            .read_u16(runtime.screen_address(7, 0, 24).unwrap())
            .unwrap(),
        0xf800
    );
}

#[test]
fn draw_text_ex_newline_skips_a_line_in_auto_mode() {
    let mut runtime =
        ExtRuntime::new(24, 16, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let text = runtime.allocate(10, 2).unwrap();
    runtime
        .memory
        .write(text, &[0, b'%', 0, b'%', 0, 0x0a, 0, b'%', 0, 0])
        .unwrap();

    // The explicit newline moves the pen to the next line (glyph height
    // plus the two-pixel gap) without drawing anything for itself, and the
    // full string fits, so the whole byte length is the stop offset.
    let cpu = dispatch_draw_text_ex(&mut runtime, text, 0, 0, 20, 8, 3, 1);
    assert_eq!(cpu.register(0), 8);
    for (x, y) in [(0, 0), (8, 0), (0, 4)] {
        assert_eq!(
            runtime
                .memory
                .read_u16(runtime.screen_address(x, y, 24).unwrap())
                .unwrap(),
            0xf800,
            "pixel ({x},{y}) should be set"
        );
    }
    assert_eq!(
        runtime
            .memory
            .read_u16(runtime.screen_address(0, 2, 24).unwrap())
            .unwrap(),
        0
    );
}

#[test]
fn draw_text_ex_answers_zero_for_a_null_text_pointer() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();

    let cpu = dispatch_draw_text_ex(&mut runtime, GuestAddr(0), 0, 0, 20, 8, 3, 1);
    assert_eq!(cpu.register(0), 0);
}

#[test]
fn legacy_string_conversion_returns_module_owned_ucs2_be() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    load_test_module(&mut runtime);
    let input = runtime.allocate(6, 1).unwrap();
    let error_output = runtime.allocate(4, 4).unwrap();
    let size_output = runtime.allocate(4, 4).unwrap();
    runtime
        .memory
        .write(input, &[b'A', 0xd6, 0xd0, 0x80, 0, 0])
        .unwrap();
    runtime.memory.write_u32(error_output, 0).unwrap();
    runtime.memory.write_u32(size_output, 0).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, input.0);
    cpu.set_register(1, error_output.0);
    cpu.set_register(2, size_output.0);

    runtime
        .dispatch(132, 0, &mut cpu, &mut StubServices)
        .unwrap();

    let output = GuestAddr(cpu.register(0));
    assert_ne!(output.0, 0);
    assert_eq!(runtime.memory.read_u32(error_output).unwrap(), u32::MAX);
    assert_eq!(runtime.memory.read_u32(size_output).unwrap(), 8);
    assert_eq!(
        runtime.memory.read(output, 8).unwrap(),
        [0x00, 0x41, 0x4e, 0x2d, 0x20, 0xac, 0x00, 0x00]
    );
    assert_eq!(
        runtime.guest_allocation_owners.get(&output.0),
        Some(&runtime.modules[0].generation)
    );
}

#[test]
fn legacy_string_conversion_handles_empty_invalid_and_optional_outputs() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    load_test_module(&mut runtime);
    let input = runtime.allocate(2, 1).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, input.0);

    runtime.memory.write(input, &[0, 0]).unwrap();
    runtime
        .dispatch(132, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(
        runtime.memory.read(GuestAddr(cpu.register(0)), 2).unwrap(),
        [0, 0]
    );

    runtime.memory.write(input, &[0x81, 0]).unwrap();
    cpu.set_register(0, input.0);
    runtime
        .dispatch(132, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(
        runtime.memory.read(GuestAddr(cpu.register(0)), 4).unwrap(),
        [0xff, 0xfd, 0, 0]
    );

    cpu.set_register(0, 0);
    runtime
        .dispatch(132, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 0);
}

#[test]
fn buffered_ucs2_conversion_uses_the_caller_owned_legacy_output() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let input = runtime.allocate(4, 2).unwrap();
    runtime
        .memory
        .write(input, &[0x00, b'A', 0x4e, 0x2d])
        .unwrap();
    let destination = runtime.allocate(16, 2).unwrap();
    let output_field = runtime.allocate(4, 4).unwrap();
    runtime
        .memory
        .write_u32(output_field, destination.0)
        .unwrap();
    let output_len = runtime.allocate(4, 4).unwrap();
    let stack = runtime.allocate(4, 4).unwrap();
    runtime.memory.write_u32(stack, output_len.0).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 1_207);
    cpu.set_register(1, input.0);
    cpu.set_register(2, 4);
    cpu.set_register(3, output_field.0);
    cpu.set_register(13, stack.0);

    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();

    assert_eq!(cpu.register(0), 0);
    assert_eq!(
        runtime.memory.read_u32(output_field).unwrap(),
        destination.0
    );
    assert_eq!(runtime.memory.read_u32(output_len).unwrap(), 3);
    assert_eq!(
        runtime.memory.read(destination, 4).unwrap(),
        [0x41, 0xd6, 0xd0, 0x00]
    );

    runtime.memory.write_u32(output_len, 99).unwrap();
    cpu.set_register(0, 1_207);
    cpu.set_register(2, 3);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), u32::MAX);
    assert_eq!(runtime.memory.read_u32(output_len).unwrap(), 0);

    runtime.memory.write_u32(output_field, 0).unwrap();
    runtime.memory.write_u32(output_len, 99).unwrap();
    cpu.set_register(0, 1_207);
    cpu.set_register(2, 4);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), u32::MAX);
    assert_eq!(runtime.memory.read_u32(output_len).unwrap(), 0);
}

#[test]
fn md5_slots_support_incremental_and_cross_block_inputs() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let context = runtime.allocate(88, 4).unwrap();
    let digest = runtime.allocate(16, 4).unwrap();
    let input = runtime.allocate(80, 4).unwrap();
    let bytes = (0_u8..80).collect::<Vec<_>>();
    runtime.memory.write(input, &bytes).unwrap();
    let mut cpu = ArmCpu::new();

    cpu.set_register(0, context.0);
    runtime
        .dispatch(113, 0, &mut cpu, &mut StubServices)
        .unwrap();
    cpu.set_register(0, context.0);
    cpu.set_register(1, input.0);
    cpu.set_register(2, 17);
    runtime
        .dispatch(114, 0, &mut cpu, &mut StubServices)
        .unwrap();
    cpu.set_register(0, context.0);
    cpu.set_register(1, input.checked_add(17).unwrap().0);
    cpu.set_register(2, 63);
    runtime
        .dispatch(114, 0, &mut cpu, &mut StubServices)
        .unwrap();
    cpu.set_register(0, context.0);
    cpu.set_register(1, digest.0);
    runtime
        .dispatch(115, 0, &mut cpu, &mut StubServices)
        .unwrap();
    let incremental = runtime.memory.read(digest, 16).unwrap();

    cpu.set_register(0, context.0);
    runtime
        .dispatch(113, 0, &mut cpu, &mut StubServices)
        .unwrap();
    cpu.set_register(0, context.0);
    cpu.set_register(1, input.0);
    cpu.set_register(2, 80);
    runtime
        .dispatch(114, 0, &mut cpu, &mut StubServices)
        .unwrap();
    cpu.set_register(0, context.0);
    cpu.set_register(1, digest.0);
    runtime
        .dispatch(115, 0, &mut cpu, &mut StubServices)
        .unwrap();

    assert_eq!(runtime.memory.read(digest, 16).unwrap(), incremental);

    cpu.set_register(0, context.0);
    runtime
        .dispatch(113, 0, &mut cpu, &mut StubServices)
        .unwrap();
    cpu.set_register(0, context.0);
    cpu.set_register(1, digest.0);
    runtime
        .dispatch(115, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(
        runtime.memory.read(digest, 16).unwrap(),
        [
            0xd4, 0x1d, 0x8c, 0xd9, 0x8f, 0x00, 0xb2, 0x04, 0xe9, 0x80, 0x09, 0x98, 0xec, 0xf8,
            0x42, 0x7e,
        ]
    );
}

#[test]
fn platform_memory_extension_returns_a_zeroed_guest_arena() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    load_test_module(&mut runtime);
    let output = runtime.allocate(4, 4).unwrap();
    let output_len = runtime.allocate(4, 4).unwrap();
    let stack = runtime.allocate(4, 4).unwrap();
    runtime.memory.write_u32(stack, output_len.0).unwrap();
    let free_before = runtime.memory.read_u32(data_slot_address(111)).unwrap();

    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 1_014);
    cpu.set_register(2, 32);
    cpu.set_register(3, output.0);
    cpu.set_register(13, stack.0);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();

    let arena = GuestAddr(runtime.memory.read_u32(output).unwrap());
    assert_eq!(cpu.register(0), 0);
    assert_eq!(arena, PLATFORM_MEMORY_BASE);
    assert_eq!(runtime.memory.read_u32(output_len).unwrap(), 32);
    assert_eq!(runtime.memory.read(arena, 32).unwrap(), vec![0; 32]);

    runtime.memory.write_u32(arena, 0xaaaa_aaaa).unwrap();
    cpu.set_register(0, 1_015);
    cpu.set_register(1, arena.0);
    cpu.set_register(2, 4);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();

    assert_eq!(cpu.register(0), 0);
    assert_eq!(
        runtime.memory.read_u32(data_slot_address(111)).unwrap(),
        free_before
    );
    assert!(runtime.memory.read(arena, 32).is_err());
    cpu.set_register(0, 1_015);
    assert!(matches!(
        runtime.dispatch(38, 0, &mut cpu, &mut StubServices),
        Err(Error::Abi(message)) if message.contains("unknown arena")
    ));
}

#[test]
fn platform_memory_extension_treats_nonpositive_probe_as_already_satisfied() {
    for requested_len in [0, 0xfffe_d800] {
        let mut runtime =
            ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
        load_test_module(&mut runtime);
        let output = runtime.allocate(4, 4).unwrap();
        let output_len = runtime.allocate(4, 4).unwrap();
        let stack = runtime.allocate(4, 4).unwrap();
        runtime.memory.write_u32(output, 0xaaaa_aaaa).unwrap();
        runtime.memory.write_u32(output_len, 0xbbbb_bbbb).unwrap();
        runtime.memory.write_u32(stack, output_len.0).unwrap();
        let cursor_before = runtime.platform_memory_cursor;

        let mut cpu = ArmCpu::new();
        cpu.set_register(0, 1_014);
        cpu.set_register(2, requested_len);
        cpu.set_register(3, output.0);
        cpu.set_register(13, stack.0);
        runtime
            .dispatch(38, 0, &mut cpu, &mut StubServices)
            .unwrap();

        let arena = GuestAddr(runtime.memory.read_u32(output).unwrap());
        assert_eq!(cpu.register(0), 0);
        assert_eq!(arena, PLATFORM_MEMORY_BASE);
        assert_eq!(runtime.memory.read_u32(output_len).unwrap(), 0);
        assert_eq!(runtime.memory.read(arena, 1).unwrap(), [0]);

        cpu.set_register(0, 1_015);
        cpu.set_register(1, arena.0);
        cpu.set_register(2, 4);
        runtime
            .dispatch(38, 0, &mut cpu, &mut StubServices)
            .unwrap();
        assert_eq!(cpu.register(0), 0);
        assert_eq!(runtime.platform_memory_cursor, cursor_before);
        assert!(runtime.memory.read(arena, 1).is_err());
    }
}

#[test]
fn platform_memory_extension_rejects_oversized_positive_requests_without_mutating_state() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    load_test_module(&mut runtime);
    let output = runtime.allocate(4, 4).unwrap();
    let output_len = runtime.allocate(4, 4).unwrap();
    let stack = runtime.allocate(4, 4).unwrap();
    runtime.memory.write_u32(output, 0xaaaa_aaaa).unwrap();
    runtime.memory.write_u32(output_len, 0xbbbb_bbbb).unwrap();
    runtime.memory.write_u32(stack, output_len.0).unwrap();
    let cursor_before = runtime.platform_memory_cursor;

    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 1_014);
    cpu.set_register(2, MAX_PLATFORM_MEMORY_EXTENSION_LEN as u32 + 1);
    cpu.set_register(3, output.0);
    cpu.set_register(13, stack.0);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();

    assert_eq!(cpu.register(0), u32::MAX);
    assert_eq!(runtime.memory.read_u32(output).unwrap(), 0);
    assert_eq!(runtime.memory.read_u32(output_len).unwrap(), 0);
    assert_eq!(runtime.platform_memory_cursor, cursor_before);
    assert!(runtime.platform_memory_extensions.is_empty());
}

#[test]
fn platform_memory_extension_rejects_an_exhausted_address_window() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    load_test_module(&mut runtime);
    let output = runtime.allocate(4, 4).unwrap();
    let output_len = runtime.allocate(4, 4).unwrap();
    let stack = runtime.allocate(4, 4).unwrap();
    runtime.memory.write_u32(output, 0xaaaa_aaaa).unwrap();
    runtime.memory.write_u32(output_len, 0xbbbb_bbbb).unwrap();
    runtime.memory.write_u32(stack, output_len.0).unwrap();
    runtime.platform_memory_cursor = DETACHED_GUEST_ALLOCATION_BASE.0 - 0x1000;
    let cursor_before = runtime.platform_memory_cursor;

    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 1_014);
    cpu.set_register(2, 0x1001);
    cpu.set_register(3, output.0);
    cpu.set_register(13, stack.0);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();

    assert_eq!(cpu.register(0), u32::MAX);
    assert_eq!(runtime.memory.read_u32(output).unwrap(), 0);
    assert_eq!(runtime.memory.read_u32(output_len).unwrap(), 0);
    assert_eq!(runtime.platform_memory_cursor, cursor_before);
    assert!(runtime.platform_memory_extensions.is_empty());
    assert!(!runtime.memory.is_mapped(GuestAddr(cursor_before), 1));
}

#[test]
fn platform_memory_extension_accepts_the_address_window_boundary() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    load_test_module(&mut runtime);
    let output = runtime.allocate(4, 4).unwrap();
    let output_len = runtime.allocate(4, 4).unwrap();
    let stack = runtime.allocate(4, 4).unwrap();
    runtime.memory.write_u32(stack, output_len.0).unwrap();
    runtime.platform_memory_cursor = DETACHED_GUEST_ALLOCATION_BASE.0 - 0x1000;
    let cursor_before = runtime.platform_memory_cursor;

    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 1_014);
    cpu.set_register(2, 0x1000);
    cpu.set_register(3, output.0);
    cpu.set_register(13, stack.0);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();

    let arena = GuestAddr(runtime.memory.read_u32(output).unwrap());
    assert_eq!(cpu.register(0), 0);
    assert_eq!(arena, GuestAddr(cursor_before));
    assert_eq!(runtime.memory.read_u32(output_len).unwrap(), 0x1000);
    assert_eq!(
        runtime.platform_memory_cursor,
        DETACHED_GUEST_ALLOCATION_BASE.0
    );
    assert_eq!(runtime.memory.read(arena, 0x1000).unwrap(), vec![0; 0x1000]);

    cpu.set_register(0, 1_015);
    cpu.set_register(1, arena.0);
    cpu.set_register(2, 4);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(runtime.platform_memory_cursor, cursor_before);
}

#[test]
fn platform_memory_extension_validates_outputs_before_mutating_state() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    load_test_module(&mut runtime);
    let output = runtime.allocate(4, 4).unwrap();
    let output_len = runtime.allocate(4, 4).unwrap();
    let stack = runtime.allocate(4, 4).unwrap();
    runtime.memory.write_u32(output, 0xaaaa_aaaa).unwrap();
    runtime.memory.write_u32(output_len, 0xbbbb_bbbb).unwrap();
    runtime.memory.write_u32(stack, output_len.0).unwrap();
    runtime
        .memory
        .remove_permissions(output_len, 4, Permissions::WRITE)
        .unwrap();
    let cursor_before = runtime.platform_memory_cursor;

    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 1_014);
    cpu.set_register(2, 32);
    cpu.set_register(3, output.0);
    cpu.set_register(13, stack.0);
    assert!(matches!(
        runtime.dispatch(38, 0, &mut cpu, &mut StubServices),
        Err(Error::ArmFault(_))
    ));

    assert_eq!(runtime.memory.read_u32(output).unwrap(), 0xaaaa_aaaa);
    assert_eq!(runtime.memory.read_u32(output_len).unwrap(), 0xbbbb_bbbb);
    assert_eq!(runtime.platform_memory_cursor, cursor_before);
    assert!(runtime.platform_memory_extensions.is_empty());
    assert!(!runtime.memory.is_mapped(PLATFORM_MEMORY_BASE, 1));
}

#[test]
fn module_cannot_register_another_modules_platform_arena_as_executable() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    load_test_module(&mut runtime);
    load_test_module(&mut runtime);
    let output = runtime.allocate(4, 4).unwrap();
    let output_len = runtime.allocate(4, 4).unwrap();
    let stack = runtime.allocate(4, 4).unwrap();
    runtime.memory.write_u32(stack, output_len.0).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 1_014);
    cpu.set_register(2, 32);
    cpu.set_register(3, output.0);
    cpu.set_register(13, stack.0);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();
    let arena = GuestAddr(runtime.memory.read_u32(output).unwrap());
    runtime.memory.write(arena, &[0x70, 0x47]).unwrap();

    cpu.set_register(0, 0);
    cpu.set_register(1, 9);
    cpu.set_register(2, arena.0);
    cpu.set_register(3, 2);
    assert!(matches!(
        runtime.dispatch(131, 1, &mut cpu, &mut StubServices),
        Err(Error::Abi(message)) if message.contains("belongs to another module")
    ));
    assert!(runtime.modules[1].dynamic_executable_ranges.is_empty());
    assert!(runtime.memory.fetch_u16(arena).is_err());
}

#[test]
fn module_merges_overlapping_dynamic_ranges_inside_one_owned_allocation() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    load_test_module(&mut runtime);
    let output = runtime.allocate(4, 4).unwrap();
    let output_len = runtime.allocate(4, 4).unwrap();
    let stack = runtime.allocate(4, 4).unwrap();
    runtime.memory.write_u32(stack, output_len.0).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 1_014);
    cpu.set_register(2, 32);
    cpu.set_register(3, output.0);
    cpu.set_register(13, stack.0);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();
    let arena = GuestAddr(runtime.memory.read_u32(output).unwrap());

    cpu.set_register(0, 0);
    cpu.set_register(1, 9);
    cpu.set_register(2, arena.0 + 4);
    cpu.set_register(3, 16);
    runtime
        .dispatch(131, 0, &mut cpu, &mut StubServices)
        .unwrap();

    cpu.set_register(0, 0);
    cpu.set_register(2, arena.0 + 8);
    cpu.set_register(3, 8);
    runtime
        .dispatch(131, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), 0);
    assert_eq!(
        runtime.modules[0].dynamic_executable_ranges,
        [Some(ExecutableRange {
            base: GuestAddr(arena.0 + 4),
            len: 16,
        })]
    );

    cpu.set_register(0, 0);
    cpu.set_register(2, arena.0 + 16);
    cpu.set_register(3, 8);
    runtime
        .dispatch(131, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(
        runtime.modules[0].dynamic_executable_ranges,
        [Some(ExecutableRange {
            base: GuestAddr(arena.0 + 4),
            len: 20,
        })]
    );

    cpu.set_register(0, 0);
    cpu.set_register(2, arena.0);
    cpu.set_register(3, 8);
    runtime
        .dispatch(131, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(
        runtime.modules[0].dynamic_executable_ranges,
        [Some(ExecutableRange {
            base: arena,
            len: 24,
        })]
    );
}

#[test]
fn module_preserves_existing_image_identities_when_registration_bridges_them() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    load_test_module(&mut runtime);
    let allocation = runtime
        .allocate_guest_block_for_module(64, 0)
        .unwrap()
        .unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 0);
    cpu.set_register(1, 9);

    for (offset, len) in [(0, 8), (16, 8)] {
        cpu.set_register(2, allocation.0 + offset);
        cpu.set_register(3, len);
        runtime
            .dispatch(131, 0, &mut cpu, &mut StubServices)
            .unwrap();
    }
    assert_eq!(
        runtime.modules[0].executable_image(allocation.0),
        Some((ExecutableImage::Dynamic(0), 0))
    );
    assert_eq!(
        runtime.modules[0].executable_image(allocation.0 + 16),
        Some((ExecutableImage::Dynamic(1), 0))
    );

    cpu.set_register(2, allocation.0);
    cpu.set_register(3, 24);
    runtime
        .dispatch(131, 0, &mut cpu, &mut StubServices)
        .unwrap();

    assert_eq!(
        runtime.modules[0].executable_image(allocation.0),
        Some((ExecutableImage::Dynamic(0), 0))
    );
    assert_eq!(
        runtime.modules[0].executable_image(allocation.0 + 8),
        Some((ExecutableImage::Dynamic(2), 0))
    );
    assert_eq!(
        runtime.modules[0].executable_image(allocation.0 + 16),
        Some((ExecutableImage::Dynamic(1), 0))
    );
    assert_eq!(
        runtime
            .memory
            .fetch_u32(allocation.checked_add(8).unwrap())
            .unwrap(),
        0
    );
}

// ---- mr_platEx MIDI media device (ACI device 1) ----

fn dispatch_midi_media(
    runtime: &mut ExtRuntime,
    command: u32,
    input: Option<(GuestAddr, u32)>,
) -> ArmCpu {
    let stack = runtime.allocate(8, 4).unwrap();
    runtime.memory.write(stack, &[0; 8]).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, command);
    if let Some((input, input_len)) = input {
        cpu.set_register(1, input.0);
        cpu.set_register(2, input_len);
    }
    cpu.set_register(13, stack.0);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();
    cpu
}

fn load_midi_path(runtime: &mut ExtRuntime, path: &[u8]) -> ArmCpu {
    let path_address = runtime.allocate(path.len(), 1).unwrap();
    runtime.memory.write(path_address, path).unwrap();
    dispatch_midi_media(runtime, 2_021, Some((path_address, path.len() as u32)))
}

#[test]
fn platform_command_2011_releases_the_midi_device_state() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    load_midi_path(&mut runtime, b"media/bgm.mid");
    dispatch_midi_media(&mut runtime, 2_041, None);

    // MR_MEDIA_INIT releases the loaded resource and moves the device to
    // the INITED state, like the C++ INIT transition.
    let cpu = dispatch_midi_media(&mut runtime, 2_011, None);
    assert_eq!(cpu.register(0), 0);
    STUB_SOUND.with(|sound| assert!(sound.borrow().is_none()));
    let cpu = dispatch_midi_media(&mut runtime, 2_091, None);
    assert_eq!(cpu.register(0), MEDIA_STATUS_INITED);
    // The resource is gone: a subsequent play fails.
    let cpu = dispatch_midi_media(&mut runtime, 2_041, None);
    assert_eq!(cpu.register(0), u32::MAX);
}

#[test]
fn platform_command_2021_loads_a_named_midi_resource_without_playing() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    STUB_SOUND.with(|sound| *sound.borrow_mut() = None);

    let cpu = load_midi_path(&mut runtime, b"media/bgm.mid");
    assert_eq!(cpu.register(0), 0);
    // FILE_LOAD only stores the resource; playback starts at the PLAY
    // request (dsm.c dsm_media_file_load keeps the data until play).
    STUB_SOUND.with(|sound| assert!(sound.borrow().is_none()));
    let cpu = dispatch_midi_media(&mut runtime, 2_091, None);
    assert_eq!(cpu.register(0), MEDIA_STATUS_LOADED);
}

#[test]
fn platform_command_2021_falls_back_to_the_package_and_preserves_state_on_failure() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();

    // Extensions are matched case-insensitively.
    let cpu = load_midi_path(&mut runtime, b"media/UPPER.MID");
    assert_eq!(cpu.register(0), 0);

    // A packaged resource backs names the sound-file directory lacks.
    let cpu = load_midi_path(&mut runtime, b"packaged.mid");
    assert_eq!(cpu.register(0), 0);
    let cpu = dispatch_midi_media(&mut runtime, 2_041, None);
    assert_eq!(cpu.register(0), 0);
    STUB_SOUND.with(|sound| {
        let (sound_type, data, looped) = sound.borrow().clone().unwrap();
        assert_eq!(sound_type, SoundType::Midi);
        assert_eq!(data, b"MThd-packaged".to_vec());
        assert!(!looped);
    });

    // A missing resource fails the load but keeps the previous media, like
    // the C++ loader.
    let cpu = load_midi_path(&mut runtime, b"media/missing.mid");
    assert_eq!(cpu.register(0), u32::MAX);
    let cpu = dispatch_midi_media(&mut runtime, 2_091, None);
    assert_eq!(cpu.register(0), MEDIA_STATUS_PLAYING);
}

#[test]
fn platform_command_2021_rejects_unverified_request_shapes() {
    let invalid_paths = [
        b"media/bgm.wav".as_slice(),
        b"media/bgm.mid\0suffix",
        b".mid",
        b"../bgm.mid",
        b"media//bgm.mid",
    ];
    for invalid_path in invalid_paths {
        let mut runtime =
            ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
        let stack = runtime.allocate(8, 4).unwrap();
        runtime.memory.write(stack, &[0; 8]).unwrap();
        let path = runtime.allocate(invalid_path.len(), 1).unwrap();
        runtime.memory.write(path, invalid_path).unwrap();
        let mut cpu = ArmCpu::new();
        cpu.set_register(0, 2_021);
        cpu.set_register(1, path.0);
        cpu.set_register(2, invalid_path.len() as u32);
        cpu.set_register(13, stack.0);
        assert!(matches!(
            runtime.dispatch(38, 0, &mut cpu, &mut StubServices),
            Err(Error::Abi(message)) if message.contains("platform media file-load")
        ));
    }

    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let stack = runtime.allocate(8, 4).unwrap();
    runtime.memory.write(stack, &[0; 8]).unwrap();
    let path = runtime.allocate(4_097, 1).unwrap();
    runtime.memory.write(path, &vec![b'a'; 4_097]).unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 2_021);
    cpu.set_register(1, path.0);
    cpu.set_register(2, 4_097);
    cpu.set_register(13, stack.0);
    assert!(matches!(
        runtime.dispatch(38, 0, &mut cpu, &mut StubServices),
        Err(Error::Abi(message)) if message.contains("platform media file-load request")
    ));

    cpu.set_register(0, 2_021);
    cpu.set_register(2, 14);
    cpu.set_register(3, 1);
    assert!(matches!(
        runtime.dispatch(38, 0, &mut cpu, &mut StubServices),
        Err(Error::Abi(message)) if message.contains("platform media file-load request")
    ));
}

#[test]
fn platform_command_2031_adopts_the_caller_supplied_buffer() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let buffer = b"MThd-inline".to_vec();
    let input = runtime.allocate(buffer.len(), 1).unwrap();
    runtime.memory.write(input, &buffer).unwrap();

    let cpu = dispatch_midi_media(&mut runtime, 2_031, Some((input, buffer.len() as u32)));
    assert_eq!(cpu.register(0), 0);

    let cpu = dispatch_midi_media(&mut runtime, 2_041, None);
    assert_eq!(cpu.register(0), 0);
    STUB_SOUND.with(|sound| {
        assert_eq!(
            sound.borrow().as_ref().unwrap().1,
            b"MThd-inline".to_vec()
        )
    });

    // Unverified buffer shapes fail loudly.
    let stack = runtime.allocate(8, 4).unwrap();
    runtime.memory.write(stack, &[0; 8]).unwrap();
    let mut shape_cpu = ArmCpu::new();
    shape_cpu.set_register(0, 2_031);
    shape_cpu.set_register(13, stack.0);
    assert!(matches!(
        runtime.dispatch(38, 0, &mut shape_cpu, &mut StubServices),
        Err(Error::Abi(message)) if message.contains("platform media buffer-load request shape")
    ));
}

#[test]
fn platform_command_2041_plays_the_loaded_resource_with_the_loop_flag() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    STUB_SOUND.with(|sound| *sound.borrow_mut() = None);

    // Playing with nothing loaded fails but leaves the device idle.
    let cpu = dispatch_midi_media(&mut runtime, 2_041, None);
    assert_eq!(cpu.register(0), u32::MAX);
    STUB_SOUND.with(|sound| assert!(sound.borrow().is_none()));

    load_midi_path(&mut runtime, b"media/bgm.mid");

    // T_DSM_MEDIA_PLAY is {cb, loop, block}; a full twelve-byte record with
    // the loop flag set plays the BGM repeatedly.
    let play = runtime.allocate(12, 4).unwrap();
    runtime.memory.write(play, &[0; 12]).unwrap();
    runtime
        .memory
        .write_u32(play.checked_add(4).unwrap(), 1)
        .unwrap();
    let cpu = dispatch_midi_media(&mut runtime, 2_041, Some((play, 12)));
    assert_eq!(cpu.register(0), 0);
    STUB_SOUND.with(|sound| {
        let (sound_type, data, looped) = sound.borrow().clone().unwrap();
        assert_eq!(sound_type, SoundType::Midi);
        assert_eq!(data, b"MThd-soundfile".to_vec());
        assert!(looped);
    });
    let cpu = dispatch_midi_media(&mut runtime, 2_091, None);
    assert_eq!(cpu.register(0), MEDIA_STATUS_PLAYING);

    // A zeroed record restarts playback unlooped.
    runtime.memory.write(play, &[0; 12]).unwrap();
    let cpu = dispatch_midi_media(&mut runtime, 2_041, Some((play, 12)));
    assert_eq!(cpu.register(0), 0);
    STUB_SOUND.with(|sound| assert!(!sound.borrow().as_ref().unwrap().2));

    // RESUME shares the play path.
    let cpu = dispatch_midi_media(&mut runtime, 2_061, None);
    assert_eq!(cpu.register(0), 0);
    STUB_SOUND.with(|sound| assert!(sound.borrow().is_some()));
}

#[test]
fn platform_command_2051_2071_stop_playback_and_keep_the_resource() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    load_midi_path(&mut runtime, b"media/bgm.mid");
    dispatch_midi_media(&mut runtime, 2_041, None);

    for command in [2_051, 2_071] {
        let cpu = dispatch_midi_media(&mut runtime, command, None);
        assert_eq!(cpu.register(0), 0);
        STUB_SOUND.with(|sound| assert!(sound.borrow().is_none()));
        let cpu = dispatch_midi_media(&mut runtime, 2_091, None);
        assert_eq!(cpu.register(0), MEDIA_STATUS_LOADED);
    }

    // The stored resource survives a stop and can play again.
    let cpu = dispatch_midi_media(&mut runtime, 2_041, None);
    assert_eq!(cpu.register(0), 0);
    STUB_SOUND.with(|sound| assert!(sound.borrow().is_some()));

    // Pausing with nothing playing still succeeds, like the C++ slot.
    dispatch_midi_media(&mut runtime, 2_071, None);
    let cpu = dispatch_midi_media(&mut runtime, 2_051, None);
    assert_eq!(cpu.register(0), 0);
}

#[test]
fn platform_command_2081_2161_release_the_loaded_resource() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    load_midi_path(&mut runtime, b"media/bgm.mid");
    dispatch_midi_media(&mut runtime, 2_041, None);

    for command in [2_081, 2_161] {
        let cpu = dispatch_midi_media(&mut runtime, command, None);
        assert_eq!(cpu.register(0), 0);
        STUB_SOUND.with(|sound| assert!(sound.borrow().is_none()));
        let cpu = dispatch_midi_media(&mut runtime, 2_091, None);
        assert_eq!(cpu.register(0), MEDIA_STATUS_IDLE);
        // The resource is gone: a subsequent play fails.
        let cpu = dispatch_midi_media(&mut runtime, 2_041, None);
        assert_eq!(cpu.register(0), u32::MAX);
        STUB_SOUND.with(|sound| assert!(sound.borrow().is_none()));
        // Reload so the second close also has something to release.
        load_midi_path(&mut runtime, b"media/bgm.mid");
    }
}

#[test]
fn platform_command_2091_reports_status_through_both_channels() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    load_test_module(&mut runtime);

    // With no output cells the status travels in the return value alone.
    let cpu = dispatch_midi_media(&mut runtime, 2_091, None);
    assert_eq!(cpu.register(0), MEDIA_STATUS_IDLE);

    // With output cells the status is also copied into a fresh guest block
    // whose pointer and length are stored back into the caller's cells.
    let output = runtime.allocate(4, 4).unwrap();
    let output_len = runtime.allocate(4, 4).unwrap();
    let stack = runtime.allocate(8, 4).unwrap();
    runtime.memory.write_u32(stack, output_len.0).unwrap();
    runtime
        .memory
        .write_u32(stack.checked_add(4).unwrap(), 0xfeed_face)
        .unwrap();
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 2_091);
    cpu.set_register(3, output.0);
    cpu.set_register(13, stack.0);
    runtime
        .dispatch(38, 0, &mut cpu, &mut StubServices)
        .unwrap();
    assert_eq!(cpu.register(0), MEDIA_STATUS_IDLE);
    let cell = GuestAddr(runtime.memory.read_u32(output).unwrap());
    assert_ne!(cell.0, 0);
    assert_eq!(
        runtime.memory.read_u32(cell).unwrap(),
        MEDIA_STATUS_IDLE
    );
    assert_eq!(runtime.memory.read_u32(output_len).unwrap(), 4);
    assert_eq!(
        runtime
            .memory
            .read_u32(stack.checked_add(4).unwrap())
            .unwrap(),
        0xfeed_face
    );

    // A null output pointer with a live length cell (or the reverse) is an
    // ABI violation.
    cpu.set_register(0, 2_091);
    cpu.set_register(3, output.0);
    runtime.memory.write_u32(stack, 0).unwrap();
    assert!(matches!(
        runtime.dispatch(38, 0, &mut cpu, &mut StubServices),
        Err(Error::Abi(message)) if message.contains("null output-length")
    ));
    cpu.set_register(3, 0);
    runtime.memory.write_u32(stack, output_len.0).unwrap();
    assert!(matches!(
        runtime.dispatch(38, 0, &mut cpu, &mut StubServices),
        Err(Error::Abi(message)) if message.contains("null output")
    ));
}

#[test]
fn platform_midi_time_queries_report_a_zero_position() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    load_test_module(&mut runtime);
    load_midi_path(&mut runtime, b"media/bgm.mid");

    // Both channels zero: the zero position rides in the return value.
    for command in [2_101, 2_111, 2_121, 2_131, 2_151] {
        let cpu = dispatch_midi_media(&mut runtime, command, None);
        assert_eq!(cpu.register(0), 0);
    }

    // Output cells receive a fresh block holding a zeroed T_MEDIA_TIME.
    let output = runtime.allocate(4, 4).unwrap();
    let output_len = runtime.allocate(4, 4).unwrap();
    for command in [2_101, 2_111, 2_121, 2_131, 2_151] {
        let stack = runtime.allocate(8, 4).unwrap();
        runtime.memory.write_u32(stack, output_len.0).unwrap();
        let mut cpu = ArmCpu::new();
        cpu.set_register(0, command);
        cpu.set_register(3, output.0);
        cpu.set_register(13, stack.0);
        runtime
            .dispatch(38, 0, &mut cpu, &mut StubServices)
            .unwrap();
        assert_eq!(cpu.register(0), 0);
        let cell = GuestAddr(runtime.memory.read_u32(output).unwrap());
        assert_ne!(cell.0, 0);
        assert_eq!(runtime.memory.read_u32(cell).unwrap(), 0);
        assert_eq!(runtime.memory.read_u32(output_len).unwrap(), 4);
    }
}

#[test]
fn platform_media_time_queries_report_the_live_playback() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    load_test_module(&mut runtime);
    let _reset = set_stub_playback_times(65_000, 185_000);

    // 单位按 platEx 语义：212/213 为秒（212 的秒单位由 mpmusic game.ext
    // 消费方证据确定），215 为毫秒；字节位置的 210/211 保持零值。
    let cases: [(u32, u32); 8] = [
        (2_103, 0),       // MP3 SETPOS: byte position not tracked
        (2_113, 0),       // MP3 GETTIME: byte position not tracked
        (2_123, 185),     // MP3 GET_TOTAL_TIME: 185 s
        (2_133, 65),      // MP3 GET_CURTIME: 65 s
        (2_153, 65_000),  // MP3 GET_CURTIME_MSEC: 65_000 ms
        (2_121, 185),     // MIDI GET_TOTAL_TIME: 185 s
        (2_131, 65),      // MIDI GET_CURTIME: 65 s
        (2_151, 65_000),  // MIDI GET_CURTIME_MSEC: 65_000 ms
    ];
    let output = runtime.allocate(4, 4).unwrap();
    let output_len = runtime.allocate(4, 4).unwrap();
    for (command, expected) in cases {
        let stack = runtime.allocate(8, 4).unwrap();
        runtime.memory.write_u32(stack, output_len.0).unwrap();
        let mut cpu = ArmCpu::new();
        cpu.set_register(0, command);
        cpu.set_register(3, output.0);
        cpu.set_register(13, stack.0);
        runtime
            .dispatch(38, 0, &mut cpu, &mut StubServices)
            .unwrap();
        assert_eq!(cpu.register(0), 0);
        let cell = GuestAddr(runtime.memory.read_u32(output).unwrap());
        assert_ne!(cell.0, 0);
        assert_eq!(runtime.memory.read_u32(cell).unwrap(), expected);
        assert_eq!(runtime.memory.read_u32(output_len).unwrap(), 4);
    }
}

#[test]
fn platform_midi_media_commands_reject_unverified_shapes() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let stack = runtime.allocate(8, 4).unwrap();
    runtime.memory.write(stack, &[0; 8]).unwrap();

    // A play request with a nonzero output pointer is not a verified shape.
    let mut cpu = ArmCpu::new();
    cpu.set_register(0, 2_041);
    cpu.set_register(3, 1);
    cpu.set_register(13, stack.0);
    assert!(matches!(
        runtime.dispatch(38, 0, &mut cpu, &mut StubServices),
        Err(Error::Abi(message)) if message.contains("platform media play request shape")
    ));

    // Stop/close requests carrying input bytes are not verified shapes
    // either.
    for command in [2_051, 2_071, 2_081, 2_161] {
        cpu.set_register(0, command);
        cpu.set_register(3, 0);
        cpu.set_register(1, 1);
        assert!(matches!(
            runtime.dispatch(38, 0, &mut cpu, &mut StubServices),
            Err(Error::Abi(message)) if message.contains("request shape")
        ));
        cpu.set_register(1, 0);
    }
}

#[test]
fn platform_command_2014_initializes_the_amr_media_device() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();

    // The mpaudio game opens with MR_MEDIA_INIT on the AMR device (2014);
    // the C++ host initializes any of the seven media devices the same way.
    let cpu = dispatch_midi_media(&mut runtime, 2_014, None);
    assert_eq!(cpu.register(0), 0);
    let cpu = dispatch_midi_media(&mut runtime, 2_094, None);
    assert_eq!(cpu.register(0), MEDIA_STATUS_INITED);
}

#[test]
fn platform_command_2024_loads_and_plays_an_amr_resource() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    STUB_SOUND.with(|sound| *sound.borrow_mut() = None);

    dispatch_midi_media(&mut runtime, 2_014, None);
    // Only the MIDI device verifies the resource extension; the AMR device
    // accepts any sanitized path like the C++ loader.
    let path = b"media/intro.amr";
    let path_address = runtime.allocate(path.len(), 1).unwrap();
    runtime.memory.write(path_address, path).unwrap();
    let cpu = dispatch_midi_media(
        &mut runtime,
        2_024,
        Some((path_address, path.len() as u32)),
    );
    assert_eq!(cpu.register(0), 0);
    let cpu = dispatch_midi_media(&mut runtime, 2_094, None);
    assert_eq!(cpu.register(0), MEDIA_STATUS_LOADED);

    // The AMR device (4) maps onto SoundType::Amr and hands the file bytes
    // to the unified audio sink; a successful play moves the device to
    // PLAYING like every other media device.
    let cpu = dispatch_midi_media(&mut runtime, 2_044, None);
    assert_eq!(cpu.register(0), 0);
    STUB_SOUND.with(|sound| {
        let (sound_type, data, looped) = sound.borrow().clone().unwrap();
        assert_eq!(sound_type, SoundType::Amr);
        assert_eq!(data, b"#!AMR\n".to_vec());
        assert!(!looped);
    });
    let cpu = dispatch_midi_media(&mut runtime, 2_094, None);
    assert_eq!(cpu.register(0), MEDIA_STATUS_PLAYING);

    // STOP and CLOSE keep their usual contract on the AMR device.
    let cpu = dispatch_midi_media(&mut runtime, 2_074, None);
    assert_eq!(cpu.register(0), 0);
    let cpu = dispatch_midi_media(&mut runtime, 2_084, None);
    assert_eq!(cpu.register(0), 0);
    let cpu = dispatch_midi_media(&mut runtime, 2_094, None);
    assert_eq!(cpu.register(0), MEDIA_STATUS_IDLE);
}

#[test]
fn platform_command_2032_loads_and_plays_a_wav_buffer() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    STUB_SOUND.with(|sound| *sound.borrow_mut() = None);

    let buffer = b"RIFF-inline".to_vec();
    let input = runtime.allocate(buffer.len(), 1).unwrap();
    runtime.memory.write(input, &buffer).unwrap();
    let cpu = dispatch_midi_media(&mut runtime, 2_032, Some((input, buffer.len() as u32)));
    assert_eq!(cpu.register(0), 0);

    // The WAV device plays through the unified audio sink as SoundType::Wav.
    let cpu = dispatch_midi_media(&mut runtime, 2_042, None);
    assert_eq!(cpu.register(0), 0);
    STUB_SOUND.with(|sound| {
        let (sound_type, data, looped) = sound.borrow().clone().unwrap();
        assert_eq!(sound_type, SoundType::Wav);
        assert_eq!(data, buffer);
        assert!(!looped);
    });
    let cpu = dispatch_midi_media(&mut runtime, 2_092, None);
    assert_eq!(cpu.register(0), MEDIA_STATUS_PLAYING);
}

#[test]
fn platform_command_2036_loads_and_plays_an_m4a_buffer() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    STUB_SOUND.with(|sound| *sound.borrow_mut() = None);

    // An M4A file starts with an MP4 container header; the device stores the
    // buffer verbatim and the audio layer's generic probe picks the codec.
    let mut buffer = vec![0u8; 8];
    buffer.extend_from_slice(b"ftypmp42");
    buffer.extend_from_slice(&[0u8; 16]);
    let input = runtime.allocate(buffer.len(), 1).unwrap();
    runtime.memory.write(input, &buffer).unwrap();
    let cpu = dispatch_midi_media(&mut runtime, 2_036, Some((input, buffer.len() as u32)));
    assert_eq!(cpu.register(0), 0);
    let cpu = dispatch_midi_media(&mut runtime, 2_096, None);
    assert_eq!(cpu.register(0), MEDIA_STATUS_LOADED);

    // Device 6 maps onto SoundType::M4a and reaches the PLAYING state on
    // a successful play.
    let cpu = dispatch_midi_media(&mut runtime, 2_046, None);
    assert_eq!(cpu.register(0), 0);
    STUB_SOUND.with(|sound| {
        let (sound_type, data, looped) = sound.borrow().clone().unwrap();
        assert_eq!(sound_type, SoundType::M4a);
        assert_eq!(data, buffer);
        assert!(!looped);
    });
    let cpu = dispatch_midi_media(&mut runtime, 2_096, None);
    assert_eq!(cpu.register(0), MEDIA_STATUS_PLAYING);
}

#[test]
fn platform_command_2037_loads_and_plays_an_amr_wideband_buffer() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    STUB_SOUND.with(|sound| *sound.borrow_mut() = None);

    // AMR-WB files carry their own magic number; like the narrowband device
    // the buffer is stored verbatim and decoded by the audio layer.
    let buffer = b"#!AMR-WB\n".to_vec();
    let input = runtime.allocate(buffer.len(), 1).unwrap();
    runtime.memory.write(input, &buffer).unwrap();
    let cpu = dispatch_midi_media(&mut runtime, 2_037, Some((input, buffer.len() as u32)));
    assert_eq!(cpu.register(0), 0);
    let cpu = dispatch_midi_media(&mut runtime, 2_097, None);
    assert_eq!(cpu.register(0), MEDIA_STATUS_LOADED);

    // Device 7 shares SoundType::Amr with the narrowband device; the file
    // magic distinguishes the variants inside the decoder.
    let cpu = dispatch_midi_media(&mut runtime, 2_047, None);
    assert_eq!(cpu.register(0), 0);
    STUB_SOUND.with(|sound| {
        let (sound_type, data, looped) = sound.borrow().clone().unwrap();
        assert_eq!(sound_type, SoundType::Amr);
        assert_eq!(data, buffer);
        assert!(!looped);
    });
    let cpu = dispatch_midi_media(&mut runtime, 2_097, None);
    assert_eq!(cpu.register(0), MEDIA_STATUS_PLAYING);
}

#[test]
fn platform_command_2035_wraps_pcm_samples_in_a_wav_container() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    STUB_SOUND.with(|sound| *sound.borrow_mut() = None);

    // Raw PCM rides in as an opaque buffer; the player wraps it as an
    // 8 kHz/16-bit/mono WAV stream like the C++ host.
    let samples: Vec<u8> = (0..64u8).collect();
    let input = runtime.allocate(samples.len(), 1).unwrap();
    runtime.memory.write(input, &samples).unwrap();
    let cpu = dispatch_midi_media(&mut runtime, 2_035, Some((input, samples.len() as u32)));
    assert_eq!(cpu.register(0), 0);

    let cpu = dispatch_midi_media(&mut runtime, 2_045, None);
    assert_eq!(cpu.register(0), 0);
    STUB_SOUND.with(|sound| {
        let (sound_type, data, _) = sound.borrow().clone().unwrap();
        assert_eq!(sound_type, SoundType::Wav);
        assert_eq!(&data[..4], b"RIFF");
        assert_eq!(&data[8..12], b"WAVE");
        assert_eq!(data.len(), samples.len() + 44);
        assert_eq!(&data[44..], &samples[..]);
    });
}

#[test]
fn platform_media_commands_beyond_device_seven_answer_mr_ignore() {
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();

    // dsm_media_get only hands out devices 1-7; anything wider answers
    // MR_IGNORE instead of touching a device slot.
    for command in [2_018, 2_028, 2_098] {
        let cpu = dispatch_midi_media(&mut runtime, command, None);
        assert_eq!(cpu.register(0), 1);
    }
}

/// Builds a T_BG_PLAY_REQ in guest memory pointing at `payload`, returning
/// the request record address (platEx 222x input shape).
fn multi_channel_request(
    runtime: &mut ExtRuntime,
    payload: &[u8],
    looped: bool,
) -> (GuestAddr, GuestAddr) {
    let data = runtime.allocate(payload.len().max(1), 4).unwrap();
    if !payload.is_empty() {
        runtime.memory.write(data, payload).unwrap();
    }
    let request = runtime.allocate(12, 4).unwrap();
    runtime
        .memory
        .write_u32(request, if payload.is_empty() { 0 } else { data.0 })
        .unwrap();
    runtime
        .memory
        .write_u32(request.checked_add(4).unwrap(), payload.len() as u32)
        .unwrap();
    runtime
        .memory
        .write_u32(request.checked_add(8).unwrap(), u32::from(looped))
        .unwrap();
    (request, data)
}

/// Allocates a guest cell holding a channel handle (platEx 223x/224x/225x
/// input shape: a pointer to the 4-byte handle).
fn channel_handle_cell(runtime: &mut ExtRuntime, handle: u32) -> GuestAddr {
    let cell = runtime.allocate(4, 4).unwrap();
    runtime.memory.write_u32(cell, handle).unwrap();
    cell
}

#[test]
fn platform_command_222x_loads_multi_channel_audio_and_controls_it_by_handle() {
    let _channels = reset_stub_channels();
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let payload = b"channel-payload".to_vec();
    let (request, _) = multi_channel_request(&mut runtime, &payload, false);

    // LOAD 2222 (MEDIA_WAVE) returns a nonzero handle instead of a status.
    let cpu = dispatch_platform_extension(
        &mut runtime,
        2_222,
        request,
        12,
        GuestAddr(0),
        GuestAddr(0),
    );
    let handle = cpu.register(0);
    // The stub hands out handles from 1; the load must not report failure.
    assert_eq!(handle, 1);
    STUB_CHANNEL_LOADS.with(|loads| {
        assert_eq!(loads.borrow().len(), 1);
        assert_eq!(
            loads.borrow()[0],
            (2usize, payload.clone(), false)
        );
    });

    // PLAY/STOP/RELEASE address the loaded voice through a handle cell.
    let cell = channel_handle_cell(&mut runtime, handle);
    for (command, call) in [(2_232, "play"), (2_242, "stop"), (2_252, "release")] {
        let cpu =
            dispatch_platform_extension(&mut runtime, command, cell, 4, GuestAddr(0), GuestAddr(0));
        assert_eq!(cpu.register(0), 0, "{call} must answer MR_SUCCESS");
    }
    STUB_CHANNEL_OPS.with(|ops| {
        assert_eq!(
            ops.borrow().clone(),
            vec![("play", handle), ("stop", handle), ("release", handle)]
        );
    });

    // After release the handle is gone: repeated release answers MR_FAILED
    // instead of touching the stub again.
    let cpu =
        dispatch_platform_extension(&mut runtime, 2_252, cell, 4, GuestAddr(0), GuestAddr(0));
    assert_eq!(cpu.register(0), u32::MAX);
    STUB_CHANNEL_OPS.with(|ops| assert_eq!(ops.borrow().len(), 3));
}

#[test]
fn platform_command_222x_rejects_malformed_shapes_softly() {
    let _channels = reset_stub_channels();
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let (request, _) = multi_channel_request(&mut runtime, b"payload", false);

    // Media types without a decoder (2227 = MEDIA_AAC and beyond) answer
    // MR_IGNORE so the guest can fall back, like the C++ default branch.
    for command in [2_227, 2_228, 2_229] {
        let cpu = dispatch_platform_extension(
            &mut runtime,
            command,
            request,
            12,
            GuestAddr(0),
            GuestAddr(0),
        );
        assert_eq!(cpu.register(0), 1, "command {command} must answer MR_IGNORE");
    }
    // A null input or a record shorter than T_BG_PLAY_REQ is a probe or a
    // bug in the caller: MR_IGNORE, not a fatal trap.
    let cpu = dispatch_platform_extension(
        &mut runtime,
        2_222,
        GuestAddr(0),
        12,
        GuestAddr(0),
        GuestAddr(0),
    );
    assert_eq!(cpu.register(0), 1);
    let cpu =
        dispatch_platform_extension(&mut runtime, 2_222, request, 8, GuestAddr(0), GuestAddr(0));
    assert_eq!(cpu.register(0), 1);
    // A null payload pointer or zero length is a real failure: MR_FAILED.
    let (empty, _) = multi_channel_request(&mut runtime, b"", false);
    let cpu =
        dispatch_platform_extension(&mut runtime, 2_222, empty, 12, GuestAddr(0), GuestAddr(0));
    assert_eq!(cpu.register(0), u32::MAX);
    // Nothing reached the stub in this test.
    STUB_CHANNEL_LOADS.with(|loads| assert!(loads.borrow().is_empty()));
}

#[test]
fn platform_command_222x_maps_decode_failure_to_mr_failed() {
    let _channels = reset_stub_channels();
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    let (request, _) = multi_channel_request(&mut runtime, b"undecodable", false);

    let cpu = dispatch_platform_extension(
        &mut runtime,
        2_222,
        request,
        12,
        GuestAddr(0),
        GuestAddr(0),
    );
    assert_eq!(cpu.register(0), u32::MAX);
    // The failed load must not leak a live handle into the stub.
    STUB_LIVE_CHANNEL_HANDLES.with(|handles| assert!(handles.borrow().is_empty()));
}

#[test]
fn platform_command_223x_rejects_null_input_and_unknown_handles() {
    let _channels = reset_stub_channels();
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();

    // Null input answers MR_IGNORE; an unknown handle answers MR_FAILED.
    let cpu = dispatch_platform_extension(
        &mut runtime,
        2_232,
        GuestAddr(0),
        4,
        GuestAddr(0),
        GuestAddr(0),
    );
    assert_eq!(cpu.register(0), 1);
    let stale = channel_handle_cell(&mut runtime, 4242);
    for command in [2_232, 2_242, 2_252] {
        let cpu =
            dispatch_platform_extension(&mut runtime, command, stale, 4, GuestAddr(0), GuestAddr(0));
        assert_eq!(cpu.register(0), u32::MAX, "command {command} must fail");
    }
}

#[test]
fn platform_command_222x_propagates_the_loop_flag_and_media_type() {
    let _channels = reset_stub_channels();
    let mut runtime =
        ExtRuntime::new(8, 8, b"test.mrp", b"start.mr", DEFAULT_HEAP_LEN as u32).unwrap();
    // 2221 is MEDIA_MIDI with loop = 1; the stub must observe both.
    let (request, _) = multi_channel_request(&mut runtime, b"looping-midi", true);
    let cpu = dispatch_platform_extension(
        &mut runtime,
        2_221,
        request,
        12,
        GuestAddr(0),
        GuestAddr(0),
    );
    let handle = cpu.register(0);
    // First stub handle; the load must not report failure.
    assert_eq!(handle, 1);
    STUB_CHANNEL_LOADS.with(|loads| {
        assert_eq!(
            loads.borrow()[0],
            (1usize, b"looping-midi".to_vec(), true)
        );
    });
}
