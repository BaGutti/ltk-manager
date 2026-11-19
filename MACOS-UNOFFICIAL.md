# LTK Manager Unofficial macOS build

This branch packages an **unofficial, experimental** macOS build of LTK Manager. It is not made,
reviewed or supported by LeagueToolkit. Report problems with it here, not upstream.

It is upstream's community PR
[#704](https://github.com/LeagueToolkit/ltk-manager/pull/704) (macOS patcher), plus a fix for the
x86_64 crash on game load, plus the changes below so it can sit next to an official build.

## Status

|                       |                                                                                  |
| --------------------- | -------------------------------------------------------------------------------- |
| Intel (x86_64)        | Tested: a champion mod loads in a Practice Tool game (macOS 26.7, League 16.23). |
| Apple Silicon (arm64) | **Not included.** The arm64 patcher is untested.                                 |
| Code signing          | **None.** macOS Gatekeeper will block the first launch.                          |

## Install

1. Download the `.dmg` from the release and drag the app to Applications.
2. Clear the quarantine flag, since the app is not signed:

   ```bash
   xattr -dr com.apple.quarantine "/Applications/LTK Manager Unofficial.app"
   ```

3. Open the app, import a mod, turn it on and press **Start**. macOS asks for your administrator
   password: the patcher needs root to write into the game process.

## Known issues

- The app does not show the patcher's progress (game found, patched, errors).
- The game can exit right as it starts while the patcher is on. Reconnecting works.
- League updates can break the patcher until it is fixed again.

## Risks

Modifying the game client can break Riot's terms of service and can get an account banned. Use
an account you can afford to lose. You use this build at your own risk.

## What differs from upstream

- `productName` and `identifier` are changed (`tauri.macos.conf.json`), so settings, mods and logs
  live apart from any official LTK Manager.
- The updater is off on macOS: upstream publishes no macOS release to update to.
- Telemetry is off: the build carries no analytics key, so nothing is sent to LeagueToolkit.
- `fopen_hook_x86_64.s` aligns the stack before calling `fopen`.

## Build

```bash
git submodule update --init --recursive
pnpm install
pnpm build:mac-host
LIBRARY_PATH=/usr/local/lib pnpm tauri build   # Homebrew on Intel keeps libwebp here
```
