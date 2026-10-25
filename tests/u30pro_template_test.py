#!/usr/bin/env python3
"""U30 Pro (native OpenWrt) template over the real HTTP/control/state path, with a fake ubus of the device's objects:
board detection, the cellular manager u30pro.cell for connect/disconnect, the screen's direct-supply switch charger_bypass
(the charge limits in uci are not written), and no call to the old paths through u30pro-native-control."""
import json, os, pathlib, socket, subprocess, sys, tempfile, time, urllib.error, urllib.request
BIN=pathlib.Path(sys.argv[1]).resolve()
with tempfile.TemporaryDirectory(prefix='datad-u30pro-test-') as name:
    base=pathlib.Path(name); fixture=base/'device.json'; calls=base/'calls.log'
    token=base/'auth.token'; token.write_text('fixture-u30pro-token')
    fake=base/'ubus'
    fake.write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys
p=pathlib.Path(os.environ['U30_FIXTURE']); args=sys.argv[1:]
if args and args[0]=='-t': args=args[2:]
if args[:1]!=['call']: print('{}'); sys.exit(0)
obj,method=args[1],args[2]; body=json.loads(args[3]) if len(args)>3 else {}
data=json.loads(p.read_text())
def save():
    t=p.with_suffix('.tmp'); t.write_text(json.dumps(data)); t.replace(p)
def reply(v): print(json.dumps(v)); sys.exit(0)
if obj not in ('system','network.device','network.interface.wan','network.interface.lan','uci','luci-rpc','hostapd.wlan0'):
    with open(os.environ['U30_CALLS'],'a') as f: f.write(json.dumps([obj,method,body])+'\\n')
if (obj,method)==('system','board'):
    reply({'board_name':'zte,u30pro','model':'ZTE U30 Pro (MU5358)','hostname':'U30Pro','release':{'description':'OpenWrt 25.12.5'}})
if (obj,method)==('system','info'):
    reply({'uptime':100,'localtime':1792900000,'memory':{'total':1000,'free':400,'available':600},'swap':{'total':0,'free':0}})
if (obj,method)==('u30pro-modem','snapshot'):
    up=data['cell']=='connected'
    reply({'ok':True,'cellular':{'state':data['cell'],'connected':up,'apn':'cmhk','address':'10.1.2.3' if up else '',
        'dns':['10.0.0.53'] if up else [],'device':'sipa_eth0','sim':'READY','operator':'CMHK','signal_csq':20},
        'serving':None,'serving_age_s':None,'sinr_db':None})
if (obj,method)==('u30pro.cell','reconnect'):
    data['cell']='connected'; save(); reply({'accepted':True})
if (obj,method)==('u30pro.cell','disconnect'):
    if data.get('cell_reject'): reply({'accepted':False,'error':'not running'})
    data['cell']='disconnected'; save(); reply({'accepted':True})
if (obj,method)==('u30pro-native-control','get'):
    reply({'soc_temperature_c':41.5,'board_temperatures_c':{'board':38.0},
        'battery':{'available':True,'capacity_percent':80,'temperature_c':30,'voltage_v':4.0,'current_ma':-100,'status':'Discharging'},
        'charger':{'available':True,'policy_available':True,'charge_behaviour':data['behaviour'],'status':'Not charging','online':1,
            'user_iin_ma':1500,'user_ichg_ma':1000,'typec_source':{}}})
if (obj,method)==('u30pro-native-control','charger_bypass'):
    if body.get('enabled') not in ('0','1'): reply({'error':'The setting must be 0 or 1'})
    if not data.get('ignore'): data['behaviour']='inhibit-charge' if body['enabled']=='1' else 'auto'; save()
    reply({'accepted':True,'enabled':data['behaviour']=='inhibit-charge'})
print('{}')
'''); fake.chmod(0o755)
    fixture.write_text(json.dumps({'cell':'connected','behaviour':'auto'}))
    def setup(**changes):
        data=json.loads(fixture.read_text()); data.update(changes)
        p=fixture.with_suffix('.new'); p.write_text(json.dumps(data)); p.replace(fixture)
    def logged(): return [json.loads(line) for line in calls.read_text().splitlines()] if calls.exists() else []
    with socket.socket() as sock: sock.bind(('127.0.0.1',0)); port=sock.getsockname()[1]
    env=dict(os.environ,ZWRT_DATAD_DIR=str(base/'cloud'),ZWRT_DATAD_UBUS='cli',ZWRT_DATAD_UBUS_BIN=str(fake),ZWRT_DATAD_UCI_BIN='/usr/bin/false',
             ZWRT_DATAD_OTA_DISABLE_AUTO='1',U30_FIXTURE=str(fixture),U30_CALLS=str(calls))
    env.pop('ZWRT_DATAD_BACKEND',None)
    proc=subprocess.Popen([str(BIN),'-i','200','-p',str(port),'--auth-token-file',str(token)],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    def req(path,body=None):
        headers={'Content-Type':'application/json','Authorization':'Bearer fixture-u30pro-token'}
        request=urllib.request.Request(f'http://127.0.0.1:{port}{path}',headers=headers,data=None if body is None else json.dumps(body).encode())
        try:
            with urllib.request.urlopen(request,timeout=8) as r: return r.status,json.load(r)
        except urllib.error.HTTPError as e:
            raw=e.read()
            try: data=json.loads(raw)
            except ValueError: data={}
            return e.code,data
    def action(name,params=None): return req('/control',{'action':name,'params':params or {}})
    def wait_state(check):
        for i in range(50):
            state=req('/state')[1]
            if check(state): return state
            time.sleep(.1)
        raise AssertionError('state not refreshed: '+json.dumps(state)[:400])
    try:
        for i in range(60):
            try:
                status,state=req('/state'); break
            except (OSError,urllib.error.URLError): time.sleep(.1)
        else: raise AssertionError('startup')
        assert state['device']['api_template']=='U30PRO' and state['device']['api_template_supported']==1, state['device']
        assert state['device']['model_name']=='MU5358', state['device']
        assert state['interfaces']['cellular']['connected'] is True and state['sim']['state']=='ready', state['interfaces']
        assert state['power']['direct_supply']=={'supported':True,'enabled':False,'mode':'disable'}, state['power']
        caps=req('/capabilities')[1]['control']
        assert {'cellular.connect','cellular.disconnect','power.direct_supply.set'} <= set(caps), caps
        # cellular: the cellular manager's own object
        code,data=action('cellular.disconnect'); assert code==200, (code,data)
        assert ['u30pro.cell','disconnect',{}] in logged(), logged()
        wait_state(lambda s: s['interfaces']['cellular']['connected'] is False)
        code,data=action('cellular.connect'); assert code==200, (code,data)
        assert ['u30pro.cell','reconnect',{}] in logged(), logged()
        wait_state(lambda s: s['interfaces']['cellular']['connected'] is True)
        setup(cell_reject=True); code,data=action('cellular.disconnect'); assert code==502 and 'not running' in json.dumps(data), (code,data)
        setup(cell_reject=False)
        # direct supply: charger_bypass, read back by the plugin
        code,data=action('power.direct_supply.set',{'enabled':True})
        assert code==200 and data['result']=={'supported':True,'enabled':True,'mode':'enable','changed':True,'verified':True}, (code,data)
        assert ['u30pro-native-control','charger_bypass',{'enabled':'1'}] in logged(), logged()
        assert action('power.direct_supply.status')[1]['result']['enabled'] is True
        wait_state(lambda s: s['power']['direct_supply']['enabled'] is True)
        code,data=action('power.direct_supply.set',{'enabled':0})
        assert code==200 and data['result']['enabled'] is False and data['result']['changed'] is True, (code,data)
        assert ['u30pro-native-control','charger_bypass',{'enabled':'0'}] in logged(), logged()
        setup(ignore=True); code,data=action('power.direct_supply.set',{'enabled':True})
        assert code==200 and data['result']['verified'] is False and data['result']['enabled'] is False, (code,data)
        setup(ignore=False)
        before=len(logged()); assert action('power.direct_supply.set',{'enabled':'yes'})[0]==400; assert len(logged())==before
        # the old paths are gone: no cellular_action, and the charge limits are never written
        methods={(o,m) for o,m,_ in logged()}
        assert ('u30pro-native-control','cellular_action') not in methods and ('u30pro-native-control','set_charger_policy') not in methods, methods
        print('U30 Pro template: board detection, u30pro.cell connect/disconnect and errors, charger_bypass set/readback/unverified, no old paths PASS')
    finally:
        proc.terminate()
        try: proc.wait(timeout=8)
        except subprocess.TimeoutExpired: proc.kill(); proc.wait()
