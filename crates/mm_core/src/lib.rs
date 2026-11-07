//! Process setup shared by every Minimodes server binary: config loading,
//! explicit thread budgets, and the network settings for running behind Gate.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use chunkedge::app::{TaskPoolOptions, TaskPoolPlugin, TaskPoolThreadAssignmentPolicy};
use chunkedge::prelude::*;
use chunkedge::{CompressionThreshold, ServerSettings};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::runtime::{Handle, Runtime};

/// Environment variable holding the Velocity modern-forwarding secret shared
/// with Gate (`GATE_VELOCITY_SECRET` on the proxy side).
pub const VELOCITY_SECRET_ENV: &str = "MINIMODES_VELOCITY_SECRET";

/// Reads a TOML config from the path given as the first CLI argument, or
/// `default_path` when none is given.
pub fn load_config<T: DeserializeOwned>(default_path: &str) -> anyhow::Result<T> {
    let path = std::env::args().nth(1).unwrap_or_else(|| default_path.to_owned());
    let path = Path::new(&path);
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config `{}`", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing config `{}`", path.display()))
}

/// The process's tokio runtime, for systems that spawn network tasks.
#[derive(Resource, Clone)]
pub struct Tokio(pub Handle);

/// Exact thread counts for one process. Several processes share four cores,
/// so nothing is sized from `available_parallelism`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThreadConfig {
    /// Tokio workers driving client connections.
    pub tokio_workers: usize,
    /// Bevy `ComputeTaskPool`: runs ECS systems in parallel.
    pub compute: usize,
    /// Bevy `AsyncComputeTaskPool`: runs Luau VM steps across ticks.
    pub async_compute: usize,
    /// Bevy `IoTaskPool`.
    pub io: usize,
}

impl Default for ThreadConfig {
    fn default() -> Self {
        Self {
            tokio_workers: 1,
            compute: 1,
            async_compute: 1,
            io: 1,
        }
    }
}

impl ThreadConfig {
    pub fn tokio_runtime(&self) -> anyhow::Result<Runtime> {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(self.tokio_workers.max(1))
            .thread_name("tokio")
            .enable_all()
            .build()
            .context("building tokio runtime")
    }

    pub fn task_pool_plugin(&self) -> TaskPoolPlugin {
        let fixed = |n: usize| TaskPoolThreadAssignmentPolicy {
            min_threads: n.max(1),
            max_threads: n.max(1),
            percent: 1.0,
            on_thread_spawn: None,
            on_thread_destroy: None,
        };
        let total = self.compute + self.async_compute + self.io;
        TaskPoolPlugin {
            task_pool_options: TaskPoolOptions {
                min_total_threads: total,
                max_total_threads: total,
                io: fixed(self.io),
                async_compute: fixed(self.async_compute),
                compute: fixed(self.compute),
            },
        }
    }
}

/// Network and server settings for a backend that only accepts Gate's
/// loopback connections.
pub struct BackendSettings {
    pub network: NetworkSettings,
    pub server: ServerSettings,
}

impl BackendSettings {
    /// Reads the forwarding secret from [`VELOCITY_SECRET_ENV`]. Compression is
    /// disabled because the hop from Gate is loopback; Gate compresses for players.
    pub fn behind_gate(
        address: SocketAddr,
        max_players: usize,
        runtime: &Runtime,
    ) -> anyhow::Result<Self> {
        let secret = std::env::var(VELOCITY_SECRET_ENV)
            .with_context(|| format!("{VELOCITY_SECRET_ENV} must be set to Gate's velocitySecret"))?;
        anyhow::ensure!(!secret.is_empty(), "{VELOCITY_SECRET_ENV} is empty");

        Ok(Self {
            network: NetworkSettings {
                tokio_handle: Some(runtime.handle().clone()),
                max_players,
                address,
                connection_mode: ConnectionMode::Velocity {
                    secret: Arc::from(secret),
                },
                ..Default::default()
            },
            server: ServerSettings {
                compression_threshold: CompressionThreshold::DEFAULT,
                ..Default::default()
            },
        })
    }

    /// Inserts both settings resources and [`Tokio`]. Must run before
    /// `DefaultPlugins` are added.
    pub fn insert_into(self, app: &mut App) {
        let handle = self.network.tokio_handle.clone().expect("set by behind_gate");
        app.insert_resource(self.network)
            .insert_resource(self.server)
            .insert_resource(Tokio(handle));
    }
}
