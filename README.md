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
- 🔴 **Review dot** — a red dot when PRs await your review, with the exact count in the tooltip and
  the tray menu, using the token [`gh`](https://cli.github.com) already holds, so there is no second
  credential to create or store (see below)
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
| **Open Requested Reviews (N)** | Opens exactly the PRs the red dot is counting, newest first. The label carries the current count, with no upper limit |
| **Quit** | Exits |

The review list URL is generated from the same query the dot counts, so the page can never
disagree with the icon.

---

## Red dot for pending PR reviews

A red dot appears in the upper-right of the tray icon when at least one open PR is awaiting your
review (Dependabot and Renovate PRs excluded).

The dot itself stays a dot on purpose. At 100% scaling the shell asks for a 16x16 icon, which leaves
about ten pixels for a digit even if the badge swallows the octocat; rendering the count there was
tried, and it was both unreadable at small sizes and ugly at large ones. The number goes where there
is room for it instead: the tooltip, and the **Open Requested Reviews (N)** menu item. Neither has an
upper limit, so 147 shows as 147.

### Why it uses the GitHub CLI

`GET /notifications` only accepts classic OAuth-app tokens with the `notifications` scope, so that
token cannot search pull requests. The only classic scope that can search private repos is `repo`,
which also grants **write** access to every repository you can reach. Rather than escalate the
notification token that far, or make you create and store a second one, the search borrows the token
[`gh`](https://cli.github.com) already holds in your OS keyring. This app never writes it to disk.

Install `gh`, log in, and make sure the login carries the `repo` scope:

```bash
gh auth login
gh auth status                    # check whether `repo` is already listed
gh auth refresh --scopes repo     # only if it is not
```

Restart the tray icon after changing your `gh` login. Set `GH_HOST` for GitHub Enterprise; `gh`
reads the same variable.

`gh` is called exactly twice, at startup and after GitHub rejects the token, never on the polling
path.

### When it cannot work, it says so

The notification half has its own credential and never depends on `gh`, so none of these stop the
app. Each one produces a dialog at startup, a line in the log, and a line in the tooltip:

| What is wrong | Tooltip says |
|---|---|
| `gh` is not on `PATH` | *Review dot off: gh not installed* |
| `gh` is not logged in | *Review dot off: run gh auth login* |
| the token has no `repo` scope | *Review dot off: run gh auth refresh --scopes repo* |
| `gh` failed or took over 5s | *Review dot off: gh call failed* |

A dark dot with no message means you genuinely have nothing to review. That distinction is the
point: without the `repo` scope, GitHub's search answers `200` with `total_count: 0` rather than an
error, so a confident zero would be indistinguishable from the truth. The app refuses to report one.

Scopes are only ever read from the search credential's own responses, never from the notification
token's, since that one is scoped to `notifications` on purpose and will never carry `repo`.

If you set this up with a PAT before, `~/.github-trayicon/review_token.txt` is no longer read. The
app logs a reminder on startup, and you can delete the file: it holds a credential you no longer
need.

---

## Troubleshooting

The icon's tooltip always states what the app currently believes, including *why* it is unsure.
A full history is appended to `~/.github-trayicon/log.txt` — the only way to see errors on
Windows, where the app runs without a console. The log never contains your token.

| Tooltip says | Meaning |
|---|---|
| `no unread notifications` / `unread notifications` | Confirmed by a successful poll |
| `N PR(s) awaiting your review` | The red dot is on; N comes straight from the search result |
| `No reviews requested` | The search ran and reports nothing pending |
| `Review dot off: ...` | `gh` could not supply a usable credential; the line names the fix |
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
