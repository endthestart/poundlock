#!/usr/bin/env python3
"""Run browser acceptance against disposable local controllers, never production."""
import os
import http.server
import json
import pathlib
import socket
import subprocess
import tempfile
import time
import threading
import urllib.request

root = pathlib.Path(__file__).resolve().parents[1]
binary = pathlib.Path(os.environ.get('POUNDLOCK_TEST_CONTROLLER', root / 'target/debug/poundlock')).resolve()
environment = {**os.environ, 'POUNDLOCK_TEST_CONTROLLER': str(binary)}
class Models(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.end_headers()
        self.wfile.write(json.dumps({'data': [{'id': 'fixture'}]}).encode())

for name in ('browser-selections.cjs', 'browser-node-inventory.cjs', 'browser-linux-setup.cjs', 'browser-models.cjs', 'browser-theme.cjs'):
    subprocess.run(['node', str(root / 'tests/uat' / name)], env=environment, check=True)
for name in ('browser-connections.cjs', 'browser-job.cjs', 'browser-session.cjs'):
    with tempfile.TemporaryDirectory(prefix='poundlock-browser-') as directory:
        model_server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Models)
        threading.Thread(target=model_server.serve_forever, daemon=True).start()
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', 0))
            port = listener.getsockname()[1]
        env = {**environment, 'POUNDLOCK_BIND': f'127.0.0.1:{port}',
               'POUNDLOCK_DATA_DIR': directory + '/data',
               'POUNDLOCK_RUNTIME_DIR': directory + '/runtime',
               'UAT_DATA': directory, 'UAT_BASE': f'http://127.0.0.1:{port}',
               'UAT_MODEL_BASE': f'http://127.0.0.1:{model_server.server_port}/v1'}
        with open(pathlib.Path(directory) / 'controller.log', 'w+') as log:
            process = subprocess.Popen([str(binary), 'controller'], env=env, stdout=log, stderr=log)
            try:
                for attempt in range(100):
                    if process.poll() is not None:
                        raise RuntimeError('Fixture controller exited before becoming ready')
                    try:
                        with urllib.request.urlopen(env['UAT_BASE'] + '/readyz', timeout=1):
                            break
                    except OSError:
                        time.sleep(.1)
                else:
                    raise RuntimeError('Fixture controller did not become ready')
                subprocess.run(['node', str(root / 'tests/uat' / name)], env=env, check=True)
            except BaseException:
                log.seek(0)
                print(log.read()[-8000:])
                raise
            finally:
                model_server.shutdown()
                model_server.server_close()
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
