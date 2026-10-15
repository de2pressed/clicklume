#!/usr/bin/env python3
"""Check repeat behavior using this test backend's virtual mouse exclusively.

Close ClickLume first. EVIOCGRAB is applied ONLY to the newly created test
virtual mouse, never to a physical input device. Test clicks cannot reach the
desktop. No clicking starts unless exclusive capture succeeds, and capture is
retained until the backend exits. Requires existing input permissions.
"""
import fcntl
import json
import os
from pathlib import Path
import select
import socket
import struct
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
SOCKET = '/tmp/clicklume.sock'
EVENT = struct.Struct('@llHHi')
EVIOCGRAB = 0x40044590


def request(command):
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(2)
        stream.connect(SOCKET)
        stream.sendall(json.dumps(command).encode())
        with stream.makefile('rb') as reader:
            return json.loads(reader.readline())['Status']


def virtual_mice():
    return {p for p in Path('/sys/devices/virtual/input').glob('input*')
            if (p / 'name').read_text().strip() == 'clicklume-virtual-mouse'}


def capture(fd, duration):
    events = []
    deadline = time.monotonic() + duration
    while (remaining := deadline - time.monotonic()) > 0:
        if select.select([fd], [], [], remaining)[0]:
            data = os.read(fd, EVENT.size * 256)
            for _, _, event_type, code, value in EVENT.iter_unpack(data):
                if event_type == 1 and code == 272:  # EV_KEY, BTN_LEFT
                    events.append(value)
    return events


try:
    request('GetStatus')
except OSError:
    pass
else:
    raise SystemExit('Close ClickLume before running this isolated stream test.')

before = virtual_mice()
with tempfile.TemporaryDirectory(prefix='clicklume-stream-') as temporary:
    with (Path(temporary) / 'backend.log').open('w') as log:
        backend = subprocess.Popen([ROOT / 'target/release/clicklume-backend'],
                                   env=dict(os.environ, XDG_CONFIG_HOME=temporary),
                                   stdout=log, stderr=log)
        fd = None
        try:
            for _ in range(100):
                assert backend.poll() is None, 'Test backend exited during startup'
                try:
                    request('GetStatus')
                    break
                except OSError:
                    time.sleep(.05)
            else:
                raise AssertionError('Backend did not start')
            created = virtual_mice() - before
            assert len(created) == 1, created
            nodes = list(created.pop().glob('event*'))
            assert len(nodes) == 1, nodes
            fd = os.open('/dev/input/' + nodes[0].name, os.O_RDONLY | os.O_NONBLOCK)
            fcntl.ioctl(fd, EVIOCGRAB, 1)

            request({'SetCps': 5})
            request({'SetRepeatCount': 1})
            request('Start')
            assert capture(fd, .6) == [1, 0], 'Finite count 1 did not click exactly once'
            assert not request('GetStatus')['enabled']

            request({'SetRepeatCount': 0})
            for _ in range(2):
                assert request('Start')['enabled']
                values = capture(fd, 1.3)
                status = request('GetStatus')
                assert status['enabled'] and status['repeat_count'] == 0, status
                assert values.count(1) >= 5, values
                assert not request('Stop')['enabled']
                values.extend(capture(fd, .1))
                assert values[-1] == 0, 'Stop left the button held'
                assert values.count(1) == values.count(0), values
                assert 1 not in capture(fd, .2), 'New press after Stop settled'
            print('PASS: finite one-click, unlimited repeat, restart, and Stop/release; no desktop clicks')
        finally:
            try:
                request('Quit')
                backend.wait(timeout=3)
            except (OSError, ValueError, KeyError, subprocess.TimeoutExpired):
                backend.kill()
                backend.wait()
            if fd is not None:
                os.close(fd)
