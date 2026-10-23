//! class 文件解析入口：magic/版本/常量池/字段/方法/类属性校验。
//!
//! 目标版本：45.0–48.0（CLDC 子集，FR-006）；超限抛 `UnsupportedVersion`。

pub mod code;
pub mod cp;
pub mod member;

use std::rc::Rc;

use crate::KvmError;
use cp::{ConstPool, CpError};
use member::{MemberInfo, parse_member};

const MAGIC: u32 = 0xCAFEBABE;
pub const MIN_MAJOR: u16 = 45;
pub const MAX_MAJOR: u16 = 48;

/// 解析完成的 class 文件。
#[derive(Debug)]
pub struct ClassFile {
    pub minor_version: u16,
    pub major_version: u16,
    pub constant_pool: Rc<ConstPool>,
    pub access_flags: u16,
    pub this_class: String,
    pub super_class: Option<String>,
    pub interfaces: Vec<String>,
    pub fields: Vec<MemberInfo>,
    pub methods: Vec<MemberInfo>,
}

impl ClassFile {
    /// 解析整份 class 字节（两遍常量池 + 结构校验，D9）。
    pub fn parse(bytes: &[u8]) -> Result<Self, KvmError> {
        let mut reader = bytes;
        let magic = member::parse_u32(&mut reader).map_err(caf)?;
        if magic != MAGIC {
            return Err(KvmError::ClassFormat(format!(
                "bad magic {magic:#010x} (expected CAFEBABE)"
            )));
        }
        let minor_version = member::parse_u16(&mut reader).map_err(caf)?;
        let major_version = member::parse_u16(&mut reader).map_err(caf)?;
        if !(MIN_MAJOR..=MAX_MAJOR).contains(&major_version) {
            return Err(KvmError::UnsupportedVersion(format!(
                "{major_version}.{minor_version} (supported {MIN_MAJOR}.0-{MAX_MAJOR}.0)"
            )));
        }
        let constant_count = member::parse_u16(&mut reader).map_err(caf)?;
        let constant_pool = Rc::new(ConstPool::parse(&mut reader, constant_count).map_err(caf)?);
        let access_flags = member::parse_u16(&mut reader).map_err(caf)?;
        let this_index = member::parse_u16(&mut reader).map_err(caf)?;
        let super_index = member::parse_u16(&mut reader).map_err(caf)?;
        let this_class = constant_pool
            .class_name(this_index)
            .map_err(caf)?
            .to_string();
        if this_class.is_empty() {
            return Err(KvmError::ClassFormat("empty this_class name".into()));
        }
        let super_class = if super_index == 0 {
            // 只有 java/lang/Object 允许无父类。
            if this_class != "java/lang/Object" {
                return Err(KvmError::ClassFormat(format!(
                    "{this_class} has no super class"
                )));
            }
            None
        } else {
            Some(
                constant_pool
                    .class_name(super_index)
                    .map_err(caf)?
                    .to_string(),
            )
        };
        let interface_count = member::parse_u16(&mut reader).map_err(caf)?;
        let mut interfaces = Vec::with_capacity(usize::from(interface_count));
        for _ in 0..interface_count {
            let index = member::parse_u16(&mut reader).map_err(caf)?;
            interfaces.push(
                constant_pool
                    .class_name(index)
                    .map_err(caf)?
                    .to_string(),
            );
        }
        let field_count = member::parse_u16(&mut reader).map_err(caf)?;
        let mut fields = Vec::with_capacity(usize::from(field_count));
        for _ in 0..field_count {
            fields.push(parse_member(&mut reader, &constant_pool, false).map_err(caf)?);
        }
        let method_count = member::parse_u16(&mut reader).map_err(caf)?;
        let mut methods = Vec::with_capacity(usize::from(method_count));
        for _ in 0..method_count {
            methods.push(parse_member(&mut reader, &constant_pool, true).map_err(caf)?);
        }
        let attribute_count = member::parse_u16(&mut reader).map_err(caf)?;
        for _ in 0..attribute_count {
            member::parse_attribute(&mut reader, &constant_pool).map_err(caf)?;
        }
        if !reader.is_empty() {
            return Err(KvmError::ClassFormat(format!(
                "{} trailing bytes after class structure",
                reader.len()
            )));
        }
        Ok(Self {
            minor_version,
            major_version,
            constant_pool,
            access_flags,
            this_class,
            super_class,
            interfaces,
            fields,
            methods,
        })
    }
}

fn caf(error: CpError) -> KvmError {
    KvmError::ClassFormat(error.to_string())
}
