# git-system-tray

A small Rust tray app that watches your pull requests — reviews requested of you, your PRs ready to
merge, your PRs with changes requested — and changes its icon the moment one needs you. Unread GitHub
notifications are an optional extra. It keeps itself up to date.

Nothing here shells out to the `gh` CLI; every credential comes from GitHub's own Device Flow.

> **Transparency notice:** largely built with AI assistance. The human author reviewed, guided and
> directed the work throughout.

---

## Icons

Everything is composited at runtime, so `assets/` holds two files and the variants cannot drift apart.
The base glyph is dark, or blue when notifications are on and something is unread. On top of it:

| Mark | Where | Means |
|---|---|---|
| 🔴 bar | Right column, slot 1 | A PR is waiting on your review |
| 🟢 bar | Right column, slot 2 | One of your PRs is approved and passing |
| 🟠 bar | Right column, slot 3 | A reviewer asked for changes on your PR |
| 🟢 up-arrow | Top-left | A newer release is available |
| 🔴 exclamation | Right-hand side | PR status is not authorized yet |

The three PR bars stack in one column, so there is one place to look rather than three corners. A fourth
slot is laid out but unused, reserved so a future signal needs no re-tuning. Positions are fixed, so a
bar always means the same thing.

Everything combines freely except one case: the exclamation *replaces* the bars, because with no
credential there are no answers to draw. The arrow still shows beside it.

---

## Setup

```bash
cargo build --release          # -> target/release/git-system-tray
```

On macOS the bare binary takes a Dock icon and an app menu, because `LSUIElement` needs a bundle:

```bash
scripts/bundle-macos.sh target/release/git-system-tray dist 1.4.0
open dist/git-system-tray.app
```

That is the same script the release workflow runs, so local and released bundles are identical.

### First launch

Nothing to register or edit. **The tray icon appears immediately** — it never blocks on a sign-in. If PR
status has no credential yet, the icon wears a red exclamation and the menu offers **Authenticate GitHub
PR Status**; pick it when it suits you. A device code appears, already on your clipboard, so entering it
on GitHub's page is a paste.

That works straight away for your own account and public repos. For a private org's repos, the App has to
be **installed** on that org — see [PR status](#pr-status).

`config.txt` is created on first run with every setting at its default; see [Settings](#settings).

---

## Tray menu

| Item | Shown when | Does |
|---|---|---|
| **Install update: X.Y.Z** | A newer release exists | Shows what changed, then verifies, installs and restarts |
| **Authenticate GitHub PR Status** | PR status has no usable credential | Starts the sign-in flow |
| **Open GitHub Notifications** | Notifications on and something unread | Opens them, then re-checks a few seconds later |
| **Open Requested Reviews (N)** | A PR waits on your review | Opens exactly what the red bar counts, newest first |
| **Open Ready to Merge (N)** | One of yours is approved and passing | Opens exactly what the green bar counts |
| **Open Changes Requested (N)** | A reviewer asked for changes | Opens exactly what the amber bar counts |
| **Quit** | Always | Exits |

Every list URL is generated from the same query its bar counts, so a page can never disagree with the
icon. Labels carry the exact count, unbounded.

---

## Settings

`~/.github-trayicon/config.txt` is created on first run with every setting at its default. An existing
file is **never** rewritten, so your edits are safe — which also means a later version's new keys will not
appear in it, and the table below is the complete list.

| Key | Default | Does |
|---|---|---|
| `updateCheck` | `on` | Check for a newer release daily and at startup |
| `notificationIndication` | `off` | Tint the icon blue on unread notifications |
| `reviewRequested` | `on` | The red bar |
| `readyToMerge` | `on` | The green bar |
| `changesRequested` | `on` | The amber bar |

Only `off`, `false`, `0` or `no` switch something off; anything else leaves the default, so a typo cannot
silently disable a feature. Restart after editing.

Turning a PR signal off removes its bar and menu entry **and stops it being searched for**, so it costs
nothing against the rate limit. Turning all three off skips PR sign-in entirely — no network call, no
dialog. Because slot positions are fixed, disabling the middle signal leaves a gap rather than closing up.

**Renamed in 1.4.0, breaking:** `notifications` → `notificationIndication`, and `update_check` →
`updateCheck`. The old spellings are no longer read. The app logs a line naming the replacement, but
cannot honour the old key — which matters most for `update_check=off`, since left unedited it stops being
read and update checks come back on.

---

## PR status

Three independent signals, each a GitHub Search query against your own pull requests:

| Bar | Query |
|---|---|
| 🔴 | `is:pr review-requested:@me state:open archived:false -label:dependencies -author:app/dependabot -author:app/renovate` |
| 🟢 | `is:pr author:@me review:approved status:success state:open draft:false archived:false` |
| 🟠 | `is:pr author:@me review:changes_requested state:open archived:false` |

"Ready to merge" is an approximation — Search has no single "mergeable" qualifier — so it can read wrong
where branch protection needs multiple approvals, named reviewers, or checks the combined commit status
does not reflect.

### Why a GitHub App

All three queries need to see PRs in private repos. The only *classic* OAuth scope that can is `repo`,
which also grants **write** to everything you can reach. Rather than hand out a key that broad, PR status
uses one shared fine-grained **GitHub App** with read-only Pull requests and Metadata permissions. Device
Flow needs no client secret, so its Client ID is public and baked in rather than configured.

Access to a private org is granted by **installing** the App there, not by a wider scope — an org owner
approves once and every member benefits.

If you have authorized but see nothing, check whether it is installed anywhere:
[github.com/settings/installations](https://github.com/settings/installations). A confident "nothing to
show" from a zero-installation App would be indistinguishable from genuinely having nothing, so the app
checks once at startup and turns the bars off with an explanation instead of guessing.

The credential renews itself — proactively before expiry, and reactively if GitHub rejects it. Only when a
renewal needs a browser does the exclamation come back.

---

## Notifications (optional)

Off by default; this used to be the whole app. Turn it on with `notificationIndication=on`.

`GET /notifications` accepts only classic OAuth-app tokens, so this needs its own Client ID that you
register:

1. [github.com/settings/developers](https://github.com/settings/developers) → **New OAuth App**
2. Any **Homepage URL** (e.g. `http://localhost`), **Callback URL** blank
3. Create it, then click **Enable Device Flow**
4. Copy the **Client ID**

On the next start the app writes `client_id.txt`, opens it in your editor and waits for you to paste the
ID in. Then a device code, same as PR status. The token lands in `access_token.txt`, owner-readable only.

> **Known limitation:** unlike PR status, this half still runs at **startup** and blocks until you have
> finished. If that is inconvenient, leave `notificationIndication=off`.

---

## Troubleshooting

The tooltip always states what the app currently believes, including *why* it is unsure. Full history is
appended to `~/.github-trayicon/log.txt` — the only way to see errors on Windows and macOS, where there is
no console. The log never contains a token.

| Tooltip | Meaning |
|---|---|
| `N PR(s) awaiting your review` / `No reviews requested` | Confirmed by a successful search |
| `N PR(s) ready to merge` / `Nothing ready to merge` | Confirmed |
| `N PR(s) with changes requested` / `No changes requested` | Confirmed |
| `unread notifications` / `no unread notifications` | Confirmed (when enabled) |
| `PR status: not authorized yet` | No credential — use the menu entry |
| `PR status off: install the GitHub App to see your PRs` | Authorized, but installed nowhere |
| `PR status off: setup failed` | No HTTP client could be built; clicking will not help |
| `PR status off in config.txt` | All three signals switched off |
| `... state unknown` | Several polls failed; that signal is no longer trustworthy |
| `Update available: X.Y.Z` | A newer release exists |

A bar that is simply absent, with no message, means you genuinely have nothing pending. That distinction
is the whole point: an unreadable answer must never be reported as a confident zero.

Signals are independent — a failing search on one never disturbs another or the notification tint. Each
cycle logs one line naming every signal it actually asked about; a switched-off one is absent:

```
poll → conditional notifications→fresh, ReviewRequested→fresh, ChangesRequested→fresh → No/No/No/No (next in 60s) [...]
```

Polling is at least every 60s, backing off automatically when GitHub says so via `x-poll-interval` or
`retry-after`. Opening a list re-checks a few seconds later, so a bar clears as soon as you have acted.

---

## Updating

Checked daily and at startup. When a newer release exists, a green up-arrow appears top-left and an
**Install update** entry appears in the menu. Picking it shows the release notes of **every** version
between yours and the newest, then asks. Nothing is ever installed silently. On confirmation it downloads,
verifies, replaces itself and restarts. Turn it off with `updateCheck=off`.

### What is verified, and what that proves

Each release publishes `sha256sums.txt` and a detached `sha256sums.txt.minisig`. Before installing:

1. The signature on the digest file is checked against a public key **compiled into the copy already on
   your machine**.
2. The asset's SHA-256 must match its entry in that signed file.
3. It must look like the right kind of artefact, at a plausible size.
4. It is run with `--print-version` and must report the version expected.

**Only step 1 proves authenticity.** The rest catch corruption and wrong-artefact mistakes. The transport
alone is not enough: TLS trust comes from the system certificate store, so a corporate TLS-inspecting
proxy or any locally installed root CA could serve arbitrary bytes, and release assets are mutable. A
checksum fetched over that same channel would be replaced along with the asset. Only a key that was
*already installed* breaks that circularity.

Any failure refuses the update and leaves the running version untouched. The app never asks for elevation:
if it lives somewhere you do not own, it says so and stops rather than escalating to write unverified
bytes.

### If a restart is interrupted

On Linux the swap is one atomic `rename`, so the binary is never missing. On Windows and macOS it is
necessarily two renames, and a crash between them leaves `git-system-tray.exe.old` (or `.app.old`) with
nothing at the original name. The app cannot repair that — the repairer is the missing file. Rename the
`.old` back. The recovery line is logged *before* the swap starts, so it is there to find.

### Command-line flags

A **compatibility contract**: the updater in one release drives the next through these, so they cannot be
renamed without breaking updates from every release already published.

| Flag | Does |
|---|---|
| `--print-version` | Prints the version and exits |
| `--await-exit <pid>` | Waits for that process to exit before starting (Windows in effect) |

### Version numbers

The binary reports the git tag it was built from, passed in by CI as `GST_VERSION`, not `Cargo.toml`'s
version. A local build has no tag and falls back to `Cargo.toml`, so local and release builds of the same
commit can report different versions. Deliberate: a local build is not a release.

---

## Releases

Download from the [Releases](../../releases) page.

| Platform | Asset |
|---|---|
| Windows x86-64 | `git-system-tray.exe` |
| Linux x86-64 | `git-system-tray` |
| macOS Apple Silicon | `git-system-tray-macos-aarch64.zip` |
| All | `sha256sums.txt`, `sha256sums.txt.minisig` |

Only those three are built. On anything else — Arm Linux, an Intel Mac — the updater reports that there is
no build for your platform rather than downloading something that will not run. `cargo build --release`
works there if you build it yourself.

### macOS

The bundle is ad-hoc signed, not notarized (that needs a paid Apple account), so Gatekeeper quarantines a
download. Clear it once:

```bash
unzip git-system-tray-macos-aarch64.zip
xattr -dr com.apple.quarantine git-system-tray.app
open git-system-tray.app
```

The icon then appears in the menu bar with no Dock icon. It keeps its colour rather than using a macOS
template image, which is drawn monochrome and would erase every bar along with the blue tint.

### Immutability

Published releases are frozen: assets and the Git tag cannot be changed or deleted, and GitHub generates a
signed attestation. So the workflow creates a **draft**, attaches everything, and publishes last. A
release therefore **cannot be re-cut** — re-running against a published tag is refused rather than
half-succeeding and leaving it without its signed manifest. Fix forward with a new patch tag.

Attestations do not replace the minisign signature: an attestation is verified against GitHub over the
network, while the updater verifies offline against a compiled-in key. That offline property is the point.

### Signing keys, for maintainers

Already set up; this is for rotating. `rsign2` rather than `minisign` itself — no `sudo` needed, and one
tool for both generating and signing removes any question about a passwordless key round-tripping.

```bash
cargo install rsign2
rsign generate -W -p minisign.pub -s minisign.key
gh secret set MINISIGN_SECRET_KEY < minisign.key    # piped, so it is never printed
```

`-W` makes it passwordless: it lives in an Actions secret, which is the real protection. `-W` is needed on
**signing** too, or `rsign` prompts and fails on a runner with no terminal.

Then copy the public key line from `minisign.pub` — the second line, not the `untrusted comment:` one —
into `PUBLIC_KEY` in `src/update.rs` and the verify step in `release.yml`. `minisign.pub` is committed;
`minisign.key` is gitignored and must stay so.

Until that is done the constant holds a placeholder and every install is refused with "this build has no
update signing key compiled in" — visible rather than silent. `the_compiled_in_key_parses` catches a bad
paste at test time.

Rotating means shipping a release signed with the *old* key that carries the *new* one, since users can
only verify with the key they already have. The signature tests use a throwaway fixture key, so they
survive a rotation.

---

## Platforms

| | |
|---|---|
| 🐧 Linux | `libappindicator` + GTK 3 |
| 🪟 Windows | `tray-icon` + `winit`, no console window |
| 🍎 macOS | `tray-icon` + `winit`, `LSUIElement` bundle, no Dock icon |

Launching a second copy shows a notice and exits (Windows; macOS handles it itself for bundles).

---

## License

[The Unlicense](LICENSE) — public domain, no restrictions.
