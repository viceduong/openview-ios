# Images

Screenshots used by the README and the docs.

| File | Shows |
|---|---|
| `remote-control-dark.png` | `OpenView.exe` Remote Control page, dark theme (test mode, throwaway test ID) |

Pending (reference them from the README once they exist):

| File | How to produce it |
|---|---|
| `remote-control-light.png`, `settings.png`, `lan-discovery.png` | Windows app in screenshot mode: `OPENVIEW_DEV_PORTS=<port>` plus `OPENVIEW_SCREENSHOT=1` (`OPENVIEW_SCREENSHOT_PAGE=remote\|settings\|about`, `OPENVIEW_SCREENSHOT_THEME=light\|dark`); a scratch host named `DEMO-PC` populates *Computers on this network* |
| `viewer.png` | `ovview` connected to a scratch host with a synthetic screen |
| `ios-home.png`, `ios-home-dark.png`, `ios-settings.png` | `scripts/ios_screenshots.sh` on macOS (or the manual `screenshots` job of the iOS workflow) |

Rules for screenshots:

- Take them in **test / screenshot mode** or on a throwaway machine.
- No real device IDs, PINs, IP addresses, host names, relay URLs, user
  names or personal desktop content.
- Crop to the window, PNG, under 400 KB each.
