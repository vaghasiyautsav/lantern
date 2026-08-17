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
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use lantern_core::{Core, CoreConfig, CoreEvent};

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

    let app = gtk::Application::builder().application_id(APP_ID).build();
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
    let left_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
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
    let title_row = gtk::CenterBox::new();
    title_row.set_center_widget(Some(&conv_title));
    title_row.set_end_widget(Some(&verify_btn));
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
    let append_row = {
        let msg_list = msg_list.clone();
        let msg_scroll = msg_scroll.clone();
        move |title: &str, body: &gtk::Widget| {
            let row_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
            row_box.set_margin_top(6);
            row_box.set_margin_bottom(6);
            row_box.set_margin_start(12);
            row_box.set_margin_end(12);
            let head = gtk::Label::new(Some(title));
            head.set_halign(gtk::Align::Start);
            head.add_css_class("caption-heading");
            row_box.append(&head);
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
        move |title: &str, text: &str| {
            let lbl = gtk::Label::new(Some(text));
            lbl.set_halign(gtk::Align::Start);
            lbl.set_wrap(true);
            lbl.set_selectable(true);
            append_row(title, lbl.upcast_ref::<gtk::Widget>());
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
            append_row(title, boxx.upcast_ref::<gtk::Widget>());
        }
    };

    let refresh_peers = {
        let state = Rc::clone(&state);
        let peer_list = peer_list.clone();
        let empty_label = empty_label.clone();
        move || {
            let st = state.borrow();
            empty_label.set_visible(st.peers.is_empty());
            while let Some(child) = peer_list.first_child() {
                peer_list.remove(&child);
            }
            for (id, name, host) in &st.peers {
                let row = gtk::Box::new(gtk::Orientation::Vertical, 1);
                row.set_margin_top(6);
                row.set_margin_bottom(6);
                row.set_margin_start(10);
                let n = gtk::Label::new(Some(name));
                n.set_halign(gtk::Align::Start);
                let verified = lantern_core_is_verified(&state, id);
                if verified {
                    n.set_text(&format!("{name} ✓"));
                }
                let h = gtk::Label::new(Some(host));
                h.set_halign(gtk::Align::Start);
                h.add_css_class("dim-label");
                row.append(&n);
                row.append(&h);
                peer_list.append(&row);
            }
        }
    };

    // ---- selection ------------------------------------------------------
    {
        let state = Rc::clone(&state);
        let backend = Rc::clone(&backend);
        let conv_title = conv_title.clone();
        let verify_btn = verify_btn.clone();
        let msg_list = msg_list.clone();
        let append_text = append_text.clone();
        peer_list.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            let idx = row.index();
            let peer = {
                let st = state.borrow();
                st.peers.get(idx as usize).cloned()
            };
            let Some((id, name, _host)) = peer else { return };
            state.borrow_mut().selected = Some(id);
            conv_title.set_text(&name);
            verify_btn.set_sensitive(true);
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
                append_text(&title, &m.text);
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
            append_text("You · ◷", &text);
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
    {
        let state = Rc::clone(&state);
        let backend = Rc::clone(&backend);
        let window_weak = window.downgrade();
        let append_file = append_file.clone();
        attach_btn.connect_clicked(move |_| {
            let Some(peer) = state.borrow().selected else {
                return;
            };
            let Some(window) = window_weak.upgrade() else {
                return;
            };
            let dialog = gtk::FileDialog::new();
            let backend = Rc::clone(&backend);
            let append_file = append_file.clone();
            dialog.open(
                Some(&window),
                None::<&gio::Cancellable>,
                move |result| {
                    let Ok(file) = result else { return };
                    let Some(path) = file.path() else { return };
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    // Fire and let events update the row (keyed by xid);
                    // we don't know the xid until send_file returns, so
                    // use a placeholder row updated via a oneshot.
                    let core = Arc::clone(&backend.core);
                    let (tx, rx) = async_channel::bounded::<String>(1);
                    backend.rt.spawn(async move {
                        if let Ok(xid) = core.send_file(peer, &path).await {
                            let _ = tx.send(xid.to_string()).await;
                        }
                    });
                    let append_file = append_file.clone();
                    let name2 = name.clone();
                    glib::MainContext::default().spawn_local(async move {
                        if let Ok(xid) = rx.recv().await {
                            append_file("You", &xid, &name2, "sending…");
                        }
                    });
                },
            );
        });
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
                    CoreEvent::MessageReceived { peer, peer_name, text, .. }
                        if state.borrow().selected == Some(peer) => {
                            append_text(&peer_name, &text);
                        }
                    CoreEvent::FileOffered { peer, peer_name, xid, name, .. }
                        if state.borrow().selected == Some(peer) => {
                            append_file(&peer_name, &xid.to_string(), &name, "receiving…");
                        }
                    CoreEvent::FileReceived { xid, .. } => {
                        if let Some(lbl) =
                            state.borrow().file_rows.get(&xid.to_string())
                        {
                            lbl.set_text("✓ saved to ~/.lantern/downloads");
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
                        append_text("⚠ Trust warning", &detail);
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

fn lantern_core_is_verified(state: &Rc<RefCell<UiState>>, _id: &[u8; 32]) -> bool {
    let _ = state;
    false // verified badge refresh arrives with the roster rework
}
