//! `javax/microedition/lcdui/Font` 原生实现（T019，Host 轨）。
//!
//! 对象模型（bootstrap DECL 注册实例字段）：
//! - `face/style/size I`：MIDP 常量原值（FACE_*/STYLE_*/SIZE_*），
//!   getFace/getStyle/getSize 直接回读
//! - `handle I`：HostContext `FontHandle` 快照（u32 窄化 i32），
//!   度量/测宽经 host 实时查询（宿主字体可运行期变化，不做本地缓存）
//!
//! 归一化：MIDP 三元组 → host 值域 (face 0-2, style 0-7, size 0-2)：
//! face SYSTEM/PROPORTIONAL/MONOSPACE(0/16/32) → 0/1/2；
//! style 位组合原样透传；size SMALL/MEDIUM/LARGE(8/16/32) → 0/1/2。
//! 同归一化三元组经 ClassSpace.font_cache 返回同实例
//!（Font 无公开构造、状态不可变，共享安全）。

use crate::cldc::lang as jl;
use crate::host::{FontHandle, HostContext};
use crate::interp::NativeCall;
use crate::runtime::classspace::ClassSpace;
use crate::runtime::heap::Heap;
use crate::runtime::object::ArrayData;
use crate::runtime::{ObjRef, Slot};
use crate::KvmError;

/// Font 内部名。
const CLASS: &str = "javax/microedition/lcdui/Font";

/// MIDP 常量（与 DECL 静态常量一致；此处供归一化/校验）。
const FACE_SYSTEM: i32 = 0;
const FACE_PROPORTIONAL: i32 = 16;
const FACE_MONOSPACE: i32 = 32;
const SIZE_SMALL: i32 = 8;
const SIZE_MEDIUM: i32 = 16;
const SIZE_LARGE: i32 = 32;
/// style 合法位掩码（PLAIN|BOLD|ITALIC|UNDERLINED）。
const STYLE_MASK: i32 = 0b111;

/// Font 实例字段槽位（每次调用现查；与 cldc::io 的 field_slot 同模式）。
struct FontSlots {
    face: usize,
    style: usize,
    size: usize,
    handle: usize,
}

fn font_slots(cs: &mut ClassSpace) -> Result<FontSlots, KvmError> {
    let class = cs.load_class(CLASS)?;
    let slot = |cs: &mut ClassSpace, name: &str| -> Result<usize, KvmError> {
        let name = cs.syms.intern(name);
        let desc = cs.syms.intern("I");
        Ok(cs.find_field(class, name, desc)?.slot as usize)
    };
    Ok(FontSlots {
        face: slot(cs, "face")?,
        style: slot(cs, "style")?,
        size: slot(cs, "size")?,
        handle: slot(cs, "handle")?,
    })
}

/// 抛 `java/lang/NullPointerException`。
fn npe(cs: &mut ClassSpace, heap: &mut Heap) -> KvmError {
    jl::throw_java(cs, heap, "java/lang/NullPointerException", None)
}

/// 抛 `java/lang/IllegalArgumentException`。
fn illegal_arg(cs: &mut ClassSpace, heap: &mut Heap, message: &str) -> KvmError {
    jl::throw_java(cs, heap, "java/lang/IllegalArgumentException", Some(message))
}

/// MIDP 三元组归一化为 host 值域；非法值 → IllegalArgumentException。
fn normalize(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    face: i32,
    style: i32,
    size: i32,
) -> Result<(u8, u8, u8), KvmError> {
    let host_face = match face {
        FACE_SYSTEM => 0,
        FACE_PROPORTIONAL => 1,
        FACE_MONOSPACE => 2,
        _ => return Err(illegal_arg(cs, heap, "invalid face")),
    };
    if style < 0 || (style & !STYLE_MASK) != 0 {
        return Err(illegal_arg(cs, heap, "invalid style"));
    }
    let host_size = match size {
        SIZE_SMALL => 0,
        SIZE_MEDIUM => 1,
        SIZE_LARGE => 2,
        _ => return Err(illegal_arg(cs, heap, "invalid size")),
    };
    Ok((host_face, style as u8, host_size))
}

/// 按已归一化三元组取/建 Font 实例（font_cache 命中直接复用）。
/// face/style/size 为回填进实例字段的 MIDP 原值。
fn create_cached(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    key: (u8, u8, u8),
    face: i32,
    style: i32,
    size: i32,
) -> Result<ObjRef, KvmError> {
    if let Some(existing) = cs.font_cache.get(&key) {
        return Ok(*existing);
    }
    // 先向 host 建字体取句柄，再分配对象（失败时无残留对象）。
    let handle = host.font_query(key.0, key.1, key.2);
    let class = cs.load_class(CLASS)?;
    let template = cs.class(class).instance_template.clone();
    let font = heap.alloc_instance(class, template)?;
    let slots = font_slots(cs)?;
    let fields = &mut heap.instance_mut(font)?.fields;
    fields[slots.face] = Slot::Int(face);
    fields[slots.style] = Slot::Int(style);
    fields[slots.size] = Slot::Int(size);
    fields[slots.handle] = Slot::Int(handle.0 as i32);
    cs.font_cache.insert(key, font);
    Ok(font)
}

/// 按 MIDP 三元组取/建 Font（归一化 + 值域校验后再走缓存建对象）。
fn get_or_create(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    face: i32,
    style: i32,
    size: i32,
) -> Result<ObjRef, KvmError> {
    let key = normalize(cs, heap, face, style, size)?;
    create_cached(cs, heap, host, key, face, style, size)
}

/// 读 this 的 handle 字段为 FontHandle；this 为 null 时抛 NPE。
fn this_handle(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
) -> Result<FontHandle, KvmError> {
    let this = call.this.unwrap_or(ObjRef::NULL);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let slots = font_slots(cs)?;
    match heap.instance(this)?.fields[slots.handle] {
        Slot::Int(v) => Ok(FontHandle(v as u32)),
        _ => Err(KvmError::Internal("Font.handle corrupt".into())),
    }
}

/// 读 this 的指定 int 字段；this 为 null 时抛 NPE。
fn this_int_field(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
    name: &str,
) -> Result<i32, KvmError> {
    let this = call.this.unwrap_or(ObjRef::NULL);
    if this == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let slots = font_slots(cs)?;
    let slot = match name {
        "face" => slots.face,
        "style" => slots.style,
        "size" => slots.size,
        _ => return Err(KvmError::Internal("unknown Font field".into())),
    };
    match heap.instance(this)?.fields[slot] {
        Slot::Int(v) => Ok(v),
        _ => Err(KvmError::Internal("Font field corrupt".into())),
    }
}

// ---- 静态方法 ---------------------------------------------------------------

/// 默认字体实例（Graphics 构造/setFont(null) 复用；缓存键 (0,0,0)）。
pub fn default_font(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
) -> Result<ObjRef, KvmError> {
    create_cached(cs, heap, host, (0, 0, 0), 0, 0, 0)
}

/// 读 Font 实例的 host 句柄（Graphics 文本绘制/度量复用）；
/// 实例为 null 时抛 NPE（防御路径，正常不达）。
pub fn handle_of(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    font: ObjRef,
) -> Result<FontHandle, KvmError> {
    if font == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let slots = font_slots(cs)?;
    match heap.instance(font)?.fields[slots.handle] {
        Slot::Int(v) => Ok(FontHandle(v as u32)),
        _ => Err(KvmError::Internal("Font.handle corrupt".into())),
    }
}

/// `getDefaultFont()LFont;`：归一化 (0,0,0)（SYSTEM/PLAIN/SMALL 档）。
pub fn get_default_font(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    // 默认档直接用归一化键 (0,0,0)（0 非 MIDP SIZE 常量、不走值域校验）。
    let font = default_font(cs, heap, host)?;
    Ok(Some(Slot::Ref(font)))
}

/// `getFont(III)LFont;`：三元组各取值域，非法抛 IllegalArgumentException。
pub fn get_font(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let face = match call.args[0] {
        Slot::Int(v) => v,
        _ => return Err(KvmError::Internal("getFont face arg corrupt".into())),
    };
    let style = match call.args[1] {
        Slot::Int(v) => v,
        _ => return Err(KvmError::Internal("getFont style arg corrupt".into())),
    };
    let size = match call.args[2] {
        Slot::Int(v) => v,
        _ => return Err(KvmError::Internal("getFont size arg corrupt".into())),
    };
    let font = get_or_create(cs, heap, host, face, style, size)?;
    Ok(Some(Slot::Ref(font)))
}

// ---- 度量（host 实时查询） --------------------------------------------------

/// `getHeight()I`。
pub fn get_height(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let handle = this_handle(cs, heap, call)?;
    Ok(Some(Slot::Int(host.font_metrics(handle).height as i32)))
}

/// `getBaselinePosition()I`：基线以上高度（ascent）。
pub fn get_baseline_position(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let handle = this_handle(cs, heap, call)?;
    Ok(Some(Slot::Int(host.font_metrics(handle).ascent as i32)))
}

/// `stringWidth(Ljava/lang/String;)I`：null 抛 NPE。
pub fn string_width(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let handle = this_handle(cs, heap, call)?;
    let text = match call.args[0] {
        Slot::Ref(r) => r,
        _ => return Err(npe(cs, heap)),
    };
    if text == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let chars = heap.str_obj(text)?.chars.clone();
    Ok(Some(Slot::Int(host.text_width(handle, &chars) as i32)))
}

/// `substringWidth(Ljava/lang/String;II)I`：
/// 越界抛 `StringIndexOutOfBoundsException`（MIDP javadoc）。
pub fn substring_width(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let handle = this_handle(cs, heap, call)?;
    let text = match call.args[0] {
        Slot::Ref(r) => r,
        _ => return Err(npe(cs, heap)),
    };
    if text == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let (offset, len) = match (&call.args[1], &call.args[2]) {
        (Slot::Int(o), Slot::Int(l)) => (*o, *l),
        _ => return Err(KvmError::Internal("substringWidth args corrupt".into())),
    };
    // 先借后判：越界布尔先算好（避免与 throw 的可变借用交错）。
    let chars = &heap.str_obj(text)?.chars;
    let bad = offset < 0
        || len < 0
        || match offset.checked_add(len) {
            Some(end) => end as usize > chars.len(),
            None => true,
        };
    if bad {
        return Err(jl::throw_java(
            cs,
            heap,
            "java/lang/StringIndexOutOfBoundsException",
            None,
        ));
    }
    let end = (offset + len) as usize;
    let sub = &chars[offset as usize..end];
    Ok(Some(Slot::Int(host.text_width(handle, sub) as i32)))
}

/// `charWidth(C)I`：单字符测宽。
pub fn char_width(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let handle = this_handle(cs, heap, call)?;
    let ch = match call.args[0] {
        Slot::Int(v) => v as u16,
        _ => return Err(KvmError::Internal("charWidth arg corrupt".into())),
    };
    Ok(Some(Slot::Int(host.text_width(handle, &[ch]) as i32)))
}

/// `charsWidth([CII)I`：字符数组区段测宽（offset/len 越界同 substring）。
pub fn chars_width(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    let handle = this_handle(cs, heap, call)?;
    let array = match call.args[0] {
        Slot::Ref(r) => r,
        _ => return Err(npe(cs, heap)),
    };
    if array == ObjRef::NULL {
        return Err(npe(cs, heap));
    }
    let (offset, len) = match (&call.args[1], &call.args[2]) {
        (Slot::Int(o), Slot::Int(l)) => (*o, *l),
        _ => return Err(KvmError::Internal("charsWidth args corrupt".into())),
    };
    // 先拷后判：越界布尔先算好（避免与 throw 的可变借用交错）。
    let data = match &heap.array(array)?.data {
        ArrayData::Char(data) => data.clone(),
        _ => return Err(KvmError::Internal("charsWidth array corrupt".into())),
    };
    let bad = offset < 0
        || len < 0
        || match offset.checked_add(len) {
            Some(end) => end as usize > data.len(),
            None => true,
        };
    if bad {
        return Err(jl::throw_java(
            cs,
            heap,
            "java/lang/ArrayIndexOutOfBoundsException",
            None,
        ));
    }
    let end = (offset + len) as usize;
    let sub = &data[offset as usize..end];
    Ok(Some(Slot::Int(host.text_width(handle, sub) as i32)))
}

// ---- 属性回读（实例字段） ---------------------------------------------------

/// `getFace()I`。
pub fn get_face(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    _host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Int(this_int_field(cs, heap, call, "face")?)))
}

/// `getStyle()I`。
pub fn get_style(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    _host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Int(this_int_field(cs, heap, call, "style")?)))
}

/// `getSize()I`。
pub fn get_size(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    _host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    Ok(Some(Slot::Int(this_int_field(cs, heap, call, "size")?)))
}

/// style 位测试共用：`isPlain` = 无任何位；其余 = 对应位置位。
fn style_bit(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    call: &NativeCall,
    mask: i32,
    plain: bool,
) -> Result<Option<Slot>, KvmError> {
    let style = this_int_field(cs, heap, call, "style")?;
    let hit = if plain {
        style & STYLE_MASK == 0
    } else {
        style & mask != 0
    };
    Ok(Some(Slot::Int(i32::from(hit))))
}

/// `isPlain()Z`。
pub fn is_plain(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    _host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    style_bit(cs, heap, call, 0, true)
}

/// `isBold()Z`。
pub fn is_bold(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    _host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    style_bit(cs, heap, call, 1, false)
}

/// `isItalic()Z`。
pub fn is_italic(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    _host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    style_bit(cs, heap, call, 2, false)
}

/// `isUnderlined()Z`。
pub fn is_underlined(
    cs: &mut ClassSpace,
    heap: &mut Heap,
    _host: &mut dyn HostContext,
    call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    style_bit(cs, heap, call, 4, false)
}
