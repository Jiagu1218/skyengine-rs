use std::collections::VecDeque;
use std::time::Duration;

use sdl2::{
    audio::{AudioCallback, AudioDevice, AudioSpecDesired},
    event::Event,
    keyboard::Keycode,
    pixels::{Color, PixelFormatEnum},
    rect::Rect,
    render::Canvas,
    video::Window,
};
use skyengine_core::{
    AUDIO_CHANNELS, AUDIO_SAMPLE_RATE, AudioPlayer, DisplayEvent, Error, Framebuffer,
    PlatformDisplay, Result,
};

/// SOFTRIGHT: the platform cancel key, shared with skyengine_api_cancel_edit.
const MR_KEY_SOFTRIGHT: i32 = 18;
const EDIT_BAR_HEIGHT: i32 = 11;
const GLYPH_HEIGHT: i32 = 7;
const GLYPH_STEP: i32 = 6;

/// Classic public-domain 5x7 bitmap font (column bytes, LSB = top row) for
/// the printable ASCII range the edit bar renders.
const FONT_5X7: [[u8; 5]; 96] = [
    [0x00, 0x00, 0x00, 0x00, 0x00], // ' '
    [0x00, 0x00, 0x5F, 0x00, 0x00], // '!'
    [0x00, 0x07, 0x00, 0x07, 0x00], // '"'
    [0x14, 0x7F, 0x14, 0x7F, 0x14], // '#'
    [0x24, 0x2A, 0x7F, 0x2A, 0x12], // '$'
    [0x23, 0x13, 0x08, 0x64, 0x62], // '%'
    [0x36, 0x49, 0x55, 0x22, 0x50], // '&'
    [0x00, 0x05, 0x03, 0x00, 0x00], // '\''
    [0x00, 0x1C, 0x22, 0x41, 0x00], // '('
    [0x00, 0x41, 0x22, 0x1C, 0x00], // ')'
    [0x08, 0x2A, 0x1C, 0x2A, 0x08], // '*'
    [0x08, 0x08, 0x3E, 0x08, 0x08], // '+'
    [0x00, 0x50, 0x30, 0x00, 0x00], // ','
    [0x08, 0x08, 0x08, 0x08, 0x08], // '-'
    [0x00, 0x60, 0x60, 0x00, 0x00], // '.'
    [0x20, 0x10, 0x08, 0x04, 0x02], // '/'
    [0x3E, 0x51, 0x49, 0x45, 0x3E], // '0'
    [0x00, 0x42, 0x7F, 0x40, 0x00], // '1'
    [0x42, 0x61, 0x51, 0x49, 0x46], // '2'
    [0x21, 0x41, 0x45, 0x4B, 0x31], // '3'
    [0x18, 0x14, 0x12, 0x7F, 0x10], // '4'
    [0x27, 0x45, 0x45, 0x45, 0x39], // '5'
    [0x3C, 0x4A, 0x49, 0x49, 0x30], // '6'
    [0x01, 0x71, 0x09, 0x05, 0x03], // '7'
    [0x36, 0x49, 0x49, 0x49, 0x36], // '8'
    [0x06, 0x49, 0x49, 0x29, 0x1E], // '9'
    [0x00, 0x36, 0x36, 0x00, 0x00], // ':'
    [0x00, 0x56, 0x36, 0x00, 0x00], // ';'
    [0x00, 0x08, 0x14, 0x22, 0x41], // '<'
    [0x14, 0x14, 0x14, 0x14, 0x14], // '='
    [0x41, 0x22, 0x14, 0x08, 0x00], // '>'
    [0x02, 0x01, 0x51, 0x09, 0x06], // '?'
    [0x32, 0x49, 0x79, 0x41, 0x3E], // '@'
    [0x7E, 0x11, 0x11, 0x11, 0x7E], // 'A'
    [0x7F, 0x49, 0x49, 0x49, 0x36], // 'B'
    [0x3E, 0x41, 0x41, 0x41, 0x22], // 'C'
    [0x7F, 0x41, 0x41, 0x22, 0x1C], // 'D'
    [0x7F, 0x49, 0x49, 0x49, 0x41], // 'E'
    [0x7F, 0x09, 0x09, 0x01, 0x01], // 'F'
    [0x3E, 0x41, 0x41, 0x51, 0x32], // 'G'
    [0x7F, 0x08, 0x08, 0x08, 0x7F], // 'H'
    [0x00, 0x41, 0x7F, 0x41, 0x00], // 'I'
    [0x20, 0x40, 0x41, 0x3F, 0x01], // 'J'
    [0x7F, 0x08, 0x14, 0x22, 0x41], // 'K'
    [0x7F, 0x40, 0x40, 0x40, 0x40], // 'L'
    [0x7F, 0x02, 0x04, 0x02, 0x7F], // 'M'
    [0x7F, 0x04, 0x08, 0x10, 0x7F], // 'N'
    [0x3E, 0x41, 0x41, 0x41, 0x3E], // 'O'
    [0x7F, 0x09, 0x09, 0x09, 0x06], // 'P'
    [0x3E, 0x41, 0x51, 0x21, 0x5E], // 'Q'
    [0x7F, 0x09, 0x19, 0x29, 0x46], // 'R'
    [0x46, 0x49, 0x49, 0x49, 0x31], // 'S'
    [0x01, 0x01, 0x7F, 0x01, 0x01], // 'T'
    [0x3F, 0x40, 0x40, 0x40, 0x3F], // 'U'
    [0x1F, 0x20, 0x40, 0x20, 0x1F], // 'V'
    [0x7F, 0x20, 0x18, 0x20, 0x7F], // 'W'
    [0x63, 0x14, 0x08, 0x14, 0x63], // 'X'
    [0x03, 0x04, 0x78, 0x04, 0x03], // 'Y'
    [0x61, 0x51, 0x49, 0x45, 0x43], // 'Z'
    [0x00, 0x00, 0x7F, 0x41, 0x41], // '['
    [0x02, 0x04, 0x08, 0x10, 0x20], // '\\'
    [0x41, 0x41, 0x7F, 0x00, 0x00], // ']'
    [0x04, 0x02, 0x01, 0x02, 0x04], // '^'
    [0x40, 0x40, 0x40, 0x40, 0x40], // '_'
    [0x00, 0x01, 0x02, 0x04, 0x00], // '`'
    [0x20, 0x54, 0x54, 0x54, 0x78], // 'a'
    [0x7F, 0x48, 0x44, 0x44, 0x38], // 'b'
    [0x38, 0x44, 0x44, 0x44, 0x20], // 'c'
    [0x38, 0x44, 0x44, 0x48, 0x7F], // 'd'
    [0x38, 0x54, 0x54, 0x54, 0x18], // 'e'
    [0x08, 0x7E, 0x09, 0x01, 0x02], // 'f'
    [0x0C, 0x52, 0x52, 0x52, 0x3E], // 'g'
    [0x7F, 0x08, 0x04, 0x04, 0x78], // 'h'
    [0x00, 0x44, 0x7D, 0x40, 0x00], // 'i'
    [0x20, 0x40, 0x44, 0x3D, 0x00], // 'j'
    [0x7F, 0x10, 0x28, 0x44, 0x00], // 'k'
    [0x41, 0x7F, 0x40, 0x00, 0x00], // 'l'
    [0x7C, 0x04, 0x18, 0x04, 0x78], // 'm'
    [0x7C, 0x08, 0x04, 0x04, 0x78], // 'n'
    [0x38, 0x44, 0x44, 0x44, 0x38], // 'o'
    [0x7C, 0x14, 0x14, 0x14, 0x08], // 'p'
    [0x08, 0x14, 0x14, 0x18, 0x7C], // 'q'
    [0x7C, 0x08, 0x04, 0x04, 0x08], // 'r'
    [0x48, 0x54, 0x54, 0x54, 0x20], // 's'
    [0x04, 0x3F, 0x44, 0x40, 0x20], // 't'
    [0x3C, 0x40, 0x40, 0x20, 0x7C], // 'u'
    [0x1C, 0x20, 0x40, 0x20, 0x1C], // 'v'
    [0x3C, 0x40, 0x30, 0x40, 0x3C], // 'w'
    [0x44, 0x28, 0x10, 0x28, 0x44], // 'x'
    [0x0C, 0x50, 0x50, 0x50, 0x3C], // 'y'
    [0x44, 0x64, 0x54, 0x4C, 0x44], // 'z'
    [0x00, 0x08, 0x36, 0x41, 0x00], // '{'
    [0x00, 0x00, 0x7F, 0x00, 0x00], // '|'
    [0x00, 0x41, 0x36, 0x08, 0x00], // '}'
    [0x08, 0x08, 0x2A, 0x08, 0x08], // '~'
    [0x7F, 0x41, 0x41, 0x41, 0x7F], // non-ASCII placeholder box
];

struct SdlAudio {
    player: AudioPlayer,
}

impl AudioCallback for SdlAudio {
    type Channel = i16;

    fn callback(&mut self, output: &mut [i16]) {
        self.player.render(output);
    }
}

pub struct SdlDisplay {
    _sdl: sdl2::Sdl,
    canvas: Canvas<Window>,
    events: sdl2::EventPump,
    scale: u32,
    audio: AudioPlayer,
    _audio_device: Option<AudioDevice<SdlAudio>>,
    text_input: sdl2::keyboard::TextInputUtil,
    editor_active: bool,
    editor_buffer: String,
    pending_events: VecDeque<DisplayEvent>,
    last_frame: Option<Framebuffer>,
    /// F9/F10 取证：待 dump 标记与序号（transform 变体实验）。
    pending_dump: bool,
    dump_seq: u32,
}

impl SdlDisplay {
    pub fn new(width: u16, height: u16, scale: u32) -> Result<Self> {
        Self::with_audio_player(width, height, scale, AudioPlayer::default())
    }

    pub fn with_audio_player(
        width: u16,
        height: u16,
        scale: u32,
        audio: AudioPlayer,
    ) -> Result<Self> {
        let sdl = sdl2::init().map_err(Error::Platform)?;
        let video = sdl.video().map_err(Error::Platform)?;
        let window = video
            .window(
                "SkyEngine",
                u32::from(width).saturating_mul(scale),
                u32::from(height).saturating_mul(scale),
            )
            .position_centered()
            .resizable()
            .build()
            .map_err(|error| Error::Platform(error.to_string()))?;
        let mut canvas = window
            .into_canvas()
            .accelerated()
            .present_vsync()
            .build()
            .or_else(|_| {
                video
                    .window(
                        "SkyEngine",
                        u32::from(width).saturating_mul(scale),
                        u32::from(height).saturating_mul(scale),
                    )
                    .position_centered()
                    .build()
                    .map_err(|error| error.to_string())?
                    .into_canvas()
                    .software()
                    .build()
                    .map_err(|error| error.to_string())
            })
            .map_err(|error| Error::Platform(error.to_string()))?;
        canvas
            .set_logical_size(u32::from(width), u32::from(height))
            .map_err(|error| Error::Platform(error.to_string()))?;
        let events = sdl.event_pump().map_err(Error::Platform)?;
        let text_input = video.text_input();
        let audio_device = sdl
            .audio()
            .and_then(|subsystem| {
                let desired = AudioSpecDesired {
                    freq: Some(AUDIO_SAMPLE_RATE as i32),
                    channels: Some(AUDIO_CHANNELS as u8),
                    samples: Some(1024),
                };
                subsystem
                    .open_playback(None, &desired, |_| SdlAudio {
                        player: audio.clone(),
                    })
                    .and_then(|device| {
                        let spec = device.spec();
                        if spec.freq != AUDIO_SAMPLE_RATE as i32
                            || usize::from(spec.channels) != AUDIO_CHANNELS
                        {
                            return Err(format!(
                                "unsupported output format {} Hz / {} channels",
                                spec.freq, spec.channels
                            ));
                        }
                        device.resume();
                        Ok(device)
                    })
            })
            .map_err(|error| eprintln!("[skyengine-sdl] audio disabled: {error}"))
            .ok();
        Ok(Self {
            _sdl: sdl,
            canvas,
            events,
            scale,
            audio,
            _audio_device: audio_device,
            text_input,
            editor_active: false,
            editor_buffer: String::new(),
            pending_events: VecDeque::new(),
            last_frame: None,
            pending_dump: false,
            dump_seq: 0,
        })
    }

    pub fn audio_player(&self) -> Option<AudioPlayer> {
        self._audio_device.as_ref().map(|_| self.audio.clone())
    }

    /// Uploads the guest framebuffer into the canvas without presenting it.
    fn blit(&mut self, framebuffer: &Framebuffer) -> Result<()> {
        let creator = self.canvas.texture_creator();
        let mut texture = creator
            .create_texture_streaming(
                PixelFormatEnum::RGB565,
                u32::from(framebuffer.width()),
                u32::from(framebuffer.height()),
            )
            .map_err(|error| Error::Platform(error.to_string()))?;
        texture
            .with_lock(None, |output, pitch| {
                for (row, source) in framebuffer
                    .pixels()
                    .chunks_exact(usize::from(framebuffer.width()))
                    .enumerate()
                {
                    let destination = &mut output[row * pitch..row * pitch + source.len() * 2];
                    for (pixel, output) in source
                        .iter()
                        .zip(destination.as_chunks_mut::<2>().0.iter_mut())
                    {
                        output.copy_from_slice(&pixel.to_ne_bytes());
                    }
                }
            })
            .map_err(Error::Platform)?;
        self.canvas.clear();
        self.canvas
            .copy(&texture, None, None)
            .map_err(Error::Platform)
    }

    /// Translates an SDL event while the modal edit bar is active. Text goes
    /// into the bar, Return submits, Escape/F2/E cancel via the SOFTRIGHT
    /// press/release pair, and every other key is swallowed because the guest
    /// cannot see keys while its editor is open. Pointer events still pass
    /// through untouched.
    fn handle_editor_event(&mut self, event: Event) -> Option<DisplayEvent> {
        match event {
            Event::Quit { .. } => Some(DisplayEvent::Quit),
            Event::TextInput { text, .. } => {
                if !text.is_empty() {
                    self.editor_buffer.push_str(&text);
                    self.draw_edit_bar();
                }
                None
            }
            Event::KeyDown {
                keycode: Some(key),
                repeat: false,
                ..
            } => match key {
                Keycode::Return | Keycode::KpEnter => {
                    let text = std::mem::take(&mut self.editor_buffer);
                    Some(DisplayEvent::TextInput { text })
                }
                Keycode::Escape | Keycode::F2 | Keycode::E => {
                    self.pending_events.push_back(DisplayEvent::Key {
                        code: MR_KEY_SOFTRIGHT,
                        pressed: false,
                    });
                    Some(DisplayEvent::Key {
                        code: MR_KEY_SOFTRIGHT,
                        pressed: true,
                    })
                }
                Keycode::Backspace => {
                    self.editor_buffer.pop();
                    self.draw_edit_bar();
                    None
                }
                _ => None,
            },
            Event::KeyUp { .. } | Event::TextEditing { .. } => None,
            other => map_event(other),
        }
    }

    /// Paints the bottom edit bar: black strip, 5x7 white text showing the
    /// tail of the buffer when it overflows, and a solid cursor block. Best
    /// effort: rendering errors are ignored because the bar is decorative.
    fn draw_edit_bar(&mut self) {
        let (width, height) = self.canvas.logical_size();
        let (width, height) = (width as i32, height as i32);
        let bar_y = height - EDIT_BAR_HEIGHT;
        self.canvas.set_draw_color(Color::BLACK);
        if self
            .canvas
            .fill_rect(Rect::new(0, bar_y, width as u32, EDIT_BAR_HEIGHT as u32))
            .is_err()
        {
            return;
        }
        self.canvas.set_draw_color(Color::WHITE);
        // 1px padding on each side; the trailing slot is reserved for the
        // cursor so at least the final glyph stays visible.
        let max_glyphs = (((width - 2) / GLYPH_STEP) as usize).max(1);
        let chars: Vec<char> = self.editor_buffer.chars().collect();
        let visible = chars.len().min(max_glyphs.saturating_sub(1));
        let text_y = bar_y + 2;
        let mut pen_x = 1;
        for &c in &chars[chars.len() - visible..] {
            self.draw_glyph(pen_x, text_y, glyph_index(c));
            pen_x += GLYPH_STEP;
        }
        let _ = self
            .canvas
            .fill_rect(Rect::new(pen_x, text_y, 1, GLYPH_HEIGHT as u32));
        self.canvas.present();
    }

    fn draw_glyph(&mut self, x: i32, y: i32, index: usize) {
        for (column, bits) in FONT_5X7[index].iter().enumerate() {
            for row in 0..GLYPH_HEIGHT {
                if bits & (1 << row) != 0 {
                    let _ = self
                        .canvas
                        .fill_rect(Rect::new(x + column as i32, y + row, 1, 1));
                }
            }
        }
    }

    /// 把最近一帧写成 P6 PPM（F9/F10 取证：文件名含 transform 变体名
    /// 与递增序号，落在进程工作目录）。写失败仅打印，不影响运行。
    fn dump_frame_ppm(&mut self) {
        let Some(frame) = self.last_frame.as_ref() else {
            return;
        };
        self.dump_seq += 1;
        let variant = skyengine_core::debug_switch::transform_variant();
        let name = skyengine_core::debug_switch::TRANSFORM_VARIANT_NAMES[variant as usize];
        let path = format!("tvar-{name}-{:03}.ppm", self.dump_seq);
        let width = usize::from(frame.width());
        let height = usize::from(frame.height());
        let mut body = Vec::with_capacity(width * height * 3);
        for pixel in frame.pixels() {
            let r = ((pixel >> 11) & 0x1f) as u8 * 255 / 31;
            let g = ((pixel >> 5) & 0x3f) as u8 * 255 / 63;
            let b = (pixel & 0x1f) as u8 * 255 / 31;
            body.extend_from_slice(&[r, g, b]);
        }
        let header = format!("P6\n{width} {height}\n255\n");
        let mut file = header.into_bytes();
        file.extend_from_slice(&body);
        match std::fs::write(&path, file) {
            Ok(()) => eprintln!("[tvar] 已导出 {path}"),
            Err(error) => eprintln!("[tvar] 导出 {path} 失败: {error}"),
        }
    }
}

fn glyph_index(c: char) -> usize {
    match c {
        ' '..='~' => c as usize - 0x20,
        _ => 0x7F - 0x20,
    }
}

impl PlatformDisplay for SdlDisplay {
    fn resize(&mut self, width: u16, height: u16) -> Result<()> {
        self.canvas
            .set_logical_size(u32::from(width), u32::from(height))
            .map_err(|error| Error::Platform(error.to_string()))?;
        self.canvas
            .window_mut()
            .set_size(
                u32::from(width).saturating_mul(self.scale),
                u32::from(height).saturating_mul(self.scale),
            )
            .map_err(|error| Error::Platform(error.to_string()))
    }

    fn update_editor(&mut self, text: Option<&str>) {
        match text {
            Some(initial) if !self.editor_active => {
                self.editor_active = true;
                self.editor_buffer.clear();
                // Debug aid: prefill the edit bar from the environment so long
                // URLs can be injected without typing them (unset the variable
                // for the normal guest-provided initial text).
                match std::env::var("SKYENGINE_EDIT_PREFILL") {
                    Ok(prefill) if !prefill.is_empty() => {
                        self.editor_buffer.push_str(&prefill);
                    }
                    _ => self.editor_buffer.push_str(initial),
                }
                self.text_input.start();
                // The guest may stop drawing while its modal editor is up, so
                // repaint the last frame plus the bar immediately instead of
                // waiting for the next present().
                if let Some(frame) = self.last_frame.clone() {
                    let _ = self.blit(&frame);
                } else {
                    let _ = self.canvas.clear();
                }
                self.draw_edit_bar();
            }
            None if self.editor_active => {
                self.editor_active = false;
                self.editor_buffer.clear();
                self.text_input.stop();
            }
            _ => {}
        }
    }

    fn present(&mut self, framebuffer: &Framebuffer) -> Result<()> {
        self.blit(framebuffer)?;
        self.last_frame = Some(framebuffer.clone());
        if self.editor_active {
            self.draw_edit_bar();
        } else {
            self.canvas.present();
        }
        if self.pending_dump {
            self.pending_dump = false;
            self.dump_frame_ppm();
        }
        Ok(())
    }

    fn poll_event(&mut self) -> Result<Option<DisplayEvent>> {
        if let Some(event) = self.pending_events.pop_front() {
            return Ok(Some(event));
        }
        while let Some(event) = self.events.poll_event() {
            // F9/F10 取证钩子（编辑栏未激活时）：F9 循环切换 drawRegion
            // transform 变体并在下一帧 dump PPM；F10 仅 dump。事件不进
            // guest（key_code 本就不映射这两个键）。
            if !self.editor_active {
                if let Event::KeyDown {
                    keycode: Some(key),
                    repeat: false,
                    ..
                } = event
                {
                    match key {
                        Keycode::F9 => {
                            let variant = skyengine_core::debug_switch::cycle_transform_variant();
                            let name =
                                skyengine_core::debug_switch::TRANSFORM_VARIANT_NAMES[variant as usize];
                            eprintln!("[tvar] F9 -> variant {variant} ({name})，下一帧将导出 PPM");
                            self.pending_dump = true;
                            continue;
                        }
                        Keycode::F10 => {
                            eprintln!("[tvar] F10 手动导出当前帧");
                            self.pending_dump = true;
                            continue;
                        }
                        Keycode::F8 => {
                            eprintln!("[kvm-profile] F8 请求导出调用剖析（需 PROFILE=1）");
                            skyengine_core::debug_switch::request_profile_dump();
                            continue;
                        }
                        _ => {}
                    }
                }
            }
            let mapped = if self.editor_active {
                self.handle_editor_event(event)
            } else {
                map_event(event)
            };
            if mapped.is_some() {
                return Ok(mapped);
            }
        }
        Ok(None)
    }

    fn wait_timeout(&mut self, milliseconds: u32) {
        std::thread::sleep(Duration::from_millis(u64::from(milliseconds)));
    }
}

/// Maps an SDL event to a guest display event outside edit mode.
fn map_event(event: Event) -> Option<DisplayEvent> {
    match event {
        Event::Quit { .. } => Some(DisplayEvent::Quit),
        Event::KeyDown {
            keycode: Some(key),
            repeat: false,
            ..
        } => key_code(key).map(|code| DisplayEvent::Key {
            code,
            pressed: true,
        }),
        Event::KeyUp {
            keycode: Some(key),
            repeat: false,
            ..
        } => key_code(key).map(|code| DisplayEvent::Key {
            code,
            pressed: false,
        }),
        Event::MouseButtonDown { x, y, .. } => {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_millis())
                .unwrap_or(0);
            eprintln!("[skyengine-sdl] [{timestamp}] click x={x} y={y}");
            Some(DisplayEvent::Pointer {
                x,
                y,
                pressed: true,
            })
        }
        Event::MouseButtonUp { x, y, .. } => Some(DisplayEvent::Pointer {
            x,
            y,
            pressed: false,
        }),
        Event::MouseMotion {
            x, y, mousestate, ..
        } if mousestate.left() => Some(DisplayEvent::PointerMove { x, y }),
        _ => None,
    }
}

fn key_code(key: Keycode) -> Option<i32> {
    Some(match key {
        Keycode::Num0 | Keycode::Kp0 => 0,
        Keycode::Num1 | Keycode::Kp1 => 1,
        Keycode::Num2 | Keycode::Kp2 => 2,
        Keycode::Num3 | Keycode::Kp3 => 3,
        Keycode::Num4 | Keycode::Kp4 => 4,
        Keycode::Num5 | Keycode::Kp5 => 5,
        Keycode::Num6 | Keycode::Kp6 => 6,
        Keycode::Num7 | Keycode::Kp7 => 7,
        Keycode::Num8 | Keycode::Kp8 => 8,
        Keycode::Num9 | Keycode::Kp9 => 9,
        Keycode::Asterisk => 10,
        Keycode::Hash => 11,
        Keycode::Up | Keycode::W => 12,
        Keycode::Down | Keycode::S => 13,
        Keycode::Left | Keycode::A => 14,
        Keycode::Right | Keycode::D => 15,
        Keycode::Escape => 16,
        Keycode::F1 | Keycode::Q => 17,
        Keycode::F2 | Keycode::Backspace | Keycode::E => 18,
        Keycode::Return | Keycode::KpEnter | Keycode::Space => 20,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letter_keys_map_to_softkeys_and_directions() {
        assert_eq!(key_code(Keycode::Q), Some(17));
        assert_eq!(key_code(Keycode::E), Some(18));
        assert_eq!(key_code(Keycode::W), Some(12));
        assert_eq!(key_code(Keycode::A), Some(14));
        assert_eq!(key_code(Keycode::S), Some(13));
        assert_eq!(key_code(Keycode::D), Some(15));
    }

    #[test]
    fn glyph_index_covers_ascii_and_folds_the_rest() {
        assert_eq!(glyph_index(' '), 0);
        assert_eq!(glyph_index('a'), 'a' as usize - 0x20);
        assert_eq!(glyph_index('~'), 94);
        assert_eq!(glyph_index('网'), 95);
        assert_eq!(glyph_index('\u{0}'), 95);
    }

    /// SDL initialization is thread-affine, so every test that constructs an
    /// SdlDisplay runs inside this single test on one thread.
    #[test]
    fn dummy_driver_editor_flow() {
        if std::env::var("SDL_VIDEODRIVER").as_deref() != Ok("dummy") {
            return;
        }

        // present() still works with the editor inactive.
        let mut display = SdlDisplay::new(32, 32, 1).unwrap();
        let mut framebuffer = Framebuffer::new(4, 3).unwrap();
        framebuffer.rect(0, 0, 4, 3, Framebuffer::rgb565(24, 160, 200));
        display.present(&framebuffer).unwrap();

        assert!(!display.editor_active);

        // Activating seeds the buffer with the guest's initial text.
        PlatformDisplay::update_editor(&mut display, Some("http://"));
        assert!(display.editor_active);
        assert_eq!(display.editor_buffer, "http://");

        // Repeated updates while active must not clobber user input.
        display.editor_buffer.push('u');
        PlatformDisplay::update_editor(&mut display, Some("http://"));
        assert_eq!(display.editor_buffer, "http://u");

        // Deactivating clears the buffer.
        PlatformDisplay::update_editor(&mut display, None);
        assert!(!display.editor_active);
        assert!(display.editor_buffer.is_empty());

        // Deactivating again is a no-op, and re-activation reseeds the buffer.
        PlatformDisplay::update_editor(&mut display, None);
        assert!(!display.editor_active);
        PlatformDisplay::update_editor(&mut display, Some("ab"));
        assert_eq!(display.editor_buffer, "ab");

        // Typed text lands in the buffer and produces no guest event.
        assert_eq!(
            display.handle_editor_event(Event::TextInput {
                timestamp: 0,
                window_id: 0,
                text: "c".to_string()
            }),
            None
        );
        assert_eq!(display.editor_buffer, "abc");

        // Backspace trims the buffer.
        assert_eq!(
            display.handle_editor_event(key_down(Keycode::Backspace)),
            None
        );
        assert_eq!(display.editor_buffer, "ab");

        // Return submits the whole buffer as one TextInput event.
        assert_eq!(
            display.handle_editor_event(key_down(Keycode::Return)),
            Some(DisplayEvent::TextInput {
                text: "ab".to_string()
            })
        );
        assert!(display.editor_buffer.is_empty());

        // Escape queues the SOFTRIGHT release behind the press.
        assert_eq!(
            display.handle_editor_event(key_down(Keycode::Escape)),
            Some(DisplayEvent::Key {
                code: MR_KEY_SOFTRIGHT,
                pressed: true
            })
        );

        // Other keys are swallowed while the editor is open, and pointer
        // events still pass through.
        assert_eq!(display.handle_editor_event(key_down(Keycode::Up)), None);
        assert_eq!(
            display.handle_editor_event(Event::MouseButtonDown {
                timestamp: 0,
                window_id: 0,
                which: 0,
                mouse_btn: sdl2::mouse::MouseButton::Left,
                clicks: 1,
                x: 3,
                y: 4
            }),
            Some(DisplayEvent::Pointer {
                x: 3,
                y: 4,
                pressed: true
            })
        );

        // The queued release comes out of poll_event first, before the press
        // delivered above is followed up inside the guest.
        assert_eq!(
            display.poll_event().unwrap(),
            Some(DisplayEvent::Key {
                code: MR_KEY_SOFTRIGHT,
                pressed: false
            })
        );
        assert_eq!(display.poll_event().unwrap(), None);
    }

    fn key_down(key: Keycode) -> Event {
        Event::KeyDown {
            timestamp: 0,
            window_id: 0,
            keycode: Some(key),
            scancode: None,
            keymod: sdl2::keyboard::Mod::empty(),
            repeat: false,
        }
    }
}
