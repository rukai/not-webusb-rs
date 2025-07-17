lock = false;

/// Takes a Uint8Array request to send to the device.
/// Returns a Uint8Array response from the device.
async function not_webusb_read_write(input) {
    if (lock) {
        // TODO: return error and handle
        return;
    }
    lock = true;

    if (input.length > 255) {
        console.log("TODO! implement packetizing")
    }
    let credential = await navigator.credentials.get({
        publicKey: {
            challenge: new Uint8Array([]),
            allowCredentials: [{
                type: "public-key",
                transports: ["usb"],
                id: input,
            }],
            userVerification: "discouraged",
        }
    });
    lock = false;
    let sig = new Uint8Array(credential.response.signature);
    return new Uint8Array(await new Blob([sig.slice(5, 36), sig.slice(39, 71)]).arrayBuffer());
}