//! 右键菜单注册管理。
//!
//! 动态标题由 `com.rs` 中的 COM 扩展（file_origin_tracker.dll）实现。
//! 本模块只负责把该扩展注册到当前用户的注册表中：
//!
//! ```text
//! HKCU\Software\Classes\*\shell\FileOriginTracker
//!     ExplorerCommandHandler = {CLSID}
//! HKCU\Software\Classes\Directory\shell\FileOriginTracker
//!     ExplorerCommandHandler = {CLSID}
//! HKCU\Software\Classes\CLSID\{CLSID}\InprocServer32
//!     (默认) = file_origin_tracker.dll 的完整路径
//!     ThreadingModel = Apartment
//! ```
//!
//! 写入 HKCU 不需要管理员权限，只影响当前登录用户。
//!
//! ## 单文件版
//!
//! 资源管理器的右键菜单扩展必须是独立的 DLL 文件，所以“只有一个 exe”
//! 的做法是：打包脚本把 DLL 追加到 exe 末尾，程序启动时再把自己的 DLL
//! 释放到数据目录 `shell\` 下，并把注册表指向那里（见 `unpack_shell_dll`）。
//! 这样用户只需要携带一个 exe，放在任何位置都能用。

use anyhow::{Context, Result};
use log::{info, warn};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HWND};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_OK, SW_SHOWNORMAL};

use crate::com::COMMAND_CLSID;
use crate::util::wide;

const REG_FILES_MENU: &str = r"Software\Classes\*\shell\FileOriginTracker";
const REG_DIRS_MENU: &str = r"Software\Classes\Directory\shell\FileOriginTracker";

/// 右键菜单扩展 DLL 的文件名。
const SHELL_DLL_NAME: &str = "file_origin_tracker_shell.dll";

/// 单文件版的“尾部数据”标记（正好 16 字节）。
///
/// 打包脚本（`tools/pack.ps1`）会把右键菜单扩展 DLL 追加到 exe 文件末尾，
/// 追加格式为：
///
/// ```text
/// [原来的 exe][DLL 内容][8 字节 DLL 长度，小端序][16 字节标记]
/// ```
///
/// Windows 加载 PE 文件时会忽略末尾的附加数据，所以 exe 照样能正常运行；
/// 程序启动时再从自己文件末尾把 DLL 读出来写到数据目录，资源管理器就能加载它。
const PAYLOAD_MAGIC: &[u8; 16] = b"FOT-SHELL-DLL-01";
/// 尾部结构大小：8 字节长度 + 16 字节标记。
const PAYLOAD_TRAILER: u64 = 8 + 16;
/// 解压时的体积上限（64MB），防止文件损坏时分配出巨大内存。
const MAX_PAYLOAD_BYTES: u64 = 64 * 1024 * 1024;

/// 从当前 exe 末尾取出打包进去的 DLL 内容。
///
/// 普通开发版 exe（直接 `cargo build` 出来的）末尾没有这段数据，
/// 此时返回 None，调用方会退回到“使用 exe 同目录的 DLL”。
fn embedded_shell_dll() -> Option<Vec<u8>> {
    let exe = std::env::current_exe().ok()?;
    let mut file = std::fs::File::open(&exe).ok()?;
    let file_len = file.metadata().ok()?.len();
    if file_len <= PAYLOAD_TRAILER {
        return None;
    }

    // 1. 先看最后 16 字节是不是我们的标记。不是就说明这是普通 exe。
    file.seek(SeekFrom::Start(file_len - 16)).ok()?;
    let mut magic = [0u8; 16];
    file.read_exact(&mut magic).ok()?;
    if &magic != PAYLOAD_MAGIC {
        return None;
    }

    // 2. 读取 DLL 长度并做合理性检查，避免坏文件导致越界读取。
    file.seek(SeekFrom::Start(file_len - PAYLOAD_TRAILER))
        .ok()?;
    let mut length_bytes = [0u8; 8];
    file.read_exact(&mut length_bytes).ok()?;
    let length = u64::from_le_bytes(length_bytes);
    if length == 0 || length > MAX_PAYLOAD_BYTES || length + PAYLOAD_TRAILER > file_len {
        return None;
    }

    // 3. 定位到 DLL 内容并整块读出来。
    file.seek(SeekFrom::Start(file_len - PAYLOAD_TRAILER - length))
        .ok()?;
    let mut buffer = vec![0u8; length as usize];
    file.read_exact(&mut buffer).ok()?;
    Some(buffer)
}

/// 把打包在 exe 里的右键菜单扩展 DLL 写出来，返回它的路径。
///
/// - 单文件版：写到数据目录下的 `shell` 子目录，返回该路径
/// - 开发版（exe 里没打包 DLL）：返回 None
///
/// 之所以不写在 exe 同目录，是为了让 exe 放在任何位置（桌面、U 盘）都能用：
/// 注册表里记录的是数据目录里的固定路径，exe 挪动后菜单依然有效。
pub fn unpack_shell_dll() -> Option<PathBuf> {
    let bytes = embedded_shell_dll()?;

    let data_dir = crate::config::Config::default_data_dir().ok()?;
    let directory = data_dir.join("shell");
    let target = directory.join(SHELL_DLL_NAME);

    // 内容完全一致就不重复写：资源管理器可能正加载着这个 DLL，
    // 覆盖写入会失败，也会让菜单短暂失效。
    if let Ok(existing) = std::fs::read(&target) {
        if existing == bytes {
            return Some(target);
        }
    }

    std::fs::create_dir_all(&directory).ok()?;

    // 先写临时文件再改名，避免 DLL 只写了一半就被加载。
    let temp = directory.join("file_origin_tracker_shell.dll.new");
    if std::fs::write(&temp, &bytes).is_err() {
        return None;
    }
    // 旧 DLL 正被资源管理器使用时删不掉，这时保留旧文件继续可用。
    let _ = std::fs::remove_file(&target);
    if std::fs::rename(&temp, &target).is_err() {
        // 改名失败（例如被杀毒软件占用）时退化为直接覆盖写入，
        // 保证至少有一份可用的 DLL，不让右键菜单彻底失效。
        let _ = std::fs::write(&target, &bytes);
    }
    let _ = std::fs::remove_file(&temp);

    if target.exists() {
        Some(target)
    } else {
        None
    }
}

/// 返回右键菜单扩展 DLL 的完整路径。
///
/// 优先使用 exe 内打包并释放到数据目录的版本（单文件版），
/// 没有打包数据时退回“与主程序同一目录”（开发版）。
pub fn dll_path() -> Result<PathBuf> {
    if let Some(path) = unpack_shell_dll() {
        return Ok(path);
    }

    let exe = std::env::current_exe().context("无法获取当前可执行文件路径")?;
    let directory = exe.parent().context("无法获取程序所在目录")?;
    Ok(directory.join(SHELL_DLL_NAME))
}

/// 把 GUID 格式化为注册表使用的 `{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}` 形式。
fn clsid_text() -> String {
    let guid = COMMAND_CLSID;
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        guid.data1,
        guid.data2,
        guid.data3,
        guid.data4[0],
        guid.data4[1],
        guid.data4[2],
        guid.data4[3],
        guid.data4[4],
        guid.data4[5],
        guid.data4[6],
        guid.data4[7],
    )
}

/// 注册文件与文件夹的右键菜单。
pub fn register_context_menu() -> Result<()> {
    let dll = dll_path()?;
    if !dll.exists() {
        return Err(anyhow::anyhow!(
            "找不到右键菜单扩展 DLL：{}\n\
             单文件版请使用 tools/pack.ps1 打包后的 exe；\
             开发版请先执行 cargo build --release，并把 DLL 与 EXE 放在同一目录。",
            dll.display()
        ));
    }

    let dll_text = dll.to_string_lossy().to_string();
    let clsid = clsid_text();

    // 1. 注册 COM 类：告诉系统 DLL 在哪里、使用哪种线程模型。
    let inproc_path = format!(r"Software\Classes\CLSID\{}\InprocServer32", clsid);
    let inproc_key = create_key(&inproc_path)?;
    set_string(inproc_key, None, &dll_text)?;
    set_string(inproc_key, Some("ThreadingModel"), "Apartment")?;
    close_key(inproc_key);

    // 2. 注册菜单项：文件和文件夹各一个。
    register_menu_key(REG_FILES_MENU, &clsid)?;
    register_menu_key(REG_DIRS_MENU, &clsid)?;

    info!("右键菜单扩展注册完成：{}", dll_text);
    Ok(())
}

fn register_menu_key(menu_path: &str, clsid: &str) -> Result<()> {
    let key = create_key(menu_path)?;

    // 传统菜单（“显示更多选项”）会使用这个静态标题；
    // Windows 11 新式菜单会调用 DLL 返回动态标题。
    set_string(key, None, "该文件/文件夹来自哪个程序创建")?;
    set_string(key, Some("ExplorerCommandHandler"), clsid)?;
    // 一次只允许选中一个对象，避免多选时显示错误信息。
    set_string(key, Some("MultiSelectModel"), "Single")?;
    close_key(key);

    // 清掉早期版本注册的静态命令，避免右键菜单出现两个相同条目。
    delete_key(&format!(r"{}\command", menu_path));

    Ok(())
}

/// 注销右键菜单（同时清理早期版本的静态菜单）。
pub fn unregister_context_menu() -> Result<()> {
    delete_key(REG_FILES_MENU);
    delete_key(REG_DIRS_MENU);
    delete_key(&format!(r"Software\Classes\CLSID\{}", clsid_text()));
    info!("右键菜单扩展已注销");
    Ok(())
}

fn create_key(path: &str) -> Result<HKEY> {
    let path = wide(path);
    let mut key = HKEY::default();

    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(path.as_ptr()),
            0,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
    };

    if status != ERROR_SUCCESS {
        return Err(anyhow::anyhow!("创建注册表项失败：0x{:08X}", status.0));
    }
    Ok(key)
}

fn set_string(key: HKEY, name: Option<&str>, value: &str) -> Result<()> {
    let name = name.map(wide);
    let value = wide(value);
    let bytes = unsafe { std::slice::from_raw_parts(value.as_ptr().cast::<u8>(), value.len() * 2) };
    let name_pointer = name
        .as_ref()
        .map_or(PCWSTR::null(), |name| PCWSTR(name.as_ptr()));

    let status = unsafe { RegSetValueExW(key, name_pointer, 0, REG_SZ, Some(bytes)) };
    if status != ERROR_SUCCESS {
        return Err(anyhow::anyhow!("写入注册表值失败：0x{:08X}", status.0));
    }
    Ok(())
}

fn close_key(key: HKEY) {
    unsafe {
        let _ = RegCloseKey(key);
    }
}

fn delete_key(path: &str) {
    let path = wide(path);
    let status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr())) };
    if status != ERROR_SUCCESS && status != ERROR_FILE_NOT_FOUND {
        warn!("删除注册表项 {} 失败：0x{:08X}", path.len(), status.0);
    }
}

/// 处理旧版命令行 `shell-menu <路径>`：查询来源并打开程序目录。
/// 保留该入口是为了兼容早期注册的菜单命令。
pub fn handle_shell_menu(target_path: &str) -> Result<()> {
    let data_dir = crate::config::Config::default_data_dir()?;
    let db = crate::db::Database::init(&data_dir)?;

    match db.get_origin(target_path)? {
        Some(origin) => {
            if let Some(directory) = Path::new(&origin.process_path).parent() {
                let operation = wide("open");
                let directory = wide(&directory.to_string_lossy());
                unsafe {
                    let _ = ShellExecuteW(
                        HWND::default(),
                        PCWSTR(operation.as_ptr()),
                        PCWSTR(directory.as_ptr()),
                        PCWSTR::null(),
                        PCWSTR::null(),
                        SW_SHOWNORMAL,
                    );
                }
            }

            show_message(&format!(
                "该文件/文件夹来自 {} 创建\n\n文件：{}\n程序：{}",
                origin.process_name, target_path, origin.process_path
            ));
        }
        None => {
            show_message(&format!(
                "未追踪到该文件/文件夹的创建来源。\n\n文件：{}\n\n只有本程序运行期间新创建的文件才会被记录。",
                target_path
            ));
        }
    }
    Ok(())
}

fn show_message(text: &str) {
    let text = wide(text);
    let title = wide("文件来源追踪");
    unsafe {
        let _ = MessageBoxW(
            HWND::default(),
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK,
        );
    }
}
