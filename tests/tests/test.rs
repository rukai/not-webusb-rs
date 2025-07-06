use authenticator::{
    Assertion, GetAssertionResult, StatusPinUv, StatusUpdate,
    authenticatorservice::{AuthenticatorService, SignArgs},
    ctap2::{
        attestation::{AuthenticatorData, AuthenticatorDataFlags, Extension},
        server::{
            AuthenticationExtensionsClientInputs, AuthenticatorAttachment,
            PublicKeyCredentialDescriptor, RpIdHash, Transport, UserVerificationRequirement,
        },
    },
    statecallback::StateCallback,
};
use pretty_assertions::assert_eq;
use std::sync::mpsc::{RecvError, channel};
use std::thread;

#[test]
fn test() {
    env_logger::init();

    let mut manager =
        AuthenticatorService::new().expect("The auth service should initialize safely");
    manager.add_u2f_usb_hid_platform_transports();

    let (status_tx, status_rx) = channel::<StatusUpdate>();
    thread::spawn(move || {
        loop {
            match status_rx.recv() {
                Ok(StatusUpdate::InteractiveManagement(..)) => {
                    panic!("STATUS: This can't happen when doing non-interactive usage");
                }
                Ok(StatusUpdate::SelectDeviceNotice) => {
                    println!("STATUS: Please select a device by touching one of them.");
                }
                Ok(StatusUpdate::PresenceRequired) => {
                    println!("STATUS: waiting for user presence");
                }
                Ok(StatusUpdate::PinUvError(StatusPinUv::PinRequired(_))) => {
                    todo!()
                }
                Ok(StatusUpdate::PinUvError(StatusPinUv::InvalidPin(_, _))) => {
                    todo!()
                }
                Ok(StatusUpdate::PinUvError(StatusPinUv::PinAuthBlocked)) => {
                    panic!(
                        "Too many failed attempts in one row. Your device has been temporarily blocked. Please unplug it and plug in again."
                    )
                }
                Ok(StatusUpdate::PinUvError(StatusPinUv::PinBlocked)) => {
                    panic!("Too many failed attempts. Your device has been blocked. Reset it.")
                }
                Ok(StatusUpdate::PinUvError(StatusPinUv::InvalidUv(attempts))) => {
                    println!(
                        "Wrong UV! {}",
                        attempts.map_or("Try again.".to_string(), |a| format!(
                            "You have {a} attempts left."
                        ))
                    );
                    continue;
                }
                Ok(StatusUpdate::PinUvError(StatusPinUv::UvBlocked)) => {
                    println!("Too many failed UV-attempts.");
                    continue;
                }
                Ok(StatusUpdate::PinUvError(e)) => {
                    panic!("Unexpected error: {e:?}")
                }
                Ok(StatusUpdate::SelectResultNotice(_, _)) => {
                    panic!("Unexpected select device notice")
                }
                Err(RecvError) => {
                    println!("STATUS: end");
                    return;
                }
            }
        }
    });

    let (sign_tx, sign_rx) = channel();
    let callback = StateCallback::new(Box::new(move |rv| {
        sign_tx.send(rv).unwrap();
    }));

    let sign_args = SignArgs {
        client_data_hash: [0; 32],
        origin: "".to_owned(),
        relying_party_id: "".to_owned(),
        allow_list: vec![PublicKeyCredentialDescriptor {
            id: "abcdefghijklmnopqrstuvwxyzabcdefghijklmnopr"
                .as_bytes()
                .to_vec(),
            transports: vec![Transport::USB],
        }],
        user_verification_req: UserVerificationRequirement::Discouraged,
        user_presence_req: false,
        extensions: AuthenticationExtensionsClientInputs::default(),
        pin: None,
        use_ctap1_fallback: true,
    };

    if let Err(e) = manager.sign(1000, sign_args, status_tx, callback) {
        panic!("Couldn't sign: {e:?}");
    };

    let sign_result = sign_rx
        .recv()
        .expect("Problem receiving, unable to continue");
    let attestation_object = match sign_result {
        Ok(a) => a,
        Err(e) => panic!("Registration failed: {e:?}"),
    };
    assert_eq!(
        GetAssertionResult {
            assertion: Assertion {
                credentials: Some(PublicKeyCredentialDescriptor {
                    id: vec![
                        // abcdef...
                        97, 98, 99, 100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112,
                        113, 114, 115, 116, 117, 118, 119, 120, 121, 122, 97, 98, 99, 100, 101,
                        102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112
                    ],
                    transports: vec!(Transport::USB)
                }),
                auth_data: AuthenticatorData {
                    rp_id_hash: RpIdHash([
                        0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8,
                        0x99, 0x6f, 0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c,
                        0xa4, 0x95, 0x99, 0x1b, 0x78, 0x52, 0xb8, 0x55
                    ]),
                    flags: AuthenticatorDataFlags::USER_PRESENT,
                    counter: 0,
                    credential_data: None,
                    extensions: Extension::default(),
                },
                signature: vec![
                    // rot13 of abcdef... is stored in an ASN.1 signature, split across two integers
                    48, 68, 2, 32, 127, 110, 111, 112, 113, 114, 115, 116, 117, 118, 119, 120, 121,
                    122, 97, 98, 99, 100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111,
                    112, 113, 114, 2, 32, 127, 115, 116, 117, 118, 119, 120, 121, 122, 97, 98, 99,
                    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                ],
                user: None,
            },
            attachment: AuthenticatorAttachment::Unknown,
            extensions: Default::default(),
        },
        attestation_object,
    );
}
