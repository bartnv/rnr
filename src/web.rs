use std::{ convert::Infallible, sync::{ Arc, RwLock } };
use rocket::{ fs::NamedFile, response::stream::{ EventStream, Event }, serde::{ json::Json, Serialize }, State };
use tokio::{ net::TcpListener, sync::broadcast };

use crate::{ Config, Job, JsonJob, Schedule };

#[derive(Default, Serialize)]
struct JsonDetails {
    log: String,
    err: String
}

pub async fn run(config: Arc<RwLock<Config>>, broadcast: broadcast::Sender<Job>) -> Result<rocket::Rocket<rocket::Ignite>, rocket::Error> {
    let mut rktconfig = rocket::Config::default();
    rktconfig.address = "0.0.0.0".parse().unwrap();
    rktconfig.port = 1234;
    let srv = rocket::custom(rktconfig)
        .manage(config)
        .manage(broadcast)
        .mount("/", routes![index, jobs, job, updates]);
    srv.launch().await
}

#[get("/")]
async fn index() -> Option<NamedFile> {
    NamedFile::open("rnr/web/index-rocket.html").await.ok()
}

#[get("/jobs")]
async fn jobs(config: &State<Arc<RwLock<Config>>>) -> Json<Vec<JsonJob>> {
    Json(config.read().unwrap().jobs.values().map(|j| j.to_json()).collect())
}

#[get("/jobs/<path>")]
async fn job(config: &State<Arc<RwLock<Config>>>, path: &str) -> Option<Json<JsonDetails>> {
    if let Some(job) = config.read().unwrap().jobs.get(path) {
        if let Some(lastrun) = &job.lastrun {
            return Some(Json(JsonDetails {
                log: String::from_utf8_lossy(&lastrun.output.stdout).into_owned(),
                err: String::from_utf8_lossy(&lastrun.output.stderr).into_owned()
            }));
        }
    }
    None
}

#[get("/updates")]
async fn updates(broadcast: &State<broadcast::Sender<Job>>) -> EventStream![] {
    let mut rx = broadcast.subscribe();
    EventStream! {
        loop {
            match rx.recv().await {
                Ok(job) => yield Event::json(&job.to_json()),
                _ => break
            }
        }
    }
}
