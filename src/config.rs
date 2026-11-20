//! 配置管理模块。
//!
//! 配置的默认位置仍由本模块确定：
//! - 如果设置了环境变量 `FILE_ORIGIN_TRACKER_DATA_DIR`，使用该目录；
//! - 否则使用当前用户的 `AppData\Local\FileOriginTracker\FileOriginTracker`。
//!
//! 自定义配置 / 数据库目录由 `storage` 模块记录。这里保留原来的读取接口，
//! 所以记录窗口、右键菜单等调用方不需要关心配置实际被移到了哪里。

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// 数据库存储目录。
    pub data_dir: PathBuf,
    /// 启动时是否自动注册右键菜单。
    pub auto_register_context_menu: bool,
    /// 是否忽略临时目录，减少无意义的临时文件记录。
    pub ignore_temp_dirs: bool,
    /// 是否忽略 Windows 和程序安装目录。
    pub ignore_system_dirs: bool,
    /// 是否记录详细日志（预留给后续诊断使用）。
    pub verbose_logging: bool,
    /// 记录保留天数：超过这个天数的旧记录会在启动时和每天自动清理。
    /// `#[serde(default)]` 保证旧版 config.json 没有该字段时也能正常读取。
    #[serde(default = "default_retention_days")]
    pub retention_days: i64,
    /// 是否记录“新建文件”。
    #[serde(default = "default_true")]
    pub monitor_create_file: bool,
    /// 是否记录“新建文件夹”。
    #[serde(default = "default_true")]
    pub monitor_create_dir: bool,
    /// 是否记录“文件内容被修改”。
    /// 写入事件数量非常大，所以默认关闭，需要用户在设置里主动开启。
    #[serde(default)]
    pub monitor_modify_file: bool,
    /// 是否记录“删除文件”。
    #[serde(default = "default_true")]
    pub monitor_delete_file: bool,
    /// 是否记录“删除文件夹”。
    #[serde(default = "default_true")]
    pub monitor_delete_dir: bool,
    /// 黑白名单模式：`black` 表示黑名单，`white` 表示白名单。
    ///
    /// 名单具体条目存在数据库的 `list_entries` 表里，配置文件只保存当前模式。
    #[serde(default = "default_list_mode")]
    pub list_mode: String,
}

/// 默认使用黑名单模式（名单为空时等价于“记录所有文件”）。
fn default_list_mode() -> String {
    crate::list_rules::MODE_BLACK.to_string()
}

/// 默认保留 7 天。
fn default_retention_days() -> i64 {
    7
}

/// 默认开启监控。
fn default_true() -> bool {
    true
}

impl Config {
    /// 默认基准目录，同时也是路径指引文件保存的位置。
    pub(crate) fn base_dir() -> Result<PathBuf> {
        if let Some(directory) = std::env::var_os("FILE_ORIGIN_TRACKER_DATA_DIR") {
            return Ok(PathBuf::from(directory));
        }

        let dirs = ProjectDirs::from("com", "FileOriginTracker", "FileOriginTracker")
            .ok_or_else(|| anyhow::anyhow!("无法获取用户数据目录"))?;
        Ok(dirs.data_local_dir().to_path_buf())
    }

    /// 从指定文件读取配置，不处理路径定位，也不自动创建文件。
    pub(crate) fn read_from(config_path: &Path) -> Result<Self> {
        let content = fs::read_to_string(config_path)
            .with_context(|| format!("无法读取配置文件: {}", config_path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("配置文件格式错误: {}", config_path.display()))
    }

    /// 生成一份默认配置。只有首次运行时才使用，路径切换会保留现有设置。
    pub(crate) fn default_with_data_dir(data_dir: PathBuf) -> Self {
        Config {
            data_dir,
            auto_register_context_menu: true,
            ignore_temp_dirs: true,
            ignore_system_dirs: true,
            verbose_logging: false,
            retention_days: default_retention_days(),
            monitor_create_file: true,
            monitor_create_dir: true,
            monitor_modify_file: false,
            monitor_delete_file: true,
            monitor_delete_dir: true,
            list_mode: default_list_mode(),
        }
    }

    /// 加载当前配置；首次运行时创建默认配置。
    pub fn load_or_default() -> Result<Self> {
        crate::storage::load_active_config()
    }

    /// 所有监控开关中是否至少有一个开启。
    /// 全部关闭时完全停止 ETW 会话，避免空转占用 CPU。
    pub fn any_monitor_enabled(&self) -> bool {
        self.monitor_create_file
            || self.monitor_create_dir
            || self.monitor_modify_file
            || self.monitor_delete_file
            || self.monitor_delete_dir
    }

    /// 当前是否为白名单模式（配置里出现无法识别的值时一律按黑名单处理）。
    pub fn is_white_list(&self) -> bool {
        self.list_mode == crate::list_rules::MODE_WHITE
    }

    /// 当前模式的名称，用于界面与日志显示。
    pub fn list_mode_name(&self) -> &'static str {
        if self.is_white_list() {
            "白名单"
        } else {
            "黑名单"
        }
    }

    /// 返回数据库实际使用的目录。
    ///
    /// 托盘窗口、右键菜单扩展等模块继续调用这个接口；路径指引会让它们
    /// 自动找到迁移后的数据库，待重启的切换任务则不会影响当前读取位置。
    pub fn default_data_dir() -> Result<PathBuf> {
        crate::storage::active_data_dir()
    }

    /// 保存到当前生效的配置位置，而不是待切换的目标位置。
    pub fn save(&self) -> Result<()> {
        crate::storage::save_active_config(self)
    }

    /// 判断是否应该忽略这个路径（临时目录或系统目录）。
    pub fn should_ignore(&self, path: &Path) -> bool {
        if self.ignore_temp_dirs && is_temp_path(path) {
            return true;
        }
        if self.ignore_system_dirs && is_system_path(path) {
            return true;
        }
        false
    }
}

fn is_temp_path(path: &Path) -> bool {
    let temp_dirs = [std::env::temp_dir(), PathBuf::from(r"C:\Windows\Temp")];
    temp_dirs
        .iter()
        .any(|directory| path.starts_with(directory))
}

fn is_system_path(path: &Path) -> bool {
    let system_dirs = [
        PathBuf::from(r"C:\Windows"),
        PathBuf::from(r"C:\Program Files"),
        PathBuf::from(r"C:\Program Files (x86)"),
        PathBuf::from(r"C:\ProgramData"),
    ];
    system_dirs
        .iter()
        .any(|directory| path.starts_with(directory))
}
