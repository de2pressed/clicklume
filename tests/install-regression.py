#!/usr/bin/env python3
"""Exercise installer control flow with mocked systemctl/cargo and temp paths."""
import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
with tempfile.TemporaryDirectory(prefix='clicklume-install-test-') as temporary:
    stage = Path(temporary)
    project = stage / 'project'
    project.mkdir()
    installer = (ROOT / 'install.sh').read_text().replace('BIN_DIR="$HOME/.local/bin"', 'BIN_DIR="${CLICKLUME_TEST_BIN_DIR:?}"')
    (project / 'install.sh').write_text(installer)
    for source in ('clicklume.desktop', 'systemd/clicklume.service'):
        destination = project / source
        destination.parent.mkdir(exist_ok=True)
        destination.write_bytes((ROOT / source).read_bytes())
    mocks = stage / 'mocks'
    mocks.mkdir()
    for name, text in {
        'systemctl': '#!/bin/sh\nprintf "%s\\n" "$*" >> "$CLICKLUME_TEST_CALLS"\n',
        'cargo': '#!/bin/sh\nexit 23\n',
    }.items():
        path = mocks / name
        path.write_text(text)
        path.chmod(0o755)
    env = dict(os.environ, PATH=str(mocks) + ':/usr/bin:/bin',
               XDG_CONFIG_HOME=str(stage / 'config'), XDG_DATA_HOME=str(stage / 'data'),
               CLICKLUME_TEST_BIN_DIR=str(stage / 'user-bin'), CLICKLUME_TEST_CALLS=str(stage / 'calls'))
    # Failed source build must never fall through to installing stale binaries.
    stale = project / 'target/release'
    stale.mkdir(parents=True)
    for binary in ('clicklume-gui', 'clicklume-backend', 'clicklume-cli'):
        path = stale / binary
        path.write_text('#!/bin/sh\nexit 0\n')
        path.chmod(0o755)
    command = ['bash', project / 'install.sh', '--non-interactive', '--skip-system-setup']
    result = subprocess.run(command, env=env, capture_output=True, timeout=5)
    assert result.returncode == 23, result.stderr
    assert not (stage / 'user-bin').exists()
    # A release archive installs without forcing current login-start off.
    stale.rename(project / 'bin')
    result = subprocess.run(command, env=env, capture_output=True, timeout=5)
    assert result.returncode == 0, result.stderr
    assert (stage / 'user-bin/clicklume').is_symlink()
    calls = (stage / 'calls').read_text().splitlines()
    assert '--user disable clicklume.service' not in calls
    assert (stage / 'data/applications/clicklume.desktop').exists()
    print('PASS: build failure rejects stale binaries; archive install succeeds; existing autostart is preserved')
