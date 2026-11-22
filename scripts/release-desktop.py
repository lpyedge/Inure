#!/usr/bin/env python3
"""Build native Windows/macOS binaries and stage their media runtime."""

from __future__ import annotations

import os
import platform
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
VCPKG_FEATURES = "avcodec,avdevice,avformat,swresample,swscale,gpl,x264,x265"


class ReleaseError(RuntimeError):
    pass


def run(args: list[str], *, env: dict[str, str] | None = None) -> str:
    print("+", subprocess.list2cmdline(args), flush=True)
    result = subprocess.run(args, cwd=ROOT, env=env, text=True, capture_output=True)
    if result.stdout:
        print(result.stdout, end="")
    if result.stderr:
        print(result.stderr, end="", file=sys.stderr)
    if result.returncode:
        raise ReleaseError(f"Command failed ({result.returncode}): {subprocess.list2cmdline(args)}")
    return result.stdout


def require_tool(name: str, guidance: str) -> str:
    path = shutil.which(name)
    if not path:
        raise ReleaseError(f"Required tool '{name}' was not found. {guidance}")
    return path


def platform_config() -> tuple[str, str, str, str]:
    os_name = platform.system()
    machine = platform.machine().lower()
    if os_name == "Windows" and machine in {"amd64", "x86_64"}:
        return "windows", "x64-windows", "x86_64-pc-windows-msvc", "rig-edge-browser.exe"
    if os_name == "Windows" and machine in {"arm64", "aarch64"}:
        return "windows", "arm64-windows", "aarch64-pc-windows-msvc", "rig-edge-browser.exe"
    if os_name == "Darwin" and machine in {"arm64", "aarch64"}:
        return "macos", "arm64-osx", "aarch64-apple-darwin", "rig-edge-browser"
    raise ReleaseError(
        f"Unsupported native build platform: {os_name} {platform.machine()}. "
        "Supported runners are Windows x64/ARM64 and Apple silicon macOS ARM64."
    )


def find_vcpkg() -> tuple[Path, Path]:
    root_env = os.environ.get("VCPKG_ROOT")
    if not root_env:
        raise ReleaseError(
            "VCPKG_ROOT must point to the official Microsoft vcpkg checkout bootstrapped by the workflow."
        )
    root = Path(root_env).expanduser().resolve()
    exe = root / ("vcpkg.exe" if os.name == "nt" else "vcpkg")
    if not exe.is_file():
        raise ReleaseError(
            f"vcpkg executable was not found at {exe}. Set VCPKG_ROOT to the bootstrapped Microsoft vcpkg checkout."
        )
    if not (root / "ports" / "ffmpeg").is_dir():
        raise ReleaseError(f"VCPKG_ROOT does not look like a Microsoft vcpkg checkout: {root}")
    return root, exe


def ffmpeg_abi(prefix: Path) -> str:
    headers = prefix / "include" / "libavcodec"
    for filename in ("version_major.h", "version.h"):
        header = headers / filename
        if header.is_file():
            contents = header.read_text(encoding="utf-8", errors="replace")
            match = re.search(r"^\s*#\s*define\s+LIBAVCODEC_VERSION_MAJOR\s+(\d+)", contents, re.M)
            if match:
                return f"ffmpeg-{match.group(1)}"
    raise ReleaseError(f"Could not read LIBAVCODEC_VERSION_MAJOR from FFmpeg headers under {headers}")


def build(stage: Path, kind: str, triplet: str, target: str, executable: str) -> None:
    vcpkg_root, vcpkg = find_vcpkg()
    # 内置 macOS triplet 默认是静态库；Windows 媒体 DLL 使用静态 CRT。
    with tempfile.TemporaryDirectory(dir=ROOT / ".temp") as overlay:
        settings = (vcpkg_root / "triplets" / f"{triplet}.cmake").read_text(encoding="ascii")
        if kind == "macos":
            settings += '\nset(VCPKG_LIBRARY_LINKAGE dynamic)\nset(VCPKG_LINKER_FLAGS "-Wl,-headerpad_max_install_names")\n'
        else:
            settings += "\nset(VCPKG_CRT_LINKAGE static)\n"
        triplet += "-rig"
        (Path(overlay) / f"{triplet}.cmake").write_text(settings, encoding="ascii")
        run([str(vcpkg), "install", f"--overlay-triplets={overlay}", f"ffmpeg[{VCPKG_FEATURES}]:{triplet}"])
    prefix = vcpkg_root / "installed" / triplet
    if not (prefix / "include" / "libavcodec").is_dir() or not (prefix / "lib").is_dir():
        raise ReleaseError(f"vcpkg did not install FFmpeg headers/libraries under {prefix}")

    abi = ffmpeg_abi(prefix)
    require_tool("rustc", "Install the stable Rust toolchain with the requested target installed.")
    require_tool("cargo", "Install Cargo from rustup.")
    require_tool("clang", "Install LLVM/Clang. Bindgen also needs libclang; set LIBCLANG_PATH to its containing directory.")
    libclang = os.environ.get("LIBCLANG_PATH")
    if not libclang or not Path(libclang).expanduser().is_dir():
        detail = "Set LIBCLANG_PATH to the directory containing libclang.dll." if kind == "windows" else "Set LIBCLANG_PATH to the directory containing libclang.dylib."
        raise ReleaseError("ffmpeg-sys-next uses bindgen and requires libclang. " + detail)

    env = os.environ.copy()
    env["FFMPEG_DIR"] = str(prefix)
    env["VCPKG_ROOT"] = str(vcpkg_root)
    env["VCPKGRS_TRIPLET"] = triplet
    rustflag = "-C target-feature=+crt-static" if kind == "windows" else "-C link-arg=-Wl,-headerpad_max_install_names"
    env["RUSTFLAGS"] = f'{env.get("RUSTFLAGS", "")} {rustflag}'.strip()
    # Honor caller-selected Cargo cache location while explicitly producing the runner's native ABI.
    cargo_target_dir = env.get("CARGO_TARGET_DIR")
    run(["cargo", "build", "--locked", "--release", "-p", "rig-edge-browser", "--target", target], env=env)
    run(["cargo", "build", "--locked", "--release", "-p", "rig-launcher", "--target", target], env=env)
    target_dir = Path(cargo_target_dir).expanduser() if cargo_target_dir else ROOT / "target"
    if not target_dir.is_absolute():
        target_dir = ROOT / target_dir
    built = target_dir / target / "release"
    launcher_name = "rig-launcher.exe" if kind == "windows" else "rig-launcher"
    launcher = built / launcher_name
    core = built / executable
    if not launcher.is_file() or not core.is_file():
        raise ReleaseError(f"Expected native build outputs at {launcher} and {core}")

    stage.mkdir(parents=True, exist_ok=True)
    runtime_lib = stage / "runtime" / "lib"
    runtime_lib.mkdir(parents=True, exist_ok=True)
    shutil.copy2(launcher, stage / executable)
    engine = stage / "engine"
    engine.mkdir(exist_ok=True)
    staged_core = engine / executable.replace("rig-edge-browser", "rig-edge-browser-core")
    shutil.copy2(core, staged_core)
    (stage / "runtime" / "abi.txt").write_text(abi + "\n", encoding="ascii")
    licenses = stage / "runtime" / "licenses"
    copy_license_notices(prefix, licenses)
    if kind == "windows":
        copy_windows_dlls(prefix / "bin", runtime_lib)
    else:
        copy_macos_closure(staged_core, prefix / "lib", runtime_lib)
        codesign = require_tool("codesign", "Xcode command line tools are required for ad-hoc signing.")
        for binary in sorted(runtime_lib.glob("*.dylib")):
            run([codesign, "--force", "--sign", "-", str(binary)])
        run([codesign, "--force", "--sign", "-", str(staged_core)])
        run([codesign, "--force", "--sign", "-", str(stage / executable)])
    print(f"Staged {kind} {platform.machine()} media ABI {abi} at {stage}")


def copy_license_notices(prefix: Path, destination: Path) -> None:
    notices = list((prefix / "share").glob("*/copyright"))
    notices.extend((prefix / "share").glob("*/copyright.txt"))
    if not notices:
        raise ReleaseError(f"No vcpkg package copyright notices found under {prefix / 'share'}")
    destination.mkdir(parents=True, exist_ok=True)
    for notice in notices:
        package = notice.parent.name
        target = destination / f"{package}-{notice.name}"
        if not target.exists():
            shutil.copy2(notice, target)


def copy_windows_dlls(source_dir: Path, destination: Path) -> None:
    dlls = sorted(source_dir.glob("*.dll"))
    if not dlls:
        raise ReleaseError(f"No FFmpeg runtime DLLs found under {source_dir}; verify the dynamic vcpkg triplet installation.")
    for dll in dlls:
        shutil.copy2(dll, destination / dll.name)


def is_macos_system_dependency(path: str) -> bool:
    return path.startswith(("/System/Library/", "/usr/lib/"))


def macos_dependencies(binary: Path) -> list[str]:
    output = run(["otool", "-L", str(binary)])
    lines = output.splitlines()[1:]
    deps: list[str] = []
    for line in lines:
        dep = line.strip().split(" (compatibility version", 1)[0]
        if dep:
            deps.append(dep)
    return deps


def resolve_macos_dependency(dependency: str, lib_dir: Path) -> Path:
    basename = Path(dependency).name
    candidate = lib_dir / basename
    if candidate.is_file():
        return candidate.resolve()
    matches = list(lib_dir.glob(f"**/{basename}"))
    if matches:
        return matches[0].resolve()
    raise ReleaseError(f"Could not resolve non-system macOS dependency {dependency} under {lib_dir}")


def copy_macos_closure(core: Path, lib_dir: Path, destination: Path) -> None:
    require_tool("otool", "Xcode command line tools are required.")
    install_name_tool = require_tool("install_name_tool", "Xcode command line tools are required.")
    queue: list[tuple[Path, bool]] = [(core, True)]
    copied: dict[Path, Path] = {}
    while queue:
        binary, is_core = queue.pop(0)
        changes: list[tuple[str, str]] = []
        for dependency in macos_dependencies(binary):
            if is_macos_system_dependency(dependency):
                continue
            source = resolve_macos_dependency(dependency, lib_dir)
            staged = copied.get(source)
            if staged is None:
                staged = destination / source.name
                if staged.exists() and source.resolve() != staged.resolve():
                    raise ReleaseError(f"Two FFmpeg libraries have the same filename: {source.name}")
                shutil.copy2(source, staged)
                copied[source] = staged
                queue.append((staged, False))
            install_ref = f"@executable_path/../runtime/lib/{staged.name}" if is_core else f"@loader_path/{staged.name}"
            if dependency != install_ref:
                changes.append((dependency, install_ref))
        if not is_core:
            run([install_name_tool, "-id", f"@loader_path/{binary.name}", str(binary)])
        for old, new in changes:
            run([install_name_tool, "-change", old, new, str(binary)])


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("Usage: python scripts/release-desktop.py <absolute-stage-root>", file=sys.stderr)
        return 2
    stage = Path(argv[1]).expanduser()
    if not stage.is_absolute():
        print("Stage root must be an absolute path.", file=sys.stderr)
        return 2
    try:
        kind, triplet, target, executable = platform_config()
        build(stage.resolve(), kind, triplet, target, executable)
    except (ReleaseError, OSError) as error:
        print(f"Desktop release preparation failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
