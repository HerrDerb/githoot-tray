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
- 🔐 **No setup if you already use `gh`** — the notification token is borrowed from
  [`gh`](https://cli.github.com) when it's logged in with the `notifications` scope; otherwise a
  one-time Device Code login creates and stores one, and re-authenticates itself if it is later
  revoked
- 🔴 **Review dot** — a red dot when PRs await your review, with the exact count in the tooltip and
  the tray menu, using the token `gh` already holds, so there is no second credential to create or
  store (see below)
- 🖱️ **Tray menu** — open GitHub Notifications in the browser or quit, right from the tray
- 🪟 **Windows** — native system tray via `tray-icon` + `winit`, no console window
- 🍎 **macOS** — native menu bar via `tray-icon` + `winit`, shipped as an `LSUIElement` app bundle
  so there is no Dock icon and no app menu
- 🐧 **Linux** — native system tray via `libappindicator` + GTK 3
- 🔁 **Single instance** — launching a second copy shows a notice and exits cleanly (Windows;
  on macOS the same is handled by macOS itself when launching the bundle)

---

## Icons

| No notifications | Unread notifications |
|:---:|:---:|
| ![No notifications](assets/github.png) | ![Unread notifications](assets/github_blue.png) |

---

## Setup

### 1. (Optional) Give `gh` the `notifications` scope

If [`gh`](https://cli.github.com) is installed and logged in with the `notifications` scope, the
app borrows that token and skips the OAuth App step below entirely:

```bash
gh auth login
gh auth status                            # check whether notifications is already listed
gh auth refresh --scopes notifications    # only if it is not
```

Skip this if you would rather not widen `gh`'s scopes, or don't use `gh` at all — step 3 below
covers what happens instead.

If `gh` is not found on `PATH` at all, startup shows a dialog explaining the option above and
asking whether to continue with the OAuth App flow (step 3) or exit so `gh` can be installed
first. `gh` being installed but not logged in, or logged in without the `notifications` scope,
skips this dialog and falls straight back to step 3 — at that point `gh` is already set up for
something, so the review dot's own dialog (see [Red dot for pending PR
reviews](#red-dot-for-pending-pr-reviews)) is the one that speaks up if it needs attention.

### 2. Build

```bash
cargo build --release
```

The binary is at `target/release/git-system-tray` (or `.exe` on Windows).

On macOS the bare binary works from a terminal, but it takes a Dock icon and an app menu, because
`LSUIElement` can only be set in an app bundle. To get the real thing:

```bash
scripts/bundle-macos.sh target/release/git-system-tray dist 2.3.0
open dist/git-system-tray.app
```

That is the same script the release workflow runs, so a local bundle and a released one are built
identically.

### 3. First launch

If `gh` supplied a token in step 1, that's it — the app uses it and there is nothing further to
set up. Otherwise it falls back to a one-time OAuth App device flow:

1. Go to [github.com/settings/developers](https://github.com/settings/developers) → **New OAuth App**
2. Set any **Homepage URL** (e.g. `http://localhost`)
3. Leave **Callback URL** blank
4. After creating, click **Enable Device Flow** in the app settings
5. Copy the **Client ID**

There is nothing to edit in the source for this. On first run the app writes
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

The notification half falls back to its own OAuth App device flow when `gh` cannot supply a
`notifications`-scoped token (see [Setup](#setup)), so none of these stop the app — only the
review dot. Each one produces a dialog at startup, a line in the log, and a line in the tooltip:

| What is wrong | Tooltip says |
|---|---|
| `gh` is not on `PATH` | *Review dot off: gh not installed* |
| `gh` is not logged in | *Review dot off: run gh auth login* |
| the token has no `repo` scope | *Review dot off: run gh auth refresh --scopes repo* |
| `gh` failed or took over 5s | *Review dot off: gh call failed* |

A dark dot with no message means you genuinely have nothing to review. That distinction is the
point: without the `repo` scope, GitHub's search answers `200` with `total_count: 0` rather than an
error, so a confident zero would be indistinguishable from the truth. The app refuses to report one.

Scopes for the review dot are only ever read from the search credential's own responses, never
from whatever is backing the notification token — the two are checked independently even when
`gh` happens to be supplying both.

If you set this up with a PAT before, `~/.github-trayicon/review_token.txt` is no longer read. The
app logs a reminder on startup, and you can delete the file: it holds a credential you no longer
need.

---

## Troubleshooting

The icon's tooltip always states what the app currently believes, including *why* it is unsure.
A full history is appended to `~/.github-trayicon/log.txt` — the only way to see errors on Windows
and on macOS, where the app runs without a console. The log never contains your token.

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

Download the latest build for your platform from the [Releases](../../releases) page.

| Platform | Asset |
|---|---|
| Windows x86-64 | `git-system-tray.exe` |
| Linux x86-64 | `git-system-tray` |
| macOS Apple Silicon | `git-system-tray-macos-aarch64.zip` |

### macOS

The bundle is ad-hoc signed, not notarized — that would need a paid Apple Developer account — so
Gatekeeper quarantines it on download and refuses to open it. Clear the flag once:

```bash
unzip git-system-tray-macos-aarch64.zip
xattr -dr com.apple.quarantine git-system-tray.app
open git-system-tray.app
```

The icon then appears in the menu bar with no Dock icon. Two things to know:

- For the red review dot to work, `gh` has to be findable. Launched from Finder an app inherits a
  minimal `PATH`, so the app also looks in `/opt/homebrew/bin` and `/usr/local/bin`. A `gh`
  installed anywhere else needs to be on the `PATH` the app inherits.
- The menu-bar image keeps its colour rather than using a macOS template image, because a template
  is drawn monochrome and that would erase both the blue "unread" glyph and the red dot.

Intel Macs are not built for; `cargo build --release` on one works if you build it yourself.

---

## License

[The Unlicense](LICENSE) — public domain, no restrictions.
