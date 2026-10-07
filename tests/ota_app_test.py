#!/usr/bin/env python3
"""Authenticated App OTA API. Isolated fixture; no external downloads or device actions."""
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
                if call(lan, '/ota/app', token)[0] == 200:
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
            for path, data in [('/ota/app', None),('/ota/app/config',{}),('/ota/app/check',{}),('/ota/app/install',{'candidate_id':'0'*64})]:
                assert call(listener,path,data=data)[0] == 401
                assert call(listener,path,'wrong',data)[0] == 401
            assert call(listener,'/ota/app?access_token='+token)[0] == 401
        code, state, cache = call(lan,'/ota/app',token)
        assert code == 200 and cache == 'no-store' and not state['busy']
        assert state['platform'] == 'update.json' and state['candidate'] is None
        config = {'enabled':False,'servers':['https://updates.example.test/datad'],'sources':['custom']}
        assert call(lan,'/ota/app/config',token,config)[0] == 200
        assert call(local,'/ota/app',token)[1]['config'] == config
        assert (folder/'ota.json').stat().st_mode & 0o777 == 0o600
        for servers in (['http://example.test'],['https://u:p@example.test'],['https://example.test/?token=x']):
            assert call(lan,'/ota/app/config',token,dict(config,servers=servers))[0] == 400
        assert call(lan,'/ota/app/install',token,{'candidate_id':'0'*64})[0] == 409
        assert not (folder/'ota-run.sh').exists()
        # Empty selected source fails locally without any outbound network request.
        assert call(lan,'/ota/app/config',token,dict(config,servers=[]))[0] == 200
        assert call(lan,'/ota/app/check',token,{})[0] == 202
        for _ in range(50):
            state = call(lan,'/ota/app',token)[1]
            if not state['busy']: break
            time.sleep(.02)
        assert state['status']['state'] == 'error' and not state['status']['signature_verified']
        assert state['candidate'] is None
        child.terminate();child.wait(timeout=10)
        child = start()
        assert call(lan,'/ota/app',token)[1]['config']['enabled'] is False
        print('PASS: authenticated App OTA routes, persistence, invalid sources, pinned install rejection and asynchronous check')
    finally:
        child.terminate();child.wait(timeout=10)
