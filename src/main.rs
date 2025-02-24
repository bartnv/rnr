#![allow(dead_code, unused_imports, unused_variables, unused_mut, unreachable_patterns)] // Please be quiet, I'm coding
use std::{ collections::HashMap, env, ffi::OsStr, io::BufRead as _, net::SocketAddr, os::unix::ffi::OsStrExt, path::PathBuf, process, str::FromStr as _, sync::{ Arc, RwLock }, time::{self, Duration} };
use git_version::git_version;
use notify::{ event::{ AccessKind, AccessMode, CreateKind, RemoveKind }, EventKind, Watcher };
use serde::Serialize;
use tokio::{fs, io::AsyncReadExt as _, sync::{ broadcast, mpsc } };
use yaml_rust2::YamlLoader;

mod control;
mod runner;
mod web;

const VERSION: &str = git_version!();

struct Config {
    dir: PathBuf,
    jobs: HashMap<String, Job>,
    env: HashMap<String, String>,
    http: Option<SocketAddr>
}

#[derive(Clone, Debug, Default)]
enum Schedule {
    #[default]
    None,
    Continuous,
    After(Vec<String>),
    Schedule(cron::Schedule)
}

#[derive(Clone, Debug)]
struct Run {
    start: chrono::DateTime<chrono::Local>,
    duration: time::Duration,
    output: process::Output
}
#[derive(Default, Serialize)]
struct JsonRun {
    start: String,
    duration: u64,
    status: u8,
    log: String,
    err: String
}
impl Run {
    fn to_json(&self) -> JsonRun {
        JsonRun {
            start: self.start.to_rfc3339(),
            duration: self.duration.as_secs(),
            status: self.output.status.code().unwrap() as u8,
            log: String::from_utf8_lossy(&self.output.stdout).to_string(),
            err: String::from_utf8_lossy(&self.output.stderr).to_string()
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Job {
    name: String,
    path: PathBuf,
    command: Vec<String>,
    workdir: Option<PathBuf>,
    schedule: Schedule,
    error: Option<String>,
    running: bool,
    laststart: Option<chrono::DateTime<chrono::Local>>,
    lastrun: Option<Run>,
    skipped: usize,
    history: bool,
    update: bool
}
#[derive(Serialize)]
struct JsonJob {
    name: String,
    path: String,
    command: String,
    nextrun: Option<chrono::DateTime<chrono::Local>>,
    after: Option<String>,
    error: Option<String>,
    running: bool,
    laststart: Option<chrono::DateTime<chrono::Local>>,
    lastrun: Option<JsonRun>,
    skipped: usize,
    history: bool
}
impl Job {
    fn clone_empty(&self) -> Job {
        Job {
            name: self.name.clone(),
            path: self.path.clone(),
            command: self.command.clone(),
            workdir: self.workdir.clone(),
            schedule: self.schedule.clone(),
            error: None,
            running: self.running,
            laststart: None,
            lastrun: None,
            skipped: 0,
            history: self.history,
            update: false
        }
    }
    fn from_yaml(path: PathBuf, yaml: String) -> Job {
        let docs = match YamlLoader::load_from_str(&yaml) {
            Ok(config) => config,
            Err(err) => { return Job::from_error(None, path, format!("Invalid YAML in jobfile: {}", err)); }
        };
        let config = &docs[0];
        let name = config["name"].as_str().unwrap_or(&path.display().to_string()).trim_start_matches("./").to_string();
        let schedule = match config["schedule"].as_str() {
            Some(sched) => {
                let mut tokens = sched.split_ascii_whitespace();
                match tokens.next().unwrap() {
                    "continuous" => Schedule::Continuous,
                    "after" => match tokens.next() {
                        Some(path) => {
                            let mut vec = Vec::from([ path.to_string() ]);
                            for path in tokens {
                                vec.push(path.to_string());
                            }
                            Schedule::After(vec)
                        },
                        None => { return Job::from_error(Some(name), path, format!("Invalid schedule expression: {}", sched)); }
                    },
                    _ => match cron::Schedule::from_str(sched) {
                        Ok(sched) => Schedule::Schedule(sched),
                        Err(err) => { return Job::from_error(Some(name), path, format!("Failed to parse schedule: {}", err)); }
                    }
                }
            },
            None => Schedule::None
        };
        let command = match config["command"].as_str() {
            Some(str) => str,
            None => { return Job::from_error(Some(name), path, "No command found in jobfile".to_string()); }
        };
        let command = match shell_words::split(command) {
            Ok(args) => args,
            Err(e) => { return Job::from_error(Some(name), path, format!("Invalid command found in jobfile: {}", e)); }
        };
        let workdir = config["workdir"].as_str().map(PathBuf::from);
        Job {
            name,
            path,
            command,
            workdir,
            schedule,
            ..Default::default()
        }
    }
    fn from_error(name: Option<String>, path: PathBuf, error: String) -> Job {
        let name = name.unwrap_or(path.display().to_string().trim_start_matches("./").to_string());
        Job { name, path, error: Some(error), ..Default::default() }
    }
    fn to_json(&self) -> JsonJob {
        JsonJob {
            path: self.path.display().to_string(),
            name: self.name.clone(),
            command: self.command.join(" "),
            nextrun: match &self.schedule {
                Schedule::Schedule(sched) => sched.upcoming(chrono::Local).next(),
                _ => None
            },
            after: match &self.schedule {
                Schedule::After(after) => Some(after.join(" ")),
                _ => None
            },
            error: self.error.clone(),
            running: self.running,
            laststart: self.laststart,
            lastrun: self.lastrun.as_ref().map(|r| r.to_json()),
            skipped: self.skipped,
            history: self.history
        }
    }
}


#[tokio::main]
async fn main() -> process::ExitCode {
    let rnrdir = match env::current_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("Failed to find current working directory: {}", e);
            return process::ExitCode::FAILURE;
        }
    };
    println!("Starting rnr {} in {}", VERSION, rnrdir.display());
    let config = Arc::new(RwLock::new(Config { dir: rnrdir.clone(), jobs: HashMap::new(), env: HashMap::new(), http: None }));
    process_config(&config).await;
    let dir = match fs::read_dir(&rnrdir).await {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("Failed to read runner directory {}: {}", rnrdir.display(), e);
            return process::ExitCode::FAILURE;
        }
    };
    read_jobs(&config, dir).await;
    check_jobs(&config);
    #[allow(deprecated)]
    match env::home_dir() {
        Some(dir) => {
            if let Err(e) = env::set_current_dir(dir) {
                eprintln!("Failed to change to home directory: {}", e);
                return process::ExitCode::FAILURE;
            }
        },
        None => {
            eprintln!("Failed to find home directory");
            return process::ExitCode::FAILURE;
        }
    }

    let (bctx, _) = broadcast::channel(100);
    let aconfig = config.clone();
    let broadcast = bctx.clone();
    tokio::spawn(async move {
        runner::run(aconfig, broadcast).await;
    });

    if config.read().unwrap().http.is_some() {
        let aconfig = config.clone();
        let broadcast = bctx.clone();
        tokio::spawn(async move {
            web::run(aconfig, broadcast).await;
        });
    }

    let (ntx, mut nrx) = mpsc::channel(10);
    let watchdir = rnrdir.clone();
    tokio::task::spawn_blocking(move || {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = notify::RecommendedWatcher::new(tx, notify::Config::default()).unwrap();
        watcher.watch(&watchdir, notify::RecursiveMode::Recursive).unwrap();
        while let Ok(res) = rx.recv() {
            let ntx = ntx.clone();
            tokio::spawn(async move {
                match res {
                    Ok(event) => ntx.send(event).await.unwrap(),
                    Err(e) => eprintln!("Notify error: {}", e)
                }
            });
        }
    });
    while let Some(event) = nrx.recv().await {
        if let EventKind::Access(AccessKind::Close(AccessMode::Write)) = event.kind {
            if event.paths.is_empty() { continue; }
            if let Some(filename) = event.paths[0].file_name() {
                if filename != OsStr::from_bytes(b"job.yml") { continue; }
            }
            else { continue; }
            let dirpath = event.paths[0].parent().unwrap().strip_prefix(&rnrdir).unwrap().to_owned();
            update_job(&config, dirpath, bctx.clone());
        }
        else if let EventKind::Create(CreateKind::Folder) = event.kind {
            if event.paths.is_empty() { continue; }
            if let Some(parent) = event.paths[0].parent() {
                if parent == rnrdir {
                    let path = event.paths[0].strip_prefix(&rnrdir).unwrap().to_owned();
                    println!("Subdirectory {} added", path.display());
                    if path.join("job.yml").is_file() {
                        update_job(&config, path, bctx.clone());
                    }
                }
            }
        }
        else if let EventKind::Remove(RemoveKind::File) = event.kind {
            if event.paths.is_empty() { continue; }
            if let Some(filename) = event.paths[0].file_name() {
                if filename != OsStr::from_bytes(b"job.yml") { continue; }
            }
            else { continue; }
            let dirname = event.paths[0].parent().unwrap().strip_prefix(&rnrdir).unwrap().display().to_string();
            println!("Jobfile with path {} removed", dirname);
            if let Ok(mut config) = config.write() {
                if config.jobs.remove(&dirname).is_some() {
                    println!("{} [{}] deleted", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), dirname);
                }
            }
        }
    }

    // Never reached
    process::ExitCode::SUCCESS
}

async fn process_config(config: &Arc<RwLock<Config>>) {
    let file = fs::File::open("rnr.yml").await;
    if let Err(e) = file {
        eprintln!("Failed to read rnr.yml configuration file: {}", e);
        return;
    }
    let mut contents = vec![];
    if let Err(e) = file.unwrap().read_to_end(&mut contents).await {
        eprintln!("Failed to read rnr.yml configuration file: {}", e);
        return;
    }
    let docs = match YamlLoader::load_from_str(&String::from_utf8_lossy(&contents)) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("Invalid YAML in configuration file: {}", err);
            return;
        }
    };
    let yaml = &docs[0];
    let mut wconfig = config.write().unwrap();
    if let Some(env) = yaml["env"].as_hash() {
        for (key, value) in env  {
            wconfig.env.insert(key.as_str().unwrap().to_owned(), value.as_str().unwrap().to_owned());
        }
    }
    if let Some(addr) = yaml["http"].as_str() {
        wconfig.http = match addr.parse() {
            Ok(sa) => Some(sa),
            Err(e) => {
                eprintln!("Failed to parse HTTP listen adress \"{}\"; HTTP API not enabled", addr);
                None
            }
        };
    }
}

#[allow(clippy::await_holding_lock)] // We're not yet multithreaded at this point, so it doesn't matter
async fn read_jobs(config: &Arc<RwLock<Config>>, mut dir: tokio::fs::ReadDir) {
    let mut wconfig = config.write().unwrap();
    while let Ok(Some(entry)) = dir.next_entry().await {
        let path = PathBuf::from(entry.file_name()); // Get the relative path
        if !path.is_dir() { continue; }
        let mut job = match fs::File::open(&path.join("job.yml")).await {
            Ok(mut file) => {
                let mut contents = vec![];
                if let Err(e) = file.read_to_end(&mut contents).await {
                    println!("Failed to read job.yml in directory \"{}\": {}", path.display(), e);
                    continue;
                }
                Job::from_yaml(path.clone(), String::from_utf8_lossy(&contents).into_owned())
            }
            Err(_) => {
                println!("Directory {} has no job.yml file", path.display());
                continue;
            }
        };
        if let Some(ref e) = job.error { eprintln!("Job \"{}\" permanent error: {}", job.name, e); }
        else { println!("Found job {} ({}) to run: {}", job.path.display(), job.name, job.command.join(" ")); }
        // if let Schedule::Schedule(ref sched) = job.schedule { println!("Next execution: {}", sched.upcoming(chrono::Local).next().map_or("not scheduled".to_string(), |n| n.to_string())); }
        // if let Schedule::After(ref after) = job.schedule { println!("Execution after job(s): {}", after.join(" ")); }
        if wconfig.dir.join(&job.path).join("runs").is_dir() { job.history = true; }
        wconfig.jobs.insert(job.path.display().to_string(), job);
    }
}

fn check_jobs(config: &Arc<RwLock<Config>>) {
    let mut wconfig = config.write().unwrap();
    let paths: Vec<String> = wconfig.jobs.keys().cloned().collect();
    for job in wconfig.jobs.values_mut() {
        if let Schedule::After(after) = &job.schedule {
            for path in after {
                if !paths.contains(path) {
                    eprintln!("Job \"{}\" scheduled after job \"{}\" which doesn't exist", job.path.display(), path);
                    job.error = Some(format!("Scheduled after nonexistent job {}", path));
                }
            }
        }
    }
}

fn update_job(config: &Arc<RwLock<Config>>, path: PathBuf, bctx: broadcast::Sender<Job>) {
    let dirname = path.display().to_string();
    println!("Jobfile with path {} written", dirname);
    let mut dir = config.read().unwrap().dir.clone();
    dir.push(&path);
    if let Ok(mut config) = config.write() {
        let job = config.jobs.get_mut(&dirname);
        match job {
            Some(job) => {
                if job.update { return; } // Update already in progress
                job.update = true;
            },
            None => {
                let job = Job { update: true, ..Default::default() };
                config.jobs.insert(dirname.clone(), job);
            }
        }
    }
    let config = config.clone();
    let broadcast = bctx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::new(1, 0)).await;
        let mut newjob = match fs::File::open(dir.join("job.yml")).await {
            Ok(mut file) => {
                let mut contents = vec![];
                if let Err(e) = file.read_to_end(&mut contents).await {
                    println!("Failed to read job.yml in directory \"{}\": {}", dirname, e);
                    return;
                }
                Job::from_yaml(path.clone(), String::from_utf8_lossy(&contents).into_owned())
            }
            Err(_) => {
                println!("Unable to open job.yml file in {}", dirname);
                return;
            }
        };
        let mut wconfig = config.write().unwrap();
        let oldjob = wconfig.jobs.remove(&dirname).unwrap();
        if !oldjob.name.is_empty() { // New jobs have empty name
            newjob.running = oldjob.running;
            newjob.laststart = oldjob.laststart;
            newjob.lastrun = oldjob.lastrun;
            newjob.history = oldjob.history;
            println!("{} [{}] reloaded", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), dirname);
        }
        else { println!("{} [{}] added", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), dirname); }
        let _ = broadcast.send(newjob.clone());
        wconfig.jobs.insert(dirname, newjob);
    });
}

pub fn duration_from(mut secs: u64) -> String {
    if secs == 0 { return String::from("0s"); }

    let mut result = String::with_capacity(10);
    let delta = [ 31449600, 604800, 86400, 3600, 60, 1 ];
    let unit = [ 'y', 'w', 'd', 'h', 'm', 's' ];
    let mut c = 0;

    loop {
        if secs >= delta[c] { break; }
        c += 1;
    }
    result.push_str(&format!("{}{}", secs/delta[c], unit[c]));
    secs %= delta[c];
    if secs != 0 {
        c += 1;
        result.push_str(&format!(" {}{}", secs/delta[c], unit[c]));
    }
    result
}
