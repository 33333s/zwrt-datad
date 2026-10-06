#!/usr/bin/env python3
"""Windows launcher regression using a fake adb.exe, never a real modem."""
import hashlib
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MOCK = r'''
using System;
using System.IO;
class FakeAdb {
    static int Main(string[] args) {
        string folder = Environment.GetEnvironmentVariable("U50_ADB_TEST_DIR");
        string mode = Environment.GetEnvironmentVariable("U50_ADB_TEST_MODE") ?? "success";
        File.AppendAllText(Path.Combine(folder, "calls.txt"), string.Join("\t", args) + "\n");
        if (args.Length == 1 && args[0] == "devices") {
            Console.WriteLine("List of devices attached\nPHONE\tdevice\nMU5120MOCK\tdevice");
            if (mode == "multiple") Console.WriteLine("SECOND\tdevice");
            return 0;
        }
        if (args.Length < 4 || args[0] != "-s") return 90;
        string serial = args[1];
        string cmd = args[3];
        if (args[2] == "push") {
            if (serial != "MU5120MOCK" || args.Length != 5 || !File.Exists(args[3])) return 91;
            return mode == "push" ? 1 : 0;
        }
        if (args[2] != "shell" || args.Length != 4) return 92;
        if (cmd == "cfg get model_name") {
            Console.WriteLine(serial == "PHONE" ? "PHONE" : "MU5120"); return 0;
        }
        if (cmd == "id -u; uname -m; cfg get model_name") {
            Console.WriteLine(mode == "root" ? "2000\narmv7l\nMU5120" : "0\narmv7l\nMU5120"); return 0;
        }
        if (cmd.StartsWith("for t in ")) {
            if (!cmd.Contains("awk '{print $4}'") || !cmd.Contains("test \"$free\" -ge ")) return 93;
            return mode == "preflight" ? 1 : 0;
        }
        if (cmd.StartsWith("umask 077;")) {
            Console.WriteLine(mode == "stage" ? "/cache/unsafe;reboot" : "/cache/zwrt-datad/.deploy.ABC123"); return 0;
        }
        if (cmd.Contains("sha256sum -c SHA256SUMS")) {
            if (mode == "hash") return 1;
            Console.WriteLine("zwrt-datad: OK\nzwrt-datad 0.10.68"); return 0;
        }
        if (cmd.StartsWith("nohup sh ")) { File.WriteAllText(Path.Combine(folder, "launched"), "1"); return 0; }
        if (cmd.Contains("/result 2>/dev/null")) {
            Console.WriteLine(mode == "failure" ? "FAILED: previous installation restored" :
                mode == "session" ? "SUCCESS: session; executable hash + socket owner stable for 10 seconds" :
                "SUCCESS: persistent boot autostart; executable hash + socket owner stable for 10 seconds");
            return 0;
        }
        if (cmd.Contains("/deploy.log")) { Console.WriteLine("simulated failure details"); return 0; }
        return 94;
    }
}
'''


@unittest.skipUnless(os.name == 'nt', 'Windows PowerShell/ADB launcher test')
class AdbLauncherTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix="u50 ADB test's & ", dir=ROOT / 'build')
        cls.base = Path(cls.temp.name).resolve()
        assert cls.base.parent == (ROOT / 'build').resolve()
        cls.addClassCleanup(cls.temp.cleanup)
        source = cls.base / 'FakeAdb.cs'
        source.write_text(MOCK, encoding='utf-8')
        compiler = cls.base / 'compile.ps1'
        compiler.write_text("Add-Type -Path (Join-Path $PSScriptRoot 'FakeAdb.cs') -OutputAssembly (Join-Path $PSScriptRoot 'adb.exe') -OutputType ConsoleApplication\n", encoding='utf-8-sig')
        subprocess.run(['powershell.exe', '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', str(compiler)], check=True, capture_output=True, timeout=30)

    def setUp(self):
        self.case = self.base / self._testMethodName
        self.case.mkdir()
        self.kit = self.case / 'kit'
        self.payload = self.kit / 'payload'
        self.payload.mkdir(parents=True)
        self.script = self.kit / 'INSTALL.ps1'
        self.script.write_text((ROOT / 'scripts/deploy-u50pro.ps1').read_text(encoding='utf-8'), encoding='utf-8-sig')
        elf = b'\x7fELF\x01\x01\x01' + bytes(9) + b'\x02\x00\x28\x00' + bytes(64)
        for name in ('zwrt-datad', 'start.sh', 'zwrt-datad.service', 'service-control.sh', 'deploy-transaction.sh'):
            (self.payload / name).write_bytes(elf if name == 'zwrt-datad' else b'fixture\n')
        self.manifest()

    def manifest(self):
        names = ('zwrt-datad', 'start.sh', 'zwrt-datad.service', 'service-control.sh', 'deploy-transaction.sh')
        (self.payload / 'SHA256SUMS').write_text(''.join(f'{hashlib.sha256((self.payload / n).read_bytes()).hexdigest()}  {n}\n' for n in names), encoding='ascii')

    def run_installer(self, mode='success', extra=(), success=True):
        for name in ('calls.txt', 'launched'):
            (self.case / name).unlink(missing_ok=True)
        env = dict(os.environ, U50_ADB_TEST_DIR=str(self.case), U50_ADB_TEST_MODE=mode)
        proc = subprocess.run(['powershell.exe', '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', str(self.script),
                               '-AdbPath', str(self.base / 'adb.exe'), *extra], env=env, capture_output=True, text=True, timeout=30)
        self.assertEqual(proc.returncode == 0, success, (proc.stdout, proc.stderr))
        return proc

    def test_success_and_correct_modem_selection(self):
        result = self.run_installer()
        self.assertIn('[5/5] SUCCESS', result.stdout)
        calls = (self.case / 'calls.txt').read_text().splitlines()
        pushes = [line for line in calls if '\tpush\t' in line]
        self.assertEqual(len(pushes), 6)
        self.assertTrue(all(line.startswith('-s\tMU5120MOCK\t') for line in pushes))
        self.assertTrue((self.case / 'launched').exists())

    def test_preflight_only_never_writes(self):
        self.run_installer(extra=('-PreflightOnly',))
        calls = (self.case / 'calls.txt').read_text()
        self.assertNotIn('\tpush\t', calls)
        self.assertNotIn('mktemp -d', calls)
        self.assertFalse((self.case / 'launched').exists())

    def test_failures_before_launch_never_start_transaction(self):
        for mode in ('root', 'multiple', 'preflight', 'stage', 'push', 'hash'):
            with self.subTest(mode=mode):
                self.run_installer(mode=mode, success=False)
                self.assertFalse((self.case / 'launched').exists())

    def test_local_corruption_never_calls_adb(self):
        (self.payload / 'zwrt-datad').write_bytes(b'tampered')
        self.run_installer(success=False)
        self.assertFalse((self.case / 'calls.txt').exists())

    def test_unexpected_manifest_entry_is_rejected(self):
        with (self.payload / 'SHA256SUMS').open('a') as output:
            output.write('0' * 64 + '  unexpected\n')
        self.run_installer(success=False)
        self.assertFalse((self.case / 'calls.txt').exists())

    def test_device_failure_is_not_success(self):
        result = self.run_installer(mode='failure', success=False)
        self.assertIn('previous installation restored', result.stdout)
        self.assertNotIn('[5/5] SUCCESS', result.stdout)

    def test_session_only_is_reported(self):
        result = self.run_installer(mode='session')
        self.assertIn('No boot autostart', result.stdout)


if __name__ == '__main__':
    unittest.main(verbosity=2)
