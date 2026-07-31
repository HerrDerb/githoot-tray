# git-system-tray

A lightweight Rust system tray app that watches your GitHub notifications and changes its icon the moment something new arrives.

> **Transparency notice:** This project was largely built with the assistance of [GitHub Copilot](https://github.com/features/copilot). The architecture, bug fixes, OAuth flow, CI pipeline, and documentation were all developed through an AI-assisted session. The human author reviewed, guided, and directed the work throughout.

---

## Features

- 🔔 **Live notification badge** — icon turns blue when you have unread notifications
- 🔄 **Adaptive polling** — checks GitHub at least every 60 seconds, backing off automatically
  when GitHub asks it to via `x-poll-interval` or `retry-after`
- ⚡ **Instant refresh** — opening the notifications page from the menu re-checks a few seconds
  later, so the icon clears as soon as you have read things
- 🤔 **Honest about failures** — a network error, rate limit, or expired token holds the last
  known icon and explains itself in the tooltip, instead of silently claiming "all clear"
- 🔐 **Device Code login** — no manual token setup; authenticate via browser in one step,
  and re-authenticates by itself if the token is later revoked
- 🖱️ **Tray menu** — open GitHub Notifications in the browser or quit, right from the tray
- 🪟 **Windows** — native system tray via `tray-icon` + `winit`, no console window
- 🐧 **Linux** — native system tray via `libappindicator` + GTK 3
- 🔁 **Single instance** — launching a second copy shows a notice and exits cleanly

---

## Icons

| No notifications | Unread notifications |
|:---:|:---:|
| ![No notifications](assets/github.png) | ![Unread notifications](assets/github_blue.png) |

---

## Setup

### 1. Register a GitHub OAuth App

1. Go to [github.com/settings/developers](https://github.com/settings/developers) → **New OAuth App**
2. Set any **Homepage URL** (e.g. `http://localhost`)
3. Leave **Callback URL** blank
4. After creating, click **Enable Device Flow** in the app settings
5. Copy the **Client ID**

### 2. Build

```bash
cargo build --release
```

The binary is at `target/release/git-system-tray` (or `.exe` on Windows).

### 3. First launch

There is nothing to edit in the source. On first run the app writes
`~/.github-trayicon/client_id.txt`, opens it in your editor and waits — paste your Client ID in,
save, and confirm.

It then opens your browser and shows a device code. Enter the code on GitHub; once authorized the
token is saved to `~/.github-trayicon/access_token.txt` (owner-readable only) and you won't be
asked again unless the token is revoked.

---

## Troubleshooting

The icon's tooltip always states what the app currently believes, including *why* it is unsure.
A full history is appended to `~/.github-trayicon/log.txt` — the only way to see errors on
Windows, where the app runs without a console. The log never contains your token.

| Tooltip says | Meaning |
|---|---|
| `no unread notifications` / `unread notifications` | Confirmed by a successful poll |
| `state unknown` | Several polls in a row failed — the icon is no longer trustworthy |
| `rate limited, waiting Ns` | GitHub asked us to slow down; the last known icon is held |
| `GitHub rejected the access token` | Re-authentication has been triggered automatically |

---

## Pre-built binaries

Download the latest Windows or Linux binary from the [Releases](../../releases) page.

---

## License

[The Unlicense](LICENSE) — public domain, no restrictions.
