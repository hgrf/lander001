use drive_74hc595::ShiftRegister;
use dummy_pin::DummyPin;
use embedded_graphics::image::{Image, ImageRawLE};
use embedded_graphics::mono_font::ascii::{FONT_10X20, FONT_6X10, FONT_8X13_BOLD};
use embedded_graphics::mono_font::MonoTextStyleBuilder;
use embedded_graphics::primitives::{
    Circle, Line, PrimitiveStyle, PrimitiveStyleBuilder, Rectangle, Triangle,
};
use embedded_graphics::text::Text;
use enumset::enum_set;
use esp_idf_svc::bt::ble::gap::{
    AdvConfiguration, AuthenticationRequest, BleEncryption, BleGapEvent, EspBleGap, IOCapabilities,
    KeyMask, SecurityConfiguration,
};
use esp_idf_svc::bt::ble::gatt::server::{ConnectionId, EspGatts, GattsEvent, TransferId};
use esp_idf_svc::bt::ble::gatt::{
    AutoResponse, GattCharacteristic, GattDescriptor, GattId, GattInterface, GattResponse,
    GattServiceId, GattStatus, Handle, Permission, Property,
};
use esp_idf_svc::bt::{BdAddr, Ble, BtDriver, BtStatus, BtUuid};
use esp_idf_svc::hal::delay::Delay;
use esp_idf_svc::hal::gpio::{Input, PinDriver, Pull};
use esp_idf_svc::hal::ledc::{config::TimerConfig, LedcDriver, LedcTimerDriver};
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::hal::spi::{config::Config as SpiConfig, SpiDeviceDriver, SpiDriver};
use esp_idf_svc::hal::units::FromValueType;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_sys::*;

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;

use mipidsi::options::ColorOrder;
use mipidsi::options::Orientation;
use mipidsi::options::Rotation;
use std::sync::{Arc, Mutex};

#[path = "../../shared/protocol.rs"]
mod protocol;

pub const BLE_SERVICE_UUID_U128: u128 = 0x0ad91b20173440479e173bed82d75f9d;
pub const BLE_TX_CHAR_UUID_U128: u128 = 0x503de214868246c4828fd59144da41be;
pub const BLE_RX_CHAR_UUID_U128: u128 = 0xb6fccb5087be44f3ae22f85485ea42c4;

const BLE_APP_ID: u16 = 0;
const BLE_MAX_CONNECTIONS: usize = 3;
const PAIRING_BUTTON_HOLD_MS: u64 = 1_000;

type BtDriverRef = Arc<BtDriver<'static, Ble>>;
type BleGap = Arc<EspBleGap<'static, Ble, BtDriverRef>>;
type BleGatts = Arc<EspGatts<'static, Ble, BtDriverRef>>;

#[derive(Clone, Debug)]
struct BlePeerConnection {
    peer: BdAddr,
    conn_id: ConnectionId,
    subscribed: bool,
    paired: bool,
}

struct BleState {
    gatt_if: Option<GattInterface>,
    service_handle: Option<Handle>,
    rx_handle: Option<Handle>,
    tx_handle: Option<Handle>,
    tx_cccd_handle: Option<Handle>,
    response: GattResponse,
    connections: Vec<BlePeerConnection>,
    paired_peers: Vec<BdAddr>,
    pairing_mode: bool,
    pairing_code: Option<u32>,
    advertising_enabled: bool,
    advertising_starting: bool,
    adv_data_configured: bool,
    scan_rsp_configured: bool,
}

impl Default for BleState {
    fn default() -> Self {
        Self {
            gatt_if: None,
            service_handle: None,
            rx_handle: None,
            tx_handle: None,
            tx_cccd_handle: None,
            response: GattResponse::new(),
            connections: Vec::new(),
            paired_peers: Vec::new(),
            pairing_mode: false,
            pairing_code: None,
            advertising_enabled: false,
            advertising_starting: false,
            adv_data_configured: false,
            scan_rsp_configured: false,
        }
    }
}

enum WriteAccessResult {
    Handled,
    Ignored,
}

#[derive(Clone)]
struct BleTransport {
    _gap: BleGap,
    gatts: BleGatts,
    state: Arc<Mutex<BleState>>,
    incoming_tx: std::sync::mpsc::SyncSender<Vec<u8>>,
}

impl BleTransport {
    fn new(
        modem: Modem<'static>,
        nvs: EspDefaultNvsPartition,
        incoming_tx: std::sync::mpsc::SyncSender<Vec<u8>>,
    ) -> anyhow::Result<Self> {
        let bt = Arc::new(BtDriver::new(modem, Some(nvs))?);
        let gap = Arc::new(EspBleGap::new(bt.clone())?);
        let gatts = Arc::new(EspGatts::new(bt.clone())?);

        let this = Self {
            _gap: gap,
            gatts,
            state: Arc::new(Mutex::new(BleState::default())),
            incoming_tx,
        };

        this._gap.set_device_name(protocol::BLE_DEVICE_NAME)?;

        let gap_handler = this.clone();
        this._gap.subscribe(move |event| {
            gap_handler.check_esp_status(gap_handler.on_gap_event(event));
        })?;

        let gatts_handler = this.clone();
        this.gatts.subscribe(move |(gatt_if, event)| {
            gatts_handler.check_esp_status(gatts_handler.on_gatts_event(gatt_if, event));
        })?;

        this.gatts.register_app(BLE_APP_ID)?;
        Ok(this)
    }

    fn notify_all(&self, data: &[u8]) {
        let (gatt_if, tx_handle, targets) = {
            let state = self.state.lock().unwrap();
            let targets = state
                .connections
                .iter()
                .filter(|c| c.subscribed)
                .map(|c| c.conn_id)
                .collect::<Vec<_>>();
            (state.gatt_if, state.tx_handle, targets)
        };

        let Some(gatt_if) = gatt_if else {
            return;
        };
        let Some(tx_handle) = tx_handle else {
            return;
        };

        for conn_id in targets {
            if let Err(err) = self.gatts.notify(gatt_if, conn_id, tx_handle, data) {
                log::warn!("BLE notify failed: {}", err);
            }
        }
    }

    fn enter_pairing_mode(&self, pairing_code: u32) -> anyhow::Result<()> {
        {
            let mut state = self.state.lock().unwrap();
            state.pairing_mode = true;
            state.pairing_code = Some(pairing_code);
            state.advertising_starting = false;
        }

        let security = SecurityConfiguration {
            auth_req_mode: AuthenticationRequest::MitmBonding,
            io_capabilities: IOCapabilities::DisplayOnly,
            initiator_key: Some(KeyMask::EncryptionKey | KeyMask::IdentityResolvingKey),
            responder_key: Some(KeyMask::EncryptionKey | KeyMask::IdentityResolvingKey),
            max_key_size: Some(16),
            min_key_size: Some(7),
            static_passkey: Some(pairing_code),
            only_accept_specified_auth: true,
            enable_oob: false,
        };

        // needs fix from esp-idf-svc 0.52
        self._gap.set_security_conf(&security)?;

        self.maybe_start_advertising()?;

        Ok(())
    }

    fn configure_pairing_advertising(&self) -> anyhow::Result<()> {
        {
            let mut state = self.state.lock().unwrap();
            state.adv_data_configured = false;
            state.scan_rsp_configured = false;
            state.advertising_starting = false;
        }

        self._gap.set_adv_conf(&AdvConfiguration {
            set_scan_rsp: false,
            // Keep payload under 31-byte legacy ADV limit when advertising 128-bit service UUID.
            include_name: false,
            include_txpower: false,
            // 0x02 (general discoverable) | 0x04 (BR/EDR not supported)
            flag: 0x06,
            service_uuid: Some(BtUuid::uuid128(BLE_SERVICE_UUID_U128)),
            ..Default::default()
        })?;

        // Publish local name in scan response so host-side name matching remains stable.
        self._gap.set_adv_conf(&AdvConfiguration {
            set_scan_rsp: true,
            include_name: true,
            include_txpower: true,
            ..Default::default()
        })?;

        Ok(())
    }

    fn maybe_start_advertising(&self) -> anyhow::Result<()> {
        let should_start = {
            let mut state = self.state.lock().unwrap();
            let has_connection_capacity = state.connections.len() < BLE_MAX_CONNECTIONS;
            let can_start = has_connection_capacity
                && !state.advertising_enabled
                && !state.advertising_starting
                && state.adv_data_configured
                && state.scan_rsp_configured;

            if can_start {
                state.advertising_starting = true;
            }

            can_start
        };

        if should_start {
            log::info!("Starting BLE advertising");

            if let Err(err) = self._gap.start_advertising() {
                self.state.lock().unwrap().advertising_starting = false;
                log::warn!("BLE advertising start failed: {}", err);
            }
        }

        Ok(())
    }

    fn is_pairing_mode(&self) -> bool {
        self.state.lock().unwrap().pairing_mode
    }

    fn current_pairing_code(&self) -> Option<u32> {
        self.state.lock().unwrap().pairing_code
    }

    fn mark_peer_authenticated(&self, addr: BdAddr, authenticated: bool) {
        {
            let mut state = self.state.lock().unwrap();

            if let Some(conn) = state.connections.iter_mut().find(|conn| conn.peer == addr) {
                conn.paired = authenticated;
            }

            if authenticated {
                if !state.paired_peers.iter().any(|peer| *peer == addr) {
                    state.paired_peers.push(addr);
                }

                if state.pairing_mode {
                    state.pairing_mode = false;
                    state.pairing_code = None;
                }
            }
        }
    }

    fn on_gap_event(&self, event: BleGapEvent) -> anyhow::Result<()> {
        log::info!("BLE GAP event: {:?}", event);
        match event {
            BleGapEvent::AdvertisingConfigured(status) => {
                if matches!(status, BtStatus::Success) {
                    self.state.lock().unwrap().adv_data_configured = true;
                    self.maybe_start_advertising()?;
                } else {
                    anyhow::bail!("BLE bt status: {:?}", status)
                }
            }
            BleGapEvent::ScanResponseConfigured(status) => {
                if matches!(status, BtStatus::Success) {
                    self.state.lock().unwrap().scan_rsp_configured = true;
                    self.maybe_start_advertising()?;
                } else {
                    anyhow::bail!("BLE bt status: {:?}", status)
                }
            }
            BleGapEvent::RawAdvertisingConfigured(status) => {
                if !matches!(status, BtStatus::Success) {
                    anyhow::bail!("BLE bt status: {:?}", status)
                }
            }
            BleGapEvent::AdvertisingStarted(status) => {
                if matches!(status, BtStatus::Success) {
                    let mut state = self.state.lock().unwrap();
                    state.advertising_enabled = true;
                    state.advertising_starting = false;
                } else if matches!(status, BtStatus::Pending) {
                    let mut state = self.state.lock().unwrap();
                    state.advertising_enabled = false;
                    state.advertising_starting = false;
                } else {
                    self.state.lock().unwrap().advertising_starting = false;
                    anyhow::bail!("BLE bt status: {:?}", status)
                }
            }
            BleGapEvent::AdvertisingStopped(status) => {
                if matches!(status, BtStatus::Success | BtStatus::Pending) {
                    let mut state = self.state.lock().unwrap();
                    state.advertising_enabled = false;
                    state.advertising_starting = false;
                    drop(state);
                    self.maybe_start_advertising()?;
                } else {
                    anyhow::bail!("BLE bt status: {:?}", status)
                }
            }
            BleGapEvent::AuthenticationComplete { bd_addr, status } => {
                if matches!(status, BtStatus::Success) {
                    self.mark_peer_authenticated(bd_addr, true);
                    log::info!("BLE peer authenticated: {:?}", bd_addr);
                } else {
                    self.mark_peer_authenticated(bd_addr, false);
                    log::warn!("BLE authentication failed for {:?}: {:?}", bd_addr, status);
                }
            }
            BleGapEvent::PasskeyNotification { addr, passkey } => {
                log::info!("BLE passkey notification for {:?}: {:06}", addr, passkey);
            }
            _ => {}
        }

        Ok(())
    }

    fn on_gatts_event(&self, gatt_if: GattInterface, event: GattsEvent) -> anyhow::Result<()> {
        log::info!("BLE GATTS event: {:?}", event);
        match event {
            GattsEvent::ServiceRegistered { status, app_id } => {
                self.check_gatt_status(status)?;
                if app_id == BLE_APP_ID {
                    self.create_service(gatt_if)?;
                }
            }
            GattsEvent::ServiceCreated {
                status,
                service_handle,
                ..
            } => {
                self.check_gatt_status(status)?;
                self.configure_and_start_service(service_handle)?;
            }
            GattsEvent::CharacteristicAdded {
                status,
                attr_handle,
                service_handle,
                char_uuid,
            } => {
                self.check_gatt_status(status)?;
                self.register_characteristic(service_handle, attr_handle, char_uuid)?;
            }
            GattsEvent::DescriptorAdded {
                status,
                attr_handle,
                service_handle,
                descr_uuid,
            } => {
                self.check_gatt_status(status)?;
                self.register_cccd_descriptor(service_handle, attr_handle, descr_uuid)?;
            }
            GattsEvent::PeerConnected { conn_id, addr, .. } => {
                self.create_conn(conn_id, addr);
                // ESP may stop connectable advertising once a link is established.
                self.state.lock().unwrap().advertising_enabled = false;
                if let Err(err) = self
                    ._gap
                    .set_encryption(addr, BleEncryption::EncryptionMitm)
                {
                    log::warn!("Failed to request BLE encryption for {:?}: {}", addr, err);
                }
            }
            GattsEvent::PeerDisconnected { addr, .. } => {
                self.delete_conn(addr);
                self.state.lock().unwrap().advertising_enabled = false;
                self.maybe_start_advertising()?;
            }
            GattsEvent::Write {
                conn_id,
                trans_id,
                handle,
                offset,
                need_rsp,
                is_prep,
                value,
                ..
            } => match self.recv(conn_id, handle, offset, value)? {
                WriteAccessResult::Handled => {
                    self.send_write_response(
                        gatt_if,
                        conn_id,
                        trans_id,
                        handle,
                        offset,
                        need_rsp,
                        is_prep,
                        value,
                        GattStatus::Ok,
                    )?;
                }
                WriteAccessResult::Ignored => {}
            },
            _ => {}
        }

        Ok(())
    }

    fn create_service(&self, gatt_if: GattInterface) -> anyhow::Result<()> {
        self.state.lock().unwrap().gatt_if = Some(gatt_if);

        self.configure_pairing_advertising()?;
        self.gatts.create_service(
            gatt_if,
            &GattServiceId {
                id: GattId {
                    uuid: BtUuid::uuid128(BLE_SERVICE_UUID_U128),
                    inst_id: 0,
                },
                is_primary: true,
            },
            8,
        )?;

        Ok(())
    }

    fn configure_and_start_service(&self, service_handle: Handle) -> anyhow::Result<()> {
        self.state.lock().unwrap().service_handle = Some(service_handle);
        self.gatts.start_service(service_handle)?;
        self.add_characteristics(service_handle)?;
        Ok(())
    }

    fn add_characteristics(&self, service_handle: Handle) -> anyhow::Result<()> {
        self.gatts.add_characteristic(
            service_handle,
            &GattCharacteristic {
                uuid: BtUuid::uuid128(BLE_RX_CHAR_UUID_U128),
                permissions: enum_set!(Permission::WriteEncryptedMitm),
                properties: enum_set!(Property::Write | Property::WriteNoResponse),
                max_len: 200,
                auto_rsp: AutoResponse::ByApp,
            },
            &[],
        )?;

        self.gatts.add_characteristic(
            service_handle,
            &GattCharacteristic {
                uuid: BtUuid::uuid128(BLE_TX_CHAR_UUID_U128),
                permissions: enum_set!(Permission::ReadEncryptedMitm),
                properties: enum_set!(Property::Notify),
                max_len: 200,
                auto_rsp: AutoResponse::ByApp,
            },
            &[],
        )?;

        Ok(())
    }

    fn register_characteristic(
        &self,
        service_handle: Handle,
        attr_handle: Handle,
        char_uuid: BtUuid,
    ) -> anyhow::Result<()> {
        let add_cccd = {
            let mut state = self.state.lock().unwrap();
            if state.service_handle != Some(service_handle) {
                false
            } else if char_uuid == BtUuid::uuid128(BLE_RX_CHAR_UUID_U128) {
                state.rx_handle = Some(attr_handle);
                false
            } else if char_uuid == BtUuid::uuid128(BLE_TX_CHAR_UUID_U128) {
                state.tx_handle = Some(attr_handle);
                true
            } else {
                false
            }
        };

        if add_cccd {
            self.gatts.add_descriptor(
                service_handle,
                &GattDescriptor {
                    uuid: BtUuid::uuid16(0x2902),
                    permissions: enum_set!(
                        Permission::ReadEncryptedMitm | Permission::WriteEncryptedMitm
                    ),
                },
            )?;
        }

        Ok(())
    }

    fn register_cccd_descriptor(
        &self,
        service_handle: Handle,
        attr_handle: Handle,
        descr_uuid: BtUuid,
    ) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.service_handle == Some(service_handle) && descr_uuid == BtUuid::uuid16(0x2902) {
            state.tx_cccd_handle = Some(attr_handle);
        }
        Ok(())
    }

    fn create_conn(&self, conn_id: ConnectionId, addr: BdAddr) {
        let mut state = self.state.lock().unwrap();
        let already_paired = state.paired_peers.iter().any(|peer| *peer == addr);
        if state.connections.len() < BLE_MAX_CONNECTIONS {
            state.connections.push(BlePeerConnection {
                peer: addr,
                conn_id,
                subscribed: false,
                paired: already_paired,
            });
        }
    }

    fn delete_conn(&self, addr: BdAddr) {
        let mut state = self.state.lock().unwrap();
        if let Some(index) = state.connections.iter().position(|conn| conn.peer == addr) {
            state.connections.swap_remove(index);
        }
    }

    fn recv(
        &self,
        conn_id: ConnectionId,
        handle: Handle,
        offset: u16,
        value: &[u8],
    ) -> anyhow::Result<WriteAccessResult> {
        let (rx_handle, tx_cccd_handle) = {
            let state = self.state.lock().unwrap();
            (state.rx_handle, state.tx_cccd_handle)
        };

        if Some(handle) == tx_cccd_handle {
            if offset == 0 && value.len() == 2 {
                let subscribe = u16::from_le_bytes([value[0], value[1]]) == 0x01;
                let mut state = self.state.lock().unwrap();
                if let Some(conn) = state
                    .connections
                    .iter_mut()
                    .find(|conn| conn.conn_id == conn_id)
                {
                    conn.subscribed = subscribe;
                }
            }
            return Ok(WriteAccessResult::Handled);
        }

        if Some(handle) == rx_handle {
            if self.incoming_tx.try_send(value.to_vec()).is_err() {
                log::warn!("BLE RX queue full, dropping inbound bytes");
            }
            return Ok(WriteAccessResult::Handled);
        }

        Ok(WriteAccessResult::Ignored)
    }

    #[allow(clippy::too_many_arguments)]
    fn send_write_response(
        &self,
        gatt_if: GattInterface,
        conn_id: ConnectionId,
        trans_id: TransferId,
        handle: Handle,
        offset: u16,
        need_rsp: bool,
        is_prep: bool,
        value: &[u8],
        status: GattStatus,
    ) -> anyhow::Result<()> {
        if !need_rsp {
            return Ok(());
        }

        if is_prep && matches!(status, GattStatus::Ok) {
            let mut state = self.state.lock().unwrap();
            state
                .response
                .attr_handle(handle)
                .auth_req(0)
                .offset(offset)
                .value(value)
                .map_err(|_| anyhow::anyhow!("BLE write response too large"))?;
            self.gatts
                .send_response(gatt_if, conn_id, trans_id, status, Some(&state.response))?;
        } else {
            self.gatts
                .send_response(gatt_if, conn_id, trans_id, status, None)?;
        }

        Ok(())
    }

    fn check_esp_status(&self, status: anyhow::Result<()>) {
        if let Err(err) = status {
            log::warn!("BLE status error: {}", err);
        }
    }

    fn check_gatt_status(&self, status: GattStatus) -> anyhow::Result<()> {
        if matches!(status, GattStatus::Ok) {
            Ok(())
        } else {
            anyhow::bail!("BLE gatt status: {:?}", status)
        }
    }
}

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

fn render_pairing_code<D>(display: &mut D, code: u32)
where
    D: DrawTarget<Color = Rgb565>,
    D::Error: core::fmt::Debug,
{
    display.clear(Rgb565::BLACK).unwrap();

    let title_style = MonoTextStyleBuilder::new()
        .font(&FONT_8X13_BOLD)
        .text_color(Rgb565::new(31, 31, 0))
        .build();
    let body_style = MonoTextStyleBuilder::new()
        .font(&FONT_10X20)
        .text_color(Rgb565::WHITE)
        .build();

    Rectangle::new(Point::new(12, 40), Size::new(216, 160))
        .into_styled(
            PrimitiveStyleBuilder::new()
                .stroke_color(Rgb565::new(31, 31, 0))
                .stroke_width(3)
                .build(),
        )
        .draw(display)
        .unwrap();

    Text::new("PAIRING MODE", Point::new(52, 76), title_style)
        .draw(display)
        .unwrap();
    Text::new("Use code:", Point::new(66, 114), title_style)
        .draw(display)
        .unwrap();
    Text::new(&format!("{:06}", code), Point::new(60, 154), body_style)
        .draw(display)
        .unwrap();
    Text::new("Waiting for BLE pair", Point::new(40, 190), title_style)
        .draw(display)
        .unwrap();
}

struct PairingButtonState {
    pressed_since: Option<std::time::Instant>,
    already_triggered: bool,
}

impl PairingButtonState {
    fn new() -> Self {
        Self {
            pressed_since: None,
            already_triggered: false,
        }
    }

    fn poll_long_press(&mut self, is_pressed: bool, now: std::time::Instant) -> bool {
        if is_pressed {
            if self.pressed_since.is_none() {
                self.pressed_since = Some(now);
            }

            if !self.already_triggered
                && self
                    .pressed_since
                    .map(|t| now.duration_since(t).as_millis() >= PAIRING_BUTTON_HOLD_MS as u128)
                    .unwrap_or(false)
            {
                self.already_triggered = true;
                return true;
            }
        } else {
            self.pressed_since = None;
            self.already_triggered = false;
        }

        false
    }
}

fn generate_pairing_code() -> u32 {
    let random = unsafe { esp_idf_svc::sys::esp_random() };
    100_000 + (random % 900_000)
}

fn poll_pairing_button_and_maybe_start(
    button: &PinDriver<'_, Input>,
    pairing_button_state: &mut PairingButtonState,
    ble_transport: &BleTransport,
) {
    let now = std::time::Instant::now();
    let is_pressed = button.is_low();

    if pairing_button_state.poll_long_press(is_pressed, now) {
        let code = generate_pairing_code();
        match ble_transport.enter_pairing_mode(code) {
            Ok(()) => {
                log::info!("Entered BLE pairing mode with code {:06}", code);
            }
            Err(err) => {
                log::warn!("Failed to enter BLE pairing mode: {}", err);
            }
        }
    }
}

fn write_wire_message(
    outbound_tx: &std::sync::mpsc::SyncSender<Vec<u8>>,
    msg: &protocol::pb::WireMessage,
) {
    let frame = match protocol::encode_frame(msg) {
        Ok(f) => f,
        Err(err) => {
            log::warn!("Failed to encode wire message: {}", err);
            return;
        }
    };

    if outbound_tx.try_send(frame).is_err() {
        log::warn!("BLE TX queue full, dropping outbound frame");
    }
}

fn send_ack(
    outbound_tx: &std::sync::mpsc::SyncSender<Vec<u8>>,
    original_msg_id: u32,
    ok: bool,
    error: String,
) {
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

    write_wire_message(outbound_tx, &ack);
}

struct LedAnimator {
    pattern_id: u32,
    repeats_left: u32,
    step: u32,
    phase: bool,
    next_at: Option<std::time::Instant>,
    idle_led4_on: bool,
    idle_next_toggle_at: std::time::Instant,
}

impl LedAnimator {
    fn new() -> Self {
        Self {
            pattern_id: 0,
            repeats_left: 0,
            step: 0,
            phase: false,
            next_at: None,
            idle_led4_on: false,
            idle_next_toggle_at: std::time::Instant::now(),
        }
    }

    fn idle_mask(&self) -> u8 {
        (1_u8 << 1) | if self.idle_led4_on { 1_u8 << 4 } else { 0 }
    }

    fn next_idle_interval_ms() -> u64 {
        const MIN_MS: u64 = 100;
        const MAX_MS: u64 = 2000;
        const SPAN: u64 = MAX_MS - MIN_MS + 1;
        MIN_MS + (unsafe { esp_random() as u64 } % SPAN)
    }

    fn tick_idle<OE, SER, SRCLR, SRCLK, RCLK>(
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
        if now >= self.idle_next_toggle_at {
            self.idle_led4_on = !self.idle_led4_on;
            self.idle_next_toggle_at =
                now + std::time::Duration::from_millis(Self::next_idle_interval_ms());
        }

        drive.load(self.idle_mask());
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
            self.tick_idle(drive, now);
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
            self.next_at = None;
            self.tick_idle(drive, now);
        } else {
            self.next_at = Some(now + std::time::Duration::from_millis(wait_ms));
        }
    }
}

fn apply_wire_message<D, OE, SER, SRCLR, SRCLK, RCLK>(
    msg: protocol::pb::WireMessage,
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
    if let Some(payload) = msg.payload {
        match payload {
            protocol::pb::wire_message::Payload::SetServo(cmd) => {
                servo
                    .set_duty(servo_angle_to_duty(cmd.angle_deg, servo.get_max_duty()))
                    .unwrap();
                log::info!("protobuf: set_servo to {:.1} deg", cmd.angle_deg);
            }
            protocol::pb::wire_message::Payload::LedAnimation(cmd) => {
                drive.enable_output();
                led_animator.schedule(cmd.pattern_id, cmd.repeats);
                led_animator.tick(drive, std::time::Instant::now());
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
    pairing_button: &PinDriver<'_, Input>,
    pairing_button_state: &mut PairingButtonState,
    ble_transport: &BleTransport,
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
        poll_pairing_button_and_maybe_start(pairing_button, pairing_button_state, ble_transport);
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
    drive.load(1_u8 << 1);

    let pairing_button = PinDriver::input(peripherals.pins.gpio9, Pull::Up).unwrap();

    let nvs = EspDefaultNvsPartition::take().unwrap();
    let (incoming_bytes_tx, incoming_bytes_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(32);
    let (outgoing_bytes_tx, outgoing_bytes_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(32);
    let ble_transport = Arc::new(
        BleTransport::new(peripherals.modem, nvs, incoming_bytes_tx)
            .expect("failed to initialize BLE transport"),
    );

    {
        let ble_transport = Arc::clone(&ble_transport);
        std::thread::spawn(move || {
            while let Ok(frame) = outgoing_bytes_rx.recv() {
                let mut start = 0;
                while start < frame.len() {
                    let end = (start + 180).min(frame.len());
                    ble_transport.notify_all(&frame[start..end]);
                    start = end;
                }
            }
        });
    }

    let (cmd_tx, cmd_rx) = std::sync::mpsc::sync_channel::<protocol::pb::WireMessage>(16);
    std::thread::spawn(move || {
        let mut decoder = protocol::StreamDecoder::new();

        loop {
            let Ok(chunk) = incoming_bytes_rx.recv() else {
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            };

            decoder.push_bytes(&chunk);
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
                                    &outgoing_bytes_tx,
                                    msg_id,
                                    false,
                                    "command queue full".to_string(),
                                );
                            }
                        } else if !is_ack {
                            send_ack(&outgoing_bytes_tx, msg_id, true, String::new());
                        }
                    }
                    Err(err) => log::warn!("Dropping invalid protobuf frame: {}", err),
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

    let spi_device = SpiDeviceDriver::new(
        spi_driver,
        None::<esp_idf_svc::hal::gpio::AnyIOPin>,
        &spi_config,
    )
    .unwrap();

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

    let mut notification_hold_until: Option<std::time::Instant> = None;
    let mut led_animator = LedAnimator::new();
    let mut pairing_button_state = PairingButtonState::new();
    let mut rendered_pairing_code: Option<u32> = None;
    log::info!("cycling cat1/cat2/cat3 with random interval");

    loop {
        poll_pairing_button_and_maybe_start(
            &pairing_button,
            &mut pairing_button_state,
            ble_transport.as_ref(),
        );

        if ble_transport.is_pairing_mode() {
            if let Some(code) = ble_transport.current_pairing_code() {
                if rendered_pairing_code != Some(code) {
                    render_pairing_code(&mut display, code);
                    rendered_pairing_code = Some(code);
                }
            }
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
                &pairing_button,
                &mut pairing_button_state,
                ble_transport.as_ref(),
            );
            continue;
        }

        rendered_pairing_code = None;

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
                    &pairing_button,
                    &mut pairing_button_state,
                    ble_transport.as_ref(),
                );
                continue;
            }
            notification_hold_until = None;
        }

        let cat1_delay_ms: u64 = {
            const MIN_MS: u64 = 1000;
            const MAX_MS: u64 = 5000;
            MIN_MS + (unsafe { esp_random() as u64 } % (MAX_MS - MIN_MS + 1))
        };
        let slides: [(&ImageRawLE<Rgb565>, u64); 3] =
            [(&cat, cat1_delay_ms), (&cat2, 100), (&cat3, 100)];

        for (img, delay_ms) in slides.iter() {
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
                std::time::Duration::from_millis(*delay_ms),
                &cmd_rx,
                &mut servo,
                &mut drive,
                &mut led_animator,
                &mut display,
                &cat,
                &cat2,
                &cat3,
                &mut notification_hold_until,
                &pairing_button,
                &mut pairing_button_state,
                ble_transport.as_ref(),
            );

            if let Some(until) = notification_hold_until {
                if std::time::Instant::now() < until {
                    break;
                }
            }
        }
    }
}
