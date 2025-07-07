#![no_std]

pub mod ctaphid;
pub mod u2f;

use arrayvec::ArrayVec;
use bbqueue::{BBBuffer, Consumer, Producer};
use defmt::panic;
use defmt::*;
use frunk::{HCons, HNil};
use usb_device::{UsbError, bus::UsbBus};
use usbd_human_interface_device::device::fido::{RawFido, RawFidoReport};
use usbd_human_interface_device::prelude::*;

use crate::ctaphid::{
    ContinuationState, CtapHidError, CtapHidRequest, CtapHidRequestTy, CtapHidResponse,
    CtapHidResponseTy, InProgressMessage, InitResponse,
};

// as per FIDO CTAP spec maximum payload size is 7609 bytes
const MAXIMUM_CTAPHID_MESSAGE: usize = 7609;
const MAXIMUM_CTAPHID_MESSAGE_X2: usize = MAXIMUM_CTAPHID_MESSAGE * 2;

// Only contains data for one message at a time.
// The reader can determine the total length of the message as the initial size of the buffer before it is partially sent.
// Needs the double the number of ctaphid message max bytes since the bytes might be marked as used.
// TODO: consider a better type than BBBuffer for this purpose.
static OUTGOING_MESSAGE_BYTES: BBBuffer<MAXIMUM_CTAPHID_MESSAGE_X2> = BBBuffer::new();

pub struct NotWebUsb<'a, UsbBusT: UsbBus> {
    cid_next: i32,
    in_progress_message_option: Option<InProgressMessage>,
    tx: Producer<'a, MAXIMUM_CTAPHID_MESSAGE_X2>,
    rx: Consumer<'a, MAXIMUM_CTAPHID_MESSAGE_X2>,
    raw_response: RawFidoReport,
    fido: UsbHidClass<'a, UsbBusT, HCons<RawFido<'a, UsbBusT>, HNil>>,

    /// User fields
    request: Option<ArrayVec<u8, 255>>,
    response: Option<ArrayVec<u8, 255>>,
}

impl<'a, UsbBusT: UsbBus> NotWebUsb<'a, UsbBusT> {
    pub fn new(fido: UsbHidClass<'a, UsbBusT, HCons<RawFido<'a, UsbBusT>, HNil>>) -> Self {
        let (tx, rx) = OUTGOING_MESSAGE_BYTES.try_split().unwrap();
        NotWebUsb {
            fido,
            tx,
            rx,
            cid_next: 1,
            in_progress_message_option: None,
            raw_response: RawFidoReport::default(),
            request: None,
            response: None,
        }
    }

    /// Use the return value in your call to `UsbDevice::poll`.
    pub fn fido_class(
        &mut self,
    ) -> &mut UsbHidClass<'a, UsbBusT, HCons<RawFido<'a, UsbBusT>, HNil>> {
        &mut self.fido
    }

    /// This must be called regularly
    pub fn poll(&mut self) {
        match self.fido.device().read_report() {
            Err(UsbError::WouldBlock) => {
                //do nothing
            }
            Err(e) => {
                panic!("Failed to read fido report: {:?}", e)
            }
            Ok(report) => {
                let request = CtapHidRequest::parse(&report);
                info!("received ctaphid request {:?}", request);
                let response = match request.ty {
                    CtapHidRequestTy::Ping => Some(CtapHidResponseTy::RawReport(report)),
                    CtapHidRequestTy::Message { length, data } => {
                        if self.in_progress_message_option.is_some() {
                            error!(
                                "Cannot create new transaction while existing transaction is in progress"
                            )
                            // TODO: handle error
                        }

                        self.in_progress_message_option = Some(InProgressMessage {
                            cid: request.cid,
                            request_buffer: [0; MAXIMUM_CTAPHID_MESSAGE],
                            current_request_payload_size: length as usize,
                            current_request_payload_bytes_written: 0,
                            response_continuation_state: ContinuationState::Initial,
                            response_ready_to_send: false,
                            response_final_packet_is_ready_to_send: false,
                        });
                        if let Some(in_progress_message) = &mut self.in_progress_message_option {
                            if let Some(request) =
                                in_progress_message.receive_user_request(&data, &mut self.tx)
                            {
                                if self.request.is_some() {
                                    panic!(
                                        "TODO: handle case where request received when already have one"
                                    )
                                }
                                self.request = Some(request)
                            }
                        }
                        None
                    }
                    CtapHidRequestTy::Continuation { data, .. } => {
                        if let Some(in_progress_message) = &mut self.in_progress_message_option {
                            if in_progress_message.cid == request.cid {
                                if let Some(request) =
                                    in_progress_message.receive_user_request(&data, &mut self.tx)
                                {
                                    if self.request.is_some() {
                                        panic!(
                                            "TODO: handle case where request received when already have one 2"
                                        )
                                    }
                                    self.request = Some(request)
                                }
                            } else {
                                // TODO: error or maybe just drop it
                            }
                        }
                        None
                    }
                    CtapHidRequestTy::Init { nonce8 } => {
                        // TODO: handle broadcast CID
                        self.cid_next += 1;
                        Some(CtapHidResponseTy::Init(InitResponse {
                            nonce_8_bytes: nonce8,
                            channel_id: self.cid_next.to_be_bytes(),
                            protocol_version: 2,
                            device_version_major: 0,
                            device_version_minor: 0,
                            device_version_build: 0,
                            capabilities: 0,
                        }))
                    }
                    CtapHidRequestTy::Unknown { cmd } => {
                        // TODO: handle error
                        warn!("Unknown CTAPHID command {}", cmd);
                        Some(CtapHidResponseTy::Error(CtapHidError::InvalidCommand))
                    }
                };

                if let Some(response) = response {
                    CtapHidResponse {
                        cid: request.cid,
                        ty: response,
                        continuation_state: ContinuationState::Initial,
                    }
                    .encode(&mut self.raw_response);
                    info!("sending direct raw response {}", self.raw_response.packet);
                    match self.fido.device().write_report(&self.raw_response) {
                        Err(UsbHidError::WouldBlock) => defmt::todo!("error handling"),
                        Err(UsbHidError::Duplicate) => defmt::todo!("What does this mean?"),
                        Ok(_) => {}
                        Err(e) => {
                            panic!("Failed to write fido report: {:?}", e)
                        }
                    }
                }
            }
        }

        if let Some(in_progress_message) = &mut self.in_progress_message_option {
            if let Some(user_response) = self.response.take() {
                in_progress_message.send_user_response(&user_response, &mut self.tx);
            }

            // USB may have been blocked, leading to a response already being created but left unsent.
            if !in_progress_message.response_ready_to_send {
                match self.rx.read() {
                    Ok(granted) => {
                        let remaining_u2f_size = granted.len();
                        info!("remaining_u2f_size {}", remaining_u2f_size);
                        let packet_size = if let ContinuationState::Initial =
                            in_progress_message.response_continuation_state
                        {
                            remaining_u2f_size.min(57)
                        } else {
                            remaining_u2f_size.min(59)
                        };
                        info!("packet_size {}", packet_size);
                        in_progress_message.response_final_packet_is_ready_to_send =
                            remaining_u2f_size == packet_size;
                        CtapHidResponse {
                            cid: in_progress_message.cid,
                            ty: CtapHidResponseTy::Message {
                                // only used in the initial message where it is treated as the full u2f size.
                                length: remaining_u2f_size as u16,
                                data: &granted[..packet_size],
                            },
                            continuation_state: in_progress_message.response_continuation_state,
                        }
                        .encode(&mut self.raw_response);
                        info!(
                            "sending prepared raw response {}",
                            &self.raw_response.packet
                        );

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
                        in_progress_message.response_ready_to_send = true;

                        granted.release(packet_size);
                    }
                    Err(bbqueue::Error::InsufficientSize) => {
                        // This is expected when there are no bytes to read.

                        // TODO: This logic is a bit sus, could it lead to deadlock if we completely fill the final packet?
                    }
                    Err(error) => panic!("Unexpected bbq error {}", error),
                }
            }

            if in_progress_message.response_ready_to_send {
                match self.fido.device().write_report(&self.raw_response) {
                    Err(UsbHidError::WouldBlock) => {
                        debug!("Failed to send response as usb would block, will retry");
                    }
                    Err(UsbHidError::Duplicate) => defmt::todo!("What does this mean?"),
                    Ok(_) => {
                        in_progress_message.response_ready_to_send = false;

                        if in_progress_message.response_final_packet_is_ready_to_send {
                            // finished!!!
                            info!("all packets for the in progress message have been sent");
                            self.in_progress_message_option = None;
                        } else {
                            info!("one packet was sent, but more remain to be sent");
                        }
                    }
                    Err(e) => {
                        panic!("Failed to write fido report: {:?}", e)
                    }
                }
            }
        }
    }

    /// Returns the current request if there is one.
    /// Calling this does not consume the request.
    pub fn check_pending_request(&self) -> Option<&[u8]> {
        self.request.as_ref().map(|x| x.as_slice())
    }

    /// Sends a response to the currently pending request.
    /// Calling this consumes the request.
    pub fn send_response(&mut self, message: ArrayVec<u8, 255>) {
        self.response = Some(message);
        self.request = None;
    }
}
