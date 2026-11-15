//! 构建脚本：把程序图标编译进 exe / dll。
//!
//! Windows 的 exe、dll 图标来自 PE 资源节，必须在链接阶段写入。
//! 这里不依赖第三方 crate（离线环境也能构建），直接调用 Windows SDK
//! 自带的资源编译器 `rc.exe`，把 assets/app.ico 编成 .res 再交给链接器。
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

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let icon = manifest_dir.join("assets").join("app.ico");
    if !icon.exists() {
        println!("cargo:warning=找不到 assets/app.ico，跳过图标嵌入");
        return;
    }

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let rc_file = out_dir.join("app.rc");
    let res_file = out_dir.join("app.res");

    // .rc 里的路径需要把反斜杠转义，否则会被当成转义字符。
    let icon_literal = icon.to_string_lossy().replace('\\', "\\\\");
    let script = format!("1 ICON \"{}\"\n", icon_literal);
    if std::fs::write(&rc_file, script).is_err() {
        println!("cargo:warning=写入 app.rc 失败，跳过图标嵌入");
        return;
    }

    let Some(rc_exe) = find_rc_exe() else {
        // 找不到 rc.exe 时不让构建失败：程序仍可运行，只是 exe 显示默认图标。
        println!("cargo:warning=未找到 Windows SDK 的 rc.exe，跳过图标嵌入");
        return;
    };

    let status = Command::new(&rc_exe)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res_file)
        .arg(&rc_file)
        .status();

    match status {
        Ok(status) if status.success() && res_file.exists() => {
            // 把编译好的资源文件作为链接参数传入，exe 和 dll 都会带上图标。
            println!("cargo:rustc-link-arg={}", res_file.display());
        }
        _ => println!("cargo:warning=rc.exe 编译资源失败，跳过图标嵌入"),
    }
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
