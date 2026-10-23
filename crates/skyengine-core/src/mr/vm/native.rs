use super::stdlib::*;
use super::*;
use std::cell::RefCell;

impl MrVm {
    pub(super) fn native(&self, name: &'static str) {
        self.globals
            .borrow_mut()
            .set(bytes(name.as_bytes()), Value::Native(name));
    }

    pub(super) fn register_libraries(&mut self) {
        for name in [
            "type",
            "tostring",
            "tonumber",
            "print",
            "next",
            "pairs",
            "ipairs",
            "assert",
            "error",
            "pcall",
            "_pCall",
            "_loads",
            "loadfile",
            "_loadFile",
            "dofile",
            "collectgarbage",
            "_gc",
            "GetSysInfo",
            "GetDatetime",
            "c2u",
            "u2c",
            "_platEx",
            "_plat",
            "BitmapLoad",
            "BitmapShow",
            "SpriteSet",
            "SpriteDraw",
            "DrawRect",
            "_drawRect",
            "_effSetCon",
            "DrawLine",
            "_drawLine",
            "DrawText",
            "DispUpEx",
            "TestCom",
            "TestCom1",
            "_com",
            "_closeNet",
            "_strCom",
            "LoadTable",
            "SaveTable",
            "LoadPack",
            "RunFile",
            "UAReset",
            "Exit",
            "TimerStart",
            "TimerStop",
            "_num",
            "_str",
            "_t",
            "mod",
            "_mod",
            "_and",
            "_or",
            "_xor",
            "_not",
            "clen",
            "cstr",
            "_textWidth",
            "Call",
            "SendSms",
            "GetNetworkID",
            "ConnectWAP",
            "SoundSet",
            "SoundPlay",
            "SoundStop",
            "BgMusicSet",
            "BgMusicStart",
            "BgMusicStop",
        ] {
            self.native(name);
        }

        // Genuine-platform handshake constants. Commercial Mythroad builds
        // seeded obfuscated globals into the script VM so packaged games
        // could verify they run inside the genuine engine and refuse
        // otherwise ("cann`t be run direct!"). qq2008's @start.mr gates its
        // mrc_loader.ext/cfunction.ext load chain on this exact marker.
        self.globals
            .borrow_mut()
            .set(bytes(b"_nes3ShP7SwK0"), Value::Number(370.0));

        // Launch parameter exposed to the entry script. The reference VM
        // (mythroad.c:3888) always pushes start_fileparameter (an empty
        // string for a plain app-list launch, which is *truthy* in Lua)
        // before dofile. hxsg's start.mr forwards it to cfunction.ext as
        // event 5001 (string.pack('iii', 5001, subV(_mr_param)...)); without
        // this global the ext never receives the launch event and the
        // "already installed" boot path stalls on the splash screen.
        self.globals
            .borrow_mut()
            .set(bytes(b"_mr_param"), Value::Bytes(Arc::from(&b""[..])));

        let string = Table::new();
        for name in [
            "byte", "char", "len", "clen", "cstr", "sub", "subV", "find", "format", "rep", "lower",
            "upper", "pack", "unpack", "new", "update", "c2u", "u2c",
        ] {
            string
                .borrow_mut()
                .set(bytes(name.as_bytes()), Value::Native(name));
        }
        self.globals
            .borrow_mut()
            .set(bytes(b"string"), Value::Table(string));

        let table = Table::new();
        for name in ["insert", "remove", "getn", "concat"] {
            table
                .borrow_mut()
                .set(bytes(name.as_bytes()), Value::Native(name));
        }
        self.globals
            .borrow_mut()
            .set(bytes(b"table"), Value::Table(table));

        let file = Table::new();
        for (name, native) in [
            ("exist", "exist"),
            ("open", "file_open"),
            ("close", "file_close"),
            ("remove", "file_remove"),
            ("rename", "rename"),
            ("getlen", "getlen"),
        ] {
            file.borrow_mut()
                .set(bytes(name.as_bytes()), Value::Native(native));
        }
        self.globals
            .borrow_mut()
            .set(bytes(b"file"), Value::Table(file));

        let sys = Table::new();
        for (name, native) in [
            ("rm", "file_remove"),
            ("getInfo", "sys_get_info"),
            ("findstart", "sys_find_start"),
            ("findnext", "sys_find_next"),
            ("findstop", "sys_find_stop"),
            ("findStart", "sys_find_start"),
            ("findNext", "sys_find_next"),
            ("findStop", "sys_find_stop"),
            // Vendor syslib (mr_iolib_target.c) exposes each entry with a
            // leading-uppercase and an all-lowercase alias; QQDSM2007
            // builds its data directory through sys.mkDir during boot.
            ("mkDir", "sys_mk_dir"),
            ("mkdir", "sys_mk_dir"),
            ("rmDir", "sys_rm_dir"),
            ("rmdir", "sys_rm_dir"),
            ("getFileInfo", "sys_get_file_info"),
            ("getfileinfo", "sys_get_file_info"),
            ("getFileLen", "sys_get_file_len"),
            ("getfilelen", "sys_get_file_len"),
            ("getUptime", "sys_get_uptime"),
            ("getuptime", "sys_get_uptime"),
            ("datetime", "sys_datetime"),
        ] {
            sys.borrow_mut()
                .set(bytes(name.as_bytes()), Value::Native(native));
        }
        self.globals
            .borrow_mut()
            .set(bytes(b"sys"), Value::Table(sys));
        // The vendor engine opens the `phone` library (mythroad.c:
        // mr_L_openlib(MRP_PHONELIBNAME, phonelib)) with the four
        // dial/SMS entry points and also mirrors them as plain globals
        // ("Call"/"SendSms"/"GetNetworkID"/"ConnectWAP"). DSM apps such
        // as QQDSM2007 index `phone.getNetID` during boot, so the table
        // itself must exist even on hosts without a modem.
        let phone = Table::new();
        for (name, native) in [
            ("call", "phone_call"),
            ("sendSms", "phone_send_sms"),
            ("getNetID", "phone_get_net_id"),
            ("wap", "phone_wap"),
        ] {
            phone
                .borrow_mut()
                .set(bytes(name.as_bytes()), Value::Native(native));
        }
        self.globals
            .borrow_mut()
            .set(bytes(b"phone"), Value::Table(phone));
        self.globals
            .borrow_mut()
            .set(bytes(b"socket"), Value::Table(self.host.socket_library()));
        // Vendor gui library (mythroad.c: mr_L_openlib(MR_GUILIBNAME,
        // guilib) with the table in mr_iolib_target.c). On the reference
        // desktop build the menu/dialog/text/window widgets are host-side
        // stubs (native_dsm_funcs.c answers MR_FAILED, which the Lua
        // wrappers surface as nil); only the editor family reaches the
        // platform editor UI. QQDSM2007 opens its QQ-number input through
        // gui.e_create, so the whole vendor table shape must exist.
        let gui = Table::new();
        for (name, native) in [
            ("m_create", "gui_menu_create"),
            ("m_setItem", "gui_menu_add_item"),
            ("m_setitem", "gui_menu_add_item"),
            ("m_show", "gui_menu_show"),
            ("m_release", "gui_menu_release"),
            ("m_focus", "gui_menu_set_focus"),
            ("m_update", "gui_menu_refresh"),
            ("d_create", "gui_dialog_create"),
            ("d_release", "gui_dialog_release"),
            ("d_update", "gui_dialog_refresh"),
            ("t_create", "gui_text_create"),
            ("t_release", "gui_text_release"),
            ("t_update", "gui_text_refresh"),
            ("e_create", "gui_edit_create"),
            ("e_release", "gui_edit_release"),
            ("e_getText", "gui_edit_get_text"),
            ("e_gettext", "gui_edit_get_text"),
            ("w_create", "gui_win_create"),
            ("w_release", "gui_win_release"),
        ] {
            gui.borrow_mut()
                .set(bytes(name.as_bytes()), Value::Native(native));
        }
        self.globals
            .borrow_mut()
            .set(bytes(b"gui"), Value::Table(gui));
        self.globals
            .borrow_mut()
            .set(bytes(b"SCROLL_W"), Value::Number(0.0));
    }

    pub(super) fn call_native(&mut self, name: &'static str, args: &[Value]) -> Result<Vec<Value>> {
        let trace = std::env::var_os("SKYENGINE_TRACE_MR_CALLS").is_some();
        if trace {
            eprintln!("[mr-call] {name}({})", trace_values(args));
        }
        let result = match name {
            "type" => Ok(vec![bytes(args.first().unwrap_or(&Value::Nil).type_name())]),
            "tostring" | "_str" => Ok(vec![native_tostring(args)]),
            "tonumber" | "_num" => Ok(vec![native_tonumber(args)]),
            "_t" => Ok(vec![bytes(match args.first().unwrap_or(&Value::Nil) {
                Value::Nil => b"nil",
                Value::Boolean(_) => b"bool",
                Value::Number(_) => b"num",
                Value::Bytes(_) | Value::Buffer(_) => b"str",
                Value::Table(_) => b"tab",
                Value::Closure(_) | Value::Native(_) => b"fun",
            })]),
            "print" => {
                let message = args
                    .iter()
                    .map(|value| {
                        value
                            .bytes()
                            .map(|bytes| {
                                let len = bytes
                                    .iter()
                                    .position(|byte| *byte == 0)
                                    .unwrap_or(bytes.len());
                                String::from_utf8_lossy(&bytes[..len]).into_owned()
                            })
                            .unwrap_or_else(|| format!("{value:?}"))
                    })
                    .collect::<Vec<_>>()
                    .join("\t");
                eprintln!("[mr] {message}");
                Ok(Vec::new())
            }
            "next" => {
                let table = table(args.first())?;
                let previous = args.get(1).unwrap_or(&Value::Nil);
                Ok(table
                    .borrow()
                    .next(previous)
                    .map(|(key, value)| vec![key, value])
                    .unwrap_or_default())
            }
            "pairs" | "ipairs" => Ok(vec![
                Value::Native("next"),
                args.first().cloned().unwrap_or(Value::Nil),
                Value::Nil,
            ]),
            "assert" => {
                if args.first().is_some_and(Value::truthy) {
                    Ok(args.to_vec())
                } else {
                    Err(crate::Error::MrFault(
                        args.get(1)
                            .and_then(Value::bytes)
                            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                            .unwrap_or_else(|| "assertion failed".into()),
                    ))
                }
            }
            "error" => Err(crate::Error::MrFault(
                args.first()
                    .and_then(Value::bytes)
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .unwrap_or_else(|| "error".into()),
            )),
            "collectgarbage" | "_gc" => Ok(Vec::new()),
            "TestCom1" if matches!(args.first(), Some(Value::Number(command)) if *command == 300.0) =>
            {
                let source = args
                    .get(1)
                    .and_then(Value::bytes)
                    .ok_or_else(|| crate::Error::MrFault("TestCom1(300) expects source".into()))?;
                Ok(vec![Value::Closure(Rc::new(Closure {
                    prototype: text::compile(&source, &self.limits)?,
                    upvalues: Vec::new(),
                }))])
            }
            "_loads" => match args.first() {
                Some(Value::Closure(closure)) => Ok(vec![Value::Closure(closure.clone())]),
                _ => Ok(vec![Value::Nil]),
            },
            "mod" | "_mod" => integer_binary(
                args,
                |left, right| {
                    if right == 0 { 0 } else { left % right }
                },
            ),
            "_and" => integer_binary(args, |left, right| left & right),
            "_or" => integer_binary(args, |left, right| left | right),
            "_xor" => integer_binary(args, |left, right| left ^ right),
            // Vendor phone library (dsm.c): the desktop engine has no
            // modem, so mr_call/mr_connectWAP only log, mr_sendSms
            // answers MR_SUCCESS, and mr_getNetworkID answers
            // MR_NET_ID_MOBILE (0).
            "phone_call" | "Call" => Ok(Vec::new()),
            "phone_wap" | "ConnectWAP" => Ok(Vec::new()),
            "phone_send_sms" | "SendSms" => Ok(vec![Value::Number(0.0)]),
            "phone_get_net_id" | "GetNetworkID" => Ok(vec![Value::Number(0.0)]),
            // Vendor gui library (mr_iolib_target.c): the menu/dialog/
            // text/window widgets delegate to host widgets that the
            // reference desktop build does not implement
            // (native_dsm_funcs.c answers MR_FAILED), so the Lua wrappers
            // surface nil for them.
            "gui_menu_create"
            | "gui_menu_add_item"
            | "gui_menu_show"
            | "gui_menu_release"
            | "gui_menu_set_focus"
            | "gui_menu_refresh"
            | "gui_dialog_create"
            | "gui_dialog_release"
            | "gui_dialog_refresh"
            | "gui_text_create"
            | "gui_text_release"
            | "gui_text_refresh"
            | "gui_win_create"
            | "gui_win_release" => Ok(vec![Value::Nil]),
            // gui.e_create(title, text, type, max_size): activate the host
            // editor prefilled with `text` (UCS-2 BE, usually a c2u
            // result) and answer the fixed editor handle like the
            // reference api. Vendor EditCreate defaults max_size to 68.
            "gui_edit_create" => {
                let text = value_bytes(args.get(1))?;
                let max_size = match args.get(3) {
                    Some(Value::Number(value)) => *value as i32,
                    _ => MR_LUA_EDIT_MAX_SIZE,
                };
                let handle = self.host.mr_gui_edit_create(&text, max_size);
                Ok(vec![Value::Number(f64::from(handle))])
            }
            // gui.e_release: vendor EditRelease answers MR_SUCCESS.
            "gui_edit_release" => {
                self.host.mr_gui_edit_release();
                Ok(vec![Value::Number(0.0)])
            }
            // gui.e_getText: the held text as UCS-2 BE with the trailing
            // null (reference EditGetText answers mr_wstrlen + 2 bytes),
            // or nil once released.
            "gui_edit_get_text" => Ok(match self.host.mr_gui_edit_text() {
                Some(text) => vec![bytes(&text)],
                None => vec![Value::Nil],
            }),
            // Vendor sound library (mythroad.c): SOUNDMAX=5 slots, with
            // slot 0 doubling as the BgMusic channel. SoundSet loads the
            // bytes through _mr_readFile (work-dir file, then the pack);
            // a "*" name only clears the slot. SoundPlay/SoundStop drive
            // mr_playSound/mr_stopSound on the platform audio backend and
            // return no values.
            "SoundSet" => {
                let index = sound_index(args.first());
                let filename = value_bytes(args.get(1))?;
                let sound_type = sound_type(args.get(2), crate::SoundType::Wav);
                self.host.mr_sound_set(index, &filename, sound_type)?;
                Ok(Vec::new())
            }
            "BgMusicSet" => {
                let filename = value_bytes(args.first())?;
                let sound_type = sound_type(args.get(1), crate::SoundType::Wav);
                self.host.mr_sound_set(0, &filename, sound_type)?;
                Ok(Vec::new())
            }
            "SoundPlay" => {
                let index = sound_index(args.first());
                let looped = args.get(1).is_some_and(Value::truthy);
                self.host.mr_sound_play(index, looped)?;
                Ok(Vec::new())
            }
            "BgMusicStart" => {
                let looped = args.first().map_or(true, Value::truthy);
                self.host.mr_sound_play(0, looped)?;
                Ok(Vec::new())
            }
            "SoundStop" => {
                let index = sound_index(args.first());
                self.host.mr_sound_stop(index)?;
                Ok(Vec::new())
            }
            "BgMusicStop" => {
                self.host.mr_sound_stop(0)?;
                Ok(Vec::new())
            }
            // Vendor sys library file helpers (mr_iolib_target.c): mkDir/
            // rmDir answer (true) or (nil, "file err: ...", code) through
            // pushresult, while getFileInfo/getFileLen answer the raw
            // mr_info/mr_getLen codes (MR_IS_* flags / -1 when missing).
            "sys_mk_dir" | "sys_rm_dir" => {
                let filename = value_bytes(args.first())?;
                let code = if name == "sys_mk_dir" {
                    self.host.mr_mk_dir(&filename)?
                } else {
                    self.host.mr_rm_dir(&filename)?
                };
                if code == 0 {
                    Ok(vec![Value::Boolean(true)])
                } else {
                    Ok(vec![
                        Value::Nil,
                        bytes(
                            format!(
                                "file err: {}: {}",
                                String::from_utf8_lossy(&filename),
                                code
                            )
                            .as_bytes(),
                        ),
                        Value::Number(code as f64),
                    ])
                }
            }
            "sys_get_file_info" => {
                let name = value_bytes(args.first())?;
                Ok(vec![Value::Number(f64::from(self.host.mr_file_info(&name)?))])
            }
            "sys_get_file_len" => {
                let name = value_bytes(args.first())?;
                Ok(vec![Value::Number(self.host.mr_file_len(&name)? as f64)])
            }
            "sys_get_uptime" => Ok(vec![Value::Number(f64::from(self.host.mr_uptime_ms()))]),
            "sys_datetime" | "GetDatetime" => {
                let datetime = self.host.mr_datetime();
                let table = Table::new();
                {
                    let mut values = table.borrow_mut();
                    values.set(bytes(b"year"), Value::Number(f64::from(datetime.year)));
                    values.set(bytes(b"mon"), Value::Number(f64::from(datetime.month)));
                    values.set(bytes(b"day"), Value::Number(f64::from(datetime.day)));
                    values.set(bytes(b"hour"), Value::Number(f64::from(datetime.hour)));
                    values.set(bytes(b"min"), Value::Number(f64::from(datetime.minute)));
                    values.set(bytes(b"sec"), Value::Number(f64::from(datetime.second)));
                }
                Ok(vec![Value::Table(table)])
            }
            "_not" => Ok(vec![Value::Number(
                (!integer_number(args.first().unwrap_or(&Value::Nil))?) as f64,
            )]),
            "byte" => string_byte(args),
            "char" => string_char(args),
            "len" => Ok(vec![Value::Number(value_bytes(args.first())?.len() as f64)]),
            "clen" => string_clen(args),
            "cstr" => string_cstr(args),
            "sub" => string_sub(args),
            "subV" => string_sub_value(args),
            "find" => string_find(args),
            "format" => string_format(args),
            "rep" => string_rep(args),
            "lower" => string_case(args, false),
            "upper" => string_case(args, true),
            "pack" => string_pack(args),
            "unpack" => string_unpack(args),
            "new" => {
                let len = usize::try_from(integer_number(args.first().unwrap_or(&Value::Nil))?)
                    .map_err(|_| crate::Error::MrFault("string.new length is negative".into()))?;
                if len > self.limits.max_mr_string_len {
                    return Err(crate::Error::ResourceLimit(format!(
                        "string.new length {len} exceeds {}",
                        self.limits.max_mr_string_len
                    )));
                }
                Ok(vec![Value::Buffer(Rc::new(RefCell::new(vec![0; len])))])
            }
            "update" => string_update(args),
            "insert" => table_insert(args),
            "remove" => table_remove(args),
            "getn" => Ok(vec![Value::Number(
                table(args.first())?.borrow().sequence_len() as f64,
            )]),
            "concat" => table_concat(args),
            "exist" => self.file_exist(args),
            "file_open" => self.file_open(args),
            "file_read" => self.file_read(args),
            "file_seek" => self.file_seek(args),
            "file_write" => self.file_write(args),
            "file_close" => self.file_close(args),
            "file_remove" => self.file_remove(args),
            "LoadPack" => {
                let name = args.first().and_then(Value::bytes);
                if self.host.load_pack(name.as_deref())? && name.is_some() {
                    self.set_global(b"loadfile", Value::Native("load_pack_file"))?;
                    Ok(vec![Value::Native("loaded_pack")])
                } else {
                    self.set_global(b"loadfile", Value::Nil)?;
                    Ok(vec![Value::Nil])
                }
            }
            "load_pack_file" => {
                let name = value_bytes(args.first())?;
                let source = self.host.read_loaded_pack(&name)?;
                let prototype = if source.starts_with(SIGNATURE) {
                    MrChunk::load(&source, &self.limits)?.root
                } else {
                    text::compile(&source, &self.limits)?
                };
                Ok(vec![Value::Closure(Rc::new(Closure {
                    prototype,
                    upvalues: Vec::new(),
                }))])
            }
            "rename" | "getlen" => Ok(vec![Value::Number(-1.0)]),
            _ => self.host.call(name, args),
        };
        if result.is_ok()
            && name == "_strCom"
            && matches!(args.first(), Some(Value::Number(command)) if *command == 800.0)
        {
            self.set_global(b"_mr_c_load", Value::Native("mr_c_load"))?;
        }
        if trace {
            match &result {
                Ok(values) => eprintln!("[mr-return] {name} -> {}", trace_values(values)),
                Err(error) => eprintln!("[mr-return] {name} -> error: {error}"),
            }
        }
        result
    }

    pub(super) fn file_exist(&self, args: &[Value]) -> Result<Vec<Value>> {
        let name = value_bytes(args.first())?;
        let in_package = self
            .host
            .package
            .entries()
            .iter()
            .any(|entry| entry.name == name.as_ref());
        let external = safe_work_path(&self.host.work_dir, &name).is_some_and(|path| path.exists());
        Ok(vec![Value::Number(if in_package || external {
            1.0
        } else {
            0.0
        })])
    }

    fn file_open(&mut self, args: &[Value]) -> Result<Vec<Value>> {
        let name = value_bytes(args.first())?;
        let mode = u32::try_from(integer_number(args.get(1).unwrap_or(&Value::Nil))?)
            .map_err(|_| crate::Error::MrFault("file.open mode is negative".into()))?;
        let handle = self.host.mr_file_open(&name, mode)?;
        if handle < 0 {
            return Ok(vec![Value::Nil]);
        }
        let file = Table::new();
        let mut values = file.borrow_mut();
        values.set(bytes(b"__handle"), Value::Number(f64::from(handle)));
        values.set(bytes(b"read"), Value::Native("file_read"));
        values.set(bytes(b"seek"), Value::Native("file_seek"));
        values.set(bytes(b"write"), Value::Native("file_write"));
        values.set(bytes(b"close"), Value::Native("file_close"));
        drop(values);
        Ok(vec![Value::Table(file)])
    }

    fn file_write(&mut self, args: &[Value]) -> Result<Vec<Value>> {
        let handle = file_handle(args.first())?;
        let data = value_bytes(args.get(1))?;
        Ok(vec![Value::Number(
            self.host
                .mr_file_write(handle, &data)?
                .and_then(|len| u32::try_from(len).ok())
                .map(f64::from)
                .unwrap_or(-1.0),
        )])
    }

    fn file_read(&mut self, args: &[Value]) -> Result<Vec<Value>> {
        let handle = file_handle(args.first())?;
        let len = usize::try_from(integer_number(args.get(1).unwrap_or(&Value::Nil))?)
            .map_err(|_| crate::Error::MrFault("file.read length is negative".into()))?;
        if len > self.limits.max_mr_string_len {
            return Err(crate::Error::ResourceLimit(format!(
                "file.read length {len} exceeds {}",
                self.limits.max_mr_string_len
            )));
        }
        Ok(vec![
            self.host
                .mr_file_read(handle, len)?
                .map_or(Value::Nil, |data| Value::Bytes(data.into())),
        ])
    }

    fn file_seek(&mut self, args: &[Value]) -> Result<Vec<Value>> {
        let handle = file_handle(args.first())?;
        let origin = u32::try_from(integer_number(args.get(1).unwrap_or(&Value::Nil))?)
            .map_err(|_| crate::Error::MrFault("file.seek origin is negative".into()))?;
        let offset = i32::try_from(integer_number(args.get(2).unwrap_or(&Value::Nil))?)
            .map_err(|_| crate::Error::MrFault("file.seek offset is out of range".into()))?;
        Ok(vec![
            self.host
                .mr_file_seek(handle, offset, origin)?
                .and_then(|position| u32::try_from(position).ok())
                .map(|position| Value::Number(f64::from(position)))
                .unwrap_or(Value::Nil),
        ])
    }

    fn file_close(&mut self, args: &[Value]) -> Result<Vec<Value>> {
        let handle = file_handle(args.first())?;
        Ok(vec![Value::Number(f64::from(
            self.host.mr_file_close(handle)?,
        ))])
    }

    fn file_remove(&mut self, args: &[Value]) -> Result<Vec<Value>> {
        let name = value_bytes(args.first())?;
        Ok(vec![Value::Number(f64::from(
            self.host.mr_file_remove(&name)?,
        ))])
    }
}

fn file_handle(value: Option<&Value>) -> Result<i32> {
    let handle = match value {
        Some(Value::Table(file)) => integer_number(&file.borrow().get(&bytes(b"__handle"))),
        Some(value) => integer_number(value),
        None => Err(crate::Error::MrFault("file handle is missing".into())),
    }?;
    i32::try_from(handle).map_err(|_| crate::Error::MrFault("file handle is out of range".into()))
}

// Vendor gui.e_create (mr_iolib_target.c EditCreate): luaL_optlong for the
// max_size argument defaults to 68.
const MR_LUA_EDIT_MAX_SIZE: i32 = 68;

fn sound_index(value: Option<&Value>) -> usize {
    // Vendor truncates the index through uint16 before the SOUNDMAX bounds
    // check (mythroad.c MRF_SoundSet/MRF_SoundPlay/MRF_SoundStop). The i64
    // hop keeps C-style wraparound (e.g. -1 -> 65535) because the direct
    // float->u16 cast saturates instead.
    value.and_then(Value::number).unwrap_or(0.0) as i64 as u16 as usize
}

fn sound_type(value: Option<&Value>, default: crate::SoundType) -> crate::SoundType {
    // mrporting.h: MR_SOUND_MIDI = 0, MR_SOUND_WAV = 1, MR_SOUND_MP3 = 2.
    // to_mr_tonumber defaults to MR_SOUND_WAV when the argument is absent;
    // any other value is passed straight to the platform in the vendor.
    match value.and_then(Value::number).map(|number| number as i64) {
        Some(1) => crate::SoundType::Wav,
        Some(2) => crate::SoundType::Mp3,
        Some(_) => crate::SoundType::Midi,
        None => default,
    }
}
