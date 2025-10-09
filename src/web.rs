use crate::{ Config, Job, JsonJob, JsonRun, read_statusfile };
use std::{ convert::Infallible, os::unix::process::ExitStatusExt as _, path::PathBuf, process::ExitStatus, sync::{ Arc, RwLock } };
use axum::{ extract::{ Path, State }, http::StatusCode, response::sse::{ Event, KeepAlive, Sse }, routing::{ get, post }, Json, Router };
use futures::Stream;
use notify::{event::{AccessKind, AccessMode, DataChange, ModifyKind}, EventKind, Watcher};
use serde::Serialize;
use tokio::{ fs::File, io::AsyncReadExt as _, sync::{broadcast, mpsc} };
use async_stream::try_stream;
use tower_http::services::ServeFile;

#[derive(Clone)]
struct AppState {
    config: Arc<RwLock<Config>>,
    broadcast: broadcast::Sender<Job>,
    spawner: mpsc::Sender<Box<Job>>
}

pub async fn run(config: Arc<RwLock<Config>>, broadcast: broadcast::Sender<Job>, spawner: mpsc::Sender<Box<Job>>) {
    let addr = config.read().unwrap().http.unwrap();
    let app = Router::new()
        .route_service("/", ServeFile::new("rnr/web/index.html"))
        .route_service("/favicon.ico", ServeFile::new("rnr/web/favicon.ico"))
        .route("/noop", get(|| async { StatusCode::OK }))
        .route("/jobs", get(get_jobs))
        .route("/jobs/{path}", get(get_job))
        .route("/jobs/{path}/runs", get(get_runs))
        .route("/jobs/{path}/runs/{run}/output", get(get_output))
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
    if let Some(job) = state.config.read().unwrap().jobs.get(&path) && let Some(lastrun) = &job.lastrun {
        return Ok(Json(lastrun.to_json()));
    }
    Err(StatusCode::NOT_FOUND)
}

async fn get_runs(State(state): State<AppState>, Path(path): Path<String>) -> Result<Json<Vec<JsonRun>>, StatusCode> {
    let dirpath = match state.config.read() {
        Ok(config) => config.dir.join(&path).join("runs"),
        Err(_) => return Err(StatusCode::NOT_FOUND)
    };
    if !dirpath.is_dir() { return Err(StatusCode::NOT_FOUND); }
    let running = match state.config.read().unwrap().jobs.get(&path) {
        Some(job) if job.running => job.laststart.map(|v| v.format("%Y-%m-%d %H:%M").to_string()),
        Some(_) => None,
        None => return Err(StatusCode::NOT_FOUND)
    };
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
                let exitstatus = read_statusfile(&path.join("status")).await.map(ExitStatus::from_raw);
                let mut status = match exitstatus {
                    Some(status) => match status.success() { true => "OK", false => "Failure" },
                    None => "Unknown"
                }.to_string();
                let mut statustext = match exitstatus {
                    Some(status) => match status.success() { true => String::new(), false => status.to_string() },
                    None => "exit status not recorded".to_string()
                };
                let start = entry.file_name().to_string_lossy().to_string();
                if let Some(ref running) = running && *running == start {
                    status = "Running".to_string();
                    statustext = String::new();
                }
                let mut run = JsonRun { start, status, statustext, ..Default::default() };
                if let Ok(mut file) = tokio::fs::File::open(path.join("dur")).await {
                    let mut str = String::new();
                    if let Err(e) = file.read_to_string(&mut str).await {
                        eprintln!("Failed to read {}/dur even though it exists: {}", path.display(), e);
                    }
                    else { run.duration = str.parse().ok(); }
                }
                if let Ok(mut file) = tokio::fs::File::open(path.join("out")).await && let Err(e) = file.read_to_string(&mut run.log).await {
                    eprintln!("Failed to read {}/out even though it exists: {}", path.display(), e);
                };
                if let Ok(mut file) = tokio::fs::File::open(path.join("err")).await && let Err(e) = file.read_to_string(&mut run.err).await {
                    eprintln!("Failed to read {}/err even though it exists: {}", path.display(), e);
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
    let jobfile = match File::open(filename).await {
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
    println!("{} [{}] received start request from web interface", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), newjob.path.display());
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

#[derive(Serialize)]
struct RunOutput {
    field: &'static str,
    value: String
}
async fn get_output(State(state): State<AppState>, Path((path, run)): Path<(String, String)>) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    let dirpath = match state.config.read() {
        Ok(config) => config.dir.join(&path).join("runs").join(&run),
        Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR)
    };
    if !dirpath.is_dir() { return Err(StatusCode::NOT_FOUND); }

    let (ntx, mut nrx) = mpsc::channel(10);
    let watchdir = dirpath.clone();
    tokio::task::spawn_blocking(move || {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = notify::RecommendedWatcher::new(tx, notify::Config::default()).unwrap();
        watcher.watch(&watchdir, notify::RecursiveMode::Recursive).unwrap();
        while let Ok(res) = rx.recv() {
            match res {
                Ok(event) => if ntx.blocking_send(event).is_err() { break; },
                Err(e) => { eprintln!("Notify error: {}", e); break; }
            };
        }
    });

    let mut stdout = match File::open(dirpath.join("out")).await {
        Ok(fh) => fh,
        Err(e) => {
            eprintln!("Failed to open {}/out: {}", dirpath.display(), e);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let mut stderr = match File::open(dirpath.join("err")).await {
        Ok(fh) => fh,
        Err(e) => {
            eprintln!("Failed to open {}/err: {}", dirpath.display(), e);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    Ok(Sse::new(try_stream! {
        let mut buf = [0; 1024];
        yield Event::default();
        while let Ok(n) = stdout.read(&mut buf).await {
            if n == 0 { break; }
            yield Event::default().json_data(RunOutput { field: "stdout", value: String::from_utf8_lossy(&buf[0..n]).to_string() }).unwrap();
        }
        while let Ok(n) = stderr.read(&mut buf).await {
            if n == 0 { break; }
            yield Event::default().json_data(RunOutput { field: "stderr", value: String::from_utf8_lossy(&buf[0..n]).to_string() }).unwrap();
        }
        while let Some(event) = nrx.recv().await {
            if let EventKind::Modify(ModifyKind::Data(_)) = event.kind {
                if event.paths[0].ends_with("out") {
                    while let Ok(n) = stdout.read(&mut buf).await {
                        if n == 0 { break; }
                        yield Event::default().json_data(RunOutput { field: "stdout", value: String::from_utf8_lossy(&buf[0..n]).to_string() }).unwrap();
                    }
                }
                else if event.paths[0].ends_with("err") {
                    while let Ok(n) = stderr.read(&mut buf).await {
                        if n == 0 { break; }
                        yield Event::default().json_data(RunOutput { field: "stderr", value: String::from_utf8_lossy(&buf[0..n]).to_string() }).unwrap();
                    }
                }
            }
            else if let EventKind::Access(AccessKind::Close(AccessMode::Write)) = event.kind && event.paths[0].ends_with("status") {
                match read_statusfile(&event.paths[0]).await.map(ExitStatus::from_raw) {
                    Some(status) => {
                        let value = match status.success() { true => "finished successfully".to_string(), false => format!("failed with {}", status) };
                        yield Event::default().json_data(RunOutput { field: "status", value }).unwrap()
                    },
                    None => yield Event::default().json_data(RunOutput { field: "status", value: "Failed to read exit status".to_string() }).unwrap()
                }
                return;
            }
        }
    }).keep_alive(KeepAlive::default()))
}
