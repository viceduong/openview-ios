OpenView {VERSION} for Windows
==============================

Self-hosted, open-source remote desktop for Windows with an iOS client.

  OpenView.exe   the app: share this PC (your ID + PIN) or control another one
  ovhost.exe     host (started by OpenView.exe; can also run on its own)
  ovview.exe     viewer window (started by OpenView.exe when you connect)
  ovrelay.exe    relay server (OpenView.exe runs one on TCP 4569 by default)
  firewall.cmd   adds/removes the Windows Firewall rules (run as administrator)

Getting started
---------------
1. Run OpenView.exe. It shows YOUR ID and a PIN for this PC.
2. On the other device (a second PC with OpenView, or the iOS app), enter
   that ID and PIN. Both sides need to reach the same relay: on a LAN that
   can be the relay this PC runs (its IP address, port 4569).
3. Settings > Relay sets the relay used for outgoing connections.

Where data lives
----------------
- Installed: %LOCALAPPDATA%\OpenView (settings, saved partners, logs,
  device ID). PINs are kept in Windows Credential Manager.
- Portable (portable.txt next to OpenView.exe): the "data" folder here.
- Files received from a partner: Downloads\OpenView.

The binaries are not code-signed yet, so Windows SmartScreen may warn on
first start ("More info" > "Run anyway"). Verify downloads with SHA256SUMS.

Command-line help: ovhost.exe --help, ovview.exe --help, ovrelay.exe --help

Documentation, source code and issues: https://github.com/OWNER/openview
License: MIT (LICENSE). Third-party notices: THIRD_PARTY_LICENSES.txt.
