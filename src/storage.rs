//! 配置与数据库的存储位置管理。
//!
//! 默认目录中只固定保留 `storage_location.json` 作为“路径指引”。真正的
//! `config.json` 和 `file_origins.db` 可以位于同一目录，也可以分别存放。
//! 设置页只登记待切换任务；正常主程序取得单实例锁后才调用本模块执行，
//! 历史窗口和右键菜单永远不会在这里触发迁移。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::Config;
use crate::db::Database;

const LOCATION_FILE_NAME: &str = "storage_location.json";
const CONFIG_FILE_NAME: &str = "config.json";
const DATABASE_FILE_NAME: &str = "file_origins.db";
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 一组生效或待生效的存储位置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageLocation {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
}

impl StorageLocation {
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join(CONFIG_FILE_NAME)
    }

    pub fn database_file(&self) -> PathBuf {
        self.data_dir.join(DATABASE_FILE_NAME)
    }
}

/// 路径切换方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwitchMode {
    /// 复制全部设置与记录，激活新位置后删除已移走的原文件。
    Migrate,
    /// 保留设置、创建空数据库，原位置的配置与记录全部保留。
    Fresh,
}

/// 保存设置后、下次正常启动前暂存的任务。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingSwitch {
    pub target: StorageLocation,
    pub mode: SwitchMode,
    /// 使用保存设置时的完整快照，避免重新开始变成“恢复默认设置”。
    pub config: Config,
}

/// 路径指引文件的完整内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageState {
    pub active: StorageLocation,
    #[serde(default)]
    pub pending: Option<PendingSwitch>,
    /// 只记录最近一次切换前的原位置。
    #[serde(default)]
    pub previous: Option<StorageLocation>,
    /// 已激活新位置、但暂未删除成功的旧文件，下次正常启动继续清理。
    #[serde(default)]
    cleanup_files: Vec<PathBuf>,
    /// 迁移后需要顺手清理的附属文件（旧版本释放的右键菜单 DLL）。
    ///
    /// 这类文件可能正被资源管理器占用，删除失败不弹窗打扰用户，
    /// 只在日志里留一条记录，下次启动再试。
    #[serde(default)]
    quiet_cleanup_files: Vec<PathBuf>,
}

/// 启动时执行存储任务的结果。
#[derive(Debug, Default)]
pub struct SwitchReport {
    pub switched: bool,
    pub mode: Option<SwitchMode>,
    /// 切换已经激活后发生的清理问题，不影响继续使用新位置。
    pub warnings: Vec<String>,
}

pub fn load_state() -> Result<StorageState> {
    let base = Config::base_dir()?;
    Ok(load_state_from(&base)?.0)
}

pub fn active_data_dir() -> Result<PathBuf> {
    let base = Config::base_dir()?;
    Ok(active_data_dir_at(&base)?)
}

pub fn load_active_config() -> Result<Config> {
    let base = Config::base_dir()?;
    load_active_config_at(&base)
}

pub fn save_active_config(config: &Config) -> Result<()> {
    let base = Config::base_dir()?;
    save_active_config_at(&base, config)
}

/// 默认 AppData 快捷目录：配置与数据库都使用原来的基准目录。
pub fn default_location() -> Result<StorageLocation> {
    let base = Config::base_dir()?;
    Ok(StorageLocation {
        config_dir: base.clone(),
        data_dir: base,
    })
}

/// “程序目录”快捷按钮使用的固定子文件夹。
pub fn program_data_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("无法确定程序所在位置")?;
    let directory = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("无法确定程序所在目录"))?;
    Ok(directory.join("FileOriginTrackerData"))
}

/// 保存一个待重启执行的切换任务，不搬动当前正在使用的文件。
pub fn schedule_switch(target: StorageLocation, mode: SwitchMode, config: &Config) -> Result<()> {
    let base = Config::base_dir()?;
    schedule_switch_at(&base, target, mode, config)
}

pub fn cancel_pending_switch() -> Result<()> {
    let base = Config::base_dir()?;
    let (mut state, _) = load_state_from(&base)?;
    state.pending = None;
    save_state_to(&base, &state)
}

/// 比较目录时忽略 Windows 大小写、重复分隔符、`.` 和末尾分隔符。
pub fn paths_equal(left: &Path, right: &Path) -> bool {
    normalized_key(left) == normalized_key(right)
}

fn active_data_dir_at(base: &Path) -> Result<PathBuf> {
    Ok(load_state_from(base)?.0.active.data_dir)
}

fn load_active_config_at(base: &Path) -> Result<Config> {
    let (state, location_file_exists) = load_state_from(base)?;
    let config_path = state.active.config_file();

    if config_path.exists() {
        let mut config = Config::read_from(&config_path)?;
        // 指引文件是当前数据库位置的权威来源，同时兼容旧配置里的 data_dir。
        config.data_dir = state.active.data_dir;
        return Ok(config);
    }

    if location_file_exists {
        return Err(anyhow::anyhow!(
            "当前位置缺少配置文件：{}\n请检查存储路径，原配置不会被自动覆盖。",
            config_path.display()
        ));
    }

    // 首次运行：还没有指引和配置，可以安全创建默认文件。
    let config = Config::default_with_data_dir(state.active.data_dir.clone());
    save_active_config_at(base, &config)?;
    Ok(config)
}

fn save_active_config_at(base: &Path, config: &Config) -> Result<()> {
    let (state, _) = load_state_from(base)?;
    let mut config = config.clone();
    config.data_dir = state.active.data_dir.clone();
    write_json_atomic(&state.active.config_file(), &config)?;
    // 旧版本升级后第一次保存时补上指引；若已有待切换任务则原样保留。
    save_state_to(base, &state)
}

fn schedule_switch_at(
    base: &Path,
    target: StorageLocation,
    mode: SwitchMode,
    config: &Config,
) -> Result<()> {
    let (mut state, _) = load_state_from(base)?;
    let target = validate_switch(&state.active, &target, mode)?;
    let mut config = config.clone();
    config.data_dir = target.data_dir.clone();
    state.pending = Some(PendingSwitch {
        target,
        mode,
        config,
    });
    save_state_to(base, &state)
}

/// 只有正常主程序在取得单实例锁之后才能调用这个函数。
pub fn apply_pending_switch() -> Result<SwitchReport> {
    let base = Config::base_dir()?;
    apply_pending_switch_at(&base)
}

fn apply_pending_switch_at(base: &Path) -> Result<SwitchReport> {
    let (mut state, location_file_exists) = load_state_from(base)?;
    let mut report = SwitchReport::default();

    if let Some(pending) = state.pending.clone() {
        let prepared = match prepare_switch(base, &state.active, &pending) {
            Ok(prepared) => prepared,
            Err(error) => {
                // 激活前失败：旧位置从未改变，取消任务，避免每次启动都卡住。
                state.pending = None;
                if let Err(save_error) = save_state_to(base, &state) {
                    return Err(anyhow::anyhow!(
                        "{error:#}\n同时无法取消待切换任务：{save_error:#}"
                    ));
                }
                return Err(error);
            }
        };

        let old_location = state.active.clone();
        state.active = prepared.target.clone();
        state.pending = None;
        // 不论“迁移数据”还是“重新记录”，都记录切换前的位置，
        // 方便用户随时确认数据原来放在哪里。
        state.previous = Some(old_location);
        state.cleanup_files.extend(prepared.cleanup_files);
        state
            .quiet_cleanup_files
            .extend(prepared.quiet_cleanup_files);

        // 这是唯一的“激活点”。只有新配置和新数据库都准备好后才执行。
        if let Err(error) = save_state_to(base, &state) {
            remove_created_files(&prepared.created_files);
            return Err(error.context("新文件已验证，但无法更新路径指引；已继续使用原位置"));
        }

        report.switched = true;
        report.mode = Some(pending.mode);
    }

    let (remaining, mut warnings) = delete_cleanup_files(&state.cleanup_files);
    state.cleanup_files = remaining;
    report.warnings.append(&mut warnings);

    // 附属文件（旧版右键菜单 DLL）删除失败不弹窗，静默留到下次启动重试。
    state.quiet_cleanup_files = delete_quiet_files(&state.quiet_cleanup_files);

    // 没有指引且没有任务时保持首次运行状态，不为只读查询主动创建文件。
    if location_file_exists || report.switched {
        if let Err(error) = save_state_to(base, &state) {
            report.warnings.push(format!(
                "存储位置已经生效，但清理进度暂时无法保存：{error:#}"
            ));
        }
    }

    Ok(report)
}

struct PreparedSwitch {
    target: StorageLocation,
    /// 仅在目标位置新生成的文件；激活失败时可以安全删除这些文件。
    created_files: Vec<PathBuf>,
    /// 激活成功后允许删除的原文件。
    cleanup_files: Vec<PathBuf>,
    /// 激活成功后尽力清理的原位置附属文件（失败不打扰用户）。
    quiet_cleanup_files: Vec<PathBuf>,
}

fn prepare_switch(
    base: &Path,
    active: &StorageLocation,
    pending: &PendingSwitch,
) -> Result<PreparedSwitch> {
    let target = validate_switch(active, &pending.target, pending.mode)?;
    fs::create_dir_all(&target.config_dir)
        .with_context(|| format!("无法创建配置目录：{}", target.config_dir.display()))?;
    fs::create_dir_all(&target.data_dir)
        .with_context(|| format!("无法创建数据库目录：{}", target.data_dir.display()))?;

    let mut created_files = Vec::new();
    let result = (|| -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
        let source_config = active.config_file();
        let target_config = target.config_file();
        let config_changed = !paths_equal(&source_config, &target_config);

        // 配置先写临时文件并完整落盘，再改名为正式文件。
        let mut config_temp = TempFile::for_target(&target_config)?;
        write_new_json(&config_temp.path, &pending.config)?;
        config_temp.install(!config_changed)?;
        if config_changed {
            created_files.push(target_config);
        }

        let source_database = active.database_file();
        let target_database = target.database_file();
        let database_changed = !paths_equal(&source_database, &target_database);
        if database_changed {
            let mut database_temp = TempFile::for_target(&target_database)?;
            match pending.mode {
                SwitchMode::Migrate => {
                    Database::export_consistent_snapshot(&source_database, &database_temp.path)?
                }
                SwitchMode::Fresh => Database::create_empty_file(&database_temp.path)?,
            }
            // 目标位置在预检时不存在，使用“不覆盖”改名防止竞态覆盖。
            database_temp.install(false)?;
            created_files.push(target_database);
        }

        let mut cleanup_files = Vec::new();
        let mut quiet_cleanup_files = Vec::new();
        if pending.mode == SwitchMode::Migrate {
            if config_changed {
                cleanup_files.push(source_config);
            }
            if database_changed {
                cleanup_files.push(source_database.clone());
                cleanup_files.push(sidecar_path(&source_database, "-wal"));
                cleanup_files.push(sidecar_path(&source_database, "-shm"));
            }
            // 旧版本会把单文件版的右键菜单 DLL 释放到数据目录。现在 DLL 固定
            // 放在路径指引所在目录，迁移后旧副本不再被使用，顺手删掉以免残留。
            // 删除失败（例如资源管理器仍加载着 DLL）只记日志，下次启动再试。
            let shell_home = paths_equal(base, &active.data_dir);
            if database_changed && !shell_home {
                quiet_cleanup_files.extend(shell_dll_artifacts(&active.data_dir));
            }
        }
        Ok((cleanup_files, quiet_cleanup_files))
    })();

    match result {
        Ok((cleanup_files, quiet_cleanup_files)) => Ok(PreparedSwitch {
            target,
            created_files,
            cleanup_files,
            quiet_cleanup_files,
        }),
        Err(error) => {
            remove_created_files(&created_files);
            Err(error)
        }
    }
}

fn validate_switch(
    active: &StorageLocation,
    target: &StorageLocation,
    mode: SwitchMode,
) -> Result<StorageLocation> {
    let target = StorageLocation {
        config_dir: normalize_absolute(&target.config_dir)?,
        data_dir: normalize_absolute(&target.data_dir)?,
    };

    for (name, directory) in [("配置", &target.config_dir), ("数据库", &target.data_dir)] {
        if directory.exists() && !directory.is_dir() {
            return Err(anyhow::anyhow!(
                "{name}目录指向了一个文件，而不是文件夹：{}",
                directory.display()
            ));
        }
    }

    let config_unchanged = paths_equal(&target.config_file(), &active.config_file());
    let database_unchanged = paths_equal(&target.database_file(), &active.database_file());
    if config_unchanged && database_unchanged {
        return Err(anyhow::anyhow!("配置和数据库目录都没有变化。"));
    }
    if mode == SwitchMode::Fresh && database_unchanged {
        return Err(anyhow::anyhow!(
            "“重新记录”必须选择不同的数据库目录，才能同时保留原数据库并创建空数据库。"
        ));
    }

    let target_config = target.config_file();
    if !config_unchanged && target_config.exists() {
        return Err(anyhow::anyhow!(
            "目标位置已经存在配置文件，未覆盖：{}",
            target_config.display()
        ));
    }

    if !database_unchanged {
        for path in database_with_sidecars(&target.database_file()) {
            if path.exists() {
                return Err(anyhow::anyhow!(
                    "目标位置已经存在数据库文件，未覆盖：{}",
                    path.display()
                ));
            }
        }
    }

    Ok(target)
}

fn normalize_absolute(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err(anyhow::anyhow!("存储目录不能为空。"));
    }
    if !path.is_absolute() {
        return Err(anyhow::anyhow!("请输入完整的绝对路径：{}", path.display()));
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    Ok(normalized)
}

fn normalized_key(path: &Path) -> String {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    let key = normalized.to_string_lossy().replace('/', "\\");
    // 根目录末尾的反斜杠必须保留，其它目录可去掉末尾分隔符再比较。
    if key.len() > 3 {
        key.trim_end_matches('\\').to_lowercase()
    } else {
        key.to_lowercase()
    }
}

fn sidecar_path(database: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}{}", database.display(), suffix))
}

/// 旧版本遗留在数据目录里的右键菜单 DLL 及其临时文件。
fn shell_dll_artifacts(data_dir: &Path) -> Vec<PathBuf> {
    let directory = data_dir.join("shell");
    let dll = directory.join(crate::shell_menu::SHELL_DLL_NAME);
    let temp = directory.join(format!("{}.new", crate::shell_menu::SHELL_DLL_NAME));
    [dll, temp]
        .into_iter()
        .filter(|path| path.exists())
        .collect()
}

fn database_with_sidecars(database: &Path) -> [PathBuf; 3] {
    [
        database.to_path_buf(),
        sidecar_path(database, "-wal"),
        sidecar_path(database, "-shm"),
    ]
}

fn load_state_from(base: &Path) -> Result<(StorageState, bool)> {
    let location_path = base.join(LOCATION_FILE_NAME);
    if location_path.exists() {
        let content = fs::read_to_string(&location_path)
            .with_context(|| format!("无法读取路径指引：{}", location_path.display()))?;
        let state = serde_json::from_str(&content)
            .with_context(|| format!("路径指引格式错误：{}", location_path.display()))?;
        return Ok((state, true));
    }

    // 没有指引表示旧版本或首次运行。旧配置里的自定义 data_dir 必须继续生效。
    let legacy_config_path = base.join(CONFIG_FILE_NAME);
    let data_dir = if legacy_config_path.exists() {
        Config::read_from(&legacy_config_path)?.data_dir
    } else {
        base.to_path_buf()
    };
    Ok((
        StorageState {
            active: StorageLocation {
                config_dir: base.to_path_buf(),
                data_dir,
            },
            pending: None,
            previous: None,
            cleanup_files: Vec::new(),
            quiet_cleanup_files: Vec::new(),
        },
        false,
    ))
}

fn save_state_to(base: &Path, state: &StorageState) -> Result<()> {
    write_json_atomic(&base.join(LOCATION_FILE_NAME), state)
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut temporary = TempFile::for_target(path)?;
    write_new_json(&temporary.path, value)?;
    temporary.install(true)
}

fn write_new_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let json = serde_json::to_vec_pretty(value)?;
    let mut file = fs::File::options()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("无法创建临时文件：{}", path.display()))?;
    file.write_all(&json)?;
    file.sync_all()?;
    Ok(())
}

/// 同目录临时文件：改名前若任何步骤失败，Drop 会清理它。
struct TempFile {
    path: PathBuf,
    target: PathBuf,
    armed: bool,
}

impl TempFile {
    fn for_target(target: &Path) -> Result<Self> {
        let parent = target
            .parent()
            .ok_or_else(|| anyhow::anyhow!("目标路径没有父目录：{}", target.display()))?;
        let name = target.file_name().unwrap_or_default().to_string_lossy();
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        Ok(Self {
            path: parent.join(format!(".{name}.{}.{}.tmp", std::process::id(), sequence)),
            target: target.to_path_buf(),
            armed: true,
        })
    }

    fn install(&mut self, replace_existing: bool) -> Result<()> {
        if replace_existing {
            move_file_replace(&self.path, &self.target)
                .with_context(|| format!("无法替换目标文件：{}", self.target.display()))?;
        } else {
            // Windows 下 rename 不会覆盖已经存在的目标，符合“绝不覆盖”的约束。
            fs::rename(&self.path, &self.target)
                .with_context(|| format!("无法将临时文件改名为：{}", self.target.display()))?;
        }
        self.armed = false;
        Ok(())
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn move_file_replace(source: &Path, target: &Path) -> Result<()> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source_wide = crate::util::wide(&source.to_string_lossy());
    let target_wide = crate::util::wide(&target.to_string_lossy());
    unsafe {
        MoveFileExW(
            PCWSTR(source_wide.as_ptr()),
            PCWSTR(target_wide.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )?;
    }
    Ok(())
}

fn remove_created_files(paths: &[PathBuf]) {
    for path in paths {
        if let Err(error) = fs::remove_file(path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                log::warn!("无法清理未激活的新文件 {}：{}", path.display(), error);
            }
        }
    }
}

fn delete_cleanup_files(paths: &[PathBuf]) -> (Vec<PathBuf>, Vec<String>) {
    let mut remaining = Vec::new();
    let mut warnings = Vec::new();
    for path in paths {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                remaining.push(path.clone());
                warnings.push(format!("原文件暂未删除：{}（{}）", path.display(), error));
            }
        }
    }
    (remaining, warnings)
}

/// 静默删除附属文件：失败时不弹窗打扰用户，只写日志并留到下次启动重试。
fn delete_quiet_files(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut remaining = Vec::new();
    for path in paths {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                log::info!("附属文件暂未删除：{}（{}）", path.display(), error);
                remaining.push(path.clone());
            }
        }
    }
    // DLL 删掉后顺手移除空的 shell 目录，避免原位置留下空文件夹。
    // 目录里还有别的文件时 remove_dir 会失败，不影响任何数据。
    for path in paths {
        let is_shell_dir = path
            .parent()
            .and_then(|directory| directory.file_name())
            .map(|name| name.eq_ignore_ascii_case("shell"))
            .unwrap_or(false);
        if is_shell_dir {
            if let Some(directory) = path.parent() {
                let _ = fs::remove_dir(directory);
            }
        }
    }
    remaining
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// 每个测试使用独立目录，避免并行测试互相删除文件。
    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "file_origin_tracker_storage_{name}_{}_{}",
                std::process::id(),
                nonce
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn join(&self, path: &str) -> PathBuf {
            self.0.join(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn location(config_dir: PathBuf, data_dir: PathBuf) -> StorageLocation {
        StorageLocation {
            config_dir,
            data_dir,
        }
    }

    fn initialize_state(base: &Path, active: &StorageLocation, retention_days: i64) -> Config {
        let config = Config::default_with_data_dir(active.data_dir.clone());
        let mut config = config;
        config.retention_days = retention_days;
        write_json_atomic(&active.config_file(), &config).unwrap();
        save_state_to(
            base,
            &StorageState {
                active: active.clone(),
                pending: None,
                previous: None,
                cleanup_files: Vec::new(),
                quiet_cleanup_files: Vec::new(),
            },
        )
        .unwrap();
        config
    }

    #[test]
    fn legacy_config_location_remains_compatible() {
        let test = TestDirectory::new("legacy");
        let base = test.join("base");
        let legacy_data = test.join("legacy-data");
        let config = Config::default_with_data_dir(legacy_data.clone());
        write_json_atomic(&base.join(CONFIG_FILE_NAME), &config).unwrap();

        let (state, pointer_exists) = load_state_from(&base).unwrap();
        assert!(!pointer_exists);
        assert_eq!(state.active.config_dir, base);
        assert_eq!(state.active.data_dir, legacy_data);
        assert_eq!(load_active_config_at(&base).unwrap().data_dir, legacy_data);
    }

    #[test]
    fn migration_includes_wal_records_and_retries_cleanup() {
        let test = TestDirectory::new("migrate");
        let base = test.join("base");
        let active = location(test.join("old-config"), test.join("old-data"));
        let target = location(test.join("new-config"), test.join("new-data"));
        let config = initialize_state(&base, &active, 30);
        let unrelated = active.data_dir.join("keep-me.txt");
        // 模拟旧版本遗留在数据目录里的右键菜单 DLL。
        let stale_dll = active
            .data_dir
            .join("shell")
            .join(crate::shell_menu::SHELL_DLL_NAME);
        fs::create_dir_all(stale_dll.parent().unwrap()).unwrap();
        fs::write(&stale_dll, "stale dll").unwrap();

        // 保持原连接打开，确保提交记录仍可能位于 WAL，而不是只复制主 db 文件。
        let source_database = Database::init(&active.data_dir).unwrap();
        fs::write(&unrelated, "not a program file").unwrap();
        source_database
            .upsert_origin(r"C:\wal-record.txt", "writer.exe", r"C:\writer.exe", false)
            .unwrap();

        schedule_switch_at(&base, target.clone(), SwitchMode::Migrate, &config).unwrap();
        let report = apply_pending_switch_at(&base).unwrap();
        assert!(report.switched);

        // “迁移数据”也要记录切换前的位置，设置页才能显示原位置。
        let switched_state = load_state_from(&base).unwrap().0;
        assert_eq!(switched_state.previous, Some(active.clone()));

        let migrated = Database::open_readonly(&target.data_dir).unwrap();
        let records = migrated.recent_filtered(50, None).unwrap();
        assert!(records
            .iter()
            .any(|record| record.file_path == r"C:\wal-record.txt"));
        drop(migrated);

        // 原连接在 Windows 上可能暂时阻止删除；激活新位置不能因此回滚。
        drop(source_database);
        let cleanup_report = apply_pending_switch_at(&base).unwrap();
        assert!(!cleanup_report.switched);
        assert!(!active.config_file().exists());
        for path in database_with_sidecars(&active.database_file()) {
            assert!(!path.exists(), "原文件应已清理：{}", path.display());
        }
        assert!(unrelated.exists(), "目录中的用户文件不能被删除");
        assert!(!stale_dll.exists(), "旧版右键菜单 DLL 应已清理");
        assert!(
            !stale_dll.parent().unwrap().exists(),
            "空的 shell 目录应一并移除"
        );

        let state = load_state_from(&base).unwrap().0;
        assert_eq!(state.active, target);
        assert!(state.pending.is_none());
    }

    #[test]
    fn fresh_start_keeps_settings_old_data_and_only_one_previous_location() {
        let test = TestDirectory::new("fresh");
        let base = test.join("base");
        let first = location(test.join("first-config"), test.join("first-data"));
        let second = location(test.join("second-config"), test.join("second-data"));
        let third_dir = test.join("third-shared");
        let third = location(third_dir.clone(), third_dir);
        let config = initialize_state(&base, &first, 42);

        let old_database = Database::init(&first.data_dir).unwrap();
        old_database
            .upsert_origin(r"C:\old.txt", "old.exe", r"C:\old.exe", false)
            .unwrap();
        drop(old_database);

        schedule_switch_at(&base, second.clone(), SwitchMode::Fresh, &config).unwrap();
        apply_pending_switch_at(&base).unwrap();

        let new_config = Config::read_from(&second.config_file()).unwrap();
        assert_eq!(new_config.retention_days, 42);
        assert_eq!(new_config.data_dir, second.data_dir);
        let new_database = Database::open_readonly(&second.data_dir).unwrap();
        assert!(new_database.recent_filtered(50, None).unwrap().is_empty());
        drop(new_database);
        let old_database = Database::open_readonly(&first.data_dir).unwrap();
        assert_eq!(old_database.recent_filtered(50, None).unwrap().len(), 1);
        drop(old_database);
        assert_eq!(load_state_from(&base).unwrap().0.previous, Some(first));

        // 第二次重新开始会覆盖原位置提示，但上一次的空数据库仍保留。
        let current = load_active_config_at(&base).unwrap();
        schedule_switch_at(&base, third.clone(), SwitchMode::Fresh, &current).unwrap();
        apply_pending_switch_at(&base).unwrap();
        let state = load_state_from(&base).unwrap().0;
        assert_eq!(state.active, third);
        assert_eq!(state.previous, Some(second.clone()));
        assert!(second.database_file().exists());
    }

    #[test]
    fn existing_target_files_are_never_overwritten() {
        let test = TestDirectory::new("conflict");
        let base = test.join("base");
        let active_dir = test.join("active");
        let active = location(active_dir.clone(), active_dir);
        let config = initialize_state(&base, &active, 7);
        let target_dir = test.join("target");
        let target = location(target_dir.clone(), target_dir.clone());
        fs::create_dir_all(&target_dir).unwrap();
        fs::write(target.config_file(), "keep this config").unwrap();

        let error =
            schedule_switch_at(&base, target.clone(), SwitchMode::Migrate, &config).unwrap_err();
        assert!(error.to_string().contains("未覆盖"));
        assert_eq!(
            fs::read_to_string(target.config_file()).unwrap(),
            "keep this config"
        );
        assert!(load_state_from(&base).unwrap().0.pending.is_none());

        fs::remove_file(target.config_file()).unwrap();
        fs::write(target.database_file(), "keep this database").unwrap();
        let error = schedule_switch_at(&base, target, SwitchMode::Migrate, &config).unwrap_err();
        assert!(error.to_string().contains("未覆盖"));
        assert_eq!(
            fs::read_to_string(target_dir.join(DATABASE_FILE_NAME)).unwrap(),
            "keep this database"
        );
    }

    #[test]
    fn failed_switch_returns_to_old_location_and_removes_new_files() {
        let test = TestDirectory::new("failure");
        let base = test.join("base");
        let active_dir = test.join("active");
        let active = location(active_dir.clone(), active_dir);
        let target_dir = test.join("target");
        let target = location(target_dir.clone(), target_dir);
        let config = initialize_state(&base, &active, 7);

        // 故意不创建原数据库，迁移会在导出快照时失败。
        schedule_switch_at(&base, target.clone(), SwitchMode::Migrate, &config).unwrap();
        assert!(apply_pending_switch_at(&base).is_err());

        let state = load_state_from(&base).unwrap().0;
        assert_eq!(state.active, active);
        assert!(state.pending.is_none());
        assert!(state.active.config_file().exists());
        assert!(!target.config_file().exists());
        assert!(!target.database_file().exists());
    }

    #[test]
    fn independent_mode_can_move_only_the_database() {
        let test = TestDirectory::new("database-only");
        let base = test.join("base");
        let active = location(test.join("config"), test.join("old-data"));
        let target = location(active.config_dir.clone(), test.join("new-data"));
        let config = initialize_state(&base, &active, 19);
        let database = Database::init(&active.data_dir).unwrap();
        database
            .upsert_origin(r"C:\move-db.txt", "app.exe", r"C:\app.exe", false)
            .unwrap();
        drop(database);

        schedule_switch_at(&base, target.clone(), SwitchMode::Migrate, &config).unwrap();
        apply_pending_switch_at(&base).unwrap();

        let state = load_state_from(&base).unwrap().0;
        assert_eq!(state.active, target);
        assert_eq!(
            Config::read_from(&active.config_file())
                .unwrap()
                .retention_days,
            19
        );
        assert!(!active.database_file().exists());
        let moved = Database::open_readonly(&state.active.data_dir).unwrap();
        assert_eq!(moved.recent_filtered(50, None).unwrap().len(), 1);
    }

    #[test]
    fn damaged_location_file_never_triggers_file_deletion() {
        let test = TestDirectory::new("damaged-pointer");
        let base = test.join("base");
        let active_dir = test.join("active");
        let active = location(active_dir.clone(), active_dir);
        initialize_state(&base, &active, 7);
        fs::write(base.join(LOCATION_FILE_NAME), "not valid json").unwrap();

        assert!(apply_pending_switch_at(&base).is_err());
        assert!(active.config_file().exists());
    }

    #[test]
    fn fresh_start_rejects_unchanged_database_directory() {
        let test = TestDirectory::new("same-data");
        let base = test.join("base");
        let active_dir = test.join("active");
        let active = location(active_dir.clone(), active_dir.clone());
        let config = initialize_state(&base, &active, 7);
        let target = location(test.join("new-config"), active_dir);

        let error = schedule_switch_at(&base, target, SwitchMode::Fresh, &config).unwrap_err();
        assert!(error.to_string().contains("必须选择不同的数据库目录"));
        assert!(load_state_from(&base).unwrap().0.pending.is_none());
    }
}
