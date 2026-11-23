#!/bin/sh
# Removes mac-action-agent. Double-click, or run with sudo.
[ "$(id -u)" -eq 0 ] || exec sudo "$0" "$@"
CONSOLE_USER=$(/usr/bin/stat -f %Su /dev/console)
/bin/launchctl bootout "gui/$(/usr/bin/id -u "$CONSOLE_USER")/me.centralhardware.mac-action-agent" 2>/dev/null
rm -f /Library/LaunchAgents/me.centralhardware.mac-action-agent.plist \
      /usr/local/libexec/mac-action-agent \
      /Users/Shared/mac-action-agent.log
/usr/sbin/pkgutil --forget me.centralhardware.mac-action-agent 2>/dev/null
echo "mac-action-agent removed."
