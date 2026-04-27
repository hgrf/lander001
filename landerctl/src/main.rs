#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::collections::HashMap;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::io::{BufRead, BufReader};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "linux")]
use std::sync::Condvar;
#[cfg(target_os = "linux")]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use btleplug::api::{
    Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, ValueNotification,
    WriteType,
};
use btleplug::platform::{Manager, Peripheral};
use clap::Parser;
use eframe::egui;
use egui_phosphor::regular;
use futures_util::StreamExt;
#[cfg(target_os = "macos")]
use serde_json::Value;
use uuid::Uuid;
#[cfg(target_os = "linux")]
use zbus::blocking::{Connection as ZbusConnection, Proxy as ZbusProxy};
#[cfg(target_os = "linux")]
use zbus::fdo::Error as ZbusFdoError;
#[cfg(target_os = "linux")]
use zbus::interface;
#[cfg(target_os = "linux")]
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value as ZvariantValue};

#[path = "../../shared/protocol.rs"]
mod protocol;

use protocol::pb;

fn list_ports() -> Vec<String> {
    discover_ble_devices().unwrap_or_default()
}

pub const BLE_SERVICE_UUID: &str = "0ad91b20-1734-4047-9e17-3bed82d75f9d";
pub const BLE_TX_CHAR_UUID: &str = "503de214-8682-46c4-828f-d59144da41be";
pub const BLE_RX_CHAR_UUID: &str = "b6fccb50-87be-44f3-ae22-f85485ea42c4";
const BLE_SCAN_STATUS: &str = "Scanning for BLE devices...";
const BLE_WRITE_TIMEOUT: Duration = Duration::from_millis(700);
const BLE_ACK_TIMEOUT: Duration = Duration::from_millis(900);
const BLE_DISCONNECT_TIMEOUT: Duration = Duration::from_millis(1200);
const BLE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const BLE_DISCOVER_TIMEOUT: Duration = Duration::from_secs(10);
const BLE_SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(5);
const BLE_NOTIFICATIONS_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(target_os = "linux")]
const BLUEZ_AGENT_PATH: &str = "/io/github/lander001/landerctl/agent";
#[cfg(target_os = "linux")]
const BLUEZ_AGENT_CAPABILITY: &str = "KeyboardDisplay";

#[cfg(target_os = "linux")]
static BLUEZ_AGENT_INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();
#[cfg(target_os = "linux")]
static BLUEZ_PAIRING_UI_STATE: OnceLock<(Mutex<BluezPairingState>, Condvar)> = OnceLock::new();
#[cfg(target_os = "linux")]
static BLUEZ_UI_CTX: OnceLock<egui::Context> = OnceLock::new();

#[cfg(target_os = "linux")]
#[derive(Clone)]
enum BluezPairingPromptKind {
    PinCode,
    Passkey,
    Confirmation { passkey: u32 },
}

#[cfg(target_os = "linux")]
#[derive(Clone)]
struct BluezPairingPrompt {
    id: u64,
    device: String,
    kind: BluezPairingPromptKind,
}

#[cfg(target_os = "linux")]
enum BluezPairingResponse {
    PinCode(String),
    Passkey(u32),
    Confirmation(bool),
    Cancelled,
}

#[cfg(target_os = "linux")]
struct BluezPairingState {
    pending: Option<BluezPairingPrompt>,
    response: Option<BluezPairingResponse>,
    next_id: u64,
}

fn ble_service_uuid() -> Result<Uuid> {
    Uuid::parse_str(BLE_SERVICE_UUID)
        .with_context(|| format!("invalid BLE service UUID {}", BLE_SERVICE_UUID))
}

fn ble_tx_uuid() -> Result<Uuid> {
    Uuid::parse_str(BLE_TX_CHAR_UUID)
        .with_context(|| format!("invalid BLE TX UUID {}", BLE_TX_CHAR_UUID))
}

fn ble_rx_uuid() -> Result<Uuid> {
    Uuid::parse_str(BLE_RX_CHAR_UUID)
        .with_context(|| format!("invalid BLE RX UUID {}", BLE_RX_CHAR_UUID))
}

fn ble_device_display_name(label: &str) -> &str {
    label.split_once('|').map(|(_, name)| name).unwrap_or(label)
}

fn discover_ble_devices() -> Result<Vec<String>> {
    let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
    rt.block_on(async {
        let manager = Manager::new()
            .await
            .context("failed to create BLE manager")?;
        let adapters = manager
            .adapters()
            .await
            .context("failed to enumerate BLE adapters")?;
        let Some(adapter) = adapters.into_iter().next() else {
            bail!("no BLE adapter available");
        };

        adapter
            .start_scan(ScanFilter::default())
            .await
            .context("failed to start BLE scan")?;
        tokio::time::sleep(Duration::from_millis(1300)).await;

        let service_uuid = ble_service_uuid()?;
        let peripherals = adapter
            .peripherals()
            .await
            .context("failed to fetch scanned peripherals")?;

        let mut labels = Vec::new();
        for peripheral in peripherals {
            let Some(props) = peripheral
                .properties()
                .await
                .context("failed to read BLE peripheral properties")?
            else {
                continue;
            };

            let has_service = props.services.contains(&service_uuid);
            let name_matches = props
                .local_name
                .as_ref()
                .map(|name| {
                    name.to_ascii_lowercase()
                        .contains(&protocol::BLE_DEVICE_NAME.to_ascii_lowercase())
                })
                .unwrap_or(false);

            if has_service || name_matches {
                let local_name = props.local_name.unwrap_or_else(|| "(unnamed)".to_string());
                labels.push(format!("{}|{}", peripheral.id(), local_name));
            }
        }

        labels.sort();
        Ok(labels)
    })
}

struct SharedController {
    ports: Vec<String>,
    selected_port_idx: usize,
    conn: Option<Connection>,
    pending_connect: Option<PendingConnection>,
    pending_scan: Option<PendingScan>,
    pending_disconnect: Option<PendingDisconnect>,
    pending_command: Option<PendingCommand>,
    next_msg_id: u32,
    logs: Vec<String>,
}

struct PendingConnection {
    port_name: String,
    started_at: Instant,
    cancel_flag: Arc<AtomicBool>,
    events_rx: std::sync::mpsc::Receiver<ConnectWorkerEvent>,
    status: String,
}

enum ConnectWorkerEvent {
    Progress(String),
    Finished(Box<Result<Connection>>),
}

struct PendingScan {
    started_at: Instant,
    result_rx: std::sync::mpsc::Receiver<Result<Vec<String>>>,
}

struct PendingDisconnect {
    port_name: String,
    started_at: Instant,
    result_rx: std::sync::mpsc::Receiver<Result<()>>,
}

struct PendingCommand {
    label: String,
    port_name: String,
    started_at: Instant,
    result_rx: std::sync::mpsc::Receiver<CommandWorkerResult>,
}

struct CommandWorkerResult {
    conn: Connection,
    next_msg_id: u32,
    outcome: Result<Vec<String>>,
}

enum CommandRequest {
    Ping,
    Servo {
        angle_deg: f32,
    },
    Led {
        pattern_id: u32,
        repeats: u32,
    },
    Icon {
        icon_id: String,
    },
    NotificationAndAnimation {
        preset: String,
        from: String,
        text: String,
    },
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    ForwardedNotification(ForwardedNotification),
}

impl CommandRequest {
    fn label(&self) -> String {
        match self {
            Self::Ping => "Sending ping...".to_string(),
            Self::Servo { angle_deg } => format!("Setting servo to {:.1} deg...", angle_deg),
            Self::Led {
                pattern_id,
                repeats,
            } => format!("Running LED pattern {} x{}...", pattern_id, repeats),
            Self::Icon { icon_id } => format!("Showing icon '{}'...", icon_id),
            Self::NotificationAndAnimation { preset, .. } => {
                format!("Sending '{}' notification...", preset)
            }
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Self::ForwardedNotification(event) => {
                format!("Forwarding '{}' notification...", event.source_app)
            }
        }
    }

    fn execute(self, conn: &mut Connection, next_msg_id: &mut u32) -> Result<Vec<String>> {
        match self {
            Self::Ping => {
                send_ping_message(conn, next_msg_id)?;
                Ok(vec!["Ping ACKed".to_string()])
            }
            Self::Servo { angle_deg } => {
                send_servo_message(conn, next_msg_id, angle_deg)?;
                Ok(vec![format!("SetServo {:.1} deg ACKed", angle_deg)])
            }
            Self::Led {
                pattern_id,
                repeats,
            } => {
                send_led_message(conn, next_msg_id, pattern_id, repeats)?;
                Ok(vec![format!(
                    "LedAnimation p={} r={} ACKed",
                    pattern_id, repeats
                )])
            }
            Self::Icon { icon_id } => {
                send_icon_message(conn, next_msg_id, &icon_id)?;
                Ok(vec![format!("ShowIcon '{}' ACKed", icon_id)])
            }
            Self::NotificationAndAnimation { preset, from, text } => {
                let (source_app, source_bundle_id, category, title, sender_name, app_icon_hint) =
                    default_notification_for_preset(&preset, &from, &text);
                let notif_id = *next_msg_id;
                send_notification_message(
                    conn,
                    next_msg_id,
                    pb::NotificationEvent {
                        id: format!("gui-{}-{}", preset, notif_id),
                        source_app: source_app.clone(),
                        title: title.clone(),
                        body: text,
                        urgency: pb::Urgency::Normal as i32,
                        category,
                        source_bundle_id,
                        sender_name,
                        sender_handle: String::new(),
                        app_icon_hint,
                    },
                )?;
                send_notification_animation(conn, next_msg_id, category)?;
                Ok(vec![format!("Notification '{}' ACKed", source_app)])
            }
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Self::ForwardedNotification(event) => {
                let source = event.source_app.clone();
                send_forwarded_notification(conn, next_msg_id, &event)?;
                Ok(vec![format!(
                    "Forwarded desktop notification from '{}'",
                    source
                )])
            }
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Clone, Debug)]
struct ForwardedNotification {
    source_app: String,
    source_bundle_id: String,
    notification_id: String,
    title: String,
    body: String,
    sender_name: String,
    category: i32,
}

fn is_broken_pipe_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        if let Some(io_err) = cause.downcast_ref::<std::io::Error>() {
            io_err.kind() == std::io::ErrorKind::BrokenPipe
        } else {
            cause
                .to_string()
                .to_ascii_lowercase()
                .contains("broken pipe")
        }
    })
}

fn disconnect_connection(conn: Connection) -> Result<()> {
    let port_name = conn.port_name.clone();
    conn.runtime.block_on(async {
        tokio::time::timeout(BLE_DISCONNECT_TIMEOUT, conn.peripheral.disconnect())
            .await
            .with_context(|| format!("timed out disconnecting from {}", port_name))?
            .context("failed to disconnect BLE peripheral")
    })
}

impl Default for SharedController {
    fn default() -> Self {
        Self {
            ports: list_ports(),
            selected_port_idx: 0,
            conn: None,
            pending_connect: None,
            pending_scan: None,
            pending_disconnect: None,
            pending_command: None,
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
        self.conn.is_some() || self.pending_command.is_some()
    }

    fn is_connecting(&self) -> bool {
        self.pending_connect.is_some()
    }

    fn is_scanning(&self) -> bool {
        self.pending_scan.is_some()
    }

    fn is_disconnecting(&self) -> bool {
        self.pending_disconnect.is_some()
    }

    fn is_command_pending(&self) -> bool {
        self.pending_command.is_some()
    }

    fn pending_command_status(&self) -> Option<&str> {
        self.pending_command
            .as_ref()
            .map(|pending| pending.label.as_str())
    }

    fn scan_status(&self) -> Option<&str> {
        self.pending_scan.as_ref().map(|_| BLE_SCAN_STATUS)
    }

    fn disconnect_status(&self) -> Option<String> {
        self.pending_disconnect.as_ref().map(|pending| {
            format!(
                "Disconnecting from {}...",
                ble_device_display_name(&pending.port_name)
            )
        })
    }

    fn connected_port_name(&self) -> Option<&str> {
        self.conn
            .as_ref()
            .map(|conn| conn.port_name.as_str())
            .or_else(|| {
                self.pending_command
                    .as_ref()
                    .map(|pending| pending.port_name.as_str())
            })
    }

    fn connect_status(&self) -> Option<&str> {
        self.pending_connect
            .as_ref()
            .map(|pending| pending.status.as_str())
    }

    fn connect_target_name(&self) -> Option<&str> {
        self.pending_connect
            .as_ref()
            .map(|pending| ble_device_display_name(&pending.port_name))
    }

    fn poll_connect(&mut self) {
        let mut completed: Option<Result<Connection>> = None;

        if let Some(pending) = self.pending_connect.as_mut() {
            loop {
                match pending.events_rx.try_recv() {
                    Ok(ConnectWorkerEvent::Progress(status)) => {
                        pending.status = status;
                    }
                    Ok(ConnectWorkerEvent::Finished(result)) => {
                        completed = Some(*result);
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        completed = Some(Err(anyhow!("BLE connect worker exited unexpectedly")));
                        break;
                    }
                }
            }
        }

        if let Some(result) = completed {
            let pending = self.pending_connect.take().unwrap();
            let cancelled = pending.cancel_flag.load(Ordering::Relaxed);
            match result {
                Ok(conn) if cancelled => {
                    if let Err(err) = disconnect_connection(conn) {
                        self.log(format!("Error: {}", err));
                    }
                    self.log(format!(
                        "Cancelled connection to {}",
                        ble_device_display_name(&pending.port_name)
                    ));
                }
                Ok(conn) => {
                    let port_name = pending.port_name;
                    self.conn = Some(conn);
                    self.log(format!(
                        "Connected to {}",
                        ble_device_display_name(&port_name)
                    ));
                    self.send_ping();
                }
                Err(err) if cancelled => {
                    self.log(format!(
                        "Cancelled connection to {}",
                        ble_device_display_name(&pending.port_name)
                    ));
                    if !err.to_string().to_ascii_lowercase().contains("cancelled") {
                        self.log(format!("Connect worker stopped: {}", err));
                    }
                }
                Err(err) => self.log(format!("Failed to connect: {:#}", err)),
            }
        }
    }

    fn poll_scan(&mut self) {
        let recv_result = self
            .pending_scan
            .as_ref()
            .map(|pending| pending.result_rx.try_recv());

        match recv_result {
            Some(Ok(result)) => {
                let pending = self.pending_scan.take().unwrap();
                match result {
                    Ok(ports) => {
                        self.ports = ports;
                        if self.selected_port_idx >= self.ports.len() {
                            self.selected_port_idx = 0;
                        }
                        self.log(format!("Found {} BLE device(s)", self.ports.len()));
                    }
                    Err(err) => self.log(format!("Scan failed: {}", err)),
                }
                let elapsed_ms = pending.started_at.elapsed().as_millis();
                if elapsed_ms > 750 {
                    self.log(format!("Scan done in {} ms", elapsed_ms));
                }
            }
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                self.pending_scan.take();
                self.log("Scan worker exited unexpectedly".to_string());
            }
            Some(Err(std::sync::mpsc::TryRecvError::Empty)) | None => {}
        }
    }

    fn poll_disconnect(&mut self) {
        let recv_result = self
            .pending_disconnect
            .as_ref()
            .map(|pending| pending.result_rx.try_recv());

        match recv_result {
            Some(Ok(result)) => {
                let pending = self.pending_disconnect.take().unwrap();
                match result {
                    Ok(()) => self.log(format!("Disconnected from {}", pending.port_name)),
                    Err(err) => self.log(format!("Disconnect failed: {}", err)),
                }
                let elapsed_ms = pending.started_at.elapsed().as_millis();
                if elapsed_ms > 750 {
                    self.log(format!("Disconnect done in {} ms", elapsed_ms));
                }
            }
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                let pending = self.pending_disconnect.take().unwrap();
                self.log(format!(
                    "Disconnect worker for {} exited unexpectedly",
                    pending.port_name
                ));
            }
            Some(Err(std::sync::mpsc::TryRecvError::Empty)) | None => {}
        }
    }

    fn poll_command(&mut self) {
        let recv_result = self
            .pending_command
            .as_ref()
            .map(|pending| pending.result_rx.try_recv());

        match recv_result {
            Some(Ok(result)) => {
                let pending = self.pending_command.take().unwrap();
                let CommandWorkerResult {
                    conn,
                    next_msg_id,
                    outcome,
                } = result;

                self.next_msg_id = next_msg_id;
                match outcome {
                    Ok(messages) => {
                        self.conn = Some(conn);
                        for message in messages {
                            self.log(message);
                        }
                    }
                    Err(err) => {
                        let broken_pipe = is_broken_pipe_error(&err);
                        self.log(format!("Error: {}", err));
                        if broken_pipe {
                            self.log(format!("Connection lost on {}", conn.port_name));
                        } else {
                            self.conn = Some(conn);
                        }
                    }
                }

                let elapsed_ms = pending.started_at.elapsed().as_millis();
                if elapsed_ms > 750 {
                    self.log(format!("{} done in {} ms", pending.label, elapsed_ms));
                }
            }
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                let pending = self.pending_command.take().unwrap();
                self.log(format!(
                    "Command worker for {} exited unexpectedly",
                    pending.port_name
                ));
            }
            Some(Err(std::sync::mpsc::TryRecvError::Empty)) | None => {}
        }
    }

    fn start_command(&mut self, request: CommandRequest) {
        if self.pending_connect.is_some() {
            self.log("Connection in progress");
            return;
        }

        if self.pending_scan.is_some() {
            self.log("Scan in progress");
            return;
        }

        if self.pending_disconnect.is_some() {
            self.log("Disconnect in progress");
            return;
        }

        if self.pending_command.is_some() {
            self.log("Another command is already in progress");
            return;
        }

        let Some(mut conn) = self.conn.take() else {
            self.log("Not connected");
            return;
        };

        let label = request.label();
        let port_name = conn.port_name.clone();
        let mut next_msg_id = self.next_msg_id;
        let (result_tx, result_rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            let outcome = request.execute(&mut conn, &mut next_msg_id);
            let _ = result_tx.send(CommandWorkerResult {
                conn,
                next_msg_id,
                outcome,
            });
        });

        self.pending_command = Some(PendingCommand {
            label: label.clone(),
            port_name,
            started_at: Instant::now(),
            result_rx,
        });
        self.log(label);
    }

    fn scan_ports(&mut self) {
        if self.pending_scan.is_some() {
            self.log("Scan already in progress");
            return;
        }

        if self.pending_connect.is_some() {
            self.log("Connection in progress");
            return;
        }

        if self.pending_disconnect.is_some() {
            self.log("Disconnect in progress");
            return;
        }

        if self.pending_command.is_some() {
            self.log("Command in progress");
            return;
        }

        let (result_tx, result_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = result_tx.send(discover_ble_devices());
        });

        self.pending_scan = Some(PendingScan {
            started_at: Instant::now(),
            result_rx,
        });
        self.log(BLE_SCAN_STATUS);
    }

    fn connect(&mut self) {
        if self.conn.is_some() {
            self.log("Already connected");
            return;
        }

        if self.pending_connect.is_some() {
            self.log("Connection already in progress");
            return;
        }

        if self.pending_scan.is_some() {
            self.log("Scan in progress");
            return;
        }

        if self.pending_disconnect.is_some() {
            self.log("Disconnect in progress");
            return;
        }

        let Some(port_name) = self.selected_port_name().map(str::to_string) else {
            self.log("No BLE device selected");
            return;
        };

        let cancel_flag = Arc::new(AtomicBool::new(false));
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        let worker_port_name = port_name.clone();
        let worker_cancel_flag = Arc::clone(&cancel_flag);

        std::thread::spawn(move || {
            let result =
                Connection::new_with_progress(worker_port_name, &worker_cancel_flag, |status| {
                    let _ = events_tx.send(ConnectWorkerEvent::Progress(status.to_string()));
                });
            let _ = events_tx.send(ConnectWorkerEvent::Finished(Box::new(result)));
        });

        self.pending_connect = Some(PendingConnection {
            port_name: port_name.clone(),
            started_at: Instant::now(),
            cancel_flag,
            events_rx,
            status: "Starting BLE scan...".to_string(),
        });
        self.log(format!(
            "Connecting to {}",
            ble_device_display_name(&port_name)
        ));
    }

    fn disconnect(&mut self) {
        if let Some(pending) = self.pending_connect.as_ref() {
            pending.cancel_flag.store(true, Ordering::Relaxed);
            self.log(format!(
                "Cancelling connection to {}",
                ble_device_display_name(&pending.port_name)
            ));
            return;
        }

        if self.pending_disconnect.is_some() {
            self.log("Disconnect already in progress");
            return;
        }

        if self.pending_scan.is_some() {
            self.log("Scan in progress");
            return;
        }

        if let Some(conn) = self.conn.take() {
            let port_name = conn.port_name.clone();
            let (result_tx, result_rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = result_tx.send(disconnect_connection(conn));
            });
            self.pending_disconnect = Some(PendingDisconnect {
                port_name: port_name.clone(),
                started_at: Instant::now(),
                result_rx,
            });
            self.log(format!("Disconnecting from {}", port_name));
        } else if self.pending_command.is_some() {
            self.log("Command in progress; wait for it to finish or time out");
        } else {
            self.log("Already disconnected");
        }
    }

    fn send_ping(&mut self) {
        self.start_command(CommandRequest::Ping);
    }

    fn send_servo(&mut self, angle_deg: f32) {
        self.start_command(CommandRequest::Servo { angle_deg });
    }

    fn send_led(&mut self, pattern_id: u32, repeats: u32) {
        self.start_command(CommandRequest::Led {
            pattern_id,
            repeats,
        });
    }

    fn send_icon(&mut self, icon_id: String) {
        self.start_command(CommandRequest::Icon { icon_id });
    }

    fn send_notification_and_animation(&mut self, preset: &str, from: &str, text: &str) {
        self.start_command(CommandRequest::NotificationAndAnimation {
            preset: preset.to_string(),
            from: from.to_string(),
            text: text.to_string(),
        });
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn forward_desktop_notification(&mut self, event: ForwardedNotification) {
        if self.conn.is_none() || self.pending_command.is_some() || self.pending_connect.is_some() {
            return;
        }

        self.start_command(CommandRequest::ForwardedNotification(event));
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn default_notification_for_preset(
    preset: &str,
    from: &str,
    text: &str,
) -> (String, String, i32, String, String, String) {
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
        payload: Some(pb::wire_message::Payload::SetServo(pb::SetServo {
            angle_deg,
        })),
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn send_forwarded_notification(
    conn: &mut Connection,
    next_msg_id: &mut u32,
    event: &ForwardedNotification,
) -> Result<()> {
    send_notification_message(
        conn,
        next_msg_id,
        pb::NotificationEvent {
            id: if event.notification_id.is_empty() {
                format!("host-{}", *next_msg_id)
            } else {
                format!("host-{}", event.notification_id)
            },
            source_app: event.source_app.clone(),
            title: event.title.clone(),
            body: event.body.clone(),
            urgency: pb::Urgency::Normal as i32,
            category: event.category,
            source_bundle_id: event.source_bundle_id.clone(),
            sender_name: event.sender_name.clone(),
            sender_handle: String::new(),
            app_icon_hint: app_icon_hint_for(
                &event.source_bundle_id,
                &event.source_app,
                event.category,
            ),
        },
    )?;

    send_notification_animation(conn, next_msg_id, event.category)
}

struct Connection {
    port_name: String,
    runtime: tokio::runtime::Runtime,
    peripheral: Peripheral,
    rx_char: Characteristic,
    _tx_char: Characteristic,
    notifications_rx: std::sync::mpsc::Receiver<Vec<u8>>,
    decoder: protocol::StreamDecoder,
}

impl Connection {
    fn check_connect_cancelled(cancel_flag: &AtomicBool) -> Result<()> {
        if cancel_flag.load(Ordering::Relaxed) {
            bail!("BLE connection cancelled");
        }
        Ok(())
    }

    fn new(port_name: String) -> Result<Self> {
        Self::new_with_progress(port_name, &AtomicBool::new(false), |_| {})
    }

    fn new_with_progress<F>(
        port_name: String,
        cancel_flag: &AtomicBool,
        mut on_progress: F,
    ) -> Result<Self>
    where
        F: FnMut(&str),
    {
        let runtime = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
        let display_name = ble_device_display_name(&port_name).to_string();
        let selected_id = port_name
            .split_once('|')
            .map(|(id, _)| id.to_string())
            .unwrap_or_else(|| port_name.clone());

        let (peripheral, rx_char, tx_char, notifications_rx) = runtime.block_on(async {
            Self::check_connect_cancelled(cancel_flag)?;
            on_progress("Creating BLE manager...");
            let manager = Manager::new()
                .await
                .context("failed to create BLE manager")?;
            Self::check_connect_cancelled(cancel_flag)?;

            on_progress("Looking for BLE adapters...");
            let adapters = manager
                .adapters()
                .await
                .context("failed to enumerate BLE adapters")?;
            let Some(adapter) = adapters.into_iter().next() else {
                bail!("no BLE adapter available");
            };

            Self::check_connect_cancelled(cancel_flag)?;
            on_progress("Scanning for BLE devices...");
            adapter
                .start_scan(ScanFilter::default())
                .await
                .context("failed to start BLE scan")?;
            tokio::time::sleep(Duration::from_millis(1500)).await;

            Self::check_connect_cancelled(cancel_flag)?;
            on_progress("Matching selected BLE device...");
            let service_uuid = ble_service_uuid()?;
            let rx_uuid = ble_rx_uuid()?;
            let tx_uuid = ble_tx_uuid()?;

            let peripherals = adapter
                .peripherals()
                .await
                .context("failed to list BLE peripherals")?;

            let mut selected: Option<Peripheral> = None;
            for peripheral in peripherals {
                let Some(props) = peripheral
                    .properties()
                    .await
                    .context("failed to read BLE peripheral properties")?
                else {
                    continue;
                };

                let id_text = peripheral.id().to_string();
                let local_name = props.local_name.unwrap_or_default();
                let has_service = props.services.contains(&service_uuid);

                if id_text == selected_id
                    || local_name
                        .to_ascii_lowercase()
                        .contains(&selected_id.to_ascii_lowercase())
                    || (selected_id.eq_ignore_ascii_case(protocol::BLE_DEVICE_NAME)
                        && (has_service
                            || local_name
                                .to_ascii_lowercase()
                                .contains(&protocol::BLE_DEVICE_NAME.to_ascii_lowercase())))
                {
                    selected = Some(peripheral);
                    break;
                }
            }

            let peripheral =
                selected.ok_or_else(|| anyhow!("BLE device '{}' not found", selected_id))?;

            #[cfg(target_os = "linux")]
            {
                Self::check_connect_cancelled(cancel_flag)?;
                on_progress("Pairing via BlueZ...");
                ensure_bluez_paired_and_trusted(&peripheral)
                    .context("failed to pair/trust device via BlueZ")?;
            }

            Self::check_connect_cancelled(cancel_flag)?;
            on_progress("Opening BLE link...");
            tokio::time::timeout(BLE_CONNECT_TIMEOUT, peripheral.connect())
                .await
                .context("timed out connecting to BLE peripheral")?
                .context("failed to connect BLE peripheral")?;
            Self::check_connect_cancelled(cancel_flag)?;

            on_progress("Discovering BLE services...");
            tokio::time::timeout(BLE_DISCOVER_TIMEOUT, peripheral.discover_services())
                .await
                .context("timed out discovering BLE services")?
                .context("failed to discover BLE services")?;

            let chars = peripheral.characteristics();
            let rx_char = chars
                .iter()
                .find(|c| c.uuid == rx_uuid)
                .cloned()
                .ok_or_else(|| anyhow!("BLE RX characteristic not found"))?;
            let tx_char = chars
                .iter()
                .find(|c| c.uuid == tx_uuid)
                .cloned()
                .ok_or_else(|| anyhow!("BLE TX characteristic not found"))?;

            Self::check_connect_cancelled(cancel_flag)?;
            on_progress("Subscribing to notifications...");
            tokio::time::timeout(BLE_SUBSCRIBE_TIMEOUT, peripheral.subscribe(&tx_char))
                .await
                .context("timed out subscribing to BLE TX characteristic")?
                .context("failed to subscribe BLE TX characteristic")?;

            Self::check_connect_cancelled(cancel_flag)?;
            on_progress("Waiting for robot responses...");
            let mut notifications =
                tokio::time::timeout(BLE_NOTIFICATIONS_TIMEOUT, peripheral.notifications())
                    .await
                    .context("timed out opening BLE notification stream")?
                    .context("failed to open BLE notification stream")?;
            let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(64);

            tokio::spawn(async move {
                while let Some(ValueNotification { value, .. }) = notifications.next().await {
                    if tx.send(value).is_err() {
                        break;
                    }
                }
            });

            Ok::<_, anyhow::Error>((peripheral, rx_char, tx_char, rx))
        })?;

        Ok(Self {
            port_name: display_name,
            runtime,
            peripheral,
            rx_char,
            _tx_char: tx_char,
            notifications_rx,
            decoder: protocol::StreamDecoder::new(),
        })
    }

    fn send_message(&mut self, msg: pb::WireMessage) -> Result<()> {
        let frame =
            protocol::encode_frame(&msg).context("failed to encode framed protobuf message")?;
        for chunk in frame.chunks(180) {
            self.runtime.block_on(async {
                tokio::time::timeout(
                    BLE_WRITE_TIMEOUT,
                    self.peripheral
                        .write(&self.rx_char, chunk, WriteType::WithResponse),
                )
                .await
                .context("timed out writing BLE RX chunk")?
                .context("failed to write BLE RX chunk")
            })?;
        }
        Ok(())
    }

    fn wait_for_ack(&mut self, expected_msg_id: u32, timeout: Duration) -> Result<()> {
        let start = Instant::now();

        while start.elapsed() < timeout {
            match self
                .notifications_rx
                .recv_timeout(Duration::from_millis(80))
            {
                Ok(bytes) => {
                    self.decoder.push_bytes(&bytes);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("BLE notification stream closed")
                }
            }

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

        bail!("timed out waiting for ACK for msg_id {}", expected_msg_id)
    }

    fn send_and_wait_ack(&mut self, msg: pb::WireMessage) -> Result<()> {
        let msg_id = msg.msg_id;
        self.send_message(msg)?;
        self.wait_for_ack(msg_id, BLE_ACK_TIMEOUT)
    }
}

fn find_default_port() -> Result<String> {
    let ports = discover_ble_devices().context("failed to enumerate BLE devices")?;
    if ports.is_empty() {
        bail!("no BLE devices found");
    }
    Ok(ports[0].clone())
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
    let (led_pattern, led_repeats, excited_deg, rest_deg) =
        animation_profile_for_category(category);

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
fn bluez_pairing_ui_state() -> &'static (Mutex<BluezPairingState>, Condvar) {
    BLUEZ_PAIRING_UI_STATE.get_or_init(|| {
        (
            Mutex::new(BluezPairingState {
                pending: None,
                response: None,
                next_id: 1,
            }),
            Condvar::new(),
        )
    })
}

#[cfg(target_os = "linux")]
fn to_bluez_rejected(err: impl Into<String>) -> ZbusFdoError {
    ZbusFdoError::Failed(err.into())
}

#[cfg(target_os = "linux")]
fn bluez_pairing_current_prompt() -> Option<BluezPairingPrompt> {
    let (lock, _) = bluez_pairing_ui_state();
    lock.lock().ok()?.pending.clone()
}

#[cfg(target_os = "linux")]
fn bluez_pairing_submit_response(response: BluezPairingResponse) -> Result<()> {
    let (lock, cv) = bluez_pairing_ui_state();
    let mut state = lock.lock().map_err(|_| anyhow!("pairing state poisoned"))?;
    if state.pending.is_none() {
        bail!("no pending BlueZ pairing prompt");
    }
    state.response = Some(response);
    cv.notify_all();
    Ok(())
}

#[cfg(target_os = "linux")]
fn bluez_pairing_cancel_pending() {
    let _ = bluez_pairing_submit_response(BluezPairingResponse::Cancelled);
}

#[cfg(target_os = "linux")]
fn bluez_pairing_request(
    device: OwnedObjectPath,
    kind: BluezPairingPromptKind,
) -> Result<BluezPairingResponse> {
    let (lock, cv) = bluez_pairing_ui_state();
    let mut state = lock.lock().map_err(|_| anyhow!("pairing state poisoned"))?;

    while state.pending.is_some() {
        state = cv
            .wait(state)
            .map_err(|_| anyhow!("pairing state poisoned"))?;
    }

    let prompt = BluezPairingPrompt {
        id: state.next_id,
        device: device.to_string(),
        kind,
    };
    state.next_id = state.next_id.saturating_add(1);
    state.pending = Some(prompt);
    state.response = None;
    cv.notify_all();

    // Wake the UI so the pairing dialog is visible even if the main window was hidden.
    if let Some(ctx) = BLUEZ_UI_CTX.get() {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint();
    }

    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Some(response) = state.response.take() {
            state.pending = None;
            cv.notify_all();
            return Ok(response);
        }

        let now = Instant::now();
        if now >= deadline {
            state.pending = None;
            cv.notify_all();
            bail!("pairing input timed out");
        }

        let wait_for = deadline.saturating_duration_since(now);
        let (next_state, _timeout) = cv
            .wait_timeout(state, wait_for)
            .map_err(|_| anyhow!("pairing state poisoned"))?;
        state = next_state;
    }
}

#[cfg(target_os = "linux")]
struct BluezAgent;

#[cfg(target_os = "linux")]
#[interface(name = "org.bluez.Agent1")]
impl BluezAgent {
    fn release(&self) {}

    fn request_pin_code(
        &self,
        device: OwnedObjectPath,
    ) -> std::result::Result<String, ZbusFdoError> {
        match bluez_pairing_request(device, BluezPairingPromptKind::PinCode)
            .map_err(|err| to_bluez_rejected(err.to_string()))?
        {
            BluezPairingResponse::PinCode(pin) if !pin.is_empty() => Ok(pin),
            _ => Err(to_bluez_rejected("pairing PIN rejected".to_string())),
        }
    }

    fn display_pin_code(&self, device: OwnedObjectPath, pincode: &str) {
        eprintln!("BlueZ pairing PIN for {}: {}", device, pincode);
    }

    fn request_passkey(&self, device: OwnedObjectPath) -> std::result::Result<u32, ZbusFdoError> {
        match bluez_pairing_request(device, BluezPairingPromptKind::Passkey)
            .map_err(|err| to_bluez_rejected(err.to_string()))?
        {
            BluezPairingResponse::Passkey(passkey) => Ok(passkey),
            _ => Err(to_bluez_rejected("pairing passkey rejected".to_string())),
        }
    }

    fn display_passkey(&self, device: OwnedObjectPath, passkey: u32, entered: u16) {
        eprintln!(
            "BlueZ passkey for {}: {:06} (entered {})",
            device, passkey, entered
        );
    }

    fn request_confirmation(
        &self,
        device: OwnedObjectPath,
        passkey: u32,
    ) -> std::result::Result<(), ZbusFdoError> {
        match bluez_pairing_request(device, BluezPairingPromptKind::Confirmation { passkey })
            .map_err(|err| to_bluez_rejected(err.to_string()))?
        {
            BluezPairingResponse::Confirmation(true) => Ok(()),
            _ => Err(to_bluez_rejected(
                "pairing confirmation rejected".to_string(),
            )),
        }
    }

    fn request_authorization(
        &self,
        _device: OwnedObjectPath,
    ) -> std::result::Result<(), ZbusFdoError> {
        Ok(())
    }

    fn authorize_service(
        &self,
        _device: OwnedObjectPath,
        _uuid: &str,
    ) -> std::result::Result<(), ZbusFdoError> {
        Ok(())
    }

    fn cancel(&self) {
        bluez_pairing_cancel_pending();
    }
}

#[cfg(target_os = "linux")]
fn start_bluez_pairing_agent() -> Result<()> {
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<std::result::Result<(), String>>(1);

    std::thread::spawn(move || {
        let conn = match ZbusConnection::system() {
            Ok(c) => c,
            Err(err) => {
                let _ = ready_tx.send(Err(format!("failed to connect to system D-Bus: {}", err)));
                return;
            }
        };

        if let Err(err) = conn.object_server().at(BLUEZ_AGENT_PATH, BluezAgent) {
            let _ = ready_tx.send(Err(format!("failed to export BlueZ agent object: {}", err)));
            return;
        }

        let agent_path = match ObjectPath::try_from(BLUEZ_AGENT_PATH) {
            Ok(p) => p,
            Err(err) => {
                let _ = ready_tx.send(Err(format!("invalid BlueZ agent path: {}", err)));
                return;
            }
        };

        let manager =
            match ZbusProxy::new(&conn, "org.bluez", "/org/bluez", "org.bluez.AgentManager1") {
                Ok(p) => p,
                Err(err) => {
                    let _ = ready_tx.send(Err(format!(
                        "failed to create BlueZ AgentManager proxy: {}",
                        err
                    )));
                    return;
                }
            };

        if let Err(err) =
            manager.call::<_, _, ()>("RegisterAgent", &(&agent_path, BLUEZ_AGENT_CAPABILITY))
        {
            let _ = ready_tx.send(Err(format!("failed to register BlueZ agent: {}", err)));
            return;
        }

        // Some systems require policy to set default agent; ignore failures and keep our app-local agent.
        let _ = manager.call::<_, _, ()>("RequestDefaultAgent", &(&agent_path,));

        let _ = ready_tx.send(Ok(()));

        // Keep the connection (and its executor) alive so the object server keeps serving requests.
        let (keepalive_tx, never_rx) = std::sync::mpsc::channel::<()>();
        let _ = never_rx.recv();
        drop(keepalive_tx);
    });

    match ready_rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => bail!(err),
        Err(_) => bail!("timed out starting BlueZ pairing agent"),
    }
}

#[cfg(target_os = "linux")]
fn ensure_bluez_pairing_agent() -> Result<()> {
    let init =
        BLUEZ_AGENT_INIT.get_or_init(|| start_bluez_pairing_agent().map_err(|e| e.to_string()));
    match init {
        Ok(()) => Ok(()),
        Err(err) => bail!("BlueZ pairing agent unavailable: {}", err),
    }
}

#[cfg(target_os = "linux")]
type BluezInterfaceMap =
    std::collections::HashMap<String, std::collections::HashMap<String, OwnedValue>>;

#[cfg(target_os = "linux")]
type BluezManagedObjects = std::collections::HashMap<OwnedObjectPath, BluezInterfaceMap>;

#[cfg(target_os = "linux")]
fn bluez_device_path_for_address(conn: &ZbusConnection, address: &str) -> Result<String> {
    let object_manager =
        ZbusProxy::new(conn, "org.bluez", "/", "org.freedesktop.DBus.ObjectManager")
            .context("failed to create BlueZ object manager proxy")?;

    let objects: BluezManagedObjects = object_manager
        .call("GetManagedObjects", &())
        .context("failed to query BlueZ managed objects")?;

    for (path, interfaces) in objects {
        let Some(device_props) = interfaces.get("org.bluez.Device1") else {
            continue;
        };
        let Some(raw_addr) = device_props.get("Address") else {
            continue;
        };

        let dev_addr: String = ZvariantValue::try_from(raw_addr)
            .and_then(|v| String::try_from(&v))
            .context("failed to decode BlueZ device address")?;

        if dev_addr.eq_ignore_ascii_case(address) {
            return Ok(path.to_string());
        }
    }

    bail!("BlueZ device {} not found", address)
}

#[cfg(target_os = "linux")]
fn bluez_get_device_bool(conn: &ZbusConnection, device_path: &str, property: &str) -> Result<bool> {
    let props = ZbusProxy::new(
        conn,
        "org.bluez",
        device_path,
        "org.freedesktop.DBus.Properties",
    )
    .context("failed to create BlueZ properties proxy")?;

    let value: OwnedValue = props
        .call("Get", &("org.bluez.Device1", property))
        .with_context(|| format!("failed to read BlueZ Device1.{}", property))?;

    value
        .try_into()
        .with_context(|| format!("failed to decode BlueZ Device1.{}", property))
}

#[cfg(target_os = "linux")]
fn bluez_set_device_bool(
    conn: &ZbusConnection,
    device_path: &str,
    property: &str,
    value: bool,
) -> Result<()> {
    let props = ZbusProxy::new(
        conn,
        "org.bluez",
        device_path,
        "org.freedesktop.DBus.Properties",
    )
    .context("failed to create BlueZ properties proxy")?;

    props
        .call::<_, _, ()>(
            "Set",
            &("org.bluez.Device1", property, OwnedValue::from(value)),
        )
        .with_context(|| format!("failed to set BlueZ Device1.{}", property))
}

#[cfg(target_os = "linux")]
fn ensure_bluez_paired_and_trusted(peripheral: &Peripheral) -> Result<()> {
    let address = peripheral.address().to_string();
    let conn = ZbusConnection::system().context("failed to connect to system D-Bus")?;
    let device_path = bluez_device_path_for_address(&conn, &address)?;

    let already_paired = bluez_get_device_bool(&conn, &device_path, "Paired").unwrap_or(false);
    if !already_paired {
        ensure_bluez_pairing_agent().context("failed to start BlueZ pairing agent")?;
        let device = ZbusProxy::new(
            &conn,
            "org.bluez",
            device_path.as_str(),
            "org.bluez.Device1",
        )
        .context("failed to create BlueZ device proxy")?;
        device
            .call::<_, _, ()>("Pair", &())
            .with_context(|| format!("BlueZ pairing failed for {}", address))?;
    }

    bluez_set_device_bool(&conn, &device_path, "Trusted", true)
        .with_context(|| format!("failed to mark {} as trusted", address))?;

    Ok(())
}

#[cfg(target_os = "linux")]
fn extract_quoted(line: &str) -> Option<String> {
    let start = line.find('"')? + 1;
    let end = line[start..].find('"')? + start;
    Some(line[start..end].to_string())
}

#[cfg(target_os = "linux")]
fn run_monitor_loop<F>(mut on_event: F) -> Result<()>
where
    F: FnMut(ForwardedNotification),
{
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
    let dedup_ttl = Duration::from_secs(4);
    let mut recent_signatures: HashMap<String, Instant> = HashMap::new();

    println!("Listening for Linux desktop notifications via dbus-monitor...");
    for line in reader.lines() {
        let line = line.context("failed to read dbus-monitor output")?;

        if line.contains("member=Notify") || line.contains("member=\"Notify\"") {
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

        // Notify(STRING app_name, UINT32 replaces_id, STRING app_icon, STRING summary, STRING body, ...)
        // replaces_id is UINT32 (not a string), so strings order is:
        //   [0] app_name, [1] app_icon, [2] summary (title), [3] body
        if strings.len() >= 4 {
            let source_app = strings[0].clone();
            let title = strings[2].clone();
            let body = strings[3].clone();
            let signature = format!("{}:{}:{}", source_app, title, body);
            let now = Instant::now();
            recent_signatures.retain(|_, seen_at| {
                now.checked_duration_since(*seen_at)
                    .map(|age| age < dedup_ttl)
                    .unwrap_or(false)
            });

            if recent_signatures.contains_key(&signature) {
                collecting = false;
                strings.clear();
                continue;
            }

            recent_signatures.insert(signature, now);
            let category = map_category(&source_app);
            let sender_name = title.split(':').next().unwrap_or("").trim().to_string();

            on_event(ForwardedNotification {
                source_app,
                source_bundle_id: String::new(),
                notification_id: String::new(),
                title,
                body,
                sender_name,
                category,
            });

            collecting = false;
            strings.clear();
        }
    }

    bail!("dbus-monitor exited unexpectedly")
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
fn extract_after<'a>(text: &'a str, marker: &str) -> Option<&'a str> {
    let start = text.find(marker)? + marker.len();
    Some(&text[start..])
}

#[cfg(target_os = "macos")]
fn extract_between(text: &str, start_marker: &str, end_marker: &str) -> Option<String> {
    let start = text.find(start_marker)? + start_marker.len();
    let rest = &text[start..];
    let end = rest.find(end_marker)?;
    let value = rest[..end].trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

#[cfg(target_os = "macos")]
fn read_until_any<'a>(text: &'a str, delims: &[char]) -> &'a str {
    let end = text.find(|c| delims.contains(&c)).unwrap_or(text.len());
    text[..end].trim()
}

#[cfg(target_os = "macos")]
fn extract_bundle_id_from_message(message: &str) -> Option<String> {
    if let Some(rest) = extract_after(message, "bundle=") {
        let bundle = read_until_any(rest, &[',', ']']);
        if !bundle.is_empty() {
            return Some(bundle.to_string());
        }
    }

    if let Some(rest) = extract_after(message, "bundleIdentifier: ") {
        let bundle = read_until_any(rest, &[';', '>']);
        if !bundle.is_empty() {
            return Some(bundle.to_string());
        }
    }

    if let Some(rest) = extract_after(message, "Adding notification to storage: ") {
        let token = read_until_any(rest, &[' ', ']']);
        if let Some((bundle, _)) = token.split_once(':') {
            if !bundle.is_empty() {
                return Some(bundle.to_string());
            }
        }
    }

    if let Some(app) = extract_between(message, "app:\"", "\"") {
        return Some(app);
    }

    None
}

#[cfg(target_os = "macos")]
fn extract_notification_id_from_message(message: &str) -> Option<String> {
    if let Some(rest) = extract_after(message, "id=") {
        let id = read_until_any(rest, &[',', ']']);
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }

    if let Some(rest) = extract_after(message, "Adding notification to storage: ") {
        let token = read_until_any(rest, &[' ', ']']);
        if let Some((_, id)) = token.split_once(':') {
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
    }

    if let Some(id) = extract_between(message, "ident:\"", "\"") {
        return Some(id);
    }

    None
}

#[cfg(target_os = "macos")]
fn should_forward_macos_event(subsystem: &str, event_message: &str) -> bool {
    subsystem == "com.apple.unc"
        && (event_message.starts_with("Presenting <NotificationRecord ")
            || event_message.starts_with("Delivering <NotificationRecord "))
}

#[cfg(target_os = "macos")]
fn extract_macos_notification_event(
    entry: &Value,
) -> Option<(String, String, String, String, String, String, i32)> {
    let subsystem = entry
        .get("subsystem")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let event_message = entry
        .get("eventMessage")
        .and_then(Value::as_str)
        .unwrap_or_default();

    if !should_forward_macos_event(subsystem, event_message) {
        return None;
    }

    let bundle_id = extract_bundle_id_from_message(event_message)
        .or_else(|| {
            entry
                .get("topic")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .or_else(|| {
            entry
                .get("bundle-id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })?;

    let category = map_category(&bundle_id);
    let source_app = bundle_id_to_app_name(&bundle_id);
    let sender_name = if bundle_id.contains("WhatsApp") {
        guess_sender_from_text(event_message)
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
        extract_notification_id_from_message(event_message).unwrap_or_default(),
        title,
        event_message.to_string(),
        sender_name,
        category,
    ))
}

#[cfg(target_os = "macos")]
fn run_monitor_loop<F>(mut on_event: F) -> Result<()>
where
    F: FnMut(ForwardedNotification),
{
    let predicate =
        "process == \"usernoted\" OR subsystem CONTAINS[c] \"UserNotifications\" OR subsystem CONTAINS[c] \"unc\"";
    let mut recent_signatures: HashMap<String, std::time::Instant> = HashMap::new();

    println!("Listening for macOS notification activity via unified log...");
    loop {
        let mut child = Command::new("log")
            .arg("stream")
            .arg("--style")
            .arg("ndjson")
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
            let Ok(entry) = serde_json::from_str::<Value>(&line) else {
                continue;
            };

            let Some((source_app, bundle_id, notification_id, title, body, sender_name, category)) =
                extract_macos_notification_event(&entry)
            else {
                continue;
            };

            let signature = if notification_id.is_empty() {
                format!("{}:{}:{}", bundle_id, title, sender_name)
            } else {
                format!("{}:{}", bundle_id, notification_id)
            };

            let now = std::time::Instant::now();
            recent_signatures
                .retain(|_, seen_at| now.duration_since(*seen_at) < Duration::from_secs(4));
            if recent_signatures
                .get(&signature)
                .is_some_and(|seen_at| now.duration_since(*seen_at) < Duration::from_secs(4))
            {
                continue;
            }

            on_event(ForwardedNotification {
                source_app,
                source_bundle_id: bundle_id,
                notification_id,
                title,
                body,
                sender_name,
                category,
            });

            recent_signatures.insert(signature, now);
        }

        eprintln!("macOS log stream exited; restarting in 1s");
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// landerctl — desktop host bridge for the lander001 desk robot
#[derive(Parser)]
#[command(
    name = "landerctl",
    after_help = "EXAMPLES:
    landerctl
    landerctl --nogui
    landerctl --nogui lander001
    landerctl --nogui --simulate whatsapp --from \"Holger\" --text \"Coffee in 5?\"
    landerctl --nogui --simulate whatsapp --from \"Bob\" --text \"Ping!\" --count 5 --interval-ms 800"
)]
struct Cli {
    /// BLE device id/name to use (auto-detected when omitted)
    port: Option<String>,

    /// Run in headless mode (no window)
    #[arg(long)]
    nogui: bool,

    /// Force GUI even when --simulate is passed
    #[arg(long, hide = true)]
    gui: bool,

    /// Send a simulated notification (whatsapp|mail|calendar|system)
    #[arg(long, value_name = "PRESET")]
    simulate: Option<String>,

    /// Sender name for simulation
    #[arg(long, default_value = "Alice")]
    from: String,

    /// Message text for simulation
    #[arg(long, default_value = "Hey! This is a test message")]
    text: String,

    /// Number of notifications to send
    #[arg(long, default_value_t = 1)]
    count: u32,

    /// Delay between burst notifications in ms
    #[arg(long, default_value_t = 1200, value_name = "MS")]
    interval_ms: u64,
}

fn run_headless(cli: Cli) -> Result<()> {
    if let Some(ref preset) = cli.simulate {
        let preset = preset.to_ascii_lowercase();
        if !matches!(preset.as_str(), "whatsapp" | "mail" | "calendar" | "system") {
            bail!(
                "invalid --simulate preset '{}'; use whatsapp|mail|calendar|system",
                preset
            );
        }

        let port_name = cli.port.map(Ok).unwrap_or_else(find_default_port)?;
        println!("Opening BLE device: {}", port_name);
        let mut conn = Connection::new(port_name)?;
        let mut next_msg_id = 1_u32;
        send_ping_message(&mut conn, &mut next_msg_id)?;
        println!("Sent Ping and received ACK");

        println!(
            "Sending {} simulated '{}' notification(s) from '{}'...",
            cli.count, preset, cli.from
        );
        for index in 0..cli.count {
            send_simulated_notification(
                &mut conn,
                &mut next_msg_id,
                &preset,
                &cli.from,
                &cli.text,
            )?;
            println!("Simulated notification {}/{} ACKed", index + 1, cli.count);
            if index + 1 < cli.count {
                std::thread::sleep(Duration::from_millis(cli.interval_ms));
            }
        }
        return Ok(());
    }

    let port_name = cli.port.map(Ok).unwrap_or_else(find_default_port)?;
    println!("Opening BLE device: {}", port_name);
    let mut conn = Connection::new(port_name)?;
    let mut next_msg_id = 1_u32;
    send_ping_message(&mut conn, &mut next_msg_id)?;
    println!("Sent Ping and received ACK");

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    return run_monitor_loop(|event| {
        if let Err(err) = send_forwarded_notification(&mut conn, &mut next_msg_id, &event) {
            eprintln!(
                "failed to forward Linux notification from '{}': {}",
                event.source_app, err
            );
        }
        println!("forwarded notification from '{}'", event.source_app);
    });

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
    last_tray_connected: Option<String>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    monitor_started: bool,

    #[cfg(target_os = "linux")]
    pairing_prompt_seen_id: Option<u64>,
    #[cfg(target_os = "linux")]
    pairing_pin_input: String,
    #[cfg(target_os = "linux")]
    pairing_passkey_input: String,
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
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            monitor_started: false,

            #[cfg(target_os = "linux")]
            pairing_prompt_seen_id: None,
            #[cfg(target_os = "linux")]
            pairing_pin_input: String::new(),
            #[cfg(target_os = "linux")]
            pairing_passkey_input: String::new(),
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
        Some(ctx.load_texture("lander_icon", color_image, egui::TextureOptions::LINEAR))
    }

    #[cfg(target_os = "linux")]
    fn render_bluez_pairing_modal(&mut self, ctx: &egui::Context) {
        let Some(prompt) = bluez_pairing_current_prompt() else {
            self.pairing_prompt_seen_id = None;
            return;
        };

        if self.pairing_prompt_seen_id != Some(prompt.id) {
            self.pairing_prompt_seen_id = Some(prompt.id);
            self.pairing_pin_input.clear();
            self.pairing_passkey_input.clear();
            // Make sure the window is visible and focused for the pairing dialog.
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }

        egui::Window::new("Bluetooth Pairing")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label("landerctl needs pairing input for BlueZ.");
                ui.label(format!("Device: {}", prompt.device));
                ui.add_space(6.0);

                match prompt.kind {
                    BluezPairingPromptKind::PinCode => {
                        ui.label("Enter PIN code");
                        ui.text_edit_singleline(&mut self.pairing_pin_input);
                        let pin_trimmed = self.pairing_pin_input.trim();
                        let pin_valid = !pin_trimmed.is_empty();
                        if !pin_valid {
                            ui.colored_label(egui::Color32::YELLOW, "PIN cannot be empty");
                        }

                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(pin_valid, egui::Button::new("Submit"))
                                .clicked()
                            {
                                let result = bluez_pairing_submit_response(
                                    BluezPairingResponse::PinCode(pin_trimmed.to_string()),
                                );
                                if let Err(err) = result {
                                    if let Ok(mut ctrl) = self.controller.lock() {
                                        ctrl.log(format!("BlueZ pairing input failed: {}", err));
                                    }
                                }
                            }
                            if ui.button("Reject").clicked() {
                                let _ =
                                    bluez_pairing_submit_response(BluezPairingResponse::Cancelled);
                            }
                        });
                    }
                    BluezPairingPromptKind::Passkey => {
                        ui.label("Enter numeric passkey");
                        ui.text_edit_singleline(&mut self.pairing_passkey_input);
                        let passkey_parse = self.pairing_passkey_input.trim().parse::<u32>();
                        let passkey_valid = passkey_parse.is_ok();
                        if !self.pairing_passkey_input.trim().is_empty() && !passkey_valid {
                            ui.colored_label(egui::Color32::YELLOW, "Passkey must be numeric");
                        }

                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(passkey_valid, egui::Button::new("Submit"))
                                .clicked()
                            {
                                if let Ok(passkey) = passkey_parse {
                                    let _ = bluez_pairing_submit_response(
                                        BluezPairingResponse::Passkey(passkey),
                                    );
                                }
                            }
                            if ui.button("Reject").clicked() {
                                let _ =
                                    bluez_pairing_submit_response(BluezPairingResponse::Cancelled);
                            }
                        });
                    }
                    BluezPairingPromptKind::Confirmation { passkey } => {
                        ui.label(format!("Confirm passkey {:06}", passkey));
                        ui.horizontal(|ui| {
                            if ui.button("Confirm").clicked() {
                                let _ = bluez_pairing_submit_response(
                                    BluezPairingResponse::Confirmation(true),
                                );
                            }
                            if ui.button("Reject").clicked() {
                                let _ = bluez_pairing_submit_response(
                                    BluezPairingResponse::Confirmation(false),
                                );
                            }
                        });
                    }
                }
            });

        ctx.request_repaint_after(Duration::from_millis(50));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn create_tray(
        controller: Arc<Mutex<SharedController>>,
        ui_ctx: egui::Context,
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
            let ui_ctx = ui_ctx.clone();
            let _id = tray
                .add_menu_item_with_id("Show window", move || {
                    ui_ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ui_ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ui_ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    ui_ctx.request_repaint();
                })
                .context("failed to add tray menu item")?;
            #[cfg(target_os = "macos")]
            tray.inner_mut().set_item_sf_symbol(_id, "macwindow");
        }

        {
            let controller = Arc::clone(&controller);
            let _id = tray
                .add_menu_item_with_id("Scan", move || {
                    if let Ok(mut controller) = controller.lock() {
                        controller.scan_ports();
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
                        if controller.is_connected() || controller.is_connecting() {
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
            tray.inner_mut()
                .set_item_sf_symbol(_id, "slider.horizontal.3");
        }

        tray.add_label("LED").context("failed to add tray label")?;
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
        tray.inner_mut()
            .set_item_sf_symbol(_quit_id, "xmark.circle");

        #[cfg(target_os = "macos")]
        tray.inner_mut().display();

        Ok((tray, status_id, connect_id))
    }
}

impl eframe::App for LanderGui {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if ctx.input(|i| i.viewport().close_requested()) {
            // Keep the app alive in the tray when the main window is closed.
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            ctx.request_repaint();
            return;
        }

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if !self.monitor_started {
            let controller = Arc::clone(&self.controller);
            std::thread::spawn(move || {
                run_monitor_loop(|event| {
                    if let Ok(mut ctrl) = controller.lock() {
                        ctrl.forward_desktop_notification(event);
                    }
                })
            });
            self.monitor_started = true;
        }

        #[cfg(target_os = "linux")]
        self.render_bluez_pairing_modal(ctx);

        {
            let mut controller = self.controller.lock().unwrap();
            controller.poll_scan();
            controller.poll_connect();
            controller.poll_disconnect();
            controller.poll_command();
            if controller.is_scanning()
                || controller.is_connecting()
                || controller.is_disconnecting()
                || controller.is_command_pending()
            {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }

        // ── Keyboard shortcut handling ──────────────────────────────────────
        // Read all key-presses in a single borrow, then act on them so that
        // we avoid holding the `input()` borrow while calling `&mut self`.
        let (key_s, key_c, key_d, key_p, key_up, key_down, key_1, key_2, key_3, key_4, key_n) = ctx
            .input(|i| {
                (
                    i.key_pressed(egui::Key::S),
                    i.key_pressed(egui::Key::C),
                    i.key_pressed(egui::Key::D),
                    i.key_pressed(egui::Key::P),
                    i.key_pressed(egui::Key::ArrowRight),
                    i.key_pressed(egui::Key::ArrowLeft),
                    i.key_pressed(egui::Key::Num1),
                    i.key_pressed(egui::Key::Num2),
                    i.key_pressed(egui::Key::Num3),
                    i.key_pressed(egui::Key::Num4),
                    i.key_pressed(egui::Key::N),
                )
            });

        // Only fire shortcuts when no text-edit (or other keyboard-consuming
        // widget) currently holds focus.
        let any_widget_focused = ctx.memory(|m| m.focused().is_some());
        if !any_widget_focused {
            // Snapshot connection state needed to guard some shortcuts.
            let (connected, connecting, scanning, disconnecting, command_pending) = {
                let controller = self.controller.lock().unwrap();
                (
                    controller.is_connected(),
                    controller.is_connecting(),
                    controller.is_scanning(),
                    controller.is_disconnecting(),
                    controller.is_command_pending(),
                )
            };

            if key_s && !scanning && !connecting && !disconnecting && !command_pending {
                self.controller.lock().unwrap().scan_ports();
            }
            if key_c && !connected && !connecting && !scanning && !disconnecting {
                self.controller.lock().unwrap().connect();
            }
            if key_d && connecting {
                self.controller.lock().unwrap().disconnect();
            }
            if key_d && connected && !command_pending && !disconnecting {
                self.controller.lock().unwrap().disconnect();
            }
            if key_p && connected && !command_pending {
                self.controller.lock().unwrap().send_ping();
            }
            if key_up && connected && !command_pending {
                self.servo_angle = (self.servo_angle + 5.0).min(270.0);
                self.controller.lock().unwrap().send_servo(self.servo_angle);
            }
            if key_down && connected && !command_pending {
                self.servo_angle = (self.servo_angle - 5.0).max(0.0);
                self.controller.lock().unwrap().send_servo(self.servo_angle);
            }
            for (pressed, pattern) in [(key_1, 1u32), (key_2, 2), (key_3, 3), (key_4, 4)] {
                if pressed && connected && !command_pending {
                    self.led_pattern = pattern;
                    self.controller
                        .lock()
                        .unwrap()
                        .send_led(pattern, self.led_repeats);
                }
            }
            if key_n && connected && !command_pending {
                let (p, f, t) = (self.preset.clone(), self.from.clone(), self.text.clone());
                self.controller
                    .lock()
                    .unwrap()
                    .send_notification_and_animation(&p, &f, &t);
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
        if let (Some(tray), Some(status_id), Some(connect_id)) =
            (&mut self._tray, self.tray_status_id, self.tray_connect_id)
        {
            let (
                is_connected,
                is_scanning,
                is_connecting,
                is_disconnecting,
                command_pending,
                port_name,
                scan_status,
                connect_target_name,
                disconnect_status,
                command_status,
            ) = {
                let ctrl = self.controller.lock().unwrap();
                (
                    ctrl.is_connected(),
                    ctrl.is_scanning(),
                    ctrl.is_connecting(),
                    ctrl.is_disconnecting(),
                    ctrl.is_command_pending(),
                    ctrl.connected_port_name().map(str::to_string),
                    ctrl.scan_status().map(str::to_string),
                    ctrl.connect_target_name().map(str::to_string),
                    ctrl.disconnect_status(),
                    ctrl.pending_command_status().map(str::to_string),
                )
            };
            let tray_state = if is_scanning {
                "scanning".to_string()
            } else if is_connecting {
                format!(
                    "connecting:{}",
                    connect_target_name.clone().unwrap_or_default()
                )
            } else if is_disconnecting {
                format!("disconnecting:{}", port_name.clone().unwrap_or_default())
            } else if command_pending {
                format!("busy:{}", command_status.clone().unwrap_or_default())
            } else if is_connected {
                format!("connected:{}", port_name.clone().unwrap_or_default())
            } else {
                "disconnected".to_string()
            };
            if self.last_tray_connected.as_deref() != Some(tray_state.as_str()) {
                let status = if is_scanning {
                    scan_status.unwrap_or_else(|| "◌ Scanning...".to_string())
                } else if is_connecting {
                    format!("◌ Connecting: {}", connect_target_name.unwrap_or_default())
                } else if is_disconnecting {
                    disconnect_status.unwrap_or_else(|| "◌ Disconnecting...".to_string())
                } else if command_pending {
                    command_status.unwrap_or_else(|| "◌ Working...".to_string())
                } else if is_connected {
                    format!("● Connected: {}", port_name.unwrap_or_default())
                } else {
                    "● Disconnected".to_string()
                };
                let _ = tray.set_item_label(status_id, &status);
                let connect_label = if is_scanning {
                    "Scan"
                } else if is_connecting {
                    "Cancel connect"
                } else if is_disconnecting {
                    "Disconnecting..."
                } else if is_connected {
                    "Disconnect"
                } else {
                    "Connect"
                };
                let _ = tray.set_item_label(connect_id, connect_label);
                #[cfg(target_os = "macos")]
                {
                    let symbol = if is_scanning {
                        "magnifyingglass"
                    } else if is_connecting || is_disconnecting {
                        "hourglass"
                    } else if is_connected {
                        "wifi.slash"
                    } else {
                        "wifi"
                    };
                    tray.inner_mut().set_item_sf_symbol(connect_id, symbol);
                }
                self.last_tray_connected = Some(tray_state);
            }
        }

        let (
            connected,
            scanning,
            connecting,
            disconnecting,
            command_pending,
            conn_port_name,
            scan_status,
            connect_status,
            disconnect_status,
            command_status,
            scan_started_at,
            connect_started_at,
            disconnect_started_at,
            command_started_at,
            ports,
            mut selected_port_idx,
            logs,
        ) = {
            let ctrl = self.controller.lock().unwrap();
            (
                ctrl.is_connected(),
                ctrl.is_scanning(),
                ctrl.pending_connect.is_some(),
                ctrl.is_disconnecting(),
                ctrl.is_command_pending(),
                ctrl.connected_port_name().map(str::to_string),
                ctrl.scan_status().map(str::to_string),
                ctrl.connect_status().map(str::to_string),
                ctrl.disconnect_status(),
                ctrl.pending_command_status().map(str::to_string),
                ctrl.pending_scan.as_ref().map(|pending| pending.started_at),
                ctrl.pending_connect
                    .as_ref()
                    .map(|pending| pending.started_at),
                ctrl.pending_disconnect
                    .as_ref()
                    .map(|pending| pending.started_at),
                ctrl.pending_command
                    .as_ref()
                    .map(|pending| pending.started_at),
                ctrl.ports.clone(),
                ctrl.selected_port_idx,
                ctrl.logs.clone(),
            )
        };

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if let Some(texture) = &self.app_icon_texture {
                    ui.image((texture.id(), egui::vec2(90.0, 90.0)));
                    ui.add_space(6.0);
                }
                ui.heading("landerctl");

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(10.0);
                    if scanning {
                        ui.add(egui::Spinner::new().size(14.0));
                        let elapsed = scan_started_at
                            .map(|started| started.elapsed().as_secs_f32())
                            .unwrap_or_default();
                        ui.label(format!(
                            "{} ({:.1}s)",
                            scan_status
                                .clone()
                                .unwrap_or_else(|| BLE_SCAN_STATUS.to_string()),
                            elapsed
                        ));
                    } else if connecting {
                        ui.add(egui::Spinner::new().size(14.0));
                        let elapsed = connect_started_at
                            .map(|started| started.elapsed().as_secs_f32())
                            .unwrap_or_default();
                        ui.label(format!(
                            "{} ({:.1}s)",
                            connect_status
                                .clone()
                                .unwrap_or_else(|| "Connecting via BLE...".to_string()),
                            elapsed
                        ));
                    } else if disconnecting {
                        ui.add(egui::Spinner::new().size(14.0));
                        let elapsed = disconnect_started_at
                            .map(|started| started.elapsed().as_secs_f32())
                            .unwrap_or_default();
                        ui.label(format!(
                            "{} ({:.1}s)",
                            disconnect_status
                                .clone()
                                .unwrap_or_else(|| "Disconnecting via BLE...".to_string()),
                            elapsed
                        ));
                    } else if command_pending {
                        ui.add(egui::Spinner::new().size(14.0));
                        let elapsed = command_started_at
                            .map(|started| started.elapsed().as_secs_f32())
                            .unwrap_or_default();
                        ui.label(format!(
                            "{} ({:.1}s)",
                            command_status
                                .clone()
                                .unwrap_or_else(|| "Waiting for BLE response...".to_string()),
                            elapsed
                        ));
                    } else if let Some(ref port_name) = conn_port_name {
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
                        if ui
                            .add_enabled(
                                !(scanning || connecting || disconnecting || command_pending),
                                egui::Button::new("Scan"),
                            )
                            .clicked()
                        {
                            self.controller.lock().unwrap().scan_ports();
                        }

                        if scanning {
                            ui.add_enabled(false, egui::Button::new("Connect"));
                        } else if connecting {
                            if ui.button("Cancel").clicked() {
                                self.controller.lock().unwrap().disconnect();
                            }
                        } else if !connected {
                            if ui.button("Connect").clicked() {
                                self.controller.lock().unwrap().connect();
                            }
                        } else if disconnecting {
                            ui.add_enabled(false, egui::Button::new("Disconnecting..."));
                        } else if ui
                            .add_enabled(!command_pending, egui::Button::new("Disconnect"))
                            .clicked()
                        {
                            self.controller.lock().unwrap().disconnect();
                        }
                    });

                    ui.horizontal(|ui| {
                        ui.label("BLE device");
                        let old_idx = selected_port_idx;
                        egui::ComboBox::from_id_salt("serial_port_combo")
                            .selected_text(
                                ports
                                    .get(selected_port_idx)
                                    .map(|name| ble_device_display_name(name))
                                    .unwrap_or("(none)"),
                            )
                            .show_ui(ui, |ui| {
                                ui.add_enabled_ui(
                                    !(scanning || connecting || disconnecting || command_pending),
                                    |ui| {
                                        for (idx, name) in ports.iter().enumerate() {
                                            ui.selectable_value(
                                                &mut selected_port_idx,
                                                idx,
                                                ble_device_display_name(name),
                                            );
                                        }
                                    },
                                );
                            });
                        if selected_port_idx != old_idx {
                            self.controller.lock().unwrap().selected_port_idx = selected_port_idx;
                        }
                    });

                    ui.add_enabled_ui(connected && !command_pending, |ui| {
                        ui.horizontal(|ui| {
                            if ui.button("Ping").clicked() {
                                self.controller.lock().unwrap().send_ping();
                            }
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

                    ui.add_enabled_ui(connected && !command_pending, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("Servo");
                            ui.add(
                                egui::Slider::new(&mut self.servo_angle, 0.0..=270.0)
                                    .suffix(" deg"),
                            );
                            if ui.button("Send").clicked() {
                                self.controller.lock().unwrap().send_servo(self.servo_angle);
                            }
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
                        });

                        ui.horizontal(|ui| {
                            ui.label("Icon");
                            ui.text_edit_singleline(&mut self.icon_id);
                            if ui.button("Show").clicked() {
                                self.controller
                                    .lock()
                                    .unwrap()
                                    .send_icon(self.icon_id.clone());
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

                    ui.add_enabled_ui(connected && !command_pending, |ui| {
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
                                let (p, f, t) =
                                    (self.preset.clone(), self.from.clone(), self.text.clone());
                                self.controller
                                    .lock()
                                    .unwrap()
                                    .send_notification_and_animation(&p, &f, &t);
                            }
                        });
                    });
                });

                ui.add_space(16.0);

                // ── Keyboard cheatsheet ─────────────────────────────────
                ui.group(|ui| {
                    ui.set_min_width(ui.available_width());

                    let header_text =
                        egui::RichText::new(format!("{} Keyboard shortcuts", regular::KEYBOARD))
                            .strong();
                    ui.label(header_text);
                    ui.add_space(8.0);
                    egui::Grid::new("cheatsheet_grid")
                        .num_columns(2)
                        .spacing([16.0, 4.0])
                        .show(ui, |ui| {
                            Self::shortcut_hint(ui, &["S"]);
                            ui.label("BLE scan");
                            ui.end_row();

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

                            Self::shortcut_hint(ui, &["1", "2", "3", "4"]);
                            ui.label("Run LED pattern 1-4");
                            ui.end_row();

                            Self::shortcut_hint(ui, &["N"]);
                            ui.label("Send notification + animation");
                            ui.end_row();
                        });
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("Shortcuts inactive while a text field has focus.")
                            .italics()
                            .weak(),
                    );
                });
                // ───────────────────────────────────────────────────────
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading(egui::RichText::new("Activity log").strong());
            ui.separator();

            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for line in &logs {
                        ui.label(line);
                    }
                });
        });
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let wants_headless = cli.nogui || cli.simulate.is_some();
    if wants_headless && !cli.gui {
        return run_headless(cli);
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
            #[cfg(target_os = "linux")]
            let _ = BLUEZ_UI_CTX.set(cc.egui_ctx.clone());
            let mut app = LanderGui {
                app_icon_texture: LanderGui::load_app_icon_texture(&cc.egui_ctx),
                ..Default::default()
            };
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                let controller = Arc::clone(&app.controller);
                match LanderGui::create_tray(controller, cc.egui_ctx.clone()) {
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
