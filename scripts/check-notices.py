#!/usr/bin/env python3
"""Check that built artifacts carry their license notices, not just the source tree.

    check-notices.py [--image IMAGE] [--archives DIR] [--release DIR]

--image reads /usr/share/doc/poundlock from a built image (docker create/cp, no run);
--archives checks native build folders (DIR/<platform>/); --release checks the
packaged .tar.gz files. Each must carry LICENSE, NOTICE and the full texts for
what it ships: Rust crates and the fonts and palette compiled into the binary, and,
in the image, the Pi runtime's JavaScript packages.
"""
import argparse
import hashlib
import pathlib
import subprocess
import tarfile
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
APACHE_SHA256 = 'cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30'
EMBEDDED = ('## Inter font', '## JetBrains Mono font', '## Catppuccin palette',
            'SIL OPEN FONT LICENSE', 'Copyright (c) 2021 Catppuccin')
problems = []


def need(label, condition, message):
    if not condition:
        problems.append(f'{label}: {message}')


def check_binary_notices(label, files):
    """files: name -> bytes for one native archive or folder."""
    for name in ('poundlock', 'LICENSE', 'NOTICE', 'THIRD_PARTY_LICENSES.md'):
        need(label, name in files, f'missing {name}')
    if 'LICENSE' in files:
        need(label, hashlib.sha256(files['LICENSE']).hexdigest() == APACHE_SHA256,
             'LICENSE is not the canonical Apache-2.0 text')
    text = files.get('THIRD_PARTY_LICENSES.md', b'').decode()
    need(label, '# Third-party Rust licenses' in text, 'no Rust crate licenses')
    for marker in EMBEDDED:
        need(label, marker in text, f'THIRD_PARTY_LICENSES.md lacks {marker!r}')


def check_image(image):
    container = subprocess.run(['docker', 'create', image], check=True, capture_output=True,
                               text=True).stdout.strip()
    try:
        with tempfile.TemporaryDirectory() as directory:
            subprocess.run(['docker', 'cp', f'{container}:/usr/share/doc/poundlock/.', directory],
                           check=True, capture_output=True)
            docs = pathlib.Path(directory)
            label = f'{image}:/usr/share/doc/poundlock'
            for name in ('LICENSE', 'NOTICE', 'THIRD_PARTY_NOTICES.md',
                         'THIRD_PARTY_LICENSES.md', 'RUNTIME_LICENSES.md'):
                need(label, (docs / name).is_file(), f'missing {name}')
            if (docs / 'LICENSE').is_file():
                need(label, hashlib.sha256((docs / 'LICENSE').read_bytes()).hexdigest() == APACHE_SHA256,
                     'LICENSE is not the canonical Apache-2.0 text')
            third = (docs / 'THIRD_PARTY_LICENSES.md').read_text() if (docs / 'THIRD_PARTY_LICENSES.md').is_file() else ''
            for marker in EMBEDDED:
                need(label, marker in third, f'THIRD_PARTY_LICENSES.md lacks {marker!r}')
            runtime = (docs / 'RUNTIME_LICENSES.md').read_text() if (docs / 'RUNTIME_LICENSES.md').is_file() else ''
            need(label, '## @earendil-works/pi-coding-agent 1.1.0' in runtime, 'no Pi 1.1.0 entry')
            pi = (ROOT / 'licenses/pi-1.1.0-LICENSE').read_text()
            need(label, pi in runtime, "Pi's MIT text is not reproduced")
    finally:
        subprocess.run(['docker', 'rm', container], check=True, capture_output=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--image')
    parser.add_argument('--archives', type=pathlib.Path)
    parser.add_argument('--release', type=pathlib.Path)
    args = parser.parse_args()
    if args.image:
        check_image(args.image)
    if args.archives:
        folders = sorted(p for p in args.archives.iterdir() if p.is_dir())
        need(str(args.archives), folders, 'no platform folders')
        for folder in folders:
            check_binary_notices(str(folder), {f.name: f.read_bytes() for f in folder.iterdir() if f.is_file()})
    if args.release:
        archives = sorted(args.release.glob('poundlock-*.tar.gz'))
        for archive in archives:
            with tarfile.open(archive) as bundle:
                files = {m.name: bundle.extractfile(m).read() for m in bundle if m.isfile()}
            if archive.name == 'poundlock-compose.tar.gz':
                for name in ('LICENSE', 'NOTICE', 'THIRD_PARTY_NOTICES.md'):
                    need(archive.name, name in files, f'missing {name}')
            else:
                check_binary_notices(archive.name, files)
    for problem in problems:
        print(problem)
    if problems:
        raise SystemExit(1)
    print('PASS: notices present in every checked artifact')


if __name__ == '__main__':
    main()
