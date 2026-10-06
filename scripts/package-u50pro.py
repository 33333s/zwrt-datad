#!/usr/bin/env python3
"""Package the current ARMv7 build and a self-contained HTTPS installer."""
import argparse
import base64
import gzip
import hashlib
import json
import re
import shutil
from pathlib import Path
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parent.parent


def render_installer(binary, version, base_url=""):
    if len(binary) < 64 or binary[:6] != b"\x7fELF\x01\x01" or binary[18:20] != b"\x28\x00":
        raise ValueError("expected ELF32 little-endian ARM binary")
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        raise ValueError("invalid version")
    if base_url:
        url = urlsplit(base_url)
        if (url.scheme not in ("http", "https") or not url.hostname or url.username or url.password
                or url.query or url.fragment or re.search(r"[\s'\\]", base_url)):
            raise ValueError("base URL must be a plain HTTP(S) directory without credentials")
    text = (ROOT / "scripts/install-u50pro.sh.in").read_text(encoding="utf-8")
    values = {
        "BASE_URL": base_url.rstrip("/"),
        "SHA256": hashlib.sha256(binary).hexdigest(),
        "BYTES": str(len(binary)),
        "VERSION": version,
        "SERVICE": (ROOT / "scripts/u50pro-service.sh").read_text(encoding="utf-8").rstrip(),
        "TRANSACTION": (ROOT / "scripts/u50pro-deploy-transaction.sh").read_text(encoding="utf-8").rstrip(),
    }
    for key, value in values.items():
        marker = f"@{key}@"
        if text.count(marker) != 1:
            raise ValueError(f"template must contain exactly one {marker}")
        text = text.replace(marker, value)
    return text


def render_bootstrap(installer, tiny, base_url=""):
    if len(tiny) < 64 or tiny[:6] != b'\x7fELF\x01\x01' or tiny[18:20] != b'\x28\x00':
        raise ValueError('expected the ARM32 u50get-tiny executable')
    text = (ROOT / 'scripts/bootstrap-u50pro.sh.in').read_text(encoding='utf-8')
    values = {'BASE_URL': "'" + base_url.rstrip('/') + "'",
              'PAYLOAD': base64.b64encode(gzip.compress(tiny, mtime=0)).decode('ascii'),
              'TINY_SHA': hashlib.sha256(tiny).hexdigest(),
              'INSTALLER_SHA': hashlib.sha256(installer.encode('utf-8')).hexdigest()}
    for key, value in values.items():
        text = text.replace('@' + key + '@', value)
    return text


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "build/zwrt-datad-armv7-candidate")
    parser.add_argument("--output", type=Path, default=ROOT / "build/deploy_kano")
    parser.add_argument("--base-url", default="", help="HTTP(S) directory; otherwise pass DATAD_BASE_URL at install time")
    parser.add_argument("--tiny-downloader", type=Path, help="Also generate a pinned bootstrap embedding this u50get-tiny ELF")
    args = parser.parse_args()
    binary = args.binary.read_bytes()
    version = json.loads((ROOT / "version.json").read_text(encoding="utf-8"))["datad"]["version"]
    provenance = json.loads((ROOT / "build/rust-release-provenance-armv7.json").read_text(encoding="utf-8"))
    digest = hashlib.sha256(binary).hexdigest()
    if provenance["sha256"] != digest or provenance["version"] != version:
        raise ValueError("binary/provenance/version mismatch; rebuild before packaging")
    installer = render_installer(binary, version, args.base_url)
    args.output.mkdir(parents=True, exist_ok=True)
    (args.output / "zwrt-datad-armv7").write_bytes(binary)
    (args.output / "install-datad.sh").write_bytes(installer.encode("utf-8"))
    names = ["zwrt-datad-armv7", "install-datad.sh"]
    if args.tiny_downloader:
        bootstrap = render_bootstrap(installer, args.tiny_downloader.read_bytes(), args.base_url)
        (args.output / 'bootstrap-u50pro.sh').write_bytes(bootstrap.encode('utf-8'))
        names.append('bootstrap-u50pro.sh')
    checksums = "".join(f"{hashlib.sha256((args.output / name).read_bytes()).hexdigest()}  {name}\n" for name in names)
    (args.output / "SHA256SUMS").write_bytes(checksums.encode("ascii"))
    shutil.copyfile(ROOT / "build/rust-release-provenance-armv7.json", args.output / "build-info.json")
    base_url = args.base_url.rstrip("/") or "https://YOUR-SERVER/datad/u50pro"
    readme = f"""U50 Pro / MU5120 部署包 — {version}

将 install-datad.sh 和 zwrt-datad-armv7 一起上传到同一个 HTTP(S) 目录。
不要复制任何设备配置、密码文件或抓包。SHA256SUMS 和 build-info.json 供核验。

在目标设备的 root shell 中执行（不是在电脑 PowerShell 中）：

curl -4fL --retry 3 '{base_url}/install-datad.sh' -o /tmp/install-datad.sh && \\
DATAD_BASE_URL='{base_url}' sh /tmp/install-datad.sh

把示例地址替换成实际目录。脚本已内置二进制 SHA-256，更新二进制时必须重新打包脚本。
无 curl/wget 时，使用本包可选的 bootstrap-u50pro.sh：将其通过 ADB 推送到 /tmp，执行：
DATAD_BASE_URL='{base_url}' sh /tmp/bootstrap-u50pro.sh
它内嵌微型下载器，并固定校验入口脚本和二进制。也可复制脚本全部内容到设备 shell，
先执行 export DATAD_BASE_URL='实际目录'。更新包时 bootstrap 也必须一起重新生成。
HTTP 可用；HTTPS 默认校验证书。自有 CA 可设 DATAD_CA_FILE，明确跳过校验可设 DATAD_TLS_INSECURE=1。
脚本不解锁 root、不启用 factory-diag、不重启。安装位置：/cache/zwrt-datad。
只有 SUCCESS: persistent boot autostart / existing persistent unit 表示有持久自启。
SUCCESS: session 表示本次开机有效，重启后执行 sh /cache/zwrt-datad/start.sh。
失败时检查终端给出的 .deploy.* 目录里的 result 和 deploy.log，再决定是否重试。

App：ZWRT 模式，设备局域网 IP（通常 192.168.0.1），端口 9461，admin + 原厂后台密码。
"""
    (args.output / "使用说明.txt").write_bytes(readme.encode("utf-8"))
    print(f"Packaged {version} ({digest}) into {args.output}")


if __name__ == "__main__":
    main()
