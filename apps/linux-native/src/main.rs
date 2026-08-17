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
use lantern_core::{Core, CoreConfig, CoreEvent, Uuid};

const APP_ID: &str = "local.lantern.gtk";

struct Backend {
    core: Arc<Core>,
    rt: tokio::runtime::Handle,
}

#[derive(Default)]
struct UiState {
    peers: Vec<([u8; 32], String, String)>, // id, name, host
    selected: Option<[u8; 32]>,
    /// xid -> status label of the file row, so events can update it.
    file_rows: HashMap<String, gtk::Label>,
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

fn build_ui(
    app: &gtk::Application,
    backend: Rc<Backend>,
    event_rx: Option<async_channel::Receiver<CoreEvent>>,
) {
    let state = Rc::new(RefCell::new(UiState::default()));

    // ---- style ----------------------------------------------------------
    // Derived from the theme's own foreground, so the badge and the drop
    // highlight follow light/dark without hardcoding a colour. Laltain green
    // stays reserved for the mark (brand guide §04).
    let css = gtk::CssProvider::new();
    css.load_from_data(
        ".lantern-unread { \
             background-color: alpha(currentColor, 0.16); \
             border-radius: 999px; \
             padding: 0 8px; \
             font-size: 0.85em; \
             font-weight: bold; \
         } \
         .lantern-drop-active { \
             background-color: alpha(currentColor, 0.06); \
         }",
    );
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
    let update_btn = gtk::Button::with_label("Updates");
    let build = lantern_core::update::BuildInfo::current();
    update_btn.set_tooltip_text(Some(&format!(
        "Build {} ({}) — check GitHub for a newer Lantern",
        build.commit, build.date
    )));
    header.pack_start(&update_btn);
    window.set_titlebar(Some(&header));

    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_position(260);
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
    // Sidebar header: what the list is, and the one control that belongs to
    // the list rather than to a conversation.
    let roster_head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    roster_head.set_margin_top(8);
    roster_head.set_margin_bottom(2);
    roster_head.set_margin_start(12);
    roster_head.set_margin_end(6);
    let roster_lbl = gtk::Label::new(Some("On this network"));
    roster_lbl.add_css_class("dim-label");
    roster_lbl.set_xalign(0.0);
    roster_lbl.set_hexpand(true);
    let refresh_btn = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh_btn.add_css_class("flat");
    refresh_btn.set_tooltip_text(Some(
        "Look for people again now — Lantern also checks on its own every \
         few seconds",
    ));
    roster_head.append(&roster_lbl);
    roster_head.append(&refresh_btn);

    let left_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    left_box.append(&roster_head);
    left_box.append(&empty_label);
    left_box.append(&peers_scroll);
    peers_scroll.set_vexpand(true);
    paned.set_start_child(Some(&left_box));

    // Right: conversation
    let right = gtk::Box::new(gtk::Orientation::Vertical, 0);

    let conv_title = gtk::Label::new(Some("Select someone to start"));
    conv_title.add_css_class("title-4");
    conv_title.set_margin_top(10);
    conv_title.set_margin_bottom(6);
    let verify_btn = gtk::Button::with_label("Verify identity");
    verify_btn.set_sensitive(false);
    let clear_btn = gtk::Button::with_label("Clear chat");
    clear_btn.set_sensitive(false);
    clear_btn.set_tooltip_text(Some(
        "Delete this conversation from this machine — the other machine \
         keeps its own copy",
    ));
    let title_actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    title_actions.append(&verify_btn);
    title_actions.append(&clear_btn);
    let title_row = gtk::CenterBox::new();
    title_row.set_center_widget(Some(&conv_title));
    title_row.set_end_widget(Some(&title_actions));
    title_row.set_margin_end(10);
    right.append(&title_row);
    right.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    let msg_list = gtk::ListBox::new();
    msg_list.set_selection_mode(gtk::SelectionMode::None);
    let msg_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&msg_list)
        .vexpand(true)
        .build();
    right.append(&msg_scroll);

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
    composer.append(&attach_btn);
    composer.append(&entry);
    composer.append(&send_btn);
    right.append(&composer);

    paned.set_end_child(Some(&right));
    window.set_child(Some(&paned));

    // ---- helpers --------------------------------------------------------
    // `mid` is what makes a row deletable: given one, the row carries a ⋯
    // menu that removes that message from this machine. Rows without one —
    // transfers, trust warnings, Lantern's own notices — aren't in the
    // message log, so offering to delete them would promise something the
    // store can't do.
    let append_row = {
        let msg_list = msg_list.clone();
        let msg_scroll = msg_scroll.clone();
        let backend = Rc::clone(&backend);
        move |title: &str, body: &gtk::Widget, mid: Option<Uuid>| {
            let row_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
            row_box.set_margin_top(6);
            row_box.set_margin_bottom(6);
            row_box.set_margin_start(12);
            row_box.set_margin_end(12);

            let head_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let head = gtk::Label::new(Some(title));
            head.set_halign(gtk::Align::Start);
            head.set_hexpand(true);
            head.add_css_class("caption-heading");
            head_row.append(&head);

            if let Some(mid) = mid {
                let more = gtk::MenuButton::new();
                more.set_icon_name("view-more-symbolic");
                more.add_css_class("flat");
                more.set_valign(gtk::Align::Center);
                more.set_tooltip_text(Some("More — delete this message"));
                let pop = gtk::Popover::new();
                let del = gtk::Button::with_label("Delete on this machine");
                del.add_css_class("destructive-action");
                del.set_margin_top(6);
                del.set_margin_bottom(6);
                del.set_margin_start(6);
                del.set_margin_end(6);
                del.set_tooltip_text(Some(
                    "Deletes your copy only — the other machine keeps theirs",
                ));
                pop.set_child(Some(&del));
                more.set_popover(Some(&pop));
                head_row.append(&more);

                let core = Arc::clone(&backend.core);
                let msg_list = msg_list.clone();
                let row_weak = row_box.downgrade();
                let pop_close = pop.clone();
                del.connect_clicked(move |_| {
                    pop_close.popdown();
                    core.delete_message(&mid);
                    // ListBox wraps an appended widget in a row of its own,
                    // so what has to go is the parent, not this box.
                    if let Some(row) = row_weak.upgrade() {
                        if let Some(parent) = row.parent() {
                            msg_list.remove(&parent);
                        }
                    }
                });
            }

            row_box.append(&head_row);
            row_box.append(body);
            msg_list.append(&row_box);
            // Scroll to bottom after layout.
            let adj = msg_scroll.vadjustment();
            glib::idle_add_local_once(move || {
                adj.set_value(adj.upper());
            });
        }
    };
    let append_text = {
        let append_row = append_row.clone();
        move |title: &str, text: &str, mid: Option<Uuid>| {
            let lbl = gtk::Label::new(Some(text));
            lbl.set_halign(gtk::Align::Start);
            lbl.set_wrap(true);
            lbl.set_selectable(true);
            append_row(title, lbl.upcast_ref::<gtk::Widget>(), mid);
        }
    };
    let append_file = {
        let append_row = append_row.clone();
        let state = Rc::clone(&state);
        move |title: &str, xid: &str, name: &str, status: &str| {
            let boxx = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let icon = gtk::Image::from_icon_name("text-x-generic-symbolic");
            let v = gtk::Box::new(gtk::Orientation::Vertical, 1);
            let n = gtk::Label::new(Some(name));
            n.set_halign(gtk::Align::Start);
            let s = gtk::Label::new(Some(status));
            s.set_halign(gtk::Align::Start);
            s.add_css_class("dim-label");
            v.append(&n);
            v.append(&s);
            boxx.append(&icon);
            boxx.append(&v);
            state
                .borrow_mut()
                .file_rows
                .insert(xid.to_string(), s.clone());
            // A transfer is not a message in the log, so no delete menu.
            append_row(title, boxx.upcast_ref::<gtk::Widget>(), None);
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
            for (i, (id, name, host)) in st.peers.iter().enumerate() {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                row.set_margin_top(6);
                row.set_margin_bottom(6);
                row.set_margin_start(10);
                row.set_margin_end(8);

                let text_col = gtk::Box::new(gtk::Orientation::Vertical, 1);
                text_col.set_hexpand(true);
                let n = gtk::Label::new(None);
                n.set_halign(gtk::Align::Start);
                // The tick means an out-of-band safety-word comparison
                // happened — never that the transport is merely encrypted.
                if backend.core.is_verified(id) {
                    n.set_text(&format!("{name} ✓"));
                    n.set_tooltip_text(Some("Verified — safety words matched"));
                } else {
                    n.set_text(name);
                }
                let h = gtk::Label::new(Some(host));
                h.set_halign(gtk::Align::Start);
                h.add_css_class("dim-label");
                text_col.append(&n);
                text_col.append(&h);
                row.append(&text_col);

                if let Some(count) = st.unread.get(id).copied().filter(|c| *c > 0) {
                    let badge = gtk::Label::new(Some(&count.to_string()));
                    badge.add_css_class("lantern-unread");
                    badge.set_valign(gtk::Align::Center);
                    badge.set_tooltip_text(Some(&format!(
                        "{count} unread from {name}"
                    )));
                    row.append(&badge);
                }

                peer_list.append(&row);
                if st.selected == Some(*id) {
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
        let verify_btn = verify_btn.clone();
        let clear_btn = clear_btn.clone();
        let msg_list = msg_list.clone();
        let append_text = append_text.clone();
        let refresh_peers = refresh_peers.clone();
        peer_list.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            let idx = row.index();
            let peer = {
                let st = state.borrow();
                st.peers.get(idx as usize).cloned()
            };
            let Some((id, name, _host)) = peer else { return };
            // refresh_peers() restores this selection after every rebuild.
            // Without this guard each beacon would reload the same history,
            // flickering the conversation and resetting the scroll.
            let already_open = state.borrow().selected == Some(id);
            if already_open {
                return;
            }
            {
                let mut st = state.borrow_mut();
                st.selected = Some(id);
                st.unread.remove(&id);
            }
            // Clear the badge on the next loop turn — refresh_peers() rebuilds
            // the very ListBox currently emitting this signal.
            {
                glib::idle_add_local_once(refresh_peers.clone());
            }
            conv_title.set_text(&name);
            verify_btn.set_sensitive(true);
            clear_btn.set_sensitive(true);
            while let Some(child) = msg_list.first_child() {
                msg_list.remove(&child);
            }
            let my_name = "You";
            for m in backend.core.history(&id, 200) {
                let title = if m.outgoing {
                    format!("{my_name} · {}", if m.state >= 1 { "✓✓" } else { "◷" })
                } else {
                    name.clone()
                };
                append_text(&title, &m.text, Some(m.mid));
            }
        });
    }

    // ---- send message ---------------------------------------------------
    let do_send = {
        let state = Rc::clone(&state);
        let backend = Rc::clone(&backend);
        let entry = entry.clone();
        let append_text = append_text.clone();
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
// The mid only exists once the core has accepted the message, and
            // this row goes up before that so typing feels instant — so a
            // just-sent line gets its delete menu when the conversation is
            // next opened, from history.
            append_text("You · ◷", &text, None);
            let core = Arc::clone(&backend.core);
            backend.rt.spawn(async move {
                let _ = core.send_message(peer, &text).await;
            });
        }
    };
    {
        let do_send = do_send.clone();
        entry.connect_activate(move |_| do_send());
    }
    send_btn.connect_clicked(move |_| do_send());

    // ---- send file ------------------------------------------------------
    // One path in, one transfer out. The paperclip and the drop target both
    // go through here so a dropped file behaves exactly like a picked one.
    let send_path = {
        let state = Rc::clone(&state);
        let backend = Rc::clone(&backend);
        let append_file = append_file.clone();
        move |path: std::path::PathBuf| {
            let Some(peer) = state.borrow().selected else {
                return;
            };
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            // Fire and let events update the row (keyed by xid); we don't
            // know the xid until send_file returns, so the row is appended
            // once a oneshot hands it back.
            let core = Arc::clone(&backend.core);
            let (tx, rx) = async_channel::bounded::<String>(1);
            backend.rt.spawn(async move {
                if let Ok(xid) = core.send_file(peer, &path).await {
                    let _ = tx.send(xid.to_string()).await;
                }
            });
            let append_file = append_file.clone();
            glib::MainContext::default().spawn_local(async move {
                if let Ok(xid) = rx.recv().await {
                    append_file("You", &xid, &name, "sending…");
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
        verify_btn.connect_clicked(move |_| {
            let Some(peer) = state.borrow().selected else {
                return;
            };
            let Some(window) = window_weak.upgrade() else {
                return;
            };
            let words = Core::words_for(&peer).join("  ·  ");
            let dlg = gtk::Window::builder()
                .transient_for(&window)
                .modal(true)
                .title("Verify identity")
                .default_width(420)
                .build();
            let v = gtk::Box::new(gtk::Orientation::Vertical, 12);
            v.set_margin_top(18);
            v.set_margin_bottom(18);
            v.set_margin_start(18);
            v.set_margin_end(18);
            let info = gtk::Label::new(Some(
                "Read these words aloud to each other. If they match what \
                 the other person sees for themselves, this connection \
                 cannot be an impostor's.",
            ));
            info.set_wrap(true);
            let wl = gtk::Label::new(Some(&words));
            wl.set_wrap(true);
            wl.add_css_class("title-4");
            let trust = gtk::Button::with_label("They match — mark verified");
            trust.add_css_class("suggested-action");
            v.append(&info);
            v.append(&wl);
            v.append(&trust);
            dlg.set_child(Some(&v));
            let backend2 = Rc::clone(&backend);
            let dlg_weak = dlg.downgrade();
            let refresh_peers = refresh_peers.clone();
            trust.connect_clicked(move |_| {
                backend2.core.set_verified(&peer, true);
                refresh_peers();
                if let Some(d) = dlg_weak.upgrade() {
                    d.close();
                }
            });
            dlg.present();
        });
    }

    // ---- look again now --------------------------------------------------
    // A HELLO beacon; peers answer with their own, which arrive as PeerSeen
    // and rebuild the roster through the usual path. Nothing to poll here.
    {
        let backend = Rc::clone(&backend);
        refresh_btn.connect_clicked(move |_| {
            let core = Arc::clone(&backend.core);
            backend.rt.spawn(async move {
                core.announce().await;
            });
        });
    }

    // ---- clear conversation ---------------------------------------------
    // Asks first, because there is no undo: the engine zeroes the pages it
    // frees and checkpoints the log. Every word here is careful to promise
    // only what happens — this machine's copy, nobody else's.
    {
        let state = Rc::clone(&state);
        let backend = Rc::clone(&backend);
        let window_weak = window.downgrade();
        let msg_list = msg_list.clone();
        let append_text = append_text.clone();
        clear_btn.connect_clicked(move |_| {
            let Some(peer) = state.borrow().selected else {
                return;
            };
            let name = state
                .borrow()
                .peers
                .iter()
                .find(|(id, ..)| *id == peer)
                .map(|(_, n, _)| n.clone())
                .unwrap_or_else(|| "them".into());
            let Some(window) = window_weak.upgrade() else {
                return;
            };
            let dlg = gtk::Window::builder()
                .transient_for(&window)
                .modal(true)
                .title("Clear chat")
                .default_width(420)
                .build();
            let v = gtk::Box::new(gtk::Orientation::Vertical, 12);
            v.set_margin_top(18);
            v.set_margin_bottom(18);
            v.set_margin_start(18);
            v.set_margin_end(18);
            let info = gtk::Label::new(Some(&format!(
                "Delete every message with {name} from this machine?\n\n\
                 This can't be undone. {name} keeps their own copy — Lantern \
                 can only delete what's stored here. Files you've already \
                 sent or received stay on disk."
            )));
            info.set_wrap(true);
            info.set_xalign(0.0);
            let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            buttons.set_halign(gtk::Align::End);
            let cancel = gtk::Button::with_label("Cancel");
            let delete = gtk::Button::with_label("Delete on this machine");
            delete.add_css_class("destructive-action");
            buttons.append(&cancel);
            buttons.append(&delete);
            v.append(&info);
            v.append(&buttons);
            dlg.set_child(Some(&v));

            {
                let dlg_weak = dlg.downgrade();
                cancel.connect_clicked(move |_| {
                    if let Some(d) = dlg_weak.upgrade() {
                        d.close();
                    }
                });
            }
            let backend = Rc::clone(&backend);
            let msg_list = msg_list.clone();
            let append_text = append_text.clone();
            let dlg_weak = dlg.downgrade();
            delete.connect_clicked(move |_| {
                let n = backend.core.clear_history(&peer);
                while let Some(child) = msg_list.first_child() {
                    msg_list.remove(&child);
                }
                // Say what happened where the conversation used to be —
                // an empty pane alone doesn't tell you it worked.
                append_text(
                    "Lantern",
                    &if n == 0 {
                        format!("Nothing was stored for {name} — already empty here.")
                    } else {
                        format!(
                            "Deleted {n} message{} from this machine. \
                             {name} still has their copy.",
                            if n == 1 { "" } else { "s" }
                        )
                    },
                    None,
                );
                if let Some(d) = dlg_weak.upgrade() {
                    d.close();
                }
            });
            dlg.present();
        });
    }

    // ---- updates ---------------------------------------------------------
    // Same mechanism as the macOS shell, because it's the same core: fetch,
    // fast-forward, install.sh, reopen — refused outright if the checkout has
    // uncommitted work in it. The app has to quit for the install to replace
    // the binary it's running from, so this hands off and closes.
    {
        let backend = Rc::clone(&backend);
        let window_weak = window.downgrade();
        let app = app.clone();
        update_btn.connect_clicked(move |_| {
            let Some(window) = window_weak.upgrade() else {
                return;
            };
            let build = lantern_core::update::BuildInfo::current();
            let dlg = gtk::Window::builder()
                .transient_for(&window)
                .modal(true)
                .title("Lantern updates")
                .default_width(460)
                .build();
            let v = gtk::Box::new(gtk::Orientation::Vertical, 12);
            v.set_margin_top(18);
            v.set_margin_bottom(18);
            v.set_margin_start(18);
            v.set_margin_end(18);
            let build_lbl = gtk::Label::new(Some(&format!(
                "Build {} · {}{}",
                build.commit,
                build.date,
                match &build.repo {
                    Some(p) => format!(" · built from {}", p.display()),
                    None => String::new(),
                }
            )));
            build_lbl.set_wrap(true);
            build_lbl.set_xalign(0.0);
            build_lbl.add_css_class("dim-label");
            let status = gtk::Label::new(Some("Asking GitHub…"));
            status.set_wrap(true);
            status.set_xalign(0.0);
            let commits = gtk::Label::new(None);
            commits.set_wrap(true);
            commits.set_xalign(0.0);
            commits.add_css_class("dim-label");
            commits.set_visible(false);
            let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            buttons.set_halign(gtk::Align::End);
            let close = gtk::Button::with_label("Close");
            let apply = gtk::Button::with_label("Update and reopen");
            apply.add_css_class("suggested-action");
            apply.set_visible(false);
            buttons.append(&close);
            buttons.append(&apply);
            v.append(&build_lbl);
            v.append(&status);
            v.append(&commits);
            v.append(&buttons);
            dlg.set_child(Some(&v));

            {
                let dlg_weak = dlg.downgrade();
                close.connect_clicked(move |_| {
                    if let Some(d) = dlg_weak.upgrade() {
                        d.close();
                    }
                });
            }

            // The check does network I/O; run it on the runtime and bring the
            // answer back to the main context.
            let core = Arc::clone(&backend.core);
            let (tx, rx) = async_channel::bounded::<(String, bool, Vec<String>)>(1);
            backend.rt.spawn(async move {
                let c = core.check_update().await;
                let _ = tx.send((c.summary(), c.can_update(), c.commits)).await;
            });
            {
                let status = status.clone();
                let commits_lbl = commits.clone();
                let apply = apply.clone();
                glib::MainContext::default().spawn_local(async move {
                    if let Ok((summary, can_update, lines)) = rx.recv().await {
                        status.set_text(&summary);
                        if !lines.is_empty() {
                            commits_lbl.set_text(&lines.join("\n"));
                            commits_lbl.set_visible(true);
                        }
                        apply.set_visible(can_update);
                    }
                });
            }

            let backend = Rc::clone(&backend);
            let app = app.clone();
            let status_for_apply = status.clone();
            let commits_for_apply = commits.clone();
            apply.connect_clicked(move |apply| {
                match backend.core.start_update() {
                    Ok(()) => {
                        apply.set_visible(false);
                        commits_for_apply.set_visible(false);
                        status_for_apply.set_text(
                            "Updating. Lantern will close, rebuild itself from \
                             GitHub, and reopen — a few minutes. Progress is in \
                             ~/.lantern/update.log.",
                        );
                        // Out of the updater's way: it can't replace a running
                        // binary, and on Linux the OS refuses outright.
                        let app = app.clone();
                        glib::timeout_add_seconds_local_once(3, move || app.quit());
                    }
                    Err(reason) => status_for_apply.set_text(&reason),
                }
            });
            dlg.present();
        });
    }

    // ---- how the last update went ---------------------------------------
    // The app that asked for an update is never the app that sees it finish,
    // so this run reports it — once.
    if let Some(last) = lantern_core::update::take_unseen_result(backend.core.data_dir()) {
        let banner = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        banner.set_margin_top(8);
        banner.set_margin_bottom(8);
        banner.set_margin_start(12);
        banner.set_margin_end(12);
        let text = if last.succeeded() {
            format!("✓ {}", last.message)
        } else {
            format!(
                "⚠ The last update didn't finish: {} Your previous Lantern is \
                 what's running. Details in ~/.lantern/update.log.",
                last.message
            )
        };
        let lbl = gtk::Label::new(Some(&text));
        lbl.set_wrap(true);
        lbl.set_xalign(0.0);
        lbl.set_hexpand(true);
        let dismiss = gtk::Button::from_icon_name("window-close-symbolic");
        dismiss.add_css_class("flat");
        dismiss.set_tooltip_text(Some("Dismiss"));
        banner.append(&lbl);
        banner.append(&dismiss);
        let banner_weak = banner.downgrade();
        let right_weak = right.downgrade();
        dismiss.connect_clicked(move |_| {
            if let (Some(b), Some(r)) = (banner_weak.upgrade(), right_weak.upgrade()) {
                r.remove(&b);
            }
        });
        right.prepend(&banner);
    }

    // ---- core events → UI ----------------------------------------------
    if let Some(rx) = event_rx {
        let state = Rc::clone(&state);
        let refresh_peers = refresh_peers.clone();
        let append_text = append_text.clone();
        let append_file = append_file.clone();
        glib::MainContext::default().spawn_local(async move {
            while let Ok(ev) = rx.recv().await {
                match ev {
                    CoreEvent::PeerSeen { id, name, host, .. } => {
                        let mut st = state.borrow_mut();
                        if let Some(p) = st.peers.iter_mut().find(|(pid, ..)| *pid == id) {
                            p.1 = name;
                            p.2 = host;
                        } else {
                            st.peers.push((id, name, host));
                            st.peers.sort_by(|a, b| a.1.cmp(&b.1));
                        }
                        drop(st);
                        refresh_peers();
                    }
                    CoreEvent::MessageReceived { peer, peer_name, text, mid, .. } => {
                        let open = state.borrow().selected == Some(peer);
                        if open {
                            append_text(&peer_name, &text, Some(mid));
                        } else {
                            *state.borrow_mut().unread.entry(peer).or_insert(0) += 1;
                            refresh_peers();
                        }
                    }
                    CoreEvent::FileOffered { peer, peer_name, xid, name, .. } => {
                        let open = state.borrow().selected == Some(peer);
                        if open {
                            append_file(&peer_name, &xid.to_string(), &name, "receiving…");
                        } else {
                            *state.borrow_mut().unread.entry(peer).or_insert(0) += 1;
                            refresh_peers();
                        }
                    }
                    CoreEvent::FileReceived { xid, path, .. } => {
                        if let Some(lbl) =
                            state.borrow().file_rows.get(&xid.to_string())
                        {
                            // Report where it actually went, rather than a
                            // literal that goes stale the moment the
                            // download directory is configurable.
                            let dir = path
                                .parent()
                                .map(abbreviate_home)
                                .unwrap_or_else(|| "the download folder".into());
                            lbl.set_text(&format!("✓ saved to {dir}"));
                        }
                    }
                    CoreEvent::TransferProgress {
                        xid, done, total, bps, eta_s, ..
                    } => {
                        if let Some(lbl) = state.borrow().file_rows.get(&xid.to_string()) {
                            lbl.set_text(&progress_line(done, total, bps, eta_s));
                        }
                    }
                    CoreEvent::ChunksSent { xid, sent, total } => {
                        if let Some(lbl) =
                            state.borrow().file_rows.get(&xid.to_string())
                        {
                            if sent < total {
                                lbl.set_text(&format!(
                                    "resume — only {sent} of {total} chunks needed"
                                ));
                            }
                        }
                    }
                    CoreEvent::FileSent { xid, ok, err } => {
                        if let Some(lbl) =
                            state.borrow().file_rows.get(&xid.to_string())
                        {
                            let text = if ok {
                                "✓ delivered & verified".to_string()
                            } else {
                                format!("✗ {}", err.unwrap_or_default())
                            };
                            lbl.set_text(&text);
                        }
                    }
                    CoreEvent::TrustWarning { detail, .. } => {
                        append_text("⚠ Trust warning", &detail, None);
                    }
                    _ => {}
                }
            }
        });
    }

    window.set_icon_name(Some("lantern"));
    window.present();
}

/// "12.0 MB of 40.0 MB · 8.4 MB/s · 3 s left" — speed and time left are
/// dropped rather than guessed when the engine hasn't measured them yet.
fn progress_line(done: u64, total: u64, bps: Option<u64>, eta_s: Option<u64>) -> String {
    let mut line = format!("{} of {}", human_size(done), human_size(total));
    if let Some(b) = bps.filter(|b| *b > 0) {
        line.push_str(&format!(" · {}/s", human_size(b)));
    }
    if let Some(s) = eta_s.filter(|s| *s > 0) {
        line.push_str(&if s < 60 {
            format!(" · {s} s left")
        } else {
            format!(" · {} min left", s / 60)
        });
    }
    line
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
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

/// The core's identity fingerprint (helper — core exposes id bytes).
fn lantern_crypto_fingerprint(core: &Core) -> [u8; 32] {
    // words_for hashes internally; reuse the identity bytes directly.
    core.identity_id()
}
