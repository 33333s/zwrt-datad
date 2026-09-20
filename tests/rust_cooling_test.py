#!/usr/bin/env python3
"""Fan discovery and recovery regression; all hardware paths are fixtures."""
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

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(sys.argv[1] if len(sys.argv) > 1 else ROOT / "rust/target/debug/zwrt-datad").resolve()


def wait_for(predicate, message, seconds=15):
    until = time.monotonic() + seconds
    while time.monotonic() < until:
        try:
            if predicate():
                return
        except (OSError, ValueError, KeyError):
            pass
        time.sleep(0.1)
    raise AssertionError(message)


with tempfile.TemporaryDirectory(prefix="datad-cooling-") as directory:
    base = Path(directory)
    thermal = base / "thermal"
    zone = thermal / "thermal_zone42"
    cpu = thermal / "thermal_zone0"
    fan = thermal / "cooling_device9"
    hotplug = thermal / "cooling_device0"
    for path in (zone, cpu, fan, hotplug, base / "data", base / "net", base / "proc", base / "uci"):
        path.mkdir(parents=True)
    for path, value in (
        (zone / "type", "sys-therm-4"), (zone / "temp", "64000"), (zone / "mode", "enabled"),
        (cpu / "type", "sdr0_pa"), (cpu / "temp", "-273000"), (cpu / "mode", "enabled"),
        (fan / "type", "pwm-fan"), (fan / "cur_state", "2"),
        (hotplug / "type", "cpu-hotplug1"), (hotplug / "cur_state", "2"),
        (base / "pwm", "76"), (base / "fan-thermal", "thermal_enable:1"),
        (base / "low-speed", "low_speed_mode:1, low_speed_max:76"),
        (base / "qos", ""), (base / "qos.0", ""), (base / "leases", ""),
    ):
        path.write_text(value)
    for index, temperature in enumerate((44, 48, 53)):
        (zone / f"trip_point_{index}_temp").write_text(str(temperature * 1000))
        (zone / f"trip_point_{index}_hyst").write_text("4000")

    env = {k: v for k, v in os.environ.items() if not k.startswith(("ZWRT_DATAD_", "MOCK_"))}
    env.update({
        "ZWRT_DATAD_UBUS_BIN": str(ROOT / "tests/mock_ubus.sh"),
        "ZWRT_DATAD_UCI_BIN": str(ROOT / "tests/mock_uci.sh"),
        "ZWRT_DATAD_IW_BIN": "/usr/bin/false", "MOCK_MODEL_NAME": "MU5252",
        "MOCK_UCI_STATE_DIR": str(base / "uci"), "MOCK_CALL_LOG": str(base / "calls"),
        "ZWRT_DATAD_OTA_DISABLE_AUTO": "1", "ZWRT_DATAD_COOLING_CONFIG": str(base / "cooling.conf"),
        "ZWRT_DATAD_THERMAL_ROOT": str(thermal), "ZWRT_DATAD_NET_CLASS_ROOT": str(base / "net"),
        "ZWRT_DATAD_PROC_ROOT": str(base / "proc"), "ZWRT_DATAD_FAN_PWM_PATH": str(base / "pwm"),
        "ZWRT_DATAD_FAN_THERMAL_ENABLE_PATH": str(base / "fan-thermal"),
        "ZWRT_DATAD_FAN_LOW_SPEED_MODE_PATH": str(base / "low-speed"),
        "ZWRT_DATAD_QOS_LOG": str(base / "qos"), "ZWRT_DATAD_QOS_LOG_ROTATED": str(base / "qos.0"),
        "ZWRT_DATAD_DHCP_LEASES_PATH": str(base / "leases"),
    })
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    url = f"http://127.0.0.1:{port}"

    def request(path, payload=None):
        data = None if payload is None else json.dumps(payload).encode()
        req = urllib.request.Request(url + path, data=data, headers={"Content-Type": "application/json"})
        try:
            with urllib.request.urlopen(req, timeout=5) as response:
                body = response.read()
                return response.status, json.loads(body) if body.startswith(b"{") else body
        except urllib.error.HTTPError as error:
            return error.code, json.loads(error.read())

    def control(mode="custom"):
        return request("/control", {"action": "cooling.fan.set_mode", "params": {"mode": mode}})

    def state():
        return request("/state")[1]["cooling"]["fan"]

    with (base / "server.log").open("w+") as log:
        process = subprocess.Popen([str(BINARY), "-b", "127.0.0.1", "-p", str(port), "--data-dir", str(base / "data")], env=env, stdout=log, stderr=log)
        try:
            wait_for(lambda: request("/healthz")[0] == 200, "server did not start")
            points = [{"temperature": t, "pwm": p} for t, p in ((40, 0), (47, 41), (58, 74), (65, 120), (73, 128), (80, 255))]
            assert request("/control", {"action": "cooling.fan.set_curve", "params": {"points": points}})[0] == 200
            wait_for(lambda: state()["pwm"] == 113 and state()["policy"]["applied"], "curve/discovery failed")
            assert state()["temperature_celsius"] == 64
            assert state()["temperature_source"] == str(zone)
            assert (base / "low-speed").read_text() == "0"
            assert (hotplug / "cur_state").read_text() == "2"
            assert (cpu / "mode").read_text() == "enabled"
            saved_curve = (base / "cooling.conf").read_text()

            # A missing/invalid sensor must restore the actual fan zone, not
            # leave custom mode latched with its last PWM and no controller.
            for invalid in ("-273000", "not-a-temperature"):
                (zone / "temp").write_text(invalid)
                wait_for(lambda: (zone / "mode").read_text() == "enabled", "sensor failure left fan zone disabled")
                wait_for(lambda: not state()["policy"]["applied"], "sensor failure was hidden")
                assert "temperature unavailable" in state()["policy"]["error"]
                assert (base / "cooling.conf").read_text() == saved_curve
                (zone / "temp").write_text("64000")
                wait_for(lambda: state()["policy"]["applied"] and (zone / "mode").read_text() == "disabled", "curve did not recover")

            (fan / "type").write_text("not-a-fan")
            assert control()[0] == 502
            assert (zone / "mode").read_text() == "enabled"
            assert (hotplug / "cur_state").read_text() == "2"
            (fan / "type").write_text("pwm-fan")
            assert control()[0] == 200

            # An unavailable driver write also restores kernel control.
            (base / "pwm").unlink()
            (base / "pwm").mkdir()
            assert control("always_on")[0] == 502
            assert (zone / "mode").read_text() == "enabled"
            assert (base / "cooling.conf").read_text() == saved_curve
            (base / "pwm").rmdir()
            (base / "pwm").write_text("76")
            assert control()[0] == 200

            # No fallback to the unrelated thermal_zone0 / cooling_device0.
            (zone / "type").write_text("not-the-fan-zone")
            assert control()[0] == 502
            assert (cpu / "mode").read_text() == "enabled"
            assert (hotplug / "cur_state").read_text() == "2"
            (zone / "type").write_text("sys-therm-4")
            assert control()[0] == 200
            process.terminate()
            process.wait(timeout=15)
            assert (zone / "mode").read_text() == "enabled", "shutdown left kernel control disabled"
            print("cooling discovery, quiet-mode release, sensor/driver recovery and shutdown OK")
        except Exception:
            log.flush()
            print((base / "server.log").read_text(), file=sys.stderr)
            raise
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
