# Terminal demo for the README, rendered with vhs (https://github.com/charmbracelet/vhs):
#
#     vhs docs/demo.tape          # -> docs/demo.gif
#
# Run it on a Linux host with coffer installed (FUSE is needed for the
# mount, so this cannot be rendered on macOS). Everything the demo shows
# is real: the container is created, mounted, written to and unmounted.
# The hidden block at the top just makes sure the demo starts from a clean
# state and has some files to copy; the one at the bottom cleans up again.

Output docs/demo.gif

Set Shell bash
Set FontSize 16
Set Width 1300
Set Height 500
Set Padding 18
Set Theme "Catppuccin Mocha"
Set TypingSpeed 35ms
# English tool output regardless of the rendering host's locale.
Env LC_ALL "C.UTF-8"

# --- setup, not shown -------------------------------------------------------
Hide
Type "coffer umount ~/vault 2>/dev/null; coffer remove work 2>/dev/null; rm -f ~/.coffer/work.coffer*; rm -rf ~/vault ~/Documents/tax-2025" Enter
Type "mkdir -p ~/Documents/tax-2025 && for f in invoice-01.pdf invoice-02.pdf notes.md; do head -c 40000 /dev/urandom > ~/Documents/tax-2025/$f; done; clear" Enter
Sleep 500ms
Show

# --- the demo ---------------------------------------------------------------
Type "coffer create ~/.coffer/work.coffer --save work --mountpoint ~/vault" Sleep 300ms Enter
Sleep 800ms
Type "correct horse battery staple" Enter
Sleep 600ms
Type "correct horse battery staple" Enter
Sleep 1.5s

Type "coffer mount" Sleep 300ms Enter
Sleep 800ms
Type "correct horse battery staple" Enter
Sleep 1.5s

Type "cp -r ~/Documents/tax-2025 ~/vault/ && ls -la ~/vault/tax-2025" Sleep 300ms Enter
Sleep 2s

Type "coffer list" Sleep 300ms Enter
Sleep 2s

Type "coffer umount" Sleep 300ms Enter
Sleep 1.5s

Type "ls -la ~/.coffer/work.coffer" Sleep 300ms Enter
Sleep 3s

# --- cleanup, not shown -----------------------------------------------------
Hide
Type "coffer remove work; rm -f ~/.coffer/work.coffer*; rm -rf ~/vault ~/Documents/tax-2025" Enter
