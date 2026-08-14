# git-system-tray

A lightweight Rust system tray app that watches your pull requests — review requests, PRs ready
to merge, PRs with changes requested — and changes its icon the moment something needs you.
GitHub notification watching is still built in, as an optional extra.

> **Transparency notice:** This project was largely built with the assistance of [GitHub Copilot](https://github.com/features/copilot). The architecture, bug fixes, OAuth flow, CI pipeline, and documentation were all developed through an AI-assisted session. The human author reviewed, guided, and directed the work throughout.

> **Status:** the shared GitHub App is registered (`src/github_app.rs`'s `CLIENT_ID`) and has been
> exercised live — Device Flow sign-in, token storage, and real search results have all been
> confirmed working end to end.

---

## Features

- 🔴 **Review requested** — red dot, upper-right, when a PR is waiting on your review
- 🟢 **Ready to merge** — green dot, lower-right, when your own PR is approved and passing checks
- 🟠 **Changes requested** — amber dot, lower-left, when a reviewer asked for changes on your PR
- 🔑 **One shared credential, nothing to register** — all three dots come from one GitHub App's
  Device Flow login. The Client ID is public and baked into the binary; you just authorize in your
  browser and, if you want private repos included, install the App on your account or org (see
  [PR status](#pr-status))
- 📋 **Device code copied for you** — the code shown during sign-in is copied to your clipboard
  automatically, so entering it on GitHub's page is a paste, not a retype
- 🔔 **Notifications, opt-in** — the blue "unread" tint is off by default; turn it on in
  `config.txt` if you want it (see [Notifications](#notifications-optional))
- 🔄 **Adaptive polling** — checks GitHub at least every 60 seconds, backing off automatically
  when GitHub asks it to via `x-poll-interval` or `retry-after`
- ⚡ **Instant refresh** — opening a list from the menu re-checks a few seconds later, so the dot
  clears as soon as you have acted on it
- 🤔 **Honest about failures** — a network error, rate limit, or expired token holds the last
  known icon and explains itself in the tooltip, instead of silently claiming "all clear"
- 🖱️ **Tray menu** — open exactly what each dot is counting, in the browser, right from the tray
- 🪟 **Windows** — native system tray via `tray-icon` + `winit`, no console window
- 🍎 **macOS** — native menu bar via `tray-icon` + `winit`, shipped as an `LSUIElement` app bundle
  so there is no Dock icon and no app menu
- 🐧 **Linux** — native system tray via `libappindicator` + GTK 3
- 🔁 **Single instance** — launching a second copy shows a notice and exits cleanly (Windows;
  on macOS the same is handled by macOS itself when launching the bundle)

Nothing in this app shells out to or depends on the `gh` CLI. Every credential — PR status and,
if you turn it on, notifications — is obtained directly from GitHub's own Device Flow.

---

## Icons

Four independent signals are composited onto one icon: the base glyph (dark, or blue when
notifications are on and unread), plus up to three colored dots.

| Nothing pending | Review requested | Ready to merge | Changes requested |
|:---:|:---:|:---:|:---:|
| ![No notifications](assets/github.png) | 🔴 upper-right | 🟢 lower-right | 🟠 lower-left |

The three dots never overlap and can appear in any combination — a PR of yours ready to merge and
someone else's PR waiting on your review show up together, each in its own corner.

---

## Setup

### 1. Build

```bash
cargo build --release
```

The binary is at `target/release/git-system-tray` (or `.exe` on Windows).

On macOS the bare binary works from a terminal, but it takes a Dock icon and an app menu, because
`LSUIElement` can only be set in an app bundle. To get the real thing:

```bash
scripts/bundle-macos.sh target/release/git-system-tray dist 1.2.0
open dist/git-system-tray.app
```

That is the same script the release workflow runs, so a local bundle and a released one are built
identically.

### 2. First launch

There is nothing to edit in the source or register yourself. On first run the app opens your
browser and shows a device code — it's already copied to your clipboard, so just paste it on
GitHub's page. PR status starts working immediately for your own account and any public repos.
To see PRs in a private org's repos too, install the App on that org (GitHub will prompt for this,
or an org owner can do it from the App's page) — see [PR status](#pr-status) for what this does
and why.

Want the notification badge too? See [Notifications (optional)](#notifications-optional) — it's
off until you turn it on.

---

## Tray menu

| Item | Shown when | Does |
|---|---|---|
| **Open GitHub Notifications** | Notifications are enabled and something is unread | Opens your notifications, then re-checks a few seconds later |
| **Open Requested Reviews (N)** | A PR is waiting on your review | Opens exactly the PRs the red dot is counting, newest first |
| **Open Ready to Merge (N)** | One of your PRs is approved and passing | Opens exactly the PRs the green dot is counting |
| **Open Changes Requested (N)** | A reviewer asked for changes on your PR | Opens exactly the PRs the amber dot is counting |
| **Quit** | Always | Exits |

Every list URL is generated from the same search query its dot counts, so a page can never
disagree with the icon. Labels carry the current count with no upper limit.

---

## PR status

Three independent dots, each backed by a GitHub Search API query against your own pull requests:

| Dot | Query |
|---|---|
| 🔴 Review requested | `is:pr review-requested:@me state:open archived:false -label:dependencies -author:app/dependabot -author:app/renovate` |
| 🟢 Ready to merge | `is:pr author:@me review:approved status:success state:open draft:false archived:false` |
| 🟠 Changes requested | `is:pr author:@me review:changes_requested state:open archived:false` |

"Ready to merge" is an approximation — GitHub's search API has no single "mergeable" qualifier.
It can read wrong for branch-protection setups that need more than one approval, name required
reviewers specifically, or require status checks the combined commit status doesn't reflect.

### Why a GitHub App

All three queries need to see pull requests in whatever repos you actually work in, including
private ones. The only *classic* OAuth scope that can do that is `repo` — which also grants
**write** access to every repository you can reach. Rather than hand out a key that broad, PR
status runs on one shared, fine-grained **GitHub App** (read-only "Pull requests" and "Metadata"
permissions) that anyone can authorize via Device Flow. Its Client ID is public by design — Device
Flow needs no client secret — so it's baked into the binary rather than something you configure.

Reach into a private organization's repos is granted by **installing** the App there, not by a
wider scope. That's GitHub's own standard third-party-access flow: an org owner approves the App
once for the whole org, and every member benefits.

If you've authorized the App but see nothing behind any dot, check whether it's actually
installed anywhere: [github.com/settings/installations](https://github.com/settings/installations)
(or the org equivalent). A confident "nothing to show" from search when the App has zero
installations would be indistinguishable from genuinely having nothing pending, so the app checks
this once at startup and turns all three dots off with an explanation instead of guessing.

### When it cannot work, it says so

| What's wrong | Tooltip says |
|---|---|
| Sign-in failed | *PR status off: sign-in failed* |
| Signed in, but the App isn't installed anywhere | *PR status off: install the GitHub App to see your PRs* |
| A specific axis's query fails after re-authentication | *PR status off: re-authentication failed* |

A dark dot with no message means you genuinely have nothing pending on that axis. That distinction
is the whole point: an unreadable answer must never be reported as a confident zero.

The credential is renewed automatically — proactively before it's due to expire (if the App has
token expiry turned on), and reactively if GitHub ever rejects it mid-run. Only if renewal itself
fails does a dot go dark with an explanation.

---

## Notifications (optional)

Off by default — this used to be the whole app; now it's an extra. Turn it on by creating
`~/.github-trayicon/config.txt` with:

```
notifications=on
```

(the file lives next to the app's other state — `access_token.txt`, `pr_token.txt`, `log.txt`).
Restart the app after changing it.

### Where the token comes from

`GET /notifications` only accepts classic OAuth-app/PAT tokens — the GitHub App above can't be
used here, so this is a separate, one-time OAuth App device flow that needs a Client ID you
register yourself:

1. Go to [github.com/settings/developers](https://github.com/settings/developers) → **New OAuth App**
2. Set any **Homepage URL** (e.g. `http://localhost`)
3. Leave **Callback URL** blank
4. After creating, click **Enable Device Flow** in the app settings
5. Copy the **Client ID**

On first run with notifications enabled, the app writes `~/.github-trayicon/client_id.txt`, opens
it in your editor and waits — paste your Client ID in, save, and confirm. It then opens your
browser and shows a device code (copied to your clipboard, same as the PR status sign-in); once
authorized the token is saved to `~/.github-trayicon/access_token.txt` (owner-readable only) and
you won't be asked again unless the token is revoked.

---

## Troubleshooting

The icon's tooltip always states what the app currently believes, including *why* it is unsure.
A full history is appended to `~/.github-trayicon/log.txt` — the only way to see errors on Windows
and on macOS, where the app runs without a console. The log never contains your token.

| Tooltip says | Meaning |
|---|---|
| `no unread notifications` / `unread notifications` | Confirmed by a successful poll (notifications only, when enabled) |
| `N PR(s) awaiting your review` / `No reviews requested` | Review-requested axis, confirmed |
| `N PR(s) ready to merge` / `Nothing ready to merge` | Ready-to-merge axis, confirmed |
| `N PR(s) with changes requested` / `No changes requested` | Changes-requested axis, confirmed |
| `PR status off: ...` | The shared PR credential couldn't supply usable data; the line names the fix |
| `... state unknown` | Several polls in a row failed for that signal — it is no longer trustworthy |
| `rate limited, waiting Ns` | GitHub asked us to slow down; the last known icon is held |
| `GitHub rejected the credential` | Renewal has been triggered automatically |

Every signal is independent: a failing search on one axis never disturbs another, or the
notification badge. Each poll cycle logs one line naming every axis it actually asked about:

```
poll → conditional notifications→fresh, ReviewRequested→fresh, ReadyToMerge→fresh, ChangesRequested→fresh → No/No/Yes/No (next in 60s) [...]
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

The icon then appears in the menu bar with no Dock icon. The menu-bar image keeps its colour
rather than using a macOS template image, because a template is drawn monochrome and that would
erase every dot along with the blue "unread" glyph.

Intel Macs are not built for; `cargo build --release` on one works if you build it yourself.

---

## License

[The Unlicense](LICENSE) — public domain, no restrictions.
