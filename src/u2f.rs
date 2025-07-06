use arrayvec::ArrayVec;
use bbqueue::Producer;
use core::iter;
use defmt::*;

use crate::{MAXIMUM_CTAPHID_MESSAGE, MAXIMUM_CTAPHID_MESSAGE_X2};

pub fn respond_to_message(message_data: &[u8], tx: &mut Producer<MAXIMUM_CTAPHID_MESSAGE_X2>) {
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
                        .take(0x1f), // TODO: increasing this beyond 0xB makes the whole thing explode
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
            warn!("unknown message request cla={} ins={}", cla, ins);
            U2fResponse::Error(MessageResponseError::InsNotSupported)
        }
    };

    let mut granted = tx.grant_exact(MAXIMUM_CTAPHID_MESSAGE).unwrap();
    let size = response.encode(&mut granted);
    info!("wrote {} bytes to outgoing response", size);
    granted.commit(size);
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
