#!/bin/bash
# /forkd-init.sh — PID 1 inside the guest. Warms Python + numpy into memory,
# then parks in time.sleep so snapshot captures the loaded state. Forked
# children resume from the sleep with numpy already imported.

mount -t proc proc /proc 2>/dev/null
mount -t sysfs sys /sys 2>/dev/null
mount -t devtmpfs devtmpfs /dev 2>/dev/null

echo "forkd-init: warming python..."
exec /usr/bin/python3 -c '
import numpy, sys, time
print(f"forkd: numpy {numpy.__version__} imported in PID 1 ({sys.executable})", flush=True)
print("forkd: parent VM ready for snapshot. children will inherit this state.", flush=True)
while True:
    time.sleep(3600)
'
