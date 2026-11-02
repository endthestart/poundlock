#!/usr/bin/env python3
"""Check the GitHub workflows' authority split without executing GitHub.

CI (ci.yml, e2e.yml) runs untrusted pull requests: read-only token, no secrets,
nothing published. release.yml reruns them on a tag and publishes only after
approval in the `release` environment, from the artifacts CI tested.
"""
import re
from pathlib import Path

root = Path(__file__).resolve().parents[1] / '.github/workflows'
problems = []


def code(text):
    """The workflow without comments, so prose cannot satisfy or trip a check."""
    lines = []
    for line in text.splitlines():
        if line.lstrip().startswith('#'):
            continue
        lines.append(re.sub(r'\s+#\s.*$', '', line))
    return '\n'.join(lines) + '\n'


def jobs(text):
    _, _, body = text.partition('\njobs:\n')
    found, current, buffer = {}, None, []
    for line in body.splitlines(True):
        if re.match(r'^  [A-Za-z0-9_-]+:\s*$', line):
            if current:
                found[current] = ''.join(buffer)
            current, buffer = line.strip()[:-1], [line]
        elif current:
            buffer.append(line)
    if current:
        found[current] = ''.join(buffer)
    return found


for path in sorted(root.glob('*.yml')):
    text = code(path.read_text())
    name = path.name
    header = text.partition('\njobs:\n')[0]
    if 'permissions:\n  contents: read' not in header:
        problems.append(f'{name}: default permissions must be contents: read')
    if 'pull_request_target' in text or 'workflow_run' in text:
        problems.append(f'{name}: privileged triggers run untrusted code with secrets')
    for ref in re.findall(r'uses:\s*([^\s#]+)', text):
        if ref.startswith('./'):
            continue
        if not re.fullmatch(r'[\w.-]+/[\w./-]+@[0-9a-f]{40}', ref):
            problems.append(f'{name}: action {ref} is not pinned to a full commit SHA')
    for job, body in jobs(text).items():
        if 'uses: ./.github/workflows/' in body:
            continue
        if 'timeout-minutes:' not in body:
            problems.append(f'{name}: job {job} has no timeout-minutes')
    if 'persist-credentials: false' not in text and 'actions/checkout' in text:
        problems.append(f'{name}: checkout must not persist credentials')

for name in ('ci.yml', 'e2e.yml'):
    text = code((root / name).read_text())
    for forbidden in ('secrets.', 'packages: write', 'id-token: write', 'contents: write',
                      'docker push', 'gh release', 'docker/login-action', 'environment:'):
        if forbidden in text:
            problems.append(f'{name}: untrusted validation must not use {forbidden!r}')
    if 'pull_request:' not in text:
        problems.append(f'{name}: pull requests must be validated')

release = code((root / 'release.yml').read_text())
release_jobs = jobs(release)
if "tags: [\"v*\"]" not in release or 'pull_request' in release.partition('\njobs:\n')[0]:
    problems.append('release.yml: must run for version tags only')
if 'uses: ./.github/workflows/ci.yml' not in release_jobs.get('checks', ''):
    problems.append('release.yml: checks must run ci.yml on the tagged commit')
publish = release_jobs.get('publish', '')
if 'needs: [checks, end-to-end]' not in publish:
    problems.append('release.yml: publish must wait for ci.yml and the end-to-end suite')
if 'environment:\n      name: release' not in publish:
    problems.append('release.yml: publish must wait for approval in the release environment')
if 'docker load' not in publish or 'poundlock:candidate' not in publish:
    problems.append('release.yml: publish must load the tested candidate image')
for forbidden in ('docker build', 'build-push-action', 'test-browser.py', 'test-image.sh', 'cargo '):
    if forbidden in publish:
        problems.append(f'release.yml: publish must not rebuild ({forbidden!r})')
if 'gh release create' not in publish or '--draft' not in publish:
    problems.append('release.yml: publish must keep a draft GitHub release')
version_check = publish.find('package-release.py --version')
push = publish.find('docker push')
if version_check < 0 or push < 0 or version_check > push:
    problems.append('release.yml: the tag must be checked before docker push')
if 'SIGNING_KEY' in release or 'UPDATE_PUBLIC_KEY' in release:
    problems.append('release.yml: public releases are keyless (docs/node-update-signing.md)')

for problem in problems:
    print(problem)
if problems:
    raise SystemExit(1)
print('PASS: workflow authority split, pinning, timeouts and approval gate')
