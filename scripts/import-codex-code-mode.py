#!/usr/bin/env python3
"""Reproduce a pinned Code Mode extraction without overwriting local changes."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]
SUPPORT = ROOT / 'third-party/codex-code-mode'
REVISION = '44984d20817fc58026a8bbccf053e288c6897c8f'


def git(repo, *args):
    return subprocess.check_output(['git', '-C', str(repo), *args])


def inventory(root):
    files = {}
    def visit(directory):
        entries = sorted(directory.iterdir())
        if directory != root and not entries:
            raise RuntimeError(f'Refusing unrecorded empty directory: {directory}')
        for p in entries:
            if p.is_symlink():
                raise RuntimeError(f'Refusing symlink in vendor tree: {p}')
            if p.parent == root and p.name in ('target', 'Cargo.lock'):
                if not (p.is_dir() if p.name == 'target' else p.is_file()):
                    raise RuntimeError(f'Refusing unexpected Cargo state type: {p}')
                continue  # Cargo-owned, never replaced by this importer.
            if p.is_dir():
                visit(p)
            elif p.is_file():
                files[str(p.relative_to(root))] = hashlib.sha256(p.read_bytes()).hexdigest()
            else:
                raise RuntimeError(f'Refusing special file: {p}')
    visit(root)
    return files


def section(source, start, end):
    # Unique semantic boundaries make upstream refactors fail rather than truncate silently.
    if source.count(start) != 1 or source.count(end) != 1:
        raise RuntimeError(f'Upstream extraction boundary changed: {start!r}')
    begin = source.index(start) + len(start)
    return source[begin:source.index(end, begin)]


def materialize(repo, revision, out):
    sources = {}
    def source(path):
        entry = git(repo, 'ls-tree', revision, '--', path).decode()
        if entry.split(' ', 1)[0] not in ('100644', '100755'):
            raise RuntimeError(f'Refusing upstream non-regular file: {path}')
        data = git(repo, 'show', f'{revision}:{path}')
        sources[path] = hashlib.sha256(data).hexdigest()
        return data
    upstream_workspace = tomllib.loads(source('codex-rs/Cargo.toml').decode())['workspace']
    template = SUPPORT / 'workspace.toml'
    local_workspace = tomllib.loads(template.read_text())['workspace']
    for name, value in local_workspace['dependencies'].items():
        if isinstance(value, dict) and 'path' in value:
            continue
        if upstream_workspace['dependencies'].get(name) != value:
            raise RuntimeError(f'Workspace dependency changed upstream: {name}. Review Cargo.toml and Cargo.lock.')
    if upstream_workspace['package']['edition'] != local_workspace['package']['edition']:
        raise RuntimeError('Upstream Rust edition changed. Review Cargo.toml.')
    if upstream_workspace['package']['license'] != local_workspace['package']['license']:
        raise RuntimeError('Upstream license changed. Review licensing before importing.')
    for name in ('code-mode-runtime', 'code-mode-protocol'):
        prefix = f'codex-rs/{name}/'
        entries = git(repo, 'ls-tree', '-rz', revision, '--', prefix).decode().split('\0')
        paths = []
        for entry in filter(None, entries):
            info, path = entry.split('\t', 1)
            if info.split()[0] not in ('100644', '100755'):
                raise RuntimeError(f'Refusing upstream non-regular file: {path}')
            paths.append(path)
        if not paths:
            raise RuntimeError(f'Missing upstream directory {prefix}')
        for path in paths:
            relative = path.removeprefix('codex-rs/')
            dest = out / relative
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.write_bytes(source(path))
    for name in ('LICENSE', 'NOTICE'):
        (out / name).write_bytes(source(name))
    (out / 'Cargo.toml').write_bytes(template.read_bytes())
    (out / '.gitignore').write_text('/target/\n')
    (out / 'models.json').write_bytes(source('codex-rs/models-manager/models.json'))
    (out / 'clippy.toml').write_bytes(source('codex-rs/clippy.toml'))
    bazel = source('MODULE.bazel').decode()
    artifacts = []
    for block in re.findall(r'http_file\(\n(.*?)\n\)', bazel, re.S):
        filename = re.search(r'downloaded_file_path = "([^"]+)"', block)
        if not filename:
            continue
        target = re.fullmatch(r'(librusty_v8|src_binding)_ptrcomp_sandbox_release_'
                              r'((?:x86_64|aarch64)-(?:unknown-linux-gnu|apple-darwin))\.(a\.gz|rs)', filename[1])
        if not target:
            continue
        checksum = re.search(r'sha256 = "([0-9a-f]{64})"', block)
        urls = re.findall(r'"(https://[^"]+)"', block)
        if not checksum or len(urls) != 1:
            raise RuntimeError('V8 artifact declaration changed upstream')
        artifacts.append({'filename': filename[1], 'sha256': checksum[1], 'url': urls[0],
                          'target': target[2], 'kind': 'binding' if target[1] == 'src_binding' else 'archive'})
    expected = {(f'{arch}-{system}', kind) for arch in ('x86_64', 'aarch64')
                for system in ('unknown-linux-gnu', 'apple-darwin') for kind in ('archive', 'binding')}
    if len(artifacts) != 8 or {(a['target'], a['kind']) for a in artifacts} != expected:
        raise RuntimeError('Expected four upstream V8 archive and binding pairs')
    (out / 'v8-artifacts.json').write_text(json.dumps(artifacts, indent=2) + '\n')
    shared = out / 'code-mode-protocol/src'
    (shared / 'tool_name.rs').write_bytes(source('codex-rs/protocol/src/tool_name.rs'))
    models = source('codex-rs/protocol/src/openai_models.rs').decode()
    message = section(models, '/// Model-owned messages for a built-in tool.\n',
                      '/// Model-owned descriptions and parameters for Multi-Agent V2 tools, independent of their namespace.\n')
    mode = section(models, "/// Model-owned instructions for Code Mode's exec and wait tools.\n",
                   '/// Model-owned defaults for the context-window token-budget feature.\n')
    (shared / 'model_messages.rs').write_text(
        'use serde::{Deserialize, Serialize};\n\n'
        '/// Model-owned messages for a built-in tool.\n' + message +
        "/// Model-owned instructions for Code Mode's exec and wait tools.\n" + mode)
    media = source('codex-rs/protocol/src/local_media.rs').decode()
    audio = section(media, '/// Maximum accepted decoded byte length for prompt audio inputs.\n',
                    '/// Snapshots local image and audio input into portable, validated data URLs.\n')
    (shared / 'audio_limits.rs').write_text('/// Maximum accepted decoded byte length for prompt audio inputs.\n' + audio)
    # Git-format patches inside an enclosing worktree can be silently skipped
    # as outside the current prefix. Apply against this standalone staging tree.
    patch_env = {**os.environ, 'GIT_CEILING_DIRECTORIES': str(out.parent)}
    for key in ('GIT_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE', 'GIT_COMMON_DIR'):
        patch_env.pop(key, None)
    for patch in sorted((SUPPORT / 'patches').glob('*.patch')):
        subprocess.run(['git', 'apply', '--check', str(patch)], cwd=out, env=patch_env, check=True)
        subprocess.run(['git', 'apply', str(patch)], cwd=out, env=patch_env, check=True)
    return inventory(out), sources


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--upstream', required=True, type=Path, help='local Git checkout, read via git show')
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument('--check', action='store_true')
    mode.add_argument('--upgrade', metavar='FULL_COMMIT', help='explicitly replace a verified import')
    args = parser.parse_args()
    record = SUPPORT / 'upstream.json'
    dest = ROOT / 'vendor/codex-code-mode'
    for path in (dest, record, SUPPORT / 'workspace.toml', SUPPORT / 'patches',
                 *sorted((SUPPORT / 'patches').glob('*.patch'))):
        if any(p.is_symlink() for p in (path, *path.parents)):
            parser.error(f'refusing symlink path: {path}')
    previous = json.loads(record.read_text()) if record.exists() else None
    revision = args.upgrade or (previous['revision'] if previous else REVISION)
    if not re.fullmatch(r'[0-9a-f]{40}', revision):
        parser.error('revision must be a full lowercase 40-character commit SHA')
    resolved = git(args.upstream, 'rev-parse', f'{revision}^{{commit}}').decode().strip()
    if revision != resolved:
        parser.error('upstream does not resolve to the pinned commit')
    if args.upgrade and (not dest.exists() or previous is None):
        parser.error('upgrade requires an existing recorded import')
    if dest.exists() and (previous is None or inventory(dest) != previous['files']):
        parser.error('vendored files have local changes or no provenance record, refusing to overwrite')
    dest.parent.mkdir(parents=True, exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix='.codemode-import-', dir=dest.parent))
    published = False
    try:
        staged = staging / 'new'
        staged.mkdir()
        files, sources = materialize(args.upstream, resolved, staged)
        metadata = {'repository': 'https://github.com/openai/codex', 'revision': resolved,
                    'files': files, 'sources': sources,
                    'inputs': {str(p.relative_to(SUPPORT)): hashlib.sha256(p.read_bytes()).hexdigest()
                               for p in [SUPPORT / 'workspace.toml', *sorted((SUPPORT / 'patches').glob('*.patch'))]}}
        if args.check or (dest.exists() and not args.upgrade):
            if not record.exists() or not dest.exists() or json.loads(record.read_text()) != metadata or inventory(dest) != files:
                parser.error('import differs from the recorded vendor tree')
            print(f'Reproducible import verified: {resolved}, {len(files)} files')
            return
        publish(staged, dest, record, metadata, staging)
        published = True
        print(f'Imported {resolved}: {len(files)} files')
    finally:
        if published or not (staging / 'old').exists():
            shutil.rmtree(staging)
        else:
            print(f'Rollback incomplete. Preserve and recover the old tree at {staging / "old"}')


def publish(staged, dest, record, metadata, staging):
    # Single writer only: no Cargo, editor, or second importer during publication.
    # Renames preserve Cargo state without copying potentially large build trees.
    # This rolls back Python-visible failures, not process death or power loss.
    pending = record.with_name('.upstream.json.pending')
    backup = staging / 'old'
    moved = []
    installed = False
    owns_pending = False
    try:
        with pending.open('x') as stream:
            owns_pending = True
            stream.write(json.dumps(metadata, indent=2, sort_keys=True) + '\n')
        if dest.exists():
            dest.rename(backup)
            for name in ('Cargo.lock', 'target'):
                if (backup / name).exists():
                    (backup / name).rename(staged / name)
                    moved.append(name)
        staged.rename(dest)
        installed = True
        pending.replace(record)
    except BaseException:
        if installed:
            dest.rename(staged)
        for name in moved:
            (staged / name).rename(backup / name)
        if backup.exists():
            backup.rename(dest)
        raise
    finally:
        # Never remove another invocation's pending file.
        if owns_pending and pending.exists():
            pending.unlink()


if __name__ == '__main__':
    main()
