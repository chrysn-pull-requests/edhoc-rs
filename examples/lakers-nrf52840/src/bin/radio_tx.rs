#![no_std]
#![no_main]

use defmt::info;
use defmt::unwrap;
use embassy_executor::Spawner;
use embassy_nrf::gpio::{Level, Output, OutputDrive};
use embassy_nrf::radio::ble::Mode;
use embassy_nrf::radio::ble::Radio;
use embassy_nrf::radio::TxPower;
use embassy_nrf::{bind_interrupts, peripherals, radio};
use embassy_time::WithTimeout;
use embassy_time::{Duration, Timer};
use {defmt_rtt as _, panic_probe as _};

use lakers::*;
use lakers_crypto::{default_crypto, CryptoTrait};

extern crate alloc;

use embedded_alloc::Heap;

use core::ffi::c_char;

#[global_allocator]
static HEAP: Heap = Heap::empty();

extern "C" {
    pub fn mbedtls_memory_buffer_alloc_init(buf: *mut c_char, len: usize);
}

mod radio_common;

bind_interrupts!(struct Irqs {
    RADIO => radio::InterruptHandler<peripherals::RADIO>;
});

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    info!("Hello world!");
    let mut config = embassy_nrf::config::Config::default();
    config.hfclk_source = embassy_nrf::config::HfclkSource::ExternalXtal;
    let p = embassy_nrf::init(config);

    info!("Starting BLE radio");
    let mut radio: Radio<'_, _> = Radio::new(p.RADIO, Irqs).into();

    let mut led = Output::new(p.P0_13, Level::Low, OutputDrive::Standard);
    led.set_high();

    radio.set_mode(Mode::BLE_1MBIT);
    radio.set_tx_power(TxPower::_0D_BM);
    radio.set_frequency(radio_common::FREQ);

    radio.set_access_address(radio_common::ADV_ADDRESS);
    radio.set_header_expansion(false);
    radio.set_crc_init(radio_common::ADV_CRC_INIT);
    radio.set_crc_poly(radio_common::CRC_POLY);

    unwrap!(spawner.spawn(transmit_and_blink(radio, led, Duration::from_millis(100))));
}

#[embassy_executor::task]
async fn transmit_and_blink(
    mut radio: Radio<'static, embassy_nrf::peripherals::RADIO>,
    mut led: Output<'static>,
    period: Duration,
) {
    info!("transmit_and_blink");

    // Memory buffer for mbedtls
    #[cfg(feature = "crypto-psa")]
    let mut buffer: [c_char; 4096 * 2] = [0; 4096 * 2];
    #[cfg(feature = "crypto-psa")]
    unsafe {
        mbedtls_memory_buffer_alloc_init(buffer.as_mut_ptr(), buffer.len());
    }

    let cred_i = CredentialRPK::new(radio_common::CRED_I.try_into().unwrap()).unwrap();
    let cred_r = CredentialRPK::new(radio_common::CRED_R.try_into().unwrap()).unwrap();

    let mut initiator = EdhocInitiator::new(lakers_crypto::default_crypto());

    // Send Message 1 over CoAP and convert the response to byte
    let c_i = generate_connection_identifier_cbor(&mut lakers_crypto::default_crypto());
    let (initiator, message_1) = initiator.prepare_message_1(Some(c_i), &None).unwrap();
    let pckt_1 = radio_common::Packet::new_from_slice(message_1.as_slice(), Some(0xf5u8))
        .expect("Buffer not long enough");

    let rcvd = radio_common::transmit_and_wait_response(&mut radio, pckt_1, Some(0xf5u8)).await;

    match rcvd {
        Ok(pckt_2) => {
            let message_2 = EdhocMessageBuffer::new_from_slice(&pckt_2.pdu[1..pckt_2.len]).unwrap();
            let (initiator, c_r, id_cred_r, ead_2) = initiator.parse_message_2(&message_2).unwrap();
            let valid_cred_r = credential_check_or_fetch(Some(cred_r), id_cred_r).unwrap();
            let initiator = initiator
                .verify_message_2(radio_common::I, cred_i, valid_cred_r)
                .unwrap();

            let (mut initiator, message_3, i_prk_out) = initiator
                .prepare_message_3(CredentialTransfer::ByReference, &None)
                .unwrap();

            radio_common::transmit_without_response(
                &mut radio,
                radio_common::Packet::new_from_slice(message_3.as_slice(), Some(c_r.as_slice()[0]))
                    .unwrap(),
            )
            .await;

            info!("Handshake completed. prk_out = {:X}", i_prk_out);
        }
        Err(_) => panic!("parsing error"),
    }
}
