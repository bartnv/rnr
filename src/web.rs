use crate::{ Config, Job, JsonJob, JsonRun, statusfile_to_integer };
use std::{ convert::Infallible, path::PathBuf, sync::{ Arc, RwLock } };
use axum::{ extract::{ Path, State }, http::StatusCode, response::sse::{ Event, KeepAlive, Sse }, routing::{ get, post }, Json, Router };
use futures::Stream;
use serde::Serialize;
use tokio::{ fs, io::AsyncReadExt as _, sync::{broadcast, mpsc} };
use async_stream::try_stream;
use tower_http::services::ServeFile;

#[derive(Clone)]
struct AppState {
    config: Arc<RwLock<Config>>,
    broadcast: broadcast::Sender<Job>,
    spawner: mpsc::Sender<Box<Job>>
}

pub async fn run(config: Arc<RwLock<Config>>, broadcast: broadcast::Sender<Job>, spawner: mpsc::Sender<Box<Job>>) {
    let addr = config.read().unwrap().http.unwrap().clone();
    let app = Router::new()
        .route_service("/", ServeFile::new("rnr/web/index.html"))
        .route_service("/favicon.ico", ServeFile::new("rnr/web/favicon.ico"))
        .route("/noop", get(|| async { StatusCode::OK }))
        .route("/jobs", get(get_jobs))
        .route("/jobs/{path}", get(get_job))
        .route("/jobs/{path}/runs", get(get_runs))
        .route("/jobs/{path}/config", get(get_config))
        .route("/jobs/{path}/start", post(do_start))
        .route("/updates", get(get_updates))
        .with_state(AppState { config, broadcast, spawner });
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    println!("Starting HTTP API on {}", addr);
    axum::serve(listener, app).await.unwrap()
}

async fn get_jobs(State(state): State<AppState>) -> Json<Vec<JsonJob>> {
    Json(state.config.read().unwrap().jobs.values().map(|j| j.to_json()).collect())
}

async fn get_job(State(state): State<AppState>, Path(path): Path<String>) -> Result<Json<JsonRun>, StatusCode> {
    if let Some(job) = state.config.read().unwrap().jobs.get(&path) {
        if let Some(lastrun) = &job.lastrun {
            return Ok(Json(lastrun.to_json()));
        }
    }
    Err(StatusCode::NOT_FOUND)
}

async fn get_runs(State(state): State<AppState>, Path(path): Path<String>) -> Result<Json<Vec<JsonRun>>, StatusCode> {
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
                let path = entry.path();
                let mut run = JsonRun { start: entry.file_name().to_string_lossy().to_string(), ..Default::default() };
                run.status = statusfile_to_integer(path.join("status")).await;
                if let Ok(mut file) = tokio::fs::File::open(path.join("dur")).await {
                    let mut str = String::new();
                    if let Err(e) = file.read_to_string(&mut str).await {
                        eprintln!("Failed to read {}/dur even though it exists: {}", path.display(), e);
                    }
                    else { run.duration = str.parse().unwrap_or_default(); }
                }
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
    res.sort_by_key(|i| i.start.clone());
    Ok(Json(res))
}

async fn get_config(State(state): State<AppState>, Path(path): Path<String>) -> Result<String, StatusCode> {
    let filename = {
        let rconfig = state.config.read().unwrap();
        if !rconfig.jobs.contains_key(&path) {
            eprintln!("Invalid job path requested on config endpoint: {}", path);
            return Err(StatusCode::BAD_REQUEST);
        }
        rconfig.dir.join(&path).join("job.yml").to_path_buf()
    };
    let jobfile = match fs::File::open(filename).await {
        Ok(mut file) => {
            let mut contents = vec![];
            if let Err(e) = file.read_to_end(&mut contents).await {
                eprintln!("Failed to read job.yml in directory \"{}\": {}", path, e);
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
            String::from_utf8_lossy(&contents).into_owned()
        }
        Err(_) => {
            eprintln!("Job \"{}\" requested on config endpoint has no job.yml file", path);
            return Err(StatusCode::NOT_FOUND);
        }
    };
    Ok(jobfile)
}

async fn do_start(State(state): State<AppState>, Path(path): Path<String>) -> StatusCode {
    let mut newjob = match state.config.read().unwrap().jobs.get(&path) {
        Some(job) => Box::new(job.clone_empty()),
        None => return StatusCode::NOT_FOUND
    };
    newjob.laststart = Some(chrono::Local::now());
    state.spawner.send(newjob).await.unwrap();
    StatusCode::OK
}

async fn get_updates(State(state): State<AppState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = state.broadcast.subscribe();
    Sse::new(try_stream! {
        yield Event::default();
        while let Ok(job) = rx.recv().await {
            yield Event::default().data(serde_json::to_string(&job.to_json()).unwrap());
        }
    }).keep_alive(KeepAlive::default())
}
