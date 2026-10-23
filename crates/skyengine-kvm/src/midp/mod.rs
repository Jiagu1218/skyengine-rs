//! javax/microedition MIDP 原生实现（T015 起逐步加入）。
//! - `io`：GCF Connector/HttpConnection 懒连接（T041，经 HostContext
//!   http_request 通道）。
//! - `lcdui`：LCDUI（Image/PNG 解码 T018，Graphics/Font 后续任务）。
//! - `media`：MMAPI Manager/Player 状态机（T038，解码后端归 T039）。
//! - `midlet`：MIDlet 生命周期请求位置位（T044，poll 消费在 vm.rs）。
//! - `rms`：RecordStore 持久化（T036，整店 blob 经 host 通道）。

pub mod io;

pub mod lcdui;

pub mod media;

pub mod midlet;

pub mod rms;
