#!/usr/bin/env python3
"""U50 Pro deployment fault injection. Only temp files/fake proc and tools.

Run with Linux/WSL python3; no adb, mounts, signals to real services or modem.
"""
import hashlib
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

MOCK = r'''#!/usr/bin/env python3
import os,sys,shutil,json
from pathlib import Path
base=Path(os.environ['SANDBOX']); data=base/'data'; proc=base/'proc'
unit=base/'system/zwrt-datad.service'; state=base/'state'
fault=os.environ.get('FAULT',''); name=Path(sys.argv[0]).name; args=sys.argv[1:]
with (base/'calls').open('a') as f: f.write(name+' '+repr(args)+'\n')
def once(key):
 p=base/key
 if p.exists(): return False
 p.touch(); return True
def is_new(): return '9.9.9' in (data/'zwrt-datad').read_text()
def stop():
 for p in proc.glob('[0-9]*'): shutil.rmtree(p)
 (proc/'net/tcp').write_text(''); state.write_text('inactive')
def start():
 if fault=='start' and is_new(): state.write_text('failed'); sys.exit(1)
 p=proc/'4242'; (p/'fd').mkdir(parents=True,exist_ok=True)
 (p/'exe').unlink(missing_ok=True); (p/'exe').symlink_to(data/'zwrt-datad')
 (p/'fd/5').unlink(missing_ok=True); (p/'fd/5').symlink_to('socket:[111]')
 (p/'stat').write_text('4242 (zwrt-datad) S '+'0 '*18+'123\n')
 (proc/'net/tcp').write_text(f'0: 0100007F:{9460:04X} 00000000:0000 0A 0 0 0 0 0 111\n' if fault!='health' or not is_new() else '')
 state.write_text('active')
if name in ('systemctl','systemd-run'):
 if name=='systemd-run': (base/'transient').touch(); start(); sys.exit(0)
 cmd=next((x for x in args if x in ('cat','show','stop','start','reset-failed','daemon-reload')), '')
 if fault=='ctl-timeout' and cmd=='cat': sys.exit(124)
 if cmd=='cat':
  if not unit.exists() and not (base/'transient').exists(): sys.exit(1)
  print('[Service]\nExecStart='+str(data/'zwrt-datad')+' --u50-model u50pro'); sys.exit(0)
 if cmd=='show':
  active=state.read_text() if state.exists() else 'inactive'
  print(('4242' if active=='active' else '0') if 'MainPID' in args else active); sys.exit(0)
 if cmd=='stop': stop()
 if cmd=='start':
  if fault=='queued-start' and '--no-block' not in args: sys.exit(1)
  start()
 if cmd=='daemon-reload' and fault=='reload' and once('reload-failed'): sys.exit(1)
elif name=='mount':
 readonly='remount,ro' in args
 if readonly and fault=='remount-ro' and once('remount-failed'): sys.exit(1)
 if not readonly and fault=='remount-rw': sys.exit(1)
 (base/'mounts').write_text('ubi0:rootfs / ubifs '+('ro' if readonly else 'rw')+',relatime 0 0\n')
elif name=='cp':
 if args[-1]==str(data/'zwrt-datad.deploy-new') and fault=='interrupt' and once('interrupted'):
  import signal
  os.kill(os.getppid(),signal.SIGTERM)
 if args[-1]==str(data/'zwrt-datad.deploy-new') and fault=='detached':
  import time
  for _ in range(100):
   if (base/'continue-detached').exists(): break
   time.sleep(.05)
  else: sys.exit(1)
 if args[-1]==str(unit)+'.deploy-new' and fault=='unit-write' and once('write-failed'): sys.exit(1)
 if args[-1]==str(data/'zwrt-datad.deploy-new') and fault=='copy' and once('copy-failed'): sys.exit(1)
 src,dst=args[-2:]; shutil.copy2(src,dst)
elif name=='sleep':
 p=proc/'uptime'; now=float(p.read_text().split()[0]); p.write_text(f'{now+1:.2f} 0.00\n')
 if fault=='restart-loop' and is_new() and (proc/'4242/stat').exists():
  p=proc/'4242/stat'; fields=p.read_text().split(); fields[21]=str(int(fields[21])+1); p.write_text(' '.join(fields))
elif name=='sync': pass
else: raise RuntimeError(name)
'''


class DeployTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='u50pro-deploy-test-')
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.data = self.base / 'data'
        self.stage = self.data / '.deploy.test'
        self.proc = self.base / 'proc'
        self.system = self.base / 'system'
        self.tools = self.base / 'bin'
        for directory in (self.stage, self.proc/'net', self.system/'multi-user.target.wants', self.tools):
            directory.mkdir(parents=True)
        for name in ('tcp', 'tcp6'): (self.proc/'net'/name).write_text('')
        (self.proc/'uptime').write_text('0.00 0.00\n')
        (self.base/'mounts').write_text('ubi0:rootfs / ubifs ro,relatime 0 0\n')
        (self.base/'ubi_ro').write_text('0\n')
        for name in ('systemctl', 'systemd-run', 'mount', 'cp', 'sleep', 'sync'):
            path=self.tools/name; path.write_text(MOCK); path.chmod(0o755)
        self.env=dict(os.environ, SANDBOX=str(self.base), PATH=f'{self.tools}:{os.environ["PATH"]}')
        for file in ('zwrt-datad', 'start.sh', 'service-control.sh', 'zwrt-datad.service'):
            (self.data/file).write_text(self.binary('0.0.1') if file=='zwrt-datad' else 'old '+file+'\n')
            (self.data/file).chmod(0o700)
        self.unit=self.system/'zwrt-datad.service'; self.unit.write_text('old unit\n')
        (self.system/'multi-user.target.wants/zwrt-datad.service').symlink_to('../zwrt-datad.service')
        (self.stage/'zwrt-datad').write_text(self.binary('9.9.9')); (self.stage/'zwrt-datad').chmod(0o700)
        (self.stage/'start.sh').write_text('new start\n')
        (self.stage/'zwrt-datad.service').write_text('new unit\n')
        helper=(ROOT/'scripts/u50pro-service.sh').read_text().replace('DIR=/cache/zwrt-datad',f'DIR={self.data}')
        helper=helper.replace('PROC=/proc',f'PROC={self.proc}').replace('WAIT_SECONDS=30','WAIT_SECONDS=4').replace('STABLE_SECONDS=10','STABLE_SECONDS=1')
        (self.stage/'service-control.sh').write_text(helper)
        transaction=(ROOT/'scripts/u50pro-deploy-transaction.sh').read_text()
        for old,new in [('DIR=/cache/zwrt-datad',f'DIR={self.data}'),('/etc/systemd/system',str(self.system)),
                        ('MOUNTS=/proc/mounts',f'MOUNTS={self.base}/mounts'),('UBI_RO=/sys/class/ubi/ubi0/ro_mode',f'UBI_RO={self.base}/ubi_ro')]:
            transaction=transaction.replace(old,new)
        (self.stage/'deploy-transaction.sh').write_text(transaction)
        self.manifest()
        subprocess.run(['systemctl','start','zwrt-datad.service'],env=self.env,check=True)

    @staticmethod
    def binary(version): return f'#!/bin/sh\necho "zwrt-datad {version}"\n'

    def manifest(self):
        files=('zwrt-datad','start.sh','zwrt-datad.service','service-control.sh','deploy-transaction.sh')
        (self.stage/'SHA256SUMS').write_text(''.join(f'{hashlib.sha256((self.stage/f).read_bytes()).hexdigest()}  {f}\n' for f in files))

    def run_deploy(self, fault='', success=True):
        p=subprocess.run(['sh',str(self.stage/'deploy-transaction.sh'),str(self.stage)],
                         env=dict(self.env,FAULT=fault),capture_output=True,text=True,timeout=20)
        self.assertEqual(p.returncode==0,success,(p.stdout,p.stderr))
        self.assertTrue((self.stage/'result').read_text().startswith('SUCCESS' if success else 'FAILED'))
        self.assertFalse((self.data/'.deploy-lock').exists())
        return p

    def assert_restored(self):
        self.assertEqual((self.data/'zwrt-datad').read_text(),self.binary('0.0.1'))
        self.assertEqual((self.data/'start.sh').read_text(),'old start.sh\n')
        self.assertEqual(self.unit.read_text(),'old unit\n')
        self.assertIn(' ubifs ro,',(self.base/'mounts').read_text())
        self.assertEqual((self.base/'state').read_text(),'active')

    def test_success_keeps_previous_binary_and_restores_ro(self):
        self.run_deploy()
        self.assertEqual((self.data/'zwrt-datad').read_text(),self.binary('9.9.9'))
        self.assertEqual((self.data/'zwrt-datad.prev').read_text(),self.binary('0.0.1'))
        self.assertEqual(self.unit.read_text(),'new unit\n')
        self.assertIn(' ubifs ro,',(self.base/'mounts').read_text())

    def test_systemctl_timeout_never_treated_as_missing_service(self):
        result = self.run_deploy('ctl-timeout', success=False)
        self.assertIn('systemctl timed out', result.stderr)
        self.assert_restored()
        self.assertNotIn("'stop'", (self.base/'calls').read_text())

    def test_start_does_not_wait_for_unrelated_systemd_jobs(self):
        self.run_deploy('queued-start')
        self.assertIn("'--no-block', 'start'", (self.base/'calls').read_text())

    def test_health_uses_elapsed_time_including_probe_cost(self):
        helper = self.stage/'service-control.sh'
        # Every probe costs two simulated seconds; sleep advances one more.
        # A four-second deadline must stop after two probes, not four sleeps.
        harness = r'''
. "$1"
healthy_token() {
    n=$(cat "$SANDBOX/probes" 2>/dev/null || echo 0)
    echo $((n + 1)) > "$SANDBOX/probes"
    now=$(awk '{print int($1)}' "$PROC/uptime")
    echo "$((now + 2)).00 0.00" > "$PROC/uptime"
    return 1
}
wait_healthy unused
'''
        p = subprocess.run(['sh', '-c', harness, 'test', str(helper)], env=self.env,
                           capture_output=True, text=True, timeout=5)
        self.assertNotEqual(p.returncode, 0)
        self.assertEqual((self.base/'probes').read_text().strip(), '2')
        self.assertIn('within 4 seconds', p.stderr)

    def test_cached_health_pid_avoids_full_process_rescan(self):
        digest = hashlib.sha256((self.data/'zwrt-datad').read_bytes()).hexdigest()
        harness = r'''
. "$1"
owned_pids() { echo unexpected-scan >&2; return 1; }
healthy_token "$2" '4242:123'
'''
        p = subprocess.run(['sh', '-c', harness, 'test', str(self.stage/'service-control.sh'), digest],
                           env=self.env, capture_output=True, text=True, timeout=5)
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(p.stdout.strip(), '4242:123')
        self.assertNotIn('unexpected-scan', p.stderr)

    def test_failures_restore_files_service_and_mount(self):
        for fault in ('start','health','restart-loop','unit-write','reload','copy','remount-ro','interrupt'):
            with self.subTest(fault=fault):
                # Each transaction is isolated, just like an independent modem.
                case=DeployTest(); case.setUp()
                try:
                    case.run_deploy(fault,success=False); case.assert_restored()
                finally: case.doCleanups()

    def test_hash_mismatch_does_not_stop_existing_service(self):
        (self.stage/'zwrt-datad').write_text('tampered')
        self.run_deploy(success=False); self.assert_restored()
        self.assertNotIn("'stop'",(self.base/'calls').read_text())

    def test_readonly_ubi_never_attempts_remount(self):
        (self.base/'ubi_ro').write_text('1\n')
        self.run_deploy()
        self.assertNotIn('mount [',(self.base/'calls').read_text())
        self.assertEqual(self.unit.read_text(),'old unit\n')

    def test_failed_rw_remount_falls_back_without_changing_root(self):
        self.run_deploy('remount-rw')
        self.assertEqual(self.unit.read_text(),'old unit\n')
        self.assertIn(' ubifs ro,',(self.base/'mounts').read_text())

    def test_first_install_on_locked_ubi_uses_transient_unit(self):
        subprocess.run(['systemctl','stop','zwrt-datad.service'],env=self.env,check=True)
        for name in ('zwrt-datad','start.sh','service-control.sh','zwrt-datad.service'): (self.data/name).unlink()
        self.unit.unlink(); (self.system/'multi-user.target.wants/zwrt-datad.service').unlink()
        (self.base/'ubi_ro').write_text('1\n')
        self.run_deploy()
        self.assertTrue((self.base/'transient').exists())
        self.assertFalse(self.unit.exists())
        self.assertFalse((self.data/'zwrt-datad.prev').exists())

    def test_matching_binary_without_socket_ownership_is_rejected(self):
        (self.proc/'4242/fd/5').unlink()
        self.run_deploy(success=False)
        self.assertEqual((self.data/'zwrt-datad').read_text(),self.binary('0.0.1'))
        self.assertNotIn("'stop'",(self.base/'calls').read_text())

    def test_original_rw_mount_is_preserved(self):
        (self.base/'mounts').write_text('ubi0:rootfs / ubifs rw,relatime 0 0\n')
        self.run_deploy()
        self.assertNotIn('mount [',(self.base/'calls').read_text())

    def test_detached_transaction_finishes_after_launcher_exits(self):
        import shlex
        import time
        cmd=f'nohup sh {shlex.quote(str(self.stage/"deploy-transaction.sh"))} {shlex.quote(str(self.stage))} > {shlex.quote(str(self.stage/"deploy.log"))} 2>&1 < /dev/null &'
        subprocess.run(['sh','-c',cmd],env=dict(self.env,FAULT='detached'),check=True,timeout=5)
        self.assertFalse((self.stage/'result').exists())
        (self.base/'continue-detached').touch()
        for _ in range(100):
            if (self.stage/'result').exists() and not (self.data/'.deploy-lock').exists(): break
            time.sleep(.05)
        self.assertTrue((self.stage/'result').read_text().startswith('SUCCESS'))
        self.assertIn(' ubifs ro,',(self.base/'mounts').read_text())

    def test_foreign_listener_does_not_stop_anything(self):
        subprocess.run(['systemctl','stop','zwrt-datad.service'],env=self.env,check=True)
        (self.base/'calls').write_text('')
        (self.proc/'net/tcp').write_text(f'0: 0100007F:{9460:04X} 00000000:0000 0A 0 0 0 0 0 999\n')
        self.run_deploy(success=False)
        self.assertNotIn("'stop'",(self.base/'calls').read_text())
        self.assertEqual((self.data/'zwrt-datad').read_text(),self.binary('0.0.1'))

    def test_lock_refuses_concurrent_deployment(self):
        (self.data/'.deploy-lock').mkdir()
        p=subprocess.run(['sh',str(self.stage/'deploy-transaction.sh'),str(self.stage)],env=self.env,capture_output=True)
        self.assertNotEqual(p.returncode,0)
        self.assertTrue((self.data/'.deploy-lock').exists())
        self.assertEqual((self.data/'zwrt-datad').read_text(),self.binary('0.0.1'))

    def test_host_rejects_wrong_machine_before_adb(self):
        adb=self.tools/'adb'; adb.write_text('#!/bin/sh\ntouch "$SANDBOX/adb-called"\nexit 1\n'); adb.chmod(0o755)
        for machine in (b'\x03\x00',b'\xb7\x00'):
            path=self.base/'wrong-elf'; path.write_bytes(b'\x7fELF\x01\x01\x01'+bytes(9)+b'\x02\x00'+machine+bytes(64))
            p=subprocess.run(['bash',str(ROOT/'scripts/deploy-u50pro.sh'),str(path)],env=self.env,capture_output=True)
            self.assertNotEqual(p.returncode,0)
            self.assertFalse((self.base/'adb-called').exists())

    def test_host_quoting_and_complete_staging_with_mock_adb(self):
        adb=self.tools/'adb'
        adb.write_text('''#!/usr/bin/env python3
import os,sys,json,subprocess
from pathlib import Path
base=Path(os.environ['SANDBOX']); args=sys.argv[1:]
if args[:1]==['-s']: args=args[2:]
with (base/'adb-calls').open('a') as f: f.write(json.dumps(args)+'\\n')
if args==['get-state']: print('device\\r'); sys.exit(0)
if args[0]=='push': sys.exit(0)
cmd=args[1]
subprocess.run(['sh','-n','-c',cmd],check=True)
if 'mktemp -d' in cmd: print('/cache/zwrt-datad/.deploy.ABC123')
elif 'cat /cache/zwrt-datad/.deploy.ABC123/result' in cmd: print('SUCCESS: simulated')
'''); adb.chmod(0o755)
        elf=self.base/'armv7'; elf.write_bytes(b'\x7fELF\x01\x01\x01'+bytes(9)+b'\x02\x00\x28\x00'+bytes(64))
        p=subprocess.run(['bash',str(ROOT/'scripts/deploy-u50pro.sh'),'--serial','mock',str(elf)],
                         env=self.env,capture_output=True,text=True,timeout=20)
        self.assertEqual(p.returncode,0,(p.stdout,p.stderr))
        import json
        calls=[json.loads(line) for line in (self.base/'adb-calls').read_text().splitlines()]
        pushes=[args for args in calls if args[0]=='push']
        self.assertEqual(len(pushes),6)
        free=next(args[1] for args in calls if args[0]=='shell' and 'free=' in args[1])
        self.assertIn('test "$free" -ge ',free)
        self.assertIn("awk '{print $4}'",free)


class RealSocketHealthTest(unittest.TestCase):
    """Use real Linux procfs/socket ownership, independent of the fake tables."""

    def test_actual_port_9460_and_ten_second_stability(self):
        import select
        import sys
        import time
        with tempfile.TemporaryDirectory(prefix='u50pro-real-socket-') as tmp:
            data = Path(tmp)
            binary = data/'zwrt-datad'
            shutil.copy2(Path(sys.executable).resolve(), binary)
            helper = data/'service-control.sh'
            helper.write_text((ROOT/'scripts/u50pro-service.sh').read_text().replace(
                'DIR=/cache/zwrt-datad', f'DIR={data}'))
            digest = hashlib.sha256(binary.read_bytes()).hexdigest()
            # Port numbers deliberately come from the datad CLI contract, not
            # the helper's implementation or a copied hexadecimal constant.
            for port in (9444, 9460):
                with self.subTest(port=port):
                    code = ('import socket,time; s=socket.socket(); '
                            's.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1); '
                            f's.bind(("127.0.0.1",{port})); s.listen(); '
                            'print("READY",flush=True); time.sleep(30)')
                    proc = subprocess.Popen([str(binary), '-c', code], stdout=subprocess.PIPE,
                                            stderr=subprocess.PIPE, text=True)
                    try:
                        self.assertTrue(select.select([proc.stdout], [], [], 5)[0], 'listener failed to start')
                        self.assertEqual(proc.stdout.readline().strip(), 'READY', 'test port unavailable')
                        command = 'healthy_token "$2"' if port == 9444 else 'wait_healthy "$2"'
                        began = time.monotonic()
                        result = subprocess.run(['sh', '-c', '. "$1"; ' + command,
                                                 'test', str(helper), digest],
                                                capture_output=True, text=True, timeout=20)
                        elapsed = time.monotonic() - began
                        self.assertEqual(result.returncode == 0, port == 9460, (result.stdout, result.stderr))
                        if port == 9460:
                            self.assertGreaterEqual(elapsed, 9.98)
                            self.assertLess(elapsed, 15)
                            self.assertIn('Healthy: 10/10 seconds', result.stdout)
                    finally:
                        proc.terminate()
                        proc.communicate(timeout=5)


if __name__=='__main__': unittest.main(verbosity=2)
