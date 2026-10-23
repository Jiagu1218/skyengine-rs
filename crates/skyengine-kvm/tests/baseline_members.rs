//! T047：168 成员基线审计（FR-015/SC-002 正式验收步骤）。
//!
//! 数据源：jar-analysis（`aerokvm-run/jar-analysis-full.txt`）对 jc.jar
//! 全量字节码的常量池统计——「top external members」段逐成员列出
//! 游戏 jar 引用的外部（CLDC/MIDP）API 面。**实提 167 条唯一成员**
//! （166 方法 + 1 字段 System.out；任务口径 168 差 1 属清单笔误，
//! 以文件实数为准并在此固化）。
//!
//! 审计分四级（活文档式断言，见尾注）：
//! 1. **可解析**：`bootstrap_lookup` 命中或 jar 条目存在（load_class 成功）；
//! 2. **可链接**：`find_method/find_field` 命中（走父链/接口闭包，与
//!    解释器 `exec_invoke` 同一解析路径）；native（body=None）方法还须
//!    定义类的 bootstrap DECL 含该 名字+描述符（`RegistryDispatch` 前提）；
//! 3. **占位**：可链接但绑定到通用桩（`is_stub`：stub_void/null/zero，
//!    FR-016 允许的规范占位语义）；方法带 `acc::ABSTRACT`（接口签名）
//!    亦计入占位——接口本体不可调用，具体实现类另行覆盖；
//! 4. **已实现**：其余（真实现或 jar 字节码体）。
//!
//! 断言策略：`missing == KNOWN_GAPS`（双向对账）。将来谁补掉某项缺口，
//! 从白名单挪走即自动收紧；白名单残留项即"已知缺口"的固化清单。
//! 占位清单不设断言上限（FR-016 允许），仅打印供 T039/T041 待办参考。

mod common;

use skyengine_kvm::interp::natives::{bootstrap_lookup, is_stub};
use skyengine_kvm::runtime::classspace::{ClassSpace, MapClassProvider};

/// (类内部名, 成员名, 描述符)；描述符以 `(` 起为方法，否则字段。
/// 顺序沿用清单（按引用次数降序），高频缺口排前面便于阅读。
static BASELINE: &[(&str, &str, &str)] = &[
    ("java/lang/Object", "<init>", "()V"),
    ("java/io/DataInputStream", "readShort", "()S"),
    ("java/lang/StringBuffer", "toString", "()Ljava/lang/String;"),
    ("java/lang/StringBuffer", "append", "(Ljava/lang/String;)Ljava/lang/StringBuffer;"),
    ("java/io/DataInputStream", "readUTF", "()Ljava/lang/String;"),
    ("java/lang/StringBuffer", "<init>", "()V"),
    ("java/lang/StringBuffer", "append", "(I)Ljava/lang/StringBuffer;"),
    ("java/lang/String", "toCharArray", "()[C"),
    ("java/io/DataInputStream", "readByte", "()B"),
    ("java/io/DataInputStream", "readBoolean", "()Z"),
    ("java/io/PrintStream", "println", "(Ljava/lang/String;)V"),
    ("java/lang/System", "arraycopy", "(Ljava/lang/Object;ILjava/lang/Object;II)V"),
    ("javax/microedition/lcdui/Graphics", "setColor", "(I)V"),
    ("java/io/DataInputStream", "close", "()V"),
    ("javax/microedition/lcdui/Image", "getWidth", "()I"),
    ("java/io/DataInputStream", "readInt", "()I"),
    ("javax/microedition/lcdui/Image", "getHeight", "()I"),
    ("java/lang/StringBuffer", "<init>", "(Ljava/lang/String;)V"),
    ("java/lang/String", "length", "()I"),
    ("javax/microedition/lcdui/Graphics", "fillRect", "(IIII)V"),
    ("java/util/Hashtable", "get", "(Ljava/lang/Object;)Ljava/lang/Object;"),
    ("java/lang/Math", "abs", "(I)I"),
    ("java/io/ByteArrayInputStream", "<init>", "([B)V"),
    ("javax/microedition/lcdui/Graphics", "drawImage", "(Ljavax/microedition/lcdui/Image;III)V"),
    ("java/io/DataInputStream", "<init>", "(Ljava/io/InputStream;)V"),
    ("java/lang/StringBuffer", "append", "([C)Ljava/lang/StringBuffer;"),
    ("java/io/DataInputStream", "read", "()I"),
    ("java/io/DataOutputStream", "writeShort", "(I)V"),
    ("java/lang/Integer", "<init>", "(I)V"),
    ("java/util/Hashtable", "put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"),
    ("java/lang/String", "substring", "(II)Ljava/lang/String;"),
    ("java/io/DataOutputStream", "writeBoolean", "(Z)V"),
    ("java/util/Hashtable", "<init>", "()V"),
    ("java/lang/Integer", "intValue", "()I"),
    ("java/io/DataOutputStream", "writeUTF", "(Ljava/lang/String;)V"),
    ("java/lang/String", "equals", "(Ljava/lang/Object;)Z"),
    ("java/lang/Integer", "parseInt", "(Ljava/lang/String;)I"),
    ("java/lang/System", "currentTimeMillis", "()J"),
    ("java/lang/String", "<init>", "([C)V"),
    ("javax/microedition/lcdui/Graphics", "drawLine", "(IIII)V"),
    ("javax/microedition/lcdui/Graphics", "setClip", "(IIII)V"),
    ("java/util/Hashtable", "size", "()I"),
    ("java/lang/StringBuffer", "append", "(C)Ljava/lang/StringBuffer;"),
    ("javax/microedition/lcdui/Graphics", "drawRect", "(IIII)V"),
    ("java/io/DataOutputStream", "writeInt", "(I)V"),
    ("java/io/DataOutputStream", "writeByte", "(I)V"),
    ("java/lang/Thread", "start", "()V"),
    ("java/io/ByteArrayOutputStream", "toByteArray", "()[B"),
    ("java/io/ByteArrayOutputStream", "<init>", "()V"),
    ("java/lang/Thread", "sleep", "(J)V"),
    ("javax/microedition/lcdui/Image", "createImage", "([BII)Ljavax/microedition/lcdui/Image;"),
    ("javax/microedition/lcdui/Image", "createRGBImage", "([IIIZ)Ljavax/microedition/lcdui/Image;"),
    ("javax/microedition/lcdui/Image", "getRGB", "([IIIIIII)V"),
    ("java/util/Hashtable", "keys", "()Ljava/util/Enumeration;"),
    ("java/io/DataOutputStream", "<init>", "(Ljava/io/OutputStream;)V"),
    ("java/util/Enumeration", "nextElement", "()Ljava/lang/Object;"),
    ("java/lang/Thread", "<init>", "(Ljava/lang/Runnable;)V"),
    ("java/lang/String", "indexOf", "(Ljava/lang/String;)I"),
    ("javax/microedition/lcdui/Font", "getHeight", "()I"),
    ("java/util/Hashtable", "clear", "()V"),
    ("java/util/Enumeration", "hasMoreElements", "()Z"),
    ("java/io/DataInputStream", "readFully", "([B)V"),
    ("java/lang/Throwable", "toString", "()Ljava/lang/String;"),
    ("java/lang/String", "getBytes", "(Ljava/lang/String;)[B"),
    ("javax/microedition/lcdui/Image", "createImage", "(II)Ljavax/microedition/lcdui/Image;"),
    ("java/io/DataInputStream", "readLong", "()J"),
    ("java/lang/StringBuffer", "append", "(Z)Ljava/lang/StringBuffer;"),
    ("java/io/DataOutputStream", "writeLong", "(J)V"),
    ("java/util/Hashtable", "<init>", "(I)V"),
    ("javax/microedition/lcdui/Image", "getGraphics", "()Ljavax/microedition/lcdui/Graphics;"),
    ("java/util/Hashtable", "remove", "(Ljava/lang/Object;)Ljava/lang/Object;"),
    ("javax/microedition/lcdui/Graphics", "translate", "(II)V"),
    ("javax/microedition/lcdui/Graphics", "setFont", "(Ljavax/microedition/lcdui/Font;)V"),
    ("java/lang/Throwable", "printStackTrace", "()V"),
    ("java/lang/String", "substring", "(I)Ljava/lang/String;"),
    ("java/lang/String", "<init>", "([BLjava/lang/String;)V"),
    ("javax/microedition/rms/RecordStore", "openRecordStore", "(Ljava/lang/String;Z)Ljavax/microedition/rms/RecordStore;"),
    ("javax/microedition/rms/RecordStore", "setRecord", "(I[BII)V"),
    ("java/util/Vector", "addElement", "(Ljava/lang/Object;)V"),
    ("java/lang/Object", "getClass", "()Ljava/lang/Class;"),
    ("java/lang/String", "trim", "()Ljava/lang/String;"),
    ("javax/microedition/lcdui/Graphics", "drawChar", "(CIII)V"),
    ("javax/microedition/rms/RecordStore", "getNumRecords", "()I"),
    ("java/io/InputStream", "close", "()V"),
    ("java/io/DataInputStream", "readFully", "([BII)V"),
    ("javax/microedition/rms/RecordStore", "addRecord", "([BII)I"),
    ("javax/microedition/lcdui/Graphics", "drawRGB", "([IIIIIIIZ)V"),
    ("java/lang/Exception", "printStackTrace", "()V"),
    ("java/util/Vector", "elementAt", "(I)Ljava/lang/Object;"),
    ("java/lang/StringBuffer", "length", "()I"),
    ("javax/microedition/lcdui/Font", "charWidth", "(C)I"),
    ("java/util/Vector", "<init>", "()V"),
    ("java/util/Hashtable", "elements", "()Ljava/util/Enumeration;"),
    ("javax/microedition/rms/RecordStore", "getRecord", "(I)[B"),
    ("java/util/Vector", "size", "()I"),
    ("javax/microedition/rms/RecordStore", "closeRecordStore", "()V"),
    ("java/io/InputStream", "read", "([B)I"),
    ("java/io/DataOutputStream", "close", "()V"),
    ("java/lang/StringBuffer", "delete", "(II)Ljava/lang/StringBuffer;"),
    ("java/lang/Object", "toString", "()Ljava/lang/String;"),
    ("javax/microedition/lcdui/Display", "getDisplay", "(Ljavax/microedition/midlet/MIDlet;)Ljavax/microedition/lcdui/Display;"),
    ("java/io/DataInputStream", "readChar", "()C"),
    ("java/lang/String", "toString", "()Ljava/lang/String;"),
    ("java/lang/Character", "isDigit", "(C)Z"),
    ("java/lang/String", "indexOf", "(II)I"),
    ("java/io/InputStream", "read", "()I"),
    ("java/lang/Boolean", "booleanValue", "()Z"),
    ("java/lang/StringBuffer", "<init>", "(I)V"),
    ("java/util/Hashtable", "isEmpty", "()Z"),
    ("java/util/Random", "nextInt", "()I"),
    ("java/io/ByteArrayOutputStream", "close", "()V"),
    ("java/lang/String", "indexOf", "(Ljava/lang/String;I)I"),
    ("java/io/DataInputStream", "skip", "(J)J"),
    ("java/lang/Thread", "<init>", "()V"),
    ("javax/microedition/lcdui/Graphics", "fillTriangle", "(IIIIII)V"),
    ("javax/microedition/rms/RecordStore", "enumerateRecords", "(Ljavax/microedition/rms/RecordFilter;Ljavax/microedition/rms/RecordComparator;Z)Ljavax/microedition/rms/RecordEnumeration;"),
    ("javax/microedition/midlet/MIDlet", "<init>", "()V"),
    ("javax/microedition/lcdui/Graphics", "getTranslateY", "()I"),
    ("java/io/DataOutputStream", "write", "([B)V"),
    ("javax/microedition/lcdui/Graphics", "drawRegion", "(Ljavax/microedition/lcdui/Image;IIIIIIII)V"),
    ("javax/microedition/lcdui/Graphics", "getTranslateX", "()I"),
    ("java/lang/RuntimeException", "<init>", "()V"),
    ("java/lang/Class", "newInstance", "()Ljava/lang/Object;"),
    ("javax/microedition/media/Manager", "createPlayer", "(Ljava/io/InputStream;Ljava/lang/String;)Ljavax/microedition/media/Player;"),
    ("java/io/DataInputStream", "read", "([B)I"),
    ("java/lang/StringBuffer", "append", "(Ljava/lang/Object;)Ljava/lang/StringBuffer;"),
    ("javax/microedition/lcdui/Graphics", "drawArc", "(IIIIII)V"),
    ("javax/microedition/lcdui/Font", "getFont", "(III)Ljavax/microedition/lcdui/Font;"),
    ("java/lang/String", "valueOf", "(Ljava/lang/Object;)Ljava/lang/String;"),
    ("javax/microedition/lcdui/Display", "setCurrent", "(Ljavax/microedition/lcdui/Displayable;)V"),
    ("javax/microedition/rms/RecordEnumeration", "nextRecordId", "()I"),
    ("javax/microedition/lcdui/Image", "createImage", "(Ljava/lang/String;)Ljavax/microedition/lcdui/Image;"),
    ("javax/microedition/lcdui/Graphics", "fillRoundRect", "(IIIIII)V"),
    ("java/io/InputStream", "read", "([BII)I"),
    ("java/io/DataOutputStream", "writeChar", "(I)V"),
    ("javax/microedition/lcdui/Font", "charsWidth", "([CII)I"),
    ("java/lang/String", "startsWith", "(Ljava/lang/String;)Z"),
    ("java/util/Vector", "removeElement", "(Ljava/lang/Object;)Z"),
    ("javax/microedition/media/Player", "start", "()V"),
    ("javax/microedition/lcdui/Canvas", "<init>", "()V"),
    ("javax/microedition/rms/RecordEnumeration", "hasNextElement", "()Z"),
    ("java/lang/Class", "getResourceAsStream", "(Ljava/lang/String;)Ljava/io/InputStream;"),
    ("java/lang/System", "getProperty", "(Ljava/lang/String;)Ljava/lang/String;"),
    ("javax/microedition/lcdui/Graphics", "fillArc", "(IIIIII)V"),
    ("java/lang/Class", "forName", "(Ljava/lang/String;)Ljava/lang/Class;"),
    ("java/util/Calendar", "getInstance", "()Ljava/util/Calendar;"),
    ("java/lang/Thread", "currentThread", "()Ljava/lang/Thread;"),
    ("java/lang/Thread", "interrupt", "()V"),
    ("java/util/Random", "<init>", "(J)V"),
    ("java/util/Hashtable", "containsKey", "(Ljava/lang/Object;)Z"),
    ("javax/microedition/media/Player", "close", "()V"),
    ("java/lang/Integer", "toHexString", "(I)Ljava/lang/String;"),
    ("javax/microedition/media/Player", "setLoopCount", "(I)V"),
    ("java/util/Calendar", "get", "(I)I"),
    ("javax/microedition/lcdui/Graphics", "drawChars", "([CIIIII)V"),
    ("java/io/ByteArrayOutputStream", "write", "([BII)V"),
    ("java/lang/RuntimeException", "<init>", "(Ljava/lang/String;)V"),
    ("java/io/ByteArrayInputStream", "close", "()V"),
    ("java/lang/Thread", "yield", "()V"),
    ("javax/microedition/lcdui/Graphics", "drawString", "(Ljava/lang/String;III)V"),
    ("java/lang/Boolean", "<init>", "(Z)V"),
    ("javax/microedition/media/Player", "prefetch", "()V"),
    ("java/lang/String", "charAt", "(I)C"),
    ("javax/microedition/media/Player", "stop", "()V"),
    ("java/io/InputStream", "skip", "(J)J"),
    ("javax/microedition/lcdui/Graphics", "getColor", "()I"),
    ("java/lang/System", "out", "Ljava/io/PrintStream;"), // 唯一字段条目
];

/// 已知缺口白名单（"类.成员+描述符"）：尚未注册/实现的成员固化于此。
/// 每补掉一项就从这里删除——测试自动收紧（活文档）。
static KNOWN_GAPS: &[&str] = &[
    // 首轮审计后回填
];

/// 逐成员审计：四级分类 + 白名单双向对账。
#[test]
fn baseline_member_audit() {
    // 数组完整性自检：清单文件实数 167（166 方法 + 1 字段），防生成截断。
    assert_eq!(BASELINE.len(), 167, "基线数组须与清单实数一致");
    let jar = common::load_jar();
    let mut cs = ClassSpace::new(Box::new(MapClassProvider::from_jar(&jar)));

    let mut implemented: Vec<String> = Vec::new();
    let mut placeholder: Vec<String> = Vec::new();
    let mut missing: Vec<String> = Vec::new();

    for &(class, name, desc) in BASELINE {
        let key = format!("{class}.{name}{desc}");
        // ── 1) 类级可解析 ──
        let id = match cs.load_class(class) {
            Ok(id) => id,
            Err(error) => {
                missing.push(format!("{key}  [类不可解析: {error}]"));
                continue;
            }
        };
        if let Err(error) = cs.ensure_layout(id) {
            missing.push(format!("{key}  [布局失败: {error}]"));
            continue;
        }
        // ── 2) 成员级可链接 ──
        if desc.starts_with('(') {
            let (n, d) = (cs.syms.intern(name), cs.syms.intern(desc));
            let Some((defining, index)) = cs.find_method(id, n, d) else {
                missing.push(format!("{key}  [方法未声明]"));
                continue;
            };
            let method = &cs.class(defining).methods[index as usize];
            if method.body.is_some() {
                // jar 字节码体（本基线面不含，防御性分支）。
                implemented.push(key.clone());
                continue;
            }
            // native：RegistryDispatch 按定义类查表，缺绑定即不可调用。
            let Some(decl) = bootstrap_lookup(cs.name_of(defining)) else {
                missing.push(format!(
                    "{key}  [native 无 bootstrap 绑定: 定义类 {}]",
                    cs.name_of(defining)
                ));
                continue;
            };
            let Some(binding) = decl
                .methods
                .iter()
                .find(|m| m.name == name && m.desc == desc)
            else {
                missing.push(format!(
                    "{key}  [bootstrap 表缺方法: 定义类 {}]",
                    cs.name_of(defining)
                ));
                continue;
            };
            if is_stub(binding.func)
                || binding.access_flags & skyengine_kvm::classfile::member::acc::ABSTRACT != 0
            {
                placeholder.push(key.clone());
            } else {
                implemented.push(key.clone());
            }
        } else {
            let (n, d) = (cs.syms.intern(name), cs.syms.intern(desc));
            match cs.find_field(id, n, d) {
                Ok(_) => implemented.push(key.clone()),
                Err(error) => missing.push(format!("{key}  [字段不可解析: {error}]")),
            }
        }
    }

    // ── 白名单双向对账 ──
    let unexpected: Vec<&String> = missing
        .iter()
        .filter(|entry| !KNOWN_GAPS.iter().any(|gap| entry.contains(gap)))
        .collect();
    let stale: Vec<&str> = KNOWN_GAPS
        .iter()
        .copied()
        .filter(|gap| !missing.iter().any(|entry| entry.contains(gap)))
        .collect();
    assert!(
        unexpected.is_empty(),
        "基线外缺口 {} 项（须补全或入白名单）:\n{}",
        unexpected.len(),
        unexpected.iter().map(|e| format!("  {e}")).collect::<Vec<_>>().join("\n")
    );
    assert!(
        stale.is_empty(),
        "白名单过期 {} 项（已补全，请从 KNOWN_GAPS 移除以收紧基线）:\n{}",
        stale.len(),
        stale.iter().map(|e| format!("  {e}")).collect::<Vec<_>>().join("\n")
    );

    // ── 四类计数（--nocapture 可见；含补全后余量=白名单）──
    let total = BASELINE.len();
    println!(
        "T047 基线审计：总 {total} = 已实现 {} + 占位 {} + 已知缺口 {}",
        implemented.len(),
        placeholder.len(),
        missing.len()
    );
    for entry in &placeholder {
        println!("  [占位] {entry}");
    }
    for entry in &missing {
        println!("  [缺口] {entry}");
    }
}

