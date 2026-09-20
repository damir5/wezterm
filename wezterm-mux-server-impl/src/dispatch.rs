use crate::sessionhandler::{PduSender, SessionHandler};
use anyhow::Context;
use async_ossl::AsyncSslStream;
use codec::{DecodedPdu, Pdu};
use futures::FutureExt;
use mux::{Mux, MuxNotification};
use smol::prelude::*;
use smol::Async;
use wezterm_uds::UnixStream;

#[cfg(unix)]
pub trait AsRawDesc: std::os::unix::io::AsRawFd + std::os::fd::AsFd {}
#[cfg(windows)]
pub trait AsRawDesc: std::os::windows::io::AsRawSocket + std::os::windows::io::AsSocket {}

impl AsRawDesc for UnixStream {}
impl AsRawDesc for AsyncSslStream {}

#[derive(Debug)]
enum Item {
    Notif(MuxNotification),
    WritePdu(DecodedPdu),
    Readable,
}

/// I/O errors that mean the client is gone: the session must be torn
/// down rather than reported as a protocol failure.
fn is_client_gone(err: &anyhow::Error) -> bool {
    match err.root_cause().downcast_ref::<std::io::Error>() {
        Some(err) => matches!(
            err.kind(),
            std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::NotConnected
        ),
        None => false,
    }
}

/// Write one PDU and flush it to the client.
/// Returns Ok(false) if the write failed because the client went away;
/// the caller must then stop servicing the connection so that its
/// session state and mux subscription are released.
async fn write_pdu<T>(stream: &mut Async<T>, pdu: &Pdu, serial: u64) -> anyhow::Result<bool>
where
    T: std::io::Read,
    T: std::io::Write,
    T: std::fmt::Debug,
    T: async_io::IoSafe,
{
    let result = async {
        pdu.encode_async(stream, serial)
            .await
            .context("encoding PDU to client")?;
        stream.flush().await.context("flushing PDU to client")?;
        Ok(())
    }
    .await;

    match result {
        Ok(()) => Ok(true),
        Err(err) => {
            if is_client_gone(&err) {
                // One line per dead connection: this is the trace that a
                // client vanished, so repeated writes to the same dead
                // socket are diagnosable.
                log::info!("client connection ended on write: {:#}", err);
                Ok(false)
            } else {
                Err(err)
            }
        }
    }
}

pub async fn process<T>(stream: T) -> anyhow::Result<()>
where
    T: 'static,
    T: std::io::Read,
    T: std::io::Write,
    T: AsRawDesc,
    T: std::fmt::Debug,
    T: async_io::IoSafe,
{
    let stream = smol::Async::new(stream)?;
    process_async(stream).await
}

pub async fn process_async<T>(stream: Async<T>) -> anyhow::Result<()>
where
    T: 'static,
    T: std::io::Read,
    T: std::io::Write,
    T: std::fmt::Debug,
    T: async_io::IoSafe,
{
    log::trace!("process_async called");

    let (item_tx, item_rx) = smol::channel::unbounded::<Item>();

    let pdu_sender = PduSender::new({
        let item_tx = item_tx.clone();
        move |pdu| {
            item_tx
                .try_send(Item::WritePdu(pdu))
                .map_err(|e| anyhow::anyhow!("{:?}", e))
        }
    });
    let handler = SessionHandler::new(pdu_sender);

    let mux = Mux::get();
    let tx = item_tx.clone();
    let sub_id = mux.subscribe(move |n| tx.try_send(Item::Notif(n)).is_ok());

    let result = connection_loop(stream, item_rx, handler).await;

    // Subscribers are otherwise only pruned lazily by Mux::notify;
    // remove ours now so that teardown doesn't depend on another
    // notification arriving after the client is gone.
    mux.unsubscribe(sub_id);

    result
}

async fn connection_loop<T>(
    mut stream: Async<T>,
    item_rx: smol::channel::Receiver<Item>,
    mut handler: SessionHandler,
) -> anyhow::Result<()>
where
    T: std::io::Read,
    T: std::io::Write,
    T: std::fmt::Debug,
    T: async_io::IoSafe,
{
    loop {
        let rx_msg = item_rx.recv();
        let wait_for_read = stream.readable().map(|_| Ok(Item::Readable));

        match smol::future::or(rx_msg, wait_for_read).await {
            Ok(Item::Readable) => {
                let decoded = match Pdu::decode_async(&mut stream, None).await {
                    Ok(data) => data,
                    Err(err) => {
                        if let Some(err) = err.root_cause().downcast_ref::<std::io::Error>() {
                            if err.kind() == std::io::ErrorKind::UnexpectedEof {
                                // Client disconnected: no need to make a noise
                                return Ok(());
                            }
                        }
                        return Err(err).context("reading Pdu from client");
                    }
                };
                handler.process_one(decoded);
            }
            Ok(Item::WritePdu(decoded)) => {
                if !write_pdu(&mut stream, &decoded.pdu, decoded.serial).await? {
                    // Client went away mid-write; tear down the session
                    return Ok(());
                }
            }
            Ok(Item::Notif(MuxNotification::PaneOutput(pane_id))) => {
                handler.schedule_pane_push(pane_id);
            }
            Ok(Item::Notif(MuxNotification::PaneAdded(_pane_id))) => {}
            Ok(Item::Notif(MuxNotification::PaneRemoved(pane_id))) => {
                let pdu = Pdu::PaneRemoved(codec::PaneRemoved { pane_id });
                if !write_pdu(&mut stream, &pdu, 0).await? {
                    return Ok(());
                }
            }
            Ok(Item::Notif(MuxNotification::Alert { pane_id, alert })) => {
                {
                    let per_pane = handler.per_pane(pane_id);
                    let mut per_pane = per_pane.lock().unwrap();
                    per_pane.notifications.push(alert);
                }
                handler.schedule_pane_push(pane_id);
            }
            Ok(Item::Notif(MuxNotification::SaveToDownloads { .. })) => {}
            Ok(Item::Notif(MuxNotification::AssignClipboard {
                pane_id,
                selection,
                clipboard,
            })) => {
                let pdu = Pdu::SetClipboard(codec::SetClipboard {
                    pane_id,
                    clipboard,
                    selection,
                });
                if !write_pdu(&mut stream, &pdu, 0).await? {
                    return Ok(());
                }
            }
            Ok(Item::Notif(MuxNotification::TabAddedToWindow { tab_id, window_id })) => {
                let pdu = Pdu::TabAddedToWindow(codec::TabAddedToWindow { tab_id, window_id });
                if !write_pdu(&mut stream, &pdu, 0).await? {
                    return Ok(());
                }
            }
            Ok(Item::Notif(MuxNotification::WindowRemoved(_window_id))) => {}
            Ok(Item::Notif(MuxNotification::WindowCreated(_window_id))) => {}
            Ok(Item::Notif(MuxNotification::WindowInvalidated(_window_id))) => {}
            Ok(Item::Notif(MuxNotification::WindowWorkspaceChanged(window_id))) => {
                let workspace = {
                    let mux = Mux::get();
                    mux.get_window(window_id)
                        .map(|w| w.get_workspace().to_string())
                };
                if let Some(workspace) = workspace {
                    let pdu = Pdu::WindowWorkspaceChanged(codec::WindowWorkspaceChanged {
                        window_id,
                        workspace,
                    });
                    if !write_pdu(&mut stream, &pdu, 0).await? {
                        return Ok(());
                    }
                }
            }
            Ok(Item::Notif(MuxNotification::PaneFocused(pane_id))) => {
                let pdu = Pdu::PaneFocused(codec::PaneFocused { pane_id });
                if !write_pdu(&mut stream, &pdu, 0).await? {
                    return Ok(());
                }
            }
            Ok(Item::Notif(MuxNotification::TabResized(tab_id))) => {
                let pdu = Pdu::TabResized(codec::TabResized { tab_id });
                if !write_pdu(&mut stream, &pdu, 0).await? {
                    return Ok(());
                }
            }
            Ok(Item::Notif(MuxNotification::TabTitleChanged { tab_id, title })) => {
                let pdu = Pdu::TabTitleChanged(codec::TabTitleChanged { tab_id, title });
                if !write_pdu(&mut stream, &pdu, 0).await? {
                    return Ok(());
                }
            }
            Ok(Item::Notif(MuxNotification::WindowTitleChanged { window_id, title })) => {
                let pdu = Pdu::WindowTitleChanged(codec::WindowTitleChanged { window_id, title });
                if !write_pdu(&mut stream, &pdu, 0).await? {
                    return Ok(());
                }
            }
            Ok(Item::Notif(MuxNotification::WorkspaceRenamed {
                old_workspace,
                new_workspace,
            })) => {
                let pdu = Pdu::RenameWorkspace(codec::RenameWorkspace {
                    old_workspace,
                    new_workspace,
                });
                if !write_pdu(&mut stream, &pdu, 0).await? {
                    return Ok(());
                }
            }
            Ok(Item::Notif(MuxNotification::ActiveWorkspaceChanged(_))) => {}
            Ok(Item::Notif(MuxNotification::Empty)) => {}
            Err(err) => {
                log::error!("process_async Err {}", err);
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
#[cfg(unix)]
mod test {
    use super::*;
    use codec::SetClientId;
    use mux::client::ClientId;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn socket_pair() -> (wezterm_uds::UnixStream, wezterm_uds::UnixStream) {
        // Socket paths are limited to about 100 bytes; keep this short
        let path = std::env::temp_dir().join(format!("wz-td-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = wezterm_uds::UnixListener::bind(&path).unwrap();
        let client = wezterm_uds::UnixStream::connect(&path).unwrap();
        let (server, _addr) = listener.accept().unwrap();
        drop(listener);
        let _ = std::fs::remove_file(&path);
        (client, server)
    }

    fn wait_until(what: &str, pred: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if pred() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {}", what);
    }

    /// A client dying while the server has a response queued must be torn
    /// down exactly once: the failed write ends the connection without an
    /// error, the mux client registration is released, and the
    /// notification subscription is removed so nothing keeps servicing
    /// the dead socket.
    #[test]
    fn write_failure_tears_down_session_and_subscription() {
        let mux = std::sync::Arc::new(Mux::new(
            None as Option<std::sync::Arc<mux::domain::Domain>>,
        ));
        Mux::set_mux(&mux);

        let (mut client, server) = socket_pair();
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            done_tx.send(smol::block_on(process(server))).ok();
        });

        let mut buf = vec![];
        Pdu::SetClientId(SetClientId {
            client_id: ClientId::new(),
            is_proxy: false,
        })
        .encode(&mut buf, 1)
        .unwrap();
        std::io::Write::write_all(&mut client, &buf).expect("write SetClientId");
        wait_until("client registration", || mux.iter_clients().len() == 1);

        // Kill the client; the pending response write now hits EPIPE
        drop(client);

        let result = done_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("server kept running after write failure");
        assert!(result.is_ok(), "dead client must not surface an error");

        assert_eq!(
            mux.iter_clients().len(),
            0,
            "dead client registration must be released"
        );
        assert_eq!(
            mux.subscriber_count(),
            0,
            "dead client subscription must be released"
        );

        Mux::shutdown();
    }
}
