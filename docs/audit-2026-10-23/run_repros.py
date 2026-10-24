#!/usr/bin/env python3
"""Run isolated audit assertions, then remove their temporary module declaration.

Current audited source is expected to fail these assertions. No daemon is started.
Only fake data and temporary files are used. Default cargo tests do not include
these assertions. Run from any directory with Python 3 and the Rust toolchain.
"""
from pathlib import Path
import os
import subprocess
import sys


def main() -> int:
    audit_dir = Path(__file__).resolve().parent
    root = audit_dir.parent.parent
    source = root / "src/app/mod.rs"
    marker = (
        b'\n#[cfg(test)]\n'
        b'#[path = "../../docs/audit-2026-10-23/repros.rs"]\n'
        b'mod audit_repros;\n'
    )
    original = source.read_bytes()
    if b"mod audit_repros;" in original:
        raise RuntimeError("audit_repros is already declared; remove the old audit declaration first")
    try:
        source.write_bytes(original + marker)
        env = dict(os.environ, TMPDIR=str(root / "target"))
        (root / "target").mkdir(exist_ok=True)
        result = subprocess.run(
            ["cargo", "test", "--offline", "--locked", "-j", "1", "audit_repros", "--", "--test-threads=1", "--nocapture"],
            cwd=root, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
        )
        (audit_dir / "repro-output.txt").write_text(result.stdout, encoding="utf-8")
        print(result.stdout)
        return result.returncode
    finally:
        # Preserve any unrelated edits made while cargo was running.
        current = source.read_bytes()
        if current.count(marker) == 1:
            source.write_bytes(current.replace(marker, b"", 1))
        else:
            raise RuntimeError("source changed around the audit declaration; inspect src/app/mod.rs")


if __name__ == "__main__":
    sys.exit(main())
