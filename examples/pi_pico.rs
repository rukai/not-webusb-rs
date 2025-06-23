#![no_std]
#![no_main]

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
                    info!("received request {:?}", request);
                    let response = match request.ty {
                        FidoRequestTy::Ping => Some(FidoResponseTy::RawReport(report)),
                        FidoRequestTy::Message { length, data } => {
                            if in_progress_message_option.is_some() {
                                error!(
                                    "Cannot create new transaction while existing transaction is in progress"
                                )
                                // TODO: handle error
                            }

                            in_progress_message_option = Some(InProgressMessage {
                                cid: request.cid,
                                buffer: [0; MAXIMUM_CTAPHID_MESSAGE],
                                current_payload_size: length as usize,
                                current_payload_bytes_written: 0,
                                packet_number_in_sequence: 0,
                            });
                            if let Some(in_progress_message) = &mut in_progress_message_option {
                                in_progress_message.write_data(&data, &mut tx);
                            }
                            None
                        }
                        FidoRequestTy::Continuation { data, .. } => {
                            if let Some(in_progress_message) = &mut in_progress_message_option {
                                if in_progress_message.cid == request.cid {
                                    in_progress_message.write_data(&data, &mut tx);
                                } else {
                                    // TODO: error or maybe just drop it
                                }
                            }
                            None
                        }
                        FidoRequestTy::Init { nonce8 } => {
                            // TODO: handle broadcast CID
                            cid_next += 1;
                            Some(FidoResponseTy::Init(InitResponse {
                                nonce_8_bytes: nonce8,
                                channel_id: cid_next.to_be_bytes(),
                                protocol_version: 2,
                                device_version_major: 0,
                                device_version_minor: 0,
                                device_version_build: 0,
                                capabilities: 0,
                            }))
                        }
                        FidoRequestTy::Unknown { cmd } => {
                            // TODO: handle error
                            panic!("Unknown command {}", cmd);
                        }
                    };

                    if let Some(response) = response {
                        FidoResponse {
                            cid: request.cid,
                            ty: response,
                            packet_number_in_sequence: 0,
                        }
                        .encode(&mut raw_response);
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
            if let Ok(granted) = rx.read() {
                info!("reading in_progress_message");
                let packet_size = if in_progress_message.packet_number_in_sequence == 0 {
                    granted.len().min(57)
                } else {
                    granted.len().min(59)
                };
                FidoResponse {
                    cid: in_progress_message.cid,
                    ty: FidoResponseTy::Message {
                        length: packet_size as u16,
                        data: &granted[..packet_size],
                    },
                    packet_number_in_sequence: in_progress_message.packet_number_in_sequence,
                }
                .encode(&mut raw_response);
                in_progress_message.packet_number_in_sequence += 1;

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
        }
    }
}

struct InProgressMessage {
    cid: u32,
    buffer: [u8; MAXIMUM_CTAPHID_MESSAGE],
    current_payload_size: usize,
    current_payload_bytes_written: usize,
    /// Starts at 0, increments for every packet sent in a sequence.
    packet_number_in_sequence: u8,
}

impl InProgressMessage {
    /// Returns true if the request has finished parsing and the response was sent
    fn write_data(&mut self, data: &[u8], tx: &mut Producer<MAXIMUM_CTAPHID_MESSAGE_X2>) {
        info!("write_data");
        self.buffer
            [self.current_payload_bytes_written..self.current_payload_bytes_written + data.len()]
            .copy_from_slice(data);

        // if we have completely received the request, respond to it.
        self.current_payload_bytes_written += data.len();
        if self.current_payload_bytes_written >= self.current_payload_size {
            respond_to_message(&self.buffer[..self.current_payload_size], tx);
        }
    }
}

fn respond_to_message(message_data: &[u8], tx: &mut Producer<MAXIMUM_CTAPHID_MESSAGE_X2>) {
    info!("decoding");
    let request = MessageRequest::decode(message_data);
    info!("received request {:?}", request);

    let response = match request {
        MessageRequest::Register { .. } => MessageResponse::Register {
            // TODO: set real values
            user_public_key: [0; 65],
            key_handle_length: 0,
            key_handle: [0; 255],
            attestation_certificate: [0; 255],
            signature: [0; 73],
        },
        MessageRequest::Authenticate { .. } => {
            let signature = [
                0x30, 0x44, // ASN.1 sequence
                0x02, 0x20, // ASN.1 integer
                0x7f, // make sure not all zero
                0, 0, 0, 0, // TODO
                0, // TODO
                0x02, 0x20, // ASN.1 integer
                0x7F, // make sure not all zero
            ]
            .into_iter()
            .collect();
            MessageResponse::Authenticate {
                user_presence: true,
                counter: 0,
                signature,
            }
        }
        MessageRequest::Version => MessageResponse::Version,
        MessageRequest::Unknown { cla, ins } => {
            panic!("unknown message request cla={} ins={}", cla, ins);
            // TODO: error handling
        }
    };

    let mut granted = tx.grant_exact(MAXIMUM_CTAPHID_MESSAGE).unwrap();
    let size = response.encode(&mut granted);
    granted.commit(size);
}

fn enter_flash_mode() -> ! {
    info!("entering flash mode");
    reset_to_usb_boot(0, 0);
    panic!()
}

fn parse_request(report: &RawFidoReport) -> FidoRequest {
    let packet = &report.packet;
    let cid = u32::from_be_bytes(packet[0..4].try_into().unwrap());
    let ty = if packet[4] & 0b10000000 == 0 {
        FidoRequestTy::Continuation {
            sequence: packet[4],
            data: packet[5..].try_into().unwrap(),
        }
    } else {
        let bcnt: u16 = u16::from_be_bytes(packet[5..7].try_into().unwrap());
        let cmd = packet[4] & 0b01111111;
        match cmd {
            0x01 => FidoRequestTy::Ping,
            0x03 => FidoRequestTy::Message {
                length: bcnt,
                data: packet[7..].try_into().unwrap(),
            },
            0x06 => FidoRequestTy::Init {
                nonce8: packet[7..15].try_into().unwrap(),
            },
            cmd => FidoRequestTy::Unknown { cmd },
        }
    };

    FidoRequest { cid, ty }
}

#[derive(Format)]
struct FidoRequest {
    cid: u32,
    ty: FidoRequestTy,
}

#[derive(Format)]
pub enum FidoRequestTy {
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

pub struct FidoResponse<'a> {
    pub cid: u32,
    /// Starts at 0, increments for every packet sent in a sequence.
    pub packet_number_in_sequence: u8,
    pub ty: FidoResponseTy<'a>,
}

pub enum FidoResponseTy<'a> {
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

impl FidoResponse<'_> {
    fn encode(&self, report: &mut RawFidoReport) {
        info!("FidoResponse::encode");
        match &self.ty {
            FidoResponseTy::Init(response) => {
                HeaderInitialization {
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
            FidoResponseTy::Message { length, data } => {
                if self.packet_number_in_sequence == 0 {
                    HeaderInitialization {
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
                } else {
                    HeaderContinuation {
                        cid: self.cid,
                        seq: self.packet_number_in_sequence,
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
            }
            FidoResponseTy::RawReport(raw) => *report = *raw,
        }
    }
}

pub struct HeaderInitialization {
    /// The channel identifier
    pub cid: u32,
    /// The command identifier
    pub cmd: u8,
    /// The payload length
    pub bcnt: u16,
}

impl HeaderInitialization {
    fn encode(self, report: &mut RawFidoReport) {
        report.packet[0..4].copy_from_slice(&self.cid.to_be_bytes());
        report.packet[4] = self.cmd;
        report.packet[5..7].copy_from_slice(&self.bcnt.to_be_bytes());
    }
}

pub struct HeaderContinuation {
    /// The channel identifier
    pub cid: u32,
    /// The packet sequence
    pub seq: u8,
}

impl HeaderContinuation {
    fn encode(self, report: &mut RawFidoReport) {
        report.packet[0..4].copy_from_slice(&self.cid.to_be_bytes());
        report.packet[4] = self.seq;
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(defmt::Format)]
pub enum MessageRequest {
    Register {
        challenge_parameter: [u8; 32],
        application_parameter: [u8; 32],
    },
    Authenticate {
        control: AuthenticateControl,
        challenge_parameter: [u8; 32],
        application_parameter: [u8; 32],
        key_handle_length: u8,
        key_handle: [u8; 255],
    },
    Version,
    Unknown {
        cla: u8,
        ins: u8,
    },
}

impl MessageRequest {
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
            0x01 => MessageRequest::Register {
                challenge_parameter: body[0..32].try_into().unwrap(),
                application_parameter: body[32..64].try_into().unwrap(),
            },
            0x02 => MessageRequest::Authenticate {
                control: AuthenticateControl::decode(p1),
                // TODO: parse
                challenge_parameter: Default::default(),
                application_parameter: Default::default(),
                key_handle_length: 255, //TODO
                key_handle: [0; 255],
            },
            0x03 => MessageRequest::Version,
            _ => MessageRequest::Unknown { cla, ins },
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
pub enum MessageResponse {
    Register {
        user_public_key: [u8; 65],
        key_handle_length: u8,
        key_handle: [u8; 255],
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

impl MessageResponse {
    /// returns the amount of bytes written
    fn encode(&self, data: &mut [u8]) -> usize {
        info!("Sending response");
        match self {
            MessageResponse::Register {
                user_public_key,
                key_handle_length,
                key_handle,
                attestation_certificate,
                signature,
            } => {
                data[0] = 5;
                data[1..66].copy_from_slice(user_public_key);
                data[66] = *key_handle_length; // TODO: shift along based on length
                data[67..322].copy_from_slice(key_handle);
                data[322..577].copy_from_slice(attestation_certificate);
                data[577..650].copy_from_slice(signature);

                // success
                data[651] = 0x90;
                data[652] = 0x00;

                // TODO: dynamically derive
                653
            }
            MessageResponse::Authenticate {
                user_presence,
                counter,
                signature,
            } => {
                data[0] = if *user_presence { 1 } else { 0 };
                data[1..5].copy_from_slice(&counter.to_be_bytes());
                data[1..5].copy_from_slice(&counter.to_be_bytes());
                data[1..5].copy_from_slice(&counter.to_be_bytes());

                let status_codes_offset = 5 + signature.len();
                data[5..status_codes_offset].copy_from_slice(signature);

                // success
                data[status_codes_offset] = 0x90;
                data[status_codes_offset + 1] = 0x00;

                status_codes_offset + 2
            }
            MessageResponse::Error(_) => {
                panic!("TODO: Implement encoding for MesageResponse error")
            }
            MessageResponse::Version => {
                data[..6].copy_from_slice("U2F_V2".as_bytes());

                // success
                data[7] = 0x90;
                data[8] = 0x00;

                8
            }
        }
    }
}

pub enum MessageResponseError {
    /// The request was rejected due to test-of-user-presence being required.
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
