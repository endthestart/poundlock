#!/usr/bin/env python3
"""Actual systemd installer fixture. Run ONLY in a disposable Linux VM.
Usage: python3 linux-node.py FIXTURE_DIRECTORY [--reconnect]
The directory contains poundlock-linux-aarch64 (or x86_64) and poundlock-node.sh.
First pass leaves the enrolled service installed for a VM reboot. --reconnect
checks automatic startup and removes it. Uses an isolated loopback controller.
"""
import hashlib
import json
import os
import pathlib
import platform
import pty
import select
import signal
import subprocess
import sys
import time
import urllib.request

assert os.environ.get('POUNDLOCK_LINUX_NODE_UAT') == '1', 'Explicit disposable-VM opt-in required'
fixture=pathlib.Path(sys.argv[1]).resolve()
reconnect='--reconnect' in sys.argv
root=pathlib.Path.home()/'.local/state/poundlock-node-acceptance'
binary=fixture / ('poundlock-linux-'+platform.machine())
installer=fixture / 'poundlock-node.sh'
base='http://127.0.0.1:17431'
if '--retry' in sys.argv:
    assert 'linux-installer-fixture' in pathlib.Path('/etc/poundlock/node.toml').read_text()
elif not reconnect:
    assert not pathlib.Path('/etc/poundlock').exists(), 'Refusing to touch an existing installation'
root.mkdir(mode=0o700,parents=True,exist_ok=True)
artifacts=root/'artifacts';artifacts.mkdir(exist_ok=True)
platform_dir=artifacts/('linux-'+platform.machine());platform_dir.mkdir(exist_ok=True)
(platform_dir/'poundlock').write_bytes(binary.read_bytes())
env={**os.environ,'POUNDLOCK_BIND':'127.0.0.1:17431','POUNDLOCK_DATA_DIR':str(root/'controller'),'POUNDLOCK_RUNTIME_DIR':str(root/'runtime'),'POUNDLOCK_CLIENT_ARTIFACTS':str(artifacts)}
log=open(root/'controller.log','a')
controller=subprocess.Popen([str(binary),'controller'],env=env,stdout=log,stderr=log)

def wait_for(predicate,timeout=25):
    end=time.monotonic()+timeout
    while time.monotonic()<end:
        try:
            result=predicate()
            if result:return result
        except (OSError,urllib.error.URLError):pass
        time.sleep(.3)
    raise AssertionError('Condition timed out')

def run(args,success=True):
    result=subprocess.run(args,capture_output=True,text=True,timeout=40)
    assert (result.returncode==0)==success,(args,result.stdout,result.stderr)
    return result.stdout

def install_with_token(args,token):
    pid,fd=pty.fork()
    if pid==0:os.execvp(args[0],args)
    output=b'';sent=False;end=time.monotonic()+45
    try:
        while time.monotonic()<end:
            if select.select([fd],[],[],.3)[0]:
                try:data=os.read(fd,65536)
                except OSError:break
                if not data:break
                output+=data
                if b'input hidden' in output and not sent:
                    os.write(fd,(token+'\n').encode());sent=True
            finished,status=os.waitpid(pid,os.WNOHANG)
            if finished:
                assert os.waitstatus_to_exitcode(status)==0,output.decode().replace(token,'[redacted]')
                break
        else:
            os.killpg(pid,signal.SIGKILL)
            raise AssertionError('Installer timed out: '+output.decode().replace(token,'[redacted]'))
        try:
            _,status=os.waitpid(pid,0)
            assert os.waitstatus_to_exitcode(status)==0,output.decode().replace(token,'[redacted]')
        except ChildProcessError:pass
        assert token.encode() not in output,'Enrollment token was echoed'
    finally:os.close(fd)

try:
    token_path=root/'controller/admin-token'
    wait_for(token_path.exists)
    admin=token_path.read_text().strip()
    def request(path,data=None):
        req=urllib.request.Request(base+path,headers={'Authorization':'Bearer '+admin,'Content-Type':'application/json'},data=None if data is None else json.dumps(data).encode())
        with urllib.request.urlopen(req,timeout=10) as response:return json.load(response)
    wait_for(lambda:request('/api/v1/nodes') is not None)
    def node_status():
        return next((n for n in request('/api/v1/nodes') if n['name']=='linux-installer-fixture'),None)
    command=['sudo','bash',str(installer)]
    if reconnect:
        assert run(['systemctl','is-active','poundlock-node.service']).strip()=='active'
        wait_for(lambda:node_status()['status']=='online')
        run(command+['doctor'])
        run(command+['remove'])
        assert node_status()['status']=='revoked'
        for path in ['/etc/poundlock','/var/lib/poundlock','/etc/systemd/system/poundlock-node.service','/etc/polkit-1/rules.d/60-poundlock-node.rules']:
            run(['sudo','test','!','-e',path])
        print('PASS: service reconnected after VM reboot; diagnostics, revocation and removal passed.')
    else:
        config=root/'node.toml'
        example=request('/api/v1/nodes/config-example')['toml']
        config.write_text('controller = '+json.dumps(base)+'\nname = "linux-installer-fixture"\n'+example)
        # Named, as the Machines page makes it: a re-run must re-enroll the same name (L8).
        token=request('/api/v1/nodes/enrollment-tokens',{'valid_minutes':15,'node_name':'linux-installer-fixture'})['token']
        digest=hashlib.sha256(binary.read_bytes()).hexdigest()
        args=command+['install','--binary',str(binary),'--sha256',digest,'--config',str(config)]
        install_with_token(args,token)
        wait_for(lambda:node_status()['status']=='online')
        original_id=node_status()['id']
        inventory=node_status()['capabilities']['inventory']
        assert inventory['schema_version']==1 and inventory['os']['name']
        assert inventory['memory']['total_bytes']>0
        assert node_status()['capabilities']['network']['interfaces']
        # The node runs as its own account, which owns its state (L11).
        unit=run(['systemctl','cat','poundlock-node.service'])
        assert 'ProtectHome=true' in unit and 'RestrictNamespaces=true' in unit, unit
        assert run(['stat','-c','%U','/proc/'+run(['systemctl','show','-p','MainPID','--value','poundlock-node.service']).strip()]).strip()=='poundlock-node'
        credential_path='/var/lib/poundlock/node-credential.json'
        assert run(['sudo','stat','-c','%a %U',credential_path]).strip()=='600 poundlock-node'
        def credential_bytes():return run(['sudo','cat',credential_path])
        credential_before=credential_bytes()
        run(['sudo','systemctl','restart','poundlock-node.service'])
        wait_for(lambda:node_status()['status']=='online')
        run(command+['doctor'])
        run(command+['upgrade','--binary',str(binary),'--sha256','0'*64],success=False)
        assert credential_bytes()==credential_before
        run(command+['upgrade','--binary',str(binary),'--sha256',digest])
        run(['sudo','test','-f','/var/lib/poundlock/poundlock.previous'])
        run(command+['rollback'])
        # Simulate an executable that cannot start. Recovery must use the previous one.
        run(['sudo','systemctl','stop','poundlock-node.service'])
        run(['sudo','-u','poundlock-node','sh','-c','printf "invalid executable\\n" > /var/lib/poundlock/broken && chmod 755 /var/lib/poundlock/broken && mv -f /var/lib/poundlock/broken /var/lib/poundlock/poundlock'])
        run(command+['rollback'])
        wait_for(lambda:node_status()['status']=='online')
        assert node_status()['id']==original_id and credential_bytes()==credential_before
        # Enrollment is resumable without consuming another token.
        run(args)
        assert credential_bytes()==credential_before
        # Exercise actual OS inhibition without invoking runtime commands.
        report=json.loads(run(['sudo','-u','poundlock-node','/var/lib/poundlock/poundlock','node-doctor']))
        assert report['sleep_inhibitor']=='available'
        # Move to the login and back (L11), keeping identity and credential.
        login=os.environ.get('SUDO_USER') or os.environ['USER']
        run(command+['account','--user','no-such-account-x'],success=False)
        assert run(['systemctl','is-active','poundlock-node.service']).strip()=='active'
        run(command+['account','--user',login])
        wait_for(lambda:node_status()['status']=='online')
        assert run(['sudo','stat','-c','%U',credential_path]).strip()==login
        run(command+['account'])
        wait_for(lambda:node_status()['status']=='online')
        assert run(['sudo','stat','-c','%U',credential_path]).strip()=='poundlock-node'
        assert node_status()['id']==original_id and credential_bytes()==credential_before
        print('PASS: hidden-token install as the poundlock-node account with systemd hardening, permissions, service restart, diagnostics, bad-checksum rejection, upgrade, rollback with a broken executable, repeat-install recovery, and account moves both ways. Service left installed for reboot acceptance.')
finally:
    controller.terminate()
    try:controller.wait(timeout=10)
    except subprocess.TimeoutExpired:controller.kill();controller.wait()
    log.close()
