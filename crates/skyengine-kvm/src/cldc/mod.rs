//! CLDC 类库原生实现：T014 起 java.lang（lang.rs），
//! T015/T016 追加 java.io / java.util，T033 追加 java.lang.Thread
//! 绿线程面（thread.rs）。
//!
//! 模块函数与 `interp::natives::NativeFn` 签名一致，由 bootstrap
//! 注册表按 类名.方法名+描述符 绑定；桩值实现逐步替换为真实现。

/// java.io 原生实现（T016）。
pub mod io;

/// java.lang 原生实现（T014）。
pub mod lang;

/// java.lang.Thread 绿线程原生实现（T033）：start 请求队列消费、
/// currentThread/isAlive 三表、sleep/yield/join 让出协议。
pub mod thread;

/// java.util 原生实现（T015）。
pub mod util;
