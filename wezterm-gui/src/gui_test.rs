//! Local-only GUI controls used by screenshot and key-binding QA.
use crate::frontend::front_end;
use crate::termwindow::TermWindowNotif;
use anyhow::{anyhow, Context};
use mux::window::WindowId;
use promise::spawn::{block_on, spawn_into_main_thread};
use serde::Deserialize;
use serde_json::json;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use window::WindowOps;

#[derive(Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case")]
enum Request {
    Key {
        window_id: WindowId,
        key: String,
        mods: Option<String>,
    },
    ScreenshotSidebar {
        window_id: WindowId,
        path: PathBuf,
    },
}

pub fn socket_path(gui_socket: &Path) -> anyhow::Result<PathBuf> {
    let name = gui_socket
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("GUI socket has no UTF-8 filename"))?;
    let suffix = name
        .strip_prefix("gui-sock-")
        .ok_or_else(|| anyhow!("GUI socket name must begin with gui-sock-"))?;
    Ok(gui_socket.with_file_name(format!("gui-test-sock-{suffix}")))
}

pub fn spawn(gui_socket: &Path) -> anyhow::Result<()> {
    let path = socket_path(gui_socket)?;
    std::fs::remove_file(&path).ok();
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("binding GUI test socket {}", path.display()))?;
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    if let Err(err) = serve(stream) {
                        log::warn!("GUI test request failed: {err:#}");
                    }
                }
                Err(err) => log::warn!("GUI test socket accept failed: {err:#}"),
            }
        }
        std::fs::remove_file(path).ok();
    });
    Ok(())
}

fn serve(mut stream: UnixStream) -> anyhow::Result<()> {
    let mut request = String::new();
    stream.read_to_string(&mut request)?;
    let response = match serde_json::from_str::<Request>(&request)
        .map_err(anyhow::Error::from)
        .and_then(run_request)
    {
        Ok(value) => json!({ "ok": true, "result": value }),
        Err(err) => json!({ "ok": false, "error": format!("{err:#}") }),
    };
    stream.write_all(response.to_string().as_bytes())?;
    Ok(())
}

fn run_request(request: Request) -> anyhow::Result<serde_json::Value> {
    block_on(spawn_into_main_thread(async move {
        match request {
            Request::Key {
                window_id,
                key,
                mods,
            } => {
                let window = front_end()
                    .gui_window_for_mux_window(window_id)
                    .ok_or_else(|| anyhow!("no GUI window for mux window {window_id}"))?;
                let event = crate::scripting::guiwin::synthetic_key_event(&key, mods.as_deref())?;
                window.window.notify(TermWindowNotif::SyntheticKeyEvent(event));
                Ok(json!({}))
            }
            Request::ScreenshotSidebar { window_id, path } => {
                let window = front_end()
                    .gui_window_for_mux_window(window_id)
                    .ok_or_else(|| anyhow!("no GUI window for mux window {window_id}"))?;
                let (tx, rx) = smol::channel::bounded(1);
                window.window.notify(TermWindowNotif::ScreenshotSidebar {
                    path,
                    hover: None,
                    offsets_ms: vec![0],
                    tx,
                });
                let paths = rx.recv().await??;
                Ok(json!({ "paths": paths }))
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_test_socket_from_gui_socket() {
        assert_eq!(
            socket_path(Path::new("/tmp/gui-sock-42")).unwrap(),
            PathBuf::from("/tmp/gui-test-sock-42")
        );
    }
}
