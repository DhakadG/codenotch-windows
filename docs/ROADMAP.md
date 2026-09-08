# Roadmap

## Settled: `claude auth status` does not refresh an expired token

It was asked because the answer decided how the app should behave when a borrowed credential
aged out. In practice neither the nudge nor the tray's Refresh credentials ever refreshed
anything, and the only cure was opening Claude Code and signing in again - every time.

The route out was not the heavier one this file proposed. The application now holds **its own
Claude session** (`oauth.rs`), which it can refresh because it owns it, so an expired borrowed
credential is no longer the end of the road. The nudge remains as the fallback for anyone who
has not signed in here, where it is still the only move available.

The phantom-session hazard described below never had to be faced, and `watcher.rs` needs no
directory exclusion.

## Upstream has moved on

The macOS app has gained providers this port does not have: GLM, Grok, OpenCode and
Perplexity, plus a `WebSessionProvider` for session-backed reads and a profile system
(`ClaudeProfile`) for people signed into more than one Claude account. Worth tracking before
claiming feature parity.

Two smaller things it does that this port does not:

- **`CredentialCache`** holds the token until it expires, so the credential is read about
  once an hour rather than twice a minute. This port re-reads `.credentials.json` on every
  poll. Harmless on Windows, where the read is a file rather than a keychain prompt, but it
  is free to fix.
- **Keychain item selection.** macOS has to pick the newest of several items because Claude
  Code files a new one per rotation. Windows has no equivalent problem - one file - so there
  is nothing to port, only a trap not to reinvent.


Work that is deliberately **not** in the first shipped release, with the reason it can wait.
Each entry is a shape of a change rather than a plan; none of it blocks a signed installer.

Ordered roughly by how much a user would notice its absence.

## Native settings window

The tray menu was described here as "already at the limit of what a menu should hold". It
has since gained a Providers submenu, a Ring shows submenu, a six-item Show submenu, sign in
and sign out, two window-starter items, three display modes, the float toggle and the hook
shell shortcut. It is well past that limit and the argument for a settings window is now much
stronger than when this was written.

Needed for anything that is not a toggle - provider order, per-window thresholds, a poll
interval, the red threshold that currently only exists in `config.json`. Wants a second Tauri
window rather than more menu items.

## Four-edge placement

`place_notch` pins the pill to the right edge of the primary monitor and nothing else. The
mixed-DPI handling it already does is the hard half of the problem; the remaining work is
choosing an edge, persisting it, and rotating the inverse-rounded fillets to match.

Related and larger: pinning to a chosen display rather than the primary one. Upstream has
two open pull requests on exactly this for macOS, so the design should follow whichever one
lands rather than inventing a third answer.

## Session list with click-to-jump

The hover card shows per-window bars and the working arc, but not which sessions are
running. The session store already holds them; what is missing is the row rendering and
raising the right terminal or editor window when one is clicked.

## More locales

`i18n.rs` covers English, Chinese, Japanese and Korean, chosen because the author could
verify them.
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
