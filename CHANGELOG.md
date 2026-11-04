# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [1.6.1] - 2026-11-04

### Added

- The install readies the Mac for being reached only over the network, in the administrator step it already had: start up after a power failure, wake for network access, Remote Login (SSH) and Screen Sharing on, and a firewall rule that lets OpenRDP accept connections instead of asking at the Mac on the first one. `uninstall` removes the firewall rule.
- The graphical install asks for the Desktop, Documents and Downloads after Accessibility, so copying files through the clipboard does not bring up a macOS prompt in the middle of a remote session.

## [1.6.0] - 2026-11-04

### Added

- OpenRDP Client shows which saved Macs are up: a green dot when RDP answers, yellow when only SSH does (FileVault waiting after a power cut, or a Mac still starting), red when nothing does. Checked every few seconds.
- OpenRDP Client unlocks FileVault after a power cut: "Unlock" signs in over SSH with the Mac's own account, kept in the Keychain, which macOS 26 and later take as the disk password. "Screen Sharing" opens `vnc://` with the same account when the login window does not come up over RDP, and "Wake" sends a Wake-on-LAN packet to a Mac that does not answer, to the hardware address learnt from the ARP table or typed in.

## [1.5.2] - 2026-11-04

### Fixed

- Client drives: the first `cp` of a file could come out padded with zeros or with a later MiB repeating an earlier one. When webdavfs dropped a GET while a read was waiting for the client, that read's reply answered the next request, and every request after it got the data of the one before. Each request to the client now runs to its own reply. Closes #22.

## [1.5.1] - 2026-11-04

### Changed

- New icons: a purple O for the server and a blue one for OpenRDP Client, so the two apps are told apart in Finder, the Dock and Privacy & Security.
- The client's disk image is laid out like the server's: the same window and background, with the client's own name and quarantine command.
- CodeQL scans the Rust code and the workflows on every push and pull request, and weekly.

## [1.5.0] - 2026-11-03

### Added

- Client drives: folders the client redirects (FreeRDP `/drive:name,path`) are mounted at `~/RDP Drives/<name>` while it is connected, through a WebDAV server on 127.0.0.1 and `mount_webdav`. Read, write, list, rename, delete and new folders go to the client over RDPDR. `--no-drives` turns it off. The IronRDP fork now sends the Server User Logged On PDU, without which clients never announce their drives.
- Files through the clipboard, both ways. Files copied in Finder are offered to the client and read from disk as it pastes. Files copied on the client are downloaded into `~/Library/Caches/OpenRDP/Clipboard` and put on the pasteboard for Finder. Copies over 16 GB are refused, and names that would leave the download folder are dropped.
- Curtain: the Mac's own displays go dark while a client is connected, when the client asks for it with `curtain` as its alternate shell (FreeRDP `/shell:curtain`). The server sets their gamma to black, so the capture is unaffected. Brightness returns when the client leaves or the server stops, crashes included. `--curtain always|never` overrides the client. IronRDP (fork) now passes the client's alternate shell and working directory to the server.
- Sound: what the Mac plays goes to the client over `rdpsnd`, as 16-bit PCM at 48 or 44.1 kHz, captured by ScreenCaptureKit without the server's own sound. `--no-sound` turns it off. Closes #4.
- OpenRDP Client, a Mac app in `client/` that connects to OpenRDP servers with FreeRDP inside it, for Apple Silicon and Intel Macs:
  - saved Macs, with passwords in the Keychain, passed to FreeRDP on stdin so they never show in `ps`;
  - one-click AVC444 sessions;
  - FreeRDP's own shortcuts on right ⌘ + ⌥ instead of right Shift, which broke capital П, К, В and Ь;
  - ⌘Space, ⌘Tab and ⌘Q go to the remote Mac, so the layout switches there.
- Releases carry the client as `openrdp-client-<version>-universal.dmg` next to the server. CI builds it on every pull request and checks that every binary in it is universal and loads nothing from outside the app or macOS.
- OpenRDP Client: each saved Mac has a "Darken the Mac's screen during the session" checkbox, off by default. When it is on, the client asks for the curtain with `/shell:curtain`.

## [1.4.0] - 2026-11-03

### Added

- The bitrate follows the link. When frames are acknowledged well later than the best time seen on the connection, the server cuts it by a quarter, down to a quarter of `--bitrate`, and raises it back slowly while acknowledgements stay quick. Closes #3.
- AVC444 by default for FreeRDP-based clients, which the server knows by the advanced input channel (`FreeRDP::Advanced::Input`) they open. Windows App for Mac does not open it and keeps AVC420, since it drops the connection on AVC444. `--avc444` forces AVC444 for every client that negotiates it, `--no-avc444` turns it off. Closes #2.
- Several RDP accounts, and `openrdp user list|set|remove|rename` to manage them without reinstalling. They live in `~/Library/Application Support/openrdp/users` (mode 600), shared by the user agent and the login-window server, and changes apply from the next connection without a restart. NLA checks each client against the account with its name: IronRDP comes from the `org-event/IronRDP` fork until upstream takes the change.

- `--avc444`: AVC444 for clients that take it, as Windows servers send: each frame goes out as two 4:2:0 views, each its own H.264 stream, and the client rebuilds full-resolution colour, so text and coloured edges no longer smear. Off by default: Windows App for Mac drops the connection on it.
- The keyboard layout switches over RDP. macOS ignores its input-source shortcuts for injected keys, so the server reads them from the user's settings (Keyboard Shortcuts, Input Sources) and selects the previous or next layout itself when the client sends one.
- The log shows the first remote key and click, and whether macOS lets the server post events.

### Changed

- IronRDP moves to current upstream (59 commits newer), still through the `org-event/IronRDP` fork, which also drops `rustls-pemfile`. The RUSTSEC-2025-0134 ignore goes; RUSTSEC-2023-0071 (`rsa`) stays, as no `rsa` release fixes it. Closes #5.

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

[Unreleased]: https://github.com/org-event/OpenRDP/compare/v1.6.1...HEAD
[1.6.1]: https://github.com/org-event/OpenRDP/compare/v1.6.0...v1.6.1
[1.6.0]: https://github.com/org-event/OpenRDP/compare/v1.5.2...v1.6.0
[1.5.2]: https://github.com/org-event/OpenRDP/compare/v1.5.1...v1.5.2
[1.5.1]: https://github.com/org-event/OpenRDP/compare/v1.5.0...v1.5.1
[1.5.0]: https://github.com/org-event/OpenRDP/compare/v1.4.0...v1.5.0
[1.4.0]: https://github.com/org-event/OpenRDP/compare/v1.3.10...v1.4.0
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
