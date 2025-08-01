use crate::u2f::{receive_user_request, send_user_response};
use crate::{MAXIMUM_CTAPHID_MESSAGE, MAXIMUM_CTAPHID_MESSAGE_X2};
use arrayvec::ArrayVec;
use bbqueue::Producer;
use usbd_human_interface_device::device::fido::RawFidoReport;

/// Represents the state of an in progress transaction.
/// The term `transaction` comes from the CTAP spec, referring to the processing of a request/response pair.
pub struct InProgressTransaction {
    pub message_type: MessageType,
    pub cid: u32,
    /// valid values are 0-127
    pub request_sequence: u8,
    pub request_buffer: [u8; MAXIMUM_CTAPHID_MESSAGE],
    pub request_payload_size: usize,
    pub request_payload_bytes_written: usize,
    pub response_continuation_state: ContinuationState,
    pub response_ready_to_send: bool,
    pub response_final_packet_is_ready_to_send: bool,
}

#[derive(Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum MessageType {
    /// The new messages for CTAP2.
    /// We implement only a very tiny part of this to get U2F messages working
    Cbor,
    /// The older style messages for CTAP1.
    /// This is what we use for smuggling user data in.
    U2f,
}

#[derive(Clone, Copy)]
pub enum ContinuationState {
    Initial,
    Continuation { sequence: u8 },
}

impl InProgressTransaction {
    pub fn new(message_type: MessageType, cid: u32, request_payload_size: u16) -> Self {
        InProgressTransaction {
            message_type,
            cid,
            request_sequence: 0,
            request_buffer: [0; MAXIMUM_CTAPHID_MESSAGE],
            request_payload_size: request_payload_size as usize,
            request_payload_bytes_written: 0,
            response_continuation_state: ContinuationState::Initial,
            response_ready_to_send: false,
            response_final_packet_is_ready_to_send: false,
        }
    }

    /// Returns true if the request has finished parsing and the response was sent
    pub fn receive_user_request(
        &mut self,
        data: &[u8],
        tx: &mut Producer<MAXIMUM_CTAPHID_MESSAGE_X2>,
        web_origin_filter: &dyn Fn([u8; 32]) -> bool,
    ) -> Option<ArrayVec<u8, 255>> {
        self.request_buffer
            [self.request_payload_bytes_written..self.request_payload_bytes_written + data.len()]
            .copy_from_slice(data);

        // if we have completely received the request, respond to it.
        self.request_payload_bytes_written += data.len();
        if self.request_payload_bytes_written >= self.request_payload_size {
            let request = &self.request_buffer[..self.request_payload_size];
            match self.message_type {
                MessageType::Cbor => {
                    let mut granted = tx.grant_exact(MAXIMUM_CTAPHID_MESSAGE).unwrap();

                    // For browsers like chrome on linux it is sufficent to simply reply to CBOR messages with `CtapHidError::InvalidCommand`.
                    // However all browsers using the webauthn.dll (all browsers running on windows) will give up on us unless we can tell them we only support U2F by handling the CBOR GetInfo request. 🙃
                    // For all other CBOR messages we can just return InvalidCommand, which we do at an earlier stage.

                    // To avoid pulling in an entire CBOR implementation, we just hardcode this CBOR GetInfo response which is generated like this:
                    //
                    // ```rust
                    //#[derive(Debug, Serialize, Deserialize)]
                    //struct GetInfo {
                    //    versions: Vec<String>,
                    //    #[serde(with = "serde_bytes")]
                    //    aaguid: Vec<u8>,
                    //}
                    //let get_info = GetInfo {
                    //    versions: vec!["U2F_V2".to_owned()],
                    //    // a unique aaguid for not-webusb
                    //    aaguid: vec![
                    //        0xe3, 0xb1, 0x76, 0x8b, 0x55, 0x91, 0x4a, 0xd7, 0xb4, 0x6e, 0xac, 0xc7, 0x60, 0x84,
                    //        0x0b, 0x3e,
                    //    ],
                    //};
                    //let bytes: Vec<u8> = serde_cbor::to_vec(&get_info).unwrap();
                    //```
                    let get_info_response = [
                        162, 104, 118, 101, 114, 115, 105, 111, 110, 115, 129, 102, 85, 50, 70, 95,
                        86, 50, 102, 97, 97, 103, 117, 105, 100, 80, 227, 177, 118, 139, 85, 145,
                        74, 215, 180, 110, 172, 199, 96, 132, 11, 62,
                    ];
                    let len = get_info_response.len();
                    granted[..len].copy_from_slice(&get_info_response);
                    granted.commit(len);
                }
                MessageType::U2f => {
                    return receive_user_request(request, tx, web_origin_filter);
                }
            }
        }
        None
    }

    pub fn send_user_response(
        &mut self,
        response: &[u8],
        bytes_sent: &mut u32,
        tx: &mut Producer<MAXIMUM_CTAPHID_MESSAGE_X2>,
    ) {
        send_user_response(response, bytes_sent, tx);
    }
}

#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct CtapHidRequest {
    pub cid: u32,
    pub ty: CtapHidRequestTy,
}

impl CtapHidRequest {
    pub fn parse(report: &RawFidoReport) -> Self {
        let packet = &report.packet;
        let cid = u32::from_be_bytes(packet[0..4].try_into().unwrap());
        let ty = if packet[4] & 0b10000000 == 0 {
            CtapHidRequestTy::MessageContinuation {
                sequence: packet[4],
                data: packet[5..].try_into().unwrap(),
            }
        } else {
            let bcnt: u16 = u16::from_be_bytes(packet[5..7].try_into().unwrap());
            let cmd = packet[4] & 0b01111111;
            match cmd {
                0x01 => CtapHidRequestTy::Ping,
                0x03 => CtapHidRequestTy::MessageInitial {
                    length: bcnt,
                    data: packet[7..].try_into().unwrap(),
                    ty: MessageType::U2f,
                },
                0x06 => CtapHidRequestTy::Init {
                    nonce8: packet[7..15].try_into().unwrap(),
                },
                0x10 => CtapHidRequestTy::MessageInitial {
                    length: bcnt,
                    data: packet[7..].try_into().unwrap(),
                    ty: MessageType::Cbor,
                },
                0x11 => CtapHidRequestTy::Cancel,
                cmd => CtapHidRequestTy::Unknown { cmd },
            }
        };

        CtapHidRequest { cid, ty }
    }
}

#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum CtapHidRequestTy {
    /// Initialize
    Init {
        /// 8-byte nonce
        nonce8: [u8; 8],
    },
    /// Send the entire raw request back as is.
    Ping,
    /// A U2F  or CBOR message.
    MessageInitial {
        /// Full length of the payload, possibly this packet and one or more continuation packets.
        length: u16,
        /// packet contents.
        /// since header is 7 bytes long and packet is max 64 bytes this is max 57 bytes
        data: [u8; 57],
        ty: MessageType,
    },
    /// A U2F or CBOR continuation packet.
    /// In theory this could be used for any command, in reality only `Message` is long enough to need it.
    MessageContinuation {
        sequence: u8,
        /// packet contents.
        /// since continuation header is 5 bytes long and packet is max 64 bytes this is max 59 bytes
        data: [u8; 59],
    },
    /// Cancel a current transaction
    Cancel,
    /// An unknown command
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
    // U2F or CBOR message response
    Message {
        /// Full length of the payload, possibly this packet and one or more continuation packets.
        length: u16,
        /// packet contents.
        /// since header is 7 bytes long and packet is max 64 bytes this is max 57 bytes
        data: &'a [u8],
        /// Is the message CBOR or U2F?
        ty: MessageType,
    },
    /// Use this to provide a response to a Ping or if you need to construct a custom response for any reason.
    RawReport(RawFidoReport),
    Error(CtapHidError),
}

#[derive(Clone, Copy)]
pub enum CtapHidError {
    InvalidCommand = 0x01,
    //InvalidParameter = 0x02,
    InvalidLen = 0x03,
    InvalidSeq = 0x04,
    //MessageTimeout = 0x05,
    ChannelBusy = 0x06,
    //LockRequired = 0x0A,
    //InvalidChannel = 0x0B,
    KeepAliveCancel = 0x2D,
    //Other = 0x7F,
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
            CtapHidResponseTy::Message { length, data, ty } => match self.continuation_state {
                ContinuationState::Initial => {
                    CtapHeaderInitialization {
                        cid: self.cid,
                        cmd: match ty {
                            MessageType::U2f => 0x83,
                            MessageType::Cbor => 0x90,
                        },
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
            CtapHidResponseTy::Error(error) => {
                CtapHeaderInitialization {
                    cid: self.cid,
                    cmd: 0x3F,
                    bcnt: 1,
                }
                .encode(report);
                report.packet[7] = *error as u8;
            }
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
