use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{Cursor, Read, Seek, SeekFrom, Write},
    net::SocketAddrV4,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use encoding_rs::GBK;

use crate::{
    DnsMapping, Framebuffer, Package, PlatformAudio, PlatformDisplay, ResourceLimits, Result,
    SoundType, VIRTUAL_IMEI, VIRTUAL_IMSI,
    arm::{
        ExtLifecycleRequest, ExtRuntime, GuestAddr, NativeExtensionProfile, NativeServices,
        PLATFORM_USER_INFO_VERSION, START_FILE_PARAMETER_LEN,
    },
};

use super::{
    chunk::{MrChunk, Prototype},
    value::{Table, TableRef, Value},
};

mod network;
mod services;

use services::PackageServices;

// Baseline headless SDK compatibility profile. Local native fixtures prove the
// >= 2000 path; this is an advertised capability floor, not a device identity.
const BASELINE_VM_VERSION: u32 = 2_000;

// Vendor mythroad.h: #define SOUNDMAX 5. `mr_sound[SOUNDMAX]` slots back the
// Sound* natives and slot 0 doubles as the background-music channel
// (mythroad.c MRF_BgMusicSet/Start/Stop reuse SoundSet(0, ...)).
const MR_SOUND_MAX: usize = 5;

// Vendor platform-event contract for host widgets (types.h):
// MR_DIALOG_EVENT 6 delivers the widget completion with
// MR_DIALOG_KEY_OK 0 (submitted) / MR_DIALOG_KEY_CANCEL 1 (dismissed).
const MR_DIALOG_EVENT: i32 = 6;
const MR_DIALOG_KEY_OK: i32 = 0;
const MR_DIALOG_KEY_CANCEL: i32 = 1;

// The host editor UI is modal and single-instance; both the reference
// skyengine_api and this host hand out the same fixed editor handle.
const MR_LUA_EDITOR_HANDLE: i32 = 1234;

// Editor session ids encode their source: ext platform editors use their
// per-creation UI handle (a u32), the Lua gui editor uses this base ORed
// with a per-activation generation counter. The ranges stay disjoint.
const LUA_EDITOR_SESSION_BASE: u64 = 1 << 32;

// Vendor key codes (mythroad.h MR_KEY_*): the platform cancel_edit command
// is a SOFTRIGHT press, so the Lua editor treats SOFTRIGHT the same way.
const MR_KEY_SOFTRIGHT: i32 = 18;

struct MrSoundSlot {
    sound_type: SoundType,
    data: Vec<u8>,
}

#[derive(Clone, Debug)]
struct Bitmap {
    width: usize,
    height: usize,
    pixels: Vec<u16>,
    frame_height: Option<usize>,
    transparent_color: Option<u16>,
}

#[derive(Clone, Copy, Debug)]
struct BlitRegion {
    source_x: usize,
    source_y: usize,
    width: usize,
    height: usize,
    destination_x: i32,
    destination_y: i32,
    transparent_color: Option<u16>,
}

struct DirectorySearch {
    entries: Vec<Arc<[u8]>>,
    next: usize,
}

enum NativeFile {
    Host(File),
    Package(Cursor<Vec<u8>>),
}

enum ExtHelperInput<'a> {
    Buffer(&'a [u8]),
    Arguments([u32; 2]),
    Motion([i32; 3]),
}

#[derive(Debug, Eq, PartialEq)]
enum ApplicationStackTransition {
    Stay,
    Push((Vec<u8>, Vec<u8>, PathBuf)),
    Pop,
}

pub(crate) enum PreparedEntry {
    Mr(Arc<Prototype>),
    Native(Vec<u8>),
}

pub(crate) struct PreparedApplication {
    package: Arc<Package>,
    entry: Vec<u8>,
    prepared_entry: PreparedEntry,
    stack_transition: ApplicationStackTransition,
    previous_application: Option<(Vec<u8>, Vec<u8>)>,
    start_file_parameter: [u8; START_FILE_PARAMETER_LEN],
    ext_runtime: Option<ExtRuntime>,
}

pub(crate) struct MrHost {
    pub package: Arc<Package>,
    pub framebuffer: Framebuffer,
    pub display: Box<dyn PlatformDisplay>,
    audio: Box<dyn PlatformAudio>,
    mr_sound_slots: [Option<MrSoundSlot>; MR_SOUND_MAX],
    pub work_dir: PathBuf,
    font: Arc<[u8]>,
    memory_limit: u32,
    dns_mappings: Arc<[DnsMapping]>,
    wap_proxy_endpoint: Option<SocketAddrV4>,
    device_date: crate::DeviceDate,
    device_clock_origin: Instant,
    bitmaps: BTreeMap<i32, Bitmap>,
    directory_searches: BTreeMap<i32, DirectorySearch>,
    next_directory_handle: i32,
    native_files: BTreeMap<i32, NativeFile>,
    next_native_file_handle: i32,
    sdk_key: Option<i32>,
    current_entry: Vec<u8>,
    application_stack: Vec<(Vec<u8>, Vec<u8>, PathBuf)>,
    previous_application: Option<(Vec<u8>, Vec<u8>)>,
    start_file_parameter: [u8; START_FILE_PARAMETER_LEN],
    ext_runtime: Option<ExtRuntime>,
    socket_library: TableRef,
    mr_sockets: BTreeMap<i32, network::MrSocket>,
    next_mr_socket_handle: i32,
    mr_timer_interval: Option<Duration>,
    mr_timer_deadline: Option<Instant>,
    mr_timer_callback: Option<Arc<[u8]>>,
    mr_timer_pending: bool,
    mr_lifecycle_request: Option<ExtLifecycleRequest>,
    loaded_pack: Option<Arc<Package>>,
    lua_edit_active: bool,
    lua_edit_text: Option<String>,
    lua_edit_max_chars: usize,
    lua_edit_generation: u32,
}

pub(crate) struct MrHostConfig {
    pub work_dir: PathBuf,
    pub font: Arc<[u8]>,
    pub memory_limit: u32,
    pub dns_mappings: Arc<[DnsMapping]>,
    pub device_date: crate::DeviceDate,
    pub wap_proxy_endpoint: Option<SocketAddrV4>,
}

impl MrHost {
    pub fn new(
        package: Arc<Package>,
        framebuffer: Framebuffer,
        display: Box<dyn PlatformDisplay>,
        audio: Box<dyn PlatformAudio>,
        config: MrHostConfig,
    ) -> Self {
        Self {
            package,
            framebuffer,
            display,
            audio,
            mr_sound_slots: std::array::from_fn(|_| None),
            work_dir: config.work_dir,
            font: config.font,
            memory_limit: config.memory_limit,
            dns_mappings: config.dns_mappings,
            wap_proxy_endpoint: config.wap_proxy_endpoint,
            device_date: config.device_date,
            device_clock_origin: Instant::now(),
            bitmaps: BTreeMap::new(),
            directory_searches: BTreeMap::new(),
            next_directory_handle: 1,
            native_files: BTreeMap::new(),
            next_native_file_handle: 1,
            sdk_key: None,
            current_entry: b"start.mr".to_vec(),
            application_stack: Vec::new(),
            previous_application: None,
            start_file_parameter: [0; START_FILE_PARAMETER_LEN],
            ext_runtime: None,
            socket_library: network::socket_library(),
            mr_sockets: BTreeMap::new(),
            next_mr_socket_handle: 1,
            mr_timer_interval: None,
            mr_timer_deadline: None,
            mr_timer_callback: None,
            mr_timer_pending: false,
            mr_lifecycle_request: None,
            loaded_pack: None,
            lua_edit_active: false,
            lua_edit_text: None,
            lua_edit_max_chars: 0,
            lua_edit_generation: 0,
        }
    }

    /// Builds the `_mr_GetSysInfo` result table (mythroad.c L2808): the
    /// lowercase field set plus the COMPATIBILITY01 leading-uppercase aliases
    /// that legacy apps (QQDSM2007) read for their layout constants.
    fn sys_info_table(&self) -> TableRef {
        let info = Table::new();
        let mut values = info.borrow_mut();
        let set_number = |values: &mut Table, key: &'static [u8], value: f64| {
            values.set(bytes(key), Value::Number(value));
        };
        set_number(&mut values, b"vmver", f64::from(BASELINE_VM_VERSION));
        set_number(
            &mut values,
            b"scrw",
            f64::from(self.framebuffer.width()),
        );
        set_number(
            &mut values,
            b"scrh",
            f64::from(self.framebuffer.height()),
        );
        set_number(&mut values, b"chw", 16.0);
        set_number(&mut values, b"chh", 16.0);
        set_number(&mut values, b"ascw", 8.0);
        set_number(&mut values, b"asch", 16.0);
        set_number(
            &mut values,
            b"hsver",
            f64::from(PLATFORM_USER_INFO_VERSION),
        );
        values.set(
            bytes(b"packname"),
            bytes(&self.package.header().internal_name),
        );
        values.set(bytes(b"PackName"), bytes(&self.package.header().internal_name));
        set_number(&mut values, b"ScreenW", f64::from(self.framebuffer.width()));
        set_number(&mut values, b"ScreenH", f64::from(self.framebuffer.height()));
        set_number(&mut values, b"ChineseWidth", 16.0);
        set_number(&mut values, b"ChineseHigh", 16.0);
        set_number(&mut values, b"EnglishWidth", 8.0);
        set_number(&mut values, b"EnglishHigh", 16.0);
        values.set(bytes(b"hsman"), bytes(b"SkyEng"));
        // 6 字符 + NUL = 7 字节,与 vendor "opense" 等长:guest ext 按字符串
        // 长度累加计算协议帧内偏移,长度不一致会移位(gjxqy 下载请求 161→160)。
        values.set(bytes(b"hstype"), bytes(b"SE-V20"));
        values.set(bytes(b"IMEI"), bytes(VIRTUAL_IMEI));
        values.set(bytes(b"IMSI"), bytes(VIRTUAL_IMSI));
        drop(values);
        info
    }

    pub fn call(&mut self, name: &str, args: &[Value]) -> Result<Vec<Value>> {
        match name {
            // Vendor registers the same `_mr_GetSysInfo` for both
            // `sys.getInfo` and the global `GetSysInfo` (mythroad.c), so both
            // answer the identical table: VM version, screen and glyph
            // metrics, package name, and the virtual handset identity shared
            // with the EXT-side user-info slots. COMPATIBILITY01 builds also
            // carry the leading-uppercase aliases, and QQDSM2007 derives its
            // COM_CH_*/COM_EN_* layout constants from them at boot.
            "sys_get_info" | "GetSysInfo" => {
                Ok(vec![Value::Table(self.sys_info_table())])
            }
            "sys_find_start" => self.find_start(args),
            "sys_find_next" => self.find_next(args),
            "sys_find_stop" => self.find_stop(args),
            "_platEx" => {
                let command = integer(args.first())?;
                match command {
                    1201 => Ok(vec![bytes(&[16, 16, 8, 16]), Value::Number(0.0)]),
                    _ => Err(crate::Error::Platform(format!(
                        "unsupported _platEx command {command}"
                    ))),
                }
            }
            // `_plat(code, param)` (mythroad.c:3947 registers it as MRF_plat
            // → mr_plat): the Lua-level platform escape hatch. Command 101
            // rotates the LCD (mrporting.h:796 MR_LCD_ROTATE_NORMAL/90/180/
            // 270; the reference kernel accepts 0..3 and answers MR_SUCCESS
            // — gtcm/SphinxJoy games gate their landscape mode on it). Any
            // other request answers the non-fatal MR_IGNORE like the C++
            // platform default instead of faulting the VM.
            "_plat" => {
                let command = integer(args.first())?;
                let param = integer(args.get(1))?;
                if command == 101 && (0..=3).contains(&param) {
                    self.apply_lcd_rotation(param & 1 == 1)?;
                    Ok(vec![Value::Number(0.0)])
                } else {
                    Ok(vec![Value::Number(1.0)])
                }
            }
            "BitmapLoad" => {
                self.bitmap_load(args)?;
                Ok(Vec::new())
            }
            "BitmapShow" => {
                self.bitmap_show(args)?;
                Ok(Vec::new())
            }
            // Vendor registers `c2u` both as a global (mythroad.c) and as
            // string.c2u (mr_strlib.c strlib[17]): GBK bytes become UCS-2 BE
            // text whose result length includes the trailing U+0000.
            // QQDSM2007 converts its input-dialog titles through the global.
            "c2u" => Ok(vec![self.c2u(args)?]),
            // `u2c` (mr_strlib.c strlib[18] via _mr_u2c) is the ASCII-only
            // inverse of c2u: it keeps the low byte of every U+00xx unit and
            // stops at the first non-ASCII unit or the trailing null.
            "u2c" => Ok(vec![self.u2c(args)?]),
            "SpriteSet" => {
                let id = integer(args.first())?;
                let frame_height = positive_usize(args.get(1), "sprite frame height")?;
                let bitmap = self.bitmaps.get_mut(&id).ok_or_else(|| {
                    crate::Error::Platform(format!("SpriteSet references missing bitmap {id}"))
                })?;
                bitmap.frame_height = Some(frame_height);
                Ok(Vec::new())
            }
            "SpriteDraw" => {
                self.sprite_draw(args)?;
                Ok(Vec::new())
            }
            "_drawRect" | "DrawRect" | "_effSetCon" => {
                let x = integer(args.first())?;
                let y = integer(args.get(1))?;
                let width = integer(args.get(2))?;
                let height = integer(args.get(3))?;
                let color = color(args, 4)?;
                self.framebuffer.rect(x, y, width, height, color);
                Ok(Vec::new())
            }
            "_drawLine" | "DrawLine" => {
                let x0 = integer(args.first())?;
                let y0 = integer(args.get(1))?;
                let x1 = integer(args.get(2))?;
                let y1 = integer(args.get(3))?;
                let color = color(args, 4)?;
                self.framebuffer.line(x0, y0, x1, y1, color);
                Ok(Vec::new())
            }
            "DrawText" => {
                let text = value_bytes(args.first())?;
                let x = integer(args.get(1))?;
                let y = integer(args.get(2))?;
                let color = color(args, 3)?;
                let unicode = args.get(6).is_some_and(Value::truthy);
                self.draw_text(&text, x, y, color, unicode);
                Ok(Vec::new())
            }
            "_textWidth" => self.text_width(args),
            "DispUpEx" => {
                self.framebuffer.mark_presented();
                self.display.present(&self.framebuffer)?;
                Ok(Vec::new())
            }
            "TestCom" => Ok(vec![Value::Number(0.0)]),
            "TestCom1" => self.test_com1(args),
            "_com" => self.com(args),
            // tcpip.mr owns the protocol state machine; these functions expose
            // the mutable buffers and host sockets that it drives.
            "_closeNet" => self.close_network(),
            "socket_tcp" => self.socket_tcp(),
            "socket_connect" => self.socket_connect(args),
            "socket_getstate" => self.socket_get_state(args),
            "socket_getinfo" => self.socket_get_info(args),
            "socket_send" => self.socket_send(args),
            "socket_receive" => self.socket_receive(args),
            "socket_close" => self.socket_close(args),
            "_strCom" => self.string_command(args),
            "LoadTable" => Ok(vec![Value::Nil]),
            "SaveTable" => Ok(vec![Value::Number(0.0)]),
            "LoadPack" => Ok(vec![Value::Nil]),
            "RunFile" => {
                let package = value_bytes(args.first())?;
                let entry = value_bytes(args.get(1))?;
                if package.is_empty() || entry.is_empty() {
                    return Err(crate::Error::MrFault(
                        "RunFile package and entry must not be empty".into(),
                    ));
                }
                self.mr_lifecycle_request = Some(ExtLifecycleRequest::Restart {
                    package: package.to_vec(),
                    entry: entry.to_vec(),
                });
                Ok(vec![Value::Number(0.0)])
            }
            "UAReset" => Ok(Vec::new()),
            "TimerStart" => self.timer_start(args),
            "TimerStop" => self.timer_stop(),
            "mr_c_load" => self.mr_c_load(args),
            "_gc" => Ok(Vec::new()),
            "Exit" => {
                self.mr_lifecycle_request = Some(ExtLifecycleRequest::Exit);
                Ok(Vec::new())
            }
            _ => Err(crate::Error::Platform(format!(
                "unsupported MR platform function {name}"
            ))),
        }
    }

    pub fn native_timer_due_in(&self) -> Option<Duration> {
        self.mr_timer_deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    pub fn take_due_native_timer(&mut self) -> Result<bool> {
        let now = Instant::now();
        let mr_due = self
            .mr_timer_deadline
            .is_some_and(|deadline| deadline <= now);
        if mr_due {
            // 倍速：重排 deadline 同样按倍速压缩。单一 VMR 定时器语义下
            // 随后的回调级联若重新 arm（last-arm-wins），会覆盖这次重排。
            self.mr_timer_deadline = self
                .mr_timer_interval
                .map(|interval| now + crate::time_scale::scale_duration(interval));
            self.mr_timer_pending = true;
            crate::time_scale::trace_timer(format_args!("mr-fire"));
        }
        Ok(mr_due)
    }

    pub fn take_due_mr_timer_callback(&mut self) -> Option<Arc<[u8]>> {
        if !std::mem::take(&mut self.mr_timer_pending) {
            return None;
        }
        self.mr_timer_callback.clone()
    }

    /// 定时器空闲：应用 TimerStart 与 ext table[31] 都从未 arm 过
    /// （单一 VMR 定时器，两者写同一组字段）。
    pub fn mr_timer_idle(&self) -> bool {
        self.mr_timer_interval.is_none() && self.mr_timer_deadline.is_none()
    }

    /// 取出 ext 故障上下文环形缓冲(SKYENGINE_FAULT_CONTEXT 开启时 MR 层在
    /// fault 时调用并打印)。
    pub fn take_ext_fault_context(&mut self) -> Vec<String> {
        match self.ext_runtime.as_mut() {
            Some(runtime) => runtime.take_fault_context(),
            None => Vec::new(),
        }
    }

    pub fn dispatch_native_timer(&mut self) -> Result<()> {
        self.call_ext_helper(2, ExtHelperInput::Buffer(&[]))?;
        Ok(())
    }

    pub fn dispatch_external_action_completion(&mut self) -> Result<bool> {
        let Some(mut runtime) = self.ext_runtime.take() else {
            return Ok(false);
        };
        let result = {
            let mut services = PackageServices {
                package: self.package.clone(),
                work_dir: self.work_dir.clone(),
                directory_searches: &mut self.directory_searches,
                next_directory_handle: &mut self.next_directory_handle,
                files: &mut self.native_files,
                next_file_handle: &mut self.next_native_file_handle,
                font: &self.font,
                framebuffer: &mut self.framebuffer,
                display: self.display.as_mut(),
                audio: self.audio.as_mut(),
                mr_timer_interval: &mut self.mr_timer_interval,
                mr_timer_deadline: &mut self.mr_timer_deadline,
            };
            runtime.dispatch_pending_external_action(&mut services)
        };
        self.ext_runtime = Some(runtime);
        result
    }

    pub fn dispatch_pending_platform_event(&mut self) -> Result<bool> {
        let Some(mut runtime) = self.ext_runtime.take() else {
            return Ok(false);
        };
        let result = {
            let mut services = PackageServices {
                package: self.package.clone(),
                work_dir: self.work_dir.clone(),
                directory_searches: &mut self.directory_searches,
                next_directory_handle: &mut self.next_directory_handle,
                files: &mut self.native_files,
                next_file_handle: &mut self.next_native_file_handle,
                font: &self.font,
                framebuffer: &mut self.framebuffer,
                display: self.display.as_mut(),
                audio: self.audio.as_mut(),
                mr_timer_interval: &mut self.mr_timer_interval,
                mr_timer_deadline: &mut self.mr_timer_deadline,
            };
            runtime.dispatch_pending_platform_event(&mut services)
        };
        self.ext_runtime = Some(runtime);
        result
    }

    pub(super) fn mr_file_remove(&mut self, name: &[u8]) -> Result<i32> {
        self.package_services().remove_file(name)
    }

    pub(super) fn mr_file_open(&mut self, name: &[u8], mode: u32) -> Result<i32> {
        self.package_services().open_file(name, mode)
    }

    pub(super) fn mr_file_write(&mut self, handle: i32, bytes: &[u8]) -> Result<Option<usize>> {
        self.package_services().write_file(handle, bytes)
    }

    pub(super) fn mr_file_read(&mut self, handle: i32, len: usize) -> Result<Option<Vec<u8>>> {
        self.package_services().read_file(handle, len)
    }

    pub(super) fn mr_file_seek(
        &mut self,
        handle: i32,
        offset: i32,
        origin: u32,
    ) -> Result<Option<u64>> {
        self.package_services().seek_file(handle, offset, origin)
    }

    pub(super) fn mr_file_close(&mut self, handle: i32) -> Result<i32> {
        self.package_services().close_file(handle)
    }

    pub(super) fn load_pack(&mut self, name: Option<&[u8]>) -> Result<bool> {
        let Some(name) = name else {
            self.loaded_pack = None;
            return Ok(true);
        };
        let Some(path) = native_file_path(
            &self.work_dir,
            self.package.path(),
            &self.package.header().internal_name,
            name,
        ) else {
            return Ok(false);
        };
        self.loaded_pack = match Package::open(path, self.package.limits().clone()) {
            Ok(package) => Some(Arc::new(package)),
            Err(crate::Error::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                None
            }
            Err(error) => return Err(error),
        };
        Ok(self.loaded_pack.is_some())
    }

    pub(super) fn read_loaded_pack(&self, name: &[u8]) -> Result<Vec<u8>> {
        self.loaded_pack
            .as_ref()
            .ok_or_else(|| crate::Error::MrFault("no package is loaded".into()))?
            .read_named(name)
    }

    fn read_active_pack_resource(&self, name: &[u8]) -> Result<Vec<u8>> {
        let bytes = match &self.loaded_pack {
            Some(package) => package.read_named(name)?,
            None => self.package.read_named(name)?,
        };
        self.trace_pack_read(name, &bytes);
        Ok(bytes)
    }

    /// 取证：SKYENGINE_TRACE_FS=1 时打印包条目读取摘要（大条目带
    /// 0x740 窗口字节——gb12.uc2 提取链左移差一定位：进入脚本 VM 前
    /// 数据是否已坏）。
    fn trace_pack_read(&self, name: &[u8], bytes: &[u8]) {
        use std::sync::OnceLock;
        static ENABLED: OnceLock<bool> = OnceLock::new();
        if !*ENABLED.get_or_init(|| std::env::var_os("SKYENGINE_TRACE_FS").is_some()) {
            return;
        }
        let name = String::from_utf8_lossy(name);
        if bytes.len() > 1000 {
            let at = 0x740.min(bytes.len() - 16);
            let win: Vec<String> = bytes[at..at + 16].iter().map(|b| format!("{b:02x}")).collect();
            eprintln!(
                "[fs] pack_read {name:?} len={} win740=[{:#x}:{}]",
                bytes.len(),
                at,
                win.join(" ")
            );
        } else {
            eprintln!("[fs] pack_read {name:?} len={}", bytes.len());
        }
    }

    // Vendor _mr_readFile (mythroad.c:1316, lookfor=0) backs the base-library
    // loadfile (mr_L_loadfile): a work-dir file wins, then the active pack —
    // the LoadPack-switched pack when one is open, else the running package.
    pub(super) fn mr_read_file(&mut self, name: &[u8]) -> Result<Option<Vec<u8>>> {
        // 取证：SKYENGINE_TRACE_FS=1 时记录该直读路径（内核启动字体
        // 检查走 _mr_readFile，不经 services 探针——最后的盲区）。
        if std::env::var_os("SKYENGINE_TRACE_FS").is_some() {
            let shown = String::from_utf8_lossy(name);
            eprintln!("[fs] mr_read_file {shown:?}");
        }
        if let Some(path) = native_file_path(
            &self.work_dir,
            self.package.path(),
            &self.package.header().internal_name,
            name,
        )
        .and_then(|path| resolve_native_work_path(&self.work_dir, &path))
        {
            match std::fs::File::open(&path) {
                Ok(file) => {
                    let limit = self.package.limits().max_expanded_file_len;
                    let mut bytes = Vec::new();
                    let mut source = file.take(limit.saturating_add(1) as u64);
                    std::io::Read::read_to_end(&mut source, &mut bytes).map_err(|source| {
                        crate::Error::Io {
                            path: path.clone(),
                            source,
                        }
                    })?;
                    if bytes.len() > limit {
                        return Err(crate::Error::ResourceLimit(format!(
                            "file {} exceeds {limit} bytes",
                            path.display()
                        )));
                    }
                    return Ok(Some(bytes));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => return Err(crate::Error::Io { path, source }),
            }
        }
        match self.read_active_pack_resource(name) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(crate::Error::EntryNotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn package_services(&mut self) -> PackageServices<'_> {
        PackageServices {
            package: self.package.clone(),
            work_dir: self.work_dir.clone(),
            directory_searches: &mut self.directory_searches,
            next_directory_handle: &mut self.next_directory_handle,
            files: &mut self.native_files,
            next_file_handle: &mut self.next_native_file_handle,
            font: &self.font,
            framebuffer: &mut self.framebuffer,
            display: self.display.as_mut(),
            audio: self.audio.as_mut(),
            mr_timer_interval: &mut self.mr_timer_interval,
            mr_timer_deadline: &mut self.mr_timer_deadline,
        }
    }

    pub fn lifecycle_request(&self) -> Result<Option<ExtLifecycleRequest>> {
        if let Some(request) = &self.mr_lifecycle_request {
            return Ok(Some(request.clone()));
        }
        match self.ext_runtime.as_ref() {
            Some(runtime) => runtime.lifecycle_request(),
            None => Ok(None),
        }
    }

    pub fn route_key_event(&mut self, code: i32, pressed: bool) -> Result<Option<(i32, i32, i32)>> {
        let Some(mut runtime) = self.ext_runtime.take() else {
            if self.lua_edit_active {
                // While the host editor owns input the guest sees no keys:
                // the reference frontend consumes everything in edit mode.
                // SOFTRIGHT is the platform cancel key (skyengine_api
                // cancel_edit) and closes the editor with CANCEL.
                if pressed && code == MR_KEY_SOFTRIGHT {
                    self.lua_edit_active = false;
                    return Ok(Some((MR_DIALOG_EVENT, MR_DIALOG_KEY_CANCEL, 0)));
                }
                return Ok(None);
            }
            return Ok(Some((if pressed { 0 } else { 1 }, code, 0)));
        };
        let result = {
            let mut services = PackageServices {
                package: self.package.clone(),
                work_dir: self.work_dir.clone(),
                directory_searches: &mut self.directory_searches,
                next_directory_handle: &mut self.next_directory_handle,
                files: &mut self.native_files,
                next_file_handle: &mut self.next_native_file_handle,
                font: &self.font,
                framebuffer: &mut self.framebuffer,
                display: self.display.as_mut(),
                audio: self.audio.as_mut(),
                mr_timer_interval: &mut self.mr_timer_interval,
                mr_timer_deadline: &mut self.mr_timer_deadline,
            };
            runtime.route_key_event(code, pressed, &mut services)
        };
        self.ext_runtime = Some(runtime);
        result
    }

    pub fn route_pointer_event(
        &mut self,
        x: i32,
        y: i32,
        pressed: bool,
    ) -> Result<Option<(i32, i32, i32)>> {
        let Some(mut runtime) = self.ext_runtime.take() else {
            if self.lua_edit_active {
                // The modal host editor swallows pointer input too.
                return Ok(None);
            }
            return Ok(Some((if pressed { 2 } else { 3 }, x, y)));
        };
        let result = {
            let mut services = PackageServices {
                package: self.package.clone(),
                work_dir: self.work_dir.clone(),
                directory_searches: &mut self.directory_searches,
                next_directory_handle: &mut self.next_directory_handle,
                files: &mut self.native_files,
                next_file_handle: &mut self.next_native_file_handle,
                font: &self.font,
                framebuffer: &mut self.framebuffer,
                display: self.display.as_mut(),
                audio: self.audio.as_mut(),
                mr_timer_interval: &mut self.mr_timer_interval,
                mr_timer_deadline: &mut self.mr_timer_deadline,
            };
            runtime.route_pointer_event(x, y, pressed, &mut services)
        };
        self.ext_runtime = Some(runtime);
        result
    }

    pub fn route_pointer_move(&self, x: i32, y: i32) -> Option<(i32, i32, i32)> {
        match self.ext_runtime.as_ref() {
            Some(runtime) => runtime.route_pointer_move(x, y),
            None if self.lua_edit_active => None,
            None => Some((12, x, y)),
        }
    }

    pub fn route_text_input(&mut self, text: &str) -> Result<Option<(i32, i32, i32)>> {
        let Some(mut runtime) = self.ext_runtime.take() else {
            if !self.lua_edit_active {
                return Ok(None);
            }
            // Host editor submission: clamp to the editor's character
            // budget (reference api_apply_edit_text_command truncates by
            // code points), keep the text readable for e_getText, and
            // complete the widget with MR_DIALOG_KEY_OK.
            let committed: String = if self.lua_edit_max_chars == 0 {
                String::new()
            } else {
                text.chars().take(self.lua_edit_max_chars).collect()
            };
            self.lua_edit_text = Some(committed);
            self.lua_edit_active = false;
            return Ok(Some((MR_DIALOG_EVENT, MR_DIALOG_KEY_OK, 0)));
        };
        let result = runtime.route_text_input(text);
        self.ext_runtime = Some(runtime);
        result
    }

    pub fn active_editor_text(&self) -> Option<String> {
        if let Some(text) = self
            .ext_runtime
            .as_ref()
            .and_then(ExtRuntime::active_editor_text)
        {
            return Some(text);
        }
        if self.lua_edit_active {
            return Some(self.lua_edit_text.clone().unwrap_or_default());
        }
        None
    }

    /// A stable identity for the currently active editor session: the ext
    /// platform editor's per-creation UI handle, or the Lua gui editor's
    /// per-activation generation. `None` mirrors [`Self::active_editor_text`].
    ///
    /// Frontends that poll `active_editor_text` to drive their own input UI
    /// use this to distinguish two consecutive editor sessions: submitting
    /// text closes the editor asynchronously (the guest consumes the event
    /// and releases the editor on a later tick), so a second editor may go
    /// active without the sampled state ever passing through `None`.
    pub fn active_editor_session(&self) -> Option<u64> {
        if let Some(handle) = self
            .ext_runtime
            .as_ref()
            .and_then(ExtRuntime::active_editor_handle)
        {
            return Some(u64::from(handle));
        }
        if self.lua_edit_active {
            return Some(LUA_EDITOR_SESSION_BASE | u64::from(self.lua_edit_generation));
        }
        None
    }

    // Vendor gui library editor (mr_iolib_target.c EditCreate/EditRelease/
    // EditGetText over mr_editCreate/...): a single modal editor backed by
    // the host's native editor UI. Mirroring the reference engine, the
    // held text survives the completion event (the guest reads it through
    // e_getText inside its MR_DIALOG_EVENT handler) until e_release.
    pub(crate) fn mr_gui_edit_create(&mut self, text_ucs2: &[u8], max_size: i32) -> i32 {
        self.lua_edit_generation = self.lua_edit_generation.wrapping_add(1);
        self.lua_edit_active = true;
        self.lua_edit_text = Some(ucs2_bytes_to_string(text_ucs2));
        self.lua_edit_max_chars = if max_size > 0 { max_size as usize } else { 0 };
        MR_LUA_EDITOR_HANDLE
    }

    pub(crate) fn mr_gui_edit_release(&mut self) {
        self.lua_edit_active = false;
        self.lua_edit_text = None;
        self.lua_edit_max_chars = 0;
    }

    pub(crate) fn mr_gui_edit_text(&self) -> Option<Vec<u8>> {
        self.lua_edit_text
            .as_ref()
            .map(|text| string_to_ucs2_bytes(text))
    }

    pub fn motion_active(&self) -> bool {
        self.ext_runtime
            .as_ref()
            .is_some_and(ExtRuntime::motion_active)
    }

    pub fn finish_platform_event(&mut self) -> Result<()> {
        let Some(mut runtime) = self.ext_runtime.take() else {
            return Ok(());
        };
        let result = {
            let mut services = PackageServices {
                package: self.package.clone(),
                work_dir: self.work_dir.clone(),
                directory_searches: &mut self.directory_searches,
                next_directory_handle: &mut self.next_directory_handle,
                files: &mut self.native_files,
                next_file_handle: &mut self.next_native_file_handle,
                font: &self.font,
                framebuffer: &mut self.framebuffer,
                display: self.display.as_mut(),
                audio: self.audio.as_mut(),
                mr_timer_interval: &mut self.mr_timer_interval,
                mr_timer_deadline: &mut self.mr_timer_deadline,
            };
            runtime.finish_platform_event(&mut services)
        };
        self.ext_runtime = Some(runtime);
        result
    }

    pub fn dispatch_native_event(
        &mut self,
        event: i32,
        parameter0: i32,
        parameter1: i32,
    ) -> Result<()> {
        let mut input = [0_u8; 12];
        input[0..4].copy_from_slice(&event.to_le_bytes());
        input[4..8].copy_from_slice(&parameter0.to_le_bytes());
        input[8..12].copy_from_slice(&parameter1.to_le_bytes());
        self.call_ext_helper(1, ExtHelperInput::Buffer(&input))?;
        Ok(())
    }

    pub fn dispatch_native_motion(&mut self, x: i32, y: i32, z: i32) -> Result<()> {
        self.call_ext_helper(1, ExtHelperInput::Motion([x, y, z]))?;
        Ok(())
    }

    pub fn prepare_restart(
        &self,
        package_name: &[u8],
        entry: &[u8],
        limits: &ResourceLimits,
    ) -> Result<PreparedApplication> {
        // A bare package identity cannot distinguish duplicate installations. In that
        // ABI form, prefer the recorded parent at the top of the stack. A request that
        // carries a path is always resolved as that path, and the resolved target path
        // below decides whether this is a push, pop, or self-restart.
        let parent_identity_path =
            self.application_stack
                .last()
                .and_then(|(package, parent_entry, parent_path)| {
                    (is_identity_only_application_reference(package_name)
                        && package == package_name
                        && parent_entry == entry)
                        .then(|| parent_path.clone())
                });
        let unresolved_path = if let Some(parent_path) = parent_identity_path {
            parent_path
        } else {
            native_file_path(
                &self.work_dir,
                self.package.path(),
                &self.package.header().internal_name,
                package_name,
            )
            .ok_or_else(|| {
                crate::Error::Platform(format!(
                    "restart package path {:?} is outside the work directory",
                    String::from_utf8_lossy(package_name)
                ))
            })?
        };
        let path = resolve_native_work_path(&self.work_dir, &unresolved_path).ok_or_else(|| {
            crate::Error::Platform(format!(
                "restart package path {:?} is outside the work directory",
                String::from_utf8_lossy(package_name)
            ))
        })?;
        let package = Arc::new(Package::open(path, self.package.limits().clone())?);
        let entry_bytes = package.read_named(entry)?;
        let start_file_parameter = match self.ext_runtime.as_ref() {
            Some(runtime) => runtime.start_file_parameter()?,
            None => self.start_file_parameter,
        };
        let stack_transition = application_stack_transition(
            &self.application_stack,
            &self.package.header().internal_name,
            &self.current_entry,
            self.package.path(),
            package.path(),
            entry,
        );
        let previous_application = match &stack_transition {
            ApplicationStackTransition::Stay => self.previous_application.clone(),
            ApplicationStackTransition::Push(_) | ApplicationStackTransition::Pop => Some((
                self.package.header().internal_name.clone(),
                self.current_entry.clone(),
            )),
        };
        let (prepared_entry, ext_runtime) = if entry_bytes.starts_with(b"MRPGCMAP") {
            ExtRuntime::validate_module_image(&entry_bytes)?;
            let runtime = self.create_ext_runtime(
                &package,
                entry,
                previous_application.as_ref(),
                &start_file_parameter,
            )?;
            (PreparedEntry::Native(entry_bytes), Some(runtime))
        } else {
            let prototype = if entry_bytes.starts_with(b"\x1bMRP") {
                MrChunk::load(&entry_bytes, limits)?.root
            } else {
                super::vm::text::compile(&entry_bytes, limits)?
            };
            (PreparedEntry::Mr(prototype), None)
        };
        Ok(PreparedApplication {
            package,
            entry: entry.to_vec(),
            prepared_entry,
            stack_transition,
            previous_application,
            start_file_parameter,
            ext_runtime,
        })
    }

    pub fn acknowledge_lifecycle_request(&mut self) -> Result<()> {
        if self.mr_lifecycle_request.take().is_some() {
            return Ok(());
        }
        match self.ext_runtime.as_mut() {
            Some(runtime) => runtime.clear_lifecycle_request(),
            None => Ok(()),
        }
    }

    pub(crate) fn stop_audio(&mut self) {
        let _ = self.audio.stop_sound();
    }

    // Vendor sound library (mythroad.c SoundSet/SoundPlay/SoundStop +
    // MRF_BgMusic*): SOUNDMAX slots loaded through _mr_readFile, played
    // through the platform audio backend. Index errors surface as catchable
    // Lua faults, mirroring the vendor's mrp_error calls.
    pub(super) fn mr_sound_set(
        &mut self,
        index: usize,
        name: &[u8],
        sound_type: SoundType,
    ) -> Result<()> {
        if index >= MR_SOUND_MAX {
            return Err(crate::Error::MrFault(format!(
                "SoundSet :index {index} invalid!"
            )));
        }
        // Vendor: a "*" filename only clears the stored slot; the audio
        // device is left running.
        if name == b"*" {
            self.mr_sound_slots[index] = None;
            return Ok(());
        }
        let data = self
            .package_services()
            .read_sound_file(name)?
            .ok_or_else(|| {
                crate::Error::MrFault(format!(
                    "SoundSet {index}:cannot read \"{}\"!",
                    String::from_utf8_lossy(name)
                ))
            })?;
        self.mr_sound_slots[index] = Some(MrSoundSlot { sound_type, data });
        Ok(())
    }

    pub(super) fn mr_sound_play(&mut self, index: usize, looped: bool) -> Result<()> {
        if index >= MR_SOUND_MAX {
            return Err(crate::Error::MrFault(format!(
                "SoundPlay:index {index} invalid!"
            )));
        }
        // Vendor calls mr_playSound even for a never-set slot; the desktop
        // platform shim tolerates empty media, so an unset slot is a no-op.
        if let Some(slot) = self.mr_sound_slots[index].as_ref() {
            self.audio.play_sound(slot.sound_type, &slot.data, looped)?;
        }
        Ok(())
    }

    pub(super) fn mr_sound_stop(&mut self, index: usize) -> Result<()> {
        if index >= MR_SOUND_MAX {
            return Err(crate::Error::MrFault(format!(
                "SoundStop:index {index} invalid!"
            )));
        }
        self.audio.stop_sound()
    }

    // Vendor sys library (mr_iolib_target.c): mkDir/rmDir/getFileInfo/
    // getFileLen mirror mr_mkDir/mr_rmDir/mr_info/mr_getLen result codes,
    // getUptime mirrors mr_getTime (ms since engine start), and datetime
    // mirrors _mr_GetDatetime's {year, mon, day, hour, min, sec} table.
    pub(super) fn mr_mk_dir(&mut self, name: &[u8]) -> Result<i32> {
        self.package_services().create_dir(name)
    }

    pub(super) fn mr_rm_dir(&mut self, name: &[u8]) -> Result<i32> {
        self.package_services().remove_dir(name)
    }

    pub(super) fn mr_file_info(&mut self, name: &[u8]) -> Result<i32> {
        self.package_services().file_info(name)
    }

    pub(super) fn mr_file_len(&mut self, name: &[u8]) -> Result<i64> {
        // Vendor my_getLen answers -1 for a missing path instead of faulting.
        Ok(match self.package_services().file_len(name)? {
            Some(len) => i64::try_from(len).unwrap_or(-1),
            None => -1,
        })
    }

    pub(super) fn mr_uptime_ms(&self) -> u32 {
        let milliseconds = self.device_clock_origin.elapsed().as_millis() as u32;
        crate::time_scale::trace_timer(format_args!("mr-uptime -> {milliseconds}ms"));
        milliseconds
    }

    pub(super) fn mr_datetime(&self) -> crate::DeviceDate {
        self.device_date.advance(self.device_clock_origin.elapsed())
    }

    pub fn commit_application(&mut self, prepared: PreparedApplication) -> PreparedEntry {
        match prepared.stack_transition {
            ApplicationStackTransition::Stay => {}
            ApplicationStackTransition::Push(application) => {
                self.application_stack.push(application)
            }
            ApplicationStackTransition::Pop => {
                self.application_stack.pop();
            }
        }

        self.start_file_parameter = prepared.start_file_parameter;
        self.previous_application = prepared.previous_application;
        self.package = prepared.package;
        self.bitmaps.clear();
        self.stop_audio();
        self.directory_searches.clear();
        self.next_directory_handle = 1;
        self.native_files.clear();
        self.next_native_file_handle = 1;
        self.sdk_key = None;
        self.current_entry = prepared.entry;
        self.ext_runtime = prepared.ext_runtime;
        self.reset_mr_platform_state();
        prepared.prepared_entry
    }

    pub(crate) fn discard_failed_application_runtime(&mut self) {
        self.ext_runtime = None;
        self.bitmaps.clear();
        self.stop_audio();
        self.directory_searches.clear();
        self.next_directory_handle = 1;
        self.native_files.clear();
        self.next_native_file_handle = 1;
        self.sdk_key = None;
        self.reset_mr_platform_state();
    }

    fn reset_mr_platform_state(&mut self) {
        self.socket_library = network::socket_library();
        self.mr_sockets.clear();
        self.next_mr_socket_handle = 1;
        self.mr_timer_interval = None;
        self.mr_timer_deadline = None;
        self.mr_timer_callback = None;
        self.mr_timer_pending = false;
        self.loaded_pack = None;
        self.lua_edit_active = false;
        self.lua_edit_text = None;
        self.lua_edit_max_chars = 0;
    }

    pub fn set_current_entry(&mut self, entry: &[u8]) {
        self.current_entry = entry.to_vec();
    }

    // Vendor c2u (mr_Gb2312toUnicode -> GBStrToUCS2BEStr): every GBK byte
    // sequence becomes a UCS-2 BE unit, unmappable sequences become U+FDFF,
    // and the result always carries a trailing U+0000 that counts toward the
    // reported length ((chars + 1) * 2 bytes).
    fn c2u(&mut self, args: &[Value]) -> Result<Value> {
        let text = value_bytes(args.first())?;
        let (decoded, _, _) = GBK.decode(&text[..]);
        let mut out = Vec::with_capacity((decoded.chars().count() + 1) * 2);
        for ch in decoded.chars() {
            let unit = match ch as u32 {
                0xFFFD => 0xFDFF,
                code @ 0..=0xFFFF => code as u16,
                _ => 0xFDFF,
            };
            out.extend_from_slice(&unit.to_be_bytes());
        }
        out.extend_from_slice(&0u16.to_be_bytes());
        Ok(Value::Bytes(out.into()))
    }

    // Vendor u2c (str_u2c -> _mr_u2c): walk UCS-2 BE pairs, keep the low
    // byte of every U+00xx unit, and stop at the first non-ASCII unit or the
    // trailing null; inputs shorter than one unit answer "".
    fn u2c(&mut self, args: &[Value]) -> Result<Value> {
        let text = value_bytes(args.first())?;
        if text.len() < 2 {
            return Ok(Value::Bytes(Vec::new().into()));
        }
        let mut out = Vec::new();
        let mut offset = 0;
        while offset + 1 < text.len()
            && text[offset] as u16 + text[offset + 1] as u16 != 0
        {
            if text[offset] != 0 {
                break;
            }
            out.push(text[offset + 1]);
            offset += 2;
        }
        Ok(Value::Bytes(out.into()))
    }

    fn bitmap_load(&mut self, args: &[Value]) -> Result<()> {
        let id = integer(args.first())?;
        // Vendor MRF_BitmapLoad bounds the slot index by BITMAPMAX (30)
        // before touching the slot, frees any previous contents, and treats
        // a "*" filename as a slot clear that succeeds without reading any
        // file. QQDSM2007 clears its cursor sprite this way when the left
        // softkey opens the menu (BitmapLoad(1, "*", 0, 0, 0, 0, 0)).
        if !(0..=MR_BITMAP_MAX).contains(&id) {
            return Err(crate::Error::MrFault(format!(
                "BitmapLoad:index {id} invalid!"
            )));
        }
        let name = value_bytes(args.get(1))?;
        self.bitmaps.remove(&id);
        if name.first() == Some(&b'*') {
            return Ok(());
        }
        let width = positive_usize(args.get(4), "bitmap width")?;
        let height = positive_usize(args.get(5), "bitmap height")?;
        let raw = self.read_active_pack_resource(&name)?;
        let pixel_count = width
            .checked_mul(height)
            .ok_or_else(|| crate::Error::Platform(format!("bitmap {id} dimensions overflow")))?;
        let byte_count = pixel_count
            .checked_mul(2)
            .ok_or_else(|| crate::Error::Platform(format!("bitmap {id} byte count overflow")))?;
        let (pixels, transparent_color) = if raw.starts_with(b"BM") {
            (decode_bmp(&raw, width, height)?, None)
        } else {
            if raw.len() < byte_count {
                return Err(crate::Error::Platform(format!(
                    "bitmap {} contains {} bytes, needs {byte_count}",
                    String::from_utf8_lossy(&name),
                    raw.len()
                )));
            }
            let pixels = raw[..byte_count]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pixel| u16::from_le_bytes([pixel[0], pixel[1]]))
                .collect::<Vec<_>>();
            let transparent_color = pixels.first().copied();
            (pixels, transparent_color)
        };
        self.bitmaps.insert(
            id,
            Bitmap {
                width,
                height,
                pixels,
                frame_height: None,
                transparent_color,
            },
        );
        Ok(())
    }

    fn bitmap_show(&mut self, args: &[Value]) -> Result<()> {
        let id = integer(args.first())?;
        let x = integer(args.get(1))?;
        let y = integer(args.get(2))?;
        let bitmap = self.bitmaps.get(&id).ok_or_else(|| {
            crate::Error::Platform(format!("BitmapShow references missing bitmap {id}"))
        })?;
        blit(
            &mut self.framebuffer,
            bitmap,
            BlitRegion {
                source_x: 0,
                source_y: 0,
                width: bitmap.width,
                height: bitmap.height,
                destination_x: x,
                destination_y: y,
                transparent_color: None,
            },
        );
        Ok(())
    }

    fn sprite_draw(&mut self, args: &[Value]) -> Result<()> {
        let id = integer(args.first())?;
        let frame = integer(args.get(1))?.max(0) as usize;
        let x = integer(args.get(2))?;
        let y = integer(args.get(3))?;
        let bitmap = self.bitmaps.get(&id).ok_or_else(|| {
            crate::Error::Platform(format!("SpriteDraw references missing bitmap {id}"))
        })?;
        let frame_height = bitmap.frame_height.unwrap_or(bitmap.height);
        let source_y = frame.saturating_mul(frame_height);
        let source_y = if source_y == bitmap.height && frame > 0 {
            (frame - 1).saturating_mul(frame_height)
        } else {
            source_y
        };
        if source_y >= bitmap.height {
            return Ok(());
        }
        blit(
            &mut self.framebuffer,
            bitmap,
            BlitRegion {
                source_x: 0,
                source_y,
                width: bitmap.width,
                height: frame_height.min(bitmap.height - source_y),
                destination_x: x,
                destination_y: y,
                transparent_color: bitmap.transparent_color,
            },
        );
        Ok(())
    }

    /// Draws text in the 16px gb16 font. GBK input is decoded to Unicode
    /// first; Unicode input (c2u output) is already UCS2 big-endian and is
    /// consumed as NUL-terminated 16-bit codepoints, mirroring the C
    /// `_DrawText` contract where `is_unicode` selects between the two
    /// encodings.
    fn draw_text(&mut self, encoded: &[u8], mut x: i32, y: i32, color: u16, unicode: bool) {
        let codepoints: Vec<u32> = if unicode {
            encoded
                .as_chunks::<2>()
                .0
                .iter()
                .map(|bytes| u32::from(u16::from_be_bytes([bytes[0], bytes[1]])))
                .take_while(|&codepoint| codepoint != 0)
                .collect()
        } else {
            let (decoded, _, _) = GBK.decode(encoded);
            decoded.chars().map(|c| c as u32).collect()
        };
        for codepoint in codepoints {
            let width = if codepoint < 128 { 8 } else { 16 };
            let Some(start) = (codepoint as usize).checked_mul(32) else {
                continue;
            };
            let Some(glyph) = self.font.get(start..start + 32) else {
                x += width;
                continue;
            };
            for row in 0..16_i32 {
                let bits =
                    u16::from_be_bytes([glyph[row as usize * 2], glyph[row as usize * 2 + 1]]);
                for column in 0..width {
                    if bits & (0x8000_u16 >> column) != 0 {
                        self.framebuffer.point(x + column, y + row, color);
                    }
                }
            }
            x += width;
        }
    }

    fn text_width(&self, args: &[Value]) -> Result<Vec<Value>> {
        let width = match args.first() {
            Some(Value::Bytes(encoded)) => {
                let unicode = args.get(1).is_some_and(Value::truthy);
                if unicode {
                    encoded
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .take_while(|bytes| **bytes != [0, 0])
                        .map(|bytes| {
                            if u16::from_be_bytes([bytes[0], bytes[1]]) < 128 {
                                8
                            } else {
                                16
                            }
                        })
                        .sum::<i32>()
                } else {
                    let (decoded, _, _) = GBK.decode(encoded);
                    decoded
                        .chars()
                        .map(|character| if character.is_ascii() { 8 } else { 16 })
                        .sum::<i32>()
                }
            }
            Some(Value::Number(codepoint)) => {
                if *codepoint >= 0.0 && *codepoint < 128.0 {
                    8
                } else {
                    16
                }
            }
            other => {
                return Err(crate::Error::MrFault(format!(
                    "_textWidth expects string or character code, got {other:?}"
                )));
            }
        };
        Ok(vec![Value::Number(f64::from(width)), Value::Number(16.0)])
    }

    fn com(&mut self, args: &[Value]) -> Result<Vec<Value>> {
        let command = integer(args.first())?;
        match command {
            // UI reset and screen mode notifications used by the baseline SDK.
            0 | 1 | 403 => Ok(vec![Value::Number(0.0)]),
            // LG memory-pool gauges (mem.c LG_mem_min/top/left). The mr
            // script heap lives in the host VM allocator instead of the
            // platform pool, so the pool reports a pristine budget: nothing
            // was ever carved out of it.
            100 | 102 => Ok(vec![Value::Number(f64::from(self.memory_limit))]),
            101 => Ok(vec![Value::Number(0.0)]),
            // Select the SDK network access point. Host networking is already
            // routed through the configured DNS/socket layer, so only validate
            // the legacy profile name and acknowledge the selection.
            402 => {
                let access_point = value_bytes(args.get(1))?;
                if !matches!(access_point.as_ref(), b"cmwap" | b"cmnet") {
                    return Err(crate::Error::Platform(format!(
                        "unsupported network access point {access_point:?}"
                    )));
                }
                self.initialize_network();
                Ok(vec![Value::Number(0.0)])
            }
            // Register the SDK compatibility key selected by start.mr.
            3629 => {
                self.sdk_key = Some(integer(args.get(1))?);
                Ok(vec![Value::Number(0.0)])
            }
            other => Err(crate::Error::Platform(format!(
                "unsupported _com command {other} with arguments {args:?}"
            ))),
        }
    }

    fn timer_start(&mut self, args: &[Value]) -> Result<Vec<Value>> {
        let milliseconds = integer(args.get(1))?;
        if milliseconds <= 0 {
            return Err(crate::Error::MrFault(format!(
                "TimerStart interval must be positive, got {milliseconds}"
            )));
        }
        let interval = Duration::from_millis(milliseconds as u64);
        let callback = value_bytes(args.get(2))?;
        self.mr_timer_interval = Some(interval);
        // 倍速：deadline 按倍速压缩，interval 保持 guest 语义原值。
        self.mr_timer_deadline = Some(Instant::now() + crate::time_scale::scale_duration(interval));
        self.mr_timer_callback = Some(callback);
        self.mr_timer_pending = false;
        crate::time_scale::trace_timer(format_args!("mr-arm interval={milliseconds}ms"));
        Ok(vec![Value::Number(0.0)])
    }

    fn timer_stop(&mut self) -> Result<Vec<Value>> {
        self.mr_timer_interval = None;
        self.mr_timer_deadline = None;
        self.mr_timer_callback = None;
        self.mr_timer_pending = false;
        crate::time_scale::trace_timer(format_args!("mr-stop"));
        Ok(vec![Value::Number(0.0)])
    }

    fn string_command(&mut self, args: &[Value]) -> Result<Vec<Value>> {
        let command = integer(args.first())?;
        match command {
            // Optional carrier/SMS metadata suffix. The deterministic headless
            // profile has no provider, so it returns the ABI's neutral string.
            501 => {
                let input = value_bytes(args.get(1))?;
                if input.len() > 1024 {
                    return Err(crate::Error::Platform(format!(
                        "_strCom 501 input is {} bytes; limit is 1024",
                        input.len()
                    )));
                }
                Ok(vec![bytes(b"")])
            }
            // Read a checked byte range from the currently loaded MRP file.
            600 => {
                let requested = value_bytes(args.get(1))?;
                if matches!(requested.as_ref(), [b'*', b'A'..=b'Z']) {
                    // M0 firmware slots are empty until a host registers one.
                    return Ok(Vec::new());
                }
                let requested = requested.strip_prefix(b"%").unwrap_or(&requested);
                let current = self
                    .package
                    .path()
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| {
                        crate::Error::Platform("current package name is not valid Unicode".into())
                    })?;
                if requested != current.as_bytes() {
                    return Err(crate::Error::Platform(format!(
                        "_strCom 600 cannot read package {} while {} is loaded",
                        String::from_utf8_lossy(requested),
                        self.package.path().display()
                    )));
                }
                let offset = nonnegative_usize(args.get(2), "package offset")?;
                let len = nonnegative_usize(args.get(3), "package range length")?;
                Ok(vec![bytes(&self.package.read_raw_range(offset, len)?)])
            }
            // 601 reads a package resource into a VM byte string.
            601 => {
                let requested = value_bytes(args.get(1))?;
                // The vendor hands the buffer to _mr_readFile as a C
                // string (mythroad.c case 601), so the lookup name ends
                // at the first NUL and a missing resource answers nil
                // instead of faulting the VM.
                let name = match requested.iter().position(|byte| *byte == 0) {
                    Some(end) => &requested[..end],
                    None => &requested[..],
                };
                match self.package.read_named(name) {
                    Ok(data) => Ok(vec![bytes(&data)]),
                    Err(crate::Error::EntryNotFound(_)) => Ok(Vec::new()),
                    Err(error) => Err(error),
                }
            }
            // Turn an MRPGCMAP image into the callable first-stage EXT loader.
            800 => {
                let code = integer(args.get(2))?;
                enum ImageSource {
                    Bytes(Arc<[u8]>),
                    GuestRange(GuestAddr, usize),
                }
                let source = match args.get(1) {
                    Some(Value::Bytes(image)) => ImageSource::Bytes(image.clone()),
                    Some(Value::Table(range)) => {
                        let range = range.borrow();
                        let address = guest_u32(&range.get(&Value::Number(1.0)), "EXT address")?;
                        let len =
                            guest_u32(&range.get(&Value::Number(2.0)), "EXT length")? as usize;
                        ImageSource::GuestRange(GuestAddr(address), len)
                    }
                    other => {
                        return Err(crate::Error::MrFault(format!(
                            "_strCom 800 expects EXT bytes or {{address, length}}, got {other:?}"
                        )));
                    }
                };
                if std::env::var_os("SKYENGINE_TRACE_EXT_IMAGE").is_some() {
                    match &source {
                        ImageSource::Bytes(image) => {
                            eprintln!("[ext-image] 800 source=Bytes len={}", image.len())
                        }
                        ImageSource::GuestRange(address, len) => eprintln!(
                            "[ext-image] 800 source=GuestRange addr={:#010x} len={}",
                            address.0, len
                        ),
                    }
                }
                let mut runtime = self.take_or_create_ext_runtime()?;
                let package = self.package.clone();
                let mut services = PackageServices {
                    package,
                    work_dir: self.work_dir.clone(),
                    directory_searches: &mut self.directory_searches,
                    next_directory_handle: &mut self.next_directory_handle,
                    files: &mut self.native_files,
                    next_file_handle: &mut self.next_native_file_handle,
                    font: &self.font,
                    framebuffer: &mut self.framebuffer,
                    display: self.display.as_mut(),
                    audio: self.audio.as_mut(),
                    mr_timer_interval: &mut self.mr_timer_interval,
                    mr_timer_deadline: &mut self.mr_timer_deadline,
                };
                let result = match source {
                    ImageSource::Bytes(image) => {
                        runtime.load_and_call_entry(&image, code, &mut services)
                    }
                    ImageSource::GuestRange(address, len) => {
                        runtime.load_guest_image_and_call_entry(address, len, code, &mut services)
                    }
                };
                self.ext_runtime = Some(runtime);
                Ok(vec![Value::Number(f64::from(result?))])
            }
            // Invoke the helper registered by the most recently loaded EXT.
            801 => {
                let input = ext_input(args.get(1))?;
                let code = integer(args.get(2))?;
                let (result, output) = self.call_ext_helper(code, input)?;
                Ok(vec![bytes(&output), Value::Number(f64::from(result))])
            }
            3 => Ok(vec![Value::Number(0.0)]),
            other => Err(crate::Error::Platform(format!(
                "unsupported _strCom command {other} with arguments {args:?}"
            ))),
        }
    }

    pub fn run_native_entry(&mut self, image: &[u8]) -> Result<()> {
        let mut runtime = self.take_or_create_ext_runtime()?;
        let package = self.package.clone();
        let result = {
            let mut services = PackageServices {
                package,
                work_dir: self.work_dir.clone(),
                directory_searches: &mut self.directory_searches,
                next_directory_handle: &mut self.next_directory_handle,
                files: &mut self.native_files,
                next_file_handle: &mut self.next_native_file_handle,
                font: &self.font,
                framebuffer: &mut self.framebuffer,
                display: self.display.as_mut(),
                audio: self.audio.as_mut(),
                mr_timer_interval: &mut self.mr_timer_interval,
                mr_timer_deadline: &mut self.mr_timer_deadline,
            };
            runtime.load_and_call_entry(image, 0, &mut services)
        };
        self.ext_runtime = Some(runtime);
        result?;

        self.call_ext_helper(6, ExtHelperInput::Arguments([1, BASELINE_VM_VERSION]))?;
        self.call_ext_helper(0, ExtHelperInput::Buffer(&[]))?;
        Ok(())
    }

    fn take_or_create_ext_runtime(&mut self) -> Result<ExtRuntime> {
        if let Some(runtime) = self.ext_runtime.take() {
            return Ok(runtime);
        }
        self.create_ext_runtime(
            &self.package,
            &self.current_entry,
            self.previous_application.as_ref(),
            &self.start_file_parameter,
        )
    }

    /// Rotates the shared screen canvas for `_plat(101, MR_LCD_ROTATE_*)`.
    /// When an EXT runtime already owns the screen state the request routes
    /// through its orientation switch (which updates the EXT data slots and
    /// re-presents); otherwise the framebuffer resize alone records the
    /// request, because `create_ext_runtime` seeds the EXT screen from the
    /// framebuffer dimensions.
    fn apply_lcd_rotation(&mut self, landscape: bool) -> Result<()> {
        if let Some(mut runtime) = self.ext_runtime.take() {
            let result = {
                let mut services = self.package_services();
                runtime.set_screen_orientation(landscape, &mut services)
            };
            self.ext_runtime = Some(runtime);
            return result;
        }
        let (width, height) = (self.framebuffer.width(), self.framebuffer.height());
        let short = width.min(height);
        let long = width.max(height);
        let (width, height) = if landscape { (long, short) } else { (short, long) };
        if (width, height) != (self.framebuffer.width(), self.framebuffer.height()) {
            self.framebuffer.resize(width, height)?;
            self.display.resize(width, height)?;
        }
        Ok(())
    }

    fn create_ext_runtime(
        &self,
        package: &Package,
        entry: &[u8],
        previous_application: Option<&(Vec<u8>, Vec<u8>)>,
        start_file_parameter: &[u8; START_FILE_PARAMETER_LEN],
    ) -> Result<ExtRuntime> {
        let mut runtime = ExtRuntime::new(
            self.framebuffer.width(),
            self.framebuffer.height(),
            &package.header().internal_name,
            entry,
            self.memory_limit,
        )?;
        runtime.set_native_extension_profile(native_extension_profile(
            package.header().platform,
            package.header().version,
        ))?;
        // KVM（platEx 0x70003）通道用当前 MRP 的宿主路径解析 guest 内联
        // 相对包名——KVM 跑的就是当前 MRP 内嵌的 jc.jar。
        runtime.set_kvm_package_path(package.path().to_path_buf());
        runtime.set_dns_mappings(self.dns_mappings.clone());
        runtime.set_wap_proxy_endpoint(self.wap_proxy_endpoint);
        runtime.set_device_date(self.device_date.advance(self.device_clock_origin.elapsed()));
        let (previous_package, previous_entry) = previous_application
            .map(|(package, entry)| (package.as_slice(), entry.as_slice()))
            .unwrap_or((&[], &[]));
        runtime.set_previous_application(previous_package, previous_entry)?;
        runtime.set_start_file_parameter(start_file_parameter)?;
        Ok(runtime)
    }

    fn mr_c_load(&mut self, args: &[Value]) -> Result<Vec<Value>> {
        let code = integer(args.first())?;
        let input = args
            .get(1)
            .and_then(Value::bytes)
            .unwrap_or_else(|| Arc::from(&b""[..]));
        let (result, output) =
            self.call_ext_helper(code, ExtHelperInput::Buffer(input.as_ref()))?;
        Ok(vec![Value::Number(f64::from(result)), bytes(&output)])
    }

    fn call_ext_helper(&mut self, code: i32, input: ExtHelperInput<'_>) -> Result<(i32, Vec<u8>)> {
        let package = self.package.clone();
        let mut runtime = self
            .ext_runtime
            .take()
            .ok_or_else(|| crate::Error::Abi("no EXT runtime has been initialized".into()))?;
        let result = {
            let mut services = PackageServices {
                package,
                work_dir: self.work_dir.clone(),
                directory_searches: &mut self.directory_searches,
                next_directory_handle: &mut self.next_directory_handle,
                files: &mut self.native_files,
                next_file_handle: &mut self.next_native_file_handle,
                font: &self.font,
                framebuffer: &mut self.framebuffer,
                display: self.display.as_mut(),
                audio: self.audio.as_mut(),
                mr_timer_interval: &mut self.mr_timer_interval,
                mr_timer_deadline: &mut self.mr_timer_deadline,
            };
            match input {
                ExtHelperInput::Buffer(input) => {
                    runtime.call_active_helper(code, input, &mut services)
                }
                ExtHelperInput::Arguments(arguments) => {
                    runtime.call_active_helper_raw(code, arguments, &mut services)
                }
                ExtHelperInput::Motion([x, y, z]) => {
                    runtime.call_active_motion_event(x, y, z, &mut services)
                }
            }
        };
        self.ext_runtime = Some(runtime);
        result
    }

    fn find_start(&mut self, args: &[Value]) -> Result<Vec<Value>> {
        let directory = value_bytes(args.first())?;
        let Some(path) = safe_work_path(&self.work_dir, &directory) else {
            return Ok(vec![Value::Number(-1.0), bytes(b"")]);
        };
        let Ok(entries) = fs::read_dir(path) else {
            return Ok(vec![Value::Number(-1.0), bytes(b"")]);
        };
        let mut names = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let (encoded, _, _) = GBK.encode(&name);
                Arc::<[u8]>::from(encoded.as_ref())
            })
            .collect::<Vec<_>>();
        names.sort();

        let handle = self.allocate_directory_handle()?;
        let first = names
            .first()
            .cloned()
            .unwrap_or_else(|| Arc::from(&b""[..]));
        self.directory_searches.insert(
            handle,
            DirectorySearch {
                entries: names,
                next: 1,
            },
        );
        Ok(vec![Value::Number(f64::from(handle)), Value::Bytes(first)])
    }

    fn find_next(&mut self, args: &[Value]) -> Result<Vec<Value>> {
        let handle = integer(args.first())?;
        let Some(search) = self.directory_searches.get_mut(&handle) else {
            return Ok(vec![Value::Nil]);
        };
        let Some(name) = search.entries.get(search.next).cloned() else {
            return Ok(vec![Value::Nil]);
        };
        search.next += 1;
        Ok(vec![Value::Bytes(name)])
    }

    fn find_stop(&mut self, args: &[Value]) -> Result<Vec<Value>> {
        let handle = integer(args.first())?;
        Ok(vec![Value::Number(
            if self.directory_searches.remove(&handle).is_some() {
                0.0
            } else {
                -1.0
            },
        )])
    }

    fn allocate_directory_handle(&mut self) -> Result<i32> {
        let start = self.next_directory_handle;
        loop {
            let handle = self.next_directory_handle;
            self.next_directory_handle = self.next_directory_handle.checked_add(1).unwrap_or(1);
            if !self.directory_searches.contains_key(&handle) {
                return Ok(handle);
            }
            if self.next_directory_handle == start {
                return Err(crate::Error::ResourceLimit(
                    "no directory search handles available".into(),
                ));
            }
        }
    }
}

fn decode_bmp(raw: &[u8], target_width: usize, target_height: usize) -> Result<Vec<u16>> {
    if raw.len() < 54 {
        return Err(crate::Error::Package("BMP header is truncated".into()));
    }
    let u16_at = |offset: usize| {
        raw.get(offset..offset + 2)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u16::from_le_bytes)
    };
    let u32_at = |offset: usize| {
        raw.get(offset..offset + 4)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_le_bytes)
    };
    let i32_at = |offset: usize| {
        raw.get(offset..offset + 4)
            .and_then(|bytes| bytes.try_into().ok())
            .map(i32::from_le_bytes)
    };
    let pixel_offset = usize::try_from(u32_at(10).unwrap()).unwrap();
    let source_width = i32_at(18).unwrap();
    let signed_height = i32_at(22).unwrap();
    let planes = u16_at(26).unwrap();
    let bits_per_pixel = usize::from(u16_at(28).unwrap());
    let compression = u32_at(30).unwrap();
    if source_width <= 0
        || signed_height == 0
        || signed_height == i32::MIN
        || planes != 1
        || !matches!(bits_per_pixel, 24 | 32)
        || compression != 0
    {
        return Err(crate::Error::Package(
            "unsupported BMP dimensions or pixel format".into(),
        ));
    }
    let source_width = usize::try_from(source_width).unwrap();
    let source_height = usize::try_from(signed_height.abs()).unwrap();
    let row_bits = source_width
        .checked_mul(bits_per_pixel)
        .ok_or_else(|| crate::Error::ResourceLimit("BMP row size overflows".into()))?;
    let row_stride = row_bits
        .checked_add(31)
        .map(|bits| bits / 32 * 4)
        .ok_or_else(|| crate::Error::ResourceLimit("BMP row stride overflows".into()))?;
    let pixel_bytes = row_stride
        .checked_mul(source_height)
        .ok_or_else(|| crate::Error::ResourceLimit("BMP pixel range overflows".into()))?;
    let pixel_end = pixel_offset
        .checked_add(pixel_bytes)
        .ok_or_else(|| crate::Error::ResourceLimit("BMP pixel offset overflows".into()))?;
    if pixel_end > raw.len() {
        return Err(crate::Error::Package("BMP pixel data is truncated".into()));
    }
    let output_len = target_width
        .checked_mul(target_height)
        .ok_or_else(|| crate::Error::ResourceLimit("BMP output dimensions overflow".into()))?;
    let bytes_per_pixel = bits_per_pixel / 8;
    let bottom_up = signed_height > 0;
    let mut pixels = Vec::with_capacity(output_len);
    for target_y in 0..target_height {
        let source_y = target_y * source_height / target_height;
        let stored_y = if bottom_up {
            source_height - 1 - source_y
        } else {
            source_y
        };
        let row = pixel_offset + stored_y * row_stride;
        for target_x in 0..target_width {
            let source_x = target_x * source_width / target_width;
            let pixel = row + source_x * bytes_per_pixel;
            let blue = u16::from(raw[pixel]);
            let green = u16::from(raw[pixel + 1]);
            let red = u16::from(raw[pixel + 2]);
            pixels.push(((red >> 3) << 11) | ((green >> 2) << 5) | (blue >> 3));
        }
    }
    Ok(pixels)
}

fn package_entry_path(internal_name: &[u8], guest_name: &[u8]) -> Option<Vec<u8>> {
    let components = guest_name
        .split(|byte| matches!(byte, b'/' | b'\\'))
        .filter(|component| !component.is_empty() && *component != b".")
        .collect::<Vec<_>>();
    if components.is_empty() || components.iter().any(|component| *component == b"..") {
        return None;
    }
    let package_stem = internal_name.strip_suffix(b".mrp").unwrap_or(internal_name);
    let components = if components.len() > 1 && components[0] == package_stem {
        &components[1..]
    } else {
        &components[..]
    };
    let mut path = Vec::new();
    for component in components {
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(component);
    }
    Some(path)
}

fn is_mrp_file_name(path: &[u8]) -> bool {
    let name = path
        .rsplit(|byte| matches!(byte, b'/' | b'\\'))
        .next()
        .unwrap_or_default();
    name.len() >= 4 && name[name.len() - 4..].eq_ignore_ascii_case(b".mrp")
}

fn native_extension_profile(platform: u8, version: u32) -> NativeExtensionProfile {
    // Versions from 1000 onward on this platform use the fixed MTK native-code window.
    if platform == 1 && version >= 1_000 {
        NativeExtensionProfile::Mtk
    } else {
        NativeExtensionProfile::Baseline
    }
}

/// Decodes a guest path for the host filesystem. Paths that are valid UTF-8
/// keep the historical interpretation; anything else is decoded as GBK, the
/// vendor platform charset, so the GBK names returned by directory searches
/// round-trip back into open/create/stat calls. Chinese paths such as
/// `skyMusic3/lst/本地列表.sky` previously failed the UTF-8 gate and every
/// file operation on them was rejected. Bytes that decode as neither are
/// still refused (None).
pub(crate) fn guest_path_string(bytes: &[u8]) -> Option<String> {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Some(text.to_owned());
    }
    let (decoded, _, had_errors) = GBK.decode(bytes);
    if had_errors {
        return None;
    }
    Some(decoded.into_owned())
}

fn safe_work_path(work_dir: &Path, bytes: &[u8]) -> Option<PathBuf> {
    let text = guest_path_string(bytes)?;
    let mut path: &str = &text;
    // Leading separators address the mythroad work-dir root (MRP apps build
    // paths like "/<app-name>/<entry>"), not the host filesystem root.
    while path.starts_with('/') || path.starts_with('\\') {
        path = &path[1..];
    }
    // An empty (or separator-only) path addressed the DSM work-dir root itself:
    // the legacy kernel resolved it as dsmWorkPath + "" = "mythroad/", and
    // shells list the mythroad root through findStart(""). Keep that contract
    // instead of rejecting the request.
    let mut resolved = work_dir.to_path_buf();
    if path.len() < 2 || path.as_bytes()[1] != b':' {
        let first_component = path
            .split(['/', '\\'])
            .find(|component| !matches!(*component, "" | "."));
        if !first_component.is_some_and(|component| component.eq_ignore_ascii_case("mythroad")) {
            resolved.push("mythroad");
        }
    } else {
        match path.as_bytes()[0].to_ascii_uppercase() {
            b'C' => {}
            drive @ (b'X' | b'Y' | b'Z') => {
                resolved.push("disk");
                resolved.push(char::from(drive.to_ascii_lowercase()).to_string());
            }
            _ => return None,
        }
        path = &path[2..];
        if !path.is_empty() && !path.starts_with('/') && !path.starts_with('\\') {
            return None;
        }
    }
    for component in path
        .split(['/', '\\'])
        .filter(|component| !matches!(*component, "" | "."))
    {
        match component {
            ".." => return None,
            component if component.contains('\0') || component.contains(':') => return None,
            component => resolved.push(component),
        }
    }
    Some(resolved)
}

fn native_file_path(
    work_dir: &Path,
    package_path: &Path,
    package_internal_name: &[u8],
    bytes: &[u8],
) -> Option<PathBuf> {
    if !package_internal_name.is_empty() && bytes == package_internal_name {
        return Some(package_path.to_path_buf());
    }
    let text = guest_path_string(bytes)?;
    let path = Path::new(&text);
    if path.components().count() == 1 && package_path.file_name() == Some(path.as_os_str()) {
        return Some(package_path.to_path_buf());
    }
    safe_work_path(work_dir, bytes)
}

fn is_identity_only_application_reference(package: &[u8]) -> bool {
    !package.is_empty() && !package.iter().any(|byte| matches!(byte, b'/' | b'\\'))
}

#[derive(Debug, PartialEq, Eq)]
enum NativePathComponent {
    Missing,
    Match(OsString),
    Ambiguous,
}

fn select_ascii_case_component(
    requested: &OsStr,
    candidates: impl IntoIterator<Item = OsString>,
) -> NativePathComponent {
    let Some(requested_text) = requested.to_str() else {
        return NativePathComponent::Missing;
    };
    let mut folded_match = None;
    let mut ambiguous = false;
    for candidate in candidates {
        if candidate == requested {
            return NativePathComponent::Match(candidate);
        }
        if candidate
            .to_str()
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(requested_text))
        {
            if folded_match.is_some() {
                ambiguous = true;
            } else {
                folded_match = Some(candidate);
            }
        }
    }
    if ambiguous {
        NativePathComponent::Ambiguous
    } else {
        folded_match
            .map(NativePathComponent::Match)
            .unwrap_or(NativePathComponent::Missing)
    }
}

fn resolve_native_work_path(work_dir: &Path, target: &Path) -> Option<PathBuf> {
    let Ok(relative) = target.strip_prefix(work_dir) else {
        return Some(target.to_path_buf());
    };
    let mut resolved = work_dir.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(requested) = component else {
            return None;
        };
        let exact = resolved.join(requested);
        match fs::symlink_metadata(&exact) {
            Ok(metadata) if metadata.file_type().is_symlink() => return None,
            Ok(_) => {
                resolved = exact;
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
        let candidates = match fs::read_dir(&resolved) {
            Ok(entries) => entries
                .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
                .collect::<Vec<_>>(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                resolved = exact;
                continue;
            }
            Err(_) => return None,
        };
        match select_ascii_case_component(requested, candidates) {
            NativePathComponent::Missing => resolved = exact,
            NativePathComponent::Match(actual) => {
                let matched = resolved.join(actual);
                let metadata = fs::symlink_metadata(&matched).ok()?;
                if metadata.file_type().is_symlink() {
                    return None;
                }
                resolved = matched;
            }
            NativePathComponent::Ambiguous => return None,
        }
    }
    Some(resolved)
}

fn application_stack_transition(
    stack: &[(Vec<u8>, Vec<u8>, PathBuf)],
    current_package: &[u8],
    current_entry: &[u8],
    current_path: &Path,
    target_path: &Path,
    target_entry: &[u8],
) -> ApplicationStackTransition {
    if current_path == target_path && current_entry == target_entry {
        return ApplicationStackTransition::Stay;
    }
    if stack
        .last()
        .is_some_and(|(_, entry, path)| path == target_path && entry == target_entry)
    {
        ApplicationStackTransition::Pop
    } else {
        ApplicationStackTransition::Push((
            current_package.to_vec(),
            current_entry.to_vec(),
            current_path.to_path_buf(),
        ))
    }
}

fn blit(framebuffer: &mut Framebuffer, bitmap: &Bitmap, region: BlitRegion) {
    for row in 0..region.height {
        for column in 0..region.width {
            let pixel =
                bitmap.pixels[(region.source_y + row) * bitmap.width + region.source_x + column];
            if Some(pixel) != region.transparent_color {
                framebuffer.point(
                    region.destination_x + column as i32,
                    region.destination_y + row as i32,
                    pixel,
                );
            }
        }
    }
}

fn color(args: &[Value], offset: usize) -> Result<u16> {
    Ok(Framebuffer::rgb565(
        integer(args.get(offset))?,
        integer(args.get(offset + 1))?,
        integer(args.get(offset + 2))?,
    ))
}

// Decode a null-terminated UCS-2 BE string (the mr VM's wide-text form,
// produced by c2u and consumed by the gui editor entry points). A missing
// terminator simply ends at the buffer edge.
fn ucs2_bytes_to_string(text: &[u8]) -> String {
    let units: Vec<u16> = text
        .chunks_exact(2)
        .take_while(|pair| pair[0] != 0 || pair[1] != 0)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

// Encode text back into the mr VM's UCS-2 BE form including the trailing
// null, matching the reference EditGetText contract (mr_wstrlen + 2 bytes).
fn string_to_ucs2_bytes(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity((text.len() + 1) * 2);
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_be_bytes());
    }
    out.extend_from_slice(&0u16.to_be_bytes());
    out
}

// Vendor BITMAPMAX (mythroad.h): mr_bitmap[] slots 0..=30 exist and
// MRF_BitmapLoad faults with "BitmapLoad:index %d invalid!" past it.
const MR_BITMAP_MAX: i32 = 30;

fn integer(value: Option<&Value>) -> Result<i32> {
    let value = value.ok_or_else(|| crate::Error::MrFault("missing numeric argument".into()))?;
    let number = value
        .number()
        .ok_or_else(|| crate::Error::MrFault(format!("expected number, got {value:?}")))?;
    if !number.is_finite() || number < i32::MIN as f64 || number > i32::MAX as f64 {
        return Err(crate::Error::MrFault(format!(
            "number {number} does not fit i32"
        )));
    }
    Ok(number as i32)
}

fn guest_u32(value: &Value, label: &str) -> Result<u32> {
    let number = value
        .number()
        .ok_or_else(|| crate::Error::MrFault(format!("{label} is not numeric: {value:?}")))?;
    if !number.is_finite() || number < 0.0 || number > f64::from(u32::MAX) {
        return Err(crate::Error::MrFault(format!("invalid {label}: {number}")));
    }
    Ok(number as u32)
}

fn ext_input<'a>(value: Option<&'a Value>) -> Result<ExtHelperInput<'a>> {
    match value {
        Some(Value::Bytes(bytes)) => Ok(ExtHelperInput::Buffer(bytes.as_ref())),
        Some(Value::Table(table)) => {
            let table = table.borrow();
            let len = table.sequence_len();
            if len > 2 {
                return Err(crate::Error::MrFault(format!(
                    "EXT helper argument table has {len} items; the ABI accepts at most 2"
                )));
            }
            let mut arguments = [0; 2];
            for index in 1..=len {
                let value = table.get(&Value::Number(index as f64));
                let number = value.number().ok_or_else(|| {
                    crate::Error::MrFault(format!(
                        "EXT input table item {index} is not numeric: {value:?}"
                    ))
                })?;
                if !number.is_finite() || number < i32::MIN as f64 || number > u32::MAX as f64 {
                    return Err(crate::Error::MrFault(format!(
                        "EXT input table item {index} does not fit 32 bits: {number}"
                    )));
                }
                arguments[index - 1] = number as i64 as u32;
            }
            Ok(ExtHelperInput::Arguments(arguments))
        }
        other => Err(crate::Error::MrFault(format!(
            "_strCom 801 expects bytes or a numeric sequence, got {other:?}"
        ))),
    }
}

fn positive_usize(value: Option<&Value>, label: &str) -> Result<usize> {
    let value = integer(value)?;
    usize::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| crate::Error::MrFault(format!("invalid {label}: {value}")))
}

fn nonnegative_usize(value: Option<&Value>, label: &str) -> Result<usize> {
    let value = integer(value)?;
    usize::try_from(value).map_err(|_| crate::Error::MrFault(format!("invalid {label}: {value}")))
}

fn value_bytes(value: Option<&Value>) -> Result<Arc<[u8]>> {
    let value = value.ok_or_else(|| crate::Error::MrFault("missing string argument".into()))?;
    value
        .bytes()
        .ok_or_else(|| crate::Error::MrFault(format!("expected string, got {value:?}")))
}

fn bytes(value: &[u8]) -> Value {
    Value::Bytes(Arc::from(value))
}

#[cfg(test)]
mod tests;
