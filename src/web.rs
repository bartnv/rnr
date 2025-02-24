use crate::{ Config, Job, JsonJob, JsonRun };
use std::{ convert::Infallible, path::PathBuf, sync::{ Arc, RwLock } };
use axum::{ extract::{ Path, State }, http::StatusCode, response::sse::{ Event, KeepAlive, Sse }, routing::{ get, post }, Json, Router };
use futures::Stream;
use serde::Serialize;
use tokio::{ fs, io::AsyncReadExt as _, sync::broadcast };
use async_stream::try_stream;
use tower_http::services::ServeFile;

#[derive(Clone)]
struct AppState {
    config: Arc<RwLock<Config>>,
    broadcast: broadcast::Sender<Job>
}

pub async fn run(config: Arc<RwLock<Config>>, broadcast: broadcast::Sender<Job>) {
    let addr = config.read().unwrap().http.unwrap().clone();
    let app = Router::new()
        .route_service("/", ServeFile::new("rnr/web/index.html"))
        .route_service("/favicon.ico", ServeFile::new("rnr/web/favicon.ico"))
        .route("/jobs", get(get_jobs))
        .route("/jobs/{path}", get(get_job))
        .route("/jobs/{path}/runs", get(get_runs))
        .route("/jobs/{path}/config", get(get_config))
        .route("/updates", get(get_updates))
        .with_state(AppState { config, broadcast });
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
                let path = dirpath.join(entry.path());
                let mut run = JsonRun { start: entry.file_name().to_string_lossy().to_string(), ..Default::default() };
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

async fn get_updates(State(state): State<AppState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = state.broadcast.subscribe();
    Sse::new(try_stream! {
        yield Event::default();
        while let Ok(job) = rx.recv().await {
            yield Event::default().data(serde_json::to_string(&job.to_json()).unwrap());
        }
    }).keep_alive(KeepAlive::default())
}
