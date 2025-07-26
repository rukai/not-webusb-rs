#![no_std]

mod ctaphid;
pub(crate) mod fmt;
mod u2f;

use crate::ctaphid::{
    ContinuationState, CtapHidError, CtapHidRequest, CtapHidRequestTy, CtapHidResponse,
    CtapHidResponseTy, InProgressTransaction, InitResponse,
};
use arrayvec::ArrayVec;
use bbqueue::{BBBuffer, Consumer, Producer};
use frunk::{HCons, HNil};
use usb_device::{UsbError, bus::UsbBus};
use usbd_human_interface_device::device::fido::{RawFido, RawFidoReport};
use usbd_human_interface_device::prelude::*;

// as per FIDO CTAP spec maximum payload size is 7609 bytes
const MAXIMUM_CTAPHID_MESSAGE: usize = 7609;
const MAXIMUM_CTAPHID_MESSAGE_X2: usize = MAXIMUM_CTAPHID_MESSAGE * 2;

// Only contains data for one message at a time.
// The reader can determine the total length of the message as the initial size of the buffer before it is partially sent.
// Needs the double the number of CTAPHID message max bytes since the bytes might be marked as used.
// TODO: consider a better type than BBBuffer for this purpose.
static OUTGOING_MESSAGE_BYTES: BBBuffer<MAXIMUM_CTAPHID_MESSAGE_X2> = BBBuffer::new();

/// The main type for not-webusb.
/// Construct this via `NotWebUsb::new` and then regularly poll it via `NotWebUsb::poll`.
/// Check for requests via `NotWebUsb::check_pending_request`, a response must be sent via `NotWebUsb::send_response` once it is ready.
pub struct NotWebUsb<'a, UsbBusT: UsbBus, const MAX_MESSAGE_LEN: usize = 1024> {
    cid_next: i32,
    in_progress_transaction_option: Option<InProgressTransaction>,
    tx: Producer<'a, MAXIMUM_CTAPHID_MESSAGE_X2>,
    rx: Consumer<'a, MAXIMUM_CTAPHID_MESSAGE_X2>,
    raw_response: RawFidoReport,
    fido: UsbHidClass<'a, UsbBusT, HCons<RawFido<'a, UsbBusT>, HNil>>,
    web_origin_filter: &'a dyn Fn([u8; 32]) -> bool,
    user_data: UserDataState<MAX_MESSAGE_LEN>,
}

impl<'a, UsbBusT: UsbBus, const MAX_MESSAGE_LEN: usize> NotWebUsb<'a, UsbBusT, MAX_MESSAGE_LEN> {
    /// Create a new NotWebusb instance.
    ///
    /// ## web_origin_filter
    /// The `web_origin_filter` is used to limit the websites that can talk to your device.
    /// The `web_origin_filter` function is called once for every request, if `web_origin_filter` returns true the request is passed on to the user, otherwise the request is dropped.
    /// If you don't care care about limiting the websites that can talk to your device, simply use `&|_| true` as the web_origin_filter to accept all requests, otherwise read on.
    ///
    /// The argument passed to the `web_origin_filter is the sha256 hash of the domain name.
    /// This could be calculated by e.g. `echo -n "example.com" | sha256 | od -t u1
    /// The web application can slightly alter the domain used via the webauth [rpId field](https://developer.mozilla.org/en-US/docs/Web/API/PublicKeyCredentialRequestOptions#rpid)
    /// Browsers will only allow this field to reduce scope e.g. `example.com` -> `sub.example.com`
    /// And browsers entirely forbid use of U2F from `http://` websites, `https://`` is required.
    /// This gives us a guarantee that the website the device is talking to is the real website at the hashed domain.
    ///
    /// Internally NotWebusb uses the `application_parameter` field of the U2F authenticate request as the argument to `web_origin_filter`.
    pub fn new(
        fido: UsbHidClass<'a, UsbBusT, HCons<RawFido<'a, UsbBusT>, HNil>>,
        web_origin_filter: &'a dyn Fn([u8; 32]) -> bool,
    ) -> Self {
        let (tx, rx) = OUTGOING_MESSAGE_BYTES.try_split().unwrap();
        NotWebUsb {
            fido,
            tx,
            rx,
            // Start at CID 1, since CID 0 is reserved
            cid_next: 1,
            in_progress_transaction_option: None,
            raw_response: RawFidoReport::default(),
            web_origin_filter,
            user_data: UserDataState::None,
        }
    }

    /// Use the return value in your call to `UsbDevice::poll`.
    pub fn fido_class(
        &mut self,
    ) -> &mut UsbHidClass<'a, UsbBusT, HCons<RawFido<'a, UsbBusT>, HNil>> {
        &mut self.fido
    }

    fn reset_state(&mut self) {
        self.cid_next = 0;
        self.in_progress_transaction_option = None;
        if let Ok(read) = self.rx.split_read() {
            read.release(MAXIMUM_CTAPHID_MESSAGE_X2);
        }
        self.raw_response = RawFidoReport::default();
        self.user_data = UserDataState::None;
    }

    /// This must be called regularly, even when there is no in progress request or response.
    ///
    /// Performs CTAPHID request/response handling.
    /// If a user request is contained within the CTAPHID requests it will be stored internally such that it is returned by `NotWebUsb::check_pending_request.
    /// If a response is set by `NotWebUsb::send_response` the response will be sent within the CTAPHID responses.
    pub fn poll(&mut self) -> Result<(), NotWebUsbError> {
        match self.fido.device().read_report() {
            Err(UsbError::WouldBlock) => {
                // do nothing
            }
            Err(e) => {
                // We failed to read this request, log and continue on, hopefully its recoverable.
                error!(
                    "Failed to read fido report: {:?} - resetting NotWebusb state",
                    e
                );
                self.reset_state();
                return Err(NotWebUsbError::UsbError);
            }
            Ok(report) => {
                let request = CtapHidRequest::parse(&report);
                info!("received ctaphid request {:?}", request);
                let response = match request.ty {
                    CtapHidRequestTy::Ping => Some(CtapHidResponseTy::RawReport(report)),
                    CtapHidRequestTy::Message { length, data } => {
                        if self.in_progress_transaction_option.is_some() {
                            warn!(
                                "New transaction was requested while a transaction is already in progress"
                            );
                            Some(CtapHidResponseTy::Error(CtapHidError::ChannelBusy))
                        } else {
                            self.in_progress_transaction_option =
                                Some(InProgressTransaction::new(request.cid, length));
                            if let Some(in_progress_message) =
                                &mut self.in_progress_transaction_option
                            {
                                if let Some(request) = in_progress_message.receive_user_request(
                                    &data,
                                    &mut self.tx,
                                    &self.web_origin_filter,
                                ) {
                                    self.user_data.receive_request(
                                        request,
                                        in_progress_message,
                                        &mut self.tx,
                                    );
                                }
                            }
                            None
                        }
                    }
                    CtapHidRequestTy::Continuation { data, sequence } => {
                        if let Some(in_progress_transaction) =
                            &mut self.in_progress_transaction_option
                        {
                            if in_progress_transaction.request_sequence != sequence {
                                error!(
                                    "Received ctaphid request with invalid sequence number was {} expected {}",
                                    sequence, in_progress_transaction.request_sequence
                                );
                                Some(CtapHidResponseTy::Error(CtapHidError::InvalidSeq))
                            } else {
                                in_progress_transaction.request_sequence += 1;

                                if in_progress_transaction.cid == request.cid {
                                    if let Some(request) = in_progress_transaction
                                        .receive_user_request(
                                            &data,
                                            &mut self.tx,
                                            &self.web_origin_filter,
                                        )
                                    {
                                        self.user_data.receive_request(
                                            request,
                                            in_progress_transaction,
                                            &mut self.tx,
                                        );
                                    }
                                } else {
                                    // TODO: error or maybe just drop it
                                }
                                None
                            }
                        } else {
                            warn!("Continuation packet with no Initial packet, ignoring");
                            None
                        }
                    }
                    CtapHidRequestTy::Init { nonce8 } => {
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

                    CtapHidRequestTy::Cancel => {
                        let will_cancel = self.in_progress_transaction_option.is_some();
                        self.in_progress_transaction_option = None;

                        if will_cancel {
                            Some(CtapHidResponseTy::Error(CtapHidError::KeepAliveCancel))
                        } else {
                            None
                        }
                    }
                    CtapHidRequestTy::CborMessage => {
                        // We dont support cbor, so return invalid command error.
                        Some(CtapHidResponseTy::Error(CtapHidError::InvalidCommand))
                    }
                    CtapHidRequestTy::Unknown { cmd } => {
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
                        Err(UsbHidError::WouldBlock) => todo!("error handling"),
                        Err(UsbHidError::Duplicate) => todo!("What does this mean?"),
                        Ok(_) => {}
                        Err(e) => {
                            error!(
                                "Failed to write fido report: {:?} - resetting NotWebusb state",
                                e
                            );
                            self.reset_state();
                            return Err(NotWebUsbError::UsbError);
                        }
                    }
                }
            }
        }

        if let Some(in_progress_transaction) = &mut self.in_progress_transaction_option {
            if let UserDataState::SendingResponse {
                data,
                bytes_sent,
                pending_request,
            } = &mut self.user_data
            {
                if *pending_request {
                    in_progress_transaction.send_user_response(data, bytes_sent, &mut self.tx);
                    *pending_request = false;
                }

                if *bytes_sent >= data.len() as u32 {
                    self.user_data = UserDataState::None;
                }
            }

            // USB may have been blocked, leading to a response already being created but left unsent.
            if !in_progress_transaction.response_ready_to_send {
                match self.rx.read() {
                    Ok(granted) => {
                        let remaining_u2f_size = granted.len();
                        let packet_size = if let ContinuationState::Initial =
                            in_progress_transaction.response_continuation_state
                        {
                            remaining_u2f_size.min(57)
                        } else {
                            remaining_u2f_size.min(59)
                        };
                        in_progress_transaction.response_final_packet_is_ready_to_send =
                            remaining_u2f_size == packet_size;
                        CtapHidResponse {
                            cid: in_progress_transaction.cid,
                            ty: CtapHidResponseTy::Message {
                                // only used in the initial message where it is treated as the full u2f size.
                                length: remaining_u2f_size as u16,
                                data: &granted[..packet_size],
                            },
                            continuation_state: in_progress_transaction.response_continuation_state,
                        }
                        .encode(&mut self.raw_response);
                        info!(
                            "one ctaphid response packet has been prepared {}",
                            &self.raw_response.packet
                        );

                        // step sequence state
                        match &mut in_progress_transaction.response_continuation_state {
                            ContinuationState::Continuation { sequence } => {
                                *sequence += 1;
                            }
                            ContinuationState::Initial => {
                                in_progress_transaction.response_continuation_state =
                                    ContinuationState::Continuation { sequence: 0 }
                            }
                        }
                        in_progress_transaction.response_ready_to_send = true;

                        granted.release(packet_size);
                    }
                    Err(bbqueue::Error::InsufficientSize) => {
                        // This is expected when there are no bytes to read.

                        // TODO: This logic is a bit sus, could it lead to deadlock if we completely fill the final packet?
                    }
                    #[cfg(feature = "defmt")]
                    Err(error) => panic!("Unexpected bbq error {}", error),
                    #[cfg(not(feature = "defmt"))]
                    Err(_) => panic!("Unexpected bbq error"),
                }
            }

            if in_progress_transaction.response_ready_to_send {
                match self.fido.device().write_report(&self.raw_response) {
                    Err(UsbHidError::WouldBlock) => {
                        debug!("Failed to send response as usb would block, will retry");
                    }
                    Err(UsbHidError::Duplicate) => todo!("What does this mean?"),
                    Ok(_) => {
                        in_progress_transaction.response_ready_to_send = false;

                        if in_progress_transaction.response_final_packet_is_ready_to_send {
                            // finished!!!
                            info!("all packets for the in progress message have been sent");
                            self.in_progress_transaction_option = None;
                        } else {
                            info!("one ctaphid packet was sent, but more remain to be sent");
                        }
                    }
                    Err(e) => {
                        panic!("Failed to write fido report: {:?}", e)
                    }
                }
            }
        }
        Ok(())
    }

    /// Returns the current request if there is one.
    /// Calling this does not consume the request.
    pub fn check_pending_request(&self) -> Option<&[u8]> {
        if let UserDataState::ReceivedRequest(request) = &self.user_data {
            Some(request.as_slice())
        } else {
            None
        }
    }

    /// Sends a response to the currently pending request.
    /// Calling this consumes the request.
    pub fn send_response(&mut self, message: ArrayVec<u8, MAX_MESSAGE_LEN>) {
        if !matches!(self.user_data, UserDataState::ReceivedRequest(_)) {
            panic!("Cannot call NotWebusb::send_response until a request has been received.");
        }
        self.user_data = UserDataState::SendingResponse {
            data: message,
            bytes_sent: 0,
            pending_request: true,
        }
    }
}

/// Represents the state of any in progress user requests or responses.
/// This is the highest level state and does not hold any fido/ctap/u2f state.
enum UserDataState<const MAX_MESSAGE_LEN: usize> {
    /// The request has been partially received from the client.
    /// The device has not looked at any of it yet.
    ReceivingRequest(ArrayVec<u8, MAX_MESSAGE_LEN>),
    /// The entire request has been received from the client.
    /// The device may or may not have looked at it yet.
    ReceivedRequest(ArrayVec<u8, MAX_MESSAGE_LEN>),
    /// The entire response has been sent by the device.
    /// The client may have partially received it but has not fully received it.
    SendingResponse {
        data: ArrayVec<u8, MAX_MESSAGE_LEN>,
        bytes_sent: u32,
        pending_request: bool,
    },
    /// There are no in progress requests or responses.
    None,
}

impl<'a, const MAX_MESSAGE_LEN: usize> UserDataState<MAX_MESSAGE_LEN> {
    fn receive_request(
        &mut self,
        request: ArrayVec<u8, 255>,
        in_progress_message: &mut InProgressTransaction,
        tx: &mut Producer<'a, MAXIMUM_CTAPHID_MESSAGE_X2>,
    ) {
        match self {
            UserDataState::ReceivingRequest(partial_request) => {
                let header = RequestHeader::parse(request[0]);
                partial_request.extend(request.as_slice()[1..].iter().copied());
                match header {
                    Some(RequestHeader::FinalRequest) => {
                        info!("continuing user request - final request packet");
                        *self = UserDataState::ReceivedRequest({
                            let mut v = ArrayVec::new();
                            v.extend(partial_request.as_slice().iter().copied());
                            v
                        });
                    }
                    Some(RequestHeader::InitialRequest) => {
                        info!("continuing user request - initial request packet");
                        in_progress_message.send_user_response(&[], &mut 0, tx);
                    }
                    Some(RequestHeader::NeedMoreResponseData) => {
                        panic!("unexpected request header")
                    }
                    None => todo!("unknown request header"),
                }
            }
            UserDataState::ReceivedRequest(_) => {
                panic!("TODO: handle case where request received when already have one")
            }
            UserDataState::SendingResponse {
                pending_request, ..
            } => match RequestHeader::parse(request[0]) {
                Some(RequestHeader::NeedMoreResponseData) => {
                    info!("received user request for more response data");
                    *pending_request = true;
                }
                _ => panic!(
                    "TODO: handle protocol violation where request is sent without correct header value"
                ),
            },
            UserDataState::None => {
                // start a new transaction
                match RequestHeader::parse(request[0]) {
                    Some(RequestHeader::FinalRequest) => {
                        info!("starting new user request - final request packet");
                        *self = UserDataState::ReceivedRequest({
                            let mut v = ArrayVec::new();
                            v.extend(request.as_slice()[1..].iter().copied());
                            v
                        });
                    }
                    Some(RequestHeader::InitialRequest) => {
                        info!("starting new user request - initial request packet");
                        in_progress_message.send_user_response(&[], &mut 0, tx);
                        *self = UserDataState::ReceivingRequest({
                            let mut v = ArrayVec::new();
                            v.extend(request.as_slice()[1..].iter().copied());
                            v
                        });
                    }
                    Some(RequestHeader::NeedMoreResponseData) => {
                        panic!("TODO: unexpected request header")
                    }
                    None => todo!("unknown user request header"),
                }
            }
        }
    }
}

#[cfg_attr(feature = "defmt", derive(defmt::Format))]
enum RequestHeader {
    InitialRequest = 0,
    FinalRequest = 2,
    NeedMoreResponseData = 1,
}

impl RequestHeader {
    fn parse(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::InitialRequest),
            1 => Some(Self::NeedMoreResponseData),
            2 => Some(Self::FinalRequest),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum NotWebUsbError {
    /// A USB error that NotWebusb cannot handle.
    /// This will not occur in regular usage and indicates something has gone terribly wrong.
    /// All NotWebUsb internal state is reset including the loss of any in progress request or response is lost.
    /// Attempt to recover by either recreating the USB connection or resetting the device.
    UsbError,
}
