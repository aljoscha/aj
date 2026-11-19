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


def matches(path, item):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest() == item['sha256']


def verify(path, item):
    if not matches(path, item):
        raise RuntimeError(f'V8 checksum mismatch: {path}')


def bootstrap(root, target, source=None, check=False):
    """Install the pinned pair for `target` and return the names of replaced stale files."""
    artifacts = json.loads((root / 'vendor/codex-code-mode/v8-artifacts.json').read_text())
    pair = [item for item in artifacts if item['target'] == target]
    if len(pair) != 2 or {item['kind'] for item in pair} != NAMES.keys():
        raise RuntimeError(f'No pinned sandbox-enabled V8 pair for {target}')
    dest = root / 'third-party/codex-code-mode/native'
    if dest.is_symlink():
        raise RuntimeError(f'Refusing symlink native directory: {dest}')
    missing = []
    stale = []
    for item in pair:
        path = dest / NAMES[item['kind']]
        if check:
            verify(path, item)
        elif not (path.exists() or path.is_symlink()):
            missing.append(item)
        elif not matches(path, item):
            # The pair is a pinned download, so anything else here is a
            # leftover from an earlier pin rather than work to preserve.
            missing.append(item)
            stale.append(NAMES[item['kind']])
    if not missing:
        return []

    # Verify every download before publishing, so a failed download leaves the
    # installation unchanged. Files that already match the pin stay in place.
    # Bootstrap requires a single writer.
    with tempfile.TemporaryDirectory(prefix='.v8-', dir=dest.parent) as tmp:
        staged = Path(tmp) / 'native'
        staged.mkdir()
        for item in missing:
            path = staged / NAMES[item['kind']]
            if source is not None:
                shutil.copyfile(source / item['filename'], path)
            else:
                subprocess.run(['curl', '--fail', '--location', '--retry', '2',
                                '--proto', '=https', '--proto-redir', '=https',
                                item['url'], '--output', str(path)], check=True)
            verify(path, item)
        if dest.exists():
            for item in missing:
                name = NAMES[item['kind']]
                (staged / name).rename(dest / name)
        else:
            staged.rename(dest)
    return stale


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--from-dir', type=Path, help='offline source containing upstream artifact filenames')
    parser.add_argument('--check', action='store_true', help='verify the installed native pair without downloading')
    args = parser.parse_args()
    version = subprocess.check_output(['rustc', '-vV'], text=True)
    target = next(line.removeprefix('host: ') for line in version.splitlines() if line.startswith('host: '))
    try:
        stale = bootstrap(ROOT, target, args.from_dir, args.check)
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        parser.exit(1, f'{error}\n')
    if stale:
        # Rusty V8's build script copies the archive into the target directory
        # and does not rerun when the archive changes.
        print(f'Replaced stale {", ".join(stale)}. Run `cargo clean -p v8` for each '
              'profile and target directory you build with so Cargo picks up the new pair.')
    print(f'Verified sandbox-enabled V8 for {target}. Ordinary cargo commands are ready.')


if __name__ == '__main__':
    main()
