//! 字节码解码：原始字节码 → 指令数组（跳转目标转成指令索引，cp 引用链接成具体目标）。
//!
//! - tableswitch/lookupswitch 严格 4 字节对齐（相对字节码起点，D10）。
//! - jsr/ret 完整支持（旧编译器 finally，D10）。
//! - wide 前缀归一化为 16 位索引形式。
//! - 解码期做结构校验：操作数长度、跳转目标在界内、栈映射属性忽略（D9）。

use std::rc::Rc;

use crate::KvmError;
use crate::runtime::{ClassId, FieldRef, MethodRef};

use super::cp::{ConstPool, Loadable};
use super::member::CodeInfo;

/// 值类别（对应操作数栈的类别 1/2）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Int,
    Long,
    Float,
    Double,
    Ref,
}

/// 数组访问操作码归一化后的元素类别。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArrOp {
    Int,
    Long,
    Float,
    Double,
    Ref,
    Byte,
    Char,
    Short,
}

/// 整型/浮点二元运算。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Shl,
    Shr,
    Ushr,
    And,
    Or,
    Xor,
}

/// 整型比较关系（分支用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Ge,
    Gt,
    Le,
}

/// ldc/ldc_w/ldc2_w 加载的常量（已链接）。
#[derive(Clone, Debug)]
pub enum LdcConst {
    Int(i32),
    Float(f32),
    Long(i64),
    Double(f64),
    /// 字符串字面量：解释器执行时驻留成 String 对象。
    Str(Rc<str>),
    /// 类常量：解释器执行时映射为 java/lang/Class 对象。
    Class(ClassId),
}

/// 解码后的指令。跳转目标统一为指令索引（u32）。
#[derive(Clone, Debug)]
pub enum Insn {
    Nop,
    PushNull,
    PushInt(i32),
    PushLong(i64),
    PushFloat(f32),
    PushDouble(f64),
    Ldc(LdcConst),
    Load { kind: Kind, index: u16 },
    Store { kind: Kind, index: u16 },
    ArrayLoad(ArrOp),
    ArrayStore(ArrOp),
    Pop,
    Pop2,
    Dup,
    DupX1,
    DupX2,
    Dup2,
    Dup2X1,
    Dup2X2,
    Swap,
    /// 二元运算（含 long/float/double 变体，由 kind 区分）。
    Bin { kind: Kind, op: BinOp },
    /// 取负（int/long/float/double）。
    Neg(Kind),
    Iinc { index: u16, delta: i32 },
    /// 数值转换（i2l/f2d 等）；i2b/i2c/i2s 见 `TruncByte/Char/Short`。
    Conv { from: Kind, to: Kind },
    TruncByte,
    TruncChar,
    TruncShort,
    Lcmp,
    Fcmp { nan_is_one: bool },
    Dcmp { nan_is_one: bool },
    /// 与 0 比较（ifeq 系列，int）。
    BranchZero { op: CmpOp, target: u32 },
    /// int 比较（if_icmp 系列）。
    BranchInt { op: CmpOp, target: u32 },
    /// 引用比较（if_acmp）。
    BranchRef { eq: bool, target: u32 },
    /// null / 非 null 分支。
    BranchNull { is_null: bool, target: u32 },
    Goto(u32),
    Jsr(u32),
    Ret(u16),
    TableSwitch {
        default: u32,
        low: i32,
        targets: Rc<Vec<u32>>,
    },
    LookupSwitch {
        default: u32,
        pairs: Rc<Vec<(i32, u32)>>,
    },
    /// 值返回；None = void return。
    Return(Option<Kind>),
    GetStatic(FieldRef),
    PutStatic(FieldRef),
    GetField(FieldRef),
    PutField(FieldRef),
    InvokeVirtual(MethodRef),
    InvokeSpecial(MethodRef),
    InvokeStatic(MethodRef),
    InvokeInterface(MethodRef),
    New(ClassId),
    /// newarray 的基本类型（4..=11）。
    NewArray(u8),
    ANewArray(ClassId),
    ArrayLength,
    Athrow,
    CheckCast(ClassId),
    InstanceOf(ClassId),
    MonitorEnter,
    MonitorExit,
    MultiNewArray { class: ClassId, dims: u8 },
    /// 复合指令（superopcode）：两条相邻指令的解码期融合。
    ///
    /// 语义：执行 `a`，结果为 Next 时继续执行 `b` 并返回其结果（b 为
    /// Next 时直接落到复合后继槽）；其余结果原样透传。合法性约束由
    /// 解码期融合 pass 保证（`fuse_superopcodes`）：`a` 白名单保证其
    /// 结果只会是 Next/Unwind 或无副作用的 Pushed/Blocked（clinit 族
    /// 在副作用前退出、monitorenter 恢复操作数——重执行从复合头重入
    /// 栈形不变）；`b` 排除 clinit 触发族与 monitorenter（其 Pushed/
    /// Blocked 不推 pc，重执行会重复 `a` 的栈效果）。原 `b` 槽位改写
    /// 为 `Goto(i+2)` 蹦床：invoke 压帧续跑、native 让出、jsr 返回
    /// 地址、异常恢复等一切落到 i+1 的路径统一跳到复合后继，全部
    /// 指令索引引用（跳转目标/处理器区间）保持不变。
    Fused { a: Box<Insn>, b: Box<Insn> },
}

/// 解码后的 catch 处理器（范围/入口均为指令索引）。
#[derive(Clone, Debug)]
pub struct Handler {
    pub start: u32,
    pub end: u32,
    pub handler: u32,
    pub catch_class: Option<ClassId>,
}

/// 解码后的方法体。
#[derive(Debug)]
pub struct CodeBody {
    pub insns: Vec<Insn>,
    pub max_locals: usize,
    pub max_stack: usize,
    pub handlers: Vec<Handler>,
}

/// 解码期链接器：把常量池引用解析为类空间中的具体目标。
/// 由 `runtime::classspace`（经 Vm）实现；引用的类在此被递归加载（eager 模型）。
pub trait Linker {
    fn link_class(&mut self, name: &str) -> Result<ClassId, KvmError>;
    fn link_field(
        &mut self,
        owner: ClassId,
        name: &str,
        desc: &str,
        is_static: bool,
    ) -> Result<FieldRef, KvmError>;
    fn link_method(
        &mut self,
        class: ClassId,
        name: &str,
        desc: &str,
    ) -> Result<MethodRef, KvmError>;
}

/// 待回填的跳转目标（前向引用在解码结束时统一回填）。
///
/// 不携带数据：目标数组只由指令本身持有（强引用计数为 1），
/// 回填时 `Rc::get_mut` 原地改写；default 槽复用 pending 的目标偏移。
enum Fixup {
    /// 单一目标槽（分支指令）。
    Direct,
    /// tableswitch：default 槽 + 目标数组整体。
    Table,
    /// lookupswitch：default 槽 + 匹配表整体。
    Lookup,
}

/// 解码 `CodeInfo` 为 `CodeBody`。
pub fn decode(
    info: &CodeInfo,
    cp: &ConstPool,
    linker: &mut dyn Linker,
) -> Result<Rc<CodeBody>, KvmError> {
    let bytes = &info.code[..];
    let mut insns: Vec<Insn> = Vec::with_capacity(bytes.len() / 2 + 4);
    // 字节偏移 → 指令索引：仅在指令起点有值；末位哨兵（偏移 == 长度）= 指令总数。
    let mut offset_to_index: Vec<Option<u32>> = vec![None; bytes.len() + 1];
    let mut pendings: Vec<(usize, Fixup, u32)> = Vec::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        offset_to_index[offset] = Some(insns.len() as u32);
        let insn = decode_one(bytes, &mut offset, cp, linker, &mut pendings, insns.len())?;
        insns.push(insn);
    }
    offset_to_index[bytes.len()] = Some(insns.len() as u32);
    let insns_len = insns.len() as u32;
    let to_index = |byte_offset: u32| -> Result<u32, KvmError> {
        match offset_to_index
            .get(byte_offset as usize)
            .copied()
            .flatten()
        {
            // 末位哨兵（== 指令总数）不是合法的分支目标。
            Some(index) if index < insns_len => Ok(index),
            _ => Err(KvmError::ClassFormat(format!(
                "branch target {byte_offset:#x} out of bounds"
            ))),
        }
    };
    // 回填前向跳转。
    for (insn_index, fixup, target_offset) in pendings {
        match fixup {
            Fixup::Direct => {
                let target = to_index(target_offset)?;
                apply_direct(&mut insns[insn_index], target)?;
            }
            Fixup::Table => {
                let default = to_index(target_offset)?;
                let Insn::TableSwitch {
                    default: slot,
                    targets,
                    ..
                } = &mut insns[insn_index]
                else {
                    return Err(KvmError::Internal("table fixup on non-switch".into()));
                };
                *slot = default;
                let targets = Rc::get_mut(targets)
                    .ok_or_else(|| KvmError::Internal("switch target list is shared".into()))?;
                for target in targets.iter_mut() {
                    *target = to_index(*target)?;
                }
            }
            Fixup::Lookup => {
                let default = to_index(target_offset)?;
                let Insn::LookupSwitch {
                    default: slot,
                    pairs,
                    ..
                } = &mut insns[insn_index]
                else {
                    return Err(KvmError::Internal("lookup fixup on non-switch".into()));
                };
                *slot = default;
                let pairs = Rc::get_mut(pairs)
                    .ok_or_else(|| KvmError::Internal("switch pair list is shared".into()))?;
                for (_, target) in pairs.iter_mut() {
                    *target = to_index(*target)?;
                }
            }
        }
    }
    // 异常表转换 + catch 类链接。
    let mut handlers = Vec::with_capacity(info.handlers.len());
    for handler in &info.handlers {
        let start = to_index(u32::from(handler.start_pc))?;
        // end_pc 是排他字节偏移；允许等于代码长度（映射到指令数哨兵）。
        let end = offset_to_index
            .get(handler.end_pc as usize)
            .copied()
            .flatten()
            .ok_or_else(|| KvmError::ClassFormat("handler end out of bounds".into()))?;
        let entry = to_index(u32::from(handler.handler_pc))?;
        let catch_class = match &handler.catch_class {
            Some(name) => Some(linker.link_class(name)?),
            None => None,
        };
        if start >= end {
            return Err(KvmError::ClassFormat("empty handler range".into()));
        }
        handlers.push(Handler {
            start,
            end,
            handler: entry,
            catch_class,
        });
    }
    let insns = fuse_superopcodes(insns, &handlers);
    Ok(Rc::new(CodeBody {
        insns,
        max_locals: info.max_locals as usize,
        max_stack: info.max_stack as usize,
        handlers,
    }))
}

// ---- superopcode 融合 pass ------------------------------------------------

/// 环境开关快照：SKYENGINE_KVM_NO_FUSE=1 时跳过融合（A/B 测量用）。
fn fusion_disabled() -> bool {
    std::env::var_os("SKYENGINE_KVM_NO_FUSE").is_some_and(|value| value == "1")
}

/// 相邻指令对的原位融合（superopcode）：`insns[i]`+`insns[i+1]` →
/// `Fused`，原 `i+1` 槽改写为 `Goto(i+2)` 蹦床。指令数组长度与全部
/// 槽位索引不变，跳转目标与处理器区间无需重映射。
///
/// 融合条件（全部满足）：
/// - `i+1` 不在保护集合：不是任何跳转目标（分支/switch/default），
///   不落在任何处理器区间的 start/end 边界（故障 pc 折叠到复合槽 i，
///   边界两侧的处理器覆盖集必须相同），不是处理器入口；
/// - `i+2` 在界内（蹦床目标有效；顺带排除 `b` 为末指令的融合）；
/// - 指令对满足首/尾白名单（见 `fuse_first_ok`/`fuse_second_ok`）。
fn fuse_superopcodes(insns: Vec<Insn>, handlers: &[Handler]) -> Vec<Insn> {
    if fusion_disabled() {
        return insns;
    }
    // 保护集合：控制流可进入的槽位 + 处理器覆盖边界。
    let mut protected = vec![false; insns.len()];
    let mut mark = |index: u32| {
        if let Some(slot) = protected.get_mut(index as usize) {
            *slot = true;
        }
    };
    for insn in &insns {
        match insn {
            Insn::BranchZero { target, .. }
            | Insn::BranchInt { target, .. }
            | Insn::BranchRef { target, .. }
            | Insn::BranchNull { target, .. }
            | Insn::Goto(target)
            | Insn::Jsr(target) => mark(*target),
            Insn::TableSwitch { default, targets, .. } => {
                mark(*default);
                for target in targets.iter() {
                    mark(*target);
                }
            }
            Insn::LookupSwitch { default, pairs, .. } => {
                mark(*default);
                for (_, target) in pairs.iter() {
                    mark(*target);
                }
            }
            _ => {}
        }
    }
    for handler in handlers {
        mark(handler.start);
        mark(handler.end);
        mark(handler.handler);
    }
    let mut out = insns;
    let mut i = 0usize;
    while let Some(tail) = i.checked_add(1).filter(|&next| next < out.len()) {
        let successor = tail + 1;
        // 末对（successor == len）蹦床目标越界：仅当 `b` 永不落入蹦床槽
        //（必跳转/必返回/必展开）时融合——越界蹦床为死代码。
        let trampoline_in_bounds = successor < out.len();
        if (trampoline_in_bounds || fuse_tail_never_falls(&out[tail]))
            && !protected[tail]
            && fuse_first_ok(&out[i])
            && fuse_second_ok(&out[tail])
        {
            let b = std::mem::replace(&mut out[tail], Insn::Nop);
            let a = std::mem::replace(&mut out[i], Insn::Nop);
            out[i] = Insn::Fused {
                a: Box::new(a),
                b: Box::new(b),
            };
            out[tail] = Insn::Goto(successor as u32);
            i = successor;
        } else {
            i = tail;
        }
    }
    out
}

/// 复合尾永不落入蹦床槽的指令（末对融合时蹦床目标越界仍安全）。
/// 注意 jsr 不在其中：其返回地址指向蹦床，经 ret 会真实落入。
fn fuse_tail_never_falls(insn: &Insn) -> bool {
    matches!(
        insn,
        Insn::Return(_)
            | Insn::Goto(_)
            | Insn::Athrow
            | Insn::TableSwitch { .. }
            | Insn::LookupSwitch { .. }
    )
}

/// 复合首指令白名单：排除 invoke 族（压帧前推进调用帧 pc，被调帧返回
/// 后落在蹦床上——`b` 会被跳过）、必跳转/必返回族（`b` 成为死代码）。
/// 其余指令要么结果只有 Next/Unwind，要么以无副作用方式返回
/// Pushed/Blocked（重执行从复合头重入，栈形不变）。
fn fuse_first_ok(insn: &Insn) -> bool {
    !matches!(
        insn,
        Insn::InvokeVirtual(_)
            | Insn::InvokeSpecial(_)
            | Insn::InvokeStatic(_)
            | Insn::InvokeInterface(_)
            | Insn::Return(_)
            | Insn::Goto(_)
            | Insn::Jsr(_)
            | Insn::Ret(_)
            | Insn::TableSwitch { .. }
            | Insn::LookupSwitch { .. }
            | Insn::Athrow
            | Insn::Fused { .. }
    )
}

/// 复合尾指令白名单：排除三类——
/// - clinit 触发族与 monitorenter：Pushed/Blocked 路径不推 pc，重执行
///   会重复 `a` 的栈效果；
/// - invoke 族（含 virtual/special/interface）：压帧前 pc 推进到蹦床槽，
///   被调帧返回后需经蹦床跳一步——invoke 密集负载下该额外分派超过
///   融合节省（实测 splash 渲染 -7%），故不融合（蹦床随之近乎死代码，
///   仅 jsr 作复合尾时经 ret 落入）。
fn fuse_second_ok(insn: &Insn) -> bool {
    !matches!(
        insn,
        Insn::GetStatic(_)
            | Insn::PutStatic(_)
            | Insn::InvokeStatic(_)
            | Insn::InvokeVirtual(_)
            | Insn::InvokeSpecial(_)
            | Insn::InvokeInterface(_)
            | Insn::New(_)
            | Insn::MonitorEnter
            | Insn::Fused { .. }
    )
}

/// 回填单目标分支指令的目标槽。
fn apply_direct(insn: &mut Insn, target: u32) -> Result<(), KvmError> {
    match insn {
        Insn::BranchZero { target: slot, .. }
        | Insn::BranchInt { target: slot, .. }
        | Insn::BranchRef { target: slot, .. }
        | Insn::BranchNull { target: slot, .. }
        | Insn::Goto(slot)
        | Insn::Jsr(slot) => {
            *slot = target;
            Ok(())
        }
        other => Err(KvmError::Internal(format!(
            "fixup on non-branch instruction {other:?}"
        ))),
    }
}

/// 解码单条指令。`offset` 前进到下一条指令起点。
fn decode_one(
    bytes: &[u8],
    offset: &mut usize,
    cp: &ConstPool,
    linker: &mut dyn Linker,
    pendings: &mut Vec<(usize, Fixup, u32)>,
    insn_index: usize,
) -> Result<Insn, KvmError> {
    let start = *offset;
    let op = *bytes
        .get(start)
        .ok_or_else(|| KvmError::ClassFormat("truncated opcode".into()))?;
    *offset = start + 1;
    macro_rules! u8_at {
        ($pos:expr) => {
            *bytes.get($pos).ok_or_else(|| KvmError::ClassFormat("truncated u8 operand".into()))?
        };
    }
    macro_rules! i8_at {
        ($pos:expr) => {
            u8_at!($pos) as i8
        };
    }
    macro_rules! u16_at {
        ($pos:expr) => {{
            let pos = $pos;
            let lo = u8_at!(pos) as u16;
            let hi = u8_at!(pos + 1) as u16;
            (lo << 8) | hi
        }};
    }
    macro_rules! i16_at {
        ($pos:expr) => {
            u16_at!($pos) as i16
        };
    }
    macro_rules! u32_at {
        ($pos:expr) => {{
            let pos = $pos;
            let b0 = u8_at!(pos) as u32;
            let b1 = u8_at!(pos + 1) as u32;
            let b2 = u8_at!(pos + 2) as u32;
            let b3 = u8_at!(pos + 3) as u32;
            (b0 << 24) | (b1 << 16) | (b2 << 8) | b3
        }};
    }
    macro_rules! i32_at {
        ($pos:expr) => {
            u32_at!($pos) as i32
        };
    }
    // 记录分支目标（u32 字节偏移），并推进 offset。
    macro_rules! branch {
        ($insn:expr, $target:expr) => {{
            pendings.push((insn_index, Fixup::Direct, $target));
            $insn
        }};
    }
    let insn = match op {
        0x00 => Insn::Nop,
        0x01 => Insn::PushNull,
        0x02..=0x08 => Insn::PushInt(i32::from(op) - 3), // iconst_m1..iconst_5
        0x09 => Insn::PushLong(0),
        0x0a => Insn::PushLong(1),
        0x0b..=0x0d => Insn::PushFloat(f32::from(op) - 0x0b as f32),
        0x0e => Insn::PushDouble(0.0),
        0x0f => Insn::PushDouble(1.0),
        0x10 => {
            *offset += 1;
            Insn::PushInt(i32::from(i8_at!(start + 1)))
        }
        0x11 => {
            *offset += 2;
            Insn::PushInt(i32::from(i16_at!(start + 1)))
        }
        0x12 => {
            *offset += 1;
            let index = u8_at!(start + 1) as u16;
            Insn::Ldc(resolve_loadable(cp, linker, index)?)
        }
        0x13 => {
            *offset += 2;
            let index = u16_at!(start + 1);
            Insn::Ldc(resolve_loadable(cp, linker, index)?)
        }
        0x14 => {
            *offset += 2;
            let index = u16_at!(start + 1);
            Insn::Ldc(resolve_loadable(cp, linker, index)?)
        }
        0x15..=0x19 => {
            *offset += 1;
            Insn::Load {
                kind: kind_of_load(op),
                index: u16::from(u8_at!(start + 1)),
            }
        }
        0x1a..=0x1d => Insn::Load {
            kind: Kind::Int,
            index: u16::from(op - 0x1a),
        },
        0x1e..=0x21 => Insn::Load {
            kind: Kind::Long,
            index: u16::from(op - 0x1e),
        },
        0x22..=0x25 => Insn::Load {
            kind: Kind::Float,
            index: u16::from(op - 0x22),
        },
        0x26..=0x29 => Insn::Load {
            kind: Kind::Double,
            index: u16::from(op - 0x26),
        },
        0x2a..=0x2d => Insn::Load {
            kind: Kind::Ref,
            index: u16::from(op - 0x2a),
        },
        0x2e..=0x35 => Insn::ArrayLoad(arr_op(op)),
        0x36..=0x3a => {
            *offset += 1;
            Insn::Store {
                kind: kind_of_load(op - 0x36 + 0x15),
                index: u16::from(u8_at!(start + 1)),
            }
        }
        0x3b..=0x3e => Insn::Store {
            kind: Kind::Int,
            index: u16::from(op - 0x3b),
        },
        0x3f..=0x42 => Insn::Store {
            kind: Kind::Long,
            index: u16::from(op - 0x3f),
        },
        0x43..=0x46 => Insn::Store {
            kind: Kind::Float,
            index: u16::from(op - 0x43),
        },
        0x47..=0x4a => Insn::Store {
            kind: Kind::Double,
            index: u16::from(op - 0x47),
        },
        0x4b..=0x4e => Insn::Store {
            kind: Kind::Ref,
            index: u16::from(op - 0x4b),
        },
        0x4f..=0x56 => Insn::ArrayStore(arr_op(op - 0x4f + 0x2e)),
        0x57 => Insn::Pop,
        0x58 => Insn::Pop2,
        0x59 => Insn::Dup,
        0x5a => Insn::DupX1,
        0x5b => Insn::DupX2,
        0x5c => Insn::Dup2,
        0x5d => Insn::Dup2X1,
        0x5e => Insn::Dup2X2,
        0x5f => Insn::Swap,
        0x60..=0x73 => {
            // iadd..drem：add/sub/mul/div/rem × (int, long, float, double)。
            let group = op - 0x60;
            let kind = [Kind::Int, Kind::Long, Kind::Float, Kind::Double][(group % 4) as usize];
            let bin =
                [BinOp::Add, BinOp::Sub, BinOp::Mul, BinOp::Div, BinOp::Rem][(group / 4) as usize];
            Insn::Bin { kind, op: bin }
        }
        0x74..=0x77 => {
            // ineg/lneg/fneg/dneg。
            Insn::Neg([Kind::Int, Kind::Long, Kind::Float, Kind::Double][(op - 0x74) as usize])
        }
        0x78..=0x83 => {
            // ishl..lxor：shl/shr/ushr/and/or/xor × (int, long)。
            let pair = op - 0x78;
            let kind = if pair % 2 == 0 { Kind::Int } else { Kind::Long };
            let bin = [BinOp::Shl, BinOp::Shr, BinOp::Ushr, BinOp::And, BinOp::Or, BinOp::Xor]
                [(pair / 2) as usize];
            Insn::Bin { kind, op: bin }
        }
        0x84 => {
            // iinc index delta
            let index = u16::from(u8_at!(start + 1));
            let delta = i32::from(i8_at!(start + 2));
            *offset += 2;
            Insn::Iinc { index, delta }
        }
        0x85..=0x93 => {
            use Kind::*;
            // i2l..i2s 数值转换；i2b/i2c/i2s 是窄化截断，单列。
            let (from, to) = match op {
                0x85 => (Int, Long),
                0x86 => (Int, Float),
                0x87 => (Int, Double),
                0x88 => (Long, Int),
                0x89 => (Long, Float),
                0x8a => (Long, Double),
                0x8b => (Float, Int),
                0x8c => (Float, Long),
                0x8d => (Float, Double),
                0x8e => (Double, Int),
                0x8f => (Double, Long),
                _ => (Double, Float), // 0x90 d2f
            };
            match op {
                0x91 => Insn::TruncByte,
                0x92 => Insn::TruncChar,
                0x93 => Insn::TruncShort,
                _ => Insn::Conv { from, to },
            }
        }
        0x94 => Insn::Lcmp,
        0x95 => Insn::Fcmp { nan_is_one: false },
        0x96 => Insn::Fcmp { nan_is_one: true },
        0x97 => Insn::Dcmp { nan_is_one: false },
        0x98 => Insn::Dcmp { nan_is_one: true },
        0x99..=0x9e => {
            *offset += 2;
            let target = i16_at!(start + 1) as i32 + start as i32;
            let op = match op {
                0x99 => CmpOp::Eq,
                0x9a => CmpOp::Ne,
                0x9b => CmpOp::Lt,
                0x9c => CmpOp::Ge,
                0x9d => CmpOp::Gt,
                _ => CmpOp::Le,
            };
            branch!(Insn::BranchZero { op, target: target as u32 }, target as u32)
        }
        0x9f..=0xa4 => {
            *offset += 2;
            let target = i16_at!(start + 1) as i32 + start as i32;
            let op = match op {
                0x9f => CmpOp::Eq,
                0xa0 => CmpOp::Ne,
                0xa1 => CmpOp::Lt,
                0xa2 => CmpOp::Ge,
                0xa3 => CmpOp::Gt,
                _ => CmpOp::Le,
            };
            branch!(Insn::BranchInt { op, target: target as u32 }, target as u32)
        }
        0xa5 | 0xa6 => {
            *offset += 2;
            let target = i16_at!(start + 1) as i32 + start as i32;
            branch!(
                Insn::BranchRef {
                    eq: op == 0xa5,
                    target: target as u32,
                },
                target as u32
            )
        }
        0xa7 => {
            *offset += 2;
            let target = i16_at!(start + 1) as i32 + start as i32;
            branch!(Insn::Goto(target as u32), target as u32)
        }
        0xa8 => {
            *offset += 2;
            let target = i16_at!(start + 1) as i32 + start as i32;
            branch!(Insn::Jsr(target as u32), target as u32)
        }
        0xa9 => {
            *offset += 1;
            Insn::Ret(u16::from(u8_at!(start + 1)))
        }
        0xaa => {
            // tableswitch：跳过对齐填充（相对字节码起点 4 字节对齐）。
            let pad = (4 - ((start + 1) % 4)) % 4;
            let base = start + 1 + pad;
            let default = i32_at!(base);
            let low = i32_at!(base + 4);
            let high = i32_at!(base + 8);
            if high < low {
                return Err(KvmError::ClassFormat("tableswitch high < low".into()));
            }
            let count = (high as i64 - low as i64 + 1) as usize;
            if count > 1 << 16 {
                return Err(KvmError::ClassFormat("tableswitch too large".into()));
            }
            let mut targets = Vec::with_capacity(count);
            for index in 0..count {
                let at = base + 12 + index * 4;
                targets.push((i32_at!(at) + start as i32) as u32);
            }
            *offset = base + 12 + count * 4;
            let default_target = (default + start as i32) as u32;
            let list = Rc::new(targets);
            // 目标数组只由指令持有（强引用计数 1）；default 偏移由 pending 携带，
            // 回填期同时改写 default 槽与数组内容。
            pendings.push((insn_index, Fixup::Table, default_target));
            Insn::TableSwitch {
                default: default_target,
                low,
                targets: list,
            }
        }
        0xab => {
            let pad = (4 - ((start + 1) % 4)) % 4;
            let base = start + 1 + pad;
            let default = i32_at!(base);
            let pairs_count = i32_at!(base + 4) as usize;
            if pairs_count > 1 << 16 {
                return Err(KvmError::ClassFormat("lookupswitch too large".into()));
            }
            let mut pairs = Vec::with_capacity(pairs_count);
            for index in 0..pairs_count {
                let at = base + 8 + index * 8;
                let match_value = i32_at!(at);
                let target = (i32_at!(at + 4) + start as i32) as u32;
                pairs.push((match_value, target));
            }
            *offset = base + 8 + pairs_count * 8;
            let default_target = (default + start as i32) as u32;
            let list = Rc::new(pairs);
            pendings.push((insn_index, Fixup::Lookup, default_target));
            Insn::LookupSwitch {
                default: default_target,
                pairs: list,
            }
        }
        0xac..=0xb0 => Insn::Return(Some(match op {
            0xac => Kind::Int,
            0xad => Kind::Long,
            0xae => Kind::Float,
            0xaf => Kind::Double,
            _ => Kind::Ref,
        })),
        0xb1 => Insn::Return(None),
        0xb2 => {
            *offset += 2;
            let (class, name, desc) = cp
                .fieldref(u16_at!(start + 1))
                .map_err(|e| KvmError::ClassFormat(e.to_string()))?;
            let owner = linker.link_class(class)?;
            Insn::GetStatic(linker.link_field(owner, name, desc, true)?)
        }
        0xb3 => {
            *offset += 2;
            let (class, name, desc) = cp
                .fieldref(u16_at!(start + 1))
                .map_err(|e| KvmError::ClassFormat(e.to_string()))?;
            let owner = linker.link_class(class)?;
            Insn::PutStatic(linker.link_field(owner, name, desc, true)?)
        }
        0xb4 => {
            *offset += 2;
            let (class, name, desc) = cp
                .fieldref(u16_at!(start + 1))
                .map_err(|e| KvmError::ClassFormat(e.to_string()))?;
            let owner = linker.link_class(class)?;
            Insn::GetField(linker.link_field(owner, name, desc, false)?)
        }
        0xb5 => {
            *offset += 2;
            let (class, name, desc) = cp
                .fieldref(u16_at!(start + 1))
                .map_err(|e| KvmError::ClassFormat(e.to_string()))?;
            let owner = linker.link_class(class)?;
            Insn::PutField(linker.link_field(owner, name, desc, false)?)
        }
        0xb6..=0xb9 => {
            let index = if op == 0xb9 {
                *offset += 4; // u16 + count + 0
                u16_at!(start + 1)
            } else {
                *offset += 2;
                u16_at!(start + 1)
            };
            let (class, name, desc) = cp
                .methodref(index)
                .map_err(|e| KvmError::ClassFormat(e.to_string()))?;
            let owner = linker.link_class(class)?;
            let method = linker.link_method(owner, name, desc)?;
            match op {
                0xb6 => Insn::InvokeVirtual(method),
                0xb7 => Insn::InvokeSpecial(method),
                0xb8 => Insn::InvokeStatic(method),
                _ => Insn::InvokeInterface(method),
            }
        }
        0xba => {
            return Err(KvmError::ClassFormat(
                "invokedynamic is not supported in CLDC".into(),
            ))
        }
        0xbb => {
            *offset += 2;
            let class = linker.link_class(
                cp.class_name(u16_at!(start + 1))
                    .map_err(|e| KvmError::ClassFormat(e.to_string()))?,
            )?;
            Insn::New(class)
        }
        0xbc => {
            *offset += 1;
            let atype = u8_at!(start + 1);
            if !(4..=11).contains(&atype) {
                return Err(KvmError::ClassFormat(format!(
                    "invalid newarray type {atype}"
                )));
            }
            Insn::NewArray(atype)
        }
        0xbd => {
            *offset += 2;
            let class = linker.link_class(
                cp.class_name(u16_at!(start + 1))
                    .map_err(|e| KvmError::ClassFormat(e.to_string()))?,
            )?;
            Insn::ANewArray(class)
        }
        0xbe => Insn::ArrayLength,
        0xbf => Insn::Athrow,
        0xc0 | 0xc1 => {
            *offset += 2;
            let class = linker.link_class(
                cp.class_name(u16_at!(start + 1))
                    .map_err(|e| KvmError::ClassFormat(e.to_string()))?,
            )?;
            if op == 0xc0 {
                Insn::CheckCast(class)
            } else {
                Insn::InstanceOf(class)
            }
        }
        0xc2 => Insn::MonitorEnter,
        0xc3 => Insn::MonitorExit,
        0xc4 => {
            // wide 前缀。
            let wide_op = u8_at!(start + 1);
            match wide_op {
                0x15..=0x19 => {
                    *offset += 3;
                    Insn::Load {
                        kind: kind_of_load(wide_op),
                        index: u16_at!(start + 2),
                    }
                }
                0x36..=0x3a => {
                    *offset += 3;
                    Insn::Store {
                        kind: kind_of_load(wide_op - 0x36 + 0x15),
                        index: u16_at!(start + 2),
                    }
                }
                0x84 => {
                    *offset += 5;
                    Insn::Iinc {
                        index: u16_at!(start + 2),
                        delta: i32::from(i16_at!(start + 4)),
                    }
                }
                0xa9 => {
                    *offset += 3;
                    Insn::Ret(u16_at!(start + 2))
                }
                other => {
                    return Err(KvmError::ClassFormat(format!(
                        "invalid wide opcode {other:#04x}"
                    )))
                }
            }
        }
        0xc5 => {
            *offset += 3;
            let class = linker.link_class(
                cp.class_name(u16_at!(start + 1))
                    .map_err(|e| KvmError::ClassFormat(e.to_string()))?,
            )?;
            Insn::MultiNewArray {
                class,
                dims: u8_at!(start + 3),
            }
        }
        0xc6 | 0xc7 => {
            *offset += 2;
            let target = i16_at!(start + 1) as i32 + start as i32;
            branch!(
                Insn::BranchNull {
                    is_null: op == 0xc6,
                    target: target as u32,
                },
                target as u32
            )
        }
        0xc8 => {
            *offset += 4;
            let target = i32_at!(start + 1) + start as i32;
            branch!(Insn::Goto(target as u32), target as u32)
        }
        0xc9 => {
            *offset += 4;
            let target = i32_at!(start + 1) + start as i32;
            branch!(Insn::Jsr(target as u32), target as u32)
        }
        0xca => Insn::Nop, // breakpoint：不期待出现，按 nop 容忍
        other => {
            return Err(KvmError::ClassFormat(format!(
                "unknown opcode {other:#04x}"
            )))
        }
    };
    Ok(insn)
}

fn kind_of_load(op: u8) -> Kind {
    match op {
        0x15 => Kind::Int,
        0x16 => Kind::Long,
        0x17 => Kind::Float,
        0x18 => Kind::Double,
        _ => Kind::Ref,
    }
}

fn arr_op(op: u8) -> ArrOp {
    match op {
        0x2e | 0x4f => ArrOp::Int,
        0x2f | 0x50 => ArrOp::Long,
        0x30 | 0x51 => ArrOp::Float,
        0x31 | 0x52 => ArrOp::Double,
        0x32 | 0x53 => ArrOp::Ref,
        0x33 | 0x54 => ArrOp::Byte,
        0x34 | 0x55 => ArrOp::Char,
        _ => ArrOp::Short,
    }
}

fn resolve_loadable(
    cp: &ConstPool,
    linker: &mut dyn Linker,
    index: u16,
) -> Result<LdcConst, KvmError> {
    Ok(match cp
        .loadable(index)
        .map_err(|e| KvmError::ClassFormat(e.to_string()))?
    {
        Loadable::Int(v) => LdcConst::Int(v),
        Loadable::Float(v) => LdcConst::Float(v),
        Loadable::Long(v) => LdcConst::Long(v),
        Loadable::Double(v) => LdcConst::Double(v),
        Loadable::Str(text) => LdcConst::Str(Rc::from(text.as_str())),
        Loadable::Class(name) => LdcConst::Class(linker.link_class(&name)?),
    })
}
