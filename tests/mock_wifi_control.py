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
if args[:1] in (['add_list'], ['del_list']):
    log()
    key, value = args[1].split('=', 1)
    items = [item for item in data['config'].get(key, '').split() if item != value]
    if args[0] == 'add_list':
        items.append(value)
    data['config'][key] = ' '.join(items)
    save()
    sys.exit(0)
if args[:1] == ['delete']:
    log()
    if args[1] not in data['config']:
        sys.exit(1)
    del data['config'][args[1]]
    save()
    sys.exit(0)
if args[:1] in (['commit'], ['revert']):
    log()
    sys.exit(0)
if args[:2] == ['call', 'zwrt_wlan']:
    method = args[2]
    params = json.loads(args[3])
    if method == 'status':
        emit({'app_status': 'idle', 'driver_status': 'idle'})
    if method == 'wlan_get_guest_access_left_time':
        emit({'wlan_guest_left_time_info': data.get('left', [])})
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
            # The firmware rejects flat legacy fields such as SwitchOption.
            if not params or not all(isinstance(value, dict) for value in params.values()):
                sys.exit(1)
            if 'lbd' in params.get('zte_mbb', {}):
                assert set(params) == {'zte_mbb'}
                assert set(params['zte_mbb']) == {'lbd'}
                assert params['zte_mbb']['lbd'] in ('0', '1')
                if not data.get('ignore'):
                    data['lbd'] = params['zte_mbb']['lbd']
                    save()
            elif not data.get('ignore'):
                for section, fields in params.items():
                    for field, value in fields.items():
                        assert isinstance(value, str), (section, field, value)
                        data['config'][f'wireless.{section}.{field}'] = value
                        if (section, field) == ('zte_mbb', 'wifi_onoff'):
                            # Stock behaviour: the module switch drives every radio.
                            data['config']['wireless.zte_mbb.wifi_onoff_by_user'] = value
                            for radio in ('wifi0', 'wifi1'):
                                data['config'][f'wireless.{radio}.disabled'] = '0' if value == '1' else '1'
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
    emit(dict({'wifimain24_wifimain5_enable': 1}, **data.get('isolate', {})))
if args[:2] == ['call', 'zwrt_router.api'] and args[2] == 'router_set_wifi_isolate':
    log()
    params = json.loads(args[3])
    # Only the guest management switch may be written, and never as a full set.
    if set(params) != {'guest_access_restrict_enable'} or 'isolate' not in data:
        sys.exit(1)
    data['isolate'].update(params)
    save()
    emit({'result': 'success'})
if args[:2] == ['call', 'zwrt_router.api'] and args[2] == 'router_get_user_list_num':
    if 'users' not in data:
        sys.exit(1)
    emit(data['users'])
if args[:1] == ['call']:
    emit({})
sys.exit(1)
