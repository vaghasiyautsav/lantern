//! Lantern — native Linux app (GTK4). No web view.
//!
//! Unlike the interim served GUI, this links `lantern-core` directly —
//! the design doc's Phase 5 architecture. The core runs on a Tokio
//! runtime in a background thread; events cross into GTK's main context
//! over an async channel; commands go the other way via runtime handle.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gtk4 as gtk;
use gtk4::gdk;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use lantern_core::{Core, CoreConfig, CoreEvent};

/// Also the Wayland `app_id`. GNOME matches a window to its launcher by
/// looking for `<app_id>.desktop`, and uses that file's `Icon=` for the dock.
/// If this constant and the installed .desktop filename ever disagree, the
/// app runs with a generic fallback icon — see `install.sh`.
const APP_ID: &str = "local.lantern.gtk";

struct Backend {
    core: Arc<Core>,
    rt: tokio::runtime::Handle,
    /// Shown as the sender on our own messages.
    my_name: String,
}

/// The widgets of one file card, kept so events can update it in place,
/// plus what is needed to turn per-chunk progress into a rate.
struct FileRow {
    status: gtk::Label,
    icon: gtk::Image,
    size: u64,
    /// Bytes and the moment they were observed, from the last repaint.
    last: Option<(u64, std::time::Instant)>,
    /// Smoothed bytes/sec. Raw per-chunk deltas jitter far too much to read.
    rate: f64,
}

#[derive(Clone)]
struct PeerRow {
    id: [u8; 32],
    name: String,
    host: String,
    addr: String,
}

#[derive(Default)]
struct UiState {
    peers: Vec<PeerRow>,
    selected: Option<[u8; 32]>,
    /// xid -> the file card, so events can update it.
    file_rows: HashMap<String, FileRow>,
    /// Unread arrivals per peer. Incremented when something lands for a
    /// peer that is not on screen; cleared when that peer is selected.
    unread: HashMap<[u8; 32], u32>,
}

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .init();

    // Tokio runtime on its own thread; keep a handle for command dispatch.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let rt_handle = rt.handle().clone();

    // Start the core before the UI; discovery begins immediately.
    let (event_tx, event_rx) = async_channel::unbounded::<CoreEvent>();
    let name = std::env::var("LANTERN_NAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "me".into());
    let data_dir = std::env::var("LANTERN_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            std::path::PathBuf::from(home).join(".lantern")
        });
    let discovery_port: u16 = std::env::var("LANTERN_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3939);
    let targets: Vec<u16> = std::env::var("LANTERN_TARGETS")
        .map(|s| s.split(',').filter_map(|p| p.trim().parse().ok()).collect())
        .unwrap_or_else(|_| vec![3939]);
    let broadcast = std::env::var("LANTERN_BROADCAST")
        .map(|v| v != "false")
        .unwrap_or(true);

    let core = rt.block_on(async {
        let (core, mut events) = Core::start(CoreConfig {
            data_dir,
            display_name: name.clone(),
            discovery_port,
            beacon_targets: targets,
            broadcast,
            quic_port: 0,
            in_memory_store: false,
            download_dir: lantern_core::user_download_dir(),
        })
        .await
        .expect("core start");
        core.announce().await;
        // Pump events into the UI channel.
        tokio::spawn(async move {
            while let Some(ev) = events.recv().await {
                if event_tx.send(ev).await.is_err() {
                    break;
                }
            }
        });
        core
    });

    let backend = Rc::new(Backend {
        core,
        rt: rt_handle,
        my_name: name,
    });

    // Keep the runtime alive for the whole app lifetime.
    std::mem::forget(rt);

    // GTK applications are single-instance: a second launch hands off to the
    // first process and exits, which is right when someone clicks the
    // launcher twice. An explicit LANTERN_DATA_DIR is a different identity
    // and store, so it gets its own process — that is how two peers are
    // tested on one machine.
    let flags = if std::env::var_os("LANTERN_DATA_DIR").is_some() {
        gio::ApplicationFlags::NON_UNIQUE
    } else {
        gio::ApplicationFlags::default()
    };
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(flags)
        .build();
    let event_rx = Rc::new(RefCell::new(Some(event_rx)));
    app.connect_activate(move |app| {
        build_ui(app, Rc::clone(&backend), event_rx.borrow_mut().take());
    });
    app.run()
}

// ---- small formatting helpers ------------------------------------------
// Deliberately mirrors apps/macos-native/Lantern.swift so a person on either
// platform sees the same initials, the same avatar colour for the same peer,
// and the same size units.

/// Up to two initials, matching the macOS shell's `initials()`.
fn initials(name: &str) -> String {
    name.split_whitespace()
        .take(2)
        .filter_map(|w| w.chars().next())
        .collect::<String>()
        .to_uppercase()
}

/// Which of the six avatar colours a key gets. Same arithmetic as the macOS
/// shell's `avatarColor()` — sum of the first four characters — so one peer
/// is the same colour on both platforms.
fn avatar_slot(key: &str) -> usize {
    let n: u32 = key.chars().take(4).map(|c| c as u32).sum();
    (n as usize) % 6
}

/// Local wall-clock time of a millisecond timestamp, e.g. "14:32".
fn fmt_time(ts: u64) -> String {
    glib::DateTime::from_unix_local((ts / 1000) as i64)
        .and_then(|d| d.format("%H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Matches the macOS shell's `fmtSize()`.
fn fmt_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// `/home/u/Downloads` → `~/Downloads`, so a status line stays readable.
fn abbreviate_home(path: &std::path::Path) -> String {
    let Some(home) = std::env::var_os("HOME") else {
        return path.display().to_string();
    };
    match path.strip_prefix(std::path::PathBuf::from(home)) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// A round, coloured initials badge.
fn avatar(name: &str, key: &str, size: i32) -> gtk::Label {
    let a = gtk::Label::new(Some(&initials(name)));
    a.add_css_class("lantern-avatar");
    a.add_css_class(&format!("lantern-av-{}", avatar_slot(key)));
    a.set_size_request(size, size);
    a.set_valign(gtk::Align::Start);
    a
}

const STYLE: &str = "
.lantern-unread {
    background-color: alpha(currentColor, 0.16);
    border-radius: 999px;
    padding: 0 8px;
    font-size: 0.85em;
    font-weight: bold;
}
.lantern-drop-active { background-color: alpha(currentColor, 0.06); }
.lantern-avatar {
    color: #ffffff;
    font-weight: 700;
    font-size: 0.8em;
    border-radius: 999px;
}
.lantern-av-0 { background-color: #6b5ce8; }
.lantern-av-1 { background-color: #0f8a73; }
.lantern-av-2 { background-color: #c2400d; }
.lantern-av-3 { background-color: #7d3bed; }
.lantern-av-4 { background-color: #0369a1; }
.lantern-av-5 { background-color: #bf175c; }
/* Both bubble tints derive from the theme's own foreground, so they track
   light and dark without hardcoding a palette. Outgoing is the stronger of
   the two; alignment already carries most of the distinction. Laltain green
   stays reserved for the mark (brand guide §04). */
.lantern-bubble {
    border-radius: 14px;
    padding: 8px 12px;
}
.lantern-bubble-in  { background-color: alpha(currentColor, 0.08); }
.lantern-bubble-out { background-color: alpha(currentColor, 0.18); }
.lantern-filecard {
    border-radius: 10px;
    padding: 10px;
    background-color: alpha(currentColor, 0.06);
    border: 1px solid alpha(currentColor, 0.14);
}
.lantern-banner {
    background-color: alpha(currentColor, 0.10);
    padding: 6px 12px;
}
.lantern-meta { font-size: 0.8em; }
";

fn build_ui(
    app: &gtk::Application,
    backend: Rc<Backend>,
    event_rx: Option<async_channel::Receiver<CoreEvent>>,
) {
    let state = Rc::new(RefCell::new(UiState::default()));

    let css = gtk::CssProvider::new();
    css.load_from_data(STYLE);
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    // ---- widgets --------------------------------------------------------
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Lantern")
        .default_width(1080)
        .default_height(720)
        .build();

    let header = gtk::HeaderBar::new();
    let me_words =
        lantern_core::safety_words(&lantern_crypto_fingerprint(&backend.core)).join(" · ");
    let subtitle = gtk::Label::new(Some("🔒 encrypted · LAN only"));
    subtitle.add_css_class("dim-label");
    subtitle.set_tooltip_text(Some(&format!("Your safety words: {me_words}")));
    header.pack_end(&subtitle);
    window.set_titlebar(Some(&header));

    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_position(280);
    paned.set_shrink_start_child(false);
    paned.set_shrink_end_child(false);

    // Left: peers
    let peer_list = gtk::ListBox::new();
    peer_list.add_css_class("navigation-sidebar");
    let peers_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&peer_list)
        .build();
    let empty_label = gtk::Label::new(Some(
        "Nobody else yet.\n\nStart Lantern on another machine\non this network and it appears here.",
    ));
    empty_label.add_css_class("dim-label");
    empty_label.set_margin_top(24);
    empty_label.set_wrap(true);
    let left_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    left_box.append(&empty_label);
    left_box.append(&peers_scroll);
    peers_scroll.set_vexpand(true);
    paned.set_start_child(Some(&left_box));

    // Right: conversation
    let right = gtk::Box::new(gtk::Orientation::Vertical, 0);

    // Conversation header: avatar, name, host · addr, verify state.
    let head_avatar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let conv_title = gtk::Label::new(Some("Select someone to start"));
    conv_title.add_css_class("heading");
    conv_title.set_halign(gtk::Align::Start);
    let conv_sub = gtk::Label::new(None);
    conv_sub.add_css_class("dim-label");
    conv_sub.add_css_class("lantern-meta");
    conv_sub.set_halign(gtk::Align::Start);
    let head_text = gtk::Box::new(gtk::Orientation::Vertical, 0);
    head_text.append(&conv_title);
    head_text.append(&conv_sub);
    let head_left = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    head_left.append(&head_avatar);
    head_left.append(&head_text);
    head_left.set_margin_start(12);

    let verify_btn = gtk::Button::with_label("Verify identity");
    verify_btn.set_sensitive(false);
    verify_btn.set_valign(gtk::Align::Center);

    let title_row = gtk::CenterBox::new();
    title_row.set_start_widget(Some(&head_left));
    title_row.set_end_widget(Some(&verify_btn));
    title_row.set_margin_top(8);
    title_row.set_margin_bottom(8);
    title_row.set_margin_end(12);
    right.append(&title_row);
    right.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    // Transient error banner, mirroring the macOS shell's flash().
    let banner = gtk::Label::new(None);
    banner.add_css_class("lantern-banner");
    banner.set_wrap(true);
    banner.set_visible(false);
    right.append(&banner);

    // Messages. A plain Box, not a ListBox — bubbles align themselves and
    // rows are not selectable.
    let msg_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    msg_box.set_margin_top(12);
    msg_box.set_margin_bottom(12);
    msg_box.set_margin_start(14);
    msg_box.set_margin_end(14);
    let msg_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&msg_box)
        .vexpand(true)
        .build();

    // Placeholder shown until a peer is picked.
    let placeholder = gtk::Box::new(gtk::Orientation::Vertical, 8);
    placeholder.set_valign(gtk::Align::Center);
    placeholder.set_halign(gtk::Align::Center);
    let ph_icon = gtk::Image::from_icon_name("system-users-symbolic");
    ph_icon.set_pixel_size(48);
    ph_icon.add_css_class("dim-label");
    let ph_title = gtk::Label::new(Some("Select someone to start"));
    ph_title.add_css_class("title-3");
    let ph_sub = gtk::Label::new(Some(
        "Messages and files go straight to their machine.\nNo server, no cloud, nothing in between.",
    ));
    ph_sub.add_css_class("dim-label");
    ph_sub.set_justify(gtk::Justification::Center);
    placeholder.append(&ph_icon);
    placeholder.append(&ph_title);
    placeholder.append(&ph_sub);

    let stack = gtk::Stack::new();
    stack.add_named(&placeholder, Some("empty"));
    stack.add_named(&msg_scroll, Some("messages"));
    stack.set_visible_child_name("empty");
    stack.set_vexpand(true);
    right.append(&stack);

    let composer = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    composer.set_margin_top(8);
    composer.set_margin_bottom(10);
    composer.set_margin_start(10);
    composer.set_margin_end(10);
    let attach_btn = gtk::Button::from_icon_name("mail-attachment-symbolic");
    attach_btn.set_tooltip_text(Some("Send a file"));
    let entry = gtk::Entry::builder()
        .placeholder_text("Message…")
        .hexpand(true)
        .build();
    let send_btn = gtk::Button::with_label("Send");
    send_btn.add_css_class("suggested-action");
    send_btn.set_sensitive(false);
    composer.append(&attach_btn);
    composer.append(&entry);
    composer.append(&send_btn);
    right.append(&composer);

    paned.set_end_child(Some(&right));
    window.set_child(Some(&paned));

    // ---- helpers --------------------------------------------------------

    // Transient banner, auto-hiding. Failures used to be swallowed entirely.
    let flash = {
        let banner = banner.clone();
        move |text: &str| {
            banner.set_text(text);
            banner.set_visible(true);
            let banner = banner.clone();
            glib::timeout_add_seconds_local_once(6, move || banner.set_visible(false));
        }
    };

    // Auto-scroll.
    //
    // Appending a row does not move the adjustment until GTK has allocated
    // the new widget, and allocation runs on the frame clock — after any idle
    // callback we could schedule here. Scrolling from an idle therefore reads
    // the previous upper bound and stops one message short, which is exactly
    // the "it doesn't follow my own messages" symptom. The bottom is also
    // `upper - page_size`, not `upper`.
    //
    // So let the adjustment announce when it has grown, and follow only when
    // the view is already parked at the bottom: someone who scrolled up to
    // read should not be yanked back down because a message arrived.
    let stick_bottom = Rc::new(std::cell::Cell::new(true));
    {
        let stick = Rc::clone(&stick_bottom);
        msg_scroll.vadjustment().connect_changed(move |a| {
            if stick.get() {
                a.set_value(a.upper() - a.page_size());
            }
        });
    }
    {
        let stick = Rc::clone(&stick_bottom);
        msg_scroll.vadjustment().connect_value_changed(move |a| {
            // A few pixels of slack — smooth scrolling rarely lands exactly
            // on the bound, and being one subpixel off must not count as
            // "the user scrolled away".
            stick.set(a.value() >= a.upper() - a.page_size() - 8.0);
        });
    }

    // Force the view to the newest message. Used when *we* act — sending, or
    // opening a conversation — where following is always what is wanted.
    let pin_to_bottom = {
        let stick = Rc::clone(&stick_bottom);
        let msg_scroll = msg_scroll.clone();
        move || {
            stick.set(true);
            let a = msg_scroll.vadjustment();
            a.set_value(a.upper() - a.page_size());
        }
    };

    // One chat row: avatar and bubble, mirrored for our own messages.
    let append_row = {
        let msg_box = msg_box.clone();
        move |outgoing: bool,
              sender: &str,
              key: &str,
              ts: u64,
              delivered: Option<bool>,
              body: &gtk::Widget| {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.set_margin_top(4);
            row.set_margin_bottom(4);

            let column = gtk::Box::new(gtk::Orientation::Vertical, 3);

            // Meta line: who, when, and — for our own — whether it landed.
            let meta = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            meta.add_css_class("lantern-meta");
            let who = gtk::Label::new(Some(sender));
            who.add_css_class("caption-heading");
            let when = gtk::Label::new(Some(&fmt_time(ts)));
            when.add_css_class("dim-label");
            meta.append(&who);
            meta.append(&when);
            if let Some(delivered) = delivered {
                let tick = gtk::Label::new(Some(if delivered { "✓✓" } else { "◷" }));
                tick.add_css_class("dim-label");
                tick.set_tooltip_text(Some(if delivered {
                    "Delivered"
                } else {
                    "Sending…"
                }));
                meta.append(&tick);
            }

            body.add_css_class("lantern-bubble");
            body.add_css_class(if outgoing {
                "lantern-bubble-out"
            } else {
                "lantern-bubble-in"
            });

            if outgoing {
                meta.set_halign(gtk::Align::End);
                body.set_halign(gtk::Align::End);
                column.set_halign(gtk::Align::End);
                column.append(&meta);
                column.append(body);
                row.set_halign(gtk::Align::End);
                row.append(&column);
                row.append(&avatar(sender, key, 30));
            } else {
                meta.set_halign(gtk::Align::Start);
                body.set_halign(gtk::Align::Start);
                column.set_halign(gtk::Align::Start);
                column.append(&meta);
                column.append(body);
                row.set_halign(gtk::Align::Start);
                row.append(&avatar(sender, key, 30));
                row.append(&column);
            }

            msg_box.append(&row);
            // No scroll here: the adjustment handler above follows when the
            // view is at the bottom, and callers that must always follow
            // (sending, opening a conversation) call pin_to_bottom().
        }
    };

    let append_text = {
        let append_row = append_row.clone();
        move |outgoing: bool, sender: &str, key: &str, ts: u64, delivered: Option<bool>, text: &str| {
            let lbl = gtk::Label::new(Some(text));
            lbl.set_wrap(true);
            lbl.set_selectable(true);
            lbl.set_xalign(0.0);
            // Keep a long message from stretching edge to edge; a bubble that
            // spans the whole window reads as a wall, not a message.
            lbl.set_max_width_chars(46);
            append_row(
                outgoing,
                sender,
                key,
                ts,
                delivered,
                lbl.upcast_ref::<gtk::Widget>(),
            );
        }
    };

    let append_file = {
        let append_row = append_row.clone();
        let state = Rc::clone(&state);
        move |outgoing: bool,
              sender: &str,
              key: &str,
              ts: u64,
              xid: &str,
              name: &str,
              size: u64,
              status: &str| {
            let card = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            card.add_css_class("lantern-filecard");
            let icon = gtk::Image::from_icon_name("document-send-symbolic");
            icon.set_pixel_size(28);
            let v = gtk::Box::new(gtk::Orientation::Vertical, 1);
            let n = gtk::Label::new(Some(name));
            n.set_halign(gtk::Align::Start);
            n.set_wrap(true);
            n.set_max_width_chars(32);
            let s = gtk::Label::new(Some(&format!("{} · {status}", fmt_size(size))));
            s.set_halign(gtk::Align::Start);
            s.add_css_class("dim-label");
            s.add_css_class("lantern-meta");
            v.append(&n);
            v.append(&s);
            card.append(&icon);
            card.append(&v);
            state.borrow_mut().file_rows.insert(
                xid.to_string(),
                FileRow {
                    status: s.clone(),
                    icon: icon.clone(),
                    size,
                    last: None,
                    rate: 0.0,
                },
            );
            append_row(
                outgoing,
                sender,
                key,
                ts,
                None,
                card.upcast_ref::<gtk::Widget>(),
            );
        }
    };

    // Update a file card in place. The size comes from the row rather than
    // being re-parsed out of the label, which the live rate keeps rewriting.
    let set_file_state = {
        let state = Rc::clone(&state);
        move |xid: &str, status: &str, icon_name: &str| {
            if let Some(row) = state.borrow().file_rows.get(xid) {
                row.status
                    .set_text(&format!("{} · {status}", fmt_size(row.size)));
                row.icon.set_icon_name(Some(icon_name));
            }
        }
    };

    // Live progress → "12.3 MB of 50.0 MB · 8.4 MB/s".
    //
    // Repaints are throttled: chunks land far faster than anyone can read,
    // and a number that changes every few milliseconds is unreadable anyway.
    // The gap between repaints doubles as the interval the rate is measured
    // over, which is why it is not simply "one chunk / one event".
    let set_file_progress = {
        let state = Rc::clone(&state);
        move |xid: &str, bytes: u64, total: u64| {
            let mut st = state.borrow_mut();
            let Some(row) = st.file_rows.get_mut(xid) else {
                return;
            };
            let now = std::time::Instant::now();
            let Some((prev_bytes, prev_at)) = row.last else {
                row.last = Some((bytes, now));
                return;
            };
            let dt = now.duration_since(prev_at).as_secs_f64();
            if dt < 0.25 {
                return;
            }
            let instant = bytes.saturating_sub(prev_bytes) as f64 / dt;
            row.rate = if row.rate == 0.0 {
                instant
            } else {
                row.rate * 0.7 + instant * 0.3
            };
            row.last = Some((bytes, now));
            row.status.set_text(&format!(
                "{} of {} · {}/s",
                fmt_size(bytes),
                fmt_size(total),
                fmt_size(row.rate as u64)
            ));
        }
    };

    let refresh_peers = {
        let state = Rc::clone(&state);
        let backend = Rc::clone(&backend);
        let peer_list = peer_list.clone();
        let empty_label = empty_label.clone();
        move || {
            let st = state.borrow();
            empty_label.set_visible(st.peers.is_empty());
            while let Some(child) = peer_list.first_child() {
                peer_list.remove(&child);
            }
            // The list is rebuilt wholesale, so note where the selected peer
            // lands and restore it below — otherwise every beacon that
            // refreshes the roster would drop the open conversation.
            let mut selected_at = None;
            for (i, p) in st.peers.iter().enumerate() {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
                row.set_margin_top(6);
                row.set_margin_bottom(6);
                row.set_margin_start(10);
                row.set_margin_end(8);
                row.append(&avatar(&p.name, &hex::encode(p.id), 34));

                let text_col = gtk::Box::new(gtk::Orientation::Vertical, 1);
                text_col.set_hexpand(true);
                text_col.set_valign(gtk::Align::Center);
                let n = gtk::Label::new(None);
                n.set_halign(gtk::Align::Start);
                // The tick means an out-of-band safety-word comparison
                // happened — never that the transport is merely encrypted.
                if backend.core.is_verified(&p.id) {
                    n.set_text(&format!("{} ✓", p.name));
                    n.set_tooltip_text(Some("Verified — safety words matched"));
                } else {
                    n.set_text(&p.name);
                }
                let h = gtk::Label::new(Some(&p.host));
                h.set_halign(gtk::Align::Start);
                h.add_css_class("dim-label");
                h.add_css_class("lantern-meta");
                text_col.append(&n);
                text_col.append(&h);
                row.append(&text_col);

                if let Some(count) = st.unread.get(&p.id).copied().filter(|c| *c > 0) {
                    let badge = gtk::Label::new(Some(&count.to_string()));
                    badge.add_css_class("lantern-unread");
                    badge.set_valign(gtk::Align::Center);
                    badge.set_tooltip_text(Some(&format!("{count} unread from {}", p.name)));
                    row.append(&badge);
                }

                peer_list.append(&row);
                if st.selected == Some(p.id) {
                    selected_at = Some(i as i32);
                }
            }
            drop(st);
            if let Some(i) = selected_at {
                if let Some(row) = peer_list.row_at_index(i) {
                    peer_list.select_row(Some(&row));
                }
            }
        }
    };

    // ---- selection ------------------------------------------------------
    {
        let state = Rc::clone(&state);
        let backend = Rc::clone(&backend);
        let conv_title = conv_title.clone();
        let conv_sub = conv_sub.clone();
        let head_avatar = head_avatar.clone();
        let verify_btn = verify_btn.clone();
        let msg_box = msg_box.clone();
        let stack = stack.clone();
        let entry = entry.clone();
        let append_text = append_text.clone();
        let refresh_peers = refresh_peers.clone();
        let pin_to_bottom = pin_to_bottom.clone();
        peer_list.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            let idx = row.index();
            let peer = {
                let st = state.borrow();
                st.peers.get(idx as usize).cloned()
            };
            let Some(peer) = peer else { return };
            // refresh_peers() restores this selection after every rebuild.
            // Without this guard each beacon would reload the same history,
            // flickering the conversation and resetting the scroll.
            let already_open = state.borrow().selected == Some(peer.id);
            if already_open {
                return;
            }
            {
                let mut st = state.borrow_mut();
                st.selected = Some(peer.id);
                st.unread.remove(&peer.id);
                st.file_rows.clear();
            }
            // Clear the badge on the next loop turn — refresh_peers() rebuilds
            // the very ListBox currently emitting this signal.
            glib::idle_add_local_once(refresh_peers.clone());

            conv_title.set_text(&peer.name);
            conv_sub.set_text(&format!("{} · {}", peer.host, peer.addr));
            while let Some(c) = head_avatar.first_child() {
                head_avatar.remove(&c);
            }
            head_avatar.append(&avatar(&peer.name, &hex::encode(peer.id), 32));

            let verified = backend.core.is_verified(&peer.id);
            verify_btn.set_label(if verified { "Verified ✓" } else { "Verify identity" });
            verify_btn.set_sensitive(true);
            entry.set_placeholder_text(Some(&format!(
                "Message {}…  (or drop files here)",
                peer.name
            )));

            while let Some(child) = msg_box.first_child() {
                msg_box.remove(&child);
            }
            stack.set_visible_child_name("messages");
            for m in backend.core.history(&peer.id, 200) {
                if m.outgoing {
                    append_text(
                        true,
                        &backend.my_name,
                        "me-self",
                        m.ts,
                        Some(m.state >= 1),
                        &m.text,
                    );
                } else {
                    append_text(
                        false,
                        &peer.name,
                        &hex::encode(peer.id),
                        m.ts,
                        None,
                        &m.text,
                    );
                }
            }
            // Open a conversation at its newest message, not its oldest.
            pin_to_bottom();
        });
    }

    // ---- send message ---------------------------------------------------
    {
        let entry = entry.clone();
        let send_btn = send_btn.clone();
        entry.connect_changed(move |e| {
            send_btn.set_sensitive(!e.text().trim().is_empty());
        });
    }
    let do_send = {
        let state = Rc::clone(&state);
        let backend = Rc::clone(&backend);
        let entry = entry.clone();
        let append_text = append_text.clone();
        let flash = flash.clone();
        let pin_to_bottom = pin_to_bottom.clone();
        move || {
            let text = entry.text().to_string();
            let text = text.trim().to_string();
            if text.is_empty() {
                return;
            }
            let Some(peer) = state.borrow().selected else {
                return;
            };
            entry.set_text("");
            let now = glib::real_time() as u64 / 1000;
            append_text(true, &backend.my_name, "me-self", now, Some(false), &text);
            // Sending always follows, even if we were reading history.
            pin_to_bottom();
            let core = Arc::clone(&backend.core);
            let (tx, rx) = async_channel::bounded::<Option<String>>(1);
            backend.rt.spawn(async move {
                let err = core.send_message(peer, &text).await.err().map(|e| e.to_string());
                let _ = tx.send(err).await;
            });
            let flash = flash.clone();
            glib::MainContext::default().spawn_local(async move {
                if let Ok(Some(err)) = rx.recv().await {
                    flash(&format!("Message didn't send — {err}"));
                }
            });
        }
    };
    {
        let do_send = do_send.clone();
        entry.connect_activate(move |_| do_send());
    }
    {
        let do_send = do_send.clone();
        send_btn.connect_clicked(move |_| do_send());
    }

    // ---- send file ------------------------------------------------------
    // One path in, one transfer out. The paperclip and the drop target both
    // go through here so a dropped file behaves exactly like a picked one.
    let send_path = {
        let state = Rc::clone(&state);
        let backend = Rc::clone(&backend);
        let append_file = append_file.clone();
        let flash = flash.clone();
        move |path: std::path::PathBuf| {
            let Some(peer) = state.borrow().selected else {
                return;
            };
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            // Fire and let events update the card (keyed by xid); we don't
            // know the xid until send_file returns, so the card is appended
            // once a oneshot hands it back.
            let core = Arc::clone(&backend.core);
            let (tx, rx) = async_channel::bounded::<Result<String, String>>(1);
            backend.rt.spawn(async move {
                let r = core
                    .send_file(peer, &path)
                    .await
                    .map(|x| x.to_string())
                    .map_err(|e| e.to_string());
                let _ = tx.send(r).await;
            });
            let append_file = append_file.clone();
            let flash = flash.clone();
            let pin_to_bottom = pin_to_bottom.clone();
            let my_name = backend.my_name.clone();
            glib::MainContext::default().spawn_local(async move {
                match rx.recv().await {
                    Ok(Ok(xid)) => {
                        let now = glib::real_time() as u64 / 1000;
                        append_file(
                            true, &my_name, "me-self", now, &xid, &name, size, "sending…",
                        );
                        pin_to_bottom();
                    }
                    Ok(Err(e)) => flash(&format!("Couldn't send {name} — {e}")),
                    Err(_) => {}
                }
            });
        }
    };

    {
        let state = Rc::clone(&state);
        let window_weak = window.downgrade();
        let send_path = send_path.clone();
        attach_btn.connect_clicked(move |_| {
            if state.borrow().selected.is_none() {
                return;
            }
            let Some(window) = window_weak.upgrade() else {
                return;
            };
            let dialog = gtk::FileDialog::new();
            let send_path = send_path.clone();
            dialog.open(Some(&window), None::<&gio::Cancellable>, move |result| {
                let Ok(file) = result else { return };
                let Some(path) = file.path() else { return };
                send_path(path);
            });
        });
    }

    // ---- drop files onto the conversation --------------------------------
    // Only a local path can be sent: the core streams from disk, and
    // invariant 7 forbids the shell fetching anything off the link.
    {
        let state = Rc::clone(&state);
        let send_path = send_path.clone();
        let flash = flash.clone();
        let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        drop.set_types(&[gdk::FileList::static_type(), gio::File::static_type()]);

        let scroll_enter = msg_scroll.clone();
        drop.connect_enter(move |_, _, _| {
            scroll_enter.add_css_class("lantern-drop-active");
            gdk::DragAction::COPY
        });
        let scroll_leave = msg_scroll.clone();
        drop.connect_leave(move |_| {
            scroll_leave.remove_css_class("lantern-drop-active");
        });

        let scroll_drop = msg_scroll.clone();
        drop.connect_drop(move |_, value, _, _| {
            scroll_drop.remove_css_class("lantern-drop-active");
            if state.borrow().selected.is_none() {
                flash("Pick a person first, then drop the file.");
                return false;
            }
            // A multi-file drag arrives as one FileList, a single file as
            // a bare File, depending on the source application.
            if let Ok(list) = value.get::<gdk::FileList>() {
                let paths: Vec<_> = list.files().iter().filter_map(|f| f.path()).collect();
                if paths.is_empty() {
                    return false;
                }
                for path in paths {
                    send_path(path);
                }
                return true;
            }
            if let Ok(file) = value.get::<gio::File>() {
                if let Some(path) = file.path() {
                    send_path(path);
                    return true;
                }
            }
            false
        });
        msg_scroll.add_controller(drop);
    }

    // ---- verify dialog --------------------------------------------------
    {
        let state = Rc::clone(&state);
        let backend = Rc::clone(&backend);
        let window_weak = window.downgrade();
        let refresh_peers = refresh_peers.clone();
        let verify_btn2 = verify_btn.clone();
        verify_btn.connect_clicked(move |_| {
            let Some(peer) = state.borrow().selected else {
                return;
            };
            let Some(window) = window_weak.upgrade() else {
                return;
            };
            let peer_name = {
                let st = state.borrow();
                st.peers
                    .iter()
                    .find(|p| p.id == peer)
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| "identity".into())
            };
            let dlg = gtk::Window::builder()
                .transient_for(&window)
                .modal(true)
                .title("Verify identity")
                .default_width(430)
                .build();
            let v = gtk::Box::new(gtk::Orientation::Vertical, 12);
            v.set_margin_top(18);
            v.set_margin_bottom(18);
            v.set_margin_start(18);
            v.set_margin_end(18);
            let heading = gtk::Label::new(Some(&format!("Verify {peer_name}")));
            heading.add_css_class("title-3");
            heading.set_halign(gtk::Align::Start);
            let info = gtk::Label::new(Some(&format!(
                "Read these words aloud to each other. If they match what \
                 {peer_name} sees for themselves, this connection cannot be \
                 an impostor's."
            )));
            info.set_wrap(true);
            info.set_xalign(0.0);
            info.add_css_class("dim-label");

            // Numbered, two columns, monospaced — same shape as the macOS
            // sheet, so two people reading to each other stay in step.
            let grid = gtk::Grid::new();
            grid.set_row_spacing(6);
            grid.set_column_spacing(18);
            grid.add_css_class("lantern-filecard");
            let words = Core::words_for(&peer);
            for (i, w) in words.iter().enumerate() {
                let lbl = gtk::Label::new(Some(&format!("{}  {w}", i + 1)));
                lbl.set_halign(gtk::Align::Start);
                lbl.add_css_class("monospace");
                grid.attach(&lbl, (i % 2) as i32, (i / 2) as i32, 1, 1);
            }

            let trust = gtk::Button::with_label("They match — mark verified");
            trust.add_css_class("suggested-action");
            let close = gtk::Button::with_label("Close");
            let btns = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            btns.set_halign(gtk::Align::End);
            btns.append(&close);
            btns.append(&trust);

            v.append(&heading);
            v.append(&info);
            v.append(&grid);
            v.append(&btns);
            dlg.set_child(Some(&v));

            let dlg_close = dlg.downgrade();
            close.connect_clicked(move |_| {
                if let Some(d) = dlg_close.upgrade() {
                    d.close();
                }
            });

            let backend2 = Rc::clone(&backend);
            let dlg_weak = dlg.downgrade();
            let refresh_peers = refresh_peers.clone();
            let verify_btn2 = verify_btn2.clone();
            trust.connect_clicked(move |_| {
                backend2.core.set_verified(&peer, true);
                verify_btn2.set_label("Verified ✓");
                refresh_peers();
                if let Some(d) = dlg_weak.upgrade() {
                    d.close();
                }
            });
            dlg.present();
        });
    }

    // ---- core events → UI ----------------------------------------------
    if let Some(rx) = event_rx {
        let state = Rc::clone(&state);
        let refresh_peers = refresh_peers.clone();
        let append_text = append_text.clone();
        let append_file = append_file.clone();
        let set_file_state = set_file_state.clone();
        let set_file_progress = set_file_progress.clone();
        let app = app.clone();
        let window = window.clone();
        glib::MainContext::default().spawn_local(async move {
            while let Ok(ev) = rx.recv().await {
                match ev {
                    CoreEvent::PeerSeen { id, name, host, addr, .. } => {
                        let mut st = state.borrow_mut();
                        if let Some(p) = st.peers.iter_mut().find(|p| p.id == id) {
                            p.name = name;
                            p.host = host;
                            p.addr = addr.to_string();
                        } else {
                            st.peers.push(PeerRow {
                                id,
                                name,
                                host,
                                addr: addr.to_string(),
                            });
                            st.peers.sort_by(|a, b| a.name.cmp(&b.name));
                        }
                        drop(st);
                        refresh_peers();
                    }
                    CoreEvent::MessageReceived { peer, peer_name, text, ts, .. } => {
                        let open = state.borrow().selected == Some(peer);
                        if open {
                            append_text(false, &peer_name, &hex::encode(peer), ts, None, &text);
                        } else {
                            *state.borrow_mut().unread.entry(peer).or_insert(0) += 1;
                            refresh_peers();
                            // The macOS shell badges the dock; the desktop
                            // equivalent is a notification, and only when the
                            // window is not already in front of the user.
                            if !window.is_active() {
                                let n = gio::Notification::new(&peer_name);
                                n.set_body(Some(&text));
                                app.send_notification(Some("lantern-message"), &n);
                            }
                        }
                    }
                    CoreEvent::MessageDelivered { .. } => {}
                    CoreEvent::FileOffered { peer, peer_name, xid, name, size } => {
                        let open = state.borrow().selected == Some(peer);
                        if open {
                            let now = glib::real_time() as u64 / 1000;
                            append_file(
                                false,
                                &peer_name,
                                &hex::encode(peer),
                                now,
                                &xid.to_string(),
                                &name,
                                size,
                                "receiving…",
                            );
                        } else {
                            *state.borrow_mut().unread.entry(peer).or_insert(0) += 1;
                            refresh_peers();
                        }
                    }
                    CoreEvent::FileReceived { xid, path, .. } => {
                        // Report where it actually went, rather than a
                        // literal that goes stale the moment the download
                        // directory is configurable.
                        let dir = path
                            .parent()
                            .map(abbreviate_home)
                            .unwrap_or_else(|| "the download folder".into());
                        set_file_state(
                            &xid.to_string(),
                            &format!("saved to {dir}"),
                            "document-save-symbolic",
                        );
                    }
                    CoreEvent::TransferProgress { xid, bytes, total, .. } => {
                        set_file_progress(&xid.to_string(), bytes, total);
                    }
                    CoreEvent::ChunksSent { xid, sent, total } => {
                        if sent < total {
                            set_file_state(
                                &xid.to_string(),
                                &format!("resume — only {sent} of {total} chunks needed"),
                                "document-send-symbolic",
                            );
                        }
                    }
                    CoreEvent::FileSent { xid, ok, err } => {
                        if ok {
                            set_file_state(
                                &xid.to_string(),
                                "delivered & verified",
                                "emblem-ok-symbolic",
                            );
                        } else {
                            set_file_state(
                                &xid.to_string(),
                                &err.unwrap_or_else(|| "failed".into()),
                                "dialog-error-symbolic",
                            );
                        }
                    }
                    CoreEvent::TrustWarning { detail, .. } => {
                        let now = glib::real_time() as u64 / 1000;
                        append_text(false, "⚠ Trust warning", "warn", now, None, &detail);
                    }
                    _ => {}
                }
            }
        });
    }

    window.set_icon_name(Some("lantern"));
    window.present();
}

/// The core's identity fingerprint (helper — core exposes id bytes).
fn lantern_crypto_fingerprint(core: &Core) -> [u8; 32] {
    // words_for hashes internally; reuse the identity bytes directly.
    core.identity_id()
}
