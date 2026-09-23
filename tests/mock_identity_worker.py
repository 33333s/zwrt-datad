"""TEST ONLY: software fixture for the private worker pipe protocol.

Never imported by datad; only debug builds accept the test worker override.
"""
import base64
import json
import os
from pathlib import Path
import struct
import sys
import time
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec

config=json.loads(Path(os.environ['ZWRT_DATAD_IDENTITY_TEST_CONFIG']).read_text())
raw=sys.stdin.buffer.read()
op=raw[0]
a,b=struct.unpack('>II',raw[1:9])
blob,message=raw[9:9+a],raw[9+a:]
assert len(raw)==9+a+b
if config.get('calls'):
    with open(config['calls'],'a') as log: log.write(str(op)+'\n')
time.sleep(config.get('delay',0))
mode=config.get('mode','ok')
code=0; first=second=b''
if mode=='garbage':
    sys.stdout.buffer.write(b'x'*10000)
    sys.exit(0)
if mode=='unavailable': code=-1002
else:
    if op==1:
        private=ec.generate_private_key(ec.SECP256R1())
        encoded=base64.b64encode(private.private_bytes(serialization.Encoding.DER,serialization.PrivateFormat.PKCS8,serialization.NoEncryption())).decode()
        first=json.dumps({'device':config['device'],'private':encoded}).encode()
    else:
        stored=json.loads(blob)
        if stored['device']!=config['device']: code=-33
        private=serialization.load_der_private_key(base64.b64decode(stored['private']),None)
    if code==0:
        if op in (1,2): second=private.public_key().public_bytes(serialization.Encoding.DER,serialization.PublicFormat.SubjectPublicKeyInfo)
        elif op==3:
            if mode=='bad_signature': message+=b'changed'
            second=private.sign(message,ec.ECDSA(hashes.SHA256()))
if code: first=second=b''
sys.stdout.buffer.write(b'DDKI1'+struct.pack('>iII',code,len(first),len(second))+first+second)
