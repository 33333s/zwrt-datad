#!/usr/bin/env python3
"""Identity persistence/auth/proof/failure tests with a debug-only worker."""
import base64
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from cryptography.hazmat.primitives import serialization, hashes
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.exceptions import InvalidSignature

binary=Path(sys.argv[1]).resolve()
with tempfile.TemporaryDirectory(prefix='datad-identity-test-') as directory:
    root=Path(directory); data=root/'data'; data.mkdir(mode=0o700)
    token=root/'token'; token.write_text('fixture-identity-token')
    config=root/'worker.json'; calls=root/'calls'
    worker=root/'worker'
    worker.write_text('#!'+sys.executable+'\n'+Path(__file__).with_name('mock_identity_worker.py').read_text())
    worker.chmod(0o700)
    settings={'device':'fixture-A','calls':str(calls)}
    def setup(**values):
        settings.update(values); tmp=config.with_suffix('.next'); tmp.write_text(json.dumps(settings)); tmp.replace(config)
    setup()
    env=dict(os.environ,ZWRT_DATAD_IDENTITY_TEST_WORKER=str(worker),ZWRT_DATAD_IDENTITY_TEST_CONFIG=str(config),
        ZWRT_DATAD_UBUS_BIN='/usr/bin/true',ZWRT_DATAD_UCI_BIN='/usr/bin/false',ZWRT_DATAD_OTA_DISABLE_AUTO='1',
        ZWRT_DATAD_COOLING_CONFIG=str(root/'cooling'),ZWRT_DATAD_WIFI_CONFIG=str(root/'wifi'))
    process=None
    def start():
        global process,port
        with socket.socket() as s: s.bind(('127.0.0.1',0)); port=s.getsockname()[1]
        process=subprocess.Popen([str(binary),'--port',str(port),'--data-dir',str(data),'--auth-token-file',str(token)],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        for _ in range(100):
            try:
                if request('/healthz')[0]==200: return
            except (OSError,urllib.error.URLError): pass
            time.sleep(.1)
        raise AssertionError('server did not start')
    def stop():
        if process:
            process.terminate()
            try: process.wait(timeout=10)
            except subprocess.TimeoutExpired: process.kill(); process.wait()
    def request(path,body=None,auth=True,extra=None):
        headers={'Content-Type':'application/json'}
        if auth: headers['Authorization']='Bearer fixture-identity-token'
        headers.update(extra or {})
        req=urllib.request.Request(f'http://127.0.0.1:{port}'+path,headers=headers,data=None if body is None else json.dumps(body).encode())
        try:
            with urllib.request.urlopen(req,timeout=15) as reply: code=reply.status; raw=reply.read()
        except urllib.error.HTTPError as error: code=error.code; raw=error.read()
        try: value=json.loads(raw)
        except ValueError: value={}
        return code,value
    def proof(purpose='enroll'):
        return dict(purpose=purpose,challenge=base64.urlsafe_b64encode(os.urandom(32)).decode().rstrip('='))
    def verify(reply,expected,public):
        kid=hashlib.sha256(base64.b64decode(public['public_key_spki'])).hexdigest()
        assert kid==reply['key_id']==public['key_id']
        nonce=base64.urlsafe_b64decode(expected['challenge']+'=')
        message=b'zwrt-datad-identity-v1\0'+expected['purpose'].encode()+b'\0'+kid.encode()+b'\0'+nonce
        assert base64.b64decode(reply['signed_message'])==message
        key=serialization.load_pem_public_key(public['public_key_pem'].encode())
        key.verify(base64.b64decode(reply['signature']),message,ec.ECDSA(hashes.SHA256()))
        try: key.verify(base64.b64decode(reply['signature']),message+b'x',ec.ECDSA(hashes.SHA256()))
        except InvalidSignature: pass
        else: raise AssertionError('altered proof verified')
        assert reply['attested'] is False and reply['signature_format']=='asn1-der'
    try:
        start()
        assert not (data/'identity').exists(), 'startup initialized keys'
        for path,body in [('/identity/public-key',None),('/identity/init',{}),('/identity/sign',proof())]:
            assert request(path,body,False)[0]==401
        assert request('/identity/public-key?access_token=fixture-identity-token',auth=False)[0]==401
        assert request('/identity/public-key')[0]==409
        assert not (data/'identity').exists(), 'GET created an identity'
        assert request('/identity/init',{'force':True})[0]==400
        setup(mode='unavailable')
        assert request('/identity/init',{})[0]==503
        assert not (data/'identity/key.json').exists(), 'software fallback created a key'
        setup(mode='ok')
        code,public=request('/identity/init',{})
        assert code==200 and public['created'] and public['backend']=='test-only' and public['attested'] is False, public
        record=data/'identity/key.json'; original=record.read_bytes()
        assert (record.stat().st_mode&0o777)==0o600
        assert ((data/'identity').stat().st_mode&0o777)==0o700
        assert 'key_blob' not in public
        code,repeat=request('/identity/init',{}); assert code==200 and not repeat['created'] and repeat['key_id']==public['key_id']
        assert record.read_bytes()==original
        code,read=request('/identity/public-key'); assert code==200 and read['key_id']==public['key_id']
        expected=proof(); code,signed=request('/identity/sign',expected); assert code==200,signed; verify(signed,expected,public)
        stop(); start()
        assert request('/identity/public-key')[1]['key_id']==public['key_id'], 'restart changed identity'
        expected=proof('authenticate'); code,signed=request('/identity/sign',expected); assert code==200; verify(signed,expected,public)
        for bad in [{'purpose':'arbitrary','challenge':expected['challenge']}, {'purpose':'enroll','challenge':expected['challenge']+'='},
                    {'purpose':'enroll','challenge':'A'},dict(proof(),key_blob='injected')]:
            assert request('/identity/sign',bad)[0]==400
        assert request('/identity/sign',{'purpose':'enroll','challenge':'a'*3000})[0] in (400,413)
        time.sleep(.3)
        setup(mode='bad_signature')
        assert request('/identity/sign',proof())[0]==409, 'bad hardware signature was returned as success'
        setup(mode='ok',device='fixture-B')
        assert request('/identity/public-key')[0]==409 and request('/identity/init',{})[0]==409
        assert record.read_bytes()==original, 'foreign key was regenerated'
        setup(device='fixture-A')
        record.write_text('broken')
        assert request('/identity/init',{})[0]==500 and record.read_text()=='broken', 'corrupt key overwritten'
        record.write_bytes(original)
        record.chmod(0o644)
        assert request('/identity/public-key')[0]==500
        record.chmod(0o600)
        alias=root/'alias'; os.link(record,alias)
        assert request('/identity/public-key')[0]==500
        alias.unlink()
        stored=root/'stored'; record.rename(stored); record.symlink_to(stored)
        assert request('/identity/public-key')[0]==500
        record.unlink(); stored.rename(record)
        setup(mode='garbage')
        assert request('/identity/public-key')[0]==503
        assert record.read_bytes()==original
        setup(mode='ok',delay=.5)
        before=len(calls.read_text().splitlines())
        with concurrent.futures.ThreadPoolExecutor() as executor:
            one=executor.submit(request,'/identity/public-key')
            until=time.monotonic()+3
            while len(calls.read_text().splitlines())==before and time.monotonic()<until: time.sleep(.02)
            assert request('/identity/public-key')[0]==429
            assert one.result()[0]==200
        setup(delay=20)
        before=time.monotonic(); assert request('/identity/public-key')[0]==503
        assert time.monotonic()-before<12, 'worker timeout did not terminate'
        setup(delay=0)
        assert request('/identity/public-key')[0]==200, 'timeout leaked lock or worker slot'
        assert record.read_bytes()==original
        print('Identity: header auth, explicit/idempotent init, restart persistence, independent ECDSA verify, domain binding, corrupt/foreign key fail-closed, file safety, bounded worker and concurrency PASS')
    finally:
        stop()
