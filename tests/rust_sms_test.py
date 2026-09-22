#!/usr/bin/env python3
"""Authenticated SMS HTTP regression with encrypted, stateful local UBus."""
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
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric import rsa

binary = Path(sys.argv[1]).resolve()
mock = Path(__file__).with_name('mock_sms_ubus.py').resolve()
with tempfile.TemporaryDirectory(prefix='datad-sms-test-') as name:
    root = Path(name)
    private = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    (root / 'private.pem').write_bytes(private.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()))
    (root / 'public.pem').write_bytes(private.public_key().public_bytes(serialization.Encoding.PEM, serialization.PublicFormat.SubjectPublicKeyInfo))
    (root / 'token').write_text('fixture-sms-token')
    config = {'count': 100, 'replace_key_once': True}
    def setup(**changes):
        config.update(changes)
        tmp = root / 'config.new'
        tmp.write_text(json.dumps(config))
        tmp.replace(root / 'config.json')
    setup()
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
    env = dict(os.environ, SMS_FIXTURE=str(root), ZWRT_DATAD_UBUS_BIN=str(mock),
               ZWRT_DATAD_UCI_BIN='/usr/bin/false', ZWRT_DATAD_OTA_DISABLE_AUTO='1',
               ZWRT_DATAD_COOLING_CONFIG=str(root / 'cooling'))
    process = subprocess.Popen([str(binary), '--port', str(port), '--data-dir', str(root / 'data'),
                                '--auth-token-file', str(root / 'token')], env=env,
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    def request(path='/state', body=None, auth=True):
        headers = {'Content-Type': 'application/json'}
        if auth:
            headers['Authorization'] = 'Bearer fixture-sms-token'
        req = urllib.request.Request(f'http://127.0.0.1:{port}{path}', headers=headers,
                                     data=None if body is None else json.dumps(body).encode())
        try:
            with urllib.request.urlopen(req, timeout=20) as response:
                return response.status, json.load(response)
        except urllib.error.HTTPError as error:
            return error.code, json.loads(error.read())
    def wait_for(predicate, timeout=20):
        end = time.monotonic() + timeout
        last = None
        while time.monotonic() < end:
            try:
                code, state = request()
                last = state.get('sms')
                if code == 200 and last is not None and predicate(last):
                    return last
            except (OSError, urllib.error.URLError):
                pass
            time.sleep(.2)
        raise AssertionError(f'SMS state timeout: {last and {k:v for k,v in last.items() if k != "list"}}')
    def action(which, params):
        return request('/control', {'action': 'sms.' + which, 'params': params})
    def calls(method):
        return [json.loads(line) for line in (root / 'calls.log').read_text().splitlines() if json.loads(line)[1] == method]
    def sends():
        return len((root / 'sends.log').read_text().splitlines()) if (root / 'sends.log').exists() else 0
    try:
        sms = wait_for(lambda s: len(s['list']) == 100 and not s['stale'], 45)
        assert sms['list'][0]['num'] == '10086' and sms['list'][-1]['text'] == '测试100'
        assert not sms['truncated']
        assert len(calls('web_http_enstr_set')) == 2, 'rekey once after vendor replaced session'
        assert request(auth=False)[0] == 401
        assert any(row[2] == 1 for row in calls('zte_libwms_get_sms_data')), 'second page read'
        read_count = len(calls('zte_libwms_get_sms_data'))
        for _ in range(5):
            assert len(request()[1]['sms']['list']) == 100
            time.sleep(.2)
        assert len(calls('zte_libwms_get_sms_data')) == read_count, 'unchanged state reused cache'

        setup(count=101, bad_ciphertext=True)
        sms = wait_for(lambda s: s['stale'])
        assert len(sms['list']) == 100 and 'error' in sms, 'keep last good data on decrypt failure'
        setup(bad_ciphertext=False)
        wait_for(lambda s: len(s['list']) == 101 and not s['stale'])
        setup(count=102, list_failure=True)
        sms = wait_for(lambda s: s['stale'])
        assert len(sms['list']) == 101
        setup(list_failure=False)
        wait_for(lambda s: len(s['list']) == 102 and not s['stale'])

        # The only sends in this test terminate in the local fixture, where
        # decrypted number/body are asserted and no network is contacted.
        params = dict(sender='host', number='10086', message_hex='6D4B8BD5', sms_time='26;09;22;12;00;00;+;0')
        assert action('send_raw', params)[0] == 200
        assert sends() == 1
        setup(send_failure=True)
        assert action('send_raw', params)[0] == 502
        assert sends() == 2, 'failed sends must not be retried'
        setup(reject_registration=True)
        assert action('send_raw', params)[0] == 502
        assert sends() == 2, 'registration failure must prevent send'
        setup(reject_registration=False, send_failure=False, count=36)
        assert action('mark_read', {'ids': '1', 'tag': 0})[0] == 200
        wait_for(lambda s: len(s['list']) == 36 and not s['stale'])
        setup(count=600)
        wait_for(lambda s: len(s['list']) == 256 and s['truncated'] and not s['stale'])
        setup(count=700, repeat_page=True)
        wait_for(lambda s: len(s['list']) == 64 and s['truncated'] and not s['stale'])
        setup(count=1, repeat_page=False, plaintext=True, reject_registration=True)
        # Force a crypto reset via a rejected fixture send, then demonstrate
        # that plaintext models remain readable without a vendor RSA session.
        assert action('send_raw', params)[0] == 502
        wait_for(lambda s: len(s['list']) == 1 and s['list'][0]['text'] == '测试1' and not s['stale'])
        print('SMS: flat PEM, encrypted pagination, rekey, stale cache, bounds, auth, plaintext, fixture send and no-retry PASS')
    finally:
        process.terminate()
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
