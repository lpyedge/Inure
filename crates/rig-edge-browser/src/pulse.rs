use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const SINK_NAME: &str = "rigsink";

pub struct PulseAudio {
    child: Child,
    runtime_dir: PathBuf,
    server: String,
}

struct PortablePaths {
    executable: PathBuf,
    library_dir: PathBuf,
    module_dir: PathBuf,
}

impl PulseAudio {
    pub async fn start(portable_dir: Option<&str>) -> Result<Option<Self>, String> {
        let portable = portable_dir
            .map(Path::new)
            .map(find_portable_paths)
            .transpose()?;
        let mut command = match &portable {
            Some(paths) => {
                let mut command = Command::new(&paths.executable);
                command.env(
                    "LD_LIBRARY_PATH",
                    join_paths(&[
                        paths.library_dir.clone(),
                        paths.library_dir.join("pulseaudio"),
                        paths.module_dir.clone(),
                    ])?,
                );
                command.arg(format!("--dl-search-path={}", paths.module_dir.display()));
                command
            }
            None => Command::new("pulseaudio"),
        };

        let runtime_dir = runtime_dir()?;
        let socket = runtime_dir.join("native");
        command
            .args([
                "--daemonize=no",
                "--exit-idle-time=-1",
                "--disallow-exit",
                "--fail",
                "-n",
                "--load=module-native-protocol-unix",
                &format!("--load=module-null-sink sink_name={SINK_NAME}"),
            ])
            .env("PULSE_RUNTIME_PATH", &runtime_dir)
            .env("XDG_CONFIG_HOME", &runtime_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) if portable.is_none() && error.kind() == std::io::ErrorKind::NotFound => {
                remove_runtime_dir(&runtime_dir);
                return Ok(None);
            }
            Err(error) => {
                remove_runtime_dir(&runtime_dir);
                return Err(format!("failed to start PulseAudio: {error}"));
            }
        };
        let mut pulse = Self {
            child,
            runtime_dir,
            server: format!("unix:{}", socket.display()),
        };

        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            if let Some(status) = pulse
                .child
                .try_wait()
                .map_err(|error| format!("failed to inspect PulseAudio process: {error}"))?
            {
                return Err(format!("PulseAudio exited during startup ({status})"));
            }
            if is_socket(&socket) {
                return Ok(Some(pulse));
            }
            if Instant::now() >= deadline {
                return Err("PulseAudio native socket did not appear within 4 seconds".into());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub fn server(&self) -> &str {
        &self.server
    }
}

impl Drop for PulseAudio {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        remove_runtime_dir(&self.runtime_dir);
    }
}

fn find_portable_paths(root: &Path) -> Result<PortablePaths, String> {
    let executable = root.join("usr/bin/pulseaudio");
    if !is_executable(&executable) {
        return Err(format!(
            "--pulseaudio-dir must contain executable usr/bin/pulseaudio: {}",
            executable.display()
        ));
    }

    let lib_root = root.join("usr/lib");
    let entries = std::fs::read_dir(&lib_root)
        .map_err(|error| format!("cannot read {}: {error}", lib_root.display()))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| format!("cannot read {}: {error}", lib_root.display()))?;
    let library_dir = entries
        .iter()
        .find(|path| path.join("pulseaudio").is_dir())
        .cloned()
        .ok_or_else(|| {
            format!(
                "--pulseaudio-dir has no usr/lib/<architecture>/pulseaudio directory: {}",
                lib_root.display()
            )
        })?;

    let module_dir = entries
        .into_iter()
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("pulse-"))
                && path
                    .join("modules/module-native-protocol-unix.so")
                    .is_file()
                && path.join("modules/module-null-sink.so").is_file()
        })
        .map(|path| path.join("modules"))
        .ok_or_else(|| {
        format!(
            "--pulseaudio-dir lacks module-native-protocol-unix.so or module-null-sink.so under usr/lib/pulse-*/modules: {}",
            lib_root.display()
        )
    })?;

    Ok(PortablePaths {
        executable,
        library_dir,
        module_dir,
    })
}

fn runtime_dir() -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("rig-edge-{}-pulse", std::process::id()));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&dir).map_err(|error| {
        format!(
            "cannot create PulseAudio runtime directory {}: {error}",
            dir.display()
        )
    })?;
    Ok(dir)
}

fn remove_runtime_dir(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

fn join_paths(paths: &[PathBuf]) -> Result<String, String> {
    std::env::join_paths(paths)
        .map(|value| value.to_string_lossy().into_owned())
        .map_err(|error| format!("invalid PulseAudio library path: {error}"))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn is_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::metadata(path)
        .map(|metadata| metadata.file_type().is_socket())
        .unwrap_or(false)
}
