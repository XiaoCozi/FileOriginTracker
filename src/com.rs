//! 资源管理器右键菜单扩展（COM）。
//!
//! 这个模块会被编译成 `file_origin_tracker.dll`，由资源管理器在右键点击文件时加载。
//! 通过实现 `IExplorerCommand` 接口，菜单标题可以“按文件动态生成”：
//!
//! ```text
//! 该文件/文件夹来自 XXX 程序创建
//! ```
//!
//! `XXX` 是数据库中记录的创建者程序名。
//!
//! Windows 11 会把 `IExplorerCommand` 注册的菜单项显示在右键主菜单中；
//! 同时也会出现在“显示更多选项”的传统菜单里。
//!
//! 安全注意：DLL 运行在资源管理器进程内，因此这里所有操作都必须容错，
//! 任何数据库或路径异常都只降级为“未知程序”，绝不能 panic。

use std::ffi::c_void;
use std::sync::Mutex;

use once_cell::sync::Lazy;
use windows::core::{implement, Interface, GUID, HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    BOOL, CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_NOTIMPL, HWND,
};
use windows::Win32::System::Com::{CoTaskMemFree, IBindCtx, IClassFactory, IClassFactory_Impl};
use windows::Win32::UI::Shell::{
    IExplorerCommand, IExplorerCommand_Impl, IShellItemArray, SHStrDupW, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_OK};

use crate::config::Config;
use crate::db::{Database, FileOrigin};
use crate::util::wide;

/// 本右键菜单扩展的 COM 类 ID。
/// 每个 shell 扩展都必须有唯一 GUID，这里使用固定值：
/// {7c3a51e0-8d64-4c2b-9f51-2a8e6b1d4409}
pub const COMMAND_CLSID: GUID = GUID::from_values(
    0x7c3a51e0,
    0x8d64,
    0x4c2b,
    [0x9f, 0x51, 0x2a, 0x8e, 0x6b, 0x1d, 0x44, 0x09],
);

/// 复用的只读数据库连接。资源管理器长期运行，连接可以一直沿用。
static DB: Lazy<Mutex<Option<Database>>> = Lazy::new(|| Mutex::new(None));

/// 菜单命令对象（无状态，每次右键由系统创建一次）。
#[implement(IExplorerCommand)]
struct FileOriginCommand;

impl IExplorerCommand_Impl for FileOriginCommand_Impl {
    /// 菜单标题：这是实现“动态标题”的关键方法。
    fn GetTitle(&self, items: Option<&IShellItemArray>) -> windows::core::Result<PWSTR> {
        let program = origin_of(items)
            .map(|origin| origin.process_name)
            .unwrap_or_else(|| String::from("未知程序"));
        sh_str_dup(&format!("该文件/文件夹来自 {} 创建", program))
    }

    /// 菜单图标：返回本程序 exe 的路径，资源管理器会从里面提取我们嵌入的图标。
    fn GetIcon(&self, _items: Option<&IShellItemArray>) -> windows::core::Result<PWSTR> {
        // DLL 运行在 explorer.exe 内，必须通过模块地址找到自身 DLL 的位置，
        // 再定位同目录下的主程序 exe（图标就嵌在 exe 里）。
        let Some(dll) = crate::util::module_path_from_address(menu_icon_anchor as *const ()) else {
            return Err(E_NOTIMPL.into());
        };
        let mut icon_path = dll.with_file_name("file-origin-tracker.exe");
        if !icon_path.exists() {
            icon_path = dll;
        }
        sh_str_dup(&icon_path.to_string_lossy())
    }

    /// 鼠标悬停时的提示文字。
    fn GetToolTip(&self, _items: Option<&IShellItemArray>) -> windows::core::Result<PWSTR> {
        sh_str_dup("点击打开创建该文件或文件夹的程序所在目录")
    }

    /// 命令的唯一标识（同一 GUID 用于注册表关联）。
    fn GetCanonicalName(&self) -> windows::core::Result<GUID> {
        Ok(COMMAND_CLSID)
    }

    /// ECS_ENABLED = 0，表示菜单项可用。
    fn GetState(
        &self,
        _items: Option<&IShellItemArray>,
        _slow_ok: BOOL,
    ) -> windows::core::Result<u32> {
        Ok(0)
    }

    /// 用户点击菜单时执行：打开程序所在目录。
    fn Invoke(
        &self,
        items: Option<&IShellItemArray>,
        _bind_context: Option<&IBindCtx>,
    ) -> windows::core::Result<()> {
        let Some(path) = (unsafe { target_path(items) }) else {
            return Ok(());
        };

        match origin_of(items) {
            Some(origin) => open_in_explorer(&origin.process_path),
            None => show_message(&format!(
                "未追踪到该文件或文件夹的创建来源。\n\n\
                 只有本程序运行期间新创建的文件才会被记录。\n\n路径：{}",
                path
            )),
        }

        Ok(())
    }

    /// ECF_DEFAULT = 0。
    fn GetFlags(&self) -> windows::core::Result<u32> {
        Ok(0)
    }

    /// 不支持子菜单。
    fn EnumSubCommands(
        &self,
    ) -> windows::core::Result<windows::Win32::UI::Shell::IEnumExplorerCommand> {
        Err(E_NOTIMPL.into())
    }
}

/// COM 类工厂：系统通过它创建 `FileOriginCommand` 实例。
#[implement(IClassFactory)]
struct CommandFactory;

impl IClassFactory_Impl for CommandFactory_Impl {
    fn CreateInstance(
        &self,
        outer: Option<&windows::core::IUnknown>,
        riid: *const GUID,
        interface: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        // 不支持 COM 聚合。
        if outer.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }

        let command: IExplorerCommand = FileOriginCommand.into();
        unsafe { command.query(riid, interface).ok() }
    }

    fn LockServer(&self, _lock: BOOL) -> windows::core::Result<()> {
        Ok(())
    }
}

/// DLL 导出函数：系统请求创建 COM 对象时调用。
#[no_mangle]
pub extern "system" fn DllGetClassObject(
    class_id: *const GUID,
    interface_id: *const GUID,
    interface: *mut *mut c_void,
) -> HRESULT {
    unsafe {
        if class_id.is_null() || *class_id != COMMAND_CLSID {
            return CLASS_E_CLASSNOTAVAILABLE;
        }

        let factory: IClassFactory = CommandFactory.into();
        factory.query(interface_id, interface)
    }
}

/// DLL 导出函数：告诉系统当前能否卸载 DLL。
/// 返回 S_FALSE(1) 表示可以卸载（DLL 不持有常驻对象）。
#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    HRESULT(1)
}

/// 取右键目标（第一个选中项）的完整文件系统路径。
unsafe fn target_path(items: Option<&IShellItemArray>) -> Option<String> {
    let items = items?;
    let item = items.GetItemAt(0).ok()?;
    let path = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
    let text = path.to_string().ok();

    // GetDisplayName 返回的内存由 COM 分配，必须用 CoTaskMemFree 释放。
    CoTaskMemFree(Some(path.0 as *const c_void));
    text
}

/// 查询文件来源记录（失败一律视为“没有记录”）。
fn origin_of(items: Option<&IShellItemArray>) -> Option<FileOrigin> {
    let path = unsafe { target_path(items) }?;

    let mut guard = match DB.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };

    if guard.is_none() {
        let data_dir = Config::default_data_dir().ok()?;
        *guard = Database::open_readonly(&data_dir).ok();
    }

    guard.as_ref()?.get_origin(&path).ok().flatten()
}

/// 在资源管理器中打开程序所在目录，并选中该程序文件。
fn open_in_explorer(process_path: &str) {
    crate::util::reveal_in_explorer(process_path);
}

/// 仅用于取“本 DLL 的模块路径”，本身不做任何事。
fn menu_icon_anchor() {}

fn show_message(text: &str) {
    let text = wide(text);
    let title = wide("文件来源追踪");
    unsafe {
        let _ = MessageBoxW(
            HWND::default(),
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

/// 把 Rust 字符串复制成 COM 分配的 PWSTR（调用方负责释放）。
fn sh_str_dup(text: &str) -> windows::core::Result<PWSTR> {
    let text = wide(text);
    unsafe { SHStrDupW(PCWSTR(text.as_ptr())) }
}
