use crate::{MAXIMUM_CTAPHID_MESSAGE, MAXIMUM_CTAPHID_MESSAGE_X2};
use arrayvec::ArrayVec;
use bbqueue::Producer;
use core::iter;

/// Receives and responds to incoming requests.
/// If a tunnelled not-webusb request is present, instead of responding to it, the bytes of the tunneled request are returned.
pub fn receive_user_request(
    message_data: &[u8],
    tx: &mut Producer<MAXIMUM_CTAPHID_MESSAGE_X2>,
    web_origin_filter: &dyn Fn([u8; 32]) -> bool,
) -> Option<ArrayVec<u8, 255>> {
    let request = U2fRequest::decode(message_data);

    //info!("received u2f request {:?}", request); // TODO: ArrayVec defmt support?
    match &request {
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
        U2fRequest::Authenticate {
            key_handle,
            control,
            application_parameter,
            ..
        } => {
            if let AuthenticateControl::CheckOnly = control {
                // Actually indicates success.
                U2fResponse::Error(MessageResponseError::ConditionsNotSatisfied)
            } else if web_origin_filter(application_parameter) {
                return Some(key_handle);
            } else {
                // web_origin_filter failed, so send a valid response, but dont give any user data.
                U2fResponse::Authenticate {
                    user_presence: true,
                    counter: 0,
                    signature: ArrayVec::new(),
                }
            }
        }
        U2fRequest::Version => U2fResponse::Version,
        U2fRequest::Unknown { cla, ins } => {
            warn!("unknown message request cla={} ins={}", cla, ins);
            U2fResponse::Error(MessageResponseError::InsNotSupported)
        }
    };

    let mut granted = tx.grant_exact(MAXIMUM_CTAPHID_MESSAGE).unwrap();
    let size = response.encode(&mut granted);
    info!("wrote {} bytes to outgoing response", size);
    granted.commit(size);

    None
}

// TODO: pull header bytes out into lib.rs level logic
pub fn send_user_response(
    response: &[u8],
    payload_written_bytes: &mut u32,
    tx: &mut Producer<MAXIMUM_CTAPHID_MESSAGE_X2>,
) {
    // the signature contains two asn.1 integers that we can smuggle data in.
    // They must be exactly 20 bytes each and must never be > 0, since they are signed integers this means starting with 0x7f

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

    if *payload_written_bytes == 0 {
        let payload_bytes_to_write = (response.len() as u32 - *payload_written_bytes).min(0x1b);
        signature.extend(
            ((response.len() as u32).to_be_bytes())
                .iter()
                .copied()
                .chain(
                    response[*payload_written_bytes as usize
                        ..*payload_written_bytes as usize + payload_bytes_to_write as usize]
                        .iter()
                        .copied(),
                )
                .chain(iter::repeat(0))
                .take(0x1f),
        );
        *payload_written_bytes += payload_bytes_to_write;
    } else {
        let payload_bytes_to_write = (response.len() as u32 - *payload_written_bytes).min(0x1f);
        signature.extend(
            response[*payload_written_bytes as usize
                ..*payload_written_bytes as usize + payload_bytes_to_write as usize]
                .iter()
                .copied()
                .chain(iter::repeat(0))
                .take(0x1f),
        );
        *payload_written_bytes += payload_bytes_to_write;
    }

    signature.extend([
        0x02, // ASN.1 integer
        0x20, // Number of bytes in integer
        0x7f, // first byte of 0x7f is used to force the signed integer to be positive for chrome compatibility
    ]);

    let payload_bytes_to_write = (response.len() as u32 - *payload_written_bytes).min(0x1f);
    // must write exactly 0x1f bytes to signature
    signature.extend(
        response[*payload_written_bytes as usize
            ..*payload_written_bytes as usize + payload_bytes_to_write as usize]
            .iter()
            .copied()
            .chain(iter::repeat(0))
            .take(0x1f),
    );
    *payload_written_bytes += payload_bytes_to_write;

    info!("payload_written_bytes {}", payload_written_bytes);
    info!("signature {}", signature.as_slice());

    let response = U2fResponse::Authenticate {
        user_presence: true,
        counter: 0,
        signature,
    };

    // TODO: move into common function
    let mut granted = tx.grant_exact(MAXIMUM_CTAPHID_MESSAGE).unwrap();
    let size = response.encode(&mut granted);
    info!("wrote {} bytes to outgoing response", size);
    granted.commit(size);
}

#[allow(clippy::large_enum_variant)]
pub enum U2fRequest {
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

#[cfg_attr(feature = "defmt", derive(defmt::Format))]
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
                data[6] = 0x90;
                data[7] = 0x00;

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
