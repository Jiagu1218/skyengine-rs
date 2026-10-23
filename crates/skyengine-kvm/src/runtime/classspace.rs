//! 类空间：类的加载-链接-解析（eager 模型，plan.md D5）。
//!
//! ## 加载协议（防环的关键顺序）
//!
//! 1. 解析 ClassFile，把方法签名/字段/父类名/接口名注册为
//!    **"加载中占位"**（`by_name` 先行命中）——此后任何递归引用本类都拿到稳定 id；
//! 2. 布局：递归加载父类 → 接口 → 实例字段绝对槽位（父类前缀）+ 静态存储；
//! 3. 方法体解码（`code::decode` 经 `Linker` 递归加载引用类）。
//!
//! 不变式：**任何类的布局都先于其解码开始**。因此解码期间引用到的类
//! 只会处于"占位已注册且布局可完成/已完成"状态——
//! - 引用已解码完的类：一切就绪；
//! - 引用正在解码的类：其布局已完成；
//! - 引用正在布局的类（父类方法引用子类字段）：重入 `ensure_layout`
//!   时父类已可解析，可就地完成布局（`InProgress` 再入推进）；
//! - 真正的继承环：再入深度超过类总数，判定 `circular`。
//!
//! 任一环节失败会把占位类标记为 `failed`（粘滞失败），重试返回同一错误。

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use crate::KvmError;
use crate::classfile::ClassFile;
use crate::classfile::code::{self, CodeBody, Linker};
use crate::classfile::member::{ConstValue, acc};
use crate::interp::natives;
use crate::runtime::object::ElemType;
use crate::runtime::{ClassId, FieldRef, MethodRef, ObjRef, Slot, Sym, Syms, ThreadId};

/// null 引用常量（零值模板用）。
const NULL_REF: ObjRef = ObjRef::NULL;

/// 类字节来源：按内部名（如 `com/fs/A`）提供 class 文件字节。
pub trait ClassProvider {
    fn class_bytes(&mut self, name: &str) -> Option<Vec<u8>>;
}

/// 内存映射提供者：测试与 `install_jar` 共用。
#[derive(Debug, Default)]
pub struct MapClassProvider {
    entries: HashMap<String, Vec<u8>>,
}

impl MapClassProvider {
    pub fn new(entries: HashMap<String, Vec<u8>>) -> Self {
        Self { entries }
    }

    /// 从 jar 条目表构建：键形如 `com/fs/A.class`，去掉后缀作为内部名。
    pub fn from_jar(entries: &BTreeMap<String, Vec<u8>>) -> Self {
        let mut map = HashMap::with_capacity(entries.len());
        for (path, bytes) in entries {
            if let Some(name) = path.strip_suffix(".class") {
                map.insert(name.to_string(), bytes.clone());
            }
        }
        Self { entries: map }
    }
}

impl ClassProvider for MapClassProvider {
    fn class_bytes(&mut self, name: &str) -> Option<Vec<u8>> {
        self.entries.get(name).cloned()
    }
}

/// 数组类的元素种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArrayElem {
    Primitive(ElemType),
    /// 引用数组：组件类（数组类或普通类）。
    Class(ClassId),
}

/// 类初始化状态（`<clinit>` 延迟到首次主动使用，由解释器 T009 驱动）。
///
/// `Failed`：clinit 抛出异常后粘滞标记（JLS 12.4.2 步骤 7），
/// 后续主动使用按链接错误处理（映射 NoClassDefFoundError）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitState {
    Loaded,
    Initializing,
    Initialized,
    Failed,
}

/// 布局状态机。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LayoutState {
    Pending,
    InProgress,
    Done,
}

/// 静态字段常量初值。字符串延后到初始化期驻留成 String 对象（需要堆）。
#[derive(Clone, Debug)]
pub enum StaticConst {
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    /// 字符串字面量文本符号。
    Str(Sym),
}

/// 已注册的方法（签名先于方法体注册）。
#[derive(Debug)]
pub struct VmMethod {
    pub name: Sym,
    pub desc: Sym,
    pub access_flags: u16,
    /// 解码后的方法体；native/abstract 为 `None`（native 绑定 T010 接入）。
    pub body: Option<Rc<CodeBody>>,
}

/// 已布局的字段（仅本类声明的字段，父类字段经链查找）。
#[derive(Debug)]
pub struct VmField {
    pub name: Sym,
    pub desc: Sym,
    pub access_flags: u16,
    /// 实例字段：继承链绝对槽位；静态字段：本类静态存储槽位。
    pub slot: u16,
    pub constant: Option<StaticConst>,
}

impl VmField {
    pub fn is_static(&self) -> bool {
        self.access_flags & acc::STATIC != 0
    }
}

/// 类空间中的运行时类。
#[derive(Debug)]
pub struct VmClass {
    pub name: Sym,
    pub access_flags: u16,
    pub is_array: bool,
    /// 数组类的元素种类；普通类为 `None`。
    pub array_elem: Option<ArrayElem>,
    pub super_name: Option<Sym>,
    /// 懒解析的父类（布局期或首次引用时落实）。
    pub super_id: Option<ClassId>,
    pub iface_names: Vec<Sym>,
    pub iface_ids: Vec<ClassId>,
    ifaces_resolved: bool,
    /// 本类声明的字段（槽位在布局期回填）。
    pub fields: Vec<VmField>,
    /// (name, desc) → `fields` 索引。
    pub field_index: HashMap<(Sym, Sym), u16>,
    /// 静态存储（准备期零值 + 基本类型常量初值）。
    pub statics: Vec<Slot>,
    /// 实例零值模板（父类前缀；分配新实例时克隆进堆）。
    pub instance_template: Vec<Slot>,
    /// 实例字段总槽位（含父类）。
    pub instance_slot_count: u16,
    pub methods: Vec<VmMethod>,
    /// (name, desc) → `methods` 索引。
    pub method_index: HashMap<(Sym, Sym), u16>,
    pub init_state: InitState,
    /// 延迟驻留的静态字符串常量：(静态槽位, 字面量符号)。
    pub pending_str_consts: Vec<(u16, Sym)>,
    /// 粘滞失败标记：加载中断后重试返回同一错误。
    failed: Option<String>,
    layout: LayoutState,
}

impl VmClass {
    pub fn is_interface(&self) -> bool {
        self.access_flags & acc::INTERFACE != 0
    }

    /// 静态方法查找辅助（本类声明）。
    pub fn declared_method(&self, name: Sym, desc: Sym) -> Option<u16> {
        self.method_index.get(&(name, desc)).copied()
    }
}

/// 类空间：类表 + 名称索引 + 类字节来源。
pub struct ClassSpace {
    pub syms: Syms,
    classes: Vec<VmClass>,
    by_name: HashMap<Sym, ClassId>,
    provider: Box<dyn ClassProvider>,
    /// `java/lang/Object` 的类 id（T010 引导期设置；数组类的父类）。
    pub object_class: Option<ClassId>,
    /// MIDlet 套件属性（install_jar 时从 META-INF/MANIFEST.MF 解析）。
    /// `MIDlet.getAppProperty` 经此查询（T025 扩展 JAD 来源）。
    pub suite_properties: BTreeMap<String, String>,
    /// MIDlet 套件资源表（install_jar 时以非 class 条目填充，T018）：
    /// 键为条目原名（如 "skyicon.png"），值为 Rc 共享字节
    ///（`Image.createImage(String)` 数据源；Rc 避免数据本体复制）。
    pub suite_resources: BTreeMap<String, Rc<Vec<u8>>>,
    /// Font 实例缓存（T019，与 suite_properties 同型的运行态挂靠）：
    /// 键为归一化三元组 (face, style, size)（值域 0-2/0-7/0-2），
    /// 同三元组返回同实例（MIDP Font 无公开构造、可安全共享）。
    pub font_cache: BTreeMap<(u8, u8, u8), ObjRef>,
    /// Display 单例（T021，与 font_cache 同型的运行态挂靠）：
    /// `Display.getDisplay` 首次分配后固定复用（单 MIDlet 进程单例）。
    pub display_cache: Option<ObjRef>,
    /// 全局重绘请求位（T021）：`Canvas.serviceRepaints` 置位，
    /// `Vm.poll` 段一消费（下次 poll 立即拉起 paint 绿线程）。
    pub repaint_requested: bool,
    /// GameCanvas 键状态位图（T031，GameCanvas.getKeyStates 数据面）：
    /// 事件泵 press 置位 / release 不清（MIDP 锁存语义），
    /// `getKeyStates()` 读取返回并清零。u32 位序见
    /// `midp::lcdui::game::gamecanvas`（UP=bit1、LEFT=bit2、DOWN=bit6、
    /// RIGHT=bit7、FIRE=bit8、GAME_A-D=bit9-12）。
    pub key_states: u32,
    /// Thread.start 请求队列（T033）：native `start()` 无调度器通道，
    /// 沿 repaint_requested 同款"cs 侧请求位 + Vm.poll 消费"协议，
    /// 待 spawn 的 Thread 实例按 start 调用序入队。
    pub pending_threads: Vec<ObjRef>,
    /// Timer 登记请求队列（T034）：`Timer.schedule*` 类 native 无调度器
    /// 通道，沿 pending_threads 同款协议入队 (到期时刻, TimerTask 实例)；
    /// Vm.poll 段零消费 → `scheduler.schedule_timer`（到期 spawn run 线程）。
    pub pending_timers: Vec<(u64, ObjRef)>,
    /// 斯凯平台异步完成队列（SP-1）：`SkyPayment.req_async` 模拟平台
    /// "请求受理 → 异步完成通知"，把 tlv 响应字节数组入队；Vm.poll 段零
    /// 消费 → spawn 根级类 f 的静态 `a([B)V`（dispatchJNI）帧——它按
    /// objs[0] 的 moduleId<<16|serviceId 找到通道回调 c.skyJNIResult
    /// （真机由斯凯平台 native 层发起，此处对等模拟）。
    pub pending_sky_results: Vec<ObjRef>,
    /// Object.notify/notifyAll 请求队列（T034）：native 无 Monitors 通道，
    /// 置位 (锁对象, 请求线程, 是否 notifyAll)；解释器 native 分派点
    /// **同步消费**（须立即抛 IllegalMonitorStateException，不能等 poll）。
    pub notify_requests: Vec<(ObjRef, ThreadId, bool)>,
    /// ThreadId → Thread 实例对象（T033 双向映射）：
    /// `currentThread` 查表命中即返回；未命中（主线程首调）懒建补录。
    pub thread_objects: BTreeMap<ThreadId, ObjRef>,
    /// Thread 实例对象 → ThreadId（T033 双向映射）：
    /// `isAlive`/`join` 由 Thread 对象解析调度线程 id。
    pub thread_ids: BTreeMap<ObjRef, ThreadId>,
    /// 线程生命周期表（T033）：true=存活；false=已终止（条目保留，
    /// `isAlive` 查此表；调度器线程转 Dead 时回写 false）。
    pub thread_alive: BTreeMap<ThreadId, bool>,
    /// 默认线程名计数器（T033）：无名构造分配 "Thread-{N}" 后自增。
    pub thread_counter: u32,
    /// MIDlet 生命周期请求队列（T044）：`notifyDestroyed`/`notifyPaused`/
    /// `resumeRequest`/`System.exit` 类 native 无调度器通道，沿
    /// pending_threads 同款"cs 侧请求位 + Vm.poll 消费"协议入队；
    /// Vm.poll 段零 c 消费 → 对主 MIDlet 实例构帧回调
    /// （destroyApp/pauseApp/startApp）spawn lcdui-lifecycle 绿线程。
    pub pending_lifecycle: Vec<MidletLifecycleOp>,
    /// 最近一次未捕获异常死亡记录（T045）：调度器线程 Uncaught 终止时
    /// 写入 (线程 id, 类名文本)。Vm 侧两用：全线程死尽时升格 fatal
    /// （错误画面根因）；destroyApp 回调死亡时判定是否
    /// MIDletStateChangeException 拒绝销毁。
    pub last_uncaught: Option<(ThreadId, String)>,
    /// 方法描述符 → (参数类别表, 返回类别) 缓存（Sym 驻留键）：
    /// invoke/帧组装热路径免逐次字符串分配与描述符解析。
    desc_kinds: HashMap<Sym, Rc<(Vec<code::Kind>, Option<code::Kind>)>>,
    /// 方法调用计数（JIT 热点剖析/分层编译触发共用）：键 (定义类,
    /// 方法索引)。SKYENGINE_KVM_PROFILE=1 时累加并可经
    /// `invoke_profile_top` 取热点表。
    pub invoke_profile: HashMap<(ClassId, u16), u64>,
    /// JIT 分层计数器（x86_64；SKYENGINE_KVM_JIT=1 时递增）。
    #[cfg(target_arch = "x86_64")]
    pub(crate) jit_counters: HashMap<(ClassId, u16), u64>,
    /// JIT 编译产物缓存：键 (定义类, 方法索引)。
    #[cfg(target_arch = "x86_64")]
    pub(crate) jit_cache: HashMap<(ClassId, u16), Rc<crate::jit::JitCode>>,
}

/// 字段描述符首字节 → 值类别（链接期一次性，热路径直接读 FieldRef.kind）。
fn desc_kind(desc: &str) -> code::Kind {
    match desc.as_bytes().first() {
        Some(b'J') => code::Kind::Long,
        Some(b'D') => code::Kind::Double,
        Some(b'F') => code::Kind::Float,
        Some(b'L') | Some(b'[') => code::Kind::Ref,
        _ => code::Kind::Int,
    }
}

/// MIDlet 生命周期请求（T044）。native 侧只置位不执行——真实 AMS 的
/// 回调（destroyApp 等）由 Vm.poll 生命周期段拉起绿线程驱动。
///
/// 语义记录：本引擎 KVM 独占前台（无多 MIDlet 抢占场景），Pause/Resume
/// 只做状态回调不移交输入路由；Destroy 回调返回（线程死）后才转 Dead。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MidletLifecycleOp {
    /// `MIDlet.notifyPaused()`：请求 pauseApp 回调（状态记录，不停 VM）。
    PauseApp,
    /// `MIDlet.notifyDestroyed()` / `System.exit`：请求 destroyApp 回调。
    /// `unconditional=false` 时回调可抛 MIDletStateChangeException 拒绝
    /// 销毁（MIDP 规范语义）；true 时必须销毁。notifyDestroyed 置
    /// false、System.exit 置 true（引擎取舍：exit 为强制退出）。
    DestroyApp { unconditional: bool },
    /// `MIDlet.resumeRequest()`：请求 startApp 回调（恢复运行）。
    Resume,
}

impl ClassSpace {
    pub fn new(provider: Box<dyn ClassProvider>) -> Self {
        Self {
            syms: Syms::new(),
            classes: Vec::new(),
            by_name: HashMap::new(),
            provider,
            object_class: None,
            suite_properties: BTreeMap::new(),
            suite_resources: BTreeMap::new(),
            font_cache: BTreeMap::new(),
            display_cache: None,
            repaint_requested: false,
            key_states: 0,
            pending_threads: Vec::new(),
            pending_sky_results: Vec::new(),
            pending_timers: Vec::new(),
            notify_requests: Vec::new(),
            thread_objects: BTreeMap::new(),
            thread_ids: BTreeMap::new(),
            thread_alive: BTreeMap::new(),
            thread_counter: 0,
            pending_lifecycle: Vec::new(),
            last_uncaught: None,
            desc_kinds: HashMap::new(),
            invoke_profile: HashMap::new(),
            #[cfg(target_arch = "x86_64")]
            jit_counters: HashMap::new(),
            #[cfg(target_arch = "x86_64")]
            jit_cache: HashMap::new(),
        }
    }

    /// JIT 分层计数递增，返回新值。
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn jit_bump(&mut self, class: ClassId, index: u16) -> u64 {
        *self.jit_counters.entry((class, index)).or_insert(0) += 1;
        self.jit_counters[&(class, index)]
    }

    /// JIT 编译产物查询。
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn jit_cache_get(&self, class: ClassId, index: u16) -> Option<Rc<crate::jit::JitCode>> {
        self.jit_cache.get(&(class, index)).cloned()
    }

    /// JIT 编译产物存入。
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn jit_store(&mut self, class: ClassId, index: u16, code: Rc<crate::jit::JitCode>) {
        self.jit_cache.insert((class, index), code);
    }

    /// 剖析热点表：调用次数降序前 `limit` 项（类名.方法名+描述符，
    /// 调用数）。SKYENGINE_KVM_PROFILE 开启时才有数据。
    pub fn invoke_profile_top(&self, limit: usize) -> Vec<(String, u64)> {
        let mut rows: Vec<((ClassId, u16), u64)> =
            self.invoke_profile.iter().map(|(k, v)| (*k, *v)).collect();
        rows.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        rows.into_iter()
            .take(limit)
            .map(|((class, index), count)| {
                let method = &self.class(class).methods[index as usize];
                let text = format!(
                    "{}.{}{}",
                    self.name_of(class),
                    self.syms.name(method.name),
                    self.syms.name(method.desc)
                );
                (text, count)
            })
            .collect()
    }

    /// 描述符 → (参数类别表, 返回类别)：首用解析后按 Sym 缓存共享
    ///（invoke 每调用一次 String 分配 + 解析的开销归零）。
    pub fn descriptor_kinds(
        &mut self,
        desc: Sym,
    ) -> Result<Rc<(Vec<code::Kind>, Option<code::Kind>)>, KvmError> {
        if let Some(cached) = self.desc_kinds.get(&desc) {
            return Ok(Rc::clone(cached));
        }
        let text = self.syms.name(desc);
        let parsed = Rc::new(crate::interp::parse_method_desc(text)?);
        self.desc_kinds.insert(desc, Rc::clone(&parsed));
        Ok(parsed)
    }

    pub fn class(&self, id: ClassId) -> &VmClass {
        self.classes
            .get(id.0 as usize)
            .expect("class id out of range")
    }

    pub fn class_mut(&mut self, id: ClassId) -> &mut VmClass {
        self.classes
            .get_mut(id.0 as usize)
            .expect("class id out of range")
    }

    pub fn class_count(&self) -> usize {
        self.classes.len()
    }

    /// 按名查找（不加载）。
    pub fn find(&self, name: &str) -> Option<ClassId> {
        let sym = Sym(self.syms.lookup(name)?);
        self.by_name.get(&sym).copied()
    }

    /// 类内部名。
    pub fn name_of(&self, id: ClassId) -> &str {
        self.syms.name(self.class(id).name)
    }

    /// 加载（含链接/布局/方法体解码），幂等；加载中占位防环。
    pub fn load_class(&mut self, name: &str) -> Result<ClassId, KvmError> {
        let sym = self.syms.intern(name);
        if let Some(id) = self.by_name.get(&sym).copied() {
            return self.check_loaded(id, name);
        }
        if name.starts_with('[') {
            return self.synth_array(sym, name);
        }
        self.define(sym, name)
    }

    fn check_loaded(&self, id: ClassId, name: &str) -> Result<ClassId, KvmError> {
        if let Some(message) = &self.class(id).failed {
            return Err(KvmError::Linkage(format!(
                "earlier load of {name} failed: {message}"
            )));
        }
        Ok(id)
    }

    fn define(&mut self, sym: Sym, name: &str) -> Result<ClassId, KvmError> {
        // 优先级 1：bootstrap 注册表（Object/Class/异常体系等无字节码声明类）。
        if let Some(decl) = natives::bootstrap_lookup(name) {
            return self.define_bootstrap(sym, decl);
        }
        // 优先级 2：jar classfile；都未命中 → NoClassDefFoundError 语义。
        // 解析失败发生在占位注册之前：无污染，可直接重试。
        let bytes = self
            .provider
            .class_bytes(name)
            .ok_or_else(|| KvmError::ClassNotFound(name.to_string()))?;
        let class_file = ClassFile::parse(&bytes)?;
        let id = self.register_placeholder(sym, &class_file)?;
        if let Err(error) = self.finish_define(id, &class_file) {
            self.class_mut(id).failed = Some(error.to_string());
            return Err(error);
        }
        Ok(id)
    }

    /// 用 bootstrap 声明构造类：无字节码，方法体为 None 走解释器
    /// 原生分派点（opcodes invoke*），由 `RegistryDispatch` 查表绑定。
    fn define_bootstrap(
        &mut self,
        sym: Sym,
        decl: &natives::BootstrapDecl,
    ) -> Result<ClassId, KvmError> {
        let id = self.register_bootstrap(sym, decl)?;
        if let Err(error) = self.ensure_layout(id) {
            self.class_mut(id).failed = Some(error.to_string());
            return Err(error);
        }
        // Object 落位后记录，供数组类等作为隐式父类。
        if decl.name == "java/lang/Object" {
            self.object_class = Some(id);
        }
        Ok(id)
    }

    /// 注册 bootstrap 占位（签名/字段先于布局，范式同 `register_placeholder`）。
    fn register_bootstrap(
        &mut self,
        sym: Sym,
        decl: &natives::BootstrapDecl,
    ) -> Result<ClassId, KvmError> {
        let mut methods = Vec::with_capacity(decl.methods.len());
        let mut method_index = HashMap::with_capacity(decl.methods.len());
        for (index, method) in decl.methods.iter().enumerate() {
            let name = self.syms.intern(method.name);
            let desc = self.syms.intern(method.desc);
            if method_index.insert((name, desc), index as u16).is_some() {
                return Err(KvmError::ClassFormat(format!(
                    "duplicate bootstrap method {}{} in {}",
                    method.name, method.desc, decl.name
                )));
            }
            // 默认 native（body=None 即分派键）；接口 abstract 方法保持
            // abstract，运行期调用按链接错误拒绝（实现由 T014 落实）。
            let access_flags = if method.access_flags & acc::ABSTRACT != 0 {
                method.access_flags
            } else {
                method.access_flags | acc::NATIVE
            };
            methods.push(VmMethod {
                name,
                desc,
                access_flags,
                body: None,
            });
        }
        let mut fields = Vec::with_capacity(decl.fields.len());
        let mut field_index = HashMap::with_capacity(decl.fields.len());
        for (index, field) in decl.fields.iter().enumerate() {
            let name = self.syms.intern(field.name);
            let desc = self.syms.intern(field.desc);
            if field_index.insert((name, desc), index as u16).is_some() {
                return Err(KvmError::ClassFormat(format!(
                    "duplicate bootstrap field {} {} in {}",
                    field.desc, field.name, decl.name
                )));
            }
            let constant = field
                .constant
                .as_ref()
                .map(|value| natives::to_static_const(self, value));
            fields.push(VmField {
                name,
                desc,
                access_flags: field.access_flags,
                slot: 0,
                constant,
            });
        }
        let super_name = decl.super_name.map(|name| self.syms.intern(name));
        let iface_names = decl
            .interfaces
            .iter()
            .map(|name| self.syms.intern(name))
            .collect();
        let id = ClassId(self.classes.len() as u32);
        self.classes.push(VmClass {
            name: sym,
            access_flags: decl.access_flags,
            is_array: false,
            array_elem: None,
            super_name,
            super_id: None,
            iface_names,
            iface_ids: Vec::new(),
            ifaces_resolved: false,
            fields,
            field_index,
            statics: Vec::new(),
            instance_template: Vec::new(),
            instance_slot_count: 0,
            methods,
            method_index,
            init_state: InitState::Loaded,
            pending_str_consts: Vec::new(),
            failed: None,
            layout: LayoutState::Pending,
        });
        self.by_name.insert(sym, id);
        Ok(id)
    }

    /// 注册加载中占位：签名/字段先于一切递归可见。
    fn register_placeholder(&mut self, sym: Sym, cf: &ClassFile) -> Result<ClassId, KvmError> {
        let mut methods = Vec::with_capacity(cf.methods.len());
        let mut method_index = HashMap::with_capacity(cf.methods.len());
        for (index, method) in cf.methods.iter().enumerate() {
            let name = self.syms.intern(&method.name);
            let desc = self.syms.intern(&method.descriptor);
            if method_index.insert((name, desc), index as u16).is_some() {
                return Err(KvmError::ClassFormat(format!(
                    "duplicate method {}{} in {}",
                    method.name, method.descriptor, cf.this_class
                )));
            }
            // 具体方法必须有 Code（native/abstract 除外）。
            if method.code.is_none() && !method.is_native() && !method.is_abstract() {
                return Err(KvmError::ClassFormat(format!(
                    "concrete method {}{} has no Code attribute",
                    method.name, method.descriptor
                )));
            }
            methods.push(VmMethod {
                name,
                desc,
                access_flags: method.access_flags,
                body: None,
            });
        }
        let mut fields = Vec::with_capacity(cf.fields.len());
        let mut field_index = HashMap::with_capacity(cf.fields.len());
        for (index, field) in cf.fields.iter().enumerate() {
            let name = self.syms.intern(&field.name);
            let desc = self.syms.intern(&field.descriptor);
            if field_index.insert((name, desc), index as u16).is_some() {
                return Err(KvmError::ClassFormat(format!(
                    "duplicate field {} {} in {}",
                    field.descriptor, field.name, cf.this_class
                )));
            }
            // ConstantValue 仅对静态字段生效（规范：非静态忽略）。
            let constant = if field.is_static() {
                self.resolve_static_const(cf, field.constant_value.as_ref())?
            } else {
                None
            };
            fields.push(VmField {
                name,
                desc,
                access_flags: field.access_flags,
                slot: 0,
                constant,
            });
        }
        let super_name = cf.super_class.as_deref().map(|name| self.syms.intern(name));
        let iface_names = cf
            .interfaces
            .iter()
            .map(|name| self.syms.intern(name))
            .collect();
        let id = ClassId(self.classes.len() as u32);
        self.classes.push(VmClass {
            name: sym,
            access_flags: cf.access_flags,
            is_array: false,
            array_elem: None,
            super_name,
            super_id: None,
            iface_names,
            iface_ids: Vec::new(),
            ifaces_resolved: false,
            fields,
            field_index,
            statics: Vec::new(),
            instance_template: Vec::new(),
            instance_slot_count: 0,
            methods,
            method_index,
            init_state: InitState::Loaded,
            pending_str_consts: Vec::new(),
            failed: None,
            layout: LayoutState::Pending,
        });
        self.by_name.insert(sym, id);
        Ok(id)
    }

    /// ConstantValue → 静态常量初值（字符串解析成文本符号，初始化期驻留）。
    fn resolve_static_const(
        &mut self,
        cf: &ClassFile,
        value: Option<&ConstValue>,
    ) -> Result<Option<StaticConst>, KvmError> {
        let Some(value) = value else {
            return Ok(None);
        };
        Ok(Some(match value {
            ConstValue::Int(v) => StaticConst::Int(*v),
            ConstValue::Long(v) => StaticConst::Long(*v),
            ConstValue::Float(v) => StaticConst::Float(*v),
            ConstValue::Double(v) => StaticConst::Double(*v),
            ConstValue::Str(index) => {
                let loadable = cf
                    .constant_pool
                    .loadable(*index)
                    .map_err(|error| KvmError::ClassFormat(error.to_string()))?;
                match loadable {
                    crate::classfile::cp::Loadable::Str(text) => {
                        StaticConst::Str(self.syms.intern(&text))
                    }
                    _ => {
                        return Err(KvmError::ClassFormat(
                            "ConstantValue does not reference a string".into(),
                        ));
                    }
                }
            }
        }))
    }

    /// 布局 + 方法体解码（占位注册之后的阶段；失败粘滞标记）。
    fn finish_define(&mut self, id: ClassId, cf: &ClassFile) -> Result<(), KvmError> {
        self.ensure_layout(id)?;
        self.decode_bodies(id, cf)?;
        Ok(())
    }

    /// 惰性加载父类（幂等）。
    pub fn ensure_super(&mut self, id: ClassId) -> Result<Option<ClassId>, KvmError> {
        if let Some(super_id) = self.class(id).super_id {
            return Ok(Some(super_id));
        }
        let Some(name) = self.class(id).super_name else {
            return Ok(None);
        };
        let super_id = self.load_by_sym(name)?;
        self.class_mut(id).super_id = Some(super_id);
        Ok(Some(super_id))
    }

    fn load_by_sym(&mut self, sym: Sym) -> Result<ClassId, KvmError> {
        if let Some(id) = self.by_name.get(&sym).copied() {
            let name = self.syms.name(sym).to_string();
            return self.check_loaded(id, &name);
        }
        let name = self.syms.name(sym).to_string();
        self.load_class(&name)
    }

    /// 惰性解析接口列表（幂等）。
    pub fn resolve_interfaces(&mut self, id: ClassId) -> Result<(), KvmError> {
        if self.class(id).ifaces_resolved {
            return Ok(());
        }
        let names = self.class(id).iface_names.clone();
        let mut ids = Vec::with_capacity(names.len());
        for name in names {
            ids.push(self.load_by_sym(name)?);
        }
        let vm = self.class_mut(id);
        vm.iface_ids = ids;
        vm.ifaces_resolved = true;
        Ok(())
    }

    /// 确保布局完成（幂等；容忍解码回调引入的良性再入）。
    pub fn ensure_layout(&mut self, id: ClassId) -> Result<(), KvmError> {
        self.layout_inner(id, 0)
    }

    fn layout_inner(&mut self, id: ClassId, depth: usize) -> Result<(), KvmError> {
        // 再入深度超过类总数 = 继承环（合法链必然短于类总数）。
        if depth > self.class_count() {
            return Err(KvmError::Linkage(format!(
                "circular class hierarchy involving {}",
                self.name_of(id)
            )));
        }
        match self.class(id).layout {
            LayoutState::Done => return Ok(()),
            LayoutState::Pending => self.class_mut(id).layout = LayoutState::InProgress,
            LayoutState::InProgress => {}
        }
        let super_id = self.ensure_super(id)?;
        if let Some(super_id) = super_id {
            self.layout_inner(super_id, depth + 1)?;
        }
        self.resolve_interfaces(id)?;
        // 良性再入：递归期间本类布局已被内层调用完成。
        if self.class(id).layout == LayoutState::Done {
            return Ok(());
        }
        self.layout_fields(id, super_id)?;
        self.class_mut(id).layout = LayoutState::Done;
        Ok(())
    }

    /// 字段布局：静态槽位从 0 起；实例槽位接在父类模板之后。
    fn layout_fields(&mut self, id: ClassId, super_id: Option<ClassId>) -> Result<(), KvmError> {
        let mut template = match super_id {
            Some(super_id) => self.class(super_id).instance_template.clone(),
            None => Vec::new(),
        };
        let mut base = template.len() as u32;
        let mut statics: Vec<Slot> = Vec::new();
        let mut pending: Vec<(u16, Sym)> = Vec::new();
        let count = self.class(id).fields.len();
        let mut slots = vec![0u16; count];
        // 索引式遍历：循环体内 apply_static_const 需 &mut self，
        // 持有 fields 迭代器会与之冲突（needless_range_loop 豁免原因）。
        #[allow(clippy::needless_range_loop)]
        for index in 0..count {
            let (desc, constant, is_static) = {
                let field = &self.class(id).fields[index];
                (field.desc, field.constant.clone(), field.is_static())
            };
            let width = self.slot_width(desc);
            let slot = if is_static {
                statics.len() as u32
            } else {
                base
            };
            if slot + width as u32 > u32::from(u16::MAX) {
                return Err(KvmError::Linkage(format!(
                    "field layout overflow in {}",
                    self.name_of(id)
                )));
            }
            // 零值模板/存储。
            let zero = self.zero_slot(desc);
            if is_static {
                statics.push(zero);
                if width == 2 {
                    statics.push(Slot::Pad);
                }
                if let Some(constant) = constant {
                    self.apply_static_const(&mut statics, slot as u16, &constant);
                    if let StaticConst::Str(sym) = constant {
                        pending.push((slot as u16, sym));
                    }
                }
            } else {
                template.push(zero);
                if width == 2 {
                    template.push(Slot::Pad);
                }
                base += width as u32;
            }
            slots[index] = slot as u16;
        }
        let vm = self.class_mut(id);
        for (index, slot) in slots.into_iter().enumerate() {
            vm.fields[index].slot = slot;
        }
        vm.statics = statics;
        vm.instance_template = template;
        vm.instance_slot_count = base as u16;
        vm.pending_str_consts = pending;
        Ok(())
    }

    /// 描述符的槽位宽度（long/double 占 2 槽）。
    fn slot_width(&self, desc: Sym) -> u8 {
        match self.syms.name(desc).as_bytes().first() {
            Some(b'J') | Some(b'D') => 2,
            _ => 1,
        }
    }

    /// 描述符的零值槽。
    fn zero_slot(&self, desc: Sym) -> Slot {
        match self.syms.name(desc).as_bytes().first() {
            Some(b'J') => Slot::Long(0),
            Some(b'D') => Slot::Double(0.0),
            Some(b'L') | Some(b'[') => Slot::Ref(NULL_REF),
            _ => Slot::Int(0),
        }
    }

    /// 应用基本类型常量初值（字符串延后，见 `pending_str_consts`）。
    fn apply_static_const(&mut self, statics: &mut [Slot], slot: u16, constant: &StaticConst) {
        match constant {
            StaticConst::Int(v) => statics[slot as usize] = Slot::Int(*v),
            StaticConst::Long(v) => statics[slot as usize] = Slot::Long(*v),
            StaticConst::Float(v) => statics[slot as usize] = Slot::Float(*v),
            StaticConst::Double(v) => statics[slot as usize] = Slot::Double(*v),
            StaticConst::Str(_) => {}
        }
    }

    /// 解码全部方法体（引用的类经 `Linker` 递归加载）。
    fn decode_bodies(&mut self, id: ClassId, cf: &ClassFile) -> Result<(), KvmError> {
        let cp = Rc::clone(&cf.constant_pool);
        let mut bodies = Vec::with_capacity(cf.methods.len());
        for method in &cf.methods {
            let body = match &method.code {
                Some(info) => Some(code::decode(info, &cp, self)?),
                None => None,
            };
            bodies.push(body);
        }
        let vm = self.class_mut(id);
        for (index, body) in bodies.into_iter().enumerate() {
            vm.methods[index].body = body;
        }
        Ok(())
    }

    /// 合成数组类（`[I` / `[Ljava/lang/String;` / `[[I`）。
    fn synth_array(&mut self, sym: Sym, name: &str) -> Result<ClassId, KvmError> {
        let component = name
            .get(1..)
            .ok_or_else(|| KvmError::ClassFormat(format!("bad array descriptor {name}")))?;
        let first = component.as_bytes().first().copied();
        let valid = matches!(
            first,
            Some(b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D')
        ) || (first == Some(b'L') && component.ends_with(';'))
            || first == Some(b'[');
        if !valid {
            return Err(KvmError::ClassFormat(format!(
                "bad array component descriptor {name}"
            )));
        }
        // 先注册占位（组件类的字节码可能反向引用本数组类）。
        let id = ClassId(self.classes.len() as u32);
        self.classes.push(VmClass {
            name: sym,
            access_flags: acc::PUBLIC,
            is_array: true,
            array_elem: None,
            super_name: None,
            super_id: self.object_class,
            iface_names: Vec::new(),
            iface_ids: Vec::new(),
            ifaces_resolved: true,
            fields: Vec::new(),
            field_index: HashMap::new(),
            statics: Vec::new(),
            instance_template: Vec::new(),
            instance_slot_count: 0,
            methods: Vec::new(),
            method_index: HashMap::new(),
            init_state: InitState::Loaded,
            pending_str_consts: Vec::new(),
            failed: None,
            layout: LayoutState::Done,
        });
        self.by_name.insert(sym, id);
        let elem = match first.unwrap() {
            b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D' => ArrayElem::Primitive(
                ElemType::from_desc(first.unwrap() as char).expect("validated above"),
            ),
            b'L' => ArrayElem::Class(self.load_class(&component[1..component.len() - 1])?),
            _ => ArrayElem::Class(self.load_class(component)?),
        };
        self.class_mut(id).array_elem = Some(elem);
        Ok(id)
    }

    /// 字段解析：本类/父链按 (name, desc)；静态字段附加搜索接口闭包。
    pub fn find_field(
        &mut self,
        start: ClassId,
        name: Sym,
        desc: Sym,
    ) -> Result<FieldRef, KvmError> {
        self.ensure_layout(start)?;
        self.find_field_inner(start, name, desc)
            .ok_or_else(|| self.field_not_found(start, name, desc))
    }

    fn find_field_inner(&self, start: ClassId, name: Sym, desc: Sym) -> Option<FieldRef> {
        let mut current = Some(start);
        while let Some(class_id) = current {
            let vm = self.class(class_id);
            if let Some(&index) = vm.field_index.get(&(name, desc)) {
                let field = &vm.fields[index as usize];
                return Some(FieldRef {
                    owner: class_id,
                    slot: field.slot,
                    is_static: field.is_static(),
                    kind: desc_kind(self.syms.name(field.desc)),
                });
            }
            current = vm.super_id;
        }
        // 接口闭包（接口字段隐式 static final）。
        for class_id in self.interface_closure(start) {
            let vm = self.class(class_id);
            if let Some(&index) = vm.field_index.get(&(name, desc)) {
                let field = &vm.fields[index as usize];
                return Some(FieldRef {
                    owner: class_id,
                    slot: field.slot,
                    is_static: true,
                    kind: desc_kind(self.syms.name(field.desc)),
                });
            }
        }
        None
    }

    /// 方法查找：本类 → 父链 → 接口闭包。返回 (定义类, 方法索引)。
    pub fn find_method(&self, start: ClassId, name: Sym, desc: Sym) -> Option<(ClassId, u16)> {
        let mut current = Some(start);
        while let Some(class_id) = current {
            let vm = self.class(class_id);
            if let Some(&index) = vm.method_index.get(&(name, desc)) {
                return Some((class_id, index));
            }
            current = vm.super_id;
        }
        // 接口闭包：接口方法与其父接口；类实现的接口方法兜底。
        for class_id in self.interface_closure(start) {
            let vm = self.class(class_id);
            if let Some(&index) = vm.method_index.get(&(name, desc)) {
                return Some((class_id, index));
            }
        }
        None
    }

    /// 类的接口闭包：自身及父链声明的全部接口（含传递接口），去重。
    pub fn interface_closure(&self, start: ClassId) -> Vec<ClassId> {
        let mut result: Vec<ClassId> = Vec::new();
        let mut queue: Vec<ClassId> = Vec::new();
        let mut current = Some(start);
        while let Some(class_id) = current {
            queue.extend(self.class(class_id).iface_ids.iter().copied());
            current = self.class(class_id).super_id;
        }
        while let Some(class_id) = queue.pop() {
            if result.contains(&class_id) {
                continue;
            }
            result.push(class_id);
            queue.extend(self.class(class_id).iface_ids.iter().copied());
        }
        result
    }

    /// 子类判断（数组/接口兼容由解释器特判；此处处理普通类链 + 接口实现）。
    pub fn is_subclass_of(&self, subclass: ClassId, superclass: ClassId) -> bool {
        let mut current = Some(subclass);
        while let Some(class_id) = current {
            if class_id == superclass {
                return true;
            }
            current = self.class(class_id).super_id;
        }
        self.interface_closure(subclass).contains(&superclass)
    }
    /// 字段解析失败信息带描述符（同名不同类型字段是合法遮蔽）。
    fn field_not_found(&self, start: ClassId, name: Sym, desc: Sym) -> KvmError {
        KvmError::Linkage(format!(
            "field {} {} not found on {}",
            self.syms.name(desc),
            self.syms.name(name),
            self.name_of(start)
        ))
    }
}

impl Linker for ClassSpace {
    fn link_class(&mut self, name: &str) -> Result<ClassId, KvmError> {
        self.load_class(name)
    }

    fn link_field(
        &mut self,
        owner: ClassId,
        name: &str,
        desc: &str,
        is_static: bool,
    ) -> Result<FieldRef, KvmError> {
        let name = self.syms.intern(name);
        let desc = self.syms.intern(desc);
        let field = self.find_field(owner, name, desc)?;
        if field.is_static != is_static {
            return Err(KvmError::Linkage(format!(
                "field {}::{} used as {} but is {}",
                self.name_of(field.owner),
                self.syms.name(name),
                if is_static { "static" } else { "instance" },
                if field.is_static {
                    "static"
                } else {
                    "instance"
                },
            )));
        }
        Ok(field)
    }

    fn link_method(
        &mut self,
        class: ClassId,
        name: &str,
        desc: &str,
    ) -> Result<MethodRef, KvmError> {
        let name_sym = self.syms.intern(name);
        let desc_sym = self.syms.intern(desc);
        // 占位类可能尚未完成布局：先落实父类与接口再查找。
        self.ensure_super(class)?;
        self.resolve_interfaces(class)?;
        match self.find_method(class, name_sym, desc_sym) {
            Some((defining, _)) => Ok(MethodRef {
                class: defining,
                name: name_sym,
                desc: desc_sym,
            }),
            None => Err(KvmError::Linkage(format!(
                "method {}{} not found on {}",
                name,
                desc,
                self.name_of(class)
            ))),
        }
    }
}
