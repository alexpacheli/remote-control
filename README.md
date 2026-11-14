# Hext Remote

**English** | [Português](README.pt-BR.md)

Remote access and support for Windows, macOS and Linux, focused on smoothness. Free software
([AGPL-3.0](LICENSE)), written in Rust.

- **Connect by ID:** each computer has a 9-digit ID. The technician types the ID, and the person on the other
  side accepts or declines.
- **Direct when possible:** the two computers connect directly, even on different networks. When they cannot
  (a corporate firewall, for example), the session goes through a relay, always end-to-end encrypted.
- **Video on the graphics card:** capture, H.264 encoding and display on the GPU, falling back to the CPU
  automatically.
- **Unattended access:** with the service installed, the computer is online from boot. The technician gets in
  with a password the owner set or, later, without a password, by their key or by the account. On Windows, this
  also works on the sign-in screen and UAC prompts.
- **In the session:** multiple monitors, clipboard, files, chat, remote restart and up to 8 sessions at once.
- **Computers without a monitor:** an optional virtual monitor (Windows and macOS) gives the session an image on
  a server or a Mac mini with no screen attached.
- **Accounts (optional):** a synced list of computers, sharing with other accounts and an access log.
- **Transparent:** every session is shown on screen while it lasts, and each access is logged on the computer.

## How it works

```
                    ┌──────────────────────────────┐
         ┌─────────►│  Signaling server            │◄─────────┐
         │          │  (hext-server): IDs,         │          │
         │          │  presence and requests       │          │
         │          └──────────────────────────────┘          │
         │                                                    │
  ┌──────┴───────┐     direct session (P2P, QUIC)      ┌──────┴───────┐
  │  Technician  │◄───────────────────────────────────►│   Customer   │
  │ Hext Remote  │                                     │ Hext Remote  │
  └──────┬───────┘                                     └──────┬───────┘
         │          ┌──────────────────────────────┐          │
         └─────────►│  Relay (iroh-relay): only    │◄─────────┘
                    │  without a direct path; it   │
                    │  cannot see the content      │
                    └──────────────────────────────┘
```

1. When it opens, the app registers with the **signaling server** using the computer's key (Ed25519, created on
   first run) and gets its ID.
2. The technician asks to connect to an ID. The server passes the request on, and the other side accepts, or
   checks the technician's password or key.
3. The two open a **QUIC** connection to each other, through NAT. If they cannot, they use the **relay**.
4. The session is end-to-end encrypted between the two computers: neither the server nor the relay sees the
   screen, the keyboard or the files. The access password never travels (SPAKE2).

Details in [ARCHITECTURE.md](ARCHITECTURE.md). Why it is built this way: [docs/DECISIONS.md](docs/DECISIONS.md);
where it stands: [docs/STATUS.md](docs/STATUS.md); every document: [docs/](docs/README.md).

## Download and use

The installers are in [Releases](https://github.com/alexpacheli/hext-remote/releases):

| System | File | Notes |
|---|---|---|
| Windows | `HextRemote-<version>-x64.msi` | Installs the app and the service (unattended access, sign-in screen). For many computers: `msiexec /i HextRemote-<version>-x64.msi /qn`. |
| Windows | `HextRemote-<version>-portable.exe` | For a quick support session, without installing. |
| macOS | `HextRemote-<version>.dmg` | Drag Hext Remote to Applications. |
| Linux | `hext-remote_<version>-1_amd64.deb` (Ubuntu 22.04+, Debian) or `hext-remote-<version>-1.x86_64.rpm` (Fedora); on ARM, `_arm64.deb` and `.aarch64.rpm`; also `.AppImage` (x86_64) | `sudo apt install ./hext-remote_<version>-1_amd64.deb` or `sudo dnf install ./hext-remote-<version>-1.x86_64.rpm` (they bring the libraries they need); it then shows in the menu. For now, to access other computers only: it has no ID, so it cannot be accessed. |

The Windows installers are not code-signed: on first open, SmartScreen warns (More info → Run anyway). The
macOS app is signed with an Apple Developer ID and notarized. See the [privacy policy](#privacy-policy).

**First session:**

1. On the customer's computer, open Hext Remote. The home screen shows its **ID**.
2. On the technician's computer, open Hext Remote, type that ID and click **Conectar** (Connect).
3. The customer sees the request with the technician's name and permissions. Once accepted, the session starts.

To get in without anyone accepting, install the service (the `.msi` does) and set the access password in the
**Segurança** (Security) tab. The full walkthrough of each feature, including the command line, is in
[docs/roteiros/testes.md](docs/roteiros/testes.md), and the field support checklists in
[docs/roteiros/](docs/roteiros/) (in Portuguese). The interface is in Portuguese for now.

**Updates.** Installed on Windows (the service) or on a Mac (in Applications), Hext Remote updates itself
between sessions. Otherwise, the home screen says when there is a new version and opens its page (on Linux,
install the new package with `apt` or `dnf`, as above). Each version is signed by the release pipeline, and the app refuses any other.

## Your own server

The official build uses our server (`signal.hext.com.br` and `relay.hext.com.br`). Whoever supports many
computers, wants the data on their own infrastructure or distributes a modified build runs their own server and
builds the app pointing to it. The server and the relay come from this repository and the
[iroh](https://github.com/n0-computer/iroh) project, with no paid service.

In short: a Linux server with a public IP (1 to 2 vCPUs and 1 to 2 GB of RAM to start), two DNS names
(`signal.<domain>` and `relay.<domain>`), ports 80/443 TCP and 7842 UDP, then `docker compose up` or
`./deploy/install.sh`, and an app built from your fork with your server's address.

**Step-by-step guide: [docs/self-hosting.md](docs/self-hosting.md)**: server (Docker or systemd), account
e-mails, your own app build, update key, backups, traffic and security.

## Building

You need [Rust](https://rustup.rs). The version is pinned in `rust-toolchain.toml` and installed by itself. On
Windows, you also need the Visual Studio Build Tools with the "Desktop development with C++" workload. The sound
codec (libopus) is compiled by [CMake](https://cmake.org): on the PATH (on Windows, `winget install Kitware.CMake`; the
one inside the Build Tools also works, through the `CMAKE` variable). On Linux, also `libasound2-dev` (ALSA).

```sh
cargo run -p hext              # the interface
cargo run -p hext -- --help    # the command line
cargo run -p hext-server       # the server, at http://127.0.0.1:8080 (test: --server ws://127.0.0.1:8080)
cargo test --workspace         # tests (including an end-to-end host → viewer one)
```

Build variables, all optional:

| Variable | What it sets | Without it |
|---|---|---|
| `HEXT_SERVER` | Signaling server | `wss://signal.hext.com.br` |
| `HEXT_RELAY` | Relay | `https://relay.hext.com.br`; with another `HEXT_SERVER`, none (direct connections only) |
| `HEXT_RELEASES` | The releases page, where the app looks for updates | this repository's Releases |
| `HEXT_UPDATE_KEY` | Public key that signs the updates | no automatic updates |

## Layout

| Folder | Contents |
|---|---|
| `apps/hext` | The app: interface (Tauri), `host`, `view` and `service` |
| `apps/hext-server` | Server: IDs, presence, signaling and accounts ([docs/hext-server.md](docs/hext-server.md)) |
| `crates/proto` | Protocol messages: handshake, video, input |
| `crates/crypto` | The computer's identity (Ed25519) and access password (SPAKE2) |
| `crates/signal` | Signaling: 9-digit ID, messages between the app and the server, WebSocket client |
| `crates/net` | Transport (`iroh`/QUIC): direct when possible, relay otherwise |
| `crates/capture` | Screen capture (DXGI on Windows, ScreenCaptureKit on macOS) and the virtual monitor |
| `crates/codec` | H.264: graphics card (Media Foundation, VideoToolbox) or OpenH264 in software |
| `crates/input` | Key map and mouse and keyboard injection (SendInput, CGEvent) |
| `crates/render` | GPU rendering (wgpu), YUV → RGB in the shader |
| `crates/audio` | Sound: capture of what the computer plays (WASAPI, ScreenCaptureKit), Opus, playback (cpal) |
| `crates/session` | Host and viewer pipelines |
| `crates/metrics` | Latency statistics |
| `deploy` | Server: systemd script, Caddy, relay and Docker |
| `installer` | Installers (WiX on Windows, `.dmg` on macOS, Linux packages) |

## Contributing

Suggestions and bug reports are welcome in [Issues](https://github.com/alexpacheli/hext-remote/issues). To send
a change, open a pull request. CI runs on all three systems:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked
cargo test --workspace --locked
```

By contributing, you agree to license your contribution under the project's license.

## License

Hext Remote is free software, under the [GNU Affero General Public License v3.0](LICENSE) (`AGPL-3.0-only`).
Whoever distributes a modified program, or offers a modified server over a network, must publish that version's
source code under the same license.

## Privacy policy

This program sends data only where its features need it:

- the Hext signaling server: the device's ID, public key, name and version, to find the other computer;
- with an account (optional): the e-mail and the sign-in data, the account's signed list of devices and
  computers and, for a computer bound to the account, its access log (who connected and when), shown on
  the account's Activity screen. Verification codes are sent to that e-mail through an e-mail provider;
- the relay: the session itself, end-to-end encrypted, when a direct connection is not possible (the relay
  cannot see its content);
- GitHub: update checks and downloads.

The screen, the keyboard, the files and the clipboard of a session go only to the other computer, end-to-end
encrypted. Nothing is sold or shared with third parties, and there is no advertising or analytics. Accounts
not confirmed by e-mail are deleted within 24 hours, and an account can be deleted at any time in the app
(Conta → Excluir a conta).

Nobody connects to a computer without the user's acceptance on screen, an unattended password the computer's
owner set, or a device of an account the owner bound. Every session is shown on screen while it lasts, and
the installed app keeps a local log of them.
