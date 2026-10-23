//! javax/microedition/midlet 包原生实现（T044 生命周期语义）。
//!
//! ## 语义总表（引擎取舍）
//!
//! | native | 置位请求 | poll 消费动作 |
//! |---|---|---|
//! | `notifyDestroyed()V` | `DestroyApp{unconditional:false}` | spawn destroyApp(false) 回调帧，回调线程死后 VM 转 Dead |
//! | `notifyPaused()V` | `PauseApp` | spawn pauseApp() 回调帧（KVM 独占前台，仅回调不停 VM） |
//! | `resumeRequest()V` | `Resume` | spawn startApp() 回调帧（恢复运行态） |
//! | `System.exit(I)V` | `DestroyApp{unconditional:true}` | 同 notifyDestroyed 路径，但回调不可拒绝 |
//!
//! 取舍记录：真实 JVM 的 `System.exit` 直接终止进程不等回调；本引擎为
//! 给游戏一次清理机会（RMS 落盘/音效停播），沿 destroyApp 回调链收尾，
//! 且 unconditional=true 保证必然销毁（回调抛
//! MIDletStateChangeException 也照销，对齐 MIDP 规范的强制销毁语义）。
//!
//! Display 切换序列：pauseApp 后 Display/current 逻辑不动——本引擎 KVM
//! 独占前台，无 AMS 移交输入路由的场景，暂停仅做状态回调记录。
//!
//! 实现轨为纯 cs 侧（无需宿主服务）：native 只入队请求，回调执行由
//! `Vm.poll` 生命周期段驱动（T021/T033"请求 → poll 消费 → spawn 回调
//! 绿线程"协议）。

use crate::interp::NativeCall;
use crate::runtime::classspace::{ClassSpace, MidletLifecycleOp};
use crate::runtime::heap::Heap;
use crate::runtime::Slot;
use crate::KvmError;

/// `MIDlet.notifyDestroyed()V`：MIDlet 主动退出（游戏侧"离开"按钮）。
/// 置 `DestroyApp{unconditional:false}`——MIDP 语义下 AMS 回调
/// destroyApp 时 MIDlet 可抛 MIDletStateChangeException 拒绝；引擎在
/// Vm.poll 生命周期收尾段按"回调线程异常死亡 + 非强制"判定拒绝销毁。
pub fn notify_destroyed(
    cs: &mut ClassSpace,
    _heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    cs.pending_lifecycle.push(MidletLifecycleOp::DestroyApp {
        unconditional: false,
    });
    Ok(None)
}

/// `MIDlet.notifyPaused()V`：MIDlet 自愿暂停。置 `PauseApp`——poll 消费
/// 为 pauseApp() 回调；本引擎 KVM 独占前台，暂停只做状态与回调记录，
/// 不移交输入路由（无 AMS 抢占场景）。
pub fn notify_paused(
    cs: &mut ClassSpace,
    _heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    cs.pending_lifecycle.push(MidletLifecycleOp::PauseApp);
    Ok(None)
}

/// `MIDlet.resumeRequest()V`：暂停中的 MIDlet 请求恢复。置 `Resume`——
/// poll 消费为 startApp() 回调（MIDP 规范：恢复即重入 active 态回调）。
pub fn resume_request(
    cs: &mut ClassSpace,
    _heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    cs.pending_lifecycle.push(MidletLifecycleOp::Resume);
    Ok(None)
}

/// `System.exit(I)V` 桥（T045 收编原 TODO 桩）：置
/// `DestroyApp{unconditional:true}`，与 notifyDestroyed 同消费路径但
/// 强制销毁（回调不可拒绝）。退出码仅记录语义（引擎单进程无传值面）。
pub fn system_exit_bridge(
    cs: &mut ClassSpace,
    _heap: &mut Heap,
    _call: &NativeCall,
) -> Result<Option<Slot>, KvmError> {
    cs.pending_lifecycle.push(MidletLifecycleOp::DestroyApp {
        unconditional: true,
    });
    Ok(None)
}
