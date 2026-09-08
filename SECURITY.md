# Security

Codenotch for Windows reads credentials that belong to other applications. This document
states exactly which files it opens, which hosts it contacts, and what it deliberately does
not do, so that a reviewer does not have to derive it from the source.

Statements here are checked against the code in this repository. If you find one that is no
longer true, that is a bug worth reporting on its own.

## What it reads

| Provider | Path | What is taken |
| --- | --- | --- |
| Claude | `%USERPROFILE%\.claude\.credentials.json` | The OAuth access token Claude Code stores under `claudeAiOauth`. Read only; never refreshed, never rewritten. |
| Claude sessions | `%USERPROFILE%\.claude\projects\**\*.jsonl` | Transcript tails, to tell whether a session is working or waiting. |
| Claude Desktop | `%APPDATA%\Claude\local-agent-mode-sessions`, and the MSIX-virtualised copy of it under `%LOCALAPPDATA%\Packages\<claude or anthropic package>\LocalCache\Roaming\Claude\` | The same, for the desktop client. |
| Codex | `%USERPROFILE%\.codex\auth.json` | The stored session. Read only, never refreshed. |
| Codex sessions | The newest rollout log under the Codex home, and the desktop app's `thread_turns` table | The last `rate_limits` snapshot, used only when Codex is signed out, and turn activity. |
| Cursor | The editor's `state.vscdb` (SQLite), opened read-only | The editor's own session cookie, used to call Cursor's usage endpoint. |
| Antigravity | Windows Credential Manager, generic credential `gemini:antigravity` | The access token for the Cloud Code quota call, when a local bridge is not answering. |

Providers that are not installed are not read, and get no cell in the UI.

`hooks_install` is the only thing that **writes** to a file it did not create: it merges hook
entries into `%USERPROFILE%\.claude\settings.json`. It writes a timestamped backup first,
identifies its own entries by command substring so it never removes anyone else's, and
uninstall restores the document to its exact prior state. This is covered by tests in
`codenotch/src/hooks_install_tests.rs`.

## Where data goes

Each credential is sent to that provider's own API and nowhere else:

- `api.anthropic.com/api/oauth/usage`
- `chatgpt.com/backend-api/wham/usage`
- `cursor.com/api/usage-summary`
- `127.0.0.1` — the Antigravity `language_server` bridge, on a port discovered from the
  running process. Its self-signed certificate is accepted **for loopback only**.
- Google's Cloud Code API, for licensed Antigravity accounts.

There is no telemetry, no analytics, no crash reporting and no update ping. The application
has no server of its own. Nothing is sent to the port's author, to the upstream Codenotch
author, or to any third party.

## The local event server

The application listens on `127.0.0.1` at a configurable port (see
`%APPDATA%\codenotch\config.json`) to receive events from `codenotch-hook.exe`, the small
binary Claude Code invokes on session events.

- Bound to loopback only, never to an external interface.
- Request bodies are capped at 256 KB.
- Every request is answered with the literal string `ok`. The server never returns usage
  data, credentials, or any other state.

It is unauthenticated, which is a deliberate trade rather than an oversight: any process
already running as the signed-in user can read the credential files directly, so requiring a
token here would defend nothing it does not already have. The worst a local process can do
by posting to it is make a ring animate at the wrong moment. If you disagree with that
trade, the hook wiring can be uninstalled from the tray menu and the port left unused.

## Diagnostics

`codenotch.exe doctor` prints where every data source lives and whether it answered. It is
meant to be pasted into a bug report, so it must not carry a secret out with it.

Two mechanisms enforce that, in `codenotch/src/doctor.rs`:

1. The whole report passes through one redaction function before it is returned. JWTs,
   `sk-`/`sk-ant-` prefixed keys, and `Bearer <token>` are replaced with `[redacted]`.
2. A test reads the real credential files present on the machine and asserts that none of
   those exact values appear anywhere in a real report. It is exact rather than
   shape-guessing, and it is the guarantee the first mechanism is only a backstop for.

The report does contain file paths, which include your Windows user name, and session
identifiers. Both are needed to diagnose anything; neither grants access to an account.

## Reporting a vulnerability

Open an issue at <https://github.com/Im-Midi/codenotch-windows/issues>. If the issue would
disclose a credential or a working exploit, say so without the detail and ask for a private
channel first.
