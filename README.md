# FileOriginTracker（文件来源追踪）

一个常驻 Windows 后台的小工具：**记录每一个新建的文件 / 文件夹是被哪个程序创建的**，
事后可以通过鼠标右键菜单或「最近记录」窗口查出来源。

> 典型场景：桌面上突然多了一个 `setup_1.tmp`、一个来路不明的文件夹，
> 想知道是谁写的、什么时候写的 —— 右键一点就能看到创建者，并直接跳到那个程序所在目录。

---

## 功能特性

| 功能 | 说明 |
| --- | --- |
| 实时记录新建文件 | 记录「谁创建了哪个文件」，精确到进程完整路径 |
| 实时记录新建文件夹 | 文件夹单独统计，不会和文件混在一起 |
| 记录文件修改（可选） | 默认关闭；打开后记录文件最后一次被哪个程序写入 |
| 右键菜单查询 | 资源管理器右键 → 菜单标题直接显示「该文件/文件夹来自 XXX 创建」，点击打开该程序所在目录 |
| 最近记录窗口 | 列表展示最近 500 条：监控时间 / 程序 / 路径 / 类型；顶部搜索框可按程序名、路径、类型、备注实时过滤 |
| 记录操作 | 双击「创建程序」列打开程序目录、双击「路径」列打开文件所在目录；右键记录可「打开文件路径 / 复制词条 / 打开创建程序所在目录 / 设置程序备注」 |
| 程序备注 | 右键记录 →「设置程序备注」，给这个程序写一句说明；之后该程序产生的所有记录都会显示「程序名（备注）」，备注按程序完整路径保存，留空即删除 |
| 黑白名单 | 设置里切换黑名单 / 白名单模式，托盘菜单「名单管理」维护名单：黑名单与白名单并排显示在同一个列表里（「名单」列标明归属），先选「添加到：黑名单 / 白名单」再点添加程序（.exe）或文件夹，也可移除选中项；选中一条后点「切换名单」或直接双击该行，即可在黑名单 / 白名单之间互换。黑名单=不记录名单里的程序与文件夹；白名单=只记录它们。记录右键菜单可直接把创建程序加入当前名单，改完 2 秒内自动生效，无需重启 |
| 三个独立监控开关 | 创建文件、创建文件夹、修改文件可分别开关，全关时彻底停止 ETW 会话，不占用 CPU |
| 设置窗口 | 分组式界面：记录保留天数、开机自启、三个监控开关、右键菜单注册 / 注销 |
| 记录自动清理 | 超过保留天数的记录自动删除，并 `VACUUM` 压缩数据库文件 |
| 单文件发行 | 正式发布版只有一个 exe，右键菜单扩展 DLL 打包在 exe 内部，启动时自动释放 |

---

## 实现原理

### 1. 如何知道“是谁创建了这个文件”？

Windows 提供了内核级的 ETW（Event Tracing for Windows）事件通道
`Microsoft-Windows-Kernel-File`（GUID `edd08927-9cc4-4e65-b970-c2560fb5c289`）。
任何进程对文件系统发起操作，内核都会在这里抛出一条事件，事件头里带着 **发起操作的进程 PID**。

程序用 `StartTraceW` + `EnableTraceEx2` 建立一个实时会话
（`PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD`），
在回调里按事件 ID 过滤：

| 事件 ID | 名称 | 用途 |
| --- | --- | --- |
| 12 | Create | 打开/创建文件，用于建立「文件对象 → 路径」映射，不上报 |
| 13 / 14 | Cleanup / Close | 句柄关闭，清理映射，防止内存增长 |
| 16 | Write | 文件被写入 → 修改监控 |
| 27 | RenamePath | 重命名/移动后的最终路径（浏览器、IM 保存文件常用「临时文件再改名」） |
| 30 | CreateNewFile | **真正新建**了文件或文件夹 → 创建监控 |

路径直接从事件负载的二进制布局里读取（Kernel-File 是内核事件，没有 manifest 模板，
不能用 TDH 按属性名取值），例如 `CreateNewFile(30)`：

```text
+0x00  PVOID  FileObject
+0x08  PVOID  Reserved
+0x10  ULONG  CreateOptions
+0x14  ULONG  FileAttributes
+0x18  ULONG  ShareAccess
+0x1C  ULONG  Disposition
+0x20  WCHAR  Name[]        ← 文件名（UTF-16，长度 = UserDataLength - 0x20）
```

`Write(16)` 事件**不携带文件名**，只有文件对象指针，所以要先靠 `Create(12)`
把「文件对象指针 → 路径」记在一张表里，写事件到来时再反查。
实测 `Write` 的文件对象在 `+0x08`（`+0x00` 是 `FileKey`），与 `Create` 的 `+0x00` 不同。
这张表只在 ETW 回调线程内使用，因此用 `thread_local! + RefCell` 即可，无需加锁。

### 2. 如何拿到 PID 对应的程序？

`OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` + `QueryFullProcessImageNameW`，
只需要「受限查询」权限，比枚举进程模块更轻量。

**关键细节**：ETW 事件存在毫秒级投递延迟，像 `cmd.exe` 这类执行完立刻退出的进程，
等工作线程再去按 PID 查询时进程已经消失，记录会整条丢失。
所以进程信息是在 **ETW 回调当场解析并缓存**，再随事件一起交给写库线程的。

### 3. 数据怎么存？

SQLite（`rusqlite` bundled 模式，无需额外安装），WAL 模式，路径：
`%LOCALAPPDATA%\FileOriginTracker\FileOriginTracker\data\file_origins.db`

```sql
file_origins(      -- 新建记录，每个路径只保留最新一条
    file_path TEXT PRIMARY KEY,
    process_name TEXT, process_path TEXT,
    created_at INTEGER, is_directory INTEGER )

file_modifications( -- 修改记录，同样按路径去重
    file_path TEXT PRIMARY KEY,
    process_name TEXT, process_path TEXT, modified_at INTEGER )

program_notes(     -- 程序备注，按程序完整路径保存
    process_path TEXT PRIMARY KEY,
    note TEXT, updated_at INTEGER )

list_entries(      -- 黑白名单
    mode TEXT,     -- black / white
    path TEXT,     -- 程序完整路径 或 文件夹路径
    kind TEXT,     -- program / folder
    added_at INTEGER,
    PRIMARY KEY(mode, path) )
```

写入统一用 `INSERT ... ON CONFLICT(file_path) DO UPDATE`，
「最近记录」用 `UNION ALL` 把两张表合并后按时间倒序取前 500 条，
再用 `LEFT JOIN program_notes` 把该程序的备注一并读出来。

黑白名单在 ETW 写库线程里判定：规则每 2 秒从数据库重新加载一次，
因此界面上增删条目后很快生效，不需要重启程序。
程序条目按完整路径精确匹配，文件夹条目按目录前缀匹配（`D:\下载` 不会误匹配 `D:\下载备份`）。
清理时两张表一起按时间删除，只有真的删过数据才执行 `VACUUM` 回收磁盘空间。

### 4. 右键菜单是怎么做到“标题动态显示程序名”的？

这是一个 COM 的 `IExplorerCommand` 扩展（`src/com.rs`），编译成
`file_origin_tracker_shell.dll`，由资源管理器在自己进程内加载，
`GetTitle()` 里查数据库把标题拼成「该文件/文件夹来自 notepad.exe 创建」，
`Invoke()` 里用 `explorer.exe /select,"<程序路径>"` 打开程序所在目录。

注册项（都写在 HKCU，不需要管理员权限）：

```text
HKCU\Software\Classes\*\shell\FileOriginTracker            ExplorerCommandHandler = {CLSID}
HKCU\Software\Classes\Directory\shell\FileOriginTracker    ExplorerCommandHandler = {CLSID}
HKCU\Software\Classes\CLSID\{7C3A51E0-...-2A8E6B1D4409}\InprocServer32 = <DLL 路径>
```

因为 DLL 运行在 explorer.exe 里，任何数据库/路径异常都必须容错降级为「未知程序」，绝不能 panic。

### 5. 单文件发行版是怎么做的？

资源管理器的 shell 扩展**必须是独立的 DLL 文件**，所以「只有一个 exe」的做法是：

```text
[主程序 exe][右键菜单 DLL][8 字节 DLL 长度][16 字节标记 "FOT-SHELL-DLL-01"]
```

Windows 加载 PE 文件时会忽略末尾的附加数据，exe 照常运行。
程序启动时读取自己文件末尾的这段数据，把 DLL 释放到
`%LOCALAPPDATA%\...\data\shell\`（内容一致就不重复写，避免 DLL 被占用导致写入失败），
再把注册表指向这个固定路径。这样 **exe 放在任何位置（桌面、U 盘）右键菜单都能用**，
移动 exe 也不会让菜单失效。

---

## 使用说明

1. 从 [Releases](../../releases) 下载 `FileOriginTracker.exe`（单个文件，无需安装）。
2. 双击运行。程序会自动请求管理员权限 —— **ETW 内核事件必须管理员权限**，否则收不到任何事件。
3. 运行后右下角出现托盘图标，右键托盘可打开：
   - 查看最近记录
   - 打开数据目录
   - 设置
   - 以管理员身份重新启动
   - 退出
4. 设置窗口里可以：调记录保留天数、开机自启、分别开关三种监控、注册/注销右键菜单。

> 注意：程序只能记录**它运行期间**发生的文件操作，之前创建的旧文件查不到来源。

---

## 从源码构建

环境要求：Windows 10/11、Rust stable（MSVC 工具链）、Windows SDK（提供 `rc.exe`，用于嵌入图标）。

```powershell
# 普通构建（exe 与 dll 分离，方便调试）
cargo build --release

# 打包成单文件发行版 → dist\FileOriginTracker.exe
powershell -ExecutionPolicy Bypass -File tools\pack.ps1
```

---

## 目录结构

```text
src/
  main.rs       主程序入口：日志、提权、配置、数据库、托盘
  tray.rs       系统托盘图标与菜单
  etw.rs        ETW Kernel-File 实时监听、事件解析、写库线程
  process.rs    PID → 进程名 / 完整路径
  db.rs         SQLite 表结构与读写、按天清理与压缩
  config.rs     配置读写（存 AppData 下的 config.json）
  settings.rs   设置窗口（原生 Win32 控件）
  history.rs    「最近记录」窗口（ListView 报表模式）
  note.rs       程序备注编辑小窗口
  list_window.rs  黑/白名单管理窗口（两个名单合并显示，可选目标名单后添加、移除）
  list_rules.rs   黑白名单过滤规则（ETW 写库前判定是否记录）
  com.rs        右键菜单 COM 扩展（IExplorerCommand）
  shell_menu.rs 右键菜单注册/注销、单文件版 DLL 释放
  util.rs       路径拼接、资源管理器调用等工具函数
tools/pack.ps1  把 exe 与 DLL 合成单文件发行版
build.rs        把 assets/app.ico 编译进 exe / dll
```

---

## 已知限制

- 必须在程序运行期间才能记录；程序未运行时的文件操作无法追溯。
- 极短命进程（几十毫秒内创建文件并退出的程序）可能因 ETW 投递延迟而查不到进程名，该类记录会被跳过。
- 只支持 Windows（依赖 ETW 与 COM shell 扩展）。
- 右键菜单扩展在 Windows 11 上需要系统版本支持 `IExplorerCommand`（Win11 21H2+ 已支持，同时也会出现在「显示更多选项」里）。
