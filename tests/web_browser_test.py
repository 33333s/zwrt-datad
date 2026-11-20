#!/usr/bin/env python3
"""Production-bundle browser checks against a fake API. Never accesses a modem."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import base64
import json
import struct
import threading
import time
from urllib.parse import parse_qs, urlsplit

from playwright.sync_api import sync_playwright, expect

ROOT = Path(__file__).resolve().parents[1]
DIST = ROOT / "web/dist"
PREFIX = "/test/device"
TOKEN = "web-browser-fixture-not-a-device-secret"
CSP = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; worker-src 'self'; img-src 'self' blob: data:; font-src 'self'; base-uri 'self'; form-action 'none'; frame-ancestors 'none'"
ASSETS = {route: (DIST / path, mime) for route, path, mime in json.loads((DIST / "asset-manifest.json").read_text())}
requests = []
auth_attempts = []
auth_delay = 0
content = b"first line\nsecond line\n"
state = {"ts": 1795158000, "datad": {"name": "zwrt-datad", "version": "0.10.72"},
         "device": {"model_name": "MU5120", "market_name": "U50 Pro", "sw_version": "fixture"},
         "net": {"type": "SA", "nr_band": "n78", "nr_pci": 123, "nr_channel": 633984,
                 "nr_rsrp": -82, "nr_rsrq": -11, "nr_snr": "23.5", "nr_cell_id": 9007199254740993,
                 "nr_bw": "100 MHz", "operator": "Test operator", "plmn": "00101", "nr_tac": 10001},
         "battery": {"percent": 72, "status": "Discharging"},
         "traffic": {"rx_speed": 5242880, "tx_speed": 655360, "rx_bytes": 314572800, "tx_bytes": 52428800},
         "system": {"uptime": 4000, "cpu_usage": 17, "cpu_temp": 41, "mem_used_pct": 52}}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, body, headers=None):
        if not isinstance(body, bytes):
            body = json.dumps(body).encode()
            headers = {"Content-Type": "application/json", **(headers or {})}
        self.send_response(status)
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        try:self.wfile.write(body)
        except (BrokenPipeError,ConnectionResetError,ConnectionAbortedError):pass

    def do_GET(self):
        uri = urlsplit(self.path)
        if not uri.path.startswith(PREFIX + "/"):
            return self.reply(404, {})
        path = uri.path[len(PREFIX):]
        if path in ASSETS:
            file, mime = ASSETS[path]
            return self.reply(200, file.read_bytes(), {"Content-Type": mime, "Content-Encoding": "gzip", "Content-Security-Policy": CSP})
        if self.headers.get("Authorization") != "Bearer " + TOKEN:
            return self.reply(401, {"ok": False})
        requests.append(("GET", path, uri.query))
        if path == "/state":
            return self.reply(200, state)
        if path == "/capabilities":
            return self.reply(200, {"controls": []})
        if path == "/efs/status":
            return self.reply(200, {"ok": True, "available": True, "safety": {"version": 1, "root_readonly": True, "recursive_delete": False, "no_symlink_mutations": True}, "limits": {"write_bytes": 32768, "transfer_bytes": 1048576}})
        params = parse_qs(uri.query)
        if path == "/efs/list":
            directory = params["path"][0]
            names = [("nvefs", "directory", 0), ("root-only", "file", 5)] if directory == "/" else [("hello.txt", "file", len(content))]
            return self.reply(200, {"ok": True, "data": {"path": directory, "entries": [{"name": name, "name_hex": name.encode().hex(), "size": size, "type": kind} for name, kind, size in names]}})
        if path == "/efs/read":
            offset = int(params["offset"][0]); data = content[offset:offset+4096]
            return self.reply(200, {"ok": True, "data": {"offset": offset, "bytes": len(data), "next_offset": offset+len(data), "eof": offset+len(data)>=len(content), "hex": data.hex()}})
        if path == "/efs/stat":
            return self.reply(200, {"ok": True, "data": {"path": params["path"][0], "type": "file", "links": 1, "mode": 33188, "size": len(content)}})
        if path == "/efs/download":
            return self.reply(200, content, {"Content-Type": "application/octet-stream"})
        if path == "/efs/nvread":
            raw = bytearray(133); raw[:12] = bytes.fromhex("262602088a66380408400200")
            return self.reply(200, {"ok": True, "data": {"encoding": "hex", "bytes": 133, "hex": raw.hex()}})
        return self.reply(404, {})

    def do_POST(self):
        global content
        body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        if self.path == PREFIX + "/auth/login":
            auth_attempts.append(True)
            if auth_delay:time.sleep(auth_delay)
            wanted = "Basic " + base64.b64encode(b"admin:test-password-not-real").decode()
            if self.headers.get("Authorization") != wanted:
                return self.reply(401, {"ok": False})
            return self.reply(200, {"ok": True, "access_token": TOKEN})
        if self.headers.get("Authorization") == "Bearer " + TOKEN and self.path == PREFIX + "/efs/write":
            payload = json.loads(body); assert payload["path"] == "/nvefs/hello.txt"
            requests.append(("POST", "/efs/write", "")); content = bytes.fromhex(payload["hex"])
            return self.reply(200, {"ok": True, "data": {"bytes": len(content)}})
        return self.reply(403, {"ok": False})


server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
try:
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(headless=True)
        page = browser.new_page(viewport={"width": 1360, "height": 950})
        page.set_default_timeout(10000)
        errors = []
        page.on("pageerror", lambda error: errors.append(str(error)))
        page.add_init_script("window.__cspErrors=[];document.addEventListener('securitypolicyviolation',e=>window.__cspErrors.push(e.violatedDirective))")
        page.goto(f"http://127.0.0.1:{server.server_port}{PREFIX}/ui/")
        page.wait_for_timeout(800)
        assert not errors, errors
        reports = ROOT / "build/reports"; reports.mkdir(parents=True, exist_ok=True)
        page.screenshot(path=str(reports / "web-login-modern.png"))
        expect(page.get_by_role('checkbox',name='记住密码',exact=True)).not_to_be_checked()
        expect(page.get_by_role('checkbox',name='自动登录',exact=True)).not_to_be_checked()
        expect(page.get_by_text('面向中兴 ARM64 5G 路由设备的统一数据与控制服务。',exact=True)).to_be_visible()
        github=page.get_by_role('link',name='zwrt-datad 开源项目',exact=False)
        expect(github).to_have_attribute('href','https://github.com/33333s/zwrt-datad')
        expect(github).to_have_attribute('target','_blank')
        expect(github).to_have_attribute('rel','noopener noreferrer')
        assert page.get_by_role("button", name="登录设备", exact=True).evaluate("e=>parseFloat(getComputedStyle(e).borderRadius)>0"), "theme radius is invalid"
        page.get_by_label("管理密码", exact=False).fill("test-password-not-real")
        page.get_by_role("button", name="登录设备", exact=True).click()
        page.get_by_text("9007199254740993", exact=True).wait_for()
        page.wait_for_timeout(600)
        expect(page.get_by_text('完整状态快照',exact=True)).to_have_count(0)
        expect(page.get_by_text('导入载波回放',exact=True)).to_have_count(0)
        expect(page.locator('.trend-panel').get_by_role('radio',name='速率',exact=True)).to_be_checked()
        page.locator('.trend-panel').get_by_text('RSRP',exact=True).click()
        expect(page.locator('.trend-panel').get_by_role('radio',name='RSRP',exact=True)).to_be_checked()
        # IDs are blurred independently, stay exact when revealed, and re-hide for a new cell or page.
        cell_button=page.get_by_role('button',name='显示小区 ID',exact=True)
        tac_button=page.get_by_role('button',name='显示TAC',exact=True)
        expect(cell_button).to_have_attribute('aria-pressed','false')
        expect(cell_button.locator('.private-value')).to_have_css('filter','blur(5px)')
        cell_button.click()
        cell_button=page.get_by_role('button',name='隐藏小区 ID',exact=True)
        expect(cell_button.locator('.private-value')).to_have_text('9007199254740993')
        expect(cell_button.locator('.private-value')).to_have_css('filter','none')
        expect(tac_button.locator('.private-value')).to_have_css('filter','blur(5px)')
        tac_button.focus();tac_button.press('Enter')
        expect(page.get_by_role('button',name='隐藏TAC',exact=True)).to_have_attribute('aria-pressed','true')
        state['net']['nr_cell_id']=9007199254740995
        page.get_by_role('button',name='刷新状态',exact=True).click()
        page.get_by_text('9007199254740995',exact=True).wait_for()
        expect(page.get_by_role('button',name='显示小区 ID',exact=True)).to_have_attribute('aria-pressed','false')
        expect(page.get_by_role('button',name='显示TAC',exact=True)).to_have_attribute('aria-pressed','false')
        state['net']['nr_cell_id']=9007199254740993
        page.get_by_role('button',name='刷新状态',exact=True).click()
        page.get_by_text('9007199254740993',exact=True).wait_for()
        page.get_by_role('button',name='显示小区 ID',exact=True).click()
        page.evaluate("location.hash='params'")
        page.get_by_role('heading',name='信号参数',exact=True).wait_for()
        page.locator('.overview-page').wait_for(state='detached')
        page.evaluate("location.hash='overview'")
        page.get_by_role('heading',name='设备概览',exact=True).wait_for()
        expect(page.get_by_role('button',name='显示小区 ID',exact=True)).to_have_attribute('aria-pressed','false')
        expect(page.locator('.trend-panel').get_by_role('radio',name='RSRP',exact=True)).to_be_checked()
        # Desktop keeps all four summaries alongside the chart, rather than below the carrier list.
        for width in [1600,1360]:
            page.set_viewport_size({'width':width,'height':1000})
            page.wait_for_timeout(250)
            summary=page.locator('.overview-summary')
            expect(summary.locator('.overview-summary-tile')).to_have_count(4)
            a=page.locator('.trend-panel').bounding_box(); b=summary.bounding_box(); c=page.locator('.resources-panel').bounding_box()
            assert a['x']+a['width']<=b['x']+1 and b['x']+b['width']<=c['x']+1, (a,b,c)
            assert abs(a['y']-b['y'])<1 and abs(a['y']-c['y'])<1, (a,b,c)
            assert all(tile.bounding_box()['height']<160 for tile in summary.locator('.overview-summary-tile').all())
            assert page.evaluate('document.documentElement.scrollWidth<=innerWidth')
        page.screenshot(path=str(reports / "web-overview-desktop.png"), full_page=True)
        page.get_by_role('button',name='切换明暗主题',exact=True).click()
        page.screenshot(path=str(reports / 'web-overview-layout-dark.png'),full_page=True)
        page.get_by_role('button',name='切换明暗主题',exact=True).click()
        for width in [1100,900,390,320]:
            page.set_viewport_size({'width':width,'height':844})
            page.wait_for_timeout(250)
            assert page.evaluate('document.documentElement.scrollWidth<=innerWidth'), width
            expect(page.locator('.overview-summary-tile')).to_have_count(4)
        page.set_viewport_size({'width':390,'height':844})
        page.screenshot(path=str(reports / 'web-overview-layout-mobile.png'),full_page=True)
        # Every close path works without navigating away or losing the current page.
        for method in ['button','overlay','escape','active-page']:
            page.get_by_role('button',name='打开导航',exact=True).click()
            drawer=page.get_by_role('dialog')
            expect(drawer.get_by_role('button',name='关闭导航',exact=True)).to_be_visible()
            if method=='button':drawer.get_by_role('button',name='关闭导航',exact=True).click()
            elif method=='overlay':page.mouse.click(380,420)
            elif method=='escape':page.keyboard.press('Escape')
            else:drawer.get_by_role('button',name='设备概览',exact=True).click()
            expect(page.get_by_role('dialog')).to_have_count(0)
            expect(page.get_by_role('heading',name='设备概览',exact=True)).to_be_visible()
            expect(page.get_by_role('button',name='打开导航',exact=True)).to_be_focused()
        page.set_viewport_size({'width':320,'height':568})
        page.get_by_role('button',name='打开导航',exact=True).click()
        expect(page.get_by_role('dialog').get_by_role('button',name='关闭导航',exact=True)).to_be_visible()
        page.wait_for_timeout(200)
        page.screenshot(path=str(reports / 'web-navigation-small-mobile.png'))
        page.get_by_role('dialog').get_by_role('button',name='关闭导航',exact=True).click()
        expect(page.get_by_role('dialog')).to_have_count(0)
        page.set_viewport_size({'width':1360,'height':950})
        page.get_by_text("EFS 管理器", exact=True).first.click()
        confirm = page.get_by_role("button", name="我已了解", exact=False)
        confirm.wait_for(); assert confirm.is_disabled(), "EFS countdown missing"
        page.wait_for_timeout(5200); confirm.click()
        page.get_by_role("button", name="nvefs", exact=True).wait_for()
        assert page.get_by_role("button", name="上传文件", exact=True).is_disabled(), "root uploads enabled"
        page.get_by_role("button", name="nvefs", exact=True).click()
        page.get_by_role("button", name="hello.txt", exact=True).wait_for()
        page.wait_for_timeout(250)
        page.screenshot(path=str(reports / "web-efs-desktop.png"), full_page=True)
        # Read-only preview keeps exact bytes/newlines, with compact text and an overlay scrollbar.
        page.get_by_role("button",name="切换明暗主题",exact=True).click()
        short_content=content
        content=b'line one\n\nline two\r\n'*400
        page.get_by_role('button',name='hello.txt',exact=True).click()
        preview=page.locator('.file-preview')
        preview.wait_for()
        assert preview.text_content()==content[:4096].decode(), 'preview altered source line breaks'
        assert preview.evaluate('(el)=>parseFloat(getComputedStyle(el).lineHeight)')<=18
        viewport=page.locator('.file-preview-scroll .mantine-ScrollArea-viewport')
        assert viewport.evaluate('(el)=>{el.scrollTop=100;return el.scrollTop>0}'), 'preview cannot scroll'
        viewport.evaluate('(el)=>{el.scrollTop=0}')
        page.wait_for_timeout(250)
        page.screenshot(path=str(reports / 'web-efs-preview-text.png'))
        page.get_by_role('dialog').get_by_text('HEX',exact=True).click()
        expect(preview).to_have_attribute('data-mode','hex')
        expect(viewport).to_have_js_property('scrollTop',0)
        assert preview.inner_text().startswith('00000000  6c 69 6e 65'), 'HEX layout changed'
        page.screenshot(path=str(reports / 'web-efs-preview-hex.png'))
        page.set_viewport_size({'width':390,'height':844})
        page.wait_for_timeout(250)
        assert page.get_by_role('dialog').bounding_box()['width']<=390
        assert page.locator('.mantine-Modal-body').evaluate('(el)=>el.scrollWidth<=el.clientWidth'), 'preview made dialog controls overflow'
        assert viewport.bounding_box()['width']<=page.get_by_role('dialog').bounding_box()['width']
        page.screenshot(path=str(reports / 'web-efs-preview-mobile.png'))
        page.get_by_role('dialog').get_by_role('button',name='关闭',exact=True).click()
        expect(page.get_by_role('dialog')).to_have_count(0)
        page.set_viewport_size({'width':1360,'height':950})
        page.get_by_role('button',name='切换明暗主题',exact=True).click()
        content=short_content
        # All following writes target this in-memory HTTP fixture, never a modem.
        def edit_hex():
            page.get_by_role("button", name="操作 hello.txt", exact=True).click()
            page.get_by_role("menuitem", name="编辑 HEX", exact=True).click()
            page.get_by_role("dialog", name="编辑 HEX", exact=True).wait_for()
        edit_hex()
        text = page.get_by_role("textbox", name="连续 HEX", exact=False)
        text.fill("invalid")
        page.get_by_role("button", name="保存文件", exact=True).click()
        page.get_by_text("HEX 必须为完整的十六进制字节", exact=True).wait_for()
        text.fill("616263")
        page.get_by_role("dialog").get_by_role("button", name="取消", exact=True).click()
        page.get_by_role("button", name="继续编辑", exact=True).click()
        assert text.input_value() == "616263"
        page.evaluate("location.hash='overview'")
        page.get_by_text("放弃未保存的修改？", exact=True).wait_for()
        page.keyboard.press("Escape")
        page.wait_for_timeout(200)
        page.wait_for_url("**#efs")
        assert text.input_value() == "616263", "back navigation lost the EFS editor"
        content += b"external edit"
        page.get_by_role("button", name="保存文件", exact=True).click()
        page.get_by_text("文件内容已变化，请重新打开后编辑", exact=True).first.wait_for()
        assert not [r for r in requests if r[0] == "POST"], "stale editor overwrote newer content"
        page.wait_for_timeout(250)
        edit_hex(); page.get_by_role("textbox", name="连续 HEX", exact=False).fill("616263")
        page.get_by_role("button", name="保存文件", exact=True).click()
        page.get_by_text("文件已保存", exact=True).wait_for()
        assert content == b"abc" and len([r for r in requests if r[0] == "POST"]) == 1
        page.wait_for_timeout(250)
        edit_hex(); page.get_by_role("textbox", name="连续 HEX", exact=False).fill("636465")
        page.evaluate("location.hash='nv'")
        page.get_by_role("button", name="放弃修改", exact=True).click()
        page.get_by_role("button", name="读取 NV", exact=True).wait_for()
        page.wait_for_timeout(220)
        expect(page.get_by_role("dialog")).to_have_count(0, timeout=3000)
        assert len([r for r in requests if r[0] == "POST"]) == 1
        page.get_by_text("NV 工具", exact=True).first.click()
        page.get_by_role("button", name="读取 NV", exact=True).click()
        page.get_by_text("866834080042000", exact=True).wait_for()
        page.get_by_role("textbox", name="IMEI", exact=True).fill("866834080042000")
        page.get_by_role("button", name="生成预览", exact=True).click()
        page.get_by_text("par=0x08,0x8A", exact=False).wait_for()
        page.get_by_text("信令分析", exact=True).first.click()
        expect(page.get_by_role("button", name="开始抓取", exact=True)).to_be_enabled() # The fixture rejects live streams; no stop/click race.
        page.get_by_role("button", name="开始抓取", exact=True).wait_for()
        page.get_by_role('button',name='展开实时采集',exact=True).click()
        packet = bytearray(40); struct.pack_into("<HH", packet, 0, 40, 0xB8D8); struct.pack_into("<I", packet, 12, 0x30002); struct.pack_into("<i", packet, 24, 128*20)
        page.locator('input[type="file"][accept=".bin"]').set_input_files({"name": "fixture.bin", "mimeType": "application/octet-stream", "buffer": struct.pack("<I", len(packet)) + packet})
        page.get_by_text("导入完成", exact=True).wait_for()
        page.get_by_text("NR 服务 SNR", exact=True).wait_for()
        page.screenshot(path=str(reports / "web-signal-desktop.png"), full_page=True)
        page.get_by_text("NR 服务 SNR", exact=True).click()
        page.get_by_role("button", name="关闭", exact=True).click()
        page.get_by_text("列表已暂停", exact=True).wait_for()
        page.wait_for_timeout(250)
        page.set_viewport_size({"width": 390, "height": 844})
        page.get_by_role("button", name="打开导航", exact=True).click()
        page.get_by_role("dialog").get_by_role("button", name="设备概览", exact=True).click()
        page.wait_for_timeout(350)
        assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), "mobile page overflows horizontally"
        page.screenshot(path=str(reports / "web-overview-mobile.png"), full_page=True)
        page.get_by_role("button", name="切换明暗主题", exact=True).click()
        page.screenshot(path=str(reports / "web-overview-dark.png"), full_page=True)
        page.get_by_role("button", name="退出登录", exact=True).click()
        page.get_by_role("button", name="登录设备", exact=True).wait_for()
        # Password opt-in, auto re-entry, explicit logout, failure, cancel and token isolation.
        page.set_viewport_size({'width':1360,'height':950})
        def login_keys():
            return page.evaluate("Object.keys(localStorage).filter(k=>k.startsWith('datad.web.login.v1:'))")
        assert not login_keys()
        page.get_by_role('checkbox',name='自动登录',exact=True).check()
        expect(page.get_by_role('checkbox',name='记住密码',exact=True)).to_be_checked()
        page.get_by_label('管理密码',exact=False).fill('wrong-fixture-password')
        page.get_by_role('button',name='登录设备',exact=True).click()
        page.get_by_text('请检查用户名和密码。',exact=True).wait_for()
        assert not login_keys(), 'failed credentials were saved'
        page.get_by_label('管理密码',exact=False).fill('test-password-not-real')
        page.get_by_role('button',name='登录设备',exact=True).click()
        page.get_by_role('heading',name='设备概览',exact=True).wait_for()
        assert len(login_keys())==1
        expect(page.locator('.app-sidebar .github-card')).to_be_visible()
        before=len(auth_attempts)
        page.reload()
        page.get_by_role('heading',name='设备概览',exact=True).wait_for()
        assert len(auth_attempts)==before+1, 'auto login did not make exactly one attempt'
        expect(page.locator('.trend-panel').get_by_role('radio',name='RSRP',exact=True)).to_be_checked()
        page.locator('.trend-panel').get_by_text('速率',exact=True).click()
        expect(page.locator('.trend-panel').get_by_role('radio',name='速率',exact=True)).to_be_checked()
        page.get_by_role('button',name='退出登录',exact=True).click()
        expect(page.get_by_label('管理密码',exact=False)).to_have_value('test-password-not-real')
        before=len(auth_attempts)
        page.reload()
        page.get_by_role('button',name='登录设备',exact=True).wait_for();page.wait_for_timeout(350)
        assert len(auth_attempts)==before, 'explicit logout immediately auto-logged in again'
        page.get_by_role('checkbox',name='自动登录',exact=True).uncheck()
        page.reload()
        expect(page.get_by_label('管理密码',exact=False)).to_have_value('test-password-not-real')
        expect(page.get_by_role('checkbox',name='自动登录',exact=True)).not_to_be_checked()
        page.get_by_role('checkbox',name='自动登录',exact=True).check()
        page.get_by_role('button',name='登录设备',exact=True).click()
        page.get_by_role('heading',name='设备概览',exact=True).wait_for()
        before=len(auth_attempts);auth_delay=2
        page.reload()
        page.get_by_role('button',name='取消自动登录',exact=True).wait_for()
        for _ in range(50):
            if len(auth_attempts)>before:break
            page.wait_for_timeout(20)
        assert len(auth_attempts)==before+1
        page.get_by_role('button',name='取消自动登录',exact=True).click()
        expect(page.get_by_role('button',name='登录设备',exact=True)).to_be_enabled()
        auth_delay=0
        key=login_keys()[0]
        page.evaluate("""key=>{const p=JSON.parse(localStorage.getItem(key));p.password='wrong-fixture-password';p.auto=true;localStorage.setItem(key,JSON.stringify(p));sessionStorage.clear();}""",key)
        before=len(auth_attempts)
        page.reload()
        page.get_by_text('自动登录失败：请检查用户名和密码。',exact=True).wait_for()
        page.wait_for_timeout(500)
        assert len(auth_attempts)==before+1, 'failed auto login looped'
        expect(page.get_by_role('checkbox',name='自动登录',exact=True)).not_to_be_checked()
        page.get_by_role('button',name='清除登录信息',exact=True).click()
        expect(page.get_by_label('管理密码',exact=False)).to_have_value('')
        assert not login_keys()
        expect(page.get_by_role('checkbox',name='记住密码',exact=True)).not_to_be_checked()
        page.get_by_text('使用服务 Token 登录',exact=True).click()
        page.get_by_label('服务 Token',exact=True).fill(TOKEN)
        page.get_by_role('button',name='连接',exact=True).click()
        page.get_by_role('heading',name='设备概览',exact=True).wait_for()
        assert not login_keys(), 'service Token login created saved credentials'
        expect(page.locator('.trend-panel').get_by_role('radio',name='速率',exact=True)).to_be_checked()
        assert not page.evaluate('(token)=>JSON.stringify(localStorage).includes(token)',TOKEN), 'Token persisted in browser storage'
        page.get_by_role('button',name='退出登录',exact=True).click()
        page.set_viewport_size({'width':390,'height':844})
        expect(page.get_by_role('heading',name='高级管理控制台',exact=True)).to_be_visible()
        expect(page.get_by_role('link',name='zwrt-datad 开源项目',exact=False)).to_be_visible()
        assert page.evaluate('document.documentElement.scrollWidth<=innerWidth')
        page.wait_for_timeout(250)
        page.screenshot(path=str(reports/'web-login-mobile.png'),full_page=True)
        assert not errors, errors
        assert not page.evaluate("window.__cspErrors"), "CSP blocked a bundled resource"
        assert all(not query or TOKEN not in query for _, _, query in requests)
        browser.close()
        print("Browser checks passed: React/Mantine/Motion, desktop/mobile, proxy prefix, exact IDs, EFS guard/countdown, NV, worker import, pause and modal exits.")
finally:
    server.shutdown()
    server.server_close()
