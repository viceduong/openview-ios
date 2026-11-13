# Installing OpenView on Windows

OpenView runs on **Windows 10 and Windows 11, x64**. The binaries link the C
runtime statically, so no Visual C++ redistributable or other runtime is
needed. Two packages are published on the
[Releases](https://github.com/viceduong/openview/releases) page:

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

## Run a relay on Linux/Docker

The relay (`ovrelay`) also runs on any Linux server, so viewers and hosts
can meet on a machine that is always on. It is one static binary with no
configuration file: it keeps no state on disk and logs to stderr. Pick one
of the three ways below.

### Release binary

Each release has two Linux archives, built against musl (no glibc version
requirement):

| Archive | CPU |
|---|---|
| `ovrelay-<version>-linux-x86_64.tar.gz` | x86-64 |
| `ovrelay-<version>-linux-aarch64.tar.gz` | 64-bit ARM (Raspberry Pi 4/5 with a 64-bit OS, Graviton, Ampere) |

Each contains a folder of the same name with `ovrelay`, `LICENSE` and
the systemd unit `ovrelay.service`.
Download the archive and `SHA256SUMS` from the same release, then:

```sh
sha256sum --ignore-missing -c SHA256SUMS
tar -xzf ovrelay-0.1.0-linux-x86_64.tar.gz
sudo install -m 0755 ovrelay-0.1.0-linux-x86_64/ovrelay /usr/local/bin/ovrelay
ovrelay --version
```

Running `ovrelay` in a terminal starts it on `0.0.0.0:4569`; `ovrelay --help`
lists the options (connection limits, park timeout, WebSocket paths).

### systemd service

[`packaging/linux/ovrelay.service`](../packaging/linux/ovrelay.service) runs
the binary above as a sandboxed service: a throw-away user
(`DynamicUser=yes`), a read-only file system, no capabilities and a system
call filter.

```sh
sudo install -m 0644 ovrelay-0.1.0-linux-x86_64/ovrelay.service /etc/systemd/system/ovrelay.service
sudo systemctl daemon-reload
sudo systemctl enable --now ovrelay
systemctl status ovrelay
journalctl -u ovrelay -f
```

To pass other flags, run `sudo systemctl edit ovrelay` and add:

```ini
[Service]
ExecStart=
ExecStart=/usr/local/bin/ovrelay --bind 0.0.0.0:4569 --max-per-ip 64
```

### Docker

The image is built from this repository
([`packaging/docker/Dockerfile`](../packaging/docker/Dockerfile)): a static
binary on a distroless base that runs as a non-root user. From the
repository root:

```sh
docker build -f packaging/docker/Dockerfile -t openview-relay .
docker run -d --name ovrelay --restart unless-stopped -p 4569:4569 \
  --read-only --cap-drop ALL --security-opt no-new-privileges openview-relay
docker logs -f ovrelay
```

Or with Compose ([`packaging/docker/compose.yaml`](../packaging/docker/compose.yaml)),
which sets the same restrictions:

```sh
docker compose -f packaging/docker/compose.yaml up -d --build
```

Arguments after the image name are passed to `ovrelay` (for example
`--max-per-ip 64`). Change the listen address with the `OVRELAY_BIND`
environment variable rather than `--bind`: the image's `HEALTHCHECK` runs
`ovrelay --healthcheck`, which reads the same variable and sends
`GET /health` to that port (on loopback when it is `0.0.0.0`). `docker ps`
shows the result as `healthy` / `unhealthy`.

### Check that it works

```sh
curl http://127.0.0.1:4569/health    # prints "ovrelay"
ovrelay --healthcheck                # exit status 0 when the relay answers
```

Then point a host and a viewer at it: **Settings > Relay** in OpenView,
`--relay relay.example.com` for `ovhost` / `ovview`, or the relay setting of
the iOS app.

### Firewall

Allow inbound **TCP 4569**. One port carries raw relay clients, WebSocket
clients and the health check. For example:

```sh
sudo ufw allow 4569/tcp
# or
sudo firewall-cmd --permanent --add-port=4569/tcp && sudo firewall-cmd --reload
```

Also open the port in your cloud provider's security group if there is one.
The relay does not need UDP; the direct path between viewer and host does
not go through it.

### TLS (`wss://`) through a reverse proxy

`ovrelay` does not terminate TLS itself. Raw TCP clients connect to port 4569
directly; for `wss://` (networks that only allow HTTPS, the iOS app behind
strict Wi-Fi) put a reverse proxy in front that forwards the WebSocket path
`/relay` with the upgrade headers. With [Caddy](https://caddyserver.com/),
which gets a certificate automatically, the whole `Caddyfile` is:

```text
relay.example.com {
    reverse_proxy /relay 127.0.0.1:4569
}
```

Clients then use `wss://relay.example.com/relay`. With nginx, forward
`location /relay` to `http://127.0.0.1:4569` with `proxy_http_version 1.1`,
`proxy_set_header Upgrade $http_upgrade`, `proxy_set_header Connection "upgrade"`
and a long `proxy_read_timeout` (the relay pings WebSocket clients every
15 s). If the proxy forwards a different path, add it with `--ws-path`.

Through a proxy on the same machine every WebSocket client appears to come
from 127.0.0.1. Loopback is exempt from the per-IP limit (`--max-per-ip`),
so only the total limits apply to them. The relay never sees decrypted
traffic, with or without TLS: sessions are end-to-end encrypted between
viewer and host.
