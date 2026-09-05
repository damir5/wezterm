use crate::termwindow::TermWindowNotif;
use crate::TermWindow;
use config::keyassignment::{ClipboardCopyDestination, ClipboardPasteSource};
use mux::pane::Pane;
use mux::Mux;
use std::sync::Arc;
use window::{Clipboard, WindowOps};

impl TermWindow {
    pub fn copy_to_clipboard(&self, clipboard: ClipboardCopyDestination, text: String) {
        let clipboard = match clipboard {
            ClipboardCopyDestination::Clipboard => [Some(Clipboard::Clipboard), None],
            ClipboardCopyDestination::PrimarySelection => [Some(Clipboard::PrimarySelection), None],
            ClipboardCopyDestination::ClipboardAndPrimarySelection => [
                Some(Clipboard::Clipboard),
                Some(Clipboard::PrimarySelection),
            ],
        };
        for &c in &clipboard {
            if let Some(c) = c {
                self.window.as_ref().unwrap().set_clipboard(c, text.clone());
            }
        }
    }

    pub fn paste_from_clipboard(&mut self, pane: &Arc<dyn Pane>, clipboard: ClipboardPasteSource) {
        let pane_id = pane.pane_id();
        if !crate::frontend::front_end().begin_clipboard_paste(pane_id) {
            return;
        }
        log::trace!(
            "paste_from_clipboard in pane {} {:?}",
            pane.pane_id(),
            clipboard
        );
        let window = self.window.as_ref().unwrap().clone();
        window.invalidate();
        let clipboard = match clipboard {
            ClipboardPasteSource::Clipboard => Clipboard::Clipboard,
            ClipboardPasteSource::PrimarySelection => Clipboard::PrimarySelection,
        };
        let future = window.get_clipboard(clipboard);
        promise::spawn::spawn(async move {
            let clip = future.await;
            window.notify(TermWindowNotif::Apply(Box::new(move |myself| {
                let pane = myself
                    .pane_state(pane_id)
                    .overlay
                    .as_ref()
                    .map(|overlay| overlay.pane.clone())
                    .or_else(|| {
                        let mux = Mux::get();
                        mux.get_pane(pane_id)
                    });
                let window = myself.window.as_ref().unwrap().clone();
                if let Ok(clip) = &clip {
                    crate::frontend::front_end().clipboard_paste_sending(pane_id, clip.len());
                    window.invalidate();
                }
                promise::spawn::spawn(async move {
                    let result = async {
                        let clip = clip
                            .map_err(|err| anyhow::anyhow!("Could not read clipboard: {err:#}"))?;
                        let pane = pane.ok_or_else(|| anyhow::anyhow!("Target pane closed"))?;
                        pane.send_paste_async(&clip).await
                    }
                    .await;
                    if let Err(err) = &result {
                        log::error!("Clipboard paste to pane {pane_id} failed: {err:#}");
                    }
                    let duration =
                        crate::frontend::front_end().finish_clipboard_paste(pane_id, result);
                    window.invalidate();
                    smol::Timer::after(duration).await;
                    window.invalidate();
                })
                .detach();
            })));
        })
        .detach();
        self.maybe_scroll_to_bottom_for_input(&pane);
    }
}
