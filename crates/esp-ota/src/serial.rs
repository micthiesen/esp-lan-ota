//! Reads the enrollment line a freshly USB-flashed device prints:
//! `esp-lan-ota enroll passcode=<n> discriminator=<n>`.

use std::io::{BufRead, BufReader};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Enrollment {
    pub passcode: u32,
    pub discriminator: u16,
}

pub fn parse_line(line: &str) -> Option<Enrollment> {
    let rest = &line[line.find("esp-lan-ota enroll ")? + "esp-lan-ota enroll ".len()..];
    let mut passcode = None;
    let mut discriminator = None;
    for field in rest.split_whitespace() {
        if let Some(value) = field.strip_prefix("passcode=") {
            passcode = value.parse().ok();
        } else if let Some(value) = field.strip_prefix("discriminator=") {
            discriminator = value.parse().ok();
        }
    }
    Some(Enrollment {
        passcode: passcode?,
        discriminator: discriminator?,
    })
}

/// The only USB serial port, when there is exactly one.
pub fn default_port() -> Result<String> {
    let ports: Vec<_> = serialport::available_ports()?
        .into_iter()
        .filter(|p| matches!(p.port_type, serialport::SerialPortType::UsbPort(_)))
        .map(|p| p.port_name)
        .filter(|name| !name.contains("/dev/tty.")) // macOS lists cu.* and tty.*
        .collect();
    match ports.as_slice() {
        [one] => Ok(one.clone()),
        [] => bail!("no USB serial port found; pass --serial or --code"),
        many => bail!(
            "several USB serial ports ({}); pass --serial",
            many.join(", ")
        ),
    }
}

/// Waits for the enrollment line on `port`.
pub fn read_enrollment(port: &str, timeout: Duration) -> Result<Enrollment> {
    let serial = serialport::new(port, 115_200)
        .timeout(Duration::from_millis(500))
        .open()
        .with_context(|| format!("opening {port}"))?;
    let mut reader = BufReader::new(serial);
    let deadline = Instant::now() + timeout;
    let mut line = String::new();
    while Instant::now() < deadline {
        // `read_line` keeps what it read before a timeout, so only clear
        // after a complete line.
        match reader.read_line(&mut line) {
            Ok(_) => {
                if let Some(enrollment) = parse_line(&line) {
                    return Ok(enrollment);
                }
                if line.ends_with('\n') {
                    line.clear();
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(error) => return Err(error).with_context(|| format!("reading {port}")),
        }
    }
    bail!(
        "no enrollment line on {port} within {}s; enrollment only opens on the first boot after a USB flash that erased otadata",
        timeout.as_secs()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_log_lines_with_prefixes() {
        assert_eq!(
            parse_line("I (1234) app: esp-lan-ota enroll passcode=20202021 discriminator=3840\r\n"),
            Some(Enrollment {
                passcode: 20202021,
                discriminator: 3840
            })
        );
        assert_eq!(parse_line("esp-lan-ota enroll passcode=1"), None);
        assert_eq!(parse_line("unrelated"), None);
    }
}
