# screenshot.tape -- VHS capture of the eight still images embedded in the
# README (screenshots/{main,hex,search,line,help,hl,filter,help2}.png).
#
# Run from the repo root (record.sh does):
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
Output screenshots/hl.png
Output screenshots/filter.png
Output screenshots/help2.png

Set Shell "bash"
Set FontFamily "Hack"
# 22 is the VHS default font size (set explicitly for clarity).
Set FontSize 22
Set Width 1200
Set Height 720
Set Padding 0
# ~10 ms per typed character: slow enough to read.
Set TypingSpeed 10ms

# Start the deterministic serial feeder in the background and wait for the
# port file to appear (all hidden from the recording).
Hide
Require uv
Type@1ms "rm -f /tmp/ncom-demo-port && uv run scripts/serial_feeder.py --port-file /tmp/ncom-demo-port >/tmp/ncom-feeder.log 2>&1 &" Enter
Type@1ms "i=0; while [ ! -f /tmp/ncom-demo-port ] && [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done" Enter

# Launch ncom against the feeder's pty.
Type "clear" Enter
Sleep 50ms
Type "./target/release/ncom --port $(cat /tmp/ncom-demo-port)" Enter
Show

# main.png: plain text view with traffic flowing, follow-tail on.
Sleep 3s
Screenshot screenshots/main.png

# hl.png: whole-line highlight on ERROR-bearing traffic.
Type ":hl ERROR" Enter
Sleep 100ms
Screenshot screenshots/hl.png
Type ":hl off" Enter

# hex.png: hexdump view with the three column colours (offset/byte/ASCII).
Type ":hex on" Enter
Sleep 100ms
Screenshot screenshots/hex.png
Type ":hex off" Enter

# search.png: whole-line :hl accent plus / search match spans, paused on the
# matched line (search pauses follow-tail by design).
Type ":hl ERROR" Enter
Sleep 100ms
Type "/sensor" Enter
Sleep 200ms
Screenshot screenshots/search.png
Type ":noh" Enter
Type ":hl off" Enter
Type "G"

# filter.png: filtered view -- only GPS lines, snapped to the filtered tail
# while live traffic keeps arriving.
Type ":filter GPS" Enter
Sleep 100ms
Screenshot screenshots/filter.png
Type ":filter off" Enter
Type "G"


# line.png: line-mode input bar with typed text and the reverse-block cursor.
Type ":line on" Enter
Type "i"
Type "Example line for TX"
Sleep 100ms
Screenshot screenshots/line.png
Escape
Type ":line off" Enter

# help.png: the :help popup (page 1) over dimmed scrollback.
Type ":help" Enter
Sleep 100ms
Screenshot screenshots/help.png

# help2.png: the help popup's second page.
Type "j"
Sleep 100ms
Screenshot screenshots/help2.png
Escape

# The session ends here. As in demo.tape, ncom keeps running in the VHS
# session, so the feeder cannot be cleaned up from inside the tape; the
# caller removes the leftovers (record.sh does this after vhs exits).
