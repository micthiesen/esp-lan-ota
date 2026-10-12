//! The esp-lan-ota Matter cluster (rs-matter 0.2).
//!
//! A manufacturer-specific cluster that carries [`Ota`] operations over the
//! device's existing Matter stack: no extra socket, no extra buffers. Every
//! command requires Administer privilege, so only administrators on one of
//! the device's fabrics (the `esp-ota` fabric, or Home) can update it.
//!
//! Put [`OtaCluster::CLUSTER`] on the app's main endpoint (not endpoint 0,
//! which rs-matter-stack routes to its own handler) and chain an
//! [`OtaCluster`] for it. Await [`OtaService::wait_reboot`] somewhere and
//! reset when it returns.

use core::cell::RefCell;

use embedded_storage::nor_flash::NorFlash;
use rs_matter::dm::{
    Access, AttrChangeNotifier, Cluster, ClusterId, Command, Dataver, Handler, InvokeContext,
    InvokeReply, ReadContext, ReadReply, Reply,
};
use rs_matter::error::{Error, ErrorCode};
use rs_matter::sc::pase::MAX_COMM_WINDOW_TIMEOUT_SECS;
use rs_matter::tlv::{TLVTag, TLVWrite};
use rs_matter::utils::sync::Notification;
use rs_matter::{attributes, commands, with, Matter};

use crate::flash::FlashError;
use crate::name::Name;
use crate::otadata::ImageState;
use crate::service::{BootInfo, Ota, Status};
use crate::upload::{UploadError, MAX_CHUNK};

/// Writes a response struct and completes the reply.
macro_rules! reply_struct {
    ($reply:expr, |$w:ident| $body:block) => {{
        let mut reply = $reply;
        let tag = reply.tag();
        {
            let mut $w = reply.writer();
            $w.start_struct(tag)?;
            $body
            $w.end_container()?;
        }
        reply.complete()
    }};
}

/// Manufacturer-specific cluster ID under the test vendor prefix `0xFFF1`.
pub const CLUSTER_ID: ClusterId = 0xFFF1_FC70;
pub const CLUSTER_REVISION: u16 = 1;

/// Command IDs. Responses use `0x80 | request`.
pub mod cmd {
    pub const STATUS: u32 = 0x00;
    pub const BEGIN: u32 = 0x01;
    pub const WRITE: u32 = 0x02;
    pub const FINISH: u32 = 0x03;
    pub const ABORT: u32 = 0x04;

    pub const STATUS_RESPONSE: u32 = 0x80;
    pub const BEGIN_RESPONSE: u32 = 0x81;
    pub const FINISH_RESPONSE: u32 = 0x83;
}

/// Field tags of `StatusResponse`.
pub mod status_field {
    pub const PROJECT: u8 = 0;
    pub const VERSION: u8 = 1;
    pub const CHIP_ID: u8 = 2;
    pub const BOOTED_SLOT: u8 = 3;
    /// Raw `esp_ota_img_states_t`; `0xFFFF_FFFF` when unknown.
    pub const IMAGE_STATE: u8 = 4;
    pub const ROLLBACK: u8 = 5;
    pub const UPLOAD_LEN: u8 = 6;
    pub const UPLOAD_NEXT: u8 = 7;
    pub const STAGED: u8 = 8;
    pub const STAGED_VERSION: u8 = 9;
    pub const LAST_ERROR: u8 = 10;
    pub const MAX_CHUNK: u8 = 11;
}

/// Codes reported in `StatusResponse.last_error` (0 = none).
pub fn error_code(error: &UploadError) -> u8 {
    match error {
        UploadError::NoUpload => 1,
        UploadError::TooLarge => 2,
        UploadError::TooSmall => 3,
        UploadError::BadChunk => 4,
        UploadError::OutOfOrder { .. } => 5,
        UploadError::Conflict => 6,
        UploadError::BadImage(_) => 7,
        UploadError::WrongChip => 8,
        UploadError::WrongProject => 9,
        UploadError::Incomplete => 10,
        UploadError::HashMismatch => 11,
        UploadError::Unconfirmed => 12,
        UploadError::RebootPending => 13,
        UploadError::Flash(_) => 14,
    }
}

/// Human-readable text for [`error_code`] values.
pub fn error_text(code: u8) -> &'static str {
    match code {
        0 => "none",
        1 => "no upload in progress",
        2 => "image larger than the OTA slot",
        3 => "image too small",
        4 => "malformed chunk",
        5 => "chunk out of order",
        6 => "resent chunk differs from flash",
        7 => "not an ESP app image",
        8 => "image built for another chip",
        9 => "image belongs to another project",
        10 => "upload incomplete",
        11 => "SHA-256 mismatch",
        12 => "running image not yet confirmed",
        13 => "update staged; reboot pending",
        14 => "flash error",
        _ => "unknown",
    }
}

/// Shared OTA state for the cluster handler and the app.
pub struct OtaService<F> {
    ota: RefCell<Ota<F>>,
    last_error: RefCell<u8>,
    reboot: Notification,
}

impl<F: NorFlash> OtaService<F> {
    pub fn new(ota: Ota<F>) -> Self {
        Self {
            ota: RefCell::new(ota),
            last_error: RefCell::new(0),
            reboot: Notification::new(),
        }
    }

    pub fn boot_info(&self) -> BootInfo {
        self.ota.borrow().boot_info()
    }

    pub fn status(&self) -> Status {
        self.ota.borrow().status()
    }

    /// See [`Ota::confirm`].
    pub fn confirm(&self) -> Result<bool, FlashError> {
        self.ota.borrow_mut().confirm()
    }

    pub fn is_unconfirmed(&self) -> bool {
        self.ota.borrow().is_unconfirmed()
    }

    /// Returns once an update has been staged and its `Finish` response sent.
    /// The caller then performs a software reset.
    pub async fn wait_reboot(&self) {
        loop {
            if self.ota.borrow().reboot_pending() {
                break;
            }
            self.reboot.wait().await;
        }
        // Let the FinishResponse and its acknowledgement go out first.
        embassy_time::Timer::after_millis(1500).await;
    }

    fn record<T>(&self, result: Result<T, UploadError>) -> Result<T, Error> {
        match result {
            Ok(value) => {
                *self.last_error.borrow_mut() = 0;
                Ok(value)
            }
            Err(error) => {
                *self.last_error.borrow_mut() = error_code(&error);
                Err(match error {
                    UploadError::OutOfOrder { .. }
                    | UploadError::NoUpload
                    | UploadError::Unconfirmed
                    | UploadError::RebootPending
                    | UploadError::Incomplete => ErrorCode::InvalidAction,
                    UploadError::Flash(_) => ErrorCode::Failure,
                    _ => ErrorCode::ConstraintError,
                }
                .into())
            }
        }
    }
}

/// Setup data printed while an enrollment window is open, so `esp-ota enroll`
/// can read it from the USB console.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Enrollment {
    pub passcode: u32,
    pub discriminator: u16,
}

impl core::fmt::Display for Enrollment {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "esp-lan-ota enroll passcode={} discriminator={}",
            self.passcode, self.discriminator
        )
    }
}

/// Opens a 15-minute basic commissioning window when this is the first boot
/// after a USB flash, so `esp-ota enroll` can add its fabric. Returns the
/// setup data to print on the USB console (see [`announce_enrollment`]).
/// Does nothing on other boots. A window that is already open (an
/// uncommissioned device opens one itself) is reused.
pub fn open_enrollment_window<C: rs_matter::crypto::Crypto>(
    boot: &BootInfo,
    matter: &Matter<'_>,
    crypto: C,
) -> Result<Option<Enrollment>, Error> {
    if !boot.enrollment_window {
        return Ok(None);
    }
    match matter.open_basic_comm_window(MAX_COMM_WINDOW_TIMEOUT_SECS, crypto, &NoNotify) {
        Ok(()) => {}
        Err(error)
            if error.code() == ErrorCode::Busy || error.code() == ErrorCode::InvalidAction => {}
        Err(error) => return Err(error),
    }
    let comm = matter.dev_comm();
    Ok(Some(Enrollment {
        passcode: u32::from_le_bytes(*comm.password.access()),
        discriminator: comm.discriminator,
    }))
}

/// Calls `print` with the enrollment line every 5 seconds for the 15
/// minutes the window stays open, so a host that attaches to the USB console
/// after flashing still sees it.
pub async fn announce_enrollment(enrollment: Enrollment, mut print: impl FnMut(&Enrollment)) {
    let rounds = u64::from(MAX_COMM_WINDOW_TIMEOUT_SECS) / 5;
    for _ in 0..rounds {
        print(&enrollment);
        embassy_time::Timer::after_secs(5).await;
    }
}

struct NoNotify;

impl AttrChangeNotifier for NoNotify {
    fn notify_attr_changed(&self, _endpoint_id: u16, _cluster_id: u32, _attr_id: u32) {}
    fn notify_cluster_changed(&self, _endpoint_id: u16, _cluster_id: u32) {}
    fn notify_endpoint_changed(&self, _endpoint_id: u16) {}
    fn notify_all_changed(&self) {}
}

/// The cluster handler.
pub struct OtaCluster<'a, F> {
    service: &'a OtaService<F>,
    dataver: Dataver,
}

impl<'a, F: NorFlash> OtaCluster<'a, F> {
    pub const CLUSTER: Cluster<'static> = Cluster::new(
        CLUSTER_ID,
        CLUSTER_REVISION,
        0,
        attributes!(),
        commands!(
            Command::new(cmd::STATUS, Some(cmd::STATUS_RESPONSE), Access::WA),
            Command::new(cmd::BEGIN, Some(cmd::BEGIN_RESPONSE), Access::WA),
            Command::new(cmd::WRITE, None, Access::WA),
            Command::new(cmd::FINISH, Some(cmd::FINISH_RESPONSE), Access::WA),
            Command::new(cmd::ABORT, None, Access::WA),
        ),
        &[],
        with!(all),
        with!(all),
        with!(all),
    );

    pub const fn new(service: &'a OtaService<F>, dataver: Dataver) -> Self {
        Self { service, dataver }
    }

    fn reply_status(&self, reply: impl InvokeReply) -> Result<(), Error> {
        let status = self.service.status();
        let last_error = *self.service.last_error.borrow();
        let staged_version = status.staged.map_or(Name::EMPTY, |staged| staged.version);
        let (len, next) = status.upload.unwrap_or((0, 0));
        reply_struct!(reply.with_command(cmd::STATUS_RESPONSE)?, |w| {
            use status_field as f;
            w.utf8(
                &TLVTag::Context(f::PROJECT),
                status.identity.project.as_str(),
            )?;
            w.utf8(
                &TLVTag::Context(f::VERSION),
                status.identity.version.as_str(),
            )?;
            w.u16(
                &TLVTag::Context(f::CHIP_ID),
                status.identity.chip.image_id(),
            )?;
            w.u8(
                &TLVTag::Context(f::BOOTED_SLOT),
                status.booted.index() as u8,
            )?;
            w.u32(
                &TLVTag::Context(f::IMAGE_STATE),
                status
                    .state
                    .map_or(ImageState::Undefined.raw(), ImageState::raw),
            )?;
            w.u8(&TLVTag::Context(f::ROLLBACK), status.rollback as u8)?;
            w.u32(&TLVTag::Context(f::UPLOAD_LEN), len)?;
            w.u32(&TLVTag::Context(f::UPLOAD_NEXT), next)?;
            w.bool(&TLVTag::Context(f::STAGED), status.staged.is_some())?;
            w.utf8(&TLVTag::Context(f::STAGED_VERSION), staged_version.as_str())?;
            w.u8(&TLVTag::Context(f::LAST_ERROR), last_error)?;
            w.u16(&TLVTag::Context(f::MAX_CHUNK), MAX_CHUNK as u16)?;
        })
    }
}

impl<F: NorFlash> Handler for OtaCluster<'_, F> {
    fn bump_dataver(&self, _ctx: impl rs_matter::dm::MatchContext) {
        self.dataver.changed();
    }

    fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        let attr = ctx.attr();
        let Some(writer) = reply.with_dataver(self.dataver.get())? else {
            return Ok(());
        };
        if attr.is_system() {
            Self::CLUSTER.read(attr, writer)
        } else {
            Err(ErrorCode::AttributeNotFound.into())
        }
    }

    fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        let data = ctx.data();
        match ctx.cmd().cmd_id {
            cmd::STATUS => self.reply_status(reply),
            cmd::BEGIN => {
                let fields = data.r#struct()?;
                let len = fields.ctx(0)?.u32()?;
                let sha: &[u8; 32] = fields
                    .ctx(1)?
                    .octets()?
                    .try_into()
                    .map_err(|_| ErrorCode::ConstraintError)?;
                let result = self.service.ota.borrow_mut().begin(len, sha);
                let next = self.service.record(result)?;
                reply_struct!(reply.with_command(cmd::BEGIN_RESPONSE)?, |w| {
                    w.u32(&TLVTag::Context(0), next)?;
                    w.u16(&TLVTag::Context(1), MAX_CHUNK as u16)?;
                })
            }
            cmd::WRITE => {
                let fields = data.r#struct()?;
                let offset = fields.ctx(0)?.u32()?;
                let chunk = fields.ctx(1)?.octets()?;
                let result = self.service.ota.borrow_mut().write_chunk(offset, chunk);
                self.service.record(result)
            }
            cmd::FINISH => {
                let result = self.service.ota.borrow_mut().finish();
                let staged = self.service.record(result)?;
                self.service.reboot.notify();
                reply_struct!(reply.with_command(cmd::FINISH_RESPONSE)?, |w| {
                    w.u8(&TLVTag::Context(0), staged.slot.index() as u8)?;
                    w.utf8(&TLVTag::Context(1), staged.version.as_str())?;
                })
            }
            cmd::ABORT => {
                self.service.ota.borrow_mut().abort();
                Ok(())
            }
            _ => Err(ErrorCode::CommandNotFound.into()),
        }
    }
}

impl<F: NorFlash> rs_matter::dm::NonBlockingHandler for OtaCluster<'_, F> {}
