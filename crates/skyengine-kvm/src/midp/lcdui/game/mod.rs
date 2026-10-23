//! javax/microedition/lcdui/game 包（T031）。
//!
//! - `gamecanvas`：GameCanvas 真实现（键状态位图 getKeyStates /
//!   suppressKeyEvents 分流 / flushGraphics / getGraphics）。
//! - `layer`：Layer/LayerManager/Sprite/TiledLayer 占位声明
//!   （FR-016：jc.jar 基线 0 引用 game 包，占位防链接失败）。
pub mod gamecanvas;
pub mod layer;
