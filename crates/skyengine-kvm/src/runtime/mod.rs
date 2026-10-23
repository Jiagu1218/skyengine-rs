//! 运行时基础类型：符号驻留、类/对象/字段/方法标识、操作数槽位。
//!
//! - `classspace`：类空间（加载-链接-解析，加载中占位防环）；
//! - `heap`：非移动对象表 + 字符串驻留；
//! - `object`：堆对象载荷（实例/数组/字符串）；
//! - `gc`：标记-清除收集（分配失败触发，静态区/帧槽为根）；
//! - `monitor`：monitor 表（重入计数 + wait/notify 等待队列）；
//! - 解释器（T009）与调度器（T011）只依赖本模块定义的抽象标识。

pub mod classspace;
pub mod gc;
pub mod heap;
pub mod monitor;
pub mod object;

use std::collections::HashMap;
use std::rc::Rc;

/// 驻留字符串符号 id。
///
/// 类名/字段名/方法名/描述符统一驻留后，运行期符号比较退化为整数比较。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Sym(pub u32);

/// 符号表：驻留即返回稳定 id，重复驻留返回同一 id。
#[derive(Debug, Default)]
pub struct Syms {
    map: HashMap<Rc<str>, u32>,
    names: Vec<Rc<str>>,
}

impl Syms {
    pub fn new() -> Self {
        Self::default()
    }

    /// 驻留一个字符串，返回其符号 id。
    pub fn intern(&mut self, text: &str) -> Sym {
        if let Some(id) = self.map.get(text) {
            return Sym(*id);
        }
        let id = self.names.len() as u32;
        let stored: Rc<str> = Rc::from(text);
        self.map.insert(Rc::clone(&stored), id);
        self.names.push(stored);
        Sym(id)
    }

    /// 查询文本的符号 id（不驻留；未见过返回 None）。
    pub fn lookup(&self, text: &str) -> Option<u32> {
        self.map.get(text).copied()
    }

    /// 查询符号对应的字符串（驻留后不变；非法 id 返回空串，仅防御）。
    pub fn name(&self, sym: Sym) -> &str {
        self.names
            .get(sym.0 as usize)
            .map(|text| text.as_ref())
            .unwrap_or("")
    }
}

/// 类空间中的类标识（`ClassSpace::classes` 索引）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ClassId(pub u32);

/// 堆对象引用（对象表索引）。索引 0 保留为 null。
/// PartialOrd/Ord（T033）：Thread 对象双向映射表以 ObjRef 为 BTreeMap 键。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjRef(pub u32);

impl ObjRef {
    pub const NULL: ObjRef = ObjRef(0);

    pub fn is_null(self) -> bool {
        self.0 == 0
    }
}

/// 绿线程标识（调度器 T011 分配；monitor 表与解释器只做比较）。
/// PartialOrd/Ord（T033）：生命周期表 thread_alive 以 ThreadId 为键。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ThreadId(pub u32);

/// 操作数栈/局部变量槽位。
///
/// 类别 2 值（long/double）占据两个槽：第二槽为 `Pad`，仅作占位不参与语义。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Slot {
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Ref(ObjRef),
    /// 类别 2 值的第二个槽。
    Pad,
}

impl Slot {
    /// 是否为类别 1（单槽）值。
    pub fn is_category1(self) -> bool {
        !matches!(self, Slot::Long(_) | Slot::Double(_) | Slot::Pad)
    }
}

/// 已解析的字段引用。
///
/// - 实例字段：`slot` 为继承链实例布局中的绝对槽位，
///   解释器直接以 `slot` 索引对象字段表（子类实例布局前缀兼容父类）。
/// - 静态字段：`slot` 为声明类静态存储中的槽位。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldRef {
    /// 声明该字段的类。
    pub owner: ClassId,
    pub slot: u16,
    pub is_static: bool,
    /// 字段值类别（链接期由描述符首字节解析）：热路径字段访问免逐次
    /// 扫描类字段表与查描述符符号（原 `field_kind` 每访问线性扫描）。
    pub kind: crate::classfile::code::Kind,
}

/// 已解析的方法引用。
///
/// 保留符号信息（名字 + 描述符）：虚调用/接口调用需要在接收者的
/// 运行时类上按 (name, desc) 查找目标方法；静态/特殊调用的
/// 具体目标由类空间（T006）的解析缓存加速。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MethodRef {
    /// 解析基类（引用中命名的类）。
    pub class: ClassId,
    pub name: Sym,
    pub desc: Sym,
}
