#!/usr/bin/env python3
"""Local-only vendor fixture for APN targets and the web login; sends nothing anywhere."""
import hashlib
import json
import os
from pathlib import Path
import sys

root = Path(os.environ['APN_FIXTURE'])
config = json.loads((root / 'config.json').read_text())
args = sys.argv[1:]
if args[:1] != ['call']:
    print('{}')
    sys.exit(0)
service, method = args[1:3]
params = json.loads(args[3]) if len(args) > 3 else {}
with (root / 'calls.log').open('a') as log:
    log.write(json.dumps([service, method, params]) + '\n')


def up(value):
    return hashlib.sha256(value).hexdigest().upper()


SALT = 'fixture-salt'
state_file = root / 'apn.json'
apn = json.loads(state_file.read_text()) if state_file.exists() else {
    key: {'mode': 1, 'enabled': 'p1', 'auto': [],
          'manual': [{'profileId': 'p1', 'profilename': 'internet', 'wanapn': 'internet',
                      'username': f'user-{key}', 'password': f'secret-{key}', 'pdpType': 1,
                      'pppAuthMode': 0, 'roamingPdpType': 1}]}
    for key in ('main', '1', '101', '201')}


def save():
    state_file.write_text(json.dumps(apn))


if service == 'zwrt_zte_mdm.api' and method == 'get_zwrt_common_info':
    print(json.dumps({'model_name': config.get('model', 'MU5252')}))
elif service == 'zwrt_apn_object':
    slot = params.get('slotId')
    key = 'main' if slot is None else str(slot)
    if slot is not None and slot in config.get('unreadable', []):
        sys.exit(1)
    entry = apn[key]
    ignore_writes = slot in config.get('ignore_writes', [])
    if method == 'get_apn_mode':
        print(json.dumps({'apn_mode': entry['mode']}))
    elif method == 'getAutoApnList':
        print(json.dumps({'apnListArray': entry['auto']}))
    elif method == 'getManuApnList':
        print(json.dumps({'apnListArray': entry['manual']}))
    elif method == 'get_enabled_manu_apn_id':
        print(json.dumps({'profileId': entry['enabled']}))
    elif ignore_writes:
        print('{}')
    elif method == 'set_apn_mode':
        entry['mode'] = params['apn_mode']; save(); print('{}')
    elif method == 'enable_manu_apn_id':
        entry['enabled'] = params['profileId']; save(); print('{}')
    elif method == 'delete_manu_apn':
        entry['manual'] = [p for p in entry['manual'] if p['profileId'] != params['profileId']]; save(); print('{}')
    elif method == 'modify_manu_apn':
        for profile in entry['manual']:
            if profile['profileId'] == params['profileId']:
                profile.update({k: v for k, v in params.items() if k != 'slotId'})
        save(); print('{}')
    elif method == 'add_manu_apn':
        entry['manual'].append(dict({k: v for k, v in params.items() if k != 'slotId'},
                                    profileId=f"p{len(entry['manual']) + 1}"))
        save(); print('{}')
    else:
        print('{}')
elif service == 'zwrt_web' and method == 'web_login_info':
    if config.get('login_info_failure'):
        sys.exit(1)
    print(json.dumps({'login_fail_num': 5, 'login_fail_lock_lefttime': str(config.get('lock', 0)),
                      'zte_web_sault': SALT}))
elif service == 'zwrt_web' and method == 'web_login':
    expected = up((up(config['password'].encode()) + SALT).encode())
    if params.get('password') == expected:
        sid = 'a1b2c3d4e5f60718293a4b5c6d7e8f90'
        sessions = root / 'sessions'
        sessions.write_text(sid)
        print(json.dumps({'result': '0', 'ubus_rpc_session': sid}))
    else:
        print(json.dumps({'result': '1'}))
elif service == 'session' and method == 'list':
    sid = params.get('ubus_rpc_session')
    sessions = root / 'sessions'
    if sid is None:
        print(json.dumps({'ubus_rpc_session': '0' * 32}))
    elif sessions.exists() and sessions.read_text() == sid:
        print(json.dumps({'ubus_rpc_session': sid, 'timeout': 300}))
    else:
        sys.stderr.write('Command failed: Not found\n')
        sys.exit(252)
else:
    print('{}')
