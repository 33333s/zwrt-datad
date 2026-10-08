#!/usr/bin/env python3
"""Authenticated App scheduling CRUD; future-only fixtures, no device actions."""
import datetime
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
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def call(port, path, token=None, data=None):
    conn = http.client.HTTPConnection('127.0.0.1', port, timeout=10)
    try:
        headers = {'Content-Type': 'application/json'}
        if token:
            headers['Authorization'] = 'Bearer ' + token
        conn.request('GET' if data is None else 'POST', path,
                     body=None if data is None else json.dumps(data), headers=headers)
        reply = conn.getresponse()
        body = reply.read()
        try:
            parsed = json.loads(body) if body else {}
        except json.JSONDecodeError:
            parsed = body.decode('utf-8', errors='replace')
        return reply.status, parsed, reply.getheader('Cache-Control')
    finally:
        conn.close()


with tempfile.TemporaryDirectory(prefix='datad-tasks-app-') as tmp:
    folder = Path(tmp)
    token = 'task-fixture-only-token'
    auth = folder / 'auth.token'
    auth.write_text(token)
    auth.chmod(0o600)
    local, lan = port(), port()
    env = dict(os.environ, ZWRT_DATAD_DIR=tmp, ZWRT_DATAD_UBUS_BIN='/usr/bin/false',
               ZWRT_DATAD_UCI_BIN='/usr/bin/false', ZWRT_DATAD_OTA_DISABLE_AUTO='1')
    args = [str(Path(sys.argv[1]).resolve()), '-p', str(local), '--lan-bind', '127.0.0.1',
            '--lan-port', str(lan), '--auth-token-file', str(auth)]

    def start():
        child = subprocess.Popen(args, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(120):
            try:
                if call(lan, '/tasks', token)[0] == 200:
                    return child
            except OSError:
                pass
            if child.poll() is not None:
                raise AssertionError('daemon exited before readiness')
            time.sleep(.1)
        child.terminate()
        child.wait(timeout=10)
        raise AssertionError('daemon not ready')

    child = start()
    try:
        code, initial, cache = call(lan, '/tasks', token)
        assert code == 200 and cache == 'no-store' and initial['tasks'] == []
        assert initial['max_tasks'] == 16
        assert 'sms.send_scheduled' in initial['actions'] and 'shell' not in initial['actions']
        # No guessed dual-SIM capability on an unknown device.
        assert 'sim.set_slot' not in initial['actions']
        future = (datetime.datetime.strptime(initial['device_time'], '%Y-%m-%d %H:%M') + datetime.timedelta(minutes=5)).strftime('%H:%M')
        task = {'id': 'fixture', 'time': future, 'repeat_daily': False,
                'action': 'sms.send_scheduled', 'params': {'number': '10086', 'text': 'fixture-private-text'}}
        for listener in (local, lan):
            for credential in (None, 'wrong'):
                for path, data in [('/tasks', None), ('/tasks', task), ('/tasks/remove', {'id': 'fixture'})]:
                    assert call(listener, path, credential, data)[0] == 401
            assert call(listener, '/tasks?access_token=' + token)[0] == 401
        assert not (folder / 'scheduled-tasks.json').exists()
        code, saved, cache = call(lan, '/tasks', token, task)
        assert code == 200 and cache == 'no-store'
        assert saved['tasks'][0]['params']['text'] == 'fixture-private-text'
        stored_path = folder / 'scheduled-tasks.json'
        assert stored_path.stat().st_mode & 0o777 == 0o600
        assert 'scheduled_tasks' not in call(lan, '/state', token)[1]
        original = stored_path.read_bytes()
        for bad in [dict(task, time='24:00'), dict(task, action='shell'),
                    dict(task, params={'number': '10086', 'text': 'x' * 281}),
                    dict(task, unexpected=True), dict(task, action='sim.set_slot', params={'slot': 2})]:
            assert call(lan, '/tasks', token, bad)[0] in (400, 422)
            assert stored_path.read_bytes() == original
        task.update(action='device.reboot', params={}, repeat_daily=True)
        assert call(local, '/tasks', token, task)[0] == 200
        assert len(call(local, '/tasks', token)[1]['tasks']) == 1
        child.terminate()
        child.wait(timeout=10)
        child = start()
        restored = call(lan, '/tasks', token)[1]['tasks']
        assert restored[0]['repeat_daily'] and restored[0]['last_result'] == ''
        for i in range(15):
            assert call(lan, '/tasks', token, dict(task, id=f'task-{i}'))[0] == 200
        assert call(lan, '/tasks', token, dict(task, id='overflow'))[:2] == (400, {'error': 'task_limit'})
        assert call(lan, '/tasks/remove', token, {'id': 'fixture', 'extra': True})[0] == 422
        assert call(lan, '/tasks/remove', token, {'id': 'fixture'})[0] == 200
        assert len(call(lan, '/tasks', token)[1]['tasks']) == 15
        assert call(lan, '/tasks/remove', token, {'id': 'fixture'})[:2] == (400, {'error': 'task_not_found'})
        print('App tasks: authentication, capabilities, CRUD, validation, private storage and restart passed')
    finally:
        child.terminate()
        child.wait(timeout=10)
