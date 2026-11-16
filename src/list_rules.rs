//! 黑白名单规则。
//!
//! 用户可以在「名单管理」窗口里维护两类条目：
//! - **程序**：按程序完整路径精确匹配（例如 `C:\Windows\System32\notepad.exe`）
//! - **文件夹**：按目录前缀匹配（例如 `D:\下载`，它会连带子目录一起生效）
//!
//! 两种模式的含义：
//! - **黑名单（black）**：名单里的程序、以及名单文件夹里产生的文件都不记录
//! - **白名单（white）**：只记录名单里的程序、以及名单文件夹里产生的文件
//!
//! 规则会被 ETW 写库线程周期性重新加载（见 `etw.rs`），
//! 因此用户在界面上增删条目后不需要重启程序。

use std::time::{Duration, Instant};

use crate::db::Database;

/// 黑名单模式。
pub const MODE_BLACK: &str = "black";
/// 白名单模式。
pub const MODE_WHITE: &str = "white";

/// 规则缓存有效期：写库线程每隔这么久重新读一次数据库。
///
/// 不能每条事件都查库（文件事件非常密集），也不能永不刷新
/// （用户在界面上改完名单要立刻生效）。
pub const RELOAD_INTERVAL: Duration = Duration::from_secs(2);

/// 一份内存中的名单快照。
#[derive(Debug, Clone, Default)]
pub struct ListRules {
    /// 是否为白名单模式。
    white_mode: bool,
    /// 程序条目（已转成小写，便于忽略大小写比较）。
    programs: Vec<String>,
    /// 文件夹条目（已转成小写、去掉末尾反斜杠）。
    folders: Vec<String>,
    /// 上次从数据库加载的时间。
    loaded_at: Option<Instant>,
}

impl ListRules {
    /// 从数据库加载指定模式的名单。
    pub fn load(db: &Database, mode: &str) -> Self {
        let mut rules = ListRules {
            white_mode: mode == MODE_WHITE,
            ..Default::default()
        };

        match db.list_entries(mode) {
            Ok(entries) => {
                for entry in entries {
                    if entry.is_folder {
                        rules.folders.push(normalize_folder(&entry.path));
                    } else {
                        rules.programs.push(entry.path.to_lowercase());
                    }
                }
            }
            Err(error) => {
                // 读不到名单时按“空名单”处理：黑名单=全部记录，白名单=全部不记录。
                log::warn!("[名单] 读取名单失败：{:#}", error);
            }
        }

        rules.loaded_at = Some(Instant::now());
        rules
    }

    /// 距离上次加载是否已经超过刷新间隔。
    pub fn needs_reload(&self) -> bool {
        match self.loaded_at {
            Some(loaded_at) => loaded_at.elapsed() >= RELOAD_INTERVAL,
            None => true,
        }
    }

    /// 名单里是否一条都没有。
    pub fn is_empty(&self) -> bool {
        self.programs.is_empty() && self.folders.is_empty()
    }

    /// 当前是否为白名单模式。
    pub fn is_white_mode(&self) -> bool {
        self.white_mode
    }

    /// 这条文件事件是否应该被记录。
    ///
    /// - 黑名单：命中程序或文件夹 → 不记录
    /// - 白名单：只有命中程序或文件夹才记录
    pub fn allows(&self, file_path: &str, process_path: &str) -> bool {
        let program_hit = self
            .programs
            .iter()
            .any(|program| program.eq_ignore_ascii_case(process_path));
        let folder_hit = self
            .folders
            .iter()
            .any(|folder| path_under_folder(file_path, folder));

        let listed = program_hit || folder_hit;
        if self.white_mode {
            listed
        } else {
            !listed
        }
    }
}

/// 把文件夹路径统一成小写、去掉末尾反斜杠的形式，方便前缀比较。
fn normalize_folder(path: &str) -> String {
    path.trim().trim_end_matches(['\\', '/']).to_lowercase()
}

/// 判断 `file_path` 是否位于 `folder` 目录（含子目录）之内。
///
/// 只做前缀比较会误判 `D:\下载备份` 属于 `D:\下载`，
/// 所以要求两个路径在文件夹长度处必须以反斜杠分隔。
fn path_under_folder(file_path: &str, folder: &str) -> bool {
    if folder.is_empty() {
        return false;
    }
    let file = file_path.to_lowercase();
    if !file.starts_with(folder) {
        return false;
    }
    // 长度相同说明就是文件夹本身（例如新建了这个文件夹），也算命中。
    match file.as_bytes().get(folder.len()) {
        None => true,
        Some(b'\\') | Some(b'/') => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(white: bool, programs: &[&str], folders: &[&str]) -> ListRules {
        ListRules {
            white_mode: white,
            programs: programs.iter().map(|value| value.to_lowercase()).collect(),
            folders: folders
                .iter()
                .map(|value| normalize_folder(value))
                .collect(),
            loaded_at: Some(Instant::now()),
        }
    }

    #[test]
    fn blacklist_blocks_programs_and_folders() {
        let rules = rules(false, &[r"C:\Windows\notepad.exe"], &[r"D:\下载"]);
        assert!(!rules.allows(r"C:\Windows\Temp\a.txt", r"C:\WINDOWS\NOTEPAD.EXE"));
        assert!(!rules.allows(r"D:\下载\a.zip", r"C:\Windows\explorer.exe"));
        assert!(!rules.allows(r"D:\下载\2026\b.zip", r"C:\Windows\explorer.exe"));
        assert!(rules.allows(r"D:\下载备份\a.zip", r"C:\Windows\explorer.exe"));
        assert!(rules.allows(r"E:\工作\xiangmu.docx", r"C:\Windows\explorer.exe"));
    }

    #[test]
    fn whitelist_only_allows_listed() {
        let rules = rules(true, &[r"C:\Windows\notepad.exe"], &[r"D:\工作"]);
        assert!(rules.allows(r"E:\任意.txt", r"C:\Windows\Notepad.exe"));
        assert!(rules.allows(r"D:\工作\a.txt", r"C:\Windows\explorer.exe"));
        assert!(!rules.allows(r"D:\下载\a.zip", r"C:\Windows\explorer.exe"));
    }
}
