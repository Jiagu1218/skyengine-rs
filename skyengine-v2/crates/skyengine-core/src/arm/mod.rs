// [skyengine] vendored arm7tdmi 集成层：总线、门面与迁移后的执行循环。
// 自研 ArmCpu 解释器已退役，行为基线由 ext/ 行为测试与真实样本回归承接。
pub(crate) mod bus;
pub(crate) mod facade;

mod ext;
mod memory;

pub(crate) use ext::{
    ExtLifecycleRequest, ExtRuntime, NativeExtensionProfile, NativeServices,
    START_FILE_PARAMETER_LEN,
};
pub use memory::{GuestAddr, GuestMemory, Permissions};
