//! SQLite 数据库模块
//!
//! 负责持久化存储"哪个进程创建了哪个文件/文件夹"的映射关系。
//! 使用 rusqlite (bundled) 内嵌 SQLite，无需额外安装。
//!
//! 表结构：
//! - file_origins : 存储文件路径 -> 创建进程 的映射

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 数据库包装器（线程安全，内部用 Mutex 保护连接）
#[derive(Clone)]
pub struct Database {
    conn: std::sync::Arc<Mutex<Connection>>,
}

/// 一条文件来源记录
#[derive(Debug, Clone, PartialEq)]
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
    /// "modify"（文件被修改）、"delete_file"（删除文件）、"delete_dir"（删除文件夹）。
    /// 列表里用来显示类型列。
    pub action: String,
    /// 用户给这个程序起的备注（没有备注时为空字符串）。
    ///
    /// 备注按“程序完整路径”保存，所以同一个程序名的不同版本可以分别备注，
    /// 之后这个程序产生的每条记录都会带上它。
    pub note: String,
}

/// 按「程序」汇总的统计结果，供「程序统计」窗口使用。
///
/// 分组依据是程序完整路径：同名的两个程序（例如装在两个目录里的 app.exe）
/// 各自算一行，避免把不同软件的数字混在一起。
#[derive(Debug, Clone, PartialEq)]
pub struct ProgramStat {
    /// 进程名，例如 "explorer.exe"
    pub process_name: String,
    /// 进程完整路径，同时作为统计的分组键
    pub process_path: String,
    /// 用户给这个程序写的备注（没有时为空字符串）
    pub note: String,
    /// 创建文件的次数
    pub create_file: i64,
    /// 创建文件夹的次数
    pub create_dir: i64,
    /// 修改文件的次数
    pub modify: i64,
    /// 删除文件的次数
    pub delete_file: i64,
    /// 删除文件夹的次数
    pub delete_dir: i64,
}

impl ProgramStat {
    /// 五类事件的总数。
    pub fn total(&self) -> i64 {
        self.create_file + self.create_dir + self.modify + self.delete_file + self.delete_dir
    }

    /// 按动作名取对应的计数。
    ///
    /// 统计窗口的「创建文件」到「删除文件夹」五列都通过它取数，
    /// 避免在界面代码里写一大串 match。
    pub fn count_of(&self, action: &str) -> i64 {
        match action {
            "create_file" => self.create_file,
            "create_dir" => self.create_dir,
            "modify" => self.modify,
            "delete_file" => self.delete_file,
            "delete_dir" => self.delete_dir,
            _ => 0,
        }
    }
}

/// 用 `PRAGMA quick_check` 快速判断数据库能否正常读取。
///
/// 返回 `false` 只表示“文件确实坏了”，**不会**把“被其它进程占用”误判为损坏：
/// 遇到锁等待超时之类的错误一律当作健康，避免误伤正常数据库。
///
/// `quick_check` 比 `integrity_check` 快得多（跳过索引与内容逐项校验），
/// 每次启动执行一次，开销可以忽略。
fn database_is_healthy(path: &Path) -> bool {
    let conn = match Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(conn) => conn,
        // 连打开都失败时，只有“不是数据库 / 已损坏”才算坏；
        // 其余（如被占用、权限不足）先假定健康，交给后面的正常流程报错。
        Err(error) => return !is_corruption_error(&error),
    };

    // 等待最多 3 秒，尽量排除“刚好在被别的进程写入”这种情况。
    let _ = conn.busy_timeout(Duration::from_secs(3));

    match conn.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0)) {
        Ok(result) => result.eq_ignore_ascii_case("ok"),
        Err(error) => !is_corruption_error(&error),
    }
}

/// 判断 SQLite 错误是否属于“文件损坏”这一类。
fn is_corruption_error(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(inner, _)
            if inner.code == rusqlite::ErrorCode::DatabaseCorrupt
                || inner.code == rusqlite::ErrorCode::NotADatabase
    )
}

/// 把损坏的数据库文件改名为 `file_origins.db.corrupt-<时间戳>` 保留下来。
///
/// 只改名不删除，用户需要时还能从备份里抢救数据；
/// 同时把 WAL / SHM 两个附属文件一起挪走，否则新建的空库会读到旧的未提交数据。
fn quarantine_database(db_path: &Path) {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let backup = db_path.with_file_name(format!("file_origins.db.corrupt-{stamp}"));
    let _ = std::fs::rename(db_path, &backup);

    for suffix in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{}", db_path.display(), suffix));
        if side.exists() {
            let side_backup = PathBuf::from(format!("{}.corrupt-{stamp}", side.display()));
            let _ = std::fs::rename(&side, &side_backup);
        }
    }

    log::error!(
        "检测到数据库损坏，已备份为 {} 并重建空库",
        backup.display()
    );
}

impl Database {
    /// 初始化数据库（创建连接 + 建表）
    pub fn init(data_dir: &Path) -> Result<Self> {
        // 确保数据目录存在
        std::fs::create_dir_all(data_dir)?;
        let db_path = data_dir.join("file_origins.db");

        // 启动自检：数据库文件已经存在时，先判断它是否完好。
        // 如果检测到文件损坏（例如异常断电导致 WAL 写坏），把整个库改名备份、
        // 再重建一个空库，避免程序因为打不开数据库而直接起不来。
        if db_path.exists() && !database_is_healthy(&db_path) {
            quarantine_database(&db_path);
        }

        // 打开 SQLite 连接
        let conn = Connection::open(&db_path)
            .with_context(|| format!("无法打开数据库: {}", db_path.display()))?;

        // 启用 WAL 模式（Write-Ahead Logging）
        // WAL 模式让读取和写入可以并发，性能更好
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
        // 主程序写库和界面删除记录可能同时发生。设置 3 秒的锁等待，
        // 避免偶发的同时写入直接报 “database is locked”。
        conn.execute_batch("PRAGMA busy_timeout=3000;")?;

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

            -- 删除记录：和新建记录分开存。
            -- 如果复用 file_origins，删除事件会把“谁创建的”这条记录覆盖掉，
            -- 所以单独建表，让创建和删除两条信息都能保留。
            CREATE TABLE IF NOT EXISTS file_deletions (
                file_path     TEXT PRIMARY KEY,
                process_name  TEXT NOT NULL,
                process_path  TEXT NOT NULL,
                deleted_at    INTEGER NOT NULL,
                is_directory  INTEGER NOT NULL DEFAULT 0
            );

            -- 为进程名建索引，方便按程序查询
            CREATE INDEX IF NOT EXISTS idx_process_name 
                ON file_origins(process_name);

            -- 为创建时间建索引，方便排序和清理
            CREATE INDEX IF NOT EXISTS idx_created_at 
                ON file_origins(created_at);

            CREATE INDEX IF NOT EXISTS idx_modified_at
                ON file_modifications(modified_at);

            CREATE INDEX IF NOT EXISTS idx_deleted_at
                ON file_deletions(deleted_at);

            -- 程序备注：给某个程序（按完整路径区分）加一句说明，
            -- 之后该程序的所有监控记录都会显示这句备注。
            CREATE TABLE IF NOT EXISTS program_notes (
                process_path TEXT PRIMARY KEY,
                note         TEXT NOT NULL,
                updated_at   INTEGER NOT NULL
            );

            -- 黑白名单：mode 区分黑名单(black)和白名单(white)，
            -- kind 区分程序(program)和文件夹(folder)，同一条目只存一份。
            CREATE TABLE IF NOT EXISTS list_entries (
                mode     TEXT NOT NULL,
                path     TEXT NOT NULL,
                kind     TEXT NOT NULL,
                added_at INTEGER NOT NULL,
                PRIMARY KEY (mode, path)
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

    /// 插入或更新一条“删除文件 / 删除文件夹”记录。
    /// 同一个路径只保留最后一次删除，避免重复删除事件把表撑大。
    pub fn upsert_deletion(
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
            INSERT INTO file_deletions
                (file_path, process_name, process_path, deleted_at, is_directory)
            VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(file_path) DO UPDATE SET
                process_name = excluded.process_name,
                process_path = excluded.process_path,
                deleted_at   = excluded.deleted_at,
                is_directory = excluded.is_directory
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

    /// 这个路径曾经作为文件夹被记录过吗？
    ///
    /// 删除事件处理时文件夹可能已经从磁盘上消失，`Path::is_dir()` 就拿不到真实类型了。
    /// 这时查一下新建记录里保存的 is_directory，能把“删除文件夹”正确区分出来。
    pub fn path_is_known_directory(&self, file_path: &str) -> bool {
        let conn = self.lock_connection();
        conn.query_row(
            "SELECT is_directory FROM file_origins WHERE file_path = ?1",
            params![file_path],
            |row| row.get::<_, i64>(0),
        )
        .map(|value| value != 0)
        .unwrap_or(false)
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
        self.recent_filtered(limit, None)
    }

    /// 与 [`Database::recent`] 相同，但可以限定时间范围（Unix 秒，含首尾）。
    ///
    /// `range` 为 `None` 时不限时间；`Some((start, end))` 时只返回这个区间内的记录，
    /// 供「最近记录」窗口按年月日时分秒筛选使用。
    pub fn recent_filtered(
        &self,
        limit: u32,
        range: Option<(i64, i64)>,
    ) -> Result<Vec<FileOrigin>> {
        let conn = self.lock_connection();
        // 没有时间范围时传 NULL，SQL 里用 `?2 IS NULL` 跳过时间条件，
        // 这样只用一条语句就能同时支持“全部记录”和“指定时间段”两种查询。
        let (start, end) = match range {
            Some((start, end)) => (Some(start), Some(end)),
            None => (None, None),
        };
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
                UNION ALL
                SELECT file_path, process_name, process_path, deleted_at AS created_at,
                       CASE WHEN is_directory <> 0 THEN 'delete_dir' ELSE 'delete_file' END AS action
                FROM file_deletions
            ) AS t
            LEFT JOIN program_notes n ON n.process_path = t.process_path
            WHERE t.file_path <> '' AND t.process_name <> '' AND t.process_path <> ''
              AND (?2 IS NULL OR (t.created_at >= ?2 AND t.created_at <= ?3))
            ORDER BY t.created_at DESC, t.file_path ASC, t.action ASC
            LIMIT ?1
            "#,
        )?;

        let rows = stmt.query_map(params![limit, start, end], |row| {
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

    /// 按程序汇总五类事件的数量，供「程序统计」窗口使用。
    ///
    /// 做法是先把三张表 UNION 成一张统一视图（每行带 `action`），
    /// 再按 `process_path` 分组，用五个 `SUM(CASE WHEN ...)` 分别统计每类次数。
    /// 排序交给界面层处理（用户点哪列就按哪列排）。
    pub fn program_stats(&self) -> Result<Vec<ProgramStat>> {
        let conn = self.lock_connection();
        let mut stmt = conn.prepare(
            r#"
            SELECT MAX(t.process_name),
                   t.process_path,
                   COALESCE(n.note, ''),
                   SUM(CASE WHEN t.action = 'create_file' THEN 1 ELSE 0 END),
                   SUM(CASE WHEN t.action = 'create_dir'  THEN 1 ELSE 0 END),
                   SUM(CASE WHEN t.action = 'modify'      THEN 1 ELSE 0 END),
                   SUM(CASE WHEN t.action = 'delete_file' THEN 1 ELSE 0 END),
                   SUM(CASE WHEN t.action = 'delete_dir'  THEN 1 ELSE 0 END)
            FROM (
                SELECT file_path, process_name, process_path, created_at,
                       CASE WHEN is_directory <> 0 THEN 'create_dir' ELSE 'create_file' END AS action
                FROM file_origins
                UNION ALL
                SELECT file_path, process_name, process_path, modified_at, 'modify'
                FROM file_modifications
                UNION ALL
                SELECT file_path, process_name, process_path, deleted_at,
                       CASE WHEN is_directory <> 0 THEN 'delete_dir' ELSE 'delete_file' END
                FROM file_deletions
            ) AS t
            LEFT JOIN program_notes n ON n.process_path = t.process_path
            WHERE t.file_path <> '' AND t.process_name <> '' AND t.process_path <> ''
            GROUP BY t.process_path
            "#,
        )?;

        let rows = stmt.query_map([], |row| {
            Ok(ProgramStat {
                process_name: row.get(0)?,
                process_path: row.get(1)?,
                note: row.get(2)?,
                create_file: row.get(3)?,
                create_dir: row.get(4)?,
                modify: row.get(5)?,
                delete_file: row.get(6)?,
                delete_dir: row.get(7)?,
            })
        })?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// 取某个程序某一类动作的明细记录（按时间倒序，最多 `limit` 条）。
    ///
    /// 统计窗口里展开「创建文件夹」等单元格时调用它：
    /// `action` 限定只取该类型，保证展开出来的明细和该列的数字对得上。
    pub fn records_by_process_action(
        &self,
        process_path: &str,
        action: &str,
        limit: u32,
    ) -> Result<Vec<FileOrigin>> {
        let conn = self.lock_connection();
        let mut stmt = conn.prepare(
            r#"
            SELECT t.file_path, t.process_name, t.process_path, t.created_at, t.action,
                   COALESCE(n.note, '')
            FROM (
                SELECT file_path, process_name, process_path, created_at,
                       CASE WHEN is_directory <> 0 THEN 'create_dir' ELSE 'create_file' END AS action
                FROM file_origins
                UNION ALL
                SELECT file_path, process_name, process_path, modified_at, 'modify'
                FROM file_modifications
                UNION ALL
                SELECT file_path, process_name, process_path, deleted_at,
                       CASE WHEN is_directory <> 0 THEN 'delete_dir' ELSE 'delete_file' END
                FROM file_deletions
            ) AS t
            LEFT JOIN program_notes n ON n.process_path = t.process_path
            WHERE t.process_path = ?1 AND t.action = ?2 AND t.file_path <> ''
            ORDER BY t.created_at DESC
            LIMIT ?3
            "#,
        )?;

        let rows = stmt.query_map(params![process_path, action, limit], |row| {
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

    /// 取某个程序的**所有类型**明细记录（按时间倒序，最多 `limit` 条）。
    ///
    /// 双击统计窗口的「合计」列时调用它：创建文件、修改、删除等会混在一起，
    /// 方便看这个程序最近到底都干了什么。
    pub fn records_by_process(&self, process_path: &str, limit: u32) -> Result<Vec<FileOrigin>> {
        let conn = self.lock_connection();
        let mut stmt = conn.prepare(
            r#"
            SELECT t.file_path, t.process_name, t.process_path, t.created_at, t.action,
                   COALESCE(n.note, '')
            FROM (
                SELECT file_path, process_name, process_path, created_at,
                       CASE WHEN is_directory <> 0 THEN 'create_dir' ELSE 'create_file' END AS action
                FROM file_origins
                UNION ALL
                SELECT file_path, process_name, process_path, modified_at, 'modify'
                FROM file_modifications
                UNION ALL
                SELECT file_path, process_name, process_path, deleted_at,
                       CASE WHEN is_directory <> 0 THEN 'delete_dir' ELSE 'delete_file' END
                FROM file_deletions
            ) AS t
            LEFT JOIN program_notes n ON n.process_path = t.process_path
            WHERE t.process_path = ?1 AND t.file_path <> ''
            ORDER BY t.created_at DESC
            LIMIT ?2
            "#,
        )?;

        let rows = stmt.query_map(params![process_path, limit], |row| {
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

    /// 删除某个程序的**全部**记录（三张表一起删）。
    ///
    /// 供「程序统计」窗口右键「删除该程序的全部记录」使用。
    /// 返回实际删除的行数，方便上层打日志。
    pub fn delete_program_records(&self, process_path: &str) -> Result<usize> {
        let conn = self.lock_connection();
        let mut deleted = 0;
        deleted += conn.execute(
            "DELETE FROM file_origins WHERE process_path = ?1",
            params![process_path],
        )?;
        deleted += conn.execute(
            "DELETE FROM file_modifications WHERE process_path = ?1",
            params![process_path],
        )?;
        deleted += conn.execute(
            "DELETE FROM file_deletions WHERE process_path = ?1",
            params![process_path],
        )?;
        Ok(deleted)
    }

    /// 删除某个程序**某一类动作**的全部记录。
    ///
    /// 供统计窗口右键某个计数单元格使用，例如「删除 explorer.exe 的全部创建文件夹记录」。
    /// 创建 / 删除两类共用一张表，靠 `is_directory` 区分文件与文件夹。
    pub fn delete_program_action_records(&self, process_path: &str, action: &str) -> Result<usize> {
        let conn = self.lock_connection();
        let deleted = match action {
            "create_file" => conn.execute(
                "DELETE FROM file_origins WHERE process_path = ?1 AND is_directory = 0",
                params![process_path],
            )?,
            "create_dir" => conn.execute(
                "DELETE FROM file_origins WHERE process_path = ?1 AND is_directory <> 0",
                params![process_path],
            )?,
            "modify" => conn.execute(
                "DELETE FROM file_modifications WHERE process_path = ?1",
                params![process_path],
            )?,
            "delete_file" => conn.execute(
                "DELETE FROM file_deletions WHERE process_path = ?1 AND is_directory = 0",
                params![process_path],
            )?,
            _ => conn.execute(
                "DELETE FROM file_deletions WHERE process_path = ?1 AND is_directory <> 0",
                params![process_path],
            )?,
        };
        Ok(deleted)
    }

    /// 删除一条具体记录。
    ///
    /// `action` 决定该去删哪张表：
    /// - `modify` → file_modifications
    /// - `delete_file` / `delete_dir` → file_deletions
    /// - 其余（创建文件 / 创建文件夹）→ file_origins
    ///
    /// 同一路径可能同时存在「创建」和「删除」两条记录（分表存放），
    /// 所以这里只按动作对应的那张表删除，不会误删另一条。
    pub fn delete_record(&self, file_path: &str, action: &str) -> Result<usize> {
        let conn = self.lock_connection();
        let deleted = match action {
            "modify" => conn.execute(
                "DELETE FROM file_modifications WHERE file_path = ?1",
                params![file_path],
            )?,
            "delete_file" | "delete_dir" => conn.execute(
                "DELETE FROM file_deletions WHERE file_path = ?1",
                params![file_path],
            )?,
            _ => conn.execute(
                "DELETE FROM file_origins WHERE file_path = ?1",
                params![file_path],
            )?,
        };
        Ok(deleted)
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
        let deletions = conn.execute(
            "DELETE FROM file_deletions WHERE deleted_at < ?1",
            params![cutoff],
        )?;

        Ok((origins + modifications + deletions) as i64)
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

    /// 读取某个模式（black / white）下的全部名单条目。
    pub fn list_entries(&self, mode: &str) -> Result<Vec<ListEntry>> {
        let conn = self.lock_connection();
        let mut stmt = conn.prepare(
            "SELECT path, kind FROM list_entries WHERE mode = ?1 ORDER BY kind, path COLLATE NOCASE",
        )?;
        let rows = stmt.query_map(params![mode], |row| {
            let kind: String = row.get(1)?;
            Ok(ListEntry {
                path: row.get(0)?,
                is_folder: kind == LIST_KIND_FOLDER,
            })
        })?;

        let mut entries = Vec::new();
        for entry in rows {
            entries.push(entry?);
        }
        Ok(entries)
    }

    /// 读取黑名单 + 白名单的全部条目，供「名单管理」窗口一起展示。
    ///
    /// 返回 `(模式, 条目)`：模式用来在列表里显示「黑名单 / 白名单」，
    /// 移除时也能知道该从哪个名单里删。
    pub fn all_list_entries(&self) -> Result<Vec<(String, ListEntry)>> {
        let conn = self.lock_connection();
        let mut stmt = conn.prepare(
            "SELECT mode, path, kind FROM list_entries
             ORDER BY mode, kind, path COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |row| {
            let kind: String = row.get(2)?;
            Ok((
                row.get::<_, String>(0)?,
                ListEntry {
                    path: row.get(1)?,
                    is_folder: kind == LIST_KIND_FOLDER,
                },
            ))
        })?;

        let mut entries = Vec::new();
        for entry in rows {
            entries.push(entry?);
        }
        Ok(entries)
    }

    /// 向名单里添加一条（程序或文件夹）。
    ///
    /// 同一条目重复添加时直接忽略，不会报错，界面上也不会出现两条一样的记录。
    pub fn add_list_entry(&self, mode: &str, path: &str, is_folder: bool) -> Result<()> {
        let conn = self.lock_connection();
        let kind = if is_folder {
            LIST_KIND_FOLDER
        } else {
            LIST_KIND_PROGRAM
        };
        conn.execute(
            "INSERT OR IGNORE INTO list_entries(mode, path, kind, added_at)
             VALUES(?1, ?2, ?3, ?4)",
            params![mode, path, kind, chrono_now()],
        )?;
        Ok(())
    }

    /// 从名单里移除一条。
    pub fn remove_list_entry(&self, mode: &str, path: &str) -> Result<()> {
        let conn = self.lock_connection();
        conn.execute(
            "DELETE FROM list_entries WHERE mode = ?1 AND path = ?2",
            params![mode, path],
        )?;
        Ok(())
    }

    /// 把一条名单条目切换到另一个名单（黑名单 ↔ 白名单）。
    ///
    /// 表的主键是 (mode, path)，所以只要把 mode 改掉就行。
    /// 但如果目标名单里已经有同一个路径，直接改会撞主键，
    /// 这里用 `UPDATE OR REPLACE`：先删掉目标名单里的旧行，再写入新行。
    pub fn move_list_entry(&self, from_mode: &str, path: &str, to_mode: &str) -> Result<()> {
        let conn = self.lock_connection();
        conn.execute(
            "UPDATE OR REPLACE list_entries
             SET mode = ?1, added_at = ?2
             WHERE mode = ?3 AND path = ?4",
            params![to_mode, chrono_now(), from_mode, path],
        )?;
        Ok(())
    }
}

/// 名单条目的类型：程序。
pub const LIST_KIND_PROGRAM: &str = "program";
/// 名单条目的类型：文件夹。
pub const LIST_KIND_FOLDER: &str = "folder";

/// 黑白名单里的一条记录。
#[derive(Debug, Clone)]
pub struct ListEntry {
    /// 程序完整路径，或文件夹路径。
    pub path: String,
    /// true 表示这是文件夹条目（按目录前缀匹配），
    /// false 表示这是程序条目（按程序完整路径精确匹配）。
    pub is_folder: bool,
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
    use crate::list_rules::{MODE_BLACK, MODE_WHITE};

    /// 验证按时间范围查询：只返回区间内的记录，不带范围时返回全部。
    #[test]
    fn recent_filtered_respects_time_range() {
        let dir = std::env::temp_dir().join("file_origin_tracker_db_test_range");
        let _ = std::fs::remove_dir_all(&dir);
        let db = Database::init(&dir).unwrap();

        let now = chrono_now();
        db.upsert_origin(r"C:\old.txt", "a.exe", r"C:\a.exe", false)
            .unwrap();
        db.upsert_origin(r"C:\new.txt", "b.exe", r"C:\b.exe", false)
            .unwrap();

        // 手动把 old.txt 的创建时间改成 10 天前。
        {
            let conn = db.lock_connection();
            conn.execute(
                "UPDATE file_origins SET created_at = ?1 WHERE file_path = ?2",
                params![now - 10 * 86400, r"C:\old.txt"],
            )
            .unwrap();
        }

        // 不带时间范围：两条都在。
        assert_eq!(db.recent_filtered(50, None).unwrap().len(), 2);

        // 只查最近 1 小时：只剩 new.txt。
        let recent = db
            .recent_filtered(50, Some((now - 3600, now + 3600)))
            .unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].file_path, r"C:\new.txt");

        // 只查 9~11 天前：只剩 old.txt。
        let old = db
            .recent_filtered(50, Some((now - 11 * 86400, now - 9 * 86400)))
            .unwrap();
        assert_eq!(old.len(), 1);
        assert_eq!(old[0].file_path, r"C:\old.txt");
    }

    /// 验证删除记录：文件/文件夹分别映射成 delete_file / delete_dir，
    /// 并且删除记录不会覆盖同一路径的新建记录。
    #[test]
    fn deletion_records_keep_origin_and_expose_action() {
        let dir = std::env::temp_dir().join("file_origin_tracker_db_test_delete");
        let _ = std::fs::remove_dir_all(&dir);
        let db = Database::init(&dir).unwrap();

        db.upsert_origin(r"C:\a.txt", "a.exe", r"C:\a.exe", false)
            .unwrap();
        db.upsert_origin(r"C:\folder", "a.exe", r"C:\a.exe", true)
            .unwrap();
        db.upsert_deletion(r"C:\a.txt", "b.exe", r"C:\b.exe", false)
            .unwrap();
        db.upsert_deletion(r"C:\folder", "b.exe", r"C:\b.exe", true)
            .unwrap();

        // 删除记录单独存表，原来的新建记录还在。
        let origins = db.recent_filtered(50, None).unwrap();
        assert!(origins
            .iter()
            .any(|row| row.file_path == r"C:\a.txt" && row.action == "create_file"));
        assert!(origins
            .iter()
            .any(|row| row.file_path == r"C:\a.txt" && row.action == "delete_file"));
        assert!(origins
            .iter()
            .any(|row| row.file_path == r"C:\folder" && row.action == "delete_dir"));

        // 建过文件夹的路径能被识别出来，供删除事件判断类型时兜底。
        assert!(db.path_is_known_directory(r"C:\folder"));
        assert!(!db.path_is_known_directory(r"C:\a.txt"));
    }

    /// 验证名单条目能在黑名单 / 白名单之间来回切换，
    /// 以及目标名单里已有同一路径时不会因主键冲突失败。
    #[test]
    fn list_entry_moves_between_lists() {
        let dir = std::env::temp_dir().join("file_origin_tracker_db_test_move");
        let _ = std::fs::remove_dir_all(&dir);
        let db = Database::init(&dir).unwrap();

        // 先加一条黑名单程序，再切到白名单。
        db.add_list_entry(MODE_BLACK, r"C:\tool.exe", false)
            .unwrap();
        assert_eq!(db.list_entries(MODE_BLACK).unwrap().len(), 1);

        db.move_list_entry(MODE_BLACK, r"C:\tool.exe", MODE_WHITE)
            .unwrap();
        assert_eq!(db.list_entries(MODE_BLACK).unwrap().len(), 0);
        let white = db.list_entries(MODE_WHITE).unwrap();
        assert_eq!(white.len(), 1);
        assert_eq!(white[0].path, r"C:\tool.exe");
        assert!(!white[0].is_folder);

        // 两个名单里都存在同一路径时，切换应覆盖而不是报主键冲突。
        db.add_list_entry(MODE_WHITE, r"C:\dir", true).unwrap();
        db.add_list_entry(MODE_BLACK, r"C:\dir", true).unwrap();
        db.move_list_entry(MODE_BLACK, r"C:\dir", MODE_WHITE)
            .unwrap();
        assert_eq!(db.list_entries(MODE_BLACK).unwrap().len(), 0);
        assert_eq!(db.list_entries(MODE_WHITE).unwrap().len(), 2);
    }

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

    /// 验证「程序统计」：五类计数、合计，以及按类型取明细时只返回该类型。
    #[test]
    fn program_stats_groups_and_filters_by_action() {
        let dir = std::env::temp_dir().join("file_origin_tracker_db_test_stats");
        let _ = std::fs::remove_dir_all(&dir);
        let db = Database::init(&dir).unwrap();

        // 程序 A：创建 1 文件 + 1 文件夹，修改 1 文件，删除 1 文件。
        db.upsert_origin(r"C:\a\one.txt", "a.exe", r"C:\a.exe", false)
            .unwrap();
        db.upsert_origin(r"C:\a\folder", "a.exe", r"C:\a.exe", true)
            .unwrap();
        db.upsert_modification(r"C:\a\one.txt", "a.exe", r"C:\a.exe")
            .unwrap();
        db.upsert_deletion(r"C:\a\one.txt", "a.exe", r"C:\a.exe", false)
            .unwrap();

        // 程序 B：只创建一个文件，用来验证分组互不干扰。
        db.upsert_origin(r"C:\b\two.txt", "b.exe", r"C:\b.exe", false)
            .unwrap();

        let stats = db.program_stats().unwrap();
        assert_eq!(stats.len(), 2);

        let a = stats
            .iter()
            .find(|stat| stat.process_path == r"C:\a.exe")
            .unwrap();
        assert_eq!(a.create_file, 1);
        assert_eq!(a.create_dir, 1);
        assert_eq!(a.modify, 1);
        assert_eq!(a.delete_file, 1);
        assert_eq!(a.delete_dir, 0);
        assert_eq!(a.total(), 4);

        let b = stats
            .iter()
            .find(|stat| stat.process_path == r"C:\b.exe")
            .unwrap();
        assert_eq!(b.create_file, 1);
        assert_eq!(b.total(), 1);

        // 明细必须严格按类型过滤：展开「创建文件夹」时不能混进创建文件。
        let folders = db
            .records_by_process_action(r"C:\a.exe", "create_dir", 100)
            .unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].file_path, r"C:\a\folder");
        assert_eq!(folders[0].action, "create_dir");

        let creates = db
            .records_by_process_action(r"C:\a.exe", "create_file", 100)
            .unwrap();
        assert_eq!(creates.len(), 1);
        assert_eq!(creates[0].file_path, r"C:\a\one.txt");
        assert_eq!(creates[0].action, "create_file");

        // 换一个程序查询同类型，结果不能串到程序 A 的数据。
        assert!(db
            .records_by_process_action(r"C:\b.exe", "create_dir", 100)
            .unwrap()
            .is_empty());

        // 「合计」展开会取全部类型：程序 A 应该有创建 / 修改 / 删除共 4 条。
        let all_a = db.records_by_process(r"C:\a.exe", 100).unwrap();
        assert_eq!(all_a.len(), 4);
        // 结果按时间倒序，这里只验证类型齐全，不依赖具体顺序。
        let actions: Vec<&str> = all_a.iter().map(|row| row.action.as_str()).collect();
        assert!(actions.contains(&"create_file"));
        assert!(actions.contains(&"create_dir"));
        assert!(actions.contains(&"modify"));
        assert!(actions.contains(&"delete_file"));
        // 另一个程序只有 1 条。
        assert_eq!(db.records_by_process(r"C:\b.exe", 100).unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 验证删除：单条删除只动动作对应的那张表，
    /// 程序级删除会清掉该程序在三张表里的所有记录，且不影响其它程序。
    #[test]
    fn delete_program_and_single_records() {
        let dir = std::env::temp_dir().join("file_origin_tracker_db_test_delete_stats");
        let _ = std::fs::remove_dir_all(&dir);
        let db = Database::init(&dir).unwrap();

        db.upsert_origin(r"C:\a\one.txt", "a.exe", r"C:\a.exe", false)
            .unwrap();
        db.upsert_origin(r"C:\a\folder", "a.exe", r"C:\a.exe", true)
            .unwrap();
        db.upsert_modification(r"C:\a\one.txt", "a.exe", r"C:\a.exe")
            .unwrap();
        db.upsert_deletion(r"C:\a\one.txt", "a.exe", r"C:\a.exe", false)
            .unwrap();
        db.upsert_origin(r"C:\b\two.txt", "b.exe", r"C:\b.exe", false)
            .unwrap();

        // 按类型删除：只删 a.exe 的「创建文件夹」，其它类型全部保留。
        assert_eq!(
            db.delete_program_action_records(r"C:\a.exe", "create_dir")
                .unwrap(),
            1
        );
        assert_eq!(db.records_by_process(r"C:\a.exe", 100).unwrap().len(), 3);

        // 单条删除：只删掉「创建文件」这一条，同路径的修改 / 删除记录必须保留。
        assert_eq!(db.delete_record(r"C:\a\one.txt", "create_file").unwrap(), 1);
        let remaining = db.recent_filtered(50, None).unwrap();
        assert!(!remaining
            .iter()
            .any(|row| row.file_path == r"C:\a\one.txt" && row.action == "create_file"));
        assert!(remaining
            .iter()
            .any(|row| row.file_path == r"C:\a\one.txt" && row.action == "modify"));
        assert!(remaining
            .iter()
            .any(|row| row.file_path == r"C:\a\one.txt" && row.action == "delete_file"));

        // 程序级删除：a.exe 还剩 2 条（one 修改、one 删除）。
        assert_eq!(db.delete_program_records(r"C:\a.exe").unwrap(), 2);
        let remaining = db.recent_filtered(50, None).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].process_path, r"C:\b.exe");

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

    /// 验证数据库损坏时能自动备份并重建空库，而不是让程序启动失败。
    #[test]
    fn corrupt_database_is_quarantined_and_recreated() {
        let dir = std::env::temp_dir().join("file_origin_tracker_db_test_corrupt");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 写一个根本不是 SQLite 格式的“数据库”文件。
        let db_path = dir.join("file_origins.db");
        std::fs::write(&db_path, b"this is definitely not a sqlite database")
            .unwrap();

        // 初始化应当成功（内部会把坏文件改名备份，再建新库）。
        let db = Database::init(&dir).unwrap();
        db.upsert_origin(r"C:\ok.txt", "a.exe", r"C:\a.exe", false)
            .unwrap();
        assert_eq!(db.recent(10).unwrap().len(), 1);

        // 坏文件必须被保留下来（改名成 .corrupt-<时间戳>），不能直接删掉。
        let has_backup = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("file_origins.db.corrupt-")
            });
        assert!(has_backup, "损坏的数据库应被改名备份");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
