//! Minimal offline-mode Minecraft client for integration tests. It completes
//! login and configuration, follows proxy server switches (play →
//! configuration → play), answers keepalives/pings/teleports, and reports what
//! it sees as [`BotEvent`]s.
//!
//! With [`Bot::connect_forwarded`] it signs its own Velocity forwarding data,
//! so it can log into a backend directly, standing in for Gate.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::ErrorKind;
use std::net::SocketAddr;

use anyhow::{Context, bail};
use chunkedge::protocol::math::DVec3;
use chunkedge::protocol::movement_flags::MovementFlags;
use chunkedge::protocol::packets::configuration::{
    self as config, client_information_c2s::ClientInformationC2s,
};
use chunkedge::protocol::packets::handshake::IntentionC2s;
use chunkedge::protocol::packets::handshake::intention_c2s::HandShakeIntent;
use chunkedge::protocol::packets::{login, play};
use chunkedge::protocol::{
    CompressionThreshold, PROTOCOL_VERSION, Packet, PacketDecoder, PacketEncoder, VarInt, ident,
};
use chunkedge_binary::{Bounded, Encode, RawBytes};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use chunkedge::protocol::decode::PacketFrame;
use chunkedge::protocol::uuid::Uuid;
use std::borrow::Cow;

use chunkedge::item::{HashedItemStack, ItemComponent};
use chunkedge::protocol::lp_vec3::LpVec3;
use chunkedge::protocol::packets::play::container_click_c2s::ClickMode;
use chunkedge::protocol::{Hand, ItemStack};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

#[derive(Clone, Debug, PartialEq)]
pub enum BotEvent {
    /// Login succeeded; the proxy or server assigned this profile.
    LoggedIn { uuid: Uuid, username: String },
    /// Configuration finished and the bot is entering the play state.
    Configured,
    /// Play-state login (sent on first join and after every server switch).
    JoinedWorld { entity_id: i32, dimension: String },
    Teleported(DVec3),
    /// The server is moving the bot back into configuration (e.g. Gate switching backends).
    Reconfiguring,
    Chat(String),
    /// A container window opened.
    ScreenOpened { window_id: i32, title: String },
    /// Full contents of a window: each slot's item name (empty for no item or no name).
    ScreenContent { window_id: i32, items: Vec<String> },
    ScreenClosed { window_id: i32 },
    /// One slot of a window changed (window 0 is the player's inventory).
    SlotChanged { window_id: i32, slot: i16, item: String },
    EntitySpawned { entity_id: i32, kind: i32, pos: DVec3 },
    Disconnected(String),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum State {
    Login,
    Configuration,
    Play,
}

pub struct Bot {
    conn: TcpStream,
    dec: PacketDecoder,
    enc: PacketEncoder,
    state: State,
    pub name: String,
    window_id: i32,
    state_id: i32,
    sequence: i32,
    /// Velocity secret, when logging into a backend directly.
    forwarding_secret: Option<String>,
}

/// A stable UUID per bot name, so a bot keeps its identity across servers.
pub fn bot_uuid(name: &str) -> Uuid {
    let mut hi = DefaultHasher::new();
    (name, 1u8).hash(&mut hi);
    let mut lo = DefaultHasher::new();
    (name, 2u8).hash(&mut lo);
    let bits = (u128::from(hi.finish()) << 64) | u128::from(lo.finish());
    // Version 3 / RFC 4122 variant bits, like offline-mode UUIDs.
    chunkedge::protocol::uuid::Builder::from_u128(bits)
        .with_version(chunkedge::protocol::uuid::Version::Md5)
        .with_variant(chunkedge::protocol::uuid::Variant::RFC4122)
        .into_uuid()
}

impl Bot {
    pub async fn connect(addr: SocketAddr, name: &str) -> anyhow::Result<Self> {
        Self::connect_inner(addr, name, None).await
    }

    /// Logs straight into a Velocity-mode backend, forwarding [`bot_uuid`].
    pub async fn connect_forwarded(addr: SocketAddr, name: &str, secret: &str) -> anyhow::Result<Self> {
        Self::connect_inner(addr, name, Some(secret.to_owned())).await
    }

    async fn connect_inner(addr: SocketAddr, name: &str, forwarding_secret: Option<String>) -> anyhow::Result<Self> {
        let conn = TcpStream::connect(addr)
            .await
            .with_context(|| format!("connecting to {addr}"))?;
        conn.set_nodelay(true)?;

        let mut bot = Self {
            conn,
            dec: PacketDecoder::new(),
            enc: PacketEncoder::new(),
            state: State::Login,
            name: name.to_owned(),
            window_id: 0,
            state_id: 0,
            sequence: 0,
            forwarding_secret,
        };

        let host = addr.ip().to_string();
        bot.enc.append_packet(&IntentionC2s {
            protocol_version: VarInt(PROTOCOL_VERSION),
            server_address: host.as_str().into(),
            server_port: addr.port(),
            intent: HandShakeIntent::Login,
        })?;
        bot.enc.append_packet(&login::HelloC2s {
            username: name.into(),
            profile_id: Uuid::nil(),
        })?;
        bot.flush().await?;
        Ok(bot)
    }

    /// Reads until something notable happens, answering protocol chatter on the way.
    pub async fn next_event(&mut self) -> anyhow::Result<BotEvent> {
        loop {
            while let Some(frame) = self.dec.try_next_packet()? {
                let event = self.handle(&frame)?;
                self.flush().await?;
                if let Some(event) = event {
                    return Ok(event);
                }
            }

            self.dec.reserve(64 * 1024);
            let mut buf = self.dec.take_capacity();
            self.conn.readable().await?;
            match self.conn.try_read_buf(&mut buf) {
                Ok(0) => return Ok(BotEvent::Disconnected("connection closed".into())),
                Ok(_) => self.dec.queue_bytes(buf),
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Waits for an event matching `pred`, failing on disconnect.
    pub async fn wait_for(
        &mut self,
        mut pred: impl FnMut(&BotEvent) -> bool,
    ) -> anyhow::Result<BotEvent> {
        loop {
            let event = self.next_event().await?;
            if pred(&event) {
                return Ok(event);
            }
            if let BotEvent::Disconnected(reason) = event {
                bail!("{} disconnected: {reason}", self.name);
            }
        }
    }

    /// Sends a chat command (without the leading `/`).
    pub async fn command(&mut self, command: &str) -> anyhow::Result<()> {
        anyhow::ensure!(self.state == State::Play, "not in play state");
        self.enc.append_packet(&play::ChatCommandC2s {
            command: command.into(),
        })?;
        self.flush().await
    }

    /// Sends an unsigned chat message.
    pub async fn chat(&mut self, message: &str) -> anyhow::Result<()> {
        anyhow::ensure!(self.state == State::Play, "not in play state");
        self.enc.append_packet(&play::ChatC2s {
            message: message.into(),
            timestamp: 0,
            salt: 0,
            signature: None,
            message_count: VarInt(0),
            acknowledgement: Default::default(),
            checksum: 0,
        })?;
        self.flush().await
    }

    /// Moves to `pos` (one movement packet; keep steps short).
    pub async fn move_to(&mut self, pos: DVec3) -> anyhow::Result<()> {
        anyhow::ensure!(self.state == State::Play, "not in play state");
        self.enc.append_packet(&play::MovePlayerPosC2s {
            position: pos,
            flags: MovementFlags::new().with_on_ground(true),
        })?;
        self.flush().await
    }

    /// Selects hotbar slot `idx` (0..=8).
    pub async fn select_hotbar(&mut self, idx: u16) -> anyhow::Result<()> {
        self.enc.append_packet(&play::SetCarriedItemC2s { slot: idx })?;
        self.flush().await
    }

    /// Right-clicks with the held item.
    pub async fn use_item(&mut self) -> anyhow::Result<()> {
        self.sequence += 1;
        self.enc.append_packet(&play::UseItemC2s {
            hand: Hand::Main,
            sequence: VarInt(self.sequence),
            yaw: 0.0,
            pitch: 0.0,
        })?;
        self.flush().await
    }

    /// Clicks `slot` of the open window (`button` 0 = left, 1 = right).
    pub async fn click(&mut self, slot: i16, button: i8) -> anyhow::Result<()> {
        self.click_mode(slot, button, ClickMode::Click).await
    }

    /// Shift-clicks `slot` of the open window.
    pub async fn shift_click(&mut self, slot: i16) -> anyhow::Result<()> {
        self.click_mode(slot, 0, ClickMode::ShiftClick).await
    }

    async fn click_mode(&mut self, slot: i16, button: i8, mode: ClickMode) -> anyhow::Result<()> {
        self.enc.append_packet(&play::ContainerClickC2s {
            window_id: VarInt(self.window_id),
            state_id: VarInt(self.state_id),
            slot_idx: slot,
            button,
            mode,
            slot_changes: Bounded(Cow::Borrowed(&[])),
            carried_item: HashedItemStack::EMPTY,
        })?;
        self.flush().await
    }

    /// Starts digging the block at `pos`, and with `finish` reports it broken
    /// (what a survival client sends once digging completes).
    pub async fn dig(&mut self, pos: [i32; 3], finish: bool) -> anyhow::Result<()> {
        use chunkedge::protocol::packets::play::player_action_c2s::PlayerAction;
        use chunkedge::protocol::{BlockPos, Direction};
        let actions: &[PlayerAction] = if finish {
            &[PlayerAction::StartDestroyBlock, PlayerAction::StopDestroyBlock]
        } else {
            &[PlayerAction::StartDestroyBlock]
        };
        for &action in actions {
            self.sequence += 1;
            self.enc.append_packet(&play::PlayerActionC2s {
                action,
                position: BlockPos::new(pos[0], pos[1], pos[2]),
                direction: Direction::Up,
                sequence: VarInt(self.sequence),
            })?;
        }
        self.flush().await
    }

    /// Right-clicks an entity.
    pub async fn interact(&mut self, entity_id: i32) -> anyhow::Result<()> {
        self.enc.append_packet(&play::InteractC2s {
            entity_id: VarInt(entity_id),
            hand: Hand::Main,
            target: LpVec3([0.0, 1.0, 0.0]),
            sneaking: false,
        })?;
        self.flush().await
    }

    async fn flush(&mut self) -> anyhow::Result<()> {
        let bytes = self.enc.take();
        if !bytes.is_empty() {
            self.conn.write_all(&bytes).await?;
        }
        Ok(())
    }

    fn handle(&mut self, frame: &PacketFrame) -> anyhow::Result<Option<BotEvent>> {
        match self.state {
            State::Login => self.handle_login(frame),
            State::Configuration => self.handle_configuration(frame),
            State::Play => self.handle_play(frame),
        }
    }

    fn handle_login(&mut self, frame: &PacketFrame) -> anyhow::Result<Option<BotEvent>> {
        match frame.id {
            login::LoginCompressionS2c::ID => {
                let pkt: login::LoginCompressionS2c = frame.decode()?;
                self.dec.set_compression(CompressionThreshold(pkt.threshold.0));
                self.enc.set_compression(CompressionThreshold(pkt.threshold.0));
            }
            login::HelloS2c::ID => bail!("server requested encryption (online mode is not supported)"),
            login::CustomQueryS2c::ID => {
                let pkt: login::CustomQueryS2c = frame.decode()?;
                let forwarding = match &self.forwarding_secret {
                    Some(secret) if pkt.channel.as_str() == "velocity:player_info" => {
                        Some(velocity_player_info(secret, &self.name)?)
                    }
                    _ => None,
                };
                self.enc.append_packet(&login::CustomQueryAnswerC2s {
                    message_id: pkt.message_id,
                    data: forwarding.as_deref().map(|d| Bounded(RawBytes(d))),
                })?;
            }
            login::CookieRequestS2c::ID => {
                let pkt: login::CookieRequestS2c = frame.decode()?;
                self.enc.append_packet(&login::CookieResponseC2s {
                    key: pkt.key,
                    payload: None,
                })?;
            }
            login::LoginDisconnectS2c::ID => {
                let pkt: login::LoginDisconnectS2c = frame.decode()?;
                return Ok(Some(BotEvent::Disconnected(pkt.reason.0.to_legacy_lossy())));
            }
            login::LoginFinishedS2c::ID => {
                let pkt: login::LoginFinishedS2c = frame.decode()?;
                let event = BotEvent::LoggedIn {
                    uuid: pkt.uuid,
                    username: pkt.username.0.to_owned(),
                };
                self.enc.append_packet(&login::LoginAcknowledgedC2s)?;
                self.enter_configuration()?;
                return Ok(Some(event));
            }
            _ => {}
        }
        Ok(None)
    }

    /// Mirrors the vanilla client (brand, then client information). When
    /// standing in for Velocity, the brand instead goes right before
    /// `FinishConfiguration`, where a Velocity-mode backend reads it.
    fn enter_configuration(&mut self) -> anyhow::Result<()> {
        self.state = State::Configuration;
        if self.forwarding_secret.is_none() {
            self.send_brand()?;
        }
        self.enc.append_packet(&ClientInformationC2s {
            locale: "en_us".into(),
            view_distance: 4,
            chat_mode: Default::default(),
            chat_colors: true,
            displayed_skin_parts: Default::default(),
            main_arm: Default::default(),
            enable_text_filtering: false,
            allow_server_listings: true,
            particle_mode: Default::default(),
        })
    }

    fn send_brand(&mut self) -> anyhow::Result<()> {
        self.enc.append_packet(&config::CustomPayloadC2s {
            channel: ident!("minecraft:brand").into(),
            data: Bounded(RawBytes(b"\x07vanilla")),
        })
    }

    fn handle_configuration(&mut self, frame: &PacketFrame) -> anyhow::Result<Option<BotEvent>> {
        match frame.id {
            config::SelectKnownPacksS2c::ID => {
                let pkt: config::SelectKnownPacksS2c = frame.decode()?;
                self.enc
                    .append_packet(&config::SelectKnownPacksC2s { packs: pkt.packs })?;
            }
            config::KeepAliveS2c::ID => {
                let pkt: config::KeepAliveS2c = frame.decode()?;
                self.enc.append_packet(&config::KeepAliveC2s(pkt.0))?;
            }
            config::PingS2c::ID => {
                let pkt: config::PingS2c = frame.decode()?;
                self.enc.append_packet(&config::PongC2s(pkt.0))?;
            }
            config::CookieRequestS2c::ID => {
                let pkt: config::CookieRequestS2c = frame.decode()?;
                self.enc.append_packet(&config::CookieResponseC2s {
                    key: pkt.key,
                    payload: None,
                })?;
            }
            config::CodeOfConductS2c::ID => {
                self.enc.append_packet(&config::AcceptCodeOfConductC2s)?;
            }
            config::DisconnectS2c::ID => {
                let pkt: config::DisconnectS2c = frame.decode()?;
                return Ok(Some(BotEvent::Disconnected(pkt.reason.text.to_legacy_lossy())));
            }
            config::FinishConfigurationS2c::ID => {
                if self.forwarding_secret.is_some() {
                    self.send_brand()?;
                }
                self.enc.append_packet(&config::FinishConfigurationC2s)?;
                self.state = State::Play;
                return Ok(Some(BotEvent::Configured));
            }
            _ => {}
        }
        Ok(None)
    }

    fn handle_play(&mut self, frame: &PacketFrame) -> anyhow::Result<Option<BotEvent>> {
        match frame.id {
            play::LoginS2c::ID => {
                let pkt: play::LoginS2c = frame.decode()?;
                return Ok(Some(BotEvent::JoinedWorld {
                    entity_id: pkt.entity_id,
                    dimension: pkt.dimension_name.to_string(),
                }));
            }
            play::KeepAliveS2c::ID => {
                let pkt: play::KeepAliveS2c = frame.decode()?;
                self.enc.append_packet(&play::KeepAliveC2s { id: pkt.id })?;
            }
            play::PingS2c::ID => {
                let pkt: play::PingS2c = frame.decode()?;
                self.enc.append_packet(&play::PongC2s { id: pkt.id })?;
            }
            play::PlayerPositionS2c::ID => {
                let pkt: play::PlayerPositionS2c = frame.decode()?;
                self.enc.append_packet(&play::AcceptTeleportationC2s {
                    teleport_id: pkt.teleport_id,
                    position: pkt.position,
                    yaw: pkt.yaw,
                    pitch: pkt.pitch,
                })?;
                self.enc.append_packet(&play::MovePlayerPosC2s {
                    position: pkt.position,
                    flags: MovementFlags::new().with_on_ground(true),
                })?;
                self.enc.append_packet(&play::PlayerLoadedC2s)?;
                return Ok(Some(BotEvent::Teleported(pkt.position)));
            }
            play::StartConfigurationS2c::ID => {
                self.enc.append_packet(&play::ConfigurationAcknowledgedC2s)?;
                self.enter_configuration()?;
                return Ok(Some(BotEvent::Reconfiguring));
            }
            play::OpenScreenS2c::ID => {
                let pkt: play::OpenScreenS2c = frame.decode()?;
                self.window_id = pkt.window_id.0;
                return Ok(Some(BotEvent::ScreenOpened {
                    window_id: pkt.window_id.0,
                    title: pkt.window_title.text.to_legacy_lossy(),
                }));
            }
            play::ContainerSetContentS2c::ID => {
                let pkt: play::ContainerSetContentS2c = frame.decode()?;
                if pkt.window_id.0 == self.window_id {
                    self.state_id = pkt.state_id.0;
                }
                let items = pkt.slots.iter().map(item_name).collect();
                return Ok(Some(BotEvent::ScreenContent {
                    window_id: pkt.window_id.0,
                    items,
                }));
            }
            play::ContainerSetSlotS2c::ID => {
                let pkt: play::ContainerSetSlotS2c = frame.decode()?;
                return Ok(Some(BotEvent::SlotChanged {
                    window_id: pkt.window_id.0,
                    slot: pkt.slot_idx,
                    item: item_name(&pkt.slot_data),
                }));
            }
            play::ContainerCloseS2c::ID => {
                let pkt: play::ContainerCloseS2c = frame.decode()?;
                if pkt.window_id.0 == self.window_id {
                    self.window_id = 0;
                }
                return Ok(Some(BotEvent::ScreenClosed {
                    window_id: pkt.window_id.0,
                }));
            }
            play::AddEntityS2c::ID => {
                let pkt: play::AddEntityS2c = frame.decode()?;
                return Ok(Some(BotEvent::EntitySpawned {
                    entity_id: pkt.entity_id.0,
                    kind: pkt.kind.0,
                    pos: pkt.position,
                }));
            }
            play::SystemChatS2c::ID => {
                let pkt: play::SystemChatS2c = frame.decode()?;
                return Ok(Some(BotEvent::Chat(pkt.chat.text.to_legacy_lossy())));
            }
            play::DisconnectS2c::ID => {
                let pkt: play::DisconnectS2c = frame.decode()?;
                return Ok(Some(BotEvent::Disconnected(pkt.reason.text.to_legacy_lossy())));
            }
            _ => {}
        }
        Ok(None)
    }
}

/// Velocity modern forwarding (version 1): HMAC-SHA256 signature, then
/// version, client address, UUID, username and an empty property list.
fn velocity_player_info(secret: &str, name: &str) -> anyhow::Result<Vec<u8>> {
    let mut body = Vec::new();
    VarInt(1).encode(&mut body)?;
    "127.0.0.1".encode(&mut body)?;
    bot_uuid(name).encode(&mut body)?;
    name.encode(&mut body)?;
    VarInt(0).encode(&mut body)?;

    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())?;
    mac.update(&body);
    let mut out = mac.finalize().into_bytes().to_vec();
    out.extend_from_slice(&body);
    Ok(out)
}

/// The custom (or item) name of a stack, then a line per lore line, with
/// legacy formatting codes.
fn item_name(stack: &ItemStack) -> String {
    let components = stack.components();
    let mut text = components
        .iter()
        .find_map(|c| match c {
            ItemComponent::CustomName(t) | ItemComponent::ItemName(t) => Some(t.text.to_legacy_lossy()),
            _ => None,
        })
        .unwrap_or_default();
    for c in &components {
        if let ItemComponent::Lore(lines) = c {
            for line in lines {
                text.push('\n');
                text.push_str(&line.text.to_legacy_lossy());
            }
        }
    }
    text
}
