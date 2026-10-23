//! 手构 class 文件字节的共享测试构造器。
//!
//! 不依赖 fixture，聚焦运行时（类空间/GC/解释器）逻辑本身；
//! 语料级验证（jc.jar 全量加载）在 bootstrap 类（T010）就绪后进行。
//!
//! 既有教训（勿回退）：
//! - 常量池 `add1`/`add2` 返回**池槽位**（1 起），long/double 占 2 槽；
//! - 根类必须叫 `java/lang/Object`（解析器只允许它无父类）；
//! - 成员的 Some 属性分支必须先写 `attributes_count = 1`；
//! - 接口常量字段访问标志需 0x0019（public static final）。

use std::collections::HashMap;

use skyengine_kvm::runtime::classspace::MapClassProvider;

/// 手构类层次的根：classfile 解析器只允许 java/lang/Object 无父类。
pub const ROOT: &str = "java/lang/Object";

/// 常量池构造器：utf8/class 自动驻留；`slots` 计入 long/double 幻影槽。
#[derive(Default)]
pub struct Pool {
    entries: Vec<Vec<u8>>,
    utf8: HashMap<String, u16>,
    classes: HashMap<String, u16>,
    slots: u32,
}

impl Pool {
    pub fn new() -> Self {
        Self {
            entries: vec![Vec::new()], // 槽 0 占位
            utf8: HashMap::new(),
            classes: HashMap::new(),
            slots: 0,
        }
    }

    /// 返回池槽位（1 起）：long/double 幻影槽使 Vec 位置与槽位错开。
    fn add1(&mut self, entry: Vec<u8>) -> u16 {
        let slot = self.slots + 1;
        self.entries.push(entry);
        self.slots += 1;
        slot as u16
    }

    fn add2(&mut self, entry: Vec<u8>) -> u16 {
        let slot = self.slots + 1;
        self.entries.push(entry);
        self.slots += 2; // long/double 幻影槽
        slot as u16
    }

    pub fn utf8(&mut self, text: &str) -> u16 {
        if let Some(&index) = self.utf8.get(text) {
            return index;
        }
        let mut entry = vec![1u8];
        entry.extend_from_slice(&(text.len() as u16).to_be_bytes());
        entry.extend_from_slice(text.as_bytes());
        let index = self.add1(entry);
        self.utf8.insert(text.to_string(), index);
        index
    }

    /// 类条目驻留（幂等）：异常表在池区写出后仍会查询 catch 类
    /// （build_class 序列化期），必须命中缓存而不能新增条目。
    pub fn class(&mut self, name: &str) -> u16 {
        if let Some(&index) = self.classes.get(name) {
            return index;
        }
        let name_index = self.utf8(name);
        let mut entry = vec![7u8];
        entry.extend_from_slice(&name_index.to_be_bytes());
        let index = self.add1(entry);
        self.classes.insert(name.to_string(), index);
        index
    }

    pub fn nat(&mut self, name: &str, desc: &str) -> u16 {
        let name_index = self.utf8(name);
        let desc_index = self.utf8(desc);
        let mut entry = vec![12u8];
        entry.extend_from_slice(&name_index.to_be_bytes());
        entry.extend_from_slice(&desc_index.to_be_bytes());
        self.add1(entry)
    }

    pub fn fieldref(&mut self, class_name: &str, field: &str, desc: &str) -> u16 {
        let class_index = self.class(class_name);
        let nat_index = self.nat(field, desc);
        let mut entry = vec![9u8];
        entry.extend_from_slice(&class_index.to_be_bytes());
        entry.extend_from_slice(&nat_index.to_be_bytes());
        self.add1(entry)
    }

    pub fn methodref(&mut self, class_name: &str, method: &str, desc: &str) -> u16 {
        let class_index = self.class(class_name);
        let nat_index = self.nat(method, desc);
        let mut entry = vec![10u8];
        entry.extend_from_slice(&class_index.to_be_bytes());
        entry.extend_from_slice(&nat_index.to_be_bytes());
        self.add1(entry)
    }

    pub fn integer(&mut self, value: i32) -> u16 {
        let mut entry = vec![3u8];
        entry.extend_from_slice(&value.to_be_bytes());
        self.add1(entry)
    }

    pub fn float(&mut self, value: f32) -> u16 {
        let mut entry = vec![4u8];
        entry.extend_from_slice(&value.to_bits().to_be_bytes());
        self.add1(entry)
    }

    pub fn double(&mut self, value: f64) -> u16 {
        let mut entry = vec![6u8];
        entry.extend_from_slice(&value.to_bits().to_be_bytes());
        self.add2(entry)
    }

    pub fn long(&mut self, value: i64) -> u16 {
        let mut entry = vec![5u8];
        entry.extend_from_slice(&value.to_be_bytes());
        self.add2(entry)
    }

    pub fn string(&mut self, text: &str) -> u16 {
        let text_index = self.utf8(text);
        let mut entry = vec![8u8];
        entry.extend_from_slice(&text_index.to_be_bytes());
        self.add1(entry)
    }
}

pub struct FieldSpec {
    pub access: u16,
    pub name: &'static str,
    pub desc: &'static str,
    /// ConstantValue 引用的池索引。
    pub constant: Option<u16>,
}

/// 异常表条目（字节偏移；`end_pc` 排他）。
pub struct HandlerSpec {
    pub start_pc: u16,
    pub end_pc: u16,
    pub handler_pc: u16,
    /// 捕获类全名；None 为 finally 兜底（catch_type 0）。
    pub catch_class: Option<&'static str>,
}

pub struct MethodSpec {
    pub access: u16,
    pub name: &'static str,
    pub desc: &'static str,
    /// (max_stack, max_locals, 字节码)。
    pub code: Option<(u16, u16, Vec<u8>)>,
    /// 异常表（仅 `code` 为 Some 时写入）。
    pub handlers: Vec<HandlerSpec>,
}

/// 组装一份完整 class 文件。
pub fn build_class(
    pool: &mut Pool,
    this: &str,
    super_class: Option<&str>,
    interfaces: &[&str],
    fields: &[FieldSpec],
    methods: &[MethodSpec],
) -> Vec<u8> {
    // 预驻留：保证所有条目写入池区（后续查找全部命中缓存）。
    let _ = pool.class(this);
    if let Some(name) = super_class {
        let _ = pool.class(name);
    }
    for name in interfaces {
        let _ = pool.class(name);
    }
    for field in fields {
        let _ = pool.utf8(field.name);
        let _ = pool.utf8(field.desc);
        if field.constant.is_some() {
            let _ = pool.utf8("ConstantValue");
        }
    }
    for method in methods {
        let _ = pool.utf8(method.name);
        let _ = pool.utf8(method.desc);
        if method.code.is_some() {
            let _ = pool.utf8("Code");
            // 异常表 catch 类预驻留：池区写出后 class() 只能命中缓存。
            for handler in &method.handlers {
                if let Some(catch) = handler.catch_class {
                    let _ = pool.class(catch);
                }
            }
        }
    }

    let this_index = pool.class(this);
    let super_index = super_class.map(|name| pool.class(name)).unwrap_or(0);
    let iface_indices: Vec<u16> = interfaces.iter().map(|name| pool.class(name)).collect();

    let mut out = Vec::new();
    out.extend_from_slice(&0xCAFEBABEu32.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // minor
    out.extend_from_slice(&47u16.to_be_bytes()); // major
    let count_pos = out.len();
    out.extend_from_slice(&0u16.to_be_bytes()); // 池计数占位，稍后回填
    for entry in &pool.entries[1..] {
        out.extend_from_slice(entry);
    }
    out.extend_from_slice(&0u16.to_be_bytes()); // 类 access_flags
    out.extend_from_slice(&this_index.to_be_bytes());
    out.extend_from_slice(&super_index.to_be_bytes());
    out.extend_from_slice(&(iface_indices.len() as u16).to_be_bytes());
    for index in iface_indices {
        out.extend_from_slice(&index.to_be_bytes());
    }
    out.extend_from_slice(&(fields.len() as u16).to_be_bytes());
    for field in fields {
        out.extend_from_slice(&field.access.to_be_bytes());
        out.extend_from_slice(&pool.utf8(field.name).to_be_bytes());
        out.extend_from_slice(&pool.utf8(field.desc).to_be_bytes());
        match field.constant {
            Some(index) => {
                out.extend_from_slice(&1u16.to_be_bytes()); // 属性计数
                out.extend_from_slice(&pool.utf8("ConstantValue").to_be_bytes());
                out.extend_from_slice(&2u32.to_be_bytes());
                out.extend_from_slice(&index.to_be_bytes());
            }
            None => out.extend_from_slice(&0u16.to_be_bytes()),
        }
    }
    out.extend_from_slice(&(methods.len() as u16).to_be_bytes());
    for method in methods {
        out.extend_from_slice(&method.access.to_be_bytes());
        out.extend_from_slice(&pool.utf8(method.name).to_be_bytes());
        out.extend_from_slice(&pool.utf8(method.desc).to_be_bytes());
        if let Some((max_stack, max_locals, bytes)) = &method.code {
            let mut content = Vec::new();
            content.extend_from_slice(&max_stack.to_be_bytes());
            content.extend_from_slice(&max_locals.to_be_bytes());
            content.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            content.extend_from_slice(bytes);
            // 异常表：条目数 + 每条 4×u16（catch_type 0 = 兜底）。
            // catch 类已在预驻留期入池，此处只取缓存槽位。
            content.extend_from_slice(&(method.handlers.len() as u16).to_be_bytes());
            for handler in &method.handlers {
                let catch_index = handler
                    .catch_class
                    .map(|name| pool.class(name))
                    .unwrap_or(0);
                content.extend_from_slice(&handler.start_pc.to_be_bytes());
                content.extend_from_slice(&handler.end_pc.to_be_bytes());
                content.extend_from_slice(&handler.handler_pc.to_be_bytes());
                content.extend_from_slice(&catch_index.to_be_bytes());
            }
            content.extend_from_slice(&0u16.to_be_bytes()); // 嵌套属性
            out.extend_from_slice(&1u16.to_be_bytes()); // 属性计数
            out.extend_from_slice(&pool.utf8("Code").to_be_bytes());
            out.extend_from_slice(&(content.len() as u32).to_be_bytes());
            out.extend_from_slice(&content);
        } else {
            out.extend_from_slice(&0u16.to_be_bytes()); // 无属性
        }
    }
    out.extend_from_slice(&0u16.to_be_bytes()); // 类属性
    let count = pool.slots + 1; // 槽 0 占位
    out[count_pos..count_pos + 2].copy_from_slice(&(count as u16).to_be_bytes());
    out
}

/// 由 (类名 → class 字节) 构造类提供者。
pub fn provider(entries: Vec<(String, Vec<u8>)>) -> MapClassProvider {
    MapClassProvider::new(entries.into_iter().collect())
}

/// 构造无方法简单类（异常层次夹具用）。
fn simple_class(
    pool: &mut Pool,
    name: &'static str,
    super_class: &str,
    fields: &[FieldSpec],
) -> (String, Vec<u8>) {
    (
        name.to_string(),
        build_class(pool, name, Some(super_class), &[], fields, &[]),
    )
}

/// 标准异常层次 + java/lang/String（解释器测试夹具，T009）。
///
/// Throwable 持有 `detailMessage`（Fault 物化时写入消息，处理器可
/// getfield 读出）；其余类无字段无方法，仅提供继承链供处理器匹配。
pub fn exception_classes(pool: &mut Pool) -> Vec<(String, Vec<u8>)> {
    let mut entries = Vec::new();
    entries.push(simple_class(
        pool,
        "java/lang/Throwable",
        ROOT,
        &[FieldSpec {
            access: 0x0002, // private
            name: "detailMessage",
            desc: "Ljava/lang/String;",
            constant: None,
        }],
    ));
    entries.push(simple_class(pool, "java/lang/Exception", "java/lang/Throwable", &[]));
    entries.push(simple_class(
        pool,
        "java/lang/RuntimeException",
        "java/lang/Exception",
        &[],
    ));
    for name in [
        "java/lang/NullPointerException",
        "java/lang/ArithmeticException",
        "java/lang/ClassCastException",
        "java/lang/IndexOutOfBoundsException",
        "java/lang/NegativeArraySizeException",
        "java/lang/IllegalMonitorStateException",
        "java/lang/ArrayStoreException",
    ] {
        entries.push(simple_class(pool, name, "java/lang/RuntimeException", &[]));
    }
    entries.push(simple_class(
        pool,
        "java/lang/ArrayIndexOutOfBoundsException",
        "java/lang/IndexOutOfBoundsException",
        &[],
    ));
    entries.push(simple_class(pool, "java/lang/String", ROOT, &[]));
    entries
}
