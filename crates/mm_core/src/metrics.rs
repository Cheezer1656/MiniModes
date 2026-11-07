//! Prometheus metrics: each process renders its numbers into a text
//! snapshot about once a second, and a small loopback HTTP listener serves
//! the latest snapshot at `/metrics`.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chunkedge::prelude::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::runtime::Handle;

/// The text the listener serves; systems replace it.
#[derive(Resource, Clone, Default)]
pub struct MetricsText(Arc<Mutex<String>>);

impl MetricsText {
    pub fn set(&self, text: String) {
        *self.0.lock().unwrap() = text;
    }
}

/// Builds one snapshot in the Prometheus text format.
#[derive(Default)]
pub struct Exposition {
    out: String,
}

impl Exposition {
    /// Starts a metric family; its samples follow.
    pub fn family(&mut self, name: &str, kind: &str, help: &str) -> &mut Self {
        let _ = writeln!(self.out, "# HELP {name} {help}\n# TYPE {name} {kind}");
        self
    }

    pub fn sample(&mut self, name: &str, labels: &[(&str, &str)], value: f64) -> &mut Self {
        self.out.push_str(name);
        if !labels.is_empty() {
            self.out.push('{');
            for (i, (k, v)) in labels.iter().enumerate() {
                if i > 0 {
                    self.out.push(',');
                }
                let v = v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n");
                let _ = write!(self.out, "{k}=\"{v}\"");
            }
            self.out.push('}');
        }
        let _ = writeln!(self.out, " {value}");
        self
    }

    /// A family with one unlabeled sample.
    pub fn single(&mut self, name: &str, kind: &str, help: &str, value: f64) -> &mut Self {
        self.family(name, kind, help).sample(name, &[], value)
    }

    pub fn finish(self) -> String {
        self.out
    }
}

/// Serves [`MetricsText`] on `bind` (keep it on loopback; Prometheus runs
/// on the same machine).
pub fn serve(handle: &Handle, bind: SocketAddr, text: MetricsText) {
    handle.spawn(async move {
        let listener = match TcpListener::bind(bind).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("metrics listener on {bind}: {e}");
                return;
            }
        };
        tracing::info!("metrics on http://{bind}/metrics");
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                continue;
            };
            let text = text.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                // Only the request line matters; stop at the end of the headers.
                while !request.windows(4).any(|w| w == b"\r\n\r\n") && request.len() < 8192 {
                    match tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut buf)).await {
                        Ok(Ok(n)) if n > 0 => request.extend_from_slice(&buf[..n]),
                        _ => return,
                    }
                }
                let response = if request.starts_with(b"GET /metrics ") || request.starts_with(b"GET /metrics?") {
                    let body = text.0.lock().unwrap().clone();
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
                };
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
}

/// How long recent ticks took (the last 10 seconds' worth), timed from
/// `First` to `Last`.
#[derive(Resource, Default)]
pub struct TickTimes {
    start: Option<Instant>,
    samples: VecDeque<f32>,
    /// Ticks that ran over 50 ms since the process started.
    pub slow_ticks: u64,
}

const TICK_SAMPLES: usize = 200;

impl TickTimes {
    /// In milliseconds; 0 before the first tick.
    pub fn quantile(&self, q: f32) -> f32 {
        quantile(self.samples.iter().copied(), q)
    }
}

/// The `q` quantile (0..=1) of `samples`; 0 when empty.
pub fn quantile(samples: impl Iterator<Item = f32>, q: f32) -> f32 {
    let mut sorted: Vec<f32> = samples.collect();
    if sorted.is_empty() {
        return 0.0;
    }
    sorted.sort_by(f32::total_cmp);
    let i = ((sorted.len() - 1) as f32 * q.clamp(0.0, 1.0)).round() as usize;
    sorted[i]
}

pub struct TickTimerPlugin;

impl Plugin for TickTimerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<TickTimes>()
            .add_systems(First, |mut times: ResMut<TickTimes>| times.start = Some(Instant::now()))
            .add_systems(Last, end_tick);
    }
}

fn end_tick(mut times: ResMut<TickTimes>) {
    if let Some(start) = times.start.take() {
        if times.samples.len() == TICK_SAMPLES {
            times.samples.pop_front();
        }
        let ms = start.elapsed().as_secs_f32() * 1000.0;
        if ms > 50.0 {
            times.slow_ticks += 1;
        }
        times.samples.push_back(ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposition_format() {
        let mut e = Exposition::default();
        e.single("up", "gauge", "Up.", 1.0);
        e.family("players", "gauge", "Players.")
            .sample("players", &[("game", "7"), ("name", "a \"b\"")], 3.0);
        assert_eq!(
            e.finish(),
            "# HELP up Up.\n# TYPE up gauge\nup 1\n# HELP players Players.\n# TYPE players gauge\n\
             players{game=\"7\",name=\"a \\\"b\\\"\"} 3\n"
        );
    }

    #[test]
    fn quantiles() {
        let samples = (1..=100).map(|n| n as f32);
        assert_eq!(quantile(samples.clone(), 0.5), 51.0);
        assert_eq!(quantile(samples.clone(), 0.99), 99.0);
        assert_eq!(quantile(samples, 1.0), 100.0);
        assert_eq!(quantile(std::iter::empty(), 0.5), 0.0);
    }
}
