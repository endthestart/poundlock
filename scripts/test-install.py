#!/usr/bin/env python3
"""Rehearse the documented Compose install and restore on a disposable Docker host."""
import argparse
import json
import os
import pathlib
import shutil
import subprocess
import tempfile
import uuid

parser = argparse.ArgumentParser()
parser.add_argument('image')
parser.add_argument('--previous-image', help='Also test upgrading from and rolling back to this image')
args = parser.parse_args()
root = pathlib.Path(__file__).resolve().parents[1]
original = 'poundlock-install-' + uuid.uuid4().hex[:10]
restored = original + '-restored'
environment = {**os.environ, 'POUNDLOCK_IMAGE': args.previous_image or args.image,
               'POUNDLOCK_DOCKER_GID': str(os.stat('/var/run/docker.sock').st_gid),
               'POUNDLOCK_PORT': '0', 'POUNDLOCK_LISTEN_ADDRESS': '127.0.0.1'}
with tempfile.TemporaryDirectory(prefix='poundlock-install-') as directory:
    shutil.copyfile(root / 'compose.yaml', pathlib.Path(directory) / 'compose.yaml')

    def compose(project, *arguments, data=None):
        result = subprocess.run(['docker', 'compose', '-p', project, *arguments],
                                cwd=directory, env=environment, input=data,
                                capture_output=True, text=True, timeout=180)
        if result.returncode:
            raise RuntimeError(f'Compose {arguments[0]} failed: {result.stderr[-4000:]}')
        return result.stdout

    def request(project, path, body=None):
        program = (
            'import json,urllib.request,pathlib\n'
            'token=pathlib.Path("/data/admin-token").read_text().strip()\n'
            f'body={body!r}\n'
            f'request=urllib.request.Request("http://127.0.0.1:7331"+{path!r},'
            'headers={"Authorization":"Bearer "+token,"Content-Type":"application/json"},'
            'data=json.dumps(body).encode() if body is not None else None)\n'
            'with urllib.request.urlopen(request,timeout=10) as response: print(response.read().decode())\n'
        )
        return json.loads(compose(project, 'exec', '-T', 'controller', 'python3', '-', data=program))

    def fingerprint(project):
        return compose(project, 'exec', '-T', 'controller', 'sha256sum', '/data/admin-token', '/data/master-key')

    def check(project, expected, identity):
        assert request(project, '/api/v1/endpoints')[0]['id'] == expected['id']
        assert request(project, '/api/v1/endpoints')[0]['api_key_set'] is True
        assert fingerprint(project) == identity, 'Administrator token or encryption key changed'
        states = compose(project, 'ps', '--format', 'json').strip()
        services = json.loads(states) if states.startswith('[') else [json.loads(line) for line in states.splitlines()]
        assert {service['Service'] for service in services if service['State'] == 'running'} == {'controller', 'runner'}

    try:
        compose(original, 'up', '-d', '--wait', '--wait-timeout', '90')
        saved = request(original, '/api/v1/endpoints', {'name': 'Install rehearsal', 'kind': 'openai',
                        'base_url': 'http://127.0.0.1:9/v1', 'api_key': 'disposable-install-fixture'})
        identity = fingerprint(original)
        check(original, saved, identity)
        compose(original, 'stop')
        compose(original, 'cp', 'controller:/data', './poundlock-data-backup')
        compose(original, 'cp', 'runner:/results', './poundlock-results-backup')
        environment['POUNDLOCK_IMAGE'] = args.image
        compose(original, 'up', '-d', '--wait', '--wait-timeout', '90')
        check(original, saved, identity)
        compose(original, 'restart', 'controller')
        compose(original, 'up', '-d', '--wait', '--wait-timeout', '90')
        check(original, saved, identity)
        compose(original, 'stop')
        environment['POUNDLOCK_IMAGE'] = args.previous_image or args.image
        compose(restored, 'create')
        compose(restored, 'cp', './poundlock-data-backup/.', 'controller:/data/')
        compose(restored, 'cp', './poundlock-results-backup/.', 'runner:/results/')
        for service, target in (('controller', '/data'), ('runner', '/results')):
            compose(restored, 'run', '--rm', '--no-deps', '--user', '0:0', '--cap-add', 'CHOWN',
                    '--entrypoint', 'chown', service, '-R', '10001:10001', target)
        compose(restored, 'up', '-d', '--wait', '--wait-timeout', '90')
        check(restored, saved, identity)
        print('PASS: clean Compose install, saved configuration, restart, stopped backup, fresh-volume restore and credentials retained.'
              + (' Upgrade and rollback images tested.' if args.previous_image else ' Same-version recovery; no cross-version claim.'))
    finally:
        failures = []
        for project in (original, restored):
            try:
                compose(project, 'down', '--volumes', '--remove-orphans')
            except Exception as error:
                failures.append(str(error))
        if failures:
            raise RuntimeError('Fixture cleanup failed: ' + '; '.join(failures))
