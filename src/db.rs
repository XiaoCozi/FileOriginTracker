//! SQLite 数据库模块
//!
//! 负责持久化存储"哪个进程创建了哪个文件/文件夹"的映射关系。
//! 使用 rusqlite (bundled) 内嵌 SQLite，无需额外安装。
//!
//! 表结构：
//! - file_origins : 存储文件路径 -> 创建进程 的映射

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;

/// 数据库包装器（线程安全，内部用 Mutex 保护连接）
#[derive(Clone)]
pub struct Database {
    conn: std::sync::Arc<Mutex<Connection>>,
}

/// 一条文件来源记录
#[derive(Debug, Clone)]
pub struct FileOrigin {
    /// 文件/文件夹的完整路径
    pub file_path: String,
    /// 创建它的进程名（如 "notepad.exe"）
    pub process_name: String,
    /// 创建它的进程路径（如 "C:\Windows\System32\notepad.exe"）
    pub process_path: String,
    /// 创建时间戳（Unix 秒），修改记录里是最后修改时间
    pub created_at: i64,
    /// 事件类型："create_file"（新建文件）、"create_dir"（新建文件夹）、
    /// "modify"（文件被修改）。列表里用来显示类型列。
    pub action: String,
    /// 用户给这个程序起的备注（没有备注时为空字符串）。
    ///
    /// 备注按“程序完整路径”保存，所以同一个程序名的不同版本可以分别备注，
    /// 之后这个程序产生的每条记录都会带上它。
    pub note: String,
}

impl Database {
    /// 初始化数据库（创建连接 + 建表）
    pub fn init(data_dir: &Path) -> Result<Self> {
        // 确保数据目录存在
        std::fs::create_dir_all(data_dir)?;
        let db_path = data_dir.join("file_origins.db");

        // 打开 SQLite 连接
        let conn = Connection::open(&db_path)
            .with_context(|| format!("无法打开数据库: {}", db_path.display()))?;

        // 启用 WAL 模式（Write-Ahead Logging）
        // WAL 模式让读取和写入可以并发，性能更好
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        conn.execute_batch("PRAGMA synchronous=NORMAL;")?;

        // 创建表结构
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS file_origins (
                file_path     TEXT PRIMARY KEY,       -- 文件路径作为主键（唯一）
                process_name  TEXT NOT NULL,           -- 进程名
                process_path  TEXT NOT NULL,           -- 进程完整路径
                created_at    INTEGER NOT NULL,        -- 创建时间戳
                is_directory  INTEGER NOT NULL DEFAULT 0  -- 1 表示这是文件夹
            );

            -- 文件修改记录：每个路径只保留最后一次修改，避免无限膨胀
            CREATE TABLE IF NOT EXISTS file_modifications (
                file_path     TEXT PRIMARY KEY,
                process_name  TEXT NOT NULL,
                process_path  TEXT NOT NULL,
                modified_at   INTEGER NOT NULL
            );

            -- 为进程名建索引，方便按程序查询
            CREATE INDEX IF NOT EXISTS idx_process_name 
                ON file_origins(process_name);

            -- 为创建时间建索引，方便排序和清理
            CREATE INDEX IF NOT EXISTS idx_created_at 
                ON file_origins(created_at);

            CREATE INDEX IF NOT EXISTS idx_modified_at
                ON file_modifications(modified_at);

            -- 程序备注：给某个程序（按完整路径区分）加一句说明，
            -- 之后该程序的所有监控记录都会显示这句备注。
            CREATE TABLE IF NOT EXISTS program_notes (
                process_path TEXT PRIMARY KEY,
                note         TEXT NOT NULL,
                updated_at   INTEGER NOT NULL
            );
            "#,
        )?;

        // 旧版本数据库没有 is_directory 列，这里补上。
        // 列已存在时 SQLite 会报错，直接忽略即可。
        let _ = conn.execute(
            "ALTER TABLE file_origins ADD COLUMN is_directory INTEGER NOT NULL DEFAULT 0",
            [],
        );

        Ok(Database {
            conn: std::sync::Arc::new(Mutex::new(conn)),
        })
    }

    /// 以只读方式打开已有数据库（供右键菜单扩展和记录窗口使用）。
    /// 数据库不存在时返回错误，调用方自行降级处理。
    pub fn open_readonly(data_dir: &Path) -> Result<Self> {
        let db_path = data_dir.join("file_origins.db");
        if !db_path.exists() {
            return Err(anyhow::anyhow!("数据库尚未创建: {}", db_path.display()));
        }

        let conn = Connection::open_with_flags(
            &db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("无法只读打开数据库: {}", db_path.display()))?;

        Ok(Database {
            conn: std::sync::Arc::new(Mutex::new(conn)),
        })
    }

    /// 取出连接锁。即使其他线程发生过 panic，也继续使用内部连接，
    /// 避免因为一把“中毒”的锁让整个程序不可用。
    fn lock_connection(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 插入或更新一条“新建文件/文件夹”记录（upsert）
    /// 如果文件路径已存在，更新它的进程信息
    pub fn upsert_origin(
        &self,
        file_path: &str,
        process_name: &str,
        process_path: &str,
        is_directory: bool,
    ) -> Result<()> {
        let conn = self.lock_connection();
        let now = chrono_now();

        conn.execute(
            r#"
            INSERT INTO file_origins (file_path, process_name, process_path, created_at, is_directory)
            VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(file_path) DO UPDATE SET
                process_name  = excluded.process_name,
                process_path  = excluded.process_path,
                created_at    = excluded.created_at,
                is_directory  = excluded.is_directory
            "#,
            params![
                file_path,
                process_name,
                process_path,
                now,
                if is_directory { 1 } else { 0 }
            ],
        )?;

        Ok(())
    }

    /// 插入或更新一条“文件被修改”记录。
    /// 同一个文件只保留最后一次修改，避免数据库被写入事件撑爆。
    pub fn upsert_modification(
        &self,
        file_path: &str,
        process_name: &str,
        process_path: &str,
    ) -> Result<()> {
        let conn = self.lock_connection();
        let now = chrono_now();

        conn.execute(
            r#"
            INSERT INTO file_modifications (file_path, process_name, process_path, modified_at)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(file_path) DO UPDATE SET
                process_name = excluded.process_name,
                process_path = excluded.process_path,
                modified_at  = excluded.modified_at
            "#,
            params![file_path, process_name, process_path, now],
        )?;

        Ok(())
    }

    /// 根据文件路径查询它的创建来源
    pub fn get_origin(&self, file_path: &str) -> Result<Option<FileOrigin>> {
        let conn = self.lock_connection();

        let mut stmt = conn.prepare(
            "SELECT o.file_path, o.process_name, o.process_path, o.created_at, o.is_directory,
                    COALESCE(n.note, '')
             FROM file_origins o
             LEFT JOIN program_notes n ON n.process_path = o.process_path
             WHERE o.file_path = ?1",
        )?;

        let result = stmt.query_row(params![file_path], |row| {
            let is_directory: i64 = row.get(4)?;
            Ok(FileOrigin {
                file_path: row.get(0)?,
                process_name: row.get(1)?,
                process_path: row.get(2)?,
                created_at: row.get(3)?,
                action: if is_directory != 0 {
                    "create_dir".to_string()
                } else {
                    "create_file".to_string()
                },
                note: row.get(5)?,
            })
        });

        match result {
            Ok(origin) => Ok(Some(origin)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// 获取总记录数
    pub fn count(&self) -> Result<i64> {
        let conn = self.lock_connection();
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM file_origins", [], |row| row.get(0))?;
        Ok(count)
    }

    /// 按时间倒序取出最近若干条记录，用于“最近记录”窗口。
    ///
    /// 新建记录和修改记录在两张表里，这里用 UNION ALL 合并后统一按时间排序，
    /// `action` 列告诉界面这是新建文件、新建文件夹还是修改文件。
    pub fn recent(&self, limit: u32) -> Result<Vec<FileOrigin>> {
        let conn = self.lock_connection();
        // 过滤掉没有程序信息的空记录：这类记录没有追踪价值，
        // 只会在列表里显示成一堆只有时间、内容空白的行。
        let mut stmt = conn.prepare(
            r#"
            SELECT t.file_path, t.process_name, t.process_path, t.created_at, t.action,
                   COALESCE(n.note, '')
            FROM (
                SELECT file_path, process_name, process_path, created_at,
                       CASE WHEN is_directory <> 0 THEN 'create_dir' ELSE 'create_file' END AS action
                FROM file_origins
                UNION ALL
                SELECT file_path, process_name, process_path, modified_at AS created_at,
                       'modify' AS action
                FROM file_modifications
            ) AS t
            LEFT JOIN program_notes n ON n.process_path = t.process_path
            WHERE t.file_path <> '' AND t.process_name <> '' AND t.process_path <> ''
            ORDER BY t.created_at DESC
            LIMIT ?1
            "#,
        )?;

        let rows = stmt.query_map(params![limit], |row| {
            Ok(FileOrigin {
                file_path: row.get(0)?,
                process_name: row.get(1)?,
                process_path: row.get(2)?,
                created_at: row.get(3)?,
                action: row.get(4)?,
                note: row.get(5)?,
            })
        })?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// 清理 N 天前的旧记录（防止数据库无限增长）
    pub fn cleanup_old(&self, days: i64) -> Result<i64> {
        let conn = self.lock_connection();
        let now = chrono_now();
        let cutoff = now - days * 24 * 3600;

        let origins = conn.execute(
            "DELETE FROM file_origins WHERE created_at < ?1",
            params![cutoff],
        )?;
        let modifications = conn.execute(
            "DELETE FROM file_modifications WHERE modified_at < ?1",
            params![cutoff],
        )?;

        Ok((origins + modifications) as i64)
    }

    /// 日常维护：按保留天数删除旧记录，并在确实删过数据时压缩数据库文件。
    ///
    /// SQLite 删除行后不会自动把空间还给磁盘，必须执行 VACUUM 重建数据库，
    /// 文件才会真正变小。VACUUM 耗时与文件大小成正比，所以只在有删除时执行。
    pub fn maintain(&self, retain_days: i64) -> Result<i64> {
        // 限制在 1~3650 天，避免配置成 0 或负数把记录全部清空。
        let days = retain_days.clamp(1, 3650);
        let deleted = self.cleanup_old(days)?;

        if deleted > 0 {
            let conn = self.lock_connection();
            // 压缩失败（例如资源管理器正持有只读连接）不影响主流程，
            // 下次维护时会再次尝试。
            let _ = conn.execute_batch("VACUUM;");
        }

        Ok(deleted)
    }

    /// 保存（或更新）某个程序的备注。
    ///
    /// 备注按程序完整路径保存；传入空字符串表示删除这条备注。
    /// 之所以用完整路径而不是文件名，是因为不同目录下可能有同名程序。
    pub fn set_program_note(&self, process_path: &str, note: &str) -> Result<()> {
        let conn = self.lock_connection();
        let note = note.trim();

        if note.is_empty() {
            conn.execute(
                "DELETE FROM program_notes WHERE process_path = ?1",
                params![process_path],
            )?;
            return Ok(());
        }

        conn.execute(
            "INSERT INTO program_notes(process_path, note, updated_at)
             VALUES(?1, ?2, ?3)
             ON CONFLICT(process_path) DO UPDATE SET
                 note = excluded.note,
                 updated_at = excluded.updated_at",
            params![process_path, note, chrono_now()],
        )?;
        Ok(())
    }

    /// 读取某个程序当前的备注（没有时返回空字符串）。
    pub fn program_note(&self, process_path: &str) -> Result<String> {
        let conn = self.lock_connection();
        let mut stmt = conn.prepare("SELECT note FROM program_notes WHERE process_path = ?1")?;
        let note = stmt
            .query_row(params![process_path], |row| row.get::<_, String>(0))
            .optional()?
            .unwrap_or_default();
        Ok(note)
    }
}

/// 获取当前 Unix 时间戳（秒）
fn chrono_now() -> i64 {
    // 用简单的方式获取时间戳，不引入 chrono crate
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证“按保留天数清理 + 压缩”能删掉过期记录、保留新记录。
    #[test]
    fn maintain_removes_expired_rows() {
        let dir = std::env::temp_dir().join("file_origin_tracker_db_test");
        let _ = std::fs::remove_dir_all(&dir);
        let db = Database::init(&dir).unwrap();

        // 写入 50 条 30 天前的记录
        {
            let conn = db.lock_connection();
            let old_time = chrono_now() - 30 * 86400;
            for index in 0..50 {
                conn.execute(
                    "INSERT INTO file_origins(file_path, process_name, process_path, created_at)
                     VALUES(?1, 'old.exe', 'C:\\old.exe', ?2)",
                    params![format!(r"C:\old\file{}.tmp", index), old_time],
                )
                .unwrap();
            }
        }

        // 再写入 1 条今天的记录
        db.upsert_origin(r"C:\new\file.txt", "new.exe", r"C:\new.exe", false)
            .unwrap();
        db.upsert_modification(r"C:\new\file.txt", "editor.exe", r"C:\editor.exe")
            .unwrap();

        // 保留 7 天：应该只删掉那 50 条旧记录
        let deleted = db.maintain(7).unwrap();
        assert_eq!(deleted, 50);
        assert_eq!(db.count().unwrap(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 验证「程序备注」：设置后备注会跟着这个程序的所有记录一起返回，
    /// 清空备注后记录里不再带备注。
    #[test]
    fn program_note_follows_records() {
        let dir = std::env::temp_dir().join("file_origin_tracker_note_test");
        let _ = std::fs::remove_dir_all(&dir);
        let db = Database::init(&dir).unwrap();

        db.upsert_origin(
            r"C:\note\a.txt",
            "notepad.exe",
            r"C:\Windows\notepad.exe",
            false,
        )
        .unwrap();

        // 一开始没有备注。
        let rows = db.recent(10).unwrap();
        assert_eq!(rows[0].note, "");

        // 设置备注后，查询结果里带上备注。
        db.set_program_note(r"C:\Windows\notepad.exe", "我自己启动的记事本")
            .unwrap();
        let rows = db.recent(10).unwrap();
        assert_eq!(rows[0].note, "我自己启动的记事本");
        assert_eq!(
            db.get_origin(r"C:\note\a.txt").unwrap().unwrap().note,
            "我自己启动的记事本"
        );

        // 「最近记录」窗口用的是只读连接：另开一个连接写入备注后，
        // 只读连接再次查询也应该立刻看到新备注（WAL 模式下已提交的数据对读者可见）。
        let reader = Database::open_readonly(&dir).unwrap();
        db.set_program_note(r"C:\Windows\notepad.exe", "只读连接也要看到")
            .unwrap();
        assert_eq!(reader.recent(10).unwrap()[0].note, "只读连接也要看到");

        // 空字符串表示删除备注。
        db.set_program_note(r"C:\Windows\notepad.exe", "").unwrap();
        let rows = db.recent(10).unwrap();
        assert_eq!(rows[0].note, "");
        assert_eq!(db.program_note(r"C:\Windows\notepad.exe").unwrap(), "");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
