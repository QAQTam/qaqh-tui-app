#!/usr/bin/env python3
"""Build/package only qaqh-tui. No daemon builds, credentials, or installer side effects."""
from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
import tempfile
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parent.parent
TARGETS = {
    "x86_64-pc-windows-msvc": ("windows-x64", ".exe", ".zip"),
    "x86_64-unknown-linux-gnu": ("linux-x64", "", ".tar.gz"),
    "x86_64-apple-darwin": ("macos-intel", "", ".tar.gz"),
    "aarch64-apple-darwin": ("macos-arm64", "", ".tar.gz"),
}


def dependencies(root: Path = ROOT) -> dict:
    config = json.loads((root / "release/dependencies.json").read_text(encoding="utf-8"))
    if not re.fullmatch(r"\d+\.\d+\.\d+", config["rust"]):
        raise ValueError("release Rust toolchain must be an exact stable version")
    for name in ("backend", "ratatui"):
        entry = config[name]
        if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", entry["repository"]):
            raise ValueError(f"invalid {name} repository")
        if not re.fullmatch(r"[0-9a-f]{40}", entry["revision"]):
            raise ValueError(f"{name} revision must be a full commit SHA")
    return config


def version(root: Path = ROOT) -> str:
    with (root / "Cargo.toml").open("rb") as file:
        return tomllib.load(file)["package"]["version"]


def validate_tag(tag: str, expected: str) -> None:
    if not re.fullmatch(r"\d+\.\d+\.\d+", tag) or tag != expected:
        raise ValueError(f"release tag {tag!r} must exactly match Cargo version {expected!r} (no v prefix)")


def export_metadata(root: Path, output: Path) -> None:
    config = dependencies(root)
    lines = [f"rust={config['rust']}", f"version={version(root)}"]
    for name in ("backend", "ratatui"):
        lines.extend(f"{name}_{key}={value}" for key, value in config[name].items())
    with output.open("a", encoding="utf-8") as file:
        file.write("\n".join(lines) + "\n")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as file:
        for chunk in iter(lambda: file.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def package_binary(binary: Path, target: str, tag: str, out: Path, root: Path = ROOT) -> Path:
    """Package an already built executable; useful separately from expensive compilation."""
    validate_tag(tag, version(root))
    label, suffix, extension = TARGETS[target]
    if not binary.is_file() or binary.stat().st_size == 0:
        raise ValueError(f"missing/empty TUI executable: {binary}")
    out.mkdir(parents=True, exist_ok=True)
    stem = f"qaqh-tui-{tag}-{label}"
    archive = out / f"{stem}{extension}"
    with tempfile.TemporaryDirectory(prefix="qaqh-package-") as temp:
        staging = Path(temp)
        executable = staging / f"qaqh-tui{suffix}"
        # Copy only the selected TUI binary, never target/release as a whole.
        import shutil
        shutil.copyfile(binary, executable)
        executable.chmod(0o755)
        for license_name in ("LICENSE-MIT", "LICENSE-APACHE"):
            shutil.copyfile(root / license_name, staging / license_name)
        info = {
            "name": "qaqh-tui", "version": tag, "target": target,
            "dependencies": dependencies(root),
            "note": "TUI only; install/connect to a compatible daemon separately.",
        }
        (staging / "BUILD-INFO.json").write_text(json.dumps(info, indent=2) + "\n", encoding="utf-8")
        files = sorted(staging.iterdir())
        if extension == ".zip":
            with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as output:
                for path in files:
                    entry = zipfile.ZipInfo(f"{stem}/{path.name}", date_time=(1980, 1, 1, 0, 0, 0))
                    entry.create_system = 3
                    entry.external_attr = (0o100755 if path == executable else 0o100644) << 16
                    entry.compress_type = zipfile.ZIP_DEFLATED
                    output.writestr(entry, path.read_bytes())
        else:
            with archive.open("wb") as raw, gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as compressed:
                with tarfile.open(fileobj=compressed, mode="w") as output:
                    for path in files:
                        data = path.read_bytes()
                        entry = tarfile.TarInfo(f"{stem}/{path.name}")
                        entry.size = len(data)
                        entry.mode = 0o755 if path == executable else 0o644
                        entry.mtime = 0
                        output.addfile(entry, io.BytesIO(data))
    (out / f"{archive.name}.sha256").write_text(f"{sha256(archive)}  {archive.name}\n", encoding="ascii")
    return archive


def verify_dependencies(root: Path) -> None:
    config = dependencies(root)
    for name, directory in (("backend", "qaqh-backend"), ("ratatui", "ratatui")):
        checkout = root.parent / directory
        actual = subprocess.check_output(["git", "-C", str(checkout), "rev-parse", "HEAD"], text=True).strip()
        if actual != config[name]["revision"]:
            raise ValueError(f"{name} checkout mismatch: expected {config[name]['revision']}, got {actual}")
        # Documentation changes are harmless locally, but release inputs must be clean.
        changes = subprocess.check_output(["git", "-C", str(checkout), "status", "--porcelain", "--untracked-files=no"], text=True)
        if changes.strip():
            raise ValueError(f"{name} checkout has tracked changes; release inputs must be clean")


def build(target: str, tag: str, out: Path, root: Path = ROOT) -> Path:
    validate_tag(tag, version(root))
    verify_dependencies(root)
    subprocess.run([
        "cargo", "build", "--locked", "--release", "--bin", "qaqh-tui", "--target", target,
    ], cwd=root, check=True)
    target_dir = Path(os.environ.get("CARGO_TARGET_DIR", root / "target"))
    if not target_dir.is_absolute():
        target_dir = root / target_dir
    binary = target_dir / target / "release" / f"qaqh-tui{TARGETS[target][1]}"
    # CI builds on the native OS/architecture, so execute a harmless CLI probe before upload.
    actual = subprocess.check_output([str(binary), "--version"], text=True).strip()
    if actual != f"qaqh-tui {tag}":
        raise ValueError(f"binary version mismatch: {actual}")
    return package_binary(binary, target, tag, out, root)


def collect(out: Path, tag: str, root: Path = ROOT) -> Path:
    validate_tag(tag, version(root))
    expected = {f"qaqh-tui-{tag}-{label}{extension}" for label, _, extension in TARGETS.values()}
    actual = {path.name for path in out.iterdir() if path.name.endswith((".zip", ".tar.gz"))}
    if actual != expected:
        raise ValueError(f"release must contain exactly four platform packages; missing={expected - actual}, extra={actual - expected}")
    lines = []
    for name in sorted(expected):
        checksum = f"{sha256(out / name)}  {name}\n"
        if (out / f"{name}.sha256").read_text(encoding="ascii") != checksum:
            raise ValueError(f"checksum mismatch: {name}")
        lines.append(checksum)
    output = out / "SHA256SUMS.txt"
    output.write_text("".join(lines), encoding="ascii")
    return output


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    metadata = sub.add_parser("metadata")
    metadata.add_argument("--github-output", type=Path, required=True)
    check = sub.add_parser("check-tag")
    check.add_argument("--tag", required=True)
    build_parser = sub.add_parser("build")
    build_parser.add_argument("--target", choices=TARGETS, required=True)
    build_parser.add_argument("--tag", required=True)
    build_parser.add_argument("--out", type=Path, default=Path("dist"))
    collect_parser = sub.add_parser("collect")
    collect_parser.add_argument("--tag", required=True)
    collect_parser.add_argument("--out", type=Path, default=Path("dist"))
    args = parser.parse_args()
    if args.command == "metadata":
        export_metadata(ROOT, args.github_output)
    elif args.command == "check-tag":
        validate_tag(args.tag, version())
        print(f"release tag/version OK: {args.tag}")
    elif args.command == "build":
        print(build(args.target, args.tag, args.out.resolve()))
    else:
        print(collect(args.out.resolve(), args.tag))


if __name__ == "__main__":
    main()
