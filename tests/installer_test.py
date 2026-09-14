#!/usr/bin/env python3
"""Boot-entry tests use isolated text fixtures, never the host's rc.local."""
import json
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
AWK = ROOT / "scripts/rc-local-datad.awk"
START = "sh /data/zwrt-datad/service.sh start\n"
UFI = "sh /data/ufi-tools/service.sh start\n"


def render(text):
    return subprocess.run(["awk", "-f", str(AWK)], input=text, text=True, capture_output=True)


class BootEntries(unittest.TestCase):
    def check(self, source, expected):
        first = render(source)
        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertEqual(first.stdout, expected)
        again = render(first.stdout)
        self.assertEqual(again.returncode, 0, again.stderr)
        self.assertEqual(again.stdout, first.stdout)
        self.assertEqual(subprocess.run(["sh", "-n"], input=first.stdout, text=True).returncode, 0)

    def test_standard_is_byte_identical(self):
        source = "#!/bin/sh\n# BEGIN ufi-tools-zwrt managed startup\n" + START + UFI + "# END ufi-tools-zwrt managed startup\nexit 0\n"
        self.check(source, source)

    def test_missing_inserted_before_ufi(self):
        self.check("#!/bin/sh\n" + UFI + "exit 0\n", "#!/bin/sh\n" + START + UFI + "exit 0\n")

    def test_duplicates_and_old_paths(self):
        self.check("#!/bin/sh\nsh /data/plugins/zwrt-datad/service.sh start\n" + START + UFI + START + "exit 0\n", "#!/bin/sh\n" + START + UFI + "exit 0\n")

    def test_after_exit_is_relocated(self):
        self.check("#!/bin/sh\nexit 0\n" + START, "#!/bin/sh\n" + START + "exit 0\n")

    def test_legacy_init_is_removed(self):
        self.check("#!/bin/sh\n/etc/init.d/zwrt-datad enable\n/etc/init.d/zwrt-datad start\nexit 0\n", "#!/bin/sh\n" + START + "exit 0\n")

    def test_nohup_continuation_and_redirect(self):
        self.check("#!/bin/sh\nnohup /data/plugins/zwrt-datad/zwrt-datad -i 1000 \\\n -p 9460 >/tmp/datad.log 2>&1 &\nexit 0\n", "#!/bin/sh\n" + START + "exit 0\n")

    def test_single_line_guard(self):
        self.check("#!/bin/sh\n[ -x /data/plugins/zwrt-datad/service.sh ] && sh /data/plugins/zwrt-datad/service.sh start\nexit 0\n", "#!/bin/sh\n" + START + "exit 0\n")

    def test_three_line_guard(self):
        self.check("#!/bin/sh\nif [ -f /data/plugins/zwrt-datad/service.sh ]; then\n sh /data/plugins/zwrt-datad/service.sh start\nfi\nexit 0\n", "#!/bin/sh\n" + START + "exit 0\n")

    def test_unrelated_service_and_comments_preserved(self):
        source = "#!/bin/sh\n# old /etc/init.d/zwrt-datad start\nsh /data/zwrt-datad/cloud-service.sh start\n/etc/init.d/icg-v3 start\nexit 0\n"
        expected = "#!/bin/sh\n# old /etc/init.d/zwrt-datad start\n" + START + "/etc/init.d/icg-v3 start\nexit 0\n"
        self.check(source, expected)

    def test_no_exit(self):
        self.check("#!/bin/sh\necho other\n", "#!/bin/sh\necho other\n" + START)

    def test_conditional_exit_does_not_capture_boot_entry(self):
        source = "#!/bin/sh\nif [ -f /tmp/skip ]; then\n  exit 0\nfi\n" + UFI + "exit 0\n"
        expected = "#!/bin/sh\nif [ -f /tmp/skip ]; then\n  exit 0\nfi\n" + START + UFI + "exit 0\n"
        self.check(source, expected)

    def test_mixed_command_rejected(self):
        for text in ["sh /data/zwrt-datad/service.sh start; echo keep\n", "sh /data/zwrt-datad/service.sh start && echo keep\n"]:
            result = render(text)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(result.stdout, "")

    def test_embedded_release_inputs(self):
        with tempfile.TemporaryDirectory() as folder:
            binary = Path(folder) / "binary"
            binary.write_bytes(b"\x7fELF\x02\x01" + bytes(12) + b"\xb7\x00" + bytes(44))
            output = Path(folder) / "install.sh"
            subprocess.run(["python3", str(ROOT / "scripts/render-installer.py"), str(binary), str(output)], check=True, capture_output=True)
            body = output.read_text()
            self.assertIn((ROOT / "scripts/service.sh").read_text().rstrip(), body)
            self.assertIn((ROOT / "version.json").read_text().rstrip(), body)
            self.assertIn((ROOT / "OPENSSL-LICENSE.txt").read_text().rstrip(), body)
            self.assertNotIn("@@", body)
            self.assertIn('BACKUP="$WORK/rollback"', body)
            self.assertNotIn('/backups/install-', body)
            self.assertNotIn('say "备份：$BACKUP"', body)
            self.assertIn('health() { curl -fsS --connect-timeout 2 --max-time 3 "http://127.0.0.1:$1/healthz" >/dev/null 2>&1; }', body)
            self.assertEqual(subprocess.run(["sh", "-n", str(output)]).returncode, 0)


if __name__ == "__main__":
    unittest.main()
