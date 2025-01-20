use std::{ fs, io::BufRead as _, path, process::ExitStatus, sync::{ Arc, RwLock } };
use futures_util::{SinkExt as _, StreamExt};
use hyper::body;
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use tokio::{ io::AsyncReadExt, net::TcpListener, sync::broadcast };

use crate::{ Config, Job, JsonJob, Schedule };

pub async fn run(config: Arc<RwLock<Config>>, listener: TcpListener, broadcast: broadcast::Sender<Job>) {
    let http = hyper::server::conn::http1::Builder::new();
    let service = hyper::service::service_fn(move |req| {
        // println!("Received HTTP request {} {}", req.method(), req.uri());
        handle_http(req, config.clone(), broadcast.subscribe())
    });
    while let Ok((stream, addr)) = listener.accept().await {
        // println!("Incoming HTTP connection from {}", addr);
        let http = http.clone();
        let service = service.clone();
        tokio::spawn(async move {
            if let Err(e) = http.serve_connection(TokioIo::new(stream), service.clone()).with_upgrades().await {
                // println!("HTTP error: {}", e);
            }
        });
    }
}

pub async fn handle_http(mut request: hyper::Request<body::Incoming>, config: Arc<RwLock<Config>>, broadcast: broadcast::Receiver<Job>) -> Result<hyper::Response<http_body_util::Full<body::Bytes>>, Box<dyn std::error::Error + Send + Sync + 'static>> {
    if hyper_tungstenite::is_upgrade_request(&request) {
        match request.headers().get("Sec-WebSocket-Protocol") {
            Some(auth) => match auth.to_str() {
                Ok(auth) => match auth.split(", ").nth(1) {
                    Some(pass) => if pass != "test123" {
                        return http_error(hyper::StatusCode::FORBIDDEN, "Password invalid")
                    },
                    None => return http_error(hyper::StatusCode::FORBIDDEN, "Sec-WebSocket-Protocol header is invalid")
                },
                Err(_) => return http_error(hyper::StatusCode::FORBIDDEN, "Sec-WebSocket-Protocol header is invalid")
            },
            None => return http_error(hyper::StatusCode::FORBIDDEN, "No Sec-WebSocket-Protocol header found")
        }
        let (response, websocket) = hyper_tungstenite::upgrade(&mut request, None)?;
        tokio::spawn(async move {
            let _ = handle_websocket(websocket, config, broadcast).await;
        });
        Ok(response)
    } else {
        match request.uri().path() {
            "/" => {
                Ok(hyper::Response::new(http_body_util::Full::<body::Bytes>::from(fs::read("rnr/web/index.html")?)))
            },
            file if [ "/favicon.ico" ].contains(&file) => {
                Ok(hyper::Response::new(http_body_util::Full::<body::Bytes>::from(fs::read(String::from("web") + file)?)))
            },
            _ => http_error(hyper::StatusCode::NOT_FOUND, "")
        }
    }
}

fn http_error(code: hyper::StatusCode, message: &'static str) -> Result<hyper::Response<http_body_util::Full<body::Bytes>>, Box<dyn std::error::Error + Send + Sync + 'static>> {
    Ok(hyper::Response::builder().status(code).body(http_body_util::Full::<body::Bytes>::from(message))?)
}

pub async fn handle_websocket(ws: hyper_tungstenite::HyperWebsocket, config: Arc<RwLock<Config>>, mut broadcast: broadcast::Receiver<Job>) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    #[derive(Default, Serialize)]
    struct JsonMsg {
        msg: &'static str,
        jobs: Vec<JsonJob>,
        details: Option<JsonDetails>
    }
    #[derive(Debug, Deserialize)]
    struct JsonReq {
        req: String,
        path: String
    }
    #[derive(Default, Serialize)]
    struct JsonDetails {
        path: String,
        log: String,
        err: String
    }
    #[derive(Default, Serialize)]
    struct JsonHistory {
        msg: &'static str,
        path: String,
        runs: Vec<JsonRun>
    }
    #[derive(Default, Serialize)]
    struct JsonRun {
        ts: String,
        status: u8,
        log: String,
        err: String
    }

    let (mut ws_tx, mut ws_rx) = ws.await?.split();

    let mut res = JsonMsg { msg: "init", ..Default::default() };
    {
        let rconfig = config.read().unwrap();
        for job in rconfig.jobs.values() {
            res.jobs.push(job.to_json());
        }
    }
    ws_tx.send(hyper_tungstenite::tungstenite::Message::Text(serde_json::to_string(&res)?.into())).await?;

    loop {
        tokio::select!{
            res = ws_rx.next() => {
                match res {
                    Some(message) => {
                        match message {
                            Ok(message) => {
                                match message {
                                    hyper_tungstenite::tungstenite::Message::Text(text) => {
                                        match text.as_str() {
                                            "ping" => {
                                                let res = JsonMsg { msg: "pong", ..Default::default() };
                                                ws_tx.send(hyper_tungstenite::tungstenite::Message::Text(serde_json::to_string(&res)?.into())).await?;
                                            },
                                            text => {
                                                let req: JsonReq = serde_json::from_str(text)?; // Returns from handler (closing the ws) on invalid JsonReq
                                                match req.req.as_str() {
                                                    "getlog" => {
                                                        let mut res = None;
                                                        {
                                                            let rconfig = config.read().unwrap();
                                                            if let Some(job) = rconfig.jobs.get(&req.path) {
                                                                if let Some(lastrun) = &job.lastrun {
                                                                    res = Some(JsonDetails {
                                                                        path: req.path,
                                                                        log: String::from_utf8_lossy(&lastrun.output.stdout).into_owned(),
                                                                        err: String::from_utf8_lossy(&lastrun.output.stderr).into_owned()
                                                                    });
                                                                }
                                                            }
                                                        }
                                                        if let Some(details) = res {
                                                            let res = JsonMsg { msg: "details", jobs: vec![], details: Some(details) };
                                                            ws_tx.send(hyper_tungstenite::tungstenite::Message::Text(serde_json::to_string(&res)?.into())).await?
                                                        }
                                                    },
                                                    "gethist" => {
                                                        let mut res = JsonHistory { msg: "hist", path: req.path.clone(), runs: vec![] };
                                                        let dirpath = match config.read() {
                                                            Ok(config) => config.dir.join(req.path).join("runs"),
                                                            Err(_) => continue
                                                        };
                                                        if !dirpath.is_dir() { continue; }
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
                                                                    res.runs.push(run);
                                                                }
                                                            },
                                                            Err(e) => {
                                                                eprintln!("Error while opening runs dir {}: {}", dirpath.display(), e);
                                                                continue;
                                                            }
                                                        }
                                                        ws_tx.send(hyper_tungstenite::tungstenite::Message::Text(serde_json::to_string(&res)?.into())).await?
                                                    },
                                                    _ => {
                                                        println!("Received unexpected websocket message: {}", text);
                                                        continue;
                                                    }
                                                }
                                            }
                                        }
                                    },
                                    hyper_tungstenite::tungstenite::Message::Close(_) => {},
                                    other => println!("Received unexpected websocket message: {:?}", other)
                                }
                            },
                            Err(_) => {} // println!("Received websocket error: {}", e)
                        }
                    },
                    None => break // Websocket closed
                }
            }
            msg = broadcast.recv() => {
                match msg {
                    Ok(job) => {
                        let mut res = JsonMsg { msg: "update", jobs: vec![], ..Default::default() };
                        res.jobs.push(job.to_json());
                        while let Ok(job) = broadcast.try_recv() { // Check if additional results are waiting
                            res.jobs.push(job.to_json());
                        }
                        ws_tx.send(hyper_tungstenite::tungstenite::Message::Text(serde_json::to_string(&res)?.into())).await?;
                    },
                    Err(e) => eprintln!("Broadcast channel error: {}", e)
                }
            }
        }
    }

    Ok(())
}
