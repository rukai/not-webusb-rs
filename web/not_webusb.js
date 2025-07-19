_not_webusb_internal_lock = false;


/// Takes a Uint8Array request to send to the device.
/// Returns a Uint8Array response from the device.
async function not_webusb_read_write(input) {
    function toU32(array, offset) {
        return (array[offset] << 24)
            + (array[offset + 1] << 16)
            + (array[offset + 2] << 8)
            + (array[offset + 3]);
    }
    async function concat_uint8array(arrays) {
        return new Uint8Array(await new Blob(arrays).arrayBuffer());
    }

    if (_not_webusb_internal_lock) {
        // TODO: device error should clear internal lock
        // TODO: unique error type?
        throw new Error("not_webusb_read_write was called while another not_webusb_read_write was in progress")
    }
    _not_webusb_internal_lock = true;

    var total_size = input.length + 4;
    var number_of_packets = Math.ceil(total_size / 254);
    console.log("number_of_packets: " + number_of_packets);

    // initial request packets
    for (var i = 0; i < number_of_packets - 1; i++) {
        var sig = await _not_webusb_read_write(await concat_uint8array([
            new Uint8Array([0]),
            input.slice(i * 254, (i + 1) * 254)
        ]));
    }

    // final request packet + initial response packet
    var sig = await _not_webusb_read_write(await concat_uint8array([
        new Uint8Array([2]),
        input.slice((number_of_packets - 1) * 254)
    ]));
    var size = toU32(sig, 5);
    var response = await concat_uint8array([
        sig.slice(9, Math.min(36, 9 + size)),
        sig.slice(39, Math.min(71, 39 + (size - 27)))
    ]);
    console.log("size: " + size);
    size -= 58;

    // final response packets
    while (size > 0) {
        var sig = await _not_webusb_read_write(new Uint8Array([1]));
        response = await concat_uint8array([response, sig.slice(5, 36), sig.slice(39, 71)]);
        size -= 62;
    }

    _not_webusb_internal_lock = false;
    console.log(response);

    return response;
}

/// Takes a Uint8array request of length 0..255
/// Returns a Uint8Array of the raw response, it must be further processed to retrieve user response data.
async function _not_webusb_read_write(input) {
    console.log("read_write input: " + input);
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
    console.log("read_write output: " + credential.response.signature);
    return new Uint8Array(credential.response.signature);
}
