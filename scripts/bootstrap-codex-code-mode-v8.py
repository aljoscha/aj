#!/usr/bin/env python3
"""Install the pinned, SHA-256-verified V8 pair for native Cargo builds."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
NAMES = {'archive': 'archive.a.gz', 'binding': 'src_binding.rs'}


def verify(path, item):
    with path.open('rb') as stream:
        actual = hashlib.file_digest(stream, 'sha256').hexdigest()
    if actual != item['sha256']:
        raise RuntimeError(f'V8 checksum mismatch: {path}')


def bootstrap(root, target, source=None, check=False):
    artifacts = json.loads((root / 'vendor/codex-code-mode/v8-artifacts.json').read_text())
    pair = [item for item in artifacts if item['target'] == target]
    if len(pair) != 2 or {item['kind'] for item in pair} != NAMES.keys():
        raise RuntimeError(f'No pinned sandbox-enabled V8 pair for {target}')
    dest = root / 'third-party/codex-code-mode/native'
    if dest.is_symlink():
        raise RuntimeError(f'Refusing symlink native directory: {dest}')
    if dest.exists() or check:
        for item in pair:
            verify(dest / NAMES[item['kind']], item)
        return

    # Publish the pair together. Failed downloads never leave a usable partial pair.
    # An existing directory is only verified, never repaired or overwritten.
    with tempfile.TemporaryDirectory(prefix='.v8-', dir=dest.parent) as tmp:
        staged = Path(tmp) / 'native'
        staged.mkdir()
        for item in pair:
            path = staged / NAMES[item['kind']]
            if source is not None:
                shutil.copyfile(source / item['filename'], path)
            else:
                subprocess.run(['curl', '--fail', '--location', '--retry', '2',
                                '--proto', '=https', '--proto-redir', '=https',
                                item['url'], '--output', str(path)], check=True)
            verify(path, item)
        staged.rename(dest)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--from-dir', type=Path, help='offline source containing upstream artifact filenames')
    parser.add_argument('--check', action='store_true', help='verify the installed native pair without downloading')
    args = parser.parse_args()
    version = subprocess.check_output(['rustc', '-vV'], text=True)
    target = next(line.removeprefix('host: ') for line in version.splitlines() if line.startswith('host: '))
    try:
        bootstrap(ROOT, target, args.from_dir, args.check)
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        parser.exit(1, f'{error}\n')
    print(f'Verified sandbox-enabled V8 for {target}. Ordinary cargo commands are ready.')


if __name__ == '__main__':
    main()
