use super::{
    MAX_METADATA, WORK_RESERVATION, WORKERS, lock,
    source::{Kind, Provider, SOURCE_RESERVATION, Spec, Stamp, digest, in_imagery_envelope},
    telemetry::{ReferenceKinematics, TelemetrySnapshot},
};
use crate::{
    geo::Footprint,
    model::{FxHashMap, StreamStatus, TelemetryState},
    raster::bounded_read,
};
use anyhow::{Context, Result, ensure};
use std::{
    fs::{self, File},
    io::{self, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, FileExt, MetadataExt},
    },
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, Weak},
    thread::JoinHandle,
    time::{Duration, Instant},
};

const MAX_ENTRIES: usize = 262_144;
const MAX_PENDING: usize = 256;
const MAX_QUEUE: usize = 64;
const MAX_LOOKAHEAD_QUEUE: usize = 32;
const SHA256_XATTR: &std::ffi::CStr = c"user.ge.sha256";

fn set_sha256_xattr(file: &File, sha256: &str) -> Result<()> {
    ensure!(
        sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid sha256 digest for xattr"
    );
    // SAFETY: `file` is a valid open file descriptor, `SHA256_XATTR` is a NUL-terminated C string,
    // and `sha256` points to `sha256.len()` valid bytes.
    let rc = unsafe {
        libc::fsetxattr(
            file.as_raw_fd(),
            SHA256_XATTR.as_ptr(),
            sha256.as_ptr().cast(),
            sha256.len(),
            0,
        )
    };
    if rc != 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EOPNOTSUPP) {
            return Ok(());
        }
        return Err(err).context("setting user.ge.sha256 xattr");
    }
    Ok(())
}

fn get_sha256_xattr(file: &File, path: &Path) -> Result<Option<String>> {
    let mut buf = [0_u8; 64];
    // SAFETY: `file` is a valid open file descriptor, `SHA256_XATTR` is NUL-terminated,
    // and `buf` is a writable 64-byte stack buffer.
    let rc = unsafe {
        libc::fgetxattr(
            file.as_raw_fd(),
            SHA256_XATTR.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    if rc < 0 {
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::ENODATA) => return Ok(None),
            Some(libc::EOPNOTSUPP) => {
                let bytes = bounded_read(path, super::source::MAX_RAW)?;
                return Ok(Some(digest(&bytes)));
            }
            _ => return Err(err).context("reading user.ge.sha256 xattr"),
        }
    }
    if rc != 64 || !buf.iter().all(|b| b.is_ascii_hexdigit()) {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8(buf.to_vec()).context("decoding sha256 xattr")?,
    ))
}

pub struct Cached {
    file: File,
    length: u64,
    stamp: Stamp,
    path: PathBuf,
}

impl Cached {
    pub fn read(&self, offset: u64, size: u32) -> Result<Vec<u8>> {
        ensure!(size <= 1024 * 1024, "filesystem read exceeds byte limit");
        self.stamp.check(&self.path)?;
        let length = self.length.saturating_sub(offset).min(size as u64) as usize;
        let mut bytes = vec![0; length];
        let mut position = 0;
        while position < length {
            let read = self
                .file
                .read_at(&mut bytes[position..], offset + position as u64)?;
            ensure!(read != 0, "immutable cache payload was truncated");
            position += read;
        }
        self.stamp.check(&self.path)?;
        Ok(bytes)
    }
}

type Completion = Box<dyn FnOnce(Result<Arc<Cached>>) + Send>;

struct Entry {
    length: u64,
    sha256: String,
    allocated: u64,
    used: u64,
    verified: Option<Stamp>,
    pinned: Weak<Cached>,
}

struct Flight {
    spec: Spec,
    waiters: Vec<Completion>,
    in_progress: bool,
    dispatchable: bool,
    lookahead_rank: u8,
    enqueued_at: u64,
}

struct State {
    entries: FxHashMap<String, Entry>,
    footprint_by_key: FxHashMap<String, Footprint>,
    flights: FxHashMap<String, Flight>,
    open_keys: FxHashMap<String, usize>,
    kinematics: Option<ReferenceKinematics>,
    demand_anchor: Option<(f64, f64)>,
    last_error_at: Option<Instant>,
    stopped: bool,
    #[cfg(test)]
    fail_publish: bool,
    stats: StreamStatus,
    clock: u64,
}

impl State {
    fn prune_unallocated_ledger_if_needed(&mut self, incoming_key: &str) -> Result<()> {
        if self.entries.contains_key(incoming_key) {
            return Ok(());
        }
        while self.entries.len() >= MAX_ENTRIES {
            let victim = self
                .entries
                .iter()
                .filter(|(k, e)| {
                    e.allocated == 0
                        && e.pinned.strong_count() == 0
                        && !self.flights.contains_key(*k)
                        && !self.open_keys.contains_key(*k)
                })
                .min_by_key(|(k, e)| (e.used, k.as_str()))
                .map(|(k, _)| k.clone())
                .context(
                    "bounded immutable identity ledger is full; start a new managed workspace after the flight",
                )?;
            self.entries.remove(&victim);
            self.footprint_by_key.remove(&victim);
        }
        Ok(())
    }
}

struct Shared {
    root: PathBuf,
    stage: PathBuf,
    provider: Arc<Provider>,
    state: Mutex<State>,
    condvar: Condvar,
}

pub struct Cache {
    shared: Arc<Shared>,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

fn allocated(length: u64) -> u64 {
    length.div_ceil(4096) * 4096
}

impl Cache {
    pub fn new(root: &Path, budget: u64, other: u64, provider: Arc<Provider>) -> Result<Self> {
        ensure!(
            budget > other + MAX_METADATA + SOURCE_RESERVATION + WORK_RESERVATION * 4,
            "managed budget is too small for retained native files, source pool, identity ledger and bounded workers"
        );
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root)?;
        ensure!(
            root.canonicalize()? == root,
            "cache directory must not be a symlink"
        );
        let objects = root.join("objects");
        let stage = root.join("staging");
        for path in [&objects, &stage] {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(path)
                .or_else(|e| {
                    if e.kind() == std::io::ErrorKind::AlreadyExists {
                        Ok(())
                    } else {
                        Err(e)
                    }
                })?;
            ensure!(
                path.canonicalize()? == *path,
                "cache subdirectory must not be a symlink"
            );
        }
        for item in fs::read_dir(&stage)? {
            let item = item?;
            let name = item.file_name();
            let name = name.to_str().context("invalid staging filename")?;
            ensure!(
                (name.starts_with("image-") || name.starts_with("asset-"))
                    && name.ends_with(".tmp")
                    && item.file_type()?.is_file(),
                "unexpected cache staging entry"
            );
            fs::remove_file(item.path()).context("removing interrupted reader staging file")?;
        }
        for legacy in ["index.sqlite", "index.sqlite-wal", "index.sqlite-shm"] {
            let legacy_path = root.join(legacy);
            if fs::symlink_metadata(&legacy_path).is_ok() {
                let _ = fs::remove_file(legacy_path);
            }
        }
        let mut entries = FxHashMap::default();
        let mut cache_bytes = 0;
        let mut clock = 0;
        for item in fs::read_dir(&objects)? {
            let item = item?;
            let name = item.file_name();
            let name = name.to_str().context("invalid cache object filename")?;
            let key = name
                .strip_suffix(".bin")
                .context("unexpected cache object")?;
            ensure!(
                key.len() == 64
                    && key.bytes().all(|b| b.is_ascii_hexdigit())
                    && item.file_type()?.is_file(),
                "invalid cache object"
            );
            let item_path = item.path();
            let file = File::open(&item_path).context("opening cached payload")?;
            let metadata = file.metadata().context("examining cached payload")?;
            let length = metadata.len();
            ensure!(
                length <= super::source::MAX_RAW,
                "invalid or oversized cache identity ledger"
            );
            let Some(sha256) = get_sha256_xattr(&file, &item_path)? else {
                drop(file);
                fs::remove_file(&item_path).context("removing unpublished cache object")?;
                continue;
            };
            ensure!(
                entries.len() < MAX_ENTRIES,
                "invalid or oversized cache identity ledger"
            );
            let bytes = metadata.blocks() * 512;
            let used = (metadata.mtime().max(0) as u64)
                .saturating_mul(1_000_000_000)
                .saturating_add(metadata.mtime_nsec().max(0) as u64);
            cache_bytes += bytes;
            clock = clock.max(used);
            entries.insert(
                key.to_owned(),
                Entry {
                    length,
                    sha256,
                    allocated: bytes,
                    used,
                    verified: None,
                    pinned: Weak::new(),
                },
            );
        }
        ensure!(
            cache_bytes + other + MAX_METADATA + SOURCE_RESERVATION <= budget,
            "existing managed storage exceeds configured ceiling"
        );
        let shared = Arc::new(Shared {
            root: objects,
            stage,
            provider,
            condvar: Condvar::new(),
            state: Mutex::new(State {
                entries,
                footprint_by_key: FxHashMap::default(),
                flights: FxHashMap::default(),
                open_keys: FxHashMap::default(),
                kinematics: None,
                demand_anchor: None,
                last_error_at: None,
                stopped: false,
                #[cfg(test)]
                fail_publish: false,
                clock,
                stats: StreamStatus {
                    budget_bytes: budget,
                    other_managed_bytes: other,
                    cache_bytes,
                    reserved_bytes: SOURCE_RESERVATION,
                    ..StreamStatus::default()
                },
            }),
        });
        let mut workers = Vec::new();
        for number in 0..WORKERS {
            let weak = Arc::downgrade(&shared);
            workers.push(
                std::thread::Builder::new()
                    .name(format!("groundeffect-source-{number}"))
                    .spawn(move || {
                        loop {
                            let Some(shared) = weak.upgrade() else { return };
                            let next = match shared.next_work() {
                                Ok(Some(key)) => key,
                                Ok(None) => return,
                                Err(e) => {
                                    eprintln!("reader source queue: {e:#}");
                                    return;
                                }
                            };
                            if let Err(e) = shared.process(&next) {
                                eprintln!("reader worker: {e:#}");
                            }
                        }
                    })?,
            );
        }
        Ok(Self {
            shared,
            workers: Mutex::new(workers),
        })
    }

    pub fn request(&self, spec: Spec, completion: Completion) {
        let key = spec.key.clone();
        let result = self.shared.enqueue(spec, completion);
        match result {
            Ok(false) => {}
            Ok(true) => {
                if let Err(error) = self.shared.activate(&key)
                    && let Err(e) = self.shared.finish(&key, Err(error))
                {
                    eprintln!("reader queue failure: {e:#}");
                }
            }
            Err(e) => eprintln!("reader dispatch: {e:#}"),
        }
    }

    pub fn update_telemetry(&self, snapshot: TelemetrySnapshot) -> Result<()> {
        self.shared.update_telemetry(snapshot)
    }

    pub fn quiesce_lookahead(&self) -> Result<()> {
        let mut state = lock(&self.shared.state)?;
        state
            .flights
            .retain(|_, f| f.in_progress || !f.waiters.is_empty());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state
            .flights
            .values()
            .any(|f| f.in_progress && f.waiters.is_empty())
        {
            let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
                break;
            };
            let (next_state, _) = self
                .shared
                .condvar
                .wait_timeout(state, remaining)
                .map_err(|_| anyhow::anyhow!("reader state lock poisoned"))?;
            state = next_state;
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Result<StreamStatus> {
        let mut state = lock(&self.shared.state)?;
        if state
            .last_error_at
            .is_some_and(|at| at.elapsed() >= Duration::from_secs(20))
        {
            state.stats.last_error = None;
            state.last_error_at = None;
        }
        let mut status = state.stats.clone();
        status.cached_assets = state.entries.values().filter(|e| e.allocated != 0).count();
        status.pinned_assets = state
            .entries
            .iter()
            .filter(|(key, e)| {
                e.allocated != 0
                    && (e.pinned.strong_count() != 0 || state.open_keys.contains_key(*key))
            })
            .count();
        status.pending = state.flights.values().map(|f| f.waiters.len()).sum();
        status.lookahead_queued = state
            .flights
            .values()
            .filter(|f| f.waiters.is_empty() && !f.in_progress)
            .count();
        (status.network_requests, status.network_bytes) = self.shared.provider.network();
        Ok(status)
    }

    pub fn classify_specs(
        &self,
        specs: &[(crate::geo::Footprint, Spec, Spec)],
    ) -> Result<Vec<(crate::geo::Footprint, bool, bool)>> {
        let state = lock(&self.shared.state)?;
        Ok(specs
            .iter()
            .map(|(fp, day, night)| {
                let is_building = state
                    .flights
                    .get(&day.key)
                    .is_some_and(|f| f.in_progress || !f.waiters.is_empty())
                    || state
                        .flights
                        .get(&night.key)
                        .is_some_and(|f| f.in_progress || !f.waiters.is_empty());
                let is_cached = state
                    .entries
                    .get(&day.key)
                    .is_some_and(|e| e.allocated != 0)
                    && state
                        .entries
                        .get(&night.key)
                        .is_some_and(|e| e.allocated != 0);
                (*fp, is_cached, is_building)
            })
            .collect())
    }

    pub fn pin(&self, key: &str) -> Result<()> {
        let mut state = lock(&self.shared.state)?;
        *state.open_keys.entry(key.to_owned()).or_default() += 1;
        Ok(())
    }

    pub fn unpin(&self, key: &str) -> Result<()> {
        let mut state = lock(&self.shared.state)?;
        let count = state
            .open_keys
            .get_mut(key)
            .context("missing open-asset pin")?;
        *count -= 1;
        if *count == 0 {
            state.open_keys.remove(key);
        }
        Ok(())
    }

    pub fn shutdown(&self) -> Result<()> {
        {
            let mut state = lock(&self.shared.state)?;
            state.stopped = true;
            state
                .flights
                .retain(|_, f| f.in_progress || !f.waiters.is_empty());
        }
        self.shared.condvar.notify_all();
        for worker in lock(&self.workers)?.drain(..) {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("reader source worker panicked"))?;
        }
        Ok(())
    }
}

impl Shared {
    fn actual_demand_subrank(&self, state: &State, spec: &Spec) -> u8 {
        if state
            .entries
            .get(&spec.key)
            .is_some_and(|e| e.allocated != 0)
        {
            return 0;
        }
        match spec.kind {
            Kind::Raw { .. } => 0,
            Kind::Day(fp) if self.provider.has_ready_source(fp).unwrap_or(false) => 1,
            Kind::Day(_) => 2,
            Kind::Night(_) => 3,
        }
    }

    fn deadline_slack_ms(
        spec: &Spec,
        kinematics: Option<&ReferenceKinematics>,
        demand_anchor: Option<(f64, f64)>,
    ) -> i64 {
        let fp = match spec.kind {
            Kind::Day(fp) | Kind::Night(fp) => fp,
            Kind::Raw { .. } => return i64::MIN / 2,
        };
        if let Some(kin) = kinematics {
            kin.deadline_slack_millis(fp)
        } else if let Some((lat, lon)) = demand_anchor {
            ReferenceKinematics::fallback(lat, lon).deadline_slack_millis(fp)
        } else {
            -((i64::from(fp.zoom) - 16) * 120_000)
        }
    }

    fn select_next_flight(&self, state: &State) -> Option<String> {
        let kin = state.kinematics.as_ref();
        let anchor = state.demand_anchor;
        if let Some(key) = state
            .flights
            .iter()
            .filter(|(_, f)| f.dispatchable && !f.in_progress && !f.waiters.is_empty())
            .min_by_key(|(k, f)| {
                (
                    self.actual_demand_subrank(state, &f.spec),
                    Self::deadline_slack_ms(&f.spec, kin, anchor),
                    f.enqueued_at,
                    k.as_str(),
                )
            })
            .map(|(k, _)| k.clone())
        {
            return Some(key);
        }

        let reserve_workers = usize::from(WORKERS > 1);
        let budget_headroom = state.stats.cache_bytes
            + state.stats.reserved_bytes
            + state.stats.other_managed_bytes
            + MAX_METADATA
            + WORK_RESERVATION * 2
            <= state.stats.budget_bytes;
        if state.stats.working + reserve_workers >= WORKERS || !budget_headroom {
            return None;
        }

        state
            .flights
            .iter()
            .filter(|(_, f)| f.dispatchable && !f.in_progress && f.waiters.is_empty())
            .min_by_key(|(k, f)| {
                let rank = match f.spec.kind {
                    Kind::Day(fp) if self.provider.has_ready_source(fp).unwrap_or(false) => 0,
                    Kind::Night(fp) if self.provider.has_ready_source(fp).unwrap_or(false) => {
                        f.lookahead_rank.min(2)
                    }
                    _ => f.lookahead_rank,
                };
                (
                    rank,
                    Self::deadline_slack_ms(&f.spec, kin, anchor),
                    f.enqueued_at,
                    k.as_str(),
                )
            })
            .map(|(k, _)| k.clone())
    }

    fn next_work(&self) -> Result<Option<String>> {
        let mut state = lock(&self.state)?;
        loop {
            if let Some(key) = self.select_next_flight(&state) {
                let flight = state
                    .flights
                    .get_mut(&key)
                    .context("selected flight disappeared")?;
                flight.in_progress = true;
                state.stats.working += 1;
                return Ok(Some(key));
            }
            if state.stopped {
                return Ok(None);
            }
            state = self
                .condvar
                .wait(state)
                .map_err(|_| anyhow::anyhow!("reader state lock poisoned"))?;
        }
    }

    fn activate(&self, key: &str) -> Result<()> {
        let mut state = lock(&self.state)?;
        if state.stopped {
            anyhow::bail!("reader source queue is stopped");
        }
        let queued_demand = state
            .flights
            .values()
            .filter(|f| f.dispatchable && !f.in_progress && !f.waiters.is_empty())
            .count();
        if queued_demand >= MAX_QUEUE {
            anyhow::bail!("bounded cold request queue is full");
        }
        if let Some(flight) = state.flights.get_mut(key) {
            flight.dispatchable = true;
        }
        drop(state);
        self.condvar.notify_one();
        Ok(())
    }

    fn enqueue(&self, spec: Spec, completion: Completion) -> Result<bool> {
        let mut state = match lock(&self.state) {
            Ok(state) => state,
            Err(e) => {
                completion(Err(anyhow::anyhow!("{e:#}")));
                return Err(e);
            }
        };
        state.clock += 1;
        let used = state.clock;
        if let Kind::Day(fp) | Kind::Night(fp) = spec.kind {
            let b = fp.geographic();
            state.demand_anchor = Some(((b.north + b.south) * 0.5, (b.east + b.west) * 0.5));
        }
        if !matches!(spec.kind, Kind::Raw { dsf: true, .. })
            && let Some(entry) = state.entries.get_mut(&spec.key)
        {
            if entry.length != spec.length {
                drop(state);
                completion(Err(anyhow::anyhow!("immutable asset length mismatch")));
                return Ok(false);
            }
            entry.used = used;
            let ready = if let Some(file) = entry.pinned.upgrade() {
                Some(Ok(file))
            } else if let Some(stamp) = &entry.verified {
                let path = self.root.join(format!("{}.bin", spec.key));
                Some((|| {
                    stamp.check(&path)?;
                    let file = Arc::new(Cached {
                        file: File::open(&path)?,
                        length: entry.length,
                        stamp: stamp.clone(),
                        path,
                    });
                    entry.pinned = Arc::downgrade(&file);
                    Ok(file)
                })())
            } else {
                None
            };
            if let Some(result) = ready {
                state.stats.cache_hits += 1;
                drop(state);
                completion(result);
                return Ok(false);
            }
        }
        if state
            .flights
            .values()
            .map(|f| f.waiters.len())
            .sum::<usize>()
            >= MAX_PENDING
        {
            drop(state);
            completion(Err(anyhow::anyhow!("bounded pending-read limit reached")));
            return Ok(false);
        }
        if let Some(flight) = state.flights.get_mut(&spec.key) {
            let was_lookahead = flight.waiters.is_empty();
            flight.waiters.push(completion);
            let should_wake = was_lookahead && flight.dispatchable && !flight.in_progress;
            state.stats.coalesced += 1;
            drop(state);
            if should_wake {
                self.condvar.notify_one();
            }
            return Ok(false);
        }
        state.flights.insert(
            spec.key.clone(),
            Flight {
                spec,
                waiters: vec![completion],
                in_progress: false,
                dispatchable: false,
                lookahead_rank: 0,
                enqueued_at: used,
            },
        );
        Ok(true)
    }

    fn update_telemetry(&self, snapshot: TelemetrySnapshot) -> Result<()> {
        let kin = {
            let mut state = lock(&self.state)?;
            state.stats.telemetry_state = snapshot.state;
            state.stats.flight_phase = snapshot.phase;
            if let Some(sample) = &snapshot.sample {
                state.stats.aircraft_lat = Some(sample.lat);
                state.stats.aircraft_lon = Some(sample.lon);
                state.stats.aircraft_agl_m = Some(sample.agl_m);
                state.stats.ground_speed_mps = Some(sample.ground_speed_mps);
                state.stats.track_deg = Some(sample.track_deg);
            }
            let kin = match (snapshot.state, snapshot.sample.as_ref()) {
                (TelemetryState::Fresh, Some(sample)) => {
                    if let Some((alat, alon)) = state.demand_anchor
                        && ((sample.lat - alat).abs() > 2.5 || (sample.lon - alon).abs() > 3.5)
                        && state
                            .flights
                            .values()
                            .any(|f| !f.waiters.is_empty() || f.in_progress)
                    {
                        Some(ReferenceKinematics::fallback(alat, alon))
                    } else {
                        Some(ReferenceKinematics::from_sample(sample))
                    }
                }
                (_, Some(sample)) => {
                    if let Some((alat, alon)) = state.demand_anchor
                        && ((sample.lat - alat).abs() > 2.5 || (sample.lon - alon).abs() > 3.5)
                    {
                        Some(ReferenceKinematics::fallback(alat, alon))
                    } else {
                        Some(ReferenceKinematics::fallback(sample.lat, sample.lon))
                    }
                }
                (_, None) => state
                    .demand_anchor
                    .map(|(alat, alon)| ReferenceKinematics::fallback(alat, alon)),
            };
            state.kinematics = kin;
            if let Some(kin) = kin {
                let mut cancelled = 0_u64;
                state.flights.retain(|_, flight| {
                    if !flight.waiters.is_empty() || flight.in_progress {
                        return true;
                    }
                    let (Kind::Day(fp) | Kind::Night(fp)) = flight.spec.kind else {
                        return true;
                    };
                    match kin.classify_lookahead(fp) {
                        Some(rank) => {
                            flight.lookahead_rank = if matches!(flight.spec.kind, Kind::Night(_)) {
                                rank.saturating_add(1)
                            } else {
                                rank
                            };
                            true
                        }
                        None => {
                            cancelled += 1;
                            false
                        }
                    }
                });
                state.stats.lookahead_cancelled += cancelled;
            }
            kin
        };

        let Some(kin) = kin else {
            return Ok(());
        };
        for (lat_floor, lon_floor) in kin.candidate_dsf_cells() {
            let _ = self.provider.request_dsf_cell(lat_floor, lon_floor);
        }
        let prepared = self.provider.prepared_specs()?;
        let mut candidates = Vec::new();
        for (fp, day_spec, night_spec) in &prepared {
            if !in_imagery_envelope(*fp) {
                continue;
            }
            if let Some(rank) = kin.classify_lookahead(*fp) {
                let slack = kin.deadline_slack_millis(*fp);
                candidates.push((rank, slack, day_spec.clone()));
                candidates.push((rank + 1, slack + 30_000, night_spec.clone()));
            }
        }
        candidates.sort_by(|a, b| (a.0, a.1, a.2.key.as_str()).cmp(&(b.0, b.1, b.2.key.as_str())));

        let mut notify = false;
        {
            let mut state = lock(&self.state)?;
            if state.stopped {
                return Ok(());
            }
            for (fp, day_spec, night_spec) in &prepared {
                state.footprint_by_key.insert(day_spec.key.clone(), *fp);
                state.footprint_by_key.insert(night_spec.key.clone(), *fp);
            }
            let protected_cache_bytes: u64 = state
                .entries
                .iter()
                .filter(|(k, e)| {
                    if e.allocated == 0 {
                        return false;
                    }
                    if e.pinned.strong_count() > 0
                        || state.flights.contains_key(*k)
                        || state.open_keys.contains_key(*k)
                    {
                        return true;
                    }
                    state
                        .footprint_by_key
                        .get(*k)
                        .is_some_and(|fp| kin.is_protected(*fp))
                })
                .map(|(_, e)| e.allocated)
                .sum();
            let budget_headroom = protected_cache_bytes
                + state.stats.reserved_bytes
                + state.stats.other_managed_bytes
                + MAX_METADATA
                + WORK_RESERVATION * 2
                <= state.stats.budget_bytes;
            if budget_headroom {
                let mut queued_lookahead = state
                    .flights
                    .values()
                    .filter(|f| f.waiters.is_empty() && !f.in_progress)
                    .count();
                for (rank, _, spec) in candidates {
                    if queued_lookahead >= MAX_LOOKAHEAD_QUEUE {
                        break;
                    }
                    if state.flights.contains_key(&spec.key)
                        || state
                            .entries
                            .get(&spec.key)
                            .is_some_and(|e| e.allocated != 0)
                    {
                        continue;
                    }
                    state.clock += 1;
                    let enqueued_at = state.clock;
                    state.flights.insert(
                        spec.key.clone(),
                        Flight {
                            spec,
                            waiters: Vec::new(),
                            in_progress: false,
                            dispatchable: true,
                            lookahead_rank: rank,
                            enqueued_at,
                        },
                    );
                    queued_lookahead += 1;
                    notify = true;
                }
            }
        }
        if notify {
            self.condvar.notify_all();
        }
        Ok(())
    }

    fn reserve(&self, key: &str) -> Result<()> {
        let mut state = lock(&self.state)?;
        let is_lookahead = state.flights.get(key).is_some_and(|f| f.waiters.is_empty());
        while state.stats.cache_bytes
            + state.stats.reserved_bytes
            + state.stats.other_managed_bytes
            + MAX_METADATA
            + WORK_RESERVATION
            > state.stats.budget_bytes
        {
            let kin = state.kinematics;
            let footprints = &state.footprint_by_key;
            let victim = state
                .entries
                .iter()
                .filter(|(k, e)| {
                    if k.as_str() == key
                        || e.allocated == 0
                        || e.pinned.strong_count() > 0
                        || state.flights.contains_key(*k)
                        || state.open_keys.contains_key(*k)
                    {
                        return false;
                    }
                    if is_lookahead
                        && let (Some(kin), Some(fp)) = (kin.as_ref(), footprints.get(*k))
                        && kin.is_protected(*fp)
                    {
                        return false;
                    }
                    true
                })
                .min_by_key(|(k, e)| {
                    let tier = match (kin.as_ref(), footprints.get(*k)) {
                        (Some(kin), Some(fp)) if kin.is_behind(*fp) => 0_u8,
                        (Some(kin), Some(fp)) if !kin.is_protected(*fp) => 1_u8,
                        _ => 2_u8,
                    };
                    (tier, e.used, k.as_str())
                })
                .map(|(k, _)| k.clone())
                .context("managed cache is full and remaining assets are pinned/in use")?;
            fs::remove_file(self.root.join(format!("{victim}.bin")))?;
            let entry = state
                .entries
                .get_mut(&victim)
                .context("missing eviction entry")?;
            let bytes = entry.allocated;
            entry.allocated = 0;
            entry.verified = None;
            state.stats.cache_bytes -= bytes;
            state.stats.evictions += 1;
        }
        state.stats.reserved_bytes += WORK_RESERVATION;
        Ok(())
    }

    fn process(&self, key: &str) -> Result<()> {
        let spec = {
            let mut state = lock(&self.state)?;
            let (spec, newly_started) = {
                let flight = state.flights.get_mut(key).context("missing queued asset")?;
                let newly_started = !flight.in_progress;
                flight.in_progress = true;
                (flight.spec.clone(), newly_started)
            };
            if newly_started {
                state.stats.working += 1;
            }
            spec
        };
        let borrowed = matches!(spec.kind, Kind::Raw { dsf: true, .. });
        let reservation = if borrowed { Ok(()) } else { self.reserve(key) };
        let reserved = !borrowed && reservation.is_ok();
        if !reserved && !borrowed {
            let dropped_lookahead = {
                let mut state = lock(&self.state)?;
                if state.flights.get(key).is_some_and(|f| f.waiters.is_empty()) {
                    state.stats.working -= 1;
                    state.stats.lookahead_cancelled += 1;
                    state.flights.remove(key);
                    true
                } else {
                    false
                }
            };
            if dropped_lookahead {
                self.condvar.notify_all();
                return Ok(());
            }
        }
        let result = reservation.and_then(|_| self.resolve(&spec));
        let succeeded = result.is_ok();
        {
            let mut state = lock(&self.state)?;
            state.stats.working -= 1;
            if reserved {
                state.stats.reserved_bytes -= WORK_RESERVATION;
            }
        }
        self.finish(key, result)?;
        self.condvar.notify_all();
        if succeeded && let Kind::Day(footprint) = spec.kind {
            self.complete_ready_siblings(footprint)?;
        }
        Ok(())
    }

    fn complete_ready_siblings(&self, footprint: crate::geo::Footprint) -> Result<()> {
        for sibling in self.provider.ready_sibling_day_specs(footprint)? {
            let Kind::Day(sibling_footprint) = sibling.kind else {
                continue;
            };
            if !self.provider.has_ready_source(sibling_footprint)? {
                break;
            }
            {
                let mut state = lock(&self.state)?;
                if state.stopped
                    || state
                        .flights
                        .values()
                        .any(|f| f.dispatchable && !f.in_progress && !f.waiters.is_empty())
                {
                    break;
                }
                if state
                    .flights
                    .get(&sibling.key)
                    .is_some_and(|f| f.in_progress || !f.waiters.is_empty())
                    || state
                        .entries
                        .get(&sibling.key)
                        .is_some_and(|e| e.allocated != 0)
                {
                    continue;
                }
                if state.stats.cache_bytes
                    + state.stats.reserved_bytes
                    + state.stats.other_managed_bytes
                    + MAX_METADATA
                    + WORK_RESERVATION
                    > state.stats.budget_bytes
                {
                    break;
                }
                state.clock += 1;
                let enqueued_at = state.clock;
                state.stats.working += 1;
                state.stats.reserved_bytes += WORK_RESERVATION;
                let existing_rank = state
                    .flights
                    .get(&sibling.key)
                    .map(|f| f.lookahead_rank)
                    .unwrap_or(1);
                state.flights.insert(
                    sibling.key.clone(),
                    Flight {
                        spec: sibling.clone(),
                        waiters: Vec::new(),
                        in_progress: true,
                        dispatchable: true,
                        lookahead_rank: existing_rank,
                        enqueued_at,
                    },
                );
            }
            let result = self.resolve(&sibling);
            let should_finish = {
                let mut state = lock(&self.state)?;
                state.stats.working -= 1;
                state.stats.reserved_bytes -= WORK_RESERVATION;
                if result.is_err()
                    && state
                        .flights
                        .get(&sibling.key)
                        .is_some_and(|f| f.waiters.is_empty())
                {
                    state.flights.remove(&sibling.key);
                    false
                } else {
                    true
                }
            };
            if should_finish {
                self.finish(&sibling.key, result)?;
            }
            self.condvar.notify_all();
        }
        Ok(())
    }

    fn resolve(&self, spec: &Spec) -> Result<Arc<Cached>> {
        if let Some(file) = self.provider.borrowed_mesh(spec)?
            && let Kind::Raw { path, stamp, .. } = &spec.kind
        {
            return Ok(Arc::new(Cached {
                file,
                length: spec.length,
                stamp: stamp.clone(),
                path: path.clone(),
            }));
        }
        let existing = {
            let state = lock(&self.state)?;
            state
                .entries
                .get(&spec.key)
                .map(|e| (e.allocated, e.sha256.clone()))
        };
        let path = self.root.join(format!("{}.bin", spec.key));
        if let Some((bytes, sha)) = &existing
            && *bytes != 0
        {
            let before = Stamp::read(&path)?;
            let contents = bounded_read(&path, super::source::MAX_RAW)?;
            ensure!(
                digest(&contents) == *sha,
                "cached checksum mismatch; asset is not safe to serve"
            );
            self.provider.validate(spec, &contents, &path)?;
            before.check(&path)?;
            let file = Arc::new(Cached {
                file: File::open(&path)?,
                length: spec.length,
                stamp: before.clone(),
                path,
            });
            let mut state = lock(&self.state)?;
            let entry = state
                .entries
                .get_mut(&spec.key)
                .context("cache record disappeared")?;
            entry.verified = Some(before);
            entry.pinned = Arc::downgrade(&file);
            state.stats.cache_hits += 1;
            return Ok(file);
        }
        let contents = self
            .provider
            .build(spec, &self.stage)
            .with_context(|| format!("building demanded asset {}", spec.key))?;
        ensure!(
            allocated(contents.len() as u64) <= WORK_RESERVATION,
            "asset exceeds reservation"
        );
        let sha = digest(&contents);
        if matches!(spec.kind, Kind::Raw { .. })
            && let Some((_, expected)) = existing
        {
            ensure!(
                sha == expected,
                "source/content changed after cache eviction; stop the reader and explicitly update source_revision"
            );
        }
        let mut staged = tempfile::Builder::new()
            .prefix("asset-")
            .suffix(".tmp")
            .tempfile_in(&self.stage)?;
        staged.write_all(&contents)?;
        staged.flush()?;
        self.provider.validate(spec, &contents, staged.path())?;
        set_sha256_xattr(staged.as_file(), &sha)?;
        staged.as_file().sync_all()?;
        {
            let mut state = lock(&self.state)?;
            state.prune_unallocated_ledger_if_needed(&spec.key)?;
        }
        let file = staged
            .persist_noclobber(&path)
            .context("publishing immutable cache object")?;
        let published = (|| -> Result<(Stamp, u64)> {
            File::open(&self.root)?.sync_all()?;
            let stamp = Stamp::read(&path)?;
            let bytes = file.metadata()?.blocks() * 512;
            Ok((stamp, bytes))
        })();
        let (stamp, bytes) = match published {
            Ok(result) => result,
            Err(error) => {
                if let Err(cleanup) = fs::remove_file(&path) {
                    let mut state = lock(&self.state)?;
                    state.stats.cache_bytes += WORK_RESERVATION;
                    return Err(error).context(format!(
                        "unpublished object cleanup failed: {cleanup}; reservation retained until restart"
                    ));
                }
                return Err(error);
            }
        };
        let mut state = lock(&self.state)?;
        #[cfg(test)]
        if state.fail_publish {
            if let Err(cleanup) = fs::remove_file(&path) {
                state.stats.cache_bytes += WORK_RESERVATION;
                return Err(anyhow::anyhow!(
                    "simulated publication failure; cleanup failed: {cleanup}"
                ));
            }
            anyhow::bail!("simulated publication failure");
        }
        if let Err(error) = state.prune_unallocated_ledger_if_needed(&spec.key) {
            if let Err(cleanup) = fs::remove_file(&path) {
                state.stats.cache_bytes += WORK_RESERVATION;
                return Err(error).context(format!(
                    "unpublished object cleanup failed: {cleanup}; reservation retained until restart"
                ));
            }
            return Err(error);
        }
        state.clock += 1;
        let used = state.clock;
        let cached = Arc::new(Cached {
            file,
            length: spec.length,
            stamp: stamp.clone(),
            path,
        });
        if let Kind::Day(fp) | Kind::Night(fp) = spec.kind {
            state.footprint_by_key.insert(spec.key.clone(), fp);
        }
        state.entries.insert(
            spec.key.clone(),
            Entry {
                length: spec.length,
                sha256: sha,
                allocated: bytes,
                used,
                verified: Some(stamp),
                pinned: Arc::downgrade(&cached),
            },
        );
        state.stats.cache_bytes += bytes;
        state.stats.cold_builds += 1;
        Ok(cached)
    }

    fn finish(&self, key: &str, result: Result<Arc<Cached>>) -> Result<()> {
        let error = result.as_ref().err().map(|e| format!("{e:#}"));
        let waiters = {
            let mut state = lock(&self.state)?;
            if let Some(error) = &error {
                state.stats.failures += 1;
                state.stats.last_error = Some(error.chars().take(2048).collect());
                state.last_error_at = Some(Instant::now());
                if state.stats.failures <= 64 {
                    eprintln!(
                        "reader asset {key}: {}",
                        state
                            .stats
                            .last_error
                            .as_deref()
                            .context("missing recorded error")?
                    );
                }
            } else if state
                .last_error_at
                .is_some_and(|at| at.elapsed() >= Duration::from_secs(15))
            {
                state.stats.last_error = None;
                state.last_error_at = None;
            }
            let flight = state
                .flights
                .remove(key)
                .context("missing completed asset")?;
            if flight.lookahead_rank > 0 && result.is_ok() {
                state.stats.lookahead_completed += 1;
            }
            flight.waiters
        };
        for completion in waiters {
            match &result {
                Ok(file) => completion(Ok(Arc::clone(file))),
                Err(_) => completion(Err(anyhow::anyhow!(
                    "{}",
                    error.as_deref().context("missing source error")?
                ))),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        dds,
        model::StreamConfig,
        streaming::{filesystem::Scene, source::Kind},
    };
    use std::{sync::mpsc, time::Duration};

    fn provider(path: &Path) -> Result<Arc<Provider>> {
        fs::write(path.join("osm.sqlite"), [])?;
        let config = StreamConfig {
            source_package: path.display().to_string(),
            mount_point: path.join("reader").display().to_string(),
            osm_db: path.join("osm.sqlite").display().to_string(),
            viirs: None,
            source_revision: "test".into(),
            tile_source: crate::model::TileSource::default(),
            size: 256,
            exposure: 0.35,
            budget_mib: 512,
        };
        Ok(Arc::new(Provider::new(
            config.clone(),
            Arc::new(Scene::new(config)?),
        )?))
    }

    fn asset(root: &Path, name: &str, value: f32) -> Result<(Spec, Vec<u8>)> {
        let path = root.join(format!("{name}.dds"));
        let bytes = dds::encode_rgb(16, vec![[value, 0.1, 0.03]; 256], false)?;
        fs::write(&path, &bytes)?;
        Ok((
            Spec {
                key: digest(name.as_bytes()),
                length: bytes.len() as u64,
                kind: Kind::Raw {
                    stamp: Stamp::read(&path)?,
                    path,
                    dsf: false,
                },
            },
            bytes,
        ))
    }

    fn resolve(cache: &Cache, spec: Spec) -> Result<Arc<Cached>> {
        let (tx, rx) = mpsc::channel();
        cache.request(
            spec,
            Box::new(move |result| {
                tx.send(result).expect("test receiver alive");
            }),
        );
        rx.recv_timeout(Duration::from_secs(5))?
    }

    #[test]
    fn coalesces_cold_requests_and_serves_exact_ranges() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path();
        let cache = Cache::new(&root.join("cache"), 512 * 1024 * 1024, 0, provider(root)?)?;
        let (spec, bytes) = asset(root, "one", 0.2)?;
        let mut receivers = Vec::new();
        for index in 0..8 {
            let (tx, rx) = mpsc::channel();
            let queued = cache.shared.enqueue(
                spec.clone(),
                Box::new(move |result| {
                    tx.send(result).expect("test receiver alive");
                }),
            )?;
            assert_eq!(queued, index == 0);
            receivers.push(rx);
        }
        cache.shared.process(&spec.key)?;
        let mut files = Vec::new();
        for rx in receivers {
            files.push(rx.recv_timeout(Duration::from_secs(5))??);
        }
        assert!(files.iter().all(|file| Arc::ptr_eq(file, &files[0])));
        assert_eq!(files[0].read(0, 128)?, bytes[..128]);
        assert_eq!(files[0].read(127, 64)?, bytes[127..191]);
        assert_eq!(
            files[0].read(bytes.len() as u64 - 3, 64)?,
            bytes[bytes.len() - 3..]
        );
        assert!(files[0].read(bytes.len() as u64, 64)?.is_empty());
        let stats = cache.snapshot()?;
        assert_eq!(
            (stats.cold_builds, stats.coalesced, stats.pending),
            (1, 7, 0)
        );
        cache.shutdown()?;
        Ok(())
    }

    #[test]
    fn pins_before_first_read_and_keeps_evicted_identity() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path();
        let cache = Cache::new(&root.join("cache"), 512 * 1024 * 1024, 0, provider(root)?)?;
        let (a, _) = asset(root, "a", 0.2)?;
        let (b, _) = asset(root, "b", 0.3)?;
        drop(resolve(&cache, a.clone())?);
        lock(&cache.shared.state)?.stats.budget_bytes =
            MAX_METADATA + SOURCE_RESERVATION + WORK_RESERVATION;
        cache.pin(&a.key)?;
        assert!(
            format!(
                "{:#}",
                resolve(&cache, b.clone())
                    .err()
                    .context("expected pinned failure")?
            )
            .contains("pinned")
        );
        cache.unpin(&a.key)?;
        drop(resolve(&cache, b)?);
        assert_eq!(cache.snapshot()?.evictions, 1);
        let (mut changed, _) = asset(root, "a", 0.7)?;
        changed.key = a.key;
        assert!(
            format!(
                "{:#}",
                resolve(&cache, changed)
                    .err()
                    .context("expected changed-source failure")?
            )
            .contains("source/content changed")
        );
        assert_eq!(cache.snapshot()?.failures, 2);
        cache.shutdown()?;
        Ok(())
    }

    #[test]
    fn warm_restart_does_not_depend_on_borrowed_payload_and_rejects_corruption() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path();
        let directory = root.join("cache");
        let (spec, expected) = asset(root, "warm", 0.2)?;
        {
            let cache = Cache::new(&directory, 512 * 1024 * 1024, 0, provider(root)?)?;
            drop(resolve(&cache, spec.clone())?);
            cache.shutdown()?;
        }
        fs::remove_file(root.join("warm.dds"))?;
        {
            let cache = Cache::new(&directory, 512 * 1024 * 1024, 0, provider(root)?)?;
            assert_eq!(resolve(&cache, spec.clone())?.read(0, 65536)?, expected);
            assert_eq!(
                (cache.snapshot()?.cache_hits, cache.snapshot()?.cold_builds),
                (1, 0)
            );
            cache.shutdown()?;
        }
        let path = directory.join("objects").join(format!("{}.bin", spec.key));
        let mut changed = fs::read(&path)?;
        changed[128] ^= 1;
        fs::write(path, changed)?;
        let cache = Cache::new(&directory, 512 * 1024 * 1024, 0, provider(root)?)?;
        assert!(
            format!(
                "{:#}",
                resolve(&cache, spec)
                    .err()
                    .context("expected corrupt-cache failure")?
            )
            .contains("checksum mismatch")
        );
        cache.shutdown()?;
        Ok(())
    }

    #[test]
    fn failed_index_publication_leaves_no_unaccounted_object() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path();
        let cache = Cache::new(&root.join("cache"), 512 * 1024 * 1024, 0, provider(root)?)?;
        let (spec, _) = asset(root, "unregistered", 0.2)?;
        lock(&cache.shared.state)?.fail_publish = true;
        assert!(resolve(&cache, spec.clone()).is_err());
        assert!(!cache.shared.root.join(format!("{}.bin", spec.key)).exists());
        assert_eq!(cache.snapshot()?.cache_bytes, 0);
        cache.shutdown()?;
        Ok(())
    }

    #[test]
    fn completes_ready_2x2_sibling_day_specs_before_source_pool_eviction() -> Result<()> {
        use crate::geo::Footprint;

        let temporary = tempfile::tempdir()?;
        let root = temporary.path();
        fs::create_dir_all(root.join("terrain"))?;
        fs::write(root.join("osm.sqlite"), [])?;
        let children = [
            Footprint {
                row: 25760,
                col: 18432,
                zoom: 16,
            },
            Footprint {
                row: 25760,
                col: 18448,
                zoom: 16,
            },
            Footprint {
                row: 25776,
                col: 18432,
                zoom: 16,
            },
            Footprint {
                row: 25776,
                col: 18448,
                zoom: 16,
            },
        ];
        let mut refs = Vec::new();
        for fp in children {
            let b = fp.geographic();
            let lat = (b.north + b.south) * 0.5;
            let lon = (b.east + b.west) * 0.5;
            let name = format!("{}_{}_BI{}", fp.row, fp.col, fp.zoom);
            fs::write(
                root.join("terrain").join(format!("{name}.ter")),
                format!(
                    "A\n800\nTERRAIN\nLOAD_CENTER {lat:.5} {lon:.5} 2248 4096\nBASE_TEX_NOWRAP ../textures/{name}.dds\n"
                ),
            )?;
            refs.push(format!("terrain/{name}.ter"));
        }
        let config = StreamConfig {
            source_package: root.display().to_string(),
            mount_point: root.join("reader").display().to_string(),
            osm_db: root.join("osm.sqlite").display().to_string(),
            viirs: None,
            source_revision: "test".into(),
            tile_source: crate::model::TileSource::default(),
            size: 256,
            exposure: 0.35,
            budget_mib: 512,
        };
        let scene = Arc::new(Scene::new(config.clone())?);
        scene.prepare_terrains(&refs)?;
        let provider = Arc::new(Provider::new(config, Arc::clone(&scene))?);
        provider.seed_source_for_test(
            Footprint {
                row: 12880,
                col: 9216,
                zoom: 15,
            },
            512,
            [64, 96, 128, 255],
        )?;
        let siblings = provider.ready_sibling_day_specs(children[0])?;
        assert_eq!(siblings.len(), 3);
        let first_siblings = provider.ready_sibling_day_specs(children[1])?;
        let first = first_siblings
            .into_iter()
            .find(|spec| matches!(spec.kind, Kind::Day(fp) if fp == children[0]))
            .context("missing first child spec")?;

        let cache = Cache::new(
            &root.join("cache"),
            512 * 1024 * 1024,
            0,
            Arc::clone(&provider),
        )?;
        drop(resolve(&cache, first)?);

        // Wait briefly for the worker to finish materializing the 3 siblings after waking `first`.
        for _ in 0..50 {
            if cache.snapshot()?.cached_assets == 4 && cache.snapshot()?.working == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(cache.snapshot()?.cached_assets, 4);

        // Evict the 2x2 parent from the 4-entry SourcePool.
        for row in [12896, 12912, 12928, 12944] {
            provider.seed_source_for_test(
                Footprint {
                    row,
                    col: 9216,
                    zoom: 15,
                },
                512,
                [32, 64, 96, 255],
            )?;
        }
        assert!(!provider.has_ready_source(children[0])?);

        // Requesting the 3 siblings now succeeds from disk cache without re-fetching the evicted parent.
        for sibling in siblings {
            let file = resolve(&cache, sibling)?;
            assert_eq!(file.length, dds::total_bytes(256, 8) as u64);
        }
        let stats = cache.snapshot()?;
        assert_eq!(
            (
                stats.cached_assets,
                stats.cold_builds,
                stats.cache_hits,
                stats.network_requests
            ),
            (4, 4, 3, 0)
        );
        cache.shutdown()?;
        Ok(())
    }

    #[test]
    fn priority_scheduler_orders_actual_demand_protects_worker_reserve_and_promotes_lookahead()
    -> Result<()> {
        use crate::{
            model::{FlightPhase, TelemetryState},
            streaming::telemetry::{
                ReferenceKinematics, TelemetrySnapshot, encode_rpos_packet, parse_rpos,
            },
        };
        use std::time::Instant;

        let temporary = tempfile::tempdir()?;
        let root = temporary.path();
        let cache = Cache::new(&root.join("cache"), 512 * 1024 * 1024, 0, provider(root)?)?;
        let sample = parse_rpos(
            &encode_rpos_packet(-78.7875, 35.8776, 3200.0, 3050.0, 0.0, 0.0, 0.0, -95.0, 0.0),
            Instant::now(),
        )?;
        cache.update_telemetry(TelemetrySnapshot {
            state: TelemetryState::Fresh,
            phase: FlightPhase::Cruise,
            sample: Some(sample),
            packets: 1,
            invalid_packets: 0,
        })?;

        let near_fp = Footprint {
            row: 25760,
            col: 18416,
            zoom: 16,
        };
        let far_fp = Footprint {
            row: 25664,
            col: 18416,
            zoom: 16,
        };
        let lookahead_spec = Spec {
            key: digest(b"lookahead"),
            length: 4096,
            kind: Kind::Day(near_fp),
        };
        let far_demand_spec = Spec {
            key: digest(b"far-demand"),
            length: 4096,
            kind: Kind::Day(far_fp),
        };
        let near_demand_spec = Spec {
            key: digest(b"near-demand"),
            length: 4096,
            kind: Kind::Day(near_fp),
        };

        {
            let mut state = lock(&cache.shared.state)?;
            state.kinematics = Some(ReferenceKinematics::from_sample(&sample));
            state.flights.insert(
                lookahead_spec.key.clone(),
                Flight {
                    spec: lookahead_spec.clone(),
                    waiters: Vec::new(),
                    in_progress: false,
                    dispatchable: true,
                    lookahead_rank: 1,
                    enqueued_at: 1,
                },
            );
            state.flights.insert(
                far_demand_spec.key.clone(),
                Flight {
                    spec: far_demand_spec.clone(),
                    waiters: vec![Box::new(|_| {})],
                    in_progress: false,
                    dispatchable: true,
                    lookahead_rank: 0,
                    enqueued_at: 2,
                },
            );
            state.flights.insert(
                near_demand_spec.key.clone(),
                Flight {
                    spec: near_demand_spec.clone(),
                    waiters: vec![Box::new(|_| {})],
                    in_progress: false,
                    dispatchable: true,
                    lookahead_rank: 0,
                    enqueued_at: 3,
                },
            );
            // ActualDemand outranks look-ahead, and nearer footprint (lower slack) outranks farther footprint.
            assert_eq!(
                cache.shared.select_next_flight(&state).as_deref(),
                Some(near_demand_spec.key.as_str())
            );
            state.flights.remove(&near_demand_spec.key);
            assert_eq!(
                cache.shared.select_next_flight(&state).as_deref(),
                Some(far_demand_spec.key.as_str())
            );
            state.flights.remove(&far_demand_spec.key);

            // When 3 of 4 workers are busy, the 4th worker is reserved for immediate demand, so look-ahead waits.
            state.stats.working = WORKERS - 1;
            assert_eq!(cache.shared.select_next_flight(&state), None);
        }

        // Promoting the queued look-ahead flight via an actual demand request makes it eligible for the reserved worker.
        let (tx, _rx) = mpsc::channel();
        let newly_created = cache.shared.enqueue(
            lookahead_spec.clone(),
            Box::new(move |res| {
                let _ = tx.send(res);
            }),
        )?;
        assert!(!newly_created);
        {
            let mut state = lock(&cache.shared.state)?;
            assert_eq!(state.stats.coalesced, 1);
            assert_eq!(
                cache.shared.select_next_flight(&state).as_deref(),
                Some(lookahead_spec.key.as_str())
            );
            state.stats.working = 0;
            state.flights.clear();
        }
        cache.shutdown()?;
        Ok(())
    }

    #[test]
    fn telemetry_update_cancels_obsolete_lookahead_and_evicts_behind_aircraft_first() -> Result<()>
    {
        use crate::{
            model::{FlightPhase, TelemetryState},
            streaming::telemetry::{TelemetrySnapshot, encode_rpos_packet, parse_rpos},
        };
        use std::time::Instant;

        let temporary = tempfile::tempdir()?;
        let root = temporary.path();
        let cache = Cache::new(&root.join("cache"), 512 * 1024 * 1024, 0, provider(root)?)?;
        let (ahead_asset, _) = asset(root, "ahead", 0.25)?;
        let (behind_asset, _) = asset(root, "behind", 0.55)?;
        drop(resolve(&cache, ahead_asset.clone())?);
        drop(resolve(&cache, behind_asset.clone())?);

        let north_fp = Footprint {
            row: 25664,
            col: 18416,
            zoom: 16,
        };
        let south_fp = Footprint {
            row: 25920,
            col: 18416,
            zoom: 16,
        };
        {
            let mut state = lock(&cache.shared.state)?;
            // Mark `ahead_asset` as older in LRU clock, `behind_asset` as newer in LRU clock.
            state
                .entries
                .get_mut(&ahead_asset.key)
                .context("ahead entry")?
                .used = 10;
            state
                .entries
                .get_mut(&behind_asset.key)
                .context("behind entry")?
                .used = 999;
            state
                .footprint_by_key
                .insert(ahead_asset.key.clone(), north_fp);
            state
                .footprint_by_key
                .insert(behind_asset.key.clone(), south_fp);
            // Queue a look-ahead flight far to the south (behind northbound aircraft).
            state.flights.insert(
                digest(b"south-lookahead"),
                Flight {
                    spec: Spec {
                        key: digest(b"south-lookahead"),
                        length: 4096,
                        kind: Kind::Day(south_fp),
                    },
                    waiters: Vec::new(),
                    in_progress: false,
                    dispatchable: false,
                    lookahead_rank: 3,
                    enqueued_at: 50,
                },
            );
        }

        // Update telemetry: northbound cruise from RDU.
        let sample = parse_rpos(
            &encode_rpos_packet(
                -78.7875, 35.8776, 3500.0, 3350.0, 0.0, 0.0, 0.0, -110.0, 0.0,
            ),
            Instant::now(),
        )?;
        cache.update_telemetry(TelemetrySnapshot {
            state: TelemetryState::Fresh,
            phase: FlightPhase::Cruise,
            sample: Some(sample),
            packets: 1,
            invalid_packets: 0,
        })?;
        assert_eq!(cache.snapshot()?.lookahead_cancelled, 1);

        // Constrain budget so `reserve` must evict exactly one of `ahead_asset` or `behind_asset`.
        {
            let mut state = lock(&cache.shared.state)?;
            let one_asset_bytes = state.entries[&ahead_asset.key].allocated;
            state.stats.budget_bytes =
                MAX_METADATA + SOURCE_RESERVATION + WORK_RESERVATION + one_asset_bytes;
        }
        cache.shared.reserve("incoming-key")?;
        {
            let mut state = lock(&cache.shared.state)?;
            state.stats.reserved_bytes -= WORK_RESERVATION;
            // Even though `behind_asset` had a newer LRU timestamp (999 > 10), it is behind the aircraft and evicted first!
            assert_eq!(state.entries[&behind_asset.key].allocated, 0);
            assert!(state.entries[&ahead_asset.key].allocated > 0);
        }
        cache.shutdown()?;
        Ok(())
    }

    #[test]
    fn prunes_evicted_ledger_rows_on_restart_and_when_full() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path();
        let directory = root.join("cache");
        let (a, _) = asset(root, "a", 0.2)?;
        let (b, _) = asset(root, "b", 0.3)?;
        {
            let cache = Cache::new(&directory, 512 * 1024 * 1024, 0, provider(root)?)?;
            drop(resolve(&cache, a.clone())?);
            lock(&cache.shared.state)?.stats.budget_bytes =
                MAX_METADATA + SOURCE_RESERVATION + WORK_RESERVATION;
            drop(resolve(&cache, b.clone())?);
            assert_eq!(cache.snapshot()?.evictions, 1);
            cache.shutdown()?;
        }
        let cache = Cache::new(&directory, 512 * 1024 * 1024, 0, provider(root)?)?;
        {
            let mut state = lock(&cache.shared.state)?;
            assert_eq!(state.entries.len(), 1);
            assert!(state.entries.contains_key(&b.key));
            assert!(!state.entries.contains_key(&a.key));
            state.entries.insert(
                a.key.clone(),
                Entry {
                    length: a.length,
                    sha256: "0".repeat(64),
                    allocated: 0,
                    used: 1,
                    verified: None,
                    pinned: Weak::new(),
                },
            );
            state.prune_unallocated_ledger_if_needed(&b.key)?;
            assert_eq!(state.entries.len(), 2);
        }
        cache.shutdown()?;
        Ok(())
    }
}
