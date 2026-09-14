#!/usr/bin/env python3
import os
import signal
import subprocess
import sys
import time

if "--fixture-daemon" in sys.argv:
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
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
with open(pidfile, "w", encoding="ascii") as handle:
    handle.write(f"{child.pid}\n")
