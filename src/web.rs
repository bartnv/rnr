use std::{ fs, io::BufRead as _, path, process::ExitStatus, sync };
use futures_util::{SinkExt as _, StreamExt};
use hyper::body;
use hyper_util::rt::TokioIo;
use serde::Serialize;
use tokio::{ net, sync::broadcast };

use crate::{ Config, Job, JsonJob, Schedule };

#[derive(Clone)]
pub struct TokioExecutor;
impl<F> hyper::rt::Executor<F> for TokioExecutor
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    fn execute(&self, fut: F) {
        tokio::task::spawn(fut);
    }
}

pub async fn run(config: sync::Arc<sync::RwLock<Config>>, listener: net::TcpListener, broadcast: broadcast::Sender<Job>) {
    let http = hyper::server::conn::http1::Builder::new();
    let service = hyper::service::service_fn(move |req| {
        println!("Received HTTP request {} {}", req.method(), req.uri());
        handle_http(req, config.clone(), broadcast.subscribe())
    });
    while let Ok((stream, addr)) = listener.accept().await {
        println!("Incoming HTTP connection from {}", addr);
        let http = http.clone();
        let service = service.clone();
        tokio::spawn(async move {
            if let Err(e) = http.serve_connection(TokioIo::new(stream), service.clone()).with_upgrades().await {
                println!("HTTP error: {}", e);
            }
        });
    }
}

pub async fn handle_http(mut request: hyper::Request<body::Incoming>, config: sync::Arc<sync::RwLock<Config>>, broadcast: broadcast::Receiver<Job>) -> Result<hyper::Response<http_body_util::Full<body::Bytes>>, Box<dyn std::error::Error + Send + Sync + 'static>> {
    if hyper_tungstenite::is_upgrade_request(&request) {
        let (response, websocket) = hyper_tungstenite::upgrade(&mut request, None)?;
        tokio::spawn(async move {
            let _ = handle_websocket(websocket, config, broadcast).await;
        });
        Ok(response)
    } else {
        match request.uri().path() {
            "/" => {
                Ok(hyper::Response::new(http_body_util::Full::<body::Bytes>::from(fs::read("web/index.html")?)))
            },
            file if [ "/favicon.ico" ].contains(&file) => {
                Ok(hyper::Response::new(http_body_util::Full::<body::Bytes>::from(fs::read(String::from("web") + file)?)))
            },
            _ => Ok(hyper::Response::builder().status(hyper::StatusCode::NOT_FOUND).body(http_body_util::Full::<body::Bytes>::from("")).unwrap())
        }
    }
}

pub async fn handle_websocket(ws: hyper_tungstenite::HyperWebsocket, config: sync::Arc<sync::RwLock<Config>>, mut broadcast: broadcast::Receiver<Job>) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    #[derive(Default, Serialize)]
    struct JsonGraph {
        msg: &'static str,
        jobs: Vec<JsonJob>
    }

    let (mut ws_tx, mut ws_rx) = ws.await?.split();

    let mut res = JsonGraph { msg: "init", ..Default::default() };
    {
        let rconfig = config.read().unwrap();

        for job in &rconfig.jobs {
            res.jobs.push(job.to_json());
        }

    }
    ws_tx.send(hyper_tungstenite::tungstenite::Message::Text(serde_json::to_string(&res).unwrap())).await?;

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
                                                let res = JsonGraph { msg: "pong", ..Default::default() };
                                                ws_tx.send(hyper_tungstenite::tungstenite::Message::Text(serde_json::to_string(&res).unwrap())).await?;
                                            },
                                            text => println!("Received websocket message: {}", text)
                                        }
                                    },
                                    other => println!("Received unexpected websocket message: {:?}", other)
                                }
                            },
                            Err(e) => println!("Received websocket error: {}", e)
                        }
                    },
                    None => break // Websocket closed
                }
            }
            msg = broadcast.recv() => {
                match msg {
                    Ok(job) => {
                        let mut res = JsonGraph { msg: "update", ..Default::default() };
                        res.jobs.push(job.to_json());
                        ws_tx.send(hyper_tungstenite::tungstenite::Message::Text(serde_json::to_string(&res).unwrap())).await?;
                    },
                    Err(e) => eprintln!("Broadcast channel error: {}", e)
                }
            }
        }
    }

    Ok(())
}
