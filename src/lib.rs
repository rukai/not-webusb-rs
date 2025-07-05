#![no_std]

pub mod ctaphid;
pub mod u2f;

use bbqueue::{BBBuffer, Consumer, Producer};
use defmt::panic;
use defmt::*;
use frunk::{HCons, HNil};
use usb_device::{UsbError, bus::UsbBus, device::UsbDevice};
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
        }
    }

    pub fn poll(&mut self, usb_dev: &mut UsbDevice<UsbBusT>) {
        if usb_dev.poll(&mut [&mut self.fido]) {
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
                            });
                            if let Some(in_progress_message) = &mut self.in_progress_message_option
                            {
                                in_progress_message.write_data(&data, &mut self.tx);
                            }
                            None
                        }
                        CtapHidRequestTy::Continuation { data, .. } => {
                            if let Some(in_progress_message) = &mut self.in_progress_message_option
                            {
                                if in_progress_message.cid == request.cid {
                                    in_progress_message.write_data(&data, &mut self.tx);
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

        if let Some(in_progress_message) = &mut self.in_progress_message_option {
            match self.rx.read() {
                Ok(granted) => {
                    let full_u2f_size = granted.len();
                    info!("full_u2f_size {}", full_u2f_size);
                    let packet_size = if let ContinuationState::Initial =
                        in_progress_message.response_continuation_state
                    {
                        full_u2f_size.min(57)
                    } else {
                        full_u2f_size.min(59)
                    };
                    info!("packet_size {}", packet_size);
                    CtapHidResponse {
                        cid: in_progress_message.cid,
                        ty: CtapHidResponseTy::Message {
                            length: full_u2f_size as u16,
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

                    if full_u2f_size == packet_size {
                        // finished!!!
                        info!("all packets for the in progress message have been sent");
                        self.in_progress_message_option = None;
                    }
                    granted.release(packet_size);
                    match self.fido.device().write_report(&self.raw_response) {
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

    #[allow(dead_code)]
    fn recv_request() {}

    #[allow(dead_code)]
    fn send_response(_data: &[u8]) {}
}
