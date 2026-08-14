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

- 🔴 **Review requested** — red bar, top of the right-hand column, when a PR is waiting on your review
- 🟢 **Ready to merge** — green bar, when your own PR is approved and passing checks
- 🟠 **Changes requested** — amber bar, when a reviewer asked for changes on your PR
- 🟣 **Self-updating** — checks for newer releases daily, shows an up-arrow and what changed, and
  installs only when you say so. Every download is verified against a signature whose public key is
  compiled into the copy you already have (see [Updating](#updating))
- 🔑 **One shared credential, nothing to register** — all three bars come from one GitHub App's
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

Everything is composited onto one icon at runtime, so `assets/` holds just two files and the variants
can never drift apart.

The base glyph is dark, or blue when notifications are on and something is unread. On top of it:

| Mark | Where | Means |
|---|---|---|
| 🔴 rounded bar | Right column, slot 1 | A PR is waiting on your review |
| 🟢 rounded bar | Right column, slot 2 | One of your PRs is approved and passing |
| 🟠 rounded bar | Right column, slot 3 | A reviewer asked for changes on your PR |
| 🟣 up-arrow | Top-left | A newer release is available |
| 🔴 exclamation | Right-hand side | PR status is not authorized yet |

The three PR bars stack in one column down the right-hand side, so there is one place to look rather
than three corners. A fourth slot below them is laid out but unused, reserved so a future signal has
somewhere to go without re-tuning the others.

The bars, the arrow and the exclamation are independent and combine freely, with one exception: the
exclamation *replaces* the bars, because with no credential the app has no PR answers to draw. The
arrow still shows alongside it — an update is worth knowing about either way, and it sits in the
opposite corner.

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
scripts/bundle-macos.sh target/release/git-system-tray dist 1.3.0
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
| **Install update: X.Y.Z** | A newer release exists | Shows what changed, then downloads, verifies, installs and restarts |
| **Authenticate GitHub PR Status** | PR status has no usable credential | Starts the sign-in flow |
| **Quit** | Always | Exits |

Every list URL is generated from the same search query its dot counts, so a page can never
disagree with the icon. Labels carry the current count with no upper limit.

---

## PR status

Three independent indicators, each backed by a GitHub Search API query against your own pull requests:

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

If you've authorized the App but see nothing behind any bar, check whether it's actually
installed anywhere: [github.com/settings/installations](https://github.com/settings/installations)
(or the org equivalent). A confident "nothing to show" from search when the App has zero
installations would be indistinguishable from genuinely having nothing pending, so the app checks
this once at startup and turns all three bars off with an explanation instead of guessing.

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

## Updating

The app checks GitHub for a newer release once a day, and immediately at startup. When it finds one, a
violet up-arrow appears in the top-left of the tray icon and an **Install update** entry appears in the
menu carrying the new version number.

Picking it shows what changed — the release notes of **every** version between the one installed and the
newest, not just the latest — and asks before doing anything. Nothing is ever installed silently. On
confirmation the app downloads the release asset for your platform, verifies it, replaces itself and
restarts.

Turn the check off with `update_check=off` in `config.txt`. It is on by default, unlike notifications.

### What is verified, and what that does and does not prove

Each release publishes `sha256sums.txt` and a detached `sha256sums.txt.minisig`. Before anything is
installed:

1. The signature on the digest file is checked against a public key **compiled into the copy of the app
   already on your machine**.
2. The downloaded asset's SHA-256 must match its entry in that signed file.
3. The file must look like the right kind of artefact, and be a plausible size.
4. The downloaded binary is run with `--print-version` and must report the version expected.

Step 1 is the only one of these that proves *authenticity*. The rest catch corruption, truncation and
wrong-artefact mistakes. This matters because the transport alone is not enough: TLS trust comes from
the system certificate store, so a corporate TLS-inspection proxy or any locally installed root CA could
serve arbitrary bytes, and GitHub release assets are mutable. A checksum file fetched over that same
channel would be replaced along with the asset. Only a key that was already installed breaks that
circularity.

Any failure at any step refuses the update and leaves the running version untouched.

The app never asks for elevation. If it is installed somewhere you do not own, it says so and stops
rather than escalating to write bytes nobody has verified.

### If a restart is interrupted

On Linux the swap is a single atomic `rename`, so there is no window where the binary is missing.

On Windows and macOS it is necessarily two renames, and a crash or power loss between them leaves
`git-system-tray.exe.old` (or `git-system-tray.app.old`) with no file at the original name. The app
cannot repair that, because the thing that would do the repairing is the file that is missing. Rename the
`.old` back and you are on the previous version again. The recovery line is written to
`~/.github-trayicon/log.txt` **before** the swap starts, so it is there to find.

### Command-line flags

Two flags exist for the updater rather than for people, and are a **compatibility contract**: the updater
in one release drives the next one through them, so they cannot be renamed without breaking updates from
every release already published.

| Flag | Does |
|---|---|
| `--print-version` | Prints the version and exits |
| `--await-exit <pid>` | Waits for that process to exit before starting (Windows only in effect) |

### Maintainer setup: signing keys

Releases must be signed or the updater will refuse them. This is already set up; what follows is for
rotating the key or starting over.

`rsign2` is used rather than `minisign` itself — it needs no `sudo` to install, and using one tool for
both generating and signing removes any question about whether a passwordless secret key round-trips
between the two:

```bash
cargo install rsign2
rsign generate -W -p minisign.pub -s minisign.key
```

`-W` makes the key passwordless. That is deliberate: it lives in an Actions secret, which is the actual
protection, and a password would only add a second secret to manage. `-W` is needed on **signing** too,
or `rsign` tries to prompt and fails on a runner with no terminal.

Then:

1. `gh secret set MINISIGN_SECRET_KEY < minisign.key` — piping the file means the key is never printed.
2. Copy the **public** key line from `minisign.pub` (the second line, not the `untrusted comment:` one)
   into `PUBLIC_KEY` in `src/update.rs`, and into the verify step in `release.yml`.
3. `minisign.pub` is committed; `minisign.key` is gitignored and must stay that way.

Until step 2 is done the constant holds a placeholder and the app refuses every install with "this build
has no update signing key compiled in". The check and the arrow still work, so the gap is visible rather
than silent. `the_compiled_in_key_parses` catches a bad paste at test time.

Rotating the key means shipping a release signed with the *old* key that carries the *new* one, since
users can only verify with the key they already have. That is the unavoidable cost of the guarantee. The
signature tests use a throwaway fixture key, so they survive a rotation without being regenerated.

### Version numbers

The binary reports the git tag it was built from, not `Cargo.toml`'s version: CI passes the tag in through
`GST_VERSION`. A locally built binary has no tag and falls back to `Cargo.toml`, so a local build and a
release build of the same commit can report different versions. That is deliberate — a local build is not
a release.

---

## Pre-built binaries

Download the latest build for your platform from the [Releases](../../releases) page.

| Platform | Asset |
|---|---|
| Windows x86-64 | `git-system-tray.exe` |
| Linux x86-64 | `git-system-tray` |
| macOS Apple Silicon | `git-system-tray-macos-aarch64.zip` |
| All | `sha256sums.txt`, `sha256sums.txt.minisig` |

Only those three platforms are built for, and the self-updater knows it: on anything else (an Arm Linux
box, an Intel Mac) it reports that there is no published build for your platform rather than downloading
something that will not run.

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
