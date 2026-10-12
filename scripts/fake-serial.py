#!/usr/bin/env python3
"""fake-serial.py -- feed synthetic but realistic serial traffic to ncom.

Python port of fake-serial.sh. Unlike the bash version it does NOT need
socat: it creates its own pty with pty.openpty() and prints the slave
device path (e.g. /dev/pts/5) so ncom can open it directly:

    uv run fake-serial.py --loop
    # in another terminal:
    ncom --port /dev/pts/5

Options:
    --loop [INTERVAL]  write a new batch every INTERVAL seconds (default 1,
                       fractional allowed). Keeps the pty alive; Ctrl-C exits.
    --lines N          batch size (default 12; some batches vary, see below)
    --quiet            suppress the per-batch stderr chatter
    --hold             create the pty and keep it open but write nothing
                       (for manual testing); Ctrl-C exits

Default (no --loop): write one batch and exit. NOTE: when this process
exits its pty dies and ncom will see the port disconnect -- use --loop
(or --hold) to keep the port alive. Ctrl-C cleans up.

Stdlib only (pty, os, time, argparse, random). Run through uv, which
manages the venv/lock (scripts/pyproject.toml, scripts/uv.lock):

    uv run fake-serial.py            # from the scripts/ directory
"""

import argparse
import os
import pty
import random
import sys
import time

BOOT_REASONS = ["power", "watchdog", "brownout", "button"]
GPS_FIX = ["3D", "2D", "none"]
QUEUE = ["ok", "draining", "full"]
TASKS = ["radio_sync", "log_upload", "selftest", "calibrate"]
PRIO = ["low", "normal", "high"]

T0 = time.time()


def pick(seq):
    return random.choice(seq)


def write_batch(master_fd, quiet, n_lines=12):
    # One "batch" = a boot-ish header plus one round of telemetry. Values
    # are randomized so every run (and every loop iteration) differs.
    # Main lines are kept under 100 chars except the one deliberate
    # ~200-char dump line used to exercise terminal wrapping.
    temp = random.randint(10, 49)            # 10-49 deg C
    hum = random.randint(20, 79)             # 20-79 %RH
    lat = f"{random.randrange(90):02d}.{random.randrange(100000):05d}"
    lon = f"{random.randrange(180):03d}.{random.randrange(100000):05d}"
    sats = random.randint(4, 11)             # 4-11 satellites
    rssi = -(random.randint(40, 99))         # -40..-99 dBm
    upt = int(time.time() - T0) + random.randrange(3600)
    vbat = f"3.{random.randint(10, 99):02d}"  # 3.10-3.99 V
    heap = random.randint(20, 219)           # 20-219 kB free

    lines = [
        f"[BOOT] fw=v{random.randrange(3)}.{random.randrange(10)}.{random.randrange(20)}"
        f" hw=rev{random.randrange(1, 5)} reset={pick(BOOT_REASONS)}",
        f"[INFO] boot complete in {random.randint(120, 919)} ms, clock={random.randrange(100):02d}MHz",
        f"[INFO] config loaded from flash slot {random.randrange(1, 3)} (crc=0x{random.randrange(65536):04x})",
        f"[WARN] brownout counter high: {random.randint(1, 5)} events in last hour",
        f"[ERROR] flash write failed at sector {random.randrange(512)} (retry {random.randint(1, 3)}/3)",
        f"[SENSOR] temp={temp}.{random.randrange(10)}C hum={hum}% pres={random.randint(980, 1019)}hPa",
        f"[GPS] fix={pick(GPS_FIX)} lat={lat} lon={lon} sats={sats} hdop=0.{random.randint(1, 9)}",
        f"[NET] rssi={rssi}dBm snr={random.randrange(20)}dB gw=gw{random.randrange(20):02d}",
        f"[HEARTBEAT] uptime={upt}s vbat={vbat}V heap={heap}kB",
        f"[MSG] sensor node {random.randint(1, 40)} reported packet seq={random.randrange(32768)}"
        f" rssi={rssi} queue={pick(QUEUE)}",
        f"[MSG] schedule: task='{pick(TASKS)}' due in {random.randrange(300)}s prio={pick(PRIO)}",
    ]
    # Occasionally (1 in 4 batches) add one long ~200-char line for wrap testing.
    if random.randrange(4) == 0 and n_lines >= 12:
        hexes = [f"{random.randrange(1 << 32):08x}" for _ in range(9)]
        lines.append(
            "[DUMP] regdump: uart=0x{} dma=0x{} rtc=0x{} gpio=0x{} tim=0x{} i2c=0x{} spi=0x{} adc=0x{} crc=0x{} end".format(*hexes)
        )

    # Respect --lines: trim (keeping the boot header) or top up with extra
    # randomized telemetry lines.
    del lines[n_lines:]
    i = 0
    while len(lines) < n_lines:
        lines.append(
            f"[MSG] sensor node {random.randint(1, 40)} reported packet seq={random.randrange(32768)}"
            f" rssi={-(random.randint(40, 99))}dBm queue={pick(QUEUE)} batch_no={i}"
        )
        i += 1
    data = ("\n".join(lines) + "\n").encode()
    # os.write on the master blocks if the slave-side buffer is full; that is
    # fine (a reader drains it). PTYs are kernel-buffered, so a batch fits.
    os.write(master_fd, data)
    if not quiet:
        print(f"[fake-serial] wrote {len(lines)} lines ({len(data)} bytes)", file=sys.stderr)


def main(argv=None):
    parser = argparse.ArgumentParser(
        prog="fake-serial",
        description="Fake serial device for testing ncom (no socat needed).",
    )
    parser.add_argument(
        "--loop",
        nargs="?",
        const="1",
        default=None,
        metavar="INTERVAL",
        help="write a new batch every INTERVAL seconds (default 1, fractional ok)",
    )
    parser.add_argument("--lines", type=int, default=12, metavar="N",
                        help="lines per batch (default 12)")
    parser.add_argument("--quiet", action="store_true",
                        help="suppress per-batch stderr chatter")
    parser.add_argument("--hold", action="store_true",
                        help="create the pty, keep it open, write nothing")
    args = parser.parse_args(argv)

    if args.loop is not None:
        try:
            interval = float(args.loop)
        except ValueError:
            parser.error(f"interval must be a number, got {args.loop!r}")
        if interval <= 0:
            parser.error(f"interval must be positive, got {args.loop!r}")
    else:
        interval = 1.0

    if args.hold and args.loop is not None:
        parser.error("--hold and --loop are mutually exclusive")

    # Create our own pty: master_fd is what we write to, the slave path is
    # what ncom opens as its serial port.
    master_fd, slave_fd = pty.openpty()
    slave_path = os.ttyname(slave_fd)
    print("=" * 60, flush=True)
    print("fake-serial: open this port in ncom:")
    print(f"    ncom --port {slave_path}", flush=True)
    print(f"[fake-serial] slave device: {slave_path}", file=sys.stderr)
    if args.hold:
        print("[fake-serial] --hold: writing nothing; Ctrl-C to exit", file=sys.stderr)

    try:
        if args.hold:
            while True:
                time.sleep(3600)
        elif args.loop is not None:
            while True:
                write_batch(master_fd, args.quiet, args.lines)
                time.sleep(interval)
        else:
            write_batch(master_fd, args.quiet, args.lines)
            print("[fake-serial] batch written; exiting (pty closes, port dies)."
                  " Use --loop to keep the port alive.", file=sys.stderr)
    except KeyboardInterrupt:
        print("[fake-serial] interrupted, cleaning up", file=sys.stderr)
    finally:
        os.close(master_fd)
        os.close(slave_fd)
    return 0


if __name__ == "__main__":
    sys.exit(main())
