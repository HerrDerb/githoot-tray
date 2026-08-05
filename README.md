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

## Tray menu

| Item | Does |
|---|---|
| **Open GitHub Notifications** | Opens your notifications, then re-checks a few seconds later |
| **Open Requested Reviews** | Opens exactly the PRs the red dot is counting, newest first |
| **Set up review dot…** | One-click setup for the red dot (see below) |
| **Quit** | Exits |

The review list URL is generated from the same query the dot counts, so the page can never
disagree with the icon.

---

## Optional: red dot for pending PR reviews

A red dot appears in the upper-right of the tray icon when at least one open PR is awaiting your
review (Dependabot and Renovate PRs excluded). **This is off until you configure a credential**,
and the app works exactly as normal without it.

### Quickest path: use the menu

Click **Set up review dot…** in the tray menu. It creates `~/.github-trayicon/review_token.txt`
(readable only by you), fills it with step-by-step instructions, and opens it in your editor.
Paste a token over the placeholder line, save, and the dot starts working within a minute —
**no restart needed**. To turn the dot back off, delete the file's contents.

The rest of this section is the same thing done by hand.

It needs a *second* credential, deliberately. `GET /notifications` only accepts classic
OAuth-app tokens with the `notifications` scope, so that token cannot be narrowed — and the only
classic scope that would let it search private repos is `repo`, which also grants **write** access
to every repository you can reach. Rather than escalate the existing token that far, the review
search gets its own narrow, read-only credential.

Pick whichever you can actually obtain:

### Option A — GitHub App (no manual rotation, ever)

1. [Create a GitHub App](https://github.com/settings/apps/new). Set **Permissions → Repository →
   Pull requests: Read-only**, enable **Device flow**, and leave the webhook off.
2. Install it on the repos or org whose reviews should count.
3. Put its **Client ID** in `~/.github-trayicon/review_client_id.txt`.

On next start the app opens a browser prompt once. After that it renews itself with a refresh
token — you never touch it again.

### Option B — fine-grained personal access token

1. Create a [fine-grained token](https://github.com/settings/personal-access-tokens/new) with
   **Pull requests: Read-only** on the relevant repos.
2. Put it in `~/.github-trayicon/review_token.txt`.

Simpler, but you must replace it whenever it expires. Note that an organisation can enforce a
maximum token lifetime, which may prevent you choosing "no expiration".

If both files exist, the token in `review_token.txt` wins.

---

## Troubleshooting

The icon's tooltip always states what the app currently believes, including *why* it is unsure.
A full history is appended to `~/.github-trayicon/log.txt` — the only way to see errors on
Windows, where the app runs without a console. The log never contains your token.

| Tooltip says | Meaning |
|---|---|
| `no unread notifications` / `unread notifications` | Confirmed by a successful poll |
| `N PR(s) awaiting your review` | The red dot is on; N comes straight from the search result |
| `No reviews requested` | Review credential is configured and reports nothing pending |
| *(no review line at all)* | Review credential is not configured — the dot is disabled |
| `notification state unknown` / `Review state unknown` | Several polls in a row failed for that signal — that half of the icon is no longer trustworthy |
| `rate limited, waiting Ns` | GitHub asked us to slow down; the last known icon is held |
| `GitHub rejected the credential` | Renewal has been triggered automatically |

The two signals are independent: a failing review search never disturbs the blue notification
icon, and vice versa. Each line in the log names which one it is:

```
unconditional poll → notifications→fresh reviews→fresh → No/Yes (next in 60s) [...]
```

---

## Pre-built binaries

Download the latest Windows or Linux binary from the [Releases](../../releases) page.

---

## License

[The Unlicense](LICENSE) — public domain, no restrictions.
