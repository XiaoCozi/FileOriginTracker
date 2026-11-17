//! 构建脚本：把程序图标和版本信息编译进 exe / dll。
//!
//! Windows 的图标、公司名、产品名、文件版本都来自 PE 资源节，
//! 必须在链接阶段写入。这里不依赖第三方 crate（离线环境也能构建），
//! 直接调用 Windows SDK 自带的资源编译器 `rc.exe`。
//!
//! 为什么要写版本信息：没有 CompanyName / ProductName / FileVersion 的 exe
//! 会被杀软的启发式引擎视为“来源不明”，误报概率明显更高。
//! 版本号直接取自 Cargo.toml，不需要在两处手动同步。
//!
//! exe 和 dll 分别编译一份资源，因为两者的文件类型不同：
//! - exe 用 VFT_APP，OriginalFilename 写 FileOriginTracker.exe
//! - dll 用 VFT_DLL，OriginalFilename 写 file_origin_tracker_shell.dll
//!
//! 图标资源 ID 为 1，程序运行时用 LoadIconW(模块, MAKEINTRESOURCE(1)) 读取。

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // 只在为 Windows 目标编译时才处理资源，避免其他平台构建失败。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    println!("cargo:rerun-if-changed=assets/app.ico");
    println!("cargo:rerun-if-changed=build.rs");
    // 版本号变化时重新生成资源，保证属性页里的版本始终和 Cargo.toml 一致。
    println!("cargo:rerun-if-env-changed=CARGO_PKG_VERSION");

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let icon_path = manifest_dir.join("assets").join("app.ico");

    // 图标缺失时仍然要写版本信息，所以这里用 Option 而不是直接返回。
    let icon = icon_path.exists().then_some(icon_path.as_path());
    if icon.is_none() {
        println!("cargo:warning=找不到 assets/app.ico，跳过图标嵌入");
    }

    let Some(rc_exe) = find_rc_exe() else {
        // 找不到 rc.exe 时不让构建失败：程序仍可运行，只是没有图标和版本信息。
        println!("cargo:warning=未找到 Windows SDK 的 rc.exe，跳过图标与版本信息嵌入");
        return;
    };

    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    let numbers = version_numbers(&version);

    build_resource(
        &rc_exe,
        &out_dir,
        icon,
        &version,
        numbers,
        ResourceKind::Exe,
    );
    build_resource(
        &rc_exe,
        &out_dir,
        icon,
        &version,
        numbers,
        ResourceKind::Dll,
    );
}

/// 资源要写入的目标类型，决定 FILETYPE 和几个字符串字段。
#[derive(Clone, Copy)]
enum ResourceKind {
    Exe,
    Dll,
}

/// 生成 .rc 脚本、编译成 .res，并把链接参数告诉 cargo。
fn build_resource(
    rc_exe: &Path,
    out_dir: &Path,
    icon: Option<&Path>,
    version: &str,
    numbers: [u32; 4],
    kind: ResourceKind,
) {
    let (stem, file_type, description, original_name, internal_name) = match kind {
        ResourceKind::Exe => (
            "app_exe",
            "0x1L", // VFT_APP：可执行程序
            "FileOriginTracker 文件来源追踪工具",
            "FileOriginTracker.exe",
            "file-origin-tracker",
        ),
        ResourceKind::Dll => (
            "app_dll",
            "0x2L", // VFT_DLL：动态链接库
            "FileOriginTracker 右键菜单扩展",
            "file_origin_tracker_shell.dll",
            "file_origin_tracker_shell",
        ),
    };

    let rc_file = out_dir.join(format!("{}.rc", stem));
    let res_file = out_dir.join(format!("{}.res", stem));

    let mut script = String::new();
    // 告诉 rc.exe 按 UTF-8 读取本文件，否则中文描述会变成乱码。
    script.push_str("#pragma code_page(65001)\r\n\r\n");

    if let Some(icon) = icon {
        // .rc 里反斜杠是转义字符，路径必须写成双反斜杠。
        let literal = icon.to_string_lossy().replace('\\', "\\\\");
        script.push_str(&format!("1 ICON \"{}\"\r\n\r\n", literal));
    }

    script.push_str(&format!(
        r#"1 VERSIONINFO
FILEVERSION {0},{1},{2},{3}
PRODUCTVERSION {0},{1},{2},{3}
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE {4}
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "080404B0"
        BEGIN
            VALUE "CompanyName", "XiaoCozi"
            VALUE "FileDescription", "{5}"
            VALUE "FileVersion", "{6}"
            VALUE "InternalName", "{7}"
            VALUE "LegalCopyright", "Copyright (C) 2026 XiaoCozi"
            VALUE "OriginalFilename", "{8}"
            VALUE "ProductName", "FileOriginTracker"
            VALUE "ProductVersion", "{6}"
            VALUE "Comments", "https://github.com/XiaoCozi/FileOriginTracker"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x804, 1200
    END
END
"#,
        numbers[0],
        numbers[1],
        numbers[2],
        numbers[3],
        file_type,
        description,
        version,
        internal_name,
        original_name
    ));

    if std::fs::write(&rc_file, script).is_err() {
        println!(
            "cargo:warning=写入 {} 失败，跳过资源嵌入",
            rc_file.display()
        );
        return;
    }

    let status = Command::new(rc_exe)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res_file)
        .arg(&rc_file)
        .status();

    match status {
        Ok(status) if status.success() && res_file.exists() => {
            // exe 的资源只给 bin 目标、dll 的资源只给 cdylib 目标，避免互相串用。
            let flag = match kind {
                ResourceKind::Exe => "cargo:rustc-link-arg-bins",
                ResourceKind::Dll => "cargo:rustc-link-arg-cdylib",
            };
            println!("{}={}", flag, res_file.display());
        }
        _ => println!("cargo:warning=rc.exe 编译资源失败，跳过资源嵌入"),
    }
}

/// 把 "0.1.6" 这样的版本号转成 VERSIONINFO 要求的四段数字。
///
/// 带后缀的版本（例如 "0.1.6-beta"）只取数字和点的部分，避免解析失败。
fn version_numbers(version: &str) -> [u32; 4] {
    let mut parts = [0u32; 4];
    let core: String = version
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    for (index, part) in core.split('.').take(4).enumerate() {
        parts[index] = part.parse().unwrap_or(0);
    }
    parts
}

/// 在 Windows SDK 安装目录里查找 x64 版的 rc.exe（取版本号最大的一个）。
fn find_rc_exe() -> Option<PathBuf> {
    let kits = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let entries = std::fs::read_dir(kits).ok()?;

    let mut versions: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    // 目录名是版本号，倒序排列后第一个就是最新的 SDK。
    versions.sort();
    versions.reverse();

    for version in versions {
        let candidate = version.join("x64").join("rc.exe");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}
