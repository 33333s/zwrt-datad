#!/usr/bin/env python3
"""Private history/LAN API, generic firmware mapping and missing-data semantics."""
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
    c = http.client.HTTPConnection('127.0.0.1', port, timeout=20)
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


with tempfile.TemporaryDirectory(prefix='datad-lan-history-') as tmp:
    folder = Path(tmp)
    auth = folder / 'token'
    auth.write_text('fixture-token')
    auth.chmod(0o600)
    # Keep fixture sampling in the future to avoid a current-day synthetic row.
    (folder / 'traffic-history.json').write_text(json.dumps({'schema': 1, 'last_sample_at': 4102444800,
        'days': {'2026-01-01': {'bytes': 120, 'last_counter': 120}, '2026-01-03': {'bytes': 400, 'last_counter': 400}}}))
    root = Path(__file__).resolve().parent
    log = folder / 'calls'
    local, lan = port(), port()
    env = dict(os.environ, ZWRT_DATAD_DIR=tmp, ZWRT_DATAD_OTA_DISABLE_AUTO='1',
        ZWRT_DATAD_UBUS_BIN=str(root / 'mock_ubus.sh'), ZWRT_DATAD_UCI_BIN=str(root / 'mock_uci.sh'), MOCK_CALL_LOG=str(log))
    child = subprocess.Popen([str(Path(sys.argv[1]).resolve()), '-p', str(local), '--lan-bind', '127.0.0.1',
        '--lan-port', str(lan), '--auth-token-file', str(auth)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(200):
            try:
                if call(lan, '/lan', 'fixture-token')[0] == 200:
                    break
            except OSError:
                pass
            time.sleep(.1)
        else:
            raise AssertionError('daemon not ready')
        for listener in (local, lan):
            for path, data in [('/traffic/history', None), ('/lan', None), ('/lan', {}), ('/lan/mtu', {'mtu': 1400})]:
                assert call(listener, path, data=data)[0] == 401
                assert call(listener, path, 'wrong', data)[0] == 401
            assert call(listener, '/lan?access_token=fixture-token')[0] == 401
            code, body, cache = call(listener, '/traffic/history', 'fixture-token')
            assert code == 200 and cache == 'no-store' and len(body['days']) == 2, body
            assert body['days'][1] == {'date': '2026-01-03', 'bytes': 400}
            assert body['sample_interval_seconds'] == 300 and body['oldest_date'] == '2026-01-01'
            assert call(listener, '/traffic/history?start=2026-01-02&end=2026-01-03', 'fixture-token')[1]['days'] == [body['days'][1]]
            assert call(listener, '/traffic/history?start=2026-01-02&end=2026-01-02', 'fixture-token')[1]['days'] == []
            for query in ('start=2026-02-30', 'start=2026-01-02&end=2026-01-01'):
                assert call(listener, '/traffic/history?' + query, 'fixture-token')[0] == 400
        code, settings, cache = call(lan, '/lan', 'fixture-token')
        assert code == 200 and cache == 'no-store' and settings['supported'] and settings['address_writable'], settings
        assert settings['lease_seconds'] == 43200, settings
        assert call(lan, '/lan', 'fixture-token', {})[1]['changed'] is False
        assert call(lan, '/lan', 'fixture-token', {'netmask':'255.0.255.0'})[0] == 400
        assert call(lan, '/lan/mtu', 'fixture-token', {'mtu':575})[0] == 400
        code, result, _ = call(lan, '/lan/mtu', 'fixture-token', {'mtu':1400})
        # Mock accepts writes without changing state: never claim verified.
        assert code == 200 and not result['verified'], result
        assert 'router_set_wan_mtu' in log.read_text() and '"wan_mtu":"1400"' in log.read_text()
        print('App LAN/history: authentication, dates, gaps, generic mapping and readback passed')
    finally:
        child.terminate()
        child.wait(timeout=10)
