cask "mac-action-agent" do
  version "0.1.0"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"

  url "https://github.com/centralhardware/macActionAgent/releases/download/v#{version}/mac-action-agent-#{version}.dmg"
  name "mac-action-agent"
  desc "Runs shell commands requested over the private network and streams the output back"
  homepage "https://github.com/centralhardware/macActionAgent"

  pkg "Install mac-action-agent.pkg"

  # Mirrors packaging/uninstall.command.
  uninstall launchctl: "me.centralhardware.mac-action-agent",
            pkgutil:   "me.centralhardware.mac-action-agent",
            delete:    "/Users/Shared/mac-action-agent.log"

  zap trash: "~/Library/Preferences/me.centralhardware.mac-action-agent.plist"
end
