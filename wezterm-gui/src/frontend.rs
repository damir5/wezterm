use crate::scripting::guiwin::GuiWin;
use crate::spawn::SpawnWhere;
use crate::termwindow::TermWindowNotif;
use crate::TermWindow;
use ::window::*;
use anyhow::{Context, Error};
use config::keyassignment::{KeyAssignment, SpawnCommand};
use config::{ConfigSubscription, NotificationHandling};
use mux::client::ClientId;
use mux::pane::PaneId;
use mux::window::WindowId as MuxWindowId;
use mux::{Mux, MuxNotification};
use promise::{Future, Promise};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use wezterm_term::{Alert, ClipboardSelection};
use wezterm_toast_notification::*;

pub struct GuiFrontEnd {
    connection: Rc<Connection>,
    switching_workspaces: RefCell<bool>,
    spawned_mux_window: RefCell<HashSet<MuxWindowId>>,
    known_windows: RefCell<BTreeMap<Window, MuxWindowId>>,
    client_id: Arc<ClientId>,
    config_subscription: RefCell<Option<ConfigSubscription>>,
    input_stacks: RefCell<HashMap<PaneId, PaneInputStack>>,
    input_stack_windows: RefCell<HashMap<MuxWindowId, InputStackWindowUi>>,
}

#[derive(Default)]
struct PaneInputStack {
    queued: Vec<String>,
    draft: String,
    auto_delivery: Option<AutoDelivery>,
}

struct AutoDelivery {
    index: usize,
}

#[derive(Default)]
pub(crate) struct InputStackWindowUi {
    pub editor: Option<PaneId>,
    pub expanded: HashSet<PaneId>,
    pub events: Vec<egui::Event>,
    pub errors: HashMap<PaneId, String>,
}

#[derive(Clone, Copy)]
pub(crate) struct InputStackPaneRect {
    pub pane_id: PaneId,
    pub rect: egui::Rect,
}

pub(crate) fn normalize_input_stack_item(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut in_newline = false;
    for ch in text.chars() {
        if ch == '\r' || ch == '\n' {
            if !in_newline {
                normalized.push(' ');
                in_newline = true;
            }
        } else {
            normalized.push(ch);
            in_newline = false;
        }
    }
    normalized
}

fn serialize_input_stack(items: &[String]) -> String {
    items
        .iter()
        .enumerate()
        .map(|(index, item)| format!("{}. {item}", index + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

impl Drop for GuiFrontEnd {
    fn drop(&mut self) {
        ::window::shutdown();
    }
}

impl GuiFrontEnd {
    pub(crate) fn open_input_stack_editor(&self, window_id: MuxWindowId, pane_id: PaneId) {
        let mut windows = self.input_stack_windows.borrow_mut();
        let ui = windows.entry(window_id).or_default();
        ui.editor = Some(pane_id);
        ui.expanded.insert(pane_id);
    }

    pub(crate) fn input_stack_ui_is_active(&self, window_id: MuxWindowId) -> bool {
        self.input_stack_windows
            .borrow()
            .get(&window_id)
            .map(|ui| ui.editor.is_some() || !ui.expanded.is_empty())
            .unwrap_or(false)
    }

    pub(crate) fn input_stack_editor_is_active(&self, window_id: MuxWindowId) -> bool {
        self.input_stack_windows
            .borrow()
            .get(&window_id)
            .and_then(|ui| ui.editor)
            .is_some()
    }

    pub(crate) fn input_stack_pane_ui(
        &self,
        window_id: MuxWindowId,
        pane_id: PaneId,
    ) -> (bool, bool) {
        self.input_stack_windows
            .borrow()
            .get(&window_id)
            .map(|ui| (ui.expanded.contains(&pane_id), ui.editor == Some(pane_id)))
            .unwrap_or_default()
    }

    pub(crate) fn has_input_stack_for_panes(&self, panes: &[InputStackPaneRect]) -> bool {
        let stacks = self.input_stacks.borrow();
        panes.iter().any(|pane| {
            stacks
                .get(&pane.pane_id)
                .map(|stack| !stack.queued.is_empty() || !stack.draft.is_empty())
                .unwrap_or(false)
        })
    }

    pub(crate) fn push_input_stack_event(&self, window_id: MuxWindowId, event: egui::Event) {
        self.input_stack_windows
            .borrow_mut()
            .entry(window_id)
            .or_default()
            .events
            .push(event);
    }

    pub(crate) fn take_input_stack_events(&self, window_id: MuxWindowId) -> Vec<egui::Event> {
        self.input_stack_windows
            .borrow_mut()
            .entry(window_id)
            .or_default()
            .events
            .drain(..)
            .collect()
    }

    pub(crate) fn paint_input_stack(
        &self,
        ctx: &egui::Context,
        window_id: MuxWindowId,
        panes: &[InputStackPaneRect],
        os_window: &Window,
    ) {
        enum Action {
            Deliver(PaneId, usize, String),
            DeliverWhenReady(PaneId, usize),
            CancelAutoDelivery(PaneId),
            Queue(PaneId, String),
            Collapse(PaneId),
        }

        self.advance_auto_deliveries(window_id, panes);
        if self
            .input_stacks
            .borrow()
            .values()
            .any(|stack| stack.auto_delivery.is_some())
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
        let mut action = None;
        let pending_outline = egui::Color32::from_rgb(240, 162, 90);
        let mut windows = self.input_stack_windows.borrow_mut();
        let window_ui = windows.entry(window_id).or_default();
        let visible = panes
            .iter()
            .map(|pane| pane.pane_id)
            .collect::<HashSet<_>>();
        window_ui
            .expanded
            .retain(|pane_id| visible.contains(pane_id));
        if window_ui
            .editor
            .map(|pane_id| !visible.contains(&pane_id))
            .unwrap_or(false)
        {
            window_ui.editor = None;
        }
        let mut stacks = self.input_stacks.borrow_mut();

        for pane in panes {
            let count = stacks
                .get(&pane.pane_id)
                .map(|stack| stack.queued.len())
                .unwrap_or(0);
            let expanded = window_ui.expanded.contains(&pane.pane_id);
            let editing = window_ui.editor == Some(pane.pane_id);
            if count == 0 && !editing {
                continue;
            }

            if !expanded {
                let pos = pane.rect.right_bottom() - egui::vec2(86.0, 34.0);
                egui::Area::new(egui::Id::new(("input-stack-badge", pane.pane_id)))
                    .fixed_pos(pos)
                    .order(egui::Order::Foreground)
                    .show(ctx, |ui| {
                        if egui::Frame::new()
                            .fill(egui::Color32::from_rgb(54, 43, 34))
                            .stroke(egui::Stroke::new(1.5_f32, pending_outline))
                            .corner_radius(5.0)
                            .inner_margin(1.0)
                            .show(ui, |ui| ui.button(format!("Queued {count}")))
                            .inner
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .clicked()
                        {
                            window_ui.expanded.insert(pane.pane_id);
                        }
                    });
                continue;
            }

            let width = pane.rect.width().min(420.0);
            let estimated_height = if editing {
                178.0
            } else {
                62.0 + 36.0 * count.min(5) as f32
            };
            let pos = egui::pos2(
                pane.rect.right() - width - 8.0,
                (pane.rect.bottom() - estimated_height - 8.0).max(pane.rect.top() + 8.0),
            );
            let response = egui::Area::new(egui::Id::new(("input-stack", pane.pane_id)))
                .fixed_pos(pos)
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    egui::Frame::new()
                        .fill(egui::Color32::from_rgb(31, 34, 40))
                        .stroke(egui::Stroke::new(
                            1.5_f32,
                            pending_outline,
                        ))
                        .corner_radius(6.0)
                        .inner_margin(8.0)
                        .show(ui, |ui| {
                            let font = egui::FontId::proportional(16.8);
                            ui.style_mut()
                                .text_styles
                                .insert(egui::TextStyle::Body, font.clone());
                            ui.style_mut()
                                .text_styles
                                .insert(egui::TextStyle::Button, font.clone());
                            ui.style_mut()
                                .text_styles
                                .insert(egui::TextStyle::Small, font);
                            ui.set_width(width - 16.0);
                            ui.horizontal(|ui| {
                                ui.strong(format!("Input stack · {count}"));
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .button("×")
                                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                                            .clicked()
                                        {
                                            action = Some(Action::Collapse(pane.pane_id));
                                        }
                                        if ui
                                            .button("+ Add")
                                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                                            .clicked()
                                        {
                                            window_ui.editor = Some(pane.pane_id);
                                        }
                                    },
                                );
                            });

                            if let Some(stack) = stacks.get(&pane.pane_id) {
                                let auto_index = stack.auto_delivery.as_ref().map(|auto| auto.index);
                                egui::ScrollArea::vertical()
                                    .max_height(150.0)
                                    .show(ui, |ui| {
                                        for (index, item) in stack.queued.iter().enumerate() {
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                        let enabled = auto_index.is_none();
                                                        if auto_index == Some(index) {
                                                            if ui
                                                                .button("Cancel")
                                                                .on_hover_cursor(
                                                                    egui::CursorIcon::PointingHand,
                                                                )
                                                                .clicked()
                                                            {
                                                                action = Some(
                                                                    Action::CancelAutoDelivery(
                                                                        pane.pane_id,
                                                                    ),
                                                                );
                                                            }
                                                            ui.label("Waiting…");
                                                        } else {
                                                            if ui
                                                                .add_enabled(
                                                                    enabled,
                                                                    egui::Button::new(
                                                                        "Send when ready",
                                                                    ),
                                                                )
                                                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                                                .on_hover_text("Paste and submit when the agent composer is ready")
                                                                .clicked()
                                                            {
                                                                action = Some(Action::DeliverWhenReady(pane.pane_id, index));
                                                            }
                                                            if ui.add_enabled(enabled, egui::Button::new("Paste"))
                                                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                                                .on_hover_text("Paste now without submitting")
                                                                .clicked()
                                                            {
                                                                action = Some(Action::Deliver(
                                                                    pane.pane_id,
                                                                    index,
                                                                    item.clone(),
                                                                ));
                                                            }
                                                        }
                                                        ui.add(egui::Label::new(item).truncate());
                                                },
                                            );
                                        }
                                    });
                            }
                            if let Some(error) = window_ui.errors.get(&pane.pane_id) {
                                ui.colored_label(egui::Color32::from_rgb(235, 95, 110), error);
                            }

                            if window_ui.editor == Some(pane.pane_id) {
                                let stack = stacks.entry(pane.pane_id).or_default();
                                let edit = ui.add(
                                    egui::TextEdit::singleline(&mut stack.draft)
                                        .code_editor()
                                        .font(egui::FontId::monospace(18.2))
                                        .desired_width(f32::INFINITY)
                                        .hint_text("Input for later"),
                                );
                                edit.request_focus();
                                // A single-line TextEdit normally loses focus on Enter, but we
                                // immediately retain focus for fast consecutive additions.
                                // Submit from the key event itself instead of relying on focus.
                                let queue = ui.input(|input| input.key_pressed(egui::Key::Enter));
                                ui.horizontal(|ui| {
                                    ui.label("Esc keeps draft · Enter queues");
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui
                                                .add_enabled(
                                                    !stack.draft.trim().is_empty(),
                                                    egui::Button::new("Queue"),
                                                )
                                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                                .clicked()
                                                || queue
                                            {
                                                action = Some(Action::Queue(
                                                    pane.pane_id,
                                                    stack.draft.clone(),
                                                ));
                                            }
                                        },
                                    );
                                });
                                if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
                                    window_ui.editor = None;
                                    action = Some(Action::Collapse(pane.pane_id));
                                }
                            }
                        });
                });

            if ctx.input(|input| input.pointer.any_click())
                && !response
                    .response
                    .rect
                    .contains(ctx.input(|input| input.pointer.interact_pos().unwrap_or_default()))
            {
                window_ui.editor = None;
            }
        }
        drop(stacks);
        drop(windows);

        match action {
            Some(Action::Queue(pane_id, input)) => {
                if let Some(backup) = self.queue_input(pane_id, &input) {
                    os_window.set_clipboard(Clipboard::Clipboard, backup);
                    self.input_stack_windows
                        .borrow_mut()
                        .entry(window_id)
                        .or_default()
                        .editor = None;
                }
            }
            Some(Action::Deliver(pane_id, index, input)) => {
                os_window.set_clipboard(Clipboard::Clipboard, input.clone());
                let result = Mux::get()
                    .get_pane(pane_id)
                    .ok_or_else(|| anyhow::anyhow!("pane no longer exists"))
                    .and_then(|pane| pane.send_paste(&input));
                let mut windows = self.input_stack_windows.borrow_mut();
                let ui = windows.entry(window_id).or_default();
                match result {
                    Ok(()) => {
                        ui.errors.remove(&pane_id);
                        drop(windows);
                        self.remove_queued_input(pane_id, index);
                    }
                    Err(error) => {
                        ui.errors
                            .insert(pane_id, format!("Paste failed: {error:#}"));
                    }
                }
            }
            Some(Action::DeliverWhenReady(pane_id, index)) => {
                if let Some(stack) = self.input_stacks.borrow_mut().get_mut(&pane_id) {
                    if stack.auto_delivery.is_none() && index < stack.queued.len() {
                        stack.auto_delivery = Some(AutoDelivery { index });
                        self.input_stack_windows
                            .borrow_mut()
                            .entry(window_id)
                            .or_default()
                            .errors
                            .remove(&pane_id);
                        ctx.request_repaint();
                    }
                }
            }
            Some(Action::CancelAutoDelivery(pane_id)) => {
                if let Some(stack) = self.input_stacks.borrow_mut().get_mut(&pane_id) {
                    stack.auto_delivery = None;
                    ctx.request_repaint();
                }
            }
            Some(Action::Collapse(pane_id)) => {
                let mut windows = self.input_stack_windows.borrow_mut();
                let ui = windows.entry(window_id).or_default();
                ui.expanded.remove(&pane_id);
                if ui.editor == Some(pane_id) {
                    ui.editor = None;
                }
            }
            None => {}
        }
    }

    fn advance_auto_deliveries(&self, window_id: MuxWindowId, panes: &[InputStackPaneRect]) {
        for pane_rect in panes {
            let pane_id = pane_rect.pane_id;
            let Some(pane) = Mux::get().get_pane(pane_id) else {
                continue;
            };
            let readiness = crate::delivery_readiness::detect_pane(pane.as_ref());
            let mut stacks = self.input_stacks.borrow_mut();
            let Some(stack) = stacks.get_mut(&pane_id) else {
                continue;
            };
            let Some(auto) = stack.auto_delivery.as_mut() else {
                continue;
            };
            if readiness != crate::delivery_readiness::DeliveryReadiness::Ready {
                continue;
            }
            let index = auto.index;
            let Some(input) = stack.queued.get(index).cloned() else {
                stack.auto_delivery = None;
                continue;
            };
            drop(stacks);
            if let Err(error) = pane.send_paste(&input) {
                if let Some(stack) = self.input_stacks.borrow_mut().get_mut(&pane_id) {
                    stack.auto_delivery = None;
                }
                self.input_stack_windows
                    .borrow_mut()
                    .entry(window_id)
                    .or_default()
                    .errors
                    .insert(pane_id, format!("Paste failed: {error:#}"));
            } else if let Err(error) = pane.send_composed_text("\r") {
                if let Some(stack) = self.input_stacks.borrow_mut().get_mut(&pane_id) {
                    stack.auto_delivery = None;
                }
                self.input_stack_windows
                    .borrow_mut()
                    .entry(window_id)
                    .or_default()
                    .errors
                    .insert(
                        pane_id,
                        format!("Submit failed; text was pasted: {error:#}"),
                    );
            } else {
                if let Some(stack) = self.input_stacks.borrow_mut().get_mut(&pane_id) {
                    stack.auto_delivery = None;
                }
                self.input_stack_windows
                    .borrow_mut()
                    .entry(window_id)
                    .or_default()
                    .errors
                    .remove(&pane_id);
                self.remove_queued_input(pane_id, index);
            }
        }
    }

    pub(crate) fn queue_input(&self, pane_id: PaneId, input: &str) -> Option<String> {
        let input = normalize_input_stack_item(input);
        if input.trim().is_empty() {
            return None;
        }
        let mut stacks = self.input_stacks.borrow_mut();
        let stack = stacks.entry(pane_id).or_default();
        stack.draft.clear();
        stack.queued.push(input);
        Some(serialize_input_stack(&stack.queued))
    }

    pub(crate) fn input_stack_count(&self, pane_id: PaneId) -> usize {
        self.input_stacks
            .borrow()
            .get(&pane_id)
            .map(|stack| stack.queued.len())
            .unwrap_or(0)
    }

    pub(crate) fn remove_queued_input(&self, pane_id: PaneId, index: usize) {
        let mut stacks = self.input_stacks.borrow_mut();
        let Some(stack) = stacks.get_mut(&pane_id) else {
            return;
        };
        if index < stack.queued.len() {
            stack.queued.remove(index);
        }
        if stack.queued.is_empty() && stack.draft.is_empty() {
            stacks.remove(&pane_id);
        }
    }

    pub fn try_new() -> anyhow::Result<Rc<GuiFrontEnd>> {
        let connection = Connection::init()?;
        connection.set_event_handler(Self::app_event_handler);

        let mux = Mux::get();
        let client_id = mux.active_identity().expect("to have set my own id");

        let front_end = Rc::new(GuiFrontEnd {
            connection,
            switching_workspaces: RefCell::new(false),
            spawned_mux_window: RefCell::new(HashSet::new()),
            known_windows: RefCell::new(BTreeMap::new()),
            client_id: client_id.clone(),
            config_subscription: RefCell::new(None),
            input_stacks: RefCell::new(HashMap::new()),
            input_stack_windows: RefCell::new(HashMap::new()),
        });

        mux.subscribe(move |n| {
            match n {
                MuxNotification::WorkspaceRenamed {
                    old_workspace,
                    new_workspace,
                } => {
                    let mux = Mux::get();
                    let active = mux.active_workspace();
                    if active == old_workspace || active == new_workspace {
                        let switcher = WorkspaceSwitcher::new(&new_workspace);
                        promise::spawn::spawn_into_main_thread(async move {
                            drop(switcher);
                        })
                        .detach();
                    }
                }
                MuxNotification::WindowWorkspaceChanged(_)
                | MuxNotification::ActiveWorkspaceChanged(_)
                | MuxNotification::WindowCreated(_)
                | MuxNotification::WindowRemoved(_) => {
                    promise::spawn::spawn_into_main_thread(async move {
                        let fe = crate::frontend::front_end();
                        if !fe.is_switching_workspace() {
                            fe.reconcile_workspace();
                        }
                    })
                    .detach();
                }
                MuxNotification::PaneFocused(pane_id) => {
                    promise::spawn::spawn_into_main_thread(async move {
                        let mux = Mux::get();
                        if let Err(err) = mux.focus_pane_and_containing_tab(pane_id) {
                            log::error!("Error reconciling PaneFocused notification: {err:#}");
                        }
                    })
                    .detach();
                }
                MuxNotification::TabTitleChanged { .. } => {}
                MuxNotification::WindowTitleChanged { .. } => {}
                MuxNotification::TabResized(_) => {}
                MuxNotification::TabAddedToWindow { .. } => {}
                MuxNotification::PaneRemoved(pane_id) => {
                    promise::spawn::spawn_into_main_thread(async move {
                        let front_end = crate::frontend::front_end();
                        front_end.input_stacks.borrow_mut().remove(&pane_id);
                        for ui in front_end.input_stack_windows.borrow_mut().values_mut() {
                            ui.expanded.remove(&pane_id);
                            ui.errors.remove(&pane_id);
                            if ui.editor == Some(pane_id) {
                                ui.editor = None;
                            }
                        }
                    })
                    .detach();
                }
                MuxNotification::WindowInvalidated(_) => {}
                MuxNotification::PaneOutput(_) => {}
                MuxNotification::PaneAdded(_) => {}
                MuxNotification::Alert {
                    pane_id,
                    alert:
                        Alert::ToastNotification {
                            title,
                            body,
                            focus: _,
                        },
                } => {
                    let mux = Mux::get();

                    if let Some((_domain, window_id, tab_id)) = mux.resolve_pane_id(pane_id) {
                        let config = config::configuration();

                        if let Some((_fdomain, f_window, f_tab, f_pane)) =
                            mux.resolve_focused_pane(&client_id)
                        {
                            let show = match config.notification_handling {
                                NotificationHandling::NeverShow => false,
                                NotificationHandling::AlwaysShow => true,
                                NotificationHandling::SuppressFromFocusedPane => f_pane != pane_id,
                                NotificationHandling::SuppressFromFocusedTab => f_tab != tab_id,
                                NotificationHandling::SuppressFromFocusedWindow => {
                                    f_window != window_id
                                }
                            };

                            if show {
                                let message = if title.is_none() { "" } else { &body };
                                let title = title.as_ref().unwrap_or(&body);
                                // FIXME: if notification.focus is true, we should do
                                // something here to arrange to focus pane_id when the
                                // notification is clicked
                                persistent_toast_notification(title, message);
                            }
                        }
                    }
                }
                MuxNotification::Alert {
                    pane_id: _,
                    alert: Alert::Bell | Alert::Progress(_),
                } => {
                    // Handled via TermWindowNotif; NOP it here.
                }
                MuxNotification::Alert {
                    pane_id: _,
                    alert:
                        Alert::OutputSinceFocusLost
                        | Alert::PaletteChanged
                        | Alert::CurrentWorkingDirectoryChanged
                        | Alert::WindowTitleChanged(_)
                        | Alert::TabTitleChanged(_)
                        | Alert::IconTitleChanged(_)
                        | Alert::SetUserVar { .. },
                } => {}
                MuxNotification::Empty => {
                    if config::configuration().quit_when_all_windows_are_closed {
                        promise::spawn::spawn_into_main_thread(async move {
                            if mux::activity::Activity::count() == 0 {
                                log::trace!("Mux is now empty, terminate gui");
                                Connection::get().unwrap().terminate_message_loop();
                            }
                        })
                        .detach();
                    }
                }
                MuxNotification::SaveToDownloads { name, data } => {
                    if !config::configuration().allow_download_protocols {
                        log::error!(
                            "Ignoring download request for {:?}, \
                                 as allow_download_protocols=false",
                            name
                        );
                    } else if let Err(err) = crate::download::save_to_downloads(name, &*data) {
                        log::error!("save_to_downloads: {:#}", err);
                    }
                }
                MuxNotification::AssignClipboard {
                    pane_id,
                    selection,
                    clipboard,
                } => {
                    promise::spawn::spawn_into_main_thread(async move {
                        let fe = crate::frontend::front_end();
                        log::trace!(
                            "set clipboard in pane {} {:?} {:?}",
                            pane_id,
                            selection,
                            clipboard
                        );
                        if let Some(window) = fe.known_windows.borrow().keys().next() {
                            window.set_clipboard(
                                match selection {
                                    ClipboardSelection::Clipboard => Clipboard::Clipboard,
                                    ClipboardSelection::PrimarySelection => {
                                        Clipboard::PrimarySelection
                                    }
                                },
                                clipboard.unwrap_or_else(String::new),
                            );
                        } else {
                            log::error!("Cannot assign clipboard as there are no windows");
                        };
                    })
                    .detach();
                }
            }
            true
        });
        // Re-evaluate the config so that folks that are using
        // `wezterm.gui.get_appearance()` can have that take effect
        // before any windows are created
        config::reload();

        // And build the initial menu bar.
        // TODO: arrange for this to happen on config reload.
        crate::commands::CommandDef::recreate_menubar(&config::configuration());

        Ok(front_end)
    }

    fn app_event_handler(event: ApplicationEvent) {
        log::trace!("Got app event {event:?}");
        match event {
            ApplicationEvent::OpenCommandScript(file_name) => {
                let quoted_file_name = match shlex::try_quote(&file_name) {
                    Ok(name) => name.to_owned().to_string(),
                    Err(_) => {
                        log::error!(
                            "OpenCommandScript: {file_name} has embedded NUL bytes and
                             cannot be launched via the shell"
                        );
                        return;
                    }
                };
                promise::spawn::spawn(async move {
                    use config::keyassignment::SpawnTabDomain;
                    use wezterm_term::TerminalSize;

                    // We send the script to execute to the shell on stdin, rather than ask the
                    // shell to execute it directly, so that we start the shell and read in the
                    // user's rc files before running the script.  Without this, wezterm on macOS
                    // is launched with a default and very anemic path, and that is frustrating for
                    // users.

                    let mux = Mux::get();
                    let window_id = None;
                    let pane_id = None;
                    let cmd = None;
                    let cwd = None;
                    let workspace = mux.active_workspace();

                    match mux
                        .spawn_tab_or_window(
                            window_id,
                            SpawnTabDomain::DomainName("local".to_string()),
                            cmd,
                            cwd,
                            TerminalSize::default(),
                            pane_id,
                            workspace,
                            None, // optional position
                        )
                        .await
                    {
                        Ok((_tab, pane, _window_id)) => {
                            log::trace!("Spawned {file_name} as pane_id {}", pane.pane_id());
                            let mut writer = pane.writer();
                            write!(writer, "{quoted_file_name} ; exit\n").ok();
                        }
                        Err(err) => {
                            log::error!("Failed to spawn {file_name}: {err:#?}");
                        }
                    };
                })
                .detach();
            }
            ApplicationEvent::PerformKeyAssignment(action) => {
                // We should only get here when there are no windows open
                // and the user picks an action from the menubar.
                // This is not currently possible, but could be in the
                // future.

                fn spawn_command(spawn: &SpawnCommand, spawn_where: SpawnWhere) {
                    let config = config::configuration();
                    let dpi = config.dpi.unwrap_or_else(|| ::window::default_dpi());
                    let size =
                        config.initial_size(dpi as u32, crate::cell_pixel_dims(&config, dpi).ok());
                    let term_config = Arc::new(config::TermConfig::with_config(config));

                    crate::spawn::spawn_command_impl(spawn, spawn_where, size, None, term_config)
                }

                match action {
                    KeyAssignment::QuitApplication => {
                        // If we get here, there are no windows that could have received
                        // the QuitApplication command, therefore it must be ok to quit
                        // immediately
                        Connection::get().unwrap().terminate_message_loop();
                    }
                    KeyAssignment::SpawnWindow => {
                        spawn_command(&SpawnCommand::default(), SpawnWhere::NewWindow);
                    }
                    KeyAssignment::SpawnTab(spawn_where) => {
                        spawn_command(
                            &SpawnCommand {
                                domain: spawn_where,
                                ..Default::default()
                            },
                            SpawnWhere::NewWindow,
                        );
                    }
                    KeyAssignment::SpawnCommandInNewTab(spawn) => {
                        spawn_command(&spawn, SpawnWhere::NewTab);
                    }
                    KeyAssignment::SpawnCommandInNewWindow(spawn) => {
                        spawn_command(&spawn, SpawnWhere::NewWindow);
                    }
                    _ => {
                        log::warn!("unhandled perform: {action:?}");
                    }
                }
            }
        }
    }

    pub fn run_forever(&self) -> anyhow::Result<()> {
        self.connection
            .run_message_loop()
            .context("running message loop")
    }

    pub fn gui_windows(&self) -> Vec<GuiWin> {
        let windows = self.known_windows.borrow();
        let mut windows: Vec<GuiWin> = windows
            .iter()
            .map(|(window, &mux_window_id)| GuiWin {
                mux_window_id,
                window: window.clone(),
            })
            .collect();
        windows.sort_by(|a, b| a.window.cmp(&b.window));
        windows
    }

    pub fn reconcile_workspace(&self) -> Future<()> {
        let mut promise = Promise::new();
        let mux = Mux::get();
        let workspace = mux.active_workspace_for_client(&self.client_id);

        if mux.is_workspace_empty(&workspace) {
            // We don't want to silently kill off things that might
            // be running in other workspaces, so let's pick one
            // and activate it
            if self.is_switching_workspace() {
                promise.ok(());
                return promise.get_future().unwrap();
            }
            for workspace in mux.iter_workspaces() {
                if !mux.is_workspace_empty(&workspace) {
                    mux.set_active_workspace_for_client(&self.client_id, &workspace);
                    log::debug!("using {} instead, as it is not empty", workspace);
                    break;
                }
            }
        }

        let workspace = mux.active_workspace_for_client(&self.client_id);
        log::debug!("workspace is {}, fixup windows", workspace);

        let mut mux_windows = mux.iter_windows_in_workspace(&workspace);

        // First, repurpose existing windows.
        // Note that both iter_windows_in_workspace and self.known_windows have a
        // deterministic iteration order, so switching back and forth should result
        // in a consistent mux <-> gui window mapping.
        let known_windows = std::mem::take(&mut *self.known_windows.borrow_mut());
        let mut windows = BTreeMap::new();
        let mut unused = BTreeMap::new();

        for (window, window_id) in known_windows.into_iter() {
            if let Some(idx) = mux_windows.iter().position(|&id| id == window_id) {
                // it already points to the desired mux window
                windows.insert(window, window_id);
                mux_windows.remove(idx);
            } else {
                unused.insert(window, window_id);
            }
        }

        let mut mux_windows = mux_windows.into_iter();

        for (window, old_id) in unused.into_iter() {
            if let Some(mux_window_id) = mux_windows.next() {
                window.notify(TermWindowNotif::SwitchToMuxWindow(mux_window_id));
                windows.insert(window, mux_window_id);
            } else {
                // We have more windows than are in the new workspace;
                // we no longer need this one!
                window.close();
                front_end().spawned_mux_window.borrow_mut().remove(&old_id);
            }
        }

        log::trace!("reconcile: windows -> {:?}", windows);
        *self.known_windows.borrow_mut() = windows;

        let future = promise.get_future().unwrap();

        // then spawn any new windows that are needed
        promise::spawn::spawn(async move {
            while let Some(mux_window_id) = mux_windows.next() {
                if front_end().has_mux_window(mux_window_id)
                    || front_end()
                        .spawned_mux_window
                        .borrow()
                        .contains(&mux_window_id)
                {
                    continue;
                }
                front_end()
                    .spawned_mux_window
                    .borrow_mut()
                    .insert(mux_window_id);
                log::trace!("Creating TermWindow for mux_window_id={}", mux_window_id);
                if let Err(err) = TermWindow::new_window(mux_window_id).await {
                    log::error!("Failed to create window: {:#}", err);
                    let mux = Mux::get();
                    mux.kill_window(mux_window_id);
                    front_end()
                        .spawned_mux_window
                        .borrow_mut()
                        .remove(&mux_window_id);
                }
            }
            *front_end().switching_workspaces.borrow_mut() = false;
            promise.ok(());
        })
        .detach();
        future
    }

    fn has_mux_window(&self, mux_window_id: MuxWindowId) -> bool {
        for &mux_id in self.known_windows.borrow().values() {
            if mux_id == mux_window_id {
                return true;
            }
        }
        false
    }

    pub fn switch_workspace(&self, workspace: &str) {
        let mux = Mux::get();
        mux.set_active_workspace_for_client(&self.client_id, workspace);
        *self.switching_workspaces.borrow_mut() = false;
        self.reconcile_workspace();
    }

    pub fn record_known_window(&self, window: Window, mux_window_id: MuxWindowId) {
        self.known_windows
            .borrow_mut()
            .insert(window, mux_window_id);
        if !self.is_switching_workspace() {
            self.reconcile_workspace();
        }
    }

    pub fn forget_known_window(&self, window: &Window) {
        self.known_windows.borrow_mut().remove(window);
        if !self.is_switching_workspace() {
            self.reconcile_workspace();
        }
    }

    pub fn is_switching_workspace(&self) -> bool {
        *self.switching_workspaces.borrow()
    }

    pub fn gui_window_for_mux_window(&self, mux_window_id: MuxWindowId) -> Option<GuiWin> {
        let windows = self.known_windows.borrow();
        for (window, v) in windows.iter() {
            if *v == mux_window_id {
                return Some(GuiWin {
                    mux_window_id,
                    window: window.clone(),
                });
            }
        }
        None
    }
}

thread_local! {
    static FRONT_END: RefCell<Option<Rc<GuiFrontEnd>>> = RefCell::new(None);
}

pub fn try_front_end() -> Option<Rc<GuiFrontEnd>> {
    FRONT_END.with(|f| f.borrow().as_ref().map(Rc::clone))
}

pub fn front_end() -> Rc<GuiFrontEnd> {
    FRONT_END
        .with(|f| f.borrow().as_ref().map(Rc::clone))
        .expect("to be called on gui thread")
}

pub struct WorkspaceSwitcher {
    new_name: String,
}

impl WorkspaceSwitcher {
    pub fn new(new_name: &str) -> Self {
        *front_end().switching_workspaces.borrow_mut() = true;
        Self {
            new_name: new_name.to_string(),
        }
    }

    pub fn do_switch(self) {
        // Drop is invoked, which will complete the switch
    }
}

impl Drop for WorkspaceSwitcher {
    fn drop(&mut self) {
        front_end().switch_workspace(&self.new_name);
    }
}

#[cfg(test)]
mod input_stack_tests {
    use super::{normalize_input_stack_item, serialize_input_stack};

    #[test]
    fn input_stack_normalizes_newline_runs_without_trimming_spaces() {
        assert_eq!(
            normalize_input_stack_item("  first\r\n\nsecond  "),
            "  first second  "
        );
        assert_eq!(normalize_input_stack_item("a\r\r\nb"), "a b");
    }

    #[test]
    fn input_stack_clipboard_backup_is_numbered() {
        assert_eq!(
            serialize_input_stack(&["first".to_string(), "second".to_string()]),
            "1. first\n2. second"
        );
    }
}

pub fn shutdown() {
    FRONT_END.with(|f| drop(f.borrow_mut().take()));
}

pub fn try_new() -> Result<Rc<GuiFrontEnd>, Error> {
    let front_end = GuiFrontEnd::try_new()?;
    FRONT_END.with(|f| *f.borrow_mut() = Some(Rc::clone(&front_end)));

    let config_subscription = config::subscribe_to_config_reload({
        move || {
            promise::spawn::spawn_into_main_thread(async {
                crate::commands::CommandDef::recreate_menubar(&config::configuration());
            })
            .detach();
            true
        }
    });
    front_end
        .config_subscription
        .borrow_mut()
        .replace(config_subscription);

    Ok(front_end)
}
