#!/usr/bin/env python3
"""4G neighbours come from the modem's own scan list over real HTTP, with no DIAG collector."""
import json, os, pathlib, socket, subprocess, sys, tempfile, time, urllib.error, urllib.request

BIN = pathlib.Path(sys.argv[1]).resolve()
RAW = '222,1650,B3,-95,-10;223,1650,B3,-101,-15;131,3590,B8,-108,-17;bad;'

with tempfile.TemporaryDirectory(prefix='datad-neighbor-lte-') as name:
    base = pathlib.Path(name)
    token = base / 'auth'
    token.write_text('lte-fixture-token')
    net = base / 'net.json'
    scan = base / 'scan'
    ubus = base / 'ubus'
    ubus.write_text('#!/bin/sh\ncase "$*" in *nwinfo_get_netinfo*) cat "$LTE_NET";; *) echo "{}";; esac\n')
    ubus.chmod(0o755)
    uci = base / 'uci'
    uci.write_text('''#!/bin/sh
if [ "$1" = "-q" ] && [ "$2" = "show" ] && [ "$3" = "zte_nwinfo" ]; then
    [ -f "$LTE_SCAN" ] && printf "zte_nwinfo.manual_scan.lteg_nbr_content='%s'\\n" "$(cat "$LTE_SCAN")"
    exit 0
fi
exit 1
''')
    uci.chmod(0o755)
    env = dict(os.environ, ZWRT_DATAD_DIR=str(base / 'cloud'), ZWRT_DATAD_UBUS_BIN=str(ubus),
               ZWRT_DATAD_UCI_BIN=str(uci), ZWRT_DATAD_NEIGHBOR_CONFIG=str(base / 'neighbor.json'),
               ZWRT_DATAD_NEIGHBOR_DIR=str(base / 'runtime'), ZWRT_DATAD_DIAG_BIN=str(base / 'missing-diag'),
               LTE_NET=str(net), LTE_SCAN=str(scan))
    procs = []

    def launch():
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            port = sock.getsockname()[1]
        proc = subprocess.Popen([str(BIN), '-i', '200', '-p', str(port), '--auth-token-file', str(token)],
                                env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        procs.append(proc)
        return port

    def get(port, path='/state', body=None):
        headers = {'Content-Type': 'application/json', 'Authorization': 'Bearer lte-fixture-token'}
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(f'http://127.0.0.1:{port}{path}', headers=headers, data=data)
        with urllib.request.urlopen(request, timeout=4) as response:
            return json.load(response)

    def wait_for(port, check):
        last = None
        for _ in range(150):
            try:
                last = get(port)['neighbor']
                if check(last):
                    return last
            except (OSError, urllib.error.URLError, KeyError):
                pass
            time.sleep(.1)
        raise AssertionError(f'timed out: {last}')

    try:
        net.write_text(json.dumps({'network_type': 'LTE-NSA', 'wan_active_channel': 1650, 'lte_pci': 222}))
        scan.write_text(RAW)
        port = launch()
        neighbor = wait_for(port, lambda n: n['lte']['status'] == 'ready')
        assert neighbor['enabled'] is False and neighbor['cells'] == [], neighbor
        lte = neighbor['lte']
        assert lte['supported'] is True and lte['source'] == 'vendor_scan', lte
        cells = {(c['pci'], c['arfcn']): c for c in lte['cells']}
        assert set(cells) == {(223, 1650), (131, 3590)}, f'serving 222/1650 and the bad record are dropped: {cells}'
        assert cells[(223, 1650)]['frequency_relation'] == 'intra' and cells[(223, 1650)]['band'] == 3
        assert cells[(131, 3590)]['frequency_relation'] == 'inter' and cells[(131, 3590)]['rsrq_db'] == -17
        assert lte['cells'][0]['rsrp_dbm'] >= lte['cells'][1]['rsrp_dbm'], 'strongest first'
        status = get(port, '/control', {'action': 'neighbor.status', 'params': {}})['result']
        assert status['lte']['status'] == 'ready' and len(status['lte']['cells']) == 2, status

        scan.write_text('223,1650,B3,-90,-9;')
        neighbor = wait_for(port, lambda n: [c['pci'] for c in n['lte']['cells']] == [223])
        assert neighbor['lte']['cells'][0]['rsrp_dbm'] == -90, neighbor

        # Stand-alone 5G has no LTE anchor, so a leftover list is not reported.
        net.write_text(json.dumps({'network_type': 'SA', 'wan_active_channel': 1650, 'lte_pci': 222}))
        neighbor = wait_for(port, lambda n: n['lte']['status'] == 'unavailable')
        assert neighbor['lte']['reason'] == 'not_on_lte' and neighbor['lte']['cells'] == [], neighbor

        # Firmware without the field: explicit "not reported", never an empty success.
        scan.unlink()
        net.write_text(json.dumps({'network_type': 'LTE', 'wan_active_channel': 1650, 'lte_pci': 222}))
        neighbor = wait_for(port, lambda n: n['lte']['reason'] == 'not_reported')
        assert neighbor['lte']['supported'] is False and neighbor['lte']['status'] == 'unavailable', neighbor
        print('4G neighbours from the modem scan list: state, serving filter, SA gating and unsupported firmware PASS')
    finally:
        for proc in procs:
            proc.terminate()
            try:
                proc.wait(timeout=8)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
