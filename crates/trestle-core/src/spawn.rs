//! 拉起 daemon 时要注意的平台细节。
//!
//! CLI 和 MCP 都会在连不上时把 daemon 拉起来。daemon 要活得比拉起它的那个进程
//! 长得多（空闲半小时才退），所以它**不能攥着父进程的任何东西**。

/// 把本进程的 stdin/stdout/stderr 标成不可继承。拉起 daemon 之前调用。
///
/// Windows 上 `std::process::Command` 是开着句柄继承去 `CreateProcess` 的，
/// 于是父进程里**所有**可继承的句柄都会进到子进程——哪怕子进程的 stdio 已经
/// 设成了 `Stdio::null()`。shell 交给 CLI 的管道句柄就是可继承的：
/// `trestle targets | grep x` 拉起 daemon 之后，daemon 攥着管道的写端，
/// `grep` 就一直等不到 EOF，直到 daemon 空闲退出——整整半小时。MCP 那边同理，
/// daemon 会攥住它和 Claude 之间的 stdio 管道。
///
/// 只动这三个：它们是本进程从外面接过来、又会在 daemon 活着期间被别人等待的东西。
/// 本进程之后不再拉起需要继承它们的子进程，所以清掉继承标记没有副作用。
pub fn keep_std_handles_private() {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;

        const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;
        unsafe extern "system" {
            fn SetHandleInformation(handle: *mut core::ffi::c_void, mask: u32, flags: u32) -> i32;
        }

        for handle in [
            std::io::stdin().as_raw_handle(),
            std::io::stdout().as_raw_handle(),
            std::io::stderr().as_raw_handle(),
        ] {
            if !handle.is_null() {
                // SAFETY: 句柄来自本进程的标准流，调用期间一直有效；失败（比如句柄是
                // 伪句柄或已关闭）只意味着它本来就不会被继承，忽略即可。
                unsafe {
                    SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
                }
            }
        }
    }
    // Unix 上 Rust 打开的文件默认带 O_CLOEXEC，`Command` 也只把配置好的 stdio
    // 交给子进程，没有这个问题。
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::os::windows::io::AsRawHandle;

    unsafe extern "system" {
        fn GetHandleInformation(handle: *mut core::ffi::c_void, flags: *mut u32) -> i32;
    }

    #[test]
    fn std_handles_stop_being_inheritable() {
        keep_std_handles_private();
        let handle = std::io::stdout().as_raw_handle();
        if handle.is_null() {
            return; // 测试进程可能根本没有 stdout
        }
        let mut flags = 0u32;
        // SAFETY: 本进程 stdout 的句柄，调用期间有效。
        let ok = unsafe { GetHandleInformation(handle, &mut flags) };
        if ok != 0 {
            assert_eq!(flags & 1, 0, "stdout is still inheritable");
        }
    }
}
