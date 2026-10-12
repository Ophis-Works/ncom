# demo.tape -- VHS recording of the ncom feature tour.
#
# Run from the repo root (record.sh does):
#
#     vhs scripts/demo.tape
#
# The serial traffic comes from scripts/serial_feeder.py, which creates its
# own pty and publishes the device path to /tmp/ncom-demo-port; it is
# started under `Hide` before ncom, then ncom is launched against the
# published port. The tour mirrors the approved scene list.

# The GIF output uses the native render
# resolution (VHS never rescales), so both outputs share this size.
Output screenshots/demo.gif
Output screenshots/demo.webm

Set Shell "bash"
Set FontFamily "Hack"
# 22 is the VHS default font size (set explicitly for clarity).
Set FontSize 22
Set Width 1200
Set Height 720
Set Padding 0
# ~130 ms per typed character: slow enough to read.
Set TypingSpeed 130ms

# Set framerate to 6 fps
Set Framerate 6

# Start the deterministic serial feeder in the background and wait for the
# port file to appear (all hidden from the recording).
Hide
Require uv
Type@1ms "rm -f /tmp/ncom-demo-port && uv run scripts/serial_feeder.py --port-file /tmp/ncom-demo-port >/tmp/ncom-feeder.log 2>&1 &" Enter
Type@1ms "i=0; while [ ! -f /tmp/ncom-demo-port ] && [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done" Enter

# Launch ncom against the feeder's pty.
Type "clear" Enter
Sleep 500ms
Type "./target/release/ncom --port $(cat /tmp/ncom-demo-port)" Enter
Show

Sleep 1.5s

# 1. serial view with traffic streaming in, plain text view (already above)

# 2. whole-line highlight for ERROR
Type@50ms ":hl ERROR" Enter
Sleep 1.5s

# 3. search with match spans
Type@50ms "/temp" Enter
Sleep 1.5s

# 4. clear the search
Type@50ms ":noh" Enter
Sleep 300ms

# 5. disable highlight
Type@50ms ":hl off" Enter
Sleep 300ms

# 6. filter: only GPS lines, live traffic keeps arriving
Type@50ms ":filter GPS" Enter
Sleep 1.5s

# 7. search again, inside the filtered view
Type@50ms "/temp" Enter
Sleep 1.5s

# 8. clear the search, then the filter
Type@50ms ":noh" Enter
Sleep 300ms
Type@50ms ":filter off" Enter

# The recording ends here, holding on the live tail. ncom keeps running in
# the VHS session, so the feeder cannot be cleaned up from inside the tape:
# closing the session kills the feeder process group (the feeder's EIO path
# cannot run), and any cleanup keystrokes would go to ncom, not the shell.
# record.sh removes the leftovers after vhs exits; the tape also rm -f's
# the port file at startup.
Sleep 1s
