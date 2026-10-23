//! class 文件解析器集成测试（T005）：
//! - jc.jar 全部 89 类逐类解析断言（版本分布、this_class 与文件名一致）。
//! - 手构坏类字节的错误分支（魔数/版本/截断/尾字节/父类/常量池索引）。

mod common;

use skyengine_kvm::KvmError;
use skyengine_kvm::classfile::ClassFile;

#[test]
fn parses_every_class_in_jc_jar() {
    let jar = common::load_jar();
    let mut class_count = 0usize;
    let mut major_counts: std::collections::BTreeMap<u16, usize> = Default::default();
    for (name, bytes) in &jar {
        let Some(stem) = name.strip_suffix(".class") else {
            continue;
        };
        class_count += 1;
        let class =
            ClassFile::parse(bytes).unwrap_or_else(|error| panic!("parse {name}: {error}"));
        assert_eq!(class.this_class, stem, "this_class mismatch for {name}");
        assert!(
            (45..=48).contains(&class.major_version),
            "{name}: major {}",
            class.major_version
        );
        *major_counts.entry(class.major_version).or_default() += 1;
        if class.this_class != "java/lang/Object" {
            assert!(class.super_class.is_some(), "{name} lacks a super class");
        }
    }
    assert_eq!(class_count, 89, "jc.jar class file count");
    // 分析基线：major 45 ×16、47 ×73。
    assert_eq!(major_counts.get(&45).copied().unwrap_or(0), 16);
    assert_eq!(major_counts.get(&47).copied().unwrap_or(0), 73);
}

#[test]
fn entry_class_matches_mainclass() {
    let jar = common::load_jar();
    let mrp = std::fs::read(common::fixture_path()).expect("read fixture");
    let mainclass = common::mrp_read_entry(&mrp, "mainclass");
    let name = String::from_utf8_lossy(&mainclass)
        .trim_end_matches('\0')
        .trim()
        .to_string();
    let path = format!("{}.class", name.replace('.', "/"));
    assert!(
        jar.contains_key(&path),
        "mainclass {name} -> {path} missing from jc.jar"
    );
}

/// 与标准 zip 库的参照提取对齐（PowerShell System.IO.Compression）：
/// a.class 为 369 字节、major 45.3 的 JNI 回调 stub 类。
#[test]
fn a_class_extraction_matches_reference() {
    let jar = common::load_jar();
    let bytes = jar
        .get("a.class")
        .expect("a.class present in jc.jar");
    assert_eq!(bytes.len(), 369, "a.class size");
    let expected_prefix = [
        0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x03, 0x00, 0x2D, 0x00, 0x1D, 0x08, 0x00, 0x1C, 0x07, 0x00,
        0x14,
    ];
    assert_eq!(&bytes[..16], &expected_prefix, "a.class prefix");
    // 整个常量池 + 类结构必须可解析（曾因池计数差一报 tag 0）。
    let class = ClassFile::parse(bytes).expect("a.class parses");
    assert_eq!(class.this_class, "a");
    assert_eq!(class.super_class.as_deref(), Some("java/lang/Object"));
}

/// 手构最小合法 class：常量池 4 项（utf8 "T"、Class、utf8
/// "java/lang/Object"、Class；count = 5，槽 0 不可用），无接口/字段/方法/属性。
fn minimal_class_bytes() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0xCAFEBABEu32.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // minor
    out.extend_from_slice(&47u16.to_be_bytes()); // major
    out.extend_from_slice(&5u16.to_be_bytes()); // constant_pool_count（= 条目数 + 1）
    out.push(1);
    out.extend_from_slice(&1u16.to_be_bytes());
    out.push(b'T');
    out.push(7);
    out.extend_from_slice(&1u16.to_be_bytes());
    out.push(1);
    out.extend_from_slice(&16u16.to_be_bytes());
    out.extend_from_slice(b"java/lang/Object");
    out.push(7);
    out.extend_from_slice(&3u16.to_be_bytes());
    out.extend_from_slice(&0x0021u16.to_be_bytes()); // public super
    out.extend_from_slice(&2u16.to_be_bytes()); // this_class
    out.extend_from_slice(&4u16.to_be_bytes()); // super_class
    out.extend_from_slice(&0u16.to_be_bytes()); // interfaces_count
    out.extend_from_slice(&0u16.to_be_bytes()); // fields_count
    out.extend_from_slice(&0u16.to_be_bytes()); // methods_count
    out.extend_from_slice(&0u16.to_be_bytes()); // attributes_count
    out
}

#[test]
fn parses_a_minimal_handcrafted_class() {
    let class = ClassFile::parse(&minimal_class_bytes()).unwrap();
    assert_eq!(class.this_class, "T");
    assert_eq!(class.super_class.as_deref(), Some("java/lang/Object"));
    assert_eq!(class.major_version, 47);
    assert!(class.fields.is_empty());
    assert!(class.methods.is_empty());
}

#[test]
fn rejects_a_bad_magic() {
    let mut bytes = minimal_class_bytes();
    bytes[0] = 0xBA;
    let error = ClassFile::parse(&bytes).unwrap_err();
    assert!(error.to_string().contains("bad magic"), "{error}");
}

#[test]
fn rejects_a_future_version() {
    let mut bytes = minimal_class_bytes();
    bytes[7] = 52; // major 版本号高字节
    let error = ClassFile::parse(&bytes).unwrap_err();
    assert!(matches!(error, KvmError::UnsupportedVersion(_)), "{error}");
}

#[test]
fn rejects_every_truncation_point() {
    let full = minimal_class_bytes();
    for cut in 0..full.len() {
        let result = ClassFile::parse(&full[..cut]);
        assert!(result.is_err(), "truncation at {cut} bytes should fail");
    }
}

#[test]
fn rejects_trailing_bytes() {
    let mut bytes = minimal_class_bytes();
    bytes.push(0);
    let error = ClassFile::parse(&bytes).unwrap_err();
    assert!(error.to_string().contains("trailing"), "{error}");
}

#[test]
fn rejects_a_missing_super_class() {
    let mut bytes = minimal_class_bytes();
    let len = bytes.len();
    // super_class 索引清零且 this_class 不是 java/lang/Object。
    bytes[len - 10..len - 8].copy_from_slice(&0u16.to_be_bytes());
    let error = ClassFile::parse(&bytes).unwrap_err();
    assert!(error.to_string().contains("no super class"), "{error}");
}

#[test]
fn rejects_a_constant_index_out_of_range() {
    let mut bytes = minimal_class_bytes();
    let len = bytes.len();
    // this_class 指向不存在的常量池槽。
    bytes[len - 12..len - 10].copy_from_slice(&99u16.to_be_bytes());
    let error = ClassFile::parse(&bytes).unwrap_err();
    assert!(matches!(error, KvmError::ClassFormat(_)), "{error}");
}

#[test]
fn rejects_a_class_entry_pointing_at_utf8() {
    let mut bytes = minimal_class_bytes();
    let len = bytes.len();
    // this_class 指向 utf8 条目（类型混淆）。
    bytes[len - 12..len - 10].copy_from_slice(&1u16.to_be_bytes());
    let error = ClassFile::parse(&bytes).unwrap_err();
    assert!(matches!(error, KvmError::ClassFormat(_)), "{error}");
}

#[test]
fn rejects_an_unknown_constant_tag() {
    let mut bytes = minimal_class_bytes();
    bytes[10] = 0x7f; // 常量池首个条目的 tag
    let error = ClassFile::parse(&bytes).unwrap_err();
    assert!(error.to_string().contains("unknown constant tag"), "{error}");
}

/// long/double 的幻影槽必须不破坏池索引：Fieldref 引用跨越幻影槽之后的
/// utf8/NameAndType 条目仍能正确校验（曾因 raw 未按槽占位而误判）。
#[test]
fn long_double_phantom_slots_keep_pool_indices_stable() {
    let mut out = Vec::new();
    out.extend_from_slice(&0xCAFEBABEu32.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // minor
    out.extend_from_slice(&47u16.to_be_bytes()); // major
    // 池：1 Long(占 1-2 槽)；3 utf8 "x"；4 utf8 "J"；5 Class(3)；
    // 6 NameAndType(4..不对，名字/描述符都用 "x" 简化)；7 Fieldref(5, 6)。
    out.extend_from_slice(&8u16.to_be_bytes()); // constant_pool_count = 8
    out.push(5); // Long
    out.extend_from_slice(&1i64.to_be_bytes());
    out.push(1);
    out.extend_from_slice(&1u16.to_be_bytes());
    out.push(b'x');
    out.push(1);
    out.extend_from_slice(&1u16.to_be_bytes());
    out.push(b'J');
    out.push(7); // Class -> utf8 #3 ("x")
    out.extend_from_slice(&3u16.to_be_bytes());
    out.push(12); // NameAndType -> #4, #3
    out.extend_from_slice(&4u16.to_be_bytes());
    out.extend_from_slice(&3u16.to_be_bytes());
    out.push(9); // Fieldref -> class #5, nat #6
    out.extend_from_slice(&5u16.to_be_bytes());
    out.extend_from_slice(&6u16.to_be_bytes());
    // 类结构：this #5、super #5（借同一个 Class 条目，够用即可）。
    out.extend_from_slice(&0u16.to_be_bytes()); // access
    out.extend_from_slice(&5u16.to_be_bytes()); // this_class
    out.extend_from_slice(&5u16.to_be_bytes()); // super_class
    out.extend_from_slice(&0u16.to_be_bytes()); // interfaces
    out.extend_from_slice(&0u16.to_be_bytes()); // fields
    out.extend_from_slice(&0u16.to_be_bytes()); // methods
    out.extend_from_slice(&0u16.to_be_bytes()); // attributes

    let class = ClassFile::parse(&out).expect("phantom-slot class parses");
    assert_eq!(class.this_class, "x");
    // 常量池可访问：类名经 utf8 #3 解出。
    assert_eq!(class.super_class.as_deref(), Some("x"));
}

/// long/double 的幻影槽落在有效索引之外是规范禁止的：
/// count=2 时唯一有效槽是 1，幻影槽 2 越界。
#[test]
fn rejects_a_long_occupying_the_final_slot() {
    let mut out = Vec::new();
    out.extend_from_slice(&0xCAFEBABEu32.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&47u16.to_be_bytes());
    out.extend_from_slice(&2u16.to_be_bytes()); // count=2：仅槽 1 有效
    out.push(5); // Long 在槽 1，幻影槽 2 越界
    out.extend_from_slice(&1i64.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // access
    out.extend_from_slice(&1u16.to_be_bytes()); // this_class
    out.extend_from_slice(&1u16.to_be_bytes()); // super_class
    for _ in 0..4 {
        out.extend_from_slice(&0u16.to_be_bytes());
    }
    let error = ClassFile::parse(&out).unwrap_err();
    assert!(
        error.to_string().contains("final slot"),
        "{error}"
    );
}
