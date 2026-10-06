#!/usr/bin/env python3
"""Create a Windows offline ADB kit using the verified ARMv7 build."""
import argparse
import hashlib
import json
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def package(output, adb_dir):
    binary = (ROOT / 'build/zwrt-datad-armv7-candidate').read_bytes()
    info = json.loads((ROOT / 'build/rust-release-provenance-armv7.json').read_text(encoding='utf-8'))
    version = json.loads((ROOT / 'version.json').read_text(encoding='utf-8'))['datad']['version']
    if hashlib.sha256(binary).hexdigest() != info['sha256'] or version != info['version']:
        raise ValueError('Build binary, provenance and version must match')
    if len(binary) < 64 or binary[:6] != b'\x7fELF\x01\x01' or binary[18:20] != b'\x28\x00':
        raise ValueError('Expected ELF32 LE ARM binary')
    kit = output / 'U50Pro_ADB'
    payload = kit / 'payload'
    tools = kit / 'tools'
    payload.mkdir(parents=True, exist_ok=True)
    tools.mkdir(parents=True, exist_ok=True)
    generated = []

    def put(path, data):
        path.write_bytes(data)
        generated.append(path)

    put(payload / 'zwrt-datad', binary)
    for src, dst in [('u50pro-service.sh', 'service-control.sh'), ('u50pro-deploy-transaction.sh', 'deploy-transaction.sh')]:
        put(payload / dst, (ROOT / 'scripts' / src).read_text(encoding='utf-8').encode('utf-8'))
    # Use exactly the same starter and unit as the existing ADB deployment.
    host = (ROOT / 'scripts/deploy-u50pro.sh').read_text(encoding='utf-8')
    starter = host.split("<<'SH'\n", 1)[1].split('\nSH\n', 1)[0] + '\n'
    unit = host.split("<<'UNIT'\n", 1)[1].split('\nUNIT\n', 1)[0] + '\n'
    put(payload / 'start.sh', starter.encode())
    put(payload / 'zwrt-datad.service', unit.encode())
    sums = ''.join(f'{hashlib.sha256(p.read_bytes()).hexdigest()}  {p.name}\n' for p in generated)
    put(payload / 'SHA256SUMS', sums.encode('ascii'))
    put(kit / 'INSTALL.ps1', (ROOT / 'scripts/deploy-u50pro.ps1').read_text(encoding='utf-8').encode('utf-8-sig'))
    batch = '''@echo off
setlocal
chcp 65001 >nul
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0INSTALL.ps1" %*
set "result=%errorlevel%"
echo.
pause
exit /b %result%
'''
    put(kit / 'INSTALL.bat', batch.replace('\n', '\r\n').encode('ascii'))
    for name in ('adb.exe', 'AdbWinApi.dll', 'AdbWinUsbApi.dll', 'NOTICE.txt'):
        put(tools / name, (adb_dir / name).read_bytes())
    put(kit / 'build-info.json', (json.dumps(info, ensure_ascii=False, indent=2) + '\n').encode())
    readme = f'''U50 Pro / MU5120 ADB 本地部署包 — {version}

1. 把整个 ZIP 解压到电脑，不能直接在压缩包内双击。
2. 用数据线连接 U50 Pro，确保已开启 root ADB 并安装好 USB 驱动。
3. 双击 INSTALL.bat，等待最后出现 [5/5] SUCCESS。

已内置 Windows ADB 和 datad 二进制，不需要 Git Bash、WSL、Python、curl、wget 或联网下载。
只适用于 MU5120 / ARMv7；脚本不会解锁 root、启用 factory-diag 或重启设备。
同时连接手机时会自动筛选 MU5120；多个 MU5120 时在终端执行 INSTALL.bat 设备序列号。
需要检查连接但不安装：INSTALL.bat -PreflightOnly

部署会检查机型、root、剩余空间、SHA-256 和新进程状态；失败时尝试回滚旧版本。
SUCCESS: persistent boot autostart / existing persistent unit 表示有开机自启。
SUCCESS: session 表示本次开机有效；重启后在本目录终端执行：
tools\\adb.exe -s 设备序列号 shell sh /cache/zwrt-datad/start.sh

App 选择 ZWRT，地址填设备 IP（通常 192.168.0.1），端口 9461，admin + 原厂后台密码。
出现 FAILED 或连接中断时，先按提示检查 .deploy.* 目录里的 result / deploy.log，再决定是否重试。
校验和或空间不足会中止安装，不要替换 payload 内的单个文件绕过校验。
'''
    put(kit / '使用说明.txt', readme.encode('utf-8-sig'))
    zip_path = output / f'U50Pro_ADB_{version}.zip'
    with zipfile.ZipFile(zip_path, 'w', compression=zipfile.ZIP_DEFLATED) as archive:
        for path in generated:
            archive.write(path, path.relative_to(output).as_posix())
    print(f'Created {zip_path} ({zip_path.stat().st_size} bytes)')
    return kit, zip_path


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--adb-dir', type=Path, required=True, help='Android SDK platform-tools directory')
    parser.add_argument('--output', type=Path, default=ROOT / 'build/deploy_kano')
    args = parser.parse_args()
    package(args.output, args.adb_dir)
