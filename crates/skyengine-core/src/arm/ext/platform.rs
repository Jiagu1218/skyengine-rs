use super::*;
use encoding_rs::GBK;
use jpeg_decoder::{Decoder, ImageInfo, PixelFormat};

const MAX_PLATFORM_JPEG_SOURCE_LEN: usize = 32 * 1024 * 1024;
const MAX_PLATFORM_JPEG_DECODED_LEN: usize = 32 * 1024 * 1024;
const MAX_PLATFORM_MEDIA_BYTES: usize = 16 * 1024 * 1024;

impl ExtRuntime {
    fn read_platform_jpeg(
        &self,
        source: GuestAddr,
        source_len: usize,
    ) -> Result<Option<(Vec<u8>, ImageInfo)>> {
        if source.0 == 0 || !(3..=MAX_PLATFORM_JPEG_SOURCE_LEN).contains(&source_len) {
            return Ok(None);
        }
        let bytes = self.memory.read(source, source_len)?;
        if !bytes.starts_with(&[0xff, 0xd8, 0xff]) {
            return Ok(None);
        }
        let mut decoder = Decoder::new(std::io::Cursor::new(&bytes));
        if decoder.read_info().is_err() {
            return Ok(None);
        }
        let Some(info) = decoder.info() else {
            return Ok(None);
        };
        let pixel_count = usize::from(info.width)
            .checked_mul(usize::from(info.height))
            .ok_or_else(|| Error::ResourceLimit("platform JPEG dimensions overflow".into()))?;
        let output_len = pixel_count
            .checked_mul(2)
            .ok_or_else(|| Error::ResourceLimit("platform JPEG output length overflow".into()))?;
        let decoded_len = pixel_count
            .checked_mul(info.pixel_format.pixel_bytes())
            .ok_or_else(|| Error::ResourceLimit("platform JPEG decoded length overflow".into()))?;
        if info.width == 0
            || info.height == 0
            || output_len > MAX_PLATFORM_JPEG_DECODED_LEN
            || decoded_len > MAX_PLATFORM_JPEG_DECODED_LEN
            || matches!(info.pixel_format, PixelFormat::L16 | PixelFormat::CMYK32)
        {
            return Ok(None);
        }
        Ok(Some((bytes, info)))
    }

    fn decode_platform_jpeg_rgb565(bytes: Vec<u8>, info: ImageInfo) -> Option<Vec<u8>> {
        let pixel_count = usize::from(info.width).checked_mul(usize::from(info.height))?;
        let decoded_len = pixel_count.checked_mul(info.pixel_format.pixel_bytes())?;
        let output_len = pixel_count.checked_mul(2)?;
        let mut decoder = Decoder::new(std::io::Cursor::new(bytes));
        decoder.set_max_decoding_buffer_size(decoded_len);
        let mut pixels = decoder.decode().ok()?;
        if pixels.len() != decoded_len {
            return None;
        }
        match info.pixel_format {
            PixelFormat::RGB24 => {
                for index in 0..pixel_count {
                    let source = index * 3;
                    let destination = index * 2;
                    let color = Self::rgb888_to_rgb565(
                        pixels[source],
                        pixels[source + 1],
                        pixels[source + 2],
                    );
                    pixels[destination..destination + 2].copy_from_slice(&color.to_le_bytes());
                }
                pixels.truncate(output_len);
            }
            PixelFormat::L8 => {
                pixels.resize(output_len, 0);
                for index in (0..pixel_count).rev() {
                    let luminance = pixels[index];
                    let color = Self::rgb888_to_rgb565(luminance, luminance, luminance);
                    let destination = index * 2;
                    pixels[destination..destination + 2].copy_from_slice(&color.to_le_bytes());
                }
            }
            PixelFormat::L16 | PixelFormat::CMYK32 => return None,
        }
        Some(pixels)
    }

    fn rgb888_to_rgb565(red: u8, green: u8, blue: u8) -> u16 {
        (u16::from(red >> 3) << 11) | (u16::from(green >> 2) << 5) | u16::from(blue >> 3)
    }

    pub(super) fn return_platform_jpeg_info(&mut self, cpu: &mut ArmCpu) -> Result<()> {
        if cpu.register(2) != 12 {
            return Err(Error::Abi(format!(
                "platform JPEG info input is {} bytes, expected 12",
                cpu.register(2)
            )));
        }
        let output = GuestAddr(cpu.register(3));
        let output_len = GuestAddr(self.memory.read_u32(GuestAddr(cpu.register(13)))?);
        if output.0 == 0 || output_len.0 == 0 {
            return Err(Error::Abi(
                "platform JPEG info requires output and output-length pointers".into(),
            ));
        }
        self.memory.write_u32(output, 0)?;
        self.memory.write_u32(output_len, 0)?;

        let input = GuestAddr(cpu.register(1));
        if input.0 == 0 {
            cpu.set_register(0, u32::MAX);
            return Ok(());
        }
        let source = GuestAddr(self.memory.read_u32(input)?);
        let source_len = self.memory.read_u32(input.checked_add(4)?)? as usize;
        let codec = self.memory.read_u32(input.checked_add(8)?)?;
        let Some((_bytes, info)) = (codec == 1)
            .then(|| self.read_platform_jpeg(source, source_len))
            .transpose()?
            .flatten()
        else {
            cpu.set_register(0, u32::MAX);
            return Ok(());
        };

        self.memory
            .write_u32(PLATFORM_JPEG_INFO_DATA, u32::from(info.width))?;
        self.memory.write_u32(
            PLATFORM_JPEG_INFO_DATA.checked_add(4)?,
            u32::from(info.height),
        )?;
        self.memory.write_u32(output, PLATFORM_JPEG_INFO_DATA.0)?;
        self.memory
            .write_u32(output_len, PLATFORM_JPEG_INFO_LEN as u32)?;
        cpu.set_register(0, 0);
        Ok(())
    }

    pub(super) fn decode_platform_jpeg(&mut self, cpu: &mut ArmCpu) -> Result<()> {
        if cpu.register(2) != 24 {
            return Err(Error::Abi(format!(
                "platform JPEG decode input is {} bytes, expected 24",
                cpu.register(2)
            )));
        }
        let output_len = self.memory.read_u32(GuestAddr(cpu.register(13)))?;
        if cpu.register(3) != 0 || output_len != 0 {
            return Err(Error::Abi(
                "platform JPEG decode does not accept output buffers".into(),
            ));
        }
        let input = GuestAddr(cpu.register(1));
        if input.0 == 0 {
            cpu.set_register(0, u32::MAX);
            return Ok(());
        }
        let source = GuestAddr(self.memory.read_u32(input)?);
        let source_len = self.memory.read_u32(input.checked_add(4)?)? as usize;
        let width = self.memory.read_u32(input.checked_add(8)?)?;
        let height = self.memory.read_u32(input.checked_add(12)?)?;
        let codec = self.memory.read_u32(input.checked_add(16)?)?;
        let destination = GuestAddr(self.memory.read_u32(input.checked_add(20)?)?);
        let Some((bytes, info)) = (codec == 1)
            .then(|| self.read_platform_jpeg(source, source_len))
            .transpose()?
            .flatten()
        else {
            cpu.set_register(0, u32::MAX);
            return Ok(());
        };
        if width != u32::from(info.width) || height != u32::from(info.height) {
            cpu.set_register(0, u32::MAX);
            return Ok(());
        }
        let output_len = usize::from(info.width)
            .checked_mul(usize::from(info.height))
            .and_then(|pixels| pixels.checked_mul(2))
            .ok_or_else(|| Error::ResourceLimit("platform JPEG output length overflow".into()))?;
        if destination.0 == 0 {
            cpu.set_register(0, u32::MAX);
            return Ok(());
        }
        if self
            .tracked_guest_allocation_len(destination)
            .is_some_and(|allocated| output_len > allocated as usize)
        {
            cpu.set_register(0, u32::MAX);
            return Ok(());
        }
        self.memory
            .check_range(destination, output_len, Permissions::WRITE)?;
        let Some(pixels) = Self::decode_platform_jpeg_rgb565(bytes, info) else {
            cpu.set_register(0, u32::MAX);
            return Ok(());
        };
        self.memory.write(destination, &pixels)?;
        cpu.set_register(0, 0);
        Ok(())
    }

    fn encode_legacy_string_as_ucs2(legacy: &[u8]) -> Vec<u8> {
        let (decoded, _, _) = GBK.decode(legacy);
        let mut encoded = Vec::with_capacity(decoded.len().saturating_add(1).saturating_mul(2));
        for unit in decoded.encode_utf16() {
            encoded.extend_from_slice(&unit.to_be_bytes());
        }
        encoded.extend_from_slice(&[0, 0]);
        encoded
    }

    pub(super) fn convert_legacy_string_to_ucs2(
        &mut self,
        module: usize,
        cpu: &mut ArmCpu,
    ) -> Result<()> {
        let error_output = GuestAddr(cpu.register(1));
        if error_output.0 != 0 {
            self.memory.write_u32(error_output, u32::MAX)?;
        }

        let input = GuestAddr(cpu.register(0));
        if input.0 == 0 {
            cpu.set_register(0, 0);
            return Ok(());
        }
        let legacy = self.read_c_string(input, 64 * 1024)?;
        let encoded = Self::encode_legacy_string_as_ucs2(&legacy);

        let size_output = GuestAddr(cpu.register(2));
        if size_output.0 != 0 {
            self.memory.write_u32(
                size_output,
                u32::try_from(encoded.len()).map_err(|_| {
                    Error::Abi("legacy string conversion output length exceeds u32".into())
                })?,
            )?;
        }
        let Some(output) = self.allocate_guest_block_for_module(encoded.len(), module)? else {
            cpu.set_register(0, 0);
            return Ok(());
        };
        self.memory.write(output, &encoded)?;
        cpu.set_register(0, output.0);
        Ok(())
    }

    pub(super) fn convert_platform_ucs2_to_legacy(&mut self, cpu: &mut ArmCpu) -> Result<()> {
        let input_len = cpu.register(2) as usize;
        if input_len > 64 * 1024 {
            return Err(Error::ResourceLimit(format!(
                "platform string conversion input is {input_len} bytes (limit 65536)"
            )));
        }
        let output_len = GuestAddr(self.memory.read_u32(GuestAddr(cpu.register(13)))?);
        if output_len.0 == 0 {
            cpu.set_register(0, u32::MAX);
            return Ok(());
        }
        self.memory.write_u32(output_len, 0)?;
        let output_field = GuestAddr(cpu.register(3));
        if output_field.0 == 0 || input_len & 1 != 0 {
            cpu.set_register(0, u32::MAX);
            return Ok(());
        }
        let output = GuestAddr(self.memory.read_u32(output_field)?);
        if output.0 == 0 {
            cpu.set_register(0, u32::MAX);
            return Ok(());
        }
        let input = GuestAddr(cpu.register(1));
        if input.0 == 0 && input_len != 0 {
            cpu.set_register(0, u32::MAX);
            return Ok(());
        }
        let wide = self.memory.read(input, input_len)?;
        let units = wide
            .as_chunks::<2>()
            .0
            .iter()
            .map(|bytes| u16::from_be_bytes(*bytes))
            .collect::<Vec<_>>();
        let decoded = String::from_utf16_lossy(&units);
        let (legacy, _, _) = GBK.encode(&decoded);
        self.memory.write(output, legacy.as_ref())?;
        self.memory
            .write(output.checked_add(legacy.len() as u32)?, &[0])?;
        self.memory.write_u32(
            output_len,
            u32::try_from(legacy.len())
                .map_err(|_| Error::Abi("converted platform string exceeds u32".into()))?,
        )?;
        cpu.set_register(0, 0);
        Ok(())
    }

    pub(super) fn return_unavailable_platform_extension(&mut self, cpu: &mut ArmCpu) -> Result<()> {
        let output = GuestAddr(cpu.register(3));
        if output.0 != 0 {
            self.memory.write_u32(output, 0)?;
        }
        let output_len = GuestAddr(self.memory.read_u32(GuestAddr(cpu.register(13)))?);
        if output_len.0 != 0 {
            self.memory.write_u32(output_len, 0)?;
        }
        cpu.set_register(0, u32::MAX);
        Ok(())
    }

    pub(super) fn return_platform_runtime_profile(&mut self, cpu: &mut ArmCpu) -> Result<()> {
        let output = GuestAddr(cpu.register(3));
        let output_len = GuestAddr(self.memory.read_u32(GuestAddr(cpu.register(13)))?);
        if output.0 == 0 || output_len.0 == 0 {
            if output_len.0 != 0 {
                self.memory.write_u32(output_len, 0)?;
            }
            cpu.set_register(0, u32::MAX);
            return Ok(());
        }
        self.memory
            .write_u32(output, PLATFORM_RUNTIME_PROFILE_DATA.0)?;
        self.memory
            .write_u32(output_len, PLATFORM_RUNTIME_PROFILE_LEN as u32)?;
        cpu.set_register(0, 0);
        Ok(())
    }

    pub(super) fn return_platform_sim_info(&mut self, cpu: &mut ArmCpu) -> Result<()> {
        let output = GuestAddr(cpu.register(3));
        if output.0 == 0 {
            return Err(Error::Abi(
                "platform SIM query has a null output pointer".into(),
            ));
        }
        let output_len = GuestAddr(self.memory.read_u32(GuestAddr(cpu.register(13)))?);
        if output_len.0 == 0 {
            return Err(Error::Abi(
                "platform SIM query has a null output-length pointer".into(),
            ));
        }
        self.memory.write_u32(output, PLATFORM_SIM_INFO_DATA.0)?;
        self.memory
            .write_u32(output_len, PLATFORM_SIM_INFO_LEN as u32)?;
        cpu.set_register(0, 0);
        Ok(())
    }

    pub(super) fn return_platform_storage_info(&mut self, cpu: &mut ArmCpu) -> Result<()> {
        // Mirrors the vendor mr_platEx case MR_GET_FREE_SPACE (1305): the
        // input is a drive letter and the output is the real-machine
        // T_DSM_FREE_SAPCE {total, tunit, account, unit} for drives A/B/C.
        // Any other drive answers MR_IGNORE so the guest uses its fallback.
        let drive = self.memory.read_u8(GuestAddr(cpu.register(1)))?;
        // 取证差分（评分公式逆向）：SKYENGINE_DEBUG_STORAGE_MIB=<n> 把 C 盘
        // 几何改写为 n MiB 总 / n-1 MiB 空闲，用于观测存储容量的评分权重。
        let debug_storage_mib = std::env::var("SKYENGINE_DEBUG_STORAGE_MIB")
            .ok()
            .and_then(|text| text.parse::<u32>().ok());
        let geometry: [u32; 4] = match debug_storage_mib {
            Some(mib) if drive == b'C' || drive == b'c' => {
                [mib, 1024 * 1024, mib.saturating_sub(1), 1024 * 1024]
            }
            _ => match drive {
                b'A' | b'a' => [1722, 1024, 1271, 1024],
                b'B' | b'b' => [95, 1024, 77, 1024],
                b'C' | b'c' => [1874, 1024 * 1024, 1873, 1024 * 1024],
                _ => {
                    cpu.set_register(0, 1);
                    return Ok(());
                }
            },
        };
        let output = GuestAddr(cpu.register(3));
        if output.0 == 0 {
            return Err(Error::Abi(
                "platform storage query has a null output pointer".into(),
            ));
        }
        let output_len = GuestAddr(self.memory.read_u32(GuestAddr(cpu.register(13)))?);
        if output_len.0 == 0 {
            return Err(Error::Abi(
                "platform storage query has a null output-length pointer".into(),
            ));
        }
        for (index, value) in geometry.into_iter().enumerate() {
            self.memory.write_u32(
                PLATFORM_STORAGE_INFO_DATA.checked_add((index * 4) as u32)?,
                value,
            )?;
        }
        self.memory
            .write_u32(output, PLATFORM_STORAGE_INFO_DATA.0)?;
        self.memory
            .write_u32(output_len, PLATFORM_STORAGE_INFO_LEN as u32)?;
        cpu.set_register(0, 0);
        Ok(())
    }

    pub(super) fn return_platform_storage_drive(&mut self, cpu: &mut ArmCpu) -> Result<()> {
        let input_len = cpu.register(2) as usize;
        let input = self.memory.read(GuestAddr(cpu.register(1)), input_len)?;
        if !matches!(input.as_slice(), b"C" | b"X" | b"Y" | b"Z") {
            cpu.set_register(0, u32::MAX);
            return Ok(());
        }
        let output = GuestAddr(cpu.register(3));
        let output_len = GuestAddr(self.memory.read_u32(GuestAddr(cpu.register(13)))?);
        if output.0 == 0 && output_len.0 == 0 {
            cpu.set_register(0, u32::MAX);
            return Ok(());
        }
        if output.0 == 0 {
            return Err(Error::Abi(
                "platform storage drive query has a null output pointer".into(),
            ));
        }
        if output_len.0 == 0 {
            return Err(Error::Abi(
                "platform storage drive query has a null output-length pointer".into(),
            ));
        }
        self.memory
            .write_u32(output, PLATFORM_STORAGE_DRIVE_DATA.0)?;
        self.memory
            .write_u32(output_len, PLATFORM_STORAGE_DRIVE_LEN as u32)?;
        cpu.set_register(0, 0);
        Ok(())
    }

    pub(super) fn allocate_platform_memory_extension(
        &mut self,
        module: usize,
        cpu: &mut ArmCpu,
    ) -> Result<()> {
        let requested_len = cpu.register(2) as i32;
        let output = GuestAddr(cpu.register(3));
        if output.0 == 0 {
            return Err(Error::Abi(
                "platform memory extension has a null output pointer".into(),
            ));
        }
        let output_len = GuestAddr(self.memory.read_u32(GuestAddr(cpu.register(13)))?);
        if output_len.0 == 0 {
            return Err(Error::Abi(
                "platform memory extension has a null output-length pointer".into(),
            ));
        }
        self.memory.check_range(output, 4, Permissions::WRITE)?;
        self.memory.check_range(output_len, 4, Permissions::WRITE)?;
        let owner_generation = self
            .modules
            .get(module)
            .map(|module| module.generation)
            .ok_or_else(|| {
                Error::Abi(format!("platform allocation for missing module {module}"))
            })?;

        // The verified memory probe passes `target - current` as input_len. A
        // non-positive result means no extra capacity is needed, but its caller
        // still requires a non-null handle that command 1015 can release.
        let allocation_len = requested_len.max(1) as usize;
        let previous_cursor = self.platform_memory_cursor;
        if !(PLATFORM_MEMORY_BASE.0..=DETACHED_GUEST_ALLOCATION_BASE.0).contains(&previous_cursor) {
            return Err(Error::ArmFault(format!(
                "platform memory cursor {previous_cursor:#010x} is outside its allocation window"
            )));
        }

        self.memory.write_u32(output, 0)?;
        self.memory.write_u32(output_len, 0)?;
        if allocation_len > MAX_PLATFORM_MEMORY_EXTENSION_LEN {
            cpu.set_register(0, u32::MAX);
            return Ok(());
        }

        let arena_value = (u64::from(previous_cursor) + 0xfff) & !0xfff;
        let arena_end = arena_value + allocation_len as u64;
        if arena_end > u64::from(DETACHED_GUEST_ALLOCATION_BASE.0) {
            cpu.set_register(0, u32::MAX);
            return Ok(());
        }
        let arena_value = u32::try_from(arena_value)
            .expect("platform memory allocation window fits in a guest address");
        let arena_end = u32::try_from(arena_end)
            .expect("platform memory allocation window fits in a guest address");
        let arena = GuestAddr(arena_value);
        self.memory.map(
            arena,
            allocation_len,
            Permissions::READ_WRITE,
            "platform memory extension",
        )?;
        self.platform_memory_cursor = arena_end;
        self.platform_memory_extensions.insert(
            arena.0,
            PlatformMemoryExtension {
                len: allocation_len,
                previous_cursor,
                owner_generation,
            },
        );
        self.memory.write_u32(output, arena.0)?;
        self.memory
            .write_u32(output_len, requested_len.max(0) as u32)?;
        cpu.set_register(0, 0);
        Ok(())
    }

    pub(super) fn release_platform_memory_extension(
        &mut self,
        module: usize,
        cpu: &mut ArmCpu,
    ) -> Result<()> {
        let arena = GuestAddr(cpu.register(1));
        if cpu.register(2) != 4 {
            return Err(Error::Abi(format!(
                "platform memory extension release input is {} bytes, expected 4",
                cpu.register(2)
            )));
        }
        let extension = self
            .platform_memory_extensions
            .get(&arena.0)
            .copied()
            .ok_or_else(|| {
                Error::Abi(format!(
                    "platform memory extension release references unknown arena {:#010x}",
                    arena.0
                ))
            })?;
        let owner_generation = self
            .modules
            .get(module)
            .map(|module| module.generation)
            .ok_or_else(|| Error::Abi(format!("platform release for missing module {module}")))?;
        if extension.owner_generation != owner_generation {
            return Err(Error::Abi(format!(
                "module {module} cannot release platform arena {:#010x} owned by another module",
                arena.0
            )));
        }
        let PlatformMemoryExtension {
            len,
            previous_cursor,
            ..
        } = extension;
        let end = arena
            .0
            .checked_add(u32::try_from(len).map_err(|_| {
                Error::Abi(format!(
                    "platform memory extension length {len} exceeds u32"
                ))
            })?)
            .ok_or_else(|| Error::Abi("platform memory extension end overflow".into()))?;
        self.revoke_executable_ranges_in(ExecutableRange { base: arena, len })?;
        self.memory.unmap(arena, len)?;
        self.platform_memory_extensions.remove(&arena.0);
        self.guest_allocation_views
            .retain(|_, view| view.backing_base != arena.0);
        if end == self.platform_memory_cursor {
            self.platform_memory_cursor = previous_cursor;
        }
        cpu.set_register(0, 0);
        Ok(())
    }

    // ---- mr_platEx ACI media devices (1=MIDI .. 7=AMR-WB) ----
    //
    // The media commands encode `command * 10 + device`; the handlers below
    // mirror the C++ state machine in dsm.c (dsm_media_platEx and helpers):
    // FILE_LOAD/BUF_LOAD only store the resource, PLAY_CUR_REQ starts
    // playback with the loop flag from the caller's T_DSM_MEDIA_PLAY record,
    // and STOP/CLOSE transition the device state without failing even when
    // nothing is loaded. Two departures upgrade the legacy kernel, which the
    // mpmusic player UI needs: PAUSE freezes the decoded stream at its
    // position (RESUME continues instead of replaying) and SETPOS seeks it.
    // Playback covers the full legacy device table: MIDI/WAV/MP3/PCM decode
    // as before, AMR (both narrow- and wideband) decodes through the
    // RFC 4867 storage reader, and M4A goes through the generic probe.

    /// Debug trace for a media device, sharing the MR call tracer's
    /// SKYENGINE_TRACE_MR_CALLS switch. Logged after the handler runs so the
    /// result rides in r0.
    pub(super) fn trace_platform_media(
        &self,
        call: &str,
        device: usize,
        cpu: &ArmCpu,
    ) -> Result<()> {
        if std::env::var_os("SKYENGINE_TRACE_MR_CALLS").is_some() {
            let stack_pointer = GuestAddr(cpu.register(13));
            let stack_cell = |address: GuestAddr| {
                self.memory
                    .read_u32(address)
                    .map(|value| format!("{value:#010x}"))
                    .unwrap_or_else(|_| "unavailable".into())
            };
            eprintln!(
                "[platEx-media{device}] {call} -> r0={} input={:#010x} input_len={} output={:#010x} output_len_cell={} stack1={}",
                cpu.register(0) as i32,
                cpu.register(1),
                cpu.register(2),
                cpu.register(3),
                stack_cell(stack_pointer),
                stack_cell(stack_pointer.checked_add(4)?),
            );
        }
        Ok(())
    }

    /// Routes a media-family command to its device handler. The dispatch
    /// guard guarantees a device in 1..=7 and a command in the media set.
    pub(super) fn dispatch_platform_media(
        &mut self,
        command: u32,
        module: usize,
        cpu: &mut ArmCpu,
        services: &mut dyn NativeServices,
    ) -> Result<()> {
        let device = (command % 10) as usize;
        match command / 10 {
            201 => {
                self.init_platform_media(device, cpu, services)?;
                self.trace_platform_media("INIT", device, cpu)?;
            }
            202 => {
                self.load_platform_media_file(device, cpu, services)?;
                self.trace_platform_media("FILE_LOAD", device, cpu)?;
            }
            203 => {
                self.load_platform_media_buffer(device, cpu)?;
                self.trace_platform_media("BUF_LOAD", device, cpu)?;
            }
            204 => {
                self.play_platform_media(device, cpu, services)?;
                self.trace_platform_media("PLAY", device, cpu)?;
            }
            205 => {
                self.pause_platform_media(device, cpu, services)?;
                self.trace_platform_media("PAUSE", device, cpu)?;
            }
            206 => {
                self.resume_platform_media(device, cpu, services)?;
                self.trace_platform_media("RESUME", device, cpu)?;
            }
            207 => {
                self.stop_platform_media(device, cpu, services)?;
                self.trace_platform_media("STOP", device, cpu)?;
            }
            208 | 216 => {
                self.close_platform_media(device, cpu, services)?;
                self.trace_platform_media("CLOSE", device, cpu)?;
            }
            209 => {
                self.return_platform_media_status(module, device, cpu)?;
                self.trace_platform_media("GET_STATUS", device, cpu)?;
            }
            // SETPOS (210): the input record T_SET_PLAY_POS carries an
            // int32 position in seconds (platform wiki 210x; mpmusic's
            // seek helper at game.ext 0x180a0 passes exactly four bytes).
            // The legacy C++ kernel ignores the request; the host sink
            // seeks the decoded stream so progress-bar drags work.
            210 => {
                self.seek_platform_media(module, cpu, services)?;
                self.trace_platform_media("SETPOS", device, cpu)?;
            }
            // Time queries (GETTIME/GET_TOTAL_TIME/GET_CURTIME/
            // GET_CURTIME_MSEC): report the live playback progress
            // through the audio service (units per command).
            211 | 212 | 213 | 215 => {
                self.return_platform_media_time(module, command, cpu, services)?;
                self.trace_platform_media("TIME_QUERY", device, cpu)?;
            }
            _ => unreachable!("media dispatch guard admits only media commands"),
        }
        Ok(())
    }

    /// INIT (201*10+device): releases any resource the device still holds
    /// and moves it to the INITED state, mirroring the C++ transition.
    pub(super) fn init_platform_media(
        &mut self,
        device: usize,
        cpu: &mut ArmCpu,
        services: &mut dyn NativeServices,
    ) -> Result<()> {
        if cpu.register(1) != 0 || cpu.register(2) != 0 || cpu.register(3) != 0 {
            return Err(Error::Abi(
                "unsupported platform media init request shape".into(),
            ));
        }
        if self.media_devices[device].status == MEDIA_STATUS_PLAYING {
            services.stop_sound()?;
        }
        self.media_devices[device] = PlatformMediaDevice {
            status: MEDIA_STATUS_INITED,
            data: None,
        };
        cpu.set_register(0, 0);
        Ok(())
    }

    /// FILE_LOAD (202*10+device): loads a named resource into the device
    /// slot. Device 1 keeps the verified MIDI resource-name contract; the
    /// other devices accept any sanitized path like the C++ loader, which
    /// checks no extension at all.
    pub(super) fn load_platform_media_file(
        &mut self,
        device: usize,
        cpu: &mut ArmCpu,
        services: &mut dyn NativeServices,
    ) -> Result<()> {
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
            return Err(Error::Abi(
                "unsupported platform media file-load request shape".into(),
            ));
        }
        let path = self.memory.read(path_address, path_len)?;
        let components = path
            .split(|byte| matches!(byte, b'/' | b'\\'))
            .collect::<Vec<_>>();
        let file_name = components.last().copied().unwrap_or_default();
        if path.contains(&0)
            || components
                .iter()
                .any(|component| component.is_empty() || matches!(*component, b"." | b".."))
            || (device == 1 && !is_midi_file_name(file_name))
        {
            return Err(Error::Abi(
                "unsupported platform media file-load path".into(),
            ));
        }
        let mut sound = services.read_sound_file(&path)?;
        if sound.as_ref().is_none_or(Vec::is_empty) {
            let package_name = self.read_c_string(PACKAGE_NAME_DATA, 256)?;
            sound = services.read_package_file(&package_name, file_name)?;
        }
        // The C++ loader leaves previously loaded media untouched when the
        // read fails, so a failed load preserves the prior device state.
        let Some(sound) = sound.filter(|bytes| !bytes.is_empty()) else {
            cpu.set_register(0, u32::MAX);
            return Ok(());
        };
        self.media_devices[device] = PlatformMediaDevice {
            status: MEDIA_STATUS_LOADED,
            data: Some(sound),
        };
        cpu.set_register(0, 0);
        Ok(())
    }

    /// BUF_LOAD (203*10+device): adopts the caller-supplied encoded bytes.
    pub(super) fn load_platform_media_buffer(
        &mut self,
        device: usize,
        cpu: &mut ArmCpu,
    ) -> Result<()> {
        let input_address = GuestAddr(cpu.register(1));
        let input_len = cpu.register(2) as usize;
        let stack_pointer = GuestAddr(cpu.register(13));
        if input_address.0 == 0
            || input_len == 0
            || input_len > MAX_PLATFORM_MEDIA_BYTES
            || cpu.register(3) != 0
            || self.memory.read_u32(stack_pointer)? != 0
            || self.memory.read_u32(stack_pointer.checked_add(4)?)? != 0
        {
            return Err(Error::Abi(
                "unsupported platform media buffer-load request shape".into(),
            ));
        }
        // The C++ slot keeps a view of the caller's buffer; the Rust runtime
        // copies the bytes so a guest free between load and play cannot pull
        // the data out from under the player.
        let data = self.memory.read(input_address, input_len)?;
        self.media_devices[device] = PlatformMediaDevice {
            status: MEDIA_STATUS_LOADED,
            data: Some(data),
        };
        cpu.set_register(0, 0);
        Ok(())
    }

    /// PLAY_CUR_REQ (204*10+device) / RESUME_REQ (206*10+device): plays the
    /// loaded resource through the device's sound type.
    pub(super) fn play_platform_media(
        &mut self,
        device: usize,
        cpu: &mut ArmCpu,
        services: &mut dyn NativeServices,
    ) -> Result<()> {
        let input_address = GuestAddr(cpu.register(1));
        let input_len = cpu.register(2) as usize;
        let stack_pointer = GuestAddr(cpu.register(13));
        if cpu.register(3) != 0
            || self.memory.read_u32(stack_pointer)? != 0
            || self.memory.read_u32(stack_pointer.checked_add(4)?)? != 0
        {
            return Err(Error::Abi(
                "unsupported platform media play request shape".into(),
            ));
        }
        // T_DSM_MEDIA_PLAY is {cb, loop, block}; the C++ player keeps a
        // zeroed record unless the caller supplies all twelve bytes.
        let looped = if input_address.0 != 0 && input_len >= 12 {
            self.memory.read_u32(input_address.checked_add(4)?)? != 0
        } else {
            false
        };
        let Some(data) = self.media_devices[device].data.clone() else {
            cpu.set_register(0, u32::MAX);
            return Ok(());
        };
        let Some((sound_type, data)) = platform_media_sound_bytes(device, data) else {
            // Unknown device numbers fail exactly like the C++ host: the play
            // request is rejected while the loaded state is kept untouched.
            cpu.set_register(0, u32::MAX);
            return Ok(());
        };
        // A decode failure is part of the ABI contract: the C++ runtime
        // returns MR_FAILED and keeps the loaded resource for a retry.
        if services.play_sound(sound_type, &data, looped).is_ok() {
            self.media_devices[device].status = MEDIA_STATUS_PLAYING;
            cpu.set_register(0, 0);
        } else {
            cpu.set_register(0, u32::MAX);
        }
        Ok(())
    }

    /// PAUSE_REQ (205*10+device): pauses the active playback, keeping the
    /// decoded position so RESUME continues where the song stopped. The
    /// legacy C++ kernel stops the sound outright (its RESUME replays from
    /// the start); the host audio sink supports a real pause, which the
    /// mpmusic player UI relies on (status 1005 while paused). A pause
    /// without an active playback stays idempotent and keeps the state.
    pub(super) fn pause_platform_media(
        &mut self,
        device: usize,
        cpu: &mut ArmCpu,
        services: &mut dyn NativeServices,
    ) -> Result<()> {
        if cpu.register(1) != 0
            || cpu.register(2) != 0
            || cpu.register(3) != 0
            || self.memory.read_u32(GuestAddr(cpu.register(13)))? != 0
            || self
                .memory
                .read_u32(GuestAddr(cpu.register(13)).checked_add(4)?)?
                != 0
        {
            return Err(Error::Abi(
                "unsupported platform media pause request shape".into(),
            ));
        }
        if self.media_devices[device].status == MEDIA_STATUS_PLAYING {
            services.sound_pause()?;
            self.media_devices[device].status = MEDIA_STATUS_PAUSED;
        }
        cpu.set_register(0, 0);
        Ok(())
    }

    /// RESUME_REQ (206*10+device): continues a paused playback from its
    /// position. When nothing is paused the request falls back to the C++
    /// kernel's behaviour of replaying the stored resource from the start.
    pub(super) fn resume_platform_media(
        &mut self,
        device: usize,
        cpu: &mut ArmCpu,
        services: &mut dyn NativeServices,
    ) -> Result<()> {
        if cpu.register(3) != 0
            || self.memory.read_u32(GuestAddr(cpu.register(13)))? != 0
            || self
                .memory
                .read_u32(GuestAddr(cpu.register(13)).checked_add(4)?)?
                != 0
        {
            return Err(Error::Abi(
                "unsupported platform media resume request shape".into(),
            ));
        }
        if self.media_devices[device].status == MEDIA_STATUS_PAUSED
            && matches!(services.sound_resume(), Ok(true))
        {
            self.media_devices[device].status = MEDIA_STATUS_PLAYING;
            cpu.set_register(0, 0);
            return Ok(());
        }
        self.play_platform_media(device, cpu, services)
    }

    /// STOP_REQ (207*10+device): stops playback and returns to the loaded
    /// state, keeping the resource. This is the legacy C++ transition that
    /// the MIDI 2051/2071 arms also share.
    pub(super) fn stop_platform_media(
        &mut self,
        device: usize,
        cpu: &mut ArmCpu,
        services: &mut dyn NativeServices,
    ) -> Result<()> {
        if cpu.register(1) != 0
            || cpu.register(2) != 0
            || cpu.register(3) != 0
            || self.memory.read_u32(GuestAddr(cpu.register(13)))? != 0
            || self
                .memory
                .read_u32(GuestAddr(cpu.register(13)).checked_add(4)?)?
                != 0
        {
            return Err(Error::Abi(
                "unsupported platform media stop request shape".into(),
            ));
        }
        services.stop_sound()?;
        self.media_devices[device].status = MEDIA_STATUS_LOADED;
        cpu.set_register(0, 0);
        Ok(())
    }

    /// SETPOS (210*10+device): the input record T_SET_PLAY_POS carries an
    /// int32 position in seconds. Like the legacy handler the command
    /// succeeds even without an active playback, and the optional output
    /// cells are only written when both are non-null.
    pub(super) fn seek_platform_media(
        &mut self,
        module: usize,
        cpu: &mut ArmCpu,
        services: &mut dyn NativeServices,
    ) -> Result<()> {
        let input = GuestAddr(cpu.register(1));
        let input_len = cpu.register(2) as usize;
        let output = GuestAddr(cpu.register(3));
        let output_len = GuestAddr(self.memory.read_u32(GuestAddr(cpu.register(13)))?);
        if input.0 == 0 {
            if input_len != 0 {
                return Err(Error::Abi(
                    "unsupported platform media seek request shape".into(),
                ));
            }
        } else {
            if input_len < 4 {
                return Err(Error::Abi(
                    "unsupported platform media seek request shape".into(),
                ));
            }
            let seconds = self.memory.read_u32(input)?;
            services.sound_seek(u64::from(seconds) * 1000)?;
        }
        if output.0 != 0 && output_len.0 != 0 {
            let cell = self.allocate_guest_block_for_module(4, module)?;
            if let Some(cell) = cell {
                self.memory.write_u32(cell, 0)?;
                self.memory.write_u32(output, cell.0)?;
                self.memory.write_u32(output_len, 4)?;
            } else {
                self.memory.write_u32(output, 0)?;
                self.memory.write_u32(output_len, 0)?;
            }
        }
        cpu.set_register(0, 0);
        Ok(())
    }

    /// CLOSE (208*10+device) / FREE (216*10+device): stops playback and
    /// releases the resource.
    pub(super) fn close_platform_media(
        &mut self,
        device: usize,
        cpu: &mut ArmCpu,
        services: &mut dyn NativeServices,
    ) -> Result<()> {
        if cpu.register(1) != 0
            || cpu.register(2) != 0
            || cpu.register(3) != 0
            || self.memory.read_u32(GuestAddr(cpu.register(13)))? != 0
            || self
                .memory
                .read_u32(GuestAddr(cpu.register(13)).checked_add(4)?)?
                != 0
        {
            return Err(Error::Abi(
                "unsupported platform media close request shape".into(),
            ));
        }
        services.stop_sound()?;
        self.media_devices[device] = PlatformMediaDevice::idle();
        cpu.set_register(0, 0);
        Ok(())
    }

    /// GET_STATUS (209*10+device): reports the device state through the
    /// optional output cell and as the return value.
    pub(super) fn return_platform_media_status(
        &mut self,
        module: usize,
        device: usize,
        cpu: &mut ArmCpu,
    ) -> Result<()> {
        let status = self.media_devices[device].status;
        let output = GuestAddr(cpu.register(3));
        let output_len = GuestAddr(self.memory.read_u32(GuestAddr(cpu.register(13)))?);
        if output.0 == 0 && output_len.0 == 0 {
            cpu.set_register(0, status);
            return Ok(());
        }
        if output.0 == 0 {
            return Err(Error::Abi(
                "platform media status query has a null output pointer".into(),
            ));
        }
        if output_len.0 == 0 {
            return Err(Error::Abi(
                "platform media status query has a null output-length pointer".into(),
            ));
        }
        // The C++ executor copies the status int into a fresh guest
        // allocation and stores that pointer back into the caller's cell.
        let cell = self.allocate_guest_block_for_module(4, module)?;
        if let Some(cell) = cell {
            self.memory.write_u32(cell, status)?;
            self.memory.write_u32(output, cell.0)?;
            self.memory.write_u32(output_len, 4)?;
        } else {
            self.memory.write_u32(output, 0)?;
            self.memory.write_u32(output_len, 0)?;
        }
        cpu.set_register(0, status);
        Ok(())
    }

    /// Position and time queries (210/211/212/213/215 * 10 + device).
    /// 单位按 platEx 语义：212（总时长）与 213（当前时间）为秒，215 为
    /// 毫秒；212 的秒单位由 mpmusic game.ext 消费方证据确定——应用把
    /// 返回值原样存入歌曲结构体，且整个二进制无 ms→s 换算常量。210/211
    /// （字节 SETPOS/GETTIME）不追踪，保持零值记录。时间来自音频服务
    /// 的活跃播放进度；无解码进度的实现返回零（00:00 契约）。
    pub(super) fn return_platform_media_time(
        &mut self,
        module: usize,
        command: u32,
        cpu: &mut ArmCpu,
        services: &mut dyn NativeServices,
    ) -> Result<()> {
        let output = GuestAddr(cpu.register(3));
        let output_len = GuestAddr(self.memory.read_u32(GuestAddr(cpu.register(13)))?);
        if output.0 == 0 && output_len.0 == 0 {
            cpu.set_register(0, 0);
            return Ok(());
        }
        if output.0 == 0 {
            return Err(Error::Abi(
                "platform media time query has a null output pointer".into(),
            ));
        }
        if output_len.0 == 0 {
            return Err(Error::Abi(
                "platform media time query has a null output-length pointer".into(),
            ));
        }
        let (position_ms, duration_ms) = services.sound_playback_times();
        let position = match command / 10 {
            212 => u32::try_from(duration_ms / 1000).unwrap_or(u32::MAX),
            213 => u32::try_from(position_ms / 1000).unwrap_or(u32::MAX),
            215 => u32::try_from(position_ms).unwrap_or(u32::MAX),
            _ => 0,
        };
        let cell = self.allocate_guest_block_for_module(4, module)?;
        if let Some(cell) = cell {
            self.memory.write_u32(cell, position)?;
            self.memory.write_u32(output, cell.0)?;
            self.memory.write_u32(output_len, 4)?;
        } else {
            self.memory.write_u32(output, 0)?;
            self.memory.write_u32(output_len, 0)?;
        }
        cpu.set_register(0, 0);
        Ok(())
    }

    /// platEx 222x 多通道加载（MULTI_LOAD）。input 指向 T_BG_PLAY_REQ
    /// （{uint8* data; uint32 len; int32 loop}，12 字节），x 是
    /// T_MEDIA_TYPE。成功返回后续 223x/224x/225x 使用的句柄；不可解码
    /// 的媒体类型（AAC/LYRIC/UNKNOW…）与畸形请求形状回 MR_IGNORE，
    /// 让 guest 走自己的回退；载荷不可用（空指针/零长度/超限）与解码
    /// 失败回 MR_FAILED。
    pub(super) fn load_platform_sound_channel(
        &mut self,
        command: u32,
        cpu: &mut ArmCpu,
        services: &mut dyn NativeServices,
    ) -> Result<()> {
        let media_type = (command % 10) as usize;
        // T_MEDIA_TYPE 超出可解码集合（1..=6）时按 C++ 默认分支回
        // MR_IGNORE，表示平台不提供该功能。
        if !(1..=6).contains(&media_type) {
            cpu.set_register(0, 1);
            return self.trace_platform_media("MULTI_IGNORE", media_type, cpu);
        }
        // 畸形形状（空 input 或不足 T_BG_PLAY_REQ 的 12 字节）可能是
        // guest 的能力探测，软性回 MR_IGNORE 而非硬错误。
        if cpu.register(1) == 0 || (cpu.register(2) as usize) < 12 {
            cpu.set_register(0, 1);
            return self.trace_platform_media("MULTI_IGNORE", media_type, cpu);
        }
        let record = self.memory.read(GuestAddr(cpu.register(1)), 12)?;
        let data_address = u32::from_le_bytes(record[0..4].try_into().unwrap());
        let data_len = u32::from_le_bytes(record[4..8].try_into().unwrap()) as usize;
        let looped = i32::from_le_bytes(record[8..12].try_into().unwrap()) != 0;
        if data_address == 0 || data_len == 0 || data_len > MAX_MULTI_CHANNEL_SOUND_BYTES {
            cpu.set_register(0, u32::MAX);
            return self.trace_platform_media("MULTI_LOAD_FAILED", media_type, cpu);
        }
        let data = self.memory.read(GuestAddr(data_address), data_len)?;
        match services.load_sound_channel(media_type, &data, looped) {
            Ok(handle) => {
                cpu.set_register(0, handle);
                self.trace_platform_media("MULTI_LOAD", media_type, cpu)
            }
            Err(error) => {
                if std::env::var_os("SKYENGINE_TRACE_MR_CALLS").is_some() {
                    eprintln!("[platEx-media{media_type}] MULTI_LOAD error: {error}");
                }
                cpu.set_register(0, u32::MAX);
                Ok(())
            }
        }
    }

    /// platEx 223x/224x/225x 多通道播放/停止/释放。input 指向 222x
    /// 返回的 4 字节句柄。播放从头启动声部，停止冻结并保留已解码数
    /// 据（可重播），释放丢弃数据。成功回 MR_SUCCESS，句柄无效回
    /// MR_FAILED。
    pub(super) fn control_platform_sound_channel(
        &mut self,
        command: u32,
        cpu: &mut ArmCpu,
        services: &mut dyn NativeServices,
    ) -> Result<()> {
        let media_type = (command % 10) as usize;
        if cpu.register(1) == 0 {
            cpu.set_register(0, 1);
            return self.trace_platform_media("MULTI_IGNORE", media_type, cpu);
        }
        let handle = self.memory.read_u32(GuestAddr(cpu.register(1)))?;
        let (call, ok) = match command / 10 {
            223 => ("MULTI_PLAY", services.play_sound_channel(handle)),
            224 => ("MULTI_STOP", services.stop_sound_channel(handle)),
            225 => ("MULTI_RELEASE", services.release_sound_channel(handle)),
            other => {
                return Err(Error::Abi(format!(
                    "unsupported multi-channel sound command family {other}"
                )))
            }
        };
        cpu.set_register(0, if ok { 0 } else { u32::MAX });
        self.trace_platform_media(call, media_type, cpu)
    }
}

/// 单个多通道载荷的字节上限：真实设备的音效都很小（几十 KB 量级），
/// 8 MiB 只是把畸形 len 字段与真实请求区分开的护栏。
const MAX_MULTI_CHANNEL_SOUND_BYTES: usize = 8 * 1024 * 1024;

fn is_midi_file_name(name: &[u8]) -> bool {
    (name.len() > 4 && name[name.len() - 4..].eq_ignore_ascii_case(b".mid"))
        || (name.len() > 5 && name[name.len() - 5..].eq_ignore_ascii_case(b".midi"))
}

/// Maps an ACI media device to the sound type its playback uses, preparing
/// the bytes the audio sink expects. MIDI/WAV/MP3 decode, raw PCM is
/// interpreted as 8 kHz/16-bit/mono and wrapped in a WAV container, AMR and
/// AMR-WB decode through the shared `.amr` storage reader (the variant comes
/// from the file magic, so both devices 4 and 7 route to the same sound
/// type), and M4A decodes through the generic symphonia container probe.
fn platform_media_sound_bytes(device: usize, data: Vec<u8>) -> Option<(SoundType, Vec<u8>)> {
    match device {
        1 => Some((SoundType::Midi, data)),
        2 => Some((SoundType::Wav, data)),
        3 => Some((SoundType::Mp3, data)),
        4 | 7 => Some((SoundType::Amr, data)),
        5 => Some((SoundType::Wav, crate::audio::wrap_pcm_as_wav(&data))),
        6 => Some((SoundType::M4a, data)),
        _ => None,
    }
}
