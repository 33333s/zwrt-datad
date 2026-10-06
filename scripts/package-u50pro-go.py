#!/usr/bin/env python3
"""Build a self-contained Linux ARMv7 installer; no third-party Go modules."""
import argparse
import hashlib
import importlib.util
import json
import os
import shutil
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_URL = ""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "build/zwrt-datad-armv7-candidate")
    parser.add_argument("--output", type=Path, default=ROOT / "build/deploy_kano")
    parser.add_argument("--ca-bundle", type=Path, required=True, help="Public Mozilla/distribution PEM root certificates, no private keys")
    parser.add_argument("--base-url", default=DEFAULT_URL)
    parser.add_argument("--upx", action="store_true", help="Produce a smaller UPX binary plus an unpacked fallback")
    args = parser.parse_args()
    args.output = args.output.resolve()
    binary = args.binary.read_bytes()
    provenance = json.loads((ROOT / "build/rust-release-provenance-armv7.json").read_text(encoding="utf-8"))
    version = json.loads((ROOT / "version.json").read_text(encoding="utf-8"))["datad"]["version"]
    digest = hashlib.sha256(binary).hexdigest()
    if provenance["sha256"] != digest or provenance["version"] != version:
        raise ValueError("binary/provenance/version mismatch")
    spec = importlib.util.spec_from_file_location("shell_package", ROOT / "scripts/package-u50pro.py")
    package = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(package)
    # Reuse validation and the exact shell installer's unit/start templates.
    rendered = package.render_installer(binary, version, args.base_url)
    ca = args.ca_bundle.read_bytes()
    if b"PRIVATE KEY" in ca or b"-----BEGIN CERTIFICATE-----" not in ca:
        raise ValueError("expected a public CA certificate bundle")
    build = ROOT / "build/u50pro-go-installer"
    build.mkdir(parents=True, exist_ok=True)
    assets = build / "assets"
    assets.mkdir(exist_ok=True)
    for path in (ROOT / "tools/u50pro-installer").iterdir():
        if path.suffix == ".go" or path.name == "go.mod":
            shutil.copyfile(path, build / path.name)
    values = {
        "rootsPEM": ("roots.pem", ca),
        "serviceScript": ("service-control.sh", (ROOT / "scripts/u50pro-service.sh").read_bytes().replace(b"\r\n", b"\n")),
        "transactionScript": ("deploy-transaction.sh", (ROOT / "scripts/u50pro-deploy-transaction.sh").read_bytes().replace(b"\r\n", b"\n")),
    }
    for variable, name, delimiter in [("startScript", "start.sh", "U50_START_EOF"), ("unitFile", "zwrt-datad.service", "U50_UNIT_EOF")]:
        content = rendered.split("<<'" + delimiter + "'\n", 1)[1].split("\n" + delimiter, 1)[0] + "\n"
        values[variable] = (name, content.encode())
    generated = ['package main', 'import _ "embed"']
    assignments = []
    for variable, (name, data) in values.items():
        (assets / name).write_bytes(data)
        generated.append(f'//go:embed assets/{name}\nvar embedded_{variable} []byte')
        assignments.append(f'{variable} = embedded_{variable}')
    assignments.append(f'release = releaseSpec{{Version:{json.dumps(version)}, SHA256:{json.dumps(digest)}, BaseURL:{json.dumps(args.base_url.rstrip("/"))}, Bytes:{len(binary)}}}')
    generated.append('func init() {\n' + '\n'.join(assignments) + '\n}')
    (build / "generated.go").write_text('\n'.join(generated) + '\n', encoding="utf-8")
    args.output.mkdir(parents=True, exist_ok=True)
    (args.output / "zwrt-datad-armv7").write_bytes(binary)
    output = args.output / "install-datad-armv7"
    env = dict(os.environ, GOOS="linux", GOARCH="arm", GOARM="7", CGO_ENABLED="0", GOWORK="off")
    subprocess.run(["go", "build", "-trimpath", "-buildvcs=false", "-ldflags=-s -w -buildid=", "-o", str(output), "."], cwd=build, env=env, check=True)
    names = [output.name, "zwrt-datad-armv7"]
    if args.upx:
        fallback = args.output / "install-datad-armv7.unpacked"
        shutil.copyfile(output, fallback)
        subprocess.run(["upx", "--best", str(output)], check=True)
        subprocess.run(["upx", "-t", str(output)], check=True)
        names.append(fallback.name)
    (args.output / "INSTALLER-SHA256SUMS").write_text(''.join(f'{hashlib.sha256((args.output/name).read_bytes()).hexdigest()}  {name}\n' for name in names), encoding="ascii")
    info = {"datad": provenance, "base_url": args.base_url, "ca_bundle_sha256": hashlib.sha256(ca).hexdigest(),
            "installer_source_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
            "installer_source_dirty": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT, text=True).strip()),
            "files": {name: {"bytes": (args.output/name).stat().st_size, "sha256": hashlib.sha256((args.output/name).read_bytes()).hexdigest()} for name in names}}
    (args.output / "installer-build-info.json").write_text(json.dumps(info, indent=2) + "\n", encoding="utf-8")
    example_url = args.base_url or "https://YOUR-SERVER/datad/u50pro"
    (args.output / "Go安装说明.txt").write_text(f"""U50 Pro / MU5120 Go 安装器（datad {version}）

在本目录打开 Windows CMD，设备需要已有 root ADB：
adb push install-datad-armv7 /cache/install-datad-armv7
adb shell chmod 700 /cache/install-datad-armv7
adb shell /cache/install-datad-armv7 --base-url "{example_url}"

多台设备连接时，每条 adb 后加 -s 目标设备序列号。
压缩程序无法运行时，推送 install-datad-armv7.unpacked 到相同目标路径再执行。
安装器自带 HTTPS 下载和公共 CA，服务器目录通过 --base-url 指定；替换示例地址后执行。
下载 zwrt-datad-armv7，无需 curl/wget。校验固定版本、字节数、SHA-256。
更新服务器二进制时必须重新生成本安装器。不要关闭 TLS 校验。

无网络时，先推送本目录里的 datad，然后执行离线安装：
adb push zwrt-datad-armv7 /cache/zwrt-datad-armv7
adb shell /cache/install-datad-armv7 --file /cache/zwrt-datad-armv7

仅检查设备（不安装）：
adb shell /cache/install-datad-armv7 --preflight

下载显示百分比；安装显示步骤、耗时和实际日志。
安装事务启动后，ADB 断开会继续执行；不要重复安装或手动删除锁。
日志和结果在输出的 /cache/zwrt-datad/.deploy.*/worker.log、result、worker-result.json。
仅 SUCCESS: persistent boot autostart / existing persistent unit 表示开机自启。
SUCCESS: session 表示本次开机有效，重启后执行 sh /cache/zwrt-datad/start.sh。
安装失败保留备份并尝试回滚，回滚失败必须先检查日志。
不会解锁、开启工厂模式、刷写分区或重启设备。仍使用设备自带 sh/systemctl 等基础工具。
App：ZWRT 模式，设备局域网 IP，端口 9461，admin + 原厂后台密码。
""", encoding="utf-8")
    print(json.dumps(info["files"], indent=2))


if __name__ == "__main__":
    main()
