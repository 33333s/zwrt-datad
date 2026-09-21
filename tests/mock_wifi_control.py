#!/usr/bin/env python3
"""Stateful, test-only UCI/ubus fixture for Wi-Fi control readback."""
import json
import os
from pathlib import Path
import sys

path = Path(os.environ['WIFI_FIXTURE'])
data = json.loads(path.read_text())
args = sys.argv[1:]


def save():
    staging = path.with_suffix('.write')
    staging.write_text(json.dumps(data))
    staging.replace(path)


def emit(value):
    print(json.dumps(value))
    sys.exit(0)


def log():
    with open(os.environ['WIFI_WRITES'], 'a') as output:
        output.write(json.dumps(args) + '\n')


if args[:2] == ['-q', 'get']:
    if args[2] not in data['config']:
        sys.exit(1)
    print(data['config'][args[2]])
    sys.exit(0)
if args[:1] == ['set']:
    log()
    key, value = args[1].split('=', 1)
    data['config'][key] = value
    save()
    sys.exit(0)
if args[:1] in (['commit'], ['revert']):
    log()
    sys.exit(0)
if args[:2] == ['call', 'zwrt_wlan']:
    method = args[2]
    params = json.loads(args[3])
    if method == 'wlan_uci_get_section':
        if data.get('read_fail'):
            sys.exit(1)
        if data.get('malformed'):
            print('not JSON')
            sys.exit(0)
        assert params == {'section': 'zte_mbb'}
        emit({} if data.get('missing') else {'lbd': data['lbd']})
    if method in ('set', 'reload'):
        log()
        if data.get('reject'):
            emit({'result': 'failure', 'key': 'must-not-leak'})
        if method == 'set':
            assert set(params) == {'zte_mbb'}
            assert set(params['zte_mbb']) == {'lbd'}
            assert params['zte_mbb']['lbd'] in ('0', '1')
            if not data.get('ignore'):
                data['lbd'] = params['zte_mbb']['lbd']
                save()
        elif data.get('restore_on_reload'):
            data['config']['wireless.main_2g.ssid'] = 'Fixture 2G'
            data['config']['wireless.main_2g.key'] = 'fixture-password'
            save()
        if data.get('empty_reply'):
            sys.exit(0)
        if data.get('bad_reply'):
            print('not JSON')
            sys.exit(0)
        emit({'result': data.get('result', 'success')})
if args[:2] == ['call', 'zwrt_router.api'] and args[2] == 'router_get_wifi_isolate':
    # Deliberately different from lbd: never use isolation as band steering.
    emit({'wifimain24_wifimain5_enable': 1})
if args[:2] == ['call', 'zwrt_router.api'] and args[2] == 'router_set_wifi_isolate':
    log()
    sys.exit(1)
if args[:1] == ['call']:
    emit({})
sys.exit(1)
