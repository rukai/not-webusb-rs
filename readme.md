# not-webUSB

A port of [I Cant Believe Its Not WebUSB](https://github.com/ArcaneNibble/i-cant-believe-its-not-webusb) to rust.
This crate allows for communication between specially programmed usb devices and websites without the use of webUSB.
Instead it uses the browsers U2F functionality to send a payload to the device.

<!--
The goal is to be a production ready library for use in real devices.
However, while it works fine for simple use cases, it is not currently in a state where I would be comfortable deploying this in production.
-->

It provides:

* a [usb-device](https://github.com/rust-embedded-community/usb-device) class implementation that runs on your microcontroller
* sample javascript code for talking to the microcontroller from a website. <!--(or a rust crate if your into wasm)-->

TODO:

* This project is a work in progress and does not work yet.
* provide wasm crate for interacting with not-webusb
