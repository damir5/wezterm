use crate::activity::Activity;
use crate::domain::{alloc_domain_id, Domain, DomainId, DomainState, SplitSource};
use crate::pane::{Pane, PaneId};
use crate::tab::{SplitRequest, Tab, TabId};
use crate::tmux_commands::{
    ListAllPanes, ListAllWindows, ListCommands, NewWindow, Resize, SplitPane, SubscribePaneCommand,
    SubscribePaneCwd, SwapWindow, TmuxCommand, PANE_COMMAND_SUBSCRIPTION, PANE_CWD_SUBSCRIPTION,
};
use crate::window::WindowId;
use crate::{Mux, MuxWindowBuilder};
use async_trait::async_trait;
use filedescriptor::FileDescriptor;
use parking_lot::{Condvar, Mutex};
use portable_pty::{CommandBuilder, PtySize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use termwiz::tmux_cc::*;
use wezterm_term::TerminalSize;

#[derive(PartialEq, Eq, Debug, Copy, Clone)]
pub enum AttachState {
    Init,
    Done,
}

#[derive(PartialEq, Eq, Debug, Copy, Clone)]
enum State {
    WaitForInitialGuard,
    Idle,
    Exit,
}

#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct TmuxRemotePane {
    // members for local
    pub local_pane_id: PaneId,
    pub output_write: FileDescriptor,
    pub active_lock: Arc<(Mutex<bool>, Condvar)>,
    // members sync with remote
    pub session_id: TmuxSessionId,
    pub window_id: TmuxWindowId,
    pub pane_id: TmuxPaneId,
    pub cursor_x: u64,
    pub cursor_y: u64,
    pub pane_width: u64,
    pub pane_height: u64,
    pub pane_left: u64,
    pub pane_top: u64,
    pub current_command: Option<String>,
}

pub(crate) type RefTmuxRemotePane = Arc<Mutex<TmuxRemotePane>>;

/// As a remote TmuxTab, keeping the TmuxPanes ID
/// within the remote tab.
#[allow(dead_code)]
pub(crate) struct TmuxTab {
    pub tab_id: TabId, // local tab ID
    pub tmux_window_id: TmuxWindowId,
    pub layout_csum: String,
    pub panes: HashSet<TmuxPaneId>, // tmux panes within tmux window
}

pub(crate) type TmuxCmdQueue = VecDeque<Box<dyn TmuxCommand>>;
type TmuxResponseQueue = VecDeque<Option<Box<dyn TmuxCommand>>>;
const RESIZE_QUIET_PERIOD: Duration = Duration::from_millis(50);

fn take_pending_commands(
    domain_id: DomainId,
    pending: &mut TmuxCmdQueue,
    awaiting_response: &mut TmuxResponseQueue,
) -> String {
    let mut output = String::new();
    while let Some(command) = pending.pop_front() {
        let encoded = command.get_command(domain_id);
        if encoded.is_empty() {
            continue;
        }
        let response_count = encoded.lines().filter(|line| !line.is_empty()).count();
        output.push_str(&encoded);
        awaiting_response.push_back(Some(command));
        awaiting_response.extend((1..response_count).map(|_| None));
    }
    output
}

fn take_response_command(
    response: &Guarded,
    awaiting_response: &mut TmuxResponseQueue,
) -> Option<Box<dyn TmuxCommand>> {
    if response.flags & 1 == 0 {
        None
    } else {
        awaiting_response.pop_front().flatten()
    }
}

fn adjacent_swap_targets<T: Copy + Eq>(
    ordered: &[T],
    source: T,
    target: T,
    before: bool,
) -> Vec<T> {
    let Some(source_index) = ordered.iter().position(|item| *item == source) else {
        return vec![];
    };
    let Some(target_index) = ordered.iter().position(|item| *item == target) else {
        return vec![];
    };
    if source_index == target_index {
        return vec![];
    }

    let insertion_index = if before {
        target_index
    } else {
        target_index + 1
    };
    let final_index = if insertion_index > source_index {
        insertion_index - 1
    } else {
        insertion_index
    };

    if final_index < source_index {
        ordered[final_index..source_index]
            .iter()
            .rev()
            .copied()
            .collect()
    } else {
        ordered[source_index + 1..=final_index].to_vec()
    }
}

pub(crate) struct TmuxDomainState {
    pub pane_id: PaneId,     // ID of the original pane
    pub domain_id: DomainId, // ID of TmuxDomain
    state: Mutex<State>,
    pub cmd_queue: Arc<Mutex<TmuxCmdQueue>>,
    response_queue: Mutex<TmuxResponseQueue>,
    pub gui_window: Mutex<Option<MuxWindowBuilder>>,
    pub gui_tabs: Mutex<HashMap<TmuxWindowId, TmuxTab>>,
    pub remote_panes: Mutex<HashMap<TmuxPaneId, RefTmuxRemotePane>>,
    pub tmux_session: Mutex<Option<TmuxSessionId>>,
    pub support_commands: Mutex<HashMap<String, String>>,
    pub attach_state: Mutex<AttachState>,
    pub pending_resizes: Mutex<HashMap<TmuxPaneId, (PtySize, u64)>>,
    next_resize_request_id: AtomicU64,
    pending_splits: Mutex<VecDeque<promise::Promise<TmuxPaneId>>>,
    pub backlog: Mutex<HashMap<TmuxPaneId, Vec<u8>>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux_commands::SendKeys;

    #[derive(Debug)]
    struct TwoLineCommand;

    impl TmuxCommand for TwoLineCommand {
        fn get_command(&self, _domain_id: DomainId) -> String {
            "first\nsecond\n".to_string()
        }

        fn process_result(&self, _domain_id: DomainId, _result: &Guarded) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn guarded(flags: i64) -> Guarded {
        Guarded {
            error: false,
            timestamp: 0,
            number: 0,
            flags,
            output: String::new(),
        }
    }

    #[test]
    fn bursty_input_is_pipelined_in_one_dispatch() {
        let mut pending: TmuxCmdQueue = VecDeque::from([
            Box::new(SendKeys {
                pane: 1,
                keys: b"first".to_vec(),
            }) as Box<dyn TmuxCommand>,
            Box::new(SendKeys {
                pane: 1,
                keys: b"second".to_vec(),
            }) as Box<dyn TmuxCommand>,
        ]);
        let mut awaiting_response = TmuxResponseQueue::new();

        let encoded = take_pending_commands(0, &mut pending, &mut awaiting_response);

        assert!(
            pending.is_empty(),
            "the whole input burst should be dispatched"
        );
        assert_eq!(awaiting_response.len(), 2);
        assert_eq!(encoded.matches("send-keys").count(), 2);
    }

    #[test]
    fn server_guard_does_not_consume_a_client_response() {
        let mut awaiting_response: TmuxResponseQueue = VecDeque::from([Some(Box::new(SendKeys {
            pane: 1,
            keys: b"input".to_vec(),
        })
            as Box<dyn TmuxCommand>)]);

        assert!(take_response_command(&guarded(0), &mut awaiting_response).is_none());
        assert_eq!(awaiting_response.len(), 1);
        assert!(take_response_command(&guarded(1), &mut awaiting_response).is_some());
        assert!(awaiting_response.is_empty());
    }

    #[test]
    fn multiline_command_does_not_consume_the_next_handler() {
        let mut pending: TmuxCmdQueue = VecDeque::from([
            Box::new(TwoLineCommand) as Box<dyn TmuxCommand>,
            Box::new(SendKeys {
                pane: 1,
                keys: b"next".to_vec(),
            }) as Box<dyn TmuxCommand>,
        ]);
        let mut awaiting_response = TmuxResponseQueue::new();

        take_pending_commands(0, &mut pending, &mut awaiting_response);

        assert_eq!(awaiting_response.len(), 3);
        assert!(take_response_command(&guarded(1), &mut awaiting_response).is_some());
        assert!(take_response_command(&guarded(1), &mut awaiting_response).is_none());
        assert!(take_response_command(&guarded(1), &mut awaiting_response).is_some());
        assert!(awaiting_response.is_empty());
    }

    #[test]
    fn resize_burst_keeps_only_the_latest_size() {
        let domain = TmuxDomain::new(1);
        let requests: Vec<_> = (0..7)
            .map(|step| {
                let size = PtySize {
                    rows: 24 + step,
                    cols: 80 + step,
                    pixel_width: 0,
                    pixel_height: 0,
                };
                (domain.inner.remember_resize(1, size), size)
            })
            .collect();
        let (latest, latest_size) = requests[6];
        assert_eq!(domain.inner.remember_resize(1, latest_size), latest);

        for (obsolete, _) in &requests[..6] {
            assert_eq!(domain.inner.take_resize(1, *obsolete), None);
        }
        assert_eq!(domain.inner.take_resize(1, latest), Some(latest_size));
        assert!(domain.inner.pending_resizes.lock().is_empty());
    }

    #[test]
    fn spawn_queues_initial_size_and_cwd() {
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = TmuxDomain::new(1);
        domain
            .inner
            .support_commands
            .lock()
            .insert("resize-window".to_string(), String::new());

        let result = promise::spawn::block_on(domain.spawn(
            TerminalSize {
                rows: 47,
                cols: 141,
                ..TerminalSize::default()
            },
            None,
            Some("/home/damir/My Project".to_string()),
            1,
            config::keyassignment::SpawnTabDomain::CurrentPaneDomain,
            None,
        ));

        assert!(result.is_err(), "tmux spawn completes from WindowAdd");
        let command = domain.inner.cmd_queue.lock().pop_front().unwrap();
        assert_eq!(
            command.get_command(domain.inner.domain_id),
            "new-window -c '/home/damir/My Project' ; resize-window -x 141 -y 47\n"
        );
    }

    #[test]
    fn spawn_without_resize_support_keeps_cwd() {
        let _executor = promise::spawn::SimpleExecutor::new();
        let domain = TmuxDomain::new(1);

        let result = promise::spawn::block_on(domain.spawn(
            TerminalSize {
                rows: 47,
                cols: 141,
                ..TerminalSize::default()
            },
            None,
            Some("/home/damir/My Project".to_string()),
            1,
            config::keyassignment::SpawnTabDomain::CurrentPaneDomain,
            None,
        ));

        assert!(result.is_err(), "tmux spawn completes from WindowAdd");
        let command = domain.inner.cmd_queue.lock().pop_front().unwrap();
        assert_eq!(
            command.get_command(domain.inner.domain_id),
            "new-window -c '/home/damir/My Project'\n"
        );
    }

    #[test]
    fn remote_reorder_uses_adjacent_swaps() {
        let ordered = [10, 20, 30, 40];
        assert_eq!(adjacent_swap_targets(&ordered, 10, 30, false), [20, 30]);
        assert_eq!(adjacent_swap_targets(&ordered, 40, 20, true), [30, 20]);
        assert!(adjacent_swap_targets(&ordered, 20, 20, true).is_empty());
    }
}

pub struct TmuxDomain {
    pub(crate) inner: Arc<TmuxDomainState>,
}

impl TmuxDomainState {
    pub fn remember_resize(&self, pane_id: TmuxPaneId, size: PtySize) -> u64 {
        let mut pending = self.pending_resizes.lock();
        if let Some((pending_size, request_id)) = pending.get(&pane_id) {
            if *pending_size == size {
                return *request_id;
            }
        }
        let request_id = self.next_resize_request_id.fetch_add(1, Ordering::Relaxed);
        pending.insert(pane_id, (size, request_id));
        request_id
    }

    fn take_resize(&self, pane_id: TmuxPaneId, request_id: u64) -> Option<PtySize> {
        let mut pending = self.pending_resizes.lock();
        (pending.get(&pane_id).map(|resize| resize.1) == Some(request_id))
            .then(|| pending.remove(&pane_id).unwrap().0)
    }

    pub fn schedule_resize(domain_id: DomainId, pane_id: TmuxPaneId, size: PtySize) {
        let Some(mux) = Mux::try_get() else {
            return;
        };
        let Some(domain) = mux.get_domain(domain_id) else {
            return;
        };
        let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() else {
            return;
        };
        let request_id = tmux_domain.inner.remember_resize(pane_id, size);
        smol::spawn(async move {
            smol::Timer::after(RESIZE_QUIET_PERIOD).await;
            let Some(mux) = Mux::try_get() else {
                return;
            };
            let Some(domain) = mux.get_domain(domain_id) else {
                return;
            };
            let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() else {
                return;
            };
            if let Some(size) = tmux_domain.inner.take_resize(pane_id, request_id) {
                tmux_domain
                    .inner
                    .cmd_queue
                    .lock()
                    .push_back(Box::new(Resize { pane_id, size }));
                Self::schedule_send_next_command(domain_id);
            }
        })
        .detach();
    }

    pub fn advance(&self, events: Box<Vec<Event>>) {
        for event in events.iter() {
            let state = *self.state.lock();
            log::debug!("tmux: {:?} in state {:?}", event, state);
            match event {
                // Tmux generic events
                Event::Guarded(response) => match state {
                    State::WaitForInitialGuard => {
                        *self.state.lock() = State::Idle;
                    }
                    State::Idle => {
                        let mut response_queue = self.response_queue.lock();
                        if let Some(cmd) = take_response_command(response, &mut response_queue) {
                            let domain_id = self.domain_id;
                            let resp = response.clone();
                            promise::spawn::spawn_into_main_thread(async move {
                                if let Err(err) = cmd.process_result(domain_id, &resp) {
                                    log::error!("Tmux processing command result error: {}", err);
                                }
                            })
                            .detach();
                        }
                    }
                    State::Exit => {}
                },

                // Tmux specific events
                Event::ConfigError { error } => {
                    // tmux config file error, not our fault, just log it and go
                    log::warn!("tmux configuration error: {error}");
                }
                Event::Exit { reason: _ } => {
                    *self.state.lock() = State::Exit;
                    let mut pane_map = self.remote_panes.lock();
                    for (_, v) in pane_map.iter_mut() {
                        let remote_pane = v.lock();
                        let (lock, condvar) = &*remote_pane.active_lock;
                        let mut released = lock.lock();
                        *released = true;
                        condvar.notify_all();
                    }
                    let mut cmd_queue = self.cmd_queue.as_ref().lock();
                    cmd_queue.clear();
                    self.response_queue.lock().clear();

                    // Force to quit the tmux mode
                    let pane_id = self.pane_id;
                    promise::spawn::spawn_into_main_thread_with_low_priority(async move {
                        if let Some(x) = Mux::get().get_pane(pane_id) {
                            let _ = write!(x.writer(), "\n\n");
                        }
                    })
                    .detach();

                    return;
                }
                Event::LayoutChange {
                    window,
                    layout,
                    visible_layout: _,
                    raw_flags: _,
                } => {
                    let mut cmd_queue = self.cmd_queue.as_ref().lock();
                    cmd_queue.push_back(Box::new(ListAllPanes {
                        window_id: *window,
                        prune: true,
                        layout_csum: if let Some(l) = layout.get(0..4) {
                            l.to_string()
                        } else {
                            "".to_string()
                        },
                    }));
                }
                Event::Output { pane, text } => {
                    let pane_map = self.remote_panes.lock();
                    if let Some(ref_pane) = pane_map.get(pane) {
                        let mut tmux_pane = ref_pane.lock();
                        if let Err(err) = tmux_pane.output_write.write_all(text) {
                            log::error!("Failed to write tmux data to output: {:#}", err);
                        }
                    } else {
                        // the output may come early then pane is ready, in this case we
                        // backlog it
                        self.backlog.lock().insert(*pane, text.to_vec());
                        log::debug!("Tmux pane {} havn't been attached", pane);
                    }
                }
                Event::SessionChanged { session, name: _ } => {
                    *self.tmux_session.lock() = Some(*session);
                    let mut cmd_queue = self.cmd_queue.as_ref().lock();
                    cmd_queue.push_back(Box::new(ListCommands));
                    cmd_queue.push_back(Box::new(SubscribePaneCwd));
                    cmd_queue.push_back(Box::new(SubscribePaneCommand));

                    self.subscribe_notification();
                    log::info!("tmux session changed:{}", session);
                }
                Event::WindowAdd { window } => {
                    // Only handle the new tab, the first empty window handled by sync_window_state
                    if !self.gui_window.lock().is_none() {
                        if let Some(session) = *self.tmux_session.lock() {
                            let mut cmd_queue = self.cmd_queue.as_ref().lock();
                            cmd_queue.push_back(Box::new(ListAllWindows {
                                session_id: session,
                                window_id: Some(*window),
                            }));
                            log::info!("tmux window add: {}:{}", session, window);
                        }
                    }
                }
                Event::WindowClose { window } => {
                    let _ = self.remove_detached_window(*window);
                }
                Event::WindowPaneChanged { window, pane } => {
                    // The tmux 2.7 WindowPaneChanged event comes early than WindowAdd, we need to
                    // skip it
                    if !self.check_window_attached(*window) {
                        continue;
                    }

                    // Split pane
                    if !self.check_pane_attached(*window, *pane) {
                        let mut pending_splits = self.pending_splits.lock();
                        if let Some(mut promise) = pending_splits.pop_front() {
                            promise.ok(*pane);
                        }
                    }
                    log::info!("tmux window pane changed: {}:{}", window, pane);
                }
                Event::WindowRenamed { window, name } => {
                    let gui_tabs = self.gui_tabs.lock();
                    if let Some(x) = gui_tabs.get(&window) {
                        let mux = Mux::get();
                        if let Some(tab) = mux.get_tab(x.tab_id) {
                            tab.set_title(&format!("{}", name));
                        }
                    }
                }
                Event::SubscriptionChanged {
                    name,
                    session,
                    window,
                    pane,
                    value,
                } => {
                    if name == PANE_CWD_SUBSCRIPTION {
                        if let (Some(window), Some(pane)) = (window, pane) {
                            self.update_pane_current_path(*session, *window, *pane, value);
                        }
                    } else if name == PANE_COMMAND_SUBSCRIPTION {
                        if let (Some(window), Some(pane)) = (window, pane) {
                            self.update_pane_current_command(*session, *window, *pane, value);
                        }
                    }
                }
                Event::UnlinkedWindowClose { window } => {
                    let _ = self.remove_detached_window(*window);
                }
                _ => {}
            }
        }

        // send pending commands to tmux
        let cmd_queue = self.cmd_queue.as_ref().lock();
        if *self.state.lock() == State::Idle && !cmd_queue.is_empty() {
            TmuxDomainState::schedule_send_next_command(self.domain_id);
        }
    }

    /// Send all queued commands without waiting for their guarded responses.
    /// tmux returns client responses in command order, so response_queue keeps
    /// the commands needed to process those responses later.
    /// must be called inside main thread
    fn send_next_command(&self) {
        if *self.state.lock() != State::Idle {
            return;
        }
        let mut cmd_queue = self.cmd_queue.as_ref().lock();
        let mut response_queue = self.response_queue.lock();
        let commands = take_pending_commands(self.domain_id, &mut cmd_queue, &mut response_queue);
        drop(response_queue);
        drop(cmd_queue);

        if !commands.is_empty() {
            log::debug!("sending tmux command batch {:?}", commands);
            let mux = Mux::get();
            if let Some(pane) = mux.get_pane(self.pane_id) {
                let mut writer = pane.writer();
                let _ = write!(writer, "{}", commands);
            }
        }
    }

    /// schedule a `send_next_command` into main thread
    pub fn schedule_send_next_command(domain_id: usize) {
        promise::spawn::spawn_into_main_thread(async move {
            let mux = Mux::get();
            if let Some(domain) = mux.get_domain(domain_id) {
                if let Some(tmux_domain) = domain.downcast_ref::<TmuxDomain>() {
                    tmux_domain.send_next_command();
                }
            }
        })
        .detach();
    }

    /// create a standalone window for tmux tabs
    pub fn create_gui_window(&self) {
        if self.gui_window.lock().is_none() {
            let mux = Mux::get();
            let window_builder =
                if let Some((_domain, window_id, _tab)) = mux.resolve_pane_id(self.pane_id) {
                    MuxWindowBuilder {
                        window_id,
                        activity: Some(Activity::new()),
                        notified: false,
                    }
                } else {
                    mux.new_empty_window(
                        None, /* TODO: pass session here */
                        None, /* position */
                    )
                };

            log::info!("Tmux create window id {}", window_builder.window_id);
            {
                let mut window_id = self.gui_window.lock();
                *window_id = Some(window_builder); // keep the builder so it won't be purged
            }
        };
    }

    /// create a tmux window
    pub fn create_tmux_window(&self, size: TerminalSize, command_dir: Option<String>) {
        let resize = self
            .support_commands
            .lock()
            .contains_key("resize-window")
            .then_some(size);
        let mut cmd_queue = self.cmd_queue.as_ref().lock();
        cmd_queue.push_back(Box::new(NewWindow {
            command_dir,
            resize,
        }));
        TmuxDomainState::schedule_send_next_command(self.domain_id);
    }

    /// split the tmux pane
    pub fn split_tmux_pane(
        &self,
        _tab: TabId,
        pane_id: PaneId,
        split_request: SplitRequest,
    ) -> anyhow::Result<()> {
        let tmux_pane_id = self
            .remote_panes
            .lock()
            .iter()
            .find(|(_, ref_pane)| ref_pane.lock().local_pane_id == pane_id)
            .map(|p| p.1.lock().pane_id);

        if let Some(id) = tmux_pane_id {
            let mut cmd_queue = self.cmd_queue.as_ref().lock();
            cmd_queue.push_back(Box::new(SplitPane {
                pane_id: id,
                direction: split_request.direction,
            }));
            TmuxDomainState::schedule_send_next_command(self.domain_id);
            return Ok(());
        } else {
            anyhow::bail!("Could not find the tmux pane peer for local pane: {pane_id}");
        }
    }
}

impl TmuxDomain {
    /// Pane that owns the tmux control connection.  GUI consumers use this to
    /// associate tmux-created tabs with their original connection pane.
    pub fn controller_pane_id(&self) -> PaneId {
        self.inner.pane_id
    }

    pub fn reorder_tab(&self, source: TabId, target: TabId, before: bool) -> bool {
        let Some(gui_window) = self
            .inner
            .gui_window
            .lock()
            .as_ref()
            .map(|window| window.window_id)
        else {
            return false;
        };
        let tab_map = self.inner.gui_tabs.lock();
        let remote_by_local = tab_map
            .values()
            .map(|tab| (tab.tab_id, tab.tmux_window_id))
            .collect::<HashMap<_, _>>();
        drop(tab_map);

        let mux = Mux::get();
        let Some(window) = mux.get_window(gui_window) else {
            return false;
        };
        let ordered = window
            .iter()
            .filter_map(|tab| remote_by_local.get(&tab.tab_id()).copied())
            .collect::<Vec<_>>();
        drop(window);
        let (Some(source), Some(target)) = (
            remote_by_local.get(&source).copied(),
            remote_by_local.get(&target).copied(),
        ) else {
            return false;
        };
        let swaps = adjacent_swap_targets(&ordered, source, target, before);
        if swaps.is_empty() {
            return false;
        }
        let mut queue = self.inner.cmd_queue.lock();
        for target in swaps {
            queue.push_back(Box::new(SwapWindow { source, target }));
        }
        drop(queue);
        TmuxDomainState::schedule_send_next_command(self.inner.domain_id);
        true
    }

    pub fn new(pane_id: PaneId) -> Self {
        let domain_id = alloc_domain_id();
        let cmd_queue = VecDeque::new();
        let inner = Arc::new(TmuxDomainState {
            domain_id,
            pane_id,
            // parser,
            state: Mutex::new(State::WaitForInitialGuard),
            cmd_queue: Arc::new(Mutex::new(cmd_queue)),
            response_queue: Mutex::new(VecDeque::new()),
            gui_window: Mutex::new(None),
            gui_tabs: Mutex::new(HashMap::default()),
            remote_panes: Mutex::new(HashMap::default()),
            tmux_session: Mutex::new(None),
            support_commands: Mutex::new(HashMap::default()),
            attach_state: Mutex::new(AttachState::Init),
            pending_resizes: Mutex::new(HashMap::default()),
            next_resize_request_id: AtomicU64::new(0),
            pending_splits: Mutex::new(VecDeque::default()),
            backlog: Mutex::new(HashMap::default()),
        });

        Self { inner }
    }

    fn send_next_command(&self) {
        self.inner.send_next_command();
    }
}

#[async_trait(?Send)]
impl Domain for TmuxDomain {
    async fn spawn(
        &self,
        size: TerminalSize,
        _command: Option<CommandBuilder>,
        command_dir: Option<String>,
        _window: WindowId,
        _domain: config::keyassignment::SpawnTabDomain,
        _current_pane_id: Option<PaneId>,
    ) -> anyhow::Result<Arc<Tab>> {
        self.inner.create_tmux_window(size, command_dir);
        // This is intention, we would not return a Tab, since we don't have now!
        // We use create_tmux_window to create back end tmux window, then the
        // Tmux WindowAdd event will triage us to do the rest things.
        anyhow::bail!("Intention: tmux WindowAdd completes new-window and initial resize");
    }

    async fn split_pane(
        &self,
        _source: SplitSource,
        tab: TabId,
        pane_id: PaneId,
        split_request: SplitRequest,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let mut promise = promise::Promise::new();
        if let Some(future) = promise.get_future() {
            {
                let mut pending_splits = self.inner.pending_splits.lock();
                let _ = self.inner.split_tmux_pane(tab, pane_id, split_request)?;
                pending_splits.push_back(promise);
            }

            if let Ok(id) = future.await {
                let pane = self.inner.split_pane(tab, pane_id, id, split_request);
                return pane;
            }
        }

        anyhow::bail!("Split_pane failed");
    }

    async fn spawn_pane(
        &self,
        _size: TerminalSize,
        _command: Option<CommandBuilder>,
        _command_dir: Option<String>,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        anyhow::bail!("Spawn_pane not yet implemented for TmuxDomain");
    }

    fn domain_id(&self) -> DomainId {
        self.inner.domain_id
    }

    fn domain_name(&self) -> &str {
        "tmux"
    }

    async fn attach(&self, _window_id: Option<crate::WindowId>) -> anyhow::Result<()> {
        Ok(())
    }

    fn detachable(&self) -> bool {
        false
    }

    fn detach(&self) -> anyhow::Result<()> {
        anyhow::bail!("detach not implemented for TmuxDomain");
    }

    fn state(&self) -> DomainState {
        DomainState::Attached
    }
}
