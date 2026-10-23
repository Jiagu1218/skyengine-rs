//! 指令执行（T009）：全操作码 match。
//!
//! 指令只操作帧栈与 `Machine`；控制流结果经 `InsnOutcome` 返回给
//! `run` 循环（mod.rs）。约定：
//! - 触发检查（clinit/monitor 竞争）在弹操作数之前完成，保证阻塞/
//!   压帧路径重执行时操作数栈形不变；
//! - cat2 值（long/double）在操作数栈/参数表/返回值中一律带 Pad 槽；
//! - 防御检查失败返回 `Unwind::Fault`，由 run 循环搜索处理器。

use crate::classfile::code::{ArrOp, BinOp, CmpOp, Insn, Kind, LdcConst};
use crate::classfile::member::acc;
use crate::runtime::classspace::{ArrayElem, InitState};
use crate::runtime::monitor::EnterOutcome;
use crate::runtime::object::ElemType;
use crate::runtime::{ClassId, MethodRef, ObjRef, Slot, ThreadId};
use crate::KvmError;

use super::{
    advance_clinit_chain, alloc_gc, ensure_initialized, is_assignable, pop_int, pop_kind, pop_ref,
    pop1, push_kind, push_method_frame, push_slot_value, push1, runtime_class_of, Fault, Frame,
    FrameKind, InsnOutcome, Machine, NativeCall, Unwind,
};

/// 执行单条指令（pc 推进策略见 `InsnOutcome`）。
///
/// 按引用执行：调用方持有 body 的 `Rc` 克隆保活（帧栈可变借用与指令
/// 引用无重叠），消除逐指令的 `Insn` 深克隆。非 Copy 载荷（ldc 常量、
/// switch 目标表）经 `ref` 绑定借用。
pub(super) fn execute_insn(
    machine: &mut Machine,
    frames: &mut Vec<Frame>,
    thread: ThreadId,
    insn: &Insn,
) -> Result<InsnOutcome, KvmError> {
    match *insn {
        // ---- 常量与局部变量 ----
        Insn::Nop => Ok(InsnOutcome::Next),
        Insn::PushNull => {
            push1(frames, Slot::Ref(ObjRef::NULL));
            Ok(InsnOutcome::Next)
        }
        Insn::PushInt(value) => {
            push1(frames, Slot::Int(value));
            Ok(InsnOutcome::Next)
        }
        Insn::PushLong(value) => {
            push_slot_value(frames, Slot::Long(value));
            Ok(InsnOutcome::Next)
        }
        Insn::PushFloat(value) => {
            push1(frames, Slot::Float(value));
            Ok(InsnOutcome::Next)
        }
        Insn::PushDouble(value) => {
            push_slot_value(frames, Slot::Double(value));
            Ok(InsnOutcome::Next)
        }
        Insn::Ldc(ref constant) => exec_ldc(machine, frames, constant),
        Insn::Load { kind, index } => {
            let slot = *frames
                .last()
                .expect("frame stack empty")
                .locals
                .get(index as usize)
                .ok_or_else(|| bad_local(index))?;
            push_kind(frames, kind, slot);
            Ok(InsnOutcome::Next)
        }
        Insn::Store { kind, index } => {
            let value = pop_kind(frames, kind)?;
            let frame = frames.last_mut().expect("frame stack empty");
            if index as usize >= frame.locals.len() {
                return Err(bad_local(index));
            }
            frame.locals[index as usize] = value;
            Ok(InsnOutcome::Next)
        }
        Insn::Iinc { index, delta } => {
            let frame = frames.last_mut().expect("frame stack empty");
            let slot = *frame
                .locals
                .get(index as usize)
                .ok_or_else(|| bad_local(index))?;
            let Slot::Int(value) = slot else {
                return Err(KvmError::Internal("iinc on non-int local".into()));
            };
            frame.locals[index as usize] = Slot::Int(value.wrapping_add(delta));
            Ok(InsnOutcome::Next)
        }

        // ---- 数组读写 ----
        Insn::ArrayLoad(op) => array_load(machine, frames, op),
        Insn::ArrayStore(op) => array_store(machine, frames, op),
        Insn::ArrayLength => {
            let reference = pop_ref(frames)?;
            if reference.is_null() {
                return npe();
            }
            let len = machine.heap.array(reference)?.data.len();
            push1(frames, Slot::Int(len as i32));
            Ok(InsnOutcome::Next)
        }
        Insn::NewArray(atype) => exec_new_array(machine, frames, atype),
        Insn::ANewArray(component) => exec_anew_array(machine, frames, component),
        Insn::MultiNewArray { class, dims } => exec_multi_new_array(machine, frames, class, dims),

        // ---- 栈操作（槽级实现；cat1/cat2 分组由校验器保证） ----
        Insn::Pop => {
            pop1(frames)?;
            Ok(InsnOutcome::Next)
        }
        Insn::Pop2 => {
            pop1(frames)?;
            pop1(frames)?;
            Ok(InsnOutcome::Next)
        }
        Insn::Dup => {
            let value = pop1(frames)?;
            push1(frames, value);
            push1(frames, value);
            Ok(InsnOutcome::Next)
        }
        Insn::DupX1 => {
            let v1 = pop1(frames)?;
            let v2 = pop1(frames)?;
            push1(frames, v1);
            push1(frames, v2);
            push1(frames, v1);
            Ok(InsnOutcome::Next)
        }
        Insn::DupX2 => {
            let v1 = pop1(frames)?;
            let v2 = pop1(frames)?;
            let v3 = pop1(frames)?;
            push1(frames, v1);
            push1(frames, v3);
            push1(frames, v2);
            push1(frames, v1);
            Ok(InsnOutcome::Next)
        }
        Insn::Dup2 => {
            let v1 = pop1(frames)?;
            let v2 = pop1(frames)?;
            push1(frames, v2);
            push1(frames, v1);
            push1(frames, v2);
            push1(frames, v1);
            Ok(InsnOutcome::Next)
        }
        Insn::Dup2X1 => {
            let v1 = pop1(frames)?;
            let v2 = pop1(frames)?;
            let v3 = pop1(frames)?;
            push1(frames, v2);
            push1(frames, v1);
            push1(frames, v3);
            push1(frames, v2);
            push1(frames, v1);
            Ok(InsnOutcome::Next)
        }
        Insn::Dup2X2 => {
            let v1 = pop1(frames)?;
            let v2 = pop1(frames)?;
            let v3 = pop1(frames)?;
            let v4 = pop1(frames)?;
            push1(frames, v2);
            push1(frames, v1);
            push1(frames, v4);
            push1(frames, v3);
            push1(frames, v2);
            push1(frames, v1);
            Ok(InsnOutcome::Next)
        }
        Insn::Swap => {
            let v1 = pop1(frames)?;
            let v2 = pop1(frames)?;
            push1(frames, v1);
            push1(frames, v2);
            Ok(InsnOutcome::Next)
        }

        // ---- 运算 ----
        Insn::Bin { kind, op } => exec_bin(frames, kind, op),
        Insn::Neg(kind) => exec_neg(frames, kind),
        Insn::Conv { from, to } => {
            let value = pop_kind(frames, from)?;
            push_slot_value(frames, convert(value, from, to)?);
            Ok(InsnOutcome::Next)
        }
        Insn::TruncByte => {
            let value = pop_int(frames)?;
            push1(frames, Slot::Int((value as i8) as i32));
            Ok(InsnOutcome::Next)
        }
        Insn::TruncChar => {
            let value = pop_int(frames)?;
            push1(frames, Slot::Int((value as u16) as i32));
            Ok(InsnOutcome::Next)
        }
        Insn::TruncShort => {
            let value = pop_int(frames)?;
            push1(frames, Slot::Int((value as i16) as i32));
            Ok(InsnOutcome::Next)
        }
        Insn::Lcmp => {
            let rhs = pop_long(frames)?;
            let lhs = pop_long(frames)?;
            push1(frames, Slot::Int(match lhs.cmp(&rhs) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }));
            Ok(InsnOutcome::Next)
        }
        Insn::Fcmp { nan_is_one } => {
            let rhs = pop_float(frames)?;
            let lhs = pop_float(frames)?;
            push1(frames, Slot::Int(cmp_float(lhs, rhs, nan_is_one)));
            Ok(InsnOutcome::Next)
        }
        Insn::Dcmp { nan_is_one } => {
            let rhs = pop_double(frames)?;
            let lhs = pop_double(frames)?;
            push1(frames, Slot::Int(cmp_double(lhs, rhs, nan_is_one)));
            Ok(InsnOutcome::Next)
        }

        // ---- 分支与跳转 ----
        Insn::BranchZero { op, target } => {
            let value = pop_int(frames)?;
            if cmp_zero(op, value) {
                jump(frames, target);
                Ok(InsnOutcome::Jumped)
            } else {
                Ok(InsnOutcome::Next)
            }
        }
        Insn::BranchInt { op, target } => {
            let rhs = pop_int(frames)?;
            let lhs = pop_int(frames)?;
            if cmp_int(op, lhs, rhs) {
                jump(frames, target);
                Ok(InsnOutcome::Jumped)
            } else {
                Ok(InsnOutcome::Next)
            }
        }
        Insn::BranchRef { eq, target } => {
            let a = pop_ref(frames)?;
            let b = pop_ref(frames)?;
            if (a == b) == eq {
                jump(frames, target);
                Ok(InsnOutcome::Jumped)
            } else {
                Ok(InsnOutcome::Next)
            }
        }
        Insn::BranchNull { is_null, target } => {
            let reference = pop_ref(frames)?;
            if reference.is_null() == is_null {
                jump(frames, target);
                Ok(InsnOutcome::Jumped)
            } else {
                Ok(InsnOutcome::Next)
            }
        }
        Insn::Goto(target) => {
            jump(frames, target);
            Ok(InsnOutcome::Jumped)
        }
        Insn::Jsr(target) => {
            // 返回地址 = jsr 的下一条指令索引，以 Int 槽存储。
            let next = frames.last().expect("frame stack empty").pc + 1;
            push1(frames, Slot::Int(next as i32));
            jump(frames, target);
            Ok(InsnOutcome::Jumped)
        }
        Insn::Ret(index) => {
            let slot = *frames
                .last()
                .expect("frame stack empty")
                .locals
                .get(index as usize)
                .ok_or_else(|| bad_local(index))?;
            let Slot::Int(pc) = slot else {
                return Err(KvmError::Internal("ret on non-address local".into()));
            };
            frames.last_mut().expect("frame stack empty").pc = pc as u32;
            Ok(InsnOutcome::Jumped)
        }
        Insn::TableSwitch {
            default,
            low,
            ref targets,
        } => {
            let index = pop_int(frames)?;
            let offset = i64::from(index) - i64::from(low);
            let target = if offset >= 0 && offset < targets.len() as i64 {
                targets[offset as usize]
            } else {
                default
            };
            jump(frames, target);
            Ok(InsnOutcome::Jumped)
        }
        Insn::LookupSwitch {
            default,
            ref pairs,
        } => {
            let key = pop_int(frames)?;
            let target = pairs
                .iter()
                .find(|(match_value, _)| *match_value == key)
                .map(|(_, target)| *target)
                .unwrap_or(default);
            jump(frames, target);
            Ok(InsnOutcome::Jumped)
        }

        // ---- 返回 ----
        Insn::Return(kind) => {
            let value = match kind {
                Some(kind) => Some(pop_kind(frames, kind)?),
                None => None,
            };
            do_return(machine, frames, value)
        }

        // ---- 字段访问 ----
        Insn::GetStatic(field) => {
            // 触发初始化在弹操作数之前：压帧路径重执行时栈形不变。
            if ensure_initialized(machine, frames, field.owner)? {
                return Ok(InsnOutcome::Pushed);
            }
            let kind = field.kind;
            let statics = &machine.cs.class(field.owner).statics;
            let value = *statics
                .get(field.slot as usize)
                .ok_or_else(|| KvmError::Internal("static slot out of bounds".into()))?;
            push_kind(frames, kind, value);
            Ok(InsnOutcome::Next)
        }
        Insn::PutStatic(field) => {
            if ensure_initialized(machine, frames, field.owner)? {
                return Ok(InsnOutcome::Pushed);
            }
            let kind = field.kind;
            let value = pop_kind(frames, kind)?;
            let statics = &mut machine.cs.class_mut(field.owner).statics;
            *statics
                .get_mut(field.slot as usize)
                .ok_or_else(|| KvmError::Internal("static slot out of bounds".into()))? = value;
            Ok(InsnOutcome::Next)
        }
        Insn::GetField(field) => {
            let kind = field.kind;
            let reference = pop_ref(frames)?;
            if reference.is_null() {
                return npe();
            }
            let instance = machine.heap.instance(reference)?;
            let value = *instance
                .fields
                .get(field.slot as usize)
                .ok_or_else(|| KvmError::Internal("instance field slot out of bounds".into()))?;
            push_kind(frames, kind, value);
            Ok(InsnOutcome::Next)
        }
        Insn::PutField(field) => {
            let kind = field.kind;
            let value = pop_kind(frames, kind)?;
            let reference = pop_ref(frames)?;
            if reference.is_null() {
                return npe();
            }
            let instance = machine.heap.instance_mut(reference)?;
            *instance
                .fields
                .get_mut(field.slot as usize)
                .ok_or_else(|| KvmError::Internal("instance field slot out of bounds".into()))? =
                value;
            Ok(InsnOutcome::Next)
        }

        // ---- 调用 ----
        Insn::InvokeVirtual(mref) => {
            exec_invoke(machine, frames, thread, InvokeMode::Virtual, mref)
        }
        Insn::InvokeSpecial(mref) => {
            exec_invoke(machine, frames, thread, InvokeMode::Special, mref)
        }
        Insn::InvokeStatic(mref) => {
            exec_invoke(machine, frames, thread, InvokeMode::Static, mref)
        }
        Insn::InvokeInterface(mref) => {
            exec_invoke(machine, frames, thread, InvokeMode::Interface, mref)
        }

        // ---- 对象与类型 ----
        Insn::New(class) => {
            // new 触发类初始化（JLS 12.4.1）；clinit 压帧后重执行本指令。
            if ensure_initialized(machine, frames, class)? {
                return Ok(InsnOutcome::Pushed);
            }
            let (illegal, template) = {
                let vm = machine.cs.class(class);
                (
                    vm.is_interface() || vm.access_flags & acc::ABSTRACT != 0,
                    vm.instance_template.clone(),
                )
            };
            if illegal {
                return Err(KvmError::Linkage(format!(
                    "cannot instantiate abstract/interface class {}",
                    machine.cs.name_of(class)
                )));
            }
            let reference =
                alloc_gc(machine, frames, &[], |heap| {
                    heap.alloc_instance(class, template.clone())
                })?;
            push1(frames, Slot::Ref(reference));
            Ok(InsnOutcome::Next)
        }
        Insn::CheckCast(class) => {
            let reference = pop_ref(frames)?;
            if reference.is_null() {
                push1(frames, Slot::Ref(reference));
                return Ok(InsnOutcome::Next);
            }
            let actual = runtime_class_of(machine.cs, machine.heap, reference)?;
            if is_assignable(machine.cs, actual, class) {
                push1(frames, Slot::Ref(reference));
                Ok(InsnOutcome::Next)
            } else {
                Ok(InsnOutcome::Unwind(Unwind::Fault(Fault::ClassCast)))
            }
        }
        Insn::InstanceOf(class) => {
            let reference = pop_ref(frames)?;
            let result = if reference.is_null() {
                0
            } else {
                let actual = runtime_class_of(machine.cs, machine.heap, reference)?;
                i32::from(is_assignable(machine.cs, actual, class))
            };
            push1(frames, Slot::Int(result));
            Ok(InsnOutcome::Next)
        }
        Insn::Athrow => {
            let reference = pop_ref(frames)?;
            if reference.is_null() {
                // athrow null 本身按 NPE 处理（JVM 规范）。
                return npe();
            }
            Ok(InsnOutcome::Unwind(Unwind::Thrown(reference)))
        }

        // ---- monitor ----
        Insn::MonitorEnter => {
            let reference = pop_ref(frames)?;
            if reference.is_null() {
                return npe();
            }
            match machine.monitors.enter(reference, thread) {
                EnterOutcome::Acquired(_) => Ok(InsnOutcome::Next),
                EnterOutcome::Contended(_) => {
                    // 竞争失败：恢复操作数、pc 不动，调度器稍后重试。
                    push1(frames, Slot::Ref(reference));
                    Ok(InsnOutcome::Blocked(reference))
                }
            }
        }
        Insn::MonitorExit => {
            let reference = pop_ref(frames)?;
            if reference.is_null() {
                return npe();
            }
            match machine.monitors.exit(reference, thread) {
                Ok(()) => Ok(InsnOutcome::Next),
                Err(_) => Ok(InsnOutcome::Unwind(Unwind::Fault(Fault::IllegalMonitor))),
            }
        }

        // ---- superopcode（复合指令） --------------------------------------
        Insn::Fused { ref a, ref b } => {
            // 复合槽索引（run 循环在 execute 返回后才推进 pc；a 的 Next
            // 路径不改 pc，此处读取与执行后读取等价）。
            let entry_pc = frames.last().expect("frame stack empty").pc;
            // 高频对的专用执行体（真实 jar 融合对频率选型，见
            // tests/pair_stats.rs）：单次分派内联两半，省一次取指与
            // 一次内层分派；语义与逐条执行严格等价（null/越界/除零
            // 防御同路径）。未命中模式走通用递归执行。
            match (&**a, &**b) {
                // this.x：aload + getfield（融合对频率第一）。
                (Insn::Load { kind: Kind::Ref, index }, Insn::GetField(field)) => {
                    let reference = match *frames
                        .last()
                        .expect("frame stack empty")
                        .locals
                        .get(*index as usize)
                        .ok_or_else(|| bad_local(*index))?
                    {
                        Slot::Ref(reference) => reference,
                        slot => {
                            return Err(KvmError::Internal(format!(
                                "expected reference local, got {slot:?}"
                            )))
                        }
                    };
                    if reference.is_null() {
                        return npe();
                    }
                    let value = *machine
                        .heap
                        .instance(reference)?
                        .fields
                        .get(field.slot as usize)
                        .ok_or_else(|| {
                            KvmError::Internal("instance field slot out of bounds".into())
                        })?;
                    let frame = frames.last_mut().expect("frame stack empty");
                    frame.stack.push(value);
                    if matches!(field.kind, Kind::Long | Kind::Double) {
                        frame.stack.push(Slot::Pad);
                    }
                    frame.pc = entry_pc + 2;
                    Ok(InsnOutcome::Jumped)
                }
                // 双 cat1 局部读取（a*b、this.x + this.y 等的前缀）。
                (
                    Insn::Load { kind: ka, index: ia },
                    Insn::Load { kind: kb, index: ib },
                ) if !matches!(*ka, Kind::Long | Kind::Double) && !matches!(*kb, Kind::Long | Kind::Double) => {
                    let frame = frames.last_mut().expect("frame stack empty");
                    let sa = *frame
                        .locals
                        .get(*ia as usize)
                        .ok_or_else(|| bad_local(*ia))?;
                    let sb = *frame
                        .locals
                        .get(*ib as usize)
                        .ok_or_else(|| bad_local(*ib))?;
                    frame.stack.push(sa);
                    frame.stack.push(sb);
                    frame.pc = entry_pc + 2;
                    Ok(InsnOutcome::Jumped)
                }
                // 局部 + 常量（x + 5、arr[i] 前缀等）。
                (
                    Insn::Load { kind: Kind::Int | Kind::Ref | Kind::Float, index },
                    Insn::PushInt(value),
                ) => {
                    let frame = frames.last_mut().expect("frame stack empty");
                    let slot = *frame
                        .locals
                        .get(*index as usize)
                        .ok_or_else(|| bad_local(*index))?;
                    frame.stack.push(slot);
                    frame.stack.push(Slot::Int(*value));
                    frame.pc = entry_pc + 2;
                    Ok(InsnOutcome::Jumped)
                }
                // 双常量。
                (Insn::PushInt(v), Insn::PushInt(w)) => {
                    let frame = frames.last_mut().expect("frame stack empty");
                    frame.stack.push(Slot::Int(*v));
                    frame.stack.push(Slot::Int(*w));
                    frame.pc = entry_pc + 2;
                    Ok(InsnOutcome::Jumped)
                }
                // 常量二元运算（x OP const：常量是弹栈序的 rhs）。
                (Insn::PushInt(rhs), Insn::Bin { kind: Kind::Int, op }) => {
                    let lhs = pop_int(frames)?;
                    let result = match int_bin(*op, lhs, *rhs) {
                        Ok(value) => value,
                        Err(unwind) => return Ok(InsnOutcome::Unwind(unwind)),
                    };
                    push1(frames, Slot::Int(result));
                    frames.last_mut().expect("frame stack empty").pc = entry_pc + 2;
                    Ok(InsnOutcome::Jumped)
                }
                // dup + 常量（new/dup/invokespecial 链外的字段写形）。
                (Insn::Dup, Insn::PushInt(value)) => {
                    let frame = frames.last_mut().expect("frame stack empty");
                    let Some(top) = frame.stack.last().copied() else {
                        return Err(KvmError::Internal("operand stack underflow".into()));
                    };
                    frame.stack.push(top);
                    frame.stack.push(Slot::Int(*value));
                    frame.pc = entry_pc + 2;
                    Ok(InsnOutcome::Jumped)
                }
                // obj.f 读取后接局部读取。
                (
                    Insn::GetField(field),
                    Insn::Load { kind: Kind::Int | Kind::Ref | Kind::Float, index },
                ) => {
                    let reference = pop_ref(frames)?;
                    if reference.is_null() {
                        return npe();
                    }
                    let value = *machine
                        .heap
                        .instance(reference)?
                        .fields
                        .get(field.slot as usize)
                        .ok_or_else(|| {
                            KvmError::Internal("instance field slot out of bounds".into())
                        })?;
                    let frame = frames.last_mut().expect("frame stack empty");
                    frame.stack.push(value);
                    if matches!(field.kind, Kind::Long | Kind::Double) {
                        frame.stack.push(Slot::Pad);
                    }
                    let slot = *frame
                        .locals
                        .get(*index as usize)
                        .ok_or_else(|| bad_local(*index))?;
                    frame.stack.push(slot);
                    frame.pc = entry_pc + 2;
                    Ok(InsnOutcome::Jumped)
                }
                // obj.f = v 写入后接局部读取。
                (
                    Insn::PutField(field),
                    Insn::Load { kind: Kind::Int | Kind::Ref | Kind::Float, index },
                ) => {
                    let value = pop_kind(frames, field.kind)?;
                    let reference = pop_ref(frames)?;
                    if reference.is_null() {
                        return npe();
                    }
                    *machine
                        .heap
                        .instance_mut(reference)?
                        .fields
                        .get_mut(field.slot as usize)
                        .ok_or_else(|| {
                            KvmError::Internal("instance field slot out of bounds".into())
                        })? = value;
                    let frame = frames.last_mut().expect("frame stack empty");
                    let slot = *frame
                        .locals
                        .get(*index as usize)
                        .ok_or_else(|| bad_local(*index))?;
                    frame.stack.push(slot);
                    frame.pc = entry_pc + 2;
                    Ok(InsnOutcome::Jumped)
                }
                // obj.f = const（常量不经操作数栈往返）。
                (Insn::PushInt(value), Insn::PutField(field)) if field.kind == Kind::Int => {
                    let reference = pop_ref(frames)?;
                    if reference.is_null() {
                        return npe();
                    }
                    *machine
                        .heap
                        .instance_mut(reference)?
                        .fields
                        .get_mut(field.slot as usize)
                        .ok_or_else(|| {
                            KvmError::Internal("instance field slot out of bounds".into())
                        })? = Slot::Int(*value);
                    frames.last_mut().expect("frame stack empty").pc = entry_pc + 2;
                    Ok(InsnOutcome::Jumped)
                }
                // arr[i] = const（常量不经操作数栈往返）。
                (
                    Insn::PushInt(value),
                    Insn::ArrayStore(ArrOp::Int | ArrOp::Byte | ArrOp::Char | ArrOp::Short),
                ) => {
                    let index = pop_int(frames)?;
                    let reference = pop_ref(frames)?;
                    if reference.is_null() {
                        return npe();
                    }
                    let array = machine.heap.array_mut(reference)?;
                    let len = array.data.len();
                    if index < 0 || index as usize >= len {
                        return Ok(InsnOutcome::Unwind(Unwind::Fault(Fault::ArrayIndex {
                            index,
                        })));
                    }
                    array.data.set(index as usize, Slot::Int(*value))?;
                    frames.last_mut().expect("frame stack empty").pc = entry_pc + 2;
                    Ok(InsnOutcome::Jumped)
                }
                // x = const（局部直写，不经操作数栈往返）。
                (Insn::PushInt(value), Insn::Store { kind: Kind::Int, index }) => {
                    let frame = frames.last_mut().expect("frame stack empty");
                    if *index as usize >= frame.locals.len() {
                        return Err(bad_local(*index));
                    }
                    frame.locals[*index as usize] = Slot::Int(*value);
                    frame.pc = entry_pc + 2;
                    Ok(InsnOutcome::Jumped)
                }
                // if (x == 0) 局部直读（不经操作数栈往返）。
                (Insn::Load { kind: Kind::Int, index }, Insn::BranchZero { op, target }) => {
                    let slot = *frames
                        .last()
                        .expect("frame stack empty")
                        .locals
                        .get(*index as usize)
                        .ok_or_else(|| bad_local(*index))?;
                    let Slot::Int(value) = slot else {
                        return Err(KvmError::Internal("expected int local".into()));
                    };
                    if cmp_zero(*op, value) {
                        jump(frames, *target);
                    } else {
                        frames.last_mut().expect("frame stack empty").pc = entry_pc + 2;
                    }
                    Ok(InsnOutcome::Jumped)
                }
                // 分支未中时后继局部读取（branch + aload 高频形）。
                (
                    Insn::BranchZero { op, target },
                    Insn::Load { kind: Kind::Int | Kind::Ref | Kind::Float, index },
                ) => {
                    let value = pop_int(frames)?;
                    if cmp_zero(*op, value) {
                        jump(frames, *target);
                    } else {
                        let frame = frames.last_mut().expect("frame stack empty");
                        let slot = *frame
                            .locals
                            .get(*index as usize)
                            .ok_or_else(|| bad_local(*index))?;
                        frame.stack.push(slot);
                        frame.pc = entry_pc + 2;
                    }
                    Ok(InsnOutcome::Jumped)
                }
                (
                    Insn::BranchInt { op, target },
                    Insn::Load { kind: Kind::Int | Kind::Ref | Kind::Float, index },
                ) => {
                    let rhs = pop_int(frames)?;
                    let lhs = pop_int(frames)?;
                    if cmp_int(*op, lhs, rhs) {
                        jump(frames, *target);
                    } else {
                        let frame = frames.last_mut().expect("frame stack empty");
                        let slot = *frame
                            .locals
                            .get(*index as usize)
                            .ok_or_else(|| bad_local(*index))?;
                        frame.stack.push(slot);
                        frame.pc = entry_pc + 2;
                    }
                    Ok(InsnOutcome::Jumped)
                }
                // 通用路径：两半各自完整分派执行。
                _ => match execute_insn(machine, frames, thread, a)? {
                    InsnOutcome::Next => match execute_insn(machine, frames, thread, b)? {
                        // b 正常完成：直接落到复合后继（i+2），省一次
                        // 蹦床分派；其余结果（Jumped 等）的 pc 语义已由
                        // b 自身就位。
                        InsnOutcome::Next => {
                            frames.last_mut().expect("frame stack empty").pc = entry_pc + 2;
                            Ok(InsnOutcome::Jumped)
                        }
                        other => Ok(other),
                    },
                    // a 白名单保证 Next 之外的结果（Pushed/Blocked/Unwind/
                    // Jumped）都是"从复合头重入仍然正确"的路径。
                    other => Ok(other),
                },
            }
        }
    }
}

// ---- 小工具 ---------------------------------------------------------------

fn npe() -> Result<InsnOutcome, KvmError> {
    Ok(InsnOutcome::Unwind(Unwind::Fault(Fault::NullPointer)))
}

fn bad_local(index: u16) -> KvmError {
    KvmError::Internal(format!("local index {index} out of bounds"))
}

/// 跳转（指令索引）。
fn jump(frames: &mut [Frame], target: u32) {
    frames.last_mut().expect("frame stack empty").pc = target;
}

fn pop_long(frames: &mut [Frame]) -> Result<i64, KvmError> {
    match pop_kind(frames, Kind::Long)? {
        Slot::Long(value) => Ok(value),
        slot => Err(KvmError::Internal(format!("expected long, got {slot:?}"))),
    }
}

fn pop_float(frames: &mut [Frame]) -> Result<f32, KvmError> {
    match pop_kind(frames, Kind::Float)? {
        Slot::Float(value) => Ok(value),
        slot => Err(KvmError::Internal(format!("expected float, got {slot:?}"))),
    }
}

fn pop_double(frames: &mut [Frame]) -> Result<f64, KvmError> {
    match pop_kind(frames, Kind::Double)? {
        Slot::Double(value) => Ok(value),
        slot => Err(KvmError::Internal(format!("expected double, got {slot:?}"))),
    }
}

fn as_int(slot: Slot) -> Result<i32, KvmError> {
    match slot {
        Slot::Int(value) => Ok(value),
        slot => Err(KvmError::Internal(format!("expected int, got {slot:?}"))),
    }
}

fn as_long(slot: Slot) -> Result<i64, KvmError> {
    match slot {
        Slot::Long(value) => Ok(value),
        slot => Err(KvmError::Internal(format!("expected long, got {slot:?}"))),
    }
}

fn as_float(slot: Slot) -> Result<f32, KvmError> {
    match slot {
        Slot::Float(value) => Ok(value),
        slot => Err(KvmError::Internal(format!("expected float, got {slot:?}"))),
    }
}

fn as_double(slot: Slot) -> Result<f64, KvmError> {
    match slot {
        Slot::Double(value) => Ok(value),
        slot => Err(KvmError::Internal(format!("expected double, got {slot:?}"))),
    }
}

fn cmp_zero(op: CmpOp, value: i32) -> bool {
    match op {
        CmpOp::Eq => value == 0,
        CmpOp::Ne => value != 0,
        CmpOp::Lt => value < 0,
        CmpOp::Ge => value >= 0,
        CmpOp::Gt => value > 0,
        CmpOp::Le => value <= 0,
    }
}

fn cmp_int(op: CmpOp, lhs: i32, rhs: i32) -> bool {
    match op {
        CmpOp::Eq => lhs == rhs,
        CmpOp::Ne => lhs != rhs,
        CmpOp::Lt => lhs < rhs,
        CmpOp::Ge => lhs >= rhs,
        CmpOp::Gt => lhs > rhs,
        CmpOp::Le => lhs <= rhs,
    }
}

fn cmp_float(lhs: f32, rhs: f32, nan_is_one: bool) -> i32 {
    if lhs.is_nan() || rhs.is_nan() {
        if nan_is_one { 1 } else { -1 }
    } else if lhs < rhs {
        -1
    } else if lhs > rhs {
        1
    } else {
        0
    }
}

fn cmp_double(lhs: f64, rhs: f64, nan_is_one: bool) -> i32 {
    if lhs.is_nan() || rhs.is_nan() {
        if nan_is_one { 1 } else { -1 }
    } else if lhs < rhs {
        -1
    } else if lhs > rhs {
        1
    } else {
        0
    }
}

// ---- 常量加载 -------------------------------------------------------------

fn exec_ldc(
    machine: &mut Machine,
    frames: &mut [Frame],
    constant: &LdcConst,
) -> Result<InsnOutcome, KvmError> {
    match *constant {
        LdcConst::Int(value) => push1(frames, Slot::Int(value)),
        LdcConst::Float(value) => push1(frames, Slot::Float(value)),
        LdcConst::Long(value) => push_slot_value(frames, Slot::Long(value)),
        LdcConst::Double(value) => push_slot_value(frames, Slot::Double(value)),
        LdcConst::Str(ref text) => {
            // 驻留成 String 对象（同内容幂等）。
            let chars: Vec<u16> = text.encode_utf16().collect();
            let reference = alloc_gc(machine, frames, &[], |heap| {
                heap.alloc_string(chars.clone())
            })?;
            push1(frames, Slot::Ref(reference));
        }
        LdcConst::Class(_) => {
            return Err(KvmError::Internal(
                "ldc of class constants requires T010 bootstrap".into(),
            ));
        }
    }
    Ok(InsnOutcome::Next)
}

// ---- 数组 -----------------------------------------------------------------

fn array_load(
    machine: &mut Machine,
    frames: &mut [Frame],
    _op: ArrOp,
) -> Result<InsnOutcome, KvmError> {
    let index = pop_int(frames)?;
    let reference = pop_ref(frames)?;
    if reference.is_null() {
        return npe();
    }
    let array = machine.heap.array(reference)?;
    let len = array.data.len();
    if index < 0 || index as usize >= len {
        // 调试面：SKYENGINE_KVM_TRACE_ARRAY=1 时打印越界现场（数组长度 +
        // 栈顶快照——脚本解释器游标错位取证）。
        if crate::interp::trace_flag("SKYENGINE_KVM_TRACE_ARRAY") {
            eprintln!("[kvm-arr] OOB index={index} len={len}");
        }
        return Ok(InsnOutcome::Unwind(Unwind::Fault(Fault::ArrayIndex {
            index,
        })));
    }
    let value = array.data.get(index as usize);
    push_slot_value(frames, value);
    Ok(InsnOutcome::Next)
}

fn array_store(
    machine: &mut Machine,
    frames: &mut [Frame],
    op: ArrOp,
) -> Result<InsnOutcome, KvmError> {
    // cat2 元素先弹 Pad（kind 仅决定弹出槽数）。
    let kind = match op {
        ArrOp::Long => Kind::Long,
        ArrOp::Double => Kind::Double,
        _ => Kind::Int,
    };
    let value = pop_kind(frames, kind)?;
    let index = pop_int(frames)?;
    let reference = pop_ref(frames)?;
    if reference.is_null() {
        return npe();
    }
    let array = machine.heap.array_mut(reference)?;
    let len = array.data.len();
    if index < 0 || index as usize >= len {
        return Ok(InsnOutcome::Unwind(Unwind::Fault(Fault::ArrayIndex {
            index,
        })));
    }
    array.data.set(index as usize, value)?;
    Ok(InsnOutcome::Next)
}

fn exec_new_array(
    machine: &mut Machine,
    frames: &mut [Frame],
    atype: u8,
) -> Result<InsnOutcome, KvmError> {
    let count = pop_int(frames)?;
    if count < 0 {
        return Ok(InsnOutcome::Unwind(Unwind::Fault(
            Fault::NegativeArraySize { size: count },
        )));
    }
    let elem = ElemType::from_atype(atype)
        .ok_or_else(|| KvmError::Internal(format!("bad newarray atype {atype}")))?;
    let desc_char = elem_desc_char(elem)?;
    let array_class = machine.cs.load_class(&format!("[{desc_char}"))?;
    let reference = alloc_gc(machine, frames, &[], |heap| {
        heap.alloc_array(array_class, elem, count as usize)
    })?;
    push1(frames, Slot::Ref(reference));
    Ok(InsnOutcome::Next)
}

fn elem_desc_char(elem: ElemType) -> Result<char, KvmError> {
    Ok(match elem {
        ElemType::Boolean => 'Z',
        ElemType::Byte => 'B',
        ElemType::Char => 'C',
        ElemType::Short => 'S',
        ElemType::Int => 'I',
        ElemType::Long => 'J',
        ElemType::Float => 'F',
        ElemType::Double => 'D',
        ElemType::Ref => return Err(KvmError::Internal("ref element has no atype".into())),
    })
}

fn exec_anew_array(
    machine: &mut Machine,
    frames: &mut [Frame],
    component: ClassId,
) -> Result<InsnOutcome, KvmError> {
    let count = pop_int(frames)?;
    if count < 0 {
        return Ok(InsnOutcome::Unwind(Unwind::Fault(
            Fault::NegativeArraySize { size: count },
        )));
    }
    let component_name = machine.cs.name_of(component);
    let desc = if component_name.starts_with('[') {
        format!("[{component_name}")
    } else {
        format!("[L{component_name};")
    };
    let array_class = machine.cs.load_class(&desc)?;
    let elem = match machine.cs.class(array_class).array_elem {
        Some(ArrayElem::Primitive(primitive)) => primitive,
        Some(ArrayElem::Class(_)) => ElemType::Ref,
        None => return Err(KvmError::Internal("anewarray resolved to non-array".into())),
    };
    let reference = alloc_gc(machine, frames, &[], |heap| {
        heap.alloc_array(array_class, elem, count as usize)
    })?;
    push1(frames, Slot::Ref(reference));
    Ok(InsnOutcome::Next)
}

fn exec_multi_new_array(
    machine: &mut Machine,
    frames: &mut Vec<Frame>,
    class: ClassId,
    dims: u8,
) -> Result<InsnOutcome, KvmError> {
    let mut counts = Vec::with_capacity(dims as usize);
    for _ in 0..dims {
        counts.push(pop_int(frames)?);
    }
    counts.reverse();
    if let Some(&size) = counts.iter().find(|count| **count < 0) {
        return Ok(InsnOutcome::Unwind(Unwind::Fault(
            Fault::NegativeArraySize { size },
        )));
    }
    // held：已分配的父层引用（GC 附加根）。
    let mut held: Vec<ObjRef> = Vec::new();
    let reference = alloc_multi(machine, frames, &mut held, class, &counts)?;
    push1(frames, Slot::Ref(reference));
    Ok(InsnOutcome::Next)
}

/// 多维数组递归分配：`class` 为本层数组类，`counts` 为剩余维度
/// （counts[0] = 本层长度）。父层压入 `held` 充当子层分配的 GC 根。
fn alloc_multi(
    machine: &mut Machine,
    frames: &mut Vec<Frame>,
    held: &mut Vec<ObjRef>,
    class: ClassId,
    counts: &[i32],
) -> Result<ObjRef, KvmError> {
    let len = counts[0] as usize;
    let elem = match machine.cs.class(class).array_elem {
        Some(ArrayElem::Primitive(primitive)) => primitive,
        Some(ArrayElem::Class(_)) => ElemType::Ref,
        None => return Err(KvmError::Internal("multianewarray on non-array".into())),
    };
    if counts.len() == 1 {
        return alloc_gc(machine, frames, held, |heap| {
            heap.alloc_array(class, elem, len)
        });
    }
    let Some(ArrayElem::Class(component)) = machine.cs.class(class).array_elem else {
        return Err(KvmError::Internal("multianewarray component mismatch".into()));
    };
    let reference = alloc_gc(machine, frames, held, |heap| {
        heap.alloc_array(class, ElemType::Ref, len)
    })?;
    held.push(reference);
    for index in 0..len {
        let child = alloc_multi(machine, frames, held, component, &counts[1..])?;
        machine
            .heap
            .array_mut(reference)?
            .data
            .set(index, Slot::Ref(child))?;
    }
    held.pop();
    Ok(reference)
}

// ---- 运算 -----------------------------------------------------------------

fn exec_bin(
    frames: &mut [Frame],
    kind: Kind,
    op: BinOp,
) -> Result<InsnOutcome, KvmError> {
    match kind {
        Kind::Int => {
            let rhs = pop_int(frames)?;
            let lhs = pop_int(frames)?;
            let result = match int_bin(op, lhs, rhs) {
                Ok(value) => value,
                Err(unwind) => return Ok(InsnOutcome::Unwind(unwind)),
            };
            push1(frames, Slot::Int(result));
        }
        Kind::Long => {
            // JVM 语义：lshl/lshr/lushr 的操作数是 (value1: long,
            // value2: int)——shift 量单槽；其余二元运算才是 (long,
            // long)。按位长移序弹（w.a CRC32 初始化的 lload/iconst_1/
            // lshr 实证：错按双 long 弹会拿 int 当 Pad 位炸栈）。
            let (lhs, rhs) = if matches!(op, BinOp::Shl | BinOp::Shr | BinOp::Ushr) {
                let shift = pop_int(frames)?;
                let value = pop_long(frames)?;
                (value, i64::from(shift))
            } else {
                let rhs = pop_long(frames)?;
                let lhs = pop_long(frames)?;
                (lhs, rhs)
            };
            let result = match op {
                BinOp::Add => lhs.wrapping_add(rhs),
                BinOp::Sub => lhs.wrapping_sub(rhs),
                BinOp::Mul => lhs.wrapping_mul(rhs),
                BinOp::Div => {
                    if rhs == 0 {
                        return arithmetic();
                    }
                    lhs.wrapping_div(rhs)
                }
                BinOp::Rem => {
                    if rhs == 0 {
                        return arithmetic();
                    }
                    lhs.wrapping_rem(rhs)
                }
                BinOp::Shl => lhs.wrapping_shl(rhs as u32),
                BinOp::Shr => lhs.wrapping_shr(rhs as u32),
                BinOp::Ushr => (lhs as u64).wrapping_shr(rhs as u32) as i64,
                BinOp::And => lhs & rhs,
                BinOp::Or => lhs | rhs,
                BinOp::Xor => lhs ^ rhs,
            };
            push_slot_value(frames, Slot::Long(result));
        }
        Kind::Float => {
            let rhs = pop_float(frames)?;
            let lhs = pop_float(frames)?;
            let result = match op {
                BinOp::Add => lhs + rhs,
                BinOp::Sub => lhs - rhs,
                BinOp::Mul => lhs * rhs,
                BinOp::Div => lhs / rhs,
                // Rust % 即 fmod，与 JVM frem 一致（符号随被除数）。
                BinOp::Rem => lhs % rhs,
                _ => return Err(KvmError::Internal("shift/logic op on float".into())),
            };
            push1(frames, Slot::Float(result));
        }
        Kind::Double => {
            let rhs = pop_double(frames)?;
            let lhs = pop_double(frames)?;
            let result = match op {
                BinOp::Add => lhs + rhs,
                BinOp::Sub => lhs - rhs,
                BinOp::Mul => lhs * rhs,
                BinOp::Div => lhs / rhs,
                BinOp::Rem => lhs % rhs,
                _ => return Err(KvmError::Internal("shift/logic op on double".into())),
            };
            push_slot_value(frames, Slot::Double(result));
        }
        Kind::Ref => return Err(KvmError::Internal("bin op on reference".into())),
    }
    Ok(InsnOutcome::Next)
}

/// 整型二元运算核心（`exec_bin` 与复合快路径共用）；除零返回
/// `Unwind::Fault(Arithmetic)`（Java 异常路径，非解释器错误）。
fn int_bin(op: BinOp, lhs: i32, rhs: i32) -> Result<i32, Unwind> {
    Ok(match op {
        BinOp::Add => lhs.wrapping_add(rhs),
        BinOp::Sub => lhs.wrapping_sub(rhs),
        BinOp::Mul => lhs.wrapping_mul(rhs),
        BinOp::Div => {
            if rhs == 0 {
                return Err(Unwind::Fault(Fault::Arithmetic));
            }
            // MIN / -1 按 JVM 回绕为 MIN，不 panic。
            lhs.wrapping_div(rhs)
        }
        BinOp::Rem => {
            if rhs == 0 {
                return Err(Unwind::Fault(Fault::Arithmetic));
            }
            lhs.wrapping_rem(rhs)
        }
        BinOp::Shl => lhs.wrapping_shl(rhs as u32),
        BinOp::Shr => lhs.wrapping_shr(rhs as u32),
        BinOp::Ushr => (lhs as u32).wrapping_shr(rhs as u32) as i32,
        BinOp::And => lhs & rhs,
        BinOp::Or => lhs | rhs,
        BinOp::Xor => lhs ^ rhs,
    })
}

fn arithmetic() -> Result<InsnOutcome, KvmError> {
    Ok(InsnOutcome::Unwind(Unwind::Fault(Fault::Arithmetic)))
}

fn exec_neg(frames: &mut [Frame], kind: Kind) -> Result<InsnOutcome, KvmError> {
    match kind {
        Kind::Int => {
            let value = pop_int(frames)?;
            push1(frames, Slot::Int(value.wrapping_neg()));
        }
        Kind::Long => {
            let value = pop_long(frames)?;
            push_slot_value(frames, Slot::Long(value.wrapping_neg()));
        }
        Kind::Float => {
            let value = pop_float(frames)?;
            push1(frames, Slot::Float(-value));
        }
        Kind::Double => {
            let value = pop_double(frames)?;
            push_slot_value(frames, Slot::Double(-value));
        }
        Kind::Ref => return Err(KvmError::Internal("neg on reference".into())),
    }
    Ok(InsnOutcome::Next)
}

/// 数值转换：Rust `as` 的浮转整语义（NaN→0、向零取整、饱和）与 JVM 一致。
fn convert(value: Slot, from: Kind, to: Kind) -> Result<Slot, KvmError> {
    Ok(match (from, to) {
        (Kind::Int, Kind::Long) => Slot::Long(i64::from(as_int(value)?)),
        (Kind::Int, Kind::Float) => Slot::Float(as_int(value)? as f32),
        (Kind::Int, Kind::Double) => Slot::Double(as_int(value)? as f64),
        (Kind::Long, Kind::Int) => Slot::Int(as_long(value)? as i32),
        (Kind::Long, Kind::Float) => Slot::Float(as_long(value)? as f32),
        (Kind::Long, Kind::Double) => Slot::Double(as_long(value)? as f64),
        (Kind::Float, Kind::Int) => Slot::Int(as_float(value)? as i32),
        (Kind::Float, Kind::Long) => Slot::Long(as_float(value)? as i64),
        (Kind::Float, Kind::Double) => Slot::Double(f64::from(as_float(value)?)),
        (Kind::Double, Kind::Int) => Slot::Int(as_double(value)? as i32),
        (Kind::Double, Kind::Long) => Slot::Long(as_double(value)? as i64),
        (Kind::Double, Kind::Float) => Slot::Float(as_double(value)? as f32),
        _ => return Err(KvmError::Internal(format!("invalid conversion {from:?}->{to:?}"))),
    })
}

// ---- 返回 -----------------------------------------------------------------

/// 弹出当前帧：
/// - Method 帧有调用者 → 回填返回值（caller pc 已指向 invoke 之后）；
///   无调用者（根帧）→ Returned；
/// - Clinit 帧 → 本类标 Initialized，推进余链：压了下一帧 → Pushed，
///   链完成 → Jumped（触发帧重执行触发指令）。
pub(super) fn do_return(
    machine: &mut Machine,
    frames: &mut Vec<Frame>,
    value: Option<Slot>,
) -> Result<InsnOutcome, KvmError> {
    let popped = frames.pop().expect("frame stack empty");
    match popped.kind {
        FrameKind::Method => {
            if frames.is_empty() {
                Ok(InsnOutcome::Returned(value))
            } else {
                if let Some(value) = value {
                    push_slot_value(frames, value);
                }
                Ok(InsnOutcome::Jumped)
            }
        }
        FrameKind::Clinit { class, rest } => {
            machine.cs.class_mut(class).init_state = InitState::Initialized;
            if advance_clinit_chain(machine, frames, &rest)? {
                Ok(InsnOutcome::Pushed)
            } else {
                Ok(InsnOutcome::Jumped)
            }
        }
    }
}

// ---- 调用 -----------------------------------------------------------------

pub(crate) enum InvokeMode {
    Virtual,
    Special,
    Static,
    Interface,
}

pub(crate) fn exec_invoke(
    machine: &mut Machine,
    frames: &mut Vec<Frame>,
    thread: ThreadId,
    mode: InvokeMode,
    mref: MethodRef,
) -> Result<InsnOutcome, KvmError> {
    // 静态调用触发类初始化：在弹操作数之前，压帧重执行时栈形不变。
    if matches!(mode, InvokeMode::Static) && ensure_initialized(machine, frames, mref.class)? {
        return Ok(InsnOutcome::Pushed);
    }
    // 描述符类别走 Sym 缓存（免逐次 String 分配与解析）。
    let arg_kinds = machine.cs.descriptor_kinds(mref.desc)?;
    // 逆序弹参（cat2 连 Pad 一起弹出），再正序成表。
    let mut args: Vec<Slot> = Vec::with_capacity(arg_kinds.0.len());
    for kind in arg_kinds.0.iter().rev() {
        args.push(pop_kind(frames, *kind)?);
    }
    args.reverse();
    // 接收者（非 static）：null 即 NPE。
    let this = if matches!(mode, InvokeMode::Static) {
        None
    } else {
        let reference = pop_ref(frames)?;
        if reference.is_null() {
            return npe();
        }
        Some(reference)
    };
    // 目标解析：虚/接口按接收者运行时类；特殊/静态按引用类
    // （链接期已解析到定义类）。
    let lookup_class = match mode {
        InvokeMode::Virtual | InvokeMode::Interface => {
            runtime_class_of(machine.cs, machine.heap, this.expect("checked above"))?
        }
        InvokeMode::Special | InvokeMode::Static => mref.class,
    };
    // 调试面：SKYENGINE_KVM_TRACE_INVOKE=<子串> 打印匹配的调用解析
    //（方法名或类名含子串），显示实际接收者类与定义类——多态死点定位。
    if let Some(filter) = super::trace_filter("SKYENGINE_KVM_TRACE_INVOKE") {
        let method_text = machine.cs.syms.name(mref.name);
        let class_text = machine.cs.name_of(lookup_class);
        let joined = format!("{class_text}.{method_text}");
        if joined.contains(filter)
            || class_text.contains(filter)
            || method_text.contains(filter)
            || machine.cs.syms.name(mref.desc).contains(filter)
        {
            eprintln!(
                "[kvm-invoke] {joined}{:?} def={}",
                machine.cs.syms.name(mref.desc),
                machine.cs.name_of(mref.class)
            );
        }
    }
    let Some((defining, index)) = machine.cs.find_method(lookup_class, mref.name, mref.desc)
    else {
        return Err(KvmError::Linkage(format!(
            "method {}{} not found on {}",
            machine.cs.syms.name(mref.name),
            machine.cs.syms.name(mref.desc),
            machine.cs.name_of(lookup_class)
        )));
    };
    // 方法级调用计数（SKYENGINE_KVM_PROFILE=1 或曾按过 F8 剖析键时
    // 累加；JIT 热点选型与分层编译触发共用数据面）。
    if super::trace_flag("SKYENGINE_KVM_PROFILE")
        || crate::debug_switch::profile_ever_requested()
    {
        *machine
            .cs
            .invoke_profile
            .entry((defining, index))
            .or_insert(0) += 1;
    }
    let (access_flags, body) = {
        let method = &machine.cs.class(defining).methods[index as usize];
        (method.access_flags, method.body.clone())
    };
    // JIT 分层触发（x86_64，SKYENGINE_KVM_JIT=1）：调用计数达阈值且
    // 可编译（叶子整数子集）→ 编译入缓存；后续方法入口由 run 循环
    // 接管执行。
    #[cfg(target_arch = "x86_64")]
    if crate::jit::jit_enabled() {
        if let Some(code) = crate::jit::maybe_compile(machine.cs, defining, index, &body) {
            machine.cs.jit_store(defining, index, code);
        }
    }
    if access_flags & acc::ABSTRACT != 0 {
        return Err(KvmError::Linkage(format!(
            "abstract method {}{} invoked",
            machine.cs.syms.name(mref.name),
            machine.cs.syms.name(mref.desc)
        )));
    }
    // 推进调用帧 pc（字节码/原生路径一致）。
    let call_pc = frames.last().expect("frame stack empty").pc;
    frames.last_mut().expect("frame stack empty").pc += 1;
    match body {
        Some(body) => {
            push_method_frame(
                machine, frames, defining, index, body, this, args, Some(call_pc),
            )?;
            Ok(InsnOutcome::Pushed)
        }
        None => {
            // 原生分派（T010 注册表实现 NativeDispatch）。
            // 参数/接收者临时压回调用帧操作数栈充当 GC 根（native 可能分配）。
            let temp_count = args.len() + usize::from(this.is_some());
            if let Some(reference) = this {
                push1(frames, Slot::Ref(reference));
            }
            for arg in &args {
                push1(frames, *arg);
            }
            let call = NativeCall {
                class: defining,
                name: mref.name,
                desc: mref.desc,
                this,
                args,
                thread,
            };
            let result = machine
                .natives
                .call_native(machine.cs, machine.heap, machine.host, call);
            for _ in 0..temp_count {
                pop1(frames)?;
            }
            match result {
                // 原生方法抛出的 Java 异常（T014）：转常规异常展开。
                // 临时根已弹出（处理器入口本就清空操作数栈）；
                // pc 归位到调用指令——与字节码路径的 caller_pc 恢复一致，
                // 处理器表按 JVM 语义（故障指令 ∈ [start, end)）匹配。
                Err(KvmError::Throw(exception)) => {
                    frames.last_mut().expect("frame stack empty").pc = call_pc;
                    Ok(InsnOutcome::Unwind(Unwind::Thrown(exception)))
                }
                // 绿线程让出（T033）：sleep/yield/join 轮询版上抛。
                // 与 Throw 相反，pc **保持已推进状态**（调用指令在分派前
                // 已 +1）：让出是正常控制流，唤醒后从下一条指令续跑；
                // 若归位重执行 sleep 调用会再次 yield 死循环。
                // 临时 GC 根已在上方统一弹出。
                // Object.wait（T034）：先经 Monitors 桥接——完全释放
                // 监视器（记录重入深度）并入 FIFO 等待队列；未持锁 →
                // IllegalMonitorStateException（JLS 17.14，pc 归位同
                // Throw 路径：异常是故障而非让出）。
                Err(KvmError::Yield(kind)) => {
                    if let crate::YieldKind::Wait { obj, .. } = kind
                        && machine.monitors.wait(obj, thread).is_err() {
                            let err = crate::cldc::lang::throw_java(
                                machine.cs,
                                machine.heap,
                                "java/lang/IllegalMonitorStateException",
                                None,
                            );
                            if let KvmError::Throw(exception) = err {
                                frames.last_mut().expect("frame stack empty").pc = call_pc;
                                return Ok(InsnOutcome::Unwind(Unwind::Thrown(exception)));
                            }
                        }
                    Ok(InsnOutcome::Yielded(kind))
                }
                other => {
                    if let Some(value) = other? {
                        push_slot_value(frames, value);
                    }
                    // notify/notifyAll 请求位桥接（T034）：native 置位，
                    // 此处经 Monitors 执行标记（实际唤醒在调用者释放锁
                    // 后由调度器 reacquire_ready 移交）。未持锁 →
                    // IllegalMonitorStateException 同步抛（pc 归位）。
                    while let Some((obj, tid, all)) = machine.cs.notify_requests.pop() {
                        let marked = if all {
                            machine.monitors.notify_all(obj, tid)
                        } else {
                            machine.monitors.notify(obj, tid)
                        };
                        if marked.is_err() {
                            let err = crate::cldc::lang::throw_java(
                                machine.cs,
                                machine.heap,
                                "java/lang/IllegalMonitorStateException",
                                None,
                            );
                            if let KvmError::Throw(exception) = err {
                                frames.last_mut().expect("frame stack empty").pc = call_pc;
                                return Ok(InsnOutcome::Unwind(Unwind::Thrown(exception)));
                            }
                        }
                    }
                    Ok(InsnOutcome::Jumped)
                }
            }
        }
    }
}
