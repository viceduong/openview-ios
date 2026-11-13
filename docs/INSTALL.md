# Installing OpenView on Windows

OpenView runs on **Windows 10 and Windows 11, x64**. The binaries link the C
runtime statically, so no Visual C++ redistributable or other runtime is
needed. Two packages are published on the
[Releases](https://github.com/OWNER/openview/releases) page:

| Package | Use it when |
|---|---|
| `OpenView-<version>-setup.exe` | You want a normal installation with a Start menu entry, upgrades and an uninstaller. No administrator rights needed. |
| `OpenView-<version>-windows-x64-portable.zip` | You want to run OpenView from a folder or USB stick without installing anything. |

Both contain the same four programs: `OpenView.exe` (the app), `ovhost.exe`
(host), `ovview.exe` (viewer window) and `ovrelay.exe` (relay). They must
stay together in one folder.

## Verify the download

The binaries are not code-signed yet. Compare the SHA-256 hash with the
`SHA256SUMS` file from the same release:

```powershell
Get-FileHash .\OpenView-0.1.0-setup.exe -Algorithm SHA256
```

On first start, Windows SmartScreen may say "Windows protected your PC".
Choose **More info > Run anyway**.

## Installer

Run `OpenView-<version>-setup.exe`. It installs for the current user only:

- Program files: `%LOCALAPPDATA%\Programs\OpenView`
- Start menu shortcut (and, optionally, a desktop shortcut)
- Registry: only under `HKEY_CURRENT_USER`

Optional tasks on the *Select Additional Tasks* page:

| Task | Default | What it does |
|---|---|---|
| Start OpenView when I sign in | on | Writes the `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` value `OpenView` = `"<install dir>\OpenView.exe" --minimized`. This is the same value as the **Settings > Start OpenView at sign-in** switch in the app, so either place can turn it off. |
| Allow OpenView through Windows Firewall on private networks | off | Asks for administrator approval (a separate UAC prompt) and adds two inbound rules, see [Firewall](#firewall). Declining the prompt does not fail the installation. |

Silent installation: `OpenView-<version>-setup.exe /VERYSILENT /SUPPRESSMSGBOXES`
(add `/TASKS="autostart"` to choose tasks explicitly, or `/TASKS=""` for
none). See the [Inno Setup command-line reference](https://jrsoftware.org/ishelp/index.php?topic=setupcmdline).

### Upgrading

Run the newer installer. It asks a running OpenView to quit
(`OpenView.exe --quit`, which also stops its host and relay), replaces the
files and keeps your settings, device ID, saved partners and PINs. The
installation folder and task choices of the previous installation are
reused.

### Uninstalling

Use **Settings > Apps > Installed apps > OpenView > Uninstall** (or
*Add or remove programs*). The uninstaller:

1. asks a running OpenView to quit;
2. removes the sign-in `Run` value if it points at this installation;
3. removes the firewall rules if the installer added them (one UAC prompt);
4. removes the program files;
5. asks whether to delete your data as well: `%LOCALAPPDATA%\OpenView`
   (settings, saved partners, logs, this PC's device ID) and the PINs
   OpenView saved in Windows Credential Manager (targets `OpenView:*`).
   The default is **No**; keeping the data means a reinstall gets the same
   device ID. Silent uninstalls always keep the data.

Files received from partners in `Downloads\OpenView` are never deleted.

## Portable zip

Unzip `OpenView-<version>-windows-x64-portable.zip` anywhere you can write
to and run `OpenView.exe` from the `OpenView-<version>` folder.

The zip contains a `portable.txt` file. While it sits next to the
executables, all data lives in a `data` folder next to them instead of
`%LOCALAPPDATA%\OpenView`. Delete `portable.txt` to switch to the normal
location. Two things are still per-user even in portable mode:

- PINs are stored in Windows Credential Manager;
- **Start OpenView at sign-in** writes the `HKCU` `Run` value (pointing at
  the portable folder). Turn it off in the app before deleting the folder.

To remove a portable copy, quit OpenView (tray icon > Quit, or
`OpenView.exe --quit`) and delete the folder. Optionally delete the
`OpenView:*` entries in *Control Panel > Credential Manager > Windows
Credentials*.

## Where OpenView keeps its data

| What | Installed | Portable |
|---|---|---|
| Settings (`settings.json`), saved partners (`connections.json`) | `%LOCALAPPDATA%\OpenView` | `<folder>\data` |
| This PC's identity = device ID (`ovhost-unattended.id`) | `%LOCALAPPDATA%\OpenView` | `<folder>\data` |
| Logs (`logs\openview.log`, `ovhost.log`, `ovrelay.log`, `ovview-<id>.log`, …) | `%LOCALAPPDATA%\OpenView\logs` | `<folder>\data\logs` |
| This PC's PIN and saved partner PINs | Windows Credential Manager (`OpenView:host-pin`, `OpenView:<device id>`) | same |
| Files received from a partner | `Downloads\OpenView` | same |
| Session recordings (`ovview` F6) | `%USERPROFILE%\ovrecord` (or `--record-dir`) | same |

`ovhost --data-dir <dir>` and the `OPENVIEW_DATA_DIR` environment variable
(for `OpenView.exe`) override the data folder. Deleting
`ovhost-unattended.id` gives the PC a new device ID.

Older development builds used other locations; their files are copied over
once on first start (GUI settings from `%APPDATA%\OpenView`, a device
identity next to `ovhost.exe`).

## Firewall

| Port | Direction | Program | Needed for |
|---|---|---|---|
| TCP 4569 | inbound | `ovrelay.exe` | Other devices using the relay that OpenView runs on this PC (**Settings > Relay > Run a relay on this PC**, on by default) |
| UDP 4570 | inbound | `ovhost.exe` | The direct path between viewer and host (`ovhost --direct-port`); without it sessions stay on the relay |
| UDP 4571 | inbound | `ovhost.exe` | LAN sessions without the relay (`ovhost --lan-port`): viewers on the same network |
| UDP 5353 | inbound (shared) | `ovhost.exe` | mDNS: announces the PC on the local network ("Computers on this network", the iOS app's "On this network") |

The host connects **out** to the relay, so a PC that only uses someone
else's relay needs no inbound rule at all. The relay address can be an IP or
host name with an optional port (raw TCP, default 4569) or a `wss://` URL
(WebSocket over HTTPS, outbound TCP 443), which passes networks and proxies
that only allow web traffic.

Ways to allow the two programs:

- **Installer task** *Allow OpenView through Windows Firewall* (one UAC
  prompt), or later from an elevated prompt:
  `"%LOCALAPPDATA%\Programs\OpenView\firewall.cmd" add` (`remove` deletes the
  rules). The rules are named `OpenView relay (TCP-In)` and
  `OpenView direct path (UDP-In)` and are bound to the program paths in that
  folder. `firewall.cmd` is also in the portable zip.
- **Windows' own prompt**: the first time `ovrelay.exe` or `ovhost.exe`
  listens, Windows Defender Firewall asks whether to allow it. Allowing
  needs administrator approval; if you cancel, Windows adds a block rule
  (remove it in *Windows Defender Firewall > Allow an app through firewall*).

The rules apply to **Private** and **Domain** networks only. If Windows
classifies your home network as *Public*, switch it to *Private* in
*Settings > Network & internet* to use the relay on this PC from your LAN.

UPnP: unless turned off (**Settings**, or `ovhost --no-upnp`), the host asks
the router to forward UDP 4570 for the direct path.

## Starting at sign-in

`OpenView.exe --minimized` starts in the tray without showing the window.
The **Start OpenView at sign-in** switch (and the installer task) register
exactly that command under the `HKCU` `Run` key, value name `OpenView`.
OpenView then keeps the host running while you are signed in.

OpenView runs as your user, in your session: it is not a Windows service.
Signing out stops it, and it cannot interact with the lock screen, the
sign-in screen or UAC prompts (see [SECURITY.md](../SECURITY.md#known-limitations)).

## Command-line switches of OpenView.exe

| Switch | Meaning |
|---|---|
| `--minimized` | Start hidden in the tray |
| `--quit` | Ask the running instance to quit (it stops its host and relay) |

Starting `OpenView.exe` while it is already running brings the existing
window to the front. Environment variables for development are listed in
[USAGE.md](USAGE.md#openviewexe-ovgui).
