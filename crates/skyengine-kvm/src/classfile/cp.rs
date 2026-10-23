//! 常量池解析：两遍解析（第一遍切分原始 tag/字节区间，第二遍解出结构化条目）。

use std::rc::Rc;

/// class 文件常量池 tag（JVM 规范 §4.4）。
pub mod tag {
    pub const CLASS: u8 = 7;
    pub const FIELDREF: u8 = 9;
    pub const METHODREF: u8 = 10;
    pub const INTERFACE_METHODREF: u8 = 11;
    pub const STRING: u8 = 8;
    pub const INTEGER: u8 = 3;
    pub const FLOAT: u8 = 4;
    pub const LONG: u8 = 5;
    pub const DOUBLE: u8 = 6;
    pub const NAME_AND_TYPE: u8 = 12;
    pub const UTF8: u8 = 1;
    pub const METHOD_HANDLE: u8 = 15;
    pub const METHOD_TYPE: u8 = 16;
    pub const DYNAMIC: u8 = 17;
    pub const INVOKE_DYNAMIC: u8 = 18;
    pub const MODULE: u8 = 19;
    pub const PACKAGE: u8 = 20;
}

/// 解析后的常量池条目。
#[derive(Clone, Debug)]
pub enum CpEntry {
    /// 常量类引用：指向 utf8 名字索引。
    Class(u16),
    Fieldref { class: u16, nat: u16 },
    Methodref { class: u16, nat: u16 },
    InterfaceMethodref { class: u16, nat: u16 },
    /// 字符串字面量：指向 utf8 索引。
    String(u16),
    Integer(i32),
    Float(f32),
    Long(i64),
    Double(f64),
    NameAndType { name: u16, desc: u16 },
    Utf8(Rc<str>),
    /// long/double 占据两个池槽，第二个槽是幻影占位。
    Phantom,
}

/// 第一遍解析得到的原始条目：tag + 未解释字节（定长）。
struct RawEntry {
    tag: u8,
    /// 原始定长载荷（引用类 2/4 字节、数值 4/8 字节、utf8 变长已展开成内容）。
    payload: Payload,
}

enum Payload {
    /// 单 u16 索引（Class/STRING）。
    Index(u16),
    /// 两个 u16（Fieldref/Methodref/NameAndType）。
    Pair(u16, u16),
    /// 4 字节数值（Integer/Float）。
    Bytes4([u8; 4]),
    /// 8 字节数值（Long/Double）。
    Bytes8([u8; 8]),
    /// 已读取的 Modified-UTF8 文本。
    Text(Rc<str>),
    /// 不识别但按长度跳过的条目（METHOD_HANDLE 等）。
    Skipped,
}

/// 常量池。
#[derive(Clone, Debug, Default)]
pub struct ConstPool {
    /// `entries[0]` 不可用（JVM 规范），用 Phantom 占位保持 1 基索引。
    entries: Vec<CpEntry>,
}

/// 常量池解析错误。
#[derive(Debug, thiserror::Error)]
#[error("constant pool: {0}")]
pub struct CpError(pub String);

pub type CpResult<T> = Result<T, CpError>;

impl ConstPool {
    /// 第一遍 + 第二遍解析。`reader` 位于常量池计数之后。
    ///
    /// 槽位共 `count` 个：槽 0 不可用（占位），有效索引 1..count-1；
    /// long/double 占两槽（第二槽为幻影，必须落在 count 内）。
    pub fn parse(reader: &mut &[u8], count: u16) -> CpResult<Self> {
        let count = usize::from(count);
        let mut raw: Vec<RawEntry> = Vec::with_capacity(count + 1);
        raw.push(RawEntry {
            tag: 0,
            payload: Payload::Skipped,
        });
        // ---- 第一遍：按 tag 切分原始载荷（幻影槽同样占位，保持 raw 与池索引一致） ----
        let mut index = 1usize;
        while index < count {
            let tag = *reader
                .first()
                .ok_or_else(|| CpError("truncated: missing tag".into()))?;
            *reader = &reader[1..];
            let payload = match tag {
                tag::CLASS | tag::STRING => {
                    let a = read_u16(reader)?;
                    Payload::Index(a)
                }
                tag::FIELDREF | tag::METHODREF | tag::INTERFACE_METHODREF
                | tag::NAME_AND_TYPE => {
                    let a = read_u16(reader)?;
                    let b = read_u16(reader)?;
                    Payload::Pair(a, b)
                }
                tag::INTEGER | tag::FLOAT => Payload::Bytes4(read_bytes4(reader)?),
                tag::LONG | tag::DOUBLE => Payload::Bytes8(read_bytes8(reader)?),
                tag::UTF8 => {
                    let len = usize::from(read_u16(reader)?);
                    let bytes = reader
                        .get(..len)
                        .ok_or_else(|| CpError("truncated utf8 payload".into()))?;
                    let text = decode_modified_utf8(bytes)?;
                    *reader = &reader[len..];
                    Payload::Text(text)
                }
                tag::METHOD_HANDLE => {
                    // 1 字节 kind + 2 字节引用。
                    if reader.len() < 3 {
                        return Err(CpError("truncated method handle".into()));
                    }
                    *reader = &reader[3..];
                    Payload::Skipped
                }
                tag::METHOD_TYPE | tag::MODULE | tag::PACKAGE => {
                    read_u16(reader)?;
                    Payload::Skipped
                }
                tag::DYNAMIC | tag::INVOKE_DYNAMIC => {
                    read_u16(reader)?;
                    read_u16(reader)?;
                    Payload::Skipped
                }
                other => return Err(CpError(format!("unknown constant tag {other}"))),
            };
            raw.push(RawEntry { tag, payload });
            // long/double 双槽：幻影槽在 raw 中同样占位，保证 raw 按池索引可查。
            index += if tag == tag::LONG || tag == tag::DOUBLE {
                raw.push(RawEntry {
                    tag: 0,
                    payload: Payload::Skipped,
                });
                2
            } else {
                1
            };
            if index > count {
                // long/double 落在最后一个槽时会越界一格（规范禁止）。
                return Err(CpError("long/double occupies the final slot".into()));
            }
        }
        // ---- 第二遍：解出结构化条目并校验索引 ----
        let mut entries: Vec<CpEntry> = Vec::with_capacity(count);
        entries.push(CpEntry::Phantom);
        let mut i = 1usize;
        while i < raw.len() {
            let entry = &raw[i];
            let resolved = match (entry.tag, &entry.payload) {
                (tag::CLASS, Payload::Index(a)) => {
                    check_utf8(&raw, *a)?;
                    CpEntry::Class(*a)
                }
                (tag::STRING, Payload::Index(a)) => {
                    check_utf8(&raw, *a)?;
                    CpEntry::String(*a)
                }
                (tag::FIELDREF, Payload::Pair(a, b)) => {
                    check_class(&raw, *a)?;
                    check_nat(&raw, *b)?;
                    CpEntry::Fieldref {
                        class: *a,
                        nat: *b,
                    }
                }
                (tag::METHODREF, Payload::Pair(a, b)) => {
                    check_class(&raw, *a)?;
                    check_nat(&raw, *b)?;
                    CpEntry::Methodref {
                        class: *a,
                        nat: *b,
                    }
                }
                (tag::INTERFACE_METHODREF, Payload::Pair(a, b)) => {
                    check_class(&raw, *a)?;
                    check_nat(&raw, *b)?;
                    CpEntry::InterfaceMethodref {
                        class: *a,
                        nat: *b,
                    }
                }
                (tag::NAME_AND_TYPE, Payload::Pair(a, b)) => {
                    check_utf8(&raw, *a)?;
                    check_utf8(&raw, *b)?;
                    CpEntry::NameAndType {
                        name: *a,
                        desc: *b,
                    }
                }
                (tag::INTEGER, Payload::Bytes4(b)) => {
                    CpEntry::Integer(i32::from_be_bytes(*b))
                }
                (tag::FLOAT, Payload::Bytes4(b)) => {
                    CpEntry::Float(f32::from_be_bytes(*b))
                }
                (tag::LONG, Payload::Bytes8(b)) => {
                    entries.push(CpEntry::Long(i64::from_be_bytes(*b)));
                    entries.push(CpEntry::Phantom);
                    i += 2;
                    continue;
                }
                (tag::DOUBLE, Payload::Bytes8(b)) => {
                    entries.push(CpEntry::Double(f64::from_be_bytes(*b)));
                    entries.push(CpEntry::Phantom);
                    i += 2;
                    continue;
                }
                (tag::UTF8, Payload::Text(t)) => CpEntry::Utf8(t.clone()),
                // 已跳过的现代条目：保留为幻影，访问时报错。
                _ => CpEntry::Phantom,
            };
            entries.push(resolved);
            i += 1;
        }
        Ok(Self { entries })
    }

    fn entry(&self, index: u16) -> CpResult<&CpEntry> {
        self.entries
            .get(usize::from(index))
            .filter(|entry| !matches!(entry, CpEntry::Phantom))
            .ok_or_else(|| CpError(format!("invalid constant index {index}")))
    }

    pub fn utf8(&self, index: u16) -> CpResult<&str> {
        match self.entry(index)? {
            CpEntry::Utf8(text) => Ok(text),
            other => Err(CpError(format!(
                "constant {index} is {other:?}, expected utf8"
            ))),
        }
    }

    /// 类条目的名字（内部名，如 `java/lang/String`）。
    pub fn class_name(&self, index: u16) -> CpResult<&str> {
        match self.entry(index)? {
            CpEntry::Class(name_index) => self.utf8(*name_index),
            other => Err(CpError(format!(
                "constant {index} is {other:?}, expected class"
            ))),
        }
    }

    pub fn name_and_type(&self, index: u16) -> CpResult<(&str, &str)> {
        match self.entry(index)? {
            CpEntry::NameAndType { name, desc } => Ok((
                self.utf8(*name)?,
                self.utf8(*desc)?,
            )),
            other => Err(CpError(format!(
                "constant {index} is {other:?}, expected name-and-type"
            ))),
        }
    }

    /// 字段引用三元组 `(类名, 字段名, 描述符)`。
    pub fn fieldref(&self, index: u16) -> CpResult<(&str, &str, &str)> {
        match self.entry(index)? {
            CpEntry::Fieldref { class, nat } => Ok((
                self.class_name(*class)?,
                self.utf8(self.entry(*nat)?.name_index_of()?)?,
                self.utf8(self.entry(*nat)?.desc_index_of()?)?,
            )),
            other => Err(CpError(format!(
                "constant {index} is {other:?}, expected fieldref"
            ))),
        }
    }

    /// 方法引用三元组（含接口方法引用）。
    pub fn methodref(&self, index: u16) -> CpResult<(&str, &str, &str)> {
        match self.entry(index)? {
            CpEntry::Methodref { class, nat }
            | CpEntry::InterfaceMethodref { class, nat } => {
                let (name, desc) = self.name_and_type(*nat)?;
                Ok((self.class_name(*class)?, name, desc))
            }
            other => Err(CpError(format!(
                "constant {index} is {other:?}, expected methodref"
            ))),
        }
    }

    /// 迭代常量池中全部类条目的内部名（含 this/super/接口/成员引用/ldc 类常量）。
    ///
    /// 供 jar 依赖面分析等工具使用；解释路径不依赖它。
    pub fn class_names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().filter_map(|entry| match entry {
            CpEntry::Class(name_index) => self.utf8(*name_index).ok(),
            _ => None,
        })
    }

    /// 池槽位总数（含槽 0 占位与 long/double 幻影槽）。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 池是否为空（仅槽 0 占位）。
    pub fn is_empty(&self) -> bool {
        self.entries.len() <= 1
    }

    /// ldc 可加载的常量。
    pub fn loadable(&self, index: u16) -> CpResult<Loadable> {
        Ok(match self.entry(index)? {
            CpEntry::Integer(v) => Loadable::Int(*v),
            CpEntry::Float(v) => Loadable::Float(*v),
            CpEntry::Long(v) => Loadable::Long(*v),
            CpEntry::Double(v) => Loadable::Double(*v),
            CpEntry::String(a) => Loadable::Str(self.utf8(*a)?.to_string()),
            CpEntry::Class(a) => Loadable::Class(self.utf8(*a)?.to_string()),
            other => {
                return Err(CpError(format!(
                    "constant {index} is {other:?}, not loadable"
                )))
            }
        })
    }
}

impl CpEntry {
    fn name_index_of(&self) -> CpResult<u16> {
        match self {
            CpEntry::NameAndType { name, .. } => Ok(*name),
            other => Err(CpError(format!("{other:?} is not name-and-type"))),
        }
    }

    fn desc_index_of(&self) -> CpResult<u16> {
        match self {
            CpEntry::NameAndType { desc, .. } => Ok(*desc),
            other => Err(CpError(format!("{other:?} is not name-and-type"))),
        }
    }
}

/// ldc / ldc_w / ldc2_w 可加载的常量（字符串与类在链接期再解析为对象）。
#[derive(Clone, Debug)]
pub enum Loadable {
    Int(i32),
    Float(f32),
    Long(i64),
    Double(f64),
    Str(String),
    Class(String),
}

fn read_u16(reader: &mut &[u8]) -> CpResult<u16> {
    let bytes = reader
        .get(..2)
        .ok_or_else(|| CpError("truncated u16".into()))?;
    *reader = &reader[2..];
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn read_bytes4(reader: &mut &[u8]) -> CpResult<[u8; 4]> {
    let bytes = reader
        .get(..4)
        .ok_or_else(|| CpError("truncated 4 bytes".into()))?;
    *reader = &reader[4..];
    Ok([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn read_bytes8(reader: &mut &[u8]) -> CpResult<[u8; 8]> {
    let bytes: [u8; 8] = reader
        .get(..8)
        .ok_or_else(|| CpError("truncated 8 bytes".into()))?
        .try_into()
        .expect("eight bytes");
    *reader = &reader[8..];
    Ok(bytes)
}

fn check_utf8(raw: &[RawEntry], index: u16) -> CpResult<()> {
    matches!(
        raw.get(usize::from(index)).map(|entry| &entry.payload),
        Some(Payload::Text(_))
    )
    .then_some(())
    .ok_or_else(|| CpError(format!("constant {index} is not utf8")))
}

fn check_class(raw: &[RawEntry], index: u16) -> CpResult<()> {
    matches!(
        raw.get(usize::from(index)).map(|entry| entry.tag),
        Some(tag::CLASS)
    )
    .then_some(())
    .ok_or_else(|| CpError(format!("constant {index} is not a class")))
}

fn check_nat(raw: &[RawEntry], index: u16) -> CpResult<()> {
    matches!(
        raw.get(usize::from(index)).map(|entry| entry.tag),
        Some(tag::NAME_AND_TYPE)
    )
    .then_some(())
    .ok_or_else(|| CpError(format!("constant {index} is not name-and-type")))
}

/// 解码 JVM Modified-UTF8（JVM 规范 §4.4.7）：与标准 UTF-8 的差异是
/// U+0000 编码为 0xC0 0x80、补充字符用代理对形式编码。
fn decode_modified_utf8(bytes: &[u8]) -> CpResult<Rc<str>> {
    let mut output = String::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        let a = bytes[index];
        if a < 0x80 {
            // ASCII 直通（0x00 不会出现在单字节形式）。
            output.push(a as char);
            index += 1;
        } else if a & 0xE0 == 0xC0 {
            let b = *bytes
                .get(index + 1)
                .filter(|byte| *byte & 0xC0 == 0x80)
                .ok_or_else(|| CpError("bad modified-utf8 2-byte sequence".into()))?;
            let code = ((u16::from(a) & 0x1F) << 6) | (u16::from(b) & 0x3F);
            output.push(
                char::from_u32(u32::from(code))
                    .ok_or_else(|| CpError("bad modified-utf8 code unit".into()))?,
            );
            index += 2;
        } else if a & 0xF0 == 0xE0 {
            let (b, c) = (
                *bytes
                    .get(index + 1)
                    .filter(|byte| *byte & 0xC0 == 0x80)
                    .ok_or_else(|| CpError("bad modified-utf8 3-byte sequence".into()))?,
                *bytes
                    .get(index + 2)
                    .filter(|byte| *byte & 0xC0 == 0x80)
                    .ok_or_else(|| CpError("bad modified-utf8 3-byte sequence".into()))?,
            );
            let code =
                ((u32::from(a) & 0x0F) << 12) | ((u32::from(b) & 0x3F) << 6) | (u32::from(c) & 0x3F);
            index += 3;
            if (0xD800..=0xDFFF).contains(&code) {
                // CESU-8：补充字符编码为代理对的两个 3 字节序列。
                if !(0xD800..=0xDBFF).contains(&code) {
                    return Err(CpError("unpaired low surrogate in modified-utf8".into()));
                }
                let (a2, b2, c2) = (
                    *bytes
                        .get(index)
                        .filter(|byte| *byte & 0xF0 == 0xE0)
                        .ok_or_else(|| CpError("truncated surrogate pair".into()))?,
                    *bytes
                        .get(index + 1)
                        .filter(|byte| *byte & 0xC0 == 0x80)
                        .ok_or_else(|| CpError("bad modified-utf8 3-byte sequence".into()))?,
                    *bytes
                        .get(index + 2)
                        .filter(|byte| *byte & 0xC0 == 0x80)
                        .ok_or_else(|| CpError("bad modified-utf8 3-byte sequence".into()))?,
                );
                let low = ((u32::from(a2) & 0x0F) << 12)
                    | ((u32::from(b2) & 0x3F) << 6)
                    | (u32::from(c2) & 0x3F);
                if !(0xDC00..=0xDFFF).contains(&low) {
                    return Err(CpError("unpaired high surrogate in modified-utf8".into()));
                }
                index += 3;
                let combined =
                    0x1_0000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                output.push(
                    char::from_u32(combined)
                        .ok_or_else(|| CpError("bad modified-utf8 code point".into()))?,
                );
            } else {
                output.push(
                    char::from_u32(code)
                        .ok_or_else(|| CpError("bad modified-utf8 code point".into()))?,
                );
            }
        } else {
            return Err(CpError("bad modified-utf8 lead byte".into()));
        }
    }
    Ok(output.into())
}
