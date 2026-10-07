#!/usr/bin/env python3
"""Authenticated App NMS configuration, no modem or real cloud access."""
import http.client
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def call(port, path, token=None, data=None):
    headers = {'Content-Type': 'application/json'}
    if token:
        headers['Authorization'] = 'Bearer ' + token
    conn = http.client.HTTPConnection('127.0.0.1', port, timeout=5)
    try:
        conn.request('GET' if data is None else 'POST', path,
                     body=None if data is None else json.dumps(data), headers=headers)
        response = conn.getresponse()
        body = response.read()
        if path.startswith('/cloud/app/') and response.status != 401:
            assert response.getheader('Cache-Control') == 'no-store'
        try:
            parsed = json.loads(body) if body else None
        except ValueError:
            parsed = body.decode(errors='replace')
        return response.status, parsed
    finally:
        conn.close()


with tempfile.TemporaryDirectory(prefix='datad-cloud-app-') as tmp:
    folder = Path(tmp)
    token = 'app-test-not-a-device-credential'
    token_file = folder/'auth.token'
    token_file.write_text(token)
    token_file.chmod(0o600)
    local, lan = free_port(), free_port()
    env = dict(os.environ, ZWRT_DATAD_DIR=tmp, ZWRT_DATAD_UBUS_BIN='/usr/bin/false',
               ZWRT_DATAD_UCI_BIN='/usr/bin/false', ZWRT_DATAD_OTA_DISABLE_AUTO='1')
    proc = subprocess.Popen([str(Path(sys.argv[1]).resolve()), '-p', str(local), '--lan-bind', '127.0.0.1',
                             '--lan-port', str(lan), '--auth-token-file', str(token_file)],
                            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(100):
            try:
                if call(lan, '/cloud/app/status', token)[0] == 200:
                    break
            except OSError:
                pass
            assert proc.poll() is None
            time.sleep(.05)
        else:
            raise AssertionError('Server not ready')
        for port in (local, lan):
            for credential in (None, 'wrong'):
                for path, data in [('config', None), ('status', None), ('config', {}),
                                   ('quick-connect', {'username': 'enr_'+'a'*32, 'password': 'fixture'})]:
                    assert call(port, '/cloud/app/'+path, credential, data)[0] == 401
            assert call(port, '/cloud/app/config?access_token='+token)[0] == 401
            malformed = http.client.HTTPConnection('127.0.0.1', port, timeout=5)
            malformed.request('POST', '/cloud/app/config', body='{', headers={'Content-Type':'application/json'})
            response = malformed.getresponse()
            assert response.status == 401, 'authenticate before JSON extraction on both listeners'
            response.read()
            malformed.close()
        # Existing privileged API stays loopback-only, even with a valid LAN token.
        assert call(lan, '/cloud/config', token)[0] == 404
        _, baseline = call(local, '/cloud/config')
        config = baseline['config']
        config.update(remote_webshell_enabled=True, password='fixture-secret')
        assert call(local, '/cloud/config', data=config)[0] == 200
        code, saved = call(lan, '/cloud/app/config', token, {
            'report_interval_seconds': 60, 'password': '',
            'web_services': [{'name': 'Web', 'port': 8080, 'kind': 'web'}]})
        assert code == 200 and saved['password_configured']
        assert 'fixture-secret' not in json.dumps(saved)
        assert saved['config']['remote_webshell_enabled'] is True
        assert [s for s in saved['config']['services'] if s['kind'] == 'terminal'] == [s for s in config['services'] if s['kind'] == 'terminal']
        original = (folder/'cloud.json').read_bytes()
        assert call(lan, '/cloud/app/config', token, {'model':'x'*70000})[0] == 413
        assert (folder/'cloud.json').read_bytes() == original
        for invalid in ({'remote_webshell_enabled': False}, {'services': []}, {'report_interval_seconds': 9},
                        {'web_services': [{'name': 'Shell', 'port': 22, 'kind': 'terminal'}]}):
            assert call(lan, '/cloud/app/config', token, invalid)[0] in (400, 422)
            assert (folder/'cloud.json').read_bytes() == original
        assert (folder/'cloud.json').stat().st_mode & 0o777 == 0o600
        assert call(lan, '/cloud/app/config', token, {'clear_password': True})[1]['password_configured'] is False
        print('App NMS: auth, redaction, password retention, terminal preservation and validation passed')
    finally:
        proc.terminate()
        proc.wait(timeout=10)
