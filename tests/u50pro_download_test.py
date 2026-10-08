#!/usr/bin/env python3
"""Exercise the download installer against fake firmware; no real device/network."""
import importlib.util
import base64
import gzip
import hashlib
import os
import re
import subprocess
import unittest
from pathlib import Path

import u50pro_deploy_test as deploy

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("package_u50pro", ROOT / "scripts/package-u50pro.py")
package = importlib.util.module_from_spec(spec)
spec.loader.exec_module(package)

MOCK = r'''#!/usr/bin/env python3
import os,sys,shutil
from pathlib import Path
base=Path(os.environ['SANDBOX']); name=Path(sys.argv[0]).name
fault=os.environ.get('DOWNLOAD_FAULT','')
if name=='id': print('1000' if fault=='root' else '0')
elif name=='uname': print('aarch64' if fault=='machine' else 'armv7l')
elif name=='cfg': print('U50S' if fault=='model' else 'MU5120')
elif name=='df': print('Filesystem 1024-blocks Used Available Capacity Mounted\ncache 100000 0 '+('1' if fault=='space' else '100000')+' 0% /cache')
elif name=='timeout':
 if sys.argv[-1]=='--version': print('zwrt-datad '+('0.0.0' if fault=='version' else '9.9.9'))
 else: os.execv('/usr/bin/timeout', ['timeout']+sys.argv[1:])
elif name in ('curl','tiny'):
 (base/'download-called').touch()
 args=sys.argv[1:]
 if name=='curl':
  assert args[args.index('--proto')+1]=='=http,https'
  assert args[args.index('--proto-redir')+1]=='=http,https'
 assert 'https://downloads.example.test/u50pro/zwrt-datad-armv7' in args
 if name=='tiny':
  assert len(args)==1 or args[1]=='--insecure'
  dst=Path('/dev/stdout')
 else: dst=Path(args[args.index('-o')+1])
 if fault=='download': dst.write_bytes(b'partial'); sys.exit(22)
 payload=(base/'payload').read_bytes()
 if fault=='hash': payload=payload[:-1]+b'x'
 if fault=='size': payload=payload[:-1]
 dst.write_bytes(payload)
else: raise RuntimeError(name)
'''


class DownloadTest(unittest.TestCase):
    def setUp(self):
        self.fixture = deploy.DeployTest()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.base = self.fixture.base
        self.data = self.fixture.data
        # ASCII ELF32 header allows the fake service harness to inspect its version.
        self.binary = b'\x7fELF\x01\x01\x01' + bytes(9) + b'\x02\x00\x28\x00' + bytes(44) + b'9.9.9\n'
        (self.base / 'payload').write_bytes(self.binary)
        for name in ('id', 'uname', 'cfg', 'df', 'curl', 'timeout'):
            path = self.fixture.tools / name
            path.write_text(MOCK)
            path.chmod(0o755)
        self.env = dict(self.fixture.env, DATAD_BASE_URL='https://downloads.example.test/u50pro')

    def run_install(self, fault='', deploy_fault='', binary=None, success=True):
        script = package.render_installer(binary or self.binary, '9.9.9')
        for old, new in [('/cache/zwrt-datad', str(self.data)),
                         ('/etc/systemd/system', str(self.fixture.system)),
                         ('PROC=/proc', f'PROC={self.fixture.proc}'),
                         ('MOUNTS=/proc/mounts', f'MOUNTS={self.base}/mounts'),
                         ('UBI_RO=/sys/class/ubi/ubi0/ro_mode', f'UBI_RO={self.base}/ubi_ro'),
                         ('WAIT_SECONDS=30', 'WAIT_SECONDS=4'),
                         ('STABLE_SECONDS=10', 'STABLE_SECONDS=1'),
                         ('sleep 2', '/bin/sleep 0.05')]:
            script = script.replace(old, new)
        path = self.base / 'install.sh'
        path.write_text(script)
        subprocess.run(['sh', '-n', str(path)], check=True)
        proc = subprocess.run(['sh', str(path)], env=dict(self.env, DOWNLOAD_FAULT=fault, FAULT=deploy_fault),
                              capture_output=True, text=True, timeout=25)
        self.assertEqual(proc.returncode == 0, success, (proc.stdout, proc.stderr))
        return proc

    def assert_untouched(self):
        self.fixture.assert_restored()
        self.assertNotIn("'stop'", (self.base / 'calls').read_text())

    def test_download_to_verified_install(self):
        result = self.run_install()
        self.assertIn('SUCCESS: persistent boot autostart', result.stdout)
        self.assertEqual((self.data / 'zwrt-datad').read_bytes(), self.binary)
        self.assertEqual((self.data / 'zwrt-datad.prev').read_text(), self.fixture.binary('0.0.1'))
        unit = self.fixture.unit.read_text()
        self.assertIn('After=local-fs.target', unit)
        self.assertIn('--u50-enable-webshell', unit)
        self.assertNotIn('After=multi-user.target', unit)

    def test_download_and_transaction_with_legacy_timeout(self):
        self.fixture.use_timeout('legacy')
        self.env.update(TIMEOUT_STYLE='legacy', TIMEOUT_VERSION='9.9.9')
        result = self.run_install()
        self.assertIn('SUCCESS: persistent boot autostart', result.stdout)
        self.assertEqual((self.data/'zwrt-datad').read_bytes(), self.binary)

    def test_unsupported_timeout_does_not_stop_old_service(self):
        self.fixture.use_timeout('unsupported')
        self.env['TIMEOUT_STYLE'] = 'unsupported'
        result = self.run_install(success=False)
        self.assertIn('unsupported timeout utility', result.stderr)
        self.assert_untouched()

    def test_tiny_download_to_verified_install(self):
        tiny = self.fixture.tools / 'tiny'
        tiny.write_text(MOCK)
        tiny.chmod(0o755)
        # Selecting the helper must not invoke even an installed curl.
        (self.fixture.tools / 'curl').write_text('#!/bin/sh\nexit 99\n')
        self.env['DATAD_DOWNLOADER'] = str(tiny)
        self.env['DATAD_TLS_INSECURE'] = '1'
        result = self.run_install()
        self.assertIn('SUCCESS: persistent boot autostart', result.stdout)
        self.assertEqual((self.data / 'zwrt-datad').read_bytes(), self.binary)

    def test_tiny_partial_download_never_stops_installed_service(self):
        tiny = self.fixture.tools / 'tiny'
        tiny.write_text(MOCK)
        tiny.chmod(0o755)
        self.env['DATAD_DOWNLOADER'] = str(tiny)
        result = self.run_install(fault='download', success=False)
        self.assertIn('Download attempt 3/3', result.stdout)
        self.assert_untouched()

    def test_preflight_rejections_do_not_download_or_stop(self):
        for fault in ('root', 'machine', 'model', 'space'):
            with self.subTest(fault=fault):
                self.run_install(fault=fault, success=False)
                self.assertFalse((self.base / 'download-called').exists())
                self.assert_untouched()

    def test_bad_downloads_do_not_stop_service(self):
        for fault in ('download', 'hash', 'size', 'version'):
            with self.subTest(fault=fault):
                self.run_install(fault=fault, success=False)
                self.assert_untouched()

    def test_deployment_failure_rolls_back(self):
        result = self.run_install(deploy_fault='start', success=False)
        self.assertIn('previous installation restored', result.stderr)
        self.fixture.assert_restored()

    def test_session_only_install_is_explicit(self):
        subprocess.run(['systemctl', 'stop', 'zwrt-datad.service'], env=self.env, check=True)
        for name in ('zwrt-datad', 'start.sh', 'service-control.sh', 'zwrt-datad.service'):
            (self.data / name).unlink()
        self.fixture.unit.unlink()
        (self.fixture.system / 'multi-user.target.wants/zwrt-datad.service').unlink()
        (self.base / 'ubi_ro').write_text('1\n')
        result = self.run_install()
        self.assertIn('SUCCESS: session;', result.stdout)
        self.assertIn('No boot autostart', result.stdout)
        self.assertNotIn('mount [', (self.base / 'calls').read_text())

    def test_missing_url_and_lock_fail_before_download(self):
        self.env['DATAD_BASE_URL'] = ''
        self.run_install(success=False)
        self.env['DATAD_BASE_URL'] = 'https://downloads.example.test/u50pro'
        (self.data / '.deploy-lock').mkdir()
        self.run_install(success=False)
        self.assertFalse((self.base / 'download-called').exists())
        self.assert_untouched()

    def test_packager_rejects_wrong_binary_and_unsafe_url(self):
        for binary in (b'bad', self.binary[:18] + b'\xb7\x00' + self.binary[20:]):
            with self.assertRaises(ValueError):
                package.render_installer(binary, '9.9.9')
        self.assertIn('http://host/path', package.render_installer(self.binary, '9.9.9', 'http://host/path'))
        for url in ('ftp://host/path', "https://host/'", 'https://user:pass@host/path', 'https://host/path?x=1'):
            with self.assertRaises(ValueError):
                package.render_installer(self.binary, '9.9.9', url)

    def test_bootstrap_pins_helper_and_script_and_has_no_default_server(self):
        installer = package.render_installer(self.binary, '9.9.9')
        bootstrap = package.render_bootstrap(installer, self.binary)
        encoded = re.search(r"printf %s '([^']+)'", bootstrap)[1]
        self.assertEqual(gzip.decompress(base64.b64decode(encoded)), self.binary)
        self.assertIn(hashlib.sha256(self.binary).hexdigest(), bootstrap)
        self.assertIn(hashlib.sha256(installer.encode()).hexdigest(), bootstrap)
        self.assertIn("${DATAD_BASE_URL:-${1:-''}}", bootstrap)
        self.assertNotIn('@PAYLOAD@', bootstrap)
        subprocess.run(['sh', '-n'], input=bootstrap, text=True, check=True)

    def test_corrupted_embedded_downloader_is_rejected_before_execution(self):
        bootstrap = package.render_bootstrap('never executed', self.binary)
        encoded = re.search(r"printf %s '([^']+)'", bootstrap)[1]
        bootstrap = bootstrap.replace(encoded, base64.b64encode(gzip.compress(b'wrong helper')).decode())
        proc = subprocess.run(['sh'], input=bootstrap, env=self.env, capture_output=True, text=True, timeout=5)
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn('did NOT match', proc.stderr)
        self.assertNotIn('Downloading checked installer', proc.stdout)
        self.assert_untouched()


if __name__ == '__main__':
    unittest.main(verbosity=2)
