//! 字段/方法成员与属性解析（Code、ConstantValue、Exceptions、SourceFile）。

use std::rc::Rc;

use super::cp::{ConstPool, CpError, CpResult};

/// JVM 访问标志位（规范 §4.1/4.5/4.6）。
pub mod acc {
    pub const PUBLIC: u16 = 0x0001;
    pub const PRIVATE: u16 = 0x0002;
    pub const PROTECTED: u16 = 0x0004;
    pub const STATIC: u16 = 0x0008;
    pub const FINAL: u16 = 0x0010;
    pub const SUPER: u16 = 0x0020;
    pub const SYNCHRONIZED: u16 = 0x0020;
    pub const VOLATILE: u16 = 0x0040;
    pub const BRIDGE: u16 = 0x0040;
    pub const TRANSIENT: u16 = 0x0080;
    pub const VARARGS: u16 = 0x0080;
    pub const NATIVE: u16 = 0x0100;
    pub const INTERFACE: u16 = 0x0200;
    pub const ABSTRACT: u16 = 0x0400;
    pub const STRICT: u16 = 0x0800;
    pub const SYNTHETIC: u16 = 0x1000;
    pub const ANNOTATION: u16 = 0x2000;
    pub const ENUM: u16 = 0x4000;
}

/// 字段/方法的常量初值（ConstantValue 属性）。
#[derive(Clone, Copy, Debug)]
pub enum ConstValue {
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    /// 字符串初值由宿主在类准备期驻留。
    Str(u16),
}

/// 一个 catch 处理器（pc 仍为字节码偏移，解码期转成指令索引）。
#[derive(Clone, Debug)]
pub struct CatchHandler {
    pub start_pc: u16,
    pub end_pc: u16,
    pub handler_pc: u16,
    /// None = catch-all（finally）。
    pub catch_class: Option<String>,
}

/// Code 属性的原始形态：字节码 + 异常表（尚未解码为指令）。
#[derive(Clone, Debug)]
pub struct CodeInfo {
    pub max_stack: u16,
    pub max_locals: u16,
    pub code: Rc<Vec<u8>>,
    pub handlers: Vec<CatchHandler>,
}

/// 字段或方法成员。
#[derive(Clone, Debug)]
pub struct MemberInfo {
    pub access_flags: u16,
    pub name: String,
    pub descriptor: String,
    pub code: Option<CodeInfo>,
    pub constant_value: Option<ConstValue>,
    /// throws 子句里的异常类名（仅记录，不做语义）。
    pub exceptions: Vec<String>,
}

impl MemberInfo {
    pub fn is_static(&self) -> bool {
        self.access_flags & acc::STATIC != 0
    }

    pub fn is_native(&self) -> bool {
        self.access_flags & acc::NATIVE != 0
    }

    pub fn is_abstract(&self) -> bool {
        self.access_flags & acc::ABSTRACT != 0
    }
}

/// 解析一个成员（字段或方法）。
pub fn parse_member(reader: &mut &[u8], cp: &ConstPool, is_method: bool) -> CpResult<MemberInfo> {
    let access_flags = parse_u16(reader)?;
    let name_index = parse_u16(reader)?;
    let descriptor_index = parse_u16(reader)?;
    let name = cp.utf8(name_index)?.to_string();
    let descriptor = cp.utf8(descriptor_index)?.to_string();
    let attribute_count = parse_u16(reader)?;
    let mut code = None;
    let mut constant_value = None;
    let mut exceptions = Vec::new();
    for _ in 0..attribute_count {
        let (attribute_name, mut content) = parse_attribute(reader, cp)?;
        match attribute_name {
            "Code" if is_method => {
                code = Some(parse_code_attribute(&mut content, cp)?);
            }
            "ConstantValue" if !is_method => {
                let index = parse_u16(&mut content)?;
                constant_value = Some(match cp.loadable(index)? {
                    super::cp::Loadable::Int(v) => ConstValue::Int(v),
                    super::cp::Loadable::Long(v) => ConstValue::Long(v),
                    super::cp::Loadable::Float(v) => ConstValue::Float(v),
                    super::cp::Loadable::Double(v) => ConstValue::Double(v),
                    super::cp::Loadable::Str(_) => ConstValue::Str(index),
                    super::cp::Loadable::Class(_) => {
                        return Err(CpError("ConstantValue references a class".into()))
                    }
                });
            }
            "Exceptions" => {
                let count = parse_u16(&mut content)?;
                for _ in 0..count {
                    let class_index = parse_u16(&mut content)?;
                    exceptions.push(cp.class_name(class_index)?.to_string());
                }
            }
            _ => {
                // 其他属性（LineNumberTable、Deprecated 等）按长度跳过。
            }
        }
    }
    Ok(MemberInfo {
        access_flags,
        name,
        descriptor,
        code,
        constant_value,
        exceptions,
    })
}

fn parse_code_attribute(reader: &mut &[u8], cp: &ConstPool) -> CpResult<CodeInfo> {
    let max_stack = parse_u16(reader)?;
    let max_locals = parse_u16(reader)?;
    // max_locals == 0 合法（无参静态空方法；jc.jar 语料中真实出现），
    // 解释器按 max(max_locals, 实参槽数) 分配。
    let code_len = parse_u32(reader)? as usize;
    // CLDC 代码上限（防御异常长度字段）。
    if code_len == 0 || code_len > 1 << 20 {
        return Err(CpError(format!("code length {code_len} out of range")));
    }
    let code_bytes = reader
        .get(..code_len)
        .ok_or_else(|| CpError("truncated bytecode".into()))?;
    let code = Rc::new(code_bytes.to_vec());
    *reader = &reader[code_len..];
    let handler_count = parse_u16(reader)?;
    let mut handlers = Vec::with_capacity(usize::from(handler_count));
    for _ in 0..handler_count {
        let start_pc = parse_u16(reader)?;
        let end_pc = parse_u16(reader)?;
        let handler_pc = parse_u16(reader)?;
        let catch_index = parse_u16(reader)?;
        let catch_class = if catch_index == 0 {
            None
        } else {
            Some(cp.class_name(catch_index)?.to_string())
        };
        // 规范：start_pc < end_pc，范围落在字节码内（end_pc 可等于长度）。
        if start_pc >= end_pc || end_pc as usize > code_len || handler_pc as usize >= code_len {
            return Err(CpError("exception table range out of bounds".into()));
        }
        handlers.push(CatchHandler {
            start_pc,
            end_pc,
            handler_pc,
            catch_class,
        });
    }
    // Code 自身的属性（LineNumberTable/StackMapTable）跳过。
    let nested = parse_u16(reader)?;
    for _ in 0..nested {
        parse_attribute(reader, cp)?;
    }
    Ok(CodeInfo {
        max_stack,
        max_locals,
        code,
        handlers,
    })
}

/// 解析一个属性，返回 `(名字, 内容切片)`；内容长度被消费。
///
/// 名字只保留白名单内的已知属性（'static 字面量）；未知名字返回空串，
/// 跳过语义由调用方决定。
pub fn parse_attribute<'a>(reader: &mut &'a [u8], cp: &ConstPool) -> CpResult<(&'static str, &'a [u8])> {
    const KNOWN: &[&str] = &[
        "Code",
        "ConstantValue",
        "Exceptions",
        "SourceFile",
        "InnerClasses",
        "EnclosingMethod",
        "Synthetic",
        "Deprecated",
        "LineNumberTable",
        "LocalVariableTable",
        "LocalVariableTypeTable",
        "StackMapTable",
        "Signature",
    ];
    let name_index = parse_u16(reader)?;
    let name = cp.utf8(name_index)?;
    let len = parse_u32(reader)? as usize;
    let content = reader
        .get(..len)
        .ok_or_else(|| CpError("truncated attribute content".into()))?;
    *reader = &reader[len..];
    let known = KNOWN
        .iter()
        .find(|candidate| **candidate == name)
        .copied()
        .unwrap_or("");
    Ok((known, content))
}

pub fn parse_u16(reader: &mut &[u8]) -> CpResult<u16> {
    let bytes = reader
        .get(..2)
        .ok_or_else(|| CpError("truncated u16".into()))?;
    *reader = &reader[2..];
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

pub fn parse_u32(reader: &mut &[u8]) -> CpResult<u32> {
    let bytes = reader
        .get(..4)
        .ok_or_else(|| CpError("truncated u32".into()))?;
    *reader = &reader[4..];
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}
