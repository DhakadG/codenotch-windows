# Roadmap

Work that is deliberately **not** in the first shipped release, with the reason it can wait.
Each entry is a shape of a change rather than a plan; none of it blocks a signed installer.

Ordered roughly by how much a user would notice its absence.

## Native settings window

Today the tray menu carries every choice: refresh, reset position, open the data folder,
start with Windows, install or uninstall the Claude Code hooks, and language. That is
already at the limit of what a menu should hold, and the next setting will not fit.

Needed once there is anything to configure that is not a toggle — provider order, per-window
thresholds, or a poll interval. Wants a second Tauri window rather than more menu items.

## Four-edge placement

`place_notch` pins the pill to the right edge of the primary monitor and nothing else. The
mixed-DPI handling it already does is the hard half of the problem; the remaining work is
choosing an edge, persisting it, and rotating the inverse-rounded fillets to match.

Related and larger: pinning to a chosen display rather than the primary one. Upstream has
two open pull requests on exactly this for macOS, so the design should follow whichever one
lands rather than inventing a third answer.

## Click-through outside the pill

The window is a 340 x 460 rectangle that is mostly transparent, and the transparent part
still swallows clicks. On Windows this is `WS_EX_TRANSPARENT` toggled by hit-testing against
the pill's actual bounds, which interacts awkwardly with the non-activating window and the
`mouseleave` handling that already needed care.

## Session list with click-to-jump

The hover card shows per-window bars and the working arc, but not which sessions are
running. The session store already holds them; what is missing is the row rendering and
raising the right terminal or editor window when one is clicked.

## More locales

`i18n.rs` covers English, Chinese and Japanese, chosen because the author could verify them.
Everything user-visible is already routed through it, so a new locale is a data change
rather than a code change. Contributions welcome; machine-translated strings are not, since
nobody can review them.

## ARM64 build

`.cargo/config.toml` already carries the static-CRT flag for `aarch64-pc-windows-msvc`, so
the toolchain side is prepared. What is missing is a runner and a device: nothing here has
been built or run on Windows on ARM, and claiming support without having done so would be
worse than not shipping it. The blocking question is whether WebView2 and the tray behave
identically there.

## Auto-updater

Tauri ships an updater plugin. It is deliberately absent from v1 because it needs a signing
key of its own, a hosted manifest, and a decision about whether the application may reach a
server it does not need for its actual job — a program that reads local credentials and
promises no telemetry should not quietly start polling a host on its own schedule.

Until then, an update is a manual reinstall from the GitHub release. Revisit once code
signing is in place, so the updater and the installer share one identity.
