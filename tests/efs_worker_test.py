"""Protocol/CLI integration tests using a fake libdiag; never access a modem."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
EXE = ROOT / "build" / ("efs-mock.exe" if os.name == "nt" else "efs-mock")
CONTENT = bytes(range(256)) * 2


class EfsTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(dir=ROOT / "tests")
        self.addCleanup(self.temp.cleanup)
        self.folder = Path(self.temp.name)
        self.trace = self.folder / "trace.txt"

    def call(self, *args, scenario="", expected=0, json_mode=True):
        env = dict(os.environ, MOCK_SCENARIO=scenario, MOCK_TRACE=str(self.trace),
                   MOCK_TMP_FILE=str(self.folder / "result.tmp"))
        self.trace.unlink(missing_ok=True)
        p = subprocess.run([str(EXE), *(["--json"] if json_mode else []), *map(str, args)],
                           capture_output=True, env=env, timeout=10)
        self.assertEqual(p.returncode, expected, (p.stdout, p.stderr))
        self.events = self.trace.read_text().splitlines() if self.trace.exists() else []
        self.packets = [bytes.fromhex(x[7:]) for x in self.events if x.startswith("PACKET ")]
        self.assertNotIn("BAD_PACKET", self.events)
        if not json_mode:
            return p.stdout.decode()
        obj = json.loads(p.stdout)
        self.assertEqual(obj["ok"], expected == 0)
        self.assertNotIn(b"mock libdiag", p.stdout)
        if "INIT" in self.events:
            self.assertIn(b"mock libdiag init log", p.stderr)
        return obj

    def assert_cleanup(self):
        self.assertEqual(self.events.count("INIT"), 1)
        self.assertEqual(self.events.count("REGISTER"), 1)
        self.assertEqual(self.events.count("RELEASE"), 1)
        self.assertEqual(self.events.count("DEINIT"), 1)

    def test_help_does_not_load_library(self):
        self.assertIn("rm", self.call("help")["data"]["commands"])
        self.assertEqual(self.events, [])

    def test_guarded_root_mutations_fail_before_library_init(self):
        for path in ("/", "/file", "/nvefs"):
            for args in (("write",path,"01"),("rm",path),("rmdir",path),("mkdir",path),
                         ("rename",path,"/nvefs/new"),("rename","/nvefs/old",path)):
                with self.subTest(args=args):
                    obj=self.call("--guarded",*args,expected=1)
                    self.assertEqual(obj["error"]["code"],"ROOT_READONLY")
                    self.assertEqual(self.events,[])

    def test_guarded_cannot_use_raw_commands(self):
        for args in (("raw","4b130000"),):
            obj=self.call("--guarded",*args,expected=1)
            self.assertEqual(obj["error"]["code"],"ROOT_READONLY")
            self.assertEqual(self.events,[])

    def test_invalid_arguments_never_touch_modem(self):
        cases = [(), ("unknown",), ("ls", "ls", "/nvefs"), ("ls", "relative"),
                 ("write", "/file", "0x"), ("write", "/file", "a"),
                 ("read", "/file", "-1"), ("read", "/file", "10garbage"),
                 ("read", "/file", "1048577"), ("read", "/file", "2", "4294967295"),
                 ("write", "/file", "aabb", "4294967295"), ("mkdir", "/dir", "888"),
                 ("mkdir", "/dir", "1000"), ("nvread", "65536"), ("raw", ""),
                 ("raw", "aa" * 1025), ("rename", "/" + "a"*600, "/" + "b"*600),
                 ("rm", "/" + "a"*1024)]
        for args in cases:
            with self.subTest(args=str(args)[:90]):
                obj = self.call(*args, expected=2)
                self.assertEqual(obj["error"]["code"], "INVALID_ARGUMENT")
                self.assertEqual(self.events, [])

    def test_listing_metadata_and_json_escaping(self):
        entries = self.call("ls", "/nvefs")["data"]["entries"]
        self.assertEqual([e["type"] for e in entries], ["file", "directory", "item"])
        self.assertEqual(entries[0]["name"], 'a"b\\\n')
        self.assertEqual(entries[2]["name"], "中文")
        self.assertEqual(bytes.fromhex(entries[2]["name_hex"]), "中文".encode())
        self.assertEqual(entries[0]["mtime"], 20)
        self.assertEqual(entries[0]["size"], 512)
        self.assertIn("OP 13", self.events)
        self.assert_cleanup()

    def test_long_directory_names_not_truncated(self):
        entries = self.call("ls", "/", scenario="long_name")["data"]["entries"]
        self.assertEqual(entries[0]["name"], "x" * 180)

    def test_alternate_end_of_directory(self):
        self.assertEqual(len(self.call("ls", scenario="end_error")["data"]["entries"]), 3)

    def test_stat(self):
        data = self.call("stat", "/file")["data"]
        self.assertEqual(data["mode"], 0o100644)
        self.assertEqual(data["size"], 512)
        self.assertEqual(data["links"], 1)

    def test_mutation_packet_layouts(self):
        cases = [("rm", ["/file"], 8), ("delete", ["/file"], 8),
                 ("unlink", ["/file"], 8), ("rmdir", ["/empty"], 10),
                 ("mkdir", ["/dir"], 9), ("rename", ["/old", "/new"], 14),
                 ("mv", ["/old", "/new"], 14)]
        for name, args, op in cases:
            with self.subTest(command=name):
                self.call(name, *args)
                expected = bytes([0x4b, 0x13, op, 0])
                if op == 9:
                    expected += (0o40755).to_bytes(2, "little")
                expected += b"".join(x.encode() + b"\0" for x in args)
                self.assertEqual(self.packets, [expected])
                self.assert_cleanup()

    def test_mkdir_mode(self):
        self.call("mkdir", "/dir", "0700")
        self.assertIn("MKDIR_MODE 16832", self.events)

    def test_read_full_and_offset(self):
        data = self.call("read", "/file")["data"]
        self.assertEqual(bytes.fromhex(data["hex"]), CONTENT)
        self.assertEqual(data["bytes"], 512)
        self.assertTrue(data["eof"])
        self.assertEqual(data["next_offset"], 512)
        self.assertIn("OP 3", self.events)
        self.assert_cleanup()
        data = self.call("read", "/file", "35", "158")["data"]
        self.assertEqual(bytes.fromhex(data["hex"]), CONTENT[158:193])
        self.assertEqual(data["next_offset"], 193)
        self.assertFalse(data["eof"])

    def test_short_reads_continue_to_eof(self):
        data = self.call("read", "/file", scenario="short_reads")["data"]
        self.assertEqual(bytes.fromhex(data["hex"]), CONTENT)
        self.assertTrue(data["eof"])

    def test_write_packets_and_partial_write_retries(self):
        for scenario in ["", "partial_writes"]:
            with self.subTest(scenario=scenario):
                data = self.call("write", "/file", CONTENT.hex(), scenario=scenario)["data"]
                self.assertEqual(data["bytes"], 512)
                self.assertIn("OPEN_FLAGS 577", self.events)
                sent = [p for p in self.packets if p[:4] == b"\x4b\x13\x05\x00"]
                for p in sent:
                    offset = int.from_bytes(p[8:12], "little")
                    self.assertEqual(p[12:], CONTENT[offset:offset+len(p)-12])
                self.assert_cleanup()

    def test_offset_write_does_not_truncate(self):
        data = self.call("write", "/file", "aabb", "0")["data"]
        self.assertIn("OPEN_FLAGS 65", self.events)
        self.assertEqual(data["offset"], 0)

    def test_empty_write_truncates(self):
        self.assertEqual(self.call("write", "/file", "")["data"]["bytes"], 0)
        self.assertIn("OPEN_FLAGS 577", self.events)
        self.assertNotIn("OP 5", self.events)

    def test_partial_write_error_reports_progress(self):
        obj = self.call("write", "/file", CONTENT.hex(), scenario="second_write_error", expected=1)
        self.assertEqual(obj["error"]["code"], "EFS_ERROR")
        self.assertEqual(obj["error"]["efs_errno"], 28)
        self.assertEqual(obj["error"]["bytes_transferred"], 150)
        self.assertIn("OP 3", self.events)
        self.assert_cleanup()

    def test_upload_binary_file(self):
        local = self.folder / "upload.bin"
        local.write_bytes(CONTENT)
        self.assertEqual(self.call("put", local, "/file")["data"]["bytes"], 512)
        sent = b"".join(p[12:] for p in self.packets if p[:4] == b"\x4b\x13\x05\x00")
        self.assertEqual(sent, CONTENT)

    def test_missing_upload_does_not_truncate(self):
        obj = self.call("put", self.folder / "missing", "/file", expected=1)
        self.assertEqual(obj["error"]["code"], "LOCAL_IO_ERROR")
        self.assertNotIn("OP 2", self.events)

    def test_private_file_write_keeps_data_out_of_arguments(self):
        local = self.folder / "write.bin"
        local.write_bytes(b'\x00\xff\x01')
        data = self.call("write-file", "/file", local, "12")["data"]
        self.assertEqual(data["bytes"], 3)
        self.assertEqual(data["offset"], 12)
        self.assertIn("OPEN_FLAGS 65", self.events)
        self.assertIn("WRITE 12 3", self.events)

    def test_oversized_upload_never_truncates(self):
        local = self.folder / "oversize.bin"
        local.write_bytes(b'x' * (1024 * 1024 + 1))
        self.call("put", local, "/file", expected=2)
        self.assertNotIn("OP 2", self.events)

    def test_download_binary_file(self):
        local = self.folder / "download.bin"
        self.assertEqual(self.call("get", "/file", local)["data"]["bytes"], 512)
        self.assertEqual(local.read_bytes(), CONTENT)
        self.assertEqual(list(self.folder.glob("*.part.*")), [])

    def test_failed_download_preserves_existing_file(self):
        local = self.folder / "download.bin"
        local.write_bytes(b"old contents")
        self.call("get", "/file", local, scenario="read_error", expected=1)
        self.assertEqual(local.read_bytes(), b"old contents")
        self.assertEqual(list(self.folder.glob("*.part.*")), [])
        self.assertIn("OP 3", self.events)

    def test_read_protocol_errors_close_file(self):
        for scenario in ["bad_read_length", "wrong_offset", "wrong_descriptor"]:
            with self.subTest(scenario=scenario):
                obj = self.call("read", "/file", scenario=scenario, expected=1)
                self.assertEqual(obj["error"]["code"], "PROTOCOL_ERROR")
                self.assertIn("OP 3", self.events)
                self.assert_cleanup()

    def test_write_protocol_errors(self):
        for scenario in ["zero_write", "bad_write_count"]:
            with self.subTest(scenario=scenario):
                obj = self.call("write", "/file", "aabb", scenario=scenario, expected=1)
                self.assertEqual(obj["error"]["code"], "PROTOCOL_ERROR")
                self.assertIn("OP 3", self.events)

    def test_directory_protocol_errors_close_directory(self):
        for scenario in ["unterminated_name", "wrong_sequence", "readdir_error"]:
            with self.subTest(scenario=scenario):
                self.call("ls", "/", scenario=scenario, expected=1)
                self.assertIn("OP 13", self.events)
                self.assert_cleanup()

    def test_efs_errors_not_reported_as_empty_results(self):
        for args, scenario, error in [(('ls', '/'), 'opendir_error', 13),
                                     (('stat', '/file'), 'stat_error', 2),
                                     (('read', '/file'), 'open_error', 2),
                                     (('rm', '/file'), 'delete_error', 13),
                                     (('read', '/file'), 'read_error', 13),
                                     (('read', '/file'), 'close_error', 5)]:
            with self.subTest(scenario=scenario):
                obj = self.call(*args, scenario=scenario, expected=1)
                self.assertNotIn("data", obj)
                self.assertEqual(obj["error"]["efs_errno"], error)
                self.assert_cleanup()

    def test_library_errors(self):
        for scenario in ["library_error", "symbol_error"]:
            obj = self.call("ls", scenario=scenario, expected=1)
            self.assertEqual(obj["error"]["code"], "LIBRARY_ERROR")
            self.assertNotIn("INIT", self.events)

    def test_initialization_cleanup(self):
        for scenario, release in [("init_error", False), ("register_error", False),
                                  ("support_error", True)]:
            self.call("ls", scenario=scenario, expected=1)
            self.assertEqual("RELEASE" in self.events, release)
            self.assertEqual("DEINIT" in self.events, scenario != "init_error")

    def test_transport_errors(self):
        for scenario, code in [("timeout", "TIMEOUT"), ("send_error", "DIAG_ERROR"),
                               ("negative_callback", "PROTOCOL_ERROR"),
                               ("oversized_callback", "PROTOCOL_ERROR"),
                               ("wrong_header", "PROTOCOL_ERROR"),
                               ("truncated_response", "PROTOCOL_ERROR")]:
            with self.subTest(scenario=scenario):
                obj = self.call("ls", scenario=scenario, expected=1)
                self.assertEqual(obj["error"]["code"], code)
                self.assert_cleanup()
                self.assertEqual(len(self.packets), 1)

    def test_cleanup_failure_is_error(self):
        obj = self.call("rm", "/file", scenario="release_error", expected=1)
        self.assertEqual(obj["error"]["diag_status"], 99)

    def test_raw_and_nvread(self):
        self.assertEqual(self.call("raw", "00abff")["data"]["hex"], "00abff")
        obj = self.call("nvread", "550")
        self.assertEqual(obj["data"]["bytes"], 133)
        self.assertEqual(self.packets[0][:3], b"\x26\x26\x02")

    def test_guarded_nvread_supports_all_u16_items(self):
        for item in (0, 1, 550, 551, 65535):
            data = self.call("--guarded", "nvread", str(item))["data"]
            self.assertEqual(data["bytes"], 133)
            self.assertEqual(self.packets, [bytes([0x26, item & 255, item >> 8]) + bytes(130)])
            self.assert_cleanup()
        for item in ("-1", "65536", "550;id"):
            self.call("--guarded", "nvread", item, expected=2)
            self.assertEqual(self.events, [])

    def test_nvread_preserves_nonstandard_responses(self):
        for scenario in ("nv_status", "nv_status_high", "nv_item", "nv_command", "nv_short", "nv_length", "nv_marker", "nv_digit"):
            with self.subTest(scenario=scenario):
                obj = self.call("--guarded", "nvread", "550", scenario=scenario)
                raw = bytes.fromhex(obj["data"]["hex"])
                self.assertEqual(len(raw), 132 if scenario == "nv_short" else 133)
                self.assertEqual(obj["data"]["bytes"], len(raw))
                if scenario == "nv_status": self.assertEqual(raw[131], 5)
                if scenario == "nv_digit": self.assertEqual(raw[6], 0xfa)
                self.assertEqual(len(self.packets), 1)
                self.assert_cleanup()

    def test_text_mode_still_works(self):
        out = self.call("read", "/file", json_mode=False)
        self.assertIn("00 01 02", out)
        self.assertIn("OP 3", self.events)


if __name__ == "__main__":
    EXE.parent.mkdir(exist_ok=True)
    subprocess.run(["cc" if os.name != "nt" else "gcc", "-std=c11", "-O2", "-Wall", "-Wextra", "-Werror",
                    "-DDIAG_EFS_TEST", "rust/efs_worker.c", "tests/efs_mock/mock_diag.c",
                    "-o", str(EXE)], cwd=ROOT, check=True)
    unittest.main(verbosity=2)
