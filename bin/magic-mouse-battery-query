#!/usr/bin/python3
"""Ask a Magic Mouse for its battery level right now (needs /dev/hidrawN, which the udev rule grants to the logged-in user).

The mouse normally pushes battery input report 0x90 on its own schedule, so the
kernel and UPower show 0% / unknown after a reconnect until it does. This sends
a HID GET_REPORT for that input report; if the mouse answers, the kernel also
sees the answer and UPower updates.
"""
import fcntl, glob, os, struct, sys

def hidraw_for_magic_mouse():
    for p in sorted(glob.glob('/sys/class/hidraw/hidraw*')):
        ue = open(f'{p}/device/uevent').read()
        if any(i in ue for i in ('HID_ID=0005:0000004C:00000269', 'HID_ID=0005:0000004C:00000323', 'HID_ID=0005:000005AC:0000030D')):
            return '/dev/' + os.path.basename(p)
    return None

def IOC(dir_, typ, nr, size):
    return (dir_ << 30) | (size << 16) | (ord(typ) << 8) | nr

dev = hidraw_for_magic_mouse()
if not dev:
    sys.exit('no Magic Mouse hidraw node found')
buf = bytearray(4)
buf[0] = 0x90
HIDIOCGINPUT = IOC(3, 'H', 0x0A, len(buf))  # _IOWR('H', 0x0A, len)
with open(dev, 'rb+', buffering=0) as f:
    try:
        fcntl.ioctl(f, HIDIOCGINPUT, buf)
    except OSError as exc:
        sys.exit(f'{dev}: GET_REPORT(0x90) refused: {exc}')
print(f'{dev}: report 0x90 = {buf.hex()}  -> flags={buf[1]:#04x} battery={buf[2]}%')
