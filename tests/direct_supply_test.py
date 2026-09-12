#!/usr/bin/env python3
"""Exercise the actual HTTP/control/state path with a stateful charger fixture."""
import json, os, pathlib, socket, subprocess, sys, tempfile, time, urllib.error, urllib.request
BIN=pathlib.Path(sys.argv[1]).resolve()
with tempfile.TemporaryDirectory(prefix='datad-power-test-') as name:
    base=pathlib.Path(name); fixture=base/'charger.json'; calls=base/'writes.log'
    token=base/'auth.token'; token.write_text('fixture-power-token')
    fake=base/'ubus'
    fake.write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys
p=pathlib.Path(os.environ['POWER_FIXTURE']); args=sys.argv[1:]
if args and args[0]=='-t': args=args[2:]
if args[:2]==['call','zwrt_bsp.charger']:
    data=json.loads(p.read_text()); method=args[2]
    if data.get('read_fail') and method=='list': sys.exit(1)
    if method=='list':
        if data.get('malformed'): print('not JSON'); sys.exit(0)
        if data.get('missing'): print('{}'); sys.exit(0)
        print(json.dumps({'direct_power_supply_mode':data.get('mode','disable'),'charger_connect':1})); sys.exit(0)
    if method=='set':
        wanted=json.loads(args[3])['direct_power_supply_mode']
        with open(os.environ['POWER_WRITES'],'a') as f: f.write(wanted+'\\n')
        if data.get('reject'): print('{"result":1}'); sys.exit(0)
        if not data.get('ignore'):
            data['mode']=wanted; temp=p.with_suffix('.tmp'); temp.write_text(json.dumps(data)); temp.replace(p)
        if data.get('empty_reply'): sys.exit(0)
        if data.get('whitespace_reply'): print('   '); sys.exit(0)
        if data.get('bad_reply'): print('not JSON'); sys.exit(0)
        print('{}'); sys.exit(0)
print('{}')
'''); fake.chmod(0o755)
    def setup(**data):
        p=fixture.with_suffix('.new'); p.write_text(json.dumps(data)); p.replace(fixture)
    setup(mode='disable')
    with socket.socket() as sock: sock.bind(('127.0.0.1',0)); port=sock.getsockname()[1]
    env=dict(os.environ,ZWRT_DATAD_DIR=str(base/'cloud'),ZWRT_DATAD_UBUS_BIN=str(fake),ZWRT_DATAD_UCI_BIN='/usr/bin/false',
             POWER_FIXTURE=str(fixture),POWER_WRITES=str(calls))
    proc=subprocess.Popen([str(BIN),'-i','200','-p',str(port),'--auth-token-file',str(token)],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    def req(path,body=None,auth=True):
        headers={'Content-Type':'application/json'}
        if auth: headers['Authorization']='Bearer fixture-power-token'
        request=urllib.request.Request(f'http://127.0.0.1:{port}{path}',headers=headers,data=None if body is None else json.dumps(body).encode())
        try:
            with urllib.request.urlopen(request,timeout=8) as r: return r.status,json.load(r)
        except urllib.error.HTTPError as e:
            raw=e.read()
            try: data=json.loads(raw)
            except ValueError: data={}
            return e.code,data
    def action(which,params=None): return req('/control',{'action':'power.direct_supply.'+which,'params':params or {}})
    def count(): return len(calls.read_text().splitlines()) if calls.exists() else 0
    try:
        for i in range(60):
            try:
                status,state=req('/state'); break
            except (OSError,urllib.error.URLError): time.sleep(.1)
        else: raise AssertionError('startup')
        assert state['power']['direct_supply']['enabled'] is False
        assert req('/state',auth=False)[0]==401
        caps=req('/capabilities')[1]['control']; assert 'power.direct_supply.set' in caps and 'power.direct_supply.status' in caps
        assert action('status')[1]['result']=={'supported':True,'enabled':False,'mode':'disable'}
        code,data=action('set',{'enabled':True}); assert code==200 and data['result']['verified'] and data['result']['changed'] and data['result']['enabled']
        before=count(); assert action('set',{'enabled':1})[1]['result']['changed'] is False; assert count()==before
        assert action('set',{'enabled':False})[1]['result']['mode']=='disable'
        for value in (None,2,'enable','true; touch /tmp/not-allowed',[],{}):
            before=count(); code,_=action('set',{} if value is None else {'enabled':value}); assert code==400,(value,code); assert count()==before
        setup(missing=True); assert action('status')[1]['result']['supported'] is False
        before=count(); assert action('set',{'enabled':True})[0]==502; assert count()==before
        setup(mode='unexpected'); result=action('status')[1]['result']; assert result['supported'] and result['enabled'] is None
        before=count(); assert action('set',{'enabled':True})[0]==502; assert count()==before
        for key in ('read_fail','malformed'):
            setup(**{key:True}); assert action('status')[0]==502; assert action('set',{'enabled':True})[0]==502
        for reply in ('empty_reply','whitespace_reply'):
            setup(mode='disable',**{reply:True}); assert action('set',{'enabled':True})[1]['result']['verified']
        setup(mode='disable',bad_reply=True); assert action('set',{'enabled':True})[0]==502
        setup(mode='disable',reject=True); assert action('set',{'enabled':True})[0]==502
        setup(mode='disable',ignore=True,empty_reply=True); code,data=action('set',{'enabled':True}); assert code==502 and 'readback' in data['error']['message']
        setup(mode='disable'); assert action('set',{'enabled':True})[0]==200
        for i in range(30):
            state=req('/state')[1]
            if state.get('power',{}).get('direct_supply',{}).get('enabled') is True: break
            time.sleep(.1)
        else: raise AssertionError('state not refreshed')
        print('direct supply: status, capabilities, authentication, state refresh, set/readback, repeat no-write, invalid/unsupported/unknown/read/write errors PASS')
    finally:
        proc.terminate()
        try: proc.wait(timeout=8)
        except subprocess.TimeoutExpired: proc.kill(); proc.wait()
