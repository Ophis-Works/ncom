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

Sleep 2s

# 1. initial display: traffic streaming in, plain text view (already above)
# 2. timestamps on
Type ":ts on" Enter
Sleep 1.5s

# 3. whole-line highlight for ERROR
Type ":hl ERROR" Enter
Sleep 1.5s

# 4. search with match spans
Type "/temp" Enter
Sleep 1.5s

# 5. clear the search
Type ":noh" Enter
Sleep 500ms

# 6. disable highlight
Type ":hl off" Enter
Sleep 500ms

# 7. filter: only GPS lines, live traffic keeps arriving
Type ":filter GPS" Enter
Sleep 1.5s

# 8. disable the filter BEFORE the navigation showcase
Type ":filter off" Enter
Sleep 500ms

# 9. pause follow, scroll up into the history and back down
Type "p"
Sleep 500ms
Type "kkkkkkkkkkkkkkkkkkkk"
Sleep 1.2s
Type "jjjjjjjjjjjjjjjjjjjj"
Sleep 500ms

# 10. back to the tail and resume follow (traffic proves it is live)
Type "G" Enter
Sleep 1.5s

# 11. hex view
Type ":hex on" Enter
Sleep 1.5s

# 12. back to text
Type ":hex off" Enter
Sleep 500ms

# 13. echo on BEFORE the TX showcase, so sent bytes appear as > lines
Type ":echo on" Enter
Sleep 1.2s

# 14. raw insert: send 8 characters to the port (echoed as a > line)
Type "i"
Sleep 300ms
Type "ncom1234"
Sleep 800ms
Escape
Sleep 500ms

# 15. line mode on
Type ":line on" Enter
Sleep 600ms

# 16. send one line (echoed as a > line with the whole sentence)
Type "i"
Sleep 300ms
Type "Hello, we can send characters 1 line at a time too" Enter
Sleep 1s
Escape
Sleep 400ms

# 17. line mode off
Type ":line off" Enter
Sleep 600ms

# 18. help popup, page 1
Type "?"
Sleep 1.5s

# 19. help page 2, then close and hold on the live tail
Type "j"
Sleep 1.5s
Escape
Sleep 1.5s

# The recording ends here, holding on the live tail. ncom keeps running in
# the VHS session, so the feeder cannot be cleaned up from inside the tape:
# closing the session kills the feeder process group (the feeder's EIO path
# cannot run), and any cleanup keystrokes would go to ncom, not the shell.
# record.sh removes the leftovers after vhs exits; the tape also rm -f's
# the port file at startup.
