#!/usr/bin/env python3
"""Exercise installer control flow with mocked systemctl/cargo and temp paths."""
import json
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
    shortcut_prefix = '/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/'
    user_bin = stage / 'user-bin'
    shortcuts = {
        shortcut_prefix + 'custom0/': {'name': 'AutoClicker Toggle', 'binding': 'F6', 'command': f'{user_bin}/autoclick-cli toggle'},
        shortcut_prefix + 'custom1/': {'name': 'ClickLume Quit', 'binding': 'F9', 'command': f'{user_bin}/clicklume-cli quit'},
        shortcut_prefix + 'custom2/': {'name': 'Unrelated shortcut', 'binding': 'F5', 'command': '/usr/bin/other toggle'},
        shortcut_prefix + 'custom3/': {'name': 'Already disabled', 'binding': '', 'command': f'{user_bin}/autoclick-cli inc'},
        shortcut_prefix + 'custom4/': {'name': 'Different command', 'binding': 'F4', 'command': f'{user_bin}/autoclick-cli toggle --custom'},
        shortcut_prefix + 'custom5/': {'name': 'Increase', 'binding': 'F7', 'command': f'{user_bin}/clicklume-cli inc'},
        shortcut_prefix + 'custom6/': {'name': 'Decrease', 'binding': 'F8', 'command': f'{user_bin}/autoclick-cli dec'},
    }
    shortcut_file = stage / 'shortcuts.json'
    shortcut_file.write_text(json.dumps(shortcuts))
    for name, text in {
        'systemctl': '#!/bin/sh\nprintf "%s\\n" "$*" >> "$CLICKLUME_TEST_CALLS"\n',
        'cargo': '#!/bin/sh\nexit 23\n',
        'gsettings': '''#!/usr/bin/python3
import ast, json, os, sys
from pathlib import Path
if os.environ.get('CLICKLUME_TEST_NO_SCHEMA'):
    sys.exit(1)
path = Path(os.environ['CLICKLUME_TEST_SHORTCUTS'])
data = json.loads(path.read_text())
verb, schema, key, *rest = sys.argv[1:]
if key == 'custom-keybindings':
    assert verb == 'get'
    print(repr(list(data)))
else:
    entry = data[schema.split(':', 1)[1]]
    if verb == 'get':
        print(repr(entry[key]))
    else:
        assert verb == 'set' and key == 'binding' and rest == ["''"]
        assert entry['binding'], 'Migration should skip disabled shortcuts'
        entry[key] = ast.literal_eval(rest[0])
        path.write_text(json.dumps(data))
''',
    }.items():
        path = mocks / name
        path.write_text(text)
        path.chmod(0o755)
    env = dict(os.environ, PATH=str(mocks) + ':/usr/bin:/bin',
               XDG_CONFIG_HOME=str(stage / 'config'), XDG_DATA_HOME=str(stage / 'data'),
               CLICKLUME_TEST_BIN_DIR=str(user_bin), CLICKLUME_TEST_CALLS=str(stage / 'calls'),
               CLICKLUME_TEST_SHORTCUTS=str(shortcut_file))
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
    expected = {key: dict(value) for key, value in shortcuts.items()}
    for suffix in ('custom0/', 'custom1/', 'custom5/', 'custom6/'):
        expected[shortcut_prefix + suffix]['binding'] = ''
    assert json.loads(shortcut_file.read_text()) == expected, 'Duplicate GNOME hotkeys were not disabled precisely'
    # Migration is idempotent and tolerates desktops without GNOME schemas.
    for extra in ({}, {'CLICKLUME_TEST_NO_SCHEMA': '1'}):
        result = subprocess.run(command, env=dict(env, **extra), capture_output=True, timeout=5)
        assert result.returncode == 0, result.stderr
        assert json.loads(shortcut_file.read_text()) == expected
    print('PASS: build failure rejects stale binaries; archive install succeeds; existing autostart is preserved')
    print('PASS: duplicate legacy/current CLI hotkeys disabled; unrelated shortcuts preserved; migration idempotent and optional')
