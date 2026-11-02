#!/usr/bin/env python3
"""Collect shipped crate license texts from locked Cargo metadata; fail on omissions."""
import json
import pathlib
import sys

metadata = json.loads(pathlib.Path(sys.argv[1]).read_text())
destination = pathlib.Path(sys.argv[2])
root = pathlib.Path(__file__).resolve().parents[1]
overrides = json.loads((root / 'licenses/sources.json').read_text())
overrides = {(row['package'], row['version']): row['files'] for row in overrides}
blocks = ['# Third-party Rust licenses', '', 'Generated from Cargo.lock. Each dependency retains its own license.', '']
for package in sorted(metadata['packages'], key=lambda item: (item['name'], item['version'])):
    if not package.get('source'):
        continue
    source = pathlib.Path(package['manifest_path']).parent
    files = sorted(file for file in source.iterdir() if file.is_file() and
                   file.name.lower().startswith(('license', 'licence', 'copying', 'notice', 'unlicense')))
    if not files:
        files = [root / 'licenses' / row['file'] for row in overrides.get((package['name'], package['version']), [])]
    if not files:
        raise SystemExit(f"Missing license text: {package['name']} {package['version']}")
    blocks += [f"## {package['name']} {package['version']}", '', f"License: {package.get('license')}", '']
    for file in files:
        blocks += [f'### {file.name}', '', file.read_text(), '']
# Compiled into the binary with include_bytes!/include_str! (src/server/mod.rs).
EMBEDDED = (
    ('Inter font', 'OFL-1.1', 'web/fonts/INTER-LICENSE'),
    ('JetBrains Mono font', 'OFL-1.1', 'web/fonts/JETBRAINS-MONO-LICENSE'),
    ('Catppuccin palette (web console colours)', 'MIT', 'docs/design/CATPPUCCIN-LICENSE'),
)
blocks += ['# Assets embedded in the binary', '',
           "The web console's fonts and its Catppuccin-derived colour palette are compiled into this binary.", '']
for name, license_id, path in EMBEDDED:
    blocks += [f'## {name}', '', f'License: {license_id}', '', (root / path).read_text(), '']
destination.parent.mkdir(parents=True, exist_ok=True)
destination.write_text('\n'.join(blocks))
