#![no_std]
#![no_main]

use arrayvec::ArrayVec;
use bsp::entry;
use bsp::hal::{
    clocks::{Clock, init_clocks_and_plls},
    pac,
    sio::Sio,
    watchdog::Watchdog,
};
use cortex_m::prelude::*;
#[cfg(feature = "defmt")]
use defmt::panic;
#[cfg(feature = "defmt")]
use defmt::*;
#[cfg(feature = "defmt")]
use defmt_rtt as _;
use embedded_hal::digital::{InputPin, OutputPin};
use fugit::ExtU32;
use not_webusb::NotWebUsb;
use panic_probe as _;
use rp_pico as bsp;
use rp2040_hal::{Timer, rom_data::reset_to_usb_boot};
use usb_device::{
    bus::UsbBusAllocator,
    device::{StringDescriptors, UsbDeviceBuilder, UsbVidPid},
};
use usbd_human_interface_device::device::fido::RawFidoConfig;
use usbd_human_interface_device::prelude::*;

#[entry]
fn main() -> ! {
    let mut pac = pac::Peripherals::take().unwrap();
    let core = pac::CorePeripherals::take().unwrap();
    let mut watchdog = Watchdog::new(pac.WATCHDOG);
    let sio = Sio::new(pac.SIO);

    // External high-speed crystal on the pico board is 12Mhz
    let clocks = init_clocks_and_plls(
        bsp::XOSC_CRYSTAL_FREQ,
        pac.XOSC,
        pac.CLOCKS,
        pac.PLL_SYS,
        pac.PLL_USB,
        &mut pac.RESETS,
        &mut watchdog,
    )
    .ok()
    .unwrap();

    let timer = Timer::new(pac.TIMER, &mut pac.RESETS, &clocks);

    let mut delay = cortex_m::delay::Delay::new(core.SYST, clocks.system_clock.freq().to_Hz());

    let pins = bsp::Pins::new(
        pac.IO_BANK0,
        pac.PADS_BANK0,
        sio.gpio_bank0,
        &mut pac.RESETS,
    );

    let usb_bus = UsbBusAllocator::new(bsp::hal::usb::UsbBus::new(
        pac.USBCTRL_REGS,
        pac.USBCTRL_DPRAM,
        clocks.usb_clock,
        true,
        &mut pac.RESETS,
    ));

    let fido = UsbHidClassBuilder::new()
        .add_device(RawFidoConfig::default())
        .build(&usb_bus);

    let mut usb_dev = UsbDeviceBuilder::new(&usb_bus, UsbVidPid(0x1209, 0x0001))
        .strings(&[StringDescriptors::default()
            .manufacturer("not-webusb")
            .product("not-webusb demo")
            .serial_number("TEST")])
        .unwrap()
        .build();

    let mut led_pin = pins.led.into_push_pull_output();
    let mut enter_flash_mode_pin = pins.gpio2.into_pull_up_input();

    for _ in 0..4 {
        led_pin.set_high().unwrap();
        delay.delay_ms(500);
        led_pin.set_low().unwrap();
        delay.delay_ms(500);
    }

    let mut tick_count_down = timer.count_down();
    tick_count_down.start(1.millis());

    let mut flash_led = timer.count_down();
    flash_led.start(100.millis());
    let mut led_state = false;

    // sha256 hash of "rukai.github.io"
    const GITHUB_ORIGIN_HASH: [u8; 32] = [
        177, 35, 155, 252, 236, 173, 132, 229, 7, 216, 88, 116, 147, 211, 15, 63, 109, 115, 157,
        167, 78, 170, 168, 131, 115, 65, 251, 76, 71, 75, 154, 114,
    ];

    let mut not_webusb = NotWebUsb::new(fido, &|origin_hash| origin_hash == GITHUB_ORIGIN_HASH);

    #[cfg(feature = "defmt")]
    info!("begin main loop");
    loop {
        if flash_led.wait().is_ok() {
            led_state = !led_state;
            led_pin.set_state(led_state.into()).unwrap();
        }

        if enter_flash_mode_pin.is_low().unwrap_or(true) {
            enter_flash_mode();
        }

        // TODO: can we make NotWebUsb poll logic allow only calling when usb_dev.poll returns true?
        usb_dev.poll(&mut [not_webusb.fido_class()]);
        not_webusb.poll();

        if let Some(request) = not_webusb.check_pending_request() {
            #[cfg(feature = "defmt")]
            info!("processing request");
            let response: ArrayVec<u8, 255> = request.iter().copied().map(rot13).collect();

            not_webusb.send_response(response);
        }
    }
}

fn rot13(x: u8) -> u8 {
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
}

fn enter_flash_mode() -> ! {
    #[cfg(feature = "defmt")]
    info!("entering flash mode");
    reset_to_usb_boot(0, 0);
    panic!()
}
