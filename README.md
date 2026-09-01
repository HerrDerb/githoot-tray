# GitHoot Tray

A small Rust tray app that watches your pull requests — reviews requested of you, your PRs ready to
merge, your PRs with changes requested — and changes its icon the moment one needs you. Unread GitHub
notifications are an optional extra. It keeps itself up to date.

Nothing here shells out to the `gh` CLI; every credential comes from GitHub's own Device Flow.

> **Transparency notice:** largely built with AI assistance — specifically Claude (Anthropic), driven
> through Claude Code, which did the bulk of the hauling: most of the code, the tests, the release
> workflow and this README. The human author set the goals, reviewed and directed the work throughout,
> and is accountable for what ships. Roughly half the commits carry a `Co-Authored-By: Claude` trailer,
> so `git log` shows where that help actually landed.

---

## Icons

Everything is composited at runtime, so `assets/` holds two image files and the variants cannot drift
apart. (A third file lives there, `hoot.mp3` — the notification sound, not an icon, and the only
bundled file that is not this project's own work: see [NOTICE](NOTICE).)

The base glyph is an owl, drawn for this project: the thing that sits still and watches so you do
not have to. It used to be GitHub's Invertocat, which is their trademark and not this app's to wear
as an application icon. The glyph carries no state, so replacing it cost nothing but the drawing.

It is dark (<img src="docs/icons/tray.png" alt="base icon" height="20" valign="middle">), or
blue when notifications are on and something is unread
(<img src="docs/icons/tray_blue.png" alt="blue base icon" height="20" valign="middle">). On top of it:

| Icon | Mark | Means |
|:---:|---|---|
| <img src="docs/icons/tray_review.png" alt="review bar" height="28"> | red bar | A PR is waiting on your review |
| <img src="docs/icons/tray_merge.png" alt="merge bar" height="28"> | green bar | One of your PRs is approved and its checks pass |
| <img src="docs/icons/tray_changes.png" alt="changes bar" height="28"> | amber bar | A reviewer asked for changes on your PR |
| <img src="docs/icons/tray_update.png" alt="update arrow" height="28"> | green up-arrow | A newer release is available |
| <img src="docs/icons/tray_alert.png" alt="exclamation" height="28"> | red exclamation | Something needs saying, see below |

The icons above are the real ones the app draws, not mock-ups.

The three PR bars stack in one column, so there is one place to look rather than three corners, and they
fill nearly the whole height — 92 of 96 pixels. Positions are fixed, so a bar always means the same thing
and switching one off leaves a gap rather than closing up.

There is deliberately no spare slot. One was reserved for a while for a signal that never arrived, and
holding it open cost a quarter of the icon's height, taken straight out of the bars that do exist. Adding a
fourth signal later means re-tuning the geometry, which is the right way round.

**Everything combines.** All six marks are independent, so any state can be drawn — bars, arrow and
exclamation together if that is the truth. The exclamation used to *replace* the bars, because it sat on top
of their column; moving it to the left is what freed the counts to stay visible while something is wrong.

A few real composites:

| Icon | State |
|:---:|---|
| <img src="docs/icons/tray_review_merge_changes.png" alt="all three bars" height="28"> | All three PR bars lit |
| <img src="docs/icons/tray_review_changes_alert.png" alt="bars beside the exclamation" height="28"> | Counts still visible beside the exclamation |
| <img src="docs/icons/tray_blue_review_merge_changes_update_alert.png" alt="every mark at once" height="28"> | Every mark at once: blue base, three bars, arrow and exclamation |

The arrow sits in the top middle and is drawn last, so it overlaps whatever is beneath it. Each bar's
**centre** survives that: clip a bar's end and it still reads as a bar, reach its middle and it stops being
one. That is asserted, not hoped for.

**The exclamation has three causes, and only the tooltip and menu say which:**

| Cause | What the menu offers |
|---|---|
| PR status is not authorized | **Authenticate GitHub PR Status** |
| GitHub reports an incident | **GitHub is githubing again, check status** |
| A poll failed, so a signal is unknown | nothing to click — the tooltip names the axis |

One mark for three causes is a deliberate trade: a second mark would need somewhere to live on an icon that
has no free space left. The causes stay separate internally, so an incident never offers a sign-in and a
missing credential never points at the status page.

"A poll failed" means exactly that — asked and not answered. A freshly started app has answered nothing yet
and shows no mark, and a single transient blip holds the last known value rather than raising one.

---

## Setup

```bash
cargo build --release          # -> target/release/githoot-tray
```

On macOS the bare binary takes a Dock icon and an app menu, because `LSUIElement` needs a bundle:

```bash
scripts/bundle-macos.sh target/release/githoot-tray dist 1.6.0
open dist/githoot-tray.app
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

### Windows Defender may flag the binary

It has happened: `Trojan:Win32/Bearfoos.B!ml`. The `!ml` suffix means a machine-learning verdict
rather than a signature match, and this binary fits the shape those models are trained to distrust —
it is **not code-signed**, so it carries no publisher reputation, it runs with no console and no
window, and its updater downloads a new `.exe`, stages it beside the running one and renames it into
place. Every one of those is also how a dropper behaves.

The releases now embed a `VERSIONINFO` resource, an application manifest and an icon, so the binary at
least identifies itself in Explorer's Properties dialog and to anything that reads publisher metadata.
That reduces the odds; it does not eliminate them, and it is not a claim that the binary is safe.

**What to do instead of taking anyone's word for it.** Verify the download against the signed manifest
— that chain is described under [What is verified](#what-is-verified-and-what-that-proves), and it is the
same check the updater runs before installing anything:

```bash
# Compare your download against the signed digest list for its tag.
curl -LO https://github.com/HerrDerb/githoot-tray/releases/download/vX.Y.Z/sha256sums.txt
curl -LO https://github.com/HerrDerb/githoot-tray/releases/download/vX.Y.Z/sha256sums.txt.minisig
minisign -Vm sha256sums.txt \
  -P 'RWSYrhd3sxiQUDZtxm8c+p0iRdj+z+fGQKdLq62ojrmfii2OjCG8PX8D'  # authenticity
sha256sum -c sha256sums.txt --ignore-missing                    # integrity
```

If both pass, the file is the one CI built and signed. Whether you then allow it to run is your call,
and a Defender exclusion is a decision to make deliberately rather than a step in an install guide.

---

## Tray menu

| Item | Shown when | Does |
|---|---|---|
| **Install update: X.Y.Z** | A newer release exists | Shows what changed, then verifies, installs and restarts |
| **GitHub is githubing again, check status** | GitHub reports an incident | Opens [githubstatus.com](https://www.githubstatus.com) |
| *— separator —* | Something above **and** below it | |
| **Authenticate GitHub PR Status** | PR status has no usable credential | Starts the sign-in flow |
| **Open GitHub Notifications** | Notifications on and something unread | Opens them, then re-checks a few seconds later |
| **Open Requested Reviews (N)** | A PR waits on your review | Opens exactly what the red bar counts, newest first |
| **Open Approved PRs (N)** | One of yours has been approved | Opens exactly what the green bar counts |
| **Open Changes Requested (N)** | A reviewer asked for changes and it is still on you | Opens GitHub's changes-requested list, which can show more than the bar counts (see below) |
| **Open PR inbox** | None of the three entries above is shown | Opens [GitHub's own PR inbox](https://github.com/pulls/inbox) |
| *— separator —* | Always | |
| **Open Settings** | Always | Opens `config.txt`, then offers a restart once your edits settle (see below) |
| **Quit** | Always | Exits |

The update and GitHub-status entries come first: they are about the app and the service rather than about
your pull requests, and when the icon is wearing a mark two of its three causes are explained up there.

The upper separator appears only when there is something above it. A rule with nothing on one side is a
stray line, which reads as a rendering fault rather than a grouping — and below it there is now always
something, because **Open PR inbox** stands in whenever the three PR entries have nothing to open.

That fallback exists because every other PR entry hides itself when its count is zero: an entry opening an
empty list is a dead end. On a quiet day that left no way into your pull requests at all. Being a plain
URL rather than a search of ours, it also needs no credential, so it stays while the app is waiting to be
authorized and the three entries that do need one are hidden.

The requested-reviews URL is generated from the same query its bar counts, so that page cannot disagree
with the icon. Labels carry the exact count, unbounded.

**The other two bars apply a filter GitHub's web search cannot express, so their pages can list more
than the bar counts — the bar is the accurate one.** Ready to merge counts approved PRs whose checks
are green (see below); the page it opens shows every approved PR, including ones with red or pending
checks. Changes requested counts PRs where a reviewer asked for changes *and no re-review is pending
from that reviewer* — so once you push fixes and re-request the review, the bar goes dark — but its
page still shows every PR with changes requested, including ones you have already handed back.

---

## Settings

`~/.githoot-tray/config.txt` is created on first run with every setting at its default. An existing
file is **never** rewritten, so your edits are safe — which also means a later version's new keys will not
appear in it, and the table below is the complete list.

> **Upgrading from `git-system-tray`?** The app was renamed, and with it the asset names, the binary
> and this directory — settings and log used to live in `~/.github-trayicon/`. Nothing is migrated
> automatically, so copy your old `config.txt` across if you want to keep it. Install the new release by
> hand as well: a pre-rename copy still *sees* the new version and offers it, then fails the integrity
> check with "the signed sums file has no entry for git-system-tray", because the asset it wants is no
> longer published.

| Key | Default | Does |
|---|---|---|
| `updateCheck` | `on` | Check for a newer release daily and at startup |
| `notificationIndication` | `off` | Tint the icon blue on unread notifications |
| `reviewRequested` | `on` | The red bar |
| `readyToMerge` | `on` | The green bar (approved PRs; the key keeps its old name so existing `config.txt` files still work) |
| `changesRequested` | `on` | The amber bar |
| `sound` | `on` | Play the hoot when a PR signal goes from none to some |
| `logLevel` | `error` | How much `log.txt` records: `error` logs only failures, `info` adds lifecycle detail for diagnosing |
| `statusComponents` | every component | Which parts of GitHub may raise the outage mark — see below |

Only `off`, `false`, `0` or `no` switch something off; anything else leaves the default, so a typo cannot
silently disable a feature. Two keys are not toggles: `logLevel` takes `error` or `info`, falling back to
`error`; `statusComponents` takes a comma-separated list.

### Which parts of GitHub count as an outage

GitHub's page-wide verdict is one judgement over everything it runs, and most of that is nothing to do
with pull requests. A single degraded component — Copilot, say — makes the whole page read "Partially
Degraded Service", which put a red exclamation on the tray for a service this app never touches. Cry wolf
often enough and the mark stops meaning anything.

So `statusComponents` names the parts that may raise it. A fresh `config.txt` lists every component GitHub
publishes, on one line; delete the ones you do not care about:

```
statusComponents=Git Operations, Webhooks, API Requests, Issues, Pull Requests, Actions, Packages, Pages, Copilot, Codespaces, Copilot AI Model Providers
```

- **One line, commas between.** There is no line-continuation syntax, so a wrapped list loses everything
  after the first line.
- **Names match GitHub's own, bar case and surrounding spaces.** `issues` is `Issues`; `operations` is not
  `Git Operations`, and `copilot` is not `Copilot AI Model Providers`. Partial matches are refused on
  purpose — one word would otherwise drag in a component you meant to leave out. A name matching nothing
  is named in `log.txt`, once, since its only other symptom would be a mark that never appears.
- **A component is faulty at `degraded_performance`, `partial_outage` or `major_outage`.** Scheduled
  maintenance is not, matching how the page-wide check has always treated it.
- **An empty list, or no key at all, watches the whole page** — the old behaviour, which is what every
  `config.txt` written before this key existed says. Those files are never rewritten, so they keep it.
- The tooltip and the log then name what is actually broken: `GitHub: Issues (Partial Outage)` rather
  than `GitHub: Partially Degraded Service`.

Naming components costs one extra request per five minutes' check: `components.json` rather than the
219-byte `status.json`. An unfiltered install still reads the small one.

**Restart after editing — the menu offers it.** Settings are read once at startup, so **Open Settings**
opens the file and then watches it. Once the contents change and stay unchanged for a few seconds, a dialog
offers to restart. Details worth knowing:

- It watches the **file**, not the editor. The usual handler for a text file is a DBus-activated,
  single-instance app (gedit, VS Code), so the launcher exits immediately and there is no editor process to
  wait on — a "wait until closed" would fire the moment it opened.
- Content, not timestamps, so saving an unchanged buffer does nothing, and typing an edit then undoing it
  before the prompt appears is correctly treated as no change.
- **Asked once.** Declining means the edits apply at the next start; it will not ask again.
- The watch is armed by the menu click and gives up quietly after 15 minutes, so editing the file by hand
  later never restarts the app unexpectedly.
- With no display server to show the dialog, nothing restarts and the edits apply on the next start.
- `$VISUAL`/`$EDITOR` is preferred over the desktop association, skipping terminal-only editors (vim, nano
  and friends) since a tray app has no terminal to host them.

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
| 🔴 | `is:pr review-requested:@me state:open draft:false archived:false -label:dependencies -author:app/dependabot -author:app/renovate` |
| 🟢 | `is:pr author:@me review:approved state:open draft:false archived:false` |
| 🟠 | `is:pr author:@me review:changes_requested state:open archived:false` |

**The green bar means approved, not mergeable.** It used to mean both: through 1.10.0 the bar counted an
approved pull request only when its GraphQL `statusCheckRollup` was `SUCCESS`, so one red or still-running
check darkened it. That hid the thing worth being told — somebody approved your work — and it also
disagreed with the entry beside it, since **Open Ready to Merge** opened the query above, red checks
included. Approval is the whole signal now. Whether CI is green is a question you answer on the page the
entry opens.

Dropping the gate makes this axis a plain `total_count` read again: it asks for one search hit instead of
a hundred, and its count is exact past 100 approved PRs, which the GraphQL page could not see past.

Still not checked, and never was: branch protection needing multiple approvals or named reviewers, and
merge conflicts (`mergeable` is computed lazily and reads `UNKNOWN` on a cold poll).

Two GitHub App permissions are now unused: **Checks: read** and **Commit statuses: read**, which the
rollup needed. They are still requested rather than withdrawn, because narrowing an installed App's
permissions makes every installation owner re-approve — a real cost to pay for tidiness.

**Changes requested does one thing more than its query.** Re-requesting a review does not dismiss the
reviewer's earlier verdict, so `review:changes_requested` keeps matching a pull request you have already
handed back, and the bar used to stay lit until the reviewer replied. That query is now sent through
GraphQL, which also returns each pull request's pending review requests, and a hit is counted only when
**no re-review is pending from a reviewer who requested changes**. Adding a *different* reviewer does not
clear it — the original objection still stands. A pending *team* request does not clear it either, since a
team has no login to match against the blocker; erring that way keeps the bar lit rather than hiding work.

Caps, which undercount rather than overcount: the first 100 matching pull requests, and the first 20
reviews and 20 pending requests within each.

### Why a GitHub App

All three queries need to see PRs in private repos. The only *classic* OAuth scope that can is `repo`,
which also grants **write** to everything you can reach. Rather than hand out a key that broad, PR status
uses one shared fine-grained **GitHub App** with read-only Pull requests, Metadata, Checks and Commit
statuses permissions. The last two are now unused — the green bar no longer reads check health — and are
kept only to spare every installation owner a re-approval; see the note above. **Contents is deliberately
not requested**, which is why no query may traverse a commit object. Device Flow needs no client secret,
so its Client ID is public and baked in rather than configured.

Access to a private org is granted by **installing** the App there, not by a wider scope — an org owner
approves once and every member benefits.

If you have authorized but see nothing, check whether it is installed anywhere:
[github.com/settings/installations](https://github.com/settings/installations). A confident "nothing to
show" from a zero-installation App would be indistinguishable from genuinely having nothing, so the app
checks once at startup and turns the bars off with an explanation instead of guessing.

The credential renews itself — proactively before expiry, and reactively if GitHub rejects it. Only when a
renewal needs a browser does the exclamation come back.

---

## The hoot

When a PR signal goes from **none to some**, the app plays a short hoot. Each of the three axes hoots for
itself: reviews requested of you, your PRs that were approved, your PRs with changes requested.

Only that one edge. Going from one PR to four does not hoot again — you already know, and the count is in
the tooltip and the menu. It re-arms once the axis has genuinely gone back to zero.

Launching into a queue that already has PRs in it hoots too, once. Strictly that is not a 0-to-1 edge —
the app knew nothing before it asked — but it is the moment you want telling, and staying silent there
would mean the hoot only ever worked for people who left the app running.

One case stays deliberately silent: an axis recovering from a run of failed polls. It looks identical to
a launch from the inside — "we do not know" either way — but that axis has already had its say, and the
PRs it comes back with are a number you have seen. Hooting there would turn every network blip into a
notification.

The clip is embedded in the binary, unpacked once per run into the system temp directory, and played by
whatever the platform already has: `winmm` (MCI) on Windows, `afplay` on macOS, and the first of `mpv`,
`ffplay`, `mpg123`, `gst-play-1.0` or `cvlc` that is installed on Linux. So no audio crate joins the
dependency tree for one short sound, and a Linux box with none of those players stays silent with a line
in the log rather than failing. Playback runs on its own thread and never delays a poll; hoots that
overlap are dropped rather than layered.

Set `sound=off` in `config.txt` to silence it. That switches off the sound and nothing else — the icon,
the tooltip and the menu counts behave identically either way, so silence costs no information. There is
no volume setting; the system mixer is the only control over how loud it is.

The clip is a sound effect by [elevenlabs.io](https://elevenlabs.io/sound-effects/free), used under
their free plan: attribution required, non-commercial use only. It is the one file in this repository
the public-domain dedication does not cover — see [NOTICE](NOTICE) before using this app for anything
commercial.

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
appended to `~/.githoot-tray/log.txt` — the only way to see errors on Windows and macOS, where there is
no console. The log never contains a token.

That directory holds the four files worth opening — `config.txt`, `log.txt` and the two credentials — plus
an `icons/` subdirectory with the 64 generated PNGs. Linux only: Windows and macOS build their icons in
memory and never write one to disk.

| Tooltip | Meaning |
|---|---|
| `N PR(s) awaiting your review` / `No reviews requested` | Confirmed by a successful search |
| `N PR(s) approved` / `No approvals yet` | Confirmed |
| `N PR(s) with changes requested` / `No changes requested` | Confirmed |
| `unread notifications` / `no unread notifications` | Confirmed (when enabled) |
| `PR status: not authorized yet` | No credential — use the menu entry |
| `PR status off: install the GitHub App to see your PRs` | Authorized, but installed nowhere |
| `PR status off: setup failed` | No HTTP client could be built; clicking will not help |
| `PR status off in config.txt` | All three signals switched off |
| `... state unknown` | Several polls failed; that signal is no longer trustworthy, and the exclamation is up |
| `GitHub: <description>` | GitHub reports an incident; quotes its own wording, and comes first in the tooltip. With `statusComponents` set, the description names the components: `Issues (Partial Outage)` |
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
necessarily two renames, and a crash between them leaves `githoot-tray.exe.old` (or `.app.old`) with
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

The binary reports the git tag it was built from, passed in by CI as `GHT_VERSION`, not `Cargo.toml`'s
version. A local build has no tag and falls back to `Cargo.toml`, so local and release builds of the same
commit can report different versions. Deliberate: a local build is not a release.

---

## Releases

Download from the [Releases](../../releases) page.

| Platform | Asset |
|---|---|
| Windows x86-64 | `githoot-tray.exe` |
| Linux x86-64 | `githoot-tray` |
| macOS Apple Silicon | `githoot-tray-macos-aarch64.zip` |
| All | `sha256sums.txt`, `sha256sums.txt.minisig` |

Only those three are built. On anything else — Arm Linux, an Intel Mac — the updater reports that there is
no build for your platform rather than downloading something that will not run. `cargo build --release`
works there if you build it yourself.

### macOS

The bundle is ad-hoc signed, not notarized (that needs a paid Apple account), so Gatekeeper quarantines a
download. Clear it once:

```bash
unzip githoot-tray-macos-aarch64.zip
xattr -dr com.apple.quarantine githoot-tray.app
open githoot-tray.app
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

[The Unlicense](LICENSE) — public domain, no restrictions. That covers the code and the icons, which
were drawn for this project.

One exception, listed in [NOTICE](NOTICE): `assets/hoot.mp3` is a sound effect by
[elevenlabs.io](https://elevenlabs.io/sound-effects/free), used under their free plan, which requires
attribution and permits non-commercial use only. This project is non-commercial, so that is the basis
it is used on here — but a commercial use of GitHoot Tray needs its own licence for that clip, or a
replacement file. Nothing in the code cares which clip is at that path.
