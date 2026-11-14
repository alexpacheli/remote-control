# Architecture

**English** | [Português](ARCHITECTURE.pt-BR.md)

How Hext Remote works today (version 0.8). The reasons behind the main choices are in
[docs/DECISIONS.md](docs/DECISIONS.md), what is done and what is next in [docs/STATUS.md](docs/STATUS.md), and the
original plan, written before the code, in [docs/planos/plano-original.md](docs/planos/plano-original.md) (in
Portuguese).

## 1. Pieces

```
            ┌───────────────────────── hext.com.br (one Linux VM) ─────────────────────────┐
            │  Caddy (TLS)  →  hext-server: IDs, presence, requests, accounts (SQLite)     │
            │                  iroh-relay: only when there is no direct path               │
            └──────────────────────────────────────────────────────────────────────────────┘
                    ▲ WebSocket (signaling)                       ▲ relay (encrypted bytes)
                    │                                             │
  ┌─────────────────┴───┐        QUIC (iroh), end to end         ┌┴────────────────────┐
  │ Technician          │◄──────────────────────────────────────►│ Customer            │
  │ ui + view           │   video, input, clipboard, files, chat │ ui + host / service │
  └─────────────────────┘                                        └─────────────────────┘
```

- **The app** (`apps/hext`): one executable, `HextRemote.exe` / `Hext Remote.app` / `hext-remote`, with modes:
  - `ui`: the interface (Tauri 2; plain HTML, CSS and JavaScript in `apps/hext/ui`). It shows the ID, the
    requests, the session bar, the account and the computer lists.
  - `view`: the session window, a separate native process (`winit` + `wgpu`). Video never goes through a
    webview, and a stuck viewer does not take the interface down. The windows share the event loop's
    thread; each session draws on a thread of its own, which the decoder wakes directly, so nothing that
    waits on the GPU or the screen runs where the windows move ([janela-travada](docs/estudos/janela-travada.md)).
  - `host`: shares this computer, inside the interface's process on a portable run.
  - `service`: installs and runs the unattended mode (section 4).
- **The server** (`apps/hext-server`, `axum` + `tokio`): hands out the 9-digit IDs, knows who is online, passes
  connection requests on, keeps the optional accounts and decides who may use the relay. It never sees a
  session. Protocol and operation: [docs/hext-server.md](docs/hext-server.md) (in Portuguese).
- **The relay** ([`iroh-relay`](https://github.com/n0-computer/iroh)): carries encrypted packets when the two
  computers cannot reach each other directly (strict NAT, corporate firewall, HTTPS on port 443 only).

## 2. A connection

1. On first run, the app creates the computer's **Ed25519 key** (protected by DPAPI on Windows and the Keychain on
   macOS). It signs in to the server by signing a challenge, and gets its **ID**, random and never sequential.
2. The technician asks for an ID. The server passes the request, with the technician's key, to that computer.
3. The host decides (section 5): it asks the person in front of it, or checks the access password, a bound
   technician's key or the account's signed device list.
4. The two open a **QUIC** connection with [`iroh`](https://iroh.computer): direct when the networks allow it
   (hole punching), through the relay otherwise. Each side proves its key in the handshake, and the viewer pins
   the host's key (TOFU), warning if it changes.

## 3. Video and channels

```
capture (GPU) → NV12 → H.264 encode → packets + FEC → QUIC datagrams → decode → wgpu render
```

| Step | Windows | macOS | Fallback |
|---|---|---|---|
| Capture | DXGI Desktop Duplication (also the sign-in screen and UAC, from the service) | ScreenCaptureKit, at the screen's logical size | — |
| Encode | Media Foundation (graphics card), zero-copy from the capture | VideoToolbox | OpenH264 (CPU) |
| Decode | Media Foundation with Direct3D 11 | VideoToolbox | OpenH264 |

- H.264 only, low-latency settings, no B-frames. Keyframes on demand (start, loss, monitor switch).
- Video travels as **QUIC datagrams**: a lost packet blocks nothing. Each frame is split into blocks of up to 64
  packets with Reed-Solomon parity (20%, 40% on keyframes), interleaved, and sent with **pacing** (at most 4× the
  bitrate) so a keyframe does not leave as a burst. The bitrate adapts to the path.
- The **cursor** is not in the video: the host sends its shape and position, and the viewer draws it locally.
- **Several monitors:** the viewer switches with `Ctrl+Alt+1..9` or the numbered buttons on its left edge.
- **Virtual monitor:** with no monitor attached, the host creates one for the session and removes it at the end
  (MttVDD driver on Windows, optional in the installer; `CGVirtualDisplay` on macOS). `Ctrl+Alt+0` turns it on
  and changes its resolution. Details: [docs/monitor-virtual.md](docs/monitor-virtual.md) (in Portuguese).
- **Sound:** what the host plays (WASAPI loopback on Windows, ScreenCaptureKit on macOS 13+) goes as Opus, 48 kHz,
  10 ms per datagram (each also carrying the previous one), beside the video's pacing and FEC, and nothing while
  silent; the viewer keeps about 40 ms in a queue that never grows past 70 ms and plays it with `cpal`.
  `Ctrl+Alt+S` mutes it. Details: [docs/planos/F7.md](docs/planos/F7.md) (in Portuguese).

Other channels, on reliable QUIC streams: keyboard by scancode (works across layouts) and mouse (`SendInput`,
`CGEvent`), clipboard text, files (drag to the window; they land in Downloads), chat, remote restart with
automatic reconnection, and `Ctrl+Alt+Del` (`Ctrl+Alt+End`) through the Windows service.

## 4. Unattended mode

**Windows** (the `.msi`, or `service install`):

```
hext service (SYSTEM, session 0)  signaling, presence, access checks, audit log, updates
   └─ agent (in the active session, or on the sign-in / UAC desktop)  capture, encode, input
ui (always as the user)  started by the service, talks to it over a pipe only the two can open
```

- A service in session 0 cannot capture the screen, so it starts the agent in the active session and starts it
  again when the desktop switches to the sign-in screen or a UAC prompt.
- The interface never runs as SYSTEM: a webview with system rights would be a privilege escalation path.
- Installed, the computer uses the **machine's ID** (the same for every user, kept across reinstalls). Settings,
  logs and the audit log are in `C:\ProgramData\HextRemote`, which only administrators open.

**macOS:** the app opens at login (a per-user LaunchAgent) and offers the same unattended access (password,
bound technicians, account), stored in the user's folder. After a restart it comes back when someone signs
in: there is no system service for the sign-in screen ([DECISIONS](docs/DECISIONS.md#mac-no-sign-in-screen)).

**Linux:** the interface, as a technician only: account, My computers, recent ones and opening other computers,
each in the viewer's window. Being accessed (a host) comes later.

## 5. Who gets in

The host decides, never the server. A compromised server or database opens no computer.

| Way in | How |
|---|---|
| **Attended** (default) | The person in front of the computer sees the technician's name and ID and chooses the permissions: control, clipboard, files, restart. |
| **Access password** | Set by the owner. Checked with **SPAKE2**: the password never travels, and neither the server nor the relay can learn it. |
| **Bound technician** | A technician accepted once can be bound: the host keeps their key and lets them in without asking. Revocable on the host. |
| **Account** | The account is a list of devices in which **each entry is signed by a device already on it**. The host bound to the account keeps that list and checks the signatures, so the server cannot forge an entry. Sharing with another account and revoking are signed entries too. |

**Transparency is not optional:** every session shows a bar on the host with who is connected and a button to
end it, and each access goes to the computer's audit log. There is no hidden mode
([decision](docs/DECISIONS.md)). Rate limits on the server and on the host slow down anyone guessing IDs or
passwords.

Accounts (optional): e-mail, password and TOTP, with recovery codes; managed in the app only. The server keeps
them in SQLite, and an unconfirmed account is deleted in 24 hours.

## 6. Distribution and updates

- `.github/workflows/release.yml` builds everything from a `v*` tag and opens a **draft** release; a person
  publishes it. Each installer is tested in CI (install, run, uninstall).
- Windows: `.msi` (app, service, optional virtual monitor driver) and a portable `.exe`, **not code-signed**:
  SmartScreen warns on first open. macOS: `.dmg`, signed with a Developer ID and notarized. Linux: `.deb`
  and `.rpm` (x86_64 and ARM64), `.AppImage` (x86_64).
- **Updates:** each release carries `latest.json` (versions and SHA-256 of each installer) signed with an
  Ed25519 key kept as a repository secret, by `hext-release`, a small program that does only that (no interface,
  no network); the app accepts only that signature. The Windows service checks every
  hour and installs between sessions; the Mac app swaps itself in Applications (Developer ID builds only);
  elsewhere the home screen offers the download.

## 7. Platforms

| | Windows | macOS | Linux |
|---|---|---|---|
| Technician (viewer) | ✅ | ✅ | ✅ (interface) |
| Customer (host) | ✅, including sign-in screen and UAC | ✅, after login | test pattern only |
| Unattended | ✅ service | ✅ after login | — |
| Virtual monitor | ✅ (optional driver) | ✅ | — |

## 8. Code

The folders are listed in the [README](README.md#layout). Platform code stays behind `#[cfg(target_os = ...)]`,
and CI builds and tests on Windows, macOS and Linux on every push. Code comments are in English; the interface
and most internal documents are in Portuguese.
