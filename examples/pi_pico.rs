#![no_std]
#![no_main]

use bbqueue::{BBBuffer, Producer};
use bsp::entry;
use bsp::hal::{
    clocks::{Clock, init_clocks_and_plls},
    pac,
    sio::Sio,
    watchdog::Watchdog,
};
use cortex_m::prelude::*;
use embedded_hal::digital::{InputPin, OutputPin};
use fugit::ExtU32;
use panic_halt as _;
use rp_pico as bsp;
use rp2040_hal::{Timer, rom_data::reset_to_usb_boot};
use usb_device::{
    UsbError,
    bus::UsbBusAllocator,
    device::{StringDescriptors, UsbDeviceBuilder, UsbVidPid},
};
use usbd_human_interface_device::device::fido::{RawFidoConfig, RawFidoReport};
use usbd_human_interface_device::prelude::*;

// 7609 is maximum size of a CTAPHID message
static OUTGOING_MESSAGE_BYTES: BBBuffer<7609> = BBBuffer::new();

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

    // as per FIDO CTAP spec maximum payload size is 7609 bytes
    let mut in_progress_message_option: Option<InProgressMessage> = None;
    let mut initialization_packet = true;
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
                    core::panic!("Failed to read fido report: {:?}", e)
                }
                Ok(report) => {
                    let request = parse_request(&report);
                    let response = match request.ty {
                        FidoRequestTy::Ping => Some(FidoResponseTy::RawReport(report)),
                        FidoRequestTy::Message { length, data } => {
                            if in_progress_message_option.is_some() {
                                // TODO: error due to in progress transaction
                            }

                            in_progress_message_option = Some(InProgressMessage {
                                cid: request.cid,
                                buffer: [0; 7609],
                                current_payload_size: length as usize,
                                current_payload_bytes_written: 0,
                            });
                            if let Some(in_progress_message) = &mut in_progress_message_option {
                                if in_progress_message.write_data(&data, &mut tx) {
                                    in_progress_message_option = None;
                                }
                            }
                            None
                        }
                        FidoRequestTy::Continuation { data, .. } => {
                            if let Some(in_progress_message) = &mut in_progress_message_option {
                                if in_progress_message.cid == request.cid {
                                    if in_progress_message.write_data(&data, &mut tx) {
                                        in_progress_message_option = None;
                                    }
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
                        FidoRequestTy::Unknown { .. } => enter_flash_mode(),
                    };

                    if let Some(response) = response {
                        FidoResponse {
                            cid: request.cid,
                            ty: response,
                        }
                        .encode(&mut raw_response);
                        match fido.device().write_report(&raw_response) {
                            Err(UsbHidError::WouldBlock) => {}
                            Err(UsbHidError::Duplicate) => {}
                            Ok(_) => {}
                            Err(e) => {
                                core::panic!("Failed to write fido report: {:?}", e)
                            }
                        }
                    }
                }
            }
        }

        let granted = rx.read().unwrap();
        let packet_size = if initialization_packet {
            initialization_packet = false;
            granted.len().min(57)
        } else {
            granted.len().min(59)
        };
        if packet_size > 0 {
            FidoResponse {
                cid: 0, // TODO
                ty: FidoResponseTy::Message {
                    length: packet_size as u16,
                    data: granted[..packet_size].try_into().unwrap(),
                },
            }
            .encode(&mut raw_response);
            granted.release(packet_size);

            match fido.device().write_report(&raw_response) {
                Err(UsbHidError::WouldBlock) => {}
                Err(UsbHidError::Duplicate) => {}
                Ok(_) => {}
                Err(e) => {
                    core::panic!("Failed to write fido report: {:?}", e)
                }
            }
        }
    }
}

struct InProgressMessage {
    cid: u32,
    buffer: [u8; 7609],
    current_payload_size: usize,
    current_payload_bytes_written: usize,
}

impl InProgressMessage {
    /// Returns true if the request has finished parsing and the response was sent
    fn write_data(&mut self, data: &[u8], tx: &mut Producer<7609>) -> bool {
        self.buffer
            [self.current_payload_bytes_written..self.current_payload_bytes_written + data.len()]
            .copy_from_slice(data);

        // if we have completely received the request, respond to it.
        self.current_payload_bytes_written += data.len();
        if self.current_payload_bytes_written >= self.current_payload_size {
            respond_to_message(&self.buffer[..self.current_payload_size], tx);
            true
        } else {
            false
        }
    }
}

fn respond_to_message(message_data: &[u8], tx: &mut Producer<7609>) {
    let request = MessageRequest::decode(message_data);

    let response = match request {
        MessageRequest::Register { .. } => enter_flash_mode(),
        MessageRequest::Authenticate { .. } => {
            enter_flash_mode();
        }
        MessageRequest::Version => MessageResponse::Version,
        MessageRequest::Unknown { .. } => {
            // TODO: error handling
            enter_flash_mode();
        }
    };

    let mut granted = tx.grant_exact(7609).unwrap();
    response.encode(&mut granted);
    granted.commit(7609);
}

fn enter_flash_mode() -> ! {
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

struct FidoRequest {
    cid: u32,
    ty: FidoRequestTy,
}

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

pub struct FidoResponse {
    pub cid: u32,
    pub ty: FidoResponseTy,
}

pub enum FidoResponseTy {
    /// Initialize
    Init(InitResponse),
    Message {
        /// Full length of the payload, possibly this packet and one or more continuation packets.
        length: u16,
        /// packet contents.
        /// since header is 7 bytes long and packet is max 64 bytes this is max 57 bytes
        data: [u8; 57],
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

impl FidoResponse {
    fn encode(&self, report: &mut RawFidoReport) {
        match &self.ty {
            FidoResponseTy::Init(response) => {
                Header {
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
                Header {
                    cid: self.cid,
                    cmd: 0x83,
                    bcnt: *length,
                }
                .encode(report);
                report.packet[7..].copy_from_slice(data);
            }
            FidoResponseTy::RawReport(raw) => *report = *raw,
        }
    }
}

pub struct Header {
    pub cid: u32,
    /// The command indentifier
    pub cmd: u8,
    /// The payload length
    pub bcnt: u16,
}

impl Header {
    fn encode(self, report: &mut RawFidoReport) {
        report.packet[0..4].copy_from_slice(&self.cid.to_be_bytes());
        report.packet[4] = self.cmd;
        report.packet[5..7].copy_from_slice(&self.bcnt.to_be_bytes());
    }
}

#[allow(clippy::large_enum_variant)]
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
                challenge_parameter: Default::default(),
                application_parameter: Default::default(),
                key_handle_length: Default::default(),
                key_handle: [0; 255],
            },
            _ => MessageRequest::Unknown { cla, ins },
        }
    }
}

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
        attestation_certificate: [u8; 255], // There seems to be no maximum length, not sure what to do here.
        signature: [u8; 73],
    },
    Authenticate {
        user_presence: bool,
        counter: u32,
        signature: [u8; 255], // There seems to be no maximum length, not sure what to do here.
    },
    Error(MessageResponseError),
    Version,
}

impl MessageResponse {
    fn encode(&self, data: &mut [u8]) {
        //
    }
}

pub enum MessageResponseError {
    /// The request was rejected due to test-of-user-presence being required.
    ConditionsNotSatisfied,
    /// The request was rejected due to an invalid key handle.
    WrongData,
    /// The length of the request was invalid.
    WrongLength,
    /// The Class byte of the request is not supported.
    ClaNotSupported,
    /// The Instruction of the request is not supported.
    InsNotSupported,
}
