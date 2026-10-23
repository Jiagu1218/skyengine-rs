use super::*;

/// 取证：SKYENGINE_TRACE_FILEIO=1 时记录 slot 40/43/45（open/write/seek）
/// 的紧凑轨迹（gb12.uc2 字体索引改写偏移差一排查）。
fn fileio_trace() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("SKYENGINE_TRACE_FILEIO").is_some())
}

impl ExtRuntime {
    /// SKYENGINE_TRACE_NETWORK 开启时输出一条网络 slot 日志。只在网络
    /// slot 里调用,量级与网络 I/O 相当,长期开启也不影响性能。
    pub(super) fn trace_network(&self, entry: String) {
        if self.network_trace_enabled {
            eprintln!("[net] {entry}");
        }
    }

    pub(super) fn dispatch(
        &mut self,
        slot: u32,
        module: usize,
        cpu: &mut ArmCpu,
        services: &mut dyn NativeServices,
    ) -> Result<()> {
        if self.fault_context_enabled {
            self.record_transition(format!(
                "slot={slot} m={module} r0={:#010x} r1={:#010x} r2={:#010x} r3={:#010x} lr={:#010x}",
                cpu.register(0),
                cpu.register(1),
                cpu.register(2),
                cpu.register(3),
                cpu.register(14)
            ));
        }
        // 取证：当前分派上下文（guest 堆水位日志联动，定位循环分配方）。
        self.heap_trace_context = (slot, module, cpu.register(14));
        // SKYENGINE_TRACE_SLOTS：slot 级调用轨迹，用于无 trace 的独立臂
        // （如设备 3 的 2013/2093）排查应用调用序列。跳过每帧热点的
        // libc/draw slot，长期开启也不影响性能。
        if std::env::var_os("SKYENGINE_TRACE_SLOTS").is_some()
            && !matches!(slot, 0..=20 | 26 | 29 | 30)
        {
            eprintln!(
                "[slot] m={module} slot={slot} r0={:#010x} r1={:#010x} r2={:#010x} r3={:#010x} lr={:#010x}",
                cpu.register(0),
                cpu.register(1),
                cpu.register(2),
                cpu.register(3),
                cpu.register(14)
            );
        }
        let trace_arm = std::env::var_os("SKYENGINE_TRACE_ARM").is_some()
            || std::env::var_os("SKYENGINE_TRACE_TRAPS").is_some();
        if trace_arm {
            // When the trap comes from the shell's free_wrapper (0x100032d4)
            // or shell_malloc (0x100035d8), the wrapper has pushed {rX, lr},
            // so the guest word at [sp+4] is the return address into the
            // wrapper's *caller* — log it to identify the true call site.
            let mut caller = String::new();
            let lr = cpu.register(14);
            // Game-side thin wrappers also push {rX, lr}: seek wrapper
            // 0x20027200 (blx at 0x2002720c), read wrapper 0x200269ec
            // (blx at 0x200269f8) and close wrapper 0x20025ff8 (blx at
            // 0x20026004). For those [sp+4] is the caller return address.
            if lr == 0x1000_32eb
                || lr == 0x1000_35e9
                || lr == 0x2002_720f
                || lr == 0x2002_69fb
                || lr == 0x2002_6007
            {
                let mut probed = String::new();
                // The map-validation search routine (0x2000d916) compares
                // markers at [search_sp+0x18] against its target at
                // [search_sp+0x68]. Frame math (verified against runtime
                // traps) puts [search_sp+0x68] at readByteAt-seek-trap sp
                // +0xA8, so probe that wide for the seek wrapper.
                let offsets: &[i32] = if lr == 0x2002_720f {
                    &[
                        -8, -4, 0, 4, 8, 12, 16, 20, 0x40, 0x44, 0x48, 0x4c, 0x50, 0x54, 0x58,
                        0x5c, 0x60, 0x64, 0x68, 0x6c, 0x70, 0x74, 0x78, 0x7c, 0x80, 0x84, 0x88,
                        0x8c, 0x90, 0x94, 0x98, 0x9c, 0xa0, 0xa4, 0xa8, 0xac, 0xb0, 0xb4, 0xb8,
                        0xbc, 0xc0, 0xc4, 0xc8, 0xcc, 0xd0, 0xd4, 0xd8, 0xdc, 0xe0, 0xe4, 0xe8,
                        0xec, 0xf0, 0xf4, 0xf8, 0xfc, 0x100, 0x104, 0x108, 0x10c, 0x110, 0x114,
                        0x118, 0x11c, 0x120,
                    ]
                } else {
                    &[-8, -4, 0, 4, 8, 12, 16, 20, 24, 28, 32]
                };
                for offset in offsets {
                    let offset = *offset;
                    let address = (cpu.register(13) as i64) + offset as i64;
                    if address < 0 || address > u32::MAX as i64 {
                        continue;
                    }
                    match self.memory.read(GuestAddr(address as u32), 4) {
                        Ok(buf) if buf.len() == 4 => probed.push_str(&format!(
                            " [{:+x}]={:#010x}",
                            offset,
                            u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])
                        )),
                        _ => probed.push_str(&format!(" [{offset:+x}]=unmapped")),
                    }
                }
                let mut absolute = String::new();
                if lr == 0x2002_720f {
                    // Game-module globals of interest (game.ext SB = 0x2002f8ac):
                    // [SB+8] = default map version parsed from fusion_setting.txt,
                    // [SB+0x76c] = shell-provided conversion callback used by the
                    // config parser (memset=0x75c / strlen=0x760 neighborhood).
                    for (name, addr) in [
                        ("SB+8", 0x2002_f8b4u32),
                        ("SB+76c", 0x2003_0018u32),
                        ("SB+1fbc", 0x2003_1868u32),
                    ] {
                        match self.memory.read(GuestAddr(addr), 4) {
                            Ok(buf) if buf.len() == 4 => absolute.push_str(&format!(
                                " [{name}@{addr:#x}]={:#010x}",
                                u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])
                            )),
                            _ => absolute.push_str(&format!(" [{name}@{addr:#x}]=unmapped")),
                        }
                    }
                }
                caller = format!(
                    " r4={:#010x} r5={:#010x} r6={:#010x} r7={:#010x}{probed}{absolute}",
                    cpu.register(4),
                    cpu.register(5),
                    cpu.register(6),
                    cpu.register(7),
                );
            }
            // Numeric/string libc conversions: log the C string at r0 (and r1
            // for sprintf-style slots) so config parsing can be followed.
            if matches!(slot, 17 | 18 | 19) {
                let mut strings = String::new();
                for (reg, name) in [(0usize, "r0"), (1, "r1")] {
                    let ptr = cpu.register(reg);
                    if ptr > 0x1000 {
                        match self.memory.read(GuestAddr(ptr), 64) {
                            Ok(buf) => {
                                let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
                                let text: Vec<u8> = buf[..end].to_vec();
                                if !text.is_empty() {
                                    strings.push_str(&format!(
                                        " {name}str={:?}",
                                        String::from_utf8_lossy(&text)
                                    ));
                                }
                            }
                            Err(_) => strings.push_str(&format!(" {name}str=unmapped")),
                        }
                    }
                }
                if !strings.is_empty() {
                    caller.push_str(&strings);
                }
            }
            eprintln!(
                "[arm-trap] module={module} slot={slot} r0={:#010x} r1={:#010x} r2={:#010x} r3={:#010x} r9={:#010x} lr={:#010x} sp={:#010x}{caller}",
                cpu.register(0),
                cpu.register(1),
                cpu.register(2),
                cpu.register(3),
                cpu.register(9),
                cpu.register(14),
                cpu.register(13),
            );
        }
        match slot {
            0..=20 => self.dispatch_libc(slot, module, cpu)?,
            25 => {
                let helper = cpu.register(0);
                let expected_image = self
                    .modules
                    .get(module)
                    .ok_or_else(|| {
                        Error::Abi(format!("helper registration for missing module {module}"))
                    })?
                    .executable_image(helper)
                    .map(|(image, _)| image)
                    .ok_or_else(|| {
                        Error::Abi(format!(
                            "helper {helper:#010x} is outside module {module} executable images"
                        ))
                    })?;
                let parameter_len = cpu.register(1).max(20) as usize;
                let parameter = self
                    .allocate_guest_block_for_module(parameter_len, module)?
                    .ok_or_else(|| {
                        Error::ArmFault(
                            "guest heap exhausted while allocating helper parameter".into(),
                        )
                    })?;
                self.memory.write(parameter, &vec![0; parameter_len])?;
                let context = self.modules.get_mut(module).ok_or_else(|| {
                    Error::Abi(format!("helper registration for missing module {module}"))
                })?;
                let function = GuestFunction {
                    module,
                    address: helper,
                    expected_image: Some(expected_image),
                    captured_r9: None,
                };
                context.helper = Some(function);
                context.helper_parameter = parameter;
                self.active_helper = Some(function);
                cpu.set_register(0, parameter.0);
            }
            26 => {
                let format = self.read_c_string(GuestAddr(cpu.register(0)), 64 * 1024)?;
                if trace_arm || std::env::var_os("SKYENGINE_TRACE_PRINTF").is_some() {
                    // Resolve %s conversions (first three varargs in r1..r3,
                    // the rest on the guest stack) so traces show real text.
                    let mut strings: Vec<String> = Vec::new();
                    let registers = [cpu.register(1), cpu.register(2), cpu.register(3)];
                    let mut arg_index = 0usize;
                    let mut cursor = 0usize;
                    while cursor < format.len() {
                        if format[cursor] != b'%' {
                            cursor += 1;
                            continue;
                        }
                        cursor += 1;
                        if cursor < format.len() && format[cursor] == b'%' {
                            cursor += 1;
                            continue;
                        }
                        while cursor < format.len()
                            && matches!(
                                format[cursor],
                                b'0'..=b'9' | b'.' | b'-' | b'+' | b' ' | b'#'
                                    | b'l' | b'h' | b'z' | b'j' | b't' | b'L'
                            )
                        {
                            cursor += 1;
                        }
                        if cursor >= format.len() {
                            break;
                        }
                        let conversion = format[cursor];
                        cursor += 1;
                        if conversion != b's' {
                            arg_index += 1;
                            continue;
                        }
                        let pointer = if arg_index < registers.len() {
                            registers[arg_index]
                        } else {
                            let stack_offset = (arg_index - registers.len()) as u32 * 4;
                            let Ok(word) = self
                                .memory
                                .read_u32(GuestAddr(cpu.register(13).wrapping_add(stack_offset)))
                            else {
                                arg_index += 1;
                                continue;
                            };
                            word
                        };
                        arg_index += 1;
                        if pointer == 0 {
                            strings.push("(null)".into());
                            continue;
                        }
                        match self.read_c_string(GuestAddr(pointer), 256) {
                            Ok(text) => strings.push(String::from_utf8_lossy(&text).into_owned()),
                            Err(_) => strings.push("(unreadable)".into()),
                        }
                    }
                    eprintln!(
                        "[guest-printf] format={:?} r1={:#010x} r2={:#010x} r3={:#010x} strings={:?}",
                        String::from_utf8_lossy(&format),
                        cpu.register(1),
                        cpu.register(2),
                        cpu.register(3),
                        strings
                    );
                }
                cpu.set_register(0, format.len() as u32);
            }
            29 => {
                let source = GuestAddr(cpu.register(0));
                let x = cpu.register(1) as i32;
                let y = cpu.register(2) as i32;
                let width = cpu.register(3) as usize;
                let height = self.memory.read_u32(GuestAddr(cpu.register(13)))? as usize;
                // Rectangles outside the screen clip to their visible part
                // instead of failing, mirroring the reference kernel's
                // per-pixel clipping: landscape games blit full 320x240
                // frames onto portrait 240x320 screens.
                let region = self.read_platform_draw_pixels(source, x, y, width, height)?;
                if region.width > 0 && region.height > 0 {
                    self.draw_platform_bitmap(
                        &region.pixels,
                        region.x,
                        region.y,
                        region.width,
                        region.height,
                        services,
                    )?;
                }
                cpu.set_register(0, 0);
            }
            30 => {
                let codepoint = cpu.register(0);
                let font = cpu.register(1);
                let width_out = GuestAddr(cpu.register(2));
                let height_out = GuestAddr(cpu.register(3));
                let key = (codepoint, font);
                let glyph = match self.glyphs.get(&key).copied() {
                    Some(glyph) => Some(glyph),
                    None => match services.char_bitmap(codepoint, font)? {
                        Some((bitmap, width, height)) => {
                            if width == 0 || width > 16 || height == 0 || height > 16 {
                                return Err(Error::Abi(format!(
                                    "unsupported character bitmap dimensions {width}x{height} for {codepoint:#06x}"
                                )));
                            }
                            let height_usize = height as usize;
                            let required = height_usize.checked_mul(2).ok_or_else(|| {
                                Error::Abi("character bitmap size overflow".into())
                            })?;
                            if bitmap.len() < required {
                                return Err(Error::Abi(format!(
                                    "character bitmap for {codepoint:#06x} has {} bytes, needs {required}",
                                    bitmap.len()
                                )));
                            }
                            let guest_stride = width.div_ceil(8) as usize;
                            let mut guest_bitmap = Vec::with_capacity(guest_stride * height_usize);
                            for row in bitmap[..required].as_chunks::<2>().0 {
                                guest_bitmap.extend(
                                    row[..guest_stride].iter().map(|byte| byte.reverse_bits()),
                                );
                            }
                            let address = self.allocate_glyph_bitmap(guest_bitmap.len())?;
                            self.memory.write(address, &guest_bitmap)?;
                            let glyph = GuestGlyph {
                                address,
                                width,
                                height,
                            };
                            self.glyphs.insert(key, glyph);
                            Some(glyph)
                        }
                        None => None,
                    },
                };
                let (address, width, height) = glyph
                    .map(|glyph| (glyph.address.0, glyph.width, glyph.height))
                    .unwrap_or((0, 0, 0));
                if width_out.0 != 0 {
                    self.memory.write_u32(width_out, width)?;
                }
                if height_out.0 != 0 {
                    self.memory.write_u32(height_out, height)?;
                }
                cpu.set_register(0, address);
            }
            31 => {
                let delay_ms = cpu.register(0);
                // racing 开始后补一次 dump：首个 >=30s 的 arm 视为 racing 起点。
                static DUMP_LATE: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if delay_ms >= 30_000 && !DUMP_LATE.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    self.dump_guest_regions("late");
                }
                self.discover_compact_repeating_timers();
                let delay = Duration::from_millis(u64::from(delay_ms));
                // 单一 VMR 定时器：与 VM TimerStart 共用同一条宿主定时器
                // （last-arm-wins），fire 只发生在 armed deadline，由宿主
                // 经 NativeServices 落到 mr_timer_* 字段。
                services.ext_timer_start(delay)?;
                self.memory.write_u32(TIMER_ACTIVE_DATA, 1)?;
                cpu.set_register(0, 0);
                crate::time_scale::trace_timer(format_args!(
                    "ext-arm delay={delay_ms}ms m={module} lr={:#010x} r9={:#010x}",
                    cpu.register(14),
                    cpu.register(9)
                ));
            }
            32 => {
                services.ext_timer_stop()?;
                self.memory.write_u32(TIMER_ACTIVE_DATA, 0)?;
                cpu.set_register(0, 0);
                crate::time_scale::trace_timer(format_args!(
                    "ext-stop m={module} lr={:#010x} r9={:#010x}",
                    cpu.register(14),
                    cpu.register(9)
                ));
            }
            33 => {
                let elapsed_ms = self.clock_origin.elapsed().as_millis() as u32;
                if trace_arm {
                    eprintln!("[arm-gettime] -> {elapsed_ms} ({elapsed_ms:#x})");
                }
                // 一次性 dump：定位堆内动态镜像（timer racing 排查用）。
                // 早期 dump：首次从堆内镜像代码（lr>=0x2000_0000）调用 gettime 时触发；
                // 晚期 dump：由 slot 31 的首个大间隔 arm 触发（racing 开始后）。
                static DUMP_EARLY: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                let lr_value = cpu.register(14);
                let early_dump = (0x2000_0000..0x3000_0000).contains(&lr_value)
                    && !DUMP_EARLY.swap(true, std::sync::atomic::Ordering::SeqCst);
                if early_dump {
                    self.dump_guest_regions("early");
                }
                cpu.set_register(0, elapsed_ms);
                crate::time_scale::trace_timer(format_args!(
                    "ext-gettime -> {elapsed_ms}ms m={module} lr={:#010x} r9={:#010x}",
                    lr_value,
                    cpu.register(9)
                ));
            }
            34 => {
                let output = GuestAddr(cpu.register(0));
                let device_datetime = self.current_device_datetime();
                self.memory.write_u16(output, device_datetime.year)?;
                self.memory.write(
                    output.checked_add(2)?,
                    // month, day, hour, minute, second, weekday (Sunday = 0)
                    &[
                        device_datetime.month,
                        device_datetime.day,
                        device_datetime.hour,
                        device_datetime.minute,
                        device_datetime.second,
                        device_datetime.weekday(),
                    ],
                )?;
                cpu.set_register(0, 0);
            }
            35 => {
                let output = GuestAddr(cpu.register(0));
                if output.0 == 0 {
                    cpu.set_register(0, u32::MAX);
                } else {
                    self.memory.write(output, &platform_user_info())?;
                    cpu.set_register(0, 0);
                }
            }
            36 => {
                // The outer runtime owns scheduling; acknowledge guest sleeps
                // without blocking the event and control loops.
                cpu.set_register(0, 0);
            }
            37 => match (cpu.register(0), cpu.register(1)) {
                // LCD rotation (mrporting.h:796 MR_LCD_ROTATE_NORMAL/90/180/
                // 270). The reference kernel's mr_plat case 101 accepts every
                // value 0..3 and answers MR_SUCCESS; 90/270 select the
                // landscape orientation. Values outside the contract answer
                // the non-fatal MR_IGNORE instead of failing.
                (101, mode @ 0..=3) => {
                    self.set_screen_orientation(mode & 1 == 1, services)?;
                    cpu.set_register(0, 0);
                }
                (101, _) => {
                    cpu.set_register(0, 1);
                }
                // Poll a non-blocking socket created through slots 84 and 85.
                (1_001, handle) => {
                    cpu.set_register(0, self.native_socket_state(handle as i32) as u32)
                }
                // Baseline SDK initialization notification; the return value is ignored.
                (1_106, 0) => cpu.set_register(0, 0),
                // Network/payment helpers announce their foreground operation.
                // The headless host has no separate foreground UI state.
                (1_011, mode) if mode <= 1 => cpu.set_register(0, 0),
                // Optional device metric. Repository EXT callers decode values above
                // 1000 and explicitly treat -1 as an unavailable neutral result.
                (1_101, 2) => cpu.set_register(0, u32::MAX),
                // Optional runtime profile query. The caller has a defined
                // fallback state for values other than the two vendor profiles.
                (1_100, 0) => cpu.set_register(0, u32::MAX),
                // Report the normal storage profile. 1002 denotes USB mass-storage
                // mode, in which applications must not access their regular volume.
                (1_218, 0) => cpu.set_register(0, 1_001),
                // RX initialization announces its default platform mode and does
                // not consume a result beyond whether the call is accepted.
                (1_214, mode) if mode <= 1 => cpu.set_register(0, 0),
                // Network request compatibility version used by message.ext.
                (1_205, 0) => cpu.set_register(0, 1_001),
                // Initialize the motion-event provider. Samples remain disabled until
                // the guest selects the verified event-driven mode below.
                (1_206, 0) => {
                    self.motion_active = false;
                    cpu.set_register(0, 0);
                }
                // 取证差分（评分公式逆向）：SKYENGINE_DEBUG_MOTION_OFF=1 令
                // 运动传感器能力查询回 MR_IGNORE(1)，guest 走"无重力"回退。
                (4_002, 0) if std::env::var_os("SKYENGINE_DEBUG_MOTION_OFF").is_some() => {
                    cpu.set_register(0, 1)
                }
                // Optional device effects used by talkcat's downloaded belly and
                // fart actions. The deterministic headless profile has no provider.
                (1_211, 2 | 3) => cpu.set_register(0, 0),
                // Query and configure the same deterministic motion provider.
                // Mode 2 enables event delivery; command 4003 disables it when
                // the guest declines motion input or leaves the active mode.
                (4_002, 0) => cpu.set_register(0, 0),
                (4_003, 0) => {
                    self.motion_active = false;
                    cpu.set_register(0, 0);
                }
                (4_005, 2) => {
                    self.motion_active = true;
                    cpu.set_register(0, 0);
                }
                // Native audio wrappers use a five-step multimedia volume.
                (1_302, volume) if volume <= 5 => {
                    services.set_sound_volume(volume as u8)?;
                    cpu.set_register(0, 0);
                }
                // The C++ platform has no 1302 case at all and answers every
                // mr_plat volume call with MR_IGNORE, so out-of-range values
                // (the guest scale can exceed the host's five steps) report
                // the same don't-care result instead of failing.
                (1_302, _) => {
                    if trace_arm {
                        eprintln!(
                            "[arm-plat] ignored out-of-range volume {} from module {module}",
                            cpu.register(1)
                        );
                    }
                    cpu.set_register(0, 1);
                }
                // Optional dual-SIM selection probe. A false result keeps the
                // guest on its default network selection path.
                (1_327, 0) => cpu.set_register(0, u32::MAX),
                // No explicit SIM/network selection is configured.
                (1_328, 0) => cpu.set_register(0, u32::MAX),
                // Optional runtime-service notifications with no headless state.
                // Accept only the observed parameterless forms.
                (1_016 | 1_018 | 1_215 | 1_216, 0) => cpu.set_register(0, 0),
                // Parameterless platform notification. Its wrapper normalizes the
                // result to 0/-1, and every verified caller discards that result.
                (2_703, 0) => cpu.set_register(0, 0),
                // The network-state notification uses 1 for the normal profile;
                // callers branch away from startup unless they receive 1 or 1000.
                (1_020, 0) => cpu.set_register(0, 1),
                // Legacy file-position query. Successful positions are encoded
                // with a 1000 bias; invalid handles retain the normal -1 result.
                (1_231, handle) => {
                    let result = services
                        .seek_file(handle as i32, 0, 1)?
                        .and_then(|position| position.checked_add(1_000))
                        .and_then(|position| u32::try_from(position).ok())
                        .unwrap_or(u32::MAX);
                    cpu.set_register(0, result);
                }
                (command, argument) => {
                    // Commands with arms above keep the strict verified
                    // contract: a request whose shape the arm rejected is a
                    // loud error rather than a silent fallback.
                    let known = matches!(
                        command,
                        1_001
                            | 1_011
                            | 1_016
                            | 1_018
                            | 1_020
                            | 1_100
                            | 1_101
                            | 1_106
                            | 1_205
                            | 1_206
                            | 1_211
                            | 1_214
                            | 1_215
                            | 1_216
                            | 1_218
                            | 1_231
                            | 1_302
                            | 1_327
                            | 1_328
                            | 2_703
                            | 4_002
                            | 4_003
                            | 4_005
                    );
                    if known {
                        return Err(Error::Abi(format!(
                            "unsupported platform slot 37 command ({command}, {argument}) called by module {module} at LR {:#010x} (r2={:#010x}, r3={:#010x})",
                            cpu.register(14),
                            cpu.register(2),
                            cpu.register(3),
                        )));
                    }
                    // Any other code is unknown to the C++ platform as well:
                    // its mr_plat default answers MR_IGNORE (1) with a log
                    // warning, telling the guest to use its own fallback, and
                    // the call is not fatal.
                    if trace_arm {
                        eprintln!(
                            "[arm-plat] ignored unsupported command ({command}, {argument}) from module {module}"
                        );
                    }
                    cpu.set_register(0, 1);
                }
            },
            38 => match cpu.register(0) {
                // Requests an additional guest-memory arena. Verified capacity
                // probes carry a signed target-minus-current delta in input_len
                // even though input is null; positive values remain byte counts.
                1_014 if cpu.register(1) == 0 => {
                    self.allocate_platform_memory_extension(module, cpu)?
                }
                // Releases an arena returned by command 1014. The ABI carries
                // the 32-bit guest address as a four-byte input buffer.
                1_015 => self.release_platform_memory_extension(module, cpu)?,
                // Resolve the logical application storage volume to a drive.
                1_204 => self.return_platform_storage_drive(cpu)?,
                // Convert caller-owned UCS-2BE into a caller-owned legacy C string.
                1_207 => self.convert_platform_ucs2_to_legacy(cpu)?,
                // Return the deterministic baseline runtime-state record.
                1_224 if cpu.register(1) == 0 && cpu.register(2) == 0 => {
                    self.return_platform_runtime_profile(cpu)?
                }
                // Optional platform metadata query. No metadata provider is configured.
                1_222 => self.return_unavailable_platform_extension(cpu)?,
                // Optional hardware and callback-backed capability queries. The
                // baseline profile exposes neither, and their callers support -1.
                1_017 | 1_324 if cpu.register(1) == 0 && cpu.register(2) == 0 => {
                    self.return_unavailable_platform_extension(cpu)?
                }
                // Optional device metadata blob used to enrich network requests.
                1_116 if cpu.register(1) == 0 && cpu.register(2) == 0 => {
                    self.return_unavailable_platform_extension(cpu)?
                }
                // Returns the available SIM slots. The headless baseline has no
                // carrier provider, so expose a valid empty result structure.
                1_307 if cpu.register(1) == 0 && cpu.register(2) == 0 => {
                    self.return_platform_sim_info(cpu)?
                }
                // Disk geometry used by the guest's startup space check.
                1_305 => self.return_platform_storage_info(cpu)?,
                // Synchronous JPEG metadata and RGB565 decode operations. These
                // commands use caller-owned source and destination buffers.
                3_001 => self.return_platform_jpeg_info(cpu)?,
                3_002 => self.decode_platform_jpeg(cpu)?,
                // The handle-backed decoder uses an opaque platform object whose
                // full layout and lifetime contract are not part of the verified subset.
                command @ (3_004 | 3_005) => {
                    return Err(Error::Abi(format!(
                        "unsupported platform JPEG handle command {command} called by module {module}"
                    )));
                }
                // Optional platform-state query. Both the parameterless probe and
                // the normal output/output-length form use the unavailable result.
                1_223 if cpu.register(1) == 0 && cpu.register(2) == 0 => {
                    self.return_unavailable_platform_extension(cpu)?
                }
                // File-backed MP3 player INIT (MR_MEDIA_INIT, device 3). The
                // all-zero probe mirrors the C++ INIT transition: the device
                // releases any resource it still holds and reports success
                // (dsm.c dsm_media_init returns MR_SUCCESS unconditionally,
                // with no device-3 special case). Non-zero shapes keep the
                // rejected contract and fall through to the catch-all.
                2_013
                    if cpu.register(1) == 0
                        && cpu.register(2) == 0
                        && cpu.register(3) == 0
                        && self.memory.read_u32(GuestAddr(cpu.register(13)))? == 0
                        && self
                            .memory
                            .read_u32(GuestAddr(cpu.register(13)).checked_add(4)?)?
                            == 0 =>
                {
                    self.init_platform_media(3, cpu, services)?;
                    self.trace_platform_media("INIT", 3, cpu)?;
                }
                // File-backed MP3 playback uses a caller-owned path. The headless
                // profile verifies the resource and consumes it through a silent sink.
                2_023 => {
                    let path_address = GuestAddr(cpu.register(1));
                    let path_len = cpu.register(2) as usize;
                    let stack_pointer = GuestAddr(cpu.register(13));
                    if path_address.0 == 0
                        || path_len == 0
                        || path_len > 4 * 1024
                        || cpu.register(3) != 0
                        || self.memory.read_u32(stack_pointer)? != 0
                        || self.memory.read_u32(stack_pointer.checked_add(4)?)? != 0
                    {
                        return Err(Error::Abi(format!(
                            "unsupported platform MP3 request called by module {module}"
                        )));
                    }
                    let path = self.memory.read(path_address, path_len)?;
                    let components = path
                        .split(|byte| matches!(byte, b'/' | b'\\'))
                        .collect::<Vec<_>>();
                    let file_name = components.last().copied().unwrap_or_default();
                    if path.contains(&0)
                        || components.iter().any(|component| {
                            component.is_empty() || matches!(*component, b"." | b"..")
                        })
                        || file_name.len() <= 4
                        || !file_name[file_name.len() - 4..].eq_ignore_ascii_case(b".mp3")
                    {
                        return Err(Error::Abi(format!(
                            "unsupported platform MP3 path called by module {module}"
                        )));
                    }
                    let mut sound = services.read_sound_file(&path)?;
                    if sound.as_ref().is_none_or(Vec::is_empty) {
                        let package_name = self.read_c_string(PACKAGE_NAME_DATA, 256)?;
                        sound = services.read_package_file(&package_name, file_name)?;
                    }
                    let sound = match sound.filter(|bytes| !bytes.is_empty()) {
                        Some(sound) => sound,
                        None => {
                            cpu.set_register(0, u32::MAX);
                            return Ok(());
                        }
                    };
                    let succeeded = services.play_sound(SoundType::Mp3, &sound, false).is_ok();
                    // Keep the loaded bytes in the device slot so the RESUME
                    // (2063) request can replay them: the C++ kernel services
                    // RESUME through dsm_media_play, which replays the stored
                    // data from the start.
                    self.media_devices[3] = PlatformMediaDevice {
                        status: if succeeded {
                            MEDIA_STATUS_PLAYING
                        } else {
                            MEDIA_STATUS_LOADED
                        },
                        data: Some(sound),
                    };
                    cpu.set_register(0, if succeeded { 0 } else { u32::MAX });
                }
                // Starts the file-backed player prepared by 2023. Playback already
                // begins in the host adapter, so this transition must preserve it.
                2_043 => {
                    let input_address = GuestAddr(cpu.register(1));
                    let input_len = cpu.register(2) as usize;
                    let stack_pointer = GuestAddr(cpu.register(13));
                    let has_empty_input = input_address.0 == 0 && input_len == 0;
                    let has_zeroed_player_options = input_address.0 != 0
                        && input_len == 12
                        && self
                            .memory
                            .read(input_address, input_len)?
                            .iter()
                            .all(|byte| *byte == 0);
                    if (!has_empty_input && !has_zeroed_player_options)
                        || cpu.register(3) != 0
                        || self.memory.read_u32(stack_pointer)? != 0
                        || self.memory.read_u32(stack_pointer.checked_add(4)?)? != 0
                    {
                        return Err(Error::Abi(format!(
                            "unsupported platform player-start request called by module {module}"
                        )));
                    }
                    // 2023 already started playback in the host adapter, so
                    // starting the player again must not restart the song.
                    // A stopped or loaded resource routes through the shared
                    // player so the start actually replays the stored data;
                    // an empty slot keeps the historical success (there is
                    // nothing to start).
                    if self.media_devices[3].status != MEDIA_STATUS_PLAYING
                        && self.media_devices[3].data.is_some()
                    {
                        self.play_platform_media(3, cpu, services)?;
                        self.trace_platform_media("PLAY", 3, cpu)?;
                    } else {
                        cpu.set_register(0, 0);
                    }
                }
                // Stops and releases the file-backed multimedia player. Talkcat
                // issues both transitions before starting its face interaction.
                // The host has no separate prepared-player handle, so both
                // transitions idempotently clear the active sink; the device
                // slot mirrors the C++ transitions (STOP keeps the loaded
                // resource, CLOSE releases it).
                command @ (2_073 | 2_083)
                    if cpu.register(1) == 0
                        && cpu.register(2) == 0
                        && cpu.register(3) == 0
                        && self.memory.read_u32(GuestAddr(cpu.register(13)))? == 0
                        && self
                            .memory
                            .read_u32(GuestAddr(cpu.register(13)).checked_add(4)?)?
                            == 0 =>
                {
                    services.stop_sound()?;
                    if command == 2_073 {
                        self.media_devices[3].status = MEDIA_STATUS_LOADED;
                    } else {
                        self.media_devices[3] = PlatformMediaDevice::idle();
                    }
                    cpu.set_register(0, 0)
                }
                // Parameterless multimedia-state query. The 209x platform
                // contract returns MR_PLAT_VALUE_BASE (1000) + the device
                // state: mpmusic's player UI compares against 1004
                // (MR_MEDIA_PLAYING) and dispatches on status-999, matching
                // the platform PLAY_STATE enum (IDLE=1001 .. PAUSED=1005).
                // A PLAYING status whose playback already finished reads as
                // LOADED ("play finished" in the enum comment).
                2_093
                    if cpu.register(1) == 0
                        && cpu.register(2) == 0
                        && cpu.register(3) == 0
                        && self.memory.read_u32(GuestAddr(cpu.register(13)))? == 0
                        && self
                            .memory
                            .read_u32(GuestAddr(cpu.register(13)).checked_add(4)?)?
                            == 0 =>
                {
                    let mut status = self.media_devices[3].status;
                    if status == MEDIA_STATUS_PLAYING && !services.sound_is_active() {
                        status = MEDIA_STATUS_LOADED;
                    }
                    cpu.set_register(0, 1_000 + status);
                }
                // Vendor initialization notification with no input or output
                // buffers. Decoded per the media convention this is
                // MR_MEDIA_INIT (device 1): the device releases any resource
                // it still holds, mirroring the C++ INIT transition.
                2_011 if cpu.register(1) == 0 && cpu.register(2) == 0 && cpu.register(3) == 0 => {
                    self.init_platform_media(1, cpu, services)?;
                    self.trace_platform_media("INIT", 1, cpu)?;
                }
                // Optional vendor capability probe. The baseline headless profile
                // does not provide it, so report the ABI failure value.
                0x0009_0003
                    if cpu.register(1) == 0 && cpu.register(2) == 0 && cpu.register(3) == 0 =>
                {
                    cpu.set_register(0, u32::MAX)
                }
                // Observed optional vendor extension with an opaque input record
                // and no output buffer. This profile does not provide it.
                0x0009_0004
                    if cpu.register(1) != 0 && cpu.register(2) != 0 && cpu.register(3) == 0 =>
                {
                    cpu.set_register(0, u32::MAX)
                }
                // Optional vendor capability structure.
                0x0007_0001 if cpu.register(1) == 0 && cpu.register(2) == 0 => {
                    self.return_unavailable_platform_extension(cpu)?
                }
                // J2ME KVM 引擎注册（T023）：guest sub80B20 传入 8 字节
                // input 记录（平台表 + 31 号蹦床表指针，KVM 对象紧跟
                // input+8），期望引擎在 output 槽写回 vtable 指针、[r13]
                // 指向的 output_len 单元写 8。guest sub80D04 读 vtable 两槽
                // 后 BLX 调用：槽 0 = KVM_INIT、槽 1 = KVM_START（空槽会被
                // 跳过且视为成功，所以必须写非零表才能启动 KVM）。两槽指向
                // KVM 魔数窗口的保留地址，由执行循环拦截分发（kvm.rs）。
                0x0007_0003 if cpu.register(2) == 0 => {
                    // 取证（jsky 支持探测逆向）：v2060 应用列表以
                    // platEx(0x70003,0,0) 探测 J2ME 引擎，探测代码位于
                    // guest 堆内动态加载的 Thumb 镜像。触发时转储堆窗口
                    // 并打印模块/动态可执行段注册表，供离线反汇编。
                    if let Some(path) = std::env::var_os("SKYENGINE_DUMP_JSKY_PROBE") {
                        let lr = cpu.register(14);
                        match self.memory.read(GuestAddr(0x2000_0000), 0x20_0000) {
                            Ok(bytes) => {
                                let _ = std::fs::write(&path, &bytes);
                                eprintln!(
                                    "[jsky-probe] heap 2MB dumped to {path:?} (lr={lr:#010x})"
                                );
                            }
                            Err(error) => eprintln!("[jsky-probe] heap dump failed: {error}"),
                        }
                        for (index, context) in self.modules.iter().enumerate() {
                            eprintln!(
                                "[jsky-probe] module {index}: base={:#010x} len={:#x} dyn={}",
                                context.base.0,
                                context.len,
                                context.dynamic_executable_ranges.len()
                            );
                            for slot in &context.dynamic_executable_ranges {
                                eprintln!("[jsky-probe]   dyn {slot:?}");
                            }
                        }
                    }
                    let input = cpu.register(1);
                    let output = cpu.register(3);
                    if output == 0 {
                        // 无 output 槽可回写（含 input/output 双空形态）：按
                        // MR_FAILED(1) 拒绝；不返回 -1，避免 guest 落入
                        // "unsupported application" 终止分支。
                        if trace_arm {
                            eprintln!(
                                "[arm-platEx] command 0x00070003 rejected: input={input:#010x} output={output:#010x}"
                            );
                        }
                        cpu.set_register(0, 1);
                    } else {
                        let output_len =
                            GuestAddr(self.memory.read_u32(GuestAddr(cpu.register(13)))?);
                        self.memory.write_u32(GuestAddr(output), kvm::KVM_VTABLE)?;
                        if output_len.0 != 0 {
                            self.memory.write_u32(output_len, 8)?;
                        }
                        // KVM 对象指针经 input 记录传递（T025）：INIT 时回读
                        // input+8 处的包路径/jar 名，这里先缓存 guest 地址。
                        // input==0 是能力查询形态（v2060 应用列表探测 jsky
                        // 支持：guest 提供 output/output_len、input 留空，
                        // 只期望 vtable + MR_SUCCESS——返回非 0 会让列表
                        // 把 jsky 显示为未知并跳过 Java 区渲染），不缓存
                        // 注册；真正的启动仍走带 input 的注册调用。
                        if input != 0 {
                            self.kvm.note_registration(input);
                        }
                        // 取证差分（评分公式逆向）：SKYENGINE_DEBUG_JSKY_OFF=1
                        // 令查询形态回 MR_FAILED(1)，模拟无 J2ME 引擎的平台。
                        if input == 0 && std::env::var_os("SKYENGINE_DEBUG_JSKY_OFF").is_some() {
                            cpu.set_register(0, 1);
                        } else {
                            cpu.set_register(0, 0);
                        }
                    }
                }
                // Optional runtime profile query. The caller has a defined
                // fallback for values other than its two recognized profiles.
                4_033
                    if cpu.register(1) == 0
                        && cpu.register(2) == 0
                        && cpu.register(3) == 0
                        && self.memory.read_u32(GuestAddr(cpu.register(13)))? == 0 =>
                {
                    cpu.set_register(0, u32::MAX)
                }
                // MIDI media FILE_LOAD (ACI device 1): the guest names a MIDI
                // resource; the runtime stores it and only starts playback at
                // the PLAY request, where the loop flag arrives (dsm.c
                // dsm_media_file_load keeps the data until dsm_media_play).
                2_021 => {
                    self.load_platform_media_file(1, cpu, services)?;
                    self.trace_platform_media("FILE_LOAD", 1, cpu)?;
                }
                // MIDI media BUF_LOAD: the caller supplies encoded bytes
                // inline instead of a resource path.
                2_031 => {
                    self.load_platform_media_buffer(1, cpu)?;
                    self.trace_platform_media("BUF_LOAD", 1, cpu)?;
                }
                // MIDI media PLAY/RESUME: plays the loaded resource with the
                // loop flag from the twelve-byte T_DSM_MEDIA_PLAY record.
                2_041 | 2_061 => {
                    self.play_platform_media(1, cpu, services)?;
                    self.trace_platform_media("PLAY", 1, cpu)?;
                }
                // MIDI media PAUSE/STOP: clears the active sink and returns
                // to the loaded state, preserving the stored resource (the
                // legacy C++ transition; a real pause is only available on
                // the shared media state machine, which the MIDI arms do
                // not use).
                2_051 | 2_071 => {
                    self.stop_platform_media(1, cpu, services)?;
                    self.trace_platform_media("STOP", 1, cpu)?;
                }
                // MIDI media CLOSE/FREE: clears the active sink and releases
                // the stored resource.
                2_081 | 2_161 => {
                    self.close_platform_media(1, cpu, services)?;
                    self.trace_platform_media("CLOSE", 1, cpu)?;
                }
                // MIDI media GET_STATUS: reports the device state both
                // through the optional output cell and as the return value.
                2_091 => {
                    self.return_platform_media_status(module, 1, cpu)?;
                    self.trace_platform_media("GET_STATUS", 1, cpu)?;
                }
                // MIDI media position/time queries: report the live playback
                // progress through the audio service (units per command).
                command @ (2_101 | 2_111 | 2_121 | 2_131 | 2_151) => {
                    self.return_platform_media_time(module, command, cpu, services)?;
                    self.trace_platform_media("TIME_QUERY", 1, cpu)?;
                }
                // platEx 222x multi-channel load: input is a T_BG_PLAY_REQ
                // record ({uint8* data; uint32 len; int32 loop}, 12 bytes)
                // and x is the T_MEDIA_TYPE of the buffer. Returns a channel
                // handle for the 223x/224x/225x family (rzsgqd's fruit-slice
                // effects load their WAVs through 2222).
                command @ 2_221..=2_229 => {
                    self.load_platform_sound_channel(command, cpu, services)?;
                }
                // platEx 223x/224x/225x multi-channel play/stop/release:
                // input points at the 4-byte handle returned by 222x. Play
                // restarts the voice from the beginning, stop rewinds it but
                // keeps the decoded samples, release drops them.
                command @ (2_231..=2_239 | 2_241..=2_249 | 2_251..=2_259) => {
                    self.control_platform_sound_channel(command, cpu, services)?;
                }
                // Remaining media-family commands for the other ACI devices
                // (2=WAV, 3=MP3, 4=AMR, 5=PCM, 6=M4A, 7=AMR-WB) share the
                // C++ dsm_media_platEx state machine. The device-3 codes with
                // standalone player arms above (2013/2023/2043/2073/2083/
                // 2093) keep their rejected shapes falling through to the
                // catch-all, and device codes beyond 7 are rejected by
                // dsm_media_get in the C++ platform the same way.
                command
                    if (command % 10) >= 1
                        && (command % 10) <= 7
                        && !(command % 10 == 3
                            && matches!(command / 10, 201 | 202 | 204 | 207 | 208 | 209))
                        && matches!(
                            command / 10,
                            201 | 202
                                | 203
                                | 204
                                | 205
                                | 206
                                | 207
                                | 208
                                | 209
                                | 210
                                | 211
                                | 212
                                | 213
                                | 215
                                | 216
                        ) =>
                {
                    self.dispatch_platform_media(command, module, cpu, services)?;
                }
                command => {
                    // Commands with arms above keep the strict verified
                    // contract: a request whose shape the arm rejected is a
                    // loud error rather than a silent fallback.
                    let known = matches!(
                        command,
                        1_014
                            | 1_015
                            | 1_017
                            | 1_116
                            | 1_204
                            | 1_207
                            | 1_222
                            | 1_223
                            | 1_224
                            | 1_307
                            | 1_324
                            | 2_013
                            | 2_023
                            | 2_043
                            | 2_073
                            | 2_083
                            | 2_093
                            | 3_001
                            | 3_002
                            | 3_004
                            | 3_005
                            | 4_033
                            | 0x0007_0001
                            | 0x0009_0003
                            | 0x0009_0004
                    );
                    if known {
                        let input_len = (cpu.register(2) as usize).min(32);
                        let input = self
                            .memory
                            .read(GuestAddr(cpu.register(1)), input_len)
                            .map(|bytes| format!("{bytes:02x?}"))
                            .unwrap_or_else(|error| format!("unavailable: {error}"));
                        let stack = (0..2)
                            .map(|index| {
                                self.memory.read_u32(GuestAddr(
                                    cpu.register(13).wrapping_add(index * 4),
                                ))
                            })
                            .collect::<skyengine_arm::Result<Vec<_>>>()
                            .map(|words| format!("{words:08x?}"))
                            .unwrap_or_else(|error| format!("unavailable: {error}"));
                        return Err(Error::Abi(format!(
                            "unsupported platform slot 38 command {command} called by module {module} (input={input}, stack={stack})"
                        )));
                    }
                    // Any other code is unknown to the C++ platform as well:
                    // its mr_platEx default answers MR_IGNORE (1), telling the
                    // guest to use its own fallback, and the call is not
                    // fatal. The output cells are left untouched.
                    if trace_arm {
                        eprintln!(
                            "[arm-platEx] ignored unsupported command {command} from module {module}"
                        );
                    }
                    cpu.set_register(0, 1);
                }
            },
            40 => {
                let name = self.read_c_string(GuestAddr(cpu.register(0)), 1024)?;
                let mode = cpu.register(1);
                let result = services.open_file(&name, mode)?;
                if fileio_trace() {
                    eprintln!("[fileio] open {name:?} mode={mode:#x} -> {result}");
                }
                if trace_arm {
                    eprintln!(
                        "[arm-open] name={name:?} mode={mode:#x} -> {result}",
                    );
                }
                cpu.set_register(0, native_file_open_result(result));
            }
            41 => {
                cpu.set_register(0, services.close_file(cpu.register(0) as i32)? as u32);
            }
            42 => {
                let name = self.read_c_string(GuestAddr(cpu.register(0)), 1024)?;
                let result = services.file_info(&name)?;
                cpu.set_register(0, result as u32);
            }
            43 => {
                let handle = cpu.register(0) as i32;
                let input = GuestAddr(cpu.register(1));
                let len = cpu.register(2) as i32;
                if handle < 0 || input.0 == 0 || len < 0 {
                    cpu.set_register(0, u32::MAX);
                    return Ok(());
                }
                let bytes = self.memory.read(input, len as usize)?;
                let written = services
                    .write_file(handle, &bytes)?
                    .and_then(|written| u32::try_from(written).ok())
                    .unwrap_or(u32::MAX);
                if fileio_trace() {
                    let head: Vec<String> = bytes.iter().take(12).map(|b| format!("{b:02x}")).collect();
                    eprintln!("[fileio] write fd={handle} len={len} -> {written} head=[{}]", head.join(" "));
                }
                cpu.set_register(0, written);
            }
            44 => {
                let handle = cpu.register(0) as i32;
                let destination = GuestAddr(cpu.register(1));
                let len = cpu.register(2) as usize;
                match services.read_file(handle, len)? {
                    Some(bytes) => {
                        self.memory.write(destination, &bytes)?;
                        if trace_arm {
                            let head: Vec<u8> = bytes.iter().copied().take(8).collect();
                            eprintln!(
                                "[arm-read] fd={handle} len={len} -> {} bytes: {head:02x?}",
                                bytes.len()
                            );
                        }
                        cpu.set_register(0, bytes.len() as u32);
                    }
                    None => cpu.set_register(0, u32::MAX),
                }
            }
            45 => {
                let handle = cpu.register(0) as i32;
                let offset = cpu.register(1) as i32;
                let whence = cpu.register(2);
                let position = services.seek_file(handle, offset, whence)?;
                if fileio_trace() {
                    eprintln!(
                        "[fileio] seek fd={handle} offset={offset} ({offset:#x}) whence={whence} -> {position:?}"
                    );
                }
                if trace_arm {
                    eprintln!(
                        "[arm-seek] fd={handle} offset={offset} ({offset:#x}) whence={whence} -> {}",
                        if position.is_some() { "ok" } else { "FAIL" }
                    );
                }
                cpu.set_register(0, if position.is_some() { 0 } else { u32::MAX });
            }
            46 => {
                let name = self.read_c_string(GuestAddr(cpu.register(0)), 1024)?;
                let result = services.file_len(&name)?;
                cpu.set_register(
                    0,
                    result
                        .and_then(|len| u32::try_from(len).ok())
                        .unwrap_or(u32::MAX),
                );
            }
            47 => {
                let name = self.read_c_string(GuestAddr(cpu.register(0)), 1024)?;
                let result = services.remove_file(&name)?;
                if trace_arm {
                    eprintln!("[arm-remove] name={name:?} -> {result}");
                }
                cpu.set_register(0, result as u32);
            }
            48 => {
                let source = self.read_c_string(GuestAddr(cpu.register(0)), 1024)?;
                let destination = self.read_c_string(GuestAddr(cpu.register(1)), 1024)?;
                cpu.set_register(0, services.rename_file(&source, &destination)? as u32);
            }
            49 | 50 => {
                let name = self.read_c_string(GuestAddr(cpu.register(0)), 1024)?;
                let result = if slot == 49 {
                    services.create_dir(&name)?
                } else {
                    services.remove_dir(&name)?
                };
                cpu.set_register(0, result as u32);
            }
            51 => {
                let directory = self.read_c_string(GuestAddr(cpu.register(0)), 1024)?;
                let output = GuestAddr(cpu.register(1));
                let output_len = cpu.register(2) as usize;
                match services.find_start(&directory)? {
                    Some((handle, entry))
                        if self.write_directory_entry(output, output_len, &entry)? =>
                    {
                        cpu.set_register(0, handle as u32);
                    }
                    Some((handle, _)) => {
                        services.find_stop(handle)?;
                        cpu.set_register(0, u32::MAX);
                    }
                    None => cpu.set_register(0, u32::MAX),
                }
            }
            52 => {
                let handle = cpu.register(0) as i32;
                let output = GuestAddr(cpu.register(1));
                let output_len = cpu.register(2) as usize;
                let succeeded = match services.find_next(handle)? {
                    Some(entry) => self.write_directory_entry(output, output_len, &entry)?,
                    None => false,
                };
                cpu.set_register(0, if succeeded { 0 } else { u32::MAX });
            }
            53 => {
                let succeeded = services.find_stop(cpu.register(0) as i32)?;
                cpu.set_register(0, if succeeded { 0 } else { u32::MAX });
            }
            54 => {
                eprintln!("[exit] guest requested exit (mr_stop)");
                self.exit_requested = true;
                cpu.set_register(0, 0);
            }
            55 => {
                services.start_shake(cpu.register(0))?;
                cpu.set_register(0, 0);
            }
            56 => {
                services.stop_shake()?;
                cpu.set_register(0, 0);
            }
            57 => {
                let sound_type = cpu.register(0);
                let sound = GuestAddr(cpu.register(1));
                let len = cpu.register(2) as usize;
                let looped = cpu.register(3);
                let Some(sound_type) = SoundType::from_mrp(sound_type) else {
                    return Err(Error::Abi(format!(
                        "unsupported headless sound request (type {}, address {:#010x}, len {len}, looped {looped}) called by module {module}",
                        cpu.register(0),
                        sound.0,
                    )));
                };
                if sound.0 == 0 || len == 0 || !matches!(looped, 0 | 1 | u32::MAX) {
                    return Err(Error::Abi(format!(
                        "unsupported headless sound request (type {}, address {:#010x}, len {len}, looped {looped}) called by module {module}",
                        cpu.register(0),
                        sound.0,
                    )));
                }
                let data = self.memory.read(sound, len)?;
                let succeeded = services.play_sound(sound_type, &data, looped != 0).is_ok();
                cpu.set_register(0, if succeeded { 0 } else { u32::MAX });
            }
            58 => {
                services.stop_sound()?;
                cpu.set_register(0, 0);
            }
            59 => {
                let number = GuestAddr(cpu.register(0));
                let message = GuestAddr(cpu.register(1));
                let message_len = cpu.register(2) as usize;
                if number.0 == 0 || message.0 == 0 || message_len == 0 || message_len > 64 * 1024 {
                    cpu.set_register(0, u32::MAX);
                    return Ok(());
                }
                let number = self.read_c_string(number, 64)?;
                if number.is_empty() {
                    cpu.set_register(0, u32::MAX);
                    return Ok(());
                }
                // Consume the bounded request without exposing a host messaging
                // capability. The deterministic headless provider reports both
                // synchronous acceptance and an asynchronous successful result.
                let _ = self.memory.read(message, message_len)?;
                if let Some(context) = self.modules.get(module) {
                    if self.pending_sms_results.len() >= MAX_PENDING_SMS_RESULTS {
                        cpu.set_register(0, u32::MAX);
                        return Ok(());
                    }
                    if let Some(helper) = context.helper {
                        self.pending_sms_results.push_back(PendingSmsResult {
                            owner_generation: context.generation,
                            helper,
                            result: 0,
                        });
                    }
                }
                cpu.set_register(0, 0);
            }
            61 => {
                // The offline profile still exposes a deterministic default
                // network identity; connectivity is reported by socket calls.
                self.trace_network("getNetworkID".into());
                cpu.set_register(0, 0);
            }
            62 => {
                // mr_connectWAP: baseline profiles expose no WAP/HTTP entry
                // point. Consume the bounded request and answer with the ABI
                // failure value instead of fabricating a browser round trip
                // or leaking the guest URL into a host network stack.
                let url = GuestAddr(cpu.register(0));
                let url_text = if url.0 != 0 {
                    self.read_c_string(url, 1024)?
                } else {
                    Vec::new()
                };
                self.trace_network(format!(
                    "connectWAP url={:?} -> FAIL",
                    String::from_utf8_lossy(&url_text)
                ));
                cpu.set_register(0, u32::MAX);
            }
            63 => {
                let title = self.read_wide_string_be(GuestAddr(cpu.register(0)), 1024)?;
                let item_count = cpu.register(1) as usize;
                let handle = self.create_platform_menu(title, item_count)?;
                cpu.set_register(0, handle);
            }
            64 => {
                let handle = cpu.register(0);
                let text = self.read_wide_string_be(GuestAddr(cpu.register(1)), 1024)?;
                let index = cpu.register(2) as usize;
                let succeeded = self.set_platform_menu_item(handle, index, text);
                cpu.set_register(0, if succeeded { 0 } else { u32::MAX });
            }
            65 => {
                let succeeded = self.show_platform_menu(cpu.register(0), services)?;
                cpu.set_register(0, if succeeded { 0 } else { u32::MAX });
            }
            67 => {
                let succeeded = self.release_platform_menu(cpu.register(0), services)?;
                cpu.set_register(0, if succeeded { 0 } else { u32::MAX });
            }
            68 => {
                let succeeded = self.refresh_platform_menu(cpu.register(0), services)?;
                cpu.set_register(0, if succeeded { 0 } else { u32::MAX });
            }
            69 => {
                let title = self.read_wide_string_be(GuestAddr(cpu.register(0)), 1024)?;
                let message = self.read_wide_string_be(GuestAddr(cpu.register(1)), 16 * 1024)?;
                let style = cpu.register(2);
                let handle = self.create_platform_dialog(&title, &message, style, services)?;
                cpu.set_register(0, handle);
            }
            70 => {
                let succeeded = self.release_platform_dialog(cpu.register(0), services)?;
                cpu.set_register(0, if succeeded { 0 } else { u32::MAX });
            }
            71 => {
                let handle = cpu.register(0);
                let Some(dialog) = self.dialogs.get(&handle) else {
                    cpu.set_register(0, u32::MAX);
                    return Ok(());
                };
                let screen = dialog.dialog_screen.clone();
                self.memory.write(self.screen_base, &screen)?;
                self.present_screen(services)?;
                cpu.set_register(0, 0);
            }
            72 => {
                let title = self.read_wide_string_be(GuestAddr(cpu.register(0)), 1024)?;
                let text = self.read_wide_string_be(GuestAddr(cpu.register(1)), 16 * 1024)?;
                let style = cpu.register(2);
                let handle = self.create_platform_text_viewer(&title, &text, style, services)?;
                cpu.set_register(0, handle);
            }
            73 => {
                let succeeded = self.release_platform_text_viewer(cpu.register(0), services)?;
                cpu.set_register(0, if succeeded { 0 } else { u32::MAX });
            }
            74 => {
                let succeeded = self.refresh_platform_text_viewer(cpu.register(0), services)?;
                cpu.set_register(0, if succeeded { 0 } else { u32::MAX });
            }
            75 => {
                let max_code_units = cpu.register(3) as usize;
                if max_code_units > MAX_PLATFORM_EDITOR_CODE_UNITS {
                    return Err(Error::ResourceLimit(format!(
                        "platform editor requested {max_code_units} code units (limit {MAX_PLATFORM_EDITOR_CODE_UNITS})"
                    )));
                }
                let title_address = GuestAddr(cpu.register(0));
                let text_address = GuestAddr(cpu.register(1));
                let title = if title_address.0 == 0 {
                    Vec::new()
                } else {
                    self.read_wide_string_be(title_address, 1024)?
                };
                let text = if text_address.0 == 0 {
                    Vec::new()
                } else {
                    self.read_wide_string_be(text_address, max_code_units.saturating_add(1))?
                };
                let handle = self.create_platform_editor(
                    module,
                    title,
                    text,
                    cpu.register(2),
                    max_code_units,
                )?;
                cpu.set_register(0, handle);
            }
            76 => {
                let succeeded = self.release_platform_editor(module, cpu.register(0))?;
                cpu.set_register(0, if succeeded { 0 } else { u32::MAX });
            }
            77 => {
                let text = self.platform_editor_text(module, cpu.register(0))?;
                cpu.set_register(0, text.map_or(0, |address| address.0));
            }
            78 => {
                // Headless windows are opaque lifetime tokens. The guest requires
                // a positive handle even though no host-native window is created.
                cpu.set_register(0, self.create_native_window(module)?);
            }
            79 => {
                let succeeded = self.release_native_window(module, cpu.register(0))?;
                cpu.set_register(0, if succeeded { 0 } else { u32::MAX });
            }
            80 => {
                let info = GuestAddr(cpu.register(0));
                let width = self.memory.read_u32(data_slot_address(92))?;
                let height = self.memory.read_u32(data_slot_address(93))?;
                self.memory.write_u32(info, width)?;
                self.memory.write_u32(info.checked_add(4)?, height)?;
                // Vendor mr_screeninfo is {width, height, bit}; the bit depth
                // is always 16 (RGB565) and guests such as the AeroKVM ext
                // abort init unless the third field reads 0x10.
                self.memory.write_u32(info.checked_add(8)?, 16)?;
                cpu.set_register(0, 0);
            }
            81 => {
                // Initializing the network service does not imply that a link is
                // available. Later DNS/socket operations report connectivity.
                self.trace_network("networkInit -> 0".into());
                cpu.set_register(0, 0);
            }
            82 => {
                self.trace_network("closeAllSockets".into());
                self.native_sockets.clear();
                cpu.set_register(0, 0);
            }
            83 => {
                let name = self.read_c_string(GuestAddr(cpu.register(0)), 1024)?;
                let resolved = self.resolve_mapped_host(&name);
                self.trace_network(format!(
                    "getHostByName {:?} -> {}",
                    String::from_utf8_lossy(&name),
                    resolved.map_or_else(
                        || "FAIL".into(),
                        |ip| std::net::Ipv4Addr::from(ip.to_be_bytes()).to_string()
                    )
                ));
                cpu.set_register(0, resolved.unwrap_or(u32::MAX));
            }
            84 => {
                let socket_type = cpu.register(0);
                let protocol = cpu.register(1);
                let handle = if socket_type == 0 && protocol == 0 {
                    self.allocate_native_socket_handle()?
                } else {
                    None
                };
                self.trace_network(format!(
                    "socketCreate type={socket_type} protocol={protocol} -> {}",
                    handle.map_or("FAIL".to_string(), |handle| format!("handle={handle}"))
                ));
                cpu.set_register(0, handle.map_or(u32::MAX, |handle| handle as u32));
            }
            85 => {
                let handle = cpu.register(0) as i32;
                let (ip, port) = self.route_mapped_endpoint(cpu.register(1), cpu.register(2));
                let timeout = cpu.register(3);
                let result = self.connect_native_socket(handle, ip, port, timeout);
                self.trace_network(format!(
                    "connect handle={handle} ip={} port={port} timeout={timeout} -> {result}",
                    std::net::Ipv4Addr::from(ip.to_be_bytes())
                ));
                cpu.set_register(0, result as u32);
            }
            86 => {
                let handle = cpu.register(0) as i32;
                let result: i32 = if self.native_sockets.remove(&handle).is_some() {
                    0
                } else {
                    -1
                };
                self.trace_network(format!("close handle={handle} -> {result}"));
                cpu.set_register(0, result as u32);
            }
            87 => {
                let handle = cpu.register(0) as i32;
                let output = GuestAddr(cpu.register(1));
                let len = cpu.register(2) as usize;
                let bytes = self.receive_native_socket(handle, len);
                self.trace_network(format!(
                    "recv handle={handle} len={len} -> {}",
                    bytes
                        .as_ref()
                        .map_or("FAIL".to_string(), |bytes| format!("{} bytes", bytes.len()))
                ));
                match bytes {
                    Some(bytes) => {
                        self.memory.write(output, &bytes)?;
                        cpu.set_register(0, bytes.len() as u32);
                    }
                    None => cpu.set_register(0, u32::MAX),
                }
            }
            88 | 90 => {
                // Datagram sockets are not part of the baseline stream profile.
                self.trace_network(format!("datagram slot={slot} -> FAIL"));
                cpu.set_register(0, u32::MAX);
            }
            89 => {
                let handle = cpu.register(0) as i32;
                let len = cpu.register(2) as usize;
                let bytes = self.memory.read(GuestAddr(cpu.register(1)), len)?;
                let written = self.send_native_socket(handle, &bytes);
                self.trace_network(format!(
                    "send handle={handle} len={len} -> {}",
                    written.map_or("FAIL".to_string(), |written| format!("{written} bytes"))
                ));
                cpu.set_register(0, written.map_or(u32::MAX, |written| written as u32));
            }
            113 => {
                self.md5_init(GuestAddr(cpu.register(0)))?;
                cpu.set_register(0, 0);
            }
            114 => {
                let context = GuestAddr(cpu.register(0));
                let input = self
                    .memory
                    .read(GuestAddr(cpu.register(1)), cpu.register(2) as usize)?;
                self.md5_append(context, &input)?;
                cpu.set_register(0, 0);
            }
            115 => {
                self.md5_finish(GuestAddr(cpu.register(0)), GuestAddr(cpu.register(1)))?;
                cpu.set_register(0, 0);
            }
            118 => {
                // _DispUpEx(int16 x, int16 y, uint16 w, uint16 h): present the
                // guest screen-buffer region to the host display, mirroring
                // `mr_drawBitmap(mr_screenBuf, x, y, w, h)`; regions outside
                // the screen clip away and the call always returns 0.
                let x = cpu.register(0) as i16 as i32;
                let y = cpu.register(1) as i16 as i32;
                let width = cpu.register(2) as u16 as usize;
                let height = cpu.register(3) as u16 as usize;
                let region = self.read_platform_draw_pixels(self.screen_base, x, y, width, height)?;
                if region.width > 0 && region.height > 0 {
                    self.draw_platform_bitmap(
                        &region.pixels,
                        region.x,
                        region.y,
                        region.width,
                        region.height,
                        services,
                    )?;
                }
                cpu.set_register(0, 0);
            }
            119 => {
                let (width, height) = self.screen_dimensions()?;
                self.write_screen_pixel(
                    cpu.register(0) as i32,
                    cpu.register(1) as i32,
                    cpu.register(2) as u16,
                    width,
                    height,
                )?;
                cpu.set_register(0, 0);
            }
            120 => {
                let source = GuestAddr(cpu.register(0));
                let x = cpu.register(1) as i32;
                let y = cpu.register(2) as i32;
                let width = cpu.register(3) as usize;
                let stack = GuestAddr(cpu.register(13));
                let height = self.memory.read_u32(stack)? as usize;
                let mode = self.memory.read_u32(stack.checked_add(4)?)?;
                let transparent_color = self.memory.read_u32(stack.checked_add(8)?)? as u16;
                // The C++ slot reads the source offsets and stride as int16:
                // a negative offset crops the sprite instead of addressing
                // before its base (aex_table.c aex_t120 passes them straight
                // to arm_ext_draw_bitmap_from_guest, which skips pixels whose
                // source index is negative).
                let source_x = self.memory.read_u32(stack.checked_add(12)?)? as u16 as i16 as i64;
                let source_y = self.memory.read_u32(stack.checked_add(16)?)? as u16 as i16 as i64;
                let source_stride =
                    self.memory.read_u32(stack.checked_add(20)?)? as u16 as i16 as i64;
                // A null source pointer draws nothing, like the C++ helper.
                // Vendor rop decoding (arm_ext_draw_bitmap_from_guest): the
                // sprite path (rop > 0x400) rotates/flips the source and only
                // accepts the COPY/TRANSPARENT raster operations; plain rops
                // 10..=0x400 fall through arm_ext_bitmap_apply_rop's default
                // arm, which draws nothing without failing.
                if mode > SPRITE_FLAG && source.0 != 0 {
                    let bitmap_rop = mode & SPRITE_INDEX_MASK;
                    if bitmap_rop == 2 || bitmap_rop == 6 {
                        let draw_mode = if bitmap_rop == 2 {
                            BitmapDrawMode::Copy
                        } else {
                            BitmapDrawMode::Transparent(transparent_color)
                        };
                        let rotation = (mode >> TILE_SHIFT) & 0x3;
                        let flip = (mode >> TILE_SHIFT) & 0x4 != 0;
                        let pixels = self.read_sprite_bitmap_source(
                            source,
                            width,
                            height,
                            rotation,
                            flip,
                        )?;
                        self.draw_masked_bitmap_region_to_screen(
                            &pixels, x, y, width, height, draw_mode,
                        )?;
                    }
                } else if source.0 != 0 {
                    let draw_mode = match mode {
                        0 => BitmapDrawMode::Or,
                        1 => BitmapDrawMode::Xor,
                        2 => BitmapDrawMode::Copy,
                        3 => BitmapDrawMode::Not,
                        4 => BitmapDrawMode::MergeNot,
                        5 => BitmapDrawMode::AndNot,
                        6 => BitmapDrawMode::Transparent(transparent_color),
                        7 => BitmapDrawMode::And,
                        8 => BitmapDrawMode::Gray(transparent_color),
                        9 => BitmapDrawMode::Reverse(transparent_color),
                        10..=SPRITE_FLAG => {
                            // Vendor apply_rop default: draw nothing.
                            cpu.set_register(0, 0);
                            return Ok(());
                        }
                        _ => {
                            return Err(Error::Abi(format!(
                                "unsupported bitmap drawing mode {mode} called by module {module}"
                            )));
                        }
                    };
                    let byte_len = width
                        .checked_mul(height)
                        .and_then(|pixels| pixels.checked_mul(2))
                        .ok_or_else(|| {
                            Error::Abi("bitmap source byte count overflow".into())
                        })?;
                    if byte_len > self.heap_len {
                        return Err(Error::Abi(format!(
                            "bitmap source region requires {byte_len} bytes"
                        )));
                    }
                    if source_x < 0 || source_y < 0 || source_stride < 0 {
                        // Negative offsets route through the masked path: each
                        // pixel whose source index is negative or unreadable
                        // leaves the destination untouched.
                        let pixels = self.read_masked_bitmap_source(
                            source,
                            width,
                            height,
                            source_x,
                            source_y,
                            source_stride,
                        )?;
                        self.draw_masked_bitmap_region_to_screen(
                            &pixels, x, y, width, height, draw_mode,
                        )?;
                    } else {
                        let source_x = source_x as usize;
                        let source_y = source_y as usize;
                        let source_stride = source_stride as usize;
                        let source_end_y = source_y
                            .checked_add(height)
                            .ok_or_else(|| {
                                Error::Abi("bitmap source height overflow".into())
                            })?;
                        let pixels = if source_x == 0 && width == source_stride {
                            let byte_offset = source_y
                                .checked_mul(source_stride)
                                .and_then(|offset| offset.checked_mul(2))
                                .and_then(|offset| u32::try_from(offset).ok())
                                .ok_or_else(|| {
                                    Error::Abi("bitmap source offset overflow".into())
                                })?;
                            self.memory
                                .read(source.checked_add(byte_offset)?, byte_len)?
                        } else {
                            let row_len = width
                                .checked_mul(2)
                                .ok_or_else(|| {
                                    Error::Abi("bitmap source row overflow".into())
                                })?;
                            let mut pixels = Vec::with_capacity(byte_len);
                            for row in source_y..source_end_y {
                                let byte_offset = row
                                    .checked_mul(source_stride)
                                    .and_then(|offset| offset.checked_add(source_x))
                                    .and_then(|offset| offset.checked_mul(2))
                                    .and_then(|offset| u32::try_from(offset).ok())
                                    .ok_or_else(|| {
                                        Error::Abi("bitmap source offset overflow".into())
                                    })?;
                                pixels.extend_from_slice(
                                    &self
                                        .memory
                                        .read(source.checked_add(byte_offset)?, row_len)?,
                                );
                            }
                            pixels
                        };
                        self.draw_bitmap_region_to_screen(
                            &pixels, x, y, width, height, draw_mode,
                        )?;
                    }
                }
                cpu.set_register(0, 0);
            }
            121 => {
                let source = self.read_bitmap_descriptor(GuestAddr(cpu.register(0)))?;
                let destination = self.read_bitmap_descriptor(GuestAddr(cpu.register(1)))?;
                let stack = GuestAddr(cpu.register(13));
                let transform_address = GuestAddr(self.memory.read_u32(stack)?);
                let transform = self.read_bitmap_transform(transform_address)?;
                let transparent_color = self.memory.read_u32(stack.checked_add(4)?)? as u16;
                self.copy_transformed_bitmap(
                    destination,
                    source,
                    cpu.register(2) as usize,
                    cpu.register(3) as usize,
                    transform,
                    transparent_color,
                    module,
                )?;
                cpu.set_register(0, 0);
            }
            122 => {
                let stack = GuestAddr(cpu.register(13));
                let color = Framebuffer::rgb565(
                    self.memory.read_u32(stack)? as i32,
                    self.memory.read_u32(stack.checked_add(4)?)? as i32,
                    self.memory.read_u32(stack.checked_add(8)?)? as i32,
                );
                let x = cpu.register(0) as i32;
                let y = cpu.register(1) as i32;
                let width = cpu.register(2) as i32;
                let height = cpu.register(3) as i32;
                self.draw_rectangle_to_screen(x, y, width, height, color)?;
                cpu.set_register(0, 0);
                // The vendor routine clobbers this volatile argument register.
                // Some legacy callers rely on that before omitting a later r3 argument.
                cpu.set_register(3, 0);
            }
            123 => {
                // 取证（信息窗口逆向）：窗口打开时恰好一次 drawText，此刻
                // 转储 guest 堆，可截获窗口已格式化的文本缓冲（如
                // "手机配置NN分"）。
                if let Some(path) = std::env::var_os("SKYENGINE_DUMP_ON_DRAWTEXT") {
                    match self.memory.read(GuestAddr(0x2000_0000), 0x80_0000) {
                        Ok(bytes) => {
                            let _ = std::fs::write(&path, &bytes);
                            eprintln!("[drawtext-dump] heap 8MB -> {path:?}");
                        }
                        Err(error) => eprintln!("[drawtext-dump] failed: {error}"),
                    }
                }
                let stack = GuestAddr(cpu.register(13));
                let is_unicode = self.memory.read_u32(stack.checked_add(8)?)?;
                let font = self.memory.read_u32(stack.checked_add(12)?)?;
                if font > 2 {
                    return Err(Error::Abi(format!(
                        "unsupported text drawing font {font} called by module {module}"
                    )));
                }
                let text_address = GuestAddr(cpu.register(0));
                let text = if text_address.0 == 0 {
                    Vec::new()
                } else if is_unicode == 0 {
                    let encoded = self.read_c_string(text_address, 64 * 1024)?;
                    let (decoded, _, _) = encoding_rs::GBK.decode(&encoded);
                    decoded.encode_utf16().collect()
                } else {
                    self.read_wide_string_be(text_address, 64 * 1024)?
                };
                let color = Framebuffer::rgb565(
                    cpu.register(3) as i32,
                    self.memory.read_u32(stack)? as i32,
                    self.memory.read_u32(stack.checked_add(4)?)? as i32,
                );
                if trace_arm {
                    eprintln!(
                        "[arm-draw-text] x={} y={} font={} text={:?}",
                        cpu.register(1),
                        cpu.register(2),
                        font,
                        String::from_utf16(&text)
                    );
                }
                self.draw_text_to_screen(
                    &text,
                    cpu.register(1) as i32,
                    cpu.register(2) as i32,
                    color,
                    font,
                    services,
                )?;
                cpu.set_register(0, 0);
            }
            125 => {
                let name = self.read_c_string(GuestAddr(cpu.register(0)), 1024)?;
                let ram_address = self.memory.read_u32(data_slot_address(104))?;
                let ram_len = self.memory.read_u32(data_slot_address(105))? as usize;
                if (ram_address == 0) != (ram_len == 0) {
                    return Err(Error::Abi(format!(
                        "RAM-backed MRP has inconsistent address {ram_address:#010x} and length {ram_len}"
                    )));
                }
                let use_ram_package = if ram_address == 0 {
                    false
                } else {
                    ram_len >= 24
                        && self.memory.read(GuestAddr(ram_address), 4)?.as_slice() == b"MRPG"
                };
                let bytes = if !use_ram_package {
                    let package_name = self.read_c_string(PACKAGE_NAME_DATA, 256)?;
                    services.read_package_file(&package_name, &name)?
                } else {
                    self.read_ram_package_file(GuestAddr(ram_address), ram_len, &name)?
                };
                if trace_arm {
                    eprintln!(
                        "[arm-package] name={:?} ram={ram_address:#010x}+{ram_len:#x} result_len={:?}",
                        String::from_utf8_lossy(&name),
                        bytes.as_ref().map(Vec::len),
                    );
                }
                let Some(bytes) = bytes else {
                    let len_pointer = GuestAddr(cpu.register(1));
                    if len_pointer.0 != 0 {
                        self.memory.write_u32(len_pointer, 0)?;
                    }
                    cpu.set_register(0, 0);
                    return Ok(());
                };
                let prepared_output = if use_ram_package {
                    self.compact_ram_output_target(
                        GuestAddr(ram_address),
                        ram_len,
                        bytes.len(),
                        module,
                        GuestAddr(cpu.register(1)),
                    )?
                } else {
                    None
                };
                let output = match prepared_output {
                    Some(output) => output,
                    None => {
                        let Some(output) =
                            self.allocate_guest_block_for_module(bytes.len(), module)?
                        else {
                            let len_pointer = GuestAddr(cpu.register(1));
                            if len_pointer.0 != 0 {
                                self.memory.write_u32(len_pointer, 0)?;
                            }
                            cpu.set_register(0, 0);
                            return Ok(());
                        };
                        output
                    }
                };
                if prepared_output.is_some() {
                    let block_len = heap::aligned_heap_len(bytes.len())?;
                    self.claim_prepared_output_for_module(output, block_len, module)?;
                }
                self.memory.write(output, &bytes)?;
                let len_pointer = GuestAddr(cpu.register(1));
                if len_pointer.0 != 0 {
                    self.memory.write_u32(len_pointer, bytes.len() as u32)?;
                }
                cpu.set_register(0, output.0);
            }
            126 => {
                // wstrlen(const char* txt) (mythroad/other.c): the byte
                // length of a NUL-terminated UTF-16LE wide string, i.e. it
                // walks two-byte steps while either byte of the pair is
                // non-zero and returns the offset of the wide terminator.
                // A null pointer measures as an empty string.
                let text = GuestAddr(cpu.register(0));
                let mut len = 0u32;
                if text.0 != 0 {
                    while self.memory.read_u16(text.checked_add(len)?)? != 0 {
                        len = len.checked_add(2).ok_or_else(|| {
                            Error::Abi("wstrlen length overflow".into())
                        })?;
                    }
                }
                cpu.set_register(0, len);
            }
            128 => {
                // DrawTextEx(pcText, x, y, rect, color, flag, font): the
                // eight-byte mr_screenRectSt splits across r3 and the first
                // stack word exactly like the vendor ABI (r3 packs
                // rect.x/rect.y, stack[0] packs rect.w/rect.h), then the
                // three-byte mr_colourSt, flag and font ride on the stack.
                let stack = GuestAddr(cpu.register(13));
                let rect_tail = self.memory.read_u32(stack)?;
                let color_word = self.memory.read_u32(stack.checked_add(4)?)?;
                let flag = self.memory.read_u32(stack.checked_add(8)?)?;
                let font = self.memory.read_u32(stack.checked_add(12)?)?;
                let rect_w = (rect_tail & 0xffff) as i32;
                let rect_h = (rect_tail >> 16) as i32;
                let text_address = GuestAddr(cpu.register(0));
                if text_address.0 == 0 {
                    // The vendor routine answers a null text buffer with
                    // zero.
                    cpu.set_register(0, 0);
                    return Ok(());
                }
                // DRAW_TEXT_EX_IS_UNICODE is bit 0 of the flag word.
                let text = if flag & 1 == 0 {
                    let encoded = self.read_c_string(text_address, 64 * 1024)?;
                    let (decoded, _, _) = encoding_rs::GBK.decode(&encoded);
                    decoded.encode_utf16().collect()
                } else {
                    self.read_wide_string_be(text_address, 64 * 1024)?
                };
                let color = Framebuffer::rgb565(
                    (color_word & 0xff) as i32,
                    ((color_word >> 8) & 0xff) as i32,
                    ((color_word >> 16) & 0xff) as i32,
                );
                if trace_arm {
                    eprintln!(
                        "[arm-draw-text-ex] x={} y={} rect={}x{} flag={} font={} text={:?}",
                        cpu.register(1) as i32,
                        cpu.register(2) as i32,
                        rect_w,
                        rect_h,
                        flag,
                        font,
                        String::from_utf16(&text)
                    );
                }
                // DRAW_TEXT_EX_IS_AUTO_NEWLINE is bit 1 of the flag word.
                let end = self.draw_text_ex_to_screen(
                    &text,
                    cpu.register(1) as i32,
                    cpu.register(2) as i32,
                    rect_w,
                    rect_h,
                    color,
                    font,
                    flag & 2 != 0,
                    services,
                )?;
                cpu.set_register(0, end as u32);
            }
            129 => {
                // _mr_EffSetCon(int16 x, int16 y, int16 w, int16 h,
                //               int16 perr, int16 perg, int16 perb)
                // (mythroad.c): multiply the RGB565 channels of the
                // screen-cache rectangle by fixed-point factors (>> 8).
                // Regions outside the screen clip away and the call always
                // returns 0.
                let (screen_width, screen_height) = self.screen_dimensions()?;
                let x = i64::from(cpu.register(0) as u16 as i16);
                let y = i64::from(cpu.register(1) as u16 as i16);
                let end_x = x.saturating_add(i64::from(cpu.register(2) as u16 as i16));
                let end_y = y.saturating_add(i64::from(cpu.register(3) as u16 as i16));
                let stack = GuestAddr(cpu.register(13));
                let per_r = i32::from(self.memory.read_u32(stack)? as u16 as i16);
                let per_g = i32::from(self.memory.read_u32(stack.checked_add(4)?)? as u16 as i16);
                let per_b = i32::from(self.memory.read_u32(stack.checked_add(8)?)? as u16 as i16);
                let min_x = x.clamp(0, i64::from(screen_width));
                let min_y = y.clamp(0, i64::from(screen_height));
                let max_x = end_x.clamp(min_x, i64::from(screen_width));
                let max_y = end_y.clamp(min_y, i64::from(screen_height));
                if max_x > min_x && max_y > min_y {
                    self.scale_screen_region(min_x, min_y, max_x, max_y, per_r, per_g, per_b)?;
                }
                cpu.set_register(0, 0);
            }
            130 => {
                // `_mr_TestCom(mrp_State *L, int32 input0, int32 input1)`
                // (mythroad.c): the genuine-platform escape hatch. Commands
                // 2-6 register native lifecycle callbacks as raw guest
                // addresses; everything else either pushes its result onto
                // the Lua stack or flips C++ globals with no guest-visible
                // effect here. The register return follows the Lua
                // C-function convention: 1 when L is non-null, else 0.
                //
                // Command 7 is the lone exception: the reference kernel
                // (MTK mythroad `_mr_TestCom+0x8c`) sets the return to
                // `input1` and pops immediately, bypassing the Lua-state
                // check, so the value is echoed back on both the script and
                // aex (L == NULL) paths. Extension modules use it as a
                // loopback probe of the host callback channel and treat a
                // mismatch as "platform unsupported" (gjxjwql's Java stub
                // refuses to boot that way).
                let state = cpu.register(0);
                let command = cpu.register(1);
                let input = cpu.register(2);
                let mut result = u32::from(state != 0);
                match command {
                    2 => self.memory.write_u32(NATIVE_EVENT_FUNCTION_DATA, input)?,
                    3 => self.memory.write_u32(NATIVE_TIMER_FUNCTION_DATA, input)?,
                    4 => self.memory.write_u32(NATIVE_STOP_FUNCTION_DATA, input)?,
                    5 => self.memory.write_u32(NATIVE_PAUSE_FUNCTION_DATA, input)?,
                    6 => self.memory.write_u32(NATIVE_RESUME_FUNCTION_DATA, input)?,
                    7 => {
                        if trace_arm {
                            eprintln!("[arm-testcom] loopback echo -> {input:#010x}");
                        }
                        result = input;
                    }
                    _ => {}
                }
                if trace_arm {
                    eprintln!("[arm-testcom] L={state:#010x} command={command} input={input:#010x} -> {result:#010x}");
                }
                cpu.set_register(0, result);
            }
            131 => match (
                cpu.register(0),
                cpu.register(1),
                cpu.register(2),
                cpu.register(3),
            ) {
                // Marks a dynamically loaded native module as executable.
                (0, 9, address, len) if len != 0 => {
                    let address = GuestAddr(address);
                    self.record_key_event(format!(
                        "exec-range m={module} addr={:#010x} len={len}",
                        address.0
                    ));
                    let requested = ExecutableRange {
                        base: address,
                        len: len as usize,
                    };
                    let Some(context_generation) =
                        self.modules.get(module).map(|context| context.generation)
                    else {
                        return Err(Error::Abi(format!(
                            "executable-range registration for missing module {module}"
                        )));
                    };
                    let requested_end = requested.end().ok_or_else(|| {
                        Error::Abi("dynamic executable image range overflow".into())
                    })?;
                    let mtk_window = ExecutableRange {
                        base: MTK_NATIVE_EXTENSION_BASE,
                        len: MTK_NATIVE_EXTENSION_LEN,
                    };
                    let uses_mtk_window = mtk_window.contains_range(requested);
                    let allocation_owner = if uses_mtk_window {
                        self.mtk_native_extension_owner
                            .unwrap_or(context_generation)
                    } else {
                        self.allocation_owner_for_range(requested).ok_or_else(|| {
                            Error::Abi(format!(
                                "dynamic executable image {:#010x}..{requested_end:#010x} is not inside memory allocated by module {module}",
                                address.0
                            ))
                        })?
                    };
                    if allocation_owner != context_generation {
                        return Err(Error::Abi(format!(
                            "dynamic executable image {:#010x}..{requested_end:#010x} belongs to another module",
                            address.0
                        )));
                    }

                    let mut compatible_images = Vec::new();
                    for (owner, candidate) in self.modules.iter().enumerate() {
                        if candidate.image_range().overlaps(requested) {
                            return Err(Error::Abi(format!(
                                "dynamic executable image {:#010x}..{requested_end:#010x} overlaps module {owner}",
                                address.0,
                            )));
                        }
                        for (image_index, image) in
                            candidate.dynamic_executable_ranges.iter().enumerate()
                        {
                            let Some(image) = image.as_ref() else {
                                continue;
                            };
                            if !image
                                .intervals
                                .iter()
                                .any(|range| range.overlaps(requested))
                            {
                                continue;
                            }
                            if owner == module {
                                compatible_images.push(image_index);
                                continue;
                            }
                            return Err(Error::Abi(format!(
                                "dynamic executable image {:#010x}..{requested_end:#010x} overlaps module {owner} image {image_index}",
                                address.0,
                            )));
                        }
                    }

                    let mut extended_image = None;
                    let new_intervals = if compatible_images.len() == 1 {
                        let image_index = compatible_images[0];
                        let mut intervals = self.modules[module].dynamic_executable_ranges
                            [image_index]
                            .as_ref()
                            .expect("compatible dynamic image exists")
                            .intervals
                            .clone();
                        intervals.push(requested);
                        let intervals = merge_executable_intervals(intervals);
                        let merged_base = intervals
                            .first()
                            .map(|range| range.base.0)
                            .unwrap_or(requested.base.0);
                        let merged_end = intervals
                            .iter()
                            .filter_map(|range| range.end())
                            .max()
                            .unwrap_or(requested_end);
                        let merged_bounds = ExecutableRange {
                            base: GuestAddr(merged_base),
                            len: (merged_end - merged_base) as usize,
                        };
                        let merged_owner = if mtk_window.contains_range(merged_bounds) {
                            self.mtk_native_extension_owner
                                .unwrap_or(context_generation)
                        } else {
                            self.allocation_owner_for_range(merged_bounds).ok_or_else(|| {
                                Error::Abi(format!(
                                    "merged dynamic executable image {merged_base:#010x}..{merged_end:#010x} is not inside one tracked allocation"
                                ))
                            })?
                        };
                        if merged_owner != context_generation {
                            return Err(Error::Abi(format!(
                                "merged dynamic executable image {merged_base:#010x}..{merged_end:#010x} belongs to another module"
                            )));
                        }
                        extended_image = Some((image_index, intervals));
                        Vec::new()
                    } else if compatible_images.is_empty() {
                        vec![requested]
                    } else {
                        // Slot 131 grants execute permission; it does not replace code that
                        // was already registered. Preserve every existing image identity so
                        // live callbacks remain tied to the bytes that established them, and
                        // track only newly covered gaps as a distinct image.
                        let mut uncovered = vec![requested];
                        for image_index in &compatible_images {
                            let image = self.modules[module].dynamic_executable_ranges
                                [*image_index]
                                .as_ref()
                                .expect("compatible dynamic image exists");
                            for interval in &image.intervals {
                                uncovered = uncovered
                                    .into_iter()
                                    .flat_map(|range| range.subtract(*interval))
                                    .collect();
                            }
                        }
                        merge_executable_intervals(uncovered)
                    };
                    if !new_intervals.is_empty()
                        && !self.modules[module]
                            .dynamic_executable_ranges
                            .iter()
                            .any(DynamicExecutableImageSlot::is_none)
                        && self.modules[module].dynamic_executable_ranges.len() >= 64
                    {
                        return Err(Error::Abi(format!(
                            "module {module} exceeded 64 dynamic executable images"
                        )));
                    }
                    let vacant_image = self.modules[module]
                        .dynamic_executable_ranges
                        .iter()
                        .position(DynamicExecutableImageSlot::is_none);
                    let new_image = if !new_intervals.is_empty() {
                        let id = self.modules[module].next_dynamic_executable_image_id;
                        let next_id = id.checked_add(1).ok_or_else(|| {
                            Error::Abi("dynamic executable image identifier overflow".into())
                        })?;
                        Some((vacant_image, id, next_id))
                    } else {
                        None
                    };
                    let mut image = self.memory.read(address, len as usize)?;
                    let veneer_patches = patch_cmap_runtime_veneers(&mut image, address.0);
                    for patch in &veneer_patches {
                        self.memory
                            .write_u32(GuestAddr(address.0 + patch.offset as u32), patch.value)?;
                    }
                    if trace_arm && !veneer_patches.is_empty() {
                        eprintln!(
                            "[arm-executable] relocated {} CMAP runtime veneer literal(s) in {:#010x}..{:#010x}",
                            veneer_patches.len(),
                            address.0,
                            requested_end
                        );
                    }
                    let module_parameter = self.registered_dynamic_image_parameter(
                        &image,
                        address,
                        len,
                        context_generation,
                    );
                    if trace_arm {
                        eprintln!(
                            "[arm-executable] address={:#010x} len={len:#x} head={:02x?}",
                            address.0,
                            &image[..image.len().min(64)]
                        );
                    }
                    self.memory
                        .add_permissions(address, len as usize, Permissions::EXECUTE)?;
                    if uses_mtk_window && self.mtk_native_extension_owner.is_none() {
                        self.mtk_native_extension_owner = Some(context_generation);
                    }
                    if let Some((image_index, intervals)) = extended_image {
                        let image = self.modules[module].dynamic_executable_ranges[image_index]
                            .as_mut()
                            .expect("compatible dynamic image exists");
                        image.intervals = intervals;
                        if image.module_parameter.is_none() {
                            image.module_parameter = module_parameter;
                        }
                    } else if let Some((vacant_image, id, next_id)) = new_image {
                        let dynamic_image = DynamicExecutableImage {
                            id,
                            intervals: new_intervals,
                            module_parameter,
                            compact_repeating_timers: Vec::new(),
                        };
                        self.modules[module].next_dynamic_executable_image_id = next_id;
                        if let Some(image_index) = vacant_image {
                            self.modules[module].dynamic_executable_ranges[image_index].0 =
                                Some(dynamic_image);
                        } else {
                            self.modules[module]
                                .dynamic_executable_ranges
                                .push(DynamicExecutableImageSlot(Some(dynamic_image)));
                        }
                    }
                    cpu.set_register(0, 0);
                }
                (command, argument, address, len) => {
                    return Err(Error::Abi(format!(
                        "unsupported platform slot 131 command ({command}, {argument}, {address:#010x}, {len}) called by module {module}"
                    )));
                }
            },
            132 => self.convert_legacy_string_to_ucs2(module, cpu)?,
            145 => {
                // mr_platDrawChar(uint16 char, int16 x, int16 y, uint16 color):
                // the platform font renderer for a single glyph. The vendor
                // kernel (all_kernel.c aex_t145) forwards straight to
                // mr_platDrawChar and returns 0. Char codes arrive as UCS-2
                // (gjxjwql draws "本" as 0x672c), which matches the host
                // font table's Unicode indexing.
                let codepoint = cpu.register(0) as u16;
                let x = cpu.register(1) as i32;
                let y = cpu.register(2) as i32;
                let color = cpu.register(3) as u16;
                if trace_arm {
                    eprintln!(
                        "[arm-draw-char] c={codepoint:#06x} x={x} y={y} color={color:#06x}"
                    );
                }
                self.draw_text_to_screen(&[codepoint], x, y, color, 0, services)?;
                cpu.set_register(0, 0);
            }
            other => {
                let return_address = cpu.register(14) & !1;
                let caller_start = return_address.saturating_sub(24);
                let caller_bytes = self
                    .memory
                    .read(GuestAddr(caller_start), 48)
                    .map(|bytes| format!("{bytes:02x?}"))
                    .unwrap_or_else(|error| format!("unavailable: {error}"));
                let stack_words = (0..6)
                    .map(|index| {
                        self.memory
                            .read_u32(GuestAddr(cpu.register(13).wrapping_add(index * 4)))
                    })
                    .collect::<skyengine_arm::Result<Vec<_>>>()
                    .map(|words| format!("{words:08x?}"))
                    .unwrap_or_else(|error| format!("unavailable: {error}"));
                let argument_bytes = self
                    .memory
                    .read(GuestAddr(cpu.register(0)), 32)
                    .map(|bytes| format!("{bytes:02x?}"))
                    .unwrap_or_else(|error| format!("unavailable: {error}"));
                let second_argument_bytes = self
                    .memory
                    .read(GuestAddr(cpu.register(1)), 32)
                    .map(|bytes| format!("{bytes:02x?}"))
                    .unwrap_or_else(|error| format!("unavailable: {error}"));
                let stack_record_bytes = self
                    .memory
                    .read_u32(GuestAddr(cpu.register(13)))
                    .and_then(|address| self.memory.read(GuestAddr(address), 32))
                    .map(|bytes| format!("{bytes:02x?}"))
                    .unwrap_or_else(|error| format!("unavailable: {error}"));
                return Err(Error::Abi(format!(
                    "unsupported platform slot {other} called by module {module} at LR {:#010x} (r0={:#010x}, r1={:#010x}, r2={:#010x}, r3={:#010x}, sp={:#010x}, stack={stack_words}, r0-bytes={argument_bytes}, r1-bytes={second_argument_bytes}, stack-record={stack_record_bytes}); guest bytes at {caller_start:#010x}: {caller_bytes}",
                    cpu.register(14),
                    cpu.register(0),
                    cpu.register(1),
                    cpu.register(2),
                    cpu.register(3),
                    cpu.register(13),
                )));
            }
        }
        Ok(())
    }

    /// Timer-racing 调试辅助：把 guest 内存的 module 槽区与堆区一次性落盘。
    /// 输出文件 = SKYENGINE_DUMP_GUEST_REGION 所指目录下
    /// `{原文件名}-{tag}-module.bin` / `{原文件名}-{tag}-heap.bin`。
    fn dump_guest_regions(&self, tag: &str) {
        if let Some(base) = std::env::var_os("SKYENGINE_DUMP_GUEST_REGION") {
            let base_path = std::path::Path::new(&base);
            let dir = base_path.with_file_name("");
            let base_name = base_path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let regions: [(&str, u32, usize); 2] = [
                ("module", 0x1000_0000, 0x4_0000),
                ("heap", 0x2000_0000, 0xA_0000),
            ];
            for (name, start, len) in regions {
                let path = dir.join(format!("{base_name}-{tag}-{name}.bin"));
                match self.memory.read(GuestAddr(start), len) {
                    Ok(region) => {
                        let _ = std::fs::write(&path, &region);
                        eprintln!(
                            "[dump] {tag} guest region {name} {start:#010x} len {len:#x} ({} bytes) -> {}",
                            region.len(),
                            path.display()
                        );
                    }
                    Err(error) => {
                        eprintln!("[dump] {tag} guest region {name} read failed: {error}");
                    }
                }
            }
        }
    }
}

/// Link address the CMAP packer assigns to image byte 0: the 8-byte `MRPGCMAP`
/// header occupies `CMAP_IMAGE_LINK_BASE..=0x7FFFF` and the entry function
/// starts at 0x80000 (image offset 8). Packer SDKs keep shared runtime
/// helpers in this fixed 0x80000+ window, so callers reach them through ARM
/// veneers of the form `ldr ip, [pc, #imm]; bx ip` that embed the helper's
/// link address as a literal. The MRP's own loader never relocates those
/// literals, and each helper body is embedded in the image itself at image
/// offset `literal - CMAP_IMAGE_LINK_BASE`. gahhx_320x480.mrp contains one
/// such veneer (literal 0x81A97 -> helper at offset 0x1A9E) and geyaxz.mrp
/// contains one (literal 0x80E35 -> helper at offset 0xE3C).
const CMAP_IMAGE_LINK_BASE: u32 = 0x7FFF8;

/// One CMAP runtime veneer literal relocation discovered in a dynamic image.
struct CmapVeneerPatch {
    /// Byte offset of the literal inside the registered image.
    offset: usize,
    /// Relocated literal value: the runtime address of the embedded helper.
    value: u32,
}

/// Relocates absolute CMAP runtime veneer literals in a registered image so
/// they point at the helper bodies embedded inside the same image. The scan
/// mirrors the packer's veneer encoding exactly (any-condition ARM
/// `ldr ip, [pc, #imm]` followed by `bx ip`/`blx ip`) and only rewrites
/// literals whose link addresses fall inside the image's own 0x80000+ code
/// window, so data words and ordinary position-relative code are untouched.
/// The image header is not checked: the guest loader overwrites the 8-byte
/// `MRPGCMAP` header with its table and state pointers before registering
/// the image through slot 131.
fn patch_cmap_runtime_veneers(image: &mut [u8], image_base: u32) -> Vec<CmapVeneerPatch> {
    let mut patches = Vec::new();
    if image.len() < 16 {
        return patches;
    }
    let image_len = image.len() as u32;
    let mut offset = 0usize;
    while offset + 8 <= image.len() {
        let load = u32::from_le_bytes(image[offset..offset + 4].try_into().expect("four bytes"));
        // ARM `ldr ip, [pc, #imm]` with any condition.
        if (load & 0x0F7FF000) == 0x051FC000 {
            let displacement = 8i64 + (load & 0x0FFF) as i64;
            let literal_offset = if (load >> 23) & 1 != 0 {
                offset as i64 + displacement
            } else {
                offset as i64 - displacement
            };
            let branch = u32::from_le_bytes(
                image[offset + 4..offset + 8]
                    .try_into()
                    .expect("four bytes"),
            );
            let is_bx_ip = (branch & 0x0FFFFFFF) == 0x012FFF1C;
            let is_blx_ip =
                (branch & 0xFEFFFFFF) == 0xFA3FFF0C || (branch & 0x0FFFFFFF) == 0x012FFF3C;
            if (is_bx_ip || is_blx_ip)
                && literal_offset >= 0
                && literal_offset + 4 <= image.len() as i64
            {
                let literal_offset = literal_offset as usize;
                let literal = u32::from_le_bytes(
                    image[literal_offset..literal_offset + 4]
                        .try_into()
                        .expect("four bytes"),
                );
                // Only literals naming an embedded helper: link addresses
                // start at 0x80000 (image offset 8) and must stay inside the
                // registered image.
                if literal >= CMAP_IMAGE_LINK_BASE + 8 && literal - CMAP_IMAGE_LINK_BASE < image_len
                {
                    let value = image_base + (literal - CMAP_IMAGE_LINK_BASE);
                    patches.push(CmapVeneerPatch {
                        offset: literal_offset,
                        value,
                    });
                    image[literal_offset..literal_offset + 4].copy_from_slice(&value.to_le_bytes());
                }
            }
        }
        offset += 4;
    }
    patches
}
