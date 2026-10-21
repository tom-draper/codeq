#!/usr/bin/env python3
"""Verify pinned installation and checksum rejection against a local release."""
import argparse
import hashlib
import os
from pathlib import Path
import platform
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parent.parent


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', type=Path)
    args = parser.parse_args()
    version = subprocess.check_output([str(args.binary.resolve()), '--version'], text=True).split()[1]
    tag = f'v{version}'
    system = platform.system()
    arch = 'aarch64' if platform.machine() in ('arm64', 'aarch64') else 'x86_64'
    target = f'{arch}-unknown-linux-musl' if system == 'Linux' else f'{arch}-apple-darwin'
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        release = root / tag
        subprocess.run(['python3', str(ROOT / 'scripts/package_release.py'), str(args.binary.resolve()), tag, target, '--out', str(release)], check=True)
        env = dict(os.environ, CODEQ_RELEASE_BASE_URL=root.as_uri())
        destination = root / 'bin'
        command = ['sh', str(ROOT / 'scripts/install.sh'), tag, str(destination)]
        subprocess.run(command, env=env, check=True)
        installed = destination / 'codeq'
        before = hashlib.sha256(installed.read_bytes()).hexdigest()
        subprocess.run(['python3', str(ROOT / 'scripts/release_smoke.py'), str(installed), '--version', version], check=True)
        archive = next(release.glob('*.tar.gz'))
        archive.write_bytes(archive.read_bytes() + b'tampered')
        failure = subprocess.run(command, env=env, capture_output=True, text=True)
        assert failure.returncode != 0 and 'checksum mismatch' in failure.stderr, failure
        assert hashlib.sha256(installed.read_bytes()).hexdigest() == before
        print('Pinned install, doctor, navigation and checksum rejection passed')


if __name__ == '__main__':
    main()
