// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>
// SPDX-FileCopyrightText: Dominic Radermacher and the ptouch-print contributors
//
// Portions derived from ptouch-print, licensed GPL-3.0-or-later:
// https://git.familie-radermacher.ch/linux/ptouch-print.git

//! USB discovery and backwards-compatible P-Touch device API.

use crate::session::{PrinterSession, Transport};
use crate::{
    device::{self, BROTHER_VENDOR_ID, DeviceFlags, DeviceInfo},
    error::{PtouchError, Result},
    protocol,
    status::PrinterStatus,
};
use log::{debug, info, warn};
use rusb::{Context, DeviceHandle, UsbContext};
use std::time::Duration;

const USB_INTERFACE: u8 = 0;

struct UsbTransport {
    handle: DeviceHandle<Context>,
    ep_out: u8,
    ep_in: u8,
}

impl Transport for UsbTransport {
    fn send(&self, data: &[u8], timeout: Duration) -> Result<()> {
        let written = self
            .handle
            .write_bulk(self.ep_out, data, timeout)
            .map_err(|e| {
                if e == rusb::Error::Timeout {
                    PtouchError::Timeout
                } else {
                    PtouchError::UsbError(e)
                }
            })?;

        if written != data.len() {
            return Err(PtouchError::SendFailed(format!(
                "Expected to write {} bytes, wrote {}",
                data.len(),
                written
            )));
        }

        Ok(())
    }

    fn receive(&self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        let read = self
            .handle
            .read_bulk(self.ep_in, buf, timeout)
            .map_err(|e| {
                if e == rusb::Error::Timeout {
                    PtouchError::Timeout
                } else {
                    PtouchError::UsbError(e)
                }
            })?;

        Ok(read)
    }

    fn close(self) -> Result<()> {
        self.handle.release_interface(USB_INTERFACE)?;
        Ok(())
    }
}

/// A connection to a Brother P-Touch USB printer.
///
/// This facade keeps the USB constructors and method signatures unchanged.
pub struct PtouchDevice {
    session: PrinterSession<UsbTransport>,
    dev_info: DeviceInfo,
}

impl PtouchDevice {
    /// Open a P-Touch printer by USB vendor/product ID.
    ///
    /// Scans the USB bus for a device matching the given VID/PID, looks it up
    /// in the supported device table, claims the USB interface, and returns
    /// a [`PtouchDevice`] ready for initialization.
    ///
    /// # Errors
    ///
    /// Returns [`PtouchError::DeviceNotFound`] if no matching USB device is
    /// found or the device is not in the supported table. Returns
    /// [`PtouchError::PLiteMode`] if the device is in PLite mode. Returns
    /// [`PtouchError::UnsupportedRaster`] if the device does not support
    /// raster printing.
    pub fn open(vid: u16, pid: u16) -> Result<Self> {
        let dev_info = device::find_device(vid, pid)
            .ok_or(PtouchError::DeviceNotFound)?
            .clone();

        if dev_info.flags.contains(DeviceFlags::PLITE) {
            return Err(PtouchError::PLiteMode(dev_info.name.to_string()));
        }

        if dev_info.flags.contains(DeviceFlags::UNSUP_RASTER) {
            return Err(PtouchError::UnsupportedRaster(dev_info.name.to_string()));
        }

        info!(
            "Opening device: {} (VID={:#06x}, PID={:#06x})",
            dev_info.name, vid, pid
        );

        let context = Context::new()?;
        let handle = context
            .open_device_with_vid_pid(vid, pid)
            .ok_or(PtouchError::DeviceNotFound)?;

        // Detach kernel driver if active (non-fatal)
        if handle.kernel_driver_active(USB_INTERFACE).unwrap_or(false) {
            debug!("Detaching kernel driver from interface {}", USB_INTERFACE);
            if let Err(e) = handle.detach_kernel_driver(USB_INTERFACE) {
                warn!("Failed to detach kernel driver: {} (continuing)", e);
            }
        }

        handle.claim_interface(USB_INTERFACE)?;

        // Find the bulk endpoints
        let (ep_out, ep_in) = find_bulk_endpoints(&handle)?;
        debug!("Endpoints: OUT={:#04x}, IN={:#04x}", ep_out, ep_in);

        Ok(PtouchDevice {
            session: PrinterSession::new(
                UsbTransport {
                    handle,
                    ep_out,
                    ep_in,
                },
                (&dev_info).into(),
            ),
            dev_info,
        })
    }

    /// Open the first Brother P-Touch printer found on the USB bus.
    ///
    /// Scans all USB devices, looking for any with the Brother vendor ID
    /// that matches an entry in the supported device table.
    pub fn open_first() -> Result<Self> {
        let context = Context::new()?;
        let devices = context.devices()?;

        for usb_dev in devices.iter() {
            let desc = match usb_dev.device_descriptor() {
                Ok(d) => d,
                Err(_) => continue,
            };

            if desc.vendor_id() != BROTHER_VENDOR_ID {
                continue;
            }

            if let Some(dev_info) = device::find_device(desc.vendor_id(), desc.product_id()) {
                if dev_info.flags.contains(DeviceFlags::PLITE)
                    || dev_info.flags.contains(DeviceFlags::UNSUP_RASTER)
                {
                    continue;
                }

                return Self::open(desc.vendor_id(), desc.product_id());
            }
        }

        Err(PtouchError::DeviceNotFound)
    }

    /// Get a reference to the USB device info.
    pub fn device_info(&self) -> &DeviceInfo {
        &self.dev_info
    }
    /// Get the device flags.
    pub fn flags(&self) -> DeviceFlags {
        self.session.flags()
    }
    /// Get the most recently read printer status.
    pub fn status(&self) -> Option<&PrinterStatus> {
        self.session.status()
    }
    /// Get the tape width in pixels, if known.
    pub fn tape_width_px(&self) -> Option<u16> {
        self.session.tape_width_px()
    }
    /// Get the maximum printable pixels for this device.
    pub fn max_px(&self) -> u16 {
        self.session.raster_width_px()
    }
    /// Whether the printer has been initialized.
    pub fn is_initialized(&self) -> bool {
        self.session.is_initialized()
    }
    /// Send raw bytes through the USB OUT endpoint.
    pub fn send(&self, data: &[u8]) -> Result<()> {
        self.session.send(data)
    }
    /// Receive raw bytes from the USB IN endpoint.
    pub fn receive(&self, buf: &mut [u8]) -> Result<usize> {
        self.session.receive(buf)
    }
    /// Initialize the printer and query status.
    pub fn init(&mut self) -> Result<()> {
        self.session.init()
    }
    /// Query status without resetting the printer.
    pub fn query_status(&mut self) -> Result<&PrinterStatus> {
        self.session.query_status()
    }
    /// Request and read the printer status.
    pub fn get_status(&mut self) -> Result<&PrinterStatus> {
        self.session.get_status()
    }
    /// Print raster lines with the existing USB job options.
    pub fn print_raster(
        &mut self,
        lines: &[Vec<u8>],
        chain_print: bool,
        precut: bool,
        quality: protocol::PrintQuality,
    ) -> Result<()> {
        self.session
            .print_raster(lines, chain_print, precut, quality)
    }
    /// Print each slice as its own page.
    ///
    /// `cut_between` ejects and cuts after every page. Otherwise only the last
    /// page is cut, and earlier pages stay on the same strip.
    pub fn print_pages(
        &mut self,
        pages: &[&[Vec<u8>]],
        chain_print: bool,
        precut: bool,
        quality: protocol::PrintQuality,
        cut_between: bool,
    ) -> Result<()> {
        self.session
            .print_pages(pages, chain_print, precut, quality, cut_between)
    }
    /// Feed tape forward and cut.
    pub fn feed_and_cut(&mut self) -> Result<()> {
        self.session.feed_and_cut()
    }
    /// Release the USB interface and close the device.
    pub fn close(self) -> Result<()> {
        self.session.close()
    }
}

/// Find the bulk IN and OUT endpoints for the printer interface.
fn find_bulk_endpoints(handle: &DeviceHandle<Context>) -> Result<(u8, u8)> {
    let device = handle.device();
    let config = device.active_config_descriptor()?;

    let mut ep_out: Option<u8> = None;
    let mut ep_in: Option<u8> = None;

    for interface in config.interfaces() {
        for desc in interface.descriptors() {
            if desc.interface_number() != USB_INTERFACE {
                continue;
            }
            for endpoint in desc.endpoint_descriptors() {
                if endpoint.transfer_type() != rusb::TransferType::Bulk {
                    continue;
                }
                match endpoint.direction() {
                    rusb::Direction::Out => {
                        ep_out = Some(endpoint.address());
                    }
                    rusb::Direction::In => {
                        ep_in = Some(endpoint.address());
                    }
                }
            }
        }
    }

    match (ep_out, ep_in) {
        (Some(out), Some(inp)) => Ok((out, inp)),
        _ => Err(PtouchError::DeviceNotFound),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn usb_facade_preserves_send_and_sync() {
        fn assert_traits<T: Send + Sync>() {}
        assert_traits::<PtouchDevice>();
    }
}
