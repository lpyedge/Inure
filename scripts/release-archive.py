"""生成原生 full/slim ZIP 和部署依赖索引。"""
import hashlib
import json
import os
from pathlib import Path
import sys
from zipfile import ZIP_DEFLATED, ZipFile


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def bundle(stage, output, include_runtime):
    with ZipFile(output, "w", compression=ZIP_DEFLATED, compresslevel=6) as archive:
        for path in sorted(stage.rglob("*")):
            relative = path.relative_to(stage)
            if relative.parts[0] == "runtime" and not include_runtime:
                continue
            archive.write(path, relative.as_posix())
    checksum = digest(output)
    output.with_suffix(".zip.sha256").write_text(
        f"{checksum}  {output.name}\n", encoding="utf-8"
    )
    print(f"Release package: {output} ({output.stat().st_size} bytes)")
    return checksum


def main():
    stage, target, prefix = sys.argv[1:]
    stage = Path(stage)
    abi = (stage / "runtime/abi.txt").read_text(encoding="ascii").strip()
    release = {"target": target, "abi": abi, "deps_base_url": os.getenv("RIG_RELEASE_BASE_URL")}
    (stage / "release.json").write_text(json.dumps(release) + "\n", encoding="utf-8")

    output = Path("dist")
    full = output / f"{prefix}-{target}-full.zip"
    slim = output / f"{prefix}-{target}-slim.zip"
    sha256 = bundle(stage, full, include_runtime=True)
    bundle(stage, slim, include_runtime=False)
    index = output / f"rig-edge-browser-deps-{target}-{abi}.json"
    index.write_text(json.dumps({"archive": full.name, "sha256": sha256}) + "\n", encoding="utf-8")
    print(f"Dependency index: {index}")


if __name__ == "__main__":
    main()
