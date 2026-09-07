# Relationship to the macOS original

This port follows `vinzdg/codenotch`. Where behaviour differs, it should be because Windows
forced it, not because nobody looked. This file records what was taken, what had to change,
and what is genuinely new — so that a reviewer can check the reasoning rather than the diff,
and so that the next change starts by reading the original instead of inventing.

That habit has already earned its keep. Raising the refetch floor to ten minutes silently
broke the staleness display, because the threshold for dimming a ring was five minutes. A
reading is refetched at most every ten minutes, so from the five minute mark until the next
eligible fetch — half of every cycle — a ring would have been dimmed while showing a number
that was correct and at most ten minutes old. The bug was not found by testing. It was found
because `UsageStore.swift` documents the trap:

> Comfortably above `idleRefreshInterval`, on purpose. With the two equal, a ring dimmed the
> instant the *first* idle refresh attempt failed — which reads as "nothing is being read
> any more" when what actually happened is one attempt, five minutes ago.

## Taken unchanged

The design rules, which are the part worth copying most carefully:

- **Never invent a number.** A window with no published denominator shows a count, marked
  derived, and the ring draws only its track. A window with no reset time is not shown at all.
- **Stale beats blank.** A failed read keeps the last reading and says how old it is, rather
  than clearing the cell.
- **Borrowed credentials are read, never written.** `ClaudeCredentials.swift` is explicit:
  "minting a new token would mean writing a credential this app does not own." That decision
  is inherited wholesale, and it is why this port asks Claude Code to refresh its own token
  rather than refreshing it behind Claude Code's back.
- **Expired is not signed out.** Upstream separates `credentialExpired` from `needsAuth`
  precisely so an aged-out token does not discard a perfectly good reading. This port
  collapsed the two at first, which is what left a ring spinning with no bar.
- **Poll cadence.** 60 seconds while a session is active, 300 seconds idle. Arrived at
  independently here and then found to match upstream exactly.
- **Backoff survives a restart.** Upstream restores `retryNoEarlierThan` from its archive on
  construction — "recreating the provider or relaunching must not bypass the server's retry
  deadline." Same rule, same reason.
- **`Retry-After` in both forms.** Seconds or an HTTP-date. This port handled only the numeric
  form until the upstream implementation was read.

## Changed because Windows is different

| Area | macOS | Here | Why |
| --- | --- | --- | --- |
| Claude credential | login keychain, newest item of several | `~/.claude/.credentials.json` | Claude Code stores it differently on Windows. The keychain's duplicate-item hazard has no equivalent — one file, no ordering problem, and no prompt to budget around. |
| Antigravity token | keychain | Windows Credential Manager, `gemini:antigravity` | Where the Go keyring puts it on this platform. Both the plain and `go-keyring-base64:` forms are accepted. |
| Claude transcripts | one location | plus the MSIX-virtualised path under `LocalCache\Roaming` | The packaged desktop app's AppData is virtualised; without this its sessions are invisible. |
| Codex executable | one path | npm shim, vendored exe, `~/.codex/bin`, PATH | `.cmd → node → exe` process trees are a Windows-only shape. |
| Window placement | one screen scale | physical size pinned from the monitor's own scale factor | Mixed-DPI multi-monitor: the physical size could be converted with the *other* monitor's scale, leaving the WebView too narrow. |
| Ring animation | native | CSS transform/opacity on promoted compositor layers | A repainting animation in a transparent always-on-top WebView stutters the desktop compositor. Only transform and opacity are free. |

## New here, with reasons

These have no upstream equivalent. Each exists because of something observed on Windows, and
each is a deliberate divergence rather than parity work.

### An hourly request ceiling

A rolling cap on requests per provider, persisted so it survives restarts.

macOS does not need this. Windows does, because of the hook messenger: when it cannot reach
the application, `codenotch-hook` may launch it — subject to two guards, a marker written by
an explicit quit and a thirty second cooldown between attempts — and every start used to fetch
from every provider. A burst therefore arrived spread across many short-lived processes, which
an in-memory counter cannot see at all. This machine rate-limited its own account twice before
the ceiling existed; both guards were added afterwards, and neither removes the need for it.

### The refetch floor

A reading younger than the floor is served from the persisted copy rather than refetched.
Ten minutes, not five, because this app is rarely the only thing reading these endpoints — a
taskbar widget or a status line on the same account shares whatever budget the endpoint
enforces, and the observed evidence is that the limit has an account-level component and not
only a per-token one.

### Local sources first

Codex records the limits it saw into its own rollout log on every turn. When that log is
recent it holds the same numbers the endpoint would return, for the cost of reading a file,
so the endpoint is asked only when the local record is too old to trust. Antigravity's local
bridge is preferred the same way and is deliberately *outside* the ceiling: it is a loopback
call to a process already running on this machine and costs nobody anything.

### Hook wiring, and what is not wired

Claude Code runs a hook command through a POSIX shell. On a Windows machine whose `bash` is
the WSL one, that shell took **up to 4.2 seconds** to start, against 57 ms for the messenger
it runs. Wired to `PreToolUse` and `PostToolUse` with a `*` matcher, that cost was paid twice
on every tool call and sessions visibly froze.

Only five events are wired now — session start, prompt submitted, attention, stop, session
end — all of which fire a handful of times per session. Tool-level activity is not lost: the
transcript watcher reports it without spawning anything.

### Click-through, by sampling instead of hit testing

Upstream's `NotchHostingView.hitTest` returns nil outside the rectangles the view reports as
interactive, so AppKit resolves a click over the panel's transparent area to whatever is
underneath. It is exact, it happens per event, and it costs nothing.

WebView2 offers no equivalent hook. The same rule is applied here by comparing the system
cursor against the same rectangles on a 50 ms tick and toggling `WS_EX_TRANSPARENT` on the
whole window. Same rectangles, same rule, sampled rather than exact - which is why the test
carries a small margin: the cursor can be a few pixels short of the pill on the tick before
the click arrives, and without the margin the first click of an approach is lost.

Until this existed the window's 270 px of transparent area swallowed every click aimed at what
was behind it, which from the outside looked like a dead strip down the right of the screen.

### Individually switchable parts of a cell

A `Show` submenu for the percentage, the countdown, the pace mark and the activity arc.

Upstream has nothing like it, and that reads as a decision rather than a gap: its settings are
about what the notch *is* - which edge, how visible, which providers connected - not about
which decorations a cell carries. This comes from the taskbar mod, where every element of a
bar is switchable, and it earns its place for the mod's reason: 70 pt is not much room, and
what counts as the useful part differs per person.

All default to on, so the app looks unchanged for anyone who does not go looking.

### A countdown on the pill itself

Upstream puts reset copy in the tooltip card only. The mod puts a countdown on the bar, and
that is the version ported here, because the pace mark on the ring already makes the same
point graphically and the number is what makes it precise.

It truncates where the card's copy rounds: rounding up claims more time than there is, and the
two sit inches apart on screen.

### Build identity

`build.rs` stamps the commit, a dirty flag and the compile time into the binary; installers
are filed under the same identity. Not an upstream concern, but a local one: without it an
installer that had silently failed to replace an executable was indistinguishable from one
that worked, and an hour went into debugging the wrong binary.

## Still behind upstream

The macOS app has moved on since this port was written. Not yet here:

- **Providers**: GLM, Grok, OpenCode, Perplexity.
- **`WebSessionProvider`** — a session this app owns, rather than one borrowed, which is the
  only route by which a real sign-out or in-app sign-in is possible.
- **`ClaudeProfile`** — multiple Claude accounts, each with its own credential.
- **`CredentialCache`** — holds a credential until it expires. On macOS this saves a keychain
  prompt; here it would only save a file read, so the value is smaller, but it is free.

Anything added to this port should start by checking whether upstream has already solved it,
and say so either way.
