//! Calls into the esp-lan-ota cluster over an operational CASE session.

use core::num::NonZeroU8;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use esp_lan_ota::matter::{cmd, error_text, status_field, CLUSTER_ID};
use esp_lan_ota::{Chip, ImageState, Slot};
use rs_matter::crypto::Crypto;
use rs_matter::im::client::ImClient;
use rs_matter::sc::case::CaseInitiator;
use rs_matter::tlv::{TLVElement, TLVTag, TLVWrite};
use rs_matter::transport::exchange::Exchange;
use rs_matter::transport::network::Address;
use rs_matter::Matter;

use crate::controller::peer;

/// Device status as reported by the cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceStatus {
    pub project: String,
    pub version: String,
    pub chip: Option<Chip>,
    pub booted: Slot,
    pub state: Option<ImageState>,
    pub rollback: &'static str,
    pub upload: Option<(u32, u32)>,
    pub staged_version: Option<String>,
    pub last_error: u8,
    pub max_chunk: u16,
}

impl DeviceStatus {
    pub fn confirmed(&self) -> bool {
        !self.state.is_some_and(ImageState::is_unconfirmed)
    }

    pub fn last_error_text(&self) -> &'static str {
        error_text(self.last_error)
    }
}

/// Writes one command, sends it, and hands the response struct (if the
/// device sent one) to `$parse`.
macro_rules! invoke {
    ($client:expr, $cmd:expr, $resp:expr, |$w:ident| $fields:block, |$data:ident| $parse:expr) => {{
        let client = $client;
        let exchange = client.exchange().await?;
        let mut chunk = exchange
            .invoke_with(None, |builder| {
                builder
                    .invoke_requests()?
                    .push()?
                    .path(client.endpoint, CLUSTER_ID, $cmd)?
                    .data(|$w| {
                        $w.start_struct(&TLVTag::Context(1))?;
                        $fields
                        $w.end_container()
                    })?
                    .end()?
                    .end()?
                    .end()
            })
            .await
            .map_err(|e| anyhow!("command {:#04x}: {e:?}", $cmd))?;
        let result: Result<_> = (|| {
            let Some(response) = chunk.response().map_err(|e| anyhow!("{e:?}"))? else {
                let $data: Option<TLVElement<'_>> = None;
                return $parse;
            };
            if let Some((_, status)) = response.statuses(CLUSTER_ID, $cmd).next() {
                status.map_err(|e| anyhow!("device refused command {:#04x}: {e:?}", $cmd))?;
            }
            let found = match $resp {
                Some(resp) => response.responses::<TLVElement<'_>>(CLUSTER_ID, resp).next(),
                None => None,
            };
            let $data = match found {
                Some((_, data)) => Some(data.map_err(|e| anyhow!("{e:?}"))?),
                None => None,
            };
            $parse
        })();
        while let Some(next) = chunk.complete().await.map_err(|e| anyhow!("{e:?}"))? {
            chunk = next;
        }
        result
    }};
}

fn require<'a>(data: Option<TLVElement<'a>>) -> Result<TLVElement<'a>> {
    data.context("device sent no response payload")
}

fn tlv<T>(result: Result<T, rs_matter::error::Error>) -> Result<T> {
    result.map_err(|e| anyhow!("malformed response: {e:?}"))
}

/// An operational session with one enrolled device.
pub struct Client<'a, C> {
    pub matter: &'a Matter<'a>,
    pub crypto: &'a C,
    pub fab_idx: NonZeroU8,
    pub node_id: u64,
    pub endpoint: u16,
    pub addr: SocketAddr,
}

impl<'a, C: Crypto> Client<'a, C> {
    /// Establishes CASE with the device at `addr`. Later exchanges reuse it.
    pub async fn connect(
        matter: &'a Matter<'a>,
        crypto: &'a C,
        fab_idx: NonZeroU8,
        node_id: u64,
        addr: SocketAddr,
    ) -> Result<Self> {
        let client = Self {
            matter,
            crypto,
            fab_idx,
            node_id,
            endpoint: 1,
            addr,
        };
        client.establish().await?;
        Ok(client)
    }

    /// A fresh CASE session with the device at its known address.
    async fn establish(&self) -> Result<()> {
        let mut exchange =
            Exchange::initiate_unsecured(self.matter, self.crypto, Address::Udp(peer(self.addr)))
                .await
                .map_err(|e| anyhow!("reaching {}: {e:?}", self.addr))?;
        CaseInitiator::initiate(&mut exchange, self.crypto, self.fab_idx, self.node_id)
            .await
            .map_err(|e| anyhow!("CASE with {}: {e:?}", self.addr))?;
        Ok(())
    }

    /// An exchange on the existing session, re-establishing CASE by address
    /// when the session is gone (device reboot, session closed after an
    /// error). The host never relies on mDNS to find an enrolled device.
    async fn exchange(&self) -> Result<Exchange<'a>> {
        if let Ok(exchange) = self.try_exchange().await {
            return Ok(exchange);
        }
        self.establish().await?;
        self.try_exchange().await
    }

    async fn try_exchange(&self) -> Result<Exchange<'a>> {
        let initiate = Exchange::initiate(self.matter, self.crypto, self.fab_idx, self.node_id);
        let timeout = async_io::Timer::after(Duration::from_secs(2));
        match embassy_futures::select::select(initiate, timeout).await {
            embassy_futures::select::Either::First(result) => {
                result.map_err(|e| anyhow!("opening exchange: {e:?}"))
            }
            embassy_futures::select::Either::Second(_) => bail!("no session"),
        }
    }

    /// Finds the endpoint hosting the cluster by asking each in turn.
    pub async fn find_endpoint(&mut self, hint: Option<u16>) -> Result<u16> {
        let candidates = hint.into_iter().chain(1..=16);
        let mut last = None;
        for endpoint in candidates {
            self.endpoint = endpoint;
            match self.status().await {
                Ok(_) => return Ok(endpoint),
                Err(error) => last = Some(error),
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("no endpoints probed")))
            .context("the device has no esp-lan-ota cluster; is the firmware integrated?")
    }

    pub async fn status(&self) -> Result<DeviceStatus> {
        invoke!(self, cmd::STATUS, Some(cmd::STATUS_RESPONSE), |w| {}, |data| {
            let fields = tlv(require(data)?.r#struct())?;
            let get = |tag: u8| tlv(fields.ctx(tag));
            let len = tlv(get(status_field::UPLOAD_LEN)?.u32())?;
            let next = tlv(get(status_field::UPLOAD_NEXT)?.u32())?;
            let staged = tlv(get(status_field::STAGED)?.bool())?;
            let state = tlv(get(status_field::IMAGE_STATE)?.u32())?;
            Ok(DeviceStatus {
                project: tlv(get(status_field::PROJECT)?.utf8())?.to_owned(),
                version: tlv(get(status_field::VERSION)?.utf8())?.to_owned(),
                chip: Chip::from_image_id(tlv(get(status_field::CHIP_ID)?.u16())?),
                booted: if tlv(get(status_field::BOOTED_SLOT)?.u8())? == 0 {
                    Slot::Ota0
                } else {
                    Slot::Ota1
                },
                state: (state != ImageState::Undefined.raw()).then(|| ImageState::from_raw(state)),
                rollback: match tlv(get(status_field::ROLLBACK)?.u8())? {
                    1 => "supported",
                    2 => "unsupported",
                    _ => "unknown",
                },
                upload: (len > 0).then_some((len, next)),
                staged_version: staged
                    .then(|| -> Result<String> {
                        Ok(tlv(get(status_field::STAGED_VERSION)?.utf8())?.to_owned())
                    })
                    .transpose()?,
                last_error: tlv(get(status_field::LAST_ERROR)?.u8())?,
                max_chunk: tlv(get(status_field::MAX_CHUNK)?.u16())?,
            })
        })
    }

    /// Starts or resumes an upload; returns the offset to send next.
    pub async fn begin(&self, len: u32, sha256: &[u8; 32]) -> Result<(u32, u16)> {
        invoke!(
            self,
            cmd::BEGIN,
            Some(cmd::BEGIN_RESPONSE),
            |w| {
                w.u32(&TLVTag::Context(0), len)?;
                w.str(&TLVTag::Context(1), sha256)?;
            },
            |data| {
                let fields = tlv(require(data)?.r#struct())?;
                Ok((
                    tlv(tlv(fields.ctx(0))?.u32())?,
                    tlv(tlv(fields.ctx(1))?.u16())?,
                ))
            }
        )
    }

    pub async fn write(&self, offset: u32, chunk: &[u8]) -> Result<()> {
        invoke!(
            self,
            cmd::WRITE,
            None::<u32>,
            |w| {
                w.u32(&TLVTag::Context(0), offset)?;
                w.str(&TLVTag::Context(1), chunk)?;
            },
            |_data| Ok(())
        )
    }

    /// Verifies and stages the image. The device reboots shortly after.
    pub async fn finish(&self) -> Result<(Slot, String)> {
        invoke!(self, cmd::FINISH, Some(cmd::FINISH_RESPONSE), |w| {}, |data| {
            let fields = tlv(require(data)?.r#struct())?;
            let slot = if tlv(tlv(fields.ctx(0))?.u8())? == 0 {
                Slot::Ota0
            } else {
                Slot::Ota1
            };
            Ok((slot, tlv(tlv(fields.ctx(1))?.utf8())?.to_owned()))
        })
    }

    pub async fn abort(&self) -> Result<()> {
        invoke!(self, cmd::ABORT, None::<u32>, |w| {}, |_data| Ok(()))
    }

    /// Uploads `image`, resuming after transient failures. Reports progress
    /// as `(sent, total)`. A refusal (wrong project, unconfirmed image, ...)
    /// fails at once with the device's reason.
    pub async fn upload(
        &self,
        image: &[u8],
        sha256: &[u8; 32],
        mut progress: impl FnMut(u32, u32),
    ) -> Result<(Slot, String)> {
        let len = u32::try_from(image.len()).context("image larger than 4 GiB")?;
        let begin = self.begin(len, sha256).await;
        let (mut offset, max_chunk) = with_reason(self, begin).await?;
        let chunk_len = usize::from(max_chunk).clamp(64, esp_lan_ota::MAX_CHUNK) & !3;
        let mut failures = 0u32;
        while offset < len {
            let end = (offset as usize + chunk_len).min(image.len());
            match self.write(offset, &image[offset as usize..end]).await {
                Ok(()) => {
                    offset = end as u32;
                    failures = 0;
                    progress(offset, len);
                }
                Err(error) => {
                    // Out of order (5) or no device error (lost packet) is
                    // recoverable; anything else is a refusal.
                    let device_error = self.status().await.map(|s| s.last_error).unwrap_or(0);
                    if device_error != 0 && device_error != 5 {
                        bail!("{error:#}: {}", error_text(device_error));
                    }
                    failures += 1;
                    if failures > 8 {
                        return Err(error.context(format!("writing at offset {offset}")));
                    }
                    log::warn!("write at {offset} failed ({error:#}); resyncing");
                    async_io::Timer::after(Duration::from_millis(500 * u64::from(failures))).await;
                    // Begin with the same image returns the device's offset.
                    offset = self.begin(len, sha256).await?.0;
                }
            }
        }
        let finish = self.finish().await;
        with_reason(self, finish).await
    }

    /// Opens a basic commissioning window (AdministratorCommissioning,
    /// timed invoke) so another ecosystem such as Apple Home can pair with the
    /// device's own setup code. Our fabric stays.
    pub async fn open_basic_window(&self, seconds: u16) -> Result<()> {
        use rs_matter::dm::clusters::decl::administrator_commissioning::AdministratorCommissioningCmdRequests as _;
        const ADMIN_COMMISSIONING: u32 = 0x003C;
        const OPEN_BASIC_WINDOW: u32 = 0x01;
        let exchange = self.exchange().await?;
        let mut chunk = exchange
            .invoke_with(Some(5000), |msg| {
                msg.timed_request(true)?
                    .invoke_requests()?
                    .administrator_commissioning_inv()
                    .open_basic_commissioning_window(0)?
                    .commissioning_timeout(seconds)?
                    .end()?
                    .end()?
                    .end()?
                    .end()
            })
            .await
            .map_err(|e| anyhow!("OpenBasicCommissioningWindow: {e:?}"))?;
        let result = match chunk.response().map_err(|e| anyhow!("{e:?}"))? {
            Some(response) => match response.statuses(ADMIN_COMMISSIONING, OPEN_BASIC_WINDOW).next() {
                Some((_, status)) => status.map_err(|e| anyhow!("device refused to open a window: {e:?}")),
                None => Ok(()),
            },
            None => Ok(()),
        };
        while let Some(next) = chunk.complete().await.map_err(|e| anyhow!("{e:?}"))? {
            chunk = next;
        }
        result
    }

    /// The device's last error text, when it has one.
    pub async fn last_error(&self) -> Option<&'static str> {
        let status = self.status().await.ok()?;
        (status.last_error != 0).then(|| status.last_error_text())
    }
}

/// Fails with the device's reason when a step was refused.
pub async fn with_reason<C: Crypto, T>(client: &Client<'_, C>, result: Result<T>) -> Result<T> {
    match result {
        Ok(value) => Ok(value),
        Err(error) => match client.last_error().await {
            Some(reason) => bail!("{error:#}: {reason}"),
            None => Err(error),
        },
    }
}
