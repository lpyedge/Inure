//! 发布包入口先处理依赖部署，再启动链接 FFmpeg 的边缘进程。
mod deploy;

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

pub(crate) type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match run(&args) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("rig-edge-browser: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[OsString]) -> Result<u8> {
    if args == ["--help"] {
        println!(
            "Usage: rig-edge-browser [browser options]\n\
             Setup or update: rig-edge-browser --install-deps [--with-chromium] [--deps-base-url HTTPS_URL]\n\
             Run --install-deps again to refresh portable dependencies."
        );
        return Ok(0);
    }
    let exe = std::env::current_exe()?;
    let root = exe.parent().ok_or("Cannot locate the release directory.")?;
    if args.first().is_some_and(|arg| arg == "--install-deps") {
        deploy::install(root, &args[1..])?;
        return Ok(0);
    }
    if args.iter().any(|arg| {
        ["--install-deps", "--with-chromium", "--deps-base-url"]
            .iter()
            .any(|flag| arg == flag)
    }) {
        return Err("Use --install-deps as a separate deployment command.".into());
    }
    if !root.join("runtime/lib").is_dir() {
        return Err("Portable runtime is missing. Run --install-deps first.".into());
    }
    let mut command = engine_command(root)?;
    command
        .env("RIG_EDGE_BUNDLED_CHROMIUM_DIR", root.join("chromium"))
        .args(args);
    #[cfg(target_os = "linux")]
    command
        .arg("--pulseaudio-dir")
        .arg(root.join("runtime/pulseaudio"));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec().into())
    }
    #[cfg(not(unix))]
    {
        let status = command.status()?;
        Ok(status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .unwrap_or(1))
    }
}

pub(crate) fn engine_path(root: &Path) -> PathBuf {
    root.join("engine").join(format!(
        "rig-edge-browser-core{}",
        std::env::consts::EXE_SUFFIX
    ))
}

pub(crate) fn engine_command(root: &Path) -> Result<Command> {
    let engine = engine_path(root);
    #[cfg(target_os = "linux")]
    {
        let loader = match std::env::consts::ARCH {
            "x86_64" => "/lib64/ld-linux-x86-64.so.2",
            "aarch64" => "/lib/ld-linux-aarch64.so.1",
            _ => return Err("Unsupported Linux architecture.".into()),
        };
        let mut command = Command::new(loader);
        command
            .arg("--library-path")
            .arg(root.join("runtime/lib"))
            .arg(engine);
        Ok(command)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let mut command = Command::new(engine);
        #[cfg(windows)]
        {
            let mut paths = vec![root.join("runtime/lib")];
            if let Some(path) = std::env::var_os("PATH") {
                paths.extend(std::env::split_paths(&path));
            }
            command.env("PATH", std::env::join_paths(paths)?);
        }
        #[cfg(not(windows))]
        command.env_remove("DYLD_LIBRARY_PATH");
        Ok(command)
    }
}

pub(crate) fn executable_name(file: &OsStr) -> bool {
    file == format!("chrome{}", std::env::consts::EXE_SUFFIX).as_str()
}
