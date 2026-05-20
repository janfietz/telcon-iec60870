//! Serial port settings for IEC 60870-5-101.
//!
//! [`SerialSettings`] exposes the physical-layer parameters needed to open a
//! serial port for an IEC 60870-5-101 link. The `Default` impl matches the
//! most common SCADA RS-232/RS-485 configuration: 9600 baud, 8 data bits,
//! no parity, one stop bit, no flow control.

pub use tokio_serial::{DataBits, FlowControl, Parity, StopBits};

/// Physical serial-port configuration.
///
/// # Example
///
/// ```
/// use iec60870::serial::SerialSettings;
///
/// // Defaults are 9600,8,N,1 — common for SCADA RS-232/RS-485.
/// let settings = SerialSettings::default();
/// assert_eq!(settings.baud, 9_600);
/// ```
#[derive(Debug, Clone)]
pub struct SerialSettings {
    /// Baud rate in bits per second (default 9600).
    pub baud: u32,
    /// Number of data bits per frame (default 8).
    pub data_bits: DataBits,
    /// Parity mode (default None / no parity).
    pub parity: Parity,
    /// Number of stop bits (default One).
    pub stop_bits: StopBits,
    /// Hardware / software flow control (default None).
    pub flow_control: FlowControl,
}

impl Default for SerialSettings {
    fn default() -> Self {
        Self {
            baud: 9_600,
            data_bits: DataBits::Eight,
            parity: Parity::None,
            stop_bits: StopBits::One,
            flow_control: FlowControl::None,
        }
    }
}

impl SerialSettings {
    /// Create a [`tokio_serial::SerialPortBuilder`] pre-filled with these
    /// settings for the given port `path`.
    pub fn builder(&self, path: &str) -> tokio_serial::SerialPortBuilder {
        tokio_serial::new(path, self.baud)
            .data_bits(self.data_bits)
            .parity(self.parity)
            .stop_bits(self.stop_bits)
            .flow_control(self.flow_control)
    }
}
