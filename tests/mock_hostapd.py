#!/usr/bin/env python3
import os
import signal
import shutil
import subprocess
import sys
import time

if "--fixture-daemon" in sys.argv:
    proc_entry = os.path.join(os.environ["ZWRT_DATAD_PROC_ROOT"], str(os.getpid()))

    def stop(*_):
        shutil.rmtree(proc_entry, ignore_errors=True)
        sys.exit(0)

    signal.signal(signal.SIGTERM, stop)
    while True:
        time.sleep(60)

args = sys.argv[1:]
pidfile = args[args.index("-P") + 1]
config = args[-1]
child = subprocess.Popen(
    [sys.executable, __file__, "--fixture-daemon", config],
    stdin=subprocess.DEVNULL,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
    start_new_session=True,
)
proc_entry = os.path.join(os.environ["ZWRT_DATAD_PROC_ROOT"], str(child.pid))
os.makedirs(proc_entry, exist_ok=True)
with open(os.path.join(proc_entry, "cmdline"), "wb") as handle:
    handle.write(b"hostapd\0" + config.encode() + b"\0")
with open(pidfile, "w", encoding="ascii") as handle:
    handle.write(f"{child.pid}\n")
