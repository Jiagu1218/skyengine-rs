//! jar（zip 格式）解包：库内自持实现，供 `Vm::install_jar` 使用。
//!
//! 不复用 `tests/common` 的测试助手（测试代码不可被库引用）。
//! 解析策略与之一致：尾部扫描 EOCD（`PK\x05\x06`）→ 遍历中央目录
//! （`PK\x01\x02`）→ 按局部头（`PK\x03\x04`）定位数据（局部头的
//! 名字/扩展字段长度可能与中央目录不一致，尺寸一律取自中央目录）。
//! method 0 = store，8 = deflate（flate2）。

use std::collections::BTreeMap;

use flate2::read::DeflateDecoder;
use std::io::Read;

/// 从尾部向前扫描 EOCD（固定 22 字节 + 至多 65535 字节注释）。
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

fn u16le(bytes: &[u8], offset: usize) -> Option<u16> {
    let raw = bytes.get(offset..offset + 2)?;
    Some(u16::from_le_bytes([raw[0], raw[1]]))
}

fn u32le(bytes: &[u8], offset: usize) -> Option<u32> {
    let raw = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

/// 内存解包 zip：返回 条目名 → 解压内容。任何结构损坏返回 Err(描述)。
pub(crate) fn unpack(zip: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let eocd = find_eocd(zip).ok_or("zip eocd not found")?;
    let total_entries = u16le(zip, eocd + 10).ok_or("zip eocd truncated")? as usize;
    let cd_offset = u32le(zip, eocd + 16).ok_or("zip eocd truncated")? as usize;

    let mut entries = BTreeMap::new();
    let mut cursor = cd_offset;
    for _ in 0..total_entries {
        if &zip
            .get(cursor..cursor + 4)
            .ok_or("central directory truncated")?
            != b"PK\x01\x02"
        {
            return Err(format!("bad central directory signature at {cursor:#x}"));
        }
        let method = u16le(zip, cursor + 10).ok_or("central directory truncated")?;
        let compressed_size =
            u32le(zip, cursor + 20).ok_or("central directory truncated")? as usize;
        let uncompressed_size =
            u32le(zip, cursor + 24).ok_or("central directory truncated")? as usize;
        let name_len = u16le(zip, cursor + 28).ok_or("central directory truncated")? as usize;
        let extra_len = u16le(zip, cursor + 30).ok_or("central directory truncated")? as usize;
        let comment_len = u16le(zip, cursor + 32).ok_or("central directory truncated")? as usize;
        let local_offset = u32le(zip, cursor + 42).ok_or("central directory truncated")? as usize;
        let name_raw = zip
            .get(cursor + 46..cursor + 46 + name_len)
            .ok_or("central directory entry name truncated")?;
        let name = String::from_utf8_lossy(name_raw).into_owned();

        // 数据定位走局部头（其名字/扩展字段长度以局部头为准）。
        if &zip
            .get(local_offset..local_offset + 4)
            .ok_or("local header out of range")?
            != b"PK\x03\x04"
        {
            return Err(format!("bad local header signature at {local_offset:#x}"));
        }
        let local_name_len = u16le(zip, local_offset + 26).ok_or("local header truncated")?;
        let local_extra_len = u16le(zip, local_offset + 28).ok_or("local header truncated")?;
        let data_start = local_offset + 30 + local_name_len as usize + local_extra_len as usize;
        let data = zip
            .get(data_start..data_start + compressed_size)
            .ok_or_else(|| format!("zip entry data out of range: {name}"))?;

        let content = match method {
            0 => data.to_vec(),
            8 => {
                let mut out = Vec::with_capacity(uncompressed_size);
                DeflateDecoder::new(data)
                    .read_to_end(&mut out)
                    .map_err(|error| format!("inflate zip entry {name}: {error}"))?;
                out
            }
            other => return Err(format!("unsupported zip method {other} for entry {name}")),
        };
        if content.len() != uncompressed_size {
            return Err(format!("zip entry size mismatch: {name}"));
        }
        entries.insert(name, content);
        cursor += 46 + name_len + extra_len + comment_len;
    }
    Ok(entries)
}
