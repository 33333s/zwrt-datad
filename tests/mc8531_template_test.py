#!/usr/bin/env python3
"""MC8531 (G5 Ultra) gets its own template, equivalent to MC8532B: a formal
template, no battery block, and an unknown sibling model stays on the fallback."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

binary = Path(sys.argv[1]).resolve()
here = Path(__file__).parent.resolve()
with tempfile.TemporaryDirectory(prefix='datad-mc8531-') as name:
    root = Path(name)
    def snapshot(model, **env):
        full = dict(os.environ, ZWRT_DATAD_UBUS_BIN=str(here / 'mock_ubus.sh'),
                    ZWRT_DATAD_UCI_BIN=str(here / 'mock_uci.sh'), ZWRT_DATAD_OTA_DISABLE_AUTO='1',
                    ZWRT_DATAD_COOLING_CONFIG=str(root / 'cooling'), ZWRT_DATAD_PROC_ROOT=str(root / 'proc'),
                    MOCK_MODEL_NAME=model, **env)
        done = subprocess.run([str(binary), '--once', '--data-dir', str(root / 'data')], env=full,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60)
        assert done.returncode == 0, done.stderr.decode()[-500:]
        return json.loads(done.stdout)
    state = snapshot('MC8531', MOCK_BATTERY_ABSENT='1')
    device = state['device']
    assert device['api_template'] == 'MC8531' and device['api_template_supported'] == 1, device
    assert device['api_template_label'] == 'MC8531', device
    assert 'battery' not in state, state.get('battery')
    other = snapshot('MC8539')['device']
    assert other['api_template'] == 'legacy_compat' and other['api_template_supported'] == 0, other
    print('MC8531 template: supported, no battery, unknown sibling stays legacy PASS')
