//! 堆：非移动对象表 + 字符串驻留表。
//!
//! 对象按索引分配（`ObjRef`），槽 0 保留为 null。分配达到软上限返回
//! `OutOfMemory`；`gc` 模块（T007）在分配失败时标记-清除并重试。
//! 清除产生的空洞记入自由表，后续分配优先复用（对象不移动，
//! `ObjRef` 在回收后保持稳定）。

use std::collections::{HashMap, HashSet};

use crate::runtime::object::{ArrayData, ArrayObj, ElemType, InstanceObj, Payload, StrObj};
use crate::runtime::{ClassId, ObjRef, Slot};
use crate::KvmError;

/// 默认对象数软上限（约 100 万对象；足够 CLDC 游戏堆规模）。
pub const DEFAULT_OBJECT_CAP: usize = 1 << 20;

/// 堆对象条目。
#[derive(Debug)]
pub struct HeapObj {
    pub payload: Payload,
}

#[derive(Debug)]
pub struct Heap {
    /// 槽 0 保留为 null（恒为 `None`）。
    objects: Vec<Option<HeapObj>>,
    /// 字符串驻留表：UTF-16 内容 → String 对象。
    interned: HashMap<Vec<u16>, ObjRef>,
    /// 回收槽复用表（sweep 填充；alloc 优先弹出）。
    free: Vec<ObjRef>,
    /// 对象数软上限。
    cap: usize,
}

impl Default for Heap {
    fn default() -> Self {
        Self::new()
    }
}

impl Heap {
    pub fn new() -> Self {
        Self::with_cap(DEFAULT_OBJECT_CAP)
    }

    pub fn with_cap(cap: usize) -> Self {
        Self {
            objects: vec![None],
            interned: HashMap::new(),
            free: Vec::new(),
            cap,
        }
    }

    /// 当前存活对象数（不含 null 槽与回收空洞）。
    pub fn object_count(&self) -> usize {
        self.objects.iter().skip(1).filter(|slot| slot.is_some()).count()
    }

    fn alloc(&mut self, payload: Payload) -> Result<ObjRef, KvmError> {
        // 复用回收槽（不增长对象表，不受软上限约束）。
        if let Some(reference) = self.free.pop() {
            let index = reference.0 as usize;
            debug_assert!(self.objects[index].is_none(), "free list corruption");
            self.objects[index] = Some(HeapObj { payload });
            return Ok(reference);
        }
        if self.objects.len() > self.cap {
            return Err(KvmError::OutOfMemory);
        }
        self.objects.push(Some(HeapObj { payload }));
        Ok(ObjRef((self.objects.len() - 1) as u32))
    }

    /// 标记-清除的清除阶段（`gc::collect` 调用）：
    /// 释放未标记对象，清理驻留表中的死串，空洞记入自由表。
    pub(crate) fn sweep(&mut self, marked: &HashSet<ObjRef>) -> usize {
        let mut freed = 0;
        for (index, slot) in self.objects.iter_mut().enumerate().skip(1) {
            let reference = ObjRef(index as u32);
            if slot.is_some() && !marked.contains(&reference) {
                *slot = None;
                self.free.push(reference);
                freed += 1;
            }
        }
        // 驻留表只保留存活串，防止悬挂引用复活死对象。
        self.interned.retain(|_, reference| marked.contains(reference));
        freed
    }

    /// 分配实例：`fields` 为布局零值模板（类空间在布局期预生成）。
    pub fn alloc_instance(
        &mut self,
        class: ClassId,
        fields: Vec<Slot>,
    ) -> Result<ObjRef, KvmError> {
        self.alloc(Payload::Instance(InstanceObj { class, fields }))
    }

    /// 分配数组（零值填充）。`class` 为运行时数组类（checkcast/instanceof/
    /// athrow 的类型判定依据）。负长度由解释器拦截；此处再限制绝对上限防御溢出。
    pub fn alloc_array(
        &mut self,
        class: ClassId,
        elem: ElemType,
        len: usize,
    ) -> Result<ObjRef, KvmError> {
        if len > (1 << 28) {
            return Err(KvmError::OutOfMemory);
        }
        self.alloc(Payload::Array(ArrayObj {
            class,
            elem,
            data: ArrayData::zeroed(elem, len),
        }))
    }

    /// 分配/驻留字符串：同内容幂等返回同一对象。
    pub fn alloc_string(&mut self, chars: Vec<u16>) -> Result<ObjRef, KvmError> {
        if let Some(existing) = self.interned.get(&chars) {
            return Ok(*existing);
        }
        let reference = self.alloc(Payload::Str(StrObj {
            chars: chars.clone(),
            hash: 0,
        }))?;
        self.interned.insert(chars, reference);
        Ok(reference)
    }

    /// 分配**不驻留**的字符串（T014）：substring/concat/toString 等计算结果
    /// 按 JVM 语义是新对象（`new String("x") != "x"`），不能进驻留表。
    /// 仅 `String.intern` 与 ldc 字面量走 `alloc_string` 驻留路径。
    pub fn alloc_string_uninterned(&mut self, chars: Vec<u16>) -> Result<ObjRef, KvmError> {
        self.alloc(Payload::Str(StrObj { chars, hash: 0 }))
    }

    /// 查询已驻留的字符串。
    pub fn interned_string(&self, chars: &[u16]) -> Option<ObjRef> {
        self.interned.get(chars).copied()
    }

    pub fn obj(&self, reference: ObjRef) -> Option<&HeapObj> {
        self.objects.get(reference.0 as usize).and_then(|o| o.as_ref())
    }

    pub fn obj_mut(&mut self, reference: ObjRef) -> Option<&mut HeapObj> {
        self.objects
            .get_mut(reference.0 as usize)
            .and_then(|o| o.as_mut())
    }

    pub fn payload(&self, reference: ObjRef) -> Result<&Payload, KvmError> {
        self.obj(reference)
            .map(|obj| &obj.payload)
            .ok_or_else(|| null_deref(reference))
    }

    pub fn payload_mut(&mut self, reference: ObjRef) -> Result<&mut Payload, KvmError> {
        self.obj_mut(reference)
            .map(|obj| &mut obj.payload)
            .ok_or_else(|| null_deref(reference))
    }

    pub fn instance(&self, reference: ObjRef) -> Result<&InstanceObj, KvmError> {
        match self.payload(reference)? {
            Payload::Instance(instance) => Ok(instance),
            other => Err(wrong_payload("instance", other)),
        }
    }

    pub fn instance_mut(&mut self, reference: ObjRef) -> Result<&mut InstanceObj, KvmError> {
        match self.payload_mut(reference)? {
            Payload::Instance(instance) => Ok(instance),
            other => Err(wrong_payload("instance", other)),
        }
    }

    pub fn array(&self, reference: ObjRef) -> Result<&ArrayObj, KvmError> {
        match self.payload(reference)? {
            Payload::Array(array) => Ok(array),
            other => Err(wrong_payload("array", other)),
        }
    }

    /// 同时借用两个 int 数组（src 只读、dst 可写）——渲染直通路径
    /// （drawImage/drawRegion 图集直绘目标，零中间拷贝）的 disjoint
    /// 借用。同一引用或任一非 int 数组时报错，调用方回退安全路径。
    pub fn int_arrays_src_dst(
        &mut self,
        src: ObjRef,
        dst: ObjRef,
    ) -> Result<(&[i32], &mut [i32]), KvmError> {
        let (si, di) = (src.0 as usize, dst.0 as usize);
        if si == di {
            return Err(KvmError::Internal("src/dst same array".into()));
        }
        let (lo, hi) = if si < di { (si, di) } else { (di, si) };
        let (left, right) = self.objects.split_at_mut(hi);
        // left 覆盖 [0, hi) 含 lo，right[0] 即 hi（split 点右侧首元素）。
        fn as_array(payload: &Payload) -> Result<&ArrayObj, KvmError> {
            match payload {
                Payload::Array(array) => Ok(array),
                other => Err(wrong_payload("array", other)),
            }
        }
        fn as_array_mut(payload: &mut Payload) -> Result<&mut ArrayObj, KvmError> {
            match payload {
                Payload::Array(array) => Ok(array),
                other => Err(wrong_payload("array", other)),
            }
        }
        fn int_slice(array: &ArrayObj) -> Result<&[i32], KvmError> {
            match &array.data {
                ArrayData::Int(data) => Ok(data),
                _ => Err(KvmError::Internal("array not int[]".into())),
            }
        }
        let slot_missing = || KvmError::Internal("src/dst slot empty".into());
        if si < di {
            // lo=src（只读，取自 left），hi=dst（可写，取自 right）。
            let src_obj = left
                .get(lo)
                .and_then(|slot| slot.as_ref())
                .ok_or_else(slot_missing)?;
            let dst_obj = right
                .get_mut(0)
                .and_then(|slot| slot.as_mut())
                .ok_or_else(slot_missing)?;
            let src = int_slice(as_array(&src_obj.payload)?)?;
            let dst = match &mut as_array_mut(&mut dst_obj.payload)?.data {
                ArrayData::Int(data) => data.as_mut_slice(),
                _ => return Err(KvmError::Internal("array not int[]".into())),
            };
            Ok((src, dst))
        } else {
            // hi=src（只读，取自 right），lo=dst（可写，取自 left）。
            let src_obj = right
                .get(0)
                .and_then(|slot| slot.as_ref())
                .ok_or_else(slot_missing)?;
            let dst_obj = left
                .get_mut(lo)
                .and_then(|slot| slot.as_mut())
                .ok_or_else(slot_missing)?;
            let src = int_slice(as_array(&src_obj.payload)?)?;
            let dst = match &mut as_array_mut(&mut dst_obj.payload)?.data {
                ArrayData::Int(data) => data.as_mut_slice(),
                _ => return Err(KvmError::Internal("array not int[]".into())),
            };
            Ok((src, dst))
        }
    }

    pub fn array_mut(&mut self, reference: ObjRef) -> Result<&mut ArrayObj, KvmError> {
        match self.payload_mut(reference)? {
            Payload::Array(array) => Ok(array),
            other => Err(wrong_payload("array", other)),
        }
    }

    pub fn str_obj(&self, reference: ObjRef) -> Result<&StrObj, KvmError> {
        match self.payload(reference)? {
            Payload::Str(string) => Ok(string),
            other => Err(wrong_payload("string", other)),
        }
    }

    pub fn str_obj_mut(&mut self, reference: ObjRef) -> Result<&mut StrObj, KvmError> {
        match self.payload_mut(reference)? {
            Payload::Str(string) => Ok(string),
            other => Err(wrong_payload("string", other)),
        }
    }

    pub fn is_string(&self, reference: ObjRef) -> bool {
        matches!(self.payload(reference), Ok(Payload::Str(_)))
    }
}

fn null_deref(reference: ObjRef) -> KvmError {
    KvmError::Internal(format!(
        "null object reference {reference:?} reached heap (interpreter must check first)"
    ))
}

fn wrong_payload(expected: &str, actual: &Payload) -> KvmError {
    KvmError::Internal(format!("expected {expected} payload, got {actual:?}"))
}
