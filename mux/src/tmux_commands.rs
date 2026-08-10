use crate::domain::{DomainId, WriterWrapper};
use crate::localpane::LocalPane;
use crate::pane::{alloc_pane_id, PaneId};
use crate::tab::{SplitDirection, SplitRequest, SplitSize, Tab, TabId};
use crate::tmux::{AttachState, TmuxDomain, TmuxDomainState, TmuxRemotePane, TmuxTab};
use crate::tmux_pty::{TmuxChild, TmuxPty};
use crate::{Mux, MuxNotification, Pane};
use anyhow::{anyhow, Context};
use parking_lot::{Condvar, Mutex};
use portable_pty::{MasterPty, PtySize};
use std::collections::HashSet;
use std::fmt::{Debug, Write};
use std::io::Write as _;
use std::sync::Arc;
use termwiz::escape::csi::{Cursor, DecPrivateMode, DecPrivateModeCode, Mode, CSI};
use termwiz::escape::{Action, OneBased, OperatingSystemCommand};
use termwiz::tmux_cc::*;
use url::Url;
use wezterm_term::TerminalSize;

pub(crate) trait TmuxCommand: Send + Debug {
    fn get_command(&self, domain_id: DomainId) -> String;
    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()>;
}

pub(crate) const PANE_CWD_SUBSCRIPTION: &str = "wezterm-pane-cwd";
pub(crate) const PANE_COMMAND_SUBSCRIPTION: &str = "wezterm-pane-command";

fn pane_command(command: &str) -> Option<String> {
    (!command.is_empty()).then(|| command.to_owned())
}

#[derive(Debug, Clone)]
pub(crate) struct PaneItem {
    session_id: TmuxSessionId,
    window_id: TmuxWindowId,
    pane_id: TmuxPaneId,
    _pane_index: u64,
    cursor_x: u64,
    cursor_y: u64,
    pane_width: u64,
    pane_height: u64,
    pane_left: u64,
    pane_top: u64,
    pane_active: bool,
    mouse_standard: bool,
    mouse_button: bool,
    mouse_all: bool,
    mouse_utf8: bool,
    mouse_sgr: bool,
    current_command: Option<String>,
    current_path: Option<String>,
}

fn tmux_mouse_mode_actions(pane: &PaneItem) -> Vec<Action> {
    let modes = [
        (pane.mouse_standard, DecPrivateModeCode::MouseTracking),
        (pane.mouse_button, DecPrivateModeCode::ButtonEventMouse),
        (pane.mouse_all, DecPrivateModeCode::AnyEventMouse),
        (pane.mouse_utf8, DecPrivateModeCode::Utf8Mouse),
        (pane.mouse_sgr, DecPrivateModeCode::SGRMouse),
    ];
    let mut actions = Vec::with_capacity(modes.len() * 2);
    for (_, mode) in &modes {
        actions.push(Action::CSI(CSI::Mode(Mode::ResetDecPrivateMode(
            DecPrivateMode::Code(mode.clone()),
        ))));
    }
    for (enabled, mode) in modes {
        if enabled {
            actions.push(Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(
                DecPrivateMode::Code(mode),
            ))));
        }
    }
    actions
}

fn tmux_current_path_action(path: &str) -> Option<Action> {
    let url = Url::from_directory_path(path).ok()?;
    Some(Action::OperatingSystemCommand(Box::new(
        OperatingSystemCommand::CurrentWorkingDirectory(url.into()),
    )))
}

fn set_tmux_pane_current_path(pane: &Arc<dyn Pane>, path: Option<&str>) {
    if let Some(action) = path.and_then(tmux_current_path_action) {
        pane.perform_actions(vec![action]);
    }
}

#[derive(Debug)]
struct WindowItem {
    session_id: TmuxSessionId,
    window_id: TmuxWindowId,
    window_width: u64,
    window_height: u64,
    window_active: bool,
    window_name: String,
    layout: Vec<WindowLayout>,
    layout_csum: String,
    history_limit: isize,
}

impl TmuxDomainState {
    pub fn pane_current_command(&self, local_pane_id: PaneId) -> Option<String> {
        self.remote_panes.lock().values().find_map(|pane| {
            let pane = pane.lock();
            (pane.local_pane_id == local_pane_id)
                .then(|| pane.current_command.clone())
                .flatten()
        })
    }

    pub fn update_pane_current_command(
        &self,
        session_id: TmuxSessionId,
        window_id: TmuxWindowId,
        pane_id: TmuxPaneId,
        command: &str,
    ) {
        if *self.tmux_session.lock() != Some(session_id) {
            return;
        }
        if let Some(pane) = self.remote_panes.lock().get(&pane_id) {
            let mut pane = pane.lock();
            if pane.window_id == window_id {
                pane.current_command = pane_command(command);
            }
        }
    }

    pub fn update_pane_current_path(
        &self,
        session_id: TmuxSessionId,
        window_id: TmuxWindowId,
        pane_id: TmuxPaneId,
        path: &str,
    ) {
        if *self.tmux_session.lock() != Some(session_id) {
            return;
        }
        let local_pane_id = self.remote_panes.lock().get(&pane_id).and_then(|pane| {
            let pane = pane.lock();
            (pane.window_id == window_id).then_some(pane.local_pane_id)
        });
        if let Some(pane) = local_pane_id.and_then(|pane_id| Mux::get().get_pane(pane_id)) {
            set_tmux_pane_current_path(&pane, Some(path));
        }
    }

    /// check if a PaneItem received from ListAllPanes has been attached
    pub fn check_pane_attached(&self, window_id: TmuxWindowId, pane_id: TmuxPaneId) -> bool {
        let gui_tabs = self.gui_tabs.lock();
        let Some(local_tab) = gui_tabs.get(&window_id) else {
            return false;
        };

        return local_tab.panes.get(&pane_id).is_some();
    }

    pub fn check_window_attached(&self, window_id: TmuxWindowId) -> bool {
        let gui_tabs = self.gui_tabs.lock();
        return gui_tabs.get(&window_id).is_some();
    }

    /// after we create a tab for a remote pane, save its ID into the
    /// TmuxPane-TmuxPane tree, so we can ref it later.
    fn add_attached_pane(
        &self,
        window_id: TmuxWindowId,
        pane_id: TmuxPaneId,
    ) -> anyhow::Result<()> {
        let mut gui_tabs = self.gui_tabs.lock();

        let panes = match gui_tabs.get_mut(&window_id) {
            Some(tab) => &mut tab.panes,
            None => anyhow::bail!("The window {window_id} is not attached"),
        };

        match panes.get(&pane_id) {
            Some(_) => {
                anyhow::bail!("Tmux pane already attached");
            }
            None => {
                panes.insert(pane_id);
                return Ok(());
            }
        }
    }

    fn add_attached_window(&self, target: &WindowItem, tab_id: &TabId) -> anyhow::Result<()> {
        let mut gui_tabs = self.gui_tabs.lock();
        if !gui_tabs.contains_key(&target.window_id) {
            gui_tabs.insert(
                target.window_id,
                TmuxTab {
                    tab_id: *tab_id,
                    tmux_window_id: target.window_id,
                    layout_csum: target.layout_csum.clone(),
                    panes: HashSet::new(),
                },
            );
        }

        Ok(())
    }

    fn remove_detached_pane(
        &self,
        window_id: TmuxWindowId,
        new_set: &HashSet<TmuxPaneId>,
    ) -> anyhow::Result<()> {
        let mut gui_tabs = self.gui_tabs.lock();

        let (tab_id, panes) = match gui_tabs.get_mut(&window_id) {
            Some(tab) => (tab.tab_id, &mut tab.panes),
            None => anyhow::bail!("The window {window_id} is not attached"),
        };

        let to_remove: Vec<_> = panes.difference(new_set).cloned().collect();

        let mux = Mux::get();
        for p in to_remove {
            let pane_map = self.remote_panes.lock();
            let Some(pane) = pane_map.get(&p) else {
                continue;
            };
            let local_pane_id = pane.lock().local_pane_id;
            mux.remove_pane(local_pane_id);
            panes.remove(&p);
        }

        if panes.is_empty() {
            mux.remove_tab(tab_id);
            gui_tabs.remove(&window_id);
        }

        Ok(())
    }

    pub fn remove_detached_window(&self, window_id: TmuxWindowId) -> anyhow::Result<()> {
        let mut gui_tabs = self.gui_tabs.lock();
        let tab = match gui_tabs.get(&window_id) {
            Some(x) => x,
            None => {
                anyhow::bail!("Cannot find the window {window_id}")
            }
        };

        let mux = Mux::get();
        mux.remove_tab(tab.tab_id);
        gui_tabs.remove(&window_id);

        Ok(())
    }

    fn set_pane_cursor_position(&self, pane: &Arc<dyn Pane>, x: usize, y: usize) {
        pane.perform_actions(vec![Action::CSI(CSI::Cursor(
            Cursor::CharacterAndLinePosition {
                col: OneBased::from_zero_based(x as u32),
                line: OneBased::from_zero_based(y as u32),
            },
        ))]);
    }

    fn create_pane(&self, pane: &PaneItem) -> anyhow::Result<Arc<dyn Pane>> {
        let local_pane_id = alloc_pane_id();
        let active_lock = Arc::new((Mutex::new(false), Condvar::new()));
        let (output_read, output_write) = filedescriptor::socketpair()?;
        let ref_pane = Arc::new(Mutex::new(TmuxRemotePane {
            local_pane_id,
            output_write,
            active_lock: active_lock.clone(),
            session_id: 0,
            window_id: pane.window_id,
            pane_id: pane.pane_id,
            cursor_x: pane.cursor_x,
            cursor_y: pane.cursor_y,
            pane_width: pane.pane_width,
            pane_height: pane.pane_height,
            pane_left: pane.pane_left,
            pane_top: pane.pane_top,
            current_command: pane.current_command.clone(),
        }));

        {
            let mut pane_map = self.remote_panes.lock();
            pane_map.insert(pane.pane_id, ref_pane.clone());
        }

        let pane_pty = TmuxPty {
            domain_id: self.domain_id,
            reader: output_read,
            cmd_queue: self.cmd_queue.clone(),
            master_pane: ref_pane,
        };

        let writer = WriterWrapper::new(pane_pty.take_writer()?);

        let size = TerminalSize {
            rows: pane.pane_height as usize,
            cols: pane.pane_width as usize,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        };

        let child = TmuxChild {
            active_lock: active_lock.clone(),
        };

        let terminal = wezterm_term::Terminal::new(
            size,
            std::sync::Arc::new(config::TermConfig::new()),
            "WezTerm",
            config::wezterm_version(),
            Box::new(writer.clone()),
        );

        let local_pane: Arc<dyn Pane> = Arc::new(LocalPane::new(
            local_pane_id,
            terminal,
            Box::new(child),
            Box::new(pane_pty),
            Box::new(writer),
            self.domain_id,
            "tmux pane".to_string(),
        ));
        set_tmux_pane_current_path(&local_pane, pane.current_path.as_deref());
        Ok(local_pane)
    }

    pub fn split_pane(
        &self,
        tab_id: TabId,
        pane_id: PaneId,
        remote_id: TmuxPaneId,
        split_request: SplitRequest,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let mux = Mux::get();
        let tab = match mux.get_tab(tab_id) {
            Some(t) => t,
            None => anyhow::bail!("Invalid tab id {}", tab_id),
        };

        let pane_index = match tab
            .iter_panes_ignoring_zoom()
            .iter()
            .find(|p| p.pane.pane_id() == pane_id)
        {
            Some(p) => p.index,
            None => anyhow::bail!("invalid pane id {}", pane_id),
        };

        let split_size = match tab.compute_split_size(pane_index, split_request) {
            Some(s) => s,
            None => anyhow::bail!("invalid pane index {}", pane_index),
        };

        let window_id = match self.gui_tabs.lock().iter().find(|t| t.1.tab_id == tab_id) {
            Some((_, tab)) => tab.tmux_window_id,
            None => anyhow::bail!("No tab {}", tab_id),
        };

        let p = PaneItem {
            session_id: 0,
            window_id: window_id,
            pane_id: remote_id,
            _pane_index: 0,
            cursor_x: 0,
            cursor_y: 0,
            pane_width: split_size.second.cols as u64,
            pane_height: split_size.second.rows as u64,
            pane_left: 0,
            pane_top: 0,
            pane_active: false,
            mouse_standard: false,
            mouse_button: false,
            mouse_all: false,
            mouse_utf8: false,
            mouse_sgr: false,
            current_command: None,
            current_path: None,
        };

        let pane = self.create_pane(&p).context("failed to create pane")?;
        tab.split_and_insert(pane_index, split_request, Arc::clone(&pane))?;

        self.add_attached_pane(window_id, remote_id)?;

        let _ = mux.add_pane(&pane);

        return Ok(pane);
    }

    fn sync_pane_state(&self, panes: &[PaneItem]) -> anyhow::Result<()> {
        let Some(current_session) = *self.tmux_session.lock() else {
            return Ok(());
        };
        let mux = Mux::get();

        for pane in panes.iter() {
            if pane.session_id != current_session
                || !self.check_pane_attached(pane.window_id, pane.pane_id)
            {
                continue;
            }

            self.update_pane_current_command(
                pane.session_id,
                pane.window_id,
                pane.pane_id,
                pane.current_command.as_deref().unwrap_or_default(),
            );

            // We now have the cursor information, fix the cursor position
            let pane_map = self.remote_panes.lock();
            let local_pane = match pane_map.get(&pane.pane_id) {
                Some(p) => {
                    let local_pane_id = p.lock().local_pane_id;
                    mux.get_pane(local_pane_id)
                }
                None => None,
            };

            if let Some(local_pane) = local_pane {
                let c = local_pane.get_cursor_position();
                // no capture, output case
                if (c.x + c.y as usize) == 0 {
                    if let Some(text) = self.backlog.lock().remove(&pane.pane_id) {
                        if let Some(ref_pane) = pane_map.get(&pane.pane_id) {
                            let mut ref_pane = ref_pane.lock();
                            if let Err(err) = ref_pane.output_write.write_all(&text) {
                                log::error!("Failed to write tmux data to output: {:#}", err);
                            }
                        }
                    }
                } else {
                    // we have capture, so remove the backlog
                    let _ = self.backlog.lock().remove(&pane.pane_id);
                    if (pane.cursor_x + pane.cursor_y) != 0 {
                        self.set_pane_cursor_position(
                            &local_pane,
                            pane.cursor_x as usize,
                            pane.cursor_y as usize,
                        );
                    }
                }
                if pane.pane_active {
                    let gui_tabs = self.gui_tabs.lock();

                    let Some(local_tab) = gui_tabs.get(&pane.window_id) else {
                        anyhow::bail!("invalid tmux window id {}", pane.window_id);
                    };

                    match mux.get_tab(local_tab.tab_id) {
                        Some(tab) => {
                            tab.set_active_pane(&local_pane);
                            mux.notify(MuxNotification::PaneFocused(local_pane.pane_id()));
                        }
                        None => {}
                    }
                }
                // tmux retains these per pane but does not replay them to new control clients.
                local_pane.perform_actions(tmux_mouse_mode_actions(pane));
                set_tmux_pane_current_path(&local_pane, pane.current_path.as_deref());
            }

            log::info!("new pane synced, id: {}", pane.pane_id);
        }

        Ok(())
    }

    fn sync_window_state(&self, windows: &[WindowItem], new_window: bool) -> anyhow::Result<()> {
        let Some(current_session) = *self.tmux_session.lock() else {
            return Ok(());
        };
        let mux = Mux::get();

        self.create_gui_window();
        let mut gui_window = self.gui_window.lock();
        let gui_window_id = match gui_window.as_mut() {
            Some(x) => x,
            None => {
                anyhow::bail!("No tmux gui created");
            }
        };

        for window in windows.iter() {
            if window.session_id != current_session {
                continue;
            }

            let size = TerminalSize {
                rows: window.window_height as usize,
                cols: window.window_width as usize,
                pixel_width: 0,
                pixel_height: 0,
                dpi: 0,
            };

            let tab = Arc::new(Tab::new(&size));
            tab.set_title(&format!("{}", &window.window_name));
            mux.add_tab_no_panes(&tab);

            let _ = self.add_attached_window(window, &tab.tab_id())?;

            let mut split_stack;
            let mut split_direction;

            let mut split_pane_index = 1;
            for l in &window.layout {
                match l {
                    WindowLayout::SinglePane(x) => {
                        let p = PaneItem {
                            session_id: window.session_id,
                            window_id: window.window_id,
                            _pane_index: 0,
                            cursor_x: 0,
                            cursor_y: 0,
                            pane_active: false,
                            pane_id: x.pane_id,
                            pane_width: x.pane_width,
                            pane_height: x.pane_height,
                            pane_left: x.pane_left,
                            pane_top: x.pane_top,
                            mouse_standard: false,
                            mouse_button: false,
                            mouse_all: false,
                            mouse_utf8: false,
                            mouse_sgr: false,
                            current_command: None,
                            current_path: None,
                        };
                        let local_pane = self.create_pane(&p).context("failed to create pane")?;
                        tab.assign_pane(&local_pane);
                        self.add_attached_pane(p.window_id, p.pane_id)?;
                        let _ = mux.add_pane(&local_pane);
                        break;
                    }

                    WindowLayout::SplitHorizontal(x) => {
                        split_direction = SplitDirection::Horizontal;
                        split_stack = x;
                    }

                    WindowLayout::SplitVertical(x) => {
                        split_direction = SplitDirection::Vertical;
                        split_stack = x;
                    }
                }

                for x in split_stack {
                    let p = PaneItem {
                        session_id: window.session_id,
                        window_id: window.window_id,
                        _pane_index: 0,
                        cursor_x: 0,
                        cursor_y: 0,
                        pane_active: false,
                        pane_id: x.pane_id,
                        pane_width: x.pane_width,
                        pane_height: x.pane_height,
                        pane_left: x.pane_left,
                        pane_top: x.pane_top,
                        mouse_standard: false,
                        mouse_button: false,
                        mouse_all: false,
                        mouse_utf8: false,
                        mouse_sgr: false,
                        current_command: None,
                        current_path: None,
                    };
                    let local_pane;
                    if !self.check_pane_attached(p.window_id, p.pane_id) {
                        local_pane = self.create_pane(&p).context("failed to create pane")?;
                        self.add_attached_pane(p.window_id, p.pane_id)?;
                        let _ = mux.add_pane(&local_pane);
                        if let None = tab.get_active_pane() {
                            tab.assign_pane(&local_pane);
                            split_pane_index = tab.get_active_idx();
                            continue;
                        }

                        split_pane_index = tab.split_and_insert(
                            split_pane_index,
                            SplitRequest {
                                direction: split_direction,
                                target_is_second: false,
                                top_level: false,
                                size: SplitSize::Cells(
                                    if split_direction == SplitDirection::Horizontal {
                                        p.pane_width as usize
                                    } else {
                                        p.pane_height as usize
                                    },
                                ),
                            },
                            local_pane.clone(),
                        )? + 1;
                    } else {
                        let pane_map = self.remote_panes.lock();
                        let local_pane_id = match pane_map.get(&p.pane_id) {
                            Some(x) => x.lock().local_pane_id,
                            None => anyhow::bail!("cannot find the local pane for {}", p.pane_id),
                        };

                        split_pane_index = match tab
                            .iter_panes_ignoring_zoom()
                            .iter()
                            .find(|x| x.pane.pane_id() == local_pane_id)
                        {
                            Some(x) => x.index,
                            None => {
                                log::info!("invalid pane id {local_pane_id}");
                                continue;
                            }
                        };
                        continue;
                    }
                }
            }

            mux.add_tab_to_window(&tab, **gui_window_id)?;
            gui_window_id.notify();

            let gui_tabs = self.gui_tabs.lock();
            let local_tab = match gui_tabs.get(&window.window_id) {
                Some(x) => x,
                None => {
                    log::info!(
                        "cannot find the local tab for tmux window {}",
                        window.window_id
                    );
                    continue;
                }
            };

            // For new window, we wait for nature ouput instead of capturing
            if !new_window {
                for p in local_tab.panes.iter() {
                    self.cmd_queue.lock().push_back(Box::new(CapturePane {
                        pane_id: *p,
                        history_limit: window.history_limit,
                    }));
                }
            }

            // To keep the active window last one to make it active after set the focus pane
            if !window.window_active {
                self.cmd_queue.lock().push_back(Box::new(ListAllPanes {
                    window_id: window.window_id,
                    prune: false,
                    layout_csum: window.layout_csum.clone(),
                }));
            }
        }

        // To keep the active window last one to make it active after set the focus pane
        match windows.iter().find(|w| w.window_active) {
            Some(window) => {
                self.cmd_queue.lock().push_back(Box::new(ListAllPanes {
                    window_id: window.window_id,
                    prune: false,
                    layout_csum: window.layout_csum.clone(),
                }));
            }
            None => {}
        }

        if *self.attach_state.lock() == AttachState::Init {
            self.cmd_queue.lock().push_back(Box::new(AttachDone));
        }

        TmuxDomainState::schedule_send_next_command(self.domain_id);

        Ok(())
    }

    pub fn subscribe_notification(&self) {
        let mux = Mux::get();
        let domain_id = self.domain_id;
        mux.subscribe(move |n| {
            promise::spawn::spawn_into_main_thread(async move {
                let mux = Mux::get();
                let domain = match mux.get_domain(domain_id) {
                    Some(d) => d,
                    None => return,
                };
                let tmux_domain = match domain.downcast_ref::<TmuxDomain>() {
                    Some(t) => t,
                    None => return,
                };

                if *tmux_domain.inner.attach_state.lock() == AttachState::Init {
                    return;
                }

                match n {
                    MuxNotification::PaneFocused(pane_id) => {
                        let tmux_pane_id = match tmux_domain
                            .inner
                            .remote_panes
                            .lock()
                            .iter()
                            .find(|(_, p)| p.lock().local_pane_id == pane_id)
                        {
                            Some((_, p)) => Some(p.lock().pane_id),
                            None => None,
                        };

                        if let Some(pane_id) = tmux_pane_id {
                            tmux_domain
                                .inner
                                .cmd_queue
                                .lock()
                                .push_back(Box::new(SelectPane { pane_id: pane_id }));
                            TmuxDomainState::schedule_send_next_command(domain_id);
                        }
                    }
                    MuxNotification::WindowInvalidated(window_id) => {
                        if let Some(window) = mux.get_window(window_id) {
                            let Some(tab) = window.get_active() else {
                                return;
                            };
                            let tmux_window_id = match tmux_domain
                                .inner
                                .gui_tabs
                                .lock()
                                .iter()
                                .find(|(_, t)| t.tab_id == tab.tab_id())
                            {
                                Some((_, t)) => Some(t.tmux_window_id),
                                None => None,
                            };
                            if let Some(window_id) = tmux_window_id {
                                tmux_domain.inner.cmd_queue.lock().push_back(Box::new(
                                    SelectWindow {
                                        window_id: window_id,
                                    },
                                ));
                                TmuxDomainState::schedule_send_next_command(domain_id);
                            }
                        }
                    }
                    _ => {}
                }
            })
            .detach();
            true
        });
    }
}

fn parse_sigil_number(text: &str) -> anyhow::Result<u64> {
    let num = text
        .get(1..)
        .ok_or_else(|| anyhow!("wrong prefixed id"))?
        .parse()?;

    Ok(num)
}

fn parse_pane_item(line: &str) -> anyhow::Result<PaneItem> {
    let mut fields = line.splitn(18, '\t');
    let session_id =
        parse_sigil_number(fields.next().ok_or_else(|| anyhow!("missing session_id"))?)?;
    let window_id = parse_sigil_number(fields.next().ok_or_else(|| anyhow!("missing window_id"))?)?;
    let pane_id = parse_sigil_number(fields.next().ok_or_else(|| anyhow!("missing pane_id"))?)?;
    let _pane_index = fields
        .next()
        .ok_or_else(|| anyhow!("missing pane_index"))?
        .parse()?;
    let cursor_x = fields
        .next()
        .ok_or_else(|| anyhow!("missing cursor_x"))?
        .parse()?;
    let cursor_y = fields
        .next()
        .ok_or_else(|| anyhow!("missing cursor_y"))?
        .parse()?;
    let pane_width = fields
        .next()
        .ok_or_else(|| anyhow!("missing pane_width"))?
        .parse()?;
    let pane_height = fields
        .next()
        .ok_or_else(|| anyhow!("missing pane_height"))?
        .parse()?;
    let pane_left = fields
        .next()
        .ok_or_else(|| anyhow!("missing pane_left"))?
        .parse()?;
    let pane_top = fields
        .next()
        .ok_or_else(|| anyhow!("missing pane_top"))?
        .parse()?;
    let pane_active = fields
        .next()
        .ok_or_else(|| anyhow!("missing pane_active"))?
        .parse::<usize>()?
        == 1;
    let mut mouse_flag = |name| -> anyhow::Result<bool> {
        Ok(fields
            .next()
            .ok_or_else(|| anyhow!("missing {name}"))?
            .parse::<usize>()?
            == 1)
    };
    let mouse_standard = mouse_flag("mouse_standard_flag")?;
    let mouse_button = mouse_flag("mouse_button_flag")?;
    let mouse_all = mouse_flag("mouse_all_flag")?;
    let mouse_utf8 = mouse_flag("mouse_utf8_flag")?;
    let mouse_sgr = mouse_flag("mouse_sgr_flag")?;
    let current_command = fields.next().and_then(pane_command);
    let current_path = fields
        .next()
        .filter(|path| !path.is_empty())
        .map(str::to_owned);

    Ok(PaneItem {
        session_id,
        window_id,
        pane_id,
        _pane_index,
        cursor_x,
        cursor_y,
        pane_width,
        pane_height,
        pane_left,
        pane_top,
        pane_active,
        mouse_standard,
        mouse_button,
        mouse_all,
        mouse_utf8,
        mouse_sgr,
        current_command,
        current_path,
    })
}

#[derive(Debug)]
pub(crate) struct ListAllPanes {
    pub window_id: TmuxWindowId,
    pub prune: bool,
    pub layout_csum: String,
}

impl TmuxCommand for ListAllPanes {
    fn get_command(&self, domain_id: DomainId) -> String {
        let mux = Mux::get();
        let domain = match mux.get_domain(domain_id) {
            Some(d) => d,
            None => return "".to_string(),
        };
        let tmux_domain = match domain.downcast_ref::<TmuxDomain>() {
            Some(t) => t,
            None => return "".to_string(),
        };

        let mut gui_tabs = tmux_domain.inner.gui_tabs.lock();

        let Some(local_tab) = gui_tabs.get_mut(&self.window_id) else {
            return "".to_string();
        };

        if local_tab.layout_csum.eq(&self.layout_csum) {
            if self.prune {
                return "".to_string();
            }
        } else {
            local_tab.layout_csum = self.layout_csum.clone();
        }

        format!(
            "list-panes -F '#{{session_id}}\t#{{window_id}}\t#{{pane_id}}\t\
            #{{pane_index}}\t#{{cursor_x}}\t#{{cursor_y}}\t#{{pane_width}}\t#{{pane_height}}\t\
            #{{pane_left}}\t#{{pane_top}}\t#{{pane_active}}\t\
            #{{?mouse_standard_flag,1,0}}\t#{{?mouse_button_flag,1,0}}\t\
            #{{?mouse_all_flag,1,0}}\t#{{?mouse_utf8_flag,1,0}}\t\
            #{{?mouse_sgr_flag,1,0}}\t#{{pane_current_command}}\t#{{pane_current_path}}' -t @{}\n",
            self.window_id
        )
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("list-pane in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        let mut items = vec![];
        let mut pane_set = HashSet::new();
        for line in result.output.split('\n') {
            if line.is_empty() {
                continue;
            }
            let pane = parse_pane_item(line)?;
            pane_set.insert(pane.pane_id);
            items.push(pane);
        }

        log::debug!("panes in domain_id {}: {:?}", domain_id, items);
        let mux = Mux::get();
        if let Some(domain) = mux.get_domain(domain_id) {
            if let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() {
                if !self.prune {
                    return tmux_domain.inner.sync_pane_state(&items);
                } else {
                    return tmux_domain
                        .inner
                        .remove_detached_pane(self.window_id, &pane_set);
                }
            }
        }
        anyhow::bail!("Tmux domain lost");
    }
}

#[derive(Debug)]
pub(crate) struct ListAllWindows {
    pub session_id: TmuxSessionId,
    pub window_id: Option<TmuxWindowId>,
}

impl TmuxCommand for ListAllWindows {
    fn get_command(&self, _domain_id: DomainId) -> String {
        format!(
            "list-windows -F \
                '#{{session_id}} #{{window_id}} \
                #{{window_width}} #{{window_height}} \
                #{{window_active}} \
                #{{window_name}} \
                #{{window_layout}} \
                #{{history_limit}}' -t ${}\n",
            self.session_id
        )
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("list-window in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        let mut items = vec![];

        for line in result.output.split('\n') {
            if line.is_empty() {
                continue;
            }
            let mut fields = line.split(' ');
            let session_id =
                parse_sigil_number(fields.next().ok_or_else(|| anyhow!("missing session_id"))?)?;
            let window_id =
                parse_sigil_number(fields.next().ok_or_else(|| anyhow!("missing window_id"))?)?;
            let window_width = fields
                .next()
                .ok_or_else(|| anyhow!("missing window_width"))?
                .parse()?;
            let window_height = fields
                .next()
                .ok_or_else(|| anyhow!("missing window_height"))?
                .parse()?;
            let window_active = fields
                .next()
                .ok_or_else(|| anyhow!("missing window_active"))?
                .parse::<usize>()?;

            let window_name = fields
                .next()
                .ok_or_else(|| anyhow!("missing window_name"))?;

            let window_layout = fields
                .next()
                .ok_or_else(|| anyhow!("missing window_layout"))?;

            let history_limit = fields
                .next()
                .ok_or_else(|| anyhow!("missing history_limit"))?
                .parse::<isize>()?;

            let window_active = window_active == 1;

            if let Some(x) = self.window_id {
                if x != window_id {
                    continue;
                }
            }

            let layout_csum = window_layout
                .get(0..4)
                .ok_or_else(|| anyhow!("missing window_layout"))?;
            let window_layout = window_layout
                .get(5..)
                .ok_or_else(|| anyhow!("missing window_layout"))?;

            let layout = parse_layout(window_layout)?;

            items.push(WindowItem {
                session_id,
                window_id,
                window_width,
                window_height,
                window_active,
                window_name: window_name.to_string(),
                layout,
                layout_csum: layout_csum.to_string(),
                history_limit,
            });
        }

        log::debug!("layout in domain_id {}: {:#?}", domain_id, items);
        let mux = Mux::get();
        if let Some(domain) = mux.get_domain(domain_id) {
            if let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() {
                let new_window = if let Some(_x) = self.window_id {
                    true
                } else {
                    false
                };
                return tmux_domain.inner.sync_window_state(&items, new_window);
            }
        }
        anyhow::bail!("Tmux domain lost");
    }
}

#[derive(Debug)]
pub(crate) struct Resize {
    pub pane_id: TmuxPaneId,
    pub size: PtySize,
}

impl TmuxCommand for Resize {
    fn get_command(&self, domain_id: DomainId) -> String {
        let mux = Mux::get();
        let domain = match mux.get_domain(domain_id) {
            Some(d) => d,
            None => return "".to_string(),
        };
        let tmux_domain = match domain.downcast_ref::<TmuxDomain>() {
            Some(t) => t,
            None => return "".to_string(),
        };

        // Not in stable state for now, don't do resizing, otherwise it will cause tmux output
        // unexpected content.
        let attach_state = tmux_domain.inner.attach_state.lock();
        if *attach_state == AttachState::Init {
            tmux_domain.inner.remember_resize(self.pane_id, self.size);
            return "".to_string();
        }
        drop(attach_state);

        let pane_map = tmux_domain.inner.remote_panes.lock();
        {
            let mut pane = match pane_map.get(&self.pane_id) {
                Some(x) => x.lock(),
                None => return "".to_string(),
            };

            if pane.pane_width == self.size.cols as u64 && pane.pane_height == self.size.rows as u64
            {
                return "".to_string();
            } else {
                pane.pane_width = self.size.cols as u64;
                pane.pane_height = self.size.rows as u64;
            }
        }

        let tmux_window_id = match pane_map.get(&self.pane_id) {
            Some(x) => x.lock().window_id,
            None => return "".to_string(),
        };

        let gui_tabs = tmux_domain.inner.gui_tabs.lock();
        let local_tab = match gui_tabs.get(&tmux_window_id) {
            Some(t) => t,
            None => return "".to_string(),
        };
        let single_pane = local_tab.panes.len() == 1;

        let size = match mux.get_tab(local_tab.tab_id) {
            Some(x) => x.get_size(),
            None => return "".to_string(),
        };

        let support_commands = tmux_domain.inner.support_commands.lock();

        if let Some(_x) = support_commands.get("resize-window") {
            if single_pane {
                format!(
                    "resize-window -x {} -y {} -t @{}\n",
                    size.cols, size.rows, tmux_window_id
                )
            } else {
                format!(
                    "resize-window -x {} -y {} -t @{}\nresize-pane -x {} -y {} -t %{}\n",
                    size.cols,
                    size.rows,
                    tmux_window_id,
                    self.size.cols,
                    self.size.rows,
                    self.pane_id
                )
            }
        } else if let Some(x) = support_commands.get("refresh-client") {
            if x.contains("-C XxY") {
                format!(
                    "refresh-client -C {}x{}\nresize-pane -x {} -y {} -t %{}\n",
                    size.cols, size.rows, self.size.cols, self.size.rows, self.pane_id
                )
            } else {
                format!(
                    "refresh-client -C {},{}\nresize-pane -x {} -y {} -t %{}\n",
                    size.cols, size.rows, self.size.cols, self.size.rows, self.pane_id
                )
            }
        } else {
            log::info!("The tmux version is not supported");
            return "".to_string();
        }
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("resize-pane in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct CapturePane {
    pane_id: TmuxPaneId,
    history_limit: isize,
}

impl TmuxCommand for CapturePane {
    fn get_command(&self, _domain_id: DomainId) -> String {
        format!(
            "capture-pane -p -t %{} -e -C -S {}\n",
            self.pane_id,
            self.history_limit * -1
        )
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("capture-pane in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        let mux = Mux::get();
        let domain = match mux.get_domain(domain_id) {
            Some(d) => d,
            None => anyhow::bail!("Tmux domain lost"),
        };
        let tmux_domain = match domain.downcast_ref::<TmuxDomain>() {
            Some(t) => t,
            None => anyhow::bail!("Tmux domain lost"),
        };

        let unescaped = termwiz::tmux_cc::unvis(&result.output).context("unescape pane content")?;
        // capturep contents returned from guarded lines which always contain a tailing '\n'
        let unescaped = &unescaped[0..unescaped.len().saturating_sub(1)].replace("\n", "\r\n");

        let pane_map = tmux_domain.inner.remote_panes.lock();
        if let Some(pane) = pane_map.get(&self.pane_id) {
            let mut pane = pane.lock();
            if let Some(p) = mux.get_pane(pane.local_pane_id) {
                tmux_domain.inner.set_pane_cursor_position(&p, 0, 0);
            }

            pane.output_write
                .write_all(unescaped.as_bytes())
                .context("writing capture pane result to output")?;
        }

        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct SendKeys {
    pub keys: Vec<u8>,
    pub pane: TmuxPaneId,
}
impl TmuxCommand for SendKeys {
    fn get_command(&self, _domain_id: DomainId) -> String {
        let mut s = String::new();
        for &byte in self.keys.iter() {
            write!(&mut s, "0x{:X} ", byte).expect("unable to write key");
        }
        format!("send-keys -H -t %{} {}\n", self.pane, s)
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("send-key in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct NewWindow {
    pub command_dir: Option<String>,
    pub resize: Option<TerminalSize>,
}
impl TmuxCommand for NewWindow {
    fn get_command(&self, _domain_id: DomainId) -> String {
        let cwd = self
            .command_dir
            .as_deref()
            .map(|cwd| format!(" -c {}", shell_words::quote(cwd)))
            .unwrap_or_default();
        match self.resize {
            Some(size) => format!(
                "new-window{cwd} ; resize-window -x {} -y {}\n",
                size.cols, size.rows
            ),
            None => format!("new-window{cwd}\n"),
        }
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let operation = if self.resize.is_some() {
                "new-window/resize-window"
            } else {
                "new-window"
            };
            let error = format!("{operation} in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct SubscribePaneCwd;

impl TmuxCommand for SubscribePaneCwd {
    fn get_command(&self, _domain_id: DomainId) -> String {
        format!(
            "refresh-client -B '{}:%*:#{{pane_current_path}}'\n",
            PANE_CWD_SUBSCRIPTION
        )
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            anyhow::bail!("pane cwd subscription in domain={domain_id} failed: {result:#?}");
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct SubscribePaneCommand;

impl TmuxCommand for SubscribePaneCommand {
    fn get_command(&self, _domain_id: DomainId) -> String {
        format!(
            "refresh-client -B '{}:%*:#{{pane_current_command}}'\n",
            PANE_COMMAND_SUBSCRIPTION
        )
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            anyhow::bail!("pane command subscription in domain={domain_id} failed: {result:#?}");
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct ListCommands;
impl TmuxCommand for ListCommands {
    fn get_command(&self, _domain_id: DomainId) -> String {
        "list-commands\n".to_owned()
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("list-command in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        let mux = Mux::get();
        let domain = match mux.get_domain(domain_id) {
            Some(d) => d,
            None => anyhow::bail!("Tmux domain lost"),
        };
        let tmux_domain = match domain.downcast_ref::<TmuxDomain>() {
            Some(t) => t,
            None => anyhow::bail!("Tmux domain lost"),
        };

        let mut support_commands = tmux_domain.inner.support_commands.lock();

        for line in result.output.split('\n') {
            if line.is_empty() {
                continue;
            }
            let v: Vec<&str> = line.split(' ').collect();
            support_commands.insert(v[0].to_string(), line.to_string());
        }

        let mut cmd_queue = tmux_domain.inner.cmd_queue.as_ref().lock();
        if let Some(session) = *tmux_domain.inner.tmux_session.lock() {
            cmd_queue.push_back(Box::new(ListAllWindows {
                session_id: session,
                window_id: None,
            }));
            TmuxDomainState::schedule_send_next_command(domain_id);
        }

        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct SplitPane {
    pub pane_id: TmuxPaneId,
    pub direction: SplitDirection,
}

impl TmuxCommand for SplitPane {
    fn get_command(&self, _domain_id: DomainId) -> String {
        if self.direction == SplitDirection::Horizontal {
            format!("split-window -h -t %{}\n", self.pane_id)
        } else {
            format!("split-window -v -t %{}\n", self.pane_id)
        }
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("split-window in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct SelectWindow {
    pub window_id: TmuxWindowId,
}

impl TmuxCommand for SelectWindow {
    fn get_command(&self, _domain_id: DomainId) -> String {
        format!("select-window -t @{}\n", self.window_id)
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("select-window in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct SwapWindow {
    pub source: TmuxWindowId,
    pub target: TmuxWindowId,
}

impl TmuxCommand for SwapWindow {
    fn get_command(&self, _domain_id: DomainId) -> String {
        format!("swap-window -s @{} -t @{}\n", self.source, self.target)
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("swap-window in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct SelectPane {
    pub pane_id: TmuxPaneId,
}

impl TmuxCommand for SelectPane {
    fn get_command(&self, _domain_id: DomainId) -> String {
        format!("select-pane -t %{}\n", self.pane_id)
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("select-pane in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        Ok(())
    }
}

// This is a dummy command which indicates the attaching is done, it prevents the tmux output
// the unexpected and unnecessary content when syncing with back end in attaching stage.
#[derive(Debug)]
pub(crate) struct AttachDone;
impl TmuxCommand for AttachDone {
    fn get_command(&self, _domain_id: DomainId) -> String {
        // The command doesn't matter, just give a legal simple command to let process_result called.
        "list-session\n".to_string()
    }

    fn process_result(&self, domain_id: DomainId, result: &Guarded) -> anyhow::Result<()> {
        if result.error {
            let error = format!("list-session in domain={domain_id} failed: {result:#?}");
            log::error!("{error}");
            anyhow::bail!("{error}");
        }
        let mux = Mux::get();
        let domain = match mux.get_domain(domain_id) {
            Some(d) => d,
            None => anyhow::bail!("Tmux domain lost"),
        };
        let tmux_domain = match domain.downcast_ref::<TmuxDomain>() {
            Some(t) => t,
            None => anyhow::bail!("Tmux domain lost"),
        };

        let mut attach_state = tmux_domain.inner.attach_state.lock();
        *attach_state = AttachState::Done;
        let pending_resizes = std::mem::take(&mut *tmux_domain.inner.pending_resizes.lock());
        drop(attach_state);
        let mut cmd_queue = tmux_domain.inner.cmd_queue.lock();
        for (pane_id, (size, _)) in pending_resizes {
            cmd_queue.push_back(Box::new(Resize { pane_id, size }));
        }
        drop(cmd_queue);
        TmuxDomainState::schedule_send_next_command(domain_id);
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn swap_window_targets_tmux_window_ids() {
        let command = SwapWindow {
            source: 11,
            target: 22,
        };

        assert_eq!(command.get_command(0), "swap-window -s @11 -t @22\n");
    }

    #[test]
    fn new_window_without_resize_keeps_optional_cwd() {
        assert_eq!(
            NewWindow {
                command_dir: Some("/home/damir/My Project".to_string()),
                resize: None,
            }
            .get_command(0),
            "new-window -c '/home/damir/My Project'\n"
        );
        assert_eq!(
            NewWindow {
                command_dir: None,
                resize: None,
            }
            .get_command(0),
            "new-window\n"
        );
    }

    #[test]
    fn attached_pane_restores_sgr_any_event_mouse() {
        let mut terminal = wezterm_term::Terminal::new(
            TerminalSize::default(),
            Arc::new(config::TermConfig::new()),
            "WezTerm",
            "test",
            Box::new(std::io::sink()),
        );
        let pane = PaneItem {
            session_id: 1,
            window_id: 2,
            pane_id: 3,
            _pane_index: 0,
            cursor_x: 0,
            cursor_y: 0,
            pane_width: 80,
            pane_height: 24,
            pane_left: 0,
            pane_top: 0,
            pane_active: true,
            mouse_standard: false,
            mouse_button: false,
            mouse_all: true,
            mouse_utf8: false,
            mouse_sgr: true,
            current_command: Some("claude".to_string()),
            current_path: None,
        };

        terminal.perform_actions(tmux_mouse_mode_actions(&pane));
        assert!(terminal.is_mouse_grabbed());

        let mouse_disabled = PaneItem {
            mouse_all: false,
            mouse_sgr: false,
            ..pane
        };
        terminal.perform_actions(tmux_mouse_mode_actions(&mouse_disabled));
        assert!(!terminal.is_mouse_grabbed());
    }

    #[test]
    fn list_pane_parser_preserves_spaced_current_path() {
        let pane = parse_pane_item(
            "$1\t@2\t%3\t0\t4\t5\t80\t24\t0\t0\t1\t0\t0\t0\t0\t0\tclaude\t/home/damir/My Project",
        )
        .unwrap();

        assert_eq!(pane.current_command.as_deref(), Some("claude"));
        assert_eq!(pane.current_path.as_deref(), Some("/home/damir/My Project"));
    }

    #[test]
    fn current_path_uses_terminal_cwd_action() {
        let mut terminal = wezterm_term::Terminal::new(
            TerminalSize::default(),
            Arc::new(config::TermConfig::new()),
            "WezTerm",
            "test",
            Box::new(std::io::sink()),
        );

        terminal.perform_actions(vec![
            tmux_current_path_action("/home/damir/My Project").unwrap()
        ]);

        assert_eq!(
            terminal.get_current_dir().map(Url::path),
            Some("/home/damir/My%20Project/")
        );
    }

    #[test]
    fn pane_cwd_subscription_uses_all_panes() {
        assert_eq!(
            SubscribePaneCwd.get_command(0),
            "refresh-client -B 'wezterm-pane-cwd:%*:#{pane_current_path}'\n"
        );
    }

    #[test]
    fn pane_command_subscription_uses_all_panes() {
        assert_eq!(
            SubscribePaneCommand.get_command(0),
            "refresh-client -B 'wezterm-pane-command:%*:#{pane_current_command}'\n"
        );
    }

    #[test]
    fn pane_command_changes_and_clears() {
        assert_eq!(pane_command("claude").as_deref(), Some("claude"));
        assert_eq!(pane_command(""), None);
    }

    #[test]
    fn tmux_attach_restores_resize_and_process_identity() {
        let _executor = promise::spawn::SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let domain = Arc::new(TmuxDomain::new(alloc_pane_id()));
        let dyn_domain: Arc<dyn crate::Domain> = domain.clone();
        mux.add_domain(&dyn_domain);
        domain
            .inner
            .support_commands
            .lock()
            .insert("resize-window".to_string(), String::new());

        let remote_pane_id = 3;
        let pane = domain
            .inner
            .create_pane(&PaneItem {
                session_id: 1,
                window_id: 2,
                pane_id: remote_pane_id,
                _pane_index: 0,
                cursor_x: 0,
                cursor_y: 0,
                pane_width: 80,
                pane_height: 24,
                pane_left: 0,
                pane_top: 0,
                pane_active: true,
                mouse_standard: false,
                mouse_button: false,
                mouse_all: false,
                mouse_utf8: false,
                mouse_sgr: false,
                current_command: None,
                current_path: None,
            })
            .unwrap();
        let initial = TerminalSize {
            rows: 24,
            cols: 80,
            ..TerminalSize::default()
        };
        let desired = TerminalSize {
            rows: 40,
            cols: 120,
            ..TerminalSize::default()
        };
        let tab = Arc::new(Tab::new(&initial));
        assert_eq!(tab.get_size().rows, 24);
        assert_eq!(tab.get_size().cols, 80);
        tab.assign_pane(&pane);
        mux.add_tab_no_panes(&tab);
        mux.add_pane(&pane).unwrap();
        domain.inner.gui_tabs.lock().insert(
            2,
            TmuxTab {
                tab_id: tab.tab_id(),
                tmux_window_id: 2,
                layout_csum: String::new(),
                panes: HashSet::from([remote_pane_id]),
            },
        );
        *domain.inner.tmux_session.lock() = Some(1);
        domain
            .inner
            .sync_pane_state(&[PaneItem {
                session_id: 1,
                window_id: 2,
                pane_id: remote_pane_id,
                _pane_index: 0,
                cursor_x: 0,
                cursor_y: 0,
                pane_width: 80,
                pane_height: 24,
                pane_left: 0,
                pane_top: 0,
                pane_active: true,
                mouse_standard: false,
                mouse_button: false,
                mouse_all: false,
                mouse_utf8: false,
                mouse_sgr: false,
                current_command: Some("claude".to_string()),
                current_path: None,
            }])
            .unwrap();
        assert_eq!(
            pane.get_foreground_process_name(crate::pane::CachePolicy::AllowStale)
                .as_deref(),
            Some("claude")
        );
        domain.inner.cmd_queue.lock().clear();
        tab.resize(desired);
        domain.inner.cmd_queue.lock().clear();

        assert_eq!(
            Resize {
                pane_id: remote_pane_id,
                size: PtySize {
                    rows: desired.rows as u16,
                    cols: desired.cols as u16,
                    pixel_width: 0,
                    pixel_height: 0,
                },
            }
            .get_command(domain.inner.domain_id),
            ""
        );
        assert_eq!(
            domain
                .inner
                .pending_resizes
                .lock()
                .get(&remote_pane_id)
                .map(|resize| resize.0),
            Some(PtySize {
                rows: 40,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0
            })
        );
        AttachDone
            .process_result(
                domain.inner.domain_id,
                &Guarded {
                    error: false,
                    timestamp: 0,
                    number: 0,
                    flags: 1,
                    output: String::new(),
                },
            )
            .unwrap();

        let replay = domain.inner.cmd_queue.lock().pop_front().unwrap();
        let encoded = replay.get_command(domain.inner.domain_id);
        assert!(encoded.contains("resize-window -x 120 -y 40"));
        assert!(!encoded.contains("resize-pane"));
        assert_eq!(encoded.matches('\n').count(), 1);
        Mux::shutdown();
    }
}
