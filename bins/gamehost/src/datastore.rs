//! Scripts' DataStore requests, served from Postgres. Each VM gets one
//! tokio worker that answers its requests in order, so a script's writes
//! land in the order it made them. Replies go back into the VM's inbox,
//! unless the scripts restarted meanwhile.

use chunkedge::prelude::*;
use mm_core::Tokio;
use mm_db::datastore::{self as db, Store, Written};
use mm_db::Pool;
use mm_script::{DataEntry, DataReply, DataRequest, DataValue, Event, RequestId, StoreRef};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::instance::Instance;
use crate::script::{ScriptConfig, ScriptGeneration, ScriptHost};
use crate::world::Storage;

type Request = (RequestId, StoreRef, DataRequest);

#[derive(Resource)]
pub struct DataReplies {
    tx: UnboundedSender<(Entity, u64, Event)>,
    rx: UnboundedReceiver<(Entity, u64, Event)>,
}

impl Default for DataReplies {
    fn default() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self { tx, rx }
    }
}

/// The worker serving one VM. A restarted VM gets a new one; the old one
/// finishes what it was sent, and its replies are dropped.
#[derive(Component)]
pub struct DataWorker {
    generation: u64,
    tx: UnboundedSender<Request>,
}

/// Hands the requests from finished steps to their instance's worker.
pub fn forward_requests(
    mut commands: Commands,
    mut layers: Query<(Entity, &mut ScriptHost, &Instance, &ScriptGeneration, Option<&DataWorker>)>,
    storage: Res<Storage>,
    replies: Res<DataReplies>,
    config: Res<ScriptConfig>,
    tokio: Res<Tokio>,
) {
    for (layer, mut host, instance, generation, worker) in &mut layers {
        let requests = host.take_data_requests();
        if requests.is_empty() {
            continue;
        }
        let tx = match worker {
            Some(w) if w.generation == generation.0 => w.tx.clone(),
            _ => {
                let (tx, rx) = mpsc::unbounded_channel();
                let scope = Scope {
                    db: storage.db.clone(),
                    game: instance.spec.game.0,
                    draft: instance.is_draft(),
                    quota: config.data_quota,
                };
                let reply = (replies.tx.clone(), layer, generation.0);
                tokio.0.spawn(serve(scope, rx, reply));
                commands.entity(layer).insert(DataWorker {
                    generation: generation.0,
                    tx: tx.clone(),
                });
                tx
            }
        };
        for request in requests {
            let _ = tx.send(request);
        }
    }
}

/// Queues replies for the VMs that asked.
pub fn deliver_replies(mut replies: ResMut<DataReplies>, mut layers: Query<(&mut ScriptHost, &ScriptGeneration)>) {
    while let Ok((layer, generation, event)) = replies.rx.try_recv() {
        if let Ok((mut host, current)) = layers.get_mut(layer)
            && current.0 == generation
        {
            host.push(event);
        }
    }
}

struct Scope {
    db: Pool,
    game: i64,
    draft: bool,
    quota: i64,
}

async fn serve(scope: Scope, mut rx: UnboundedReceiver<Request>, reply: (UnboundedSender<(Entity, u64, Event)>, Entity, u64)) {
    let (tx, layer, generation) = reply;
    while let Some((request, store, op)) = rx.recv().await {
        let result = handle(&scope, &store, op).await.unwrap_or_else(|e| {
            tracing::error!("DataStore request for game {}: {e}", scope.game);
            Err("the DataStore is unavailable right now; try again later".to_owned())
        });
        let _ = tx.send((layer, generation, Event::DataReply { request, result }));
    }
}

fn to_db(value: DataValue) -> db::Value {
    match value {
        DataValue::Json(s) => db::Value::Json(s),
        DataValue::Int(n) => db::Value::Int(n),
    }
}

fn from_db(entry: db::Entry) -> DataEntry {
    DataEntry {
        value: match entry.value {
            db::Value::Json(s) => DataValue::Json(s),
            db::Value::Int(n) => DataValue::Int(n),
        },
        version: entry.version,
    }
}

/// The outer `Err` is a database failure; the inner one goes to the script.
async fn handle(scope: &Scope, store: &StoreRef, op: DataRequest) -> Result<Result<DataReply, String>, mm_db::Error> {
    let target = Store {
        game: scope.game,
        draft: scope.draft,
        ordered: store.ordered,
        name: &store.name,
    };
    let written = |w: Written| match w {
        Written::Ok(entry) => Ok(DataReply::Written(from_db(entry))),
        Written::Conflict(entry) => Ok(DataReply::Conflict(entry.map(from_db))),
        Written::OverQuota => Err(format!(
            "this game's DataStores are full ({} MB); remove keys to make room",
            scope.quota >> 20
        )),
        Written::NotInteger => Err(format!("IncrementAsync: `{}` doesn't hold a whole number", store.name)),
    };
    Ok(match op {
        DataRequest::Get { key } => Ok(DataReply::Entry(db::get(&scope.db, target, &key).await?.map(from_db))),
        DataRequest::Set { key, value, expect } => {
            written(db::set(&scope.db, target, &key, to_db(value), expect, scope.quota).await?)
        }
        DataRequest::Increment { key, delta } => {
            written(db::increment(&scope.db, target, &key, delta, scope.quota).await?)
        }
        DataRequest::Remove { key } => Ok(DataReply::Entry(db::remove(&scope.db, target, &key).await?.map(from_db))),
        DataRequest::Sorted {
            ascending,
            limit,
            min,
            max,
            after,
        } => {
            let query = db::SortedQuery {
                ascending,
                limit: i64::from(limit),
                after: after.as_ref().map(|(v, k)| (*v, k.as_str())),
                min,
                max,
            };
            Ok(DataReply::Sorted(db::sorted(&scope.db, target, query).await?))
        }
    })
}
