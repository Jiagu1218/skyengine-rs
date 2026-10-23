//! PNG 解码器（T018）：`javax/microedition/lcdui/Image` 的数据源。
//!
//! 纯 Rust、无 VM 依赖：输入字节 → 输出行优先 `0xAARRGGBB` 像素。
//!
//! 支持面（MIDP-2.0 常用格式）：
//! - 颜色类型 0（灰度，位深 1/2/4/8）、2（真彩 8bit）、
//!   3（调色板 1/2/4/8bit）、6（真彩+alpha 8bit）
//! - Adam7 interlace（MIDP 强制支持）
//! - tRNS：调色板逐项 alpha + 8bit 灰度/真彩色键透明
//! - 行过滤五式：None/Sub/Up/Average/Paeth（按 bpp 回退）
//!
//! 不支持（返回错误，调用方转 IOException 语义）：位深 16、
//! 颜色类型 4（灰度+alpha）及非法颜色类型/位深组合。

use flate2::read::ZlibDecoder;
use std::io::Read;

/// PNG 文件签名。
const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Adam7 七趟扫描的（x 起点, y 起点, x 步进, y 步进）。
const ADAM7: [(usize, usize, usize, usize); 7] = [
    (0, 0, 8, 8),
    (4, 0, 8, 8),
    (0, 4, 4, 8),
    (2, 0, 4, 4),
    (0, 2, 2, 4),
    (1, 0, 2, 2),
    (0, 1, 1, 2),
];

/// 解码结果：尺寸 + 行优先像素（`0xAARRGGBB`，长度 = width * height）。
pub struct Decoded {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u32>,
}

/// 解码错误（静态消息；native 层转 `java/io/IOException`）。
#[derive(Debug)]
pub struct Error(pub &'static str);

type Result<T> = std::result::Result<T, Error>;

/// 颜色类型的每像素通道数（0=灰度 1、2=真彩 3、3=调色板 1、4=灰度α 2、6=真彩α 4）。
fn channels(color_type: u8) -> Option<usize> {
    Some(match color_type {
        0 => 1,
        2 => 3,
        3 => 1,
        4 => 2,
        6 => 4,
        _ => return None,
    })
}

/// 大端 u32。
fn be32(bytes: &[u8]) -> usize {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize
}

/// zlib 解压 IDAT 拼接流。
fn inflate(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    ZlibDecoder::new(data)
        .read_to_end(&mut out)
        .map_err(|_| Error("bad zlib stream in IDAT"))?;
    Ok(out)
}

/// Paeth 预测器（PNG 规范定义）。
fn paeth(a: i32, b: i32, c: i32) -> i32 {
    let p = a + b - c;
    let pa = (p - a).abs();
    let pb = (p - b).abs();
    let pc = (p - c).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// 就地反过滤一行（line 含纯数据字节，不含过滤类型字节）。
/// `bpp` = 每像素字节数向下取整且至少 1（Sub/Average/Paeth 的回退距离）；
/// `prior` = 上一行已反过滤数据（首行为 None，Up 系视作 0）。
fn unfilter_line(filter: u8, line: &mut [u8], bpp: usize, prior: Option<&[u8]>) -> Result<()> {
    let stride = line.len();
    match filter {
        0 => {} // None
        1 => {
            // Sub：加左邻
            for i in bpp..stride {
                line[i] = line[i].wrapping_add(line[i - bpp]);
            }
        }
        2 => {
            // Up：加上邻
            if let Some(p) = prior {
                for i in 0..stride {
                    line[i] = line[i].wrapping_add(p[i]);
                }
            }
        }
        3 => {
            // Average：加 floor((左 + 上) / 2)
            for i in 0..stride {
                let left = if i >= bpp { u16::from(line[i - bpp]) } else { 0 };
                let up = prior.map_or(0u16, |p| u16::from(p[i]));
                line[i] = line[i].wrapping_add(((left + up) / 2) as u8);
            }
        }
        4 => {
            // Paeth
            for i in 0..stride {
                let a = if i >= bpp { i32::from(line[i - bpp]) } else { 0 };
                let b = prior.map_or(0, |p| i32::from(p[i]));
                let c = if i >= bpp { prior.map_or(0, |p| i32::from(p[i - bpp])) } else { 0 };
                line[i] = line[i].wrapping_add(paeth(a, b, c) as u8);
            }
        }
        _ => return Err(Error("bad filter type")),
    }
    Ok(())
}

/// 从一行数据中取第 index 个样本（按位深比特解包；padding 位不被引用）。
fn sample(line: &[u8], index: usize, depth: u8) -> u8 {
    match depth {
        8 => line.get(index).copied().unwrap_or(0),
        4 => {
            let byte = line[index >> 1];
            if index & 1 == 0 {
                byte >> 4
            } else {
                byte & 0x0F
            }
        }
        2 => {
            let byte = line[index >> 2];
            (byte >> (6 - 2 * (index & 3))) & 0x03
        }
        1 => {
            let byte = line[index >> 3];
            (byte >> (7 - (index & 7))) & 0x01
        }
        _ => 0,
    }
}

/// 灰度样本按位深等比放大到 8bit（1bit: 0/255；2bit: 0/85/170/255；4bit: v*17）。
fn scale_gray(value: u8, depth: u8) -> u8 {
    match depth {
        8 => value,
        4 => value * 17,
        2 => value * 85,
        1 => value * 255,
        _ => value,
    }
}

/// 逐 pass/行消费过滤字节 + 数据行，反过滤并展开像素到最终坐标。
// PNG 解码管线上下文参数组（宽高/深度/通道/pass 索引…）为解码器域约束。
#[allow(clippy::too_many_arguments)]
fn unfilter_and_expand(
    decoded_width: usize,
    decoded_height: usize,
    depth: u8,
    color_type: u8,
    interlaced: bool,
    palette: &[[u8; 3]],
    trns_palette: &[u8],
    trns_key: &Option<(u8, u8, u8)>,
    raw: &[u8],
) -> Result<Vec<u32>> {
    let ch = channels(color_type).unwrap_or(1);
    // Sub/Average/Paeth 的字节回退距离：不足一字节按一字节。
    let bpp = (ch * depth as usize).div_ceil(8);
    let bpp = bpp.max(1);
    let mut pixels = vec![0u32; decoded_width * decoded_height];
    let mut cursor = 0usize;

    // 行情（样本数, 目标坐标步进）按是否 interlace 划分：
    // 非 interlace 视作单一 pass（起点 0,0 步进 1,1）。
    let passes: Vec<(usize, usize, usize, usize)> = if interlaced {
        ADAM7.to_vec()
    } else {
        vec![(0, 0, 1, 1)]
    };

    for &(x_start, y_start, x_step, y_step) in &passes {
        let pass_width = if decoded_width > x_start {
            (decoded_width - x_start).div_ceil(x_step)
        } else {
            0
        };
        let pass_height = if decoded_height > y_start {
            (decoded_height - y_start).div_ceil(y_step)
        } else {
            0
        };
        if pass_width == 0 || pass_height == 0 {
            continue; // 空 pass 在流中不占字节
        }
        let stride = (pass_width * ch * depth as usize).div_ceil(8);
        let mut prior: Option<Vec<u8>> = None;
        for row in 0..pass_height {
            // 过滤类型字节 + 行数据
            if cursor + 1 + stride > raw.len() {
                return Err(Error("truncated image data"));
            }
            let filter = raw[cursor];
            let mut line = raw[cursor + 1..cursor + 1 + stride].to_vec();
            cursor += 1 + stride;
            unfilter_line(filter, &mut line, bpp, prior.as_deref())?;
            for i in 0..pass_width {
                let x = x_start + i * x_step;
                let y = y_start + row * y_step;
                pixels[y * decoded_width + x] = compose_pixel(
                    &line,
                    i,
                    depth,
                    color_type,
                    palette,
                    trns_palette,
                    trns_key,
                )?;
            }
            prior = Some(line);
        }
    }
    Ok(pixels)
}

/// 单像素合成 `0xAARRGGBB`（调色板索引越界按坏数据处理）。
fn compose_pixel(
    line: &[u8],
    index: usize,
    depth: u8,
    color_type: u8,
    palette: &[[u8; 3]],
    trns_palette: &[u8],
    trns_key: &Option<(u8, u8, u8)>,
) -> Result<u32> {
    let rgba: [u8; 4] = match color_type {
        0 => {
            // 灰度；tRNS 色键仅 8bit 下生效（低位深键值域不同，忽略）
            let raw_sample = sample(line, index, depth);
            let gray = scale_gray(raw_sample, depth);
            let alpha = match (trns_key, depth) {
                (Some((kr, _, _)), 8) if *kr == raw_sample => 0,
                _ => 0xFF,
            };
            [gray, gray, gray, alpha]
        }
        2 => {
            // 真彩：R G B
            let base = index * 3;
            let (r, g, b) = (line[base], line[base + 1], line[base + 2]);
            let alpha = match trns_key {
                Some((kr, kg, kb)) if *kr == r && *kg == g && *kb == b => 0,
                _ => 0xFF,
            };
            [r, g, b, alpha]
        }
        3 => {
            // 调色板：样本为索引
            let idx = sample(line, index, depth) as usize;
            let [r, g, b] = palette
                .get(idx)
                .copied()
                .ok_or(Error("palette index out of range"))?;
            let a = trns_palette.get(idx).copied().unwrap_or(0xFF);
            [r, g, b, a]
        }
        _ => {
            // 真彩+alpha：R G B A
            let base = index * 4;
            [line[base], line[base + 1], line[base + 2], line[base + 3]]
        }
    };
    Ok((u32::from(rgba[3]) << 24)
        | (u32::from(rgba[0]) << 16)
        | (u32::from(rgba[1]) << 8)
        | u32::from(rgba[2]))
}

/// 解码 PNG 字节流。
pub fn decode(bytes: &[u8]) -> Result<Decoded> {
    if bytes.len() < 8 || bytes[..8] != SIGNATURE {
        return Err(Error("bad png signature"));
    }

    let mut width = 0usize;
    let mut height = 0usize;
    let mut depth = 0u8;
    let mut color_type = 0u8;
    let mut interlace = 0u8;
    let mut have_ihdr = false;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut trns_palette: Vec<u8> = Vec::new();
    let mut trns_key: Option<(u8, u8, u8)> = None;
    let mut idat: Vec<u8> = Vec::new();

    let mut pos = 8usize;
    while pos + 8 <= bytes.len() {
        let length = be32(&bytes[pos..pos + 4]);
        let tag = &bytes[pos + 4..pos + 8];
        let data_start = pos + 8;
        let Some(data_end) = data_start.checked_add(length) else {
            return Err(Error("bad chunk length"));
        };
        if data_end + 4 > bytes.len() {
            return Err(Error("truncated chunk"));
        }
        let data = &bytes[data_start..data_end];
        match tag {
            b"IHDR" => {
                if data.len() != 13 {
                    return Err(Error("bad IHDR"));
                }
                width = be32(&data[0..4]);
                height = be32(&data[4..8]);
                depth = data[8];
                color_type = data[9];
                if data[10] != 0 || data[11] != 0 {
                    return Err(Error("unsupported compression/filter method"));
                }
                interlace = data[12];
                if interlace > 1 {
                    return Err(Error("bad interlace method"));
                }
                have_ihdr = true;
            }
            b"PLTE" => {
                if data.is_empty() || !data.len().is_multiple_of(3) {
                    return Err(Error("bad PLTE"));
                }
                palette = data.chunks(3).map(|c| [c[0], c[1], c[2]]).collect();
            }
            b"tRNS" => match color_type {
                3 => trns_palette = data.to_vec(),
                0 => {
                    // 16bit 灰度键取低字节（8bit 位深下即样本值）
                    if data.len() >= 2 {
                        trns_key = Some((data[1], data[1], data[1]));
                    }
                }
                2
                    if data.len() >= 6 => {
                        trns_key = Some((data[1], data[3], data[5]));
                    }
                _ => {}
            },
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => break,
            _ => {} // 辅助块跳过（CRC 一律不校验）
        }
        pos = data_end + 4;
    }

    if !have_ihdr || width == 0 || height == 0 {
        return Err(Error("missing IHDR"));
    }

    // 支持面检查（16bit / 灰度+alpha 明确拒绝）。
    if depth == 16 {
        return Err(Error("16-bit depth unsupported"));
    }
    match color_type {
        0 => {
            if !matches!(depth, 1 | 2 | 4 | 8) {
                return Err(Error("bad gray depth"));
            }
        }
        2 | 6 => {
            if depth != 8 {
                return Err(Error("truecolor requires 8-bit depth"));
            }
        }
        3 => {
            if !matches!(depth, 1 | 2 | 4 | 8) {
                return Err(Error("bad palette depth"));
            }
            if palette.is_empty() {
                return Err(Error("palette image without PLTE"));
            }
        }
        4 => return Err(Error("grayscale+alpha unsupported")),
        _ => return Err(Error("bad color type")),
    }

    let raw = inflate(&idat)?;
    let pixels = unfilter_and_expand(
        width,
        height,
        depth,
        color_type,
        interlace == 1,
        &palette,
        &trns_palette,
        &trns_key,
        &raw,
    )?;
    Ok(Decoded {
        width,
        height,
        pixels,
    })
}
