#![allow(dead_code, unused_imports, unused_variables, unused_mut, unreachable_patterns)] // Please be quiet, I'm coding
use std::{ collections::HashMap, env, error, io::BufRead as _, path, process, str::FromStr as _, sync::{ Arc, RwLock }, time };
use git_version::git_version;
use serde::Serialize;
use tokio::{fs, io::AsyncReadExt as _, net, sync::{ broadcast, mpsc } };
use yaml_rust2::YamlLoader;

mod control;
mod runner;
mod web;

const VERSION: &str = git_version!();

struct Config {
    jobs: Vec<Job>,
    env: HashMap<String, String>
}

#[derive(Clone, Debug, Default)]
enum Schedule {
    #[default]
    None,
    Continuous,
    After(path::PathBuf),
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
    laststatus: Option<i32>,
    lastrun: Option<chrono::DateTime<chrono::Local>>,
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
    laststart: Option<chrono::DateTime<chrono::Local>>,
    lastrun: Option<Run>
}
impl Job {
    fn clone_empty(&self) -> Job {
        Job {
            name: self.name.clone(),
            path: self.path.clone(),
            schedule: Schedule::None,
            command: self.command.clone(),
            error: None,
            laststart: None,
            lastrun: None
        }
    }
    fn from_yaml(path: path::PathBuf, yaml: String) -> Option<Job> {
        let docs = match YamlLoader::load_from_str(&yaml) {
            Ok(config) => config,
            Err(err) => { return Some(Job::from_error(None, path, format!("Invalid YAML in jobfile: {}", err))); }
        };
        let config = &docs[0];
        let name = config["name"].as_str().unwrap_or(&path.display().to_string()).trim_start_matches("./").to_string();
        let schedule = match config["schedule"].as_str() {
            Some(sched) => {
                let mut tokens = sched.split_ascii_whitespace();
                match tokens.next().unwrap() {
                    "@continuous" => Schedule::Continuous,
                    "@after" => match tokens.next() {
                        Some(path) => Schedule::After(path::PathBuf::from(path)),
                        None => { return Some(Job::from_error(Some(name), path, format!("Invalid schedule expression: {}", sched))); }
                    },
                    _ => match cron::Schedule::from_str(sched) {
                        Ok(sched) => Schedule::Schedule(sched),
                        Err(err) => { return Some(Job::from_error(Some(name), path, format!("Failed to parse schedule expression: {}", err))); }
                    }
                }
            },
            None => Schedule::None
        };
        let command = match config["command"].as_str() {
            Some(str) => str,
            None => { return Some(Job::from_error(Some(name), path, format!("No command found in jobfile"))); }
        };
        let command = match shell_words::split(command) {
            Ok(args) => args,
            Err(e) => { return Some(Job::from_error(Some(name), path, format!("Invalid command found in jobfile: {}", e))); }
        };
        Some(Job {
            name,
            path,
            schedule,
            command,
            ..Default::default()
        })
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
                Schedule::After(after) => Some(after.display().to_string()),
                _ => None
            },
            error: self.error.clone(),
            laststatus: match &self.lastrun {
                Some(run) => Some(run.output.status.code().unwrap()),
                None => None
            },
            lastrun: self.laststart.clone(),
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
    let config = Arc::new(RwLock::new(Config { jobs: vec![], env: HashMap::new() }));
    process_config(&config).await;
    {
        let mut dir = match fs::read_dir(".").await {
            Ok(dir) => dir,
            Err(e) => { eprintln!("Failed to read current working directory: {}", e.to_string()); return process::ExitCode::FAILURE; }
        };
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
                    println!("Directory \"{}\" has no job.yml file", path.display());
                    continue;
                }
            };
            if let Some(job) = job {
                if let Some(ref e) = job.error { eprintln!("Job \"{}\" permanent error: {}", job.name, e); }
                else { println!("Found job \"{}\" at path {} to run: {}", job.name, job.path.display(), job.command.join(" ")); }
                if let Schedule::Schedule(ref sched) = job.schedule { println!("Next execution: {}", sched.upcoming(chrono::Local).next().unwrap()); }
                if let Schedule::After(ref path) = job.schedule { println!("Execution after job with path {}", path.display()); }
                wconfig.jobs.push(job);
            }
        }
    }
    let (bctx, bcrx) = broadcast::channel(100);
    let aconfig = config.clone();
    let broadcast = bctx.clone();
    let rnr = tokio::spawn(async move {
        runner::run(aconfig, broadcast).await;
    });

    match net::TcpListener::bind("0.0.0.0:1234").await {
        Ok(listener) => {
            let aconfig = config.clone();
            let web = tokio::spawn(async move {
                web::run(aconfig, listener, bctx).await;
            });
        },
        Err(e) => eprintln!("Failed to bind: {}", e)
    };

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
