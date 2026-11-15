//! 配置管理模块。
//!
//! 配置文件固定保存在“基准目录”中：
//! - 如果设置了环境变量 `FILE_ORIGIN_TRACKER_DATA_DIR`，使用该目录；
//! - 否则使用当前用户的 `AppData\Local\FileOriginTracker\FileOriginTracker`。
//!
//! `config.json` 里的 `data_dir` 字段决定**数据库**保存位置。
//! 修改该字段后重启程序，记录就会保存到指定文件夹。

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
    /// 配置文件所在的基准目录。
    fn base_dir() -> Result<PathBuf> {
        if let Some(directory) = std::env::var_os("FILE_ORIGIN_TRACKER_DATA_DIR") {
            return Ok(PathBuf::from(directory));
        }

        let dirs = ProjectDirs::from("com", "FileOriginTracker", "FileOriginTracker")
            .ok_or_else(|| anyhow::anyhow!("无法获取用户数据目录"))?;
        Ok(dirs.data_local_dir().to_path_buf())
    }

    fn config_path() -> Result<PathBuf> {
        Ok(Self::base_dir()?.join("config.json"))
    }

    /// 加载配置；首次运行时创建默认配置。
    pub fn load_or_default() -> Result<Self> {
        let config_path = Self::config_path()?;

        if config_path.exists() {
            let content = fs::read_to_string(&config_path)
                .with_context(|| format!("无法读取配置文件: {}", config_path.display()))?;
            let config: Config = serde_json::from_str(&content)
                .with_context(|| format!("配置文件格式错误: {}", config_path.display()))?;
            return Ok(config);
        }

        let base = Self::base_dir()?;
        let config = Config {
            data_dir: base,
            auto_register_context_menu: true,
            ignore_temp_dirs: true,
            ignore_system_dirs: true,
            verbose_logging: false,
            retention_days: default_retention_days(),
            monitor_create_file: true,
            monitor_create_dir: true,
            monitor_modify_file: false,
        };
        config.save()?;
        Ok(config)
    }

    /// 三个监控开关中是否至少有一个开启。
    /// 全部关闭时完全停止 ETW 会话，避免空转占用 CPU。
    pub fn any_monitor_enabled(&self) -> bool {
        self.monitor_create_file || self.monitor_create_dir || self.monitor_modify_file
    }

    /// 返回数据库实际使用的目录。
    ///
    /// 托盘窗口、右键菜单扩展等模块无法访问主程序的 `Config` 对象，
    /// 因此统一从这里读取 `config.json`，保证各处使用同一个目录。
    pub fn default_data_dir() -> Result<PathBuf> {
        let base = Self::base_dir()?;
        let config_path = base.join("config.json");

        if let Ok(content) = fs::read_to_string(&config_path) {
            if let Ok(config) = serde_json::from_str::<Config>(&content) {
                return Ok(config.data_dir);
            }
        }

        Ok(base)
    }

    /// 保存配置到基准目录（配置位置始终固定，避免移动数据库后找不到配置）。
    pub fn save(&self) -> Result<()> {
        let config_path = Self::config_path()?;
        if let Some(directory) = config_path.parent() {
            fs::create_dir_all(directory)
                .with_context(|| format!("无法创建配置目录: {}", directory.display()))?;
        }

        let json = serde_json::to_string_pretty(self)?;
        fs::write(&config_path, json)
            .with_context(|| format!("无法写入配置文件: {}", config_path.display()))?;
        Ok(())
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
