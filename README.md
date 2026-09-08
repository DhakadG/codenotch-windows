# Codenotch for Windows

A Windows port of [Codenotch](https://github.com/vinzdg/codenotch) — the usage notch that
sits on the edge of your screen and answers two questions at a glance:
**how much of my AI allowance is left**, and **is Claude still working**.

Same design language as the macOS original (inverse-rounded pill, colour-graded rings,
hover card with per-window bars), rebuilt for Windows in Rust + Tauri 2 / WebView2.
No code is copied from the Swift app; the providers are reimplemented from their
documented behaviour and the wire formats.

## What it shows

| Cell | Source | How it reads it |
|---|---|---|
| **Claude** | `GET https://api.anthropic.com/api/oauth/usage` with this app's own OAuth session, or Claude Code's borrowed token when you have not signed in | Session / weekly windows, 429 back-off with a persisted deadline, stale readings dimmed with their age. A thin arc spins inside the ring while a Claude session is working, and pulses amber when one is waiting on you (Claude Code hooks + transcript watcher, desktop app included). |
| **Codex** | `GET https://chatgpt.com/backend-api/wham/usage` with the session Codex keeps in `~/.codex/auth.json` (read only, never refreshed), falling back to the `rate_limits` snapshot in the newest rollout log | Live primary/secondary windows (5h + weekly on paid plans, a monthly window on free) while Codex is signed in; otherwise the last snapshot, marked stale by its own timestamp. |
| **Cursor** | The editor's own session from `state.vscdb` → `cursor.com/api/usage-summary` | Included usage / API usage / on-demand, reset at billing-cycle end. Nothing to sign into: it borrows the editor's session, so there is only ever one account. |
| **Antigravity** | The local `language_server` bridge (quota summary), then Google's Cloud Code API for licensed accounts, then a plain count of today's model turns | Honest degradation: a percentage only when one exists, a `~count` when it does not. |

Providers that are not installed simply do not get a cell.

## Reading the pill

Every provider gets one cell, and every cell is the same four things: a ring, a mark, a
percentage and a countdown. Nothing here is decorative — each part answers a different question.

```
      ╭─────────╮
      │  ╭───╮  │   ← ring:      how much of the window is used
      │  │ ✳ │  │   ← mark:      which provider
      │  ╰───╯  │   ← inner arc: whether a session is working right now
      │   55%   │   ← percentage
      │  2h14m  │   ← countdown: time until this window resets
      ╰─────────╯
```

### Two rings, not one

The thick outer ring is the **five-hour window**; the thin ring just inside it is the **week**.
Both are drawn together because they answer different questions and neither answer implies the
other: a comfortable weekly figure says nothing about the next five hours, and a spent
five-hour window says nothing about the week.

The percentage and the countdown follow whichever window the tray's *Ring shows* is set to, so
the number and the thick ring always agree. With the ring pinned to the weekly window the thin
one is not drawn, because it would be saying the same thing twice.

### Hour marks

Five faint notches on the outer ring, one per hour of the five-hour window.

The ring fills by *usage*, not by time, so on its own it cannot answer "am I spending too
fast" - and the pace mark alone says where you are without saying what that is worth. The
notches turn the ring into a clock face: the pace mark sits between two of them, and the gap
between the filled arc and the pace mark becomes readable in hours rather than in degrees.

Only on the five-hour ring. A week divided into five would be a claim about a length it does
not have.

### The ring, and its colour

The ring fills clockwise from twelve o'clock as the window is used up. Its colour is a
judgement about how much room is left, not a decoration:

| Colour | Used | What it means |
|---|---|---|
| **Green** `#00FF88` | under 50 % | Ample. Nothing to think about. |
| **Yellow** `#F2FF00` | 50 % – 80 % | Worth knowing. Pace yourself if the reset is far off. |
| **Red** `#FF3F00` | 80 % and over | Close to the cap. |
| **Grey** `#303030` | — | The track: the part of the window still unused. A ring showing *only* grey is not an error, it is a window with no published denominator (see below). |

The whole cell **dims to 55 %** when the reading is stale — older than twenty minutes, or the
last fetch failed. The number stays: a stale reading is still the truth about your account,
just old, and blanking it would be less informative rather than more. The hover card says how
old it is.

### The mark in the middle

The provider's own brand mark, or the installed application's icon if the app is on this
machine. It dims when the window is fully used (100 %), which is the one state where the
number alone is easy to misread as "fine, it says a number".

### The pace marks

A short white tick on each ring showing **how far through that window you are**, drawn at
the same angle the ring would reach if you were spending evenly.

That is the second half of the sentence the percentage starts. Sixty percent used is
comfortable an hour before a reset and alarming five minutes into a new window — so:

- **Ring ahead of the tick** — spending faster than the window replenishes.
- **Ring behind the tick** — comfortable.

Both rings get one, sized to their own band: the same idea at two scales rather than two marks
that happen to look alike.

No tick is drawn when the window's length is not one of the two Anthropic publishes, because a
pace mark computed from a guessed length is worse than none.

### The inner arc: is it working?

A thinner arc *inside* the ring, and the only moving part.

| What you see | State | Meaning |
|---|---|---|
| **Green fragment turning** `#28E07B` | running | A session is working right now. |
| **Amber circle pulsing** `#FFBF00` | attention | A session is waiting on **you** — a permission prompt, a question, a finished turn. |
| **Nothing** | idle | No session, or the app cannot tell. |

The two are different shapes on purpose, not one shape in two colours: colour alone is not a
signal everyone can read, and the difference between "it is busy" and "it needs you" is the
most important thing on the pill. They cross-fade over 220 ms so the change reads as a change
of state rather than a flicker.

For Claude the state comes from Claude Code's hooks and the transcript watcher; for the others
it is inferred from recent local activity, so those cells can show *running* and never *attention* —
the signal available cannot tell the difference, and inventing it would be a lie with a colour.

### The percentage, and the tilde

`55%` is a real percentage: the provider published both a used figure and a limit.

`~12` is a **count**, not a percentage, and the tilde says so. Some windows publish what you
have spent but never what the ceiling is; rather than invent a denominator, the cell shows the
count, marks it derived, and the ring draws only its track. A window with no reset time is not
shown at all. This rule is inherited from the macOS original and it is the one rule the whole
app is built on: **never invent a number.**

`—` means no reading: not signed in, or nothing to report.

### The countdown

Time until the ring's window resets: `47m`, `2h14m`, `3d`. It truncates rather than rounds —
rounding up would claim more time than you have, directly under a percentage that says how
little is left.

It disappears while a reading is stale, because a countdown is a claim about *right now*, and
one computed from an old reading keeps ticking toward a moment that has already passed.

### When a window fills up

A desktop notification the first time a window crosses into the red, and **only** the first
time. Each window is armed while it sits below the threshold, fires once on the way up, and
re-arms when it drops back - so a full window is one notification rather than one every poll
for the rest of the afternoon.

The first reading after a launch primes without firing. Starting the app at 90 % should be a
number, not an alert about a crossing that happened before the app was running.

On by default, because the whole point is the times you are not looking at the pill.

### Two ways to read the number

**Show what is left** inverts the number and the arc: `45%` becomes `55%` and the ring empties
instead of filling.

The colour does not invert with it. Red keeps meaning trouble, because a palette that flipped
with the setting would make a nearly-full green ring mean two opposite things depending on
something you cannot see from across the room. The pace marks mirror along with the arc, so the
one rule holds in either mode: **the arc falling short of the mark is the warning.**

**Colourblind palette** rebuilds the ramp along blue → amber → orange rather than recolouring
the red-green one. Red and green are the pair that goes, so the axis changes rather than the
shades; the three steps also differ in lightness, so they survive a greyscale screenshot.

### The stale warning dot

A small amber dot on the ring when a reading is **stale and failing**.

Dimming already says "this number is old". A reading can be old because one refresh was skipped,
which resolves itself, or because every attempt is failing, which does not - and only the second
is worth acting on. The dot is the difference.

### Clicking

| Gesture | What happens |
|---|---|
| **Click** a cell | Refresh that provider. The cell dips as you press and beats once when the request goes out. |
| **Double-click** a cell | Open that provider's usage page in a browser. |
| **Right-click** | Menu: refresh, open usage page, hide this provider. |
| **Drag** the pill | Move it up and down the edge. The position persists. |
| **Hover** | The card, with every window listed and the sessions behind them. |

Refresh is the single-click action deliberately. Opening a browser tab used to be, which made
the most casual gesture in the app the most disruptive one — a stray click on something pinned
to the screen edge threw a window in front of whatever you were doing.

Everything outside the pill and the card is click-through: the window is wider than what it
draws, and the rest belongs to whatever is underneath it.

## Signing in

Tray → **Sign in to Claude**. Two tabs open: Anthropic's authorization page, and a local page
that takes the code it hands back. Paste it, and that is the last time you are asked — the app
holds its own session in Windows Credential Manager and refreshes it before it expires.

If you never sign in, the app falls back to borrowing Claude Code's credential from
`~/.claude/.credentials.json`, read and never written, exactly as it always did. A borrowed
credential cannot be renewed by the borrower, which is why signing in is worth the one click:
when it expires, the only cure is opening Claude Code and signing in there again.

Signing out here forgets only this app's token. Claude Code's credential is never read,
written or invalidated by any of it.

## Keeping out of the way

This app reads endpoints that rate-limit hard, and a lockout costs hours of freshness. Several
guards exist because each was earned:

- **A refetch floor.** A reading younger than ten minutes is served from disk rather than
  refetched — this app is rarely the only thing reading these endpoints.
- **An hourly request ceiling per provider**, persisted, so a burst spread across restarts is
  still visible to it.
- **Local sources first.** Codex records the limits it saw into its own rollout log; Antigravity
  has a bridge on this machine. Reading those costs nobody anything, so the endpoint is asked
  only when the local record is too old to trust.
- **Backoff survives a restart.** A `Retry-After` deadline is written to disk, so relaunching
  cannot bypass it.
- **A provider switched off is not polled at all**, not merely hidden.

## Install / build

Prerequisites: Rust (MSVC toolchain), WebView2 runtime (ships with Windows 11).

```powershell
# from this directory (the repo root here; `windows/` inside the upstream repo)
cargo build --release
.\target\release\codenotch.exe          # pill appears on the right edge of the primary monitor
.\target\release\codenotch.exe doctor   # self-diagnosis: credentials, data sources, icons, hooks
```

Tray menu: sign in / out, providers on and off, which window the ring follows, which parts of
a cell to show, float the pill clear of the edge, refresh now, reset position, open data folder
(`%APPDATA%\codenotch` — logs, persisted readings, icon overrides), start with Windows,
install/uninstall Claude Code hooks.

### Icons

Provider marks are the SVGs from [`@lobehub/icons-static-svg`](https://github.com/lobehub/lobe-icons)
(MIT), embedded unmodified — see `codenotch/glyphs/NOTICE.md`. Drop your own
`claude|codex|cursor|gemini.svg` (or `.png`) into `%APPDATA%\codenotch\glyphs\` to override.
The marks remain the trademarks of their owners.

## Layout

```
.
├── codenotch/          Tauri 2 app: window, tray, providers (usage.rs, codex.rs, cursor.rs, antigravity.rs),
│   ├── src/            oauth.rs (this app's own Claude session), session engine (watcher.rs,
│   │                   state.rs, focus.rs), glyphs.rs, doctor.rs
│   ├── ui/notch.html   the pill + hover card (single file, no framework)
│   └── glyphs/         provider marks (+ NOTICE.md)
└── codenotch-hook/     <5 ms hook messenger Claude Code calls; forwards events to the app
```

## Relationship to upstream

This port follows the upstream design spec (`docs/specs/2026-08-28-usage-notch-design.md`)
and provider semantics. It is developed at
[Im-Midi/codenotch-windows](https://github.com/Im-Midi/codenotch-windows) and offered to the
upstream project as its `windows/` tree; the two are kept in sync. The session-detection engine
originated in [Im-Midi/Pac-Man](https://github.com/Im-Midi/Pac-Man) (MIT).

## License

MIT — see `LICENSE`. The Codenotch design and name belong to the upstream author.
