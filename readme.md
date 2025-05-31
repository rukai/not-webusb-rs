# not-webUSB

TODO: This project is a work in progress and does not work yet.

A port of [I Cant Believe Its Not WebUSB](https://github.com/ArcaneNibble/i-cant-believe-its-not-webusb) to rust.
This crate allows for communication between specially programmed usb devices and websites without the use of webUSB.
Instead it uses the browsers U2F functionality to send a payload to the device.

It provides:

* a [usb-device](https://github.com/rust-embedded-community/usb-device) class implementation that runs on your microcontroller
* sample code for talking to the microcontroller from a website. TODO: JS or wasm or both?
