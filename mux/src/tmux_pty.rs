use crate::tmux::{RefTmuxRemotePane, TmuxCmdQueue, TmuxDomainState};
use crate::tmux_commands::SendKeys;
use crate::DomainId;
use filedescriptor::FileDescriptor;
use parking_lot::{Condvar, Mutex};
use portable_pty::{Child, ChildKiller, ExitStatus, MasterPty};
use std::io::{Read, Write};
use std::sync::Arc;

// tmux 3.7c overflows its parser at 9,995 send-keys byte arguments.
const TMUX_SEND_KEYS_CHUNK_SIZE: usize = 8 * 1024;

fn send_keys_chunks(buf: &[u8]) -> impl Iterator<Item = &[u8]> {
    buf.chunks(TMUX_SEND_KEYS_CHUNK_SIZE)
}

/// A local tmux pane(tab) based on a tmux pty
#[derive(Debug)]
pub(crate) struct TmuxPty {
    pub domain_id: DomainId,
    pub master_pane: RefTmuxRemotePane,
    pub reader: FileDescriptor,
    pub cmd_queue: Arc<Mutex<TmuxCmdQueue>>,
}

struct TmuxPtyWriter {
    domain_id: DomainId,
    master_pane: RefTmuxRemotePane,
    cmd_queue: Arc<Mutex<TmuxCmdQueue>>,
}

impl Write for TmuxPtyWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let pane_id = {
            let pane_lock = self.master_pane.lock();
            pane_lock.pane_id
        };
        log::trace!("pane:{}, content:{:?}", &pane_id, buf);
        let mut cmd_queue = self.cmd_queue.lock();
        for keys in send_keys_chunks(buf) {
            cmd_queue.push_back(Box::new(SendKeys {
                pane: pane_id,
                keys: keys.to_vec(),
            }));
        }
        drop(cmd_queue);
        TmuxDomainState::schedule_send_next_command(self.domain_id);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Write for TmuxPty {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let pane_id = {
            let pane_lock = self.master_pane.lock();
            pane_lock.pane_id
        };
        log::trace!("pane:{}, content:{:?}", &pane_id, buf);
        let mut cmd_queue = self.cmd_queue.lock();
        for keys in send_keys_chunks(buf) {
            cmd_queue.push_back(Box::new(SendKeys {
                pane: pane_id,
                keys: keys.to_vec(),
            }));
        }
        drop(cmd_queue);
        TmuxDomainState::schedule_send_next_command(self.domain_id);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TmuxChild {
    pub active_lock: Arc<(Mutex<bool>, Condvar)>,
}

impl Child for TmuxChild {
    fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
        todo!()
    }

    fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
        let &(ref lock, ref var) = &*self.active_lock;
        let mut released = lock.lock();
        while !*released {
            var.wait(&mut released);
        }
        return Ok(ExitStatus::with_exit_code(0));
    }

    fn process_id(&self) -> Option<u32> {
        None
    }

    #[cfg(windows)]
    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        None
    }
}

#[derive(Clone, Debug)]
struct TmuxChildKiller {}

impl ChildKiller for TmuxChildKiller {
    fn kill(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "TmuxChildKiller: kill not implemented!",
        ))
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(self.clone())
    }
}

impl ChildKiller for TmuxChild {
    fn kill(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "TmuxPty: kill not implemented!",
        ))
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(TmuxChildKiller {})
    }
}

impl MasterPty for TmuxPty {
    fn resize(&self, size: portable_pty::PtySize) -> Result<(), anyhow::Error> {
        TmuxDomainState::schedule_resize(self.domain_id, self.master_pane.lock().pane_id, size);
        Ok(())
    }

    fn get_size(&self) -> Result<portable_pty::PtySize, anyhow::Error> {
        let pane = self.master_pane.lock();
        Ok(portable_pty::PtySize {
            rows: pane.pane_height as u16,
            cols: pane.pane_width as u16,
            pixel_width: 0,
            pixel_height: 0,
        })
    }

    fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>, anyhow::Error> {
        Ok(Box::new(self.reader.try_clone()?))
    }

    fn take_writer(&self) -> Result<Box<dyn Write + Send>, anyhow::Error> {
        Ok(Box::new(TmuxPtyWriter {
            domain_id: self.domain_id,
            master_pane: self.master_pane.clone(),
            cmd_queue: self.cmd_queue.clone(),
        }))
    }

    #[cfg(unix)]
    fn process_group_leader(&self) -> Option<libc::pid_t> {
        return None;
    }

    #[cfg(unix)]
    fn as_raw_fd(&self) -> Option<std::os::fd::RawFd> {
        None
    }

    #[cfg(unix)]
    fn tty_name(&self) -> Option<std::path::PathBuf> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_paste_is_split_into_safe_send_keys_commands() {
        const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
        const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";

        let mut paste = Vec::with_capacity(64 * 1024 + 13);
        paste.extend_from_slice(BRACKETED_PASTE_START);
        paste.resize(paste.len() + 64 * 1024 + 1, b'x');
        paste.extend_from_slice(BRACKETED_PASTE_END);

        let chunks: Vec<_> = send_keys_chunks(&paste).collect();
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|chunk| chunk.len() < 9_995));

        let reconstructed = chunks.concat();
        assert_eq!(reconstructed, paste);
        assert_eq!(
            reconstructed
                .windows(BRACKETED_PASTE_START.len())
                .filter(|window| *window == BRACKETED_PASTE_START)
                .count(),
            1
        );
        assert_eq!(
            reconstructed
                .windows(BRACKETED_PASTE_END.len())
                .filter(|window| *window == BRACKETED_PASTE_END)
                .count(),
            1
        );
    }
}
