#!/usr/bin/env python3
"""Local App forwarding configuration: no SMS or external delivery is performed."""
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
        try:
            parsed = json.loads(body) if body else None
        except ValueError:
            parsed = body.decode(errors='replace')
        return response.status, parsed, response.getheader('Cache-Control')
    finally:
        conn.close()


with tempfile.TemporaryDirectory(prefix='datad-forward-app-') as tmp:
    folder = Path(tmp)
    token = 'forward-test-not-a-device-credential'
    token_file = folder/'auth.token'
    token_file.write_text(token)
    token_file.chmod(0o600)
    local, lan = free_port(), free_port()
    env = dict(os.environ, ZWRT_DATAD_DIR=tmp, ZWRT_DATAD_UBUS_BIN='/usr/bin/false',
               ZWRT_DATAD_UCI_BIN='/usr/bin/false', ZWRT_DATAD_OTA_DISABLE_AUTO='1')
    args = [str(Path(sys.argv[1]).resolve()), '-p', str(local), '--lan-bind', '127.0.0.1',
            '--lan-port', str(lan), '--auth-token-file', str(token_file)]

    def start():
        child = subprocess.Popen(args, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(100):
            try:
                if call(lan, '/sms/forward/status', token)[0] == 200:
                    return child
            except OSError:
                pass
            assert child.poll() is None
            time.sleep(.05)
        child.terminate()
        child.wait(timeout=10)
        raise AssertionError('Server not ready')

    proc = start()
    try:
        patch = {'enabled': False, 'method': 'smtp', 'nickname': 'test device',
                 'webhook_url': 'https://example.com/hook',
                 'dingtalk_webhook': 'https://oapi.dingtalk.com/robot/send?access_token=fixture',
                 'dingtalk_secret': 'fixture-signing-secret',
                 'smtp': {'host': 'smtp.example.com', 'port': 465,
                          'username': 'sender@example.com', 'password': 'fixture-mail-secret',
                          'to': 'recipient@example.com'},
                 'blacklist_phone': ['10086'], 'blacklist_keywords': ['advertisement']}
        for port in (local, lan):
            for credential in (None, 'wrong'):
                for path, data in [('config', None), ('status', None), ('config', patch), ('test', {})]:
                    assert call(port, '/sms/forward/'+path, credential, data)[0] == 401
            assert call(port, '/sms/forward/config?access_token='+token)[0] == 401
        assert not (folder/'sms-forward.json').exists()
        assert call(lan, '/sms/forward/config', token, patch)[0] == 200
        code, view, cache = call(lan, '/sms/forward/config', token)
        assert code == 200 and cache == 'no-store'
        assert view['config']['smtp']['password_configured'] is True
        assert view['config']['dingtalk_secret_configured'] is True
        assert view['config']['blacklist_phone'] == ['10086']
        assert not any(secret in json.dumps(view) for secret in ['fixture-signing-secret', 'fixture-mail-secret'])
        assert 'seen' not in view['config']
        status = call(lan, '/sms/forward/status', token)[1]
        assert 'webhook_url' not in status and 'smtp' not in status
        assert 'sms_forward' not in call(lan, '/state', token)[1]
        assert call(lan, '/sms/forward/config', token, {'enabled': False, 'method': 'smtp', 'nickname': 'renamed'})[0] == 200
        config_path = folder/'sms-forward.json'
        stored = json.loads(config_path.read_text())
        assert stored['smtp']['password'] == 'fixture-mail-secret'
        assert stored['dingtalk_secret'] == 'fixture-signing-secret'
        original = config_path.read_bytes()
        for invalid in ({'enabled': False, 'method': 'shell'},
                        {'enabled': False, 'method': 'smtp', 'seen': []},
                        {'enabled': False, 'method': 'smtp', 'power_forward_enabled': True},
                        {'enabled': False, 'method': 'smtp', 'blacklist_phone': ['bad']},
                        {'enabled': False, 'method': 'smtp', 'webhook_url': 'http://127.0.0.1/'},
                        {'enabled': False, 'method': 'smtp', 'smtp': dict(patch['smtp'], port=25)}):
            assert call(lan, '/sms/forward/config', token, invalid)[0] in (400, 422)
            assert config_path.read_bytes() == original
        assert call(lan, '/sms/forward/test', token, {'to': 'unexpected'})[0] == 400
        assert config_path.stat().st_mode & 0o777 == 0o600
        proc.terminate()
        proc.wait(timeout=10)
        proc = start()
        restored = call(lan, '/sms/forward/config', token)[1]['config']
        assert restored['nickname'] == 'renamed' and restored['smtp']['password_configured']
        assert call(lan, '/sms/forward/config', token, {'enabled': False, 'method': 'smtp', 'dingtalk_secret': ''})[0] == 200
        assert not call(lan, '/sms/forward/config', token)[1]['config']['dingtalk_secret_configured']
        print('App forwarding: auth, private config, redaction, secret retention, validation and persistence passed')
    finally:
        proc.terminate()
        proc.wait(timeout=10)
