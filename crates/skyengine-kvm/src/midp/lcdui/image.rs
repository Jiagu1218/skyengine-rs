//! `javax/microedition/lcdui/Image` 原生实现（T018）。
//!
//! 对象模型（bootstrap DECL 注册实例字段，Throwable.detailMessage 先例）：
//! - `width I` / `height I`：尺寸
//! - `pixels [I`：堆上 int 数组引用，`0xAARRGGBB` 每像素一个 int、
//!   行优先。经实例字段被 GC 可达，无 native 侧悬挂表；
//!   后续 Graphics.drawImage（T020）直接取该数组绘制。
//! - `mutable Z`：isMutable 语义位（仅 createImage(II) 可变空图为 true）
//!
//! createImage 全套重载（MIDP-2.0）：String 资源名 / byte[] 区段 /
//! 源拷贝 / 可变空图 / 区域裁剪 / createRGBImage / InputStream。
//! 资源数据源为类空间 `suite_resources`（install_jar 填充）。

use crate::KvmError;
use crate::cldc::io as jio;
use crate::cldc::lang as jl;
use crate::host::HostContext;
use crate::interp::NativeCall;
use crate::midp::lcdui::font;
use crate::midp::lcdui::graphics;
use crate::midp::lcdui::png;
use crate::runtime::classspace::ClassSpace;
use crate::runtime::heap::Heap;
use crate::runtime::object::{ArrayData, ElemType};
use crate::runtime::{ObjRef, Slot};

/// Image 内部名。
const CLASS: &str = "javax/microedition/lcdui/Image";

/// Image 实例字段槽位（每次调用现查；与 cldc::io 的 field_slot 同模式）。
struct ImageSlots {
    width: usize,
    height: usize,
    pixels: usize,
    mutable: usize,
}

fn image_slots(cs: &mut ClassSpace) -> Result<ImageSlots, KvmError> {
    let class = cs.load_class(CLASS)?;
    let slot = |cs: &mut ClassSpace, name: &str, desc: &str| -> Result<usize, KvmError> {
        let name = cs.syms.intern(name);
        let desc = cs.syms.intern(desc);
        Ok(cs.find_field(class, name, desc)?.slot as usize)
    };
    Ok(ImageSlots {
        width: slot(cs, "width", "I")?,
        height: slot(cs, "height", "I")?,
        pixels: slot(cs, "pixels", "[I")?,
        mutable: slot(cs, "mutable", "Z")?,
    })
}

/// 抛 `java/io/IOException`（PNG 解码/资源缺失语义）。
fn io_exception(cs: &mut ClassSpace, heap: &mut Heap, message: &str) -> KvmError {
    jl::throw_java(cs, heap, "java/io/IOException", Some(message))
}

/// 抛 `java/lang/IllegalArgumentException`。
fn illegal_arg(cs: &mut ClassSpace, heap: &mut Heap, message: &str) -> KvmError {
    jl::throw_java(
        cs,
        heap,
        "java/lang/IllegalArgumentException",
        Some(message),
    )
}

fn npe(cs: &mut ClassSpace, heap: &mut Heap) -> KvmError {
    jl::throw_java(cs, heap, "java/lang/NullPointerException", None)
}

/// 实参提取（描述符序；解释器已按描述符对齐类型）。
fn arg_ref(call: &NativeCall, index: usize) -> ObjRef {
    match call.args[index] {
        Slot::Ref(r) => r,
        _ => ObjRef::NULL,
    }
}

fn arg_i32(call: &NativeCall, index: usize) -> Result<i32, KvmError> {
    match call.args.get(index) {
        Some(Slot::Int(v)) => Ok(*v),
        other => Err(KvmError::Internal(format!(
            "native arg {index} expected int, got {other:?}"
        ))),
    }
}

fn arg_int(call: &NativeCall, index: usize) -> i32 {
    match call.args[index] {
        Slot::Int(v) => v,
        _ => 0,
    }
}

/// 分配 Image 实例：像素写入新建 `[I` 数组并挂到实例字段。
fn alloc_image(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    width: i32,
    height: i32,
    pixels: Vec<u32>,
    mutable: bool,
) -> Result<ObjRef, KvmError> {
    let slots = image_slots(cs)?;
    let class = cs.load_class(CLASS)?;
    let template = cs.class(class).instance_template.clone();
    let image = heap.alloc_instance(class, template)?;
    let array_class = cs.load_class("[I")?;
    let array = heap.alloc_array(array_class, ElemType::Int, pixels.len())?;
    if let ArrayData::Int(data) = &mut heap.array_mut(array)?.data {
        for (dst, src) in data.iter_mut().zip(pixels) {
            *dst = src as i32;
        }
    }
    let fields = &mut heap.instance_mut(image)?.fields;
    fields[slots.width] = Slot::Int(width);
    fields[slots.height] = Slot::Int(height);
    fields[slots.pixels] = Slot::Ref(array);
    fields[slots.mutable] = Slot::Int(i32::from(mutable));
    Ok(image)
}

/// 源图数据快照（宽、高、像素、可变位）。pub 化供 Graphics 绘制复用（T020）。
pub struct SourceImage {
    pub width: i32,
    pub height: i32,
    pub pixels: Vec<u32>,
}

// ---- T021 Canvas 屏图数据面（复用可变空图分配路径） -----------------------

/// 分配可变空图（全零像素）：Canvas 屏图懒分配复用的同一路径
///（与 `createImage(II)` 原生共用 alloc_image）。
pub fn alloc_mutable_image(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    width: i32,
    height: i32,
) -> Result<ObjRef, KvmError> {
    if width < 1 || height < 1 {
        return Err(illegal_arg(cs, heap, "image size < 1"));
    }
    alloc_image(
        cs,
        heap,
        width,
        height,
        vec![0u32; (width as usize) * (height as usize)],
        true,
    )
}

/// 读 Image 实例宽（字段直读；Canvas.getWidth/屏呈现共用）。
pub fn image_width(cs: &mut ClassSpace, heap: &Heap, image: ObjRef) -> Result<i32, KvmError> {
    let slots = image_slots(cs)?;
    match heap.instance(image)?.fields[slots.width] {
        Slot::Int(v) => Ok(v),
        _ => Err(KvmError::Internal("Image.width corrupt".into())),
    }
}

/// 读 Image 实例高。
pub fn image_height(cs: &mut ClassSpace, heap: &Heap, image: ObjRef) -> Result<i32, KvmError> {
    let slots = image_slots(cs)?;
    match heap.instance(image)?.fields[slots.height] {
        Slot::Int(v) => Ok(v),
        _ => Err(KvmError::Internal("Image.height corrupt".into())),
    }
}

/// `getRGB([IIIIIII)V`：把实例 [x,y,w,h] 区域像素（0xAARRGGBB）写入
/// rgbData（offset 起、行距 scanlength；scanlength≤0 按 MIDP 宽容——
/// 数组访问本身按公式进行，越界抛 ArrayIndexOutOfBoundsException）。
/// 冒烟实证：游戏位图字库经此取像素切字，空桩致方块。
pub fn image_get_rgb(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = match call.this {
        Some(r) => r,
        None => return Err(KvmError::Internal("getRGB without receiver".into())),
    };
    let arr = arg_ref(call, 0);
    if arr == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let (offset, scan, x, y, w, h) = (
        arg_i32(call, 1)?,
        arg_i32(call, 2)?,
        arg_i32(call, 3)?,
        arg_i32(call, 4)?,
        arg_i32(call, 5)?,
        arg_i32(call, 6)?,
    );
    let img_w = image_width(cs, heap, this)?;
    let img_h = image_height(cs, heap, this)?;
    let pixels = pixels_array_ref(cs, heap, this)?;
    let src: Vec<i32> = match &heap.array(pixels)?.data {
        crate::runtime::object::ArrayData::Int(data) => data.clone(),
        _ => return Err(KvmError::Internal("Image pixels array corrupt".into())),
    };
    let dst_len = match &heap.array(arr)?.data {
        crate::runtime::object::ArrayData::Int(data) => data.len(),
        _ => return Err(KvmError::Internal("getRGB arg 0 expected int[]".into())),
    };
    // 越界前置校验（MIDP：参数非法抛；这里按数组访问语义统一 AIOOBE）。
    let need: i64 = if w <= 0 || h <= 0 {
        0
    } else {
        i64::from(offset) + (i64::from(h) - 1) * i64::from(scan) + i64::from(w)
    };
    if offset < 0 || need < 0 || need > dst_len as i64 {
        return Err(jl::throw_java(
            cs,
            heap,
            "java/lang/ArrayIndexOutOfBoundsException",
            Some("getRGB range exceeds rgbData"),
        ));
    }
    let out = match &mut heap.array_mut(arr)?.data {
        crate::runtime::object::ArrayData::Int(data) => data,
        _ => return Err(KvmError::Internal("getRGB arg 0 expected int[]".into())),
    };
    for row in 0..h.max(0) as usize {
        for col in 0..w.max(0) as usize {
            let sx = x as usize + col;
            let sy = y as usize + row;
            let v = if sx < img_w.max(0) as usize && sy < img_h.max(0) as usize {
                src[sy * img_w.max(0) as usize + sx]
            } else {
                0
            };
            out[offset as usize + row * scan.max(0) as usize + col] = v;
        }
    }
    Ok(None)
}

/// 取 Image 像素 `[I` 数组引用（Canvas paint 的 Graphics 绑定目标）。
pub fn pixels_array_ref(
    cs: &mut ClassSpace,
    heap: &Heap,
    image: ObjRef,
) -> Result<ObjRef, KvmError> {
    let slots = image_slots(cs)?;
    match heap.instance(image)?.fields[slots.pixels] {
        Slot::Ref(r) => Ok(r),
        _ => Err(KvmError::Internal("Image.pixels corrupt".into())),
    }
}

/// 取 Image 像素 `[I` 数组借用（Canvas 屏呈现快照）。
pub fn pixels_of<'a>(
    cs: &mut ClassSpace,
    heap: &'a Heap,
    image: ObjRef,
) -> Result<&'a [i32], KvmError> {
    let slots = image_slots(cs)?;
    let array = match heap.instance(image)?.fields[slots.pixels] {
        Slot::Ref(r) => r,
        _ => return Err(KvmError::Internal("Image.pixels corrupt".into())),
    };
    match &heap.array(array)?.data {
        ArrayData::Int(data) => Ok(data),
        _ => Err(KvmError::Internal("Image pixels array corrupt".into())),
    }
}

pub fn read_source(
    cs: &mut ClassSpace,
    heap: &Heap,
    image: ObjRef,
) -> Result<SourceImage, KvmError> {
    let slots = image_slots(cs)?;
    let fields = &heap.instance(image)?.fields;
    let (width, height) = match (fields[slots.width], fields[slots.height]) {
        (Slot::Int(w), Slot::Int(h)) => (w, h),
        _ => return Err(KvmError::Internal("Image size field corrupt".into())),
    };
    let array = match fields[slots.pixels] {
        Slot::Ref(r) => r,
        _ => return Err(KvmError::Internal("Image pixels field corrupt".into())),
    };
    let pixels = match &heap.array(array)?.data {
        ArrayData::Int(data) => data.iter().map(|&v| v as u32).collect(),
        _ => return Err(KvmError::Internal("Image pixels array corrupt".into())),
    };
    Ok(SourceImage {
        width,
        height,
        pixels,
    })
}

/// 零拷贝借源：返回 (width, height, 像素数组引用)——不借用堆、
/// 不拷贝像素，供渲染直通路径后续做拆分借用（int_arrays_src_dst）。
pub fn source_dims_array(
    cs: &mut ClassSpace,
    heap: &Heap,
    image: ObjRef,
) -> Result<(i32, i32, ObjRef), KvmError> {
    let slots = image_slots(cs)?;
    let fields = &heap.instance(image)?.fields;
    let (width, height) = match (fields[slots.width], fields[slots.height]) {
        (Slot::Int(w), Slot::Int(h)) => (w, h),
        _ => return Err(KvmError::Internal("Image size field corrupt".into())),
    };
    let array = match fields[slots.pixels] {
        Slot::Ref(r) => r,
        _ => return Err(KvmError::Internal("Image pixels field corrupt".into())),
    };
    Ok((width, height, array))
}

/// 字符串实参 → Rust String（UTF-16 解码）。
fn string_arg(heap: &Heap, reference: ObjRef) -> Result<String, KvmError> {
    Ok(String::from_utf16_lossy(&heap.str_obj(reference)?.chars))
}

// ---- createImage 静态重载 --------------------------------------------------

/// `createImage(Ljava/lang/String;)`：套件资源加载。
/// 资源键先去前导 '/'（"/skyicon.png" → "skyicon.png"），再试原键。
pub fn create_image_string(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let name_ref = arg_ref(call, 0);
    if name_ref.is_null() {
        return Err(npe(cs, heap));
    }
    let name = string_arg(heap, name_ref)?;
    let stripped = name.strip_prefix('/').unwrap_or(&name).to_string();
    // Rc 浅拷贝（数据本体不复制），解码期不占 cs/heap 借用。
    let data = cs
        .suite_resources
        .get(&stripped)
        .or_else(|| cs.suite_resources.get(&name))
        .cloned();
    let Some(data) = data else {
        return Err(io_exception(
            cs,
            heap,
            &format!("resource not found: {name}"),
        ));
    };
    let decoded = png::decode(&data).map_err(|e| io_exception(cs, heap, e.0))?;
    let image = alloc_image(
        cs,
        heap,
        decoded.width as i32,
        decoded.height as i32,
        decoded.pixels,
        false,
    )?;
    Ok(Some(Slot::Ref(image)))
}

/// `createImage([BII)`：字节数组区段解码。
pub fn create_image_bytes(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let array = arg_ref(call, 0);
    if array.is_null() {
        return Err(npe(cs, heap));
    }
    let offset = arg_int(call, 1);
    let length = arg_int(call, 2);
    let bytes: Vec<u8> = match &heap.array(array)?.data {
        ArrayData::Byte(data) => data.iter().map(|&b| b as u8).collect(),
        _ => return Err(illegal_arg(cs, heap, "image data must be byte[]")),
    };
    if offset < 0 || length < 0 || offset as usize + length as usize > bytes.len() {
        return Err(jl::throw_java(
            cs,
            heap,
            "java/lang/ArrayIndexOutOfBoundsException",
            Some("image byte range"),
        ));
    }
    let decoded = png::decode(&bytes[offset as usize..offset as usize + length as usize])
        .map_err(|e| io_exception(cs, heap, e.0))?;
    let image = alloc_image(
        cs,
        heap,
        decoded.width as i32,
        decoded.height as i32,
        decoded.pixels,
        false,
    )?;
    Ok(Some(Slot::Ref(image)))
}

/// `createImage(Ljavax/microedition/lcdui/Image;)`：源拷贝（不可变副本）。
pub fn create_image_source(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let source = arg_ref(call, 0);
    if source.is_null() {
        return Err(npe(cs, heap));
    }
    let src = read_source(cs, heap, source)?;
    let image = alloc_image(cs, heap, src.width, src.height, src.pixels, false)?;
    Ok(Some(Slot::Ref(image)))
}

/// `createImage(II)`：可变空图（全 0 像素 = 透明黑）。
pub fn create_image_empty(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let width = arg_int(call, 0);
    let height = arg_int(call, 1);
    if width < 1 || height < 1 {
        return Err(illegal_arg(cs, heap, "image size < 1"));
    }
    let image = alloc_image(
        cs,
        heap,
        width,
        height,
        vec![0u32; (width as usize) * (height as usize)],
        true,
    )?;
    Ok(Some(Slot::Ref(image)))
}

/// `createImage(Ljavax/microedition/lcdui/Image;IIII)`：区域裁剪副本。
/// 区域负尺寸或越出源图 → IllegalArgumentException（MIDP javadoc）。
pub fn create_image_region(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let source = arg_ref(call, 0);
    if source.is_null() {
        return Err(npe(cs, heap));
    }
    let x = arg_int(call, 1);
    let y = arg_int(call, 2);
    let width = arg_int(call, 3);
    let height = arg_int(call, 4);
    let src = read_source(cs, heap, source)?;
    if width < 0 || height < 0 {
        return Err(illegal_arg(cs, heap, "region size < 0"));
    }
    if x < 0 || y < 0 || x + width > src.width || y + height > src.height {
        return Err(illegal_arg(cs, heap, "region outside source"));
    }
    let sw = src.width as usize;
    let mut pixels = Vec::with_capacity((width as usize) * (height as usize));
    for row in y..y + height {
        let base = row as usize * sw + x as usize;
        pixels.extend_from_slice(&src.pixels[base..base + width as usize]);
    }
    let image = alloc_image(cs, heap, width, height, pixels, false)?;
    Ok(Some(Slot::Ref(image)))
}

/// `createRGBImage([IIIZ)`：int 像素数组直接建图
///（rgbData, width, height, processAlpha——无 offset 参数，MIDP javadoc）；
/// `processAlpha == false` 时 alpha 统一置 0xFF（不透明）。
pub fn create_rgb_image(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let array = arg_ref(call, 0);
    if array.is_null() {
        return Err(npe(cs, heap));
    }
    let width = arg_int(call, 1);
    let height = arg_int(call, 2);
    let process_alpha = arg_int(call, 3) != 0;
    if width < 1 || height < 1 {
        return Err(illegal_arg(cs, heap, "image size < 1"));
    }
    let source: Vec<u32> = match &heap.array(array)?.data {
        ArrayData::Int(data) => data.iter().map(|&v| v as u32).collect(),
        _ => return Err(illegal_arg(cs, heap, "rgbData must be int[]")),
    };
    let needed = (width as usize) * (height as usize);
    if needed > source.len() {
        return Err(jl::throw_java(
            cs,
            heap,
            "java/lang/ArrayIndexOutOfBoundsException",
            Some("rgbData overflow"),
        ));
    }
    let pixels: Vec<u32> = source[..needed]
        .iter()
        .map(|&p| if process_alpha { p } else { p | 0xFF00_0000 })
        .collect();
    let image = alloc_image(cs, heap, width, height, pixels, false)?;
    Ok(Some(Slot::Ref(image)))
}

/// `createImage(Ljava/io/InputStream;)`：读尽流后按字节区段解码。
pub fn create_image_stream(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let stream = arg_ref(call, 0);
    if stream.is_null() {
        return Err(npe(cs, heap));
    }
    // 复用 cldc::io 的按运行时类递归读取通道（BAIS/DataInputStream 等）。
    let bytes = jio::input_read_to_vec(cs, heap, stream)
        .map_err(|_| io_exception(cs, heap, "stream read failure"))?;
    let decoded = png::decode(&bytes).map_err(|e| io_exception(cs, heap, e.0))?;
    let image = alloc_image(
        cs,
        heap,
        decoded.width as i32,
        decoded.height as i32,
        decoded.pixels,
        false,
    )?;
    Ok(Some(Slot::Ref(image)))
}

// ---- 实例方法 ---------------------------------------------------------------

/// `getWidth()I`。
pub fn get_width(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = call.this.unwrap_or(ObjRef::NULL);
    let slots = image_slots(cs)?;
    match heap.instance(this)?.fields[slots.width] {
        Slot::Int(v) => Ok(Some(Slot::Int(v))),
        _ => Err(KvmError::Internal("Image.width corrupt".into())),
    }
}

/// `getHeight()I`。
pub fn get_height(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = call.this.unwrap_or(ObjRef::NULL);
    let slots = image_slots(cs)?;
    match heap.instance(this)?.fields[slots.height] {
        Slot::Int(v) => Ok(Some(Slot::Int(v))),
        _ => Err(KvmError::Internal("Image.height corrupt".into())),
    }
}

/// `isMutable()Z`：仅 createImage(II) 产物为 true。
pub fn is_mutable(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = call.this.unwrap_or(ObjRef::NULL);
    let slots = image_slots(cs)?;
    match heap.instance(this)?.fields[slots.mutable] {
        Slot::Int(v) => Ok(Some(Slot::Int(v))),
        _ => Err(KvmError::Internal("Image.mutable corrupt".into())),
    }
}

/// `getGraphics()`：仅可变图（不可变抛 IllegalStateException）；
/// 分配 Graphics 绑定本图 width/height/pixels，clip=全图、font=默认
/// （T020；构造路径唯一，T021 Canvas 屏缓冲经 graphics::alloc_graphics 复用）。
pub fn get_graphics(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let this = call.this.unwrap_or(ObjRef::NULL);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let slots = image_slots(cs)?;
    // 先取值后判（借用即刻释放，异常构造要再借 heap）。
    let (width, height, pixels, mutable) = {
        let fields = &heap.instance(this)?.fields;
        let w = match fields[slots.width] {
            Slot::Int(v) => v,
            _ => return Err(KvmError::Internal("Image.width corrupt".into())),
        };
        let h = match fields[slots.height] {
            Slot::Int(v) => v,
            _ => return Err(KvmError::Internal("Image.height corrupt".into())),
        };
        let arr = match fields[slots.pixels] {
            Slot::Ref(r) => r,
            _ => return Err(KvmError::Internal("Image.pixels corrupt".into())),
        };
        let m = match fields[slots.mutable] {
            Slot::Int(v) => v,
            _ => 0,
        };
        (w, h, arr, m)
    };
    if mutable == 0 {
        return Err(jl::throw_java(
            cs,
            heap,
            "java/lang/IllegalStateException",
            Some("immutable image"),
        ));
    }
    let font = font::default_font(cs, heap, host)?;
    let g = graphics::alloc_graphics(cs, heap, pixels, width, height, font)?;
    Ok(Some(Slot::Ref(g)))
}
