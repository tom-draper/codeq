#!/usr/bin/env python3
"""Package a built executable and its matching guide, with SHA-256 checksums."""
import argparse
import hashlib
from pathlib import Path
import shutil
import tarfile
import tempfile
import zipfile

ROOT = Path(__file__).resolve().parent.parent


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("version")
    parser.add_argument("target")
    parser.add_argument("--out", type=Path, default=ROOT / "dist")
    args = parser.parse_args()
    if not args.binary.is_file():
        parser.error(f"missing executable: {args.binary}")
    args.out.mkdir(parents=True, exist_ok=True)
    windows = "windows" in args.target
    extension = ".zip" if windows else ".tar.gz"
    archive = args.out / f"codeq-{args.version}-{args.target}{extension}"
    with tempfile.TemporaryDirectory() as directory:
        stage = Path(directory)
        shutil.copy2(args.binary, stage / ("codeq.exe" if windows else "codeq"))
        shutil.copy2(ROOT / ".agents/skills/codeq/SKILL.md", stage / "GUIDE.md")
        shutil.copy2(ROOT / "LICENSE", stage / "LICENSE")
        if windows:
            with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as output:
                for path in sorted(stage.iterdir()):
                    output.write(path, path.name)
        else:
            with tarfile.open(archive, "w:gz") as output:
                for path in sorted(stage.iterdir()):
                    output.add(path, arcname=path.name)
    checksum = hashlib.sha256(archive.read_bytes()).hexdigest()
    archive.with_name(archive.name + ".sha256").write_text(f"{checksum}  {archive.name}\n")
    print(archive)


if __name__ == "__main__":
    main()
