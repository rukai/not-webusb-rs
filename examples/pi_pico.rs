#![no_std]
#![no_main]

use core::iter;

use arrayvec::ArrayVec;
use bbqueue::{BBBuffer, Producer};
use bsp::entry;
use bsp::hal::{
    clocks::{Clock, init_clocks_and_plls},
    pac,
    sio::Sio,
    watchdog::Watchdog,
};
use cortex_m::prelude::*;
use defmt::panic;
use defmt::*;
use defmt_rtt as _;
use embedded_hal::digital::{InputPin, OutputPin};
use fugit::ExtU32;
use panic_probe as _;
use rp_pico as bsp;
use rp2040_hal::{Timer, rom_data::reset_to_usb_boot};
use usb_device::{
    UsbError,
    bus::UsbBusAllocator,
    device::{StringDescriptors, UsbDeviceBuilder, UsbVidPid},
};
use usbd_human_interface_device::device::fido::{RawFidoConfig, RawFidoReport};
use usbd_human_interface_device::prelude::*;

// as per FIDO CTAP spec maximum payload size is 7609 bytes
const MAXIMUM_CTAPHID_MESSAGE: usize = 7609;
const MAXIMUM_CTAPHID_MESSAGE_X2: usize = MAXIMUM_CTAPHID_MESSAGE * 2;

// Only contains data for one message at a time.
// The reader can determine the total length of the message as the initial size of the buffer before it is partially sent.
// Needs the double the number of ctaphid message max bytes since the bytes might be marked as used.
// TODO: consider a better type than BBBuffer for this purpose.
static OUTGOING_MESSAGE_BYTES: BBBuffer<MAXIMUM_CTAPHID_MESSAGE_X2> = BBBuffer::new();

#[entry]
fn main() -> ! {
    let mut pac = pac::Peripherals::take().unwrap();
    let core = pac::CorePeripherals::take().unwrap();
    let mut watchdog = Watchdog::new(pac.WATCHDOG);
    let sio = Sio::new(pac.SIO);

    // External high-speed crystal on the pico board is 12Mhz
    let clocks = init_clocks_and_plls(
        bsp::XOSC_CRYSTAL_FREQ,
        pac.XOSC,
        pac.CLOCKS,
        pac.PLL_SYS,
        pac.PLL_USB,
        &mut pac.RESETS,
        &mut watchdog,
    )
    .ok()
    .unwrap();

    let timer = Timer::new(pac.TIMER, &mut pac.RESETS, &clocks);

    let mut delay = cortex_m::delay::Delay::new(core.SYST, clocks.system_clock.freq().to_Hz());

    let pins = bsp::Pins::new(
        pac.IO_BANK0,
        pac.PADS_BANK0,
        sio.gpio_bank0,
        &mut pac.RESETS,
    );

    let usb_bus = UsbBusAllocator::new(bsp::hal::usb::UsbBus::new(
        pac.USBCTRL_REGS,
        pac.USBCTRL_DPRAM,
        clocks.usb_clock,
        true,
        &mut pac.RESETS,
    ));

    let mut fido = UsbHidClassBuilder::new()
        .add_device(RawFidoConfig::default())
        .build(&usb_bus);

    let mut usb_dev = UsbDeviceBuilder::new(&usb_bus, UsbVidPid(0x1209, 0x0001))
        .strings(&[StringDescriptors::default()
            .manufacturer("not-webusb")
            .product("not-webusb demo")
            .serial_number("TEST")])
        .unwrap()
        .build();

    let mut led_pin = pins.led.into_push_pull_output();
    let mut enter_flash_mode_pin = pins.gpio2.into_pull_up_input();

    for _ in 0..4 {
        led_pin.set_high().unwrap();
        delay.delay_ms(500);
        led_pin.set_low().unwrap();
        delay.delay_ms(500);
    }

    let mut tick_count_down = timer.count_down();
    tick_count_down.start(1.millis());

    let mut flash_led = timer.count_down();
    flash_led.start(100.millis());
    let mut led_state = false;

    let mut raw_response = RawFidoReport::default();
    let mut cid_next: u32 = 1;

    let (mut tx, mut rx) = OUTGOING_MESSAGE_BYTES.try_split().unwrap();

    let mut in_progress_message_option: Option<InProgressMessage> = None;
    info!("begin main loop");
    loop {
        if flash_led.wait().is_ok() {
            led_state = !led_state;
            led_pin.set_state(led_state.into()).unwrap();
        }

        if enter_flash_mode_pin.is_low().unwrap_or(true) {
            enter_flash_mode();
        }

        if usb_dev.poll(&mut [&mut fido]) {
            match fido.device().read_report() {
                Err(UsbError::WouldBlock) => {
                    //do nothing
                }
                Err(e) => {
                    panic!("Failed to read fido report: {:?}", e)
                }
                Ok(report) => {
                    let request = parse_request(&report);
                    info!("received ctaphid request {:?}", request);
                    let response = match request.ty {
                        CtapHidRequestTy::Ping => Some(CtapHidResponseTy::RawReport(report)),
                        CtapHidRequestTy::Message { length, data } => {
                            if in_progress_message_option.is_some() {
                                error!(
                                    "Cannot create new transaction while existing transaction is in progress"
                                )
                                // TODO: handle error
                            }

                            in_progress_message_option = Some(InProgressMessage {
                                cid: request.cid,
                                request_buffer: [0; MAXIMUM_CTAPHID_MESSAGE],
                                current_request_payload_size: length as usize,
                                current_request_payload_bytes_written: 0,
                                response_continuation_state: ContinuationState::Initial,
                            });
                            if let Some(in_progress_message) = &mut in_progress_message_option {
                                in_progress_message.write_data(&data, &mut tx);
                            }
                            None
                        }
                        CtapHidRequestTy::Continuation { data, .. } => {
                            if let Some(in_progress_message) = &mut in_progress_message_option {
                                if in_progress_message.cid == request.cid {
                                    in_progress_message.write_data(&data, &mut tx);
                                } else {
                                    // TODO: error or maybe just drop it
                                }
                            }
                            None
                        }
                        CtapHidRequestTy::Init { nonce8 } => {
                            // TODO: handle broadcast CID
                            cid_next += 1;
                            Some(CtapHidResponseTy::Init(InitResponse {
                                nonce_8_bytes: nonce8,
                                channel_id: cid_next.to_be_bytes(),
                                protocol_version: 2,
                                device_version_major: 0,
                                device_version_minor: 0,
                                device_version_build: 0,
                                capabilities: 0,
                            }))
                        }
                        CtapHidRequestTy::Unknown { cmd } => {
                            // TODO: handle error
                            panic!("Unknown command {}", cmd);
                        }
                    };

                    if let Some(response) = response {
                        CtapHidResponse {
                            cid: request.cid,
                            ty: response,
                            continuation_state: ContinuationState::Initial,
                        }
                        .encode(&mut raw_response);
                        info!("sending direct raw response {}", raw_response.packet);
                        match fido.device().write_report(&raw_response) {
                            Err(UsbHidError::WouldBlock) => {}
                            Err(UsbHidError::Duplicate) => {}
                            Ok(_) => {}
                            Err(e) => {
                                panic!("Failed to write fido report: {:?}", e)
                            }
                        }
                    }
                }
            }
        }

        if let Some(in_progress_message) = &mut in_progress_message_option {
            match rx.read() {
                Ok(granted) => {
                    info!("initial granted.len() {}", granted.len());
                    info!("reading in_progress_message");
                    let packet_size = if let ContinuationState::Initial =
                        in_progress_message.response_continuation_state
                    {
                        granted.len().min(57)
                    } else {
                        granted.len().min(59)
                    };
                    info!("packet_size {}", packet_size);
                    CtapHidResponse {
                        cid: in_progress_message.cid,
                        ty: CtapHidResponseTy::Message {
                            length: packet_size as u16,
                            data: &granted[..packet_size],
                        },
                        continuation_state: in_progress_message.response_continuation_state,
                    }
                    .encode(&mut raw_response);
                    info!("sending prepared raw response {}", raw_response.packet);

                    // step sequence state
                    match &mut in_progress_message.response_continuation_state {
                        ContinuationState::Continuation { sequence } => {
                            *sequence += 1;
                        }
                        ContinuationState::Initial => {
                            in_progress_message.response_continuation_state =
                                ContinuationState::Continuation { sequence: 0 }
                        }
                    }

                    if granted.len() == packet_size {
                        // finished!!!
                        info!("finished writing response");
                        in_progress_message_option = None;
                    }
                    granted.release(packet_size);
                    match fido.device().write_report(&raw_response) {
                        Err(UsbHidError::WouldBlock) => {}
                        Err(UsbHidError::Duplicate) => {}
                        Ok(_) => {}
                        Err(e) => {
                            panic!("Failed to write fido report: {:?}", e)
                        }
                    }
                }
                Err(bbqueue::Error::InsufficientSize) => {
                    // This is expected when there are no bytes to read.
                }
                Err(error) => panic!("Unexpected bbq error {}", error),
            }
        }
    }
}

struct InProgressMessage {
    cid: u32,
    request_buffer: [u8; MAXIMUM_CTAPHID_MESSAGE],
    current_request_payload_size: usize,
    current_request_payload_bytes_written: usize,
    response_continuation_state: ContinuationState,
}

#[derive(Clone, Copy)]
pub enum ContinuationState {
    Initial,
    Continuation { sequence: u8 },
}

impl InProgressMessage {
    /// Returns true if the request has finished parsing and the response was sent
    fn write_data(&mut self, data: &[u8], tx: &mut Producer<MAXIMUM_CTAPHID_MESSAGE_X2>) {
        info!("write_data");
        self.request_buffer[self.current_request_payload_bytes_written
            ..self.current_request_payload_bytes_written + data.len()]
            .copy_from_slice(data);

        // if we have completely received the request, respond to it.
        self.current_request_payload_bytes_written += data.len();
        if self.current_request_payload_bytes_written >= self.current_request_payload_size {
            respond_to_message(
                &self.request_buffer[..self.current_request_payload_size],
                tx,
            );
        }
    }
}

fn respond_to_message(message_data: &[u8], tx: &mut Producer<MAXIMUM_CTAPHID_MESSAGE_X2>) {
    let request = U2fRequest::decode(message_data);

    //info!("received u2f request {:?}", request); // TODO: ArrayVec defmt support?
    match &request {
        U2fRequest::Register {
            challenge_parameter,
            application_parameter,
        } => info!(
            "received u2f request: register challenge_parameter={} application_parameter={}",
            challenge_parameter, application_parameter
        ),
        U2fRequest::Authenticate {
            control,
            challenge_parameter,
            application_parameter,
            key_handle,
        } => info!(
            "received u2f request: authenticate control={} challenge_parameter={} application_parameter={} key_handle={}",
            control,
            challenge_parameter,
            application_parameter,
            key_handle.as_slice()
        ),
        U2fRequest::Version => info!("received u2f request: version"),
        U2fRequest::Unknown { cla, ins } => {
            info!("received u2f request: unknown cla={} ins={}", cla, ins)
        }
    }

    let response = match request {
        U2fRequest::Register { .. } => U2fResponse::Register {
            // TODO: set real values
            user_public_key: [0; 65],
            key_handle: ArrayVec::new(),
            attestation_certificate: [0; 255],
            signature: [0; 73],
        },
        U2fRequest::Authenticate {
            key_handle,
            control,
            ..
        } => {
            if let AuthenticateControl::CheckOnly = control {
                // Actually indicates success.
                U2fResponse::Error(MessageResponseError::ConditionsNotSatisfied)
            } else {
                // TODO: pull this logic out
                let response: ArrayVec<u8, 255> = key_handle
                    .into_iter()
                    .map(|x| {
                        // apply rot13
                        if ('A'..'N').contains(&(x as char)) {
                            x + 13
                        } else if ('N'..='Z').contains(&(x as char)) {
                            x - 13
                        } else if ('a'..'n').contains(&(x as char)) {
                            x + 13
                        } else if ('n'..='z').contains(&(x as char)) {
                            x - 13
                        } else {
                            x
                        }
                    })
                    .collect();

                // The signature contains two ASN.1 integers that we can smuggle data in.
                // They must be exactly 20 bytes each and must never be > 0, since they are signed integers this means starting with 0x7f

                let mut payload_written_bytes = 0;

                let mut signature: ArrayVec<u8, 255> = [
                    0x30, // ASN.1 sequence
                    0x44, // Number of bytes in ASN.1 sequence
                    0x02, // ASN.1 integer
                    0x20, // Number of bytes in integer
                    0x7f, // first byte of 0x7f is used to force the signed integer to be positive for chrome compatibility
                ]
                .into_iter()
                .collect();

                // must write exactly 0x1f bytes to signature
                let payload_bytes_to_write = (response.len() - payload_written_bytes).min(0x1f);
                signature.extend(
                    response[payload_written_bytes..payload_written_bytes + payload_bytes_to_write]
                        .iter()
                        .copied()
                        .chain(iter::repeat(0))
                        .take(0x1f),
                );
                payload_written_bytes += payload_bytes_to_write;

                signature.extend([
                    0x02, // ASN.1 integer
                    0x20, // Number of bytes in integer
                    0x7f, // first byte of 0x7f is used to force the signed integer to be positive for chrome compatibility
                ]);

                let payload_bytes_to_write = (response.len() - payload_written_bytes).min(0x1f);
                // must write exactly 0x1f bytes to signature
                signature.extend(
                    response[payload_written_bytes..payload_written_bytes + payload_bytes_to_write]
                        .iter()
                        .copied()
                        .chain(iter::repeat(0))
                        .take(0xc), // TODO: increasing this beyond 0xB makes the whole thing explode
                );
                payload_written_bytes += payload_bytes_to_write;

                info!("payload_written_bytes {}", payload_written_bytes);
                info!("signature {}", signature.as_slice());

                U2fResponse::Authenticate {
                    user_presence: true,
                    counter: 0,
                    signature,
                }
            }
        }
        U2fRequest::Version => U2fResponse::Version,
        U2fRequest::Unknown { cla, ins } => {
            panic!("unknown message request cla={} ins={}", cla, ins);
            // TODO: error handling
        }
    };

    let mut granted = tx.grant_exact(MAXIMUM_CTAPHID_MESSAGE).unwrap();
    let size = response.encode(&mut granted);
    info!("wrote {} bytes to outgoing response", size);
    granted.commit(size);
}

fn enter_flash_mode() -> ! {
    info!("entering flash mode");
    reset_to_usb_boot(0, 0);
    panic!()
}

fn parse_request(report: &RawFidoReport) -> CtapHidRequest {
    let packet = &report.packet;
    let cid = u32::from_be_bytes(packet[0..4].try_into().unwrap());
    let ty = if packet[4] & 0b10000000 == 0 {
        CtapHidRequestTy::Continuation {
            sequence: packet[4],
            data: packet[5..].try_into().unwrap(),
        }
    } else {
        let bcnt: u16 = u16::from_be_bytes(packet[5..7].try_into().unwrap());
        let cmd = packet[4] & 0b01111111;
        match cmd {
            0x01 => CtapHidRequestTy::Ping,
            0x03 => CtapHidRequestTy::Message {
                length: bcnt,
                data: packet[7..].try_into().unwrap(),
            },
            0x06 => CtapHidRequestTy::Init {
                nonce8: packet[7..15].try_into().unwrap(),
            },
            cmd => CtapHidRequestTy::Unknown { cmd },
        }
    };

    CtapHidRequest { cid, ty }
}

#[derive(Format)]
struct CtapHidRequest {
    cid: u32,
    ty: CtapHidRequestTy,
}

#[derive(Format)]
pub enum CtapHidRequestTy {
    /// Initialize
    Init {
        /// 8-byte nonce
        nonce8: [u8; 8],
    },
    /// Send the entire raw request back as is.
    Ping,
    Message {
        /// Full length of the payload, possibly this packet and one or more continuation packets.
        length: u16,
        /// packet contents.
        /// since header is 7 bytes long and packet is max 64 bytes this is max 57 bytes
        data: [u8; 57],
    },
    /// A continuation packet.
    /// In theory this could be used for any command, in reality only Message is long enough to need it.
    Continuation {
        sequence: u8,
        /// packet contents.
        /// since continuation header is 5 bytes long and packet is max 64 bytes this is max 59 bytes
        data: [u8; 59],
    },
    Unknown {
        /// The unknown command ID
        cmd: u8,
    },
}

pub struct CtapHidResponse<'a> {
    pub cid: u32,
    pub continuation_state: ContinuationState,
    pub ty: CtapHidResponseTy<'a>,
}

pub enum CtapHidResponseTy<'a> {
    /// Initialize
    Init(InitResponse),
    Message {
        /// Full length of the payload, possibly this packet and one or more continuation packets.
        length: u16,
        /// packet contents.
        /// since header is 7 bytes long and packet is max 64 bytes this is max 57 bytes
        data: &'a [u8],
    },
    /// Use this to provide a response to a Ping or if you need to construct a custom response for any reason.
    RawReport(RawFidoReport),
}

pub struct InitResponse {
    /// 8-byte nonce
    pub nonce_8_bytes: [u8; 8],
    /// channel ID (CID)
    pub channel_id: [u8; 4],
    /// CTAPHID protocol version identifier
    pub protocol_version: u8,
    pub device_version_major: u8,
    pub device_version_minor: u8,
    pub device_version_build: u8,
    pub capabilities: u8,
}

impl CtapHidResponse<'_> {
    fn encode(&self, report: &mut RawFidoReport) {
        // Not technically needed but makes it easier to debug outgoing packets.
        report.packet.fill(0);

        info!("FidoResponse::encode");
        match &self.ty {
            CtapHidResponseTy::Init(response) => {
                CtapHeaderInitialization {
                    cid: self.cid,
                    cmd: 0x86,
                    bcnt: 17,
                }
                .encode(report);

                let data = &mut report.packet[7..];
                data[0..8].copy_from_slice(&response.nonce_8_bytes);
                data[8..12].copy_from_slice(&response.channel_id);
                data[13] = response.protocol_version;
                data[14] = response.device_version_major;
                data[15] = response.device_version_minor;
                data[16] = response.device_version_build;
                data[17] = response.capabilities;
            }
            CtapHidResponseTy::Message { length, data } => match self.continuation_state {
                ContinuationState::Initial => {
                    info!("data!!! {}", data);
                    CtapHeaderInitialization {
                        cid: self.cid,
                        cmd: 0x83,
                        bcnt: *length,
                    }
                    .encode(report);
                    if data.len() > report.packet.len() - 7 {
                        panic!(
                            "message data is too long for one initial packet, was {} but must be less than or equal to {}",
                            data.len(),
                            report.packet.len() - 7
                        );
                    }
                    report.packet[7..7 + data.len()].copy_from_slice(data);
                }
                ContinuationState::Continuation { sequence } => {
                    CtapHeaderContinuation {
                        cid: self.cid,
                        seq: sequence,
                    }
                    .encode(report);
                    if data.len() > report.packet.len() - 5 {
                        panic!(
                            "message data is too long for one continuation packet, was {} but must be less than or equal to {}",
                            data.len(),
                            report.packet.len() - 5
                        );
                    }
                    report.packet[5..5 + data.len()].copy_from_slice(data);
                }
            },
            CtapHidResponseTy::RawReport(raw) => *report = *raw,
        }
    }
}

pub struct CtapHeaderInitialization {
    /// The channel identifier
    pub cid: u32,
    /// The command identifier
    pub cmd: u8,
    /// The payload length
    pub bcnt: u16,
}

impl CtapHeaderInitialization {
    fn encode(self, report: &mut RawFidoReport) {
        report.packet[0..4].copy_from_slice(&self.cid.to_be_bytes());
        report.packet[4] = self.cmd;
        report.packet[5..7].copy_from_slice(&self.bcnt.to_be_bytes());
    }
}

pub struct CtapHeaderContinuation {
    /// The channel identifier
    pub cid: u32,
    /// The packet sequence
    pub seq: u8,
}

impl CtapHeaderContinuation {
    fn encode(self, report: &mut RawFidoReport) {
        report.packet[0..4].copy_from_slice(&self.cid.to_be_bytes());
        report.packet[4] = self.seq;
    }
}

#[allow(clippy::large_enum_variant)]
pub enum U2fRequest {
    Register {
        challenge_parameter: [u8; 32],
        application_parameter: [u8; 32],
    },
    Authenticate {
        control: AuthenticateControl,
        challenge_parameter: [u8; 32],
        application_parameter: [u8; 32],
        key_handle: ArrayVec<u8, 255>,
    },
    Version,
    Unknown {
        cla: u8,
        ins: u8,
    },
}

impl U2fRequest {
    fn decode(message_data: &[u8]) -> Self {
        let cla = message_data[0];
        let ins = message_data[1];
        let p1 = message_data[2];
        let _p2 = message_data[3];

        let (length, data_start) = if message_data[4] == 0 {
            (
                u16::from_be_bytes(message_data[5..7].try_into().unwrap()),
                7,
            )
        } else {
            (message_data[4] as u16, 5)
        };
        let body = &message_data[data_start..data_start + length as usize];

        match ins {
            0x01 => U2fRequest::Register {
                challenge_parameter: body[0..32].try_into().unwrap(),
                application_parameter: body[32..64].try_into().unwrap(),
            },
            0x02 => {
                let key_handle_length = body[64];
                let mut key_handle = [0; 255];
                key_handle[0..key_handle_length as usize]
                    .copy_from_slice(&body[65..65 + key_handle_length as usize]);
                U2fRequest::Authenticate {
                    control: AuthenticateControl::decode(p1),
                    challenge_parameter: body[0..32].try_into().unwrap(),
                    application_parameter: body[32..64].try_into().unwrap(),
                    key_handle: ArrayVec::from_iter(
                        key_handle.iter().copied().take(key_handle_length as usize),
                    ),
                }
            }
            0x03 => U2fRequest::Version,
            _ => U2fRequest::Unknown { cla, ins },
        }
    }
}

#[derive(defmt::Format)]
pub enum AuthenticateControl {
    CheckOnly,
    EnforceUserPresenceAndSign,
    DontEnforceUserPresenceAndSign,
    Unknown(u8),
}

impl AuthenticateControl {
    fn decode(byte: u8) -> Self {
        match byte {
            0x07 => AuthenticateControl::CheckOnly,
            0x03 => AuthenticateControl::EnforceUserPresenceAndSign,
            0x08 => AuthenticateControl::DontEnforceUserPresenceAndSign,
            unknown => AuthenticateControl::Unknown(unknown),
        }
    }
}

#[allow(clippy::large_enum_variant)]
pub enum U2fResponse {
    Register {
        user_public_key: [u8; 65],
        key_handle: ArrayVec<u8, 255>,
        attestation_certificate: [u8; 255], // TODO: There seems to be no maximum length, not sure what to do here.
        signature: [u8; 73],
    },
    Authenticate {
        user_presence: bool,
        counter: u32,
        signature: ArrayVec<u8, 255>, // TODO: There seems to be no maximum length, not sure what to do here.
    },
    Error(MessageResponseError),
    Version,
}

impl U2fResponse {
    /// returns the amount of bytes written
    fn encode(&self, data: &mut [u8]) -> usize {
        info!("Sending response");
        match self {
            U2fResponse::Register {
                user_public_key,
                key_handle,
                attestation_certificate,
                signature,
            } => {
                data[0] = 5;
                data[1..66].copy_from_slice(user_public_key);
                data[66] = key_handle.len() as u8; // TODO: shift along next fields based on length
                data[67..67 + key_handle.len()].copy_from_slice(key_handle);
                data[322..577].copy_from_slice(attestation_certificate);
                data[577..650].copy_from_slice(signature);

                // success
                data[651] = 0x90;
                data[652] = 0x00;

                // TODO: dynamically derive
                653
            }
            U2fResponse::Authenticate {
                user_presence,
                counter,
                signature,
            } => {
                data[0] = if *user_presence { 1 } else { 0 };
                data[1..5].copy_from_slice(&counter.to_be_bytes());

                let status_codes_offset = 5 + signature.len();
                data[5..status_codes_offset].copy_from_slice(signature);

                // success
                data[status_codes_offset] = 0x90;
                data[status_codes_offset + 1] = 0x00;

                debug!(
                    "authenticate response raw {}",
                    &data[..status_codes_offset + 2]
                );
                status_codes_offset + 2
            }
            U2fResponse::Error(error) => {
                data[0..2].copy_from_slice(&(*error as u16).to_be_bytes());

                2
            }
            U2fResponse::Version => {
                data[..6].copy_from_slice("U2F_V2".as_bytes());

                // success
                data[7] = 0x90;
                data[8] = 0x00;

                8
            }
        }
    }
}

#[derive(Clone, Copy)]
pub enum MessageResponseError {
    /// The request was rejected due to test-of-user-presence being required.
    /// This actually indicates success when responding to check-only authenticate requests. This protocol is cursed.
    ConditionsNotSatisfied = 0x6985,
    /// The request was rejected due to an invalid key handle.
    WrongData = 0x6A80,
    /// The length of the request was invalid.
    WrongLength = 0x6700,
    /// The Class byte of the request is not supported.
    ClaNotSupported = 0x6E00,
    /// The Instruction of the request is not supported.
    InsNotSupported = 0x6D00,
}
