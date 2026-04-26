use drive_74hc595::ShiftRegister;
use dummy_pin::DummyPin;
use embedded_graphics::image::{Image, ImageRawLE};
use embedded_graphics::mono_font::ascii::{FONT_10X20, FONT_6X10, FONT_8X13_BOLD};
use embedded_graphics::mono_font::MonoTextStyleBuilder;
use embedded_graphics::primitives::{
    Circle, Line, PrimitiveStyle, PrimitiveStyleBuilder, Rectangle, Triangle,
};
use embedded_graphics::text::Text;
use esp_idf_svc::hal::delay::Delay;
use esp_idf_svc::hal::gpio::PinDriver;
use esp_idf_svc::hal::ledc::{config::TimerConfig, LedcDriver, LedcTimerDriver};
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::hal::spi::{config::Config as SpiConfig, SpiDeviceDriver, SpiDriver};
use esp_idf_svc::hal::units::FromValueType;
use esp_idf_svc::hal::usb_serial::{UsbSerialConfig, UsbSerialDriver};
use esp_idf_sys::*;

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;

use mipidsi::options::ColorOrder;
use mipidsi::options::Orientation;
use mipidsi::options::Rotation;

#[path = "../shared/protocol.rs"]
mod protocol;

fn truncate_text(input: &str, max_chars: usize) -> String {
    let chars: Vec<char> = input.chars().collect();
    let out = if chars.len() <= max_chars {
        chars.into_iter().collect()
    } else if max_chars > 3 {
        let mut truncated: String = chars.into_iter().take(max_chars - 3).collect();
        truncated.push_str("...");
        truncated
    } else {
        chars.into_iter().take(max_chars).collect()
    };

    if out.is_empty() {
        String::from("-")
    } else {
        out
    }
}

fn wrap_text_with_ellipsis(input: &str, max_chars: usize, max_lines: usize) -> Vec<String> {
    if max_chars == 0 || max_lines == 0 {
        return Vec::new();
    }

    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut truncated = false;

    'word_loop: for word in input.split_whitespace() {
        let word_len = word.chars().count();
        let mut segments: Vec<String> = Vec::new();

        if word_len <= max_chars {
            segments.push(word.to_string());
        } else {
            let chars: Vec<char> = word.chars().collect();
            let mut idx = 0;
            while idx < chars.len() {
                let end = (idx + max_chars).min(chars.len());
                segments.push(chars[idx..end].iter().collect());
                idx = end;
            }
        }

        for segment in segments {
            if current.is_empty() {
                current = segment;
                continue;
            }

            let current_len = current.chars().count();
            let segment_len = segment.chars().count();
            if current_len + 1 + segment_len <= max_chars {
                current.push(' ');
                current.push_str(&segment);
            } else {
                lines.push(std::mem::take(&mut current));
                if lines.len() >= max_lines {
                    truncated = true;
                    break 'word_loop;
                }
                current = segment;
            }
        }
    }

    if !current.is_empty() {
        if lines.len() < max_lines {
            lines.push(std::mem::take(&mut current));
        } else {
            truncated = true;
        }
    }

    if lines.is_empty() {
        lines.push(String::from("-"));
    }

    if lines.len() > max_lines {
        lines.truncate(max_lines);
        truncated = true;
    }

    if truncated {
        let last_idx = lines.len().saturating_sub(1);
        let suffix = "...";
        let keep_chars = max_chars.saturating_sub(suffix.len());
        let mut base: String = lines[last_idx].chars().take(keep_chars).collect();
        while base.ends_with(' ') {
            base.pop();
        }

        if base.is_empty() {
            lines[last_idx] = suffix.chars().take(max_chars).collect();
        } else {
            base.push_str(suffix);
            lines[last_idx] = base;
        }
    }

    lines
}

fn draw_mail_icon<D>(display: &mut D, origin: Point) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    let style = PrimitiveStyleBuilder::new()
        .stroke_color(Rgb565::WHITE)
        .stroke_width(3)
        .fill_color(Rgb565::new(4, 18, 10))
        .build();
    Rectangle::new(origin, Size::new(52, 36))
        .into_styled(style)
        .draw(display)?;
    Line::new(origin, origin + Point::new(26, 18))
        .into_styled(PrimitiveStyle::with_stroke(Rgb565::WHITE, 2))
        .draw(display)?;
    Line::new(origin + Point::new(52, 0), origin + Point::new(26, 18))
        .into_styled(PrimitiveStyle::with_stroke(Rgb565::WHITE, 2))
        .draw(display)?;
    Ok(())
}

fn draw_chat_icon<D>(display: &mut D, origin: Point) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    let bubble_style = PrimitiveStyleBuilder::new()
        .stroke_color(Rgb565::WHITE)
        .stroke_width(3)
        .fill_color(Rgb565::new(2, 24, 9))
        .build();
    Rectangle::new(origin, Size::new(50, 34))
        .into_styled(bubble_style)
        .draw(display)?;
    Triangle::new(
        origin + Point::new(10, 34),
        origin + Point::new(20, 34),
        origin + Point::new(14, 45),
    )
    .into_styled(PrimitiveStyle::with_fill(Rgb565::new(2, 24, 9)))
    .draw(display)?;
    for i in 0..3 {
        Circle::new(origin + Point::new(10 + i * 14, 12), 6)
            .into_styled(PrimitiveStyle::with_fill(Rgb565::WHITE))
            .draw(display)?;
    }
    Ok(())
}

fn draw_calendar_icon<D>(display: &mut D, origin: Point) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    Rectangle::new(origin, Size::new(50, 44))
        .into_styled(
            PrimitiveStyleBuilder::new()
                .stroke_color(Rgb565::WHITE)
                .stroke_width(3)
                .fill_color(Rgb565::new(6, 8, 22))
                .build(),
        )
        .draw(display)?;
    Rectangle::new(origin, Size::new(50, 12))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::RED))
        .draw(display)?;
    Ok(())
}

fn draw_system_icon<D>(display: &mut D, origin: Point) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    Circle::new(origin, 44)
        .into_styled(
            PrimitiveStyleBuilder::new()
                .stroke_color(Rgb565::WHITE)
                .stroke_width(3)
                .fill_color(Rgb565::new(10, 10, 10))
                .build(),
        )
        .draw(display)?;
    for delta in [0, 12, 24, 36] {
        Line::new(origin + Point::new(22, 4), origin + Point::new(22, 40))
            .translate(Point::new(delta - 18, 0))
            .into_styled(PrimitiveStyle::with_stroke(Rgb565::YELLOW, 2))
            .draw(display)?;
    }
    Ok(())
}

fn render_notification_card<D>(display: &mut D, evt: &protocol::pb::NotificationEvent)
where
    D: DrawTarget<Color = Rgb565>,
    D::Error: core::fmt::Debug,
{
    display.clear(Rgb565::BLACK).unwrap();

    let bundle = evt.source_bundle_id.to_ascii_lowercase();
    let icon_hint = evt.app_icon_hint.to_ascii_lowercase();
    let app = evt.source_app.to_ascii_lowercase();

    let accent = if bundle.contains("whatsapp")
        || icon_hint.contains("whatsapp")
        || app.contains("whatsapp")
    {
        Rgb565::new(4, 28, 10)
    } else if bundle.contains("outlook") || bundle.contains("mail") || icon_hint.contains("mail") {
        Rgb565::new(2, 12, 28)
    } else if bundle.contains("calendar") || icon_hint.contains("calendar") {
        Rgb565::RED
    } else {
        Rgb565::new(18, 10, 4)
    };

    Rectangle::new(Point::new(10, 14), Size::new(220, 212))
        .into_styled(
            PrimitiveStyleBuilder::new()
                .stroke_color(accent)
                .stroke_width(3)
                .build(),
        )
        .draw(display)
        .unwrap();
    Rectangle::new(Point::new(10, 14), Size::new(220, 34))
        .into_styled(PrimitiveStyle::with_fill(accent))
        .draw(display)
        .unwrap();
    Rectangle::new(Point::new(86, 60), Size::new(132, 92))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
        .draw(display)
        .unwrap();

    if bundle.contains("whatsapp") || icon_hint.contains("whatsapp") || app.contains("whatsapp") {
        draw_chat_icon(display, Point::new(24, 68)).unwrap();
    } else if bundle.contains("outlook") || bundle.contains("mail") || icon_hint.contains("mail") {
        draw_mail_icon(display, Point::new(24, 70)).unwrap();
    } else if bundle.contains("calendar") || icon_hint.contains("calendar") {
        draw_calendar_icon(display, Point::new(24, 66)).unwrap();
    } else {
        draw_system_icon(display, Point::new(24, 68)).unwrap();
    }

    let title_style = MonoTextStyleBuilder::new()
        .font(&FONT_8X13_BOLD)
        .text_color(Rgb565::WHITE)
        .build();
    let sender_style = MonoTextStyleBuilder::new()
        .font(&FONT_10X20)
        .text_color(Rgb565::WHITE)
        .build();
    let body_style = MonoTextStyleBuilder::new()
        .font(&FONT_6X10)
        .text_color(Rgb565::WHITE)
        .build();

    let header = if evt.source_app.is_empty() {
        "Notification".to_string()
    } else {
        truncate_text(&evt.source_app, 24)
    };
    let title_text = if !evt.title.is_empty() {
        evt.title.replace('\n', " ")
    } else if !evt.sender_name.is_empty() {
        evt.sender_name.replace('\n', " ")
    } else {
        "Someone".to_string()
    };
    let title_lines = wrap_text_with_ellipsis(&title_text, 12, 2);
    let body_source = if !evt.body.is_empty() {
        evt.body.replace('\n', " ")
    } else {
        evt.title.replace('\n', " ")
    };
    let title_start_y = 96;
    let title_line_height = 20;
    let handle_y = title_start_y + (title_lines.len() as i32) * title_line_height + 2;
    let body_start_y = if !evt.sender_handle.is_empty() {
        handle_y + 16
    } else {
        handle_y + 4
    };
    let body_bottom_y = 214;
    let body_line_height = 11;
    let body_max_lines = if body_start_y > body_bottom_y {
        1
    } else {
        (((body_bottom_y - body_start_y) / body_line_height) + 1)
            .max(1)
            .min(7)
    } as usize;
    let snippet_lines = wrap_text_with_ellipsis(&body_source, 21, body_max_lines);

    Text::new(&header, Point::new(18, 36), title_style)
        .draw(display)
        .unwrap();
    for (idx, line) in title_lines.iter().enumerate() {
        let y = title_start_y + (idx as i32) * title_line_height;
        Text::new(line, Point::new(92, y), sender_style)
            .draw(display)
            .unwrap();
    }

    if !evt.sender_handle.is_empty() {
        Text::new(
            &truncate_text(&evt.sender_handle, 21),
            Point::new(92, handle_y),
            body_style,
        )
        .draw(display)
        .unwrap();
    }

    for (idx, line) in snippet_lines.iter().enumerate() {
        let y = body_start_y + (idx as i32) * body_line_height;
        Text::new(line, Point::new(92, y), body_style)
            .draw(display)
            .unwrap();
    }

    // Let FreeRTOS schedule IDLE task after a burst of SPI draw calls.
    std::thread::yield_now();
}

fn write_wire_message(usb_serial: &mut UsbSerialDriver<'_>, msg: &protocol::pb::WireMessage) {
    let frame = match protocol::encode_frame(msg) {
        Ok(f) => f,
        Err(err) => {
            log::warn!("Failed to encode wire message: {}", err);
            return;
        }
    };
    let mut written = 0;
    while written < frame.len() {
        match usb_serial.write(&frame[written..], esp_idf_svc::hal::delay::NON_BLOCK) {
            Ok(0) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Ok(n) => written += n,
            Err(err) => {
                log::warn!("USB serial write error: {}", err);
                break;
            }
        }
    }
}

fn send_ack(usb_serial: &mut UsbSerialDriver<'_>, original_msg_id: u32, ok: bool, error: String) {
    let ack = protocol::pb::WireMessage {
        msg_id: original_msg_id.saturating_add(10_000),
        payload: Some(protocol::pb::wire_message::Payload::Ack(
            protocol::pb::Ack {
                msg_id: original_msg_id,
                ok,
                error,
            },
        )),
    };

    write_wire_message(usb_serial, &ack);
}

struct LedAnimator {
    pattern_id: u32,
    repeats_left: u32,
    step: u32,
    phase: bool,
    next_at: Option<std::time::Instant>,
}

impl LedAnimator {
    fn new() -> Self {
        Self {
            pattern_id: 0,
            repeats_left: 0,
            step: 0,
            phase: false,
            next_at: None,
        }
    }

    fn schedule(&mut self, pattern_id: u32, repeats: u32) {
        self.pattern_id = pattern_id;
        self.repeats_left = repeats.max(1);
        self.step = 0;
        self.phase = false;
        self.next_at = Some(std::time::Instant::now());
    }

    fn tick<OE, SER, SRCLR, SRCLK, RCLK>(
        &mut self,
        drive: &mut ShiftRegister<OE, SER, SRCLR, SRCLK, RCLK>,
        now: std::time::Instant,
    ) where
        OE: embedded_hal::digital::OutputPin,
        SER: embedded_hal::digital::OutputPin,
        SRCLR: embedded_hal::digital::OutputPin,
        SRCLK: embedded_hal::digital::OutputPin,
        RCLK: embedded_hal::digital::OutputPin,
    {
        if self.repeats_left == 0 {
            return;
        }

        let Some(next) = self.next_at else {
            return;
        };
        if now < next {
            return;
        }

        let wait_ms = match self.pattern_id {
            1 => {
                drive.load(1_u8 << (self.step % 8));
                self.step += 1;
                if self.step >= 8 {
                    self.step = 0;
                    self.repeats_left = self.repeats_left.saturating_sub(1);
                }
                60
            }
            2 => {
                if self.phase {
                    drive.load(0b0101_0101);
                    self.repeats_left = self.repeats_left.saturating_sub(1);
                } else {
                    drive.load(0b1010_1010);
                }
                self.phase = !self.phase;
                70
            }
            3 => {
                let i = self.step % 4;
                let m = (1_u8 << i) | (1_u8 << (7 - i));
                drive.load(m);
                self.step += 1;
                if self.step >= 4 {
                    self.step = 0;
                    self.repeats_left = self.repeats_left.saturating_sub(1);
                }
                65
            }
            _ => {
                if self.phase {
                    drive.load(0x00);
                    self.repeats_left = self.repeats_left.saturating_sub(1);
                } else {
                    drive.load(0xFF);
                }
                self.phase = !self.phase;
                80
            }
        };

        if self.repeats_left == 0 {
            drive.load(0x00);
            self.next_at = None;
        } else {
            self.next_at = Some(now + std::time::Duration::from_millis(wait_ms));
        }
    }
}

fn apply_wire_message<D, OE, SER, SRCLR, SRCLK, RCLK>(
    msg: protocol::pb::WireMessage,
    servo: &mut LedcDriver<'_>,
    _drive: &mut ShiftRegister<OE, SER, SRCLR, SRCLK, RCLK>,
    led_animator: &mut LedAnimator,
    display: &mut D,
    cat: &ImageRawLE<Rgb565>,
    cat2: &ImageRawLE<Rgb565>,
    cat3: &ImageRawLE<Rgb565>,
    notification_hold_until: &mut Option<std::time::Instant>,
) where
    D: DrawTarget<Color = Rgb565>,
    D::Error: core::fmt::Debug,
    OE: embedded_hal::digital::OutputPin,
    SER: embedded_hal::digital::OutputPin,
    SRCLR: embedded_hal::digital::OutputPin,
    SRCLK: embedded_hal::digital::OutputPin,
    RCLK: embedded_hal::digital::OutputPin,
{
    if let Some(payload) = msg.payload {
        match payload {
            protocol::pb::wire_message::Payload::SetServo(cmd) => {
                servo
                    .set_duty(servo_angle_to_duty(cmd.angle_deg, servo.get_max_duty()))
                    .unwrap();
                log::info!("protobuf: set_servo to {:.1} deg", cmd.angle_deg);
            }
            protocol::pb::wire_message::Payload::LedAnimation(cmd) => {
                led_animator.schedule(cmd.pattern_id, cmd.repeats);
                log::info!(
                    "protobuf: led_animation pattern={} repeats={}",
                    cmd.pattern_id,
                    cmd.repeats
                );
            }
            protocol::pb::wire_message::Payload::ShowIcon(cmd) => match cmd.icon_id.as_str() {
                "cat1" => Image::new(cat, Point::zero()).draw(display).unwrap(),
                "cat2" => Image::new(cat2, Point::zero()).draw(display).unwrap(),
                "cat3" => Image::new(cat3, Point::zero()).draw(display).unwrap(),
                _ => log::warn!("protobuf: unknown icon_id '{}', ignoring", cmd.icon_id),
            },
            protocol::pb::wire_message::Payload::Notification(evt) => {
                render_notification_card(display, &evt);
                *notification_hold_until =
                    Some(std::time::Instant::now() + std::time::Duration::from_secs(5));
                log::info!(
                    "protobuf: notification '{}' from '{}' (urgency {})",
                    evt.source_app,
                    evt.sender_name,
                    evt.urgency
                );
            }
            protocol::pb::wire_message::Payload::Ping(_) => {
                log::info!("protobuf: ping received");
            }
            protocol::pb::wire_message::Payload::IconChunk(_) => {
                log::info!("protobuf: icon chunk received (not applied yet)");
            }
            protocol::pb::wire_message::Payload::Ack(_) => {
                log::info!("protobuf: ack received");
            }
        }
    }
}

fn drain_commands<D, OE, SER, SRCLR, SRCLK, RCLK>(
    cmd_rx: &std::sync::mpsc::Receiver<protocol::pb::WireMessage>,
    servo: &mut LedcDriver<'_>,
    drive: &mut ShiftRegister<OE, SER, SRCLR, SRCLK, RCLK>,
    led_animator: &mut LedAnimator,
    display: &mut D,
    cat: &ImageRawLE<Rgb565>,
    cat2: &ImageRawLE<Rgb565>,
    cat3: &ImageRawLE<Rgb565>,
    notification_hold_until: &mut Option<std::time::Instant>,
) where
    D: DrawTarget<Color = Rgb565>,
    D::Error: core::fmt::Debug,
    OE: embedded_hal::digital::OutputPin,
    SER: embedded_hal::digital::OutputPin,
    SRCLR: embedded_hal::digital::OutputPin,
    SRCLK: embedded_hal::digital::OutputPin,
    RCLK: embedded_hal::digital::OutputPin,
{
    while let Ok(msg) = cmd_rx.try_recv() {
        apply_wire_message(
            msg,
            servo,
            drive,
            led_animator,
            display,
            cat,
            cat2,
            cat3,
            notification_hold_until,
        );
    }
}

fn sleep_with_command_pump<D, OE, SER, SRCLR, SRCLK, RCLK>(
    duration: std::time::Duration,
    cmd_rx: &std::sync::mpsc::Receiver<protocol::pb::WireMessage>,
    servo: &mut LedcDriver<'_>,
    drive: &mut ShiftRegister<OE, SER, SRCLR, SRCLK, RCLK>,
    led_animator: &mut LedAnimator,
    display: &mut D,
    cat: &ImageRawLE<Rgb565>,
    cat2: &ImageRawLE<Rgb565>,
    cat3: &ImageRawLE<Rgb565>,
    notification_hold_until: &mut Option<std::time::Instant>,
) where
    D: DrawTarget<Color = Rgb565>,
    D::Error: core::fmt::Debug,
    OE: embedded_hal::digital::OutputPin,
    SER: embedded_hal::digital::OutputPin,
    SRCLR: embedded_hal::digital::OutputPin,
    SRCLK: embedded_hal::digital::OutputPin,
    RCLK: embedded_hal::digital::OutputPin,
{
    let mut remaining_ms = duration.as_millis() as u64;
    while remaining_ms > 0 {
        drain_commands(
            cmd_rx,
            servo,
            drive,
            led_animator,
            display,
            cat,
            cat2,
            cat3,
            notification_hold_until,
        );
        led_animator.tick(drive, std::time::Instant::now());
        let slice_ms = remaining_ms.min(20);
        std::thread::sleep(std::time::Duration::from_millis(slice_ms));
        remaining_ms -= slice_ms;
    }
}

fn servo_angle_to_duty(angle_deg: f32, max_duty: u32) -> u32 {
    let clamped = angle_deg.clamp(0.0, 270.0);
    let min_pulse_us = 500.0_f32;
    let max_pulse_us = 2500.0_f32;
    let period_us = 20_000.0_f32; // 50 Hz
    let pulse_us = min_pulse_us + (clamped / 270.0) * (max_pulse_us - min_pulse_us);
    ((pulse_us / period_us) * (max_duty as f32)) as u32
}

#[inline]
fn gpio_reset_without_pull(pin: gpio_num_t) -> Result<(), EspError> {
    let cfg = gpio_config_t {
        pin_bit_mask: (1u64 << pin),
        mode: esp_idf_sys::gpio_mode_t_GPIO_MODE_DISABLE,
        pull_up_en: esp_idf_sys::gpio_pullup_t_GPIO_PULLUP_DISABLE,
        pull_down_en: esp_idf_sys::gpio_pulldown_t_GPIO_PULLDOWN_DISABLE,
        intr_type: esp_idf_sys::gpio_int_type_t_GPIO_INTR_DISABLE,
    };

    unsafe {
        esp!(gpio_config(&cfg))?;
    }
    Ok(())
}

fn main() {
    // It is necessary to call this function once. Otherwise, some patches to the runtime
    // implemented by esp-idf-sys might not link properly. See https://github.com/esp-rs/esp-idf-template/issues/71
    esp_idf_svc::sys::link_patches();

    // Bind the log crate to the ESP Logging facilities
    esp_idf_svc::log::EspLogger::initialize_default();

    if let Err(err) = protocol::startup_protocol_self_check() {
        log::warn!("Protocol self-check failed: {}", err);
    }

    log::info!("Initializing display...");

    let peripherals = Peripherals::take().unwrap();

    let servo_timer_cfg = TimerConfig::new()
        .frequency(50.Hz().into())
        .resolution(esp_idf_svc::hal::ledc::config::Resolution::Bits14);
    let servo_timer = LedcTimerDriver::new(peripherals.ledc.timer0, &servo_timer_cfg).unwrap();
    let mut servo = LedcDriver::new(
        peripherals.ledc.channel0,
        &servo_timer,
        peripherals.pins.gpio5,
    )
    .unwrap();
    servo
        .set_duty(servo_angle_to_duty(90.0, servo.get_max_duty()))
        .unwrap();
    log::info!("Servo initialized on GPIO5 at 90 degrees");

    gpio_reset_without_pull(gpio_num_t_GPIO_NUM_20).unwrap();
    gpio_reset_without_pull(gpio_num_t_GPIO_NUM_21).unwrap();
    let mut drive = ShiftRegister::new(
        DummyPin::new_high(),
        PinDriver::output(peripherals.pins.gpio10).unwrap(),
        DummyPin::new_high(),
        PinDriver::output(peripherals.pins.gpio21).unwrap(),
        PinDriver::output(peripherals.pins.gpio20).unwrap(),
    );
    drive.begin();
    drive.enable_output();
    drive.load(0x01);

    let usb_cfg = UsbSerialConfig::new()
        .rx_buffer_size(1024)
        .tx_buffer_size(512);
    let mut usb_serial = UsbSerialDriver::new(
        peripherals.usb_serial,
        peripherals.pins.gpio18,
        peripherals.pins.gpio19,
        &usb_cfg,
    )
    .unwrap();

    let (cmd_tx, cmd_rx) = std::sync::mpsc::sync_channel::<protocol::pb::WireMessage>(16);
    std::thread::spawn(move || {
        let mut decoder = protocol::StreamDecoder::new();
        let mut read_buf = [0_u8; 128];

        loop {
            if !usb_serial.is_connected() {
                std::thread::sleep(std::time::Duration::from_millis(25));
                continue;
            }

            match usb_serial.read(&mut read_buf, esp_idf_svc::hal::delay::NON_BLOCK) {
                Ok(0) => std::thread::sleep(std::time::Duration::from_millis(10)),
                Ok(n) => {
                    decoder.push_bytes(&read_buf[..n]);
                    while let Some(result) = decoder.next_message() {
                        match result {
                            Ok(msg) => {
                                let is_ack = matches!(
                                    msg.payload,
                                    Some(protocol::pb::wire_message::Payload::Ack(_))
                                );
                                let msg_id = msg.msg_id;

                                if cmd_tx.try_send(msg).is_err() {
                                    log::warn!("Dropping protobuf message: command queue full");
                                    if !is_ack {
                                        send_ack(
                                            &mut usb_serial,
                                            msg_id,
                                            false,
                                            "command queue full".to_string(),
                                        );
                                    }
                                } else if !is_ack {
                                    send_ack(&mut usb_serial, msg_id, true, String::new());
                                }
                            }
                            Err(err) => log::warn!("Dropping invalid protobuf frame: {}", err),
                        }
                    }
                }
                Err(err) => {
                    log::warn!("USB serial read error: {}", err);
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
            }
        }
    });

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
        None::<esp_idf_svc::hal::gpio::AnyIOPin>, // No MISO
        &esp_idf_svc::hal::spi::SpiDriverConfig::default(),
    )
    .unwrap();

    let spi_device =
        SpiDeviceDriver::new(spi_driver, None::<esp_idf_svc::hal::gpio::AnyIOPin>, &spi_config).unwrap();

    let dc = PinDriver::output(dc_pin).unwrap();
    let mut rst = PinDriver::output(rst_pin).unwrap();
    let mut delay = Delay::new_default();
    rst.set_high().unwrap(); // Ensure reset pin is high before initialization

    // Create display interface
    let mut buffer = vec![0u8; 8192];
    let di = mipidsi::interface::SpiInterface::new(spi_device, dc, &mut buffer);

    // Initialize the display
    let orientation = Orientation::new();
    let mut display = mipidsi::Builder::new(mipidsi::models::ST7789, di)
        .reset_pin(rst)
        .invert_colors(mipidsi::options::ColorInversion::Inverted)
        .display_size(240, 240)
        .orientation(orientation.rotate(Rotation::Deg270))
        .color_order(ColorOrder::Rgb)
        .init(&mut delay)
        .unwrap();

    log::info!("Display initialized!");

    display.clear(Rgb565::BLACK).unwrap();

    let cat = ImageRawLE::<Rgb565>::new(
        include_bytes!(concat!(env!("OUT_DIR"), "/cats/cat1_rgb565_le.bin")),
        240,
    );
    let cat2 = ImageRawLE::<Rgb565>::new(
        include_bytes!(concat!(env!("OUT_DIR"), "/cats/cat2_rgb565_le.bin")),
        240,
    );
    let cat3 = ImageRawLE::<Rgb565>::new(
        include_bytes!(concat!(env!("OUT_DIR"), "/cats/cat3_rgb565_le.bin")),
        240,
    );

    let images = [&cat, &cat2, &cat3];
    let mut notification_hold_until: Option<std::time::Instant> = None;
    let mut led_animator = LedAnimator::new();
    log::info!("cycling cat1/cat2/cat3 with random interval (100..=1000 ms)");

    loop {
        if let Some(until) = notification_hold_until {
            if std::time::Instant::now() < until {
                sleep_with_command_pump(
                    std::time::Duration::from_millis(20),
                    &cmd_rx,
                    &mut servo,
                    &mut drive,
                    &mut led_animator,
                    &mut display,
                    &cat,
                    &cat2,
                    &cat3,
                    &mut notification_hold_until,
                );
                continue;
            }
            notification_hold_until = None;
        }

        drain_commands(
            &cmd_rx,
            &mut servo,
            &mut drive,
            &mut led_animator,
            &mut display,
            &cat,
            &cat2,
            &cat3,
            &mut notification_hold_until,
        );
        led_animator.tick(&mut drive, std::time::Instant::now());

        for img in images.iter() {
            drain_commands(
                &cmd_rx,
                &mut servo,
                &mut drive,
                &mut led_animator,
                &mut display,
                &cat,
                &cat2,
                &cat3,
                &mut notification_hold_until,
            );
            led_animator.tick(&mut drive, std::time::Instant::now());

            if let Some(until) = notification_hold_until {
                if std::time::Instant::now() < until {
                    break;
                }
            }

            Image::new(*img, Point::zero()).draw(&mut display).unwrap();

            sleep_with_command_pump(
                std::time::Duration::from_millis(300),
                &cmd_rx,
                &mut servo,
                &mut drive,
                &mut led_animator,
                &mut display,
                &cat,
                &cat2,
                &cat3,
                &mut notification_hold_until,
            );

            if let Some(until) = notification_hold_until {
                if std::time::Instant::now() < until {
                    break;
                }
            }
        }
    }
}
