#!/usr/bin/env python3
"""Regression tests through real HTTP for Wi-Fi configuration and steering."""
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
mock = Path(__file__).with_name('mock_wifi_control.py').resolve()
with tempfile.TemporaryDirectory(prefix='datad-wifi-test-') as name:
    base = Path(name)
    fixture = base / 'wifi.json'
    calls = base / 'writes.log'
    token = base / 'auth.token'
    token.write_text('fixture-wifi-token')
    config = {}
    for section, ssid in [('main_2g', 'Fixture 2G'), ('main_5g', 'Fixture 5G')]:
        values = dict(ssid=ssid, key='fixture-password', encryption='sae-mixed',
                      disabled='0', hidden='1', isolate='1', pmf='2', maxassoc='17')
        config.update({f'wireless.{section}.{key}': value for key, value in values.items()})

    def setup(**data):
        fresh = dict(config=config, lbd='0', **data)
        staging = fixture.with_suffix('.new')
        staging.write_text(json.dumps(fresh))
        staging.replace(fixture)

    def writes():
        return [json.loads(line) for line in calls.read_text().splitlines()] if calls.exists() else []

    setup()
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
    env = dict(os.environ, WIFI_FIXTURE=str(fixture), WIFI_WRITES=str(calls),
               ZWRT_DATAD_UBUS_BIN=str(mock), ZWRT_DATAD_UCI_BIN=str(mock),
               ZWRT_DATAD_DIR=str(base / 'cloud'), ZWRT_DATAD_COOLING_CONFIG=str(base / 'cooling'))
    proc = subprocess.Popen([str(binary), '--port', str(port), '--data-dir', str(base / 'data'),
                             '--auth-token-file', str(token)], env=env,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    def request(action, params=None, auth=True):
        headers = {'Content-Type': 'application/json'}
        if auth:
            headers['Authorization'] = 'Bearer fixture-wifi-token'
        body = json.dumps(dict(action=action, params={} if params is None else params)).encode()
        req = urllib.request.Request(f'http://127.0.0.1:{port}/control', data=body, headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=12) as response:
                return response.status, json.load(response)
        except urllib.error.HTTPError as error:
            return error.code, json.loads(error.read())

    try:
        for _ in range(100):
            try:
                if request('wifi.status')[0] == 200:
                    break
            except (OSError, urllib.error.URLError):
                pass
            time.sleep(.05)
        else:
            raise AssertionError('startup failed')
        assert request('wifi.status', auth=False)[0] == 401
        assert request('wifi.set_dual_band', {'enabled': True}, auth=False)[0] == 401
        result = request('wifi.status')[1]['result']
        for section in ('main_2g', 'main_5g'):
            assert result[section] == {key.split('.')[-1]: value for key, value in config.items()
                                       if key.startswith(f'wireless.{section}.')}, result
        changed = dict(section='main_2g', hidden='0', isolate='0', pmf='1', maxassoc='12')
        code, body = request('wifi.configure', changed)
        assert code == 200 and body['result']['verified'] and body['result']['changed'], body
        result = request('wifi.status')[1]['result']['main_2g']
        for key in ('hidden', 'isolate', 'pmf', 'maxassoc'):
            assert result[key] == changed[key], result
        count = len(writes())
        assert request('wifi.configure', changed)[1]['result']['changed'] is False
        assert len(writes()) == count
        # Re-saving the complete configuration must not reset hidden/PMF/etc.
        setup()
        saved = request('wifi.status')[1]['result']['main_2g']
        saved.pop('disabled')
        assert request('wifi.configure', dict(section='main_2g', **saved))[1]['result']['changed'] is False
        assert request('wifi.status')[1]['result']['main_2g']['hidden'] == '1'
        assert request('wifi.configure', dict(section='main_2g', key=''))[0] == 200
        assert request('wifi.status')[1]['result']['main_2g']['key'] == 'fixture-password'

        # Isolation reports 1 while real steering is 0. Reading must not confuse them.
        setup()
        result = request('wifi.dual_band_status')[1]['result']
        assert result['enabled'] is False and result['WiFiDualBandEnabled'] == '0', result
        for enabled in (True, False, 1, 0):
            code, body = request('wifi.set_dual_band', {'enabled': enabled})
            assert code == 200 and body['result']['verified'], body
            result = request('wifi.dual_band_status')[1]['result']
            assert result['enabled'] == bool(enabled) and result['BandSteeringSwitch'] == str(int(enabled))
        count = len(writes())
        assert request('wifi.set_dual_band', {'enabled': False})[1]['result']['changed'] is False
        assert len(writes()) == count
        for value in (None, 2, '1', [], {}):
            count = len(writes())
            code, _ = request('wifi.set_dual_band', {} if value is None else {'enabled': value})
            assert code == 400 and len(writes()) == count
        for value in ('0', '1', 0, 1, False, True):
            data = json.loads(fixture.read_text()); data['lbd'] = value
            fixture.write_text(json.dumps(data))
            assert request('wifi.dual_band_status')[1]['result']['enabled'] == (str(value) in ('1', 'True'))
        for fault in ('missing', 'read_fail', 'malformed'):
            setup(**{fault: True})
            count = len(writes())
            assert request('wifi.dual_band_status')[0] == 502
            assert request('wifi.set_dual_band', {'enabled': True})[0] == 502
            assert len(writes()) == count
        for value in ('', 'unknown', 2, None, [], {}):
            setup()
            data = json.loads(fixture.read_text()); data['lbd'] = value
            fixture.write_text(json.dumps(data))
            assert request('wifi.dual_band_status')[0] == 502
        for fault in ('reject', 'ignore', 'bad_reply'):
            setup(**{fault: True})
            code, body = request('wifi.set_dual_band', {'enabled': True})
            assert code == 502 and 'must-not-leak' not in json.dumps(body), body
        setup(empty_reply=True)
        assert request('wifi.set_dual_band', {'enabled': True})[1]['result']['verified']
        for result in (0, '0', True, 'success', 'ok'):
            setup(result=result)
            assert request('wifi.set_dual_band', {'enabled': True})[0] == 200
        for fault in ('reject', 'bad_reply', 'restore_on_reload'):
            setup(**{fault: True})
            code, body = request('wifi.configure', {'section': 'main_2g', 'ssid': 'New Name'})
            assert code == 502 and 'must-not-leak' not in json.dumps(body), body
        setup(restore_on_reload=True)
        code, body = request('wifi.configure', {'section': 'main_2g', 'key': 'private-new-key'})
        assert code == 502 and 'private-new-key' not in json.dumps(body), body

        # 6 GHz model (MC8531): radios are wifi2 (2.4G), wifi0 (5G), wifi1 (6G), not wifi0/wifi1.
        six = dict(config)
        for band, radio, ssid, enc, channels, country in [
                ('2g', 'wifi2', 'Six 2G', 'sae-mixed', '1,6,11', 'HK'),
                ('5g', 'wifi0', 'Six 5G', 'sae-mixed', '36,40,44', 'HK'),
                ('6g', 'wifi1', 'Six 6G', 'sae', '1,5,9', 'CN')]:
            six[f'wireless.main_{band}.device'] = radio
            six[f'wireless.main_{band}.ssid'] = ssid
            six[f'wireless.main_{band}.encryption'] = enc
            six[f'wireless.main_{band}.disabled'] = '0'
            six[f'wireless.{radio}.channellist'] = channels
            six[f'wireless.{radio}.country'] = country
            six[f'wireless.{radio}.channel'] = '0'
        six['wireless.guest_6g.ssid'] = 'Guest 6G'
        fixture.write_text(json.dumps(dict(config=six, lbd='0')))
        status = request('wifi.status')[1]['result']
        assert status['main_6g']['ssid'] == 'Six 6G' and status['main_6g']['encryption'] == 'sae', status
        report = request('wireless.config')[1]['result']
        assert set(report['radios']) == {'2g', '5g', '6g'}, report
        assert [report['radios'][b]['section'] for b in ('2g', '5g', '6g')] == ['wifi2', 'wifi0', 'wifi1'], report
        assert report['radios']['6g']['supported_channels'] == [0, 1, 5, 9], report
        assert report['radios']['6g']['ap_section'] == 'main_6g' and report['country'] == '', report
        before = len(writes())
        code, body = request('wireless.config', dict(band='6g', channel=5))
        assert code == 200 and body['result']['changed'], body
        code, body = request('wireless.config', dict(band='2g', channel=6))
        assert code == 200 and body['result']['changed'], body
        new = [json.dumps(args) for args in writes()[before:]]
        assert any('wireless.wifi1.channel=5' in item for item in new), new
        assert any('wireless.wifi2.channel=6' in item for item in new), new
        assert not any('wireless.wifi0.channel' in item for item in new), new
        assert request('wireless.config', dict(band='6g', channel=7))[0] == 400
        assert request('wireless.config', dict(band='7g', channel=1))[0] == 400
        before = len(writes())
        assert request('wireless.config', dict(band='5g', country='CN'))[1]['result']['changed']
        new = [json.dumps(args) for args in writes()[before:]]
        for radio in ('wifi2', 'wifi0', 'wifi1'):
            assert any(f'wireless.{radio}.country=CN' in item for item in new), new
        # 6 GHz needs WPA3-SAE; everything else would never associate.
        for encryption in ('psk2', 'sae-mixed', 'none'):
            assert request('wifi.configure', dict(section='main_6g', encryption=encryption))[0] == 400
        code, body = request('wifi.configure', dict(section='main_6g', ssid='New 6G', encryption='sae'))
        assert code == 200 and body['result']['changed'], body
        assert request('wifi.status')[1]['result']['main_6g']['ssid'] == 'New 6G'
        assert request('wifi.configure', dict(section='guest_6g', ssid='Guest 6G 2'))[0] == 200
        before = len(writes())
        assert request('client.block', dict(mac='00:11:22:33:44:55'))[0] == 200
        new = [json.dumps(args) for args in writes()[before:]]
        for section in ('main_2g', 'main_5g', 'guest_2g', 'guest_5g', 'main_6g', 'guest_6g'):
            assert any(f'wireless.{section}.denymaclist=00:11:22:33:44:55' in item for item in new), (section, new)
        # A model without a 6 GHz AP must neither accept nor create the 6 GHz sections.
        setup()
        assert request('wifi.configure', dict(section='main_6g', ssid='x'))[0] == 400
        assert 'main_6g' not in request('wifi.status')[1]['result']
        assert set(request('wireless.config')[1]['result']['radios']) == {'2g', '5g'}
        assert request('wireless.config', dict(band='6g', channel=1))[0] == 400
        before = len(writes())
        assert request('client.block', dict(mac='00:11:22:33:44:66'))[0] == 200
        assert not any('6g' in json.dumps(args) for args in writes()[before:])
        assert not any('router_set_wifi_isolate' in args for args in writes())
        print('Wi-Fi status fields, steering mapping, no-op, validation, auth, failure/readback and credential redaction PASS')
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=8)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
