//! ETW 文件事件监听模块。
//!
//! 监听 Microsoft-Windows-Kernel-File 的三类事件：
//! - CreateNewFile(30) / RenamePath(27)：新建文件、新建文件夹，以及
//!   浏览器/QQ/微信“临时文件保存完成后改成最终文件名”的场景
//! - Write(16)：文件内容被写入（修改监控，默认关闭）
//!
//! Write 事件本身不带文件名，所以要用 Create 事件里记录的
//! “文件对象指针 -> 路径”对应关系反查。这套映射只在 ETW 回调线程内使用，
//! 不需要加锁。
//!
//! 为降低性能开销，ETW 回调里只解析路径并发送消息；查询进程和写 SQLite
//! 都在另一个工作线程中执行，避免阻塞 Windows 的 ETW 事件分发线程。

use anyhow::Result;
use log::{error, info, warn};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::db::Database;
use crate::process::ProcessInfo;
use crate::util::wide;

use once_cell::sync::Lazy;
use windows::core::{GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_SUCCESS};
use windows::Win32::Storage::FileSystem::QueryDosDeviceW;
use windows::Win32::System::Diagnostics::Etw::{
    CloseTrace, ControlTraceW, EnableTraceEx2, OpenTraceW, ProcessTrace, StartTraceW,
    TdhGetProperty, TdhGetPropertySize, CONTROLTRACE_HANDLE, EVENT_RECORD,
    EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_LOGFILEW, EVENT_TRACE_PROPERTIES,
    EVENT_TRACE_REAL_TIME_MODE, PROCESS_TRACE_MODE_EVENT_RECORD, PROCESS_TRACE_MODE_REAL_TIME,
    PROPERTY_DATA_DESCRIPTOR, WNODE_FLAG_TRACED_GUID,
};

/// Microsoft-Windows-Kernel-File 的 GUID。
/// 可通过 `wevtutil gp Microsoft-Windows-Kernel-File /ge:true` 查询确认。
const KERNEL_FILE_PROVIDER_GUID: GUID = GUID::from_values(
    0xedd08927,
    0x9cc4,
    0x4e65,
    [0xb9, 0x70, 0xc2, 0x56, 0x0f, 0xb5, 0xc2, 0x89],
);

/// 以下事件 ID 通过 `wevtutil gp Microsoft-Windows-Kernel-File /ge:true` 确认：
/// - 12 = Create：打开或创建文件，用于建立“文件对象 -> 路径”映射，不上报
/// - 13 = Cleanup、14 = Close：文件句柄关闭，用于清理映射
/// - 16 = Write：文件内容被写入，用于修改监控
/// - 27 = RenamePath：重命名/移动后携带最终完整路径
/// - 30 = CreateNewFile：真正新建了文件或目录
const EVENT_ID_CREATE: u16 = 12;
const EVENT_ID_CLEANUP: u16 = 13;
const EVENT_ID_CLOSE: u16 = 14;
const EVENT_ID_WRITE: u16 = 16;
const EVENT_ID_RENAME_PATH: u16 = 27;
const EVENT_ID_CREATE_NEW_FILE: u16 = 30;

/// 同一个文件对象在 1 秒内只上报一次修改，避免一个进程连续写文件时
/// 产生成百上千条重复记录。
const MODIFY_THROTTLE: Duration = Duration::from_secs(1);

/// “文件对象 -> 路径”映射的最大条目数，超过后整体清空，防止内存无限增长。
const MAX_TRACKED_FILES: usize = 50_000;

/// Write 事件负载中文件对象指针的偏移。
///
/// 实测 Write(16) 的负载开头是 `FileKey`（8 字节，常见 -1 或小整数），
/// 真正的文件对象指针位于 +0x08；而 Create(12/30) 的文件对象在 +0x00。
const WRITE_FILE_OBJECT_OFFSET: usize = 0x08;

/// Kernel-File 提供者的关键词掩码。
///
/// 理论上可以只启用 CreateNewFile(0x1000) + FileName(0x10) + FileRename(0x2000)
/// 来减少事件量，但不同 Windows 版本的关键词位定义可能略有差异，
/// 为确保能捕获到重命名事件，这里使用 0xFFFFFFFFFFFFFFFF 接收该提供者的
/// 所有事件，然后在回调中按事件 ID 精确过滤（只处理 30 和 27）。
/// 其他事件在回调里只做一次比较就 return，开销极小。
const KERNEL_FILE_KEYWORD_ALL: u64 = 0xFFFFFFFFFFFFFFFF;

const EVENT_CONTROL_CODE_ENABLE_PROVIDER: u32 = 1;
const SESSION_NAME: &str = "FileOriginTrackerKernelFile";

/// 事件类型：新建（文件/文件夹由工作线程判断）或修改。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileAction {
    Create,
    Modify,
}

/// 发送给工作线程的一条精简事件。
#[derive(Debug)]
struct FileEvent {
    pid: u32,
    file_path: String,
    action: FileAction,
    /// 事件发生瞬间解析到的进程名/路径。
    ///
    /// 必须在这里就取好：像 cmd.exe 这种执行完立刻退出的程序，
    /// 等工作线程再去按 PID 查询时已经查不到了，记录会被漏掉。
    process_name: Option<String>,
    process_path: Option<String>,
}

/// ETW 回调是 C ABI，不能直接捕获 Rust 变量，所以用全局发送器转交事件。
static EVENT_SENDER: Lazy<Mutex<Option<Sender<FileEvent>>>> = Lazy::new(|| Mutex::new(None));

/// 当前是否记录“新建文件/文件夹”。由启动监控时的配置决定。
static MONITOR_CREATE: AtomicBool = AtomicBool::new(true);

/// 当前是否记录“文件修改”。
static MONITOR_MODIFY: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// “文件对象指针 -> 文件路径”。Write 事件只有文件对象，没有文件名，
    /// 必须靠这张表反查出是哪个文件被修改了。
    ///
    /// ETW 回调都在同一个线程里顺序执行，所以用 thread_local + RefCell
    /// 就够了，不需要 Mutex，避免每次写入都争锁。
    static FILE_PATHS: RefCell<HashMap<usize, String>> = RefCell::new(HashMap::new());

    /// 记录每个文件对象上次上报修改的时间，用于 1 秒节流。
    static WRITE_THROTTLE: RefCell<HashMap<usize, Instant>> = RefCell::new(HashMap::new());

    /// “PID -> 进程名/路径”缓存。
    ///
    /// ETW 回调里为需要上报的事件立即解析进程信息，避免进程退出后查不到。
    /// 缓存可以避免同一个进程反复调用 OpenProcess。
    static PROCESS_INFO: RefCell<HashMap<u32, Option<(String, String)>>> =
        RefCell::new(HashMap::new());
}

/// 后台 ETW 任务的控制器， dropped 时会自动停止所有线程。
pub struct EtwTraceHandle {
    session_handle: CONTROLTRACE_HANDLE,
    running: Arc<AtomicBool>,
    consumer_thread: Option<thread::JoinHandle<()>>,
    worker_thread: Option<thread::JoinHandle<()>>,
}

impl EtwTraceHandle {
    /// 停止 ETW 会话，并等待消费线程、写库线程退出。
    pub fn stop(&mut self) {
        if !self.running.swap(false, Ordering::SeqCst) {
            return;
        }

        stop_session(self.session_handle);

        if let Some(thread) = self.consumer_thread.take() {
            let _ = thread.join();
        }

        // 取走发送器后 channel 会断开，工作线程处理完剩余事件便退出。
        if let Ok(mut sender) = EVENT_SENDER.lock() {
            sender.take();
        }
        if let Some(thread) = self.worker_thread.take() {
            let _ = thread.join();
        }

        info!("ETW 文件追踪已停止");
    }
}

impl Drop for EtwTraceHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 当前正在运行的追踪句柄。
///
/// 放到全局是为了让“设置”页面可以在程序运行期间直接开启或关闭监控，
/// 不需要退出并重启程序。
static ACTIVE_TRACE: Lazy<Mutex<Option<EtwTraceHandle>>> = Lazy::new(|| Mutex::new(None));

/// 按设置启动监控。如果已经在运行，会先停止再重新开始。
pub fn start_monitoring(db: Database, config: Config) -> Result<()> {
    stop_monitoring();
    let handle = start_file_tracing(db, config)?;
    if let Ok(mut slot) = ACTIVE_TRACE.lock() {
        *slot = Some(handle);
    }
    Ok(())
}

/// 停止监控；没有在运行时什么都不做。
pub fn stop_monitoring() {
    if let Ok(mut slot) = ACTIVE_TRACE.lock() {
        if let Some(mut handle) = slot.take() {
            handle.stop();
        }
    }
}

/// 当前是否正在监控。
pub fn is_monitoring() -> bool {
    ACTIVE_TRACE
        .lock()
        .map(|slot| slot.is_some())
        .unwrap_or(false)
}

/// 启动 ETW 监听和后台写库线程。
pub fn start_file_tracing(db: Database, config: Config) -> Result<EtwTraceHandle> {
    let session_handle = start_session()?;

    // 启用所有关键词，确保能接收到 CreateNewFile 和 RenamePath 事件。
    // 不同 Windows 版本的关键词位定义可能有差异，用全量最稳妥；
    // 回调里只处理事件 ID 30 和 27，其他事件立即返回。
    let status = unsafe {
        EnableTraceEx2(
            session_handle,
            &KERNEL_FILE_PROVIDER_GUID,
            EVENT_CONTROL_CODE_ENABLE_PROVIDER,
            0, // Level=0 表示接收该提供者的所有级别
            KERNEL_FILE_KEYWORD_ALL,
            0,
            0,
            None,
        )
    };
    if status != ERROR_SUCCESS {
        stop_session(session_handle);
        return Err(anyhow::anyhow!(
            "启用 Kernel-File 提供者失败：0x{:08X}",
            status.0
        ));
    }

    // 把用户选择的监控范围写入全局开关，供 ETW 回调快速判断。
    MONITOR_CREATE.store(
        config.monitor_create_file || config.monitor_create_dir,
        Ordering::SeqCst,
    );
    MONITOR_MODIFY.store(config.monitor_modify_file, Ordering::SeqCst);

    let (sender, receiver) = channel::<FileEvent>();
    if let Ok(mut slot) = EVENT_SENDER.lock() {
        *slot = Some(sender);
    }

    let worker_thread = thread::spawn(move || {
        process_events(receiver, db, config);
    });

    let running = Arc::new(AtomicBool::new(true));
    let consumer_running = running.clone();
    let consumer_thread = thread::spawn(move || {
        if let Err(error) = consume_events(consumer_running) {
            error!("ETW 事件消费线程异常：{:#}", error);
        }
    });

    info!("ETW 文件追踪已启动（新建文件/文件夹 + 可选的文件修改）");
    Ok(EtwTraceHandle {
        session_handle,
        running,
        consumer_thread: Some(consumer_thread),
        worker_thread: Some(worker_thread),
    })
}

/// 创建实时 ETW 会话。
fn start_session() -> Result<CONTROLTRACE_HANDLE> {
    let session_name = wide(SESSION_NAME);
    let mut properties = create_properties_buffer();
    let mut handle = CONTROLTRACE_HANDLE::default();

    let mut status = unsafe {
        StartTraceW(
            &mut handle,
            PCWSTR(session_name.as_ptr()),
            properties.as_mut_ptr().cast(),
        )
    };

    // 上次程序异常退出可能留下同名会话，先停止旧会话再重试一次。
    if status == ERROR_ALREADY_EXISTS {
        warn!("发现旧的 ETW 会话，正在清理...");
        stop_session(CONTROLTRACE_HANDLE::default());
        properties = create_properties_buffer();
        status = unsafe {
            StartTraceW(
                &mut handle,
                PCWSTR(session_name.as_ptr()),
                properties.as_mut_ptr().cast(),
            )
        };
    }

    if status != ERROR_SUCCESS {
        return Err(anyhow::anyhow!(
            "StartTraceW 失败：0x{:08X}。请以管理员身份运行程序。",
            status.0
        ));
    }

    Ok(handle)
}

/// 停止指定 ETW 会话；默认句柄表示按会话名称停止。
fn stop_session(handle: CONTROLTRACE_HANDLE) {
    let session_name = wide(SESSION_NAME);
    let name = if handle.Value == 0 {
        PCWSTR(session_name.as_ptr())
    } else {
        PCWSTR::null()
    };
    let mut properties = create_properties_buffer();
    unsafe {
        let _ = ControlTraceW(
            handle,
            name,
            properties.as_mut_ptr().cast(),
            EVENT_TRACE_CONTROL_STOP,
        );
    }
}

/// StartTraceW 要求“属性结构体 + 会话名称”位于同一块连续内存中。
fn create_properties_buffer() -> Vec<u8> {
    let name = wide(SESSION_NAME);
    let properties_size = std::mem::size_of::<EVENT_TRACE_PROPERTIES>();
    let total_size = properties_size + name.len() * 2;
    let mut buffer = vec![0u8; total_size];

    unsafe {
        let properties = buffer.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();
        (*properties).Wnode.BufferSize = total_size as u32;
        (*properties).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        (*properties).BufferSize = 64; // 每个内核缓冲区 64 KB
        (*properties).MinimumBuffers = 2;
        (*properties).MaximumBuffers = 16;
        (*properties).LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
        (*properties).LoggerNameOffset = properties_size as u32;

        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            buffer.as_mut_ptr().add(properties_size).cast::<u16>(),
            name.len(),
        );
    }

    buffer
}

/// 打开实时事件流；ProcessTrace 会一直阻塞到会话被停止。
fn consume_events(running: Arc<AtomicBool>) -> Result<()> {
    let mut session_name = wide(SESSION_NAME);
    let mut logfile = EVENT_TRACE_LOGFILEW::default();
    logfile.LoggerName = PWSTR(session_name.as_mut_ptr());
    // 关键：必须同时设置 PROCESS_TRACE_MODE_EVENT_RECORD，
    // 否则 ETW 不会正确填充 EVENT_RECORD.UserData 指针（会是 null），
    // 导致无法读取事件负载数据。
    logfile.Anonymous1.ProcessTraceMode =
        PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD;
    logfile.Anonymous2.EventRecordCallback = Some(on_event_record);

    let trace_handle = unsafe { OpenTraceW(&mut logfile) };
    if trace_handle.Value == 0 || trace_handle.Value == u64::MAX {
        return Err(anyhow::anyhow!("OpenTraceW 失败，无法打开 ETW 实时事件流"));
    }

    let status = unsafe { ProcessTrace(&[trace_handle], None, None) };
    unsafe {
        let _ = CloseTrace(trace_handle);
    }

    if status != ERROR_SUCCESS && running.load(Ordering::SeqCst) {
        return Err(anyhow::anyhow!("ProcessTrace 失败：0x{:08X}", status.0));
    }
    Ok(())
}

/// ETW 高频回调：过滤事件、取出文件路径，然后立即发送给工作线程。
///
/// 字段读取说明：
/// windows crate 0.58 的 EVENT_HEADER / EVENT_RECORD 布局已经过验证与
/// Windows 内核实际写入一致：
///   - EventHeader.ProcessId      偏移 12
///   - EventHeader.ProviderId     偏移 24
///   - EventHeader.EventDescriptor 偏移 40（Id 在 40，Task 在 46）
///   - UserDataLength             偏移 86
///   - UserData                   偏移 96
/// 因此直接用结构体字段读取即可，无需手动偏移。
unsafe extern "system" fn on_event_record(record: *mut EVENT_RECORD) {
    if record.is_null() {
        return;
    }
    let rec = &*record;
    let pid = rec.EventHeader.ProcessId;

    // 先过滤提供者和 PID，避免为无关事件做任何内存解析。
    if rec.EventHeader.ProviderId != KERNEL_FILE_PROVIDER_GUID || pid == 0 {
        return;
    }

    let event_id = rec.EventHeader.EventDescriptor.Id;
    let user_data_len = rec.UserDataLength as usize;
    let user_data = rec.UserData as *const core::ffi::c_void;

    // Kernel-File 是内核事件，没有可供 TDH 查询的 manifest 模板，
    // 因此按各类事件已确认的二进制布局直接读取。
    match event_id {
        // 打开或新建：取出路径，建立“文件对象 -> 路径”映射。
        EVENT_ID_CREATE | EVENT_ID_CREATE_NEW_FILE => {
            let Some(file_path) = read_filename_from_userdata(user_data, user_data_len) else {
                return;
            };
            if let Some(object) = read_file_object(user_data, user_data_len) {
                remember_path(object, &file_path);
            }

            // Create(12) 打开已有文件时也会触发，只有 CreateNewFile(30)
            // 才是真正新建，所以只上报 30。
            if event_id == EVENT_ID_CREATE_NEW_FILE && MONITOR_CREATE.load(Ordering::Relaxed) {
                send_event(pid, file_path, FileAction::Create);
            }
        }
        // 重命名/移动后的最终路径，当作“新建最终文件”上报。
        EVENT_ID_RENAME_PATH => {
            let Some(file_path) = read_newname_from_userdata(user_data, user_data_len) else {
                return;
            };
            if let Some(object) = read_file_object(user_data, user_data_len) {
                remember_path(object, &file_path);
            }
            if MONITOR_CREATE.load(Ordering::Relaxed) {
                send_event(pid, file_path, FileAction::Create);
            }
        }
        // 写入：Write 事件没有文件名，用文件对象反查路径。
        EVENT_ID_WRITE => {
            if !MONITOR_MODIFY.load(Ordering::Relaxed) {
                return;
            }
            // 实测 Write(16) 的负载是“FileKey(8) + FileObject(8) + ...”，
            // 文件对象指针在 +0x08；而 Create(12/30) 的文件对象在 +0x00。
            let Some(object) =
                read_file_object_at(user_data, user_data_len, WRITE_FILE_OBJECT_OFFSET)
            else {
                return;
            };
            if !should_report_write(object) {
                return;
            }
            if let Some(file_path) = lookup_path(object) {
                send_event(pid, file_path, FileAction::Modify);
            }
        }
        // 句柄关闭：清理映射，避免内存一直增长。
        // 兼容两种负载布局：Create 系列文件对象在 +0x00，
        // Write / Cleanup / Close 系列在 +0x08。
        EVENT_ID_CLEANUP | EVENT_ID_CLOSE => {
            for offset in [0usize, WRITE_FILE_OBJECT_OFFSET] {
                if let Some(object) = read_file_object_at(user_data, user_data_len, offset) {
                    FILE_PATHS.with(|paths| {
                        paths.borrow_mut().remove(&object);
                    });
                    WRITE_THROTTLE.with(|throttle| {
                        throttle.borrow_mut().remove(&object);
                    });
                }
            }
        }
        _ => {}
    }
}

/// 读取事件负载中指定偏移处的文件对象指针。
unsafe fn read_file_object_at(
    user_data: *const core::ffi::c_void,
    data_len: usize,
    offset: usize,
) -> Option<usize> {
    if user_data.is_null() || data_len < offset + 8 {
        return None;
    }
    let object = (user_data as *const u8)
        .add(offset)
        .cast::<u64>()
        .read_unaligned();
    if object == 0 || object == u64::MAX {
        None
    } else {
        Some(object as usize)
    }
}

/// 读取 Create 系列事件的文件对象指针（位于负载开头）。
unsafe fn read_file_object(user_data: *const core::ffi::c_void, data_len: usize) -> Option<usize> {
    read_file_object_at(user_data, data_len, 0)
}

/// 记录“文件对象 -> 路径”。条目过多时整体清空，防止长时间运行内存膨胀。
fn remember_path(object: usize, file_path: &str) {
    FILE_PATHS.with(|paths| {
        let mut paths = paths.borrow_mut();
        if paths.len() >= MAX_TRACKED_FILES {
            paths.clear();
        }
        paths.insert(object, file_path.to_string());
    });
}

/// 按文件对象查路径。
fn lookup_path(object: usize) -> Option<String> {
    FILE_PATHS.with(|paths| paths.borrow().get(&object).cloned())
}

/// 同一个文件对象 1 秒内只允许上报一次修改。
fn should_report_write(object: usize) -> bool {
    WRITE_THROTTLE.with(|throttle| {
        let mut throttle = throttle.borrow_mut();
        let now = Instant::now();
        if let Some(last) = throttle.get(&object) {
            if now.duration_since(*last) < MODIFY_THROTTLE {
                return false;
            }
        }
        if throttle.len() >= MAX_TRACKED_FILES {
            throttle.clear();
        }
        throttle.insert(object, now);
        true
    })
}

/// 把事件发送给工作线程；接收端已经退出时直接忽略。
fn send_event(pid: u32, file_path: String, action: FileAction) {
    let (process_name, process_path) = match process_info_for(pid) {
        Some((name, path)) => (Some(name), Some(path)),
        None => (None, None),
    };

    if let Ok(sender) = EVENT_SENDER.lock() {
        if let Some(sender) = sender.as_ref() {
            let _ = sender.send(FileEvent {
                pid,
                file_path,
                action,
                process_name,
                process_path,
            });
        }
    }
}

/// 取得进程名与完整路径，并按 PID 缓存。
fn process_info_for(pid: u32) -> Option<(String, String)> {
    let cached = PROCESS_INFO.with(|cache| cache.borrow().get(&pid).cloned());
    if let Some(cached) = cached {
        return cached;
    }

    let info = ProcessInfo::from_pid(pid)
        .ok()
        .flatten()
        .map(|process| (process.name, process.path));

    PROCESS_INFO.with(|cache| {
        let mut cache = cache.borrow_mut();
        // 防止长时间运行后缓存无限增长。
        if cache.len() >= 4096 {
            cache.clear();
        }
        cache.insert(pid, info.clone());
    });

    info
}

/// 从 CreateNewFile 事件的 UserData 中读取文件名。
///
/// Kernel-File 是内核事件，manifest 中没有定义模板，无法用 TdhGetProperty
/// 按属性名读取。必须直接解析 UserData 的二进制格式。
///
/// CreateNewFile(30) 的 UserData 布局（64 位 Windows）：
///   +0x00  PVOID  FileObject       (8 字节，内核指针，忽略)
///   +0x08  PVOID  Reserved         (8 字节，另一个指针/保留，忽略)
///   +0x10  ULONG  CreateOptions    (4 字节)
///   +0x14  ULONG  FileAttributes   (4 字节)
///   +0x18  ULONG  ShareAccess      (4 字节)
///   +0x1C  ULONG  Disposition      (4 字节，注意：这里不是 NameLength！)
///   +0x20  WCHAR  Name[]           (文件名，UTF-16，长度 = data_len - 0x20)
///
/// 重要：该结构没有 NameLength 字段，文件名紧跟在 32 字节头部之后，
/// 文件名长度由 UserDataLength - 32 决定（可能包含 null 终止符）。
unsafe fn read_filename_from_userdata(
    user_data: *const core::ffi::c_void,
    data_len: usize,
) -> Option<String> {
    if user_data.is_null() || data_len <= 0x20 || data_len > 64 * 1024 {
        return None;
    }
    let data = std::slice::from_raw_parts(user_data as *const u8, data_len);

    // 文件名从偏移 0x20 开始，长度 = 总长度 - 0x20
    let name_byte_len = data_len - 0x20;
    if name_byte_len == 0 || name_byte_len % 2 != 0 {
        return None;
    }

    // 读取 UTF-16 文件名，去掉可能的 null 终止符
    let name_bytes = &data[0x20..0x20 + name_byte_len];
    let name_u16: Vec<u16> = name_bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&c| c != 0)
        .collect();
    String::from_utf16(&name_u16).ok()
}

/// 从 RenamePath 事件的 UserData 中读取完整路径。
///
/// RenamePath(27) 的 UserData 布局（64 位，实测验证）：
///   +0x00  PVOID  FileObject       (8 字节)
///   +0x08  PVOID  Reserved1        (8 字节)
///   +0x10  PVOID  Reserved2        (8 字节)
///   +0x18  ULONG64 Reserved3       (8 字节)
///   +0x20  ULONG  InformationClass (4 字节)
///   +0x24  ULONG  FileInformationLength / 保留 (4 字节)
///   +0x28  WCHAR  FileName[]       (完整路径，UTF-16，以 null 结尾)
///
/// 与 Rename(19) 不同，RenamePath(27) 在 40 字节头部之后直接带完整路径，
/// 并没有“旧路径长度 + 旧路径 + 新路径长度 + 新路径”两段结构。
unsafe fn read_newname_from_userdata(
    user_data: *const core::ffi::c_void,
    data_len: usize,
) -> Option<String> {
    if user_data.is_null() || data_len <= 0x28 || data_len > 64 * 1024 {
        return None;
    }
    let data = std::slice::from_raw_parts(user_data as *const u8, data_len);

    let name_byte_len = data_len - 0x28;
    if name_byte_len == 0 || name_byte_len % 2 != 0 {
        return None;
    }

    let name_bytes = &data[0x28..];
    let name_u16: Vec<u16> = name_bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&c| c != 0)
        .collect();
    String::from_utf16(&name_u16).ok()
}

/// （保留）使用 TDH 按属性名读取 UTF-16 字符串。
/// 注意：Kernel-File 内核事件没有 manifest 模板，此函数对它们无效，
/// 仅对有模板的用户态事件有效。
#[allow(dead_code)]
unsafe fn read_unicode_property(record: *mut EVENT_RECORD, name: &str) -> Option<String> {
    let property_name = wide(name);
    let descriptor = PROPERTY_DATA_DESCRIPTOR {
        PropertyName: property_name.as_ptr() as u64,
        ArrayIndex: u32::MAX,
        Reserved: 0,
    };

    let mut size = 0u32;
    let status = TdhGetPropertySize(record, None, std::slice::from_ref(&descriptor), &mut size);
    if status != ERROR_SUCCESS.0 || size < 2 || size > 128 * 1024 {
        return None;
    }

    let mut buffer = vec![0u8; size as usize];
    let status = TdhGetProperty(record, None, std::slice::from_ref(&descriptor), &mut buffer);
    if status != ERROR_SUCCESS.0 {
        return None;
    }

    let wide_data = std::slice::from_raw_parts(buffer.as_ptr().cast::<u16>(), buffer.len() / 2);
    let length = wide_data
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(wide_data.len());
    Some(String::from_utf16_lossy(&wide_data[..length]))
}

/// 工作线程：转换路径、过滤目录、查询进程并写入数据库。
fn process_events(receiver: Receiver<FileEvent>, db: Database, config: Config) {
    let drive_mappings = query_drive_mappings();
    let mut process_cache: HashMap<u32, (String, String)> = HashMap::new();

    for event in receiver {
        let file_path = normalize_device_path(&event.file_path, &drive_mappings);
        if file_path.is_empty() || config.should_ignore(Path::new(&file_path)) {
            continue;
        }

        // 新建事件需要区分文件还是文件夹：直接看磁盘上的真实类型。
        // 如果文件已经被删除，就按“文件”处理。
        let is_directory = event.action == FileAction::Create && Path::new(&file_path).is_dir();

        // 按用户勾选的监控范围过滤。
        match event.action {
            FileAction::Create => {
                if is_directory && !config.monitor_create_dir {
                    continue;
                }
                if !is_directory && !config.monitor_create_file {
                    continue;
                }
            }
            FileAction::Modify => {
                // 文件夹本身不是“文件修改”的监控对象：
                // 目录的写操作（如创建/删除子项）会产生大量噪音，直接忽略。
                if !config.monitor_modify_file {
                    continue;
                }
                if Path::new(&file_path).is_dir() {
                    continue;
                }
            }
        }

        // 优先使用 ETW 回调里当场解析好的进程信息（对短命进程也有效）。
        // 只有当回调里没取到时，才在写库线程再尝试一次。
        let process = match (&event.process_name, &event.process_path) {
            (Some(name), Some(path)) => Some((name.clone(), path.clone())),
            _ => match process_cache.get(&event.pid) {
                Some(process) => Some(process.clone()),
                None => {
                    let process = ProcessInfo::from_pid(event.pid)
                        .ok()
                        .flatten()
                        .map(|process| (process.name, process.path));
                    if let Some(process) = &process {
                        if process_cache.len() >= 1024 {
                            process_cache.clear();
                        }
                        process_cache.insert(event.pid, process.clone());
                    }
                    process
                }
            },
        };

        let Some((process_name, process_path)) = process else {
            continue;
        };

        let result = match event.action {
            FileAction::Create => {
                db.upsert_origin(&file_path, &process_name, &process_path, is_directory)
            }
            FileAction::Modify => db.upsert_modification(&file_path, &process_name, &process_path),
        };

        if let Err(error) = result {
            error!("写入文件来源记录失败：{:#}", error);
        }
    }
}

/// 获取 C:、D: 等盘符对应的 \Device\HarddiskVolumeX 路径。
fn query_drive_mappings() -> Vec<(String, String)> {
    let mut mappings = Vec::new();

    for letter in b'A'..=b'Z' {
        let drive = format!("{}:", letter as char);
        let drive_wide = wide(&drive);
        let mut buffer = vec![0u16; 1024];
        let length =
            unsafe { QueryDosDeviceW(PCWSTR(drive_wide.as_ptr()), Some(&mut buffer)) } as usize;
        if length == 0 {
            continue;
        }

        let device = String::from_utf16_lossy(&buffer[..length])
            .trim_end_matches('\0')
            .to_string();
        mappings.push((device, drive));
    }

    // 长前缀优先，避免多个相似设备路径发生错误匹配。
    mappings.sort_by_key(|(device, _)| std::cmp::Reverse(device.len()));
    mappings
}

/// 把内核设备路径转换为资源管理器可识别的盘符路径。
fn normalize_device_path(path: &str, mappings: &[(String, String)]) -> String {
    if let Some(path) = path.strip_prefix(r"\??\") {
        return path.to_string();
    }
    if let Some(path) = path.strip_prefix(r"\\?\") {
        return path.to_string();
    }

    for (device, drive) in mappings {
        if path.len() < device.len() {
            continue;
        }
        let Some(prefix) = path.get(..device.len()) else {
            continue;
        };
        if prefix.eq_ignore_ascii_case(device) {
            return format!("{}{}", drive, &path[device.len()..]);
        }
    }

    path.to_string()
}
