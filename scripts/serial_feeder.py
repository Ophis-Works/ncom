#!/usr/bin/env python3
"""serial_feeder.py -- deterministic serial device for the VHS demo tape.

Creates its own pty (exactly like fake-serial.py) and writes the same
deterministic 5 lines/s traffic the approved tour uses, so the VHS tape can
start the feeder first, read the pty path from a file, then launch ncom
against it. Values derive only from the line index, so every run is
byte-identical.

    uv run serial_feeder.py --port-file /tmp/ncom-demo-port

The slave device path is printed to stdout AND written to --port-file for
the tape to consume:

    ncom --port $(cat /tmp/ncom-demo-port)

After a deterministic boot header (written once) one line is written every
200 ms (5 lines/s) on a fixed 10-line rotation; [GPS] and [ERROR] lines
(and the temp= readings the /temp search matches) therefore occur
regularly. The feeder exits when the port is closed for good (EIO on the
master, i.e. ncom has exited) or on Ctrl-C.

Stdlib only. Run from the repo root through uv, which manages the
venv/lock (scripts/pyproject.toml, scripts/uv.lock).
"""

import argparse
import os
import pty
import sys
import time

# Deterministic live traffic: identical to the approved tour rotation.
TRAFFIC_ROTATION = [
    "[INFO] telemetry batch {n} committed (14 records)",
    "[GPS] fix=3D lat=41.15{lat:05d} lon=-8.62{lon:05d} sats={sats} hdop=0.{hdop}",
    "[SENSOR] temp={temp}C hum={hum}% pres=1013hPa",
    "[ERROR] radio sync timeout after 3 attempts, backoff {n}0s",
    "[MSG] sensor node {node} reported packet seq={n} rssi=-{rssi}dBm queue=ok",
    "[GPS] fix=3D lat=41.15{lat:05d} lon=-8.62{lon:05d} sats={sats} hdop=0.{hdop}",
    "[HEARTBEAT] uptime={n}00s vbat=3.8{mv}V heap=13{k}kB",
    "[SENSOR] temp={temp}C hum={hum}% pres=1013hPa",
    "[NET] rssi=-{rssi}dBm snr=9dB gw=gw{gw:02d}",
    "[ERROR] adc drift beyond tolerance: ch3 offset=+0.{off}V",
]
LINE_INTERVAL = 0.2  # one line every 200 ms -> 5 lines/s

BOOT_HEADER = [
    "[BOOT] fw=v2.7.3 hw=rev2 reset=power",
    "[INFO] boot complete in 412 ms, clock=160MHz",
    "[INFO] config loaded from flash slot 1 (crc=0xa5f3)",
]


def format_line(n):
    """Format rotation line `n` (1-based); values derive from the index."""
    return TRAFFIC_ROTATION[n % len(TRAFFIC_ROTATION)].format(
        n=n, lat=70000 + (n * 7) % 9999, lon=90000 + (n * 13) % 9999,
        sats=7 + n % 5, hdop=5 + n % 5, temp=22.5 + (n % 30) * 0.1,
        hum=35 + n % 15, node=10 + n % 40, rssi=65 + n % 25,
        mv=n % 10, k=n % 9, gw=n % 40, off=30 + n % 20)


def main(argv=None):
    parser = argparse.ArgumentParser(
        prog="serial-feeder",
        description="Deterministic 5 lines/s fake serial device for the "
                    "VHS demo tape (no socat needed).")
    parser.add_argument("--port-file", required=True, metavar="PATH",
                        help="write the slave pty path to this file")
    parser.add_argument("--interval", type=float, default=LINE_INTERVAL,
                        metavar="SEC", help="seconds between lines")
    args = parser.parse_args(argv)

    master_fd, slave_fd = pty.openpty()
    slave_path = os.ttyname(slave_fd)
    with open(args.port_file, "w") as f:
        f.write(slave_path + "\n")
    print(f"[serial-feeder] slave device: {slave_path} "
          f"(written to {args.port_file})", file=sys.stderr, flush=True)

    try:
        data = ("\n".join(BOOT_HEADER) + "\n").encode()
        os.write(master_fd, data)
        n = 0
        next_line_t = time.monotonic() + args.interval
        while True:
            now = time.monotonic()
            if next_line_t > now:
                time.sleep(min(0.05, next_line_t - now))
                continue
            n += 1
            os.write(master_fd, (format_line(n) + "\n").encode())
            next_line_t += args.interval
    except OSError as exc:
        # EIO on the master means every slave fd is closed: ncom has exited
        # (or the recording is over), so shut down cleanly.
        print(f"[serial-feeder] port closed ({exc}); exiting",
              file=sys.stderr, flush=True)
        return 0
    except KeyboardInterrupt:
        print("[serial-feeder] interrupted, cleaning up",
              file=sys.stderr, flush=True)
        return 0
    finally:
        try:
            os.remove(args.port_file)
        except OSError:
            pass
        os.close(master_fd)
        os.close(slave_fd)


if __name__ == "__main__":
    sys.exit(main())
