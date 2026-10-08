#!/usr/bin/env python3
"""Private activity/recovery API. No real SMS, network probes or device actions."""
import http.client
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time


def port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


def call(port, path, token=None, data=None):
    c = http.client.HTTPConnection('127.0.0.1', port, timeout=15)
    try:
        h = {'Content-Type': 'application/json'}
        if token:
            h['Authorization'] = 'Bearer ' + token
        c.request('GET' if data is None else 'POST', path, None if data is None else json.dumps(data), h)
        r = c.getresponse()
        raw = r.read()
        try:
            body = json.loads(raw) if raw else {}
        except ValueError:
            body = raw.decode()
        return r.status, body, r.getheader('Cache-Control')
    finally:
        c.close()


with tempfile.TemporaryDirectory(prefix='datad-automation-') as tmp:
    folder = Path(tmp)
    token = 'automation-fixture-token'
    auth = folder / 'token'
    auth.write_text(token)
    auth.chmod(0o600)
    local, lan = port(), port()
    env = dict(os.environ, ZWRT_DATAD_DIR=tmp, ZWRT_DATAD_OTA_DISABLE_AUTO='1',
        ZWRT_DATAD_UBUS_BIN='/usr/bin/false', ZWRT_DATAD_UCI_BIN='/usr/bin/false')
    args = [str(Path(sys.argv[1]).resolve()), '-p', str(local), '--lan-bind', '127.0.0.1',
        '--lan-port', str(lan), '--auth-token-file', str(auth)]

    def start():
        p = subprocess.Popen(args, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(160):
            try:
                if call(lan, '/network/recovery', token)[0] == 200:
                    return p
            except OSError:
                pass
            assert p.poll() is None
            time.sleep(.05)
        p.terminate()
        p.wait(timeout=10)
        raise AssertionError('daemon not ready')

    child = start()
    try:
        for listener in (local, lan):
            for path, data in [('/activity', None), ('/activity/clear', {}), ('/network/recovery', None),
                               ('/network/recovery', {'enabled': True}), ('/network/recovery/resume', {}), ('/network/recovery/check', {})]:
                assert call(listener, path, data=data)[0] == 401
                assert call(listener, path, 'wrong', data)[0] == 401
            assert call(listener, '/network/recovery?access_token=' + token)[0] == 401
        code, initial, cache = call(lan, '/network/recovery', token)
        assert code == 200 and cache == 'no-store' and not initial['config']['enabled']
        assert initial['status'] == 'disabled' and not initial['config']['reboot_after_failures']
        assert initial['config']['probe_urls'] == ['https://www.baidu.com/', 'https://www.qq.com/',
            'https://cp.cloudflare.com/generate_204'], initial['config']
        assert initial['probe_hosts'] == ['www.baidu.com', 'www.qq.com', 'cp.cloudflare.com'], initial
        for bad in ({'interval_seconds': 1}, {'failure_threshold': 0}, {'max_redials': 6},
                    {'cooldown_seconds': 0}, {'reboot_after_failures': 'true'}, {'unexpected': True},
                    {'probe_urls': []}, {'probe_urls': 'https://example.org/'},
                    {'probe_urls': ['http://example.org/']}, {'probe_urls': ['https://192.168.0.1/']},
                    {'probe_urls': ['https://localhost/']}, {'probe_urls': ['https://example.org:8443/']},
                    {'probe_urls': ['https://u:p@example.org/']}, {'probe_urls': ['https://example.org/?x=1']},
                    {'probe_urls': ['https://example.org/', 'https://EXAMPLE.org/']},
                    {'probe_urls': ['https://a.example.org/', 'https://b.example.org/', 'https://c.example.org/',
                                    'https://d.example.org/', 'https://e.example.org/']},
                    {'probe_hosts': ['example.org']}):
            assert call(lan, '/network/recovery', token, bad)[0] in (400, 422)
        assert not (folder / 'network-recovery.json').exists()
        enabled = dict(initial['config'], enabled=True)
        assert call(lan, '/network/recovery', token, enabled)[0] == 200
        custom = dict(enabled, probe_urls=['https://example.org/', 'https://cp.cloudflare.com/generate_204'])
        code, saved, _ = call(lan, '/network/recovery', token, custom)
        assert code == 200 and saved['config']['probe_urls'] == custom['probe_urls'], saved
        assert saved['probe_hosts'] == ['example.org', 'cp.cloudflare.com'], saved
        assert call(local, '/network/recovery', token)[1]['config']['probe_urls'] == custom['probe_urls']
        assert call(lan, '/network/recovery', token, enabled)[0] == 200
        assert call(local, '/network/recovery', token)[1]['config']['probe_urls'] == enabled['probe_urls']
        assert call(local, '/network/recovery', token)[1]['status'] == 'waiting'
        # Underlying stub fails, but manual intent must still pause recovery.
        assert call(lan, '/control', token, {'action': 'cellular.disconnect', 'params': {}})[0] == 502
        assert call(lan, '/network/recovery', token)[1]['manual_paused']
        assert (folder / 'network-recovery.json').stat().st_mode & 0o777 == 0o600
        child.terminate()
        child.wait(timeout=10)
        child = start()
        assert call(lan, '/network/recovery', token)[1]['status'] == 'manual_pause'
        assert call(lan, '/network/recovery/resume', token, {})[1]['manual_paused'] is False
        assert call(lan, '/network/recovery', token, initial['config'])[0] == 200
        assert call(lan, '/network/recovery', token)[1]['status'] == 'disabled'
        # Blank default webhook cannot deliver anything; record only the error code.
        assert call(lan, '/sms/forward/test', token, {})[0] == 400
        view = call(lan, '/activity?category=notification&failed=true', token)[1]
        assert view['entries'][0]['action'] == 'test' and view['entries'][0]['reason'] == 'delivery_failed', view
        activity = call(lan, '/activity', token)
        assert activity[0] == 200 and activity[2] == 'no-store'
        assert len(activity[1]['entries']) >= 4
        assert all(set(e) == {'id', 'timestamp', 'device_time', 'category', 'action', 'result', 'reason', 'task'} for e in activity[1]['entries'])
        assert token not in json.dumps(activity[1]) and 'params' not in json.dumps(activity[1])
        assert call(lan, '/activity?category=unknown', token)[0] == 400
        first = activity[1]['entries'][0]['id']
        assert all(e['id'] < first for e in call(lan, '/activity?before=' + str(first), token)[1]['entries'])
        assert call(lan, '/activity/clear', token, {})[0] == 200
        assert call(lan, '/activity', token)[1]['entries'] == []
        child.terminate()
        child.wait(timeout=10)
        child = start()
        assert call(lan, '/activity', token)[1]['entries'] == []
        assert call(lan, '/network/recovery', token)[1]['config'] == initial['config']
        print('Automation: private auth, validation, probe URL list, persistent intent, notification errors, filtering and clearing passed')
    finally:
        child.terminate()
        child.wait(timeout=10)
