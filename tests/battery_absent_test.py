#!/usr/bin/env python3
"""A CPE without a battery answers `zwrt_bsp.battery list` with a placeholder
(battery_online 0, capacity 0). That must not become a 0 % `battery` block,
while a real battery that reads 0 % (online 1) is kept."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

binary = Path(sys.argv[1]).resolve()
here = Path(__file__).parent.resolve()
with tempfile.TemporaryDirectory(prefix='datad-battery-') as name:
    root = Path(name)
    def snapshot(**env):
        full = dict(os.environ, ZWRT_DATAD_UBUS_BIN=str(here / 'mock_ubus.sh'),
                    ZWRT_DATAD_UCI_BIN=str(here / 'mock_uci.sh'), ZWRT_DATAD_OTA_DISABLE_AUTO='1',
                    ZWRT_DATAD_COOLING_CONFIG=str(root / 'cooling'), ZWRT_DATAD_PROC_ROOT=str(root / 'proc'), **env)
        done = subprocess.run([str(binary), '--once', '--data-dir', str(root / 'data')], env=full,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60)
        assert done.returncode == 0, done.stderr.decode()[-500:]
        return json.loads(done.stdout)
    present = snapshot()
    assert present['battery']['online'] == 1 and present['battery']['percent'] == 0, present.get('battery')
    absent = snapshot(MOCK_BATTERY_ABSENT='1')
    assert 'battery' not in absent, absent.get('battery')
    unavailable = snapshot(MOCK_NO_BATTERY='1')
    assert 'battery' not in unavailable
    print('battery: placeholder omitted, real 0 % kept, unavailable omitted PASS')
