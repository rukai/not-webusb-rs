#![no_std]
#![no_main]

use arrayvec::ArrayVec;
use bsp::entry;
use bsp::hal::{clocks::init_clocks_and_plls, pac, sio::Sio, watchdog::Watchdog};
use cortex_m::prelude::*;
#[cfg(feature = "defmt")]
use defmt::*;
#[cfg(feature = "defmt")]
use defmt_rtt as _;
use embedded_hal::digital::{InputPin, OutputPin};
use fugit::ExtU32;
use not_webusb::NotWebUsb;
use panic_probe as _;
use rp_pico as bsp;
use rp2040_hal::Timer;
use usb_device::{
    bus::UsbBusAllocator,
    device::{StringDescriptors, UsbDeviceBuilder, UsbVidPid},
};
use usbd_human_interface_device::device::fido::RawFidoConfig;
use usbd_human_interface_device::prelude::*;

#[entry]
fn main() -> ! {
    let mut pac = pac::Peripherals::take().unwrap();
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

    let mut flash_led = timer.count_down();
    flash_led.start(100.millis());
    let mut led_state = false;

    let mut not_webusb = NotWebUsb::<_, 10000>::new(fido, &|_| true);

    #[cfg(feature = "defmt")]
    info!("begin main loop");

    loop {
        if flash_led.wait().is_ok() {
            led_state = !led_state;
            led_pin.set_state(led_state.into()).unwrap();
        }

        if enter_flash_mode_pin.is_low().unwrap_or(true) {
            // Use this for entering bootsel mode without disconnecting/reconnecting the pico if you dont have a debugger
            pico_example::enter_flash_mode();
        }

        // TODO: can we make NotWebUsb poll logic allow only calling when usb_dev.poll returns true?
        usb_dev.poll(&mut [not_webusb.fido_class()]);
        not_webusb.poll().unwrap();

        if let Some(request) = not_webusb.check_pending_request() {
            #[cfg(feature = "defmt")]
            info!("processing request");
            let response: ArrayVec<u8, 10000> =
                request.iter().copied().map(pico_example::rot13).collect();

            not_webusb.send_response(response);
        }
    }
}
