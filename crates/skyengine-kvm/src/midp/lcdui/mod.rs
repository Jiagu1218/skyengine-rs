//! javax/microedition/lcdui 原生实现。
//!
//! - `canvas`：Canvas 对象模型/脏矩形 + paint 数据面（T021）
//! - `display`：Display 单例与 current 管理（T021）
//! - `font`：Font 度量/测宽（Host 轨，T019）
//! - `game`：GameCanvas 键状态位图/flushGraphics + Layer 族占位（T031）
//! - `graphics`：Graphics 光栅化（T020）
//! - `image`：Image 对象模型与 createImage 全套重载（T018）
//! - `input`：引擎键名→MIDP keyCode 映射 + Canvas 游戏动作 API（T030）
//! - `png`：PNG 解码器（T018）

pub mod canvas;
pub mod display;
pub mod font;
pub mod game;
pub mod graphics;
pub mod image;
pub mod input;
pub mod png;
