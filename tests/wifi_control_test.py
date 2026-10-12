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
    net = base / 'net'
    for name, state in [('wlan0', 'up'), ('wlan2', 'down')]:
        (net / name).mkdir(parents=True)
        (net / name / 'operstate').write_text(state + '\n')
    env = dict(os.environ, WIFI_FIXTURE=str(fixture), WIFI_WRITES=str(calls), ZWRT_DATAD_NET_CLASS_DIR=str(net),
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

        # MAC filter: a whitelist set in the stock UI must survive block/unblock.
        setup()
        sections = ('main_2g', 'main_5g', 'guest_2g', 'guest_5g')
        status = request('wifi.acl.status')[1]['result']
        assert status['mode'] == 'disable' and status['allow'] == [] and status['consistent'], status
        assert request('wifi.acl.set', dict(mode='allow', allow=[]))[0] == 400
        assert request('wifi.acl.set', dict(mode='allow'))[0] == 400
        assert request('wifi.acl.set', dict(mode='block', allow=['00:11:22:33:44:55']))[0] == 400
        assert request('wifi.acl.set', dict(mode='allow', allow=['not-a-mac']))[0] == 400
        assert request('wifi.acl.set', dict(mode='allow', allow='00:11:22:33:44:55'))[0] == 400
        too_many = [f'02:00:00:00:00:{i:02x}' for i in range(65)]
        assert request('wifi.acl.set', dict(mode='allow', allow=too_many))[0] == 400
        before = len(writes())
        code, body = request('wifi.acl.set', dict(mode='allow', allow=['AA:BB:CC:DD:EE:01', 'aa:bb:cc:dd:ee:01', 'aa:bb:cc:dd:ee:02']))
        assert code == 200 and body['result']['changed'] and body['result']['verified'], body
        assert body['result']['allow'] == ['aa:bb:cc:dd:ee:01', 'aa:bb:cc:dd:ee:02'], body
        new = [json.dumps(args) for args in writes()[before:]]
        for section in sections:
            assert any(f'wireless.{section}.macfilter=allow' in item for item in new), (section, new)
            assert any(f'wireless.{section}.allowmaclist=aa:bb:cc:dd:ee:02' in item for item in new), (section, new)
        assert any('"reload"' in item for item in new), new
        status = request('wifi.acl.status')[1]['result']
        assert status == dict(mode='allow', allow=['aa:bb:cc:dd:ee:01', 'aa:bb:cc:dd:ee:02'], deny=[],
                              consistent=True, max_entries=64), status
        before = len(writes())
        assert request('wifi.acl.set', dict(mode='allow', allow=['aa:bb:cc:dd:ee:02', 'aa:bb:cc:dd:ee:01']))[1]['result']['changed']
        assert request('wifi.acl.set', dict(mode='allow'))[1]['result']['changed'] is False
        before = len(writes())
        for action in ('client.block', 'client.unblock'):
            code, body = request(action, dict(mac='00:11:22:33:44:77'))
            assert code == 409 and body['error']['code'] == 'wifi_acl_allow_mode', body
        assert len(writes()) == before
        assert request('wifi.acl.status')[1]['result']['mode'] == 'allow'
        # Switching back keeps the stored allow list; blocking then works again.
        code, body = request('wifi.acl.set', dict(mode='deny'))
        assert code == 200 and body['result']['allow'] == ['aa:bb:cc:dd:ee:02', 'aa:bb:cc:dd:ee:01'], body
        assert request('client.block', dict(mac='00:11:22:33:44:77'))[0] == 200
        status = request('wifi.acl.status')[1]['result']
        assert status['mode'] == 'deny' and status['deny'] == ['00:11:22:33:44:77'], status
        code, body = request('wifi.acl.set', dict(mode='disable', deny=[]))
        assert code == 200 and body['result']['deny'] == [], body
        assert 'wireless.main_2g.denymaclist' not in json.loads(fixture.read_text())['config']
        # Module, radio and AP switches are reported separately; unknown is null.
        switches = dict(config)
        switches.update({
            'wireless.zte_mbb': 'zte_mbb', 'wireless.zte_mbb.wifi_onoff': '1',
            'wireless.zte_mbb.wifi_onoff_by_user': '1',
            'wireless.wifi0': 'wifi-device', 'wireless.wifi0.disabled': '1',
            'wireless.wifi1': 'wifi-device', 'wireless.wifi1.disabled': '0',
            'wireless.main_2g': 'wifi-iface', 'wireless.main_2g.ifname': 'wlan0',
            'wireless.main_5g': 'wifi-iface', 'wireless.main_5g.ifname': 'wlan2',
            'wireless.guest_2g': 'wifi-iface', 'wireless.guest_2g.ifname': 'wlan1',
            'wireless.guest_2g.disabled': '1', 'wireless.guest_2g.guest_active_time': '240',
            'wireless.guest_5g': 'wifi-iface', 'wireless.guest_5g.ifname': 'wlan3',
            'wireless.guest_5g.disabled': '0', 'wireless.guest_5g.guest_active_time': '240',
            'wireless.guest_5g.ssid': 'Fixture Guest', 'wireless.guest_5g.encryption': 'none',
            'wireless.readonly': 'readonly',
            'wireless.readonly.wifi_attr_chip1_support_guest_ssid': '1',
            'wireless.readonly.wifi_attr_chip2_support_guest_ssid': '1',
        })
        fixture.write_text(json.dumps(dict(
            config=switches, lbd='0', isolate={'guest_access_restrict_enable': 0},
            users={'guest_num_24g': 0, 'guest_num_5g': 2},
            left=[{'section_name': 'guest_5g', 'guest_left_time': '7179'},
                  {'section_name': 'guest_2g', 'guest_left_time': '99'}])))
        wifi = request('wifi.state')[1]['result']
        assert wifi['busy'] is False and wifi['module'] == {'enabled': True, 'user_enabled': True}, wifi
        # A radio-level disabled 2.4 GHz (left by 0.10.74-0.10.76) reads as the band off.
        assert wifi['radios'] == [
            {'band': '2g', 'device': 'wifi0', 'enabled': False, 'radio_enabled': False},
            {'band': '5g', 'device': 'wifi1', 'enabled': True, 'radio_enabled': True}], wifi
        aps = {item['section']: item for item in wifi['aps']}
        assert aps['main_2g']['enabled'] is True and aps['main_2g']['active'] is True, aps
        assert aps['main_5g']['state'] == 'down' and aps['main_5g']['active'] is False, aps
        assert aps['guest_2g']['enabled'] is False and aps['guest_2g']['state'] == 'absent', aps
        assert aps['guest_5g']['kind'] == 'guest' and aps['guest_5g']['enabled'] is True, aps
        guest = request('wifi.guest.status')[1]['result']
        assert guest['management_access'] is True and guest['active_time_minutes'] == 240, guest
        assert guest['active_time_supported'] is None, guest
        bands = {item['band']: item for item in guest['guests']}
        # A switched-off guest SSID reports no remaining time, even if the
        # firmware still counts a stale timer for it.
        assert bands['2g']['left_seconds'] is None and bands['2g']['clients'] == 0, bands
        assert bands['5g']['left_seconds'] == 7179 and bands['5g']['clients'] == 2, bands
        assert bands['5g']['supported'] is True and bands['5g']['ssid'] == 'Fixture Guest', bands
        before = len(writes())
        code, body = request('wifi.set_module', dict(enabled=0))
        assert code == 200 and body['result']['changed'] and body['result']['verified'], body
        new = [json.dumps(args) for args in writes()[before:]]
        assert any('wifi_onoff' in item for item in new), new
        assert not any('SwitchOption' in item for item in new), new
        assert request('wifi.state')[1]['result']['module']['enabled'] is False
        assert request('wifi.set_module', dict(enabled=False))[1]['result']['changed'] is False
        assert request('wifi.set_module', dict(enabled=1))[0] == 200
        assert request('wifi.set_module', dict(enabled=2))[0] == 400
        code, body = request('wifi.set_chip', dict(chip='chip2', guest_enabled=0))
        assert code == 400 and 'not supported' in body['error']['message'], body
        # A band is switched through its SSIDs like the stock web UI, never
        # through the radio: a disabled 2.4 GHz radio keeps every band down on
        # the single-chip QCA firmwares and can hang the device at boot.
        before = len(writes())
        code, body = request('wifi.radio.set', dict(band='5g', enabled=False))
        assert code == 200 and body['result'] == dict(band='5g', device='wifi1', enabled=False,
                                                      changed=True, verified=True,
                                                      sections=['main_5g', 'guest_5g']), body
        sets = [json.loads(args[3]) for args in writes()[before:] if args[:3] == ['call', 'zwrt_wlan', 'set']]
        assert sets == [{'main_5g': {'disabled': '1'}, 'guest_5g': {'disabled': '1'}}], sets
        radios = {item['band']: item for item in request('wifi.state')[1]['result']['radios']}
        assert radios['5g']['enabled'] is False and radios['5g']['radio_enabled'] is True, radios
        assert request('wifi.radio.set', dict(band='5g', enabled=False))[1]['result']['changed'] is False
        before = len(writes())
        code, body = request('wifi.radio.set', dict(band='2g', enabled=False))
        assert code == 200 and body['result']['sections'] == ['main_2g'], body
        sets = [json.loads(args[3]) for args in writes()[before:] if args[:3] == ['call', 'zwrt_wlan', 'set']]
        assert sets == [{'main_2g': {'disabled': '1'}}] and 'wifi0' not in json.dumps(sets), sets
        # Switching a band on clears a radio-level disable left by older releases;
        # the guest SSID keeps its own switch.
        data = json.loads(fixture.read_text())
        data['config']['wireless.wifi0.disabled'] = '1'
        fixture.write_text(json.dumps(data))
        before = len(writes())
        code, body = request('wifi.radio.set', dict(band='2g', enabled=True))
        assert code == 200 and body['result']['sections'] == ['main_2g', 'wifi0'], body
        sets = [json.loads(args[3]) for args in writes()[before:] if args[:3] == ['call', 'zwrt_wlan', 'set']]
        assert sets == [{'main_2g': {'disabled': '0'}, 'wifi0': {'disabled': '0'}}], sets
        radios = {item['band']: item for item in request('wifi.state')[1]['result']['radios']}
        assert radios['2g'] == {'band': '2g', 'device': 'wifi0', 'enabled': True, 'radio_enabled': True}, radios
        code, body = request('wifi.radio.set', dict(band='5g', enabled=True))
        assert code == 200 and body['result']['sections'] == ['main_5g'], body
        aps = {item['section']: item for item in request('wifi.state')[1]['result']['aps']}
        assert aps['main_5g']['enabled'] is True and aps['guest_5g']['enabled'] is False, aps
        # Single-chip qcacld32 firmwares (U60 Pro, MC7523, TopFlow): QCMAP starts a lone
        # AP only on wlan0, so the band that stays on takes wlan0 (TopFlow's own layout).
        data = json.loads(fixture.read_text())
        data['config'].update({
            'wireless.wifi0.type': 'qcacld32', 'wireless.wifi1.type': 'qcacld32',
            'wireless.wifi0.disabled': '0', 'wireless.wifi1.disabled': '0',
            'wireless.main_2g.disabled': '0', 'wireless.main_5g.disabled': '0',
            'wireless.main_2g.device': 'wifi0', 'wireless.main_5g.device': 'wifi1',
            'wireless.main_2g.ifname': 'wlan0', 'wireless.guest_2g.ifname': 'wlan1',
            'wireless.main_5g.ifname': 'wlan2', 'wireless.guest_5g.ifname': 'wlan3',
            'wireless.backhaul_2g': 'wifi-iface', 'wireless.backhaul_2g.ifname': 'wlan1'})
        fixture.write_text(json.dumps(data))
        def layout():
            config = json.loads(fixture.read_text())['config']
            return {key.split('.', 1)[1]: config.get(key) for key in (
                'wireless.wifi0.disabled', 'wireless.wifi1.disabled',
                'wireless.main_2g.ifname', 'wireless.guest_2g.ifname', 'wireless.backhaul_2g.ifname',
                'wireless.main_5g.ifname', 'wireless.guest_5g.ifname')}
        # The main SSID on wlan0 cannot be switched off through wifi.configure here.
        before = len(writes())
        code, body = request('wifi.configure', dict(section='main_2g', enabled=False))
        assert code == 400 and 'wifi.radio.set' in body['error']['message'], body
        assert len(writes()) == before
        # With Mesh on the single-band layout never comes up (MC7523): refused.
        data = json.loads(fixture.read_text())
        data['config']['wireless.zte_mbb.mesh_onoff'] = '1'
        fixture.write_text(json.dumps(data))
        before = len(writes())
        code, body = request('wifi.radio.set', dict(band='2g', enabled=False))
        assert code == 400 and 'Mesh' in body['error']['message'], body
        assert len(writes()) == before
        data['config']['wireless.zte_mbb.mesh_onoff'] = '0'
        fixture.write_text(json.dumps(data))
        before = len(writes())
        code, body = request('wifi.radio.set', dict(band='2g', enabled=False))
        assert code == 200 and body['result']['changed'] and body['result']['verified'], body
        assert layout() == {'wifi0.disabled': '1', 'wifi1.disabled': '0',
                            'main_2g.ifname': 'wlanx', 'guest_2g.ifname': 'wlanx', 'backhaul_2g.ifname': 'wlanx',
                            'main_5g.ifname': 'wlan0', 'guest_5g.ifname': 'wlan1'}, layout()
        new = writes()[before:]
        assert any(args[:3] == ['call', 'zwrt_wlan', 'reload'] for args in new), new
        assert not any(args[:3] == ['call', 'zwrt_wlan', 'set'] for args in new), new
        assert 'backhaul_5g' not in json.dumps(new), new
        radios = {item['band']: item for item in request('wifi.state')[1]['result']['radios']}
        assert radios['2g']['enabled'] is False and radios['5g']['enabled'] is True, radios
        assert request('wifi.radio.set', dict(band='2g', enabled=False))[1]['result']['changed'] is False
        # The last band on is never switched off here; that is the module switch.
        code, body = request('wifi.radio.set', dict(band='5g', enabled=False))
        assert code == 400 and 'wifi.set_module' in body['error']['message'], body
        code, body = request('wifi.radio.set', dict(band='2g', enabled=True))
        assert code == 200 and body['result']['changed'], body
        assert layout() == {'wifi0.disabled': '0', 'wifi1.disabled': '0',
                            'main_2g.ifname': 'wlan0', 'guest_2g.ifname': 'wlan1', 'backhaul_2g.ifname': 'wlan1',
                            'main_5g.ifname': 'wlan2', 'guest_5g.ifname': 'wlan3'}, layout()
        # Switching 5 GHz off keeps 2.4 GHz on wlan0.
        assert request('wifi.radio.set', dict(band='5g', enabled=False))[0] == 200
        assert layout()['main_2g.ifname'] == 'wlan0' and layout()['main_5g.ifname'] == 'wlanx', layout()
        assert layout()['wifi1.disabled'] == '1', layout()
        assert request('wifi.radio.set', dict(band='5g', enabled=True))[0] == 200
        assert layout()['main_5g.ifname'] == 'wlan2' and layout()['wifi1.disabled'] == '0', layout()
        assert request('wifi.radio.set', dict(band='6g', enabled=True))[0] == 400
        assert request('wifi.radio.set', dict(band='5g'))[0] == 400
        assert request('wifi.guest.set', {})[0] == 400
        assert request('wifi.guest.set', dict(active_time=100))[0] == 400
        assert request('wifi.guest.set', dict(bands={'7g': True}))[0] == 400
        assert request('wifi.guest.set', dict(bands={'6g': True}))[0] == 400
        before = len(writes())
        code, body = request('wifi.guest.set', dict(active_time=120, management_access=False))
        assert code == 200 and body['result']['changed'], body
        assert body['result']['active_time_minutes'] == 120 and body['result']['management_access'] is False, body
        new = writes()[before:]
        sets = [args for args in new if args[:3] == ['call', 'zwrt_wlan', 'set']]
        assert len(sets) == 1 and json.loads(sets[0][3]) == {
            'guest_2g': {'guest_active_time': '120'}, 'guest_5g': {'guest_active_time': '120'}}, sets
        isolate = [args for args in new if 'router_set_wifi_isolate' in args]
        assert len(isolate) == 1 and json.loads(isolate[0][3]) == {'guest_access_restrict_enable': 1}, isolate
        before = len(writes())
        assert request('wifi.guest.set', dict(active_time=120, management_access=False))[1]['result']['changed'] is False
        assert len(writes()) == before
        code, body = request('wifi.guest.set', dict(enabled=True))
        assert code == 200 and all(item['enabled'] for item in body['result']['guests']), body
        code, body = request('wifi.guest.set', dict(bands={'5g': False}))
        assert {item['band']: item['enabled'] for item in body['result']['guests']} == {'2g': True, '5g': False}, body
        # Missing counters and isolation are unknown, never zero.
        data = json.loads(fixture.read_text())
        del data['users'], data['isolate']
        fixture.write_text(json.dumps(data))
        guest = request('wifi.guest.status')[1]['result']
        assert guest['management_access'] is None, guest
        assert all(item['clients'] is None for item in guest['guests']), guest
        assert request('wifi.guest.set', dict(management_access=True))[0] == 502
        print('Wi-Fi status fields, steering mapping, no-op, validation, auth, failure/readback and credential redaction PASS')
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=8)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
