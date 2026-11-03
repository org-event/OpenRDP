# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- AVC444 by default for FreeRDP-based clients, which the server knows by the advanced input channel (`FreeRDP::Advanced::Input`) they open. Windows App for Mac does not open it and keeps AVC420, since it drops the connection on AVC444. `--avc444` forces AVC444 for every client that negotiates it, `--no-avc444` turns it off. Closes #2.
- Several RDP accounts, and `openrdp user list|set|remove|rename` to manage them without reinstalling. They live in `~/Library/Application Support/openrdp/users` (mode 600), shared by the user agent and the login-window server, and changes apply from the next connection without a restart. NLA checks each client against the account with its name: IronRDP comes from the `org-event/IronRDP` fork until upstream takes the change.

- `--avc444`: AVC444 for clients that take it, as Windows servers send: each frame goes out as two 4:2:0 views, each its own H.264 stream, and the client rebuilds full-resolution colour, so text and coloured edges no longer smear. Off by default: Windows App for Mac drops the connection on it.
- The keyboard layout switches over RDP. macOS ignores its input-source shortcuts for injected keys, so the server reads them from the user's settings (Keyboard Shortcuts, Input Sources) and selects the previous or next layout itself when the client sends one.
- The log shows the first remote key and click, and whether macOS lets the server post events.

### Changed

- `install` no longer writes the password into the agent plist. An older agent copies it into the account file on its first start.

### Fixed

- FreeRDP left black margins: it resizes the session to its window and draws the picture 1:1, and windows past 3072×1920 got a smaller display. Clients that resize the session now get the size they ask for, past H.264 level 5.2 since they decode in software, up to the 4096 pixels a side the VideoToolbox encoder takes; `--max-size` and level 5.2 still apply to clients that stretch the picture, like Windows App.
- A client wider or taller than 4096 pixels got a display the encoder refused every frame of. It is scaled down whole now.
- Scrolling from FreeRDP flew to the top or bottom of the page: its ainput channel sends wheel units in 16.16 fixed point, which the server took as whole units, 65536 times too far. A horizontal step that FreeRDP's SDL client flags as vertical too no longer scrolls up and down.
- The login window showed a black screen: a server started with `launchctl bsexec` saw no display and could not wake a sleeping one. `install` now adds `com.openrdp.loginwindow`, a LaunchAgent limited to the LoginWindow session, which launchd starts as root while the login window is up. The server exits when somebody logs in.
- A session without a virtual display looks up the main display when the client connects, instead of the one seen at startup, which may be gone.
- At the login window input events come from the HID system state, so loginwindow takes remote clicks and keys, not only pointer moves.
- Physical displays get their own mode back when the virtual display goes away. Left to itself macOS could pick another refresh rate, such as a 50 Hz TV timing on a 75 Hz monitor, which made text grainy.
- `install` retries starting the login-window service while the old one is still being removed.

## [1.3.10] - 2026-11-01

### Changed

- The graphical install explains the administrator password, Screen Recording, and Accessibility one window at a time, and starts the server only after both permissions are on.

## [1.3.9] - 2026-10-31

### Fixed

- `command.txt` on the disk image sits directly under the arrow.

## [1.3.8] - 2026-10-31

### Fixed

- The disk-image window shows the quarantine instructions again. `command.txt` under the arrow holds the same line, so it can be opened and copied.

## [1.3.7] - 2026-10-31

### Fixed

- The disk image includes `Paste in Terminal.txt` with the quarantine command, so the line can be copied. It is no longer only painted on the window.

## [1.3.6] - 2026-10-31

### Fixed

- The disk-image arrow crosses the middle of the icons, and Applications sits to the right of the instruction text.

## [1.3.5] - 2026-10-31

### Fixed

- The disk image uses one 8-point grid. OpenRDP and Applications share a row through the middle of the window, and the arrow crosses the center of both icons.

## [1.3.4] - 2026-10-31

### Fixed

- The disk-image window shows only OpenRDP and Applications, on one line. The background picture is no longer a folder in that window.

## [1.3.3] - 2026-10-31

### Added

- The disk-image window shows how to leave quarantine: drag OpenRDP to Applications, then run `xattr -dr com.apple.quarantine /Applications/OpenRDP.app`. The app does not open until that attribute is gone.

## [1.3.2] - 2026-10-31

### Added

- The release includes a disk image. Opening it shows OpenRDP and Applications; drag the app across. The first launch then asks for the RDP account and the administrator password.

## [1.3.1] - 2026-10-31

### Added

- The graphical install starts with a reminder to unlock the downloaded file: open Terminal, paste `xattr -d com.apple.quarantine openrdp`, and enter the password.

## [1.3.0] - 2026-10-31

### Added

- Opening OpenRDP with no terminal asks for the RDP name and password, then shows the system administrator prompt, so a clean install does not have to be typed in a console.

## [1.2.0] - 2026-10-31

### Added

- The login window is served by a root LaunchDaemon. `openrdp install` asks for an administrator password once and starts the server in that window's session as the installing user, so the existing Screen Recording and Accessibility grants apply. Logging in hands the port to the LaunchAgent; the client connects again.

### Security

- `libz-sys` is updated so its build script no longer writes the build environment into `target/`.
- The unused QOI+zstd codec is off, so the vulnerable `zstd-safe` decompression code is not linked.

## [1.1.0] - 2026-10-31

### Added

- The running server checks GitHub for a newer release, asks with a dialog, and installs it after confirmation. `--update auto` installs without asking.

### Changed

- The binary, app and agent are now `openrdp`, `~/Applications/OpenRDP.app` and `com.openrdp.server`. `install` deletes a previous install (agent, app, log and support directory) instead of keeping it, and macOS asks for Screen Recording and Accessibility again.

## [1.0.0] - 2026-10-31

First release.

### Changed

- The project is now called OpenRDP (github.com/org-event/OpenRDP). The binary, app bundle and agent label keep the macrdp names for now.

### Added

- RDP server on IronRDP: ScreenCaptureKit capture, VideoToolbox H.264, EGFX AVC420, NLA.
- A virtual display sized to the client, kept for a reconnect.
- Keyboard, mouse, scrolling; the cursor sent as RDP pointer updates.
- Clipboard: text and images, both ways.
- Power assertions; a connecting client wakes a sleeping display.
- `macrdp install` / `uninstall`: a LaunchAgent with a locally signed `macRDP.app`, so updates keep the macOS permissions.
- `scripts/macrdp-doctor`: checks the session, agent, port, permissions, signing, power settings and network, and can wake the display, restart the agent, bring up WireGuard or send Wake-on-LAN.
- The server logs the state of both macOS permissions on every start.
- CI on Apple Silicon runners, release builds, Dependabot and cargo-deny.
- Licenses (MIT OR Apache-2.0), English README, contributing guide, security policy.

[Unreleased]: https://github.com/org-event/OpenRDP/compare/v1.3.10...HEAD
[1.3.10]: https://github.com/org-event/OpenRDP/compare/v1.3.9...v1.3.10
[1.3.9]: https://github.com/org-event/OpenRDP/compare/v1.3.8...v1.3.9
[1.3.8]: https://github.com/org-event/OpenRDP/compare/v1.3.7...v1.3.8
[1.3.7]: https://github.com/org-event/OpenRDP/compare/v1.3.6...v1.3.7
[1.3.6]: https://github.com/org-event/OpenRDP/compare/v1.3.5...v1.3.6
[1.3.5]: https://github.com/org-event/OpenRDP/compare/v1.3.4...v1.3.5
[1.3.4]: https://github.com/org-event/OpenRDP/compare/v1.3.3...v1.3.4
[1.3.3]: https://github.com/org-event/OpenRDP/compare/v1.3.2...v1.3.3
[1.3.2]: https://github.com/org-event/OpenRDP/compare/v1.3.1...v1.3.2
[1.3.1]: https://github.com/org-event/OpenRDP/compare/v1.3.0...v1.3.1
[1.3.0]: https://github.com/org-event/OpenRDP/compare/v1.2.0...v1.3.0
[1.2.0]: https://github.com/org-event/OpenRDP/compare/v1.1.0...v1.2.0
[1.1.0]: https://github.com/org-event/OpenRDP/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/org-event/OpenRDP/releases/tag/v1.0.0
