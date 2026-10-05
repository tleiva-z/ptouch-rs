// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>
// SPDX-FileCopyrightText: Dominic Radermacher and the ptouch-print contributors
//
// Portions derived from ptouch-print, licensed GPL-3.0-or-later:
// https://git.familie-radermacher.ch/linux/ptouch-print.git

//! Transport-independent printer state and command lifecycle.

use crate::model::{Dialect, ModelProfile};
use crate::p300bt;
use crate::{
    device::DeviceFlags,
    error::{PtouchError, Result},
    protocol,
    status::{PrinterStatus, STATUS_PACKET_SIZE},
};
use log::{debug, info, warn};
use std::time::{Duration, Instant};

/// Internal byte transport. Native handles need not be Send or Sync.
pub(crate) trait Transport {
    fn send(&self, data: &[u8], timeout: Duration) -> Result<()>;
    fn receive(&self, buf: &mut [u8], timeout: Duration) -> Result<usize>;
    fn close(self) -> Result<()>;
}

/// Default timeout for byte transfers.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(5);

/// A long label fills the printer's receive buffer. Bulk OUT then waits while
/// the printer prints, so a short write timeout aborts the job mid-label.
const PRINT_TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait, after a page that is not the last, for the printer to
/// say it can take the next label. Phase changes arrive as soon as the page
/// command is accepted; silence means this model does not notify.
const NEXT_PAGE_IDLE_TIMEOUT: Duration = Duration::from_secs(3);

/// Short timeout for flushing stale input.
const FLUSH_TIMEOUT: Duration = Duration::from_millis(100);

/// Delay between status read retries.
const STATUS_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Maximum number of status read retries.
const STATUS_MAX_RETRIES: usize = 10;

/// Maximum time to wait without hearing anything from the printer after a
/// print command. The deadline restarts on every status transfer, so a label
/// that takes minutes to feed is bounded by printer silence, not by the total
/// length of the job.
const PRINT_STATUS_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound each USB read so transient silence cannot consume the whole idle
/// deadline in a single transfer.
const PRINT_STATUS_POLL_TIMEOUT: Duration = Duration::from_millis(250);

/// Never hand libusb a zero timeout, which means "wait forever".
const PRINT_STATUS_MIN_POLL_TIMEOUT: Duration = Duration::from_millis(1);

/// Pace the loop after a zero-length bulk transfer so a printer that keeps
/// completing empty reads cannot spin a core until the deadline expires.
const ZERO_LENGTH_TRANSFER_DELAY: Duration = Duration::from_millis(10);

pub(crate) struct PrinterSession<T: Transport> {
    pub(crate) transport: T,
    profile: ModelProfile,
    status_frames: StatusFrameBuffer,
    status: Option<PrinterStatus>,
    tape_width_px: Option<u16>,
    initialized: bool,
}

impl<T: Transport> PrinterSession<T> {
    pub(crate) fn new(transport: T, profile: ModelProfile) -> Self {
        Self {
            transport,
            profile,
            status_frames: StatusFrameBuffer::new(),
            status: None,
            tape_width_px: None,
            initialized: false,
        }
    }
    /// Get the device flags.
    pub fn flags(&self) -> DeviceFlags {
        self.profile.flags
    }

    /// Get the most recently read printer status, if available.
    pub fn status(&self) -> Option<&PrinterStatus> {
        self.status.as_ref()
    }

    /// Get the tape width in pixels, if known.
    pub fn tape_width_px(&self) -> Option<u16> {
        self.tape_width_px
    }

    /// Get the raster transfer width, which may exceed the printable area.
    pub fn raster_width_px(&self) -> u16 {
        self.profile.raster_width_px
    }

    /// Whether the device has been initialized.
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Send raw bytes through the selected transport.
    pub fn send(&self, data: &[u8]) -> Result<()> {
        self.transport.send(data, TRANSFER_TIMEOUT)
    }

    pub fn receive(&self, buf: &mut [u8]) -> Result<usize> {
        self.receive_with_timeout(buf, TRANSFER_TIMEOUT)
    }

    fn receive_with_timeout(&self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        self.transport.receive(buf, timeout)
    }

    fn flush_input(&mut self) {
        // Discard cached prefixes together with their unread tails in the transport.
        self.status_frames.clear();
        let mut buf = [0u8; 64];
        let start = Instant::now();
        loop {
            let timeout = match self.profile.dialect {
                Dialect::Usb => FLUSH_TIMEOUT,
                Dialect::P300Bt => match FLUSH_TIMEOUT.checked_sub(start.elapsed()) {
                    Some(remaining) if !remaining.is_zero() => remaining,
                    _ => break,
                },
            };
            match self.transport.receive(&mut buf, timeout) {
                Ok(n) if n > 0 => debug!("Flushed {} stale bytes", n),
                _ => break,
            }
        }
    }

    /// Initialize the printer.
    ///
    /// Sends the init sequence (100 zeros + ESC @) and queries the status.
    /// Raster start is sent per-job in `print_raster()`.
    pub fn init(&mut self) -> Result<()> {
        // Flush any stale data from previous sessions
        self.flush_input();

        // Send the init command (100 zeros + ESC @)
        match self.profile.dialect {
            Dialect::Usb => self.send(&protocol::cmd_init())?,
            Dialect::P300Bt => self.send(&p300bt::cmd_init())?,
        }

        // Request and read status
        self.get_status()?;

        if self
            .profile
            .flags
            .contains(DeviceFlags::AUTO_STATUS_NOTIFICATION)
        {
            // Enable the phase changes used by the post-print readiness handshake.
            self.send(&protocol::cmd_auto_status_notification(true))?;
        }

        self.initialized = true;
        info!(
            "Device initialized: {}, tape={}mm ({}px)",
            self.profile.name,
            self.status.as_ref().map_or(0, |s| s.media_width),
            self.tape_width_px.unwrap_or(0)
        );

        Ok(())
    }

    /// Query printer status without sending the init command.
    ///
    /// Flushes stale USB data and reads the printer status. Unlike
    /// [`init`](Self::init), this does not send the 100-zero + ESC @
    /// reset sequence, so it will not disturb the printer.
    pub fn query_status(&mut self) -> Result<&PrinterStatus> {
        self.flush_input();
        self.get_status()
    }

    /// Request and read the printer status.
    ///
    /// Sends the status request command and reads the 32-byte response.
    /// Retries up to STATUS_MAX_RETRIES times with STATUS_RETRY_DELAY
    /// between attempts.
    /// Updates internal status and tape width fields.
    pub fn get_status(&mut self) -> Result<&PrinterStatus> {
        if self.profile.dialect == Dialect::P300Bt {
            return self.get_p300bt_status();
        }
        self.send(&protocol::cmd_status_request())?;

        let mut buf = [0u8; STATUS_PACKET_SIZE];
        let mut frames = StatusFrameBuffer::new();
        let mut response = None;

        // Retry loop: sleep then read
        for attempt in 0..STATUS_MAX_RETRIES {
            std::thread::sleep(STATUS_RETRY_DELAY);

            match self.receive_with_timeout(&mut buf, TRANSFER_TIMEOUT) {
                Ok(0) => {
                    debug!("Empty status read (attempt {})", attempt + 1);
                    continue;
                }
                Ok(n) => {
                    frames.push(&buf[..n]);
                    response = frames.pop();
                }
                Err(PtouchError::Timeout) => {
                    debug!("Status read timeout (attempt {})", attempt + 1);
                    continue;
                }
                Err(e) => return Err(e),
            }

            if response.is_some() {
                break;
            }
            debug!(
                "Short status read ({} bytes, attempt {})",
                frames.len(),
                attempt + 1
            );
        }

        let Some(response) = response else {
            // Flush junk data before returning error
            self.flush_input();
            return Err(PtouchError::StatusError(format!(
                "Status packet too short: {} bytes (expected {})",
                frames.len(),
                STATUS_PACKET_SIZE
            )));
        };

        let status = match parse_status_packet(&response, "Invalid status header") {
            Ok(status) => status,
            Err(error) => {
                self.flush_input();
                return Err(error);
            }
        };

        debug!(
            "Status: type={}, media_width={}mm, media_type={}, tape_color={}, text_color={}",
            status.status_type_name(),
            status.media_width,
            status.media_type_name(),
            status.tape_color_name(),
            status.text_color_name()
        );

        if status.has_error() {
            warn!("Printer reports error: {}", status.error_description());
        }

        // Resolve tape width to pixel count for this printer's resolution,
        // clamped to the head width (wide tapes exceed narrow heads).
        self.tape_width_px = self.profile.tape_width_px(status.media_width);
        if self.tape_width_px.is_none() && status.media_width != 0 {
            warn!("Unknown tape width: {} mm", status.media_width);
        }

        self.status = Some(status);

        // The unwrap is safe because we just assigned Some above
        Ok(self.status.as_ref().unwrap())
    }

    /// Print raster image data.
    ///
    /// `lines` is a slice of raster line buffers, each `ceil(max_px/8)` bytes
    /// wide. The printer will print one raster line per entry.
    ///
    /// # Arguments
    /// * `lines` - Raster image data, one byte-slice per line.
    /// * `chain_print` - If true, don't cut the tape (chain mode).
    /// * `precut` - If true AND device supports precut, send precut command.
    /// * `quality` - Print quality mode (device must support non-standard).
    ///
    /// # Errors
    ///
    /// Returns [`PtouchError::NotInitialized`] if [`init`](Self::init) was
    /// not called, or [`PtouchError::UnsupportedQuality`] if a non-standard
    /// quality is requested on a device without quality modes.
    pub fn print_raster(
        &mut self,
        lines: &[Vec<u8>],
        chain_print: bool,
        precut: bool,
        quality: protocol::PrintQuality,
    ) -> Result<()> {
        self.print_pages(&[lines], chain_print, precut, quality, false)
    }

    /// Print several labels as separate pages of one job.
    ///
    /// `cut_between` ends every page with the eject-and-cut command. A form
    /// feed between pages does not cut on printers such as the PT-D600, so the
    /// labels would come out as one strip. Without `cut_between`, only the last
    /// page ejects and the earlier pages stay chained.
    pub fn print_pages(
        &mut self,
        pages: &[&[Vec<u8>]],
        chain_print: bool,
        precut: bool,
        quality: protocol::PrintQuality,
        cut_between: bool,
    ) -> Result<()> {
        if pages.is_empty() {
            return Ok(());
        }
        if !self.initialized {
            return Err(PtouchError::NotInitialized);
        }
        if quality != protocol::PrintQuality::Standard
            && !self.profile.flags.contains(DeviceFlags::LEGACY_HIRES)
        {
            return Err(PtouchError::UnsupportedQuality(
                self.profile.name.to_string(),
            ));
        }

        let separate_cuts = cut_between && pages.len() > 1;
        for (index, lines) in pages.iter().enumerate() {
            let more_pages = index + 1 < pages.len();
            if separate_cuts {
                // 0x1A after every label. The completion read waits until this
                // cut is underway before the next label is sent.
                self.print_one_page(lines, false, precut, quality, false)?;
            } else {
                self.print_one_page(lines, chain_print, precut, quality, more_pages)?;
                if more_pages && self.profile.dialect != Dialect::P300Bt {
                    self.wait_for_next_page()?;
                }
            }
        }
        Ok(())
    }

    fn print_one_page(
        &mut self,
        lines: &[Vec<u8>],
        chain_print: bool,
        precut: bool,
        quality: protocol::PrintQuality,
        more_pages: bool,
    ) -> Result<()> {
        if self.profile.dialect == Dialect::P300Bt {
            let status = self.status.as_ref().ok_or(PtouchError::NotInitialized)?;
            let job = p300bt::build_print_job(lines, status, chain_print || more_pages)?;
            let result = self
                .send_job(job)
                .and_then(|_| self.receive_p300bt_completion());
            if result.is_err() {
                self.initialized = false;
            }
            return result;
        }

        let opts = protocol::JobOptions {
            media_width: self.status.as_ref().map_or(0, |s| s.media_width),
            chain_print,
            precut,
            quality,
            more_pages,
            margin_dots: feed_margin_dots(&self.profile),
        };

        let job = protocol::build_print_job(lines, self.profile.flags, &opts);
        self.send_job(job)?;

        if more_pages {
            return Ok(());
        }
        if self
            .profile
            .flags
            .contains(DeviceFlags::WAIT_FOR_RECEIVE_READY)
        {
            self.wait_until_ready()
        } else {
            self.receive_print_completion()
        }
    }

    /// Feed tape forward and cut.
    ///
    /// Prints a minimal blank strip (a few blank raster lines) then
    /// ejects and cuts. The printer needs actual raster data to engage
    /// the feed mechanism.
    pub fn feed_and_cut(&mut self) -> Result<()> {
        if self.profile.dialect == Dialect::P300Bt {
            return Err(PtouchError::UnsupportedOperation(
                "PT-P300BT has a manual cutter",
            ));
        }
        if !self.initialized {
            return Err(PtouchError::NotInitialized);
        }

        // One blank line makes the printer engage the feed mechanism.
        let lines = vec![protocol::rasterline_blank(self.profile.raster_width_px)];
        let opts = protocol::JobOptions {
            media_width: self.status.as_ref().map_or(0, |s| s.media_width),
            ..protocol::JobOptions::default()
        };

        let job = protocol::build_print_job(&lines, self.profile.flags, &opts);
        self.send_job(job)?;

        if self
            .profile
            .flags
            .contains(DeviceFlags::WAIT_FOR_RECEIVE_READY)
        {
            self.wait_until_ready()?;
        }

        info!("Feed and cut");
        Ok(())
    }

    fn send_job(&self, job: Vec<Vec<u8>>) -> Result<()> {
        for chunk in job {
            self.transport.send(&chunk, PRINT_TRANSFER_TIMEOUT)?;
        }

        Ok(())
    }

    /// Read the phase change that follows a non-final page.
    ///
    /// A model that never notifies must not block the rest of the batch, so
    /// silence is not a failure. An error status still aborts the job.
    fn wait_for_next_page(&mut self) -> Result<()> {
        match receive_print_status_with_timeout(
            |buf, timeout| self.receive_with_timeout(buf, timeout),
            NEXT_PAGE_IDLE_TIMEOUT,
        ) {
            Ok(status) => {
                self.status = Some(status);
                Ok(())
            }
            Err(PtouchError::Timeout) => {
                warn!("Printer did not report readiness for the next label, continuing");
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn wait_until_ready(&mut self) -> Result<()> {
        match receive_print_status(|buf, timeout| self.receive_with_timeout(buf, timeout)) {
            Ok(status) => {
                self.status = Some(status);
                Ok(())
            }
            // The printer stopped talking without announcing the receiving
            // phase. The page itself has most likely printed, so fall back to
            // the best-effort contract instead of failing a finished job.
            Err(PtouchError::Timeout) => {
                warn!("Printer did not report the receiving phase, continuing anyway");
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Preserve the original best-effort completion read for models whose
    /// readiness lifecycle has not been documented or tested.
    fn receive_print_completion(&mut self) -> Result<()> {
        let mut response = [0u8; STATUS_PACKET_SIZE];
        match self.receive(&mut response) {
            Ok(n) if n >= STATUS_PACKET_SIZE => {
                if let Some(status) = PrinterStatus::from_bytes(&response) {
                    if status.has_error() {
                        return Err(PtouchError::StatusError(status.error_description()));
                    }
                    debug!("Print completed: status_type={}", status.status_type_name());
                    self.status = Some(status);
                }
            }
            Ok(n) => {
                debug!("Short status response after print: {} bytes", n);
            }
            Err(PtouchError::Timeout) => {
                debug!("Timeout waiting for print completion status");
            }
            Err(error) => return Err(error),
        }

        Ok(())
    }

    fn get_p300bt_status(&mut self) -> Result<&PrinterStatus> {
        self.send(&protocol::cmd_status_request())?;
        let transport = &self.transport;
        let status = read_p300bt_status(
            &mut self.status_frames,
            |buf, timeout| transport.receive(buf, timeout),
            Duration::from_secs(10),
            false,
        )?;
        self.tape_width_px = self.profile.tape_width_px(status.media_width);
        self.status = Some(status);
        Ok(self.status.as_ref().unwrap())
    }

    fn receive_p300bt_completion(&mut self) -> Result<()> {
        // A fixed total deadline; phase changes alone never complete the job.
        let transport = &self.transport;
        self.status = Some(read_p300bt_status(
            &mut self.status_frames,
            |buf, timeout| transport.receive(buf, timeout),
            Duration::from_secs(60),
            true,
        )?);
        Ok(())
    }

    pub(crate) fn close(self) -> Result<()> {
        self.transport.close()?;
        info!("Device closed: {}", self.profile.name);
        Ok(())
    }
}

fn read_p300bt_status<F>(
    frames: &mut StatusFrameBuffer,
    mut receive: F,
    timeout: Duration,
    wait_for_completion: bool,
) -> Result<PrinterStatus>
where
    F: FnMut(&mut [u8], Duration) -> Result<usize>,
{
    let start = Instant::now();
    loop {
        while let Some(packet) = frames.pop() {
            let status = parse_status_packet(&packet, "Invalid PT-P300BT status header")?;
            if status.brother_code != 0x42
                || status.series_code != 0x30
                || status.model_code != 0x72
            {
                return Err(PtouchError::StatusError(
                    "Unexpected PT-P300BT status identity".into(),
                ));
            }
            // Error and power-off checks are shared with the USB readiness loop,
            // but P300BT completion does not require a later receiving phase.
            print_status_is_ready(&status)?;
            if (!wait_for_completion && status.status_type == 0)
                || (wait_for_completion && status.status_type == 1)
            {
                return Ok(status);
            }
        }
        let remaining = timeout
            .checked_sub(start.elapsed())
            .filter(|d| !d.is_zero())
            .ok_or(PtouchError::Timeout)?;
        // A larger receive buffer deliberately permits coalesced notifications.
        let mut bytes = [0u8; STATUS_PACKET_SIZE * 4];
        match receive(&mut bytes, remaining.min(PRINT_STATUS_POLL_TIMEOUT)) {
            Ok(n) if n > bytes.len() => {
                return Err(PtouchError::StatusError(
                    "Transport returned more bytes than the receive buffer".into(),
                ));
            }
            Ok(0) => std::thread::sleep(ZERO_LENGTH_TRANSFER_DELAY),
            Ok(n) => frames.push(&bytes[..n]),
            Err(PtouchError::Timeout) => {}
            Err(error) => return Err(error),
        }
    }
}

/// 2 mm of feed, except on the D460BT init sequence which already owns `ESC i d`.
fn feed_margin_dots(profile: &ModelProfile) -> Option<u16> {
    if profile.flags.contains(DeviceFlags::D460BT_MAGIC) {
        None
    } else {
        Some(protocol::minimum_feed_margin_dots(profile.dpi))
    }
}

/// Receive the printer's automatic status after a print command.
fn receive_print_status<F>(mut receive: F) -> Result<PrinterStatus>
where
    F: FnMut(&mut [u8], Duration) -> Result<usize>,
{
    receive_print_status_with_timeout(&mut receive, PRINT_STATUS_IDLE_TIMEOUT)
}

fn receive_print_status_with_timeout<F>(
    mut receive: F,
    idle_timeout: Duration,
) -> Result<PrinterStatus>
where
    F: FnMut(&mut [u8], Duration) -> Result<usize>,
{
    let mut last_transfer = Instant::now();
    let mut frames = StatusFrameBuffer::new();

    loop {
        let Some(remaining) = idle_timeout.checked_sub(last_transfer.elapsed()) else {
            return Err(PtouchError::Timeout);
        };

        let mut transfer = [0u8; STATUS_PACKET_SIZE];
        let read_timeout = remaining
            .min(PRINT_STATUS_POLL_TIMEOUT)
            .max(PRINT_STATUS_MIN_POLL_TIMEOUT);
        let read = match receive(&mut transfer, read_timeout) {
            Ok(read) => read,
            Err(PtouchError::Timeout) => {
                debug!("No print status available yet");
                continue;
            }
            Err(error) => return Err(error),
        };

        if read > transfer.len() {
            return Err(PtouchError::StatusError(format!(
                "USB read reported {} bytes for a {}-byte buffer",
                read,
                transfer.len()
            )));
        }

        // A successful zero-byte bulk transfer is USB framing, not a Brother
        // status packet. Keep waiting within the overall deadline.
        if read == 0 {
            debug!("Ignoring zero-length USB transfer after print");
            std::thread::sleep(ZERO_LENGTH_TRANSFER_DELAY);
            continue;
        }

        // Real bytes mean the job is still alive. Restart the idle deadline so
        // a label that feeds for a long time is not cut short mid-print.
        last_transfer = Instant::now();

        frames.push(&transfer[..read]);
        if frames.len() < STATUS_PACKET_SIZE {
            debug!(
                "Accumulated {} of {} print-status bytes",
                frames.len(),
                STATUS_PACKET_SIZE
            );
            continue;
        }

        let Some(response) = frames.pop() else {
            continue;
        };
        let status = parse_status_packet(&response, "Invalid status header after print")?;

        debug!(
            "Print status: type={}, phase_type={:#04x}, phase={:#04x}{:02x}",
            status.status_type_name(),
            status.phase_type,
            status.phase_number_hi,
            status.phase_number_lo
        );

        if print_status_is_ready(&status)? {
            debug!("Printer is ready to receive the next page");
            return Ok(status);
        }
    }
}

struct StatusFrameBuffer {
    pending: Vec<u8>,
}

impl StatusFrameBuffer {
    fn new() -> Self {
        Self {
            pending: Vec::with_capacity(STATUS_PACKET_SIZE * 2),
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
    }

    fn clear(&mut self) {
        self.pending.clear();
    }

    fn len(&self) -> usize {
        self.pending.len()
    }

    fn pop(&mut self) -> Option<[u8; STATUS_PACKET_SIZE]> {
        if self.pending.len() < STATUS_PACKET_SIZE {
            return None;
        }

        let mut frame = [0u8; STATUS_PACKET_SIZE];
        frame.copy_from_slice(&self.pending[..STATUS_PACKET_SIZE]);
        self.pending.drain(..STATUS_PACKET_SIZE);
        Some(frame)
    }
}

fn parse_status_packet(
    response: &[u8; STATUS_PACKET_SIZE],
    invalid_header_message: &str,
) -> Result<PrinterStatus> {
    let status = PrinterStatus::from_bytes(response)
        .ok_or_else(|| PtouchError::StatusError("Failed to parse status packet".to_string()))?;

    if status.print_head_mark != 0x80 || status.size != 0x20 {
        return Err(PtouchError::StatusError(format!(
            "{}: mark={:#04x} size={:#04x}",
            invalid_header_message, status.print_head_mark, status.size
        )));
    }

    Ok(status)
}

fn print_status_is_ready(status: &PrinterStatus) -> Result<bool> {
    if status.has_error() || status.status_type == 0x02 {
        let description = if status.has_error() {
            status.error_description()
        } else {
            "Printer reported an unspecified error".to_string()
        };
        return Err(PtouchError::StatusError(description));
    }

    if status.status_type == 0x04 {
        return Err(PtouchError::StatusError("Printer turned off".to_string()));
    }

    Ok(status.is_waiting_to_receive())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    fn status_packet(status_type: u8, phase_type: u8) -> [u8; STATUS_PACKET_SIZE] {
        let mut packet = [0u8; STATUS_PACKET_SIZE];
        packet[0] = 0x80;
        packet[1] = 0x20;
        packet[18] = status_type;
        packet[19] = phase_type;
        packet
    }

    struct ScriptedTransport {
        writes: std::cell::RefCell<Vec<Vec<u8>>>,
        fail_on_write: std::cell::Cell<Option<usize>>,
        reads: std::cell::RefCell<VecDeque<Vec<u8>>>,
        // Prove the session works with a transport that cannot cross threads.
        _owner: std::rc::Rc<()>,
    }
    impl ScriptedTransport {
        fn new() -> Self {
            Self {
                writes: Default::default(),
                fail_on_write: Default::default(),
                reads: Default::default(),
                _owner: std::rc::Rc::new(()),
            }
        }
    }
    impl Transport for ScriptedTransport {
        fn send(&self, data: &[u8], _timeout: Duration) -> Result<()> {
            self.writes.borrow_mut().push(data.to_vec());
            if self.fail_on_write.get() == Some(self.writes.borrow().len()) {
                return Err(PtouchError::SendFailed("Injected transfer failure".into()));
            }
            if data == [0x1b, 0x69, 0x53] {
                let mut packet = status_packet(0, 0);
                packet[2] = 0x42;
                packet[3] = 0x30;
                packet[4] = 0x72;
                packet[10] = 12;
                self.reads
                    .borrow_mut()
                    .extend([packet[..11].to_vec(), packet[11..].to_vec()]);
            }
            Ok(())
        }
        fn receive(&self, buf: &mut [u8], _timeout: Duration) -> Result<usize> {
            let bytes = self
                .reads
                .borrow_mut()
                .pop_front()
                .ok_or(PtouchError::Timeout)?;
            buf[..bytes.len()].copy_from_slice(&bytes);
            Ok(bytes.len())
        }
        fn close(self) -> Result<()> {
            Ok(())
        }
    }
    fn usb_session(flags: DeviceFlags) -> PrinterSession<ScriptedTransport> {
        PrinterSession::new(
            ScriptedTransport::new(),
            ModelProfile {
                dialect: Dialect::Usb,
                name: "Test USB",
                raster_width_px: 128,
                dpi: 180,
                flags,
            },
        )
    }
    fn p300bt_packet(kind: u8, phase: u8) -> [u8; STATUS_PACKET_SIZE] {
        let mut packet = status_packet(kind, phase);
        packet[2] = 0x42;
        packet[3] = 0x30;
        packet[4] = 0x72;
        packet[10] = 12;
        packet
    }
    #[test]
    fn p300bt_profile_keeps_printable_and_raster_widths_separate() {
        let mut session = PrinterSession::new(ScriptedTransport::new(), ModelProfile::P300BT);
        session.init().unwrap();
        assert_eq!(session.raster_width_px(), 128);
        assert_eq!(session.tape_width_px(), Some(64));
        assert_eq!(ModelProfile::P300BT.tape_width_px(9), None);
        let mut reset = vec![0; 64];
        reset.extend([0x1b, 0x40, 0x1b, 0x69, 0x61, 1]);
        assert_eq!(
            *session.transport.writes.borrow(),
            vec![reset, vec![0x1b, 0x69, 0x53]]
        );
        assert!(matches!(
            session.feed_and_cut(),
            Err(PtouchError::UnsupportedOperation(_))
        ));
    }
    #[test]
    fn p300bt_failed_job_stops_sending_and_cannot_be_replayed() {
        let mut session = PrinterSession::new(ScriptedTransport::new(), ModelProfile::P300BT);
        session.init().unwrap();
        session.transport.writes.borrow_mut().clear();
        session.transport.fail_on_write.set(Some(2));
        assert!(matches!(
            session.print_raster(
                &[vec![0; 16]],
                false,
                false,
                protocol::PrintQuality::Standard
            ),
            Err(PtouchError::SendFailed(_))
        ));
        assert_eq!(session.transport.writes.borrow().len(), 2);
        assert!(!session.is_initialized());
        assert!(matches!(
            session.print_raster(
                &[vec![0; 16]],
                false,
                false,
                protocol::PrintQuality::Standard
            ),
            Err(PtouchError::NotInitialized)
        ));
        assert_eq!(session.transport.writes.borrow().len(), 2);
    }

    #[test]
    fn p300bt_completion_handles_fragmented_and_coalesced_notifications() {
        let printing = p300bt_packet(6, 1);
        let done = p300bt_packet(1, 1);
        let mut transfers = VecDeque::from([
            printing[..7].to_vec(),
            [printing[7..].as_ref(), done.as_ref()].concat(),
        ]);
        let result = read_p300bt_status(
            &mut StatusFrameBuffer::new(),
            |buf, _| {
                let bytes = transfers.pop_front().ok_or(PtouchError::Timeout)?;
                buf[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            },
            Duration::from_secs(1),
            true,
        )
        .unwrap();
        assert_eq!(result.status_type, 1);
        assert_eq!(result.phase_type, 1); // No later receiving phase is required.
        assert!(transfers.is_empty());
    }
    #[test]
    fn p300bt_errors_override_completion_and_disconnects_propagate() {
        let mut done = p300bt_packet(1, 1);
        done[8] = 8;
        let result = read_p300bt_status(
            &mut StatusFrameBuffer::new(),
            |buf, _| {
                buf[..32].copy_from_slice(&done);
                Ok(32)
            },
            Duration::from_secs(1),
            true,
        );
        assert!(matches!(result, Err(PtouchError::StatusError(_))));
        let off = p300bt_packet(4, 0);
        assert!(
            read_p300bt_status(
                &mut StatusFrameBuffer::new(),
                |buf, _| {
                    buf[..32].copy_from_slice(&off);
                    Ok(32)
                },
                Duration::from_secs(1),
                true
            )
            .is_err()
        );
        assert!(matches!(
            read_p300bt_status(
                &mut StatusFrameBuffer::new(),
                |_, _| Err(PtouchError::Bluetooth("Disconnected".into())),
                Duration::from_secs(1),
                true
            ),
            Err(PtouchError::Bluetooth(_))
        ));
    }
    #[test]
    fn p300bt_query_skips_stale_notifications_and_checks_identity() {
        let mut transfers = VecDeque::from([p300bt_packet(1, 1), p300bt_packet(0, 0)]);
        let status = read_p300bt_status(
            &mut StatusFrameBuffer::new(),
            |buf, _| {
                let bytes = transfers.pop_front().ok_or(PtouchError::Timeout)?;
                buf[..32].copy_from_slice(&bytes);
                Ok(32)
            },
            Duration::from_secs(1),
            false,
        )
        .unwrap();
        assert_eq!(status.status_type, 0);
        let mut wrong = p300bt_packet(0, 0);
        wrong[4] = 0;
        assert!(
            read_p300bt_status(
                &mut StatusFrameBuffer::new(),
                |buf, _| {
                    buf[..32].copy_from_slice(&wrong);
                    Ok(32)
                },
                Duration::from_secs(1),
                false
            )
            .is_err()
        );
    }
    #[test]
    fn p300bt_phase_changes_do_not_extend_the_completion_deadline() {
        let packet = p300bt_packet(6, 1);
        let result = read_p300bt_status(
            &mut StatusFrameBuffer::new(),
            |buf, _| {
                buf[..32].copy_from_slice(&packet);
                Ok(32)
            },
            Duration::from_millis(2),
            true,
        );
        assert!(matches!(result, Err(PtouchError::Timeout)));
        assert!(matches!(
            read_p300bt_status(
                &mut StatusFrameBuffer::new(),
                |_, _| Ok(0),
                Duration::ZERO,
                true
            ),
            Err(PtouchError::Timeout)
        ));
    }

    #[test]
    fn usb_session_keeps_initialization_sequence_and_tape_resolution() {
        let mut session = usb_session(DeviceFlags::AUTO_STATUS_NOTIFICATION);
        session.init().unwrap();
        let mut reset = vec![0; 100];
        reset.extend([0x1b, 0x40]);
        assert_eq!(
            *session.transport.writes.borrow(),
            vec![reset, vec![0x1b, 0x69, 0x53], vec![0x1b, 0x69, 0x21, 0]]
        );
        assert_eq!(session.tape_width_px(), Some(76));
        assert!(session.is_initialized());
    }
    #[test]
    fn usb_session_keeps_plain_job_and_best_effort_completion() {
        let mut session = usb_session(DeviceFlags::NONE);
        session.init().unwrap();
        session.transport.writes.borrow_mut().clear();
        // The undocumented USB-model path still permits a completion timeout.
        session
            .print_raster(
                &[vec![0; 16], vec![0x80; 16]],
                false,
                false,
                protocol::PrintQuality::Standard,
            )
            .unwrap();
        let mut raster = vec![0x47, 16, 0];
        raster.extend([0x80; 16]);
        assert_eq!(
            *session.transport.writes.borrow(),
            vec![
                vec![0x1b, 0x69, 0x52, 1],
                vec![0x1b, 0x69, 0x64, 0x0e, 0x00],
                vec![0x5a],
                raster,
                vec![0x1a]
            ]
        );
    }

    #[test]
    fn usb_pages_form_feed_between_labels_and_eject_the_last() {
        let mut session = usb_session(DeviceFlags::NONE);
        session.init().unwrap();
        session.transport.writes.borrow_mut().clear();
        let ready = status_packet(0x06, 0x00);
        session
            .transport
            .reads
            .borrow_mut()
            .push_back(ready.to_vec());
        let first = vec![vec![0x11; 16]];
        let second = vec![vec![0x22; 16]];
        session
            .print_pages(
                &[&first, &second],
                false,
                false,
                protocol::PrintQuality::Standard,
                false,
            )
            .unwrap();
        let writes = session.transport.writes.borrow();
        let form_feed = writes.iter().position(|chunk| chunk == &vec![0x0c]);
        let eject = writes.iter().rposition(|chunk| chunk == &vec![0x1a]);
        assert!(form_feed.is_some());
        assert!(eject.is_some());
        assert!(form_feed.unwrap() < eject.unwrap());
        assert_eq!(
            writes
                .iter()
                .filter(|chunk| chunk.as_slice() == [0x1b, 0x69, 0x64, 0x0e, 0x00])
                .count(),
            2
        );
    }

    #[test]
    fn usb_pages_cut_between_labels_ejects_every_page() {
        let mut session = usb_session(DeviceFlags::NONE);
        session.init().unwrap();
        session.transport.writes.borrow_mut().clear();
        session.transport.reads.borrow_mut().clear();
        let first = vec![vec![0x11; 16]];
        let second = vec![vec![0x22; 16]];
        session
            .print_pages(
                &[&first, &second],
                true,
                false,
                protocol::PrintQuality::Standard,
                true,
            )
            .unwrap();
        let writes = session.transport.writes.borrow();
        let ejects = writes
            .iter()
            .filter(|chunk| chunk.as_slice() == [0x1a])
            .count();
        assert_eq!(ejects, 2);
        assert!(writes.iter().all(|chunk| chunk.as_slice() != [0x0c]));
    }

    #[test]
    fn usb_session_query_does_not_reset_or_enable_notifications() {
        let mut session = usb_session(DeviceFlags::AUTO_STATUS_NOTIFICATION);
        session.query_status().unwrap();
        assert_eq!(
            *session.transport.writes.borrow(),
            vec![vec![0x1b, 0x69, 0x53]]
        );
        assert!(!session.is_initialized());
    }

    #[test]
    fn print_status_waits_until_reception_is_possible() {
        let mut packets = VecDeque::from([
            status_packet(0x06, 0x01),
            status_packet(0x01, 0x00),
            status_packet(0x06, 0x00),
        ]);

        let status = receive_print_status(|buf, _timeout| {
            let packet = packets.pop_front().ok_or(PtouchError::Timeout)?;
            buf.copy_from_slice(&packet);
            Ok(packet.len())
        })
        .unwrap();

        assert_eq!(status.status_type, 0x06);
        assert_eq!(status.phase_type, 0x00);
        assert!(packets.is_empty());
    }

    #[test]
    fn print_status_does_not_accept_printing_completed_as_ready() {
        let mut packets = VecDeque::from([status_packet(0x01, 0x00)]);

        let result = receive_print_status_with_timeout(
            |buf, _timeout| {
                let packet = packets.pop_front().ok_or(PtouchError::Timeout)?;
                buf.copy_from_slice(&packet);
                Ok(packet.len())
            },
            Duration::from_millis(1),
        );

        assert!(matches!(result, Err(PtouchError::Timeout)));
    }

    #[test]
    fn print_status_propagates_printer_errors() {
        let mut packet = status_packet(0x02, 0x00);
        packet[8] = 0x04;

        let result = receive_print_status(|buf, _timeout| {
            buf.copy_from_slice(&packet);
            Ok(packet.len())
        });

        assert!(matches!(
            result,
            Err(PtouchError::StatusError(message)) if message == "Cutter jam"
        ));
    }

    #[test]
    fn print_status_propagates_power_off() {
        let packet = status_packet(0x04, 0x00);

        let result = receive_print_status(|buf, _timeout| {
            buf.copy_from_slice(&packet);
            Ok(packet.len())
        });

        assert!(matches!(
            result,
            Err(PtouchError::StatusError(message)) if message == "Printer turned off"
        ));
    }

    #[test]
    fn print_status_times_out_after_incomplete_packet() {
        let mut transfers = VecDeque::from([Ok(vec![0u8; 12]), Err(PtouchError::Timeout)]);

        let result = receive_print_status_with_timeout(
            |buf, _timeout| match transfers.pop_front().unwrap_or(Err(PtouchError::Timeout)) {
                Ok(transfer) => {
                    buf[..transfer.len()].copy_from_slice(&transfer);
                    Ok(transfer.len())
                }
                Err(error) => Err(error),
            },
            Duration::from_millis(1),
        );

        assert!(matches!(result, Err(PtouchError::Timeout)));
    }

    #[test]
    fn print_status_ignores_zero_length_usb_transfers() {
        let mut transfers = VecDeque::from([
            Vec::new(),
            status_packet(0x06, 0x01).to_vec(),
            status_packet(0x01, 0x00).to_vec(),
            status_packet(0x06, 0x00).to_vec(),
        ]);

        let status = receive_print_status(|buf, _timeout| {
            let transfer = transfers.pop_front().ok_or(PtouchError::Timeout)?;
            buf[..transfer.len()].copy_from_slice(&transfer);
            Ok(transfer.len())
        })
        .unwrap();

        assert!(status.is_waiting_to_receive());
        assert!(transfers.is_empty());
    }

    #[test]
    fn print_status_accumulates_fragmented_packets() {
        let ready = status_packet(0x06, 0x00);
        let mut transfers = VecDeque::from([ready[..11].to_vec(), ready[11..].to_vec()]);

        let status = receive_print_status(|buf, _timeout| {
            let transfer = transfers.pop_front().ok_or(PtouchError::Timeout)?;
            buf[..transfer.len()].copy_from_slice(&transfer);
            Ok(transfer.len())
        })
        .unwrap();

        assert!(status.is_waiting_to_receive());
        assert!(transfers.is_empty());
    }

    #[test]
    fn status_frame_buffer_preserves_partial_next_packet() {
        let first = status_packet(0x01, 0x00);
        let second = status_packet(0x06, 0x00);
        let mut frames = StatusFrameBuffer::new();

        frames.push(&first[..9]);
        frames.push(&[first[9..].as_ref(), second[..7].as_ref()].concat());

        assert_eq!(frames.pop(), Some(first));
        assert_eq!(frames.len(), 7);

        frames.push(&second[7..]);
        assert_eq!(frames.pop(), Some(second));
        assert_eq!(frames.len(), 0);
    }

    #[test]
    fn print_status_tolerates_transient_usb_timeouts() {
        let ready = status_packet(0x06, 0x00);
        let mut transfers = VecDeque::from([
            Err(PtouchError::Timeout),
            Err(PtouchError::Timeout),
            Ok(ready.to_vec()),
        ]);

        let status = receive_print_status(|buf, _timeout| {
            match transfers.pop_front().ok_or(PtouchError::Timeout)? {
                Ok(transfer) => {
                    buf[..transfer.len()].copy_from_slice(&transfer);
                    Ok(transfer.len())
                }
                Err(error) => Err(error),
            }
        })
        .unwrap();

        assert!(status.is_waiting_to_receive());
        assert!(transfers.is_empty());
    }

    #[test]
    fn consecutive_pages_each_wait_for_their_receiving_phase() {
        let mut transfers = VecDeque::from([
            status_packet(0x06, 0x01),
            status_packet(0x01, 0x00),
            status_packet(0x06, 0x00),
            status_packet(0x06, 0x01),
            status_packet(0x01, 0x00),
            status_packet(0x06, 0x00),
        ]);
        let mut receive = |buf: &mut [u8], _timeout: Duration| {
            let packet = transfers.pop_front().ok_or(PtouchError::Timeout)?;
            buf.copy_from_slice(&packet);
            Ok(packet.len())
        };

        let first = receive_print_status(&mut receive).unwrap();
        let second = receive_print_status(&mut receive).unwrap();

        assert!(first.is_waiting_to_receive());
        assert!(second.is_waiting_to_receive());
        assert!(transfers.is_empty());
    }

    #[test]
    fn print_status_has_an_overall_deadline() {
        let result = receive_print_status_with_timeout(|_, _| Ok(0), Duration::ZERO);

        assert!(matches!(result, Err(PtouchError::Timeout)));
    }

    #[test]
    fn print_status_deadline_restarts_on_every_transfer() {
        let mut packets = VecDeque::from([
            status_packet(0x06, 0x01),
            status_packet(0x06, 0x01),
            status_packet(0x01, 0x00),
            status_packet(0x06, 0x00),
        ]);

        // Every gap stays inside the idle deadline while the total elapsed
        // time runs past it, which is what a long label looks like.
        let status = receive_print_status_with_timeout(
            |buf, _timeout| {
                std::thread::sleep(Duration::from_millis(100));
                let packet = packets.pop_front().ok_or(PtouchError::Timeout)?;
                buf.copy_from_slice(&packet);
                Ok(packet.len())
            },
            Duration::from_millis(300),
        )
        .unwrap();

        assert!(status.is_waiting_to_receive());
        assert!(packets.is_empty());
    }

    #[test]
    fn print_status_paces_zero_length_transfers() {
        let mut transfers = 0usize;

        let result = receive_print_status_with_timeout(
            |_, _| {
                transfers += 1;
                Ok(0)
            },
            Duration::from_millis(50),
        );

        assert!(matches!(result, Err(PtouchError::Timeout)));
        assert!(
            transfers <= 50,
            "zero-length transfers were not paced ({} reads)",
            transfers
        );
    }

    #[test]
    fn p300bt_status_stream_preserves_error_notifications_across_calls() {
        fn packet(kind: u8, error: u8) -> Vec<u8> {
            let mut bytes = p300bt_packet(kind, 0).to_vec();
            bytes[8] = error;
            bytes
        }

        for limit in [7, 32, 64] {
            let mut stream: VecDeque<u8> =
                [packet(0, 0), packet(2, 8), packet(1, 0)].concat().into();
            let mut receive = |buf: &mut [u8], _: Duration| {
                if stream.is_empty() {
                    return Err(PtouchError::Timeout);
                }
                let count = stream.len().min(buf.len()).min(limit);
                for byte in &mut buf[..count] {
                    *byte = stream.pop_front().unwrap();
                }
                Ok(count)
            };
            let mut frames = StatusFrameBuffer::new();

            let result =
                read_p300bt_status(&mut frames, &mut receive, Duration::from_secs(1), false)
                    .and_then(|_| {
                        read_p300bt_status(&mut frames, &mut receive, Duration::from_secs(1), true)
                    })
                    .map(|_| ());

            assert!(
                matches!(result, Err(PtouchError::StatusError(ref message)) if message == "Weak battery"),
                "lost error at read limit {limit}: {result:?}"
            );
        }
    }

    fn p300bt_session_after_print(notification_prefix: usize) -> PrinterSession<ScriptedTransport> {
        let mut session = PrinterSession::new(ScriptedTransport::new(), ModelProfile::P300BT);
        session.init().unwrap();

        let completed = p300bt_packet(1, 1);
        let receiving = p300bt_packet(6, 0);
        // A read can include completion and the start of the next notification.
        // Its remaining bytes stay in the transport until the next operation.
        session
            .transport
            .reads
            .borrow_mut()
            .push_back([completed.as_ref(), &receiving[..notification_prefix]].concat());
        if notification_prefix < receiving.len() {
            session
                .transport
                .reads
                .borrow_mut()
                .push_back(receiving[notification_prefix..].to_vec());
        }
        session
            .print_raster(
                &[vec![0; 16]],
                false,
                false,
                protocol::PrintQuality::Standard,
            )
            .unwrap();
        session
    }

    #[test]
    fn p300bt_query_after_print_flushes_partial_notifications() {
        for prefix in 0..=STATUS_PACKET_SIZE {
            let mut session = p300bt_session_after_print(prefix);
            let status = session.query_status().unwrap_or_else(|error| {
                panic!("query failed with {prefix} buffered bytes: {error}")
            });
            assert_eq!(status.status_type, 0);
            assert_eq!(status.media_width, 12);
            assert!(!status.has_error());
            assert_eq!(session.tape_width_px(), Some(64));
        }
    }

    #[test]
    fn p300bt_init_after_print_flushes_partial_notifications() {
        for prefix in 0..=STATUS_PACKET_SIZE {
            let mut session = p300bt_session_after_print(prefix);
            session.init().unwrap_or_else(|error| {
                panic!("init failed with {prefix} buffered bytes: {error}")
            });
            let status = session.status().unwrap();
            assert_eq!(status.status_type, 0);
            assert_eq!(status.media_width, 12);
            assert!(!status.has_error());
            assert_eq!(session.tape_width_px(), Some(64));
            assert!(session.is_initialized());
        }
    }
}
