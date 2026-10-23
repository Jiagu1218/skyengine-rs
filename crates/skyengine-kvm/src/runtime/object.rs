//! 堆对象载荷：实例（字段槽）、数组（8 种元素类型 + 引用）、字符串（UTF-16）。

use crate::runtime::{ClassId, ObjRef, Slot};

/// 数组元素类型。
///
/// JVM 的 8 种原始元素类型 + 引用（对象/数组）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElemType {
    Boolean,
    Byte,
    Char,
    Short,
    Int,
    Long,
    Float,
    Double,
    Ref,
}

impl ElemType {
    /// 按描述符字符解析（数组描述符去掉 `[` 后的首字符）。
    pub fn from_desc(desc_char: char) -> Option<Self> {
        Some(match desc_char {
            'Z' => Self::Boolean,
            'B' => Self::Byte,
            'C' => Self::Char,
            'S' => Self::Short,
            'I' => Self::Int,
            'J' => Self::Long,
            'F' => Self::Float,
            'D' => Self::Double,
            _ => return None,
        })
    }

    /// newarray 的 atype 操作数（4..=11）。
    pub fn from_atype(atype: u8) -> Option<Self> {
        Some(match atype {
            4 => Self::Boolean,
            5 => Self::Char,
            6 => Self::Float,
            7 => Self::Double,
            8 => Self::Byte,
            9 => Self::Short,
            10 => Self::Int,
            11 => Self::Long,
            _ => return None,
        })
    }
}

/// 具体类型化的数组存储（避免逐元素 enum 的开销）。
#[derive(Debug)]
pub enum ArrayData {
    Bool(Vec<bool>),
    Byte(Vec<i8>),
    Char(Vec<u16>),
    Short(Vec<i16>),
    Int(Vec<i32>),
    Long(Vec<i64>),
    Float(Vec<f32>),
    Double(Vec<f64>),
    Refs(Vec<ObjRef>),
}

impl ArrayData {
    /// 按元素类型构造零值数组。
    pub fn zeroed(elem: ElemType, len: usize) -> Self {
        match elem {
            ElemType::Boolean => Self::Bool(vec![false; len]),
            ElemType::Byte => Self::Byte(vec![0; len]),
            ElemType::Char => Self::Char(vec![0; len]),
            ElemType::Short => Self::Short(vec![0; len]),
            ElemType::Int => Self::Int(vec![0; len]),
            ElemType::Long => Self::Long(vec![0; len]),
            ElemType::Float => Self::Float(vec![0.0; len]),
            ElemType::Double => Self::Double(vec![0.0; len]),
            ElemType::Ref => Self::Refs(vec![ObjRef::NULL; len]),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Bool(v) => v.len(),
            Self::Byte(v) => v.len(),
            Self::Char(v) => v.len(),
            Self::Short(v) => v.len(),
            Self::Int(v) => v.len(),
            Self::Long(v) => v.len(),
            Self::Float(v) => v.len(),
            Self::Double(v) => v.len(),
            Self::Refs(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 读一个元素为槽位值（调用方已完成边界检查）。
    ///
    /// boolean/byte/char/short 一律符号/零扩展成 `Int`（JVM aload 语义）。
    pub fn get(&self, index: usize) -> Slot {
        match self {
            Self::Bool(v) => Slot::Int(i32::from(v[index])),
            Self::Byte(v) => Slot::Int(i32::from(v[index])),
            Self::Char(v) => Slot::Int(i32::from(v[index])),
            Self::Short(v) => Slot::Int(i32::from(v[index])),
            Self::Int(v) => Slot::Int(v[index]),
            Self::Long(v) => Slot::Long(v[index]),
            Self::Float(v) => Slot::Float(v[index]),
            Self::Double(v) => Slot::Double(v[index]),
            Self::Refs(v) => Slot::Ref(v[index]),
        }
    }

    /// 写一个元素；类型不匹配视为解释器内部错误。
    ///
    /// `Int` 写入 boolean/byte/char/short 数组按 JVM astore 语义截断。
    pub fn set(&mut self, index: usize, value: Slot) -> Result<(), crate::KvmError> {
        match (self, value) {
            (Self::Bool(v), Slot::Int(x)) => v[index] = x != 0,
            (Self::Byte(v), Slot::Int(x)) => v[index] = x as i8,
            (Self::Char(v), Slot::Int(x)) => v[index] = x as u16,
            (Self::Short(v), Slot::Int(x)) => v[index] = x as i16,
            (Self::Int(v), Slot::Int(x)) => v[index] = x,
            (Self::Long(v), Slot::Long(x)) => v[index] = x,
            (Self::Float(v), Slot::Float(x)) => v[index] = x,
            (Self::Double(v), Slot::Double(x)) => v[index] = x,
            (Self::Refs(v), Slot::Ref(x)) => v[index] = x,
            (data, slot) => {
                return Err(crate::KvmError::Internal(format!(
                    "array store type mismatch: {data:?} <- {slot:?}"
                )));
            }
        }
        Ok(())
    }
}

/// 数组载荷。`class` 为运行时数组类（如 `[I`、`[Ljava/lang/String;`），
/// checkcast/instanceof/athrow 依赖它做类型判定。
#[derive(Debug)]
pub struct ArrayObj {
    pub class: ClassId,
    pub elem: ElemType,
    pub data: ArrayData,
}

/// java/lang/String 载荷（D3）：UTF-16 码元数组，charAt/length 按码元。
///
/// `hash` 惰性缓存；0 同时表示"未计算"，空串哈希重复计算与 Java 语义一致且无害。
#[derive(Debug)]
pub struct StrObj {
    pub chars: Vec<u16>,
    pub hash: i32,
}

/// 实例载荷：`fields` 按继承链绝对布局排列（父类字段为前缀）。
#[derive(Debug)]
pub struct InstanceObj {
    pub class: ClassId,
    pub fields: Vec<Slot>,
}

/// 堆对象载荷。
#[derive(Debug)]
pub enum Payload {
    Instance(InstanceObj),
    Array(ArrayObj),
    Str(StrObj),
}
