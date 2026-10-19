//! Cleanup for subprocess groups created exclusively by Poundlock.
use anyhow::{Context, Result};
use tokio::process::{Child, Command};

pub(crate) fn isolate(command: &mut Command) {
    command.kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
}

pub(crate) struct ProcessGroup {
    pid: u32,
    armed: bool,
}
impl ProcessGroup {
    pub(crate) fn new(child: &Child) -> Result<Self> {
        Ok(Self {
            pid: child.id().context("Subprocess has no ID")?,
            armed: true,
        })
    }
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
    pub(crate) fn terminate(&self) {
        if !self.armed {
            return;
        }
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.pid as i32), libc::SIGKILL);
        }
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.terminate();
        self.armed = false;
    }
}
