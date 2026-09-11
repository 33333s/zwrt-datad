#!/usr/bin/env python3
"""Exercise actual HTTP, process ownership, persistence and collector failures."""
import json, os, pathlib, socket, subprocess, sys, tempfile, time, urllib.error, urllib.request
from neighbor_parser_test import qsh, snapshot
BIN=pathlib.Path(sys.argv[1]).resolve()
with tempfile.TemporaryDirectory(prefix='datad-neighbor-http-') as name:
    base=pathlib.Path(name); config=base/'neighbor.json'; runtime=base/'runtime'; net=base/'net.json'; payload=base/'payload'; pids=base/'pids'
    token=base/'auth';token.write_text('neighbor-fixture-token')
    net.write_text(json.dumps({'network_type':'LTE-NSA','nr5g_action_channel':640000,'nr5g_pci':123,'wan_active_channel':1650,'lte_pci':222,'nrca':'124,78,0,640000,100;', 'lteca':'0,223,0,3,1650,20,0,-95,-10,10,-60;'}))
    payload.write_bytes(qsh(3640397572,[78,640000,123,0])+snapshot(3657540452,pci=123)+qsh(3640397572,[78,640000,124,0])+snapshot(3657540452,pci=124)+qsh(3640397572,[41,520000,125,0])+snapshot(3657540452,pci=125)+qsh(3640546464,[1650,222,-920,0])+qsh(3640546464,[1650,223,-950,0])+qsh(3640546464,[1650,224,-960,0]))
    ubus=base/'ubus';ubus.write_text('''#!/bin/sh
case "$*" in *nwinfo_get_netinfo*) cat "$NEIGHBOR_NET";; *) echo '{}';; esac
''');ubus.chmod(0o755)
    diag=base/'diag';diag.write_text('''#!/usr/bin/env python3
import os,pathlib,sys,time
args=sys.argv[1:]; ring=pathlib.Path(args[args.index('-o')+1]); out=ring/'fixture.qmdl'
with open(os.environ['NEIGHBOR_PIDS'],'a') as f:f.write(str(os.getpid())+'\\n')
out.write_bytes(pathlib.Path(os.environ['NEIGHBOR_PAYLOAD']).read_bytes())
while True:time.sleep(1)
''');diag.chmod(0o755)
    env=dict(os.environ,ZWRT_DATAD_UBUS_BIN=str(ubus),ZWRT_DATAD_UCI_BIN='/usr/bin/false',ZWRT_DATAD_NEIGHBOR_CONFIG=str(config),ZWRT_DATAD_NEIGHBOR_DIR=str(runtime),ZWRT_DATAD_DIAG_BIN=str(diag),NEIGHBOR_NET=str(net),NEIGHBOR_PAYLOAD=str(payload),NEIGHBOR_PIDS=str(pids))
    processes=[]
    def launch(extra=(),overrides=None):
        with socket.socket() as s:s.bind(('127.0.0.1',0));port=s.getsockname()[1]
        p=subprocess.Popen([str(BIN),'-i','200','-p',str(port),'--auth-token-file',str(token),*extra],env=dict(env,**(overrides or {})),stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL);processes.append(p)
        return port,p
    def req(port,path='/state',body=None,auth=True):
        headers={'Content-Type':'application/json'}
        if auth:headers['Authorization']='Bearer neighbor-fixture-token'
        r=urllib.request.Request(f'http://127.0.0.1:{port}{path}',headers=headers,data=None if body is None else json.dumps(body).encode())
        try:
            with urllib.request.urlopen(r,timeout=4) as response:
                raw=response.read();return response.status,json.loads(raw) if path!='/healthz' else raw.decode()
        except urllib.error.HTTPError as e:return e.code,{}
    def control(port,action,params=None,auth=True):return req(port,'/control',{'action':'neighbor.'+action,'params':params or {}},auth)
    def wait(port,predicate,timeout=12):
        end=time.monotonic()+timeout;last=None
        while time.monotonic()<end:
            try:
                last=req(port)[1]['neighbor']
                if predicate(last):return last
            except (OSError,KeyError,urllib.error.URLError):pass
            time.sleep(.15)
        raise AssertionError(('timeout',last))
    def stop(p):
        p.terminate()
        try:p.wait(timeout=6)
        except subprocess.TimeoutExpired:p.kill();p.wait();raise AssertionError('unbounded shutdown')
    try:
        port,proc=launch(); n=wait(port,lambda n:n['status']=='disabled');assert not runtime.exists()
        assert control(port,'set',{'enabled':True},False)[0]==401
        assert control(port,'set',{'enabled':'enable'})[0]==400
        assert control(port,'set',{'enabled':True})[0]==200
        n=wait(port,lambda n:n['status']=='ready');assert {(c['rat'],c['pci']) for c in n['cells']}=={('NR',125),('LTE',224)},n
        assert next(c for c in n['cells'] if c['rat']=='NR')['frequency_relation']=='inter'
        old=config.stat().st_mtime_ns;assert control(port,'set',{'enabled':True})[0]==200;assert config.stat().st_mtime_ns==old
        assert control(port,'status')[1]['result']['enabled']
        sampled=n['sampled_at'];age=n['age_ms'];time.sleep(1.2);n=wait(port,lambda n:n['status']=='ready');assert n['age_ms']>age and abs(n['sampled_at']-sampled)<=1
        # Firmware unknown-ID aliases must not repeatedly discard valid capture.
        gen=n['generation']; stable=json.loads(net.read_text())
        for unknown in (0,4294967295,'0xFFFFFFFF',-1,''):
            stable['nr5g_cell_id']=unknown; net.write_text(json.dumps(stable)); time.sleep(.9)
            check=req(port)[1]['neighbor']; assert check['generation']==gen and check['status']=='ready',check
        for identity in (123456789,123456790):
            stable['nr5g_cell_id']=identity; net.write_text(json.dumps(stable))
            n=wait(port,lambda n:n['generation']>gen and n['status']=='ready'); gen=n['generation']
        port2,proc2=launch();wait(port2,lambda n:n['reason']=='another_neighbor_instance');stop(proc2)
        gen=n['generation'];net.write_text(json.dumps({'network_type':'LTE','wan_active_channel':1650,'lte_pci':222}))
        n=wait(port,lambda n:n['generation']>gen and n['status']=='ready');assert {c['rat'] for c in n['cells']}=={'LTE'}
        started=time.monotonic();assert req(port,'/healthz')[0]==200;assert time.monotonic()-started<2
        assert control(port,'set',{'enabled':False})[0]==200;wait(port,lambda n:n['status']=='disabled' and not n['collector_running']);stop(proc)
        assert not list(runtime.glob('capture.*'))
        port,proc=launch();wait(port,lambda n:n['status']=='disabled');stop(proc)
        # B28 identities + comparison report reach HTTP with serving filtering.
        net.write_text(json.dumps(stable))
        payload.write_bytes(qsh(3640166840,[78,640000,123,3])+qsh(3640387444,[41,520000,125,3])+snapshot(3657934788,pci=125))
        port,proc=launch(('--neighbor',));n=wait(port,lambda n:n['status']=='ready')
        assert len(n['cells'])==1,n
        c=n['cells'][0];assert (c['rat'],c['pci'],c['arfcn'],c['band'],c['rsrp_dbm'])==('NR',125,520000,41,-90),c
        assert c['frequency_relation']=='inter' and c['frequency_evidence']=='explicit'
        stop(proc)
        # Explicit B28 inter-frequency results carry their own ARFCN and signal.
        payload.write_bytes(qsh(3657646332,[0,628704,587,-121*128,-15*128,0,1])+qsh(3640387444,[78,628704,587,3]))
        port,proc=launch(('--neighbor',));n=wait(port,lambda n:n['status']=='ready')
        c=n['cells'][0];assert (c['pci'],c['arfcn'],c['band'],c['rsrp_dbm'])==(587,628704,78,-121),c
        assert c['frequency_relation']=='inter' and c['frequency_evidence']=='explicit'
        stop(proc)
        # Valid transport with an unknown firmware map must say why it is empty.
        payload.write_bytes(qsh(0x12345678,[1,2,3]));port,proc=launch(('--neighbor',));n=wait(port,lambda n:n['reason']=='no_supported_reports');assert n['frames']==1 and n['cells']==[];stop(proc)
        port,proc=launch(('--neighbor',),{'ZWRT_DATAD_DIAG_BIN':str(base/'missing')});wait(port,lambda n:n['status']=='dependency_missing');assert req(port,'/healthz')[0]==200;stop(proc)
        # Directory budget violation stops only the collector and remains observable.
        diag.write_text('#!/usr/bin/env python3\nimport sys,pathlib,time\na=sys.argv; p=pathlib.Path(a[a.index("-o")+1]); (p/"one.bin").write_bytes(bytes(12*1024*1024)); (p/"two.bin").write_bytes(bytes(12*1024*1024)); (p/"three.bin").write_bytes(bytes(12*1024*1024)); time.sleep(60)\n')
        port,proc=launch(('--neighbor',));wait(port,lambda n:n['reason']=='capture_limit');assert req(port,'/healthz')[0]==200;stop(proc)
        print('neighbor HTTP: default off, auth, persistent toggle, serving/CA filter, NSA, context reset, freshness, lock exclusion, dependency/map/capture errors, health responsiveness and cleanup PASS')
    finally:
        for p in processes:
            if p.poll() is None:stop(p)
        if pids.exists():
            for pid in map(int,pids.read_text().splitlines()):
                try:os.kill(pid,0)
                except ProcessLookupError:continue
                raise AssertionError(('orphan collector',pid))
