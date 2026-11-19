//! File Origin Tracker 的共享代码库。
//!
//! 该库同时被两个产物使用：
//! 1. 主程序 `file-origin-tracker.exe`：托盘 + ETW 追踪 + 记录窗口
//! 2. 右键菜单扩展 `file_origin_tracker.dll`：由资源管理器加载，动态显示菜单标题
//!
//! 两个产物共用同一份 SQLite 数据库，因此把配置和数据库代码放在这里。

pub mod about;
pub mod com;
pub mod config;
pub mod db;
pub mod datetime_popup;
pub mod etw;
pub mod export_dialog;
pub mod history;
pub mod list_rules;
pub mod list_window;
pub mod note;
pub mod process;
pub mod settings;
pub mod shell_menu;
pub mod stats;
pub mod theme;
pub mod tray;
pub mod util;
