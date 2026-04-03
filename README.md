# git-system-tray

A lightweight Rust system tray app that watches your GitHub notifications and changes its icon the moment something new arrives.

---

## Features

- 🔔 **Live notification badge** — icon turns blue when you have unread notifications
- 🔄 **Background polling** — checks GitHub every 60 seconds without blocking the UI
- 🔐 **Device Code login** — no manual token setup; authenticate via browser in one step
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

### 2. Add your Client ID

Open `src/access_token.rs` and replace the placeholder:

```rust
const GITHUB_CLIENT_ID: &str = "YOUR_GITHUB_OAUTH_APP_CLIENT_ID";
```

### 3. Build

```bash
cargo build --release
```

The binary is at `target/release/git-system-tray` (or `.exe` on Windows).

### 4. First launch

On first run the app opens your browser and shows a device code dialog.  
Enter the code on GitHub — once authorized the token is saved to `~/.github-trayicon/access_token.txt` and you won't be asked again.

---

## Pre-built binaries

Download the latest Windows or Linux binary from the [Releases](../../releases) page.

---

## License

[The Unlicense](LICENSE) — public domain, no restrictions.
