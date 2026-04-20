#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::io::{BufRead, BufReader};
use std::io::{Read, Write};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use eframe::egui;
use egui_phosphor::regular;
use serialport::SerialPort;

#[path = "../../shared/protocol.rs"]
mod protocol;

use protocol::pb;

fn list_ports() -> Vec<String> {
    let mut names = serialport::available_ports()
        .map(|ports| ports.into_iter().map(|p| p.port_name).collect::<Vec<_>>())
        .unwrap_or_default();

    fn port_priority(name: &str) -> u8 {
        let lower = name.to_ascii_lowercase();

        if lower.contains("/dev/cu.usb") || lower.contains("/dev/ttyusb") {
            0
        } else if lower.contains("/dev/cu.") || lower.contains("/dev/tty") {
            1
        } else {
            2
        }
    }

    names.sort_by(|a, b| {
        let pa = port_priority(a);
        let pb = port_priority(b);
        pa.cmp(&pb).then_with(|| a.cmp(b))
    });

    names
}

struct SharedController {
    ports: Vec<String>,
    selected_port_idx: usize,
    conn: Option<Connection>,
    next_msg_id: u32,
    logs: Vec<String>,
}

impl Default for SharedController {
    fn default() -> Self {
        Self {
            ports: list_ports(),
            selected_port_idx: 0,
            conn: None,
            next_msg_id: 1,
            logs: Vec::new(),
        }
    }
}

impl SharedController {
    fn log(&mut self, text: impl Into<String>) {
        let ts = now_unix_ms();
        self.logs.push(format!("[{}] {}", ts, text.into()));
        if self.logs.len() > 200 {
            let drop_count = self.logs.len() - 200;
            self.logs.drain(0..drop_count);
        }
    }

    fn selected_port_name(&self) -> Option<&str> {
        self.ports.get(self.selected_port_idx).map(String::as_str)
    }

    fn is_connected(&self) -> bool {
        self.conn.is_some()
    }

    fn with_conn<F>(&mut self, mut f: F)
    where
        F: FnMut(&mut Self, &mut Connection) -> Result<()>,
    {
        if let Some(mut conn) = self.conn.take() {
            let result = f(self, &mut conn);
            self.conn = Some(conn);
            if let Err(err) = result {
                self.log(format!("Error: {}", err));
            }
        } else {
            self.log("Not connected");
        }
    }

    fn refresh_ports(&mut self) {
        self.ports = list_ports();
        if self.selected_port_idx >= self.ports.len() {
            self.selected_port_idx = 0;
        }
        self.log(format!("Found {} serial port(s)", self.ports.len()));
    }

    fn connect(&mut self) {
        if self.conn.is_some() {
            self.log("Already connected");
            return;
        }

        self.refresh_ports();

        let Some(port_name) = self.selected_port_name().map(str::to_string) else {
            self.log("No serial port selected");
            return;
        };

        match Connection::new(port_name.clone()) {
            Ok(conn) => {
                self.conn = Some(conn);
                self.log(format!("Connected to {}", port_name));
                self.send_ping();
            }
            Err(err) => self.log(format!("Failed to connect: {}", err)),
        }
    }

    fn disconnect(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.log(format!("Disconnected from {}", conn.port_name));
        } else {
            self.log("Already disconnected");
        }
    }

    fn send_ping(&mut self) {
        self.with_conn(|this, conn| {
            send_ping_message(conn, &mut this.next_msg_id)?;
            this.log("Ping ACKed");
            Ok(())
        });
    }

    fn send_servo(&mut self, angle_deg: f32) {
        self.with_conn(|this, conn| {
            send_servo_message(conn, &mut this.next_msg_id, angle_deg)?;
            this.log(format!("SetServo {:.1} deg ACKed", angle_deg));
            Ok(())
        });
    }

    fn send_led(&mut self, pattern_id: u32, repeats: u32) {
        self.with_conn(|this, conn| {
            send_led_message(conn, &mut this.next_msg_id, pattern_id, repeats)?;
            this.log(format!("LedAnimation p={} r={} ACKed", pattern_id, repeats));
            Ok(())
        });
    }

    fn send_icon(&mut self, icon_id: String) {
        self.with_conn(|this, conn| {
            send_icon_message(conn, &mut this.next_msg_id, &icon_id)?;
            this.log(format!("ShowIcon '{}' ACKed", icon_id));
            Ok(())
        });
    }

    fn send_notification_and_animation(&mut self, preset: &str, from: &str, text: &str) {
        let (source_app, source_bundle_id, category, title, sender_name, app_icon_hint) =
            default_notification_for_preset(preset, from, text);
        let preset = preset.to_string();
        let text = text.to_string();

        self.with_conn(|this, conn| {
            let notif_id = this.next_msg_id;
            send_notification_message(
                conn,
                &mut this.next_msg_id,
                pb::NotificationEvent {
                    id: format!("gui-{}-{}", preset, notif_id),
                    source_app: source_app.clone(),
                    title: title.clone(),
                    body: text.clone(),
                    urgency: pb::Urgency::Normal as i32,
                    category,
                    source_bundle_id: source_bundle_id.clone(),
                    sender_name: sender_name.clone(),
                    sender_handle: String::new(),
                    app_icon_hint: app_icon_hint.clone(),
                },
            )?;
            this.log(format!("Notification '{}' ACKed", source_app));
            Ok(())
        });

        self.with_conn(|this, conn| {
            send_notification_animation(conn, &mut this.next_msg_id, category)?;
            Ok(())
        });
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn default_notification_for_preset(preset: &str, from: &str, text: &str) -> (String, String, i32, String, String, String) {
    match preset {
        "whatsapp" => (
            "WhatsApp".to_string(),
            "net.whatsapp.WhatsApp".to_string(),
            pb::Category::Chat as i32,
            format!("{}: {}", from, text),
            from.to_string(),
            "whatsapp".to_string(),
        ),
        "mail" => (
            "Outlook".to_string(),
            "com.microsoft.Outlook".to_string(),
            pb::Category::Mail as i32,
            format!("Mail from {}", from),
            from.to_string(),
            "mail".to_string(),
        ),
        "calendar" => (
            "Calendar".to_string(),
            "com.apple.calendar".to_string(),
            pb::Category::Calendar as i32,
            format!("Meeting with {}", from),
            from.to_string(),
            "calendar".to_string(),
        ),
        _ => (
            "System".to_string(),
            "com.example.system".to_string(),
            pb::Category::System as i32,
            format!("System event from {}", from),
            from.to_string(),
            "system".to_string(),
        ),
    }
}

fn animation_profile_for_category(category: i32) -> (u32, u32, f32, f32) {
    if category == pb::Category::Chat as i32 {
        (2, 6, 200.0, 90.0)
    } else if category == pb::Category::Mail as i32 {
        (1, 3, 150.0, 90.0)
    } else if category == pb::Category::Calendar as i32 {
        (3, 3, 60.0, 90.0)
    } else {
        (4, 3, 120.0, 90.0)
    }
}

fn take_next_msg_id(next_msg_id: &mut u32) -> u32 {
    let msg_id = *next_msg_id;
    *next_msg_id = next_msg_id.saturating_add(1);
    msg_id
}

fn send_ping_message(conn: &mut Connection, next_msg_id: &mut u32) -> Result<()> {
    let msg = pb::WireMessage {
        msg_id: take_next_msg_id(next_msg_id),
        payload: Some(pb::wire_message::Payload::Ping(pb::Ping {
            unix_ms: now_unix_ms(),
        })),
    };
    conn.send_and_wait_ack(msg)
}

fn send_servo_message(conn: &mut Connection, next_msg_id: &mut u32, angle_deg: f32) -> Result<()> {
    let msg = pb::WireMessage {
        msg_id: take_next_msg_id(next_msg_id),
        payload: Some(pb::wire_message::Payload::SetServo(pb::SetServo { angle_deg })),
    };
    conn.send_and_wait_ack(msg)
}

fn send_led_message(
    conn: &mut Connection,
    next_msg_id: &mut u32,
    pattern_id: u32,
    repeats: u32,
) -> Result<()> {
    let msg = pb::WireMessage {
        msg_id: take_next_msg_id(next_msg_id),
        payload: Some(pb::wire_message::Payload::LedAnimation(pb::LedAnimation {
            pattern_id,
            repeats,
        })),
    };
    conn.send_and_wait_ack(msg)
}

fn send_icon_message(conn: &mut Connection, next_msg_id: &mut u32, icon_id: &str) -> Result<()> {
    let msg = pb::WireMessage {
        msg_id: take_next_msg_id(next_msg_id),
        payload: Some(pb::wire_message::Payload::ShowIcon(pb::ShowIcon {
            icon_id: icon_id.to_string(),
        })),
    };
    conn.send_and_wait_ack(msg)
}

fn send_notification_message(
    conn: &mut Connection,
    next_msg_id: &mut u32,
    event: pb::NotificationEvent,
) -> Result<()> {
    let msg = pb::WireMessage {
        msg_id: take_next_msg_id(next_msg_id),
        payload: Some(pb::wire_message::Payload::Notification(event)),
    };
    conn.send_and_wait_ack(msg)
}

struct Connection {
    port_name: String,
    port: Box<dyn SerialPort>,
    decoder: protocol::StreamDecoder,
}

impl Connection {
    fn new(port_name: String) -> Result<Self> {
        let port = serialport::new(&port_name, 115_200)
            .timeout(Duration::from_millis(150))
            .open()
            .with_context(|| format!("failed to open serial port {}", port_name))?;

        Ok(Self {
            port_name,
            port,
            decoder: protocol::StreamDecoder::new(),
        })
    }

    fn send_message(&mut self, msg: pb::WireMessage) -> Result<()> {
        let frame = protocol::encode_frame(&msg).context("failed to encode framed protobuf message")?;
        self.port
            .write_all(&frame)
            .context("failed to write framed protobuf message")?;
        self.port.flush().context("failed to flush serial port")?;
        Ok(())
    }

    fn wait_for_ack(&mut self, expected_msg_id: u32, timeout: Duration) -> Result<()> {
        let start = Instant::now();
        let mut temp = [0_u8; 256];

        while start.elapsed() < timeout {
            match self.port.read(&mut temp) {
                Ok(n) if n > 0 => {
                    self.decoder.push_bytes(&temp[..n]);
                    while let Some(result) = self.decoder.next_message() {
                        let msg = result.context("failed to decode inbound frame")?;
                        if let Some(pb::wire_message::Payload::Ack(ack)) = msg.payload {
                            if ack.msg_id == expected_msg_id {
                                if ack.ok {
                                    return Ok(());
                                }
                                bail!("firmware NACK for {}: {}", expected_msg_id, ack.error);
                            }
                        }
                    }
                }
                Ok(_) => {}
                Err(err) if err.kind() == std::io::ErrorKind::TimedOut => {}
                Err(err) => return Err(anyhow!("serial read failed: {}", err)),
            }
        }

        bail!("timed out waiting for ACK for msg_id {}", expected_msg_id)
    }

    fn send_and_wait_ack(&mut self, msg: pb::WireMessage) -> Result<()> {
        let msg_id = msg.msg_id;
        self.send_message(msg)?;
        self.wait_for_ack(msg_id, Duration::from_secs(2))
    }
}

fn find_default_port() -> Result<String> {
    let ports = serialport::available_ports().context("failed to enumerate serial ports")?;
    if ports.is_empty() {
        bail!("no serial ports found");
    }

    for port in &ports {
        if port.port_name.contains("ttyACM")
            || port.port_name.contains("ttyUSB")
            || port.port_name.contains("cu.usbmodem")
            || port.port_name.contains("cu.wchusbserial")
        {
            return Ok(port.port_name.clone());
        }
    }

    Ok(ports[0].port_name.clone())
}

fn app_icon_hint_for(bundle: &str, source_app: &str, category: i32) -> String {
    let lower = format!("{} {}", bundle, source_app).to_ascii_lowercase();
    if lower.contains("whatsapp") {
        "whatsapp".to_string()
    } else if lower.contains("mail") || lower.contains("outlook") {
        "mail".to_string()
    } else if lower.contains("calendar") {
        "calendar".to_string()
    } else if category == pb::Category::Chat as i32 {
        "chat".to_string()
    } else {
        "system".to_string()
    }
}

fn send_notification_animation(
    conn: &mut Connection,
    next_msg_id: &mut u32,
    category: i32,
) -> Result<()> {
    let (led_pattern, led_repeats, excited_deg, rest_deg) = animation_profile_for_category(category);

    send_servo_message(conn, next_msg_id, excited_deg)?;
    send_led_message(conn, next_msg_id, led_pattern, led_repeats)?;

    let anim_duration_ms = match led_pattern {
        1 => 8 * 60 * led_repeats as u64,
        2 => 2 * 70 * led_repeats as u64,
        3 => 4 * 65 * led_repeats as u64,
        _ => 2 * 80 * led_repeats as u64,
    };
    std::thread::sleep(Duration::from_millis(anim_duration_ms + 100));

    send_servo_message(conn, next_msg_id, rest_deg)
}

fn send_simulated_notification(
    conn: &mut Connection,
    next_msg_id: &mut u32,
    preset: &str,
    from: &str,
    text: &str,
) -> Result<()> {
    let (source_app, source_bundle_id, category, title, sender_name, app_icon_hint) =
        default_notification_for_preset(preset, from, text);

    send_notification_message(
        conn,
        next_msg_id,
        pb::NotificationEvent {
            id: format!("sim-{}-{}", preset, *next_msg_id),
            source_app: source_app.clone(),
            title,
            body: text.to_string(),
            urgency: pb::Urgency::Normal as i32,
            category,
            source_bundle_id: source_bundle_id.clone(),
            sender_name,
            sender_handle: String::new(),
            app_icon_hint: app_icon_hint_for(&source_bundle_id, &source_app, category)
                .if_empty_then(app_icon_hint),
        },
    )?;

    send_notification_animation(conn, next_msg_id, category)
}

trait StringExt {
    fn if_empty_then(self, fallback: String) -> String;
}

impl StringExt for String {
    fn if_empty_then(self, fallback: String) -> String {
        if self.is_empty() {
            fallback
        } else {
            self
        }
    }
}

fn map_category(app: &str) -> i32 {
    let lower = app.to_ascii_lowercase();
    if lower.contains("mail") || lower.contains("thunderbird") || lower.contains("outlook") {
        pb::Category::Mail as i32
    } else if lower.contains("whatsapp")
        || lower.contains("slack")
        || lower.contains("teams")
        || lower.contains("discord")
        || lower.contains("chat")
        || lower.contains("messages")
    {
        pb::Category::Chat as i32
    } else if lower.contains("calendar") {
        pb::Category::Calendar as i32
    } else {
        pb::Category::System as i32
    }
}

#[cfg(target_os = "linux")]
fn extract_quoted(line: &str) -> Option<String> {
    let start = line.find('"')? + 1;
    let end = line[start..].find('"')? + start;
    Some(line[start..end].to_string())
}

#[cfg(target_os = "linux")]
fn run_monitor(conn: &mut Connection, next_msg_id: &mut u32) -> Result<()> {
    let mut child = Command::new("dbus-monitor")
        .arg("--session")
        .arg("type='method_call',interface='org.freedesktop.Notifications',member='Notify'")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to spawn dbus-monitor")?;

    let stdout = child
        .stdout
        .take()
        .context("dbus-monitor stdout unavailable")?;
    let reader = BufReader::new(stdout);

    let mut collecting = false;
    let mut strings: Vec<String> = Vec::new();

    println!("Listening for Linux desktop notifications via dbus-monitor...");
    for line in reader.lines() {
        let line = line.context("failed to read dbus-monitor output")?;

        if line.contains("member=\"Notify\"") {
            collecting = true;
            strings.clear();
            continue;
        }

        if !collecting {
            continue;
        }

        let trimmed = line.trim_start();
        if trimmed.starts_with("string ") {
            if let Some(value) = extract_quoted(trimmed) {
                strings.push(value);
            }
        }

        if strings.len() >= 3 {
            let source_app = strings[0].clone();
            let title = strings[1].clone();
            let body = strings[2].clone();
            let category = map_category(&source_app);
            let sender_name = title
                .split(':')
                .next()
                .unwrap_or("")
                .trim()
                .to_string();

            send_notification_message(
                conn,
                next_msg_id,
                pb::NotificationEvent {
                    id: format!("linux-{}", *next_msg_id),
                    source_app: source_app.clone(),
                    title: title.clone(),
                    body: body.clone(),
                    urgency: pb::Urgency::Normal as i32,
                    category,
                    source_bundle_id: String::new(),
                    sender_name,
                    sender_handle: String::new(),
                    app_icon_hint: app_icon_hint_for("", &source_app, category),
                },
            )?;
            println!("Forwarded notification from '{}'", source_app);

            send_notification_animation(conn, next_msg_id, category)?;

            collecting = false;
            strings.clear();
        }
    }

    bail!("dbus-monitor exited unexpectedly")
}

#[cfg(target_os = "macos")]
fn extract_json_string(line: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{}\" : \"", key);
    let start = line.find(&pattern)? + pattern.len();
    let end = line[start..].find('"')? + start;
    Some(line[start..end].replace("\\/", "/"))
}

#[cfg(target_os = "macos")]
fn bundle_id_to_app_name(bundle_id: &str) -> String {
    let last = bundle_id.rsplit('.').next().unwrap_or(bundle_id);
    match last {
        "WhatsApp" => "WhatsApp".to_string(),
        "Outlook" => "Outlook".to_string(),
        "Chrome" => "Chrome".to_string(),
        other => other.to_string(),
    }
}

#[cfg(target_os = "macos")]
fn guess_sender_from_text(text: &str) -> String {
    for delimiter in [":", " - ", " | "] {
        if let Some((sender, _)) = text.split_once(delimiter) {
            let sender = sender.trim();
            if !sender.is_empty() && sender.len() < 40 {
                return sender.to_string();
            }
        }
    }

    String::new()
}

#[cfg(target_os = "macos")]
fn extract_macos_notification_event(
    line: &str,
) -> Option<(String, String, String, String, String, i32)> {
    let bundle_id = extract_json_string(line, "bundle-id")
        .or_else(|| extract_json_string(line, "topic"))?;
    let category = map_category(&bundle_id);
    let source_app = bundle_id_to_app_name(&bundle_id);
    let sender_name = if bundle_id.contains("WhatsApp") {
        guess_sender_from_text(line)
    } else {
        String::new()
    };

    let title = if !sender_name.is_empty() {
        format!("{} message", sender_name)
    } else if category == pb::Category::Chat as i32 {
        format!("Message on {}", source_app)
    } else {
        format!("Notification from {}", source_app)
    };

    Some((
        source_app,
        bundle_id,
        title,
        line.to_string(),
        sender_name,
        category,
    ))
}

#[cfg(target_os = "macos")]
fn run_monitor(conn: &mut Connection, next_msg_id: &mut u32) -> Result<()> {
    let predicate =
        "process == \"usernoted\" OR subsystem CONTAINS[c] \"UserNotifications\" OR subsystem CONTAINS[c] \"unc\"";
    let mut last_signature = String::new();
    let mut last_sent_at = std::time::Instant::now() - Duration::from_secs(10);

    println!("Listening for macOS notification activity via unified log...");
    loop {
        let mut child = Command::new("log")
            .arg("stream")
            .arg("--style")
            .arg("json")
            .arg("--predicate")
            .arg(predicate)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("failed to spawn macOS log stream")?;

        let stdout = child
            .stdout
            .take()
            .context("log stream stdout unavailable")?;
        let reader = BufReader::new(stdout);

        for line in reader.lines() {
            let line = line.context("failed to read macOS log stream")?;
            let Some((source_app, bundle_id, title, body, sender_name, category)) =
                extract_macos_notification_event(&line)
            else {
                continue;
            };

            let signature = format!("{}:{}:{}", bundle_id, title, sender_name);
            if signature == last_signature && last_sent_at.elapsed() < Duration::from_secs(2) {
                continue;
            }

            send_notification_message(
                conn,
                next_msg_id,
                pb::NotificationEvent {
                    id: format!("macos-{}", *next_msg_id),
                    source_app: source_app.clone(),
                    title,
                    body,
                    urgency: pb::Urgency::Normal as i32,
                    category,
                    source_bundle_id: bundle_id.clone(),
                    sender_name: sender_name.clone(),
                    sender_handle: String::new(),
                    app_icon_hint: app_icon_hint_for(&bundle_id, &source_app, category),
                },
            )?;
            println!("Forwarded macOS notification activity from '{}'", source_app);

            send_notification_animation(conn, next_msg_id, category)?;

            last_signature = signature;
            last_sent_at = std::time::Instant::now();
        }

        eprintln!("macOS log stream exited; restarting in 1s");
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn print_help() {
    println!(
        "landerctl — desktop host bridge for the lander001 desk robot

USAGE:
    landerctl [OPTIONS] [PORT]

ARGUMENTS:
    PORT                     Serial port to use (auto-detected when omitted)

OPTIONS:
    -h, --help               Print this help message and exit

MODES (headless, mutually exclusive with GUI):
    --nogui                  Run in headless mode (no window)
    --background             Run headless in background style (no window)

SIMULATION:
    --simulate PRESET        Send a simulated notification (whatsapp|mail|calendar|system)
    --from NAME              Sender name for simulation  [default: Alice]
    --text TEXT              Message text for simulation [default: Hey! This is a test message]
    --count N                Number of notifications to send [default: 1]
    --interval-ms MS         Delay between burst notifications in ms [default: 1200]

EXAMPLES:
    # Launch GUI (default)
    landerctl

    # Headless mode forwarding notifications (auto-detected port)
    landerctl --nogui

    # Explicit port in headless mode
    landerctl --nogui /dev/cu.usbmodemXXXX

    # Simulate a WhatsApp notification
    landerctl --nogui --simulate whatsapp --from \"Holger\" --text \"Coffee in 5?\"

    # Burst simulation
    landerctl --nogui --simulate whatsapp --from \"Bob\" --text \"Ping!\" --count 5 --interval-ms 800
"
    );
}

fn run_with_args(args: Vec<String>) -> Result<()> {
    let mut simulate: Option<String> = None;
    let mut sim_from = String::from("Alice");
    let mut sim_text = String::from("Hey! This is a test message");
    let mut sim_count: u32 = 1;
    let mut sim_interval_ms: u64 = 1200;
    let mut port_override: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--simulate" => {
                let value = args
                    .get(i + 1)
                    .context("--simulate needs a preset: whatsapp|mail|calendar|system")?;
                simulate = Some(value.to_ascii_lowercase());
                i += 2;
            }
            "--from" => {
                let value = args.get(i + 1).context("--from needs a value")?;
                sim_from = value.clone();
                i += 2;
            }
            "--text" => {
                let value = args.get(i + 1).context("--text needs a value")?;
                sim_text = value.clone();
                i += 2;
            }
            "--count" => {
                let value = args.get(i + 1).context("--count needs a number")?;
                sim_count = value
                    .parse::<u32>()
                    .with_context(|| format!("invalid --count value '{}'", value))?;
                i += 2;
            }
            "--interval-ms" => {
                let value = args.get(i + 1).context("--interval-ms needs a number")?;
                sim_interval_ms = value
                    .parse::<u64>()
                    .with_context(|| format!("invalid --interval-ms value '{}'", value))?;
                i += 2;
            }
            "-h" | "--help" => {
                print_help();
                return Ok(());
            }
            other if other.starts_with('-') => {
                bail!("unknown argument '{}'; expected monitor/sim flags", other);
            }
            value => {
                if port_override.is_none() {
                    port_override = Some(value.to_string());
                    i += 1;
                } else {
                    bail!("unexpected extra positional argument '{}'", value);
                }
            }
        }
    }

    let port_name = if let Some(port) = port_override {
        port
    } else {
        find_default_port()?
    };
    println!("Opening serial port: {}", port_name);

    let mut conn = Connection::new(port_name)?;
    let mut next_msg_id = 1_u32;

    send_ping_message(&mut conn, &mut next_msg_id)?;
    println!("Sent Ping and received ACK");

    if let Some(preset) = simulate {
        if !matches!(preset.as_str(), "whatsapp" | "mail" | "calendar" | "system") {
            bail!(
                "invalid --simulate preset '{}'; use whatsapp|mail|calendar|system",
                preset
            );
        }

        println!(
            "Sending {} simulated '{}' notification(s) from '{}'...",
            sim_count, preset, sim_from
        );

        for index in 0..sim_count {
            send_simulated_notification(&mut conn, &mut next_msg_id, &preset, &sim_from, &sim_text)?;
            println!("Simulated notification {}/{} ACKed", index + 1, sim_count);
            if index + 1 < sim_count {
                std::thread::sleep(Duration::from_millis(sim_interval_ms));
            }
        }

        return Ok(());
    }

    #[cfg(target_os = "linux")]
    return run_monitor(&mut conn, &mut next_msg_id);

    #[cfg(target_os = "macos")]
    return run_monitor(&mut conn, &mut next_msg_id);

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    bail!("notification forwarding is only supported on Linux and macOS");
}

struct LanderGui {
    controller: Arc<Mutex<SharedController>>,

    servo_angle: f32,
    led_pattern: u32,
    led_repeats: u32,
    icon_id: String,

    preset: String,
    from: String,
    text: String,

    app_icon_texture: Option<egui::TextureHandle>,

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    _tray: Option<tray_item::TrayItem>,
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        tray_status_id: Option<u32>,
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        tray_connect_id: Option<u32>,
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        last_tray_connected: Option<bool>,
}

impl Default for LanderGui {
    fn default() -> Self {
        Self {
            controller: Arc::new(Mutex::new(SharedController::default())),

            servo_angle: 90.0,
            led_pattern: 2,
            led_repeats: 3,
            icon_id: "cat1".to_string(),

            preset: "mail".to_string(),
            from: "Alice".to_string(),
            text: "Please review PR #23".to_string(),

            app_icon_texture: None,

            #[cfg(any(target_os = "linux", target_os = "macos"))]
            _tray: None,
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                tray_status_id: None,
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                tray_connect_id: None,
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                last_tray_connected: None,
        }
    }
}

impl LanderGui {
    fn shortcut_keycap(ui: &mut egui::Ui, text: &str) {
        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::symmetric(4, 1))
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(text)
                        .size(12.0)
                        .monospace()
                        .strong()
                        .color(egui::Color32::from_rgb(220, 236, 220)),
                );
            });
    }

    fn shortcut_hint(ui: &mut egui::Ui, keys: &[&str]) {
        ui.horizontal(|ui| {
            for key in keys {
                Self::shortcut_keycap(ui, key);
            }
        });
    }

    fn install_fonts_and_text_style(ctx: &egui::Context) {
        let mut fonts = egui::FontDefinitions::default();
        egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
        if let Some(font_keys) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
            font_keys.insert(1, "phosphor".to_string());
        }
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "Hack".to_string());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .insert(0, "Hack".to_string());
        ctx.set_fonts(fonts);

        let mut style = (*ctx.style()).clone();
        style.text_styles.insert(
            egui::TextStyle::Body,
            egui::FontId::new(13.0, egui::FontFamily::Monospace),
        );
        style.text_styles.insert(
            egui::TextStyle::Button,
            egui::FontId::new(13.0, egui::FontFamily::Monospace),
        );
        style.text_styles.insert(
            egui::TextStyle::Monospace,
            egui::FontId::new(13.0, egui::FontFamily::Monospace),
        );
        style.text_styles.insert(
            egui::TextStyle::Heading,
            egui::FontId::new(22.0, egui::FontFamily::Monospace),
        );
        ctx.set_style(style);
    }

    fn load_app_icon_data() -> Result<egui::IconData> {
        let bytes = include_bytes!("../../assets/gui/icon.png");
        eframe::icon_data::from_png_bytes(bytes).context("failed to decode GUI icon.png")
    }

    fn load_app_icon_texture(ctx: &egui::Context) -> Option<egui::TextureHandle> {
        let bytes = include_bytes!("../../assets/gui/icon.png");
        let img = image::load_from_memory(bytes).ok()?.to_rgba8();
        let size = [img.width() as usize, img.height() as usize];
        let color_image = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
        Some(ctx.load_texture(
            "lander_icon",
            color_image,
            egui::TextureOptions::LINEAR,
        ))
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn create_tray(
        controller: Arc<Mutex<SharedController>>,
    ) -> Result<(tray_item::TrayItem, u32, u32)> {
        use tray_item::{IconSource, TrayItem};

        fn is_dark_mode() -> bool {
            #[cfg(target_os = "macos")]
            {
                let output = std::process::Command::new("defaults")
                    .args(["read", "-g", "AppleInterfaceStyle"])
                    .output();
                match output {
                    Ok(out) if out.status.success() => {
                        let text = String::from_utf8_lossy(&out.stdout);
                        text.trim().eq_ignore_ascii_case("dark")
                    }
                    _ => false,
                }
            }

            #[cfg(target_os = "linux")]
            {
                // GNOME 42+: check color-scheme setting
                let output = std::process::Command::new("gsettings")
                    .args(["get", "org.gnome.desktop.interface", "color-scheme"])
                    .output();
                if let Ok(out) = output {
                    if out.status.success() {
                        let text = String::from_utf8_lossy(&out.stdout);
                        if text.trim().to_ascii_lowercase().contains("dark") {
                            return true;
                        }
                    }
                }
                // Fallback: check gtk-theme name
                let output = std::process::Command::new("gsettings")
                    .args(["get", "org.gnome.desktop.interface", "gtk-theme"])
                    .output();
                if let Ok(out) = output {
                    if out.status.success() {
                        let text = String::from_utf8_lossy(&out.stdout);
                        if text.trim().to_ascii_lowercase().contains("dark") {
                            return true;
                        }
                    }
                }
                false
            }

            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            false
        }

        let tray_icon_bytes = if is_dark_mode() {
            include_bytes!("../../assets/gui/tray_satellite_dark.png").to_vec()
        } else {
            include_bytes!("../../assets/gui/tray_satellite_light.png").to_vec()
        };
        let tray_icon_img = image::load_from_memory(&tray_icon_bytes)
            .context("failed to decode tray satellite icon")?;

        // ksni (Linux) requires raw ARGB32 pixels in network byte order; macOS
        // uses NSImage which can decode PNG bytes directly.
        #[cfg(target_os = "linux")]
        let tray_icon_data = tray_icon_img
            .to_rgba8()
            .pixels()
            .flat_map(|p| [p[3], p[0], p[1], p[2]])
            .collect::<Vec<u8>>();
        #[cfg(target_os = "macos")]
        let tray_icon_data = tray_icon_bytes;

        let mut tray = TrayItem::new(
            "lander001",
            IconSource::Data {
                height: tray_icon_img.height() as i32,
                width: tray_icon_img.width() as i32,
                data: tray_icon_data,
            },
        )
        .context("failed to create tray icon")?;

        tray.add_label("lander001 running")
            .context("failed to add tray label")?;

        let status_id = tray
            .add_label_with_id("● Disconnected")
            .context("failed to add tray status label")?;

        tray.add_label("Robot")
            .context("failed to add tray label")?;

        {
            let controller = Arc::clone(&controller);
            let _id = tray
                .add_menu_item_with_id("Refresh ports", move || {
                    if let Ok(mut controller) = controller.lock() {
                        controller.refresh_ports();
                    }
                })
                .context("failed to add tray menu item")?;
            #[cfg(target_os = "macos")]
            tray.inner_mut().set_item_sf_symbol(_id, "arrow.clockwise");
        }

        let connect_id = {
            let controller = Arc::clone(&controller);
            let id = tray
                .add_menu_item_with_id("Connect", move || {
                    if let Ok(mut controller) = controller.lock() {
                        if controller.is_connected() {
                            controller.disconnect();
                        } else {
                            controller.connect();
                        }
                    }
                })
                .context("failed to add tray menu item")?;
            #[cfg(target_os = "macos")]
            tray.inner_mut().set_item_sf_symbol(id, "wifi");
            id
        };

        {
            let controller = Arc::clone(&controller);
            let _id = tray
                .add_menu_item_with_id("Ping", move || {
                    if let Ok(mut controller) = controller.lock() {
                        controller.send_ping();
                    }
                })
                .context("failed to add tray menu item")?;
            #[cfg(target_os = "macos")]
            tray.inner_mut()
                .set_item_sf_symbol(_id, "antenna.radiowaves.left.and.right");
        }

        tray.add_label("Servo")
            .context("failed to add tray label")?;
        for angle in [0.0_f32, 90.0, 180.0, 270.0] {
            let controller = Arc::clone(&controller);
            let _id = tray
                .add_menu_item_with_id(&format!("Set servo {:.0} deg", angle), move || {
                    if let Ok(mut controller) = controller.lock() {
                        controller.send_servo(angle);
                    }
                })
                .context("failed to add tray menu item")?;
            #[cfg(target_os = "macos")]
            tray.inner_mut().set_item_sf_symbol(_id, "slider.horizontal.3");
        }

        tray.add_label("LED")
            .context("failed to add tray label")?;
        for pattern_id in 1..=4_u32 {
            let controller = Arc::clone(&controller);
            let _id = tray
                .add_menu_item_with_id(&format!("Run LED pattern {} x3", pattern_id), move || {
                    if let Ok(mut controller) = controller.lock() {
                        controller.send_led(pattern_id, 3);
                    }
                })
                .context("failed to add tray menu item")?;
            #[cfg(target_os = "macos")]
            tray.inner_mut().set_item_sf_symbol(_id, "lightbulb");
        }

        tray.add_label("Display")
            .context("failed to add tray label")?;
        for icon_id in ["cat1", "cat2", "cat3"] {
            let controller = Arc::clone(&controller);
            let _id = tray
                .add_menu_item_with_id(&format!("Show {}", icon_id), move || {
                    if let Ok(mut controller) = controller.lock() {
                        controller.send_icon(icon_id.to_string());
                    }
                })
                .context("failed to add tray menu item")?;
            #[cfg(target_os = "macos")]
            tray.inner_mut().set_item_sf_symbol(_id, "photo");
        }

        let _quit_id = tray
            .add_menu_item_with_id("Quit", || std::process::exit(0))
            .context("failed to add tray menu item")?;
        #[cfg(target_os = "macos")]
        tray.inner_mut().set_item_sf_symbol(_quit_id, "xmark.circle");

        #[cfg(target_os = "macos")]
        tray.inner_mut().display();

        Ok((tray, status_id, connect_id))
    }
}

impl eframe::App for LanderGui {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ── Keyboard shortcut handling ──────────────────────────────────────
        // Read all key-presses in a single borrow, then act on them so that
        // we avoid holding the `input()` borrow while calling `&mut self`.
        let (
            key_c, key_d, key_p,
            key_up, key_down, key_s,
            key_1, key_2, key_3, key_4,
            key_n,
        ) = ctx.input(|i| (
            i.key_pressed(egui::Key::C),
            i.key_pressed(egui::Key::D),
            i.key_pressed(egui::Key::P),
            i.key_pressed(egui::Key::ArrowRight),
            i.key_pressed(egui::Key::ArrowLeft),
            i.key_pressed(egui::Key::S),
            i.key_pressed(egui::Key::Num1),
            i.key_pressed(egui::Key::Num2),
            i.key_pressed(egui::Key::Num3),
            i.key_pressed(egui::Key::Num4),
            i.key_pressed(egui::Key::N),
        ));

        // Only fire shortcuts when no text-edit (or other keyboard-consuming
        // widget) currently holds focus.
        let any_widget_focused = ctx.memory(|m| m.focused().is_some());
        if !any_widget_focused {
            // Snapshot connection state needed to guard some shortcuts.
            let connected = self.controller.lock().unwrap().is_connected();

            if key_c && !connected {
                self.controller.lock().unwrap().connect();
            }
            if key_d && connected {
                self.controller.lock().unwrap().disconnect();
            }
            if key_p && connected {
                self.controller.lock().unwrap().send_ping();
            }
            if key_up && connected {
                self.servo_angle = (self.servo_angle + 5.0).min(270.0);
                self.controller.lock().unwrap().send_servo(self.servo_angle);
            }
            if key_down && connected {
                self.servo_angle = (self.servo_angle - 5.0).max(0.0);
                self.controller.lock().unwrap().send_servo(self.servo_angle);
            }
            if key_s && connected {
                self.controller.lock().unwrap().send_servo(self.servo_angle);
            }
            for (pressed, pattern) in [(key_1, 1u32), (key_2, 2), (key_3, 3), (key_4, 4)] {
                if pressed && connected {
                    self.led_pattern = pattern;
                    self.controller.lock().unwrap().send_led(pattern, self.led_repeats);
                }
            }
            if key_n && connected {
                let (p, f, t) = (self.preset.clone(), self.from.clone(), self.text.clone());
                self.controller.lock().unwrap().send_notification_and_animation(&p, &f, &t);
            }
        }
        // ───────────────────────────────────────────────────────────────────

        let mut visuals = egui::Visuals::dark();
        visuals.override_text_color = Some(egui::Color32::from_rgb(235, 246, 235));
        visuals.panel_fill = egui::Color32::from_rgb(34, 36, 40);
        visuals.window_fill = egui::Color32::from_rgb(40, 42, 46);
        visuals.faint_bg_color = egui::Color32::from_rgb(46, 48, 54);
        visuals.extreme_bg_color = egui::Color32::from_rgb(38, 40, 46);
        visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(36, 39, 44);
        visuals.widgets.noninteractive.fg_stroke.color = egui::Color32::from_rgb(220, 234, 220);
        visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(42, 46, 52);
        visuals.widgets.inactive.weak_bg_fill = egui::Color32::from_rgb(42, 46, 52);
        visuals.widgets.inactive.fg_stroke.color = egui::Color32::from_rgb(235, 255, 235);
        visuals.widgets.inactive.bg_stroke.color = egui::Color32::from_rgb(86, 94, 106);
        visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(20, 72, 54);
        visuals.widgets.hovered.fg_stroke.color = egui::Color32::from_rgb(248, 255, 248);
        visuals.widgets.active.bg_fill = egui::Color32::from_rgb(30, 144, 96);
        visuals.widgets.active.fg_stroke.color = egui::Color32::from_rgb(255, 255, 255);
        visuals.selection.bg_fill = egui::Color32::from_rgb(24, 120, 86);
        visuals.selection.stroke.color = egui::Color32::from_rgb(240, 255, 240);
        visuals.widgets.open.bg_fill = egui::Color32::from_rgb(20, 40, 32);
        ctx.set_visuals(visuals);

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let (Some(tray), Some(status_id), Some(connect_id)) = (
            &mut self._tray,
            self.tray_status_id,
            self.tray_connect_id,
        ) {
            let (is_connected, port_name) = {
                let ctrl = self.controller.lock().unwrap();
                (ctrl.is_connected(), ctrl.conn.as_ref().map(|c| c.port_name.clone()))
            };
            if self.last_tray_connected != Some(is_connected) {
                let status = if is_connected {
                    format!("● Connected: {}", port_name.unwrap_or_default())
                } else {
                    "● Disconnected".to_string()
                };
                let _ = tray.set_item_label(status_id, &status);
                let connect_label = if is_connected { "Disconnect" } else { "Connect" };
                let _ = tray.set_item_label(connect_id, connect_label);
                #[cfg(target_os = "macos")]
                {
                    let symbol = if is_connected { "wifi.slash" } else { "wifi" };
                    tray.inner_mut().set_item_sf_symbol(connect_id, symbol);
                }
                self.last_tray_connected = Some(is_connected);
            }
        }

        let (connected, conn_port_name, ports, mut selected_port_idx, logs) = {
            let ctrl = self.controller.lock().unwrap();
            (
                ctrl.conn.is_some(),
                ctrl.conn.as_ref().map(|c| c.port_name.clone()),
                ctrl.ports.clone(),
                ctrl.selected_port_idx,
                ctrl.logs.clone(),
            )
        };

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if let Some(texture) = &self.app_icon_texture {
                    ui.image((texture.id(), egui::vec2(128.0, 128.0)));
                    ui.add_space(6.0);
                }
                ui.heading("landerctl");

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(ref port_name) = conn_port_name {
                        ui.label(format!("connected: {}", port_name));
                        ui.colored_label(egui::Color32::from_rgb(40, 220, 120), "●");
                    } else {
                        ui.label("disconnected");
                        ui.colored_label(egui::Color32::from_rgb(230, 70, 70), "●");
                    }
                });
            });
        });

        egui::SidePanel::left("left")
            .resizable(true)
            .default_width(340.0)
            .show(ctx, |ui| {
                ui.add_space(10.0);

                ui.group(|ui| {
                    ui.set_min_width(ui.available_width());
                    ui.label(
                        egui::RichText::new(format!("{} Connection", regular::PLUGS_CONNECTED))
                            .strong(),
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Refresh ports").clicked() {
                            self.controller.lock().unwrap().refresh_ports();
                        }

                        if !connected {
                            if ui.button("Connect").clicked() {
                                self.controller.lock().unwrap().connect();
                            }
                            Self::shortcut_hint(ui, &["C"]);
                        } else if ui.button("Disconnect").clicked() {
                            self.controller.lock().unwrap().disconnect();
                            Self::shortcut_hint(ui, &["D"]);
                        }
                    });

                    ui.horizontal(|ui| {
                        ui.label("Serial port");
                        let old_idx = selected_port_idx;
                        egui::ComboBox::from_id_salt("serial_port_combo")
                            .selected_text(
                                ports
                                    .get(selected_port_idx)
                                    .map(String::as_str)
                                    .unwrap_or("(none)"),
                            )
                            .show_ui(ui, |ui| {
                                for (idx, name) in ports.iter().enumerate() {
                                    ui.selectable_value(&mut selected_port_idx, idx, name);
                                }
                            });
                        if selected_port_idx != old_idx {
                            self.controller.lock().unwrap().selected_port_idx = selected_port_idx;
                        }
                    });

                    ui.add_enabled_ui(connected, |ui| {
                        ui.horizontal(|ui| {
                            if ui.button("Ping").clicked() {
                                self.controller.lock().unwrap().send_ping();
                            }
                            Self::shortcut_hint(ui, &["P"]);
                        });
                    });
                });

                ui.add_space(16.0);

                ui.group(|ui| {
                    ui.set_min_width(ui.available_width());
                    ui.label(
                        egui::RichText::new(format!("{} Manual controls", regular::SLIDERS))
                            .strong(),
                    );
                    ui.add_space(8.0);

                    ui.add_enabled_ui(connected, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("Servo");
                            ui.add(egui::Slider::new(&mut self.servo_angle, 0.0..=270.0).suffix(" deg"));
                            if ui.button("Send").clicked() {
                                self.controller.lock().unwrap().send_servo(self.servo_angle);
                            }
                            Self::shortcut_hint(ui, &["S"]);
                        });

                        ui.horizontal(|ui| {
                            ui.label("LED pattern");
                            ui.add(egui::DragValue::new(&mut self.led_pattern).range(1..=4));
                            ui.label("repeats");
                            ui.add(egui::DragValue::new(&mut self.led_repeats).range(1..=20));
                            if ui.button("Run").clicked() {
                                self.controller
                                    .lock()
                                    .unwrap()
                                    .send_led(self.led_pattern, self.led_repeats);
                            }
                            Self::shortcut_hint(ui, &["1", "2", "3", "4"]);
                        });

                        ui.horizontal(|ui| {
                            ui.label("Icon");
                            ui.text_edit_singleline(&mut self.icon_id);
                            if ui.button("Show").clicked() {
                                self.controller.lock().unwrap().send_icon(self.icon_id.clone());
                            }
                        });
                    });
                });

                ui.add_space(16.0);

                ui.group(|ui| {
                    ui.set_min_width(ui.available_width());
                    ui.label(
                        egui::RichText::new(format!("{} Notification simulator", regular::BELL))
                            .strong(),
                    );
                    ui.add_space(8.0);

                    ui.add_enabled_ui(connected, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("Preset");
                            egui::ComboBox::from_id_salt("preset_combo")
                                .selected_text(self.preset.clone())
                                .show_ui(ui, |ui| {
                                    for p in ["whatsapp", "mail", "calendar", "system"] {
                                        ui.selectable_value(&mut self.preset, p.to_string(), p);
                                    }
                                });
                        });
                        ui.horizontal(|ui| {
                            ui.label("From");
                            ui.text_edit_singleline(&mut self.from);
                        });
                        ui.horizontal(|ui| {
                            ui.label("Text");
                            ui.text_edit_singleline(&mut self.text);
                        });

                        ui.horizontal(|ui| {
                            if ui.button("Send notification + fun animation").clicked() {
                                let (p, f, t) = (self.preset.clone(), self.from.clone(), self.text.clone());
                                self.controller.lock().unwrap().send_notification_and_animation(&p, &f, &t);
                            }
                            Self::shortcut_hint(ui, &["N"]);
                        });
                    });
                });

                ui.add_space(16.0);

                // ── Keyboard cheatsheet ─────────────────────────────────
                ui.group(|ui| {
                    ui.set_min_width(ui.available_width());

                    let header_text = egui::RichText::new(
                        format!("{} Keyboard shortcuts", regular::KEYBOARD)
                    ).strong();
                    ui.label(header_text);
                    ui.add_space(8.0);
                    egui::Grid::new("cheatsheet_grid")
                        .num_columns(2)
                        .spacing([16.0, 4.0])
                        .show(ui, |ui| {
                            Self::shortcut_hint(ui, &["C"]);
                            ui.label("Connect");
                            ui.end_row();

                            Self::shortcut_hint(ui, &["D"]);
                            ui.label("Disconnect");
                            ui.end_row();

                            Self::shortcut_hint(ui, &["P"]);
                            ui.label("Ping");
                            ui.end_row();

                            ui.horizontal(|ui| {
                                Self::shortcut_keycap(ui, "←");
                                ui.label("/");
                                Self::shortcut_keycap(ui, "→");
                            });
                            ui.label("Servo +5° / −5°  (auto-send)");
                            ui.end_row();

                            Self::shortcut_hint(ui, &["S"]);
                            ui.label("Send current servo angle");
                            ui.end_row();

                            Self::shortcut_hint(ui, &["1", "2", "3", "4"]);
                            ui.label("Run LED pattern 1-4");
                            ui.end_row();

                            Self::shortcut_hint(ui, &["N"]);
                            ui.label("Send notification + animation");
                            ui.end_row();
                        });
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(
                            "Shortcuts inactive while a text field has focus."
                        )
                        .italics()
                        .weak(),
                    );
                });
                // ───────────────────────────────────────────────────────
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading(egui::RichText::new("Activity log").strong());
            ui.separator();

            egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                for line in &logs {
                    ui.label(line);
                }
            });
        });
    }
}

fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "-h" || a == "--help") {
        print_help();
        return Ok(());
    }

    let force_gui = args.iter().any(|a| a == "--gui");
    let wants_headless = args.iter().any(|a| {
        matches!(
            a.as_str(),
            "--nogui" | "--background" | "--simulate"
        )
    });

    if wants_headless && !force_gui {
        args.retain(|a| a != "--nogui" && a != "--background" && a != "--gui");
        return run_with_args(args);
    }

    let app_icon = LanderGui::load_app_icon_data().ok();
    let native_options = eframe::NativeOptions {
        viewport: if let Some(icon) = app_icon {
            egui::ViewportBuilder::default()
                .with_inner_size([1200.0, 820.0])
                .with_icon(icon)
        } else {
            egui::ViewportBuilder::default().with_inner_size([1200.0, 820.0])
        },
        ..Default::default()
    };

    eframe::run_native(
        "landerctl",
        native_options,
        Box::new(|cc| {
            LanderGui::install_fonts_and_text_style(&cc.egui_ctx);
            let mut app = LanderGui {
                app_icon_texture: LanderGui::load_app_icon_texture(&cc.egui_ctx),
                ..Default::default()
            };
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                let controller = Arc::clone(&app.controller);
                match LanderGui::create_tray(controller) {
                    Ok((tray, status_id, connect_id)) => {
                        eprintln!("tray: initialized");
                        app._tray = Some(tray);
                        app.tray_status_id = Some(status_id);
                        app.tray_connect_id = Some(connect_id);
                    }
                    Err(err) => {
                        eprintln!("tray: failed to initialize tray icon: {}", err);
                    }
                }
            }
            Ok(Box::new(app))
        }),
    )
    .map_err(|err| anyhow!("failed to start GUI: {}", err))
}
