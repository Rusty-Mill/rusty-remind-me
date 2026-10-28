#!/usr/bin/env python3
"""Reproduce pinned crypto source patches from authenticated crate archives.

Run from any directory: python3 vendor/verify.py. Requires network and git.
Downloads only the URLs/checksums in sources.json and writes only to a temporary
folder. Compare every vendored byte, including licenses and standalone locks.
"""
import hashlib
import io
import json
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import tomllib
import urllib.request

ROOT = Path(__file__).resolve().parent


def files(root: Path) -> dict[str, bytes]:
    return {
        p.relative_to(root).as_posix(): p.read_bytes()
        for p in root.rglob("*")
        if p.is_file() and "target" not in p.relative_to(root).parts
    }


def verify_manifest_alignment(root: Path) -> None:
    """Keep maintained crypto dependency scopes aligned with Cargo's manifest."""
    original = tomllib.loads((root / "Cargo.toml.orig").read_text())
    normalized = tomllib.loads((root / "Cargo.toml").read_text())
    name = normalized["package"]["name"]
    dependencies = (
        ["p256", "crypto-bigint"]
        if name == "aws-sigv4"
        else ["p256", "p384", "signature-v3"]
    )
    for dependency in dependencies:
        source = original.get("dependencies", {}).get(dependency)
        resolved = normalized.get("dependencies", {}).get(dependency)
        if source is None or source != resolved:
            raise SystemExit(f"{name}: {dependency} must match in global dependencies of both manifests")
        if any(
            dependency in target.get("dependencies", {})
            for target in original.get("target", {}).values()
        ):
            raise SystemExit(f"{name}: patched dependency {dependency} unexpectedly target-scoped")
    if name == "jsonwebtoken":
        features = original.get("features", {}).get("rust_crypto")
        if (
            features != normalized.get("features", {}).get("rust_crypto")
            or "dep:signature-v3" not in (features or [])
        ):
            raise SystemExit("jsonwebtoken: rust_crypto must enable the same global signature-v3 alias")


def main() -> None:
    sources = json.loads((ROOT / "sources.json").read_text())
    with tempfile.TemporaryDirectory(prefix="rusty-crypto-provenance-") as temporary:
        stage = Path(temporary)
        for name, source in sources.items():
            with urllib.request.urlopen(source["archive"], timeout=60) as response:
                data = response.read()
            if hashlib.sha256(data).hexdigest() != source["sha256"]:
                raise SystemExit(f"{name}: upstream archive checksum mismatch")
            with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
                archive.extractall(stage, filter="data")
            shutil.move(stage / f"{name}-{source['version']}", stage / name)
            patch = ROOT / "patches" / f"{name}.patch"
            subprocess.run(["git", "apply", "--check", str(patch)], cwd=stage, check=True)
            subprocess.run(["git", "apply", str(patch)], cwd=stage, check=True)
            expected, actual = files(stage / name), files(ROOT / name)
            changed = sorted(k for k in expected.keys() | actual.keys() if expected.get(k) != actual.get(k))
            if changed:
                raise SystemExit(f"{name}: unreproducible files: {changed}")
            verify_manifest_alignment(ROOT / name)
            print(f"{name} {source['version']}: verified archive + patch + manifest alignment, {len(actual)} files")


if __name__ == "__main__":
    main()
