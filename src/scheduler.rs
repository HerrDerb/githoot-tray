//! Notification scheduler.
//!
//! Both platforms share one polling loop (`run_poll_loop`) and differ only in how an update is
//! handed to the UI thread. Linux forwards through an mpsc channel drained by a GTK timer;
//! Windows forwards through `EventLoopProxy::send_event`.
//!
//! The loop waits on `Receiver::recv_timeout` rather than `thread::sleep`. That one substitution
//! buys three things at once: pacing that adapts to GitHub's `x-poll-interval`, an on-demand
//! refresh when the user opens their notifications, and a clean exit when the UI goes away.

use crate::access_token::TokenStore;
use crate::github;
use crate::logln;
use crate::state::{IconState, PollState, REFRESH_BURST};
use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Floor between device-flow re-authentication attempts, so a token GitHub keeps rejecting
/// cannot produce a storm of authorization prompts.
const MIN_REAUTH_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Reason the poll loop was woken early.
pub enum Wake {
    /// The user opened the notifications page, so what they have read is about to change.
    Refresh,
}

/// One rendering instruction for the UI thread.
#[derive(Debug)]
pub struct Update {
    pub icon: IconState,
    pub tooltip: String,
}

// ─── Shared polling core ──────────────────────────────────────────────────────

/// Spawns the poll thread.
///
/// `emit` returns `false` once the UI is gone, which ends the loop. A panic in here used to
/// freeze the icon at its last value with no trace; now it at least says so in the log.
fn spawn_poll_thread(
    tokens: TokenStore,
    wake_rx: Receiver<Wake>,
    emit: impl FnMut(Update) -> bool + Send + 'static,
) {
    std::thread::spawn(move || {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            run_poll_loop(tokens, wake_rx, emit)
        }));
        if outcome.is_err() {
            logln!("poll thread panicked — notification updates have stopped");
        }
    });
}

fn run_poll_loop(
    mut tokens: TokenStore,
    wake_rx: Receiver<Wake>,
    mut emit: impl FnMut(Update) -> bool,
) {
    let client = match github::build_client() {
        Ok(client) => client,
        Err(e) => {
            logln!("fatal: could not build HTTP client: {e}");
            return;
        }
    };

    let mut state = PollState::new();
    let mut burst: VecDeque<Duration> = VecDeque::new();
    let mut last_reauth: Option<Instant> = None;

    // While a refresh burst is draining we send no `If-None-Match`. A conditional request can
    // legitimately answer 304 from a cached view, which would leave a just-read icon stuck on
    // "unread" — the exact symptom the burst exists to cure.
    let mut skip_etag = false;

    loop {
        let etag = if skip_etag {
            None
        } else {
            state.etag().map(str::to_string)
        };

        let response = github::poll(&client, tokens.token(), etag.as_deref());
        let kind = response.result.kind();
        let conditional = if etag.is_some() { "conditional" } else { "unconditional" };

        let icon = state.apply(response);
        let tooltip = state.tooltip();

        // Update the UI before anything else here can block.
        if !emit(Update { icon, tooltip: tooltip.clone() }) {
            return; // UI has gone away
        }

        // A rejected token never recovers by waiting, so try to replace it rather than sitting
        // in Unknown until the user restarts the app.
        if state.take_needs_reauth() {
            let allowed = last_reauth.is_none_or(|at| at.elapsed() >= MIN_REAUTH_INTERVAL);
            if allowed {
                last_reauth = Some(Instant::now());
                logln!("{conditional} poll → {kind} → {icon:?} (re-authenticating now)");
                match tokens.reauthenticate() {
                    Ok(()) => {
                        state.clear_etag();
                        continue; // retry at once with the new token
                    }
                    Err(e) => logln!("re-authentication failed: {e}"),
                }
            } else {
                logln!("skipping re-authentication — attempted too recently");
            }
        }

        // A queued burst entry wins over normal pacing, so resolve the real delay before
        // logging it — otherwise the log would claim the steady-state interval during a burst,
        // which is exactly when someone is watching it.
        let delay = match burst.pop_front() {
            Some(delay) => delay,
            None => {
                skip_etag = false;
                state.next_delay()
            }
        };

        logln!(
            "{conditional} poll → {kind} → {icon:?} (next in {}s) [{}]",
            delay.as_secs(),
            // The tooltip is deliberately multi-line for the UI; keep the log one line per poll.
            tooltip.replace('\n', " · ")
        );

        match wake_rx.recv_timeout(delay) {
            Ok(Wake::Refresh) => {
                logln!("refresh requested — polling {} more times", REFRESH_BURST.len());
                burst = REFRESH_BURST.iter().copied().collect();
                skip_etag = true;
            }
            Err(RecvTimeoutError::Timeout) => {}
            // The sender lives in the UI, so a closed channel means the app is shutting down.
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

// ─── Linux ────────────────────────────────────────────────────────────────────

/// How often the GTK main loop checks for pending updates. Cheap — it only touches a channel.
#[cfg(target_os = "linux")]
const UI_DRAIN_INTERVAL: Duration = Duration::from_secs(1);

#[cfg(target_os = "linux")]
pub fn start_notification_scheduler(
    indicator: libappindicator::AppIndicator,
    icon_path: String,
    icon_with_notification_path: String,
    tokens: TokenStore,
    wake_rx: Receiver<Wake>,
) {
    // Use the glib that `gtk` itself was built against, so this timer is attached by the same
    // bindings that run the main loop.
    use gtk::glib;

    let (update_tx, update_rx) = std::sync::mpsc::channel::<Update>();
    spawn_poll_thread(tokens, wake_rx, move |update| update_tx.send(update).is_ok());

    // No `Arc<Mutex<_>>` here: `timeout_add_local` requires only `FnMut + 'static`, and this
    // closure runs on the GTK main thread, so the indicator can simply be owned by it.
    // (`glib::idle_add_once` would look tidier but demands `Send`, which `AppIndicator` is not.)
    let mut indicator = indicator;
    let mut applied_notification_icon: Option<bool> = None;

    glib::timeout_add_local(UI_DRAIN_INTERVAL, move || {
        while let Ok(update) = update_rx.try_recv() {
            // `Unknown` deliberately leaves the picture alone — a brief failure should change
            // the words, not make the icon flap. Only a confirmed answer moves the image.
            let wanted = match update.icon {
                IconState::Unread => Some(true),
                IconState::Clear => Some(false),
                IconState::Unknown => None,
            };

            if let Some(want_notification) = wanted
                && applied_notification_icon != Some(want_notification)
            {
                let path = if want_notification {
                    icon_with_notification_path.as_str()
                } else {
                    icon_path.as_str()
                };
                indicator.set_icon(path);
                applied_notification_icon = Some(want_notification);
            }

            indicator.set_title(&update.tooltip);
            // A label is the only part of an indicator visible without hovering, so an
            // unknown state gets a marker the user can actually notice.
            let label = if update.icon == IconState::Unknown { "!" } else { "" };
            indicator.set_label(label, "");
        }
        glib::ControlFlow::Continue
    });
}

// ─── Windows ──────────────────────────────────────────────────────────────────

/// The custom event type sent from the polling thread to the winit event loop.
#[cfg(target_os = "windows")]
pub enum TrayEvent {
    Update(Update),
}

#[cfg(target_os = "windows")]
pub fn start_notification_scheduler(
    tokens: TokenStore,
    wake_rx: Receiver<Wake>,
    proxy: winit::event_loop::EventLoopProxy<TrayEvent>,
) {
    spawn_poll_thread(tokens, wake_rx, move |update| {
        proxy.send_event(TrayEvent::Update(update)).is_ok()
    });
}
