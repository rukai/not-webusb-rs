#![no_std]

#[cfg(feature = "defmt")]
use defmt::*;
use rp2040_hal::rom_data::reset_to_usb_boot;

pub fn rot13(x: u8) -> u8 {
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

pub fn enter_flash_mode() -> ! {
    #[cfg(feature = "defmt")]
    info!("entering flash mode");
    reset_to_usb_boot(0, 0);
    core::panic!()
}
