use crate::{
    compiler::{self, write_atomic},
    model::{
        Generation, Job, JobState, MAX_MESSAGE, PROTOCOL_VERSION, PrepareConfig, Request, Response,
        State, StreamConfig, StreamStatus, now,
    },
    raster::bounded_read,
    streaming::{self, Runtime},
};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::Deserialize;
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, PermissionsExt, symlink},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

struct Engine {
    data: PathBuf,
    state: Mutex<State>,
    stopping: AtomicBool,
    stream: Mutex<Option<Runtime>>,
    stream_error: Mutex<Option<String>>,
}

enum Work {
    Prepare(PrepareConfig),
    Verify(Generation),
    Install(Generation, PathBuf),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerationParams {
    generation: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallParams {
    generation: String,
    target: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StreamInstallParams {
    target: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StreamSourceParams {
    tile_source: crate::model::TileSource,
    #[serde(default)]
    size: Option<u32>,
}

fn new_install_target(target: &Path) -> Result<PathBuf> {
    ensure!(
        target.is_absolute(),
        "install target must be an absolute path"
    );
    ensure!(
        fs::symlink_metadata(target).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "install target already exists or cannot be inspected; it will not be replaced"
    );
    let name = target
        .file_name()
        .context("install target needs a package name")?;
    let parent = target
        .parent()
        .context("install target needs a parent")?
        .canonicalize()?;
    ensure!(parent.is_dir(), "install parent is not a directory");
    Ok(parent.join(name))
}

impl Engine {
    fn persist(&self, state: &State) -> Result<()> {
        write_atomic(
            &self.data.join("state.json"),
            &serde_json::to_vec_pretty(state)?,
        )
        .context("persisting engine state")
    }

    fn snapshot(&self) -> Result<State> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("engine state lock poisoned"))?
            .clone();
        let stream = self
            .stream
            .lock()
            .map_err(|_| anyhow::anyhow!("reader runtime lock poisoned"))?;
        let stream_error = self
            .stream_error
            .lock()
            .map_err(|_| anyhow::anyhow!("reader error lock poisoned"))?
            .clone();
        state.stream = if let Some(runtime) = stream.as_ref() {
            Some(runtime.snapshot()?)
        } else {
            state.stream_config.as_ref().map(|config| StreamStatus {
                mount_point: config.mount_point.clone(),
                source_label: config.tile_source.label().to_owned(),
                budget_bytes: config.budget_mib * 1024 * 1024,
                last_error: stream_error.clone(),
                ..StreamStatus::default()
            })
        };
        Ok(state)
    }

    fn idle(state: &State) -> Result<()> {
        ensure!(
            !state
                .job
                .as_ref()
                .is_some_and(|j| matches!(j.state, JobState::Running)),
            "another job is running"
        );
        Ok(())
    }

    fn generation(&self, id: &str) -> Result<Generation> {
        ensure!(
            uuid::Uuid::parse_str(id).is_ok() && id.len() == 32,
            "generation ID must be the published 32-character UUID"
        );
        self.snapshot()?
            .generations
            .into_iter()
            .find(|g| g.id == id)
            .context("unknown generation; use status to select a published ID")
    }

    fn request(self: &Arc<Self>, request: &Request) -> Result<State> {
        ensure!(
            request.version == PROTOCOL_VERSION,
            "unsupported protocol version"
        );
        ensure!(request.id.len() <= 64, "request ID exceeds limit");
        match request.method.as_str() {
            "status" => self.snapshot(),
            "shutdown" => {
                let state = self.snapshot()?;
                Self::idle(&state)?;
                ensure!(
                    self.stream
                        .lock()
                        .map_err(|_| anyhow::anyhow!("reader runtime lock poisoned"))?
                        .is_none(),
                    "stop the streaming reader before stopping the engine"
                );
                self.stopping.store(true, Ordering::Relaxed);
                Ok(state)
            }
            "stream_configure" => {
                let config: StreamConfig = serde_json::from_value(request.params.clone())?;
                streaming::validate(&config, &self.data)?;
                ensure!(
                    self.stream
                        .lock()
                        .map_err(|_| anyhow::anyhow!("reader runtime lock poisoned"))?
                        .is_none(),
                    "stop the reader before changing source identity or storage policy"
                );
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("engine state lock poisoned"))?;
                Self::idle(&state)?;
                let mut updated = state.clone();
                updated.stream_config = Some(config);
                self.persist(&updated)?;
                *state = updated;
                drop(state);
                self.snapshot()
            }
            "stream_source" => {
                let params: StreamSourceParams = serde_json::from_value(request.params.clone())?;
                let was_running = {
                    let mut runtime = self
                        .stream
                        .lock()
                        .map_err(|_| anyhow::anyhow!("reader runtime lock poisoned"))?;
                    if let Some(reader) = runtime.as_ref() {
                        let status = reader.snapshot()?;
                        ensure!(
                            status.open_handles == 0
                                && status.active_consumers == 0
                                && status.pending == 0
                                && status.working == 0,
                            "reader is in use; exit X-Plane/readers first"
                        );
                    }
                    let running = runtime.is_some();
                    if let Some(reader) = runtime.as_mut() {
                        reader.stop()?;
                    }
                    runtime.take();
                    running
                };
                {
                    let mut state = self
                        .state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("engine state lock poisoned"))?;
                    Self::idle(&state)?;
                    let mut updated = state.clone();
                    let mut config = updated
                        .stream_config
                        .clone()
                        .context("configure the streaming reader first")?;
                    config.tile_source = params.tile_source;
                    if let Some(size) = params.size {
                        config.size = size;
                    }
                    streaming::validate(&config, &self.data)?;
                    updated.stream_config = Some(config);
                    self.persist(&updated)?;
                    *state = updated;
                }
                if was_running {
                    self.start_stream()
                } else {
                    self.snapshot()
                }
            }
            "stream_start" => self.start_stream(),
            "stream_stop" => {
                let mut runtime = self
                    .stream
                    .lock()
                    .map_err(|_| anyhow::anyhow!("reader runtime lock poisoned"))?;
                if let Some(reader) = runtime.as_ref() {
                    let status = reader.snapshot()?;
                    ensure!(
                        status.open_handles == 0
                            && status.active_consumers == 0
                            && status.pending == 0
                            && status.working == 0,
                        "reader is in use; exit X-Plane/readers first"
                    );
                }
                {
                    let mut state = self
                        .state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("engine state lock poisoned"))?;
                    let mut updated = state.clone();
                    updated.stream_enabled = false;
                    self.persist(&updated)?;
                    *state = updated;
                }
                if let Some(reader) = runtime.as_mut() {
                    reader.stop()?;
                }
                runtime.take();
                drop(runtime);
                self.snapshot()
            }
            "stream_install" => {
                let params: StreamInstallParams = serde_json::from_value(request.params.clone())?;
                let target = new_install_target(Path::new(&params.target))?;
                let snapshot = self.snapshot()?;
                let reader = snapshot
                    .stream
                    .context("start the reader before installing")?;
                ensure!(reader.mounted, "reader is not mounted");
                symlink(&reader.mount_point, &target)
                    .context("creating new reader scenery symlink")?;
                File::open(target.parent().context("missing install parent")?)?.sync_all()?;
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("engine state lock poisoned"))?;
                let mut updated = state.clone();
                updated.stream_installed =
                    Some(format!("{} -> {}", target.display(), reader.mount_point));
                self.persist(&updated)?;
                *state = updated;
                drop(state);
                self.snapshot()
            }
            "configure" => {
                let config: PrepareConfig = serde_json::from_value(request.params.clone())?;
                compiler::inspect(&config).context("validating source mesh and mappings")?;
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("engine state lock poisoned"))?;
                Self::idle(&state)?;
                let mut updated = state.clone();
                updated.config = Some(config);
                self.persist(&updated)?;
                *state = updated;
                Ok(state.clone())
            }
            "prepare" => {
                let config = self
                    .snapshot()?
                    .config
                    .context("configure a source package first")?;
                self.start("prepare", Work::Prepare(config))
            }
            "verify" => {
                let params: GenerationParams = serde_json::from_value(request.params.clone())?;
                self.start("verify", Work::Verify(self.generation(&params.generation)?))
            }
            "install" => {
                let params: InstallParams = serde_json::from_value(request.params.clone())?;
                let target = new_install_target(Path::new(&params.target))?;
                self.start(
                    "install",
                    Work::Install(self.generation(&params.generation)?, target),
                )
            }

            _ => bail!("unknown engine method: {}", request.method),
        }
    }

    fn start_stream(&self) -> Result<State> {
        let config = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("engine state lock poisoned"))?;
            Self::idle(&state)?;
            let config = state
                .stream_config
                .clone()
                .context("configure the reader once with stream configure")?;
            let mut updated = state.clone();
            updated.stream_enabled = true;
            self.persist(&updated)?;
            *state = updated;
            config
        };
        let mut stream = self
            .stream
            .lock()
            .map_err(|_| anyhow::anyhow!("reader runtime lock poisoned"))?;
        if stream.is_none() {
            match Runtime::start(&config, &self.data) {
                Ok(runtime) => {
                    *stream = Some(runtime);
                    *self
                        .stream_error
                        .lock()
                        .map_err(|_| anyhow::anyhow!("reader error lock poisoned"))? = None;
                }
                Err(error) => {
                    *self
                        .stream_error
                        .lock()
                        .map_err(|_| anyhow::anyhow!("reader error lock poisoned"))? =
                        Some(format!("{error:#}"));
                    return Err(error);
                }
            }
        }
        ensure!(
            stream
                .as_ref()
                .context("reader runtime missing")?
                .snapshot()?
                .mounted,
            "reader disconnected; exit its consumers and stop it before starting again"
        );
        drop(stream);
        self.snapshot()
    }

    fn start(self: &Arc<Self>, kind: &str, work: Work) -> Result<State> {
        ensure!(
            self.stream
                .lock()
                .map_err(|_| anyhow::anyhow!("reader runtime lock poisoned"))?
                .is_none(),
            "stop the streaming reader before running an offline generation job"
        );
        let id = uuid::Uuid::new_v4().simple().to_string();
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("engine state lock poisoned"))?;
            Self::idle(&state)?;
            ensure!(
                !matches!(&work, Work::Prepare(_)) || state.generations.len() < 32,
                "alpha generation limit reached (32); no generation is automatically deleted"
            );
            let mut updated = state.clone();
            let generation_id = match &work {
                Work::Prepare(_) => id.clone(),
                Work::Verify(g) | Work::Install(g, _) => g.id.clone(),
            };
            for generation in &mut updated.generations {
                if generation.id == generation_id {
                    generation.verified = false;
                }
            }
            updated.job = Some(Job {
                id: id.clone(),
                kind: kind.into(),
                generation: generation_id,
                state: JobState::Running,
                stage: "starting".into(),
                done: 0,
                total: 0,
                error: None,
                started: now(),
                finished: None,
            });
            self.persist(&updated)?;
            *state = updated;
        }
        let engine = Arc::clone(self);
        let worker_id = id.clone();
        let spawn = std::thread::Builder::new()
            .name("groundeffect-job".into())
            .spawn(move || {
                let result = engine.run_work(&worker_id, work);
                if let Err(error) = engine.finish(result) {
                    eprintln!("job {worker_id}: unable to persist result: {error:#}");
                }
            });
        if let Err(error) = spawn {
            self.finish(Err(error.into()))?;
        }
        self.snapshot()
    }

    fn progress(&self, stage: &str, done: usize, total: usize) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("engine state lock poisoned"))?;
        let job = state.job.as_mut().context("missing current job")?;
        job.stage = stage.into();
        job.done = done;
        job.total = total;
        self.persist(&state)
    }

    fn run_work(&self, id: &str, work: Work) -> Result<(Option<Generation>, Option<String>)> {
        match work {
            Work::Prepare(config) => Ok((
                Some(compiler::prepare(
                    &self.data,
                    id,
                    &config,
                    |stage, done, total| self.progress(stage, done, total),
                )?),
                None,
            )),
            Work::Verify(generation) => {
                self.progress("checking hashes and dependencies", 0, 1)?;
                compiler::verify_directory(
                    &self.data.join("generations").join(&generation.id),
                    &generation.manifest_sha256,
                )?;
                Ok((None, None))
            }
            Work::Install(generation, target) => {
                self.progress("verifying before installation", 0, 1)?;
                let root = self.data.join("generations").join(&generation.id);
                compiler::verify_directory(&root, &generation.manifest_sha256)?;
                symlink(root.join("scenery"), &target)
                    .context("creating new scenery symlink without replacing existing data")?;
                File::open(target.parent().context("missing install parent")?)?.sync_all()?;
                Ok((
                    None,
                    Some(format!("{} -> {}", target.display(), generation.id)),
                ))
            }
        }
    }

    fn finish(&self, result: Result<(Option<Generation>, Option<String>)>) -> Result<()> {
        let mut persisted = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("engine state lock poisoned"))?;
        let mut state = persisted.clone();
        let job = state.job.as_mut().context("missing completed job")?;
        job.finished = Some(now());
        match result {
            Ok((generation, installed)) => {
                job.state = JobState::Succeeded;
                job.stage = "complete".into();
                job.done = job.total;
                let target = job.generation.clone();
                for existing in &mut state.generations {
                    if existing.id == target {
                        existing.verified = true;
                    }
                }
                if let Some(generation) = generation {
                    state.generations.push(generation);
                }
                if installed.is_some() {
                    state.installed = installed;
                }
            }
            Err(error) => {
                job.state = JobState::Failed;
                job.stage = "failed".into();
                job.error = Some(format!("{error:#}"));
            }
        }
        if let Err(error) = self.persist(&state) {
            if let Some(job) = persisted.job.as_mut() {
                job.state = JobState::Failed;
                job.stage = "state persistence failed".into();
                job.error = Some(format!("{error:#}; completed output was not registered"));
            }
            return Err(error);
        }
        *persisted = state;
        Ok(())
    }
}

pub fn serve(data: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(data)?;
    let data = data.canonicalize()?;
    let meta = fs::metadata(&data)?;
    let uid = fs::metadata("/proc/self")?.uid();
    ensure!(uid != 0, "do not run GroundEffect as root");
    ensure!(
        meta.uid() == uid && meta.mode() & 0o077 == 0,
        "data directory must be owned by this user with mode 0700"
    );
    let lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(data.join("engine.lock"))?;
    lock.try_lock_exclusive()
        .context("another engine owns this data directory")?;
    let mut state: State = if data.join("state.json").exists() {
        serde_json::from_slice(&bounded_read(&data.join("state.json"), MAX_MESSAGE)?)?
    } else {
        State::default()
    };
    ensure!(
        state.version == PROTOCOL_VERSION,
        "unsupported persisted state version"
    );
    if let Some(job) = state.job.as_mut()
        && matches!(job.state, JobState::Running)
    {
        job.state = JobState::Failed;
        job.stage = "interrupted".into();
        job.error = Some(
            "engine stopped before completion; unregistered/staged output is not ready".into(),
        );
        job.finished = Some(now());
    }
    let engine = Arc::new(Engine {
        data: data.clone(),
        state: Mutex::new(state),
        stopping: AtomicBool::new(false),
        stream: Mutex::new(None),
        stream_error: Mutex::new(None),
    });
    engine.persist(&engine.snapshot()?)?;
    let socket = data.join("control.sock");
    match fs::remove_file(&socket) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("removing stale control socket"),
    }
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    if engine.snapshot()?.stream_enabled
        && let Err(error) = engine.start_stream()
    {
        eprintln!("configured automatic reader failed to start: {error:#}");
    }
    eprintln!("GroundEffect engine listening on {}", socket.display());
    for connection in listener.incoming() {
        match connection {
            Ok(mut stream) => {
                if let Err(error) = handle(&engine, &mut stream) {
                    eprintln!("control connection: {error:#}");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("accepting local control connection"),
        }
        if engine.stopping.load(Ordering::Relaxed) {
            break;
        }
    }
    fs::remove_file(&socket)?;
    Ok(())
}

fn handle(engine: &Arc<Engine>, stream: &mut UnixStream) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut bytes = Vec::new();
    let read = BufReader::new((&mut *stream).take(MAX_MESSAGE + 1)).read_until(b'\n', &mut bytes);
    let parsed = read.context("reading request").and_then(|_| {
        ensure!(
            bytes.len() as u64 <= MAX_MESSAGE && bytes.last() == Some(&b'\n'),
            "request must be a bounded JSON line"
        );
        serde_json::from_slice::<Request>(&bytes).context("decoding request")
    });
    let (id, result) = match parsed {
        Ok(request) => (request.id.clone(), engine.request(&request)),
        Err(error) => (String::new(), Err(error)),
    };
    let response = match result {
        Ok(state) => Response {
            version: PROTOCOL_VERSION,
            id,
            result: Some(state),
            error: None,
        },
        Err(error) => Response {
            version: PROTOCOL_VERSION,
            id,
            result: None,
            error: Some(format!("{error:#}")),
        },
    };
    let mut bytes = serde_json::to_vec(&response)?;
    ensure!(
        (bytes.len() as u64) < MAX_MESSAGE,
        "response exceeds protocol limit"
    );
    bytes.push(b'\n');
    stream.write_all(&bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistence_failure_never_reports_a_successful_job() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let data = temporary.path().join("state");
        fs::create_dir(&data)?;
        let job = Job {
            id: "job".into(),
            kind: "prepare".into(),
            generation: "job".into(),
            state: JobState::Running,
            stage: "starting".into(),
            done: 0,
            total: 1,
            error: None,
            started: now(),
            finished: None,
        };
        let engine = Engine {
            data: data.clone(),
            state: Mutex::new(State {
                job: Some(job),
                ..State::default()
            }),
            stopping: AtomicBool::new(false),
            stream: Mutex::new(None),
            stream_error: Mutex::new(None),
        };
        engine.persist(&engine.snapshot()?)?;
        fs::set_permissions(&data, fs::Permissions::from_mode(0o500))?;
        let result = engine.finish(Ok((None, None)));
        fs::set_permissions(&data, fs::Permissions::from_mode(0o700))?;
        assert!(result.is_err());
        assert!(matches!(
            engine.snapshot()?.job.context("job")?.state,
            JobState::Failed
        ));
        let disk: State = serde_json::from_slice(&fs::read(data.join("state.json"))?)?;
        assert!(matches!(disk.job.context("job")?.state, JobState::Running));
        Ok(())
    }
}
