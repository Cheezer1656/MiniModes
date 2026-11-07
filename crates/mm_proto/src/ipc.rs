//! Shard ⇄ lobby messages. Each frame is a big-endian `u32` length followed by
//! a postcard-encoded [`ShardMsg`] or [`LobbyMsg`]. The shard dials the lobby
//! and opens with [`ShardMsg::Hello`].

use std::io;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

use crate::{GameId, GameKind};

/// Frames larger than this are rejected as corrupt.
pub const MAX_FRAME: usize = 1 << 20;

/// Shard number, `N` in `gamehost@N` and `gh-N`.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ShardId(pub u16);

/// Allocated by the lobby; unique across lobby restarts so that instances a
/// shard reports after reconnecting never collide with new ones.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct InstanceId(pub u64);

/// Pairs an [`LobbyMsg::ExpectPlayer`] with its [`ShardMsg::ExpectAck`].
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Ticket(pub u64);

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum GameVersion {
    /// The creator's editable draft.
    Draft,
    /// An immutable published version.
    Published(u32),
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct InstanceSpec {
    pub id: InstanceId,
    pub game: GameId,
    pub version: GameVersion,
    pub kind: GameKind,
    pub max_players: u32,
}

/// One instance as last reported by its shard.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct InstanceStatus {
    pub spec: InstanceSpec,
    pub players: u32,
    /// Players the shard has accepted an `ExpectPlayer` for who haven't arrived yet.
    pub expected: u32,
    /// For match games: the match has started and new players can't join.
    pub started: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiagnosticLevel {
    Info,
    Warn,
    Error,
}

/// Shard → lobby.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub enum ShardMsg {
    /// First frame on every connection. `instances` lets the lobby rebuild its
    /// directory after either side restarts.
    Hello {
        shard: ShardId,
        /// Server name registered with Gate, e.g. `gh-1`.
        gate_name: String,
        instances: Vec<InstanceStatus>,
    },
    /// Sent about once a second.
    Heartbeat {
        instances: Vec<InstanceStatus>,
        tick_p99_ms: f32,
    },
    ExpectAck {
        ticket: Ticket,
        /// False when the instance is gone or full.
        accepted: bool,
    },
    InstanceEnded { instance: InstanceId },
    Diagnostic {
        game: GameId,
        level: DiagnosticLevel,
        message: String,
    },
}

/// Lobby → shard.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub enum LobbyMsg {
    CreateInstance(InstanceSpec),
    /// Reserve a slot for `uuid` in `instance` for a few seconds, then answer
    /// with [`ShardMsg::ExpectAck`]. The lobby moves the player through Gate
    /// only after an accepting ack.
    ExpectPlayer {
        uuid: Uuid,
        instance: InstanceId,
        ticket: Ticket,
    },
    /// Send the instance's players to the lobby and unload it.
    StopInstance { instance: InstanceId },
    /// Stop every instance of a game (moderation).
    KillGame { game: GameId },
}

pub fn encode_frame<T: Serialize>(msg: &T) -> io::Result<Vec<u8>> {
    let mut buf = vec![0; 4];
    buf = postcard::to_extend(msg, buf).map_err(io::Error::other)?;
    let len = buf.len() - 4;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "frame too large"));
    }
    buf[..4].copy_from_slice(&(len as u32).to_be_bytes());
    Ok(buf)
}

pub async fn write_frame<W, T>(w: &mut W, msg: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    w.write_all(&encode_frame(msg)?).await
}

/// Returns `None` on a clean EOF between frames.
pub async fn read_frame<R, T>(r: &mut R) -> io::Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len = [0; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body).await?;
    postcard::from_bytes(&body)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> InstanceStatus {
        InstanceStatus {
            spec: InstanceSpec {
                id: InstanceId(u64::MAX - 3),
                game: GameId(-7),
                version: GameVersion::Published(12),
                kind: GameKind::OpenWorld {
                    persistent_world: true,
                },
                max_players: 40,
            },
            players: 3,
            expected: 1,
            started: false,
        }
    }

    #[tokio::test]
    async fn frames_round_trip() {
        let sent = vec![
            ShardMsg::Hello {
                shard: ShardId(2),
                gate_name: "gh-2".into(),
                instances: vec![status()],
            },
            ShardMsg::Heartbeat {
                instances: vec![],
                tick_p99_ms: 4.5,
            },
            ShardMsg::ExpectAck {
                ticket: Ticket(9),
                accepted: true,
            },
            ShardMsg::Diagnostic {
                game: GameId(1),
                level: DiagnosticLevel::Error,
                message: "x".repeat(10_000),
            },
        ];
        let (mut a, mut b) = tokio::io::duplex(64);
        let writer = {
            let sent = sent.clone();
            tokio::spawn(async move {
                for msg in &sent {
                    write_frame(&mut a, msg).await.unwrap();
                }
            })
        };
        let mut got = Vec::new();
        while let Some(msg) = read_frame::<_, ShardMsg>(&mut b).await.unwrap() {
            got.push(msg);
        }
        writer.await.unwrap();
        assert_eq!(got, sent);
    }

    #[tokio::test]
    async fn lobby_messages_round_trip() {
        let msg = LobbyMsg::ExpectPlayer {
            uuid: Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef),
            instance: InstanceId(5),
            ticket: Ticket(77),
        };
        let frame = encode_frame(&msg).unwrap();
        let got: LobbyMsg = read_frame(&mut frame.as_slice()).await.unwrap().unwrap();
        assert_eq!(got, msg);
    }

    #[tokio::test]
    async fn rejects_oversized_and_truncated_frames() {
        let huge = ((MAX_FRAME + 1) as u32).to_be_bytes();
        let err = read_frame::<_, ShardMsg>(&mut huge.as_slice()).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        let mut frame = encode_frame(&LobbyMsg::KillGame { game: GameId(3) }).unwrap();
        frame.pop();
        assert!(read_frame::<_, LobbyMsg>(&mut frame.as_slice()).await.is_err());
    }
}
