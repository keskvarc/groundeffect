mod cache;
mod filesystem;
mod source;
mod telemetry;

use crate::model::{FlightPhase, StreamConfig, StreamStatus, TelemetryState};
use anyhow::{Context, Result, ensure};
use cache::Cache;
use filesystem::{Reader, Scene};
use fuser::{BackgroundSession, MountOption, Session};
use source::{Provider, Stamp};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd},
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, atomic::Ordering},
    thread::JoinHandle,
    time::{Duration, Instant},
};
use telemetry::{Telemetry, TelemetrySnapshot};

const MIB: u64 = 1024 * 1024;
const MAX_METADATA: u64 = 64 * MIB;
const WORK_RESERVATION: u64 = 80 * MIB;
const WORKERS: usize = 16;

fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    mutex
        .lock()
        .map_err(|_| anyhow::anyhow!("reader lock poisoned"))
}

pub fn validate(config: &StreamConfig, data: &Path) -> Result<()> {
    ensure!(
        matches!(config.size, 256 | 512 | 1024 | 2048),
        "reader profile must be 256, 512, 1024 or 2048 pixels"
    );
    ensure!(
        config.exposure.is_finite() && (0.0..=2.0).contains(&config.exposure),
        "invalid exposure"
    );
    ensure!(
        (512..=131072).contains(&config.budget_mib),
        "managed budget must be 512..131072 MiB"
    );
    ensure!(
        !config.source_revision.is_empty()
            && config.source_revision.len() <= 128
            && config.source_revision.bytes().all(|b| b.is_ascii_graphic()),
        "source revision must be 1..128 printable ASCII bytes"
    );
    if !config.source_package.trim().is_empty() {
        let package = Path::new(&config.source_package);
        ensure!(package.is_absolute(), "physical mesh path must be absolute");
        let root = package
            .canonicalize()
            .context("opening physical source mesh")?;
        ensure!(root.is_dir(), "source package is not a directory");
        for directory in ["Earth nav data", "terrain", "textures"] {
            let path = root.join(directory).canonicalize()?;
            ensure!(
                path.is_dir() && path.starts_with(&root),
                "invalid source directory {directory}"
            );
        }
        ensure!(
            !root.starts_with(data),
            "borrowed mesh must be outside engine-managed storage"
        );
    }
    let mount = Path::new(&config.mount_point);
    ensure!(
        mount.is_absolute() && mount == data.join("reader"),
        "mount point must be DATA_DIR/reader"
    );
    let osm_path = Path::new(&config.osm_db);
    ensure!(
        osm_path.is_absolute(),
        "borrowed lighting paths must be absolute"
    );
    ensure!(
        !osm_path.canonicalize()?.starts_with(data),
        "borrowed lighting must be outside engine-managed storage"
    );
    Stamp::read_source(osm_path)?;
    if let Some(viirs) = config.viirs.as_deref() {
        let viirs_path = Path::new(viirs);
        ensure!(
            viirs_path.is_absolute(),
            "borrowed lighting paths must be absolute"
        );
        ensure!(
            !viirs_path.canonicalize()?.starts_with(data),
            "borrowed lighting must be outside engine-managed storage"
        );
        Stamp::read(viirs_path)?;
    }
    crate::lighting::Lighting::open(
        Path::new(&config.osm_db),
        config.viirs.as_deref().map(Path::new),
    )?;
    Ok(())
}

fn other_managed_bytes(data: &Path) -> Result<u64> {
    let mut pending = vec![data.to_path_buf()];
    let mut bytes = 0_u64;
    let mut count = 0;
    while let Some(path) = pending.pop() {
        for item in fs::read_dir(path)? {
            let item = item?;
            if item.path() == data.join("reader-cache") || item.path() == data.join("reader") {
                continue;
            }
            count += 1;
            ensure!(
                count <= 200_000,
                "managed file inventory exceeds reader limit"
            );
            let metadata = item.metadata()?;
            let kind = item.file_type()?;
            if kind.is_dir() {
                pending.push(item.path());
            } else if kind.is_file() {
                bytes = bytes
                    .checked_add(metadata.blocks() * 512)
                    .context("managed byte overflow")?;
            } else {
                ensure!(
                    !kind.is_symlink(),
                    "unexpected symlink in engine-managed storage"
                );
            }
        }
    }
    Ok(bytes)
}

fn connection_abort(fd: BorrowedFd<'_>) -> Result<Option<File>> {
    let info = crate::raster::bounded_read(
        &PathBuf::from(format!("/proc/self/fdinfo/{}", fd.as_raw_fd())),
        65536,
    )?;
    let info = std::str::from_utf8(&info)?;
    let Some(connection) = info
        .lines()
        .find_map(|line| line.strip_prefix("fuse_connection:"))
    else {
        eprintln!("reader recovery: kernel fdinfo does not expose its FUSE connection");
        return Ok(None);
    };
    let connection: u64 = connection.trim().parse()?;
    let path = PathBuf::from(format!("/sys/fs/fuse/connections/{connection}/abort"));
    let file = match OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            eprintln!("reader recovery: owned FUSE abort control is unavailable");
            return Ok(None);
        }
        Err(error) => return Err(error).context("opening owned FUSE connection recovery"),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.uid() == fs::metadata("/proc/self")?.uid(),
        "FUSE recovery control is not owned by this engine user"
    );
    Ok(Some(file))
}

pub struct Runtime {
    scene: Arc<Scene>,
    cache: Arc<Cache>,
    telemetry: Option<Telemetry>,
    dsf_prep: Vec<JoinHandle<()>>,
    session: Option<BackgroundSession>,
    joining: Option<JoinHandle<io::Result<()>>>,
    abort: Option<File>,
    mount: PathBuf,
    borrowed: u64,
}

impl Runtime {
    pub fn start(config: &StreamConfig, data: &Path) -> Result<Self> {
        validate(config, data)?;
        let other = other_managed_bytes(data)?;
        let mount = PathBuf::from(&config.mount_point);
        let stale_fuse = matches!(
            fs::symlink_metadata(&mount),
            Err(ref error) if error.raw_os_error() == Some(libc::ENOTCONN)
        ) || matches!(
            fs::read_dir(&mount),
            Err(ref error) if error.raw_os_error() == Some(libc::ENOTCONN)
        );
        if stale_fuse {
            for bin in [
                "/run/wrappers/bin/fusermount3",
                "fusermount3",
                "/run/wrappers/bin/fusermount",
                "fusermount",
            ] {
                if std::process::Command::new(bin)
                    .arg("-u")
                    .arg("-z")
                    .arg(&mount)
                    .status()
                    .is_ok_and(|s| s.success())
                {
                    break;
                }
            }
        }
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&mount)?;
        ensure!(
            mount.canonicalize()? == mount && fs::read_dir(&mount)?.next().is_none(),
            "reader mount must be empty and not a symlink or an existing mount"
        );
        let scene = Arc::new(Scene::new(config.clone())?);
        let provider = Arc::new(Provider::new(config.clone(), Arc::clone(&scene))?);
        let borrowed = Stamp::read_source(Path::new(&config.osm_db))?.length
            + config
                .viirs
                .as_deref()
                .map(Path::new)
                .map(Stamp::read)
                .transpose()?
                .map_or(0, |s| s.length);
        let cache = Arc::new(Cache::new(
            &data.join("reader-cache"),
            config.budget_mib * MIB,
            other,
            provider,
        )?);
        let reader = Reader::new(Arc::clone(&scene), Arc::clone(&cache));
        let session = Session::new(
            reader,
            &mount,
            &[
                MountOption::RO,
                MountOption::NoAtime,
                MountOption::DefaultPermissions,
                MountOption::FSName("groundeffect".into()),
                MountOption::NoDev,
                MountOption::NoSuid,
            ],
        )
        .context("mounting read-only scenery; /dev/fuse and fusermount3 are required")?;
        let abort = connection_abort(session.as_fd())?;
        let session = session.spawn().context("starting scenery reader session")?;
        let started = Instant::now();
        while !scene.mounted.load(Ordering::Acquire) {
            ensure!(
                !session.guard.is_finished() && started.elapsed() < Duration::from_secs(2),
                "FUSE reader did not initialize"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        ensure!(
            fs::metadata(mount.join("Earth nav data"))?.is_dir(),
            "mounted reader is not responsive"
        );
        let telemetry_cache = Arc::downgrade(&cache);
        let telemetry = Telemetry::start(Arc::new(move |sample| {
            if let Some(cache) = telemetry_cache.upgrade() {
                let _ = cache.update_telemetry(TelemetrySnapshot {
                    state: TelemetryState::Fresh,
                    phase: sample.phase,
                    sample: Some(sample),
                    packets: 1,
                    invalid_packets: 0,
                });
            }
        }))
        .ok();
        let mut dsf_prep = Vec::with_capacity(4);
        for index in 0..4 {
            let prep_scene = Arc::clone(&scene);
            if let Ok(handle) = std::thread::Builder::new()
                .name(format!("groundeffect-dsf-prep-{index}"))
                .spawn(move || {
                    while let Ok(Some((lat, lon))) = prep_scene.next_pending_dsf_cell() {
                        let _ = prep_scene.prepare_dsf_cell(lat, lon);
                    }
                })
            {
                dsf_prep.push(handle);
            }
        }
        Ok(Self {
            scene,
            cache,
            telemetry,
            dsf_prep,
            session: Some(session),
            joining: None,
            abort,
            mount,
            borrowed,
        })
    }

    pub fn snapshot(&self) -> Result<StreamStatus> {
        if let Some(telemetry) = &self.telemetry {
            let snap = telemetry.snapshot(Instant::now());
            if snap.state == TelemetryState::Stale {
                let _ = self.cache.update_telemetry(snap);
            }
        }
        let mut status = self.cache.snapshot()?;
        if let Some(telemetry) = &self.telemetry {
            let snap = telemetry.snapshot(Instant::now());
            status.telemetry_state = snap.state;
            status.flight_phase = match snap.state {
                TelemetryState::Fresh => snap.phase,
                TelemetryState::Stale | TelemetryState::Unavailable => FlightPhase::Unknown,
            };
        }
        status.mounted = self.scene.mounted.load(Ordering::Acquire)
            && self
                .session
                .as_ref()
                .is_some_and(|s| !s.guard.is_finished());
        status.mount_point = self.mount.display().to_string();
        status.source_label = self.scene.source_label();
        status.borrowed_lighting_bytes = self.borrowed;
        let specs = self.scene.prepared_specs()?;
        let classified = self.cache.classify_specs(&specs)?;
        self.scene.status_with_tiles(&mut status, &classified)?;
        Ok(status)
    }

    pub fn stop(&mut self) -> Result<()> {
        let mut status = self.snapshot()?;
        if status.open_handles == 0 && status.active_consumers == 0 && status.pending == 0 {
            if let Some(telemetry) = self.telemetry.take() {
                telemetry.shutdown();
            }
            self.cache.quiesce_lookahead()?;
            status = self.snapshot()?;
        }
        ensure!(
            status.open_handles == 0
                && status.active_consumers == 0
                && status.pending == 0
                && status.working == 0,
            "reader is in use: {} handles, {} live reader processes, {} pending; exit X-Plane/readers first",
            status.open_handles,
            status.active_consumers,
            status.pending
        );
        if let Some(telemetry) = self.telemetry.take() {
            telemetry.shutdown();
        }
        self.scene.quiesce()?;
        for prep in self.dsf_prep.drain(..) {
            let _ = prep.join();
        }
        if let Some(session) = self.session.take() {
            // Moving the guard drops the SDK mount while preserving thread ownership.
            self.joining = Some(session.guard);
        }
        let started = Instant::now();
        while self
            .joining
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
        {
            if started.elapsed() >= Duration::from_millis(250)
                && let Some(abort) = self.abort.as_mut()
            {
                abort
                    .write_all(b"1\n")
                    .context("aborting the quiesced owned FUSE connection")?;
                break;
            }
            ensure!(
                started.elapsed() < Duration::from_secs(2),
                "reader unmount did not finish; owned FUSE recovery is unavailable"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let started = Instant::now();
        while self
            .joining
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
        {
            ensure!(
                started.elapsed() < Duration::from_secs(2),
                "owned FUSE connection did not stop; retry reader stop"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        if let Some(thread) = self.joining.take() {
            thread
                .join()
                .map_err(|_| anyhow::anyhow!("reader session thread panicked"))?
                .context("reader session failed during teardown")?;
        }
        ensure!(
            !self.scene.mounted.load(Ordering::Acquire),
            "reader session exited without destroying its filesystem"
        );
        self.abort.take();
        self.cache.shutdown()
    }
}
