#!/usr/bin/env python3
"""Bounded resource/overload check; never enables clicking or sends hotkeys.

Close ClickLume first. Uses temporary preferences and leaves installation and
autostart untouched. --measure-only can measure an older installed build.
"""
import argparse
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time
import tomllib

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--bin-dir', type=Path, default=Path(__file__).resolve().parents[1] / 'target/release')
parser.add_argument('--seconds', type=int, default=60)
parser.add_argument('--measure-only', action='store_true')
parser.add_argument('--gui', action='store_true')
args = parser.parse_args()
SOCKET = '/tmp/clicklume.sock'


def connect():
    stream = socket.socket(socket.AF_UNIX)
    stream.settimeout(2)
    try:
        stream.connect(SOCKET)
    except BaseException:
        stream.close()
        raise
    return stream


def request(command):
    with connect() as stream:
        stream.sendall(json.dumps(command).encode())
        with stream.makefile('rb') as reader:
            return json.loads(reader.readline())['Status']


def wait_for(check, timeout=12):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            result = check()
            if result:
                return result
        except (OSError, ValueError, KeyError):
            pass
        time.sleep(0.05)
    raise AssertionError('Timed out waiting for expected state')


def sample(pid):
    root = Path('/proc') / str(pid)
    stat = (root / 'stat').read_text().split(') ', 1)[1].split()
    switches = 0
    for task in (root / 'task').iterdir():
        try:
            switches += sum(int(line.split(':')[1]) for line in (task / 'status').read_text().splitlines()
                            if line.startswith('voluntary_ctxt_switches:'))
        except FileNotFoundError:
            pass
    return dict(cpu_ticks=int(stat[11]) + int(stat[12]),
                rss_kib=int(stat[21]) * os.sysconf('SC_PAGE_SIZE') // 1024,
                threads=len(list((root / 'task').iterdir())),
                fds=len(list((root / 'fd').iterdir())), switches=switches)


def idle_measure(process):
    start = sample(process.pid)
    began = time.monotonic()
    samples = [start]
    while time.monotonic() - began < args.seconds:
        time.sleep(1)
        assert process.poll() is None, 'Application exited during soak'
        samples.append(sample(process.pid))
    elapsed = time.monotonic() - began
    end = samples[-1]
    return dict(seconds=round(elapsed, 2),
                cpu_percent_one_core=round((end['cpu_ticks'] - start['cpu_ticks']) / os.sysconf('SC_CLK_TCK') / elapsed * 100, 3),
                voluntary_switches_per_second=round((end['switches'] - start['switches']) / elapsed, 2),
                rss_kib_start=start['rss_kib'], rss_kib_end=end['rss_kib'],
                rss_kib_peak=max(s['rss_kib'] for s in samples),
                threads_start=start['threads'], threads_end=end['threads'],
                fds_start=start['fds'], fds_end=end['fds'])


try:
    with connect():
        raise SystemExit('Close ClickLume before running the isolated resource test.')
except OSError:
    pass

with tempfile.TemporaryDirectory(prefix='clicklume-resource-') as temporary:
    env = dict(os.environ, XDG_CONFIG_HOME=temporary)
    log_path = Path(temporary) / 'app.log'
    executable = args.bin_dir / ('clicklume-gui' if args.gui else 'clicklume-backend')
    with log_path.open('w') as log:
        process = subprocess.Popen([executable], env=env, stdout=log, stderr=log)
        try:
            wait_for(lambda: request('GetStatus'))
            time.sleep(2)
            result = dict(binary=str(executable), idle=idle_measure(process))
            if not args.measure_only:
                baseline = sample(process.pid)
                if not args.gui:
                    # Hold far more connections than worker/queue capacity.
                    slow_clients = []
                    try:
                        for _ in range(96):
                            try:
                                client = connect()
                                slow_clients.append(client)
                                client.sendall(b'{"SetCps":')
                            except OSError:
                                pass
                        time.sleep(0.2)
                        overloaded = sample(process.pid)
                        assert overloaded['threads'] <= baseline['threads'], (baseline, overloaded)
                        assert overloaded['fds'] <= baseline['fds'] + 22, (baseline, overloaded)
                        result['overload_peak'] = overloaded
                    finally:
                        for client in slow_clients:
                            client.close()
                    wait_for(lambda: request('GetStatus'))

                for _ in range(3000):
                    assert not request('GetStatus')['enabled']

                if not args.gui:
                    # A non-reading subscriber must time out and release its
                    # worker, allowing a replacement subscription afterward.
                    with connect() as stalled:
                        stalled.sendall(json.dumps('SubscribeGui').encode())
                        time.sleep(0.1)
                        for value in range(2000):
                            request({'SetCps': 1 + value % 1000})
                        with connect() as replacement:
                            replacement.sendall(json.dumps('SubscribeGui').encode())
                            with replacement.makefile('rb') as reader:
                                assert 'HotkeyAvailability' in json.loads(reader.readline())
                    # Quit must also clean up a still-connected subscriber.
                    subscriber = connect()
                    subscriber.sendall(json.dumps('SubscribeGui').encode())
                    time.sleep(0.1)
                else:
                    for value in range(2000):
                        request({'SetCps': 1 + value % 1000})
                    state_file = Path(temporary) / 'clicklume/state.toml'
                    wait_for(lambda: tomllib.loads(state_file.read_text()).get('cps') == 1000)
                    time.sleep(1)
                    saved_mtime = state_file.stat().st_mtime_ns
                    for _ in range(100):
                        assert not request('Stop')['enabled']
                    time.sleep(1)
                    assert state_file.stat().st_mtime_ns == saved_mtime, 'Unchanged status rewrote preferences'

                time.sleep(2)
                after = sample(process.pid)
                assert after['fds'] <= baseline['fds'] + (2 if not args.gui else 0), (baseline, after)
                assert after['threads'] <= baseline['threads'], (baseline, after)
                assert after['rss_kib'] <= baseline['rss_kib'] + 8192, (baseline, after)
                result['after_stress'] = after

            if args.gui:
                process.send_signal(signal.SIGINT)
            else:
                request('Quit')
            assert process.wait(timeout=8) == 0
            wait_for(lambda: not Path(SOCKET).exists())
            if not args.gui and not args.measure_only:
                subscriber.close()
            result['log_bytes'] = log_path.stat().st_size
            print(json.dumps(result, indent=2))
        finally:
            if process.poll() is None:
                if args.gui:
                    process.send_signal(signal.SIGINT)
                else:
                    try:
                        request('Quit')
                    except (OSError, ValueError, KeyError):
                        process.terminate()
                try:
                    process.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
