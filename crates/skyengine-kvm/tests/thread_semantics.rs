//! java/lang/Thread 绿线程语义测试（T033）：spawn 链 + 让出协议。
//!
//! 自组装夹具（cs/heap/monitors/natives/scheduler/host 全件，不经
//! `vm::Vm` 门面——门面的 host 装箱后测试无法推进 mock 时钟，
//! sleep/join 用例需要逐轮 advance_ms）。
//!
//! 覆盖：Runnable start 后 run 真执行、Thread 子类覆写 run、重复 start
//! 抛 IllegalThreadStateException、currentThread/getName（默认名
//! "Thread-N" 与指定名）、sleep 让出后从下一条指令续跑（pc 不回退）、
//! join（2ms 轮询版）等待语义、isAlive 两态、优先级钳制。
//!
//! 字节码铁律：池索引先取号再构码；带参构造 NEW+DUP+参数+INVOKESPECIAL；
//! 分支消耗条件值后不再 POP。

#[allow(dead_code)]
mod common;

use common::classbytes::{build_class, provider, FieldSpec, HandlerSpec, MethodSpec, Pool, ROOT};

use skyengine_kvm::cldc::thread as jt;
use skyengine_kvm::cldc::util as ju;
use skyengine_kvm::host::mock::MockHostContext;
use skyengine_kvm::host::HostContext;
use skyengine_kvm::interp::natives::RegistryDispatch;
use skyengine_kvm::interp::{enter, Machine};
use skyengine_kvm::runtime::classspace::ClassSpace;
use skyengine_kvm::runtime::heap::Heap;
use skyengine_kvm::runtime::monitor::Monitors;
use skyengine_kvm::runtime::{Slot, Sym};
use skyengine_kvm::scheduler::Scheduler;

// ---- 操作码 ----------------------------------------------------------------

const ICONST_1: u8 = 0x04;
const ICONST_2: u8 = 0x05;
const BIPUSH: u8 = 0x10;
const LDC2_W: u8 = 0x14;
const ALOAD_0: u8 = 0x2a;
const ASTORE_0: u8 = 0x4b;
const IADD: u8 = 0x60;
const POP: u8 = 0x57;
const DUP: u8 = 0x59;
const GOTO: u8 = 0xa7;
const GETSTATIC: u8 = 0xb2;
const PUTSTATIC: u8 = 0xb3;
const INVOKEVIRTUAL: u8 = 0xb6;
const INVOKESPECIAL: u8 = 0xb7;
const INVOKESTATIC: u8 = 0xb8;
const NEW: u8 = 0xbb;
const RETURN: u8 = 0xb1;
const IFNULL: u8 = 0xc6;
const MONITORENTER: u8 = 0xc2;
const MONITOREXIT: u8 = 0xc3;

fn op3(op: u8, index: u16) -> [u8; 3] {
    let [hi, lo] = index.to_be_bytes();
    [op, hi, lo]
}

fn branch(op: u8, offset: i16) -> [u8; 3] {
    let [hi, lo] = offset.to_be_bytes();
    [op, hi, lo]
}

// ---- 夹具 ------------------------------------------------------------------

/// 全件测试 VM：与 vm::Vm 同构的组成（scheduler 供绿线程轮转），
/// host 由测试持有以逐轮推进 mock 时钟（sleep/join 唤醒源）。
struct Fixture {
    cs: ClassSpace,
    heap: Heap,
    monitors: Monitors,
    natives: RegistryDispatch,
    host: MockHostContext,
    scheduler: Scheduler,
}

impl Fixture {
    fn new(user: Vec<(&'static str, Vec<u8>)>) -> Self {
        let entries = user
            .into_iter()
            .map(|(name, bytes)| (name.to_string(), bytes))
            .collect();
        let mut cs = ClassSpace::new(Box::new(provider(entries)));
        let object = cs.load_class(ROOT).expect("load Object");
        cs.object_class = Some(object);
        cs.load_class("java/lang/String").expect("load String");
        // 重复 start 用例的处理器表按类名匹配（预载）。
        cs.load_class("java/lang/IllegalThreadStateException")
            .expect("load IllegalThreadStateException");
        Self {
            cs,
            heap: Heap::new(),
            monitors: Monitors::new(),
            natives: RegistryDispatch,
            host: MockHostContext::new(240, 320),
            scheduler: Scheduler::new(),
        }
    }

    /// 压 main 入口帧并 spawn 主线程（登记生命周期表；名字 "main"）。
    fn spawn_main(&mut self) {
        let class = self.cs.load_class("T").expect("load T");
        let mut frames = Vec::new();
        {
            // 字段级分离借用（五个组成部分互不相交，编译器可证）。
            let mut machine = Machine {
                cs: &mut self.cs,
                heap: &mut self.heap,
                monitors: &mut self.monitors,
                natives: &mut self.natives,
                host: &mut self.host,
            };
            enter(&mut machine, &mut frames, class, "main", "()V", Vec::new())
                .expect("enter main");
        }
        let id = self.scheduler.spawn(frames, "main");
        jt::register_thread_alive(&mut self.cs, id);
    }

    /// 驱动一轮：start/timer 请求队列消费 → 调度心跳 → 时钟 +1ms。
    fn drive_round(&mut self) {
        // now_ms 须在 host 被 Machine 可变借用前取完（同 vm.rs poll 段二）；
        // HostContext trait 对 dyn 对象暴露。
        let now = HostContext::now_ms(&mut self.host);
        {
            let mut machine = Machine {
                cs: &mut self.cs,
                heap: &mut self.heap,
                monitors: &mut self.monitors,
                natives: &mut self.natives,
                host: &mut self.host,
            };
            jt::spawn_pending(&mut machine, &mut self.scheduler).expect("spawn pending");
            // Timer 请求位消费（T034，同 Vm.poll 段零 b）。
            ju::pump_pending_timers(machine.cs, &mut self.scheduler);
            self.scheduler.poll(now, &mut machine, 200_000).expect("poll");
        }
        self.host.advance_ms(1);
    }

    /// 跑满 n 轮（无谓词驱动：main 结束后的收尾轮）。
    fn drive_rounds(&mut self, rounds: usize) {
        for _ in 0..rounds {
            self.drive_round();
        }
    }

    /// 驱动至谓词为真（每轮 +1ms 模拟时钟），上限 rounds 轮。
    fn drive_until(&mut self, rounds: usize, mut predicate: impl FnMut(&Fixture) -> bool) -> bool {
        for _ in 0..rounds {
            if predicate(self) {
                return true;
            }
            self.drive_round();
        }
        predicate(self)
    }

    /// 只读读 T 类静态 int 字段（drive_until 谓词用；未初始化为 0）。
    fn static_int_ro(&self, name: &str) -> Option<i32> {
        let class = self.cs.find("T")?;
        let name_sym = Sym(self.cs.syms.lookup(name)?);
        let desc_sym = Sym(self.cs.syms.lookup("I")?);
        let index = *self.cs.class(class).field_index.get(&(name_sym, desc_sym))?;
        let field = &self.cs.class(class).fields[index as usize];
        match self.cs.class(class).statics.get(field.slot as usize) {
            Some(Slot::Int(value)) => Some(*value),
            _ => None,
        }
    }

    /// 只读读 T 类静态字符串字段（未写为 None）。
    fn static_str_ro(&self, name: &str, desc: &str) -> Option<String> {
        let class = self.cs.find("T")?;
        let name_sym = Sym(self.cs.syms.lookup(name)?);
        let desc_sym = Sym(self.cs.syms.lookup(desc)?);
        let index = *self.cs.class(class).field_index.get(&(name_sym, desc_sym))?;
        let field = &self.cs.class(class).fields[index as usize];
        match self.cs.class(class).statics.get(field.slot as usize) {
            Some(Slot::Ref(reference)) => self
                .heap
                .str_obj(*reference)
                .ok()
                .map(|s| String::from_utf16_lossy(&s.chars)),
            _ => None,
        }
    }

    /// 读 T 类静态 int 字段（观测面）。
    fn static_int(&mut self, name: &str) -> i32 {
        let class = self.cs.find("T").expect("find T");
        let name_sym = self.cs.syms.intern(name);
        let desc_sym = self.cs.syms.intern("I");
        let field = self
            .cs
            .find_field(class, name_sym, desc_sym)
            .expect("find field");
        match self.cs.class(class).statics[field.slot as usize] {
            Slot::Int(value) => value,
            other => panic!("static {name} expected int, got {other:?}"),
        }
    }

    /// 读 T 类静态引用字段的字符串内容（None = null/非串）。
    fn static_str(&mut self, name: &str, desc: &str) -> Option<String> {
        let class = self.cs.find("T").expect("find T");
        let name_sym = self.cs.syms.intern(name);
        let desc_sym = self.cs.syms.intern(desc);
        let field = self
            .cs
            .find_field(class, name_sym, desc_sym)
            .expect("find field");
        match self.cs.class(class).statics[field.slot as usize] {
            Slot::Ref(reference) => self
                .heap
                .str_obj(reference)
                .ok()
                .map(|s| String::from_utf16_lossy(&s.chars)),
            other => panic!("static {name} expected ref, got {other:?}"),
        }
    }
}

// ---- 用户类构码 ------------------------------------------------------------

/// Runnable 实现类 R：run 里按 `body` 附加逻辑（计数/自报名等）。
/// 附加段统一以 GETSTATIC T.count I / ICONST_1 / IADD / PUTSTATIC 开头
/// （count += 1，标记 run 已进入）。
fn runnable_class(
    pool: &mut Pool,
    extra: Vec<u8>,
    stack: u16,
) -> (&'static str, Vec<u8>) {
    let count_get = op3(GETSTATIC, pool.fieldref("T", "count", "I"));
    let count_put = op3(PUTSTATIC, pool.fieldref("T", "count", "I"));
    let one = [ICONST_1];
    let add = [IADD];
    let mut code = Vec::new();
    code.extend_from_slice(&count_get);
    code.extend_from_slice(&one);
    code.extend_from_slice(&add);
    code.extend_from_slice(&count_put);
    code.extend_from_slice(&extra);
    code.push(RETURN);
    let object_init = op3(INVOKESPECIAL, pool.methodref(ROOT, "<init>", "()V"));
    let init_code = {
        let mut bytes = Vec::new();
        bytes.push(ALOAD_0);
        bytes.extend_from_slice(&object_init);
        bytes.push(RETURN);
        bytes
    };
    (
        "R",
        build_class(
            pool,
            "R",
            Some(ROOT),
            &["java/lang/Runnable"],
            &[],
            &[
                MethodSpec {
                    access: 0x0001,
                    name: "<init>",
                    desc: "()V",
                    code: Some((1, 1, init_code)),
                    handlers: Vec::new(),
                },
                MethodSpec {
                    access: 0x0001,
                    name: "run",
                    desc: "()V",
                    code: Some((stack, 1, code)),
                    handlers: Vec::new(),
                },
            ],
        ),
    )
}

/// Thread 子类 SubT：覆写 run（计数）。
fn subclass_class(pool: &mut Pool) -> (&'static str, Vec<u8>) {
    let count_get = op3(GETSTATIC, pool.fieldref("T", "count", "I"));
    let count_put = op3(PUTSTATIC, pool.fieldref("T", "count", "I"));
    let thread_init = pool.methodref("java/lang/Thread", "<init>", "()V");
    (
        "SubT",
        build_class(
            pool,
            "SubT",
            Some("java/lang/Thread"),
            &[],
            &[],
            &[
                MethodSpec {
                    access: 0x0001,
                    name: "<init>",
                    desc: "()V",
                    code: Some((1, 1, {
                        let init = op3(INVOKESPECIAL, thread_init);
                        let mut bytes = Vec::new();
                        bytes.push(ALOAD_0);
                        bytes.extend_from_slice(&init);
                        bytes.push(RETURN);
                        bytes
                    })),
                    handlers: Vec::new(),
                },
                MethodSpec {
                    access: 0x0001,
                    name: "run",
                    desc: "()V",
                    code: Some((2, 1, {
                        let mut bytes = Vec::new();
                        bytes.extend_from_slice(&count_get);
                        bytes.push(ICONST_1);
                        bytes.push(IADD);
                        bytes.extend_from_slice(&count_put);
                        bytes.push(RETURN);
                        bytes
                    })),
                    handlers: Vec::new(),
                },
            ],
        ),
    )
}

/// 主类 T：静态观测字段 + 指定 main 字节码。
fn main_class(pool: &mut Pool, main_code: Vec<u8>, stack: u16, locals: u16) -> (&'static str, Vec<u8>) {
    (
        "T",
        build_class(
            pool,
            "T",
            Some(ROOT),
            &[],
            &[
                FieldSpec { access: 0x0009, name: "count", desc: "I", constant: None },
                FieldSpec { access: 0x0009, name: "woke", desc: "I", constant: None },
                FieldSpec { access: 0x0009, name: "after", desc: "I", constant: None },
                FieldSpec { access: 0x0009, name: "flag", desc: "I", constant: None },
                FieldSpec { access: 0x0009, name: "preAlive", desc: "I", constant: None },
                FieldSpec { access: 0x0009, name: "postAlive", desc: "I", constant: None },
                FieldSpec { access: 0x0009, name: "selfAlive", desc: "I", constant: None },
                FieldSpec { access: 0x0009, name: "prio", desc: "I", constant: None },
                FieldSpec { access: 0x0009, name: "prioClamped", desc: "I", constant: None },
                FieldSpec { access: 0x0009, name: "mainName", desc: "Ljava/lang/String;", constant: None },
                FieldSpec { access: 0x0009, name: "workerName", desc: "Ljava/lang/String;", constant: None },
            ],
            &[MethodSpec {
                access: 0x0008,
                name: "main",
                desc: "()V",
                code: Some((stack, locals, main_code)),
                handlers: Vec::new(),
            }],
        ),
    )
}

/// new Thread(new R()) 构码（不含 start；14 字节，结束栈 [t]）。
/// join/double_start/isAlive 等需持有引用的用例以 ASTORE_0 接续。
fn code_new_thread_no_start(pool: &mut Pool) -> Vec<u8> {
    let r_init = op3(INVOKESPECIAL, pool.methodref("R", "<init>", "()V"));
    let t_init = op3(
        INVOKESPECIAL,
        pool.methodref("java/lang/Thread", "<init>", "(Ljava/lang/Runnable;)V"),
    );
    // 栈形：t, t, r, r → R.<init> 弹 r → Thread.<init> 弹 t/r → [t]。
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&op3(NEW, pool.class("java/lang/Thread")));
    bytes.push(DUP);
    bytes.extend_from_slice(&op3(NEW, pool.class("R")));
    bytes.push(DUP);
    bytes.extend_from_slice(&r_init);
    bytes.extend_from_slice(&t_init);
    bytes
}

/// new Thread(new R()) → start() 的构码（结束栈空，接 RETURN）。
fn code_new_thread_with_r(pool: &mut Pool) -> Vec<u8> {
    let mut bytes = code_new_thread_no_start(pool);
    bytes.extend_from_slice(&op3(
        INVOKEVIRTUAL,
        pool.methodref("java/lang/Thread", "start", "()V"),
    ));
    bytes
}

// ---- 用例 ------------------------------------------------------------------

/// Runnable 版：start 后 poll 若干轮，run 真执行（count==1）。
#[test]
fn start_runs_runnable() {
    let mut pool = Pool::new();
    let main = {
        let mut code = code_new_thread_with_r(&mut pool);
        code.push(RETURN);
        code
    };
    let t = main_class(&mut pool, main, 6, 0);
    let r = runnable_class(&mut pool, Vec::new(), 3);
    let mut fx = Fixture::new(vec![t, r]);
    fx.spawn_main();
    assert!(
        fx.drive_until(20, |fx| fx.static_int_ro("count") == Some(1)),
        "工作线程应在若干轮内跑完 run"
    );
    assert_eq!(fx.static_int("count"), 1, "run 计数应为 1（恰好执行一次）");
}

/// Thread 子类覆写 run：receiver=Thread 实例路径。
#[test]
fn start_runs_subclass_override() {
    let mut pool = Pool::new();
    let main = {
        let sub_init = op3(INVOKESPECIAL, pool.methodref("SubT", "<init>", "()V"));
        let start = op3(INVOKEVIRTUAL, pool.methodref("java/lang/Thread", "start", "()V"));
        let mut code = Vec::new();
        code.extend_from_slice(&op3(NEW, pool.class("SubT")));
        code.push(DUP);
        code.extend_from_slice(&sub_init);
        code.extend_from_slice(&start);
        code.push(RETURN);
        code
    };
    let t = main_class(&mut pool, main, 3, 0);
    let sub = subclass_class(&mut pool);
    let mut fx = Fixture::new(vec![t, sub]);
    fx.spawn_main();
    fx.drive_rounds(20);
    assert_eq!(fx.static_int("count"), 1, "子类覆写 run 应被调度执行");
}

/// 重复 start → IllegalThreadStateException（Java 侧捕获后置 flag=2）。
#[test]
fn double_start_throws() {
    let mut pool = Pool::new();
    // 构码（pc 布局对齐异常表）：
    // 0..14 new Thread(new R())；14 ASTORE_0；15..23 两次 start；
    // 23..27 flag=1 + GOTO 35；30..34 handler：POP + flag=2；35 RETURN。
    let base = code_new_thread_no_start(&mut pool);
    assert_eq!(base.len(), 14, "前置构码长度需与异常表 pc 对齐");
    let start = op3(INVOKEVIRTUAL, pool.methodref("java/lang/Thread", "start", "()V"));
    let flag_put = op3(PUTSTATIC, pool.fieldref("T", "flag", "I"));
    let mut code = base; // 0..14：栈 [t]
    code.push(ASTORE_0); // 14
    code.push(ALOAD_0); // 15
    code.extend_from_slice(&start); // 16..19 第一次 start
    code.push(ALOAD_0); // 19
    code.extend_from_slice(&start); // 20..23 第二次 start：抛出点
    code.push(ICONST_1); // 23
    code.extend_from_slice(&flag_put); // 24..27
    code.extend_from_slice(&branch(GOTO, 8)); // 27..30 → 35
    code.push(POP); // 30 handler：处理器入口异常对象在栈顶
    code.push(ICONST_2); // 31
    code.extend_from_slice(&flag_put); // 32..35
    code.push(RETURN); // 35
    let handlers = vec![HandlerSpec {
        start_pc: 14,
        end_pc: 23,
        handler_pc: 30,
        catch_class: Some("java/lang/IllegalThreadStateException"),
    }];
    let t_bytes = build_class(
        &mut pool,
        "T",
        Some(ROOT),
        &[],
        &[
            FieldSpec { access: 0x0009, name: "count", desc: "I", constant: None },
            FieldSpec { access: 0x0009, name: "flag", desc: "I", constant: None },
        ],
        &[MethodSpec {
            access: 0x0008,
            name: "main",
            desc: "()V",
            code: Some((6, 1, code)),
            handlers,
        }],
    );
    let r = runnable_class(&mut pool, Vec::new(), 3);
    let mut fx = Fixture::new(vec![("T", t_bytes), r]);
    fx.spawn_main();
    fx.drive_rounds(20);
    assert_eq!(fx.static_int("flag"), 2, "第二次 start 应抛并被捕获（flag=2）");
}

/// currentThread：主线程非 null 名 "main"；工作线程默认名 "Thread-0"。
#[test]
fn current_thread_and_names() {
    let mut pool = Pool::new();
    let current = op3(
        INVOKESTATIC,
        pool.methodref("java/lang/Thread", "currentThread", "()Ljava/lang/Thread;"),
    );
    let get_name = op3(
        INVOKEVIRTUAL,
        pool.methodref("java/lang/Thread", "getName", "()Ljava/lang/String;"),
    );
    let main_name_put = op3(PUTSTATIC, pool.fieldref("T", "mainName", "Ljava/lang/String;"));
    // main: currentThread → astore_0 → ifnull fail → getName → putstatic
    //       → new Thread(new R()).start()（驱动 workerName）→ return
    let spawned = code_new_thread_with_r(&mut pool); // 17 字节
    let mut main = Vec::new();
    main.extend_from_slice(&current); // 0..3
    main.push(ASTORE_0); // 3
    main.push(ALOAD_0); // 4
    main.extend_from_slice(&branch(IFNULL, 10)); // 5..8 → 目标 15
    main.push(ALOAD_0); // 8
    main.extend_from_slice(&get_name); // 9..12
    main.extend_from_slice(&main_name_put); // 12..15
    main.extend_from_slice(&spawned); // 15..32
    main.push(RETURN); // 32（fail 目标 = 15 → 直接 start/return）
    let t = main_class(&mut pool, main, 2, 1);
    // R.run: currentThread().getName() → putstatic T.workerName
    let worker_name_put = op3(PUTSTATIC, pool.fieldref("T", "workerName", "Ljava/lang/String;"));
    let mut extra = Vec::new();
    extra.extend_from_slice(&current);
    extra.extend_from_slice(&get_name);
    extra.extend_from_slice(&worker_name_put);
    let r = runnable_class(&mut pool, extra, 2);
    let mut fx = Fixture::new(vec![t, r]);
    fx.spawn_main();
    let mut started = false;
    assert!(fx.drive_until(20, |fx| {
        if !started && fx.static_str_ro("mainName", "Ljava/lang/String;").is_some() {
            started = true;
        }
        started && fx.static_str_ro("workerName", "Ljava/lang/String;").is_some()
    }));
    assert_eq!(
        fx.static_str("mainName", "Ljava/lang/String;").as_deref(),
        Some("main"),
        "主线程 Thread 对象懒建且名为 main"
    );
    assert_eq!(
        fx.static_str("workerName", "Ljava/lang/String;").as_deref(),
        Some("Thread-0"),
        "无名构造默认名 Thread-N，N 从 0 起"
    );
}

/// 指定名构造：new Thread(r, "worker-7")。
#[test]
fn named_thread() {
    let mut pool = Pool::new();
    let name_index = pool.string("worker-7");
    let r_init = op3(INVOKESPECIAL, pool.methodref("R", "<init>", "()V"));
    let t_init = op3(
        INVOKESPECIAL,
        pool.methodref(
            "java/lang/Thread",
            "<init>",
            "(Ljava/lang/Runnable;Ljava/lang/String;)V",
        ),
    );
    let start = op3(INVOKEVIRTUAL, pool.methodref("java/lang/Thread", "start", "()V"));
    let current = op3(
        INVOKESTATIC,
        pool.methodref("java/lang/Thread", "currentThread", "()Ljava/lang/Thread;"),
    );
    let get_name = op3(
        INVOKEVIRTUAL,
        pool.methodref("java/lang/Thread", "getName", "()Ljava/lang/String;"),
    );
    let worker_name_put = op3(PUTSTATIC, pool.fieldref("T", "workerName", "Ljava/lang/String;"));
    let mut main = Vec::new();
    main.extend_from_slice(&op3(NEW, pool.class("R")));
    main.push(DUP);
    main.extend_from_slice(&r_init);
    main.extend_from_slice(&op3(NEW, pool.class("java/lang/Thread")));
    main.push(DUP);
    // 栈 [t, t]；下面压 r 与名字成 [t, t, r, "worker-7"]
    main.extend_from_slice(&op3(NEW, pool.class("R")));
    main.push(DUP);
    main.extend_from_slice(&r_init);
    // ldc（0x12）：池索引单字节，防御校验 <256。
    assert!(name_index < 256, "ldc 池索引需 <256");
    main.push(0x12);
    main.push(name_index as u8);
    main.extend_from_slice(&t_init);
    main.extend_from_slice(&start);
    main.push(RETURN);
    let t = main_class(&mut pool, main, 6, 0);
    let mut extra = Vec::new();
    extra.extend_from_slice(&current);
    extra.extend_from_slice(&get_name);
    extra.extend_from_slice(&worker_name_put);
    let r = runnable_class(&mut pool, extra, 2);
    let mut fx = Fixture::new(vec![t, r]);
    fx.spawn_main();
    assert!(fx.drive_until(20, |fx| {
        fx.static_str_ro("workerName", "Ljava/lang/String;").is_some()
    }));
    assert_eq!(
        fx.static_str("workerName", "Ljava/lang/String;").as_deref(),
        Some("worker-7")
    );
}

/// sleep(5)：让出后工作线程应从 sleep 的下一条指令续跑（woke==1），
/// 同时证明 pc 未回退（回退将反复 yield，woke 永不置位）。
#[test]
fn sleep_yields_and_resumes() {
    let mut pool = Pool::new();
    let main = {
        let mut code = code_new_thread_with_r(&mut pool);
        code.push(RETURN);
        code
    };
    let t = main_class(&mut pool, main, 6, 0);
    let five = pool.long(5);
    let sleep = op3(INVOKESTATIC, pool.methodref("java/lang/Thread", "sleep", "(J)V"));
    let woke_put = op3(PUTSTATIC, pool.fieldref("T", "woke", "I"));
    let mut extra = Vec::new();
    extra.extend_from_slice(&op3(LDC2_W, five));
    extra.extend_from_slice(&sleep);
    extra.push(ICONST_1);
    extra.extend_from_slice(&woke_put);
    let r = runnable_class(&mut pool, extra, 4);
    let mut fx = Fixture::new(vec![t, r]);
    fx.spawn_main();
    assert!(
        fx.drive_until(30, |fx| fx.static_int_ro("woke") == Some(1)),
        "sleep(5) 应在时钟越过唤醒点后从下一条指令续跑"
    );
    assert_eq!(fx.static_int("count"), 1);
}

/// join（轮询版）：主线程 join 工作线程后，其 run 计数已就绪。
#[test]
fn join_waits_for_completion() {
    let mut pool = Pool::new();
    let base = code_new_thread_no_start(&mut pool);
    assert_eq!(base.len(), 14);
    let join = op3(INVOKEVIRTUAL, pool.methodref("java/lang/Thread", "join", "()V"));
    let count_get = op3(GETSTATIC, pool.fieldref("T", "count", "I"));
    let after_put = op3(PUTSTATIC, pool.fieldref("T", "after", "I"));
    let start = op3(INVOKEVIRTUAL, pool.methodref("java/lang/Thread", "start", "()V"));
    let mut code = base; // 0..14：栈 [t]
    code.push(ASTORE_0); // 14：t 存局部 0
    code.push(ALOAD_0);
    code.extend_from_slice(&start); // 15..18
    code.push(ALOAD_0);
    code.extend_from_slice(&join); // 19..23
    code.extend_from_slice(&count_get);
    code.extend_from_slice(&after_put); // 快照 join 返回时的 count
    code.push(RETURN);
    let t_bytes = build_class(
        &mut pool,
        "T",
        Some(ROOT),
        &[],
        &[
            FieldSpec { access: 0x0009, name: "count", desc: "I", constant: None },
            FieldSpec { access: 0x0009, name: "after", desc: "I", constant: None },
        ],
        &[MethodSpec {
            access: 0x0008,
            name: "main",
            desc: "()V",
            code: Some((6, 1, code)),
            handlers: Vec::new(),
        }],
    );
    let r = runnable_class(&mut pool, Vec::new(), 3);
    let mut fx = Fixture::new(vec![("T", t_bytes), r]);
    fx.spawn_main();
    assert!(
        fx.drive_until(40, |fx| fx.static_int_ro("after") == Some(1)),
        "join 返回时目标 run 应已跑完（after 快照 count）"
    );
}

/// isAlive 两态：未 start=false；run 中=true；join 后=false。
#[test]
fn is_alive_transitions() {
    let mut pool = Pool::new();
    let is_alive = op3(INVOKEVIRTUAL, pool.methodref("java/lang/Thread", "isAlive", "()Z"));
    let join = op3(INVOKEVIRTUAL, pool.methodref("java/lang/Thread", "join", "()V"));
    let pre_put = op3(PUTSTATIC, pool.fieldref("T", "preAlive", "I"));
    let post_put = op3(PUTSTATIC, pool.fieldref("T", "postAlive", "I"));
    let current = op3(
        INVOKESTATIC,
        pool.methodref("java/lang/Thread", "currentThread", "()Ljava/lang/Thread;"),
    );
    let self_put = op3(PUTSTATIC, pool.fieldref("T", "selfAlive", "I"));
    let mut code = Vec::new();
    // new Thread(new R()) 存 t（不起动），先查 preAlive。
    let base = code_new_thread_no_start(&mut pool);
    code.extend_from_slice(&base); // 0..14：栈 [t]
    code.push(ASTORE_0);
    // 未 start：isAlive==0
    code.push(ALOAD_0);
    code.extend_from_slice(&is_alive);
    code.extend_from_slice(&pre_put);
    // start → join → isAlive==0
    code.push(ALOAD_0);
    code.extend_from_slice(&op3(INVOKEVIRTUAL, pool.methodref("java/lang/Thread", "start", "()V")));
    code.push(ALOAD_0);
    code.extend_from_slice(&join);
    code.push(ALOAD_0);
    code.extend_from_slice(&is_alive);
    code.extend_from_slice(&post_put);
    code.push(RETURN);
    let t_bytes = build_class(
        &mut pool,
        "T",
        Some(ROOT),
        &[],
        &[
            FieldSpec { access: 0x0009, name: "count", desc: "I", constant: None },
            FieldSpec { access: 0x0009, name: "preAlive", desc: "I", constant: None },
            FieldSpec { access: 0x0009, name: "postAlive", desc: "I", constant: None },
            FieldSpec { access: 0x0009, name: "selfAlive", desc: "I", constant: None },
        ],
        &[MethodSpec {
            access: 0x0008,
            name: "main",
            desc: "()V",
            code: Some((6, 1, code)),
            handlers: Vec::new(),
        }],
    );
    // R.run：count++ 后 currentThread().isAlive() 快照。
    let mut extra = Vec::new();
    extra.extend_from_slice(&current);
    extra.extend_from_slice(&is_alive);
    extra.extend_from_slice(&self_put);
    let r = runnable_class(&mut pool, extra, 2);
    let mut fx = Fixture::new(vec![("T", t_bytes), r]);
    fx.spawn_main();
    assert!(
        fx.drive_until(
            40,
            |fx| fx.static_int_ro("postAlive").is_some() && fx.static_int_ro("count") == Some(1),
        ),
        "join 后应能继续执行到 main 尾（postAlive 已写、run 计数完成）"
    );
    assert_eq!(fx.static_int("preAlive"), 0, "未 start → isAlive false");
    assert_eq!(fx.static_int("selfAlive"), 1, "run 执行中 → isAlive true");
    assert_eq!(fx.static_int("postAlive"), 0, "join 返回（线程死）→ isAlive false");
}

/// 优先级：setPriority(9) 与越界钳制（99→10）。
#[test]
fn priority_clamped() {
    let mut pool = Pool::new();
    let set_prio = op3(INVOKEVIRTUAL, pool.methodref("java/lang/Thread", "setPriority", "(I)V"));
    let get_prio = op3(INVOKEVIRTUAL, pool.methodref("java/lang/Thread", "getPriority", "()I"));
    let prio_put = op3(PUTSTATIC, pool.fieldref("T", "prio", "I"));
    let clamped_put = op3(PUTSTATIC, pool.fieldref("T", "prioClamped", "I"));
    let thread_init = op3(INVOKESPECIAL, pool.methodref("java/lang/Thread", "<init>", "()V"));
    // INVOKEVIRTUAL 会消费 this：实例存局部 0，每段调用前 ALOAD_0 重压。
    let mut code = Vec::new();
    code.extend_from_slice(&op3(NEW, pool.class("java/lang/Thread")));
    code.push(DUP);
    code.extend_from_slice(&thread_init); // 栈 [t]
    code.push(ASTORE_0);
    // setPriority(9) → getPriority → prio
    code.push(ALOAD_0);
    code.push(BIPUSH);
    code.push(9);
    code.extend_from_slice(&set_prio);
    code.push(ALOAD_0);
    code.extend_from_slice(&get_prio);
    code.extend_from_slice(&prio_put);
    // setPriority(99) → getPriority → prioClamped（钳到 10）
    code.push(ALOAD_0);
    code.push(BIPUSH);
    code.push(99);
    code.extend_from_slice(&set_prio);
    code.push(ALOAD_0);
    code.extend_from_slice(&get_prio);
    code.extend_from_slice(&clamped_put);
    code.push(RETURN);
    let t = main_class(&mut pool, code, 3, 1);
    let r = runnable_class(&mut pool, Vec::new(), 3);
    let mut fx = Fixture::new(vec![t, r]);
    fx.spawn_main();
    fx.drive_rounds(5);
    assert_eq!(fx.static_int("prio"), 9);
    assert_eq!(fx.static_int("prioClamped"), 10, "越界优先级钳制到 10");
}

// ---------------------------------------------------------------------------
// Timer 触发链（T034）：schedule → pending_timers 请求位 → 调度器定时
// 队列 → 到期惰性查取消 + fixed-rate 重排 + spawn run 线程。
// ---------------------------------------------------------------------------

/// TimerTask 子类 TT：覆写 run（count += 1），<init> 链到 bootstrap 桩。
fn timer_task_class(pool: &mut Pool) -> (&'static str, Vec<u8>) {
    let count_get = op3(GETSTATIC, pool.fieldref("T", "count", "I"));
    let count_put = op3(PUTSTATIC, pool.fieldref("T", "count", "I"));
    let task_init = pool.methodref("java/util/TimerTask", "<init>", "()V");
    (
        "TT",
        build_class(
            pool,
            "TT",
            Some("java/util/TimerTask"),
            &[],
            &[],
            &[
                MethodSpec {
                    access: 0x0001,
                    name: "<init>",
                    desc: "()V",
                    code: Some((1, 1, {
                        let init = op3(INVOKESPECIAL, task_init);
                        let mut bytes = Vec::new();
                        bytes.push(ALOAD_0);
                        bytes.extend_from_slice(&init);
                        bytes.push(RETURN);
                        bytes
                    })),
                    handlers: Vec::new(),
                },
                MethodSpec {
                    access: 0x0001,
                    name: "run",
                    desc: "()V",
                    code: Some((2, 1, {
                        let mut bytes = Vec::new();
                        bytes.extend_from_slice(&count_get);
                        bytes.push(ICONST_1);
                        bytes.push(IADD);
                        bytes.extend_from_slice(&count_put);
                        bytes.push(RETURN);
                        bytes
                    })),
                    handlers: Vec::new(),
                },
            ],
        ),
    )
}

/// Timer 类字段/方法引用速记（构码期间持池）。
struct TimerRefs {
    tt_class: u16,
    tt_init: u16,
    timer_class: u16,
    timer_init: u16,
    schedule_j: u16,
    schedule_jj: u16,
    task_cancel: u16,
}

fn timer_refs(pool: &mut Pool) -> TimerRefs {
    TimerRefs {
        tt_class: pool.class("TT"),
        tt_init: pool.methodref("TT", "<init>", "()V"),
        timer_class: pool.class("java/util/Timer"),
        timer_init: pool.methodref("java/util/Timer", "<init>", "()V"),
        schedule_j: pool.methodref("java/util/Timer", "schedule", "(Ljava/util/TimerTask;J)V"),
        schedule_jj: pool.methodref("java/util/Timer", "schedule", "(Ljava/util/TimerTask;JJ)V"),
        task_cancel: pool.methodref("java/util/TimerTask", "cancel", "()Z"),
    }
}

/// main 构码：new TT（task → 局部 0）→ new Timer（→ 局部 1）→
/// aload_1, aload_0, delay[, period] → schedule；再接 after 尾码。
fn code_timer_setup(
    pool: &mut Pool,
    refs: &TimerRefs,
    delay: i64,
    period: Option<i64>,
    after: Vec<u8>,
) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&op3(NEW, refs.tt_class));
    bytes.push(DUP);
    bytes.extend_from_slice(&op3(INVOKESPECIAL, refs.tt_init));
    bytes.push(ASTORE_0);
    bytes.extend_from_slice(&op3(NEW, refs.timer_class));
    bytes.push(DUP);
    bytes.extend_from_slice(&op3(INVOKESPECIAL, refs.timer_init));
    bytes.push(0x4c); // ASTORE_1
    bytes.push(0x2b); // ALOAD_1（timer 作 this）
    bytes.push(ALOAD_0); // task
    bytes.extend_from_slice(&op3(LDC2_W, pool.long(delay)));
    if let Some(p) = period {
        bytes.extend_from_slice(&op3(LDC2_W, pool.long(p)));
        bytes.extend_from_slice(&op3(INVOKEVIRTUAL, refs.schedule_jj));
    } else {
        bytes.extend_from_slice(&op3(INVOKEVIRTUAL, refs.schedule_j));
    }
    bytes.extend_from_slice(&after);
    bytes
}

/// 一次性任务：schedule(task, 100) → t=100 触发恰好一次；
/// run 线程完成后消亡（alive_count 归零，isAlive 面）。
#[test]
fn timer_one_shot_runs_once() {
    let mut pool = Pool::new();
    let refs = timer_refs(&mut pool);
    let main_code = code_timer_setup(&mut pool, &refs, 100, None, vec![RETURN]);
    let t = main_class(&mut pool, main_code, 6, 2);
    let tt = timer_task_class(&mut pool);
    let mut fx = Fixture::new(vec![t, tt]);
    fx.spawn_main();
    assert!(
        fx.drive_until(130, |fx| fx.static_int_ro("count") == Some(1)),
        "t=100 到期后 run 应恰好执行一次"
    );
    // 再推进 100ms：一次性任务不得重触发。
    fx.drive_rounds(100);
    assert_eq!(fx.static_int("count"), 1, "无 period 的一次性任务不重排");
    // main 与 timer-task 线程均应消亡（isAlive 生命周期面）。
    fx.drive_rounds(5);
    assert_eq!(fx.scheduler.alive_count(), 0, "run 完成线程应消亡");
}

/// 周期任务（fixed-rate 简化）：schedule(task, 100, 100) →
/// 350ms 处恰触发 3 次（100/200/300），400ms 第 4 次。
#[test]
fn timer_periodic_fixed_rate() {
    let mut pool = Pool::new();
    let refs = timer_refs(&mut pool);
    let main_code = code_timer_setup(&mut pool, &refs, 100, Some(100), vec![RETURN]);
    let t = main_class(&mut pool, main_code, 8, 2);
    let tt = timer_task_class(&mut pool);
    let mut fx = Fixture::new(vec![t, tt]);
    fx.spawn_main();
    fx.drive_rounds(350);
    assert_eq!(fx.static_int("count"), 3, "100/200/300ms 三次触发");
    fx.drive_rounds(60); // 时钟越过后一轮才 fire（now 到 400）
    assert_eq!(fx.static_int("count"), 4, "第 4 个周期到点再触发");
}

/// cancel 后不再触发：schedule(task, 100, 100) → 立即 task.cancel()。
#[test]
fn timer_cancel_stops_firing() {
    let mut pool = Pool::new();
    let refs = timer_refs(&mut pool);
    let after = {
        let mut tail = Vec::new();
        tail.push(ALOAD_0);
        tail.extend_from_slice(&op3(INVOKEVIRTUAL, refs.task_cancel));
        tail.push(POP); // 弹 cancel 返回值
        tail.push(RETURN);
        tail
    };
    let main_code = code_timer_setup(&mut pool, &refs, 100, Some(100), after);
    let t = main_class(&mut pool, main_code, 8, 2);
    let tt = timer_task_class(&mut pool);
    let mut fx = Fixture::new(vec![t, tt]);
    fx.spawn_main();
    fx.drive_rounds(350);
    assert_eq!(fx.static_int("count"), 0, "已取消任务到期跳过且不重排");
}

// ---------------------------------------------------------------------------
// Object.wait/notify/notifyAll（T034）：monitorenter/exit + 让出协议 +
// 调度器 BlockedWait/reacquire 移交。锁对象经静态字段 T.lock 跨线程共享。
// ---------------------------------------------------------------------------

const LOCK_DESC: &str = "Ljava/lang/Object;";

/// 同步 run 体骨架：getstatic T.lock → astore_0 → monitorenter →
/// body（锁内）→ aload_0 → monitorexit → return。
fn code_sync_body(body: &[u8], lock_get: u16) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&op3(GETSTATIC, lock_get));
    bytes.push(ASTORE_0);
    bytes.push(ALOAD_0);
    bytes.push(MONITORENTER);
    bytes.extend_from_slice(body);
    bytes.push(ALOAD_0);
    bytes.push(MONITOREXIT);
    bytes.push(RETURN);
    bytes
}

/// 具名 Runnable 类（run = 指定码体，无计数副作用）。
fn plain_runnable_class(
    pool: &mut Pool,
    name: &'static str,
    run_code: Vec<u8>,
    stack: u16,
) -> (&'static str, Vec<u8>) {
    let object_init = op3(INVOKESPECIAL, pool.methodref(ROOT, "<init>", "()V"));
    (
        name,
        build_class(
            pool,
            name,
            Some(ROOT),
            &["java/lang/Runnable"],
            &[],
            &[
                MethodSpec {
                    access: 0x0001,
                    name: "<init>",
                    desc: "()V",
                    code: Some((1, 1, {
                        let mut bytes = Vec::new();
                        bytes.push(ALOAD_0);
                        bytes.extend_from_slice(&object_init);
                        bytes.push(RETURN);
                        bytes
                    })),
                    handlers: Vec::new(),
                },
                MethodSpec {
                    access: 0x0001,
                    name: "run",
                    desc: "()V",
                    code: Some((stack, 1, run_code)),
                    handlers: Vec::new(),
                },
            ],
        ),
    )
}

/// main 起手码：lock = new Object() → putstatic T.lock。
fn code_init_lock(pool: &mut Pool) -> Vec<u8> {
    let lock_put = op3(PUTSTATIC, pool.fieldref("T", "lock", LOCK_DESC));
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&op3(NEW, pool.class(ROOT)));
    bytes.push(DUP);
    bytes.extend_from_slice(&op3(INVOKESPECIAL, pool.methodref(ROOT, "<init>", "()V")));
    bytes.extend_from_slice(&lock_put);
    bytes
}

/// new Thread(new NAME()).start()（实例经局部 0 中转）。
fn code_start_runnable(pool: &mut Pool, runnable: &str) -> Vec<u8> {
    let r_init = op3(INVOKESPECIAL, pool.methodref(runnable, "<init>", "()V"));
    let t_init = op3(
        INVOKESPECIAL,
        pool.methodref("java/lang/Thread", "<init>", "(Ljava/lang/Runnable;)V"),
    );
    let start = op3(INVOKEVIRTUAL, pool.methodref("java/lang/Thread", "start", "()V"));
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&op3(NEW, pool.class(runnable)));
    bytes.push(DUP);
    bytes.extend_from_slice(&r_init);
    bytes.push(ASTORE_0);
    bytes.extend_from_slice(&op3(NEW, pool.class("java/lang/Thread")));
    bytes.push(DUP);
    bytes.push(ALOAD_0);
    bytes.extend_from_slice(&t_init);
    bytes.extend_from_slice(&start);
    bytes
}

/// T 主类（lock/flag/woke/after 静态观测字段 + 指定 main 体）。
fn observe_main_class(
    pool: &mut Pool,
    main_code: Vec<u8>,
    handlers: Vec<HandlerSpec>,
    stack: u16,
    locals: u16,
) -> (&'static str, Vec<u8>) {
    (
        "T",
        build_class(
            pool,
            "T",
            Some(ROOT),
            &[],
            &[
                FieldSpec { access: 0x0009, name: "lock", desc: LOCK_DESC, constant: None },
                FieldSpec { access: 0x0009, name: "flag", desc: "I", constant: None },
                FieldSpec { access: 0x0009, name: "woke", desc: "I", constant: None },
                FieldSpec { access: 0x0009, name: "after", desc: "I", constant: None },
            ],
            &[MethodSpec {
                access: 0x0008,
                name: "main",
                desc: "()V",
                code: Some((stack, locals, main_code)),
                handlers,
            }],
        ),
    )
}

/// 未持锁 wait → IllegalMonitorStateException（Java 侧捕获置 flag=2）。
#[test]
fn wait_without_lock_throws_imse() {
    let mut pool = Pool::new();
    let flag_put = op3(PUTSTATIC, pool.fieldref("T", "flag", "I"));
    let object_init = op3(INVOKESPECIAL, pool.methodref(ROOT, "<init>", "()V"));
    // pc 布局：0..3 NEW；3 DUP；4..7 <init>；7 ASTORE_0；8 ALOAD_0；
    // 9..12 wait（抛出点 ∈ [8,12)）；12 ICONST_1；13..16 flag=1；
    // 16..19 GOTO 24；19 POP；20 ICONST_2；21..24 flag=2；24 RETURN。
    let mut code = Vec::new();
    code.extend_from_slice(&op3(NEW, pool.class(ROOT)));
    code.push(DUP);
    code.extend_from_slice(&object_init);
    code.push(ASTORE_0);
    code.push(ALOAD_0);
    code.extend_from_slice(&op3(
        INVOKEVIRTUAL,
        pool.methodref("java/lang/Object", "wait", "()V"),
    ));
    code.push(ICONST_1);
    code.extend_from_slice(&flag_put);
    code.extend_from_slice(&branch(GOTO, 5));
    code.push(POP);
    code.push(ICONST_2);
    code.extend_from_slice(&flag_put);
    code.push(RETURN);
    let handlers = vec![HandlerSpec {
        start_pc: 8,
        end_pc: 12,
        handler_pc: 19,
        catch_class: Some("java/lang/IllegalMonitorStateException"),
    }];
    let t = observe_main_class(&mut pool, code, handlers, 4, 1);
    let mut fx = Fixture::new(vec![t]);
    fx.spawn_main();
    fx.drive_rounds(10);
    assert_eq!(fx.static_int("flag"), 2, "未持锁 wait 应抛 IMSE 且被捕获");
}

/// 双线程 wait/notify 汇合：消费者持锁 wait；生产者持锁置 flag=1 后
/// notify；消费者被移交唤醒（重持锁）后快照 flag。spawn 顺序即轮转
/// 顺序（消费者先入表 → 先执行到 wait，notify 必有等待者）。
#[test]
fn wait_notify_rendezvous() {
    let mut pool = Pool::new();
    let lock_field = pool.fieldref("T", "lock", LOCK_DESC);
    let wait_call = op3(INVOKEVIRTUAL, pool.methodref("java/lang/Object", "wait", "()V"));
    let notify_call = op3(INVOKEVIRTUAL, pool.methodref("java/lang/Object", "notify", "()V"));
    let flag_put = op3(PUTSTATIC, pool.fieldref("T", "flag", "I"));
    let flag_get = op3(GETSTATIC, pool.fieldref("T", "flag", "I"));
    let woke_put = op3(PUTSTATIC, pool.fieldref("T", "woke", "I"));
    // main：lock 初始化 → 起消费者 WC → 起生产者 WP → return。
    let mut main_code = code_init_lock(&mut pool);
    main_code.extend_from_slice(&code_start_runnable(&mut pool, "WC"));
    main_code.extend_from_slice(&code_start_runnable(&mut pool, "WP"));
    main_code.push(RETURN);
    // 消费者：synchronized(lock){ lock.wait(); woke = flag; }
    let consumer_body = {
        let mut body = Vec::new();
        body.push(ALOAD_0);
        body.extend_from_slice(&wait_call);
        body.extend_from_slice(&flag_get);
        body.extend_from_slice(&woke_put);
        body
    };
    let wc = plain_runnable_class(
        &mut pool,
        "WC",
        code_sync_body(&consumer_body, lock_field),
        4,
    );
    // 生产者：synchronized(lock){ flag = 1; lock.notify(); }
    let producer_body = {
        let mut body = Vec::new();
        body.push(ICONST_1);
        body.extend_from_slice(&flag_put);
        body.push(ALOAD_0);
        body.extend_from_slice(&notify_call);
        body
    };
    let wp = plain_runnable_class(
        &mut pool,
        "WP",
        code_sync_body(&producer_body, lock_field),
        4,
    );
    let t = observe_main_class(&mut pool, main_code, Vec::new(), 6, 1);
    let mut fx = Fixture::new(vec![t, wc, wp]);
    fx.spawn_main();
    assert!(
        fx.drive_until(60, |fx| fx.static_int_ro("woke") == Some(1)),
        "消费者应被 notify 唤醒并快照 flag"
    );
    assert_eq!(fx.static_int("flag"), 1);
    assert_eq!(fx.static_int("woke"), 1, "唤醒后应读到生产者写入的 flag");
}

/// notifyAll：两消费者先后 wait，生产者 notifyAll 后全醒（count==2）。
#[test]
fn notify_all_wakes_both_waiters() {
    let mut pool = Pool::new();
    let lock_field = pool.fieldref("T", "lock", LOCK_DESC);
    let wait_call = op3(INVOKEVIRTUAL, pool.methodref("java/lang/Object", "wait", "()V"));
    let notify_all = op3(INVOKEVIRTUAL, pool.methodref("java/lang/Object", "notifyAll", "()V"));
    let count_get = op3(GETSTATIC, pool.fieldref("T", "count", "I"));
    let count_put = op3(PUTSTATIC, pool.fieldref("T", "count", "I"));
    let mut main_code = code_init_lock(&mut pool);
    // 两个消费者实例（同类 CN）先入表，生产者最后。
    main_code.extend_from_slice(&code_start_runnable(&mut pool, "CN"));
    main_code.extend_from_slice(&code_start_runnable(&mut pool, "CN"));
    main_code.extend_from_slice(&code_start_runnable(&mut pool, "NP"));
    main_code.push(RETURN);
    // 消费者 CN：synchronized(lock){ lock.wait(); count += 1; }
    let consumer_body = {
        let mut body = Vec::new();
        body.push(ALOAD_0);
        body.extend_from_slice(&wait_call);
        body.extend_from_slice(&count_get);
        body.push(ICONST_1);
        body.push(IADD);
        body.extend_from_slice(&count_put);
        body
    };
    let cn = plain_runnable_class(&mut pool, "CN", code_sync_body(&consumer_body, lock_field), 4);
    // 生产者 NP：synchronized(lock){ lock.notifyAll(); }
    let producer_body = {
        let mut body = Vec::new();
        body.push(ALOAD_0);
        body.extend_from_slice(&notify_all);
        body
    };
    let np = plain_runnable_class(&mut pool, "NP", code_sync_body(&producer_body, lock_field), 4);
    let t_bytes = build_class(
        &mut pool,
        "T",
        Some(ROOT),
        &[],
        &[
            FieldSpec { access: 0x0009, name: "lock", desc: LOCK_DESC, constant: None },
            FieldSpec { access: 0x0009, name: "count", desc: "I", constant: None },
        ],
        &[MethodSpec {
            access: 0x0008,
            name: "main",
            desc: "()V",
            code: Some((6, 1, main_code)),
            handlers: Vec::new(),
        }],
    );
    let mut fx = Fixture::new(vec![("T", t_bytes), cn, np]);
    fx.spawn_main();
    assert!(
        fx.drive_until(80, |fx| fx.static_int_ro("count") == Some(2)),
        "notifyAll 应唤醒全部等待者"
    );
    assert_eq!(fx.static_int("count"), 2);
}

/// 超时 wait 自醒：单线程 synchronized(lock){ lock.wait(50); after=1; }
/// ——超时醒后经"重新持有监视器"移交路径续跑（monitorexit 不落 IMSE）。
#[test]
fn timed_wait_wakes_on_timeout() {
    let mut pool = Pool::new();
    let wait_timed = op3(INVOKEVIRTUAL, pool.methodref("java/lang/Object", "wait", "(J)V"));
    let after_put = op3(PUTSTATIC, pool.fieldref("T", "after", "I"));
    // main：lock 初始化 → 同步块内 wait(50) → after=1。
    let mut code = code_init_lock(&mut pool);
    code.extend_from_slice(&op3(GETSTATIC, pool.fieldref("T", "lock", LOCK_DESC)));
    code.push(ASTORE_0);
    code.push(ALOAD_0);
    code.push(MONITORENTER);
    code.push(ALOAD_0);
    code.extend_from_slice(&op3(LDC2_W, pool.long(50)));
    code.extend_from_slice(&wait_timed);
    code.push(ALOAD_0);
    code.push(MONITOREXIT);
    code.push(ICONST_1);
    code.extend_from_slice(&after_put);
    code.push(RETURN);
    let t = observe_main_class(&mut pool, code, Vec::new(), 6, 1);
    let mut fx = Fixture::new(vec![t]);
    fx.spawn_main();
    assert!(
        fx.drive_until(90, |fx| fx.static_int_ro("after") == Some(1)),
        "wait(50) 超时后应自醒并跑完同步块"
    );
    assert_eq!(fx.static_int("after"), 1);
}

// ===========================================================================
// T035 并发语义测试汇合：双向握手 / join 链 / notify FIFO / Timer 边界与
// fixed-rate 无漂移 / 交叉锁防饿死 / interrupt 标志位现状。
// ===========================================================================

const ICONST_0: u8 = 0x03;
const ICONST_3: u8 = 0x06;
const ICONST_4: u8 = 0x07;
const IFEQ: u8 = 0x99;
const IF_ICMPEQ: u8 = 0x9f;
const ILOAD_1: u8 = 0x1b;
const ILOAD_3: u8 = 0x1d;
const ISTORE_1: u8 = 0x3c;
const ISTORE_3: u8 = 0x3e;
const ALOAD_1: u8 = 0x2b;
const ASTORE_1: u8 = 0x4c;
const ISUB: u8 = 0x64;
const IMUL: u8 = 0x68;

/// 小整数压栈（-128..127；本段循环计数/档位均在该域内）。
fn push_i(code: &mut Vec<u8>, v: i32) {
    match v {
        0 => code.push(ICONST_0),
        1 => code.push(ICONST_1),
        2 => code.push(ICONST_2),
        3 => code.push(ICONST_3),
        4 => code.push(ICONST_4),
        -128..=127 => {
            code.push(BIPUSH);
            code.push(v as u8);
        }
        _ => panic!("push_i 值域外: {v}"),
    }
}

/// 前向分支占位（3 字节），返回指令位置；稍后 patch_forward 回填目标。
fn fwd_branch(code: &mut Vec<u8>, op: u8) -> usize {
    code.push(op);
    code.push(0);
    code.push(0);
    code.len() - 3
}

/// 回填前向分支偏移（相对指令起点）。
fn patch_forward(code: &mut [u8], at: usize, target: usize) {
    let offset = (target as i64 - at as i64) as i16;
    code[at + 1..at + 3].copy_from_slice(&offset.to_be_bytes());
}

/// 同步块构码（不收尾，可多块串联）：getstatic lock → astore_0 →
/// monitorenter → body（ALOAD_0 即锁对象）→ aload_0 → monitorexit。
fn sync_block(body: &[u8], lock_get: u16) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&op3(GETSTATIC, lock_get));
    bytes.push(ASTORE_0);
    bytes.push(ALOAD_0);
    bytes.push(MONITORENTER);
    bytes.extend_from_slice(body);
    bytes.push(ALOAD_0);
    bytes.push(MONITOREXIT);
    bytes
}

/// 块内等待循环：while (静态字段 != expect) lock.wait();
///（Mesa 风格——标志先于 notify 在同一锁内置位，无丢失唤醒）。
fn wait_until_eq(code: &mut Vec<u8>, field_get: u16, expect: i32, wait_call: [u8; 3]) {
    let loop_start = code.len();
    code.extend_from_slice(&op3(GETSTATIC, field_get));
    push_i(code, expect);
    let skip = fwd_branch(code, IF_ICMPEQ);
    code.push(ALOAD_0);
    code.extend_from_slice(&wait_call);
    let at = code.len();
    code.extend_from_slice(&branch(GOTO, (loop_start as i64 - at as i64) as i16));
    let end = code.len();
    patch_forward(code, skip, end);
}

/// 具名 Runnable（run = 指定码体，locals 可调——交叉锁用例需 >1）。
fn named_runnable_class(
    pool: &mut Pool,
    name: &'static str,
    run_code: Vec<u8>,
    stack: u16,
    locals: u16,
) -> (&'static str, Vec<u8>) {
    let object_init = op3(INVOKESPECIAL, pool.methodref(ROOT, "<init>", "()V"));
    (
        name,
        build_class(
            pool,
            name,
            Some(ROOT),
            &["java/lang/Runnable"],
            &[],
            &[
                MethodSpec {
                    access: 0x0001,
                    name: "<init>",
                    desc: "()V",
                    code: Some((1, 1, {
                        let mut bytes = Vec::new();
                        bytes.push(ALOAD_0);
                        bytes.extend_from_slice(&object_init);
                        bytes.push(RETURN);
                        bytes
                    })),
                    handlers: Vec::new(),
                },
                MethodSpec {
                    access: 0x0001,
                    name: "run",
                    desc: "()V",
                    code: Some((stack, locals, run_code)),
                    handlers: Vec::new(),
                },
            ],
        ),
    )
}

/// 自定义静态字段版主类（fields 为 (名, 描述符) 列表）。
fn custom_main_class(
    pool: &mut Pool,
    fields: &[(&'static str, &'static str)],
    main_code: Vec<u8>,
    stack: u16,
    locals: u16,
) -> (&'static str, Vec<u8>) {
    let fields = fields
        .iter()
        .map(|&(name, desc)| FieldSpec { access: 0x0009, name, desc, constant: None })
        .collect::<Vec<_>>();
    (
        "T",
        build_class(
            pool,
            "T",
            Some(ROOT),
            &[],
            &fields,
            &[MethodSpec {
                access: 0x0008,
                name: "main",
                desc: "()V",
                code: Some((stack, locals, main_code)),
                handlers: Vec::new(),
            }],
        ),
    )
}

/// new Thread(new NAME()) 构码（栈顶留 [t]，不含 start）。
fn code_new_thread_of(pool: &mut Pool, runnable: &str) -> Vec<u8> {
    let r_init = op3(INVOKESPECIAL, pool.methodref(runnable, "<init>", "()V"));
    let t_init = op3(
        INVOKESPECIAL,
        pool.methodref("java/lang/Thread", "<init>", "(Ljava/lang/Runnable;)V"),
    );
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&op3(NEW, pool.class("java/lang/Thread")));
    bytes.push(DUP);
    bytes.extend_from_slice(&op3(NEW, pool.class(runnable)));
    bytes.push(DUP);
    bytes.extend_from_slice(&r_init);
    bytes.extend_from_slice(&t_init);
    bytes
}

/// 静态 Object 字段初始化构码：new Object() → putstatic。
fn code_init_field(pool: &mut Pool, field: u16) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&op3(NEW, pool.class(ROOT)));
    bytes.push(DUP);
    bytes.extend_from_slice(&op3(INVOKESPECIAL, pool.methodref(ROOT, "<init>", "()V")));
    bytes.extend_from_slice(&op3(PUTSTATIC, field));
    bytes
}

// ---- 用例 ------------------------------------------------------------------

/// 双线程双向握手两次往返（ping-pong）：PA 置 flag=1→notify；PB 醒后置
/// flag=2→notify；PA 醒（round1）置 flag=3→notify；PB 醒置 flag=4→notify；
/// PA 二次醒（done=1）。全程 Mesa 循环（while+wait），无丢失唤醒。
#[test]
fn ping_pong_two_round_trips() {
    let mut pool = Pool::new();
    let lock_field = pool.fieldref("T", "lock", LOCK_DESC);
    let flag = pool.fieldref("T", "flag", "I");
    let round1 = pool.fieldref("T", "round1", "I");
    let done = pool.fieldref("T", "done", "I");
    let wait_call = op3(INVOKEVIRTUAL, pool.methodref("java/lang/Object", "wait", "()V"));
    let notify_call = op3(INVOKEVIRTUAL, pool.methodref("java/lang/Object", "notify", "()V"));
    // PA：发球 → 等回球（flag==2）记 round1 再回传（flag=3）→ 等终球（flag==4）。
    let pa_run = {
        let mut body1 = Vec::new();
        push_i(&mut body1, 1);
        body1.extend_from_slice(&op3(PUTSTATIC, flag));
        body1.push(ALOAD_0);
        body1.extend_from_slice(&notify_call);
        let mut body2 = Vec::new();
        wait_until_eq(&mut body2, flag, 2, wait_call);
        body2.push(ICONST_1);
        body2.extend_from_slice(&op3(PUTSTATIC, round1));
        push_i(&mut body2, 3);
        body2.extend_from_slice(&op3(PUTSTATIC, flag));
        body2.push(ALOAD_0);
        body2.extend_from_slice(&notify_call);
        let mut body3 = Vec::new();
        wait_until_eq(&mut body3, flag, 4, wait_call);
        body3.push(ICONST_1);
        body3.extend_from_slice(&op3(PUTSTATIC, done));
        let mut run = sync_block(&body1, lock_field);
        run.extend(sync_block(&body2, lock_field));
        run.extend(sync_block(&body3, lock_field));
        run.push(RETURN);
        run
    };
    // PB：等发球（flag==1）→ 回球（flag=2）→ 等回传（flag==3）→ 终球（flag=4）。
    let pb_run = {
        let mut body1 = Vec::new();
        wait_until_eq(&mut body1, flag, 1, wait_call);
        let mut body2 = Vec::new();
        push_i(&mut body2, 2);
        body2.extend_from_slice(&op3(PUTSTATIC, flag));
        body2.push(ALOAD_0);
        body2.extend_from_slice(&notify_call);
        let mut body3 = Vec::new();
        wait_until_eq(&mut body3, flag, 3, wait_call);
        push_i(&mut body3, 4);
        body3.extend_from_slice(&op3(PUTSTATIC, flag));
        body3.push(ALOAD_0);
        body3.extend_from_slice(&notify_call);
        let mut run = sync_block(&body1, lock_field);
        run.extend(sync_block(&body2, lock_field));
        run.extend(sync_block(&body3, lock_field));
        run.push(RETURN);
        run
    };
    let mut main_code = code_init_lock(&mut pool);
    main_code.extend(code_start_runnable(&mut pool, "PA"));
    main_code.extend(code_start_runnable(&mut pool, "PB"));
    main_code.push(RETURN);
    let t = custom_main_class(
        &mut pool,
        &[("lock", LOCK_DESC), ("flag", "I"), ("round1", "I"), ("done", "I")],
        main_code,
        6,
        1,
    );
    let pa = named_runnable_class(&mut pool, "PA", pa_run, 4, 1);
    let pb = named_runnable_class(&mut pool, "PB", pb_run, 4, 1);
    let mut fx = Fixture::new(vec![t, pa, pb]);
    fx.spawn_main();
    assert!(
        fx.drive_until(200, |fx| fx.static_int_ro("done") == Some(1)),
        "两次往返后 PA 应置 done"
    );
    assert_eq!(fx.static_int("round1"), 1, "第一往返完成标记");
    assert_eq!(fx.static_int("flag"), 4, "最终旗标应为 PB 终球值 4");
}

/// join 汇合链：main → join(R1)；R1 → join(R2)；R2 先产 count=2，R1 在
/// join 返回后 count=count*10+3（=23），main join 后快照 after——
/// 嵌套 join 的完成顺序语义（内层先于外层返回）。
#[test]
fn join_chain_through_two_workers() {
    let mut pool = Pool::new();
    let count = pool.fieldref("T", "count", "I");
    let after = pool.fieldref("T", "after", "I");
    let start = op3(INVOKEVIRTUAL, pool.methodref("java/lang/Thread", "start", "()V"));
    let join = op3(INVOKEVIRTUAL, pool.methodref("java/lang/Thread", "join", "()V"));
    // R2：count=2。
    let r2_run = {
        let mut run = Vec::new();
        run.push(ICONST_2);
        run.extend_from_slice(&op3(PUTSTATIC, count));
        run.push(RETURN);
        run
    };
    // R1：起 R2 → join → count = count*10 + 3。
    let r1_run = {
        let mut run = code_new_thread_of(&mut pool, "R2");
        run.push(ASTORE_1);
        run.push(ALOAD_1);
        run.extend_from_slice(&start);
        run.push(ALOAD_1);
        run.extend_from_slice(&join);
        run.extend_from_slice(&op3(GETSTATIC, count));
        run.push(BIPUSH);
        run.push(10);
        run.push(IMUL);
        run.push(ICONST_3);
        run.push(IADD);
        run.extend_from_slice(&op3(PUTSTATIC, count));
        run.push(RETURN);
        run
    };
    // main：起 R1 → join → after = count。
    let main_code = {
        let mut code = code_new_thread_of(&mut pool, "R1");
        code.push(ASTORE_1);
        code.push(ALOAD_1);
        code.extend_from_slice(&start);
        code.push(ALOAD_1);
        code.extend_from_slice(&join);
        code.extend_from_slice(&op3(GETSTATIC, count));
        code.extend_from_slice(&op3(PUTSTATIC, after));
        code.push(RETURN);
        code
    };
    let t = custom_main_class(&mut pool, &[("count", "I"), ("after", "I")], main_code, 6, 2);
    let r1 = named_runnable_class(&mut pool, "R1", r1_run, 6, 2);
    let r2 = named_runnable_class(&mut pool, "R2", r2_run, 2, 1);
    let mut fx = Fixture::new(vec![t, r1, r2]);
    fx.spawn_main();
    let ok = fx.drive_until(60, |fx| fx.static_int_ro("after") == Some(23));
    println!("join_chain after={} count={} ok={}", fx.static_int("after"), fx.static_int("count"), ok);
    assert!(
        fx.drive_until(1, |fx| fx.static_int_ro("after") == Some(23)),
        "嵌套 join 链应按内层先完成收敛（after=23）"
    );
    assert_eq!(fx.static_int("count"), 23, "R1 读到的必须是 R2 的产出");
    assert_eq!(fx.static_int("after"), 23);
}

/// monitor 唤醒顺序：三线程先后 wait 同一锁（entered 自增后等待），
/// 生产者等 entered==3 后连续两次 notify——FIFO 语义应恰好唤醒入队
/// 前二（W1、W2 各自把编号 order=order*10+d），第三位保持等待。
#[test]
fn notify_wakes_fifo_first_two_of_three() {
    let mut pool = Pool::new();
    let lock_field = pool.fieldref("T", "lock", LOCK_DESC);
    let entered = pool.fieldref("T", "entered", "I");
    let order = pool.fieldref("T", "order", "I");
    let signaled = pool.fieldref("T", "signaled", "I");
    let wait_call = op3(INVOKEVIRTUAL, pool.methodref("java/lang/Object", "wait", "()V"));
    let wait_timed = op3(INVOKEVIRTUAL, pool.methodref("java/lang/Object", "wait", "(J)V"));
    let notify_call = op3(INVOKEVIRTUAL, pool.methodref("java/lang/Object", "notify", "()V"));
    let one_long = pool.long(1);
    // 等待者 W{d}：入闸计数 → wait → 醒后序号拼接（order=order*10+d）。
    let waiter_run = |d: i32| -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&op3(GETSTATIC, entered));
        body.push(ICONST_1);
        body.push(IADD);
        body.extend_from_slice(&op3(PUTSTATIC, entered));
        body.push(ALOAD_0);
        body.extend_from_slice(&wait_call);
        body.extend_from_slice(&op3(GETSTATIC, order));
        body.push(BIPUSH);
        body.push(10);
        body.push(IMUL);
        push_i(&mut body, d);
        body.push(IADD);
        body.extend_from_slice(&op3(PUTSTATIC, order));
        let mut run = sync_block(&body, lock_field);
        run.push(RETURN); // 同步块不收尾，显式 RETURN
        run
    };
    // 生产者：while (entered != 3) wait(1)（1ms 超时自醒轮询闸门）→ 两次 notify。
    let producer_body = {
        let mut body = Vec::new();
        let loop_start = body.len();
        body.extend_from_slice(&op3(GETSTATIC, entered));
        body.push(ICONST_3);
        let skip = fwd_branch(&mut body, IF_ICMPEQ);
        body.push(ALOAD_0);
        body.extend_from_slice(&op3(LDC2_W, one_long));
        body.extend_from_slice(&wait_timed);
        let at = body.len();
        body.extend_from_slice(&branch(GOTO, (loop_start as i64 - at as i64) as i16));
        let end = body.len();
        patch_forward(&mut body, skip, end);
        body.push(ALOAD_0);
        body.extend_from_slice(&notify_call);
        body.push(ALOAD_0);
        body.extend_from_slice(&notify_call);
        body.push(ICONST_1);
        body.extend_from_slice(&op3(PUTSTATIC, signaled));
        body
    };
    let mut main_code = code_init_lock(&mut pool);
    main_code.extend(code_start_runnable(&mut pool, "W1"));
    main_code.extend(code_start_runnable(&mut pool, "W2"));
    main_code.extend(code_start_runnable(&mut pool, "W3"));
    main_code.extend(code_start_runnable(&mut pool, "PW"));
    main_code.push(RETURN);
    let t = custom_main_class(
        &mut pool,
        &[("lock", LOCK_DESC), ("entered", "I"), ("order", "I"), ("signaled", "I")],
        main_code,
        6,
        1,
    );
    let w1 = named_runnable_class(&mut pool, "W1", waiter_run(1), 4, 1);
    let w2 = named_runnable_class(&mut pool, "W2", waiter_run(2), 4, 1);
    let w3 = named_runnable_class(&mut pool, "W3", waiter_run(3), 4, 1);
    let mut pw_run = sync_block(&producer_body, lock_field);
    pw_run.push(RETURN); // 同步块不收尾，此处显式 RETURN
    let pw = named_runnable_class(&mut pool, "PW", pw_run, 4, 1);
    let mut fx = Fixture::new(vec![t, w1, w2, w3, pw]);
    fx.spawn_main();
    assert!(
        fx.drive_until(300, |fx| fx.static_int_ro("order") == Some(12)),
        "两次 notify 应按 FIFO 唤醒 W1、W2（order 拼为 12）"
    );
    assert_eq!(fx.static_int("signaled"), 1, "生产者应在入闸满 3 后完成两次 notify");
    // 追加推进：第三次唤醒不存在（仅两次 notify），W3 不得拼接序号。
    fx.drive_rounds(30);
    assert_eq!(
        fx.static_int("order"),
        12,
        "W3 应仍在等待——两次 notify 不覆盖第三个等待者"
    );
}

/// Timer 一次性任务边界（可控时钟）：schedule(task, 100) 在 main 首轮
///（t=0）登记 → due=100；推进至 t=99 不触发，跨过 t=100（第 101ms）触发。
#[test]
fn timer_one_shot_boundary_99_vs_101() {
    let mut pool = Pool::new();
    let refs = timer_refs(&mut pool);
    let main_code = code_timer_setup(&mut pool, &refs, 100, None, vec![RETURN]);
    let t = main_class(&mut pool, main_code, 6, 2);
    let tt = timer_task_class(&mut pool);
    let mut fx = Fixture::new(vec![t, tt]);
    fx.spawn_main();
    // 99 轮 = poll(0..98)：due=100 未到。
    fx.drive_rounds(99);
    assert_eq!(fx.static_int_ro("count"), Some(0), "t=99 不得触发（due=100）");
    // 再 2 轮 = poll(99)、poll(100)：跨过到期点恰好触发一次。
    fx.drive_rounds(2);
    assert_eq!(fx.static_int("count"), 1, "t=101 应已触发恰好一次");
    fx.drive_rounds(50);
    assert_eq!(fx.static_int("count"), 1, "一次性任务此后不再触发");
}

/// Timer fixed-rate 长跑无漂移：schedule(task, 100, 100) → 触发点严格
/// 落在 100/200/300/400/500（整倍推进）；t=599 处恰 5 次，跨过 600 第 6 次。
#[test]
fn timer_fixed_rate_five_periods_no_drift() {
    let mut pool = Pool::new();
    let refs = timer_refs(&mut pool);
    let main_code = code_timer_setup(&mut pool, &refs, 100, Some(100), vec![RETURN]);
    let t = main_class(&mut pool, main_code, 8, 2);
    let tt = timer_task_class(&mut pool);
    let mut fx = Fixture::new(vec![t, tt]);
    fx.spawn_main();
    fx.drive_rounds(599); // poll(0..598)：五个整倍点全部跨过、第六点未到。
    assert_eq!(fx.static_int("count"), 5, "t=599 应恰触发 5 次（100..500）");
    fx.drive_rounds(2); // poll(599)、poll(600)：第六个整倍点。
    assert_eq!(fx.static_int("count"), 6, "t=600 第 6 次触发（fixed-rate 无漂移）");
}

/// 交叉锁竞争防饿死：XA 与 XB 各 8 轮"先 lockA 后 lockB / 先 lockB 后
/// lockA"的顺序竞争（逐块获取释放，不嵌套持双锁——真嵌套是合法死锁，
/// 不在防饿死语义范围），第三线程 sleep 轮转穿插。三线程全部在轮数
/// 上限内完成（BlockedMonitor 在锁释放后被调度器唤醒重试，无饿死）。
#[test]
fn cross_lock_racing_threads_all_progress() {
    let mut pool = Pool::new();
    let lock_a = pool.fieldref("T", "lockA", LOCK_DESC);
    let lock_b = pool.fieldref("T", "lockB", LOCK_DESC);
    let count = pool.fieldref("T", "count", "I");
    let count2 = pool.fieldref("T", "count2", "I");
    let w1_done = pool.fieldref("T", "w1Done", "I");
    let w2_done = pool.fieldref("T", "w2Done", "I");
    let sleep_done = pool.fieldref("T", "sleepDone", "I");
    let sleep_call = op3(INVOKESTATIC, pool.methodref("java/lang/Thread", "sleep", "(J)V"));
    let one_long = pool.long(1);
    // 通用竞争体：8 轮 { sync(outer){ c += 1 } sync(inner){ c += 1 } } → done=1。
    let racer_run = |outer: u16, inner: u16, c: u16, done: u16| -> Vec<u8> {
        let mut run = Vec::new();
        run.push(BIPUSH);
        run.push(8); // 轮数
        run.push(ISTORE_3); // 计数器 → local 3（0=this，1/2=锁）
        let loop_start = run.len();
        run.push(ILOAD_3);
        let out = fwd_branch(&mut run, IFEQ);
        let bump = |run: &mut Vec<u8>, lock: u16, c: u16| {
            run.extend_from_slice(&op3(GETSTATIC, lock));
            run.push(ASTORE_1);
            run.push(ALOAD_1);
            run.push(MONITORENTER);
            run.extend_from_slice(&op3(GETSTATIC, c));
            run.push(ICONST_1);
            run.push(IADD);
            run.extend_from_slice(&op3(PUTSTATIC, c));
            run.push(ALOAD_1);
            run.push(MONITOREXIT);
        };
        bump(&mut run, outer, c);
        bump(&mut run, inner, c);
        run.push(ILOAD_3);
        run.push(ICONST_1);
        run.push(ISUB);
        run.push(ISTORE_3);
        let at = run.len();
        run.extend_from_slice(&branch(GOTO, (loop_start as i64 - at as i64) as i16));
        let end = run.len();
        patch_forward(&mut run, out, end);
        run.push(ICONST_1);
        run.extend_from_slice(&op3(PUTSTATIC, done));
        run.push(RETURN);
        run
    };
    // 穿插线程：3 × sleep(1) → sleepDone=1（与时钟依赖方共享轮转）。
    let sleeper_run = {
        let mut run = Vec::new();
        run.push(ICONST_3);
        run.push(ISTORE_1);
        let loop_start = run.len();
        run.push(ILOAD_1);
        let out = fwd_branch(&mut run, IFEQ);
        run.extend_from_slice(&op3(LDC2_W, one_long));
        run.extend_from_slice(&sleep_call);
        run.push(ILOAD_1);
        run.push(ICONST_1);
        run.push(ISUB);
        run.push(ISTORE_1);
        let at = run.len();
        run.extend_from_slice(&branch(GOTO, (loop_start as i64 - at as i64) as i16));
        let end = run.len();
        patch_forward(&mut run, out, end);
        run.push(ICONST_1);
        run.extend_from_slice(&op3(PUTSTATIC, sleep_done));
        run.push(RETURN);
        run
    };
    let mut main_code = code_init_field(&mut pool, lock_a);
    main_code.extend(code_init_field(&mut pool, lock_b));
    main_code.extend(code_start_runnable(&mut pool, "XA"));
    main_code.extend(code_start_runnable(&mut pool, "XB"));
    main_code.extend(code_start_runnable(&mut pool, "SL"));
    main_code.push(RETURN);
    let t = custom_main_class(
        &mut pool,
        &[
            ("lockA", LOCK_DESC),
            ("lockB", LOCK_DESC),
            ("count", "I"),
            ("count2", "I"),
            ("w1Done", "I"),
            ("w2Done", "I"),
            ("sleepDone", "I"),
        ],
        main_code,
        6,
        1,
    );
    let xa = named_runnable_class(&mut pool, "XA", racer_run(lock_a, lock_b, count, w1_done), 5, 4);
    let xb = named_runnable_class(&mut pool, "XB", racer_run(lock_b, lock_a, count2, w2_done), 5, 4);
    let sl = named_runnable_class(&mut pool, "SL", sleeper_run, 4, 2);
    let mut fx = Fixture::new(vec![t, xa, xb, sl]);
    fx.spawn_main();
    let all_done = |fx: &Fixture| {
        fx.static_int_ro("w1Done") == Some(1)
            && fx.static_int_ro("w2Done") == Some(1)
            && fx.static_int_ro("sleepDone") == Some(1)
    };
    assert!(fx.drive_until(400, all_done), "交叉竞争 + sleep 穿插下三线程均应完成（不饿死）");
    assert_eq!(fx.static_int("count"), 16, "XA 每轮两块各 +1 × 8 轮");
    assert_eq!(fx.static_int("count2"), 16, "XB 同构");
}

/// interrupt 现状（标志位语义）：main 在 start 后立刻 interrupt（同轮内
/// 先于工作线程 spawn_pending，时序确定）→ 目标线程 run 里经
/// currentThread().isInterrupted() 读到真；发起方 isInterrupted 同真。
/// 注意（唤醒面 TODO，thread.rs T034+ 注释）：当前 interrupt 不提前打断
/// sleep/join/wait——仅标志位，本用例如实断言该现状。
#[test]
fn interrupt_flag_observed_by_target_thread() {
    let mut pool = Pool::new();
    let saw = pool.fieldref("T", "saw", "I");
    let after = pool.fieldref("T", "after", "I");
    let start = op3(INVOKEVIRTUAL, pool.methodref("java/lang/Thread", "start", "()V"));
    let interrupt = op3(INVOKEVIRTUAL, pool.methodref("java/lang/Thread", "interrupt", "()V"));
    let is_interrupted = op3(INVOKEVIRTUAL, pool.methodref("java/lang/Thread", "isInterrupted", "()Z"));
    let current_thread =
        op3(INVOKESTATIC, pool.methodref("java/lang/Thread", "currentThread", "()Ljava/lang/Thread;"));
    // IW：ct = currentThread(); saw = ct.isInterrupted() ? 1 : 0。
    let iw_run = {
        let mut run = Vec::new();
        run.extend_from_slice(&current_thread);
        run.push(ASTORE_1);
        run.push(ALOAD_1);
        run.extend_from_slice(&is_interrupted);
        run.extend_from_slice(&op3(PUTSTATIC, saw));
        run.push(RETURN);
        run
    };
    // main：起 IW → interrupt → after = t.isInterrupted()。
    let main_code = {
        let mut code = code_new_thread_of(&mut pool, "IW");
        code.push(ASTORE_1);
        code.push(ALOAD_1);
        code.extend_from_slice(&start);
        code.push(ALOAD_1);
        code.extend_from_slice(&interrupt);
        code.push(ALOAD_1);
        code.extend_from_slice(&is_interrupted);
        code.extend_from_slice(&op3(PUTSTATIC, after));
        code.push(RETURN);
        code
    };
    let t = custom_main_class(&mut pool, &[("saw", "I"), ("after", "I")], main_code, 4, 2);
    let iw = named_runnable_class(&mut pool, "IW", iw_run, 3, 2);
    let mut fx = Fixture::new(vec![t, iw]);
    fx.spawn_main();
    assert!(
        fx.drive_until(30, |fx| fx.static_int_ro("saw") == Some(1)),
        "目标线程应经 currentThread().isInterrupted() 观察到标志位"
    );
    assert_eq!(fx.static_int("after"), 1, "发起方 isInterrupted 同样为真");
}
