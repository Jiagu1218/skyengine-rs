use super::stdlib::*;
use super::*;

struct LifecycleTestDisplay;

impl PlatformDisplay for LifecycleTestDisplay {
    fn present(&mut self, _framebuffer: &Framebuffer) -> Result<()> {
        Ok(())
    }

    fn poll_event(&mut self) -> Result<Option<crate::DisplayEvent>> {
        Ok(None)
    }

    fn wait_timeout(&mut self, _milliseconds: u32) {}
}

#[test]
fn v80_division_uses_integer_semantics_without_changing_v50() {
    assert!(
        arithmetic(
            MrProfile::V80,
            15,
            &Value::Number(2.0),
            &Value::Number(13.0),
        )
        .unwrap()
        .raw_equal(&Value::Number(0.0))
    );
    assert!(
        arithmetic(
            MrProfile::V80,
            15,
            &Value::Number(-15.0),
            &Value::Number(4.0),
        )
        .unwrap()
        .raw_equal(&Value::Number(-3.0))
    );
    assert!(
        arithmetic(
            MrProfile::V50,
            15,
            &Value::Number(2.0),
            &Value::Number(13.0),
        )
        .unwrap()
        .raw_equal(&Value::Number(2.0 / 13.0))
    );
}

#[test]
fn legacy_pcall_alias_is_registered_and_protects_native_calls() {
    let (mut vm, root, _) = immediate_restart_vm();
    assert!(vm.global(b"_pCall").raw_equal(&Value::Native("_pCall")));

    let result = vm
        .call_value(
            Value::Native("_pCall"),
            vec![Value::Native("error"), bytes(b"expected failure")],
            None,
            false,
        )
        .unwrap();
    let CallResult::Immediate(values) = result else {
        panic!("native protected call must return immediately");
    };
    assert!(values[0].raw_equal(&Value::Boolean(false)));
    assert!(
        values[1]
            .bytes()
            .is_some_and(|error| error.ends_with(b"expected failure"))
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn legacy_network_close_is_registered_and_returns_success() {
    let (mut vm, root, _) = immediate_restart_vm();
    assert!(
        vm.global(b"_closeNet")
            .raw_equal(&Value::Native("_closeNet"))
    );

    let result = vm
        .call_value(Value::Native("_closeNet"), Vec::new(), None, false)
        .unwrap();
    let CallResult::Immediate(values) = result else {
        panic!("native network close must return immediately");
    };
    assert_eq!(values.len(), 1);
    assert_eq!(values[0].number(), Some(0.0));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn vendor_phone_library_is_registered_with_modemless_stubs() {
    let (mut vm, root, _) = immediate_restart_vm();

    // The vendor engine opens the `phone` library (mr_L_openlib with
    // MRP_PHONELIBNAME) and mirrors its four entry points as plain
    // globals. DSM apps such as QQDSM2007 index `phone.getNetID`
    // during boot, so the table itself must exist on modemless hosts.
    let phone = vm.global(b"phone");
    let Value::Table(table) = &phone else {
        panic!("phone library must be a table");
    };
    for (name, native) in [
        (&b"call"[..], "phone_call"),
        (&b"sendSms"[..], "phone_send_sms"),
        (&b"getNetID"[..], "phone_get_net_id"),
        (&b"wap"[..], "phone_wap"),
    ] {
        assert!(
            table
                .borrow()
                .get(&bytes(name))
                .raw_equal(&Value::Native(native)),
            "phone.{native} must be registered"
        );
    }
    for name in [
        &b"Call"[..],
        &b"SendSms"[..],
        &b"GetNetworkID"[..],
        &b"ConnectWAP"[..],
    ] {
        assert!(
            vm.global(name).raw_equal(&Value::Native(std::str::from_utf8(name).unwrap())),
            "global {} must be registered",
            String::from_utf8_lossy(name)
        );
    }

    // Vendor desktop semantics (dsm.c): mr_getNetworkID answers
    // MR_NET_ID_MOBILE (0), mr_sendSms answers MR_SUCCESS, and
    // mr_call/mr_connectWAP only log, returning no values.
    let net = vm
        .call_value(Value::Native("phone_get_net_id"), Vec::new(), None, false)
        .unwrap();
    let CallResult::Immediate(values) = net else {
        panic!("phone.getNetID must return immediately");
    };
    assert_eq!(values.len(), 1);
    assert!(values[0].raw_equal(&Value::Number(0.0)));

    let sms = vm
        .call_value(
            Value::Native("SendSms"),
            vec![bytes(b"10086"), bytes(b"hello"), Value::Number(0.0)],
            None,
            false,
        )
        .unwrap();
    let CallResult::Immediate(values) = sms else {
        panic!("phone.sendSms must return immediately");
    };
    assert_eq!(values.len(), 1);
    assert!(values[0].raw_equal(&Value::Number(0.0)));

    let dialed = vm
        .call_value(Value::Native("phone_call"), vec![bytes(b"10086")], None, false)
        .unwrap();
    let CallResult::Immediate(values) = dialed else {
        panic!("phone.call must return immediately");
    };
    assert!(values.is_empty());

    let wap = vm
        .call_value(
            Value::Native("phone_wap"),
            vec![bytes(b"http://wap.example/")],
            None,
            false,
        )
        .unwrap();
    let CallResult::Immediate(values) = wap else {
        panic!("phone.wap must return immediately");
    };
    assert!(values.is_empty());

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn vendor_sound_library_is_registered_with_five_playback_slots() {
    let (mut vm, root, _) = immediate_restart_vm();

    // The vendor engine registers the six sound entry points as plain
    // globals (mythroad.c mrp_register: BgMusicSet/Start/Stop and
    // SoundSet/Play/Stop). QQDSM2007 calls SoundSet(1, "msg.mid", 0)
    // during boot, so the natives must resolve.
    for name in [
        &b"SoundSet"[..],
        &b"SoundPlay"[..],
        &b"SoundStop"[..],
        &b"BgMusicSet"[..],
        &b"BgMusicStart"[..],
        &b"BgMusicStop"[..],
    ] {
        assert!(
            vm.global(name).raw_equal(&Value::Native(std::str::from_utf8(name).unwrap())),
            "global {} must be registered",
            String::from_utf8_lossy(name)
        );
    }

    // Vendor index errors are catchable Lua faults (mrp_error), and the
    // uint16 truncation wraps out-of-range indices before the SOUNDMAX
    // check (70000 -> 4464).
    for (native, index, message) in [
        ("SoundSet", 5.0, "SoundSet :index 5 invalid!"),
        ("SoundPlay", 70000.0, "SoundPlay:index 4464 invalid!"),
        ("SoundStop", -1.0, "SoundStop:index 65535 invalid!"),
    ] {
        let error = match vm.call_value(
            Value::Native(native),
            vec![Value::Number(index), bytes(b"x")],
            None,
            false,
        ) {
            Err(error) => error,
            Ok(_) => panic!("{native}({index}) must fault with {message}"),
        };
        assert!(
            error.to_string().contains(message),
            "{native}({index}) must fault with {message}, got {error}"
        );
    }

    // Vendor SoundSet reads through _mr_readFile: a missing file raises a
    // catchable fault, while "*" clears the slot silently.
    let error = match vm.call_value(
        Value::Native("SoundSet"),
        vec![Value::Number(1.0), bytes(b"missing.mid"), Value::Number(0.0)],
        None,
        false,
    ) {
        Err(error) => error,
        Ok(_) => panic!("SoundSet on a missing file must fault"),
    };
    assert!(
        error.to_string().contains("SoundSet 1:cannot read"),
        "missing sound file must fault, got {error}"
    );

    // A packaged file loads into the slot, so SoundPlay reaches the audio
    // backend; BgMusic* drives slot 0, which is still unset and therefore
    // a tolerant no-op like the vendor's desktop shim.
    for (native, args) in [
        (
            "SoundSet",
            vec![Value::Number(1.0), bytes(b"start.mr"), Value::Number(0.0)],
        ),
        ("SoundPlay", vec![Value::Number(1.0), Value::Boolean(false)]),
        ("BgMusicStart", Vec::new()),
        ("BgMusicStop", Vec::new()),
        ("SoundStop", vec![Value::Number(1.0)]),
        ("SoundSet", vec![Value::Number(1.0), bytes(b"*"), Value::Number(0.0)]),
    ] {
        let result = vm
            .call_value(Value::Native(native), args, None, false)
            .unwrap_or_else(|error| panic!("{native} must not fault: {error}"));
        let CallResult::Immediate(values) = result else {
            panic!("{native} must return immediately");
        };
        assert!(values.is_empty(), "{native} returns no values");
    }

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn vendor_sys_library_exposes_directory_and_time_helpers() {
    let (mut vm, root, _) = immediate_restart_vm();

    // Vendor syslib (mr_iolib_target.c) registers every entry with a
    // leading-uppercase and an all-lowercase alias; QQDSM2007 boots through
    // sys.mkDir("qqdsm"), so the directory helpers must exist.
    let sys = vm.global(b"sys");
    let Value::Table(table) = &sys else {
        panic!("sys library must be a table");
    };
    for (name, native) in [
        (&b"mkDir"[..], "sys_mk_dir"),
        (&b"mkdir"[..], "sys_mk_dir"),
        (&b"rmDir"[..], "sys_rm_dir"),
        (&b"rmdir"[..], "sys_rm_dir"),
        (&b"getFileInfo"[..], "sys_get_file_info"),
        (&b"getfileinfo"[..], "sys_get_file_info"),
        (&b"getFileLen"[..], "sys_get_file_len"),
        (&b"getfilelen"[..], "sys_get_file_len"),
        (&b"getUptime"[..], "sys_get_uptime"),
        (&b"getuptime"[..], "sys_get_uptime"),
        (&b"datetime"[..], "sys_datetime"),
    ] {
        assert!(
            table
                .borrow()
                .get(&bytes(name))
                .raw_equal(&Value::Native(native)),
            "sys.{native} must be registered"
        );
    }
    assert!(
        vm.global(b"GetDatetime").raw_equal(&Value::Native("GetDatetime")),
        "global GetDatetime must be registered"
    );

    // Vendor my_mkDir answers success for an existing path, my_info answers
    // MR_IS_DIR (2) / MR_IS_INVALID (8), and my_getLen answers -1 when the
    // path is missing.
    let call = |vm: &mut MrVm, native: &'static str, args: Vec<Value>| {
        match vm.call_value(Value::Native(native), args, None, false) {
            Ok(CallResult::Immediate(values)) => values,
            Ok(_) => panic!("{native} must return immediately"),
            Err(error) => panic!("{native} must not fault: {error}"),
        }
    };
    let made = call(&mut vm, "sys_mk_dir", vec![bytes(b"qqdsm")]);
    assert_eq!(made.len(), 1);
    assert!(made[0].raw_equal(&Value::Boolean(true)));
    // Repeat mkdir on the existing directory stays successful (vendor
    // access(F_OK) short-circuit).
    let made_again = call(&mut vm, "sys_mk_dir", vec![bytes(b"qqdsm")]);
    assert!(made_again[0].raw_equal(&Value::Boolean(true)));

    let info = call(&mut vm, "sys_get_file_info", vec![bytes(b"qqdsm")]);
    assert!(info[0].raw_equal(&Value::Number(2.0)));

    let missing = call(&mut vm, "sys_get_file_info", vec![bytes(b"no-such-file")]);
    assert!(missing[0].raw_equal(&Value::Number(8.0)));
    let missing_len = call(&mut vm, "sys_get_file_len", vec![bytes(b"no-such-file")]);
    assert!(missing_len[0].raw_equal(&Value::Number(-1.0)));

    let removed = call(&mut vm, "sys_rm_dir", vec![bytes(b"qqdsm")]);
    assert!(removed[0].raw_equal(&Value::Boolean(true)));
    let gone = call(&mut vm, "sys_get_file_info", vec![bytes(b"qqdsm")]);
    assert!(gone[0].raw_equal(&Value::Number(8.0)));

    // mr_getTime answers a millisecond uptime; _mr_GetDatetime answers the
    // {year, mon, day, hour, min, sec} table (vendor also exposes it as the
    // global GetDatetime).
    let uptime = call(&mut vm, "sys_get_uptime", Vec::new());
    assert!(
        matches!(uptime.first(), Some(Value::Number(_))),
        "getUptime answers a number"
    );

    let datetime = call(&mut vm, "GetDatetime", Vec::new());
    let Value::Table(datetime) = &datetime[0] else {
        panic!("GetDatetime answers a table");
    };
    let datetime = datetime.borrow();
    for field in [
        &b"year"[..],
        &b"mon"[..],
        &b"day"[..],
        &b"hour"[..],
        &b"min"[..],
        &b"sec"[..],
    ] {
        assert!(
            matches!(datetime.get(&bytes(field)), Value::Number(_)),
            "datetime field {} must be a number",
            String::from_utf8_lossy(field)
        );
    }

    std::fs::remove_dir_all(root).unwrap();
}

fn lifecycle_test_root(label: &str) -> std::path::PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "skyengine-lifecycle-{label}-{}-{nonce}",
        std::process::id()
    ))
}

fn immediate_self_restart_image() -> Vec<u8> {
    const MODULE_BASE: u32 = 0x1000_0000;
    const TRAP_BASE: u32 = 0xff00_0000;
    const LIFECYCLE_CALLBACK_DATA: u32 = 0x0100_1984;
    const APPLICATION_STATE_DATA: u32 = 0x0100_1980;

    let helper = MODULE_BASE + 40;
    let restart = MODULE_BASE + 84;
    let instructions = [
        0xe92d_4000, // entry: push {lr}
        0xe59f_000c, // ldr r0, [pc, #12] (helper)
        0xe3a0_1014, // mov r1, #20
        0xe59f_c008, // ldr ip, [pc, #8] (slot 25)
        0xe12f_ff3c, // blx ip
        0xe8bd_8000, // pop {pc}
        helper,
        TRAP_BASE + 25 * 4,
        0xe59f_0018, // helper: ldr r0, [pc, #24] (callback pointer)
        0xe59f_1018, // ldr r1, [pc, #24] ("restart")
        0xe580_1000, // str r1, [r0]
        0xe59f_0014, // ldr r0, [pc, #20] (application state)
        0xe3a0_1003, // mov r1, #3
        0xe580_1000, // str r1, [r0]
        0xe3a0_0000, // mov r0, #0
        0xe12f_ff1e, // bx lr
        LIFECYCLE_CALLBACK_DATA,
        restart,
        APPLICATION_STATE_DATA,
        u32::from_le_bytes(*b"rest"),
        u32::from_le_bytes(*b"art\0"),
    ];
    let mut image = b"MRPGCMAP".to_vec();
    image.extend(instructions.into_iter().flat_map(u32::to_le_bytes));
    image
}

fn failing_native_init_image() -> Vec<u8> {
    const TRAP_BASE: u32 = 0xff00_0000;
    let instructions = [
        0xe59f_c000, // ldr ip, [pc] (unsupported platform trap)
        0xe12f_ff3c, // blx ip
        TRAP_BASE + 21 * 4,
    ];
    let mut image = b"MRPGCMAP".to_vec();
    image.extend(instructions.into_iter().flat_map(u32::to_le_bytes));
    image
}

fn write_lifecycle_test_package(path: &std::path::Path, image: &[u8]) {
    const LIST_START: usize = 0xf0;
    const ENTRY: &[u8] = b"start.mr";
    let directory_len = 4 + ENTRY.len() + 1 + 12;
    let payload_start = LIST_START + directory_len;
    let mut package = vec![0_u8; payload_start + image.len()];
    let package_len = package.len() as u32;
    package[0..4].copy_from_slice(b"MRPG");
    package[4..8].copy_from_slice(&((payload_start - 8) as u32).to_le_bytes());
    package[8..12].copy_from_slice(&package_len.to_le_bytes());
    package[12..16].copy_from_slice(&(LIST_START as u32).to_le_bytes());
    package[0x10..0x18].copy_from_slice(b"self.mrp");
    package[LIST_START..LIST_START + 4].copy_from_slice(&((ENTRY.len() + 1) as u32).to_le_bytes());
    let name = LIST_START + 4;
    package[name..name + ENTRY.len()].copy_from_slice(ENTRY);
    let fields = name + ENTRY.len() + 1;
    package[fields..fields + 4].copy_from_slice(&(payload_start as u32).to_le_bytes());
    package[fields + 4..fields + 8].copy_from_slice(&(image.len() as u32).to_le_bytes());
    package[payload_start..].copy_from_slice(image);
    std::fs::write(path, package).unwrap();
}

fn immediate_restart_vm() -> (MrVm, std::path::PathBuf, std::path::PathBuf) {
    let root = lifecycle_test_root("immediate-restart");
    let mythroad = root.join("mythroad");
    std::fs::create_dir_all(&mythroad).unwrap();
    let package_path = mythroad.join("self.mrp");
    write_lifecycle_test_package(&package_path, &immediate_self_restart_image());
    let limits = ResourceLimits::default();
    let package = Arc::new(Package::open(&package_path, limits.clone()).unwrap());
    let vm = MrVm::new(
        package,
        Framebuffer::new(240, 320).unwrap(),
        Box::new(LifecycleTestDisplay),
        Box::new(crate::SilentAudio),
        MrHostConfig {
            work_dir: root.clone(),
            font: Arc::from(&b""[..]),
            memory_limit: 2 * 1024 * 1024,
            dns_mappings: Vec::<crate::DnsMapping>::new().into(),
            device_date: crate::DeviceDate::default(),
            wap_proxy_endpoint: None,
        },
        limits,
    );
    (vm, root, package_path)
}

#[test]
fn immediate_self_restart_commits_at_most_once_per_dispatch() {
    let (mut vm, root, _) = immediate_restart_vm();
    vm.run_entry(b"start.mr").unwrap();
    assert!(matches!(
        vm.host.lifecycle_request().unwrap(),
        Some(ExtLifecycleRequest::Restart { .. })
    ));

    assert_eq!(
        vm.process_lifecycle_request().unwrap(),
        LifecycleOutcome::Continue
    );
    assert!(matches!(
        vm.host.lifecycle_request().unwrap(),
        Some(ExtLifecycleRequest::Restart { .. })
    ));
    assert_eq!(
        vm.process_lifecycle_request().unwrap(),
        LifecycleOutcome::Continue
    );
    assert!(matches!(
        vm.host.lifecycle_request().unwrap(),
        Some(ExtLifecycleRequest::Restart { .. })
    ));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn failed_restart_preparation_acknowledges_the_request_and_keeps_the_old_app_runnable() {
    let (mut vm, root, package_path) = immediate_restart_vm();
    vm.run_entry(b"start.mr").unwrap();
    std::fs::remove_file(package_path).unwrap();

    assert!(matches!(
        vm.process_lifecycle_request(),
        Err(LifecycleError::BeforeCommit(_))
    ));
    assert_eq!(vm.host.lifecycle_request().unwrap(), None);
    vm.host.dispatch_native_event(1, 2, 3).unwrap();
    assert!(matches!(
        vm.host.lifecycle_request().unwrap(),
        Some(ExtLifecycleRequest::Restart { .. })
    ));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn native_init_fault_after_restart_commit_is_terminal_for_the_lifecycle_dispatch() {
    let (mut vm, root, package_path) = immediate_restart_vm();
    vm.run_entry(b"start.mr").unwrap();
    write_lifecycle_test_package(&package_path, &failing_native_init_image());

    assert!(matches!(
        vm.process_lifecycle_request(),
        Err(LifecycleError::AfterCommit(_))
    ));
    assert_eq!(vm.host.lifecycle_request().unwrap(), None);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn table_insert_shifts_sequence_values() {
    let table = Table::new();
    table.borrow_mut().set(Value::Number(1.0), bytes(b"a"));
    table.borrow_mut().set(Value::Number(2.0), bytes(b"c"));
    table_insert(&[Value::Table(table.clone()), Value::Number(2.0), bytes(b"b")]).unwrap();
    assert_eq!(table.borrow().sequence_len(), 3);
    assert!(
        table
            .borrow()
            .get(&Value::Number(2.0))
            .raw_equal(&bytes(b"b"))
    );
}

#[test]
fn native_number_supports_radix() {
    assert!(native_tonumber(&[bytes(b"7f"), Value::Number(16.0)]).raw_equal(&Value::Number(127.0)));
    assert!(matches!(
        native_tonumber(&[bytes(b"not-a-number")]),
        Value::Nil
    ));
}

#[test]
fn mythroad_string_conversion_matches_tostring() {
    assert!(native_tostring(&[]).raw_equal(&bytes(b"nil")));
    assert!(native_tostring(&[Value::Number(54_892_597.0)]).raw_equal(&bytes(b"54892597")));
    assert!(native_tostring(&[Value::Boolean(true)]).raw_equal(&bytes(b"true")));
}

#[test]
fn c_string_helpers_stop_at_nul() {
    let value = bytes(b"abc\0def");
    assert_eq!(
        string_clen(std::slice::from_ref(&value)).unwrap()[0].number(),
        Some(3.0)
    );
    assert!(string_cstr(&[value]).unwrap()[0].raw_equal(&bytes(b"abc")));
}

#[test]
fn mutable_string_update_uses_one_based_offsets() {
    let buffer = Value::Buffer(std::rc::Rc::new(std::cell::RefCell::new(vec![0; 5])));
    string_update(&[buffer.clone(), bytes(b"abc"), Value::Number(2.0)]).unwrap();
    assert_eq!(buffer.bytes().unwrap().as_ref(), b"\0abc\0");
}

#[test]
fn mutable_string_update_moves_an_exclusive_source_range_with_overlap() {
    let buffer = Value::Buffer(std::rc::Rc::new(std::cell::RefCell::new(
        b"0123456789".to_vec(),
    )));
    string_update(&[
        buffer.clone(),
        buffer.clone(),
        Value::Number(1.0),
        Value::Number(4.0),
        Value::Number(8.0),
    ])
    .unwrap();
    assert_eq!(buffer.bytes().unwrap().as_ref(), b"3456456789");

    let unchanged = buffer.bytes().unwrap();
    string_update(&[
        buffer.clone(),
        buffer.clone(),
        Value::Number(1.0),
        Value::Number(8.0),
        Value::Number(8.0),
    ])
    .unwrap();
    assert_eq!(buffer.bytes().unwrap().as_ref(), unchanged.as_ref());
}

#[test]
fn mutable_string_update_copies_a_source_suffix_with_destination_truncation() {
    let buffer = Value::Buffer(std::rc::Rc::new(std::cell::RefCell::new(vec![0; 4])));
    string_update(&[
        buffer.clone(),
        bytes(b"abcdef"),
        Value::Number(2.0),
        Value::Number(3.0),
    ])
    .unwrap();
    assert_eq!(buffer.bytes().unwrap().as_ref(), b"\0cde");
}

#[test]
fn empty_string_update_is_a_noop_after_network_cleanup() {
    string_update(&[
        bytes(b""),
        bytes(b""),
        Value::Number(1.0),
        Value::Number(4_814.0),
    ])
    .unwrap();
}

#[test]
fn pure_mr_file_objects_write_and_remove_work_files() {
    let (mut vm, root, _) = immediate_restart_vm();
    let Value::Table(sys) = vm.global(b"sys") else {
        panic!("sys must be a table");
    };
    assert!(
        sys.borrow()
            .get(&bytes(b"rm"))
            .raw_equal(&Value::Native("file_remove"))
    );

    let file = vm
        .call_native("file_open", &[bytes(b"applist.mrp"), Value::Number(10.0)])
        .unwrap()
        .remove(0);
    assert!(matches!(file, Value::Table(_)));
    let written = vm
        .call_native("file_write", &[file.clone(), bytes(b"MRPG\0")])
        .unwrap();
    assert_eq!(written[0].number(), Some(5.0));
    assert_eq!(
        vm.call_native("file_close", &[file]).unwrap()[0].number(),
        Some(0.0)
    );
    let path = root.join("mythroad/applist.mrp");
    assert_eq!(std::fs::read(&path).unwrap(), b"MRPG\0");

    let file = vm
        .call_native("file_open", &[bytes(b"applist.mrp"), Value::Number(1.0)])
        .unwrap()
        .remove(0);
    assert_eq!(
        vm.call_native(
            "file_seek",
            &[file.clone(), Value::Number(0.0), Value::Number(1.0)],
        )
        .unwrap()[0]
            .number(),
        Some(1.0)
    );
    assert!(
        vm.call_native("file_read", &[file.clone(), Value::Number(3.0)])
            .unwrap()[0]
            .raw_equal(&bytes(b"RPG"))
    );
    vm.call_native("file_close", &[file]).unwrap();
    assert_eq!(
        vm.call_native("file_remove", &[bytes(b"applist.mrp")])
            .unwrap()[0]
            .number(),
        Some(0.0)
    );
    assert!(!path.exists());

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_com_300_compiles_an_executable_network_callback() {
    let source = br#"print("code frame received")
def progress(data)
  p = tonumber(data)
  if p == nil then
    p = 0
  end
  if g_dialog then
    if g_dialog.update then
      g_dialog.update(g_dialog, nil, data .. "%", p)
    end
  end
  if win then
    if win.refresh then
      win.refresh()
    end
  end
end
cmd.progress = progress"#;
    let (mut vm, root, _) = immediate_restart_vm();
    let commands = Table::new();
    vm.set_global(b"cmd", Value::Table(commands.clone()))
        .unwrap();

    let compiled = vm
        .call_native(
            "TestCom1",
            &[
                Value::Number(300.0),
                Value::Bytes(Arc::from(source.as_slice())),
            ],
        )
        .unwrap()
        .remove(0);
    assert!(matches!(compiled, Value::Closure(_)));
    assert!(vm.global(b"_loads").raw_equal(&Value::Native("_loads")));
    let compiled = vm.call_native("_loads", &[compiled]).unwrap().remove(0);
    let CallResult::Pushed = vm.call_value(compiled, Vec::new(), None, false).unwrap() else {
        panic!("compiled source must execute as an MR frame");
    };
    vm.run_frames().unwrap();
    assert!(matches!(
        commands.borrow().get(&bytes(b"progress")),
        Value::Closure(_)
    ));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn text_network_callbacks_share_assigned_global_state() {
    let source = br#"dl_written = 0
def dl_file(data)
  dl_written = dl_written + data
end
cmd.file = dl_file"#;
    let (mut vm, root, _) = immediate_restart_vm();
    let commands = Table::new();
    vm.set_global(b"cmd", Value::Table(commands.clone()))
        .unwrap();

    let compiled = vm
        .call_native(
            "TestCom1",
            &[
                Value::Number(300.0),
                Value::Bytes(Arc::from(source.as_slice())),
            ],
        )
        .unwrap()
        .remove(0);
    let compiled = vm.call_native("_loads", &[compiled]).unwrap().remove(0);
    let CallResult::Pushed = vm.call_value(compiled, Vec::new(), None, false).unwrap() else {
        panic!("compiled source must execute as an MR frame");
    };
    vm.run_frames().unwrap();

    let callback = commands.borrow().get(&bytes(b"file"));
    let CallResult::Pushed = vm
        .call_value(callback.clone(), vec![Value::Number(17.0)], None, false)
        .unwrap()
    else {
        panic!("file callback must execute as an MR frame");
    };
    vm.run_frames().unwrap();
    let CallResult::Pushed = vm
        .call_value(callback, vec![Value::Number(25.0)], None, false)
        .unwrap()
    else {
        panic!("file callback must execute as an MR frame");
    };
    vm.run_frames().unwrap();

    assert_eq!(vm.global(b"dl_written").number(), Some(42.0));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn load_pack_installs_a_text_entry_loader_until_the_handle_is_released() {
    let (mut vm, root, _) = immediate_restart_vm();
    let package_path = root.join("mythroad/applist.mrp");
    write_lifecycle_test_package(
        &package_path,
        br#"list = {{t = "APP", e = "talkcat", ic = 1}}"#,
    );

    let handle = vm
        .call_native("LoadPack", &[bytes(b"applist.mrp")])
        .unwrap()
        .remove(0);
    assert!(handle.raw_equal(&Value::Native("loaded_pack")));
    assert!(
        vm.global(b"loadfile")
            .raw_equal(&Value::Native("load_pack_file"))
    );
    let entry = vm
        .call_native("load_pack_file", &[bytes(b"start.mr")])
        .unwrap()
        .remove(0);
    let CallResult::Pushed = vm.call_value(entry, Vec::new(), None, false).unwrap() else {
        panic!("loaded text entry must execute as an MR frame");
    };
    vm.run_frames().unwrap();
    let Value::Table(list) = vm.global(b"list") else {
        panic!("loaded entry must create the application list");
    };
    let Value::Table(application) = list.borrow().get(&Value::Number(1.0)) else {
        panic!("application list must contain the first record");
    };
    assert!(
        application
            .borrow()
            .get(&bytes(b"e"))
            .raw_equal(&bytes(b"talkcat"))
    );

    vm.call_native("LoadPack", &[handle]).unwrap();
    assert!(vm.global(b"loadfile").raw_equal(&Value::Nil));
    assert!(vm.host.read_loaded_pack(b"start.mr").is_err());

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn sub_value_splits_numbers_and_byte_strings_into_little_endian_words() {
    let number_bits = 0x1122_3344_aabb_ccdd_u64;
    let number = string_sub_value(&[Value::Number(f64::from_bits(number_bits))]).unwrap();
    assert_eq!(number[0].number(), Some(0xaabb_ccdd_u32 as f64));
    assert_eq!(number[1].number(), Some(0x1122_3344_u32 as f64));

    let string = string_sub_value(&[bytes(b"unknow")]).unwrap();
    assert_eq!(string[0].number(), Some(0x6e6b_6e75_u32 as f64));
    assert_eq!(string[1].number(), Some(0x0000_776f_u32 as f64));
}

#[test]
fn work_paths_map_the_guest_c_drive_to_the_runtime_root() {
    let root = std::path::Path::new("device");
    assert_eq!(
        safe_work_path(root, b"C:/mythroad/system/font.uc2"),
        Some(PathBuf::from("device/mythroad/system/font.uc2"))
    );
    assert_eq!(
        safe_work_path(root, b"c:\\mythroad\\system\\font.uc2"),
        Some(PathBuf::from("device/mythroad/system/font.uc2"))
    );
    assert_eq!(
        safe_work_path(root, b"X:/mythroad/file"),
        Some(root.join("disk/x/mythroad/file"))
    );
    assert_eq!(
        safe_work_path(root, b"y:\\cache\\file"),
        Some(root.join("disk/y/cache/file"))
    );
    assert_eq!(
        safe_work_path(root, b"Z:/data/file"),
        Some(root.join("disk/z/data/file"))
    );
    assert_eq!(safe_work_path(root, b"D:/mythroad/file"), None);
    assert_eq!(safe_work_path(root, b"C:/../file"), None);
    // GBK-decoded names resolve like UTF-8 ones (host::guest_path_string):
    // directory searches hand GBK bytes back to open/stat, and those must
    // reach the same host file. GBK "新建.ce".
    assert_eq!(
        safe_work_path(root, b"C:/mythroad/\xd0\xc2\xbd\xa8.ce"),
        Some(PathBuf::from("device/mythroad/新建.ce"))
    );
    // Bytes that are neither valid UTF-8 nor valid GBK are still rejected.
    assert_eq!(safe_work_path(root, b"\xff\xff.ce"), None);
}

#[test]
fn genuine_platform_handshake_global_is_seeded() {
    // qq2008's @start.mr refuses to run ("cann`t be run direct!") unless the
    // script VM exposes the obfuscated genuine-platform marker global.
    let (vm, root, _) = immediate_restart_vm();
    assert!(
        vm.global(b"_nes3ShP7SwK0")
            .raw_equal(&Value::Number(370.0)),
        "genuine-platform handshake global must equal 370"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn plat_rotates_the_lcd_and_ignores_unknown_commands() {
    let (mut vm, root, _) = immediate_restart_vm();
    assert!(
        vm.global(b"_plat").raw_equal(&Value::Native("_plat")),
        "_plat must be registered like the reference kernel (mythroad.c:3947)"
    );

    // _plat(101, 3) is MR_LCD_ROTATE_270: the request answers MR_SUCCESS and
    // the shared canvas flips to landscape before any EXT loads (gtcm's
    // SphinxJoy engine gates its landscape mode on this call).
    let result = call_native_immediate(
        &mut vm,
        "_plat",
        vec![Value::Number(101.0), Value::Number(3.0)],
    );
    assert_eq!(result.len(), 1);
    assert!(result[0].raw_equal(&Value::Number(0.0)));
    assert_eq!((vm.host.framebuffer.width(), vm.host.framebuffer.height()), (320, 240));

    // MR_LCD_ROTATE_NORMAL (0) restores the portrait canvas.
    let result = call_native_immediate(
        &mut vm,
        "_plat",
        vec![Value::Number(101.0), Value::Number(0.0)],
    );
    assert_eq!(result.len(), 1);
    assert!(result[0].raw_equal(&Value::Number(0.0)));
    assert_eq!((vm.host.framebuffer.width(), vm.host.framebuffer.height()), (240, 320));

    // Values outside the 0..3 rotation contract and unknown commands answer
    // the non-fatal MR_IGNORE like the C++ platform default.
    let result = call_native_immediate(
        &mut vm,
        "_plat",
        vec![Value::Number(101.0), Value::Number(9.0)],
    );
    assert_eq!(result.len(), 1);
    assert!(result[0].raw_equal(&Value::Number(1.0)));
    let result = call_native_immediate(
        &mut vm,
        "_plat",
        vec![Value::Number(999.0), Value::Number(0.0)],
    );
    assert_eq!(result.len(), 1);
    assert!(result[0].raw_equal(&Value::Number(1.0)));
    assert_eq!((vm.host.framebuffer.width(), vm.host.framebuffer.height()), (240, 320));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn vendor_c2u_converts_gbk_to_null_terminated_ucs2be() {
    let (mut vm, root, _) = immediate_restart_vm();
    assert!(vm.global(b"c2u").raw_equal(&Value::Native("c2u")));
    let string = vm.global(b"string");
    let Value::Table(table) = &string else {
        panic!("string library must be a table");
    };
    assert!(
        table.borrow().get(&bytes(b"c2u")).raw_equal(&Value::Native("c2u")),
        "string.c2u must be registered (vendor strlib[17])"
    );

    let call = |vm: &mut MrVm, args: Vec<Value>| {
        match vm.call_value(Value::Native("c2u"), args, None, false) {
            Ok(CallResult::Immediate(values)) => values,
            Ok(_) => panic!("c2u must return immediately"),
            Err(error) => panic!("c2u must not fault: {error}"),
        }
    };

    // ASCII maps to U+00xx and the trailing U+0000 counts toward the result
    // length, mirroring GBStrToUCS2BEStr's (chars + 1) * 2 sizing.
    let ascii = call(&mut vm, vec![bytes(b"QQ")]);
    assert!(ascii[0].raw_equal(&bytes(&[0x00, b'Q', 0x00, b'Q', 0x00, 0x00])));

    // "确定" in GBK (C8 B7 B6 A8) becomes U+786E U+5B9A in big endian.
    // QQDSM2007 converts its input-dialog titles through this path.
    let chinese = call(&mut vm, vec![bytes(&[0xC8, 0xB7, 0xB6, 0xA8])]);
    assert!(chinese[0].raw_equal(&bytes(&[0x78, 0x6E, 0x5B, 0x9A, 0x00, 0x00])));

    // The empty string still answers a lone terminator.
    let empty = call(&mut vm, vec![bytes(b"")]);
    assert!(empty[0].raw_equal(&bytes(&[0x00, 0x00])));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn vendor_u2c_extracts_the_ascii_prefix_of_ucs2be_text() {
    let (mut vm, root, _) = immediate_restart_vm();
    assert!(vm.global(b"u2c").raw_equal(&Value::Native("u2c")));

    let call = |vm: &mut MrVm, args: Vec<Value>| {
        match vm.call_value(Value::Native("u2c"), args, None, false) {
            Ok(CallResult::Immediate(values)) => values,
            Ok(_) => panic!("u2c must return immediately"),
            Err(error) => panic!("u2c must not fault: {error}"),
        }
    };

    // _mr_u2c keeps the low byte of every U+00xx unit.
    let ascii = call(&mut vm, vec![bytes(&[0x00, b'Q', 0x00, b'Q', 0x00, 0x00])]);
    assert!(ascii[0].raw_equal(&bytes(b"QQ")));

    // Non-ASCII units stop the conversion (vendor breaks out of the loop).
    let mixed = call(&mut vm, vec![bytes(&[0x00, b'A', 0x78, 0x6E, 0x00, 0x00])]);
    assert!(mixed[0].raw_equal(&bytes(b"A")));

    // Inputs shorter than one UCS-2 unit answer "".
    let short = call(&mut vm, vec![bytes(&[0x00])]);
    assert!(short[0].raw_equal(&bytes(b"")));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn gui_library_registers_the_vendor_entry_points() {
    let (vm, root, _) = immediate_restart_vm();
    let gui = vm.global(b"gui");
    let Value::Table(table) = &gui else {
        panic!("gui library must be a table");
    };
    for name in [
        b"m_create".as_slice(),
        b"m_setItem".as_slice(),
        b"m_setitem".as_slice(),
        b"m_show".as_slice(),
        b"m_release".as_slice(),
        b"m_focus".as_slice(),
        b"m_update".as_slice(),
        b"d_create".as_slice(),
        b"d_release".as_slice(),
        b"d_update".as_slice(),
        b"t_create".as_slice(),
        b"t_release".as_slice(),
        b"t_update".as_slice(),
        b"e_create".as_slice(),
        b"e_release".as_slice(),
        b"e_getText".as_slice(),
        b"e_gettext".as_slice(),
        b"w_create".as_slice(),
        b"w_release".as_slice(),
    ] {
        assert!(
            matches!(table.borrow().get(&bytes(name)), Value::Native(_)),
            "gui.{} must be registered",
            String::from_utf8_lossy(name)
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}

fn call_native_immediate(vm: &mut MrVm, name: &'static str, args: Vec<Value>) -> Vec<Value> {
    match vm.call_value(Value::Native(name), args, None, false) {
        Ok(CallResult::Immediate(values)) => values,
        Ok(_) => panic!("{name} must return immediately"),
        Err(error) => panic!("{name} must not fault: {error}"),
    }
}

fn ucs2_of(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_be_bytes());
    }
    out.extend_from_slice(&[0, 0]);
    out
}

#[test]
fn gui_editor_activates_consumes_input_and_commits_text() {
    let (mut vm, root, _) = immediate_restart_vm();

    // e_create(title, text, type, max_size) answers the fixed editor
    // handle and activates the host editor prefilled with the UCS-2 text.
    let result = call_native_immediate(
        &mut vm,
        "gui_edit_create",
        vec![
            bytes(&ucs2_of("QQ号码")),
            bytes(&ucs2_of("确定")),
            Value::Number(0.0),
            Value::Number(10.0),
        ],
    );
    assert!(result[0].raw_equal(&Value::Number(1234.0)));
    assert_eq!(vm.host.active_editor_text(), Some("确定".to_string()));
    let first_session = vm.host.active_editor_session().expect("editor session");

    // While the editor owns input, keys and pointers never reach the guest.
    assert_eq!(vm.host.route_key_event(12, true).unwrap(), None);
    assert_eq!(vm.host.route_key_event(12, false).unwrap(), None);
    assert_eq!(vm.host.route_pointer_event(10, 10, true).unwrap(), None);
    assert_eq!(vm.host.route_pointer_move(20, 20), None);

    // Host submission clamps to max_size code points and completes the
    // widget with MR_DIALOG_EVENT/MR_DIALOG_KEY_OK.
    assert_eq!(
        vm.host.route_text_input("1234567890ABC").unwrap(),
        Some((6, 0, 0))
    );
    assert_eq!(vm.host.active_editor_text(), None);
    assert_eq!(vm.host.active_editor_session(), None);

    // The committed text stays readable through e_getText (UCS-2 BE with
    // the trailing null) until e_release.
    let text = call_native_immediate(&mut vm, "gui_edit_get_text", vec![]);
    assert!(text[0].raw_equal(&bytes(&ucs2_of("1234567890"))));

    call_native_immediate(&mut vm, "gui_edit_release", vec![]);
    let text = call_native_immediate(&mut vm, "gui_edit_get_text", vec![]);
    assert!(matches!(text[0], Value::Nil));

    // With the editor gone, input flows to the guest again.
    assert_eq!(
        vm.host.route_key_event(12, true).unwrap(),
        Some((0, 12, 0))
    );

    // A second editor activation gets a fresh session id: polling hosts
    // must be able to distinguish consecutive editors even when the
    // release and the re-creation land between two polls.
    call_native_immediate(
        &mut vm,
        "gui_edit_create",
        vec![
            bytes(&ucs2_of("")),
            bytes(&ucs2_of("")),
            Value::Number(0.0),
            Value::Number(10.0),
        ],
    );
    let second_session = vm.host.active_editor_session().expect("editor session");
    assert_ne!(second_session, first_session);
    call_native_immediate(&mut vm, "gui_edit_release", vec![]);
    assert_eq!(vm.host.active_editor_session(), None);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn gui_editor_softright_cancels_with_dialog_event() {
    let (mut vm, root, _) = immediate_restart_vm();

    call_native_immediate(
        &mut vm,
        "gui_edit_create",
        vec![
            bytes(&ucs2_of("")),
            bytes(&ucs2_of("12345")),
            Value::Number(0.0),
            Value::Number(68.0),
        ],
    );
    assert_eq!(vm.host.active_editor_text(), Some("12345".to_string()));

    // SOFTRIGHT is the platform cancel key: it closes the editor and
    // completes the widget with MR_DIALOG_KEY_CANCEL.
    assert_eq!(
        vm.host.route_key_event(18, true).unwrap(),
        Some((6, 1, 0))
    );
    assert_eq!(vm.host.active_editor_text(), None);
    assert_eq!(vm.host.active_editor_session(), None);

    // The held text survives the cancel event for the guest's handler.
    let text = call_native_immediate(&mut vm, "gui_edit_get_text", vec![]);
    assert!(text[0].raw_equal(&bytes(&ucs2_of("12345"))));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn gui_widget_stubs_answer_nil_like_the_reference_build() {
    let (mut vm, root, _) = immediate_restart_vm();
    for name in [
        "gui_menu_create",
        "gui_menu_add_item",
        "gui_menu_show",
        "gui_menu_release",
        "gui_menu_set_focus",
        "gui_menu_refresh",
        "gui_dialog_create",
        "gui_dialog_release",
        "gui_dialog_refresh",
        "gui_text_create",
        "gui_text_release",
        "gui_text_refresh",
        "gui_win_create",
        "gui_win_release",
    ] {
        let result = call_native_immediate(&mut vm, name, vec![]);
        assert!(matches!(result[0], Value::Nil), "{name} must answer nil");
    }
    std::fs::remove_dir_all(root).unwrap();
}

fn loadfile_test_vm(label: &str, image: &[u8]) -> (MrVm, std::path::PathBuf) {
    let root = lifecycle_test_root(label);
    let mythroad = root.join("mythroad");
    std::fs::create_dir_all(&mythroad).unwrap();
    let package_path = mythroad.join("self.mrp");
    write_lifecycle_test_package(&package_path, image);
    let limits = ResourceLimits::default();
    let package = Arc::new(Package::open(&package_path, limits.clone()).unwrap());
    let vm = MrVm::new(
        package,
        Framebuffer::new(240, 320).unwrap(),
        Box::new(LifecycleTestDisplay),
        Box::new(crate::SilentAudio),
        MrHostConfig {
            work_dir: root.clone(),
            font: Arc::from(&b""[..]),
            memory_limit: 2 * 1024 * 1024,
            dns_mappings: Vec::<crate::DnsMapping>::new().into(),
            device_date: crate::DeviceDate::default(),
            wap_proxy_endpoint: None,
        },
        limits,
    );
    (vm, root)
}

#[test]
fn base_loadfile_compiles_a_package_chunk_without_running_it() {
    let (mut vm, root) = loadfile_test_vm("loadfile-text", br#"loaded_marker = 1"#);

    // Vendor base_funcs registers the loader both as "loadfile" and as the
    // "_loadFile" alias (mr_baselib.c base_funcs[16]/[25]).
    assert!(vm.global(b"loadfile").raw_equal(&Value::Native("loadfile")));
    assert!(vm.global(b"_loadFile").raw_equal(&Value::Native("_loadFile")));

    let result = vm
        .call_value(
            Value::Native("loadfile"),
            vec![bytes(b"start.mr")],
            None,
            false,
        )
        .unwrap();
    let CallResult::Immediate(values) = result else {
        panic!("loadfile must answer immediately");
    };
    let Some(Value::Closure(chunk)) = values.first() else {
        panic!("loadfile must return the compiled chunk");
    };

    // mr_B_loadfile returns the chunk WITHOUT executing it (unlike dofile).
    assert!(matches!(vm.global(b"loaded_marker"), Value::Nil));

    let CallResult::Pushed = vm
        .call_value(Value::Closure(chunk.clone()), Vec::new(), None, false)
        .unwrap()
    else {
        panic!("compiled chunk must execute as an MR frame");
    };
    vm.run_frames().unwrap();
    assert_eq!(vm.global(b"loaded_marker").number(), Some(1.0));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn base_loadfile_answers_nil_and_cannot_read_for_a_missing_file() {
    let (mut vm, root) = loadfile_test_vm("loadfile-missing", br#"present = 1"#);

    let result = vm
        .call_value(
            Value::Native("loadfile"),
            vec![bytes(b"missing.mr")],
            None,
            false,
        )
        .unwrap();
    let CallResult::Immediate(values) = result else {
        panic!("loadfile must answer immediately");
    };
    // load_mr_aux answers `nil` plus errfile's "cannot read %s" message.
    assert!(values.first().is_some_and(|value| value.raw_equal(&Value::Nil)));
    assert!(
        values
            .get(1)
            .and_then(Value::bytes)
            .is_some_and(|message| message.as_ref() == b"cannot read missing.mr")
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn base_loadfile_prefers_a_work_dir_file_over_the_package_entry() {
    let (mut vm, root) = loadfile_test_vm("loadfile-workdir", br#"from_package = 1"#);
    std::fs::write(root.join("mythroad/start.mr"), br#"from_workdir = 1"#).unwrap();

    let result = vm
        .call_value(
            Value::Native("loadfile"),
            vec![bytes(b"start.mr")],
            None,
            false,
        )
        .unwrap();
    let CallResult::Immediate(values) = result else {
        panic!("loadfile must answer immediately");
    };
    let Some(Value::Closure(chunk)) = values.first() else {
        panic!("loadfile must return the compiled chunk");
    };
    let CallResult::Pushed = vm
        .call_value(Value::Closure(chunk.clone()), Vec::new(), None, false)
        .unwrap()
    else {
        panic!("compiled chunk must execute as an MR frame");
    };
    vm.run_frames().unwrap();

    // _mr_readFile serves the work-dir copy first; the package entry stays
    // untouched.
    assert_eq!(vm.global(b"from_workdir").number(), Some(1.0));
    assert!(matches!(vm.global(b"from_package"), Value::Nil));

    std::fs::remove_dir_all(root).unwrap();
}

fn run_text_source(vm: &mut MrVm, source: &[u8]) {
    let compiled = vm
        .call_native(
            "TestCom1",
            &[
                Value::Number(300.0),
                Value::Bytes(Arc::from(source)),
            ],
        )
        .unwrap()
        .remove(0);
    assert!(matches!(compiled, Value::Closure(_)));
    let compiled = vm.call_native("_loads", &[compiled]).unwrap().remove(0);
    let CallResult::Pushed = vm
        .call_value(compiled, Vec::new(), None, false)
        .unwrap()
    else {
        panic!("compiled source must execute as an MR frame");
    };
    vm.run_frames().unwrap();
}

fn call_text_global(vm: &mut MrVm, name: &[u8], args: Vec<Value>) {
    let function = vm.global(name);
    let CallResult::Pushed = vm.call_value(function, args, None, false).unwrap() else {
        panic!("{} must execute as an MR frame", String::from_utf8_lossy(name));
    };
    vm.run_frames().unwrap();
}

#[test]
fn text_dialect_supports_locals_control_flow_and_logic() {
    let source = br#"total = 0
local i = 0
while i < 4 do
  i = i + 1
  if i == 2 then
    total = total + 10
  elif i == 3 then
    total = total + 100
  else
    total = total + 1
  end
end
for k = 1, 4 do
  total = total + k
end
items = {1, 2, 3}
for index, item in items do
  total = total + item
end
n = 0
repeat
  n = n + 1
until n >= 3
alt = nil || 7
both = 2 && 3
negated = !nil
differs = 1 != 2
grade = 0
if total == 128 then
  grade = 1
elif total == 999 then
  grade = 2
else
  grade = 3
end"#;
    let (mut vm, root, _) = immediate_restart_vm();
    run_text_source(&mut vm, source);

    assert_eq!(vm.global(b"total").number(), Some(128.0));
    assert_eq!(vm.global(b"n").number(), Some(3.0));
    assert_eq!(vm.global(b"alt").number(), Some(7.0));
    assert_eq!(vm.global(b"both").number(), Some(3.0));
    assert!(matches!(vm.global(b"negated"), Value::Boolean(true)));
    assert!(matches!(vm.global(b"differs"), Value::Boolean(true)));
    assert_eq!(vm.global(b"grade").number(), Some(1.0));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn text_functions_unpack_multivalue_calls_and_forward_returns() {
    let source = br#"state = 0
def add(x)
  state = state + x
  return state
end
def guarded(x)
  local ok, value = pcall(add, x)
  if ok then
    result = value
  else
    result = -1
  end
end
def passthrough(x)
  return add(x)
end
def record(x)
  forwarded = passthrough(x)
end"#;
    let (mut vm, root, _) = immediate_restart_vm();
    run_text_source(&mut vm, source);

    call_text_global(&mut vm, b"guarded", vec![Value::Number(5.0)]);
    assert_eq!(vm.global(b"state").number(), Some(5.0));
    assert_eq!(vm.global(b"result").number(), Some(5.0));

    call_text_global(&mut vm, b"guarded", vec![Value::Number(2.0)]);
    assert_eq!(vm.global(b"state").number(), Some(7.0));
    assert_eq!(vm.global(b"result").number(), Some(7.0));

    // `return add(x)` keeps the call multiret through RETURN, then the
    // caller truncates it to the single `forwarded` slot.
    call_text_global(&mut vm, b"record", vec![Value::Number(3.0)]);
    assert_eq!(vm.global(b"state").number(), Some(10.0));
    assert_eq!(vm.global(b"forwarded").number(), Some(10.0));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn bitmap_load_star_clears_the_slot_and_out_of_range_indices_fault() {
    let (mut vm, root, _) = immediate_restart_vm();

    // Vendor MRF_BitmapLoad treats "*" as a slot clear that succeeds without
    // reading a file, even with zero dimensions; QQDSM2007 issues
    // BitmapLoad(1, "*", 0, 0, 0, 0, 0) when the left softkey opens the menu.
    let result = vm
        .call_value(
            Value::Native("BitmapLoad"),
            vec![
                Value::Number(1.0),
                bytes(b"*"),
                Value::Number(0.0),
                Value::Number(0.0),
                Value::Number(0.0),
                Value::Number(0.0),
                Value::Number(0.0),
            ],
            None,
            false,
        )
        .unwrap();
    let CallResult::Immediate(_) = result else {
        panic!("BitmapLoad must return immediately");
    };

    // Indices past BITMAPMAX fault with the vendor error text.
    let error = vm
        .call_value(
            Value::Native("BitmapLoad"),
            vec![
                Value::Number(31.0),
                bytes(b"roll.bmp"),
                Value::Number(0.0),
                Value::Number(0.0),
                Value::Number(1.0),
                Value::Number(1.0),
                Value::Number(0.0),
            ],
            None,
            false,
        )
        .err()
        .expect("BitmapLoad must fault on an out-of-range index");
    assert!(
        error.to_string().contains("BitmapLoad:index 31 invalid!"),
        "unexpected error: {error}"
    );

    std::fs::remove_dir_all(root).unwrap();
}
