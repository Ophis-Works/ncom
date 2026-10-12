# screenshot.tape -- VHS capture of the five still images embedded in the
# README (screenshots/{main,hex,search,line,help}.png).
#
# Run from the repo root (or via `scripts/record.sh --stills-only` once wired
# up):
#
#     vhs scripts/screenshot.tape
#
# Same session setup as demo.tape: the serial traffic comes from
# scripts/serial_feeder.py, started under `Hide` before ncom, which publishes
# its pty path to /tmp/ncom-demo-port. Each still is captured in a known state
# with VHS's Screenshot command; afterwards the toggles are reverted so the
# next still starts from a clean view.

Output screenshots/main.png
Output screenshots/hex.png
Output screenshots/search.png
Output screenshots/line.png
Output screenshots/help.png

Set Shell "bash"
Set FontFamily "Hack"
# 22 is the VHS default font size (set explicitly for clarity).
Set FontSize 22
Set Width 1200
Set Height 720
Set Padding 0
# ~130 ms per typed character: slow enough to read.
Set TypingSpeed 130ms

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

# main.png: plain text view with traffic flowing, follow-tail on.
Sleep 2.5s
Screenshot screenshots/main.png

# hex.png: hexdump view with the three column colours (offset/byte/ASCII).
Type ":hex on" Enter
Sleep 1.5s
Screenshot screenshots/hex.png
Type ":hex off" Enter
Sleep 500ms

# search.png: whole-line :hl accent plus / search match spans, paused on the
# matched line (search pauses follow-tail by design).
Type ":hl ERROR" Enter
Sleep 500ms
Type "/flash" Enter
Sleep 1.5s
Screenshot screenshots/search.png
Type ":noh" Enter
Sleep 300ms
Type ":hl off" Enter
Sleep 300ms
Type "G"
Sleep 500ms

# line.png: line-mode input bar with typed text and the reverse-block cursor.
Type ":line on" Enter
Sleep 500ms
Type "i"
Sleep 300ms
Type "set baud 921600"
Sleep 800ms
Screenshot screenshots/line.png
Escape
Sleep 300ms
Type ":line off" Enter
Sleep 500ms

# help.png: the :help popup (page 1) over dimmed scrollback.
Type ":help" Enter
Sleep 1s
Screenshot screenshots/help.png
Escape
Sleep 500ms

# The session ends here. As in demo.tape, ncom keeps running in the VHS
# session, so the feeder cannot be cleaned up from inside the tape; the
# caller removes the leftovers (record.sh does this after vhs exits).
