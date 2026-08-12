use anyhow::{anyhow, Context};
use clap::{Parser, Subcommand};
use serde::Serialize;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

#[derive(Debug, Parser, Clone)]
pub struct GuiTestCommand {
    /// Path of the target GUI's gui-sock-* socket.
    #[arg(long)]
    socket: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand, Clone)]
enum Command {
    /// Dispatch a key through the GUI's configured key bindings.
    Key {
        #[arg(long)]
        window_id: usize,
        #[arg(long)]
        key: String,
        #[arg(long, default_value = "")]
        mods: String,
    },
    /// Save the rendered sidebar for one GUI window.
    ScreenshotSidebar {
        #[arg(long)]
        window_id: usize,
        #[arg(long)]
        path: PathBuf,
    },
}

#[derive(Serialize)]
#[serde(tag = "command", rename_all = "kebab-case")]
enum Request<'a> {
    Key {
        window_id: usize,
        key: &'a str,
        mods: &'a str,
    },
    ScreenshotSidebar {
        window_id: usize,
        path: &'a Path,
    },
}

impl GuiTestCommand {
    pub async fn run(&self) -> anyhow::Result<()> {
        let socket = test_socket_path(&self.socket)?;
        let request = match &self.command {
            Command::Key {
                window_id,
                key,
                mods,
            } => Request::Key {
                window_id: *window_id,
                key,
                mods,
            },
            Command::ScreenshotSidebar { window_id, path } => Request::ScreenshotSidebar {
                window_id: *window_id,
                path,
            },
        };
        let payload = serde_json::to_vec(&request)?;
        let response = std::thread::spawn(move || request_response(&socket, &payload))
            .join()
            .map_err(|_| anyhow!("GUI test client thread panicked"))??;
        let value: serde_json::Value = serde_json::from_slice(&response)?;
        if value.get("ok").and_then(|ok| ok.as_bool()) == Some(true) {
            if let Some(result) = value.get("result") {
                println!("{}", serde_json::to_string(result)?);
            }
            Ok(())
        } else {
            Err(anyhow!(
                "{}",
                value
                    .get("error")
                    .and_then(|error| error.as_str())
                    .unwrap_or("invalid GUI test response")
            ))
        }
    }
}

fn request_response(socket: &Path, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("connecting to GUI test socket {}", socket.display()))?;
    stream.write_all(payload)?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut response = vec![];
    stream.read_to_end(&mut response)?;
    Ok(response)
}

fn test_socket_path(gui_socket: &Path) -> anyhow::Result<PathBuf> {
    let name = gui_socket
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("GUI socket has no UTF-8 filename"))?;
    let suffix = name
        .strip_prefix("gui-sock-")
        .ok_or_else(|| anyhow!("GUI socket name must begin with gui-sock-"))?;
    Ok(gui_socket.with_file_name(format!("gui-test-sock-{suffix}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_test_socket_from_gui_socket() {
        assert_eq!(
            test_socket_path(Path::new("/tmp/gui-sock-42")).unwrap(),
            PathBuf::from("/tmp/gui-test-sock-42")
        );
    }
}
