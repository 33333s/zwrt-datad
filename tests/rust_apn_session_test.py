#!/usr/bin/env python3
"""APN targets, device session login and expiry through the real HTTP control path.

Everything terminates in a local ubus fixture: no modem, vendor service or
network is contacted and no SMS is sent."""
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

binary = Path(sys.argv[1]).resolve()
mock = Path(__file__).with_name('mock_apn_session_ubus.py').resolve()
PASSWORD = ' Fixture pass/Ωord 123 '
WRONG = 'definitely-not-it'

with tempfile.TemporaryDirectory(prefix='datad-apn-session-') as name:
    root = Path(name)
    (root / 'token').write_text('fixture-apn-token')
    config = {}

    def setup(**changes):
        config.update(changes)
        tmp = root / 'config.new'
        tmp.write_text(json.dumps(config))
        tmp.replace(root / 'config.json')

    def reset_calls():
        (root / 'calls.log').write_text('')

    def calls(service=None, method=None):
        rows = [json.loads(line) for line in (root / 'calls.log').read_text().splitlines() if line]
        return [row for row in rows if (service is None or row[0] == service) and (method is None or row[1] == method)]

    def writes():
        names = {'set_apn_mode', 'enable_manu_apn_id', 'delete_manu_apn', 'modify_manu_apn', 'add_manu_apn'}
        return [row for row in calls('zwrt_apn_object') if row[1] in names]

    setup(model='MU5252', password=PASSWORD)
    reset_calls()
    outputs = []

    def start():
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            port = sock.getsockname()[1]
        env = dict(os.environ, APN_FIXTURE=str(root), ZWRT_DATAD_UBUS_BIN=str(mock),
                   ZWRT_DATAD_UCI_BIN='/usr/bin/false', ZWRT_DATAD_OTA_DISABLE_AUTO='1',
                   ZWRT_DATAD_COOLING_CONFIG=str(root / 'cooling'))
        process = subprocess.Popen([str(binary), '--port', str(port), '--data-dir', str(root / 'data'),
                                    '--auth-token-file', str(root / 'token')], env=env,
                                   stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        return process, port

    def stop(process):
        process.terminate()
        try:
            out, _ = process.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            out, _ = process.communicate()
        outputs.append(out.decode(errors='replace'))

    def request(port, path='/state', body=None):
        headers = {'Content-Type': 'application/json', 'Authorization': 'Bearer fixture-apn-token'}
        req = urllib.request.Request(f'http://127.0.0.1:{port}{path}', headers=headers,
                                     data=None if body is None else json.dumps(body).encode())
        try:
            with urllib.request.urlopen(req, timeout=20) as response:
                return response.status, json.load(response)
        except urllib.error.HTTPError as error:
            return error.code, json.loads(error.read())

    def control(port, action, params):
        return request(port, '/control', {'action': action, 'params': params})

    def wait_template(port, template):
        end = time.monotonic() + 30
        while time.monotonic() < end:
            try:
                code, state = request(port)
                if code == 200 and state.get('device', {}).get('api_template') == template:
                    return
            except (OSError, urllib.error.URLError):
                pass
            time.sleep(.2)
        raise AssertionError(f'template {template} not reached')

    def error_code(response):
        return response[1].get('error', {}).get('code')

    # ---- multi-modem APN targets -------------------------------------------------
    process, port = start()
    try:
        wait_template(port, 'MU5252')
        reset_calls()
        # Targeted modify translates slot_id to slotId, keeps vendor credentials, confirms by readback.
        code, body = control(port, 'apn.modify', {'slot_id': 101, 'profile_id': 'p1', 'name': 'ext', 'apn': 'ext.apn'})
        assert code == 200, body
        modify = [row for row in writes() if row[1] == 'modify_manu_apn']
        assert len(modify) == 1 and modify[0][2]['slotId'] == 101, modify
        assert modify[0][2]['username'] == 'user-101' and modify[0][2]['password'] == 'secret-101'
        assert all(row[2].get('slotId') == 101 for row in calls('zwrt_apn_object') if row[1] != 'getManuApnList' or row[2].get('slotId') is not None)
        saved = json.loads((root / 'apn.json').read_text())
        assert saved['101']['manual'][0]['wanapn'] == 'ext.apn'
        assert saved['main']['manual'][0]['wanapn'] == 'internet' and saved['1']['manual'][0]['wanapn'] == 'internet'

        # The main modem accepts slot 1 and writes slotId 1.
        reset_calls()
        assert control(port, 'apn.set_mode', {'slot_id': 1, 'mode': 0})[0] == 200
        assert [row[2] for row in writes()] == [{'apn_mode': 0, 'slotId': 1}]

        # Add/delete on external modems are refused by datad itself; nothing is written.
        reset_calls()
        for action, params in [('apn.add', {'slot_id': 101, 'name': 'n', 'apn': 'a'}),
                               ('apn.add', {'slot_id': 201, 'name': 'n', 'apn': 'a'}),
                               ('apn.delete', {'slot_id': 101, 'profile_id': 'p1'}),
                               ('apn.delete', {'slot_id': 201, 'profile_id': 'p1'})]:
            response = control(port, action, params)
            assert response[0] == 400 and error_code(response) == 'invalid_parameter', (action, response)
        # Out-of-range, wrongly typed and unknown targets never reach the device or fall back to the main modem.
        for slot in (0, 2, 3, 102, 999, -1, '101', 101.5, True, None):
            response = control(port, 'apn.enable', {'slot_id': slot, 'profile_id': 'p1'})
            assert response[0] == 400 and error_code(response) == 'invalid_parameter', (slot, response)
        assert calls('zwrt_apn_object') == [], 'invalid targets must not touch the device'

        # Slot 1 add works (primary modem) and is read back; legacy params keep the old single-modem call.
        assert control(port, 'apn.add', {'slot_id': 1, 'name': 'extra', 'apn': 'extra.apn'})[0] == 200
        assert any(p['wanapn'] == 'extra.apn' for p in json.loads((root / 'apn.json').read_text())['1']['manual'])
        reset_calls()
        assert control(port, 'apn.enable', {'profile_id': 'p1'})[0] == 200
        assert [row[2] for row in writes()] == [{'profileId': 'p1'}]
        assert calls('zwrt_apn_object', 'get_apn_mode') == [], 'legacy path has no readback'

        # An unreadable target is never written.
        setup(unreadable=[201])
        reset_calls()
        response = control(port, 'apn.modify', {'slot_id': 201, 'profile_id': 'p1', 'name': 'n', 'apn': 'a'})
        assert response[0] == 502 and error_code(response) == 'device_call_failed', response
        assert writes() == []
        setup(unreadable=[])

        # A write the device silently ignores is reported, not claimed.
        setup(ignore_writes=[101])
        response = control(port, 'apn.set_mode', {'slot_id': 101, 'mode': 0})
        assert response[0] == 502 and error_code(response) == 'device_call_failed', response
        setup(ignore_writes=[])
    finally:
        stop(process)

    # Single-modem templates never accept slot_id (the vendor call would silently hit the only modem).
    setup(model='MU5250')
    process, port = start()
    try:
        wait_template(port, 'MU5250')
        reset_calls()
        response = control(port, 'apn.enable', {'slot_id': 1, 'profile_id': 'p1'})
        assert response[0] == 400 and error_code(response) == 'invalid_parameter', response
        assert calls('zwrt_apn_object') == []
        assert control(port, 'apn.enable', {'profile_id': 'p1'})[0] == 200
    finally:
        stop(process)
    setup(model='MU5252')

    # ---- device session ----------------------------------------------------------
    def login(port, password=PASSWORD):
        return control(port, 'device.session.login', {'password': password})

    process, port = start()
    try:
        wait_template(port, 'MU5252')
        # Invalid shapes are rejected without any vendor call.
        reset_calls()
        for params in ({}, {'password': ''}, {'password': 'a\nb'}, {'password': 'a\x00b'}, {'password': 'p' * 1025},
                       {'password': 'x', 'extra': 1}, {'password': 5}):
            response = control(port, 'device.session.login', params)
            assert response[0] == 400 and error_code(response) == 'invalid_parameter', (params, response)
        assert calls('zwrt_web') == []
        # Before any login nothing is expired: the legacy device behaviour is unchanged.
        response = control(port, 'apn.enable', {'profile_id': 'p1'})
        assert response[0] == 200, response

        # Wrong password: explicit rejection, no session.
        response = login(port, WRONG)
        assert response[0] == 401 and error_code(response) == 'invalid_credentials', response
        # Correct password (leading/trailing spaces kept): the session is verified, then reported active.
        response = login(port)
        assert response == (200, {'ok': True, 'action': 'device.session.login', 'result': {'active': True}}), response
        login_calls = calls('zwrt_web', 'web_login')
        assert len(login_calls) == 2 and login_calls[0][2] != login_calls[1][2]
        assert set(login_calls[1][2]) == {'password'} and len(login_calls[1][2]['password']) == 64
        assert calls('session', 'list'), 'login must be confirmed against the device session list'

        # While the session exists writes proceed normally.
        assert control(port, 'apn.enable', {'profile_id': 'p1'})[0] == 200
        # Vendor lockout is reported as rate limiting without burning another attempt.
        setup(lock=300)
        reset_calls()
        response = login(port)
        assert response[0] == 429 and error_code(response) == 'device_session_rate_limited', response
        assert calls('zwrt_web', 'web_login') == []
        setup(lock=0)
        # datad's own limiter: three attempts per minute.
        response = login(port)
        assert response[0] == 429 and error_code(response) == 'device_session_rate_limited', response

        # The vendor replaces/expires the session: writes and SMS fail before any change or send.
        (root / 'sessions').unlink()
        reset_calls()
        for action, params in [('apn.enable', {'profile_id': 'p1'}),
                               ('apn.set_mode', {'slot_id': 101, 'mode': 1}),
                               ('sms.send_raw', {'sender': 'host', 'number': '10086', 'message_hex': '6D4B8BD5',
                                                 'sms_time': '26;09;22;12;00;00;+;0'})]:
            response = control(port, action, params)
            assert response[0] == 409 and error_code(response) == 'device_session_expired', (action, response)
        assert writes() == []
        assert calls('zwrt_wms', 'zte_libwms_send_sms') == [], 'no SMS send after expiry'
    finally:
        stop(process)

    # A fresh process starts without a session, and vendor trouble is distinguished from bad passwords.
    process, port = start()
    try:
        wait_template(port, 'MU5252')
        setup(login_info_failure=True)
        response = login(port)
        assert response[0] == 503 and error_code(response) == 'device_session_unavailable', response
        setup(login_info_failure=False)
        assert login(port)[0] == 200
    finally:
        stop(process)

    # ---- automatic update switch -------------------------------------------------
    process, port = start()
    try:
        wait_template(port, 'MU5252')
        ota_file = root / 'data' / 'ota.json'
        for params in ({}, {'enabled': 'false'}, {'enabled': 0}, {'enabled': False, 'servers': ['https://x.example']}):
            response = control(port, 'datad.ota.set', params)
            assert response[0] == 400 and error_code(response) == 'invalid_parameter', (params, response)
        assert not ota_file.exists(), 'invalid requests must not write the configuration'
        response = control(port, 'datad.ota.set', {'enabled': False})
        assert response == (200, {'ok': True, 'action': 'datad.ota.set', 'result': {'auto_update_enabled': False}}), response
        stored = json.loads(ota_file.read_text())
        assert stored['enabled'] is False and stored['servers'] == [] and 'custom' in stored['sources']
        assert oct(ota_file.stat().st_mode & 0o777) == '0o600'
        assert control(port, 'datad.ota.set', {'enabled': True})[0] == 200
        again = json.loads(ota_file.read_text())
        assert again['enabled'] is True and {k: v for k, v in again.items() if k != 'enabled'} == {k: v for k, v in stored.items() if k != 'enabled'}
        assert 'datad.ota.set' in request(port, '/capabilities')[1]['controls']
        assert 'device.session.login' in request(port, '/capabilities')[1]['controls']
    finally:
        stop(process)

    # The password never reaches the vendor log, output, or any response.
    assert PASSWORD.strip() not in (root / 'calls.log').read_text()
    for out in outputs:
        assert PASSWORD.strip() not in out and PASSWORD not in out
    print('APN targets, slot validation, readback, device session login, expiry and secret handling PASS')
