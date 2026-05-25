//! Async IEC 60870-5-101 outstation (secondary station).
//!
//! The outstation responds to requests from the master. It only transmits when
//! polled — data enqueued via [`Outstation101::send_asdu`] is held until the
//! master issues a class-1 or class-2 poll. The internal driver task owns the
//! serial stream; the [`Outstation101`] handle communicates with it over channels.
//!
//! # Example
//!
//! ```ignore
//! use iec60870::outstation101::Outstation101;
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
//!     let mut outstation = Outstation101::open("/dev/ttyUSB1", SerialSettings::default(), config).await?;
//!     while let Some(evt) = outstation.recv().await {
//!         println!("event: {evt:?}");
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

/// IEC 60870-5-101 outstation (secondary station) handle.
///
/// The outstation only sends data when the master polls it. ASDUs enqueued
/// via [`send_asdu`](Self::send_asdu) are buffered in the link-layer send
/// queue and transmitted in response to the next USER_DATA poll from the
/// master.
///
/// Wraps an internal driver task. Drop the handle to stop the driver.
#[derive(Debug)]
pub struct Outstation101 {
    cmd_tx: mpsc::Sender<Command101>,
    evt_rx: mpsc::Receiver<DriverEvent101>,
    _task: tokio::task::JoinHandle<Result<()>>,
}

impl Outstation101 {
    /// Open a serial port at `path` using `settings` and start the outstation
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

    /// Drive the outstation over an arbitrary `AsyncRead + AsyncWrite + Unpin`
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
        let task = tokio::spawn(run(stream, Role::Outstation, config, cmd_rx, evt_tx));
        Self {
            cmd_tx,
            evt_rx,
            _task: task,
        }
    }

    /// Enqueue an ASDU for transmission to the master on the next poll.
    ///
    /// The link state machine holds the data until the master issues a
    /// REQUEST_USER_DATA_CLASS_1 or REQUEST_USER_DATA_CLASS_2 poll.
    pub async fn send_asdu(&self, asdu: Vec<u8>) -> Result<()> {
        self.cmd_tx
            .send(Command101::SendAsdu(asdu))
            .await
            .map_err(|_| Error::DriverGone)
    }

    /// Receive the next event. Returns `None` when the driver has shut down.
    pub async fn recv(&mut self) -> Option<Outstation101Event> {
        match self.evt_rx.recv().await? {
            DriverEvent101::Asdu(bytes) => Some(Outstation101Event::Asdu(bytes)),
            DriverEvent101::LinkStateChanged(state) => {
                Some(Outstation101Event::LinkStateChanged(state))
            }
            DriverEvent101::Closed(reason) => Some(Outstation101Event::Closed(reason)),
        }
    }

    /// Drain events until the next ASDU arrives. Returns `None` if the
    /// driver shuts down before an ASDU arrives.
    pub async fn recv_asdu(&mut self) -> Option<Vec<u8>> {
        loop {
            match self.recv().await? {
                Outstation101Event::Asdu(bytes) => return Some(bytes),
                Outstation101Event::Closed(_) => return None,
                _ => continue,
            }
        }
    }

    /// Gracefully stop the outstation.
    pub fn stop(self) {
        drop(self);
    }
}

/// Events surfaced to the application by [`Outstation101::recv`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Outstation101Event {
    /// An ASDU was received from the master.
    Asdu(Vec<u8>),
    /// The link state changed.
    LinkStateChanged(LinkState),
    /// The link closed, optionally with an error reason.
    Closed(Option<Reason>),
}
