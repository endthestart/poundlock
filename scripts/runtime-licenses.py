#!/usr/bin/env python3
"""Collect notices from the installed Pi tree; fail on missing license texts.

The argument is the install prefix (the folder holding package-lock.json), whose
node_modules holds Pi and every dependency npm ci placed there.
"""
import hashlib
import json
import pathlib
import sys

root = pathlib.Path(__file__).resolve().parents[1]
source = pathlib.Path(sys.argv[1]).resolve()
destination = pathlib.Path(sys.argv[2])
overrides = {(row['package'], row['version']): row
             for row in json.loads((root / 'licenses/runtime-sources.json').read_text())}
blocks = ['# Third-party JavaScript runtime licenses', '',
          'Generated from the installed Pi package tree. Optional desktop integrations are omitted.', '']
seen = set()


def collect(package):
    package = package.resolve()
    if package in seen:
        return
    if not package.is_relative_to(source):
        raise SystemExit('Dependency resolves outside the installed package tree')
    seen.add(package)
    if package != source:
        notice(package)
    modules = package / 'node_modules'
    if modules.exists():
        for dependency in sorted(modules.iterdir()):
            if dependency.name.startswith('.'):
                continue
            for child in sorted(dependency.iterdir()) if dependency.name.startswith('@') else [dependency]:
                if (child / 'package.json').is_file():
                    collect(child)


def notice(package):
    info = json.loads((package / 'package.json').read_text())
    files = sorted(file for file in package.iterdir() if file.is_file() and
                   file.name.lower().startswith(('license', 'licence', 'copying', 'notice', 'unlicense')))
    override = overrides.get((info['name'], info['version']))
    if not files and override:
        file = root / 'licenses' / override['file']
        if hashlib.sha256(file.read_bytes()).hexdigest() != override['sha256']:
            raise SystemExit('License hash mismatch: ' + override['file'])
        files = [file]
    if not files:
        raise SystemExit(f"Missing license text: {info['name']} {info['version']}")
    blocks.extend([f"## {info['name']} {info['version']}", '', f"License: {info.get('license')}", ''])
    if override:
        blocks.extend(['Upstream: ' + override['upstream'], ''])
    for file in files:
        blocks.extend([f'### {file.name}', '', file.read_text(), ''])


collect(source)
for vendor in json.loads((root / 'licenses/pi-vendored-sources.json').read_text()):
    file = root / 'licenses' / vendor['file']
    if hashlib.sha256(file.read_bytes()).hexdigest() != vendor['sha256']:
        raise SystemExit('Bundled license hash mismatch: ' + vendor['file'])
    blocks.extend([f"## Bundled {vendor['package']} {vendor['version']}", '',
                   'Upstream: ' + vendor['upstream'], '', file.read_text(), ''])
destination.parent.mkdir(parents=True, exist_ok=True)
destination.write_text('\n'.join(blocks))
print(f'Collected notices for {len(seen) - 1} installed packages and bundled HTML libraries.')
