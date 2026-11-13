# Images

Screenshots used by the README and the docs. All are taken in screenshot
mode with demo data (no real device IDs, PINs, addresses or relay URLs).

| File | Shows | How to reproduce |
|---|---|---|
| `remote-control-dark.png`, `remote-control-light.png` | Windows app, Remote Control page | `OPENVIEW_DEV_PORTS=<port>` + `OPENVIEW_SCREENSHOT=1` (`OPENVIEW_SCREENSHOT_THEME=dark\|light`) |
| `settings.png` | Windows app, Settings page | as above with `OPENVIEW_SCREENSHOT_PAGE=settings` |
| `lan-discovery.png` | *Computers on this network* card | as above, with a scratch host named `DEMO-PC` on the LAN |
| `viewer.png` | `ovview` window on a host's synthetic test pattern | `ovhost --test-pattern video` + `ovview --id` |
| `ios-home.png`, `ios-home-dark.png`, `ios-settings.png` | iOS app (iPhone simulator) | `scripts/ios_screenshots.sh` on macOS, or the manual `screenshots` job of the iOS workflow |

Rules for new screenshots:

- Take them in **screenshot / test mode** or on a throwaway machine.
- No real device IDs, PINs, IP addresses, host names, relay URLs, user
  names or personal desktop content.
- Crop to the window, PNG, under 400 KB each.
