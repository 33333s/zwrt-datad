#!/usr/bin/env python3
"""Long-charge protection over HTTP against an emulated ZTE battery driver.

The emulator follows the driver as read from the TopFlow kernel: "2 N" sets the
period and drops the mode to 1 without leaving a running hold, "1 1" restores
the default only while the debug flag is set, and any write to
ui_chg_policy_mode resets the charging state (mode 1, hold left). The stock
bat_mode follows the hold, not the mode: a threshold write leaves it at 2.
"""
import json, os, pathlib, socket, subprocess, sys, tempfile, threading, time, urllib.error, urllib.request
BIN = pathlib.Path(sys.argv[1]).resolve()

HELP = """===========current state============

customer mode is {mode}

*****customer mode define info*****
0: CUSTOMER_UNKNOWN_MODE
1: CUSTOMER_DEFAULT_MODE
2: CUSTOMER_SINGLE_LONG_CHARGING_MODE

=====customer mode details parameter=====
2: CUSTOMER_SINGLE_LONG_CHARGING_MODE parameter
single_lcm_period is {period} minutes

=====customer_mode_dbg details=====
customer_mode_dbg is {debug:x}
"""


class Driver:
    def __init__(self, root):
        self.root = root
        self.lock = threading.Lock()
        self.stop = False
        self.resets = 0
        self.boot()
        threading.Thread(target=self.run, daemon=True).start()

    def boot(self, hold=False):
        with self.lock:
            self.mode, self.period, self.debug, self.policy = 2 if hold else 1, 960, 0, 0
            self.hold = hold
            self.root.mkdir(parents=True, exist_ok=True)
            (self.root / 'ui_chg_policy_mode').write_text('0\n')
            self.policy_stamp = (self.root / 'ui_chg_policy_mode').stat().st_mtime_ns
            self.publish()

    def publish(self):
        (self.root / 'customer_mode').write_text(f'{self.mode}\n')
        (self.root / 'oem_bat_mode').write_text('2' if self.hold else '1')
        (self.root / 'customer_mode_help').write_text(HELP.format(mode=self.mode, period=self.period, debug=self.debug))

    def enter_hold(self):
        with self.lock:
            self.mode, self.hold = 2, True
            self.publish()

    def run(self):
        while not self.stop:
            time.sleep(0.01)
            with self.lock:
                try:
                    command = (self.root / 'customer_mode').read_text().split()
                    stamp = (self.root / 'ui_chg_policy_mode').stat().st_mtime_ns
                except OSError:
                    continue
                if len(command) == 2:
                    a, b = int(command[0]), int(command[1])
                    if a == 2 and b >= 1:
                        self.debug, self.period, self.mode = 1, b, 1
                    elif a == 1 and b != 0 and self.debug:
                        self.debug, self.period, self.mode = 0, 960, 1
                    self.publish()
                if stamp != self.policy_stamp:
                    try:
                        policy = int((self.root / 'ui_chg_policy_mode').read_text())
                    except (OSError, ValueError):
                        continue  # caught between truncate and write; sysfs stores are atomic
                    self.policy_stamp = stamp
                    self.policy = policy
                    self.mode, self.hold = 1, False
                    self.resets += 1
                    self.publish()


with tempfile.TemporaryDirectory(prefix='datad-charge-test-') as name:
    base = pathlib.Path(name)
    data = base / 'cloud'
    driver = Driver(base / 'battery_zte')
    token = base / 'auth.token'
    token.write_text('fixture-charge-token')
    fake = base / 'ubus'
    fake.write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys
args = sys.argv[1:]
if args and args[0] == '-t': args = args[2:]
if args[:3] == ['call', 'zwrt_bsp.battery', 'list']:
    print(json.dumps({'battery_online': 1, 'battery_capacity': 100, 'battery_temperature': 30, 'battery_health': 1}))
elif args[:3] == ['call', 'zwrt_bsp.charger', 'list']:
    print(json.dumps({'charger_connect': 1, 'charge_status': 4}))
elif args[:3] == ['call', 'zwrt_mc.device.manager', 'get_device_info']:
    mode = (pathlib.Path(os.environ['ZWRT_DATAD_BATTERY_ZTE_DIR']) / 'oem_bat_mode').read_text()
    print(json.dumps({'bat_mode': mode, 'hightemp_datalimit_status': '0'}))
else:
    print('{}')
''')
    fake.chmod(0o755)
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
    env = dict(os.environ, ZWRT_DATAD_DIR=str(data), ZWRT_DATAD_UBUS_BIN=str(fake), ZWRT_DATAD_UCI_BIN='/usr/bin/false',
               ZWRT_DATAD_BATTERY_ZTE_DIR=str(driver.root))

    def start():
        return subprocess.Popen([str(BIN), '-i', '200', '-p', str(port), '--auth-token-file', str(token)],
                                env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    def stop(proc):
        proc.terminate()
        try:
            proc.wait(timeout=8)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()

    def req(path, body=None):
        headers = {'Content-Type': 'application/json', 'Authorization': 'Bearer fixture-charge-token'}
        request = urllib.request.Request(f'http://127.0.0.1:{port}{path}', headers=headers,
                                         data=None if body is None else json.dumps(body).encode())
        try:
            with urllib.request.urlopen(request, timeout=8) as r:
                return r.status, json.load(r)
        except urllib.error.HTTPError as e:
            raw = e.read()
            try:
                return e.code, json.loads(raw)
            except ValueError:
                return e.code, {}

    def action(which, params=None):
        return req('/control', {'action': 'battery.charge_protection.' + which, 'params': params or {}})

    def wait_up():
        for _ in range(100):
            try:
                return req('/state')[1]
            except (OSError, urllib.error.URLError):
                time.sleep(.1)
        raise AssertionError('startup')

    proc = start()
    try:
        state = wait_up()
        caps = req('/capabilities')[1]['control']
        assert {'battery.charge_protection.status', 'battery.charge_protection.release', 'battery.charge_protection.set'} <= set(caps)
        protection = state['battery']['charge_protection']
        assert protection == {'active': False, 'mode': 1, 'supported': True, 'disabled': False, 'threshold_minutes': 960}, protection
        status = action('status')[1]['result']
        assert status == {'supported': True, 'active': False, 'mode': 1, 'disabled': False, 'threshold_minutes': 960,
                          'default_threshold_minutes': 960}, status

        resets = driver.resets
        result = action('release')[1]['result']
        assert result['released'] is False and driver.resets == resets

        driver.enter_hold()
        assert action('status')[1]['result']['active'] is True
        code, data_ = action('release')
        assert code == 200 and data_['result']['released'] and data_['result']['verified'], data_
        assert data_['result']['active'] is False
        for _ in range(50):  # the emulator applies writes asynchronously; the driver does not
            if not driver.hold and driver.resets > resets:
                break
            time.sleep(.02)
        else:
            raise AssertionError('hold kept')
        assert driver.policy == 0

        # Somebody else set the threshold during a hold: mode 1, hold running.
        driver.enter_hold()
        (driver.root / 'customer_mode').write_text('2 30')
        for _ in range(50):
            if driver.period == 30:
                break
            time.sleep(.05)
        assert driver.mode == 1 and driver.hold
        assert action('status')[1]['result']['active'] is True
        reply = action('release')[1]
        assert reply['result']['released'] is True, reply
        for _ in range(50):  # the emulator applies writes asynchronously; the driver does not
            if not driver.hold:
                break
            time.sleep(.02)
        else:
            raise AssertionError('hold kept')
        (driver.root / 'customer_mode').write_text('1 1')
        for _ in range(50):
            if driver.period == 960 and driver.debug == 0:
                break
            time.sleep(.05)
        assert (driver.root / 'ui_chg_policy_mode').read_text().strip() == '0'

        for value in (None, 2, 'yes', [], {}):
            code, _ = action('set', {} if value is None else {'disabled': value})
            assert code == 400, (value, code)
        assert not (data / 'charge_protection.conf').exists()

        code, reply = action('set', {'disabled': True}); assert code == 200, reply; result = reply['result']
        assert result['changed'] and result['verified'] and result['disabled'] and result['threshold_minutes'] == 71582, result
        assert driver.debug == 1 and driver.period == 71582
        assert (data / 'charge_protection.conf').read_text() == 'disabled=1\n'
        assert action('set', {'disabled': 1})[1]['result']['changed'] is False
        for _ in range(30):
            protection = req('/state')[1]['battery']['charge_protection']
            if protection['disabled'] and protection['threshold_minutes'] == 71582:
                break
            time.sleep(.1)
        else:
            raise AssertionError(protection)

        # A reboot restores the driver default and may start a hold before
        # datad is up; the saved switch is applied again on start.
        stop(proc)
        driver.boot(hold=True)
        proc = start()
        wait_up()
        for _ in range(50):
            if driver.period == 71582 and not driver.hold:
                break
            time.sleep(.1)
        else:
            raise AssertionError((driver.period, driver.mode, driver.hold))
        assert action('status')[1]['result']['disabled'] is True

        result = action('set', {'disabled': False})[1]['result']
        assert result['changed'] and not result['disabled'] and result['threshold_minutes'] == 960, result
        assert driver.debug == 0 and driver.period == 960
        assert (data / 'charge_protection.conf').read_text() == 'disabled=0\n'

        for node in ('customer_mode', 'customer_mode_help', 'ui_chg_policy_mode'):
            (driver.root / node).unlink()
        driver.stop = True
        assert action('status')[1]['result'] == {'supported': False, 'disabled': False}
        assert action('release')[0] == 502 and action('set', {'disabled': True})[0] == 502
        for _ in range(100):
            protection = req('/state')[1]['battery']['charge_protection']
            if 'supported' not in protection:
                break
            time.sleep(.1)
        else:
            raise AssertionError(protection)
        print('charge protection: status, state, release, invalid input, disable/persist/boot re-apply, restore default, unsupported PASS')
    finally:
        driver.stop = True
        stop(proc)
