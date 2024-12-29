#![allow(dead_code, unused_imports, unused_variables, unused_mut, unreachable_patterns)] // Please be quiet, I'm coding
use std::{ collections::HashMap, env, error, ffi::OsStr, io::BufRead as _, os::unix::ffi::OsStrExt, path::{self, Path, PathBuf}, process, str::FromStr as _, sync::{ Arc, RwLock }, time::{self, Duration} };
use futures_util::StreamExt;
use git_version::git_version;
use notify::{ event::{ AccessKind, AccessMode, CreateKind, ModifyKind, RemoveKind }, EventKind, Watcher };
use serde::Serialize;
use tokio::{fs, io::AsyncReadExt as _, net, sync::{ broadcast, mpsc } };
use yaml_rust2::YamlLoader;

mod control;
mod runner;
mod web;

const VERSION: &str = git_version!();

struct Config {
    jobs: HashMap<String, Job>,
    env: HashMap<String, String>
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

#[derive(Serialize)]
struct JsonJob {
    path: String,
    name: String,
    nextrun: Option<chrono::DateTime<chrono::Local>>,
    after: Option<String>,
    command: String,
    error: Option<String>,
    running: bool,
    laststatus: Option<i32>,
    lastrun: Option<chrono::DateTime<chrono::Local>>,
    lastdur: u64,
    lastlog: usize,
    lasterr: usize
}

#[derive(Clone, Debug, Default)]
struct Job {
    name: String,
    path: path::PathBuf,
    schedule: Schedule,
    command: Vec<String>,
    error: Option<String>,
    running: bool,
    laststart: Option<chrono::DateTime<chrono::Local>>,
    lastrun: Option<Run>,
    update: bool
}
impl Job {
    fn clone_empty(&self) -> Job {
        Job {
            name: self.name.clone(),
            path: self.path.clone(),
            schedule: self.schedule.clone(),
            command: self.command.clone(),
            error: None,
            running: self.running,
            laststart: None,
            lastrun: None,
            update: false.into()
        }
    }
    fn from_yaml(path: path::PathBuf, yaml: String) -> Job {
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
                            while let Some(path) = tokens.next() {
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
            None => { return Job::from_error(Some(name), path, format!("No command found in jobfile")); }
        };
        let command = match shell_words::split(command) {
            Ok(args) => args,
            Err(e) => { return Job::from_error(Some(name), path, format!("Invalid command found in jobfile: {}", e)); }
        };
        Job {
            name,
            path,
            schedule,
            command,
            ..Default::default()
        }
    }
    fn from_error(name: Option<String>, path: path::PathBuf, error: String) -> Job {
        let name = name.unwrap_or(path.display().to_string().trim_start_matches("./").to_string());
        Job { name, path, error: Some(error), ..Default::default() }
    }
    fn to_json(&self) -> JsonJob {
        JsonJob {
            path: self.path.display().to_string(),
            name: self.name.clone(),
            command: self.command.join(" "),
            nextrun: match &self.schedule {
                Schedule::Schedule(sched) => Some(sched.upcoming(chrono::Local).next().unwrap()),
                _ => None
            },
            after: match &self.schedule {
                Schedule::After(after) => Some(after.join(" ")),
                _ => None
            },
            error: self.error.clone(),
            running: self.running,
            laststatus: match &self.lastrun {
                Some(run) => Some(run.output.status.code().unwrap()),
                None => None
            },
            lastrun: self.laststart.clone(),
            lastdur: match &self.lastrun {
                Some(run) => run.duration.as_secs(),
                None => 0
            },
            lastlog: match &self.lastrun {
                Some(run) => run.output.stdout.lines().count(),
                None => 0
            },
            lasterr: match &self.lastrun {
                Some(run) => run.output.stderr.lines().count(),
                None => 0
            }
        }
    }
}

#[tokio::main]
async fn main() -> process::ExitCode {
    let cwd = match env::current_dir() {
        Ok(dir) => dir,
        Err(e) => { eprintln!("Failed to find current working directory"); return process::ExitCode::FAILURE; }
    };
    println!("Starting rnr {} in {}", VERSION, cwd.display());
    let config = Arc::new(RwLock::new(Config { jobs: HashMap::new(), env: HashMap::new() }));
    process_config(&config).await;
    let mut dir = match fs::read_dir(".").await {
        Ok(dir) => dir,
        Err(e) => { eprintln!("Failed to read current working directory: {}", e.to_string()); return process::ExitCode::FAILURE; }
    };
    read_jobs(&config, dir).await;
    check_jobs(&config);
    let (bctx, bcrx) = broadcast::channel(100);
    let aconfig = config.clone();
    let broadcast = bctx.clone();
    let rnr = tokio::spawn(async move {
        runner::run(aconfig, broadcast).await;
    });

    match net::TcpListener::bind("0.0.0.0:1234").await {
        Ok(listener) => {
            let aconfig = config.clone();
            let broadcast = bctx.clone();
            let web = tokio::spawn(async move {
                web::run(aconfig, listener, broadcast).await;
            });
        },
        Err(e) => eprintln!("Failed to bind: {}", e)
    };

    let (ntx, mut nrx) = mpsc::channel(10);
    let watchdir = cwd.clone();
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
            if event.paths.len() < 1 { continue; }
            if let Some(filename) = event.paths[0].file_name() {
                if filename != OsStr::from_bytes(b"job.yml") { continue; }
            }
            else { continue; }
            let dirpath = event.paths[0].parent().unwrap().strip_prefix(&cwd).unwrap().to_owned();
            update_job(&config, dirpath, bctx.clone());
        }
        else if let EventKind::Create(CreateKind::Folder) = event.kind {
            if event.paths.len() < 1 { continue; }
            if let Some(parent) = event.paths[0].parent() {
                if parent == cwd {
                    let path = event.paths[0].strip_prefix(&cwd).unwrap().to_owned();
                    println!("Subdirectory {} added", path.display());
                    if path.join("job.yml").is_file() {
                        update_job(&config, path, bctx.clone());
                    }
                }
            }
        }
        else if let EventKind::Remove(RemoveKind::File) = event.kind {
            if event.paths.len() < 1 { continue; }
            if let Some(filename) = event.paths[0].file_name() {
                if filename != OsStr::from_bytes(b"job.yml") { continue; }
            }
            else { continue; }
            let dirname = event.paths[0].parent().unwrap().strip_prefix(&cwd).unwrap().display().to_string();
            println!("Jobfile with path {} removed", dirname);
            if let Ok(mut config) = config.write() {
                if config.jobs.remove(&dirname).is_some() {
                    println!("{} Job {} deleted", chrono::Local::now(), dirname);
                }
            }
        }
    }

    tokio::join!(rnr).0.unwrap();
    process::ExitCode::SUCCESS
}

async fn process_config(mut config: &Arc<RwLock<Config>>) {
    let file = fs::File::open("rnr.yml").await;
    if let Err(e) = file {
        eprintln!("Failed to read rnr.yml configuration file");
        return;
    }
    let mut contents = vec![];
    if let Err(e) = file.unwrap().read_to_end(&mut contents).await {
        eprintln!("Failed to read rnr.yml configuration file");
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
    if let Some(env) = yaml["env"].as_hash() {
        let mut wconfig = config.write().unwrap();
        for (key, value) in env  {
            wconfig.env.insert(key.as_str().unwrap().to_owned(), value.as_str().unwrap().to_owned());
        }
    }
}

async fn read_jobs(mut config: &Arc<RwLock<Config>>, mut dir: tokio::fs::ReadDir) {
    let mut wconfig = config.write().unwrap();
    while let Ok(Some(entry)) = dir.next_entry().await {
        let path = path::PathBuf::from(entry.file_name()); // Get the relative path
        if !path.is_dir() { continue; }
        let job = match fs::File::open(&path.join("job.yml")).await {
            Ok(mut file) => {
                let mut contents = vec![];
                if let Err(e) = file.read_to_end(&mut contents).await {
                    println!("Failed to read job.yml in directory \"{}\": {}", path.display(), e.to_string());
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
        if let Schedule::Schedule(ref sched) = job.schedule { println!("Next execution: {}", sched.upcoming(chrono::Local).next().unwrap()); }
        if let Schedule::After(ref after) = job.schedule { println!("Execution after job(s): {}", after.join(" ")); }
        wconfig.jobs.insert(job.path.display().to_string(), job);
    }
}

fn check_jobs(mut config: &Arc<RwLock<Config>>) {
    let mut wconfig = config.write().unwrap();
    let paths: Vec<String> = wconfig.jobs.keys().map(|v| v.clone()).collect();
    for job in wconfig.jobs.values_mut() {
        if let Schedule::After(after) = &job.schedule {
            for path in after {
                if !paths.contains(path) {
                    eprintln!("Job {} scheduled after job {} which doesn't exist", job.path.display(), path);
                    job.error = Some(format!("Scheduled after nonexistent job {}", path));
                }
            }
        }
    }
}

fn update_job(mut config: &Arc<RwLock<Config>>, dirpath: PathBuf, bctx: broadcast::Sender<Job>) {
    let dirname = dirpath.display().to_string();
    println!("Jobfile with path {} written", dirname);
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
        let mut newjob = match fs::File::open(dirpath.join("job.yml")).await {
            Ok(mut file) => {
                let mut contents = vec![];
                if let Err(e) = file.read_to_end(&mut contents).await {
                    println!("Failed to read job.yml in directory \"{}\": {}", dirname, e.to_string());
                    return;
                }
                Job::from_yaml(dirpath.clone(), String::from_utf8_lossy(&contents).into_owned())
            }
            Err(_) => {
                println!("Unable to open job.yml file in {}", dirname);
                return;
            }
        };
        let mut wconfig = config.write().unwrap();
        let oldjob = wconfig.jobs.remove(&dirname).unwrap();
        if !oldjob.name.is_empty() { // New jobs have empty name
            if !newjob.error.is_some() { newjob.error = oldjob.error; }
            newjob.running = oldjob.running;
            newjob.laststart = oldjob.laststart;
            newjob.lastrun = oldjob.lastrun;
            println!("{} Job {} reloaded", chrono::Local::now(), dirname);
        }
        else { println!("{} Job {} added", chrono::Local::now(), dirname); }
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
