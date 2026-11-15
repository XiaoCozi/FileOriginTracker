//! 进程信息解析模块。
//!
//! 使用 QueryFullProcessImageNameW，只需“受限查询”权限即可取得程序路径，
//! 比读取进程模块列表更轻量，也不需要 PROCESS_VM_READ 权限。

use anyhow::Result;
use std::path::Path;

use windows::core::PWSTR;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// 创建文件的进程信息。
#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub path: String,
}

impl ProcessInfo {
    /// 根据 PID 获取进程名和程序完整路径。
    /// 进程已经退出或权限不足时返回 Ok(None)，不影响后续事件处理。
    pub fn from_pid(pid: u32) -> Result<Option<Self>> {
        let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
            Ok(handle) => handle,
            Err(_) => return Ok(None),
        };

        // 使用堆缓冲区，支持超过传统 MAX_PATH 的程序路径。
        let mut buffer = vec![0u16; 32_768];
        let mut length = buffer.len() as u32;
        let query_result = unsafe {
            QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(buffer.as_mut_ptr()),
                &mut length,
            )
        };
        unsafe {
            let _ = CloseHandle(handle);
        }

        if query_result.is_err() || length == 0 {
            return Ok(None);
        }

        let path = String::from_utf16_lossy(&buffer[..length as usize]);
        let name = Path::new(&path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&path)
            .to_string();

        Ok(Some(Self { pid, name, path }))
    }
}

/// 从完整路径中提取文件名。
pub fn extract_filename(full_path: &str) -> String {
    Path::new(full_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(full_path)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_filename() {
        assert_eq!(
            extract_filename(r"C:\Windows\System32\notepad.exe"),
            "notepad.exe"
        );
        assert_eq!(extract_filename("notepad.exe"), "notepad.exe");
    }
}
