#!/usr/bin/env python3
"""Package verified binaries and a pull-only Compose install with an immutable image."""
import argparse
import hashlib
import pathlib
import re
import shutil
import tarfile
import tempfile

parser = argparse.ArgumentParser()
parser.add_argument('--image')
parser.add_argument('--version', required=True)
parser.add_argument('--output', default='dist/release')
parser.add_argument('--platform', action='append', choices=('linux-x86_64', 'linux-aarch64', 'macos-x86_64', 'macos-aarch64'), help='Limit a rehearsal to explicitly selected platforms; releases require all four by default')
args = parser.parse_args()
root = pathlib.Path(__file__).resolve().parents[1]
if not re.fullmatch(r'v?[0-9]+\.[0-9]+\.[0-9]+(?:-[a-zA-Z0-9.-]+)?', args.version):
    parser.error('version must be a semantic version')
def cargo_version():
    text = (root / 'Cargo.toml').read_text()
    try:
        import tomllib  # Python 3.11+
    except ImportError:  # CI's job image ships Python 3.10
        package = re.search(r'^\[package\]\n(.*?)(?=^\[|\Z)', text, re.M | re.S)
        return re.search(r'^version\s*=\s*"([^"]+)"', package.group(1), re.M).group(1)
    return tomllib.loads(text)['package']['version']


if args.version.removeprefix('v') != cargo_version():
    parser.error('version must match Cargo.toml')
if args.image is None:
    raise SystemExit(0)
if not re.fullmatch(r'[a-z0-9][a-z0-9./:_-]*@sha256:[a-f0-9]{64}', args.image):
    parser.error('image must be a lowercase registry reference pinned by SHA-256')
output = pathlib.Path(args.output).resolve()
output.mkdir(parents=True, exist_ok=True)
if any(output.iterdir()):
    parser.error('output directory must be empty')
platforms = args.platform or ('linux-x86_64', 'linux-aarch64', 'macos-x86_64', 'macos-aarch64')
for platform in dict.fromkeys(platforms):
    source = root / 'node-artifacts' / platform
    for required in ('poundlock', 'LICENSE', 'NOTICE', 'THIRD_PARTY_LICENSES.md'):
        if not (source / required).is_file():
            raise SystemExit(f'Missing {platform}/{required}')
    with tarfile.open(output / f'poundlock-{platform}.tar.gz', 'w:gz') as archive:
        for name in ('poundlock', 'LICENSE', 'NOTICE', 'THIRD_PARTY_LICENSES.md'):
            archive.add(source / name, arcname=name)
with tempfile.TemporaryDirectory(prefix='poundlock-release-') as temporary:
    stage = pathlib.Path(temporary)
    for name in ('compose.yaml', 'compose.wake.yaml', 'compose.repositories.yaml', 'README.md', 'LICENSE', 'NOTICE', 'SECURITY.md', 'CONTRIBUTING.md', 'THIRD_PARTY_NOTICES.md'):
        shutil.copyfile(root / name, stage / name)
    shutil.copytree(root / 'docs' / 'guide', stage / 'docs' / 'guide')
    maps = stage / 'docs' / 'maps'
    maps.mkdir(parents=True)
    for name in ('poundlock-release.architecture.html', 'poundlock-release.architecture.json'):
        shutil.copyfile(root / 'docs' / 'maps' / name, maps / name)
    shutil.copytree(root / 'deploy' / 'macos', stage / 'deploy' / 'macos')
    shutil.copytree(root / 'config' / 'examples', stage / 'config' / 'examples')
    shutil.copytree(root / 'licenses', stage / 'licenses')
    environment = (root / '.env.example').read_text().replace('POUNDLOCK_IMAGE=\n', f'POUNDLOCK_IMAGE={args.image}\n')
    (stage / '.env').write_text(environment)
    (stage / 'VERSION').write_text(args.version + '\n')
    with tarfile.open(output / 'poundlock-compose.tar.gz', 'w:gz') as archive:
        for child in sorted(stage.iterdir()):
            archive.add(child, arcname=child.name)
files = sorted(output.glob('*.tar.gz'))
(output / 'SHA256SUMS').write_text(''.join(f'{hashlib.sha256(file.read_bytes()).hexdigest()}  {file.name}\n' for file in files))
print(f'Packaged {len(files)} archives and SHA256SUMS in {output}')
