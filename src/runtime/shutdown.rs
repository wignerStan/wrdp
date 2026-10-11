//! Daemon shutdown signals, registered before listener initialization.

use std::io;

use tokio::signal::unix::{Signal, SignalKind, signal};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShutdownReason {
    Interrupt,
    Terminate,
}

pub(crate) struct ShutdownSignal {
    interrupt: Signal,
    terminate: Signal,
}

impl ShutdownSignal {
    pub(crate) fn new() -> io::Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    pub(crate) async fn receive(&mut self) -> io::Result<ShutdownReason> {
        let (received, reason) = tokio::select! {
            received = self.interrupt.recv() => (received, ShutdownReason::Interrupt),
            received = self.terminate.recv() => (received, ShutdownReason::Terminate),
        };
        received.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "shutdown signal stream closed",
            )
        })?;
        Ok(reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Write};
    use std::process::{Command, Stdio};

    // Run real signals in a child so Tokio signal handlers cannot affect other tests.
    #[test]
    fn unix_shutdown_signals_exit_a_child_cleanly() {
        for signal in [
            nix::sys::signal::Signal::SIGINT,
            nix::sys::signal::Signal::SIGTERM,
        ] {
            let mut child = Command::new(std::env::current_exe().expect("test executable"))
                .args([
                    "--exact",
                    "runtime::shutdown::tests::shutdown_signal_child",
                    "--nocapture",
                ])
                .env("WRDP_TEST_SHUTDOWN_SIGNAL", (signal as i32).to_string())
                .stdout(Stdio::piped())
                .spawn()
                .expect("spawn signal fixture");
            let stdout = child.stdout.take().expect("signal fixture stdout");
            let mut output = std::io::BufReader::new(stdout);
            let ready = (&mut output)
                .lines()
                .any(|line| line.is_ok_and(|line| line.contains("WRDP_SHUTDOWN_READY")));
            if !ready {
                let status = child.wait().expect("wait failed signal fixture");
                panic!("signal fixture did not become ready: {status}");
            }
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(child.id() as i32), signal)
                .expect("signal isolated test child");
            assert!(
                child.wait().expect("wait signal fixture").success(),
                "{signal:?} did not exit cleanly"
            );
        }
    }

    #[test]
    fn shutdown_signal_child() {
        let Ok(expected) = std::env::var("WRDP_TEST_SHUTDOWN_SIGNAL") else {
            return;
        };
        let expected = if expected == (libc::SIGTERM).to_string() {
            ShutdownReason::Terminate
        } else {
            ShutdownReason::Interrupt
        };
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("signal fixture runtime")
            .block_on(async {
                let mut shutdown = ShutdownSignal::new().expect("register Unix shutdown signals");
                println!("WRDP_SHUTDOWN_READY");
                std::io::stdout().flush().expect("flush fixture readiness");
                let reason =
                    tokio::time::timeout(std::time::Duration::from_secs(3), shutdown.receive())
                        .await
                        .expect("signal fixture timed out")
                        .expect("receive Unix shutdown signal");
                assert_eq!(reason, expected);
            });
    }
}
