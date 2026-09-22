#!/usr/bin/env python3
"""Read-only USB fixture: CLI, authenticated HTTP, state and SSE refresh."""
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
with tempfile.TemporaryDirectory(prefix='datad-usb-test-') as directory:
    root = Path(directory)
    udc, host = root / 'udc', root / 'usb'
    controller = udc / 'controller0'
    controller.mkdir(parents=True)
    host.mkdir()
    def write(path, text):
        path.parent.mkdir(parents=True, exist_ok=True)
        temp = path.with_suffix('.new')
        temp.write_text(text)
        temp.replace(path)
    write(controller / 'state', 'not attached\n')
    write(controller / 'current_speed', 'super-speed\n')
    write(controller / 'maximum_speed', 'super-speed-plus\n')
    for name, speed in [('usb1', '480'), ('usb2', '5000'), ('1-1', '480'), ('1-1:1.0', '480'), ('1-2.3', '10000')]:
        write(host / name / 'speed', speed)
    write(host / '1-2.3' / 'rx_lanes', '1')
    write(host / '1-2.3' / 'tx_lanes', '1')
    env = dict(os.environ, ZWRT_DATAD_USB_UDC_ROOT=str(udc), ZWRT_DATAD_USB_HOST_ROOT=str(host),
               ZWRT_DATAD_UBUS_BIN='/usr/bin/true', ZWRT_DATAD_UCI_BIN='/usr/bin/false',
               ZWRT_DATAD_OTA_DISABLE_AUTO='1', ZWRT_DATAD_COOLING_CONFIG=str(root / 'cooling'),
               ZWRT_DATAD_WIFI_CONFIG=str(root / 'wifi'))
    def cli():
        return json.loads(subprocess.check_output([str(binary), '--usb-status', '--data-dir', str(root / 'unused')], env=env))
    value = cli()
    assert not (root / 'unused').exists(), 'CLI must not initialize services/configuration'
    assert value['gadget_available'] and value['host_available'] and not value['truncated']
    current = value['controllers'][0]
    assert current['state'] == 'not-attached' and current['connected'] is False
    assert current['speed'] is None and current['speed_mbps'] is None
    assert current['maximum_speed'] == 'super-speed-plus'
    assert [d['name'] for d in value['devices']] == ['1-1', '1-2.3'], 'exclude hubs and interfaces'
    assert [d['speed_mbps'] for d in value['devices']] == [480, 10000]
    assert value['devices'][1]['rx_lanes'] == 1
    write(controller / 'state', 'configured')
    for speed, expected in [('low-speed', 1.5), ('full-speed', 12), ('high-speed', 480), ('super-speed', 5000), ('super-speed-plus', None), ('UNKNOWN', None)]:
        write(controller / 'current_speed', speed)
        current = cli()['controllers'][0]
        assert current['connected'] is True and current['speed_mbps'] == expected
    for malformed in ['NaN', '-1', '0', '480 Mbps', '9' * 129]:
        write(host / '1-1' / 'speed', malformed)
        assert cli()['devices'][0]['speed_mbps'] is None
    write(host / '1-1' / 'speed', '480')
    write(controller / 'current_speed', 'high-speed')
    token = root / 'token'
    token.write_text('fixture-usb-token')
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
    process = subprocess.Popen([str(binary), '--port', str(port), '--auth-token-file', str(token),
                                '--data-dir', str(root / 'data')], env=env,
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    def request(path, body=None, auth=True):
        headers = {'Content-Type': 'application/json'}
        if auth: headers['Authorization'] = 'Bearer fixture-usb-token'
        req = urllib.request.Request(f'http://127.0.0.1:{port}{path}', headers=headers,
                                     data=None if body is None else json.dumps(body).encode())
        try: return urllib.request.urlopen(req, timeout=10)
        except urllib.error.HTTPError as error: return error
    def data(path, body=None):
        with request(path, body) as reply:
            assert reply.status == 200, reply.read()
            return json.load(reply)
    try:
        for _ in range(100):
            try:
                with request('/usb/status') as reply:
                    if reply.status == 200: break
            except (OSError, urllib.error.URLError): pass
            time.sleep(.1)
        else: raise AssertionError('server startup')
        with request('/usb/status', auth=False) as reply: assert reply.status == 401
        with request('/usb/status', {}) as reply: assert reply.status == 405
        value = data('/usb/status')
        assert value == data('/state')['usb']
        assert value == data('/control', {'action': 'usb.status', 'params': {}})['result']['link']
        assert value['controllers'][0]['speed_mbps'] == 480
        with request('/events') as events:
            write(controller / 'current_speed', 'super-speed')
            for _ in range(60):
                line = events.readline().decode()
                if line.startswith('data:') and json.loads(line[5:])['usb']['controllers'][0]['speed_mbps'] == 5000: break
            else: raise AssertionError('SSE USB speed not refreshed')
        assert data('/usb/status')['controllers'][0]['speed_mbps'] == 5000
    finally:
        process.terminate()
        try: process.wait(timeout=10)
        except subprocess.TimeoutExpired: process.kill(); process.wait()
    for i in range(100): write(host / f'3-{i+1}' / 'speed', '12')
    value = cli()
    assert value['truncated'] and len(value['devices']) == 64
    env['ZWRT_DATAD_USB_HOST_ROOT'] = str(root / 'missing-host')
    env['ZWRT_DATAD_USB_UDC_ROOT'] = str(root / 'missing-udc')
    value = cli()
    assert not value['host_available'] and not value['gadget_available']
    assert value['devices'] == value['controllers'] == []
    print('USB: negotiated vs maximum speed, disconnected/unknown, hub filtering, bounds, CLI, HTTP auth, control/state/SSE PASS')
