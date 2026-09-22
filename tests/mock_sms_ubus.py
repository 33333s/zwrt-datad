#!/usr/bin/env python3
"""Local-only vendor fixture; never talks to a modem or sends real SMS."""
import base64
import json
import os
from pathlib import Path
import sys
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric import padding
from cryptography.hazmat.primitives.ciphers.aead import AESGCM

root = Path(os.environ['SMS_FIXTURE'])
config = json.loads((root / 'config.json').read_text())
args = sys.argv[1:]
if args[:1] != ['call']:
    print('{}')
    sys.exit(0)
service, method = args[1:3]
params = json.loads(args[3]) if len(args) > 3 else {}
with (root / 'calls.log').open('a') as log:
    log.write(json.dumps([service, method, params.get('page'), params.get('mem_store')]) + '\n')
keyfile = root / 'session.key'
if service == 'zwrt_web' and method == 'web_crt_get':
    pem = (root / 'public.pem').read_text()
    print(json.dumps({'result': pem.replace('\n', '')}))
elif service == 'zwrt_web' and method == 'web_http_enstr_set':
    if config.get('reject_registration'):
        print('{"result":1}')
    else:
        private = serialization.load_pem_private_key((root / 'private.pem').read_bytes(), password=None)
        keyfile.write_bytes(bytes.fromhex(private.decrypt(base64.b64decode(params['web_enstr']), padding.PKCS1v15()).decode()))
        print('{"result":0}')
elif service == 'zwrt_wms' and method == 'zwrt_wms_get_wms_capacity':
    if config.get('capacity_failure'):
        sys.exit(1)
    print(json.dumps({'sms_nv_rev_total': config['count'], 'sms_dev_unread_num': config.get('unread', 0)}))
elif service == 'zwrt_wms' and method == 'zte_libwms_get_sms_data':
    if config.get('list_failure'):
        sys.exit(1)
    if params['mem_store'] == 0:
        print('{"messages":[]}')
        sys.exit(0)
    if config.get('replace_key_once') and not (root / 'replaced').exists():
        keyfile.write_bytes(os.urandom(32))
        (root / 'replaced').touch()
    key = keyfile.read_bytes() if keyfile.exists() else bytes(32)
    if config.get('bad_ciphertext'):
        key = bytes(32)
    def envelope(text):
        raw = text.encode('utf-16-be').hex().upper()
        if config.get('plaintext'):
            return raw
        nonce = os.urandom(12)
        encrypted = AESGCM(key).encrypt(nonce, raw.encode(), None)
        return base64.b64encode(nonce + encrypted[-16:] + encrypted[:-16]).decode()
    page = 0 if config.get('repeat_page') else params['page']
    start = page * params['data_per_page']
    rows = [dict(id=i + 1, number='10086' if config.get('plaintext') else envelope('10086'),
                 content=envelope('测试' + str(i + 1)), date='26,09,22,12,00,00,+,0', tag='0')
            for i in range(start, min(start + params['data_per_page'], config['count']))]
    print(json.dumps({'messages': rows}))
elif service == 'zwrt_wms' and method == 'zte_libwms_send_sms':
    key = keyfile.read_bytes()
    def decrypt(value):
        raw = base64.b64decode(value)
        return AESGCM(key).decrypt(raw[:12], raw[28:] + raw[12:28], None).decode()
    assert decrypt(params['number']) == '10086'
    assert decrypt(params['message_body']) == '6D4B8BD5'
    with (root / 'sends.log').open('a') as log:
        log.write('fixture send verified\n')
    print('{"result":"success"}')
elif service == 'zwrt_wms' and method == 'zwrt_wms_get_cmd_status':
    print(json.dumps({'sms_cmd_status_result': 2 if config.get('send_failure') else 3}))
else:
    print('{}')
