#!/usr/bin/env python3
"""U30 Pro (native OpenWrt) template over the real HTTP/control/state path, with a fake ubus of the device's objects:
board detection, the cellular manager u30pro.cell for connect/disconnect, the screen's direct-supply switch charger_bypass
(the charge limits in uci are not written), no call to the old paths through u30pro-native-control, and the LAN login with the
router account (admin, what apps for the ZTE firmware send, is checked as root)."""
import base64, json, os, pathlib, socket, subprocess, sys, tempfile, time, urllib.error, urllib.request
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
    reply({'ok':True,'cellular':{'state':data['cell'],'connected':up,'desired_up':up,'apn':'cmhk','pdp':'ipv4v6','address':'10.1.2.3' if up else '',
        'dns':['10.0.0.53'] if up else [],'device':'sipa_eth0','sim':'READY','operator':'CMHK','signal_csq':20},
        'serving':None,'serving_age_s':None,'sinr_db':None})
if (obj,method)==('network.interface.wan','status'):
    up=data['cell']=='connected'
    reply({'up':up,'l3_device':'sipa_eth0','uptime':60,'ipv4-address':[{'address':'10.1.2.3','mask':30}] if up else [],'ipv6-address':[]})
if (obj,method)==('u30pro-modem','sms_list'):
    reply({'ok':True,'capacity':{'used':2,'total':50},'messages':[{'index':4,'stat':0,'address':'10086','ts':1792900000,'text':'new'},
        {'index':5,'stat':1,'address':'10010','ts':1792800000,'text':'old'}]})
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
# rpcd: only the router account root, with its password; a refused login is an error of the ubus call
if (obj,method)==('session','login'):
    if body.get('username')=='root' and body.get('password')=='fixture-root-pass': reply({'ubus_rpc_session':'0123456789abcdef0123456789abcdef'})
    sys.exit(6)
if (obj,method)==('session','destroy'): reply({})
print('{}')
'''); fake.chmod(0o755)
    fixture.write_text(json.dumps({'cell':'connected','behaviour':'auto'}))
    serial=base/'serial-number'; serial.write_bytes(b'0099FIXTURE\0')
    def setup(**changes):
        data=json.loads(fixture.read_text()); data.update(changes)
        p=fixture.with_suffix('.new'); p.write_text(json.dumps(data)); p.replace(fixture)
    def logged(): return [json.loads(line) for line in calls.read_text().splitlines()] if calls.exists() else []
    ports=[]
    for _ in range(2):
        with socket.socket() as sock: sock.bind(('127.0.0.1',0)); ports.append(sock.getsockname()[1])
    port,lan_port=ports
    env=dict(os.environ,ZWRT_DATAD_DIR=str(base/'cloud'),ZWRT_DATAD_UBUS='cli',ZWRT_DATAD_UBUS_BIN=str(fake),ZWRT_DATAD_UCI_BIN='/usr/bin/false',
             ZWRT_DATAD_OTA_DISABLE_AUTO='1',U30_FIXTURE=str(fixture),U30_CALLS=str(calls),
             ZWRT_DATAD_U30PRO_SERIAL_FILE=str(serial))
    env.pop('ZWRT_DATAD_BACKEND',None)
    # the LAN listener (login required; loopback counts as a LAN source) next to the local one
    proc=subprocess.Popen([str(BIN),'-i','200','-p',str(port),'--auth-token-file',str(token),'--lan-bind','127.0.0.1','--lan-port',str(lan_port),'--webshell'],
                          env=env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    def login(user,password):
        basic=base64.b64encode(f'{user}:{password}'.encode()).decode()
        request=urllib.request.Request(f'http://127.0.0.1:{lan_port}/auth/login',method='POST',headers={'Authorization':'Basic '+basic})
        try:
            with urllib.request.urlopen(request,timeout=8) as r: return r.status,json.load(r)
        except urllib.error.HTTPError as e: return e.code,{}
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
        assert state['device']['serial_number']=='0099FIXTURE', state['device']
        # cloud management defaults of this device: no UFI or WebSSH back end, LuCI on port 80
        code,cloud=req('/cloud/config')
        assert code==200 and cloud['config']['platform']=='generic' and cloud['config']['enabled'] is False, cloud
        assert cloud['config']['services']==[{'name':'设备后台','port':80,'kind':'web'}], cloud['config']['services']
        # the field names of the ZTE templates
        cell=state['interfaces']['cellular']
        assert cell['connect_status']=='ipv4_connected' and cell['enable']==1 and cell['connect_mode']=='auto', cell
        assert cell['ipv4_dev_name']=='sipa_eth0' and cell['pdp_type']=='IPV4V6' and cell['ipv4_address']=='10.1.2.3', cell
        assert 'connected' not in cell and 'state' not in cell, cell
        assert state['sim']['state']=='ready' and state['sim']['dual_sim']==0 and state['sim']['current_slot']==1, state['sim']
        assert {'controllers','devices'} <= set(state.get('usb',{})), state.get('usb')
        sms=state['sms']; assert sms['truncated'] is False and [m['tag'] for m in sms['list']]==[1,0] and sms['unread']==1, sms
        assert {'tcp4','udp4'} <= set(state['runtime']['connections']) and isinstance(state['runtime']['link_rates'],list), state['runtime']
        assert state['thermal']['protection']['active'] is False and state['battery']['charge_protection']=={'active':False}, state['thermal']
        assert state['power']['direct_supply']=={'supported':True,'enabled':False,'mode':'disable'}, state['power']
        caps=req('/capabilities')[1]['control']
        assert {'cellular.connect','cellular.disconnect','power.direct_supply.set'} <= set(caps), caps
        # cellular: the cellular manager's own object
        code,data=action('cellular.disconnect'); assert code==200, (code,data)
        assert ['u30pro.cell','disconnect',{}] in logged(), logged()
        wait_state(lambda s: s['interfaces']['cellular']['connect_status']=='disconnected')
        code,data=action('cellular.connect'); assert code==200, (code,data)
        assert ['u30pro.cell','reconnect',{}] in logged(), logged()
        wait_state(lambda s: s['interfaces']['cellular']['connect_status']=='ipv4_connected')
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
        # a temporary token from the loopback listener (for the device's own pages), valid on the LAN listener's web shell
        code,data=req('/auth/local',{})
        assert code==200 and data['token_type']=='Bearer' and data['access_token'], (code,data)
        def lan(path,token=None):
            headers={'Authorization':'Bearer '+token} if token else {}
            try:
                with urllib.request.urlopen(urllib.request.Request(f'http://127.0.0.1:{lan_port}{path}',headers=headers),timeout=8) as r: return r.status,json.load(r)
            except urllib.error.HTTPError as e: return e.code,{}
        code,shell=lan('/webshell/status',data['access_token'])
        assert code==200 and shell['enabled'] is True, (code,shell)
        assert lan('/webshell/status')[0]==401 and lan('/webshell/status','wrong')[0]==401
        request=urllib.request.Request(f'http://127.0.0.1:{lan_port}/auth/local',method='POST',data=b'{}',headers={'Content-Type':'application/json'})
        try: urllib.request.urlopen(request,timeout=8); assert False, '/auth/local must not exist on the LAN listener'
        except urllib.error.HTTPError as e: assert e.code in (401,404), e.code
        # LAN login: the router account root, and admin (what apps for the ZTE firmware send) checked as root
        for user,password,want in (('root','fixture-root-pass',200),('admin','fixture-root-pass',200),('admin','wrong',401),
                                   ('Admin','fixture-root-pass',401),('operator','fixture-root-pass',401)):
            code,data=login(user,password); assert code==want,(user,password,code,data)
            if want==200: assert data['ok'] is True and data['token_type']=='Bearer' and data['access_token'], data
        users={b['username'] for o,m,b in logged() if (o,m)==('session','login')}
        assert users=={'root','Admin','operator'}, users
        print('U30 Pro template: board detection, board serial and cloud defaults, ZTE field names (cellular, sim, sms tags, usb, runtime, protection), u30pro.cell '
              'connect/disconnect and errors, charger_bypass set/readback/unverified, no old paths, local token for the web shell, LAN login as root or admin PASS')
    finally:
        proc.terminate()
        try: proc.wait(timeout=8)
        except subprocess.TimeoutExpired: proc.kill(); proc.wait()
