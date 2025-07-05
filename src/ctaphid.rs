use crate::u2f::respond_to_message;
use bbqueue::Producer;
use defmt::panic;
use defmt::*;
use usbd_human_interface_device::device::fido::RawFidoReport;

use crate::{MAXIMUM_CTAPHID_MESSAGE, MAXIMUM_CTAPHID_MESSAGE_X2};

pub struct InProgressMessage {
    pub cid: u32,
    pub request_buffer: [u8; MAXIMUM_CTAPHID_MESSAGE],
    pub current_request_payload_size: usize,
    pub current_request_payload_bytes_written: usize,
    pub response_continuation_state: ContinuationState,
}

#[derive(Clone, Copy)]
pub enum ContinuationState {
    Initial,
    Continuation { sequence: u8 },
}

impl InProgressMessage {
    /// Returns true if the request has finished parsing and the response was sent
    pub fn write_data(&mut self, data: &[u8], tx: &mut Producer<MAXIMUM_CTAPHID_MESSAGE_X2>) {
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

pub fn parse_request(report: &RawFidoReport) -> CtapHidRequest {
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
pub struct CtapHidRequest {
    pub cid: u32,
    pub ty: CtapHidRequestTy,
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
    pub fn encode(&self, report: &mut RawFidoReport) {
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
