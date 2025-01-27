use crate::{ Config, Job, JsonJob };
use std::{ convert::Infallible, sync::{ Arc, RwLock } };
use axum::{ extract::{ Path, State }, http::StatusCode, response::sse::{ Event, KeepAlive, Sse }, routing::{ get, post }, Json, Router };
use futures::Stream;
use serde::Serialize;
use tokio::{io::AsyncReadExt as _, sync::broadcast};
use async_stream::try_stream;
use tower_http::services::ServeFile;

#[derive(Default, Serialize)]
struct JsonRun {
    ts: String,
    status: u8,
    log: String,
    err: String
}
#[derive(Clone)]
struct AppState {
    config: Arc<RwLock<Config>>,
    broadcast: broadcast::Sender<Job>
}

pub async fn run(config: Arc<RwLock<Config>>, broadcast: broadcast::Sender<Job>) {
    let app = Router::new()
        .route_service("/", ServeFile::new("rnr/web/index.html"))
        .route_service("/favicon.ico", ServeFile::new("rnr/web/favicon.ico"))
        .route("/jobs", get(jobs))
        .route("/jobs/{path}", get(job))
        .route("/jobs/{path}/runs", get(runs))
        .route("/updates", get(updates))
        .with_state(AppState { config, broadcast });
    let listener = tokio::net::TcpListener::bind("0.0.0.0:1234").await.unwrap();
    axum::serve(listener, app).await.unwrap()
}

async fn jobs(State(state): State<AppState>) -> Json<Vec<JsonJob>> {
    Json(state.config.read().unwrap().jobs.values().map(|j| j.to_json()).collect())
}

async fn job(State(state): State<AppState>, Path(path): Path<String>) -> Result<Json<JsonRun>, StatusCode> {
    if let Some(job) = state.config.read().unwrap().jobs.get(&path) {
        if let Some(lastrun) = &job.lastrun {
            return Ok(Json(JsonRun {
                ts: lastrun.start.to_rfc3339(),
                status: lastrun.output.status.code().unwrap() as u8,
                log: String::from_utf8_lossy(&lastrun.output.stdout).into_owned(),
                err: String::from_utf8_lossy(&lastrun.output.stderr).into_owned()
            }));
        }
    }
    Err(StatusCode::NOT_FOUND)
}

async fn runs(State(state): State<AppState>, Path(path): Path<String>) -> Result<Json<Vec<JsonRun>>, StatusCode> {
    let dirpath = match state.config.read() {
        Ok(config) => config.dir.join(path).join("runs"),
        Err(_) => return Err(StatusCode::NOT_FOUND)
    };
    if !dirpath.is_dir() { return Err(StatusCode::NOT_FOUND); }
    let mut res = vec![];
    match tokio::fs::read_dir(&dirpath).await {
        Ok(mut dir) => {
            while let Ok(Some(entry)) = dir.next_entry().await {
                match entry.file_type().await {
                    Ok(ftype) => {
                        if !ftype.is_dir() { continue; }
                    },
                    Err(_) => continue
                }
                let path = dirpath.join(entry.path());
                let mut run = JsonRun { ts: entry.file_name().to_string_lossy().to_string(), ..Default::default() };
                run.status = match tokio::fs::File::open(path.join("status")).await {
                    Ok(mut file) => {
                        let mut status = String::new();
                        if let Err(e) = file.read_to_string(&mut status).await {
                            eprintln!("Failed to read {}/status even though it exists: {}", path.display(), e);
                            continue;
                        }
                        if status.len() < 2 { // Statusfile should contain at least one digit and a newline
                            eprintln!("Status file in {} is invalid", path.display());
                            continue;
                        }
                        match status[..status.len()-1].parse::<u8>() {
                            Ok(value) => value,
                            Err(e) => {
                                eprintln!("Status file in {} did not contain a valid 8-bit integer: {}", path.display(), e);
                                continue;
                            }
                        }
                    },
                    Err(e) => {
                        eprintln!("Failed to read {}/status: {}", path.display(), e);
                        continue;
                    }
                };
                if let Ok(mut file) = tokio::fs::File::open(path.join("out")).await {
                    if let Err(e) = file.read_to_string(&mut run.log).await {
                        eprintln!("Failed to read {}/out even though it exists: {}", path.display(), e);
                    }
                };
                if let Ok(mut file) = tokio::fs::File::open(path.join("err")).await {
                    if let Err(e) = file.read_to_string(&mut run.err).await {
                        eprintln!("Failed to read {}/err even though it exists: {}", path.display(), e);
                    }
                };
                res.push(run);
            }
        },
        Err(e) => {
            eprintln!("Error while opening runs dir {}: {}", dirpath.display(), e);
            return Err(StatusCode::NOT_FOUND);
        }
    }
    res.sort_by_key(|i| i.ts.clone());
    Ok(Json(res))
}

async fn updates(State(state): State<AppState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = state.broadcast.subscribe();
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
