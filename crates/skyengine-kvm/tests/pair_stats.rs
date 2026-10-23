//! 融合对频率统计（`--ignored` 手动跑）：解码真实 jc.jar 后统计
//! 实际产生的 Fused 对模式分布，指导专用复合执行体的选型。

#[allow(dead_code)]
mod common;

use std::collections::BTreeMap;

use common::{fixture_path, mrp_read_entry};
use skyengine_kvm::classfile::code::Insn;

use skyengine_kvm::runtime::classspace::{ClassSpace, MapClassProvider};

fn tag(insn: &Insn) -> &'static str {
    match insn {
        Insn::Nop => "Nop",
        Insn::PushNull => "PushNull",
        Insn::PushInt(_) => "PushInt",
        Insn::PushLong(_) => "PushLong",
        Insn::PushFloat(_) => "PushFloat",
        Insn::PushDouble(_) => "PushDouble",
        Insn::Ldc(_) => "Ldc",
        Insn::Load { kind, .. } => match kind {
            skyengine_kvm::classfile::code::Kind::Int => "LoadInt",
            skyengine_kvm::classfile::code::Kind::Long => "LoadLong",
            skyengine_kvm::classfile::code::Kind::Float => "LoadFloat",
            skyengine_kvm::classfile::code::Kind::Double => "LoadDouble",
            skyengine_kvm::classfile::code::Kind::Ref => "LoadRef",
        },
        Insn::Store { kind, .. } => match kind {
            skyengine_kvm::classfile::code::Kind::Int => "StoreInt",
            skyengine_kvm::classfile::code::Kind::Long => "StoreLong",
            skyengine_kvm::classfile::code::Kind::Float => "StoreFloat",
            skyengine_kvm::classfile::code::Kind::Double => "StoreDouble",
            skyengine_kvm::classfile::code::Kind::Ref => "StoreRef",
        },
        Insn::ArrayLoad(_) => "ArrayLoad",
        Insn::ArrayStore(_) => "ArrayStore",
        Insn::Pop => "Pop",
        Insn::Pop2 => "Pop2",
        Insn::Dup => "Dup",
        Insn::DupX1 => "DupX1",
        Insn::DupX2 => "DupX2",
        Insn::Dup2 => "Dup2",
        Insn::Dup2X1 => "Dup2X1",
        Insn::Dup2X2 => "Dup2X2",
        Insn::Swap => "Swap",
        Insn::Bin { .. } => "Bin",
        Insn::Neg(_) => "Neg",
        Insn::Iinc { .. } => "Iinc",
        Insn::Conv { .. } => "Conv",
        Insn::TruncByte => "TruncByte",
        Insn::TruncChar => "TruncChar",
        Insn::TruncShort => "TruncShort",
        Insn::Lcmp => "Lcmp",
        Insn::Fcmp { .. } => "Fcmp",
        Insn::Dcmp { .. } => "Dcmp",
        Insn::BranchZero { .. } => "BranchZero",
        Insn::BranchInt { .. } => "BranchInt",
        Insn::BranchRef { .. } => "BranchRef",
        Insn::BranchNull { .. } => "BranchNull",
        Insn::Goto(_) => "Goto",
        Insn::Jsr(_) => "Jsr",
        Insn::Ret(_) => "Ret",
        Insn::TableSwitch { .. } => "TableSwitch",
        Insn::LookupSwitch { .. } => "LookupSwitch",
        Insn::Return(_) => "Return",
        Insn::GetStatic(_) => "GetStatic",
        Insn::PutStatic(_) => "PutStatic",
        Insn::GetField(_) => "GetField",
        Insn::PutField(_) => "PutField",
        Insn::InvokeVirtual(_) => "InvokeVirtual",
        Insn::InvokeSpecial(_) => "InvokeSpecial",
        Insn::InvokeStatic(_) => "InvokeStatic",
        Insn::InvokeInterface(_) => "InvokeInterface",
        Insn::New(_) => "New",
        Insn::NewArray(_) => "NewArray",
        Insn::ANewArray(_) => "ANewArray",
        Insn::ArrayLength => "ArrayLength",
        Insn::Athrow => "Athrow",
        Insn::CheckCast(_) => "CheckCast",
        Insn::InstanceOf(_) => "InstanceOf",
        Insn::MonitorEnter => "MonitorEnter",
        Insn::MonitorExit => "MonitorExit",
        Insn::MultiNewArray { .. } => "MultiNewArray",
        Insn::Fused { .. } => "Fused",
    }
}

#[test]
#[ignore = "统计（手动跑）：真实 jar 的融合对频率"]
fn fused_pair_frequency() {
    let mrp = std::fs::read(fixture_path()).expect("read mrp fixture");
    let jar = mrp_read_entry(&mrp, "jc.jar");
    // zip 解包（复用 common 的 zip 读取器路径：经 install 侧同款逻辑）。
    let entries = common::zip_read_all(&jar);
    // 走 jar 条目逐类加载。
    let mut cs2 = ClassSpace::new(Box::new(MapClassProvider::from_jar(&entries)));
    let mut names: Vec<String> = entries
        .keys()
        .filter(|k| k.ends_with(".class"))
        .cloned()
        .collect();
    names.sort();
    for name in &names {
        let internal = name.trim_end_matches(".class");
        let _ = cs2.load_class(internal);
    }
    let total_classes = cs2.class_count();
    let mut pairs: BTreeMap<(&'static str, &'static str), usize> = BTreeMap::new();
    let mut total_fused = 0usize;
    let mut total_insns = 0usize;
    for index in 0..total_classes {
        let class = cs2.class(skyengine_kvm::runtime::ClassId(index as u32));
        for method in &class.methods {
            if let Some(body) = &method.body {
                total_insns += body.insns.len();
                for insn in &body.insns {
                    if let Insn::Fused { a, b } = insn {
                        *pairs.entry((tag(a), tag(b))).or_insert(0) += 1;
                        total_fused += 1;
                    }
                }
            }
        }
    }
    let mut rows: Vec<_> = pairs.into_iter().collect();
    rows.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    println!("classes={total_classes} insns={total_insns} fused={total_fused}");
    for ((a, b), count) in rows.into_iter().take(25) {
        println!("{count:6}  {a} + {b}");
    }
}
