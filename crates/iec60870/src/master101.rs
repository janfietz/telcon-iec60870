//! Async IEC 60870-5-101 master (primary station).
//!
//! The master initiates link reset, sends confirmed user data, and polls the
//! outstation for class-1 and class-2 data. The internal driver task owns the
//! serial stream; the [`Master101`] handle communicates with it over channels.
//!
//! # Example
//!
//! ```ignore
//! use iec60870::master101::{Master101, Master101Event};
//! use iec60870::serial::SerialSettings;
//! use iec60870_proto::frame101::link::Config as LinkConfig;
//! use iec60870_proto::frame101::frame::{LinkAddress, LinkAddressSize};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let config = LinkConfig {
//!         link_address: LinkAddress(1),
//!         addr_size: LinkAddressSize::One,
//!         ..LinkConfig::default()
//!     };
//!     let mut master = Master101::open("/dev/ttyUSB0", SerialSettings::default(), config).await?;
//!     master.reset_link().await?;
//!     while let Some(evt) = master.recv().await {
//!         match evt {
//!             Master101Event::Asdu(bytes) => println!("got {} bytes", bytes.len()),
//!             Master101Event::LinkStateChanged(state) => println!("state: {state:?}"),
//!             Master101Event::Closed(_) => break,
//!         }
//!     }
//!     Ok(())
//! }
//! ```

use iec60870_proto::frame101::link::{Config as LinkConfig, LinkState, Reason, Role};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use crate::driver101::{run, Command101, DriverEvent101};
use crate::error::{Error, Result};

#[cfg(feature = "serial")]
use crate::serial::SerialSettings;
#[cfg(feature = "serial")]
use tokio_serial::SerialPortBuilderExt as _;

/// IEC 60870-5-101 master (primary station) handle.
///
/// Wraps an internal driver task. Drop the handle to stop the driver.
#[derive(Debug)]
pub struct Master101 {
    cmd_tx: mpsc::Sender<Command101>,
    evt_rx: mpsc::Receiver<DriverEvent101>,
    _task: tokio::task::JoinHandle<Result<()>>,
}

impl Master101 {
    /// Open a serial port at `path` using `settings` and start the master
    /// driver with the given link-layer `config`.
    ///
    /// # Errors
    ///
    /// Returns an [`Error::Io`] if the serial port cannot be opened.
    #[cfg(feature = "serial")]
    pub async fn open(path: &str, settings: SerialSettings, config: LinkConfig) -> Result<Self> {
        let stream = settings
            .builder(path)
            .open_native_async()
            .map_err(|e| std::io::Error::other(e.description))?;
        Ok(Self::spawn(stream, config))
    }

    /// Drive the master over an arbitrary `AsyncRead + AsyncWrite + Unpin`
    /// stream. Useful for testing with `tokio::io::duplex`.
    pub fn open_stream<S>(stream: S, config: LinkConfig) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        Self::spawn(stream, config)
    }

    fn spawn<S>(stream: S, config: LinkConfig) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let (evt_tx, evt_rx) = mpsc::channel(64);
        let task = tokio::spawn(run(stream, Role::Master, config, cmd_rx, evt_tx));
        Self {
            cmd_tx,
            evt_rx,
            _task: task,
        }
    }

    /// Send an ASDU as a USER_DATA_CONFIRMED frame.
    pub async fn send_asdu(&self, asdu: Vec<u8>) -> Result<()> {
        self.cmd_tx
            .send(Command101::SendAsdu(asdu))
            .await
            .map_err(|_| Error::DriverGone)
    }

    /// Issue a RESET_REMOTE_LINK to the outstation.
    pub async fn reset_link(&self) -> Result<()> {
        self.cmd_tx
            .send(Command101::ResetLink)
            .await
            .map_err(|_| Error::DriverGone)
    }

    /// Request class-1 (high priority) user data from the outstation.
    pub async fn request_class1(&self) -> Result<()> {
        self.cmd_tx
            .send(Command101::RequestClass1)
            .await
            .map_err(|_| Error::DriverGone)
    }

    /// Request class-2 (low priority) user data from the outstation.
    pub async fn request_class2(&self) -> Result<()> {
        self.cmd_tx
            .send(Command101::RequestClass2)
            .await
            .map_err(|_| Error::DriverGone)
    }

    /// Receive the next event from the connection. Returns `None` when the
    /// driver has shut down.
    pub async fn recv(&mut self) -> Option<Master101Event> {
        match self.evt_rx.recv().await? {
            DriverEvent101::Asdu(bytes) => Some(Master101Event::Asdu(bytes)),
            DriverEvent101::LinkStateChanged(state) => {
                Some(Master101Event::LinkStateChanged(state))
            }
            DriverEvent101::Closed(reason) => Some(Master101Event::Closed(reason)),
        }
    }

    /// Drain events until the next ASDU arrives. Returns `None` if the
    /// driver shuts down before an ASDU arrives.
    pub async fn recv_asdu(&mut self) -> Option<Vec<u8>> {
        loop {
            match self.recv().await? {
                Master101Event::Asdu(bytes) => return Some(bytes),
                Master101Event::Closed(_) => return None,
                _ => continue,
            }
        }
    }

    /// Gracefully stop the master by dropping the command sender, which
    /// causes the driver loop to exit on the next iteration.
    pub fn stop(self) {
        drop(self);
    }
}

/// Events surfaced to the application by [`Master101::recv`].
#[derive(Debug, Clone)]
pub enum Master101Event {
    /// An ASDU was delivered from the outstation.
    Asdu(Vec<u8>),
    /// The link state changed.
    LinkStateChanged(LinkState),
    /// The link closed, optionally with an error reason.
    Closed(Option<Reason>),
}
