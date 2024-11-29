#![allow(dead_code, unused_imports, unused_variables, unused_mut, unreachable_patterns)] // Please be quiet, I'm coding
use std::{ env, error, path, process, str::FromStr as _, sync, time };
use git_version::git_version;
use tokio::{fs, io::AsyncReadExt as _, sync::mpsc };
use yaml_rust2::YamlLoader;

mod control;
mod runner;

const VERSION: &str = git_version!();

struct Config {
    jobs: Vec<Job>
}

#[derive(Debug, Default)]
enum Schedule {
    #[default]
    None,
    Continuous,
    Schedule(cron::Schedule)
}

#[derive(Debug)]
struct Run {
    start: chrono::DateTime<chrono::Local>,
    duration: time::Duration,
    output: process::Output
}

#[derive(Debug, Default)]
struct Job {
    name: String,
    path: path::PathBuf,
    schedule: Schedule,
    command: Vec<String>,
    error: Option<String>,
    laststart: Option<chrono::DateTime<chrono::Local>>,
    lastrun: Option<Run>
}
impl Clone for Job {
    fn clone(&self) -> Job {
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
}
impl Job {
    fn from_yaml(path: path::PathBuf, yaml: String) -> Option<Job> {
        let docs = match YamlLoader::load_from_str(&yaml) {
            Ok(config) => config,
            Err(err) => { return Some(Job::from_error(None, path, format!("Invalid YAML in jobfile: {}", err))); }
        };
        let config = &docs[0];
        let name = config["name"].as_str().unwrap_or(&path.display().to_string()).trim_start_matches("./").to_string();
        let schedule = match config["schedule"].as_str() {
            Some(sched) => match sched {
                "@continuous" => Schedule::Continuous,
                sched => match cron::Schedule::from_str(sched) {
                    Ok(sched) => Schedule::Schedule(sched),
                    Err(err) => { return Some(Job::from_error(Some(name), path, format!("Failed to parse schedule expression: {}", err))); }
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
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn error::Error>> {
    let cwd = env::current_dir()?;
    println!("Starting rnr {} in {}", VERSION, cwd.display());
    let config = sync::Arc::new(sync::RwLock::new(Config { jobs: vec![] }));
    {
        let mut dir = fs::read_dir(".").await?;
        let mut wconfig = config.write().unwrap();
        while let Ok(Some(entry)) = dir.next_entry().await {
            let path = entry.path();
            if !path.is_dir() { continue; }
            let job = match fs::File::open(&path.join("job.yml")).await {
                Ok(mut file) => {
                    let mut contents = vec![];
                    file.read_to_end(&mut contents).await?;
                    Job::from_yaml(path.clone(), String::from_utf8_lossy(&contents).into_owned())
                }
                Err(_) => {
                    println!("Directory {} has no job.yml file", path.display());
                    continue;
                }
            };
            if let Some(job) = job {
                if let Some(e) = job.error {
                    eprintln!("Job {} permanent error: {}", job.name, e);
                    continue;
                }
                println!("Found job \"{}\" to run: {}", job.name, job.command[0]);
                if let Schedule::Schedule(ref sched) = job.schedule { println!("Next execution: {}", sched.upcoming(chrono::Local).next().unwrap()); }
                wconfig.jobs.push(job);
            }
        }
    }
    let (tx, rx) = mpsc::channel(100);
    let aconfig = config.clone();
    let ctrl = tokio::spawn(async move {
        control::run(aconfig.clone(), rx).await;
    });
    let rnr = tokio::spawn(async move {
        runner::run(config.clone(), tx).await;
    });
    tokio::join!(ctrl).0.unwrap();
    tokio::join!(rnr).0.unwrap();
    println!("Exiting");
    Ok(())
}
