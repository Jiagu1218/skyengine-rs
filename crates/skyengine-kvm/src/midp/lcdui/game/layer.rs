//! Layer 族占位声明（T031，FR-016）。
//!
//! **考证结论**：`tests/baseline_members.rs`（jc.jar 168 类全量基线）
//! 对 `lcdui/game` 的引用计数为 **0**——jc.jar 从不触碰
//! Layer/LayerManager/Sprite/TiledLayer。故本模块只注册"占位声明"：
//! extends 关系与公开方法签名按 MIDP-2.0 规范齐全（防 jar 侧子类化/
//! 反射/链接失败），实现全部为通用桩（stub_void/stub_zero/stub_null，
//! 调用返回默认值语义——FR-016 长尾成员占位策略）。
//!
//! 真实现（含 Sprite 帧动画/碰撞、TiledLayer 砖图渲染）待实证需求
//! 出现再排任务；届时 Sprite 的 TRANS_* 变换常量亦一并考证注册
//!（占位阶段不注册未经实证的常量值，防误导）。
//!
//! 注：static 声明要求 'static 字面量，描述符不走 format!/常量拼接。

use crate::classfile::member::acc;
use crate::cldc::io as jio;
use crate::interp::natives::{BootstrapDecl, field, m, stub_null, stub_void, stub_zero};

/// Layer（抽象基类：位置/尺寸/可见性 + 抽象 paint）。
pub(crate) static LAYER_DECL: BootstrapDecl = BootstrapDecl {
    name: "javax/microedition/lcdui/game/Layer",
    access_flags: acc::PUBLIC | acc::ABSTRACT,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[
        field("x", "I", acc::PRIVATE),
        field("y", "I", acc::PRIVATE),
        field("width", "I", acc::PRIVATE),
        field("height", "I", acc::PRIVATE),
        field("visible", "Z", acc::PRIVATE),
    ],
    methods: &[
        // protected Layer(int width, int height)
        m("<init>", "(II)V", acc::PROTECTED, stub_void),
        m("setPosition", "(II)V", acc::PUBLIC, stub_void),
        m("move", "(II)V", acc::PUBLIC, stub_void),
        m("getX", "()I", acc::PUBLIC, stub_zero),
        m("getY", "()I", acc::PUBLIC, stub_zero),
        m("getWidth", "()I", acc::PUBLIC, stub_zero),
        m("getHeight", "()I", acc::PUBLIC, stub_zero),
        // protected 维护面（Sprite/TiledLayer 子类构造用）
        m("setWidth", "(I)V", acc::PROTECTED, stub_void),
        m("setHeight", "(I)V", acc::PROTECTED, stub_void),
        m("setVisible", "(Z)V", acc::PUBLIC, stub_void),
        m("isVisible", "()Z", acc::PUBLIC, stub_zero),
        // 抽象绘制：子类（真实现时）覆写
        m(
            "paint",
            "(Ljavax/microedition/lcdui/Graphics;)V",
            acc::PUBLIC | acc::ABSTRACT,
            jio::abstract_stub,
        ),
    ],
};

/// LayerManager（图层栈 + 视窗）：全桩。
pub(crate) static LAYER_MANAGER_DECL: BootstrapDecl = BootstrapDecl {
    name: "javax/microedition/lcdui/game/LayerManager",
    access_flags: acc::PUBLIC,
    super_name: Some("java/lang/Object"),
    interfaces: &[],
    fields: &[],
    methods: &[
        m("<init>", "()V", acc::PUBLIC, stub_void),
        m(
            "append",
            "(Ljavax/microedition/lcdui/game/Layer;)V",
            acc::PUBLIC,
            stub_void,
        ),
        m(
            "insert",
            "(Ljavax/microedition/lcdui/game/Layer;I)V",
            acc::PUBLIC,
            stub_void,
        ),
        m(
            "remove",
            "(Ljavax/microedition/lcdui/game/Layer;)V",
            acc::PUBLIC,
            stub_void,
        ),
        m("getSize", "()I", acc::PUBLIC, stub_zero),
        m(
            "getLayerAt",
            "(I)Ljavax/microedition/lcdui/game/Layer;",
            acc::PUBLIC,
            stub_null,
        ),
        m(
            "paint",
            "(Ljavax/microedition/lcdui/Graphics;II)V",
            acc::PUBLIC,
            stub_void,
        ),
        m("setViewWindow", "(IIII)V", acc::PUBLIC, stub_void),
    ],
};

/// Sprite（帧动画图层）：全桩（真实现待实证需求）。
pub(crate) static SPRITE_DECL: BootstrapDecl = BootstrapDecl {
    name: "javax/microedition/lcdui/game/Sprite",
    access_flags: acc::PUBLIC,
    super_name: Some("javax/microedition/lcdui/game/Layer"),
    interfaces: &[],
    fields: &[],
    methods: &[
        // Sprite(Image) / Sprite(Image, w, h) / Sprite(Sprite 拷贝)
        m(
            "<init>",
            "(Ljavax/microedition/lcdui/Image;)V",
            acc::PUBLIC,
            stub_void,
        ),
        m(
            "<init>",
            "(Ljavax/microedition/lcdui/Image;II)V",
            acc::PUBLIC,
            stub_void,
        ),
        m(
            "<init>",
            "(Ljavax/microedition/lcdui/game/Sprite;)V",
            acc::PUBLIC,
            stub_void,
        ),
        m(
            "setImage",
            "(Ljavax/microedition/lcdui/Image;II)V",
            acc::PUBLIC,
            stub_void,
        ),
        m(
            "defineCollisionRectangle",
            "(IIII)V",
            acc::PUBLIC,
            stub_void,
        ),
        m("setFrameSequence", "([I)V", acc::PUBLIC, stub_void),
        m("getFrameSequenceLength", "()I", acc::PUBLIC, stub_zero),
        m("getRawFrameCount", "()I", acc::PUBLIC, stub_zero),
        m("getFrame", "()I", acc::PUBLIC, stub_zero),
        m("setFrame", "(I)V", acc::PUBLIC, stub_void),
        m("nextFrame", "()V", acc::PUBLIC, stub_void),
        m("prevFrame", "()V", acc::PUBLIC, stub_void),
        m("setTransform", "(I)V", acc::PUBLIC, stub_void),
        m("setRefPixelPosition", "(II)V", acc::PUBLIC, stub_void),
        m("getRefPixelX", "()I", acc::PUBLIC, stub_zero),
        m("getRefPixelY", "()I", acc::PUBLIC, stub_zero),
        // collidesWith 三重载（Sprite/图片/TiledLayer，像素级标志）
        m(
            "collidesWith",
            "(Ljavax/microedition/lcdui/game/Sprite;Z)Z",
            acc::PUBLIC,
            stub_zero,
        ),
        m(
            "collidesWith",
            "(Ljavax/microedition/lcdui/Image;IZ)Z",
            acc::PUBLIC,
            stub_zero,
        ),
        m(
            "collidesWith",
            "(Ljavax/microedition/lcdui/game/TiledLayer;Z)Z",
            acc::PUBLIC,
            stub_zero,
        ),
        // 具体类实现抽象 paint（占位空绘）
        m(
            "paint",
            "(Ljavax/microedition/lcdui/Graphics;)V",
            acc::PUBLIC,
            stub_void,
        ),
    ],
};

/// TiledLayer（砖图图层）：全桩。
pub(crate) static TILED_LAYER_DECL: BootstrapDecl = BootstrapDecl {
    name: "javax/microedition/lcdui/game/TiledLayer",
    access_flags: acc::PUBLIC,
    super_name: Some("javax/microedition/lcdui/game/Layer"),
    interfaces: &[],
    fields: &[],
    methods: &[
        // TiledLayer(cols, rows, Image, tileW, tileH)
        m(
            "<init>",
            "(IILjavax/microedition/lcdui/Image;II)V",
            acc::PUBLIC,
            stub_void,
        ),
        m("getColumns", "()I", acc::PUBLIC, stub_zero),
        m("getRows", "()I", acc::PUBLIC, stub_zero),
        m("setCell", "(III)V", acc::PUBLIC, stub_void),
        m("getCell", "(II)I", acc::PUBLIC, stub_zero),
        m("fillCells", "(IIIII)V", acc::PUBLIC, stub_void),
        m("createAnimatedTile", "(I)I", acc::PUBLIC, stub_zero),
        m("setAnimatedTile", "(II)V", acc::PUBLIC, stub_void),
        m("getAnimatedTile", "(I)I", acc::PUBLIC, stub_zero),
        m(
            "setStaticTileSet",
            "(Ljavax/microedition/lcdui/Image;II)V",
            acc::PUBLIC,
            stub_void,
        ),
        m(
            "paint",
            "(Ljavax/microedition/lcdui/Graphics;)V",
            acc::PUBLIC,
            stub_void,
        ),
    ],
};
