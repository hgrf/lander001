use esp_idf_hal::delay::Delay;
use esp_idf_hal::gpio::{PinDriver};
use esp_idf_hal::prelude::*;
use esp_idf_hal::spi::{config::Config as SpiConfig, SpiDeviceDriver, SpiDriver};
use esp_idf_hal::units::FromValueType;

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Circle, PrimitiveStyle};

use mipidsi::options::ColorOrder;

fn next_rand(state: &mut u32) -> u32 {
    // xorshift32
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *state = x;
    x
}

fn main() {
    // It is necessary to call this function once. Otherwise, some patches to the runtime
    // implemented by esp-idf-sys might not link properly. See https://github.com/esp-rs/esp-idf-template/issues/71
    esp_idf_svc::sys::link_patches();

    // Bind the log crate to the ESP Logging facilities
    esp_idf_svc::log::EspLogger::initialize_default();

    log::info!("Initializing display...");

    let peripherals = Peripherals::take().unwrap();

    // SPI pins
    let sclk = peripherals.pins.gpio4;
    let mosi = peripherals.pins.gpio6;
    let dc_pin = peripherals.pins.gpio1;
    let rst_pin = peripherals.pins.gpio0;

    // Configure SPI
    let spi_config = SpiConfig::new()
        .baudrate(40.MHz().into())
        .data_mode(embedded_hal::spi::MODE_3);

    let spi_driver = SpiDriver::new(
        peripherals.spi2,
        sclk,
        mosi,
        None::<esp_idf_hal::gpio::AnyIOPin>, // No MISO
        &esp_idf_hal::spi::SpiDriverConfig::default(),
    )
    .unwrap();

    let spi_device = SpiDeviceDriver::new(spi_driver, None::<esp_idf_hal::gpio::AnyIOPin>, &spi_config).unwrap();

    let dc = PinDriver::output(dc_pin).unwrap();
    let mut rst = PinDriver::output(rst_pin).unwrap();
    let mut delay = Delay::new_default();
    rst.set_high().unwrap(); // Ensure reset pin is high before initialization

    // Create display interface
    let mut buffer = [0u8; 512];
    let di = mipidsi::interface::SpiInterface::new(spi_device, dc, &mut buffer);

    // Initialize the display
    let mut display = mipidsi::Builder::new(mipidsi::models::ST7789, di)
        .reset_pin(rst)
        .invert_colors(mipidsi::options::ColorInversion::Inverted)
        .display_size(240, 240)
        .color_order(ColorOrder::Rgb)
        .init(&mut delay)
        .unwrap();

    log::info!("Display initialized!");

    display.clear(Rgb565::BLACK).unwrap();

    let screen = display.bounding_box().size;
    let width = screen.width as i32;
    let height = screen.height as i32;

    let radius: i32 = 14;
    let diameter: u32 = (radius * 2) as u32;

    let mut rng: u32 = 0xA5A5_1234;
    let mut x = width / 2;
    let mut y = height / 2;

    let mut vx = ((next_rand(&mut rng) % 4) as i32) + 1;
    let mut vy = ((next_rand(&mut rng) % 4) as i32) + 1;
    if (next_rand(&mut rng) & 1) != 0 {
        vx = -vx;
    }
    if (next_rand(&mut rng) & 1) != 0 {
        vy = -vy;
    }

    log::info!("Starting animation...");

    loop {
        std::thread::sleep(std::time::Duration::from_millis(30));

        // Erase previous circle
        Circle::new(Point::new(x - radius, y - radius), diameter)
            .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
            .draw(&mut display)
            .unwrap();

        x += vx;
        y += vy;

        // Bounce and randomize speed a bit on impact
        if x - radius <= 0 || x + radius >= width {
            x = x.clamp(radius, width - radius);
            vx = -vx.signum() * ((((next_rand(&mut rng) % 4) as i32) + 1).max(1));
        }

        if y - radius <= 0 || y + radius >= height {
            y = y.clamp(radius, height - radius);
            vy = -vy.signum() * ((((next_rand(&mut rng) % 4) as i32) + 1).max(1));
        }

        // Draw new circle
        Circle::new(Point::new(x - radius, y - radius), diameter)
            .into_styled(PrimitiveStyle::with_fill(Rgb565::RED))
            .draw(&mut display)
            .unwrap();
    }
}
