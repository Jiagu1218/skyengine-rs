//! Vm 门面（T013）：引擎侧驱动 J2ME 虚拟机的唯一入口。
//!
//! 状态机：`Empty --install_jar--> JarReady --start_midlet--> Running --(全线程
//! 终止 / shutdown / 内部故障)--> Dead`。`shutdown` 可重入；`Dead` 为终态。
//!
//! 持有形态：`Machine<'a>` 借用 cs/heap/monitors/natives，无法整存进自持
//! 结构，故 Vm 直接持有四个组成部分，在 `poll`/`start_midlet` 内就地组装
//! （各字段借用互不相交，借用检查器可证）。

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

use crate::classfile::member::acc;
use crate::host::HostContext;
use crate::interp::natives::RegistryDispatch;
use crate::interp::{Machine, ensure_initialized, push_entry_frame};
use crate::midp::lcdui::game::gamecanvas;
use crate::midp::lcdui::{canvas, display, font, graphics, image, input};
use crate::runtime::classspace::{ClassProvider, ClassSpace, MidletLifecycleOp};
use crate::runtime::heap::Heap;
use crate::runtime::monitor::Monitors;
use crate::runtime::{ClassId, ObjRef, Slot, Sym, ThreadId};
use crate::scheduler::{PollOutcome, Scheduler};
use crate::{KvmError, jar};

/// VM 生命周期状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmState {
    /// 尚未安装 jar。
    Empty,
    /// jar 已索引，等待 start_midlet。
    JarReady,
    /// MIDlet 线程已在调度器中运行。
    Running,
    /// 终态：线程跑尽、shutdown 或内部故障。
    Dead,
}

/// VM 配置。
#[derive(Clone, Copy, Debug)]
pub struct VmConfig {
    /// 托管堆预算（字节）。按每对象约 64 字节折算堆对象数软上限。
    pub heap_bytes: usize,
}

impl Default for VmConfig {
    fn default() -> Self {
        Self {
            heap_bytes: 16 << 20, // 16MB
        }
    }
}

/// 门面层错误：引擎据其区分"可重入失败"（换 jar / 换入口重试）与 VM 内部故障。
#[derive(Debug, thiserror::Error)]
pub enum VmError {
    #[error("class not found: {0}")]
    ClassNotFound(String),
    #[error("bad jar: {0}")]
    BadJar(String),
    #[error("invalid vm state: {0}")]
    InvalidState(String),
    /// 解释器/类空间内部故障（KvmError 透传）。
    #[error(transparent)]
    Kvm(#[from] KvmError),
}

/// 待投递给 MIDlet 的事件（poll 段前置事件泵消费，T030 Key / T031
/// Pointer）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmEvent {
    /// 按键：引擎键名（`input` 模块值域）+ 按下/抬起。
    Key { key: u8, pressed: bool },
    /// 指针：坐标 + 种类（0=按下 1=抬起 2=拖拽，T031 细化）。
    Pointer { x: i16, y: i16, kind: u8 },
}

/// 单次 poll 的结果快照。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PollResult {
    /// 本次驱动的线程数（0 或 1：poll 每次驱动一个线程到让出点，
    /// 引擎帧循环反复调用实现轮转）。
    pub ran_threads: u32,
    /// 全部存活线程阻塞时最近的定时唤醒时刻；否则 None。
    pub all_blocked_next_wake: Option<u64>,
    /// poll 后的 VM 状态。
    pub state: VmState,
}

/// 存活的 LCDUI paint 回调会话（T021 三段式 poll）：
/// 回调绿线程 id + 被绘 Canvas。canvas 一并记录——段三呈现不应依赖
/// poll 时刻的 `Display.current`（paint 期间 `setCurrent(null)` 也
/// 应呈现已完成的一帧）。
struct PaintSession {
    thread: ThreadId,
    canvas: ObjRef,
}

/// 存活的 MIDlet 生命周期回调会话（T044，PaintSession 同款三段式）：
/// 回调绿线程 id + 发起的请求种类。destroyApp 会话须等回调线程死亡
/// （正常返回或异常逃逸）后再做销毁判定——记录 op 以区分
/// Pause/Resume（无事收尾）与 Destroy（含拒绝销毁分支）。
struct LifecycleSession {
    thread: ThreadId,
    op: MidletLifecycleOp,
}

/// jar 条目供给器：与 Vm 共享条目表（install_jar 时更新，
/// load_class 时惰性取原始字节再解析）。
struct JarProvider {
    entries: Rc<RefCell<BTreeMap<String, Vec<u8>>>>,
}

impl ClassProvider for JarProvider {
    fn class_bytes(&mut self, name: &str) -> Option<Vec<u8>> {
        self.entries.borrow().get(name).cloned()
    }
}

/// J2ME 虚拟机门面。
pub struct Vm {
    host: Box<dyn HostContext>,
    cs: ClassSpace,
    heap: Heap,
    monitors: Monitors,
    natives: Box<RegistryDispatch>,
    scheduler: Scheduler,
    state: VmState,
    /// jar 条目：类名为去 `.class` 的内部名，资源为原名。
    /// 与 ClassSpace 的 provider 共享（惰性解析）。
    jar: Rc<RefCell<BTreeMap<String, Vec<u8>>>>,
    /// install_jar 时统计的类条目数（观测面）。
    jar_class_count: usize,
    /// 待消费事件队列（poll 段前置 `pump_input_events` 消费，T030）。
    events: VecDeque<VmEvent>,
    /// 存活的 paint 回调会话（T021）；None = 无进行中的 paint。
    painting: Option<PaintSession>,
    /// 主 MIDlet 实例（T044）：start_midlet 时记录，生命周期回调
    /// （pauseApp/destroyApp/startApp）的 receiver。
    midlet: Option<ObjRef>,
    /// 存活的生命周期回调会话（T044）；None = 无进行中的回调，
    /// pending_lifecycle 队列消费的前置。
    lifecycle: Option<LifecycleSession>,
    /// 致命根因文本（T045）：异常类名（未捕获逃逸）或解释器内部故障
    /// （含堆上限 OOM）的文本。全线程死尽时置入，引擎侧检测
    /// `Dead + last_fatal` 绘制错误画面；正常退出（destroyApp 回调
    /// 返回后转 Dead）为 None。
    last_fatal: Option<String>,
}

impl Vm {
    /// 创建空 VM。`host` 为宿主引擎服务通道（时钟/屏幕/RMS/...）。
    pub fn new(host: Box<dyn HostContext>, config: VmConfig) -> Vm {
        let jar: Rc<RefCell<BTreeMap<String, Vec<u8>>>> = Rc::new(RefCell::new(BTreeMap::new()));
        let provider = JarProvider {
            entries: Rc::clone(&jar),
        };
        // 对象数软上限：按每对象约 64 字节从堆预算折算，下限防御。
        let object_cap = (config.heap_bytes / 64).max(1024);
        Vm {
            host,
            cs: ClassSpace::new(Box::new(provider)),
            heap: Heap::with_cap(object_cap),
            monitors: Monitors::new(),
            natives: Box::new(RegistryDispatch),
            scheduler: Scheduler::new(),
            state: VmState::Empty,
            jar,
            jar_class_count: 0,
            events: VecDeque::new(),
            painting: None,
            midlet: None,
            lifecycle: None,
            last_fatal: None,
        }
    }

    /// 当前状态。
    pub fn state(&self) -> VmState {
        self.state
    }

    /// MIDlet 是否存活运行中。
    pub fn is_active(&self) -> bool {
        self.state == VmState::Running
    }

    /// jar 已索引的类条目数。
    pub fn jar_class_count(&self) -> usize {
        self.jar_class_count
    }

    /// 查询 jar 条目（类名为去 `.class` 的内部名；资源为原名）。
    /// 资源自 T018 起存套件资源表，两处并查。
    pub fn has_jar_entry(&self, name: &str) -> bool {
        self.jar.borrow().contains_key(name) || self.cs.suite_resources.contains_key(name)
    }

    /// 并入平台文件资源（init 装配后、start 前）：斯凯真机的 jar 外资源
    /// 语义——mythroad/ 下的渠道文件（如 gjxjwql 的 1.link 菜单数据，
    /// jar 内不存在、真机从平台文件系统读）对 getResourceAsStream 可见。
    /// 键=文件名（与 "/x" 去斜杠后的查询键一致）；jar 内已有条目不覆盖。
    pub fn merge_platform_resources(&mut self, files: Vec<(String, Vec<u8>)>) {
        for (name, bytes) in files {
            self.cs
                .suite_resources
                .entry(name)
                .or_insert_with(|| Rc::new(bytes));
        }
    }

    /// 读取静态字段槽（引擎/测试观测面；类未加载或字段不存在返回 None）。
    pub fn static_field(&self, class: &str, field: &str, desc: &str) -> Option<Slot> {
        let id = self.cs.find(class)?;
        let vm_class = self.cs.class(id);
        let name = Sym(self.cs.syms.lookup(field)?);
        let descriptor = Sym(self.cs.syms.lookup(desc)?);
        let index = vm_class.field_index.get(&(name, descriptor))?;
        let vm_field = &vm_class.fields[*index as usize];
        if !vm_field.is_static() {
            return None;
        }
        vm_class.statics.get(vm_field.slot as usize).copied()
    }

    /// 安装 jar（init 阶段）：zip 解包 + 索引全部条目。
    ///
    /// 类条目惰性解析（仅存原始字节，load_class 时才 parse，对齐类空间
    /// 既有 jar 接入形态）；重复安装覆盖旧 jar。`name` 仅作记录（TODO
    /// T025：随包信息透传给 read_suite_file 通道）。
    pub fn install_jar(&mut self, _name: &str, data: &[u8]) -> Result<(), VmError> {
        if self.state == VmState::Running || self.state == VmState::Dead {
            return Err(VmError::InvalidState(format!(
                "install_jar requires Empty/JarReady, got {:?}",
                self.state
            )));
        }
        let entries = jar::unpack(data).map_err(VmError::BadJar)?;
        let mut map = self.jar.borrow_mut();
        map.clear();
        self.cs.suite_resources.clear();
        let mut classes = 0usize;
        for (entry_name, bytes) in entries {
            if let Some(class_name) = entry_name.strip_suffix(".class") {
                classes += 1;
                map.insert(class_name.to_string(), bytes);
            } else {
                // 非 class 条目（资源：png/manifest 等）以 Rc 入套件资源表
                //（T018 createImage 数据源）；字节本体不复制，jar 表仅存类。
                self.cs.suite_resources.insert(entry_name, Rc::new(bytes));
            }
        }
        self.jar_class_count = classes;
        // MIDlet 套件属性：从资源表的 META-INF/MANIFEST.MF 解析
        //（getAppProperty 数据源）。
        self.cs.suite_properties = self
            .cs
            .suite_resources
            .get("META-INF/MANIFEST.MF")
            .map(|bytes| parse_manifest(bytes))
            .unwrap_or_default();
        if self.state == VmState::Empty {
            self.state = VmState::JarReady;
        }
        Ok(())
    }

    /// 启动 MIDlet（start 阶段）：加载入口类 → 分配实例 → 构帧
    /// （clinit → `<init>` → startApp 顺序执行）→ 入调度器。
    pub fn start_midlet(&mut self, main_class: &str) -> Result<(), VmError> {
        if self.state != VmState::JarReady {
            return Err(VmError::InvalidState(format!(
                "start_midlet requires JarReady, got {:?}",
                self.state
            )));
        }
        // mainclass 文件内容为二进制名（点号分隔）；类空间用内部名（斜杠）。
        let main_class = main_class.trim().replace('.', "/");
        let class = self
            .cs
            .load_class(&main_class)
            .map_err(|error| match error {
                KvmError::ClassNotFound(name) => VmError::ClassNotFound(name),
                other => VmError::Kvm(other),
            })?;
        // 分配实例：字段零值模板来自类空间布局期。
        let template = self.cs.class(class).instance_template.clone();
        let instance = self.heap.alloc_instance(class, template)?;

        // 帧序（栈底→顶）：startApp（若有公开 ()V）→ <init>（若有）→ clinit 链。
        // run 从栈顶执行：clinit 最先，<init> 返回后轮到 startApp。
        // 无 <init>/startApp 则跳过对应帧；MIDlet 生命周期语义在 T026 细化。
        let mut frames = Vec::new();
        {
            let host = &mut *self.host;
            let mut machine = Machine {
                cs: &mut self.cs,
                heap: &mut self.heap,
                monitors: &mut self.monitors,
                natives: self.natives.as_mut(),
                host,
            };
            if Self::has_public_method(&*machine.cs, class, "startApp", "()V") {
                push_entry_frame(
                    &mut machine,
                    &mut frames,
                    class,
                    "startApp",
                    "()V",
                    Some(instance),
                    Vec::new(),
                )?;
            }
            if Self::has_any_method(&*machine.cs, class, "<init>", "()V") {
                push_entry_frame(
                    &mut machine,
                    &mut frames,
                    class,
                    "<init>",
                    "()V",
                    Some(instance),
                    Vec::new(),
                )?;
            }
            ensure_initialized(&mut machine, &mut frames, class)?;
        }
        // 主线程入调度器 + 生命周期表登记（T033）：Thread 对象不在此
        // 分配（懒建于 Thread.currentThread 首调），三表其余两侧同样
        // 由 currentThread/spawn_pending 补录。
        let main_thread = self.scheduler.spawn(frames, main_class.to_string());
        crate::cldc::thread::register_thread_alive(&mut self.cs, main_thread);
        // T044：记录主 MIDlet 实例——后续 notifyDestroyed/notifyPaused/
        // resumeRequest 请求的回调（destroyApp/pauseApp/startApp）以它
        // 为 receiver 构帧。
        self.midlet = Some(instance);
        self.state = VmState::Running;
        Ok(())
    }

    /// 引擎帧循环心跳：以宿主时钟驱动调度器跑一个线程到让出点。
    ///
    /// T021 起为**三段式**（LCDUI paint 回调链，不搞解释器重入）：
    /// 0. **段前置（T030 追加）**：输入事件泵（`pump_input_events`）：
    ///    drain `events` → Key 事件映射 MIDP keyCode → spawn
    ///    keyPressed/keyPressed 回调绿线程（Pointer 留 T031）。
    /// 0b. **段零（T033 追加）**：Thread.start 请求队列消费
    ///    （`cldc::thread::spawn_pending`）：drain `cs.pending_threads`
    ///    → 解析 run 帧 → `scheduler.spawn` 拉起真绿线程 + 登记三表。
    /// 1. **调度段前**：无存活 paint 线程时，检查全局重绘请求位
    ///    （serviceRepaints）或 current Canvas 的脏标记（repaint 并集）
    ///    → 探测子类 `paint` 实现 → 懒分配屏图 + Graphics
    ///    （clip=脏矩形∩屏）→ `push_entry_frame` → spawn 绿线程
    ///    （`maybe_spawn_paint`；无实现/分配失败等静默清脏跳过）。
    /// 2. **调度段**：原逻辑不动（跑一个线程到让出点）。
    /// 3. **调度段后**：paint 线程已死 → 屏图 `[I` 逐像素拆 RGBA8
    ///    呈现宿主（`present_screen`）。
    ///
    /// 非 Running 态为无害 no-op。全部线程终止（或解释器内部故障，视作
    /// VM 死亡）时转入 Dead。T034 起段零 b 推进 Timer 队列
    /// （pending_timers → scheduler 定时队列，fire 在段二内）。
    pub fn poll(&mut self, budget: u64) -> PollResult {
        if self.state != VmState::Running {
            return PollResult {
                ran_threads: 0,
                all_blocked_next_wake: None,
                state: self.state,
            };
        }
        // 段前：输入事件泵（T030）——drain `events` 队列，Key 事件转
        // Canvas keyPressed/keyReleased 绿线程回调（Pointer 留 T031）。
        // 先于段零/段一：本 poll 内即驱动回调，且回调内 repaint 置位
        // 可被同 poll 的段一消费。
        self.pump_input_events();
        // 段零：Thread.start 请求队列消费（T033）。与 T021 repaint 请求位
        // 同款"native 置位 → poll 消费"协议；消费失败（链接错误等）视作
        // VM 内部故障 → Dead（与调度段 Err 同归）。
        {
            let host = &mut *self.host;
            let mut machine = Machine {
                cs: &mut self.cs,
                heap: &mut self.heap,
                monitors: &mut self.monitors,
                natives: self.natives.as_mut(),
                host,
            };
            if crate::cldc::thread::spawn_pending(&mut machine, &mut self.scheduler).is_err() {
                self.state = VmState::Dead;
                return PollResult {
                    ran_threads: 0,
                    all_blocked_next_wake: None,
                    state: self.state,
                };
            }
            // 段零 a2：斯凯平台异步完成队列消费（SP-1）——drain
            // `cs.pending_sky_results`（req_async 模拟平台受理入队），
            // 每个响应字节数组 spawn 一个根级类 f 的静态 `a([B)V`
            // （dispatchJNI）帧。链接失败与段零 a 同归 Dead。
            let sky_results = std::mem::take(&mut machine.cs.pending_sky_results);
            for payload in sky_results {
                match machine.cs.load_class("f") {
                    Ok(class) => {
                        let mut frames = Vec::new();
                        match crate::interp::push_entry_frame(
                            &mut machine,
                            &mut frames,
                            class,
                            "a",
                            "([B)V",
                            None,
                            vec![Slot::Ref(payload)],
                        ) {
                            Ok(_) => {
                                let id = self.scheduler.spawn(frames, "sky-dispatch");
                                machine.cs.thread_alive.insert(id, true);
                            }
                            Err(error) => {
                                self.state = VmState::Dead;
                                eprintln!("[kvm] sky dispatch spawn failed: {error}");
                                return PollResult {
                                    ran_threads: 0,
                                    all_blocked_next_wake: None,
                                    state: self.state,
                                };
                            }
                        }
                    }
                    Err(error) => {
                        self.state = VmState::Dead;
                        eprintln!("[kvm] sky dispatch class f missing: {error}");
                        return PollResult {
                            ran_threads: 0,
                            all_blocked_next_wake: None,
                            state: self.state,
                        };
                    }
                }
            }
            // 段零 b：Timer 请求队列消费（T034）：`Timer.schedule*` 置位
            // （delay 版取宿主时钟）→ 调度器定时队列登记；到期触发在
            // 段二调度器 poll 唤醒段内（惰性查取消、fixed-rate 重排、
            // spawn timer-task 绿线程，本 poll 即参与轮转）。
            crate::cldc::util::pump_pending_timers(machine.cs, &mut self.scheduler);
            // 段零 c：MIDlet 生命周期请求消费（T044）：drain
            // `cs.pending_lifecycle`（notifyDestroyed/notifyPaused/
            // resumeRequest/System.exit 类 native 置位），无进行中会话时
            // 取首个请求，对主 MIDlet 实例构帧回调（PauseApp→pauseApp、
            // DestroyApp→destroyApp(Z)、Resume→startApp——MIDP 规范：
            // 恢复运行态即重入 startApp）spawn "lcdui-lifecycle" 绿线程；
            // 其余请求留队待会话收尾后消费（同 T021 paint 串行约束）。
            // 子类未覆写回调（find_method 落空或 bootstrap 桩无方法体）：
            // destroy 直接完成销毁（无回调可跑），pause/resume 静默丢弃。
            let mut requests = std::mem::take(&mut machine.cs.pending_lifecycle);
            if self.lifecycle.is_none()
                && let Some(&op) = requests.first()
                && let Some(instance) = self.midlet
                && let Ok(object) = machine.heap.instance(instance)
            {
                let runtime_class = object.class;
                let (callback, desc, args) = match op {
                    MidletLifecycleOp::PauseApp => ("pauseApp", "()V", Vec::new()),
                    MidletLifecycleOp::DestroyApp { unconditional } => (
                        "destroyApp",
                        "(Z)V",
                        vec![Slot::Int(i32::from(unconditional))],
                    ),
                    MidletLifecycleOp::Resume => ("startApp", "()V", Vec::new()),
                };
                let name_sym = machine.cs.syms.intern(callback);
                let desc_sym = machine.cs.syms.intern(desc);
                let resolved = if let Some((defining, index)) =
                    machine.cs.find_method(runtime_class, name_sym, desc_sym)
                {
                    machine.cs.class(defining).methods[usize::from(index)]
                        .body
                        .is_some()
                } else {
                    false
                };
                if resolved {
                    // 构帧：receiver=主 MIDlet 实例（实例存在 ⇒ 运行时类已
                    // 初始化，无须补 clinit 链）；构帧失败静默出队丢弃
                    // （与输入事件泵同口径）。
                    let mut frames = Vec::new();
                    if crate::interp::push_entry_frame(
                        &mut machine,
                        &mut frames,
                        runtime_class,
                        callback,
                        desc,
                        Some(instance),
                        args,
                    )
                    .is_ok()
                    {
                        let thread = self.scheduler.spawn(frames, "lcdui-lifecycle");
                        self.lifecycle = Some(LifecycleSession { thread, op });
                    }
                } else if matches!(op, MidletLifecycleOp::DestroyApp { .. }) {
                    // 子类无 destroyApp 实现可回调：直接完成销毁语义
                    // （防御分支：真实 MIDlet 子类必须实现抽象方法）。
                    self.state = VmState::Dead;
                    return PollResult {
                        ran_threads: 0,
                        all_blocked_next_wake: None,
                        state: self.state,
                    };
                }
                requests.remove(0);
            }
            machine.cs.pending_lifecycle = requests;
        }
        // 段一：paint 请求判定与回调线程拉起（可能为调度器补充一个
        // lcdui-paint 绿线程，同 poll 内即被段二驱动）。
        if self.painting.is_none() {
            self.maybe_spawn_paint();
        }
        // 段二：调度段（now_ms 须在 host 被 Machine 可变借用前取完）。
        let now = self.host.now_ms();
        let outcome = {
            let host = &mut *self.host;
            let mut machine = Machine {
                cs: &mut self.cs,
                heap: &mut self.heap,
                monitors: &mut self.monitors,
                natives: self.natives.as_mut(),
                host,
            };
            self.scheduler.poll(now, &mut machine, budget)
        };
        let result = match outcome {
            Ok(PollOutcome::Ran(_)) => PollResult {
                ran_threads: 1,
                all_blocked_next_wake: None,
                state: self.state,
            },
            Ok(PollOutcome::AllBlocked { next_wake }) => PollResult {
                ran_threads: 0,
                all_blocked_next_wake: next_wake,
                state: self.state,
            },
            Ok(PollOutcome::Idle) => {
                // 全线程死尽 → 终态。T045 fatal 升格：存在未捕获异常
                // 死亡记录（cs.last_uncaught）时作为致命根因上报（引擎侧
                // 绘错误画面）；正常退出无记录 → 无 fatal。
                if let Some((_, text)) = self.cs.last_uncaught.take() {
                    self.last_fatal = Some(text);
                }
                self.state = VmState::Dead;
                PollResult {
                    ran_threads: 0,
                    all_blocked_next_wake: None,
                    state: self.state,
                }
            }
            Err(ref error) => {
                // 解释器内部故障（含堆上限 OOM 的 KvmError::OutOfMemory）
                // → 终态；错误文本接进 fatal 通道。
                self.last_fatal = Some(format!("{error}"));
                self.state = VmState::Dead;
                PollResult {
                    ran_threads: 0,
                    all_blocked_next_wake: None,
                    state: self.state,
                }
            }
        };
        // 段三：paint 回调收尾（本次或上次调度中跑完 → 呈现帧）。
        let paint_finished = self
            .painting
            .as_ref()
            .is_some_and(|session| self.scheduler.is_dead(session.thread));
        if paint_finished && let Some(session) = self.painting.take() {
            self.present_screen(session.canvas);
        }
        // 段三 b：生命周期回调收尾（T044）：回调线程死亡后按请求种类
        // 收尾——Destroy 做销毁判定（拒绝销毁/强制销毁/fatal），Pause/
        // Resume 仅结束会话（状态回调已执行，语义记录见 midp::midlet）。
        let lifecycle_finished = self
            .lifecycle
            .as_ref()
            .is_some_and(|session| self.scheduler.is_dead(session.thread));
        if lifecycle_finished
            && let Some(session) = self.lifecycle.take()
            && let MidletLifecycleOp::DestroyApp { unconditional } = session.op
        {
            self.finish_destroy(session.thread, unconditional);
        }
        result
    }

    /// destroyApp 回调线程死亡后的销毁判定（T044/T045）。
    ///
    /// 死因证据来自 `cs.last_uncaught`（scheduler 在线程 Uncaught 终止
    /// 时写入 (线程 id, 异常类名)）：
    /// - 本回调线程抛 `MIDletStateChangeException` 且非强制
    ///   （unconditional=false）→ **拒绝销毁**：MIDP 规范语义，VM 保持
    ///   Running（等待后续再次 destroy 请求）；记录已消费。
    /// - 本回调线程抛同异常但强制（unconditional=true，System.exit）→
    ///   异常吞掉照销（规范允许），转 Dead 无 fatal。
    /// - 其他未捕获异常（本回调或此前任何线程的死亡记录）→ fatal 通道
    ///   上报根因文本，转 Dead（引擎侧绘错误画面）。
    /// - 无记录（正常返回）→ 完成，转 Dead。
    fn finish_destroy(&mut self, thread: ThreadId, unconditional: bool) {
        match self.cs.last_uncaught.take() {
            Some((tid, text)) if tid == thread && text.ends_with("MIDletStateChangeException") => {
                if !unconditional {
                    // 拒绝销毁：保持 Running，等待再次 destroy 请求。
                    return;
                }
                // 强制销毁：回调异常吞掉（规范允许），照常完成。
                self.state = VmState::Dead;
            }
            Some((_, text)) => {
                self.last_fatal = Some(text);
                self.state = VmState::Dead;
            }
            None => self.state = VmState::Dead,
        }
    }

    /// 类空间只读访问（测试/剖析面：热点表查询）。
    pub fn classspace(&self) -> &ClassSpace {
        &self.cs
    }

    /// 致命根因文本（T045 观测面）：未捕获异常类名或内部故障/OOM 文本。
    /// 引擎侧（core KvmRuntime）在 VM 停摆后检测此值绘制错误画面。
    pub fn last_fatal(&self) -> Option<&str> {
        self.last_fatal.as_deref()
    }

    /// 输入事件泵（T030）：drain `events`，Key 事件经 `input::
    /// midp_keycode` 映射后以绿线程回调 current Canvas 的
    /// keyPressed/keyReleased（与 paint/Thread.start 同款"请求 →
    /// poll 消费 → spawn 回调绿线程"协议，T021/T033 模式）。
    ///
    /// 逐事件独立 spawn（多事件多线程，调度段轮转驱动）。任一事件
    /// 的前置不满足（未知键/无 Display/current 非 Canvas/子类未覆写
    /// 回调——find_method 落到 bootstrap 桩本体 body=None/构帧失败）
    /// 静默丢弃该事件，不影响后续事件与 VM 状态。
    ///
    /// TODO：keyRepeated 重复键策略——MVP 仅单次 press/release，长按
    /// 重复（引擎层连发 pressed 或 VM 层自造 repeated）待实证需求再定。
    fn pump_input_events(&mut self) {
        while let Some(event) = self.events.pop_front() {
            // current displayable 提前判定（T031：Key 分支的 suppress
            // 检查与指针回调共用前置）：无 Display/current 非 Canvas
            // 一律丢弃，不影响后续事件。
            let Some(displayable) = display::current_displayable(&mut self.cs, &self.heap) else {
                continue; // 无 Display/current：丢弃
            };
            let Ok(instance) = self.heap.instance(displayable) else {
                continue;
            };
            let runtime_class = instance.class;
            if !canvas::is_canvas_class(&self.cs, runtime_class) {
                continue; // current 非 Canvas（Screen 子体系留桩）
            }
            // 键：MIDP keyCode 单参；指针：坐标双参（kind 0按下/1抬起/2拖拽）。
            let (callback, desc, args) = match event {
                VmEvent::Key { key, pressed } => {
                    let Some(keycode) = input::midp_keycode(key) else {
                        continue; // 未知引擎键名：丢弃
                    };
                    // T031：键状态位图（GameCanvas.getKeyStates 数据面）
                    // ——press 按"物理键→keyCode→gameAction→位"置位；
                    // release 不清位（MIDP 锁存语义：位保持到
                    // getKeyStates 读取清零）。数字/软键无 gameAction →
                    // 无状态位。suppress 与否都置位（规范：suppress=
                    // false 时回调与位图两者都发生）。
                    if pressed && let Some(bit) = gamecanvas::state_bit(keycode) {
                        self.cs.key_states |= bit;
                    }
                    // T031：current 为 GameCanvas 子类且其实例
                    // suppressKeyEvents=true → 跳过键回调 spawn（事件
                    // 只进状态位）；指针事件不受 suppress 影响。
                    if gamecanvas::suppresses_key_events(&mut self.cs, &self.heap, displayable) {
                        continue;
                    }
                    (
                        if pressed { "keyPressed" } else { "keyReleased" },
                        input::KEY_CB_DESC,
                        vec![Slot::Int(keycode)],
                    )
                }
                VmEvent::Pointer { x, y, kind } => {
                    let name = match kind {
                        0 => "pointerPressed",
                        1 => "pointerReleased",
                        _ => "pointerDragged",
                    };
                    (
                        name,
                        input::POINTER_CB_DESC,
                        vec![Slot::Int(x as i32), Slot::Int(y as i32)],
                    )
                }
            };
            // 子类回调实现探测：find_method 沿父链解析；落到 bootstrap
            // 桩本体（body=None）即游戏未实现该回调 → 丢弃。
            let name = self.cs.syms.intern(callback);
            let desc_sym = self.cs.syms.intern(desc);
            let Some((defining, index)) = self.cs.find_method(runtime_class, name, desc_sym) else {
                continue;
            };
            if self.cs.class(defining).methods[index as usize]
                .body
                .is_none()
            {
                continue;
            }
            // 帧构：receiver=Canvas 实例、args=[Int(keyCode)]；clinit 无须
            // 补（实例存在 ⇒ 运行时类已初始化）。
            let mut frames = Vec::new();
            let pushed = {
                let host = &mut *self.host;
                let mut machine = Machine {
                    cs: &mut self.cs,
                    heap: &mut self.heap,
                    monitors: &mut self.monitors,
                    natives: self.natives.as_mut(),
                    host,
                };
                push_entry_frame(
                    &mut machine,
                    &mut frames,
                    runtime_class,
                    callback,
                    desc,
                    Some(displayable),
                    args,
                )
            };
            if pushed.is_ok() {
                self.scheduler.spawn(frames, "lcdui-input");
            }
        }
    }

    /// 段一：LCDUI paint 请求判定与回调线程拉起（T021）。
    ///
    /// 触发条件：无存活 paint 线程（调用方保证）且（全局重绘请求位
    /// 或 current 为 Canvas 且脏）。任一前置不满足、子类未覆写 paint
    /// （bootstrap 抽象桩）或屏图/Graphics 分配失败 → 清请求清脏
    /// 静默跳过（避免每 poll 重复探测）。
    fn maybe_spawn_paint(&mut self) {
        let requested = self.cs.repaint_requested;
        let Some(displayable) = display::current_displayable(&mut self.cs, &self.heap) else {
            // 未建 Display / current 为 null：请求作废。
            self.cs.repaint_requested = false;
            return;
        };
        let Ok(instance) = self.heap.instance(displayable) else {
            return;
        };
        let runtime_class = instance.class;
        if !canvas::is_canvas_class(&self.cs, runtime_class) {
            // current 非 Canvas（Form/List 等 Screen 子体系留桩）：请求作废。
            self.cs.repaint_requested = false;
            return;
        }
        let dirty = canvas::dirty_rect(&mut self.cs, &self.heap, displayable)
            .ok()
            .flatten();
        if !requested && dirty.is_none() {
            return;
        }
        // 请求位与脏位在此一次性消费（含下方所有跳过路径）。
        self.cs.repaint_requested = false;
        let _ = canvas::clear_dirty(&mut self.cs, &mut self.heap, displayable);
        // 子类 paint 实现探测：find_method 沿父链解析；落到 bootstrap
        // 抽象桩（body=None）即子类未覆写 → 跳过本帧。
        let name = self.cs.syms.intern("paint");
        let desc = self.cs.syms.intern(canvas::PAINT_DESC);
        let Some((defining, index)) = self.cs.find_method(runtime_class, name, desc) else {
            return;
        };
        if self.cs.class(defining).methods[index as usize]
            .body
            .is_none()
        {
            return;
        }
        // 屏图懒分配（host.screen_size）→ Graphics 绑定（目标=屏图像素
        // 数组，clip=脏矩形∩屏）。任一步失败静默放弃本帧。
        let Ok(screen) =
            canvas::ensure_screen(&mut self.cs, &mut self.heap, &mut *self.host, displayable)
        else {
            return;
        };
        if screen == ObjRef::NULL {
            return; // 宿主屏不可用（防御路径）
        }
        let (w, h) = (
            image::image_width(&mut self.cs, &self.heap, screen).unwrap_or(0),
            image::image_height(&mut self.cs, &self.heap, screen).unwrap_or(0),
        );
        if w <= 0 || h <= 0 {
            return;
        }
        let (x1, y1, x2, y2) = match dirty {
            // 脏矩形与屏求交（repaint() 全屏占位在此收敛到屏边界）。
            Some((dx, dy, dw, dh)) => (
                dx.max(0),
                dy.max(0),
                dx.saturating_add(dw).min(w),
                dy.saturating_add(dh).min(h),
            ),
            None => (0, 0, w, h),
        };
        if x2 <= x1 || y2 <= y1 {
            return; // 脏区域完全在屏外（或负尺寸并集退化）
        }
        let Ok(target) = image::pixels_array_ref(&mut self.cs, &self.heap, screen) else {
            return;
        };
        let Ok(default_font) = font::default_font(&mut self.cs, &mut self.heap, &mut *self.host)
        else {
            return;
        };
        let Ok(g) =
            graphics::alloc_graphics(&mut self.cs, &mut self.heap, target, w, h, default_font)
        else {
            return;
        };
        if graphics::set_clip_to(&mut self.cs, &mut self.heap, g, x1, y1, x2, y2).is_err() {
            return;
        }
        // 帧构：receiver=Canvas 实例、args=[Graphics]；clinit 无须补
        //（实例存在 ⇒ 运行时类已初始化）。
        let mut frames = Vec::new();
        let pushed = {
            let host = &mut *self.host;
            let mut machine = Machine {
                cs: &mut self.cs,
                heap: &mut self.heap,
                monitors: &mut self.monitors,
                natives: self.natives.as_mut(),
                host,
            };
            push_entry_frame(
                &mut machine,
                &mut frames,
                runtime_class,
                "paint",
                canvas::PAINT_DESC,
                Some(displayable),
                vec![Slot::Ref(g)],
            )
        };
        if pushed.is_err() {
            return;
        }
        let thread = self.scheduler.spawn(frames, "lcdui-paint");
        self.painting = Some(PaintSession {
            thread,
            canvas: displayable,
        });
    }

    /// 段三：Canvas 屏图 `[I`（0xAARRGGBB）逐像素拆 4 字节 RGBA8 呈现
    /// 宿主；无屏图/字段损坏静默跳过（防御）。
    fn present_screen(&mut self, canvas_obj: ObjRef) {
        if let Ok(Some((rgba, w, h))) = canvas::screen_rgba(&mut self.cs, &self.heap, canvas_obj) {
            self.host.present(&rgba, w, h);
        }
    }

    /// 按键事件入队：`key` 为引擎键名（`input` 模块值域约定），poll 段
    /// 前置转投 Canvas keyPressed/keyReleased（T030）。
    pub fn dispatch_key(&mut self, key: u8, pressed: bool) {
        self.events.push_back(VmEvent::Key { key, pressed });
    }

    /// 指针事件入队（TODO T031：poll 消费转投 pointerPressed 等）。
    pub fn dispatch_pointer(&mut self, x: i16, y: i16, kind: u8) {
        self.events.push_back(VmEvent::Pointer { x, y, kind });
    }

    /// 观测面：待消费事件数（引擎侧键/指针路由测试断言用）。
    pub fn pending_event_count(&self) -> usize {
        self.events.len()
    }

    /// 观测面：GameCanvas 键状态位图快照（T031，**不清零**——引擎侧/
    /// 测试断言用；Java 侧经 `getKeyStates()` 读取才清零）。
    pub fn key_states(&self) -> u32 {
        self.cs.key_states
    }

    /// 关停：清线程与事件，转入 Dead。可重入（重复调用无副作用）。
    /// TODO T026：先走 destroyApp 通知再释放。
    pub fn shutdown(&mut self) {
        self.scheduler = Scheduler::new();
        self.events.clear();
        self.state = VmState::Dead;
    }

    /// 是否在本类（不含父链）声明了指定方法（含父链会命中 bootstrap
    /// Object 的 native `<init>` 等占位方法，故限定本类）。
    fn declared_method(cs: &ClassSpace, class: ClassId, name: &str, desc: &str) -> Option<u16> {
        let name_sym = Sym(cs.syms.lookup(name)?);
        let desc_sym = Sym(cs.syms.lookup(desc)?);
        cs.class(class).declared_method(name_sym, desc_sym)
    }

    /// 是否存在指定名字/描述符的公开方法。
    fn has_public_method(cs: &ClassSpace, class: ClassId, name: &str, desc: &str) -> bool {
        match Self::declared_method(cs, class, name, desc) {
            Some(index) => cs.class(class).methods[index as usize].access_flags & acc::PUBLIC != 0,
            None => false,
        }
    }

    /// 是否存在指定名字/描述符的方法（不限访问级别；用于 `<init>`）。
    fn has_any_method(cs: &ClassSpace, class: ClassId, name: &str, desc: &str) -> bool {
        Self::declared_method(cs, class, name, desc).is_some()
    }
}

/// 解析 MANIFEST.MF：`Key: value` 行；以单空格起始的行是上一条的续行
/// （JAR 规范 72 字节折行）。行内 CRLF 由 `lines()` 归一。
fn parse_manifest(bytes: &[u8]) -> BTreeMap<String, String> {
    let text = String::from_utf8_lossy(bytes);
    let mut props: BTreeMap<String, String> = BTreeMap::new();
    let mut last_key: Option<String> = None;
    for line in text.lines() {
        if line.is_empty() {
            last_key = None;
            continue;
        }
        let cont = line.strip_prefix(' ').map(str::trim_end);
        if let (Some(key), Some(extra)) = (&last_key, cont)
            && let Some(value) = props.get_mut(key)
        {
            value.push_str(extra);
            continue;
        }
        if let Some((key, value)) = line.split_once(':') {
            props.insert(key.to_string(), value.trim().to_string());
            last_key = Some(key.to_string());
        }
    }
    props
}
