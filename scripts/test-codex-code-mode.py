#!/usr/bin/env python3
"""Test extraction fidelity and native bootstrap without building AJ (Python 3.11+)."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
UPSTREAM = None
sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location('bootstrap', ROOT / 'scripts/bootstrap-codex-code-mode-v8.py')
bootstrap = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bootstrap)


def tree(root):
    return {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in root.rglob('*') if p.is_file()}


class ImportContract(unittest.TestCase):
    def setUp(self):
        if UPSTREAM is None:
            self.skipTest('pass --upstream to exercise the pinned Git import')
        guard = tempfile.TemporaryDirectory(prefix='codex-code-mode-test-')
        self.addCleanup(guard.cleanup)
        self.root = Path(guard.name)
        (self.root / 'scripts').mkdir()
        shutil.copy2(ROOT / 'scripts/import-codex-code-mode.py', self.root / 'scripts')
        self.support = self.root / 'third-party/codex-code-mode'
        self.support.mkdir(parents=True)
        shutil.copytree(ROOT / 'third-party/codex-code-mode/patches', self.support / 'patches')
        shutil.copy2(ROOT / 'third-party/codex-code-mode/workspace.toml', self.support)
        self.vendor = self.root / 'vendor/codex-code-mode'
        self.upstream = UPSTREAM
        self.run_import()

    def run_import(self, *args, succeeds=True):
        result = subprocess.run([sys.executable, str(self.root / 'scripts/import-codex-code-mode.py'),
                                 '--upstream', str(self.upstream), *args], capture_output=True, text=True)
        self.assertEqual(result.returncode == 0, succeeds, result.stdout + result.stderr)
        return result

    def test_reimport_is_reproducible_and_leaves_cargo_state_alone(self):
        (self.vendor / 'Cargo.lock').write_text('local cargo lock\n')
        (self.vendor / 'target').mkdir()
        (self.vendor / 'target/keep').write_text('build artifact\n')
        before = tree(self.root)
        self.run_import()
        self.run_import('--check')
        self.assertEqual(tree(self.root), before)

    def test_local_work_is_never_overwritten(self):
        target = self.vendor / 'code-mode-runtime/src/lib.rs'
        original = target.read_bytes()
        for change in ('edit', 'delete', 'extra'):
            with self.subTest(change=change):
                target.write_bytes(original)
                if change == 'edit':
                    target.write_bytes(original + b'\n// local work\n')
                elif change == 'delete':
                    target.unlink()
                else:
                    (self.vendor / 'local-work').write_text('keep me')
                before = tree(self.root)
                result = self.run_import(succeeds=False)
                self.assertIn('local changes', result.stderr)
                revision = json.loads((self.support / 'upstream.json').read_text())['revision']
                self.run_import('--upgrade', revision, succeeds=False)
                self.assertEqual(tree(self.root), before)

    def test_patch_failure_leaves_import_intact(self):
        (self.support / 'patches/9999-incompatible.patch').write_text(
            '--- a/code-mode-runtime/src/lib.rs\n+++ b/code-mode-runtime/src/lib.rs\n'
            '@@ -1 +1 @@\n-this line is not present upstream\n+replacement\n')
        before = tree(self.root)
        revision = json.loads((self.support / 'upstream.json').read_text())['revision']
        self.run_import('--upgrade', revision, succeeds=False)
        self.assertEqual(tree(self.root), before)

    def test_upgrade_and_repeat_use_recorded_full_revision(self):
        repo = self.root / 'upstream'
        subprocess.run(['git', 'clone', '--quiet', '--shared', '--no-checkout', str(UPSTREAM), str(repo)], check=True)
        self.upstream = repo
        revision = json.loads((self.support / 'upstream.json').read_text())['revision']
        env = {**os.environ, 'GIT_AUTHOR_NAME': 'Test', 'GIT_AUTHOR_EMAIL': 'test@example.com',
               'GIT_COMMITTER_NAME': 'Test', 'GIT_COMMITTER_EMAIL': 'test@example.com'}
        def git(*args, data=None):
            return subprocess.check_output(['git', '-C', str(repo), *args], input=data, env=env).decode().strip()
        git('read-tree', revision)
        blob = git('hash-object', '-w', '--stdin', data=b'upgrade fixture\n')
        git('update-index', '--add', '--cacheinfo', f'100644,{blob},codex-rs/code-mode-runtime/upgrade-fixture.txt')
        upgraded = git('commit-tree', git('write-tree'), '-p', revision, data=b'upgrade fixture\n')
        (self.vendor / 'Cargo.lock').write_text('keep lock')
        (self.vendor / 'target').mkdir()
        (self.vendor / 'target/keep').write_text('keep build')
        self.run_import('--upgrade', upgraded)
        self.assertEqual((self.vendor / 'code-mode-runtime/upgrade-fixture.txt').read_text(), 'upgrade fixture\n')
        self.assertEqual(json.loads((self.support / 'upstream.json').read_text())['revision'], upgraded)
        self.assertEqual((self.vendor / 'Cargo.lock').read_text(), 'keep lock')
        self.assertEqual((self.vendor / 'target/keep').read_text(), 'keep build')
        before = tree(self.vendor)
        self.run_import('--check')
        self.run_import()
        self.run_import('--upgrade', upgraded)
        self.assertEqual(tree(self.vendor), before)

    def test_moving_refs_and_symlinks_are_rejected(self):
        before = tree(self.root)
        for ref in ('HEAD', 'main', '44984d2'):
            self.run_import('--upgrade', ref, succeeds=False)
        self.assertEqual(tree(self.root), before)
        revision = json.loads((self.support / 'upstream.json').read_text())['revision']
        for name in ('Cargo.lock', 'target', 'local-link'):
            link = self.vendor / name
            link.symlink_to(self.support / 'workspace.toml')
            self.run_import('--upgrade', revision, succeeds=False)
            self.assertTrue(link.is_symlink())
            link.unlink()
        self.assertEqual(tree(self.root), before)

    def test_patch_changes_require_explicit_upgrade(self):
        (self.support / 'patches/9999-fixture.patch').write_text(
            '--- /dev/null\n+++ b/upgrade-fixture.txt\n@@ -0,0 +1 @@\n+fixture\n')
        before = tree(self.root)
        self.run_import(succeeds=False)
        self.run_import('--check', succeeds=False)
        self.assertEqual(tree(self.root), before)
        revision = json.loads((self.support / 'upstream.json').read_text())['revision']
        self.run_import('--upgrade', revision)
        self.assertEqual((self.vendor / 'upgrade-fixture.txt').read_text(), 'fixture\n')
        self.run_import('--check')

    def test_git_format_patches_apply_inside_a_parent_checkout(self):
        subprocess.run(['git', 'init', '--quiet', str(self.root)], check=True)
        (self.support / 'patches/9999-git-fixture.patch').write_text(
            'diff --git a/patch-fixture.txt b/patch-fixture.txt\n'
            'new file mode 100644\n--- /dev/null\n+++ b/patch-fixture.txt\n'
            '@@ -0,0 +1 @@\n+applied\n')
        revision = json.loads((self.support / 'upstream.json').read_text())['revision']
        self.run_import('--upgrade', revision)
        self.assertEqual((self.vendor / 'patch-fixture.txt').read_text(), 'applied\n')
        self.assertIn('fn store', (self.vendor / 'code-mode-protocol/src/session.rs').read_text())
        self.run_import('--check')

    def test_missing_provenance_and_pending_work_are_preserved(self):
        record = self.support / 'upstream.json'
        original = record.read_bytes()
        revision = json.loads(original)['revision']
        record.unlink()
        before = tree(self.root)
        self.run_import('--upgrade', revision, succeeds=False)
        self.assertEqual(tree(self.root), before)
        record.write_bytes(original)
        (self.support / '.upstream.json.pending').write_text('unrecovered work')
        before = tree(self.root)
        self.run_import('--upgrade', revision, succeeds=False)
        self.assertEqual(tree(self.root), before)

    def test_publication_failure_restores_tree_record_and_cargo_state(self):
        spec = importlib.util.spec_from_file_location('importer', self.root / 'scripts/import-codex-code-mode.py')
        importer = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(importer)
        (self.vendor / 'Cargo.lock').write_text('keep lock')
        (self.vendor / 'target').mkdir()
        (self.vendor / 'target/keep').write_text('keep build')
        before = tree(self.root)
        with tempfile.TemporaryDirectory(dir=self.root) as tmp:
            staging = Path(tmp)
            staged = staging / 'new'
            staged.mkdir()
            (staged / 'new-file').write_text('new import')
            with patch.object(Path, 'replace', side_effect=OSError('publication failed')):
                with self.assertRaisesRegex(OSError, 'publication failed'):
                    importer.publish(staged, self.vendor, self.support / 'upstream.json', {}, staging)
        self.assertEqual(tree(self.root), before)

    def test_catalog_is_the_full_upstream_file(self):
        revision = json.loads((self.support / 'upstream.json').read_text())['revision']
        upstream = subprocess.check_output(['git', '-C', str(UPSTREAM), 'show',
                                            f'{revision}:codex-rs/models-manager/models.json'])
        self.assertEqual((self.vendor / 'models.json').read_bytes(), upstream)
        models = json.loads(upstream)['models']
        self.assertTrue(any(m.get('tool_mode') == 'code_mode_only' for m in models))
        self.assertTrue(any(m.get('tool_mode') != 'code_mode_only' for m in models))

    def test_parent_can_depend_on_both_nested_workspace_crates(self):
        (self.root / 'Cargo.toml').write_text(
            '[workspace]\nmembers = ["app"]\nexclude = ["vendor/codex-code-mode"]\nresolver = "3"\n')
        app = self.root / 'app'
        (app / 'src').mkdir(parents=True)
        (app / 'src/lib.rs').write_text('')
        (app / 'Cargo.toml').write_text(
            '[package]\nname = "parent-probe"\nversion = "0.1.0"\nedition = "2024"\n'
            '[dependencies]\n'
            'codex-code-mode-protocol = { path = "../vendor/codex-code-mode/code-mode-protocol" }\n'
            'codex-code-mode-runtime = { path = "../vendor/codex-code-mode/code-mode-runtime" }\n')
        result = subprocess.run(['cargo', 'metadata', '--offline', '--format-version', '1'],
                                cwd=self.root, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        metadata = json.loads(result.stdout)
        imported = [p for p in metadata['packages'] if p['name'].startswith('codex-code-mode-')]
        self.assertEqual(len(imported), 2)
        for package in imported:
            self.assertEqual(package['license'], 'Apache-2.0')
            self.assertEqual(package['edition'], '2024')
            self.assertEqual(package['version'], '0.0.0')
            self.assertNotIn(package['id'], metadata['workspace_members'])
        self.assertFalse(any(p['name'] in ('tonic', 'prost', 'protoc-bin-vendored')
                             for p in metadata['packages']))


class LicenseEvidence(unittest.TestCase):
    def test_collected_bytes_and_artifact_scope_match_provenance(self):
        root = ROOT / 'third-party/codex-code-mode/licenses'
        manifest = json.loads((root / 'provenance.json').read_text())
        actual = tree(root)
        del actual['provenance.json']
        self.assertEqual(actual, {name: item['sha256'] for name, item in manifest['files'].items()})
        self.assertEqual(manifest['native_artifacts'],
                         json.loads((ROOT / 'vendor/codex-code-mode/v8-artifacts.json').read_text()))
        if UPSTREAM:
            for name, digest in manifest['codex_evidence'].items():
                data = subprocess.check_output(['git', '-C', str(UPSTREAM), 'show',
                                                f'{manifest["codex_revision"]}:{name}'])
                self.assertEqual(hashlib.sha256(data).hexdigest(), digest, name)


class BootstrapContract(unittest.TestCase):
    def setUp(self):
        guard = tempfile.TemporaryDirectory(prefix='codex-v8-test-')
        self.addCleanup(guard.cleanup)
        self.root = Path(guard.name)
        self.source = self.root / 'downloads'
        self.source.mkdir()
        (self.root / 'third-party/codex-code-mode').mkdir(parents=True)
        self.native = self.root / 'third-party/codex-code-mode/native'
        vendor = self.root / 'vendor/codex-code-mode'
        vendor.mkdir(parents=True)
        # Distinct bytes ensure selecting another platform or swapping a pair fails.
        self.items = json.loads((ROOT / 'vendor/codex-code-mode/v8-artifacts.json').read_text())
        for item in self.items:
            data = item['filename'].encode()
            (self.source / item['filename']).write_bytes(data)
            item['sha256'] = hashlib.sha256(data).hexdigest()
        (vendor / 'v8-artifacts.json').write_text(json.dumps(self.items))

    def test_each_native_platform_gets_its_matching_pair(self):
        targets = {item['target'] for item in self.items}
        self.assertEqual(targets, {f'{arch}-{system}' for arch in ('x86_64', 'aarch64')
                                  for system in ('unknown-linux-gnu', 'apple-darwin')})
        for target in sorted(targets):
            with self.subTest(target=target):
                bootstrap.bootstrap(self.root, target, self.source)
                for item in self.items:
                    if item['target'] == target:
                        self.assertEqual((self.native / bootstrap.NAMES[item['kind']]).read_bytes(),
                                         item['filename'].encode())
                before = tree(self.native)
                bootstrap.bootstrap(self.root, target, check=True)
                bootstrap.bootstrap(self.root, target, self.source)
                self.assertEqual(tree(self.native), before)
                shutil.rmtree(self.native)

    def test_corrupt_download_does_not_publish_partial_pair(self):
        item = self.items[1]
        (self.source / item['filename']).write_bytes(b'corrupt download')
        with self.assertRaisesRegex(RuntimeError, 'checksum mismatch'):
            bootstrap.bootstrap(self.root, item['target'], self.source)
        self.assertFalse(self.native.exists())

    def test_corrupt_installed_pair_is_rejected_not_overwritten(self):
        target = self.items[0]['target']
        bootstrap.bootstrap(self.root, target, self.source)
        (self.native / 'src_binding.rs').write_bytes(b'local change')
        before = tree(self.native)
        with self.assertRaisesRegex(RuntimeError, 'checksum mismatch'):
            bootstrap.bootstrap(self.root, target, self.source)
        self.assertEqual(tree(self.native), before)

    def test_unsupported_platform_has_no_fallback(self):
        with self.assertRaisesRegex(RuntimeError, 'No pinned sandbox-enabled V8 pair'):
            bootstrap.bootstrap(self.root, 'x86_64-unknown-linux-musl', self.source)
        self.assertFalse(self.native.exists())


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--upstream', type=Path)
    args, remaining = parser.parse_known_args()
    UPSTREAM = args.upstream
    unittest.main(argv=[sys.argv[0], *remaining])
