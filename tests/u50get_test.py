#!/usr/bin/env python3
"""Opt-in ARM runtime tests: --qemu QEMU --sysroot ROOT, or --adb SERIAL.
Only temporary downloads; never installs datad or changes firmware settings.
"""
import argparse
import hashlib
import http.server
import os
from pathlib import Path
import re
import shlex
import shutil
import ssl
import subprocess
import tempfile
import threading

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--binary', type=Path, default=Path('build/u50get/u50get-tiny'))
p.add_argument('--qemu')
p.add_argument('--sysroot')
p.add_argument('--adb')
a = p.parse_args()
assert bool(a.adb) != bool(a.qemu), 'select exactly one runtime'
BODY = bytes(range(256)) * 17


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.path in ('/ok', '/chunked', '/partial'):
            self.send_response(200)
            if self.path == '/chunked':
                self.send_header('Transfer-Encoding', 'chunked')
            else:
                self.send_header('Content-Length', str(len(BODY) + (100 if self.path == '/partial' else 0)))
            self.end_headers()
            self.wfile.write(('%x\r\n' % len(BODY)).encode() + BODY + b'\r\n0\r\n\r\n'
                             if self.path == '/chunked' else BODY)
        elif self.path in ('/redirect', '/loop', '/downgrade'):
            self.send_response(302)
            self.send_header('Location', {'/redirect': tls_url + '/ok', '/loop': tls_url + '/loop',
                                         '/downgrade': plain_url + '/ok'}[self.path])
            self.send_header('Content-Length', '0')
            self.end_headers()
        else:
            self.send_response(404 if self.path == '/404' else 503)
            self.send_header('Content-Length', '0')
            self.end_headers()


with tempfile.TemporaryDirectory(prefix='u50get-test-') as tmp:
    work = Path(tmp)
    cert, key = work / 'cert.pem', work / 'key.pem'
    if shutil.which('openssl'):
        openssl, cert_arg, key_arg = ['openssl'], str(cert), str(key)
    else:
        openssl = ['wsl', '--exec', 'openssl']
        linux_path = subprocess.check_output(['wsl', '--exec', 'wslpath', '-u', str(work)], text=True).strip()
        cert_arg, key_arg = linux_path + '/cert.pem', linux_path + '/key.pem'
    subprocess.run(openssl + ['req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1',
                   '-keyout', key_arg, '-out', cert_arg, '-subj', '/CN=localhost',
                   '-addext', 'subjectAltName=DNS:localhost'], check=True, capture_output=True)
    plain = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    tls = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(cert, key)
    tls.socket = ctx.wrap_socket(tls.socket, server_side=True)
    plain_url = f'http://127.0.0.1:{plain.server_port}'
    tls_url = f'https://localhost:{tls.server_port}'
    for s in (plain, tls):
        threading.Thread(target=s.serve_forever, daemon=True).start()
    remote = None
    ports = []
    adb = ['adb', '-s', a.adb] if a.adb else []
    env = {k: v for k, v in os.environ.items() if not k.lower().endswith('_proxy')}
    try:
        if a.adb:
            remote = subprocess.check_output(adb + ['shell', 'mktemp -d /tmp/ugt.XXXXXX'], text=True).strip()
            assert re.fullmatch(r'/tmp/ugt\.[a-zA-Z0-9]+', remote)
            for local, name in ((a.binary, 'g'), (cert, 'cert.pem')):
                subprocess.run(adb + ['push', str(local), remote + '/' + name], check=True, capture_output=True)
            subprocess.run(adb + ['shell', 'chmod 700 ' + remote + '/g'], check=True)
            for s in (plain, tls):
                port = f'tcp:{s.server_port}'
                subprocess.run(adb + ['reverse', port, port], check=True, capture_output=True)
                ports.append(port)
            ca = remote + '/cert.pem'
        else:
            ca = str(cert)
        def run(name, url, code=0, option='ca', full=False):
            args = [url] if url else []
            if url and option != 'default':
                args.append(ca if option == 'ca' else '--insecure')
            if a.adb:
                command = shlex.join([remote + '/g'] + args) + ' > ' + ('/dev/full' if full else remote + '/out')
                command += '; rc=$?; echo U50_RC:$rc; sha256sum ' + remote + '/out 2>/dev/null'
                r = subprocess.check_output(adb + ['shell', command], timeout=25).decode()
                actual = int(re.search(r'U50_RC:(\d+)', r)[1])
                if code == 0:
                    assert hashlib.sha256(BODY).hexdigest() in r, (name, r)
            else:
                prefix = [a.qemu, '-L', a.sysroot, '-E', f'LD_LIBRARY_PATH={a.sysroot}/usr/lib:{a.sysroot}/lib']
                with open('/dev/full' if full else work / 'out', 'wb') as out:
                    r = subprocess.run(prefix + [str(a.binary)] + args, env=env, stdout=out,
                                       stderr=subprocess.PIPE, timeout=25)
                actual = r.returncode
                if code == 0:
                    assert (work / 'out').read_bytes() == BODY, name
            assert actual == code, (name, actual, code)
            print('PASS', name, flush=True)
        run('usage', '', 2)
        run('HTTP binary', plain_url + '/ok')
        run('HTTP chunked', plain_url + '/chunked')
        run('HTTPS trusted certificate', tls_url + '/ok')
        run('HTTPS untrusted certificate rejected by default', tls_url + '/ok', 60, 'default')
        run('HTTPS hostname mismatch rejected', tls_url.replace('localhost', '127.0.0.1') + '/ok', 60)
        run('HTTPS explicit insecure', tls_url + '/ok', 0, 'insecure')
        run('HTTPS explicit insecure hostname', tls_url.replace('localhost', '127.0.0.1') + '/ok', 0, 'insecure')
        run('HTTP to HTTPS', plain_url + '/redirect')
        run('HTTPS to HTTP', tls_url + '/downgrade')
        run('redirect loop', tls_url + '/loop', 47)
        run('404', plain_url + '/404', 22)
        run('503', plain_url + '/503', 22)
        run('truncated body', plain_url + '/partial', 18)
        run('file scheme rejected', 'file:///etc/passwd', 1)
        run('stdout failure', plain_url + '/ok', 23, full=True)
    finally:
        if remote and re.fullmatch(r'/tmp/ugt\.[a-zA-Z0-9]+', remote):
            subprocess.run(adb + ['shell', f'rm -f {remote}/g {remote}/cert.pem {remote}/out; rmdir {remote}'], check=True)
        for port in ports:
            # Old U50 adbd can close after removing the mapping without the
            # final status expected by modern adb. Still clean all resources.
            subprocess.run(adb + ['reverse', '--remove', port], capture_output=True)
        for s in (plain, tls):
            s.shutdown()
            s.server_close()
