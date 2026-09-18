#![deny(clippy::unwrap_used, clippy::expect_used)]

use crate::DATA_DIR;
use crate::HIDDEN_HWNDS;
use crate::HIDING_BEHAVIOUR;
use crate::HidingBehaviour;
use crate::NotificationEvent;
use crate::Window;
use crate::WindowManager;
use crate::WindowManagerEvent;
use crate::border_manager;
use crate::current_virtual_desktop;
use crate::notify_subscribers;
use crate::windows_api::WindowsApi;
use crate::winevent::WinEvent;

use crossbeam_channel::Receiver;
use crossbeam_channel::Sender;
use lazy_static::lazy_static;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

/// How long a window must continuously be reported on another virtual desktop before it is
/// reaped. This absorbs the moment during a desktop switch where the shell has already moved
/// windows but the registry still reports the previous desktop
const OTHER_DESKTOP_GRACE: Duration = Duration::from_millis(1000);
/// Minimum interval between shell queries for the same candidate window
const OTHER_DESKTOP_POLL: Duration = Duration::from_millis(250);

lazy_static! {
    pub static ref HWNDS_CACHE: Arc<Mutex<HashMap<isize, (usize, usize)>>> =
        Arc::new(Mutex::new(HashMap::new()));
}

pub struct ReaperNotification(pub HashMap<isize, (usize, usize)>);

static CHANNEL: OnceLock<(Sender<ReaperNotification>, Receiver<ReaperNotification>)> =
    OnceLock::new();

pub fn channel() -> &'static (Sender<ReaperNotification>, Receiver<ReaperNotification>) {
    CHANNEL.get_or_init(|| crossbeam_channel::bounded(50))
}

fn event_tx() -> Sender<ReaperNotification> {
    channel().0.clone()
}

fn event_rx() -> Receiver<ReaperNotification> {
    channel().1.clone()
}

pub fn send_notification(hwnds: HashMap<isize, (usize, usize)>) {
    if event_tx().try_send(ReaperNotification(hwnds)).is_err() {
        tracing::warn!("channel is full; dropping notification")
    }
}

pub fn listen_for_notifications(
    wm: Arc<Mutex<WindowManager>>,
    known_hwnds: HashMap<isize, (usize, usize)>,
    virtual_desktop_id: Option<Vec<u8>>,
) {
    watch_for_orphans(known_hwnds, virtual_desktop_id);

    std::thread::spawn(move || {
        loop {
            match handle_notifications(wm.clone()) {
                Ok(()) => {
                    tracing::warn!("restarting finished thread");
                }
                Err(error) => {
                    tracing::warn!("restarting failed thread: {}", error);
                }
            }
        }
    });
}

fn handle_notifications(wm: Arc<Mutex<WindowManager>>) -> color_eyre::Result<()> {
    tracing::info!("listening");

    let receiver = event_rx();

    for notification in receiver {
        let orphan_hwnds = notification.0;
        let mut wm = wm.lock();

        let mut update_borders = false;

        for (hwnd, (m_idx, w_idx)) in orphan_hwnds.iter() {
            if let Some(monitor) = wm.monitors_mut().get_mut(*m_idx) {
                let focused_workspace_idx = monitor.focused_workspace_idx();

                if let Some(workspace) = monitor.workspaces_mut().get_mut(*w_idx) {
                    // Remove orphan window
                    if let Err(error) = workspace.remove_window(*hwnd) {
                        tracing::warn!(
                            "error reaping orphan window ({}) on monitor: {}, workspace: {}. Error: {}",
                            hwnd,
                            m_idx,
                            w_idx,
                            error,
                        );
                    }

                    if focused_workspace_idx == *w_idx {
                        // If this is not a focused workspace there is no need to update the
                        // workspace or the borders. That will already be done when the user
                        // changes to this workspace.
                        workspace.update()?;
                        update_borders = true;
                    }
                    tracing::info!(
                        "reaped orphan window ({}) on monitor: {}, workspace: {}",
                        hwnd,
                        m_idx,
                        w_idx,
                    );
                }
            }

            wm.known_hwnds.remove(hwnd);

            let window = Window::from(*hwnd);
            notify_subscribers(
                crate::Notification {
                    event: NotificationEvent::WindowManager(WindowManagerEvent::Destroy(
                        WinEvent::ObjectDestroy,
                        window,
                    )),
                    state: wm.as_ref().into(),
                },
                true,
            )?;
        }

        if update_borders {
            border_manager::send_notification(None);
        }

        // Save to file
        let hwnd_json = DATA_DIR.join("komorebi.hwnd.json");
        let file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .open(hwnd_json)?;

        serde_json::to_writer_pretty(&file, &wm.known_hwnds.keys().collect::<Vec<_>>())?;
    }

    Ok(())
}

fn watch_for_orphans(
    known_hwnds: HashMap<isize, (usize, usize)>,
    virtual_desktop_id: Option<Vec<u8>>,
) {
    // Cache current hwnds
    {
        let mut cache = HWNDS_CACHE.lock();
        *cache = known_hwnds;
    }

    std::thread::spawn(move || {
        WindowsApi::initialise_com_for_thread();
        loop {
            match find_orphans(virtual_desktop_id.as_ref()) {
                Ok(()) => {
                    tracing::warn!("restarting finished thread");
                }
                Err(error) => {
                    if cfg!(debug_assertions) {
                        tracing::error!("restarting failed thread: {:?}", error)
                    } else {
                        tracing::error!("restarting failed thread: {}", error)
                    }
                }
            }
        }
    });
}

fn find_orphans(virtual_desktop_id: Option<&Vec<u8>>) -> color_eyre::Result<()> {
    tracing::info!("watching");

    // komorebi is bound to the virtual desktop it started on and ignores events while another
    // desktop is active. A managed window moved to another desktop therefore never gets its
    // Cloak/Hide event processed and lingers as a ghost tile. Ask the shell directly, but only
    // for cloaked windows komorebi did not hide itself, and only while our own desktop is active
    let virtual_desktop_manager = match virtual_desktop_id {
        Some(_) => match WindowsApi::virtual_desktop_manager() {
            Ok(manager) => Some(manager),
            Err(error) => {
                tracing::warn!(
                    "could not create IVirtualDesktopManager; windows moved to other virtual desktops will not be reaped: {error}"
                );
                None
            }
        },
        None => None,
    };

    let mut off_desktop_since: HashMap<isize, Instant> = HashMap::new();
    let mut last_off_desktop_check: HashMap<isize, Instant> = HashMap::new();

    loop {
        std::thread::sleep(Duration::from_millis(20));
        let hiding_behaviour = *HIDING_BEHAVIOUR.lock();

        let mut cache = HWNDS_CACHE.lock();
        let mut orphan_hwnds = HashMap::new();

        if let (Some(manager), Some(our_desktop)) = (&virtual_desktop_manager, virtual_desktop_id) {
            let candidates = {
                let hidden = HIDDEN_HWNDS.lock();
                cache
                    .keys()
                    .copied()
                    .filter(|hwnd| !hidden.contains(hwnd))
                    .filter(|hwnd| Window::from(*hwnd).is_cloaked().unwrap_or(false))
                    .collect::<Vec<_>>()
            };

            off_desktop_since.retain(|hwnd, _| candidates.contains(hwnd));
            last_off_desktop_check.retain(|hwnd, _| candidates.contains(hwnd));

            if !candidates.is_empty() {
                // never act while the user is looking at a different desktop: everything of ours
                // is legitimately off-desktop then
                if current_virtual_desktop().as_ref() == Some(our_desktop) {
                    let now = Instant::now();
                    for hwnd in candidates {
                        let due = last_off_desktop_check
                            .get(&hwnd)
                            .is_none_or(|last| now.duration_since(*last) >= OTHER_DESKTOP_POLL);
                        if !due {
                            continue;
                        }

                        last_off_desktop_check.insert(hwnd, now);

                        match WindowsApi::is_window_on_current_virtual_desktop(manager, hwnd) {
                            Ok(false) => {
                                let since = *off_desktop_since.entry(hwnd).or_insert(now);
                                if now.duration_since(since) >= OTHER_DESKTOP_GRACE
                                    && let Some((m_idx, w_idx)) = cache.get(&hwnd)
                                {
                                    tracing::info!(
                                        "reaping window {hwnd} which has been moved to another virtual desktop"
                                    );
                                    orphan_hwnds.insert(hwnd, (*m_idx, *w_idx));
                                }
                            }
                            Ok(true) => {
                                off_desktop_since.remove(&hwnd);
                            }
                            Err(error) => {
                                tracing::debug!(
                                    "could not query virtual desktop for window {hwnd}: {error}"
                                );
                            }
                        }
                    }
                } else {
                    off_desktop_since.clear();
                }
            }
        }

        for (hwnd, (m_idx, w_idx)) in cache.iter() {
            let window = Window::from(*hwnd);

            #[allow(deprecated)]
            if !window.is_window()
                || (
                    // This one is a hack because WINWORD.EXE is an absolute trainwreck of an app
                    // when multiple docs are open, it keeps open an invisible window, with WS_EX_LAYERED
                    // (A STYLE THAT THE REGULAR WINDOWS NEED IN ORDER TO BE MANAGED!) when one of the
                    // docs is closed
                    //
                    // I hate every single person who worked on Microsoft Office 365, especially Word
                    !window.is_visible()
                    // We cannot execute this lovely hack if the user is using HidingBehaviour::Hide because
                    // it will result in legitimate hidden, non-visible windows being yeeted from the state
                    && !matches!(hiding_behaviour, HidingBehaviour::Hide)
                )
            {
                orphan_hwnds.insert(window.hwnd, (*m_idx, *w_idx));
            }
        }

        if !orphan_hwnds.is_empty() {
            // Update reaper cache
            cache.retain(|h, _| !orphan_hwnds.contains_key(h));

            // Send handles to remove
            event_tx().send(ReaperNotification(orphan_hwnds))?;
        }
    }
}
