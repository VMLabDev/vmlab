// Copyright 2019 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE.crosvm file.

//! Host USB passthrough backend: a device on the host's usbdevfs behind the
//! xHCI's `UsbDevice` seam.
//!
//! Lifted from crosvm's `devices/src/usb/backend` host backend and
//! `usb_util`, pruned of FIDO and of crosvm's event-loop/job-queue plumbing:
//! this controller is polled (1 ms), so URB completions are reaped inside
//! `control`/`transfer` retries instead of by a handler thread. The lift
//! source never rebinds host kernel drivers after releasing an interface
//! (notes/03 F23); this backend issues USBDEVFS_CONNECT on detach and on
//! claim failure so the device returns to the host.
//!
//! The lift source makes its synchronous usbdevfs calls (SETCONFIGURATION,
//! SETINTERFACE, CLEAR_HALT, the claims, GET_CONFIGURATION) on the thread
//! that serves the guest request. Here that thread holds the xHCI lock, so
//! a guest request makes none of them: each runs as a [`Job`] on a thread
//! of its own while the request is `Nak`ed (review usb.md F4).

pub mod descriptor;
pub mod device;
pub mod error;
pub mod fs;
pub mod sys;
pub mod types;

#[cfg(test)]
pub mod fake;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;

use descriptor::ConfigDescriptorTree;
pub use device::{Device, Transfer, TransferStatus};
pub use error::{Error, Result};
pub use fs::{UsbFs, Usbdevfs};
use types::{
    ControlRequestDataPhaseTransferDirection, ControlRequestRecipient, DeviceSpeed, EndpointType,
    StandardControlRequest,
};
use zerocopy::IntoBytes;

use crate::usb::{SetupPacket, UsbDevice, UsbSpeed, UsbTransfer};
use crate::{DeviceError, Snapshot};

/// Standard ENDPOINT_HALT feature selector cleared to clear a stall.
const STD_FEATURE_ENDPOINT_HALT: u16 = 0;
/// Descriptor rewriting target for interfaces the guest may not drive.
const VENDOR_SPECIFIC: u8 = 0xff;
/// Where the reattached device node lives.
const USB_DEV_ROOT: &str = "/dev/bus/usb";

#[derive(Clone, Copy)]
struct EndpointInfo {
    ty: EndpointType,
    max_packet: u16,
}

/// What one in-flight or completed transfer means to the guest.
enum Pending {
    InFlight,
    Done(UsbTransfer),
}

/// A blocking usbdevfs call that a guest request asked for. usbdevfs has
/// no asynchronous form of these, and each waits on the device or on a
/// host driver for as long as that takes.
#[derive(Clone)]
enum Job {
    /// Release `release`, set configuration `config` when `set`, then
    /// claim interfaces `0..interfaces`.
    SetConfig {
        config: u8,
        release: Vec<u8>,
        set: bool,
        interfaces: u8,
    },
    /// Switch `interface` to `alt`; `endpoints` is the endpoint table that
    /// holds once the switch succeeded.
    SetInterface {
        interface: u8,
        alt: u8,
        endpoints: HashMap<u8, EndpointInfo>,
    },
    ClearHalt {
        endpoint: u8,
    },
}

/// What a finished [`Job`] left behind.
struct JobDone {
    result: Result<()>,
    /// The interfaces held now; meaningful for `SetConfig` only.
    claimed: Vec<u8>,
    /// The kernel answered ENODEV.
    lost: bool,
}

/// The job in flight. At most one runs, and while it does this side makes
/// no usbdevfs call at all: the kernel serialises the ioctls of one device
/// on its device lock, so even a URB submission would wait for the job.
struct Running {
    job: Job,
    worker: JoinHandle<JobDone>,
    /// The guest dropped the control request that started the job. The job
    /// still finishes and its effect on the device is still recorded; only
    /// its answer goes nowhere.
    abandoned: bool,
}

/// Runs on the job's thread. Touches nothing but `fs`.
fn run_job(fs: &dyn UsbFs, job: &Job) -> JobDone {
    let gone = |e: &std::io::Error| e.raw_os_error() == Some(libc::ENODEV);
    match job {
        Job::SetConfig {
            config,
            release,
            set,
            interfaces,
        } => {
            for interface in release {
                // Errors are expected when the device is already gone.
                let _ = fs.release_interface(*interface);
            }
            if *set && let Err(e) = fs.set_configuration(*config) {
                let lost = gone(&e);
                let e = Error::IoctlFailed(sys::USBDEVFS_SETCONFIGURATION, e);
                return JobDone {
                    result: Err(Error::SetConfiguration(*config, Box::new(e))),
                    claimed: Vec::new(),
                    lost,
                };
            }
            let mut claimed = Vec::new();
            for interface in 0..*interfaces {
                if let Err(e) = fs.claim_interface(interface) {
                    // All or none, as at the open: give back what was
                    // taken so the host's drivers rebind.
                    for taken in &claimed {
                        let _ = fs.release_interface(*taken);
                    }
                    for taken in &claimed {
                        let _ = fs.connect_interface(*taken);
                    }
                    let lost = gone(&e);
                    let e = device::claim_error(e);
                    return JobDone {
                        result: Err(Error::ClaimInterface(interface, Box::new(e))),
                        claimed: Vec::new(),
                        lost,
                    };
                }
                claimed.push(interface);
            }
            JobDone {
                result: Ok(()),
                claimed,
                lost: false,
            }
        }
        Job::SetInterface { interface, alt, .. } => {
            let result = fs.set_interface(*interface, *alt);
            let lost = result.as_ref().is_err_and(gone);
            JobDone {
                result: result.map_err(|e| {
                    let e = Error::IoctlFailed(sys::USBDEVFS_SETINTERFACE, e);
                    Error::SetInterface(*interface, *alt, Box::new(e))
                }),
                claimed: Vec::new(),
                lost,
            }
        }
        Job::ClearHalt { endpoint } => {
            let result = fs.clear_halt(*endpoint);
            let lost = result.as_ref().is_err_and(gone);
            JobDone {
                result: result.map_err(|e| Error::IoctlFailed(sys::USBDEVFS_CLEAR_HALT, e)),
                claimed: Vec::new(),
                lost,
            }
        }
    }
}

/// A host USB device passed through to the guest. One instance owns the
/// usbdevfs file, its interface claims, and the in-flight URBs.
pub struct HostUsbDevice {
    id: String,
    device: Device,
    /// Whether the guest has issued its first SET_CONFIGURATION.
    initialized: bool,
    alt_settings: HashMap<u8, u8>,
    claimed_interfaces: Vec<u8>,
    endpoints: HashMap<u8, EndpointInfo>,
    /// Per-endpoint transfer state, keyed by USB endpoint address
    /// (0 for the control endpoint).
    pending: HashMap<u8, Pending>,
    /// Endpoints whose in-flight work was dropped; their cancelled
    /// completions are discarded rather than delivered.
    discarded: HashSet<u8>,
    /// Read once at the open; the xHCI asks for it under its lock.
    speed: UsbSpeed,
    /// bConfigurationValue of the active configuration: read from the
    /// device at the open, then followed through the guest's
    /// SET_CONFIGURATION requests. `None` when it is not known (the device
    /// did not answer, or a SET_CONFIGURATION failed in the device).
    active_config: Option<u8>,
    /// bConfigurationValue of the configuration the host had the device in
    /// at the open, which the detach puts back (review usb.md F6). `None`
    /// when the device did not say.
    opened_config: Option<u8>,
    /// The blocking usbdevfs call in flight, if any.
    job: Option<Running>,
    /// Endpoints dropped while a job ran: their URBs are discarded once
    /// the job is over.
    deferred_discards: HashSet<u8>,
}

impl HostUsbDevice {
    /// Open `/dev/bus/usb/<bus>/<dev>` and take over its interfaces. This
    /// blocks: the claim waits for the host driver to let go, and a device
    /// with several configurations is asked which one is active. Call it
    /// before the device is on a controller, with no controller lock held.
    pub fn open(id: impl Into<String>, bus: u16, dev: u16) -> Result<Self> {
        let path = device_node(bus, dev);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| Error::Other(format!("opening {}: {e}", path.display())))?;
        Self::new(id, Box::new(Usbdevfs::new(file)))
    }

    /// Build the backend over an open usbdevfs (or a fake of one).
    pub fn new(id: impl Into<String>, fs: Box<dyn UsbFs>) -> Result<Self> {
        let device = Device::new(Arc::from(fs))?;
        let speed = match device.get_speed().unwrap_or(None) {
            Some(DeviceSpeed::Low) => UsbSpeed::Low,
            Some(DeviceSpeed::Full) => UsbSpeed::Full,
            Some(DeviceSpeed::Super | DeviceSpeed::SuperPlus) => UsbSpeed::Super,
            _ => UsbSpeed::High,
        };
        let active_config = device.get_active_configuration().ok();
        let mut this = Self {
            id: id.into(),
            device,
            initialized: false,
            alt_settings: HashMap::new(),
            claimed_interfaces: Vec::new(),
            endpoints: HashMap::new(),
            pending: HashMap::new(),
            discarded: HashSet::new(),
            speed,
            active_config,
            opened_config: active_config,
            job: None,
            deferred_discards: HashSet::new(),
        };
        // Claim the interfaces of the active configuration, if the device
        // is configured; a freshly plugged device may still be at
        // configuration 0 and is claimed when the guest configures it.
        if let Some(config) = active_config
            && config != 0
            && let Ok(descriptor) = this.device.get_config_descriptor(config)
        {
            this.claim_interfaces(&descriptor)?;
            this.endpoints = this.endpoint_table(&descriptor, &this.alt_settings)?;
        }
        Ok(this)
    }

    /// "vendor:product" for logs and refusal messages.
    pub fn describe(&self) -> String {
        let descriptor = self.device.get_device_descriptor();
        // Copy the packed fields before formatting borrows them.
        let (vendor, product) = (descriptor.idVendor, descriptor.idProduct);
        format!("{vendor:04x}:{product:04x}")
    }

    fn usb_speed(&self) -> UsbSpeed {
        self.speed
    }

    /// Hand `job` to a thread of its own and `Nak` the request: a retry
    /// finds the answer once the job is over.
    fn start_job(&mut self, job: Job) -> UsbTransfer {
        let fs = self.device.fs();
        let work = job.clone();
        let spawned = std::thread::Builder::new()
            .name("usb-host-job".into())
            .spawn(move || run_job(&*fs, &work));
        match spawned {
            Ok(worker) => {
                self.job = Some(Running {
                    job,
                    worker,
                    abandoned: false,
                });
                UsbTransfer::Nak
            }
            Err(e) => {
                log::warn!("usb host {}: no thread for a request: {e}", self.describe());
                UsbTransfer::Stall
            }
        }
    }

    /// Take over the result of the job once it is over (`wait`: block until
    /// it is): record what it did to the device, file its answer for the
    /// guest, and catch up on the discards that waited for it.
    fn settle_job(&mut self, wait: bool) {
        let Some(running) = self.job.take_if(|r| wait || r.worker.is_finished()) else {
            return;
        };
        let done = running.worker.join().unwrap_or_else(|_| JobDone {
            result: Err(Error::Other("the usbdevfs worker panicked".into())),
            claimed: Vec::new(),
            lost: false,
        });
        if done.lost {
            self.device.mark_lost();
        }
        let mut result = done.result;
        let what = match running.job {
            Job::SetConfig { config, .. } => {
                self.claimed_interfaces = done.claimed;
                self.endpoints.clear();
                // SET_CONFIGURATION puts every interface of the device at
                // alt setting 0; what SET_INTERFACE chose before belongs to
                // the old configuration.
                self.alt_settings.clear();
                self.active_config = match &result {
                    // The device may be at its old configuration or at
                    // none; nothing here can tell which.
                    Err(Error::SetConfiguration(..)) => None,
                    _ => Some(config),
                };
                if result.is_ok() && config != 0 {
                    result = self
                        .device
                        .get_config_descriptor(config)
                        .and_then(|descriptor| self.endpoint_table(&descriptor, &self.alt_settings))
                        .map(|endpoints| self.endpoints = endpoints);
                }
                format!("set config {config}")
            }
            Job::SetInterface {
                interface,
                alt,
                endpoints,
            } => {
                if result.is_ok() {
                    self.alt_settings.insert(interface, alt);
                    self.endpoints = endpoints;
                }
                format!("set interface {interface} alt {alt}")
            }
            Job::ClearHalt { endpoint } => format!("clear halt {endpoint:#x}"),
        };
        let outcome = match result {
            Ok(()) => UsbTransfer::Ack(Vec::new()),
            Err(e) => {
                log::warn!("usb host {}: {what}: {e}", self.describe());
                UsbTransfer::Stall
            }
        };
        if !running.abandoned {
            self.pending.insert(0, Pending::Done(outcome));
        }
        for endpoint in std::mem::take(&mut self.deferred_discards) {
            self.device.discard_endpoint(endpoint);
        }
    }

    /// Reap completed URBs and file them under their endpoints. While a
    /// job runs this makes no usbdevfs call, the reap included.
    fn reap(&mut self) {
        self.settle_job(false);
        if self.job.is_some() {
            return;
        }
        let completed = match self.device.poll_transfers() {
            Ok(completed) => completed,
            Err(_) => return,
        };
        for transfer in completed {
            let endpoint = transfer.endpoint();
            match transfer.status() {
                _ if self.discarded.remove(&endpoint) => {}
                status => {
                    let outcome = match status {
                        TransferStatus::Completed => {
                            let isochronous = self
                                .endpoints
                                .get(&endpoint)
                                .is_some_and(|ep| ep.ty == EndpointType::Isochronous);
                            if endpoint == 0 {
                                UsbTransfer::Ack(transfer.control_data().to_vec())
                            } else if isochronous && endpoint & 0x80 != 0 {
                                UsbTransfer::Ack(transfer.iso_data())
                            } else if endpoint & 0x80 != 0 {
                                UsbTransfer::Ack(transfer.data().to_vec())
                            } else {
                                UsbTransfer::Ack(Vec::new())
                            }
                        }
                        _ => UsbTransfer::Stall,
                    };
                    self.pending.insert(endpoint, Pending::Done(outcome));
                }
            }
        }
    }

    /// Start a transfer unless one is already in flight; `Nak` keeps the
    /// guest's TD queued until a retry finds the completion.
    fn start(&mut self, endpoint: u8, transfer: Transfer) -> UsbTransfer {
        if self.discarded.contains(&endpoint) {
            return UsbTransfer::Nak;
        }
        match self.device.submit_transfer(transfer) {
            Ok(()) => {
                self.pending.insert(endpoint, Pending::InFlight);
                UsbTransfer::Nak
            }
            Err(_) => UsbTransfer::Stall,
        }
    }

    fn take_pending(&mut self, endpoint: u8) -> UsbTransfer {
        match self.pending.remove(&endpoint) {
            Some(Pending::Done(outcome)) => outcome,
            _ => UsbTransfer::Nak,
        }
    }

    /// Claims every interface of `config_descriptor`, detaching whatever
    /// host kernel driver is bound to each. Either all of them are claimed
    /// or none are; on failure the detached interfaces are reconnected so
    /// the host's drivers rebind.
    fn claim_interfaces(&mut self, config_descriptor: &ConfigDescriptorTree) -> Result<()> {
        for i in 0..config_descriptor.bNumInterfaces {
            // Bound to a variable, not matched on directly: the Err arm
            // below releases and reconnects, which must not hold a borrow.
            let claimed = self.device.claim_interface(i);
            match claimed {
                Ok(()) => {
                    self.claimed_interfaces.push(i);
                }
                Err(e) => {
                    self.release_and_reconnect();
                    return Err(Error::ClaimInterface(i, Box::new(e)));
                }
            }
        }
        Ok(())
    }

    /// Release every claimed interface and let host kernel drivers bind
    /// again (USBDEVFS_CONNECT). This is the claim-failure path of the open
    /// and the detach of a device that is still in the configuration found
    /// at the open ([`Self::return_to_host`]); a guest-driven
    /// reconfiguration is a [`Job`].
    fn release_and_reconnect(&mut self) {
        for i in &self.claimed_interfaces {
            // Errors are expected when the device is already gone.
            let _ = self.device.release_interface(*i);
        }
        for i in &self.claimed_interfaces {
            // Errors are expected when the device is already gone.
            let _ = self.device.connect_interface(*i);
        }
        self.claimed_interfaces.clear();
        self.endpoints.clear();
    }

    /// The detach: give the device back to the host as the host had it.
    ///
    /// The guest may have left the device in another configuration than the
    /// one found at the open, or in none (SET_CONFIGURATION with value 0).
    /// A device without a configuration has no interfaces, so no host
    /// driver binds to it until a replug. The configuration found at the
    /// open is set again in that case. Setting it when the device still has
    /// it would only reset the device, so that is not done.
    ///
    /// A device that was at configuration 0 at the open stays in the
    /// guest's configuration: that leaves the host more than it had.
    fn return_to_host(&mut self) {
        let restore = self
            .opened_config
            .filter(|opened| *opened != 0 && self.active_config != Some(*opened));
        let Some(opened) = restore else {
            self.release_and_reconnect();
            return;
        };
        // The kernel refuses SETCONFIGURATION while an interface is
        // claimed, so the guest's claims go first.
        for i in &self.claimed_interfaces {
            // Errors are expected when the device is already gone.
            let _ = self.device.release_interface(*i);
        }
        self.claimed_interfaces.clear();
        self.endpoints.clear();
        if self.device.is_lost() {
            return;
        }
        if let Err(e) = self.device.fs().set_configuration(opened) {
            log::warn!(
                "usb host {}: the device does not return to configuration {opened}: {e}",
                self.describe()
            );
            return;
        }
        self.active_config = Some(opened);
        // The kernel probes the drivers of a configuration it has just
        // set. The connect covers an interface whose probe did not bind.
        let interfaces = self
            .device
            .get_config_descriptor(opened)
            .map_or(0, |descriptor| descriptor.bNumInterfaces);
        for i in 0..interfaces {
            let _ = self.device.connect_interface(i);
        }
    }

    /// The endpoint table of the claimed interfaces of `config_descriptor`
    /// at the alt settings `alt_settings`.
    fn endpoint_table(
        &self,
        config_descriptor: &ConfigDescriptorTree,
        alt_settings: &HashMap<u8, u8>,
    ) -> Result<HashMap<u8, EndpointInfo>> {
        let mut endpoints = HashMap::new();
        for interface in &self.claimed_interfaces {
            let alt_setting = alt_settings.get(interface).copied().unwrap_or(0);
            let interface_descriptor = config_descriptor
                .get_interface_descriptor(*interface, alt_setting)
                .ok_or(Error::NoSuchEndpoint(0x80))?;
            for ep_idx in 0..interface_descriptor.bNumEndpoints {
                let Some(ep_dp) = interface_descriptor.get_endpoint_descriptor(ep_idx) else {
                    continue;
                };
                if ep_dp.get_endpoint_number() == 0 {
                    continue;
                }
                endpoints.insert(
                    ep_dp.address(),
                    EndpointInfo {
                        ty: ep_dp
                            .get_endpoint_type()
                            .ok_or_else(|| Error::Other("invalid endpoint type".into()))?,
                        max_packet: ep_dp.max_packet_size(),
                    },
                );
            }
        }
        Ok(endpoints)
    }

    /// Forget every transfer of the endpoints `keep` selects: an in-flight
    /// URB is discarded and its cancelled completion dropped at the reap,
    /// and a completion that was never collected is thrown away.
    fn drop_transfers(&mut self, keep: impl Fn(u8) -> bool) {
        let endpoints: Vec<u8> = self.pending.keys().copied().filter(|e| keep(*e)).collect();
        for endpoint in endpoints {
            self.endpoint_dropped(endpoint);
        }
    }

    /// SET_CONFIGURATION: everything that can be refused from the
    /// descriptors is refused here, the rest is a job.
    fn set_config(&mut self, config: u8) -> Result<UsbTransfer> {
        let interfaces = if config == 0 {
            0
        } else {
            self.device.get_config_descriptor(config)?.bNumInterfaces
        };
        // The guest's first SET_CONFIGURATION usually names the
        // configuration the device is in already; setting it again would
        // reset the device for nothing.
        // A guest that chose an alt setting first has changed the device:
        // only setting the configuration puts its interfaces back at 0.
        let set =
            self.initialized || self.active_config != Some(config) || !self.alt_settings.is_empty();
        self.initialized = true;
        // The old configuration's endpoints end here. Their URBs are
        // discarded before the job releases the interfaces; left alone, the
        // release cancels them and the reap files each as a stall on the
        // new configuration's endpoint. The control endpoint carries this
        // very request and stays.
        self.drop_transfers(|endpoint| endpoint != 0);
        Ok(self.start_job(Job::SetConfig {
            config,
            release: self.claimed_interfaces.clone(),
            set,
            interfaces,
        }))
    }

    /// SET_INTERFACE: as `set_config`.
    fn set_interface(&mut self, interface: u8, alt: u8) -> Result<UsbTransfer> {
        let refused = |e| Error::SetInterface(interface, alt, Box::new(e));
        let config = self.active_config.ok_or_else(|| {
            let unknown = Error::Other("the active configuration is not known".into());
            refused(Error::GetActiveConfiguration(Box::new(unknown)))
        })?;
        let descriptor = self.device.get_config_descriptor(config).map_err(refused)?;
        let mut alt_settings = self.alt_settings.clone();
        alt_settings.insert(interface, alt);
        let endpoints = self
            .endpoint_table(&descriptor, &alt_settings)
            .map_err(refused)?;
        Ok(self.start_job(Job::SetInterface {
            interface,
            alt,
            endpoints,
        }))
    }

    /// Serve a GET_DESCRIPTOR(Configuration) request from the cached
    /// descriptor tree, rewriting unclaimed interfaces to vendor-specific so
    /// guest drivers leave them alone. Lifted from crosvm's
    /// `get_config_descriptor_filtered`.
    fn config_descriptor_filtered(&self, descriptor_index: u8) -> Result<Vec<u8>> {
        let config_descriptor = self
            .device
            .get_config_descriptor_by_index(descriptor_index)?;
        let device_descriptor = self.device.get_device_descriptor_tree();
        let config_start = config_descriptor.offset();
        let config_end = config_start + config_descriptor.total_length() as usize;
        let mut descriptor_data = device_descriptor.raw()[config_start..config_end].to_vec();

        if self.active_config == Some(config_descriptor.bConfigurationValue) {
            for i in 0..config_descriptor.bNumInterfaces {
                if !self.claimed_interfaces.contains(&i) {
                    // Rewrite descriptors for unclaimed interfaces to vendor-specific class.
                    // This prevents them from being recognized by the guest drivers.
                    let alt_setting = self.alt_settings.get(&i).copied().unwrap_or(0);
                    let interface = config_descriptor
                        .get_interface_descriptor(i, alt_setting)
                        .ok_or(Error::NoSuchDescriptor)?;
                    let mut interface_data: types::InterfaceDescriptor = **interface;
                    interface_data.bInterfaceClass = VENDOR_SPECIFIC;
                    interface_data.bInterfaceSubClass = VENDOR_SPECIFIC;
                    interface_data.bInterfaceProtocol = VENDOR_SPECIFIC;

                    let interface_start = interface.offset()
                        + std::mem::size_of::<types::DescriptorHeader>()
                        - config_start;
                    let interface_end =
                        interface_start + std::mem::size_of::<types::InterfaceDescriptor>();
                    descriptor_data[interface_start..interface_end]
                        .copy_from_slice(interface_data.as_bytes());
                }
            }
        }
        Ok(descriptor_data)
    }

    fn intercept_control(&mut self, setup: &SetupPacket) -> Option<UsbTransfer> {
        use ControlRequestDataPhaseTransferDirection as Dir;
        use ControlRequestRecipient as Recipient;
        let direction = if setup.request_type & 0x80 != 0 {
            Dir::DeviceToHost
        } else {
            Dir::HostToDevice
        };
        let recipient = match setup.request_type & 0b1111 {
            0 => Recipient::Device,
            1 => Recipient::Interface,
            2 => Recipient::Endpoint,
            _ => Recipient::Other,
        };
        // Only standard requests are intercepted; class and vendor requests
        // pass through to the device.
        if setup.request_type & 0x60 != 0 {
            return None;
        }
        let request = match setup.request {
            5 => StandardControlRequest::SetAddress,
            6 => StandardControlRequest::GetDescriptor,
            9 => StandardControlRequest::SetConfiguration,
            1 => StandardControlRequest::ClearFeature,
            11 => StandardControlRequest::SetInterface,
            _ => return None,
        };
        match (request, recipient, direction) {
            (StandardControlRequest::SetAddress, Recipient::Device, Dir::HostToDevice) => {
                // usbdevfs owns addressing; nothing to do.
                Some(UsbTransfer::Ack(Vec::new()))
            }
            (StandardControlRequest::SetConfiguration, Recipient::Device, Dir::HostToDevice) => {
                let config = (setup.value & 0xff) as u8;
                Some(match self.set_config(config) {
                    Ok(started) => started,
                    Err(e) => {
                        log::warn!("usb host {}: set config {config}: {e}", self.describe());
                        UsbTransfer::Stall
                    }
                })
            }
            (StandardControlRequest::SetInterface, Recipient::Interface, Dir::HostToDevice) => {
                let interface = setup.index as u8;
                let alt = setup.value as u8;
                Some(match self.set_interface(interface, alt) {
                    Ok(started) => started,
                    Err(e) => {
                        log::warn!(
                            "usb host {}: set interface {interface} alt {alt}: {e}",
                            self.describe()
                        );
                        UsbTransfer::Stall
                    }
                })
            }
            (StandardControlRequest::ClearFeature, Recipient::Endpoint, Dir::HostToDevice) => {
                if setup.value == STD_FEATURE_ENDPOINT_HALT {
                    Some(self.start_job(Job::ClearHalt {
                        endpoint: setup.index as u8,
                    }))
                } else {
                    None
                }
            }
            (StandardControlRequest::GetDescriptor, Recipient::Device, Dir::DeviceToHost) => {
                let descriptor_type = (setup.value >> 8) as u8;
                if descriptor_type != types::DescriptorType::Configuration as u8 {
                    return None;
                }
                Some(match self.config_descriptor_filtered(setup.value as u8) {
                    Ok(bytes) => UsbTransfer::Ack(bytes),
                    Err(e) => {
                        log::warn!(
                            "usb host {}: configuration descriptor: {e}",
                            self.describe()
                        );
                        UsbTransfer::Stall
                    }
                })
            }
            // These three change state the host kernel tracks for the
            // device. In any other shape (another recipient, the IN
            // direction) they are not valid requests, and a device that
            // decodes by bRequest alone would still act on them behind the
            // kernel's back. They are refused here and never forwarded.
            (
                StandardControlRequest::SetAddress
                | StandardControlRequest::SetConfiguration
                | StandardControlRequest::SetInterface,
                _,
                _,
            ) => Some(UsbTransfer::Stall),
            _ => None,
        }
    }
}

impl UsbDevice for HostUsbDevice {
    fn speed(&self) -> UsbSpeed {
        self.usb_speed()
    }

    fn reset(&mut self) {
        // The lift source skips USBDEVFS_RESET (it would also drop the
        // configuration and the claims); the xHCI re-initializes slot and
        // endpoint state itself. Nothing in flight belongs to the guest
        // after a reset, so none of it may complete a later transfer.
        self.drop_transfers(|_| true);
        self.device.reset();
    }

    fn control(&mut self, setup: SetupPacket, data: &[u8]) -> UsbTransfer {
        // usbdevfs owns addressing, so SET_ADDRESS is answered at once in
        // every state: the xHCI sends it from its Address Device command,
        // which takes no NAK, and it must not consume the answer that
        // waits for another request.
        if setup.request_type == 0 && setup.request == 5 {
            return UsbTransfer::Ack(Vec::new());
        }
        self.reap();
        if self.job.is_some() {
            return UsbTransfer::Nak;
        }
        // Before the interception: the retry of a request that became a
        // job must find the job's answer, not start the job again.
        if matches!(self.pending.get(&0), Some(Pending::Done(_))) {
            return self.take_pending(0);
        }
        if let Some(outcome) = self.intercept_control(&setup) {
            return outcome;
        }
        if self.device.is_lost() {
            return UsbTransfer::Stall;
        }
        if self.pending.contains_key(&0) {
            return UsbTransfer::Nak;
        }
        // The control buffer carries the 8-byte setup packet followed by
        // the data phase: the guest's OUT payload, or receive capacity.
        let mut buffer = Vec::with_capacity(8 + usize::from(setup.length));
        buffer.extend_from_slice(&[
            setup.request_type,
            setup.request,
            setup.value.to_le_bytes()[0],
            setup.value.to_le_bytes()[1],
            setup.index.to_le_bytes()[0],
            setup.index.to_le_bytes()[1],
            setup.length.to_le_bytes()[0],
            setup.length.to_le_bytes()[1],
        ]);
        if setup.request_type & 0x80 != 0 {
            buffer.resize(8 + usize::from(setup.length), 0);
        } else {
            buffer.extend_from_slice(data);
        }
        match Transfer::new_control(buffer) {
            Ok(transfer) => self.start(0, transfer),
            Err(_) => UsbTransfer::Stall,
        }
    }

    fn transfer(&mut self, endpoint: u8, data: &[u8], max_length: usize) -> UsbTransfer {
        self.reap();
        if self.job.is_some() {
            return UsbTransfer::Nak;
        }
        if self.device.is_lost() {
            return UsbTransfer::Stall;
        }
        if matches!(self.pending.get(&endpoint), Some(Pending::Done(_))) {
            return self.take_pending(endpoint);
        }
        if self.pending.contains_key(&endpoint) {
            return UsbTransfer::Nak;
        }
        let Some(info) = self.endpoints.get(&endpoint).copied() else {
            return UsbTransfer::Stall;
        };
        if info.ty == EndpointType::Isochronous {
            return self.transfer_iso(endpoint, data, max_length, info.max_packet as u32);
        }
        let buffer = if endpoint & 0x80 != 0 {
            vec![0; max_length]
        } else {
            data.to_vec()
        };
        let transfer = match info.ty {
            EndpointType::Bulk => Transfer::new_bulk(endpoint, buffer),
            _ => Transfer::new_interrupt(endpoint, buffer),
        };
        match transfer {
            Ok(transfer) => self.start(endpoint, transfer),
            Err(_) => UsbTransfer::Stall,
        }
    }

    fn transfer_iso(
        &mut self,
        endpoint: u8,
        data: &[u8],
        max_length: usize,
        packet_size: u32,
    ) -> UsbTransfer {
        self.reap();
        if self.job.is_some() {
            return UsbTransfer::Nak;
        }
        if self.device.is_lost() {
            return UsbTransfer::Stall;
        }
        if matches!(self.pending.get(&endpoint), Some(Pending::Done(_))) {
            return self.take_pending(endpoint);
        }
        if self.pending.contains_key(&endpoint) {
            return UsbTransfer::Nak;
        }
        let Some(info) = self.endpoints.get(&endpoint).copied() else {
            return UsbTransfer::Stall;
        };
        if info.ty != EndpointType::Isochronous {
            return self.transfer(endpoint, data, max_length);
        }
        // The lift source does not support isochronous OUT passthrough.
        if endpoint & 0x80 == 0 {
            return UsbTransfer::Stall;
        }
        let packet_size = if packet_size == 0 {
            u32::from(info.max_packet)
        } else {
            packet_size
        };
        let buffer = vec![0; max_length];
        match Transfer::new_isochronous(endpoint, buffer, packet_size) {
            Ok(transfer) => self.start(endpoint, transfer),
            Err(_) => UsbTransfer::Stall,
        }
    }

    fn endpoint_dropped(&mut self, endpoint: u8) {
        if endpoint == 0
            && let Some(running) = &mut self.job
        {
            running.abandoned = true;
        }
        if matches!(self.pending.remove(&endpoint), Some(Pending::InFlight)) {
            self.discarded.insert(endpoint);
            if self.job.is_some() {
                // DISCARDURB would wait for the job in the kernel.
                self.deferred_discards.insert(endpoint);
            } else {
                self.device.discard_endpoint(endpoint);
            }
        }
    }

    fn is_lost(&self) -> bool {
        self.device.is_lost()
    }

    fn is_host_backed(&self) -> bool {
        true
    }
}

impl Drop for HostUsbDevice {
    fn drop(&mut self) {
        // Give the device back to the host: releasing the claims alone
        // leaves the interfaces driverless, because the claim detached the
        // kernel driver and only USBDEVFS_CONNECT rebinds it. A job in
        // flight is waited for first, so the claims it takes are known;
        // the callers drop a backend with no controller lock held.
        self.settle_job(true);
        self.return_to_host();
    }
}

impl Snapshot for HostUsbDevice {
    fn state_id(&self) -> String {
        format!("usb-host-{}", self.id)
    }

    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        Err(DeviceError::State(format!(
            "host USB device {} is attached: online snapshots are refused while it is (D9)",
            self.describe()
        )))
    }

    fn restore_state(&mut self, _bytes: &[u8]) -> Result<(), DeviceError> {
        Err(DeviceError::State(format!(
            "host USB device {} is attached: online snapshots are refused while it is (D9)",
            self.describe()
        )))
    }
}

/// The usbdevfs node for a device number on a bus.
pub fn device_node(bus: u16, dev: u16) -> PathBuf {
    Path::new(USB_DEV_ROOT)
        .join(format!("{bus:03}"))
        .join(format!("{dev:03}"))
}

#[cfg(test)]
mod tests {
    use super::fake::{Completion, FakeUsbFs};
    use super::*;
    use crate::usb::UsbDevice;

    /// A device over the fake, plus the test's handle on the fake.
    fn device() -> (HostUsbDevice, FakeUsbFs) {
        let fake = FakeUsbFs::new();
        let host = HostUsbDevice::new("usbhost-1-2", Box::new(fake.clone())).expect("device");
        (host, fake)
    }

    fn setup(request_type: u8, request: u8, value: u16, index: u16, length: u16) -> SetupPacket {
        SetupPacket {
            request_type,
            request,
            value,
            index,
            length,
        }
    }

    /// GET_DESCRIPTOR(Configuration), by descriptor index.
    fn get_config(index: u8) -> SetupPacket {
        setup(0x80, 6, (2 << 8) | u16::from(index), 0, 0xffff)
    }

    fn set_config(value: u8) -> SetupPacket {
        setup(0x00, 9, u16::from(value), 0, 0)
    }

    fn set_interface(index: u8, alt: u8) -> SetupPacket {
        setup(0x01, 11, u16::from(alt), u16::from(index), 0)
    }

    fn clear_halt(endpoint: u8) -> SetupPacket {
        setup(0x02, 1, 0, u16::from(endpoint), 0)
    }

    /// A control request as the guest sees it in the end: when the request
    /// became a job, wait for the job and retry, as the controller's poll
    /// does.
    fn control(host: &mut HostUsbDevice, request: SetupPacket) -> UsbTransfer {
        let first = host.control(request, &[]);
        if host.job.is_none() {
            return first;
        }
        assert_eq!(first, UsbTransfer::Nak, "a request with a job in flight");
        host.settle_job(true);
        host.control(request, &[])
    }

    #[test]
    fn intercepted_errors_use_the_logging_facade() {
        struct Warnings(parking_lot::Mutex<Vec<String>>);
        impl log::Log for Warnings {
            fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
                metadata.level() == log::Level::Warn
                    && metadata.target() == "vmlab_hv_devices::usb::host"
            }

            fn log(&self, record: &log::Record<'_>) {
                if self.enabled(record.metadata()) {
                    self.0.lock().push(record.args().to_string());
                }
            }

            fn flush(&self) {}
        }

        static WARNINGS: Warnings = Warnings(parking_lot::Mutex::new(Vec::new()));
        log::set_logger(&WARNINGS).unwrap();
        log::set_max_level(log::LevelFilter::Warn);
        for request in [set_config(255), set_interface(0, 255), get_config(255)] {
            let (mut host, _fake) = device();
            assert_eq!(host.control(request, &[]), UsbTransfer::Stall);
        }
        let messages = WARNINGS.0.lock();
        for operation in [
            "set config 255",
            "set interface 0 alt 255",
            "configuration descriptor",
        ] {
            assert!(messages.iter().any(|message| message.contains(operation)));
        }
    }

    #[test]
    fn device_node_paths() {
        assert_eq!(device_node(1, 5), Path::new("/dev/bus/usb/001/005"));
        assert_eq!(device_node(12, 321), Path::new("/dev/bus/usb/012/321"));
    }

    #[test]
    fn opening_claims_the_active_configuration() {
        let (host, fake) = device();
        assert_eq!(host.usb_speed(), UsbSpeed::High);
        assert_eq!(host.describe(), "4242:6666");
        // Construction claims both interfaces of configuration 1, which the
        // fake reports as active.
        assert_eq!(fake.calls().claims, vec![0, 1]);
        // Dropping releases the claims and reconnects them: the device
        // returns to the host instead of staying driverless.
        drop(host);
        let calls = fake.calls();
        assert_eq!(calls.releases, vec![0, 1]);
        assert_eq!(calls.connects, vec![0, 1]);
    }

    /// Review usb.md F6: a guest that unconfigures the device does not
    /// hand it back to the host without a configuration.
    #[test]
    fn detach_restores_the_configuration_found_at_open() {
        let (mut host, fake) = device();
        assert_eq!(
            control(&mut host, set_config(0)),
            UsbTransfer::Ack(Vec::new())
        );
        assert_eq!(fake.calls().set_configurations, vec![0]);
        assert_eq!(fake.calls().connects, Vec::<u8>::new());
        drop(host);
        let calls = fake.calls();
        assert_eq!(calls.set_configurations, vec![0, 1]);
        assert_eq!(calls.connects, vec![0, 1]);
    }

    /// Review usb.md F6, second half: the host does not get the device
    /// back in another configuration the guest chose.
    #[test]
    fn detach_restores_the_configuration_the_guest_replaced() {
        let fake = FakeUsbFs::with_two_configurations();
        let mut host = HostUsbDevice::new("usbhost-1-2", Box::new(fake.clone())).expect("device");
        assert_eq!(
            control(&mut host, set_config(2)),
            UsbTransfer::Ack(Vec::new())
        );
        assert_eq!(fake.calls().set_configurations, vec![2]);
        drop(host);
        let calls = fake.calls();
        // The guest's claims are released before the configuration is set:
        // the kernel refuses SETCONFIGURATION on a claimed interface.
        assert_eq!(calls.releases, vec![0, 1, 0, 1]);
        assert_eq!(calls.set_configurations, vec![2, 1]);
        assert_eq!(calls.connects, vec![0, 1]);
    }

    /// A guest that leaves the device in the configuration it had makes
    /// the detach set none: SETCONFIGURATION would reset the device.
    #[test]
    fn detach_sets_no_configuration_the_device_still_has() {
        let (mut host, fake) = device();
        assert_eq!(
            control(&mut host, set_config(1)),
            UsbTransfer::Ack(Vec::new())
        );
        drop(host);
        let calls = fake.calls();
        assert_eq!(calls.set_configurations, Vec::<u8>::new());
        assert_eq!(calls.connects, vec![0, 1]);
    }

    /// An unplugged device is asked for nothing at the detach.
    #[test]
    fn detach_sets_no_configuration_on_a_lost_device() {
        let (mut host, fake) = device();
        assert_eq!(
            control(&mut host, set_config(0)),
            UsbTransfer::Ack(Vec::new())
        );
        host.device.mark_lost();
        drop(host);
        assert_eq!(fake.calls().set_configurations, vec![0]);
    }

    #[test]
    fn claim_failure_releases_and_reconnects() {
        let fake = FakeUsbFs::new();
        fake.fail_claim(1);
        let error = match HostUsbDevice::new("usbhost-1-2", Box::new(fake.clone())) {
            Ok(_) => panic!("claiming interface 1 must fail"),
            Err(error) => error,
        };
        assert!(matches!(error, Error::ClaimInterface(1, _)), "{error:?}");
        let calls = fake.calls();
        assert_eq!(calls.claims, vec![0]);
        assert_eq!(calls.releases, vec![0]);
        assert_eq!(calls.connects, vec![0]);
    }

    /// Review usb.md F1: the kernel answers EBUSY for an interface another
    /// usbdevfs user holds, and the refusal says so.
    #[test]
    fn a_claim_another_usbfs_user_holds_is_refused_by_name() {
        let fake = FakeUsbFs::new();
        fake.fail_claim(0);
        let error = match HostUsbDevice::new("usbhost-1-2", Box::new(fake.clone())) {
            Ok(_) => panic!("claiming a held interface must fail"),
            Err(error) => error,
        };
        assert!(
            matches!(&error, Error::ClaimInterface(0, held) if matches!(**held, Error::InterfaceHeld)),
            "{error:?}"
        );
        let text = error.to_string();
        assert!(text.contains("claiming interface 0 failed"), "{text}");
        assert!(text.contains("held by another program"), "{text}");
        // Nothing was claimed, so nothing is released or reconnected: the
        // holder keeps its claim.
        let calls = fake.calls();
        assert!(calls.releases.is_empty() && calls.connects.is_empty());
    }

    #[test]
    fn intercepted_requests_never_reach_usbdevfs() {
        let (mut host, fake) = device();
        // SET_ADDRESS is a no-op behind usbdevfs.
        assert_eq!(
            host.control(setup(0x00, 5, 0x22, 0, 0), &[]),
            UsbTransfer::Ack(Vec::new())
        );
        // GET_DESCRIPTOR(Configuration) is served from the cached tree.
        let descriptor = host.control(get_config(0), &[]);
        let UsbTransfer::Ack(bytes) = descriptor else {
            panic!("configuration descriptor must be acknowledged");
        };
        assert_eq!(bytes.len(), 0x46);
        assert_eq!(&bytes[..2], &[0x09, 0x02]);
        // Neither interception submits a control URB.
        assert!(fake.submitted_payloads().is_empty());
        assert_eq!(
            control(&mut host, set_config(1)),
            UsbTransfer::Ack(Vec::new())
        );
        assert!(fake.submitted_payloads().is_empty());
    }

    /// Review usb.md F7: SET_ADDRESS, SET_CONFIGURATION and SET_INTERFACE
    /// change state the host kernel tracks. With another recipient or the IN
    /// direction they are malformed, and a device that decodes by bRequest
    /// alone would still obey them, so none may reach the device.
    #[test]
    fn standard_requests_with_odd_recipient_or_direction_never_reach_the_device() {
        for request in [5u8, 9, 11] {
            for request_type in (0x00u8..=0x03).chain(0x80..=0x83) {
                let (mut host, fake) = device();
                let before = fake.calls();
                // A value that is valid for the well-formed request:
                // address 1, configuration 1, alternate setting 0.
                let value = if request == 11 { 0 } else { 1 };
                let outcome = control(&mut host, setup(request_type, request, value, 0, 0));
                let case = format!("bmRequestType {request_type:#04x} bRequest {request}");
                assert!(
                    fake.submitted_payloads().is_empty(),
                    "{case} reached the device as a control URB"
                );
                let well_formed = matches!((request, request_type), (5 | 9, 0x00) | (11, 0x01));
                if well_formed {
                    assert_eq!(outcome, UsbTransfer::Ack(Vec::new()), "{case}");
                    continue;
                }
                assert_eq!(outcome, UsbTransfer::Stall, "{case}");
                let after = fake.calls();
                assert_eq!(
                    after.set_configurations, before.set_configurations,
                    "{case} changed the configuration"
                );
                assert_eq!(
                    after.set_interfaces, before.set_interfaces,
                    "{case} changed an alternate setting"
                );
            }
        }
    }

    /// Review usb.md F4: `UsbFs::control` is a synchronous transfer that
    /// holds its caller for up to its timeout, and the caller of a guest
    /// request holds the xHCI lock. A device with two configurations cannot
    /// answer "which is active" from its descriptors, so the backend must
    /// know it without asking the device.
    #[test]
    fn a_guest_request_makes_no_synchronous_control_transfer() {
        let fake = FakeUsbFs::with_two_configurations();
        let mut host = HostUsbDevice::new("usbhost-1-2", Box::new(fake.clone())).expect("device");
        // Opening may ask: it runs before the device is on a controller.
        let opened = fake.calls().controls;
        let descriptor = host.control(get_config(0), &[]);
        assert!(matches!(descriptor, UsbTransfer::Ack(_)), "{descriptor:?}");
        assert_eq!(
            fake.calls().controls - opened,
            0,
            "GET_DESCRIPTOR(Configuration) made a synchronous control transfer"
        );
        let switched = control(&mut host, set_interface(0, 1));
        assert_eq!(switched, UsbTransfer::Ack(Vec::new()));
        assert_eq!(fake.calls().set_interfaces, vec![(0, 1)]);
        assert_eq!(
            fake.calls().controls - opened,
            0,
            "SET_INTERFACE made a synchronous control transfer"
        );
    }

    /// Review usb.md F4: SETINTERFACE (like SETCONFIGURATION, CLEAR_HALT and
    /// the claims) waits in the kernel for the device. While it does, every
    /// call of the controller into the backend returns at once and makes
    /// no usbdevfs call, because that call would wait behind the first.
    #[test]
    fn a_blocked_usbdevfs_call_holds_no_caller() {
        let (mut host, fake) = device();
        // A bulk IN is in flight before the guest switches the interface.
        assert_eq!(host.transfer(0x81, &[], 64), UsbTransfer::Nak);
        let before = fake.submitted_payloads().len();
        let (entered, release) = fake.block_set_interface();
        assert_eq!(host.control(set_interface(0, 1), &[]), UsbTransfer::Nak);
        entered.recv().expect("the call reached the kernel");

        // The device does not answer. The guest and the poll go on.
        assert_eq!(host.control(set_interface(0, 1), &[]), UsbTransfer::Nak);
        assert_eq!(host.control(get_config(0), &[]), UsbTransfer::Nak);
        assert_eq!(
            host.control(setup(0x00, 5, 0x22, 0, 0), &[]),
            UsbTransfer::Ack(Vec::new()),
            "SET_ADDRESS needs no device"
        );
        assert_eq!(host.transfer(0x02, b"ping", 64), UsbTransfer::Nak);
        assert_eq!(host.transfer_iso(0x83, &[], 1024, 512), UsbTransfer::Nak);
        assert_eq!(host.speed(), UsbSpeed::High);
        assert!(!host.is_lost());
        host.endpoint_dropped(0x81);
        assert_eq!(
            fake.submitted_payloads().len(),
            before,
            "a URB was submitted"
        );
        assert_eq!(host.deferred_discards.len(), 1, "the discard must wait");
        assert!(fake.calls().set_interfaces.is_empty());

        // The device answers: the retry acknowledges, the discard is made.
        release.send(()).expect("the call still waits");
        host.settle_job(true);
        assert_eq!(
            host.control(set_interface(0, 1), &[]),
            UsbTransfer::Ack(Vec::new())
        );
        assert_eq!(fake.calls().set_interfaces, vec![(0, 1)]);
        assert!(host.deferred_discards.is_empty());
        host.reap();
        assert!(host.discarded.is_empty(), "the cancelled URB was reaped");
        assert_eq!(host.transfer(0x02, b"ping", 64), UsbTransfer::Nak);
        assert_eq!(fake.submitted_payloads(), vec![(0x02, b"ping".to_vec())]);
    }

    /// A request the guest gave up on still changes the device, and its
    /// answer is not handed to the next request.
    #[test]
    fn an_abandoned_request_records_its_effect_and_drops_its_answer() {
        let (mut host, fake) = device();
        assert_eq!(host.control(set_config(0), &[]), UsbTransfer::Nak);
        host.endpoint_dropped(0);
        host.settle_job(true);
        assert_eq!(fake.calls().set_configurations, vec![0]);
        assert!(host.claimed_interfaces.is_empty());
        assert_eq!(host.active_config, Some(0));
        // GET_STATUS goes to the device as a URB of its own.
        assert_eq!(host.control(setup(0x80, 0, 0, 0, 2), &[]), UsbTransfer::Nak);
        assert_eq!(fake.submitted_payloads().len(), 1);
    }

    /// A SET_CONFIGURATION whose claim fails leaves nothing claimed, gives
    /// the taken interfaces back to the host, and stalls.
    #[test]
    fn a_failed_reclaim_stalls_and_returns_the_interfaces() {
        let (mut host, fake) = device();
        fake.fail_claim(1);
        assert_eq!(control(&mut host, set_config(1)), UsbTransfer::Stall);
        let calls = fake.calls();
        assert_eq!(calls.claims, vec![0, 1, 0]);
        assert_eq!(calls.releases, vec![0, 1, 0]);
        assert_eq!(calls.connects, vec![0]);
        assert!(host.claimed_interfaces.is_empty());
        assert_eq!(host.transfer(0x81, &[], 64), UsbTransfer::Stall);
    }

    #[test]
    fn set_config_unconfigures_reconfigures_and_reclaims() {
        let (mut host, fake) = device();
        // The first SET_CONFIGURATION(1) matches the active configuration,
        // so no SETCONFIGURATION ioctl is issued, but the claims are retaken.
        assert_eq!(
            control(&mut host, set_config(1)),
            UsbTransfer::Ack(Vec::new())
        );
        assert_eq!(fake.calls().set_configurations, Vec::<u8>::new());
        let after = fake.calls();
        assert_eq!(after.claims, vec![0, 1, 0, 1]);
        assert_eq!(after.releases, vec![0, 1]);

        // Unconfigure: the claims go back and configuration 0 is set.
        assert_eq!(
            control(&mut host, set_config(0)),
            UsbTransfer::Ack(Vec::new())
        );
        let after = fake.calls();
        assert_eq!(after.set_configurations, vec![0]);
        assert_eq!(after.releases, vec![0, 1, 0, 1]);
        assert_eq!(after.claims, vec![0, 1, 0, 1]);

        // Reconfigure: set again and claimed again.
        assert_eq!(
            control(&mut host, set_config(1)),
            UsbTransfer::Ack(Vec::new())
        );
        let after = fake.calls();
        assert_eq!(after.set_configurations, vec![0, 1]);
        assert_eq!(after.claims.len(), 6);
    }

    #[test]
    fn set_interface_switches_alt_setting() {
        let (mut host, fake) = device();
        assert_eq!(
            control(&mut host, set_interface(0, 1)),
            UsbTransfer::Ack(Vec::new())
        );
        assert_eq!(fake.calls().set_interfaces, vec![(0, 1)]);
        // The bulk endpoints survive the alt switch.
        assert_eq!(host.transfer(0x02, b"ping", 64), UsbTransfer::Nak);
    }

    /// F13 of docs/reviews/usb.md: SET_CONFIGURATION puts every interface
    /// of the device back at alt setting 0, so the endpoint table is that
    /// of alt setting 0 and not of the alt setting the guest chose before.
    #[test]
    fn set_configuration_resets_alternate_settings() {
        let fake = FakeUsbFs::with_distinct_alt_setting();
        let mut host = HostUsbDevice::new("usbhost-1-2", Box::new(fake.clone())).expect("device");
        assert_eq!(
            control(&mut host, set_interface(0, 1)),
            UsbTransfer::Ack(Vec::new())
        );
        // Alt setting 1 has endpoint 0x05 and lacks 0x02.
        assert_eq!(host.transfer(0x02, b"ping", 64), UsbTransfer::Stall);
        assert_eq!(host.transfer(0x05, b"ping", 64), UsbTransfer::Nak);

        assert_eq!(
            control(&mut host, set_config(1)),
            UsbTransfer::Ack(Vec::new())
        );
        // The device was told, or its interface is still at alt setting 1.
        assert_eq!(fake.calls().set_configurations, vec![1]);
        assert!(host.alt_settings.is_empty());
        assert_eq!(
            host.transfer(0x05, b"ping", 64),
            UsbTransfer::Stall,
            "an endpoint only alt setting 1 has"
        );
        assert_eq!(
            host.transfer(0x02, b"ping", 64),
            UsbTransfer::Nak,
            "an endpoint only alt setting 0 has"
        );
        assert_eq!(
            fake.submitted_payloads().last(),
            Some(&(0x02, b"ping".to_vec()))
        );
    }

    /// F8 of docs/reviews/usb.md, the backend's half: the control transfer
    /// is kept under endpoint 0, which is what the controller reports when
    /// the guest stops the control endpoint. The old URB's answer, already
    /// complete, does not answer the next, different request.
    #[test]
    fn a_dropped_control_transfer_does_not_answer_the_next_request() {
        let (mut host, fake) = device();
        // GET_STATUS, completed by the device but not collected.
        fake.complete_with(Completion::Data(b"old".to_vec()));
        assert_eq!(host.control(setup(0x80, 0, 0, 0, 2), &[]), UsbTransfer::Nak);
        fake.run_irq();
        host.endpoint_dropped(0);
        // GET_DESCRIPTOR(String) is a URB of its own.
        assert_eq!(
            host.control(setup(0x80, 6, 3 << 8, 0, 255), &[]),
            UsbTransfer::Nak
        );
        let in_flight = fake.submitted_payloads();
        assert_eq!(in_flight.len(), 1, "the old URB was reaped and dropped");
        assert_eq!(in_flight[0].1[..8], [0x80, 6, 0, 3, 0, 0, 255, 0]);
    }

    #[test]
    fn clear_feature_halt_clears_the_endpoint() {
        let (mut host, fake) = device();
        assert_eq!(
            control(&mut host, clear_halt(0x81)),
            UsbTransfer::Ack(Vec::new())
        );
        assert_eq!(fake.calls().cleared_halts, vec![0x81]);
    }

    #[test]
    fn bulk_out_naks_then_acknowledges() {
        let (mut host, fake) = device();
        // Submission, then the device's IRQ, then the retry.
        assert_eq!(host.transfer(0x02, b"payload", 64), UsbTransfer::Nak);
        assert_eq!(fake.submitted_payloads(), vec![(0x02, b"payload".to_vec())]);
        fake.run_irq();
        assert_eq!(
            host.transfer(0x02, b"payload", 64),
            UsbTransfer::Ack(Vec::new())
        );
    }

    #[test]
    fn bulk_in_naks_then_delivers_data() {
        let (mut host, fake) = device();
        fake.complete_with(Completion::Data(b"hello".to_vec()));
        assert_eq!(host.transfer(0x81, &[], 64), UsbTransfer::Nak);
        fake.run_irq();
        assert_eq!(
            host.transfer(0x81, &[], 64),
            UsbTransfer::Ack(b"hello".to_vec())
        );
    }

    #[test]
    fn isochronous_in_is_packetized() {
        let (mut host, fake) = device();
        let payload = vec![0xa5u8; 1024];
        fake.complete_with(Completion::Data(payload.clone()));
        assert_eq!(host.transfer_iso(0x83, &[], 1024, 512), UsbTransfer::Nak);
        fake.run_irq();
        assert_eq!(
            host.transfer_iso(0x83, &[], 1024, 512),
            UsbTransfer::Ack(payload)
        );
    }

    /// Review usb-2.md F18: one isochronous TD of xHCI is the data of one
    /// service interval, and one usbdevfs packet descriptor is one service
    /// interval. A high-bandwidth endpoint sends three packets of 1024 bytes
    /// in it, so the TD of 3072 bytes is one descriptor, not three.
    #[test]
    fn one_isochronous_td_is_one_usbdevfs_packet() {
        let fake = FakeUsbFs::with_high_bandwidth_iso();
        let mut host = HostUsbDevice::new("usbhost-1-2", Box::new(fake.clone())).expect("device");
        assert_eq!(host.transfer_iso(0x83, &[], 3072, 1024), UsbTransfer::Nak);
        assert_eq!(fake.submitted_iso_packets(), [(0x83, vec![3072])]);

        // The three packets of the interval come back as one payload.
        let payload: Vec<u8> = (0..3072u32).map(|n| n as u8).collect();
        fake.complete_with(Completion::Data(payload.clone()));
        fake.run_irq();
        assert_eq!(
            host.transfer_iso(0x83, &[], 3072, 1024),
            UsbTransfer::Ack(payload)
        );
    }

    #[test]
    fn stalled_completion_halts() {
        let (mut host, fake) = device();
        fake.complete_with(Completion::Stall);
        assert_eq!(host.transfer(0x81, &[], 64), UsbTransfer::Nak);
        fake.run_irq();
        assert_eq!(host.transfer(0x81, &[], 64), UsbTransfer::Stall);
    }

    #[test]
    fn unknown_endpoint_stalls() {
        let (mut host, _fake) = device();
        assert_eq!(host.transfer(0x04, b"x", 8), UsbTransfer::Stall);
    }

    #[test]
    fn unplug_marks_the_device_lost() {
        let (mut host, fake) = device();
        assert_eq!(host.transfer(0x81, &[], 64), UsbTransfer::Nak);
        fake.unplug();
        assert!(!host.is_lost());
        // The retry reaps the -ENODEV completions and stalls.
        assert_eq!(host.transfer(0x81, &[], 64), UsbTransfer::Stall);
        assert!(host.is_lost());
    }

    #[test]
    fn dropped_endpoint_forgets_in_flight_work() {
        let (mut host, fake) = device();
        // A transfer is in flight when the guest stops the endpoint.
        assert_eq!(host.transfer(0x81, &[], 64), UsbTransfer::Nak);
        // The guest stops the endpoint and re-arms it; the in-flight URB is
        // discarded, so its cancelled completion is never delivered.
        host.endpoint_dropped(0x81);
        fake.run_irq();
        assert_eq!(host.transfer(0x81, &[], 64), UsbTransfer::Nak);
        fake.complete_with(Completion::Data(b"fresh".to_vec()));
        fake.run_irq();
        assert_eq!(
            host.transfer(0x81, &[], 64),
            UsbTransfer::Ack(b"fresh".to_vec())
        );
    }

    /// F9 of docs/reviews/usb.md: a transfer in flight when the guest sets
    /// a configuration belongs to the old one, and must not answer the
    /// first transfer on the new configuration's endpoint.
    #[test]
    fn set_config_forgets_in_flight_work() {
        let (mut host, fake) = device();
        assert_eq!(host.transfer(0x81, &[], 64), UsbTransfer::Nak);
        assert_eq!(
            control(&mut host, set_config(1)),
            UsbTransfer::Ack(Vec::new())
        );
        fake.run_irq();
        // A fresh submission, not the old URB's result.
        assert_eq!(host.transfer(0x81, &[], 64), UsbTransfer::Nak);
        fake.complete_with(Completion::Data(b"fresh".to_vec()));
        fake.run_irq();
        assert_eq!(
            host.transfer(0x81, &[], 64),
            UsbTransfer::Ack(b"fresh".to_vec())
        );
        assert!(host.discarded.is_empty());
    }

    /// F9: the same for a device reset, with a completion nobody collected
    /// on a second endpoint.
    #[test]
    fn reset_forgets_in_flight_and_uncollected_work() {
        let (mut host, fake) = device();
        assert_eq!(host.transfer(0x02, b"old", 64), UsbTransfer::Nak);
        fake.run_irq();
        host.reap();
        assert_eq!(host.transfer(0x81, &[], 64), UsbTransfer::Nak);
        host.reset();
        fake.run_irq();
        assert_eq!(host.transfer(0x81, &[], 64), UsbTransfer::Nak);
        assert_eq!(host.transfer(0x02, b"new", 64), UsbTransfer::Nak);
        assert!(host.discarded.is_empty());
    }

    #[test]
    fn snapshots_are_refused() {
        let (mut host, _fake) = device();
        assert!(host.save_state().is_err());
        assert!(host.restore_state(&[]).is_err());
    }

    #[test]
    fn repeated_endpoint_drops_are_bounded() {
        let (mut host, fake) = device();
        for _ in 0..10_000 {
            host.endpoint_dropped(0x81);
        }
        assert!(host.discarded.is_empty());
        assert_eq!(host.transfer(0x81, &[], 64), UsbTransfer::Nak);
        for _ in 0..10_000 {
            host.endpoint_dropped(0x81);
        }
        assert_eq!(host.discarded.len(), 1);
        fake.run_irq();
        host.reap();
        assert!(host.discarded.is_empty());
    }
}
