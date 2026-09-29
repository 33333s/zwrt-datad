#!/usr/bin/env python3
"""Pinned U50 installer: atomic swap, hash pinning, health check and rollback."""
import hashlib
import os
import shutil
import stat
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def script(version):
    return f"#!/bin/sh\n[ \"$1\" = --version ] && echo 'zwrt-datad {version}'\n# marker-{version}\n"


def sha(data):
    return hashlib.sha256(data if isinstance(data, bytes) else data.encode()).hexdigest()


def render(folder, version, binary_text):
    """Behavior tests pin a shell-script "binary" directly from the template."""
    text = (ROOT / "scripts/install-u50.sh.in").read_text()
    text = text.replace("@@VERSION@@", version).replace("@@SHA256@@", sha(binary_text))
    assert "@@" not in text
    path = folder / "installer.sh"
    path.write_text(text)
    return path


def check_release_renderer():
    """The real release script must fill every marker and only accept ELF32 ARM."""
    with tempfile.TemporaryDirectory(prefix="u50-render-") as tmp:
        folder = Path(tmp)
        scratch = folder / "repo"
        (scratch / "scripts").mkdir(parents=True)
        shutil.copy(ROOT / "scripts/render-installer.py", scratch / "scripts/")
        shutil.copy(ROOT / "scripts/install-u50.sh.in", scratch / "scripts/")
        (scratch / "version.json").write_text(
            '{"schema":1,"datad":{"version":"1.2.3","asset":"zwrt-datad-aarch64"}}')
        render_script = str(scratch / "scripts/render-installer.py")
        # ELF32, little-endian, EM_ARM: what the release script inspects.
        good = folder / "armv7"
        good.write_bytes(b"\x7fELF\x01\x01\x01" + bytes(9) + b"\x02\x00\x28\x00" + b"payload")
        out = folder / "install.sh"
        subprocess.run([sys.executable, render_script, "--profile", "armv7", str(good), str(out)],
                       check=True, capture_output=True, text=True)
        rendered = out.read_text()
        assert "@@" not in rendered
        assert "VERSION='1.2.3'" in rendered
        assert f"SHA256='{hashlib.sha256(good.read_bytes()).hexdigest()}'" in rendered
        subprocess.run(["sh", "-n", str(out)], check=True)
        # An AArch64 or arbitrary file must be refused.
        bad = folder / "aarch64"
        bad.write_bytes(b"\x7fELF\x02\x01\x01" + bytes(9) + b"\x02\x00\xb7\x00")
        result = subprocess.run([sys.executable, render_script, "--profile", "armv7", str(bad), str(folder / "x.sh")],
                                capture_output=True, text=True)
        assert result.returncode != 0 and "ELF32 ARM" in result.stderr


def run_case(name, *, staged, expect_ok, expect_bin, break_new=False, pin=None):
    with tempfile.TemporaryDirectory(prefix="u50-installer-") as tmp:
        folder = Path(tmp)
        data = folder / "data"
        proc = folder / "proc"
        state = folder / "state"
        bindir = folder / "bin"
        for path in (data, proc, state, bindir):
            path.mkdir()
        old = script("0.0.1")
        new = script("9.9.9")
        (data / "zwrt-datad").write_text(old)
        (data / "zwrt-datad").chmod(0o700)
        (data / "zwrt-datad.new").write_text(staged if staged is not None else new)
        (data / "zwrt-datad.new").chmod(0o600)
        installer = render(folder, "9.9.9", pin if pin is not None else new)
        # A fake systemd: `restart` "starts" the installed file (its /proc exe is
        # a copy of it), or a different program when the new build is meant to fail.
        (bindir / "systemctl").write_text(f"""#!/bin/sh
case "$1" in
  cat) exit 0 ;;
  restart)
    mkdir -p "{proc}/4242"
    if [ -n "${{BREAK_NEW:-}}" ] && grep -q 9.9.9 "{data}/zwrt-datad"; then cp /bin/sh "{proc}/4242/exe"; else cp "{data}/zwrt-datad" "{proc}/4242/exe"; fi ;;
  is-active) exit 0 ;;
  show) echo 4242 ;;
esac
""")
        (bindir / "systemctl").chmod(0o755)
        if shutil.which("sha256sum") is None:
            (bindir / "sha256sum").write_text('#!/bin/sh\nexec shasum -a 256 "$@"\n')
            (bindir / "sha256sum").chmod(0o755)
        env = dict(os.environ, PATH=f"{bindir}:{os.environ['PATH']}", DATAD_DIR=str(data),
                   DATAD_PROC=str(proc), DATAD_HEALTH_WAIT="6", DATAD_HEALTH_STABLE="1")
        if break_new:
            env["BREAK_NEW"] = "1"
        result = subprocess.run(["sh", str(installer)], env=env, capture_output=True, text=True, timeout=60)
        installed = (data / "zwrt-datad").read_text()
        assert (result.returncode == 0) == expect_ok, (name, result.returncode, result.stderr)
        assert installed == expect_bin(old, new), (name, installed)
        assert not (data / "zwrt-datad.prev").exists() or not expect_ok, name
        assert not (data / "zwrt-datad.new").exists() or not expect_ok, name
        assert stat.S_IMODE((data / "zwrt-datad").stat().st_mode) == 0o700, name
        return result


check_release_renderer()
ok = run_case("success", staged=None, expect_ok=True, expect_bin=lambda old, new: new)
assert "installed zwrt-datad 9.9.9" in ok.stdout
bad = run_case("hash mismatch", staged=script("9.9.9") + "# tampered\n", expect_ok=False,
               expect_bin=lambda old, new: old, pin=script("9.9.9"))
assert "SHA-256 mismatch" in bad.stderr
wrong_version = script("8.8.8")
run_case("wrong version", staged=wrong_version, expect_ok=False, expect_bin=lambda old, new: old, pin=wrong_version)
rolled = run_case("rollback", staged=None, expect_ok=False, expect_bin=lambda old, new: old, break_new=True)
assert "restoring the previous binary" in rolled.stderr
print("U50 installer: swap, pin, version check and rollback OK")
