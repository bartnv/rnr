use crate::{ Config, Job, JsonJob };
use std::{ convert::Infallible, sync::{ Arc, RwLock } };
use axum::{ extract::{ Path, State }, http::StatusCode, response::sse::{ Event, KeepAlive, Sse }, routing::{ get, post }, Json, Router };
use futures::Stream;
use serde::Serialize;
use tokio::sync::broadcast;
use async_stream::try_stream;
use tower_http::services::ServeFile;

#[derive(Default, Serialize)]
struct JsonDetails {
    log: String,
    err: String
}

pub async fn run(config: Arc<RwLock<Config>>, broadcast: broadcast::Sender<Job>) {
    let app = Router::new()
        .route_service("/", ServeFile::new("rnr/web/index-axum.html"))
        .route("/jobs", get(jobs))
        .route("/jobs/{path}", get(job))
        .route("/updates", get(updates))
        .with_state((config, broadcast));
    let listener = tokio::net::TcpListener::bind("0.0.0.0:1234").await.unwrap();
    axum::serve(listener, app).await.unwrap()
}

async fn jobs(State(state): State<(Arc<RwLock<Config>>, broadcast::Sender<Job>)>) -> Json<Vec<JsonJob>> {
    Json(state.0.read().unwrap().jobs.values().map(|j| j.to_json()).collect())
}

async fn job(State(state): State<(Arc<RwLock<Config>>, broadcast::Sender<Job>)>, Path(path): Path<String>) -> Result<Json<JsonDetails>, StatusCode> {
    if let Some(job) = state.0.read().unwrap().jobs.get(&path) {
        if let Some(lastrun) = &job.lastrun {
            return Ok(Json(JsonDetails {
                log: String::from_utf8_lossy(&lastrun.output.stdout).into_owned(),
                err: String::from_utf8_lossy(&lastrun.output.stderr).into_owned()
            }));
        }
    }
    Err(StatusCode::NOT_FOUND)
}

async fn updates(State(state): State<(Arc<RwLock<Config>>, broadcast::Sender<Job>)>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = state.1.subscribe();
    Sse::new(try_stream! {
        yield Event::default();
        loop {
            match rx.recv().await {
                Ok(job) => {
                    let data = serde_json::to_string(&job.to_json()).unwrap();
                    yield Event::default().data(data);
                },
                Err(_) => break
            }
        }
    }).keep_alive(KeepAlive::default())
}
