#!/usr/bin/env python3
"""App migration interaction/contract tests. All modem actions hit an in-memory fake.

Production React bundles and CSP are served unchanged below a reverse-proxy prefix.
No real credentials, device calls, shell commands, SMS or EFS writes occur.
"""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit, parse_qs
import base64
import copy
import hashlib
import json
import struct
import threading
import time
from playwright.sync_api import sync_playwright, expect

ROOT = Path(__file__).resolve().parents[1]
DIST = ROOT / 'web/dist'
PREFIX = '/web-test/device'
TOKEN = 'a' * 48
CSP = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; worker-src 'self'; img-src 'self' blob: data:; font-src 'self'; base-uri 'self'; form-action 'none'; frame-ancestors 'none'"
ASSETS = {route: (DIST / file, mime) for route, file, mime in json.loads((DIST / 'asset-manifest.json').read_text())}
trace, shell_input = [], []
signal_stress = threading.Event()
shell_closed = threading.Event()
state = {'ts': 1795158000, 'datad': {'version': '0.10.72'}, 'device': {'model_name': 'MU5120', 'market_name': 'U50 Pro'},
         'net': {'type': 'SA', 'net_select': 'Only_5G', 'nr_band': 'n78', 'nr_pci': 123, 'nr_channel': 633984, 'nr_bw': '100MHz', 'nr_rsrp': -85, 'nr_rsrq': -11, 'nr_snr': '20', 'nr_cell_id': 9007199254740993, 'band': 'B3', 'lte_pci': 10, 'lte_channel': 1650, 'lte_bw': '20MHz', 'lte_rsrp': -90, 'lte_snr': '15', 'lte_band': '1,3', 'sa_bands': '1,78', 'nsa_bands': '1,78', 'band_capabilities': {'complete': True, 'lte': [1, 3, 8], 'nr_sa': [1, 78], 'nr_nsa': [1, 78]}, 'nrca': '0,801,1,n1,427210,20MHz,0,-95,-12,10,-70;', 'nr_ca_dl_state': '1', 'nr_ca_ul_state': '1', 'wan_status': 'connected'},
         'sim': {'dual_sim': 2, 'current_slot': 1, 'iccid': 'fixture-not-a-real-sim'}, 'system': {'imei': 'fixture-not-a-real-imei', 'uptime': 10000, 'cpu_temp': 40, 'cpu_usage': 10, 'mem_used_pct': 30},
         'wlan': {'enabled': 1, 'band_steering_enabled': 0}, 'battery': {'percent': 70, 'charging': 2},
         'traffic': {'today_rx_bytes': 1048576, 'today_tx_bytes': 1000, 'month_rx_bytes': 2097152, 'month_tx_bytes': 1000, 'limit': {'enable': 0, 'value': '100_1024'}},
         'neighbor': {'cells': [{'rat': 'NR5G', 'pci': 125, 'arfcn': 633984, 'band': 78, 'rsrp_dbm': -95, 'sinr_db': 10}]},
         'sms': {'stale': False, 'list': [{'id': '9007199254740993', 'num': '10086', 'text': '测试收件正文', 'date': '2026-11-20 10:01:00', 'tag': 1}, {'id': 2, 'num': '10010', 'text': '失败发件记录', 'tag': 3}, {'id': 3, 'num': '10000', 'text': '草稿正文', 'tag': 4}]}}
controls = ['sim.set_slot', 'sms.send_raw', 'sms.delete', 'oem.access.status', 'oem.access.set']
apn = {'mode': {'apn_mode': 0}, 'enabled': {'profileId': 'automatic-1'}, 'writable': True, 'auth_modes': [0, 1, 2, 3], 'automatic': {'apnListArray': [{'profileId': 'automatic-1', 'profilename': '自动档案', 'wanapn': 'automatic', 'pdpType': 3}]}, 'manual': {'apnListArray': [{'profileId': 'manual-2', 'profilename': '手动档案', 'wanapn': 'internet', 'pdpType': 3, 'pppAuthMode': 0}]}}
wifi = {key: {'ssid': key, 'encryption': 'psk2', 'key': 'fixture-password', 'hidden': '0', 'disabled': '0', 'maxassoc': '32', 'channel': '0'} for key in ['main_2g', 'main_5g', 'guest_2g', 'guest_5g']}
lan = {'supported': True, 'address_writable': False, 'ip': '192.168.0.1', 'netmask': '255.255.255.0', 'dhcp_enabled': True, 'dhcp_start': '192.168.0.2', 'dhcp_end': '192.168.0.200', 'lease_seconds': 86400, 'mtu': 1500, 'mtu_min': 576, 'mtu_max': 1500}
tasks = {'supported': True, 'tasks': [], 'actions': ['device.reboot', 'network.set_mode', 'sim.set_slot', 'sms.send_scheduled', 'cell.lock_nr'], 'network_modes': ['WL_AND_5G', 'Only_LTE', 'Only_5G'], 'max_tasks': 16, 'device_time': '2026-11-20 10:00'}
recovery = {'config': {'enabled': False, 'interval_seconds': 60, 'failure_threshold': 3, 'cooldown_seconds': 300, 'max_redials': 3, 'reboot_after_failures': False}, 'status': 'disabled', 'consecutive_failures': 0, 'redials': 0, 'storage_ok': True}
forward = {'config': {'enabled': True, 'method': 'smtp', 'smtp': {'host': 'smtp.example.com', 'port': 465, 'username': 'sender@example.com', 'to': 'receiver@example.com', 'password_configured': True}, 'power_forward_enabled': True, 'nickname': ''}, 'status': {'supported': True, 'enabled': True, 'method': 'smtp', 'power_supported': True, 'power_forward_enabled': True, 'last_result': 'sent'}}
nms = {'password_configured': True, 'config': {'enabled': False, 'broker': 'wss://example.com/mqtt', 'platform_url': 'https://example.com', 'username': 'fixture-user', 'vendor': 'ZTE', 'model': 'MU5120', 'identity': 'fixture-device', 'services': [{'name': 'Web', 'port': 80, 'kind': 'web'}, {'name': 'Existing terminal', 'port': 8899, 'kind': 'terminal'}]}}
oem = {'supported': True, 'paused': False, 'remaining_seconds': None, 'restart_resumes': True}
ota = {'busy': False, 'config': {'enabled': False, 'servers': [], 'sources': ['netdisk', 'github']}, 'status': {'current_version': '0.10.72', 'state': 'idle', 'last_check_at': 0, 'signature_verified': False, 'wait_reasons': []}, 'platform': 'update-armv7.json'}


def signal_fixture(wire=False, mcs=17):
    serving = bytearray(29)
    struct.pack_into('<BHIIBBIHHBHB', serving, 0, 3, 10, 1650, 19650, 100, 100, 0x1234, 1, 3, 0, 460, 2)
    # Explicit offsets match LTE v3: band u32 at 19, MCC u16 at 23, MNC digits at 25.
    struct.pack_into('<I', serving, 19, 3); struct.pack_into('<H', serving, 23, 460); serving[25] = 2; struct.pack_into('<H', serving, 26, 11)
    nas = bytes([1, 0, 0, 0, 0x52, 1, 0xc1, 1, 9, 9, 8]) + b'internet' + bytes([5, 1, 10, 0, 0, 1, 0x5e, 2, 0xfe, 0xfe])
    pdsch = bytearray(44); pdsch[0] = 48; pdsch[1] = 1; pdsch[6] = 50; pdsch[8] = 2
    for index in range(2):
        off = 16 + index * 12; pdsch[off] = 128 if index == 0 else 0; pdsch[off+6] = mcs; pdsch[off+7] = 50; pdsch[off+8] = 6
    out = b''
    for index, (code, body) in enumerate([(0xb0c2, serving), (0xb0e2, nas), (0xb173, pdsch)]):
        packet = struct.pack('<HHQ', len(body)+12, code, 1000000+index) + body
        out += struct.pack('<I', len(packet)) + (struct.pack('<I', code) if wire else b'') + packet
    return out


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, data, status=200, headers=None):
        if not isinstance(data, bytes):
            data = json.dumps(data).encode()
            headers = {'Content-Type': 'application/json', **(headers or {})}
        self.send_response(status)
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        path = urlsplit(self.path).path.removeprefix(PREFIX)
        if path in ASSETS:
            file, mime = ASSETS[path]
            return self.reply(file.read_bytes(), headers={'Content-Type': mime, 'Content-Encoding': 'gzip', 'Content-Security-Policy': CSP})
        if path == '/webshell':
            return self.shell()
        if self.headers.get('Authorization') != 'Bearer ' + TOKEN:
            return self.reply({'ok': False}, 401)
        trace.append(('GET', path, None))
        if path == '/signal/stream' and signal_stress.is_set():
            self.send_response(200)
            self.send_header('Content-Type','application/octet-stream')
            self.send_header('X-Signal-Framing','diag-le32-v1')
            self.end_headers()
            i=0
            try:
                while signal_stress.is_set():
                    self.wfile.write(signal_fixture(True,10+i%16)*25);self.wfile.flush()
                    i+=1;time.sleep(.04)
            except (BrokenPipeError,ConnectionResetError,ConnectionAbortedError):pass
            return
        if path == '/signal/stream':
            return self.reply(signal_fixture(True), headers={'Content-Type': 'application/octet-stream', 'X-Signal-Framing': 'diag-le32-v1'})
        routes = {'/state': state, '/capabilities': {'controls': controls}, '/lan': lan, '/tasks': tasks, '/network/recovery': recovery, '/sms/forward/config': forward, '/sms/forward/status': forward['status'], '/cloud/app/config': nms, '/cloud/app/status': {'state': 'connected', 'last_report_at': 1795158000}, '/ota/app': ota,
                  '/activity': {'entries': [{'id': 1, 'category': 'recovery', 'action': 'redial', 'result': 'requested', 'reason': 'awaiting_probe', 'device_time': '2026-11-20 10:00'}], 'storage_ok': True, 'max_entries': 300},
                  '/traffic/history': {'device_date': '2026-11-20', 'oldest_date': '2026-11-01', 'days': [{'date': '2026-11-18', 'bytes': 1048576}, {'date': '2026-11-20', 'bytes': 2097152}]}}
        return self.reply(routes[path]) if path in routes else self.reply({'error': 'not_found'}, 404)

    def do_POST(self):
        path = urlsplit(self.path).path.removeprefix(PREFIX)
        payload = json.loads(self.rfile.read(int(self.headers.get('Content-Length', '0'))))
        if path == '/auth/login':
            assert self.headers.get('Authorization') == 'Basic ' + base64.b64encode(b'admin:fixture-password').decode()
            return self.reply({'ok': True, 'access_token': TOKEN})
        if self.headers.get('Authorization') != 'Bearer ' + TOKEN:
            return self.reply({'ok': False}, 401)
        trace.append(('POST', path, copy.deepcopy(payload)))
        if path == '/control':
            action, p = payload['action'], payload['params']
            if action == 'sms.send_raw':
                text = bytes.fromhex(p['message_hex']).decode('utf-16-be')
                if text == 'reject':
                    return self.reply({'ok': False, 'error': {'message': '设备拒绝发送测试'}})
                if text == 'unknown':
                    return self.reply({'ok': True, 'result': {}})
                assert p['sender'] == 'sim2' and p['ID'] == '-1' and p['encode_type'] == 'UNICODE'
                state['sms']['list'].append({'id': 4, 'num': p['number'], 'text': text, 'tag': 2, 'date': '2026-11-20 10:03:00'})
                return self.reply({'ok': True, 'result': {'status': 3}})
            if action == 'sms.delete':
                state['sms']['list'] = [r for r in state['sms']['list'] if str(r['id']) + ';' != p['ids']]
            elif action == 'apn.list':
                return self.reply({'ok': True, 'result': apn})
            elif action in ['apn.add', 'apn.modify']:
                row = {'profileId': p.get('profile_id', 'manual-3'), 'profilename': p['name'], 'wanapn': p['apn'], 'pdpType': p['pdp_type'], 'pppAuthMode': p.get('auth_mode', 0)}
                apn['manual']['apnListArray'] = [r for r in apn['manual']['apnListArray'] if r['profileId'] != row['profileId']] + [row]
            elif action == 'apn.enable':
                apn['enabled']['profileId'] = p['profile_id']; apn['mode']['apn_mode'] = 1
            elif action == 'apn.delete':
                apn['manual']['apnListArray'] = [r for r in apn['manual']['apnListArray'] if r['profileId'] != p['profile_id']]
            elif action == 'apn.set_mode':
                apn['mode']['apn_mode'] = p['mode']
            elif action == 'network.set_mode':
                state['net']['net_select'] = {'4G': 'Only_LTE', '5G': 'Only_5G', 'auto': 'WL_AND_5G'}.get(p['mode'], p['mode'])
            elif action.startswith('band.set_'):
                key = {'band.set_lte': 'lte_band', 'band.set_nr_sa': 'sa_bands', 'band.set_nr_nsa': 'nsa_bands'}[action]; state['net'][key] = p['bands']
            elif action == 'cell.lock_nr':
                state['net']['nr_cell_lock'] = p
            elif action == 'cell.unlock_all':
                state['net'].pop('nr_cell_lock', None); state['net'].pop('lte_cell_lock', None)
            elif action == 'wifi.status':
                return self.reply({'ok': True, 'result': wifi})
            elif action == 'wifi.configure':
                row = wifi[p['section']]
                for k, v in p.items():
                    if k == 'enabled': row['disabled'] = '0' if v else '1'
                    elif k != 'section': row[k] = v
            elif action == 'wifi.set_module':
                state['wlan']['enabled'] = p['enabled']
            elif action == 'wifi.set_dual_band':
                state['wlan']['band_steering_enabled'] = int(p['enabled'])
            elif action == 'oem.access.status':
                return self.reply({'ok': True, 'result': oem})
            elif action == 'oem.access.set':
                oem.update(paused=p['paused'], remaining_seconds=p['duration_seconds'] or None)
                return self.reply({'ok': True, 'result': oem})
            elif action not in ['cellular.disconnect', 'cellular.connect', 'signal.stream.stop', 'device.reboot']:
                return self.reply({'ok': False, 'error': {'message': 'unexpected_action'}})
            return self.reply({'ok': True, 'result': {}})
        if path in ['/lan', '/lan/mtu']:
            lan.update(payload); return self.reply({'settings': lan, 'verified': True, 'changed': True})
        if path == '/tasks':
            tasks['tasks'] = [t for t in tasks['tasks'] if t['id'] != payload['id']] + [payload]; return self.reply(tasks)
        if path == '/tasks/remove':
            tasks['tasks'] = [t for t in tasks['tasks'] if t['id'] != payload['id']]; return self.reply(tasks)
        if path == '/network/recovery':
            recovery['config'] = payload; return self.reply(recovery)
        if path == '/network/recovery/check': return self.reply({'online': False})
        if path == '/sms/forward/config':
            forward['config'].update(payload); return self.reply(forward['status'])
        if path == '/sms/forward/test': return self.reply(forward['status'])
        if path == '/cloud/app/config':
            assert 'remote_webshell_enabled' not in payload and 'services' not in payload
            services = payload.pop('web_services'); nms['config'].update(payload)
            nms['config']['services'] = services + [s for s in nms['config']['services'] if s['kind'] == 'terminal']
            return self.reply(nms)
        if path == '/ota/app/check':
            ota['status'].update(state='available', signature_verified=True, last_check_at=1795158000)
            ota['candidate'] = {'id': 'f' * 64, 'manifest': {'version': '99.0.0', 'notes': 'Fixture release', 'artifacts': {'binary': {'size': 1024}}}}
            return self.reply({'started': True}, 202)
        if path == '/ota/app/install':
            assert payload == {'candidate_id': 'f' * 64}
            ota['busy'] = True; ota['status']['state'] = 'installing'; return self.reply({'started': True}, 202)
        return self.reply({'error': 'unexpected_route'}, 404)

    def shell(self):
        protocols = self.headers.get('Sec-WebSocket-Protocol', '')
        if 'datad-auth.' + TOKEN not in protocols or 'datad-webshell-v1' not in protocols:
            return self.reply({'ok': False}, 401)
        assert '?' not in self.path
        accept = base64.b64encode(hashlib.sha1((self.headers['Sec-WebSocket-Key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest()).decode()
        self.send_response(101); self.send_header('Upgrade', 'websocket'); self.send_header('Connection', 'Upgrade'); self.send_header('Sec-WebSocket-Accept', accept); self.send_header('Sec-WebSocket-Protocol', 'datad-webshell-v1'); self.end_headers()
        def send(opcode, data):
            self.wfile.write(bytes([0x80 | opcode, len(data)]) + data); self.wfile.flush()
        send(1, b'{"type":"ready","cols":80,"rows":24}'); send(2, b'fixture / # ')
        try:
            while True:
                head = self.rfile.read(2)
                if len(head) < 2: break
                opcode, length = head[0] & 15, head[1] & 127
                if length == 126: length = struct.unpack('!H', self.rfile.read(2))[0]
                if length == 127: length = struct.unpack('!Q', self.rfile.read(8))[0]
                assert length <= 16384
                mask = self.rfile.read(4); data = self.rfile.read(length)
                data = bytes(v ^ mask[i % 4] for i, v in enumerate(data))
                if opcode == 8: break
                if opcode == 2: shell_input.append(data)
                if opcode == 1:
                    resize = json.loads(data); assert resize['type'] == 'resize'
        finally:
            shell_closed.set()


server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
try:
    with sync_playwright() as p:
        browser = p.chromium.launch(headless=True)
        page = browser.new_page(viewport={'width': 1360, 'height': 950})
        page.set_default_timeout(12000)
        errors = []
        page.on('pageerror', lambda error: errors.append(str(error)))
        page.add_init_script("Object.defineProperty(navigator,'clipboard',{value:undefined,configurable:true});window.__csp=[];document.addEventListener('securitypolicyviolation',e=>window.__csp.push(e.violatedDirective))")
        page.goto(f'http://127.0.0.1:{server.server_port}{PREFIX}/ui/')
        page.get_by_label('管理密码', exact=False).fill('fixture-password')
        page.get_by_role('button', name='登录设备', exact=True).click()
        page.get_by_text('9007199254740993', exact=True).first.wait_for()

        def go(route, title):
            page.evaluate('(route)=>location.hash=route', route)
            page.get_by_role('heading', name=title, exact=True).wait_for()
            page.wait_for_timeout(350)
            assert page.evaluate('scrollY') == 0, 'new page kept the previous page scroll offset'

        go('params', '信号参数')
        page.get_by_text('2CC 配置 · 未聚合', exact=True).first.wait_for()
        assert page.get_by_text('SCC0', exact=True).count() > 0
        state['sim']['dual_sim']=1
        go('sms', '短信')
        page.get_by_text('测试收件正文', exact=True).first.wait_for()
        expect(page.get_by_text('卡槽未记录',exact=False)).to_have_count(0)
        expect(page.get_by_text('卡槽未关联',exact=False)).to_have_count(0)
        page.get_by_role('button', name='查看短信 10086', exact=True).click()
        expect(page.get_by_role('dialog').get_by_text('卡槽',exact=False)).to_have_count(0)
        page.get_by_role('dialog', name='短信详情', exact=True).get_by_role('button', name='复制', exact=True).click()
        page.get_by_text('正文已复制', exact=True).wait_for()
        page.keyboard.press('Escape'); page.wait_for_timeout(220)
        page.get_by_role('button',name='新建短信',exact=True).click()
        expect(page.get_by_role('combobox',name='发送卡槽',exact=True)).to_have_count(0)
        page.get_by_role('dialog',name='新建短信',exact=True).get_by_role('button',name='取消',exact=True).click()
        expect(page.get_by_role('dialog')).to_have_count(0)
        state['sim']['dual_sim']=2
        page.get_by_role('button',name='刷新',exact=True).click()
        expect(page.get_by_text('卡槽未记录',exact=True)).to_have_count(1)
        page.get_by_role('tab', name='发件箱', exact=False).click()
        page.get_by_text('失败发件记录', exact=True).first.wait_for()
        page.get_by_role('button', name='新建短信', exact=True).click()
        page.get_by_label('收件号码', exact=True).fill('10086')
        page.get_by_label('正文', exact=True).fill('你好 😀')
        page.get_by_role('combobox', name='发送卡槽', exact=True).click()
        page.get_by_role('option', name='SIM 2', exact=True).click()
        page.get_by_role('dialog', name='新建短信', exact=True).get_by_role('button', name='发送', exact=True).click()
        page.get_by_role('dialog').filter(has_text='发送短信？').get_by_role('button', name='发送', exact=True).click()
        page.get_by_text('短信发送成功', exact=True).first.wait_for()
        assert len([t for t in trace if t[1] == '/control' and t[2]['action'] == 'sms.send_raw']) == 1


        # A 200 response with a business failure is never reported as success, nor replayed.
        for body, message in [('reject', '设备拒绝发送测试'), ('unknown', '请求已提交，设备尚未确认发送结果')]:
            page.get_by_role('button', name='新建短信', exact=True).click()
            page.get_by_label('收件号码', exact=True).fill('10086')
            page.get_by_label('正文', exact=True).fill(body)
            page.get_by_role('dialog', name='新建短信', exact=True).get_by_role('button', name='发送', exact=True).click()
            page.get_by_role('dialog').filter(has_text='发送短信？').get_by_role('button', name='发送', exact=True).click()
            page.get_by_text(message, exact=False).first.wait_for()
            before = len([t for t in trace if t[1] == '/control' and t[2]['action'] == 'sms.send_raw'])
            page.wait_for_timeout(250)
            assert before == len([t for t in trace if t[1] == '/control' and t[2]['action'] == 'sms.send_raw'])
            page.get_by_role('dialog').filter(has_text='新建短信').get_by_role('button', name='取消', exact=True).click()
            page.get_by_role('dialog').filter(has_text='放弃未发送的短信？').get_by_role('button', name='放弃', exact=True).click()
            page.wait_for_timeout(200)
        go('apn', 'APN 管理')
        page.get_by_text('手动档案', exact=True).first.wait_for()
        page.wait_for_timeout(250)  # Finish the page entry motion before measuring layout.
        apn_panel_top=page.locator('.panel').first.evaluate('(el)=>el.getBoundingClientRect().top+scrollY')
        page.get_by_role('button', name='启用', exact=True).first.click()
        page.get_by_role('dialog').get_by_role('button', name='确认', exact=True).click()
        page.get_by_text('已保存并回读确认', exact=True).first.wait_for()
        expect(page.locator('button[data-loading]')).to_have_count(0)
        expect(page.get_by_role('dialog')).to_have_count(0,timeout=3000)
        expect(page.locator('.app-toast')).to_have_count(1)
        expect(page.get_by_role('dialog')).to_have_count(0,timeout=3000)
        apn_panel_after=page.locator('.panel').first.evaluate('(el)=>el.getBoundingClientRect().top+scrollY')
        assert abs(apn_panel_after-apn_panel_top)<1, f'operation feedback pushed page layout: {apn_panel_top} -> {apn_panel_after}'
        assert page.locator('main .mantine-Alert-root').count()==0, 'operation result remained in page content'
        assert page.locator('.app-toast').evaluate('(el)=>!el.closest("main")'), 'Toast must be rendered outside page flow'
        page.set_viewport_size({'width':390,'height':844})
        expect(page.locator('.app-toast-host[data-position="top-center"] .app-toast')).to_be_visible()
        expect(page.locator('.app-toast')).to_have_count(1)
        bounds=page.locator('.app-toast').bounding_box()
        assert bounds['x']>=0 and bounds['x']+bounds['width']<=390, bounds
        page.screenshot(path=str(ROOT / 'build/reports/web-toast-mobile.png'),full_page=True)
        page.set_viewport_size({'width':1360,'height':950})
        expect(page.locator('.app-toast')).to_have_count(0,timeout=6500)
        assert apn['enabled']['profileId'] == 'manual-2'
        page.get_by_role('button', name='新建', exact=True).click()
        page.get_by_role('textbox', name='APN', exact=False).fill('new.internet')
        page.get_by_role('button', name='保存', exact=True).click()
        page.get_by_text('APN 已保存；新建档案不会自动启用', exact=True).first.wait_for()
        assert apn['enabled']['profileId'] == 'manual-2'

        go('bands', '频段与小区')
        page.get_by_role('combobox', name='网络模式', exact=True).click()
        page.get_by_role('option', name='仅 4G', exact=True).click()
        page.get_by_role('button', name='应用', exact=True).click()
        page.get_by_role('dialog').get_by_role('button', name='确认', exact=True).click()
        page.get_by_text('已保存并回读确认', exact=True).first.wait_for()
        expect(page.locator('button[data-loading]')).to_have_count(0)
        expect(page.get_by_role('dialog')).to_have_count(0,timeout=3000)
        page.get_by_text('B3', exact=True).click()
        page.get_by_role('button', name='应用频段', exact=True).click()
        page.get_by_role('dialog').get_by_role('button', name='确认', exact=True).click()
        page.get_by_text('已保存并回读确认', exact=True).first.wait_for()
        expect(page.locator('button[data-loading]')).to_have_count(0)
        expect(page.get_by_role('dialog')).to_have_count(0,timeout=3000)
        expect(page.get_by_role('button', name='应用频段', exact=True)).to_be_disabled()
        assert state['net']['lte_band'] == '1'
        assert not [t for t in trace if t[1] == '/control' and t[2]['action'] == 'device.reboot']

        go('cell-lock', '小区锁定')
        page.get_by_label('PCI · 0–1007', exact=True).fill('125')
        page.get_by_label('NR-ARFCN', exact=True).fill('633984')
        page.get_by_label('NR 频段 · 如 78', exact=True).fill('78')
        page.get_by_role('button', name='保存锁定', exact=True).click()
        page.get_by_role('dialog').get_by_role('button', name='确认', exact=True).click()
        page.get_by_role('button', name='稍后', exact=True).click()
        assert state['net']['nr_cell_lock']['pci'] == 125
        assert state['net']['net_select'] == 'Only_5G'

        go('wifi', '热点设置')
        page.get_by_label('SSID', exact=True).first.fill('Updated-2G')
        page.evaluate("location.hash='device'")
        page.get_by_role('dialog').filter(has_text='放弃未保存的修改？').get_by_role('button', name='取消', exact=True).click()
        expect(page.get_by_label('SSID', exact=True).first).to_have_value('Updated-2G')
        page.get_by_role('button', name='保存此频段', exact=True).first.click()
        page.get_by_role('dialog').get_by_role('button', name='确认', exact=True).click()
        page.get_by_text('配置已回读确认，热点正在应用', exact=True).first.wait_for()
        assert wifi['main_2g']['ssid'] == 'Updated-2G' and wifi['main_2g']['channel'] == '0'

        go('lan', 'LAN 设置')
        page.get_by_label('起始地址', exact=True).fill('192.168.0.5')
        page.get_by_label('MTU · 576–1500', exact=True).fill('1400')
        page.get_by_role('button', name='保存 MTU', exact=True).click()
        page.get_by_role('dialog').get_by_role('button', name='确认', exact=True).click()
        page.get_by_text('已保存并回读确认', exact=True).first.wait_for()
        expect(page.locator('button[data-loading]')).to_have_count(0)
        expect(page.get_by_role('dialog')).to_have_count(0,timeout=3000)
        expect(page.get_by_label('起始地址', exact=True)).to_have_value('192.168.0.5')
        page.get_by_role('button', name='保存 LAN 设置', exact=True).click()
        page.get_by_role('dialog').get_by_role('button', name='确认', exact=True).click()
        page.wait_for_timeout(300)
        assert lan['mtu'] == 1400 and lan['dhcp_start'] == '192.168.0.5'

        go('tasks', '定时任务')
        page.get_by_role('button', name='新建', exact=True).click()
        page.get_by_label('任务名称', exact=True).fill('Fixture task')
        page.get_by_label('设备本地执行时间', exact=True).fill('08:30')
        page.get_by_role('button', name='保存', exact=True).click()
        page.get_by_role('dialog').filter(has_text='保存定时任务？').get_by_role('button', name='确认', exact=True).click()
        page.get_by_text('任务已保存', exact=True).first.wait_for()
        assert tasks['tasks'][0]['time'] == '08:30'
        go('activity', '通知与执行记录')
        page.get_by_text('等待检测', exact=True).first.wait_for()
        go('recovery', '断网自动恢复')
        page.get_by_role('button', name='检测一次', exact=True).click()
        page.get_by_text('本次检测：两个地址均未连通', exact=True).first.wait_for()

        go('forward', '短信转发')
        page.get_by_label('设备备注', exact=True).fill('Updated nickname')
        page.get_by_role('button', name='保存配置', exact=True).click()
        page.get_by_text('转发配置已保存并重新读取', exact=True).first.wait_for()
        patch = next(t[2] for t in reversed(trace) if t[0] == 'POST' and t[1] == '/sms/forward/config')
        assert set(patch) == {'enabled', 'method', 'nickname'}, patch
        go('nms', 'NMS 远程管理')
        page.get_by_label('上报间隔 · 10–3600 秒', exact=True).fill('45')
        page.get_by_role('button', name='保存配置', exact=True).click()
        page.get_by_text('配置已保存，正在更新云端连接', exact=True).first.wait_for()
        assert nms['config']['services'][-1]['kind'] == 'terminal'
        go('oem', '官方后台避让')
        page.get_by_label('暂停 datad 自动登录', exact=True).click()
        page.get_by_text('已暂停 datad 自动登录，可使用官方后台', exact=True).first.wait_for()
        assert oem['paused']

        go('ota', 'datad 更新')
        expect(page.get_by_label('空闲时自动更新', exact=True)).to_be_enabled()
        ota['automatic_updates_allowed'] = False
        go('device', '设备信息')
        go('ota', 'datad 更新')
        expect(page.get_by_label('空闲时自动更新', exact=True)).to_be_disabled()
        del ota['automatic_updates_allowed']
        go('device', '设备信息')
        go('ota', 'datad 更新')
        expect(page.get_by_label('空闲时自动更新', exact=True)).to_be_enabled()
        page.get_by_role('button', name='检查更新', exact=True).click()
        page.get_by_text('Fixture release', exact=True).first.wait_for()
        page.get_by_role('button', name='安装更新', exact=True).click()
        page.get_by_role('dialog').get_by_role('button', name='安装更新', exact=True).click()
        page.get_by_text('正在安装，等待服务恢复', exact=True).first.wait_for()
        assert ota['busy']

        go('shell', 'Root Shell')
        page.get_by_text('已连接 · 设备 Shell', exact=True).first.wait_for()
        page.locator('.xterm-helper-textarea').focus()
        page.keyboard.type('id'); page.keyboard.press('Enter')
        page.wait_for_timeout(200)
        assert b''.join(shell_input) == b'id\r', shell_input
        go('device', '设备信息')
        assert shell_closed.wait(2), 'PTY was not released on navigation'

        go('signal', '信令分析')
        expect(page.get_by_role('button',name='展开实时采集',exact=True)).to_have_attribute('aria-expanded','false')
        expect(page.get_by_role('button',name='导入抓包',exact=True)).to_have_count(0)
        expect(page.get_by_role('button',name='开始抓取',exact=True)).to_have_count(0)
        expect(page.get_by_role('button',name='停止',exact=True)).to_have_count(1)
        for label, mode in [('仅 4G','Only_LTE'),('仅 5G SA','Only_5G')]:
            page.get_by_role('combobox',name='信令页网络模式',exact=True).click()
            page.get_by_role('option',name=label,exact=True).click()
            page.get_by_role('dialog').get_by_role('button',name='确认',exact=True).click()
            page.get_by_text('网络模式已回读确认',exact=True).first.wait_for()
            expect(page.get_by_role('combobox',name='信令页网络模式',exact=True)).to_be_enabled()
            assert state['net']['net_select']==mode
            expect(page.get_by_role('button',name='展开实时采集',exact=True)).to_have_attribute('aria-expanded','false')
            expect(page.get_by_role('button',name='停止',exact=True)).to_have_count(1)

        expect(page.locator('text=/包 [0-9,]+ · RRC/').first).to_be_visible()
        page.get_by_role('button',name='暂停列表',exact=True).click()
        page.get_by_role('button',name='继续更新',exact=True).click()
        page.get_by_role('button', name='停止', exact=True).click()
        page.get_by_role('button',name='展开实时采集',exact=True).click()
        expect(page.get_by_role('button',name='导入抓包',exact=True)).to_be_visible()
        page.locator('input[type=file][accept=".bin"]').set_input_files({'name':'synthetic-layouts.bin','mimeType':'application/octet-stream','buffer':signal_fixture()})
        page.get_by_text('导入完成', exact=True).first.wait_for()
        page.get_by_text('50.00%', exact=True).first.wait_for()
        page.get_by_text('MCS 17 · 64QAM', exact=True).first.wait_for()
        page.get_by_text('QCI 9', exact=True).first.wait_for()
        page.get_by_text('PCI 10', exact=True).first.wait_for()
        page.get_by_role('button', name='BLER / MCS', exact=False).click()
        page.get_by_role('dialog').get_by_text('本包 TB', exact=True).wait_for()
        page.get_by_role('dialog').get_by_text('2', exact=True).first.wait_for()
        page.get_by_role('dialog').get_by_role('button', name='关闭', exact=True).click()
        page.get_by_role('button', name='导出', exact=True).click()
        with page.expect_download() as exported:
            page.get_by_role('menuitem', name='完整帧与解码 NDJSON', exact=False).click()
        records = [json.loads(line) for line in Path(exported.value.path()).read_text().splitlines()]
        assert len(records) == 3 and all(r.get('diag_packet_hex') and r.get('payload_hex') for r in records)
        page.screenshot(path=str(ROOT / 'build/reports/web-signal-cards.png'), full_page=True)
        # The live stream uses Bearer and an outer code; stop must cancel reconnect work.
        state['net']['type'] = 'LTE'
        page.get_by_role('button', name='开始抓取', exact=True).click()
        page.get_by_role('button', name='停止', exact=True).wait_for()
        expect(page.get_by_role('button',name='开始抓取',exact=True)).to_have_count(0)
        expect(page.get_by_role('button',name='停止',exact=True)).to_be_enabled()
        inactive_button=page.get_by_role('button',name='导入抓包',exact=True)
        expect(inactive_button).to_be_disabled()
        for scheme, inactive, normal in [('light','rgb(240, 243, 248)','rgb(248, 250, 255)'),('dark','rgb(36, 49, 67)','rgb(32, 41, 54)')]:
            if page.locator('html').get_attribute('data-mantine-color-scheme')!=scheme:
                page.get_by_role('button',name='切换明暗主题',exact=True).click()
            expect(inactive_button).to_have_css('background-color',inactive)
            expect(page.get_by_role('combobox',name='信令页网络模式',exact=True)).to_have_css('background-color',normal)
            page.screenshot(path=str(ROOT / f'build/reports/web-toast-{scheme}.png'),full_page=True)
        page.get_by_role('button',name='切换明暗主题',exact=True).click()
        page.get_by_text('50.00%', exact=True).first.wait_for()
        page.get_by_role('button', name='停止', exact=True).click()
        page.wait_for_timeout(300)
        requests_at_stop = len([t for t in trace if t[1] == '/signal/stream'])
        page.wait_for_timeout(1200)
        assert requests_at_stop == len([t for t in trace if t[1] == '/signal/stream'])
        # A busy capture must keep recording while its background DOM stays quiet behind a modal.
        signal_stress.set()
        page.get_by_role('button',name='开始抓取',exact=True).click()
        page.wait_for_timeout(1400)
        page.get_by_role('button',name='BLER / MCS',exact=False).click()
        page.get_by_role('dialog').wait_for()
        page.wait_for_timeout(250)
        page.evaluate("""()=>{
            window.__modalMutations={background:0,details:0};
            for(const [selector,key] of [['.record-table','background'],['[role=dialog]','details']]){
                const observer=new MutationObserver(rows=>window.__modalMutations[key]+=rows.length);
                observer.observe(document.querySelector(selector),{subtree:true,childList:true,characterData:true});
            }
        }""")
        before_metrics=page.locator('text=/包 [0-9,]+ · RRC/').first.inner_text()
        page.wait_for_timeout(1800)
        mutations=page.evaluate('window.__modalMutations')
        assert mutations['background']==0, mutations
        assert 0<mutations['details']<100, mutations
        assert page.locator('text=/包 [0-9,]+ · RRC/').first.inner_text()==before_metrics, 'background metrics refreshed behind modal'
        page.get_by_role('dialog').get_by_role('button',name='关闭',exact=True).click()
        expect(page.get_by_role('dialog')).to_have_count(0)
        page.wait_for_timeout(350)
        after_metrics=page.locator('text=/包 [0-9,]+ · RRC/').first.inner_text()
        def packet_count(text):return int(text.split(' · ')[0].removeprefix('包 ').replace(',',''))
        assert packet_count(after_metrics)>packet_count(before_metrics)+100, (before_metrics,after_metrics)
        page.get_by_role('button',name='停止',exact=True).click()
        signal_stress.clear()
        page.wait_for_timeout(300)
        final_count=packet_count(page.locator('text=/包 [0-9,]+ · RRC/').first.inner_text())
        page.get_by_role('button',name='导出',exact=True).click()
        with page.expect_download() as exported_raw:
            page.get_by_role('menuitem',name='原始 BIN',exact=False).click()
        raw=Path(exported_raw.value.path()).read_bytes();at=0;packets=0
        while at<len(raw):
            size=struct.unpack_from('<I',raw,at)[0];at+=size+4;packets+=1
        assert at==len(raw) and packets==final_count, (packets,final_count)
        open_height=page.locator('.capture-panel').bounding_box()['height']
        page.get_by_role('button',name='收起实时采集',exact=True).click()
        expect(page.get_by_role('combobox',name='信令页网络模式',exact=True)).to_be_visible()
        expect(page.get_by_role('button',name='导入抓包',exact=True)).to_have_count(0)
        assert page.locator('.capture-panel').bounding_box()['height']<open_height
        expect(page.get_by_role('button',name='开始抓取',exact=True)).to_be_visible()
        assert page.locator('text=/包 [0-9,]+ · RRC/').first.inner_text().endswith('0/s')
        page.screenshot(path=str(ROOT/'build/reports/web-signal-capture-compact.png'))
        page.set_viewport_size({'width':390,'height':844})
        expect(page.get_by_role('button',name='暂停列表',exact=True)).to_be_visible()
        page.wait_for_timeout(250)
        assert page.evaluate('document.documentElement.scrollWidth<=innerWidth')
        page.screenshot(path=str(ROOT/'build/reports/web-signal-capture-compact-mobile.png'))
        page.set_viewport_size({'width':1360,'height':950})
        go('usage', '历史流量统计')
        page.get_by_text('2 天记录', exact=False).first.wait_for()
        page.screenshot(path=str(ROOT / 'build/reports/web-usage-desktop.png'), full_page=True)
        go('settings', '设置')
        page.screenshot(path=str(ROOT / 'build/reports/web-settings-desktop.png'), full_page=True)
        page.get_by_role('button', name='编辑名称', exact=True).click()
        page.get_by_label('名称 · 仅保存在当前浏览器', exact=True).fill('Fixture device')
        page.get_by_role('dialog').get_by_role('button', name='保存', exact=True).click()
        page.get_by_role('button', name='切换设备', exact=True).click()
        page.get_by_label('网页管理地址 · 包含端口和代理前缀', exact=True).fill('http://user:secret@example.com/')
        page.get_by_role('button', name='前往登录', exact=True).click()
        page.get_by_text('请输入不含账号、查询参数的 HTTP / HTTPS 管理地址', exact=True).wait_for()
        page.get_by_role('dialog').get_by_role('button', name='取消', exact=True).click()
        page.wait_for_timeout(220)
        page.set_viewport_size({'width': 390, 'height': 844})
        go('nms', 'NMS 远程管理')
        assert page.evaluate('document.documentElement.scrollWidth <= innerWidth'), 'mobile page overflows horizontally'
        page.screenshot(path=str(ROOT / 'build/reports/web-nms-mobile.png'), full_page=True)
        assert not errors, errors
        assert not page.evaluate('window.__csp'), page.evaluate('window.__csp')
        assert all(TOKEN not in t[1] for t in trace)
        browser.close()
        print('App browser checks passed: SMS/APN/modes/bands/cell locks/WiFi/LAN/tasks/activity/recovery/forward/NMS/OEM/OTA/PTY/history/signal cards, exports and stream cleanup; real bundles, authenticated proxy prefix, no modem access.')
finally:
    signal_stress.clear()
    server.shutdown(); server.server_close()
