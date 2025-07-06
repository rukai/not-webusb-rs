use authenticator::{
    StatusPinUv, StatusUpdate,
    authenticatorservice::{AuthenticatorService, SignArgs},
    ctap2::server::{
        AuthenticationExtensionsClientInputs, PublicKeyCredentialDescriptor, PublicKeyCredentialId,
        Transport, UserVerificationRequirement,
    },
    statecallback::StateCallback,
};
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
            id: PublicKeyCredentialId::default(),
            transports: vec![Transport::USB],
        }],
        user_verification_req: UserVerificationRequirement::Discouraged,
        user_presence_req: false,
        extensions: AuthenticationExtensionsClientInputs::default(),
        pin: None,
        use_ctap1_fallback: true,
    };

    if let Err(e) = manager.sign(1000, sign_args, status_tx.clone(), callback) {
        panic!("Couldn't register: {e:?}");
    };

    let sign_result = sign_rx
        .recv()
        .expect("Problem receiving, unable to continue");
    let attestation_object = match sign_result {
        Ok(a) => {
            println!("Ok!");
            a
        }
        Err(e) => panic!("Registration failed: {e:?}"),
    };

    println!("Register result: {:?}", &attestation_object);
}
