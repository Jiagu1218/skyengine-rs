//! 集成测试共享夹具：从 gjxjwql.mrp fixture 解出 jc.jar 并在内存中开箱。
//!
//! 刻意不依赖 skyengine-core（kvm crate 禁止反向依赖），
//! MRP 容器与 zip 目录均按格式手工解析：
//! - MRP：`MRPG` 魔数 + 头部 0xd1 字节；`file_start@4`/`declared_len@8`/
//!   `list_start@12`（u32 LE）；目录条目 `{u32 名长, 名字, u32 偏移,
//!   u32 存储长, u32 保留}`；载荷以 `1f 8b 08` 开头即为 gzip。
//! - zip：尾部扫描 EOCD（`PK\x05\x06`）→ 中央目录（`PK\x01\x02`）→
//!   局部头（`PK\x03\x04`）定位数据；method 0 = store，8 = deflate。
//!   尺寸一律取自中央目录（局部头可能因数据描述符置 0）。

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::PathBuf;

use flate2::read::{DeflateDecoder, GzDecoder};

/// 手构 class 字节构造器（GC/解释器等运行时测试共享）。
/// 各测试二进制按需引用，未用到的条目不告警。
#[allow(dead_code)]
pub mod classbytes;

/// gjxjwql.mrp 夹具路径（相对本 crate 清单目录）。
pub fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../test/fixtures/395021_1002_320480_gjxjwql.mrp")
}

#[allow(dead_code)] // 按测试二进制分别编译，未引用属预期
fn u16le(bytes: &[u8], offset: usize) -> u16 {
    let raw = bytes
        .get(offset..offset + 2)
        .unwrap_or_else(|| panic!("u16 read at {offset:#x} out of range"));
    u16::from_le_bytes([raw[0], raw[1]])
}

fn u32le(bytes: &[u8], offset: usize) -> u32 {
    let raw = bytes
        .get(offset..offset + 4)
        .unwrap_or_else(|| panic!("u32 read at {offset:#x} out of range"));
    u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]])
}

/// 从 MRP 字节中读取指定名字的条目（gzip 载荷自动解压）。
pub fn mrp_read_entry(mrp: &[u8], want: &str) -> Vec<u8> {
    assert!(mrp.len() >= 0xd1, "mrp header truncated: {} bytes", mrp.len());
    assert_eq!(&mrp[0..4], b"MRPG", "bad mrp signature");
    let file_start = u32le(mrp, 4) as usize;
    let declared_len = u32le(mrp, 8) as usize;
    let list_start = u32le(mrp, 12) as usize;
    assert!(
        declared_len <= mrp.len(),
        "declared length {declared_len} exceeds file {}",
        mrp.len()
    );
    let directory_end = file_start + 8;
    assert!(
        list_start >= 0xd0 && list_start <= directory_end && directory_end <= declared_len,
        "invalid directory range {list_start:#x}..{directory_end:#x}"
    );

    let mut cursor = list_start;
    while cursor < directory_end {
        let name_len = u32le(mrp, cursor) as usize;
        assert!(
            name_len > 0 && cursor + 4 + name_len + 12 <= directory_end,
            "bad directory entry at {cursor:#x}"
        );
        let mut name = mrp[cursor + 4..cursor + 4 + name_len].to_vec();
        if name.last() == Some(&0) {
            name.pop();
        }
        let fields = cursor + 4 + name_len;
        let payload_offset = u32le(mrp, fields) as usize;
        let stored_len = u32le(mrp, fields + 4) as usize;
        if name == want.as_bytes() {
            let stored = &mrp[payload_offset..payload_offset + stored_len];
            if stored.len() >= 3 && stored[0] == 0x1f && stored[1] == 0x8b && stored[2] == 0x08 {
                let mut out = Vec::new();
                GzDecoder::new(stored)
                    .read_to_end(&mut out)
                    .unwrap_or_else(|error| panic!("gunzip entry {want}: {error}"));
                return out;
            }
            return stored.to_vec();
        }
        cursor = fields + 12;
    }
    panic!("mrp entry not found: {want}");
}

/// 内存 zip 开箱：返回 文件名 → 解压内容（store/deflate）。
#[allow(dead_code)] // 仅 midp_image/midp_font 等部分二进制引用
pub fn zip_read_all(zip: &[u8]) -> BTreeMap<String, Vec<u8>> {
    // 从尾部向前扫描 EOCD（固定 22 字节 + 最长 65535 字节注释）。
    let eocd = find_eocd(zip).expect("zip eocd not found");
    let total_entries = u16le(zip, eocd + 10) as usize;
    let cd_offset = u32le(zip, eocd + 16) as usize;

    let mut entries = BTreeMap::new();
    let mut cursor = cd_offset;
    for _ in 0..total_entries {
        assert_eq!(
            &zip[cursor..cursor + 4],
            b"PK\x01\x02",
            "central directory signature at {cursor:#x}"
        );
        let method = u16le(zip, cursor + 10);
        let compressed_size = u32le(zip, cursor + 20) as usize;
        let uncompressed_size = u32le(zip, cursor + 24) as usize;
        let name_len = u16le(zip, cursor + 28) as usize;
        let extra_len = u16le(zip, cursor + 30) as usize;
        let comment_len = u16le(zip, cursor + 32) as usize;
        let local_offset = u32le(zip, cursor + 42) as usize;
        let name =
            String::from_utf8_lossy(&zip[cursor + 46..cursor + 46 + name_len]).into_owned();

        // 局部头的名字/扩展字段长度可能与中央目录不同，按局部头定位数据。
        assert_eq!(
            &zip[local_offset..local_offset + 4],
            b"PK\x03\x04",
            "local header signature at {local_offset:#x}"
        );
        let local_name_len = u16le(zip, local_offset + 26) as usize;
        let local_extra_len = u16le(zip, local_offset + 28) as usize;
        let data_start = local_offset + 30 + local_name_len + local_extra_len;
        let data = &zip[data_start..data_start + compressed_size];

        let content = match method {
            0 => data.to_vec(),
            8 => {
                let mut out = Vec::with_capacity(uncompressed_size);
                DeflateDecoder::new(data)
                    .read_to_end(&mut out)
                    .unwrap_or_else(|error| panic!("inflate zip entry {name}: {error}"));
                out
            }
            other => panic!("unsupported zip method {other} for entry {name}"),
        };
        assert_eq!(
            content.len(),
            uncompressed_size,
            "zip entry size mismatch: {name}"
        );
        entries.insert(name, content);
        cursor += 46 + name_len + extra_len + comment_len;
    }
    entries
}

#[allow(dead_code)] // 同 zip_read_all
fn find_eocd(zip: &[u8]) -> Option<usize> {
    if zip.len() < 22 {
        return None;
    }
    let upper = zip.len() - 22;
    let lower = upper.saturating_sub(65535);
    (lower..=upper)
        .rev()
        .find(|&offset| &zip[offset..offset + 4] == b"PK\x05\x06")
}

/// 加载夹具 jc.jar 的全部条目（文件名 → 解压内容）。
#[allow(dead_code)] // 仅 midp_image 引用
pub fn load_jar() -> BTreeMap<String, Vec<u8>> {
    let mrp = fs::read(fixture_path()).expect("read gjxjwql.mrp fixture");
    let jar = mrp_read_entry(&mrp, "jc.jar");
    zip_read_all(&jar)
}
