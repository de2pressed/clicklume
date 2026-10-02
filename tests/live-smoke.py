#!/usr/bin/env python3
"""Bounded live Wayland smoke test. Requires the application to be closed.

Does not emit clicks or physical hotkeys. Uses a temporary config and leaves
login-start preferences unchanged. Uses release binaries by default; pass --debug after a debug build.
"""
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
BIN = ROOT / 'target' / ('debug' if '--debug' in sys.argv else 'release')
SOCKET = '/tmp/clicklume.sock'


def connect():
    stream = socket.socket(socket.AF_UNIX)
    stream.settimeout(2)
    stream.connect(SOCKET)
    return stream


def request(command, fragmented=False):
    with connect() as stream:
        payload = json.dumps(command).encode()
        if fragmented:
            stream.sendall(payload[:5])
            time.sleep(0.05)
            stream.sendall(payload[5:])
        else:
            stream.sendall(payload)
        with stream.makefile('rb') as reader:
            return json.loads(reader.readline())['Status']


def wait_for(check, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            result = check()
            if result:
                return result
        except (OSError, KeyError, ValueError):
            pass
        time.sleep(0.1)
    raise AssertionError('Timed out waiting for expected state')


def backend_pid(gui):
    for entry in Path('/proc').iterdir():
        if not entry.name.isdigit():
            continue
        try:
            stat = (entry / 'stat').read_text().split(') ', 1)[1].split()
            if int(stat[1]) == gui.pid and (entry / 'exe').resolve() == BIN / 'clicklume-backend':
                assert stat[0] != 'Z', 'Zombie backend'
                return int(entry.name)
        except (OSError, ValueError):
            pass
    return None


try:
    with connect():
        raise SystemExit('Close ClickLume before running the isolated smoke test.')
except OSError:
    pass

with tempfile.TemporaryDirectory(prefix='clicklume-smoke-') as temporary:
    env = dict(os.environ, XDG_CONFIG_HOME=temporary)
    config = Path(temporary) / 'clicklume'
    config.mkdir()
    (config / 'config.toml').write_text('default_cps = 20\nmax_cps = 80\n')
    with open(Path(temporary) / 'backend.log', 'w') as log:
        backend = subprocess.Popen([BIN / 'clicklume-backend'], env=env, stdout=log, stderr=log)
        try:
            wait_for(lambda: request('GetStatus'))
            cli = subprocess.run([BIN / 'clicklume-cli', 'status'], capture_output=True, timeout=3)
            assert cli.returncode == 0 and not json.loads(cli.stdout)['Status']['enabled']
            assert subprocess.run([BIN / 'clicklume-cli', 'bad-command'], capture_output=True).returncode == 2
            assert request({'SetCps': 42}, True)['cps'] == 42
            assert request({'SetCps': 999})['cps'] == 80
            request({'SetCps': 20})
            assert request('DecreaseCps')['cps'] == 19
            subscriber = connect()
            subscriber.sendall(json.dumps('SubscribeGui').encode())
            reader = subscriber.makefile('rb')
            assert 'HotkeyAvailability' in json.loads(reader.readline())
            # Health probes must not steal the long-lived subscription.
            for _ in range(3):
                with connect():
                    pass
            request('IncreaseCps')
            assert json.loads(reader.readline())['Status']['cps'] == 20
            duplicate = subprocess.run([BIN / 'clicklume-backend'], env=env, capture_output=True, timeout=3)
            assert duplicate.returncode != 0
            assert request('GetStatus')['cps'] == 20
            assert os.stat(SOCKET).st_mode & 0o777 == 0o600
            mice = [path for path in Path('/sys/devices/virtual/input').glob('input*')
                    if (path / 'name').read_text().strip() == 'clicklume-virtual-mouse']
            assert mice and all((path / 'capabilities/rel').read_text().strip() == '3' for path in mice)
            reader.close()
            subscriber.close()
            request('Quit')
            assert backend.wait(timeout=3) == 0
            assert not Path(SOCKET).exists()
        finally:
            if backend.poll() is None:
                backend.terminate()
                backend.wait(timeout=3)
    print('PASS: fragmented IPC, CPS bounds/steps, subscription/probes, duplicate startup, socket permissions, REL axes, clean quit')
    with open(Path(temporary) / 'gui.log', 'w') as log:
        gui = subprocess.Popen([BIN / 'clicklume-gui'], env=env, stdout=log, stderr=log)
        try:
            wait_for(lambda: request('GetStatus'))
            pid = wait_for(lambda: backend_pid(gui))
            os.kill(pid, signal.SIGKILL)
            wait_for(lambda: backend_pid(gui) not in (None, pid))
            wait_for(lambda: request('GetStatus'))
            pid = wait_for(lambda: backend_pid(gui))
            request('Quit')
            wait_for(lambda: backend_pid(gui) not in (None, pid))
            wait_for(lambda: request('GetStatus'))
            assert gui.poll() is None
            gui.send_signal(signal.SIGINT)
            assert gui.wait(timeout=8) == 0
            wait_for(lambda: not Path(SOCKET).exists())
        finally:
            if gui.poll() is None:
                gui.send_signal(signal.SIGINT)
                try:
                    gui.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    gui.kill()
                    gui.wait()
    print('PASS: native GUI launch, backend crash recovery, IPC Quit recovery, graceful GUI shutdown')
