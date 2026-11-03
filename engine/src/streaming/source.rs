use super::{filesystem::Scene, lock};
use crate::{
    dds,
    geo::Footprint,
    lighting::Lighting,
    model::{FxHashMap, StreamConfig},
    raster::bounded_read,
    scenery,
};
use anyhow::{Context, Result, ensure};
use gdal::{
    Dataset,
    raster::{ColorInterpretation, GdalDataType},
};
#[cfg(test)]
use reqwest::Url;
use reqwest::{blocking::Client, redirect::Policy};
#[cfg(test)]
use serde::Deserialize;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    fs::{self, File},
    io::{BufReader, Read, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

const TILE_SIZE: u32 = 256;
const MAX_TILE_BYTES: u64 = 4 * 1024 * 1024;
const TILE_FETCH_CONCURRENCY: usize = 16;
const GLOBAL_HTTP_CONCURRENCY: usize = 48;
const HTTP_CLIENT_SHARDS: usize = 8;
const ARCGIS_MISSING_ETAG: &str = "\"vvvvvvvvvvvvf\"";
const ARCGIS_MISSING_SHA256: &str =
    "9eafd300d61393184a4abc1d458564cfd1cd9b6f9c4e9c74687045c0a0e5b858";
const BING_MISSING_SHA256: &str =
    "45d35034a62d3044e4a1aa5fb4ce3fe5abfccecec9fbddffce6fceafbd362f0e";
pub const MAX_RAW: u64 = 64 * 1024 * 1024;
pub const SOURCE_RESERVATION: u64 = 64 * 1024 * 1024;
const MAX_FETCH_ATTEMPTS: usize = 3;
const MAX_ANCESTOR_FALLBACK: u32 = 4;
const ALL_QUADRANTS: u8 = 0b1111;
static GDAL_LOCK: Mutex<()> = Mutex::new(());

struct HttpLimiter {
    active: Mutex<usize>,
    available: Condvar,
}

struct HttpPermit<'a> {
    limiter: &'a HttpLimiter,
}

impl HttpLimiter {
    fn acquire(&self) -> Result<HttpPermit<'_>> {
        let mut active = lock(&self.active)?;
        while *active >= GLOBAL_HTTP_CONCURRENCY {
            active = self
                .available
                .wait(active)
                .map_err(|_| anyhow::anyhow!("HTTP limiter mutex was poisoned"))?;
        }
        *active += 1;
        Ok(HttpPermit { limiter: self })
    }
}

impl Drop for HttpPermit<'_> {
    fn drop(&mut self) {
        if let Ok(mut active) = self.limiter.active.lock() {
            *active = active.saturating_sub(1);
            self.limiter.available.notify_one();
        }
    }
}

fn http_limiter() -> &'static HttpLimiter {
    static LIMITER: OnceLock<HttpLimiter> = OnceLock::new();
    LIMITER.get_or_init(|| HttpLimiter {
        active: Mutex::new(0),
        available: Condvar::new(),
    })
}

fn tile_stage_dir(fallback: &Path) -> &Path {
    static SHM_AVAILABLE: OnceLock<bool> = OnceLock::new();
    let use_shm = *SHM_AVAILABLE.get_or_init(|| {
        let shm = Path::new("/dev/shm");
        shm.is_dir()
            && tempfile::Builder::new()
                .prefix("ge-probe-")
                .tempfile_in(shm)
                .is_ok()
    });
    if use_shm {
        Path::new("/dev/shm")
    } else {
        fallback
    }
}

enum TileFetchError {
    MissingAtZoom(anyhow::Error),
    Transient(anyhow::Error),
    Fatal(anyhow::Error),
}

fn upsample_ancestor_tile(
    ancestor: &[[u8; 4]],
    delta: u32,
    child_row: u32,
    child_col: u32,
) -> Vec<[u8; 4]> {
    if delta == 0 {
        return ancestor.to_vec();
    }
    let n = TILE_SIZE as usize;
    let scale = 1_u32 << delta;
    let sub_row = child_row & (scale - 1);
    let sub_col = child_col & (scale - 1);
    let scale_f = scale as f32;
    let max_coord = (TILE_SIZE - 1) as f32;
    let mut out = vec![[0_u8, 0_u8, 0_u8, 255_u8]; n * n];
    for py in 0..TILE_SIZE {
        let gy = (((sub_row * TILE_SIZE + py) as f32 + 0.5) / scale_f - 0.5).clamp(0.0, max_coord);
        let y0 = gy.floor() as usize;
        let y1 = (y0 + 1).min(n - 1);
        let fy = gy - y0 as f32;
        for px in 0..TILE_SIZE {
            let gx =
                (((sub_col * TILE_SIZE + px) as f32 + 0.5) / scale_f - 0.5).clamp(0.0, max_coord);
            let x0 = gx.floor() as usize;
            let x1 = (x0 + 1).min(n - 1);
            let fx = gx - x0 as f32;
            let p00 = ancestor[y0 * n + x0];
            let p10 = ancestor[y0 * n + x1];
            let p01 = ancestor[y1 * n + x0];
            let p11 = ancestor[y1 * n + x1];
            let mut pixel = [0_u8; 4];
            for c in 0..4 {
                let top = p00[c] as f32 * (1.0 - fx) + p10[c] as f32 * fx;
                let bot = p01[c] as f32 * (1.0 - fx) + p11[c] as f32 * fx;
                pixel[c] = (top * (1.0 - fy) + bot * fy).round().clamp(0.0, 255.0) as u8;
            }
            out[(py as usize) * n + (px as usize)] = pixel;
        }
    }
    out
}

fn composite_under(dst: &mut [[u8; 4]], under: &[[u8; 4]]) {
    for (fg, bg) in dst.iter_mut().zip(under.iter().copied()) {
        if fg[3] == 255 || bg[3] == 0 {
            continue;
        }
        if fg[3] == 0 {
            *fg = bg;
            continue;
        }
        let fa = fg[3] as f32 / 255.0;
        let ba = (bg[3] as f32 / 255.0) * (1.0 - fa);
        let out_a = fa + ba;
        if out_a > 0.0 {
            for c in 0..3 {
                fg[c] = ((fg[c] as f32 * fa + bg[c] as f32 * ba) / out_a)
                    .round()
                    .clamp(0.0, 255.0) as u8;
            }
            fg[3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
}

struct Mask {
    size: u32,
    pixels: Vec<u8>,
}

impl Mask {
    fn allows_uncovered(&self, col: u32, row: u32, crop_size: u32) -> bool {
        if self.size == 0 || crop_size == 0 {
            return false;
        }
        let x0 = ((col as u64 * self.size as u64) / crop_size as u64) as u32;
        let x1 = ((col as u64 + 1) * self.size as u64)
            .div_ceil(crop_size as u64)
            .max(x0 as u64 + 1)
            .min(self.size as u64) as u32;
        let y0 = ((row as u64 * self.size as u64) / crop_size as u64) as u32;
        let y1 = ((row as u64 + 1) * self.size as u64)
            .div_ceil(crop_size as u64)
            .max(y0 as u64 + 1)
            .min(self.size as u64) as u32;
        for my in y0.saturating_sub(1)..=y1.min(self.size - 1) {
            for mx in x0.saturating_sub(1)..=x1.min(self.size - 1) {
                if self.pixels[(my * self.size + mx) as usize] != 0 {
                    return false;
                }
            }
        }
        true
    }
}

struct Raster {
    size: u32,
    quadrants: u8,
    pixels: Vec<[u8; 4]>,
}

impl Raster {
    fn has_crop(&self, x: u32, y: u32, size: u32) -> bool {
        if size > 0
            && size * 2 == self.size
            && x < self.size
            && y < self.size
            && x.is_multiple_of(size)
            && y.is_multiple_of(size)
        {
            let qx = x / size;
            let qy = y / size;
            (self.quadrants & (1_u8 << (qy * 2 + qx))) != 0
        } else {
            self.quadrants == ALL_QUADRANTS
        }
    }

    fn crop(&self, x: u32, y: u32, size: u32) -> Result<Vec<[f32; 3]>> {
        self.crop_masked(x, y, size, None)
    }

    fn crop_masked(&self, x: u32, y: u32, size: u32, mask: Option<&Mask>) -> Result<Vec<[f32; 3]>> {
        ensure!(
            size > 0
                && x.checked_add(size).is_some_and(|end| end <= self.size)
                && y.checked_add(size).is_some_and(|end| end <= self.size)
                && self.has_crop(x, y, size),
            "imagery crop is outside its qualified source"
        );
        let mut result = Vec::with_capacity((size * size) as usize);
        let mut covered = 0_usize;
        for row in 0..size {
            for col in 0..size {
                let pixel = self.pixels[((y + row) * self.size + (x + col)) as usize];
                if pixel[3] == 255 {
                    covered += 1;
                    result.push([
                        pixel[0] as f32 / 255.0,
                        pixel[1] as f32 / 255.0,
                        pixel[2] as f32 / 255.0,
                    ]);
                } else {
                    ensure!(
                        mask.is_some_and(|m| m.allows_uncovered(col, row, size)),
                        "USGS imagery contains uncovered pixels"
                    );
                    result.push([0.0, 0.0, 0.0]);
                }
            }
        }
        ensure!(covered > 0, "USGS imagery contains uncovered pixels");
        Ok(result)
    }
}

#[derive(Default)]
struct Images {
    ready: VecDeque<((Footprint, u32), Arc<Raster>)>,
    working: FxHashMap<(Footprint, u32), Arc<Acquisition>>,
}

#[derive(Default)]
struct Acquisition {
    result: Mutex<Option<std::result::Result<Arc<Raster>, String>>>,
    changed: Condvar,
}

#[derive(Default)]
struct SourcePool {
    images: Mutex<Images>,
}

impl SourcePool {
    fn get(
        &self,
        footprint: Footprint,
        size: u32,
        required_quadrants: u8,
        load: impl FnOnce(u8) -> Result<Raster>,
    ) -> Result<Arc<Raster>> {
        let key = (footprint, size);
        let mut prior_quadrants = 0_u8;
        let mut images = lock(&self.images)?;
        if let Some(index) = images
            .ready
            .iter()
            .position(|(candidate, _)| *candidate == key)
        {
            let entry = images
                .ready
                .remove(index)
                .context("source pool entry disappeared")?;
            if (entry.1.quadrants & required_quadrants) == required_quadrants {
                let raster = Arc::clone(&entry.1);
                images.ready.push_back(entry);
                return Ok(raster);
            }
            prior_quadrants = entry.1.quadrants;
        }
        if let Some(acquisition) = images.working.get(&key).cloned() {
            drop(images);
            let mut result = lock(&acquisition.result)?;
            while result.is_none() {
                result = acquisition
                    .changed
                    .wait(result)
                    .map_err(|_| anyhow::anyhow!("source acquisition mutex was poisoned"))?;
            }
            let completed = match result
                .as_ref()
                .context("missing completed source acquisition")?
            {
                Ok(image) => Arc::clone(image),
                Err(error) => return Err(anyhow::anyhow!("{error}")),
            };
            if (completed.quadrants & required_quadrants) == required_quadrants {
                return Ok(completed);
            }
            images = lock(&self.images)?;
            prior_quadrants |= completed.quadrants;
            if let Some(index) = images
                .ready
                .iter()
                .position(|(candidate, _)| *candidate == key)
            {
                let entry = images
                    .ready
                    .remove(index)
                    .context("source pool entry disappeared")?;
                if (entry.1.quadrants & required_quadrants) == required_quadrants {
                    let raster = Arc::clone(&entry.1);
                    images.ready.push_back(entry);
                    return Ok(raster);
                }
                prior_quadrants |= entry.1.quadrants;
            }
        }
        ensure!(
            images.working.len() < super::WORKERS,
            "bounded source acquisition limit reached"
        );
        let acquisition = Arc::new(Acquisition::default());
        images.working.insert(key, Arc::clone(&acquisition));
        drop(images);
        let result = load(prior_quadrants | required_quadrants).and_then(|image| {
            ensure!(
                image.size > 0
                    && image.size <= 2048
                    && image.size == size
                    && (image.quadrants & required_quadrants) == required_quadrants
                    && image.pixels.len() == (image.size * image.size) as usize,
                "source raster exceeds the reserved layout"
            );
            Ok(Arc::new(image))
        });
        let mut images = lock(&self.images)?;
        images.working.remove(&key);
        if let Ok(image) = &result {
            if let Some(index) = images
                .ready
                .iter()
                .position(|(candidate, _)| *candidate == key)
            {
                images.ready.remove(index);
            } else if images.ready.len() == 4 {
                images.ready.pop_front();
            }
            images.ready.push_back((key, Arc::clone(image)));
        }
        *lock(&acquisition.result)? = Some(match &result {
            Ok(image) => Ok(Arc::clone(image)),
            Err(error) => Err(format!("{error:#}").chars().take(2048).collect()),
        });
        acquisition.changed.notify_all();
        result
    }

    fn is_ready(&self, footprint: Footprint, size: u32, required_quadrants: u8) -> Result<bool> {
        let key = (footprint, size);
        Ok(lock(&self.images)?.ready.iter().any(|(candidate, raster)| {
            *candidate == key && (raster.quadrants & required_quadrants) == required_quadrants
        }))
    }
}

pub fn source_window(key: Footprint, size: u32) -> (Footprint, u32, u32, u32) {
    if key.zoom > 4 && size <= 1024 {
        let parent = Footprint {
            row: key.row / 32 * 16,
            col: key.col / 32 * 16,
            zoom: key.zoom - 1,
        };
        if in_test_envelope(parent) {
            return (
                parent,
                size * 2,
                key.col % 32 / 16 * size,
                key.row % 32 / 16 * size,
            );
        }
    }
    (key, size, 0, 0)
}

fn quadrant_bit(x: u32, y: u32, size: u32, source_size: u32) -> u8 {
    if size > 0
        && size * 2 == source_size
        && x < source_size
        && y < source_size
        && x.is_multiple_of(size)
        && y.is_multiple_of(size)
    {
        let qx = x / size;
        let qy = y / size;
        1_u8 << (qy * 2 + qx)
    } else {
        ALL_QUADRANTS
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Stamp {
    device: u64,
    inode: u64,
    pub length: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl Stamp {
    pub fn read(path: &Path) -> Result<Self> {
        let m = fs::metadata(path).with_context(|| format!("examining {}", path.display()))?;
        ensure!(
            m.is_file(),
            "source is not a regular file: {}",
            path.display()
        );
        Ok(Self {
            device: m.dev(),
            inode: m.ino(),
            length: m.len(),
            modified: (m.mtime(), m.mtime_nsec()),
            changed: (m.ctime(), m.ctime_nsec()),
        })
    }

    pub fn check(&self, path: &Path) -> Result<()> {
        ensure!(
            *self == Self::read(path)?,
            "borrowed source changed: {}",
            path.display()
        );
        Ok(())
    }

    pub fn read_source(path: &Path) -> Result<Self> {
        let m = fs::metadata(path).with_context(|| format!("examining {}", path.display()))?;
        ensure!(
            m.is_file() || m.is_dir(),
            "source is not a regular file or directory: {}",
            path.display()
        );
        if m.is_dir() {
            Ok(Self {
                device: m.dev(),
                inode: m.ino(),
                length: 0,
                modified: (0, 0),
                changed: (0, 0),
            })
        } else {
            Ok(Self {
                device: m.dev(),
                inode: m.ino(),
                length: m.len(),
                modified: (m.mtime(), m.mtime_nsec()),
                changed: (m.ctime(), m.ctime_nsec()),
            })
        }
    }

    pub fn check_source(&self, path: &Path) -> Result<()> {
        ensure!(
            *self == Self::read_source(path)?,
            "borrowed source changed: {}",
            path.display()
        );
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub enum Kind {
    Day(Footprint),
    Night(Footprint),
    Raw {
        path: PathBuf,
        stamp: Stamp,
        dsf: bool,
    },
}

#[derive(Clone, Debug)]
pub struct Spec {
    pub key: String,
    pub length: u64,
    pub kind: Kind,
}

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn in_test_envelope(key: Footprint) -> bool {
    let b = key.geographic();
    b.west >= -126.0 && b.east <= -66.0 && b.south >= 24.0 && b.north <= 51.0
}

pub fn in_imagery_envelope(key: Footprint) -> bool {
    let b = key.geographic();
    b.west >= -180.0 && b.east <= 180.0 && b.south >= -85.06 && b.north <= 85.06
}

#[cfg(test)]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SpatialReference {
    wkid: Option<u32>,
    latest_wkid: Option<u32>,
}

#[cfg(test)]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Extent {
    xmin: f64,
    ymin: f64,
    xmax: f64,
    ymax: f64,
    spatial_reference: SpatialReference,
}

#[cfg(test)]
#[derive(Deserialize)]
struct Export {
    href: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    extent: Option<Extent>,
    error: Option<serde_json::Value>,
}

#[cfg(test)]
impl Export {
    fn validate(&self, key: Footprint, size: u32) -> Result<Url> {
        ensure!(self.error.is_none(), "USGS export failed: {:?}", self.error);
        ensure!(
            self.width == Some(size) && self.height == Some(size),
            "USGS returned wrong image dimensions"
        );
        let e = self.extent.as_ref().context("USGS response lacks extent")?;
        let sr = &e.spatial_reference;
        ensure!(
            sr.wkid.is_none_or(|n| matches!(n, 3857 | 102100))
                && sr.latest_wkid.is_none_or(|n| matches!(n, 3857 | 102100))
                && (sr.wkid.is_some() || sr.latest_wkid.is_some()),
            "USGS returned wrong projection"
        );
        let b = key.meters();
        let tolerance = ((b.east - b.west) / size as f64 * 1e-5).max(1e-6);
        for (actual, expected) in [
            (e.xmin, b.west),
            (e.ymin, b.south),
            (e.xmax, b.east),
            (e.ymax, b.north),
        ] {
            ensure!(
                actual.is_finite() && (actual - expected).abs() <= tolerance,
                "USGS returned a shifted footprint"
            );
        }
        let url = Url::parse(
            self.href
                .as_deref()
                .context("USGS response lacks image URL")?,
        )?;
        ensure!(
            url.scheme() == "https"
                && url.host_str() == Some("basemap.nationalmap.gov")
                && url.port_or_known_default() == Some(443)
                && url.username().is_empty()
                && url.password().is_none()
                && url.path().starts_with(
                    "/arcgis/rest/directories/basemap_output/USGSImageryOnly_MapServer/"
                ),
            "USGS returned an unexpected image destination"
        );
        Ok(url)
    }
}

pub struct Provider {
    config: StreamConfig,
    clients: [Client; HTTP_CLIENT_SHARDS],
    scene: Arc<Scene>,
    osm: Stamp,
    viirs: Option<Stamp>,
    lighting: Mutex<Option<Lighting>>,
    masks: Mutex<FxHashMap<PathBuf, (Stamp, Arc<Mask>)>>,
    sources: SourcePool,
    requests: AtomicU64,
    bytes: AtomicU64,
}

impl Provider {
    pub fn new(config: StreamConfig, scene: Arc<Scene>) -> Result<Self> {
        let mut clients_vec = Vec::with_capacity(HTTP_CLIENT_SHARDS);
        for _ in 0..HTTP_CLIENT_SHARDS {
            clients_vec.push(
                Client::builder()
                    .user_agent("GroundEffect/0.1")
                    .redirect(Policy::none())
                    .tcp_nodelay(true)
                    .connect_timeout(Duration::from_secs(6))
                    .timeout(Duration::from_secs(12))
                    .build()?,
            );
        }
        let clients: [Client; HTTP_CLIENT_SHARDS] = clients_vec
            .try_into()
            .map_err(|_| anyhow::anyhow!("initializing HTTP client shards"))?;
        Ok(Self {
            osm: Stamp::read_source(Path::new(&config.osm_db))?,
            viirs: config
                .viirs
                .as_deref()
                .map(Path::new)
                .map(Stamp::read)
                .transpose()?,
            config,
            scene,
            clients,
            lighting: Mutex::new(None),
            masks: Mutex::new(FxHashMap::default()),
            sources: SourcePool::default(),
            requests: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        })
    }

    fn active_quadrants(
        &self,
        key: Footprint,
        size: u32,
        source_size: u32,
        required_quadrants: u8,
    ) -> Result<u8> {
        if size * 2 != source_size {
            return Ok(ALL_QUADRANTS);
        }
        let siblings = self.scene.sibling_day_specs(key)?;
        if siblings.is_empty() {
            return if self.scene.has_prepared_terrains()? {
                Ok(required_quadrants)
            } else {
                Ok(ALL_QUADRANTS)
            };
        }
        let mut mask = required_quadrants;
        for spec in siblings {
            if let Kind::Day(sibling) = spec.kind {
                let (_, sibling_source_size, sx, sy) = source_window(sibling, size);
                mask |= quadrant_bit(sx, sy, size, sibling_source_size);
            }
        }
        Ok(mask)
    }

    fn imagery(&self, key: Footprint, stage: &Path) -> Result<Vec<[f32; 3]>> {
        ensure!(
            in_imagery_envelope(key),
            "imagery footprint is outside the qualified continental-US test envelope"
        );
        let size = self.config.size;
        let (parent, source_size, x, y) = source_window(key, size);
        let required_quadrants = quadrant_bit(x, y, size, source_size);
        let target_quadrants = self.active_quadrants(key, size, source_size, required_quadrants)?;
        let raster = self
            .sources
            .get(parent, source_size, required_quadrants, |needed| {
                self.fetch_imagery(parent, source_size, needed | target_quadrants, stage)
            })?;
        if let Ok(pixels) = raster.crop(x, y, size) {
            return Ok(pixels);
        }
        let Some((mask_path, mask_stamp)) = self.scene.sea_mask(key)? else {
            return raster.crop(x, y, size);
        };
        let mask = self.cached_mask(&mask_path, &mask_stamp)?;
        raster.crop_masked(x, y, size, Some(&mask))
    }

    fn cached_mask(&self, path: &Path, stamp: &Stamp) -> Result<Arc<Mask>> {
        {
            let guard = lock(&self.masks)?;
            if let Some((cached_stamp, mask)) = guard.get(path)
                && cached_stamp == stamp
            {
                stamp.check(path)?;
                return Ok(Arc::clone(mask));
            }
        }
        let decoded = Arc::new(Self::decode_mask(path, stamp)?);
        let mut guard = lock(&self.masks)?;
        if guard.len() >= 256 {
            guard.clear();
        }
        guard.insert(path.to_path_buf(), (stamp.clone(), Arc::clone(&decoded)));
        Ok(decoded)
    }

    fn fetch_imagery(
        &self,
        key: Footprint,
        size: u32,
        quadrants: u8,
        stage: &Path,
    ) -> Result<Raster> {
        ensure!(
            (TILE_SIZE..=2048).contains(&size)
                && size.is_power_of_two()
                && (size / TILE_SIZE) <= 16,
            "USGS tile mosaic requires a power-of-two size between 256 and 2048"
        );
        let grid = size / TILE_SIZE;
        let scale = 16 / grid;
        let shift = scale.trailing_zeros();
        ensure!(
            key.zoom > shift && key.row.is_multiple_of(scale) && key.col.is_multiple_of(scale),
            "footprint is not aligned to the USGS Web Mercator tile pyramid"
        );
        let tile_zoom = key.zoom - shift;
        let base_row = key.row / scale;
        let base_col = key.col / scale;
        let half = (grid / 2).max(1);
        let mut jobs = Vec::with_capacity((grid * grid) as usize);
        for ty in 0..grid {
            for tx in 0..grid {
                let active = if quadrants == ALL_QUADRANTS || grid < 2 {
                    true
                } else {
                    let qx = tx / half;
                    let qy = ty / half;
                    (quadrants & (1_u8 << (qy * 2 + qx))) != 0
                };
                if active {
                    jobs.push((tx, ty, base_row + ty, base_col + tx));
                }
            }
        }
        ensure!(!jobs.is_empty(), "no active USGS tile quadrants requested");
        let worker_count = jobs.len().clamp(1, TILE_FETCH_CONCURRENCY);
        let next_job = AtomicUsize::new(0);
        let failed = AtomicBool::new(false);
        let tiles = Mutex::new(Vec::with_capacity(jobs.len()));
        let first_error: Mutex<Option<anyhow::Error>> = Mutex::new(None);
        std::thread::scope(|scope| {
            for _ in 0..worker_count {
                scope.spawn(|| {
                    while !failed.load(Ordering::Relaxed) {
                        let index = next_job.fetch_add(1, Ordering::Relaxed);
                        let Some(&(tx, ty, tile_row, tile_col)) = jobs.get(index) else {
                            break;
                        };
                        match self.fetch_tile(tile_zoom, tile_row, tile_col, stage) {
                            Ok(tile_pixels) => {
                                if let Ok(mut guard) = lock(&tiles) {
                                    guard.push((tx, ty, tile_pixels));
                                } else {
                                    failed.store(true, Ordering::Relaxed);
                                    break;
                                }
                            }
                            Err(error) => {
                                failed.store(true, Ordering::Relaxed);
                                if let Ok(mut guard) = lock(&first_error)
                                    && guard.is_none()
                                {
                                    *guard = Some(error);
                                }
                                break;
                            }
                        }
                    }
                });
            }
        });
        if let Some(error) = lock(&first_error)?.take() {
            return Err(error);
        }
        let decoded_tiles = std::mem::take(&mut *lock(&tiles)?);
        ensure!(
            decoded_tiles.len() == jobs.len(),
            "incomplete USGS tile pyramid acquisition"
        );
        let mut pixels = vec![[0_u8; 4]; (size * size) as usize];
        for (tx, ty, tile_pixels) in decoded_tiles {
            for row in 0..TILE_SIZE {
                let dst_y = (ty * TILE_SIZE + row) as usize;
                let dst_start = dst_y * size as usize + (tx * TILE_SIZE) as usize;
                let src_start = (row * TILE_SIZE) as usize;
                pixels[dst_start..dst_start + TILE_SIZE as usize]
                    .copy_from_slice(&tile_pixels[src_start..src_start + TILE_SIZE as usize]);
            }
        }
        Ok(Raster {
            size,
            quadrants,
            pixels,
        })
    }

    fn fetch_tile(&self, zoom: u32, row: u32, col: u32, stage: &Path) -> Result<Vec<[u8; 4]>> {
        let max_delta = MAX_ANCESTOR_FALLBACK.min(zoom.saturating_sub(8));
        let mut last_error = None;
        let mut composited: Option<Vec<[u8; 4]>> = None;
        for delta in 0..=max_delta {
            let cur_zoom = zoom - delta;
            let cur_row = row >> delta;
            let cur_col = col >> delta;
            let mut layer = None;
            for attempt in 1..=MAX_FETCH_ATTEMPTS {
                match self.fetch_tile_once(cur_zoom, cur_row, cur_col, stage) {
                    Ok(pixels) => {
                        layer = Some(upsample_ancestor_tile(&pixels, delta, row, col));
                        break;
                    }
                    Err(TileFetchError::MissingAtZoom(error)) => {
                        last_error = Some(error);
                        break;
                    }
                    Err(TileFetchError::Transient(error)) if attempt < MAX_FETCH_ATTEMPTS => {
                        last_error = Some(error);
                        std::thread::sleep(Duration::from_millis(120 * attempt as u64));
                    }
                    Err(TileFetchError::Transient(error)) if delta < max_delta => {
                        last_error = Some(error);
                        break;
                    }
                    Err(TileFetchError::Transient(error) | TileFetchError::Fatal(error)) => {
                        if let Some(existing) = composited {
                            return Ok(existing);
                        }
                        return match last_error {
                            Some(prior) => Err(error.context(format!(
                                "after retry following transient source error: {prior:#}"
                            ))),
                            None => Err(error),
                        };
                    }
                }
            }
            if let Some(layer_pixels) = layer {
                match &mut composited {
                    None => {
                        if layer_pixels.iter().all(|p| p[3] == 255) {
                            return Ok(layer_pixels);
                        }
                        composited = Some(layer_pixels);
                    }
                    Some(dst) => {
                        composite_under(dst, &layer_pixels);
                        if dst.iter().all(|p| p[3] == 255) {
                            return Ok(std::mem::take(dst));
                        }
                    }
                }
            }
        }
        if let Some(existing) = composited {
            return Ok(existing);
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("USGS tile acquisition failed")))
    }

    fn fetch_tile_once(
        &self,
        zoom: u32,
        row: u32,
        col: u32,
        stage: &Path,
    ) -> std::result::Result<Vec<[u8; 4]>, TileFetchError> {
        let url = self.config.tile_source.tile_url(zoom, row, col);
        let shard =
            ((zoom ^ row.rotate_left(5) ^ col.rotate_left(11)) as usize) % HTTP_CLIENT_SHARDS;
        let response = {
            let _permit = http_limiter()
                .acquire()
                .map_err(TileFetchError::Transient)?;
            self.requests.fetch_add(1, Ordering::Relaxed);
            let resp = self.clients[shard]
                .get(&url)
                .send()
                .with_context(|| format!("requesting imagery tile {zoom}/{row}/{col}"))
                .map_err(TileFetchError::Transient)?;
            let status = resp.status();
            if status == reqwest::StatusCode::NOT_FOUND || status.is_redirection() {
                return Err(TileFetchError::MissingAtZoom(anyhow::anyhow!(
                    "imagery tile {zoom}/{row}/{col} unavailable at zoom {zoom} (HTTP {status})"
                )));
            }
            if status != reqwest::StatusCode::OK {
                return Err(TileFetchError::Transient(anyhow::anyhow!(
                    "imagery tile {zoom}/{row}/{col} returned HTTP {status}"
                )));
            }
            if resp
                .headers()
                .get(reqwest::header::ETAG)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|etag| etag == ARCGIS_MISSING_ETAG)
                || resp
                    .headers()
                    .get("x-ve-tile-info")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|info| info.contains("no-tile"))
            {
                return Err(TileFetchError::MissingAtZoom(anyhow::anyhow!(
                    "imagery tile {zoom}/{row}/{col} returned no-data placeholder at zoom {zoom}"
                )));
            }
            let expected_length = resp.content_length();
            if expected_length.is_some_and(|n| n > MAX_TILE_BYTES) {
                return Err(TileFetchError::Fatal(anyhow::anyhow!(
                    "imagery tile exceeds byte limit"
                )));
            }
            let mut bytes = Vec::with_capacity(expected_length.unwrap_or(24 * 1024) as usize);
            resp.take(MAX_TILE_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| TileFetchError::Transient(e.into()))?;
            (bytes, expected_length)
        };
        let (bytes, expected_length) = response;
        self.bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        if bytes.len() as u64 > MAX_TILE_BYTES {
            return Err(TileFetchError::Fatal(anyhow::anyhow!(
                "imagery tile exceeds byte limit"
            )));
        }
        if let Some(expected) = expected_length
            && bytes.len() as u64 != expected
        {
            return Err(TileFetchError::Transient(anyhow::anyhow!(
                "imagery tile transfer was truncated ({} of {expected} bytes)",
                bytes.len()
            )));
        }
        if (bytes.len() == 2521 && digest(&bytes) == ARCGIS_MISSING_SHA256)
            || (bytes.len() == 1033 && digest(&bytes) == BING_MISSING_SHA256)
        {
            return Err(TileFetchError::MissingAtZoom(anyhow::anyhow!(
                "imagery tile {zoom}/{row}/{col} returned no-data placeholder at zoom {zoom}"
            )));
        }
        let suffix = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            ".png"
        } else if bytes.starts_with(b"\xff\xd8") {
            ".jpg"
        } else {
            ".tmp"
        };
        let mut file = tempfile::Builder::new()
            .prefix("tile-")
            .suffix(suffix)
            .tempfile_in(tile_stage_dir(stage))
            .map_err(|e| TileFetchError::Fatal(e.into()))?;
        file.write_all(&bytes)
            .map_err(|e| TileFetchError::Fatal(e.into()))?;
        drop(bytes);
        file.flush().map_err(|e| TileFetchError::Fatal(e.into()))?;
        Self::decode_tile_checked(file.path()).map_err(|(transient, e)| {
            if transient {
                TileFetchError::Transient(e)
            } else {
                TileFetchError::Fatal(e)
            }
        })
    }

    fn decode_tile_checked(
        path: &Path,
    ) -> std::result::Result<Vec<[u8; 4]>, (bool, anyhow::Error)> {
        let n = TILE_SIZE as usize;
        let mut bands = Vec::with_capacity(4);
        {
            let _gdal = lock(&GDAL_LOCK).map_err(|e| (false, e))?;
            let dataset = Dataset::open(path)
                .context("decoding imagery tile")
                .map_err(|e| (true, e))?;
            let driver = dataset.driver().short_name();
            let count = dataset.raster_count();
            if dataset.raster_size() != (n, n)
                || !((driver == "JPEG" && count == 3)
                    || (driver == "PNG" && (count == 3 || count == 4)))
            {
                return Err((
                    false,
                    anyhow::anyhow!(
                        "imagery tile must be exact-size 256x256 8-bit RGB JPEG/PNG or RGBA PNG"
                    ),
                ));
            }
            let channels = [
                ColorInterpretation::RedBand,
                ColorInterpretation::GreenBand,
                ColorInterpretation::BlueBand,
                ColorInterpretation::AlphaBand,
            ];
            for (channel, interpretation) in channels.into_iter().take(count).enumerate() {
                let band = dataset
                    .rasterband(channel + 1)
                    .map_err(|e| (true, e.into()))?;
                if band.band_type() != GdalDataType::UInt8
                    || band.color_interpretation() != interpretation
                {
                    return Err((
                        false,
                        anyhow::anyhow!(
                            "imagery tile bands are not explicitly tagged 8-bit RGB/RGBA"
                        ),
                    ));
                }
                let values = band
                    .read_as::<u8>((0, 0), (n, n), (n, n), None)
                    .map_err(|e| (true, e.into()))?;
                bands.push(values);
            }
        }
        let mut pixels = vec![[0_u8, 0_u8, 0_u8, 255_u8]; n * n];
        for (channel, values) in bands.iter().enumerate() {
            for (pixel, value) in pixels.iter_mut().zip(values.data()) {
                pixel[channel] = *value;
            }
        }
        Ok(pixels)
    }

    #[cfg(test)]
    fn decode_png(path: &Path, size: u32) -> Result<Raster> {
        Self::decode_png_checked(path, size).map_err(|(_, e)| e)
    }

    #[cfg(test)]
    fn decode_png_checked(
        path: &Path,
        size: u32,
    ) -> std::result::Result<Raster, (bool, anyhow::Error)> {
        let _gdal = lock(&GDAL_LOCK).map_err(|e| (false, e))?;
        let dataset = Dataset::open(path)
            .context("decoding USGS PNG")
            .map_err(|e| (true, e))?;
        let n = size as usize;
        if dataset.driver().short_name() != "PNG"
            || dataset.raster_size() != (n, n)
            || dataset.raster_count() != 4
        {
            return Err((
                false,
                anyhow::anyhow!("USGS image must be exact-size 8-bit RGBA PNG"),
            ));
        }
        let mut pixels = vec![[0; 4]; n * n];
        for (channel, interpretation) in [
            ColorInterpretation::RedBand,
            ColorInterpretation::GreenBand,
            ColorInterpretation::BlueBand,
            ColorInterpretation::AlphaBand,
        ]
        .into_iter()
        .enumerate()
        {
            let band = dataset
                .rasterband(channel + 1)
                .map_err(|e| (true, e.into()))?;
            if band.band_type() != GdalDataType::UInt8
                || band.color_interpretation() != interpretation
            {
                return Err((
                    false,
                    anyhow::anyhow!("USGS PNG bands are not explicitly tagged 8-bit RGBA"),
                ));
            }
            let values = band
                .read_as::<u8>((0, 0), (n, n), (n, n), None)
                .map_err(|e| (true, e.into()))?;
            for (pixel, value) in pixels.iter_mut().zip(values.data()) {
                pixel[channel] = *value;
            }
        }
        Ok(Raster {
            size,
            quadrants: ALL_QUADRANTS,
            pixels,
        })
    }

    fn decode_mask(path: &Path, stamp: &Stamp) -> Result<Mask> {
        stamp.check(path)?;
        let _gdal = lock(&GDAL_LOCK)?;
        let dataset = Dataset::open(path)
            .with_context(|| format!("decoding coastline mask {}", path.display()))?;
        let (width, height) = dataset.raster_size();
        ensure!(
            dataset.driver().short_name() == "PNG"
                && width == height
                && matches!(
                    width,
                    4 | 8 | 16 | 32 | 64 | 128 | 256 | 512 | 1024 | 2048 | 4096
                )
                && dataset.raster_count() == 1,
            "coastline mask must be a square 8-bit grayscale PNG: {}",
            path.display()
        );
        let band = dataset.rasterband(1)?;
        ensure!(
            band.band_type() == GdalDataType::UInt8
                && band.color_interpretation() == ColorInterpretation::GrayIndex,
            "coastline mask band is not 8-bit grayscale: {}",
            path.display()
        );
        let values = band.read_as::<u8>((0, 0), (width, height), (width, height), None)?;
        stamp.check(path)?;
        Ok(Mask {
            size: width as u32,
            pixels: values.data().to_vec(),
        })
    }

    pub fn network(&self) -> (u64, u64) {
        (
            self.requests.load(Ordering::Relaxed),
            self.bytes.load(Ordering::Relaxed),
        )
    }

    pub fn has_ready_source(&self, key: Footprint) -> Result<bool> {
        let size = self.config.size;
        let (parent, source_size, x, y) = source_window(key, size);
        let required = quadrant_bit(x, y, size, source_size);
        self.sources.is_ready(parent, source_size, required)
    }

    pub fn ready_sibling_day_specs(&self, key: Footprint) -> Result<Vec<Spec>> {
        if !self.has_ready_source(key)? {
            return Ok(Vec::new());
        }
        let mut ready = Vec::new();
        for spec in self.scene.sibling_day_specs(key)? {
            if let Kind::Day(footprint) = spec.kind
                && self.has_ready_source(footprint)?
            {
                ready.push(spec);
            }
        }
        Ok(ready)
    }

    pub fn prepared_specs(&self) -> Result<Vec<(Footprint, Spec, Spec)>> {
        self.scene.prepared_specs()
    }

    pub fn request_dsf_cell(&self, lat_floor: i32, lon_floor: i32) -> Result<bool> {
        self.scene.request_dsf_cell(lat_floor, lon_floor)
    }

    #[cfg(test)]
    pub fn seed_source_for_test(
        &self,
        footprint: Footprint,
        size: u32,
        color: [u8; 4],
    ) -> Result<()> {
        self.sources.get(footprint, size, ALL_QUADRANTS, |_| {
            Ok(Raster {
                size,
                quadrants: ALL_QUADRANTS,
                pixels: vec![color; (size * size) as usize],
            })
        })?;
        Ok(())
    }

    pub fn borrowed_mesh(&self, spec: &Spec) -> Result<Option<File>> {
        let Kind::Raw {
            path,
            stamp,
            dsf: true,
        } = &spec.kind
        else {
            return Ok(None);
        };
        (|| -> Result<Option<File>> {
            stamp.check(path)?;
            let mut file = File::open(path)?;
            let terrains = scenery::read_dsf_terrains(
                &mut BufReader::with_capacity(65536, &mut file),
                stamp.length,
            )?;
            self.scene
                .prepare_dsf_terrains(Scene::dsf_cell(path), &terrains)?;
            stamp.check(path)?;
            Ok(Some(file))
        })()
        .with_context(|| format!("validating borrowed mesh {}", path.display()))
    }

    fn day_rgb_for_night(&self, key: Footprint, stage: &Path) -> Option<Vec<[f32; 3]>> {
        if self.has_ready_source(key).unwrap_or(false)
            && let Ok(pixels) = self.imagery(key, stage)
        {
            return Some(pixels);
        }
        if let Ok(Some(day_key)) = self.scene.day_key(key)
            && let Some(objects_dir) = stage.parent().map(|p| p.join("objects"))
        {
            let cached_day = objects_dir.join(format!("{day_key}.bin"));
            if let Ok(bytes) = bounded_read(&cached_day, MAX_RAW)
                && let Ok(pixels) = dds::decode_rgb_level0(&bytes)
                && pixels.len() == (self.config.size * self.config.size) as usize
            {
                return Some(pixels);
            }
        }
        if self.config.source_revision != "process-test" {
            self.imagery(key, stage).ok()
        } else {
            None
        }
    }

    pub fn build(&self, spec: &Spec, stage: &Path) -> Result<Vec<u8>> {
        match &spec.kind {
            Kind::Day(key) => dds::encode_rgb(
                self.config.size,
                self.imagery(*key, stage).with_context(|| {
                    format!(
                        "{} imagery row {} column {} zoom {} at {} pixels",
                        self.config.tile_source.label(),
                        key.row,
                        key.col,
                        key.zoom,
                        self.config.size
                    )
                })?,
                false,
            ),
            Kind::Night(key) => {
                self.osm.check_source(Path::new(&self.config.osm_db))?;
                if let (Some(stamp), Some(path)) = (&self.viirs, &self.config.viirs) {
                    stamp.check(Path::new(path))?;
                }
                let mut renderer = {
                    let mut guard = lock(&self.lighting)?;
                    match guard.as_ref() {
                        Some(renderer) => renderer.clone(),
                        None => {
                            let opened = Lighting::open(
                                Path::new(&self.config.osm_db),
                                self.config.viirs.as_deref().map(Path::new),
                            )?;
                            *guard = Some(opened.clone());
                            opened
                        }
                    }
                };
                let day_rgb = self.day_rgb_for_night(*key, stage);
                let pixels = renderer.render_modulated(
                    *key,
                    self.config.size,
                    self.config.exposure,
                    day_rgb.as_deref(),
                )?;
                self.osm.check_source(Path::new(&self.config.osm_db))?;
                if let (Some(stamp), Some(path)) = (&self.viirs, &self.config.viirs) {
                    stamp.check(Path::new(path))?;
                }
                dds::encode_rgb(self.config.size, pixels, true)
            }
            Kind::Raw { path, stamp, .. } => {
                stamp.check(path)?;
                let bytes = bounded_read(path, MAX_RAW)?;
                stamp.check(path)?;
                Ok(bytes)
            }
        }
    }

    pub fn validate(&self, spec: &Spec, bytes: &[u8], payload: &Path) -> Result<()> {
        ensure!(bytes.len() as u64 == spec.length, "asset length changed");
        match &spec.kind {
            Kind::Day(_) | Kind::Night(_) => {
                ensure!(
                    dds::validate(bytes)? == (self.config.size, 8),
                    "unexpected DDS layout"
                );
            }
            Kind::Raw { dsf: true, .. } => {
                anyhow::bail!("borrowed meshes require verified source-descriptor delivery")
            }
            Kind::Raw { path, .. } if path.extension().is_some_and(|s| s == "dds") => {
                dds::validate(bytes)?;
            }
            Kind::Raw { .. } => {
                let _gdal = lock(&GDAL_LOCK)?;
                let dataset = Dataset::open(payload)?;
                ensure!(
                    dataset.driver().short_name() == "PNG",
                    "unqualified material resource"
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gdal::{
        DriverManager,
        raster::{Buffer, RasterCreationOptions},
    };

    #[test]
    fn grouped_source_preserves_child_grid_and_boundary_sampling() -> Result<()> {
        let key = Footprint::parse("25760_18432_BI16.dds")?;
        let (parent, _, _, _) = source_window(key, 1024);
        let bounds = parent.meters();
        for row in 0..2 {
            for col in 0..2 {
                let child = Footprint {
                    row: key.row + row * 16,
                    col: key.col + col * 16,
                    ..key
                };
                let (source, size, x, y) = source_window(child, 1024);
                assert_eq!(source, parent);
                assert_eq!((size, x, y), (2048, col * 1024, row * 1024));
                let child_bounds = child.meters();
                let spacing = (bounds.east - bounds.west) / size as f64;
                assert!((child_bounds.west - bounds.west - x as f64 * spacing).abs() < 1e-7);
                assert!((bounds.north - child_bounds.north - y as f64 * spacing).abs() < 1e-7);
                assert!(((child_bounds.east - child_bounds.west) / 1024.0 - spacing).abs() < 1e-7);
            }
        }
        let boundary = Footprint::parse("25760_20736_BI16.dds")?;
        assert!(in_test_envelope(boundary));
        assert_eq!(source_window(boundary, 1024), (boundary, 1024, 0, 0));
        Ok(())
    }

    #[test]
    fn source_pool_separates_resolution_and_preserves_evicted_leases() -> Result<()> {
        let pool = SourcePool::default();
        let key = Footprint::parse("25760_18432_BI16.dds")?;
        let raster = |size, quadrants, value| Raster {
            size,
            quadrants,
            pixels: vec![[value, 0, 0, 255]; (size * size) as usize],
        };
        let first = pool.get(key, 4, ALL_QUADRANTS, |q| Ok(raster(4, q, 100)))?;
        let cached = pool.get(key, 4, ALL_QUADRANTS, |_| {
            anyhow::bail!("unexpected repeated acquisition")
        })?;
        assert!(Arc::ptr_eq(&first, &cached));
        let partial_key = Footprint {
            row: key.row + 256,
            ..key
        };
        let partial = pool.get(partial_key, 8, 0b0001, |q| Ok(raster(8, q, 150)))?;
        assert_eq!(partial.quadrants, 0b0001);
        assert!(partial.crop(0, 0, 4).is_ok());
        assert!(partial.crop(4, 0, 4).is_err());
        let upgraded = pool.get(partial_key, 8, 0b0010, |q| Ok(raster(8, q, 175)))?;
        assert_eq!(upgraded.quadrants, 0b0011);
        assert!(upgraded.crop(0, 0, 4).is_ok());
        assert!(upgraded.crop(4, 0, 4).is_ok());
        let larger = pool.get(key, 8, ALL_QUADRANTS, |q| Ok(raster(8, q, 200)))?;
        assert_eq!(larger.crop(0, 0, 4)?[0][0], 200.0 / 255.0);
        for index in 1..5 {
            pool.get(
                Footprint {
                    row: key.row + index * 16,
                    ..key
                },
                4,
                ALL_QUADRANTS,
                |q| Ok(raster(4, q, 20)),
            )?;
        }
        assert_eq!(lock(&pool.images)?.ready.len(), 4);
        assert_eq!(first.crop(0, 0, 4)?[0][0], 100.0 / 255.0);
        assert!(!Arc::ptr_eq(
            &first,
            &pool.get(key, 4, ALL_QUADRANTS, |q| Ok(raster(4, q, 40)))?
        ));
        let failed = Footprint {
            row: key.row + 128,
            ..key
        };
        assert!(
            pool.get(failed, 4, ALL_QUADRANTS, |_| anyhow::bail!(
                "source unavailable"
            ))
            .is_err()
        );
        assert!(
            pool.get(failed, 4, ALL_QUADRANTS, |q| Ok(raster(4, q, 30)))
                .is_ok()
        );
        Ok(())
    }

    #[test]
    fn concurrent_parent_requests_share_one_acquisition() -> Result<()> {
        use std::sync::{Barrier, atomic::AtomicUsize};
        let pool = Arc::new(SourcePool::default());
        let start = Arc::new(Barrier::new(3));
        let calls = Arc::new(AtomicUsize::new(0));
        let key = Footprint::parse("25760_18432_BI16.dds")?;
        let mut workers = Vec::new();
        for _ in 0..2 {
            let pool = Arc::clone(&pool);
            let start = Arc::clone(&start);
            let calls = Arc::clone(&calls);
            workers.push(std::thread::spawn(move || {
                start.wait();
                pool.get(key, 4, ALL_QUADRANTS, |quadrants| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(10));
                    Ok(Raster {
                        size: 4,
                        quadrants,
                        pixels: vec![[20, 30, 40, 255]; 16],
                    })
                })
            }));
        }
        start.wait();
        let first = workers
            .remove(0)
            .join()
            .map_err(|_| anyhow::anyhow!("source worker panicked"))??;
        let second = workers
            .remove(0)
            .join()
            .map_err(|_| anyhow::anyhow!("source worker panicked"))??;
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[test]
    #[ignore = "requires the live USGS service; source qualification, not simulator startup"]
    fn actual_adjacent_dds_builds_share_one_exact_export() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path();
        fs::write(root.join("osm.sqlite"), [])?;
        let config = StreamConfig {
            source_package: root.display().to_string(),
            mount_point: root.join("reader").display().to_string(),
            osm_db: root.join("osm.sqlite").display().to_string(),
            viirs: None,
            source_revision: "live-source-qualification".into(),
            tile_source: crate::model::TileSource::default(),
            size: 1024,
            exposure: 0.35,
            budget_mib: 512,
        };
        let provider = Provider::new(config.clone(), Arc::new(Scene::new(config)?))?;
        let mut assets = Vec::new();
        let start = std::time::Instant::now();
        for row in [25760, 25776] {
            for col in [18432, 18448] {
                let footprint = Footprint::parse(&format!("{row}_{col}_BI16.dds"))?;
                let spec = Spec {
                    key: digest(format!("{row}/{col}").as_bytes()),
                    length: dds::total_bytes(1024, 8) as u64,
                    kind: Kind::Day(footprint),
                };
                let bytes = provider.build(&spec, root)?;
                provider.validate(&spec, &bytes, root)?;
                println!(
                    "row={row} col={col} bytes={} sha256={}",
                    bytes.len(),
                    digest(&bytes)
                );
                assets.push((spec, bytes));
            }
        }
        assert_eq!(provider.network().0, 64);
        println!(
            "four complete DDSs: {:.3}s, network={:?}",
            start.elapsed().as_secs_f64(),
            provider.network()
        );
        for (spec, original) in assets {
            assert_eq!(provider.build(&spec, root)?, original);
        }
        assert_eq!(provider.network().0, 64);
        Ok(())
    }

    #[test]
    fn export_must_match_footprint_projection_and_destination() -> Result<()> {
        let key = Footprint::parse("25760_18432_BI16.dds")?;
        let b = key.meters();
        let mut value = serde_json::json!({
            "href": "https://basemap.nationalmap.gov/arcgis/rest/directories/basemap_output/USGSImageryOnly_MapServer/test.png",
            "width": 1024, "height": 1024,
            "extent": {"xmin": b.west, "ymin": b.south, "xmax": b.east, "ymax": b.north,
                "spatialReference": {"wkid": 102100, "latestWkid":3857}}
        });
        serde_json::from_value::<Export>(value.clone())?.validate(key, 1024)?;
        value["extent"]["xmin"] = serde_json::json!(b.west + 1.0);
        assert!(
            serde_json::from_value::<Export>(value.clone())?
                .validate(key, 1024)
                .is_err()
        );
        value["extent"]["xmin"] = serde_json::json!(b.west);
        value["href"] = serde_json::json!("https://example.com/test.png");
        assert!(
            serde_json::from_value::<Export>(value)?
                .validate(key, 1024)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn png_decode_preserves_rows_and_refuses_missing_coverage() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let memory = DriverManager::get_driver_by_name("MEM")?;
        let dataset = memory.create_with_band_type::<u8, _>("", 4, 4, 4)?;
        for (index, interpretation) in [
            ColorInterpretation::RedBand,
            ColorInterpretation::GreenBand,
            ColorInterpretation::BlueBand,
            ColorInterpretation::AlphaBand,
        ]
        .into_iter()
        .enumerate()
        {
            let mut band = dataset.rasterband(index + 1)?;
            band.set_color_interpretation(interpretation)?;
            let pixels = if index == 3 {
                vec![255; 16]
            } else {
                (0..16)
                    .map(|p| [200, 160, 100, 20][p / 8 * 2 + p % 4 / 2])
                    .collect()
            };
            band.write((0, 0), (4, 4), &mut Buffer::new((4, 4), pixels))?;
        }
        let png = DriverManager::get_driver_by_name("PNG")?;
        let path = temporary.path().join("covered.png");
        dataset.create_copy(&png, &path, &RasterCreationOptions::default())?;
        let source = Provider::decode_png(&path, 4)?;
        let pixels = source.crop(0, 0, 4)?;
        assert!(pixels[0][0] > 0.7 && pixels[15][0] < 0.1);
        for (x, y, value) in [(0, 0, 200.0), (2, 0, 160.0), (0, 2, 100.0), (2, 2, 20.0)] {
            assert!(source.crop(x, y, 2)?.iter().all(|p| p[0] == value / 255.0));
        }
        dataset.rasterband(4)?.write(
            (0, 0),
            (4, 4),
            &mut Buffer::new(
                (4, 4),
                (0..16).map(|p| if p < 8 { 255 } else { 0 }).collect(),
            ),
        )?;
        let partial = temporary.path().join("partial.png");
        dataset.create_copy(&png, &partial, &RasterCreationOptions::default())?;
        let partial = Provider::decode_png(&partial, 4)?;
        assert!(partial.crop(0, 0, 2).is_ok());
        assert!(partial.crop(0, 2, 2).is_err());
        dataset
            .rasterband(4)?
            .write((0, 0), (4, 4), &mut Buffer::new((4, 4), vec![0_u8; 16]))?;
        let hole = temporary.path().join("hole.png");
        dataset.create_copy(&png, &hole, &RasterCreationOptions::default())?;
        assert!(Provider::decode_png(&hole, 4)?.crop(0, 0, 4).is_err());
        assert!(
            matches!(Provider::decode_png_checked(&path, 8), Err((false, _))),
            "dimension mismatch must be non-retryable"
        );
        let full_bytes = fs::read(&path)?;
        let truncated = temporary.path().join("truncated.png");
        fs::write(&truncated, &full_bytes[..full_bytes.len() / 2])?;
        assert!(
            matches!(Provider::decode_png_checked(&truncated, 4), Err((true, _))),
            "truncated PNG stream must be classified as retryable"
        );
        Ok(())
    }

    #[test]
    fn coastline_mask_permits_unmasked_ocean_and_rejects_uncovered_land() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path();
        for dir in ["Earth nav data", "terrain", "textures"] {
            fs::create_dir_all(root.join(dir))?;
        }
        fs::write(root.join("osm.sqlite"), [])?;
        let memory = DriverManager::get_driver_by_name("MEM")?;
        let png = DriverManager::get_driver_by_name("PNG")?;
        let mask_ds = memory.create_with_band_type::<u8, _>("", 8, 8, 1)?;
        let mut mask_band = mask_ds.rasterband(1)?;
        mask_band.set_color_interpretation(ColorInterpretation::GrayIndex)?;
        let mut mask_pixels = vec![0_u8; 64];
        for row in 0..3 {
            for col in 0..8 {
                mask_pixels[row * 8 + col] = 255;
            }
        }
        mask_band.write((0, 0), (8, 8), &mut Buffer::new((8, 8), mask_pixels))?;
        let mask_path = root.join("textures/26096_18624_ZL16.png");
        mask_ds.create_copy(&png, &mask_path, &RasterCreationOptions::default())?;
        fs::write(
            root.join("terrain/26096_18624_BI16_sea_overlay.ter"),
            "A\n800\nTERRAIN\nLOAD_CENTER 34.34344 -77.65137 8078 4096\nBASE_TEX_NOWRAP ../textures/26096_18624_BI16.dds\nLOAD_CENTER_BORDER 34.34344 -77.65137 8078 1024\nBORDER_TEX ../textures/26096_18624_ZL16.png\nWET\nNO_SHADOW\n",
        )?;
        let config = StreamConfig {
            source_package: root.display().to_string(),
            mount_point: root.join("reader").display().to_string(),
            osm_db: root.join("osm.sqlite").display().to_string(),
            viirs: None,
            source_revision: "coastal-mask-test".into(),
            tile_source: crate::model::TileSource::default(),
            size: 1024,
            exposure: 0.35,
            budget_mib: 512,
        };
        let scene = Scene::new(config)?;
        let key = Footprint::parse("26096_18624_BI16.dds")?;
        let (resolved_path, stamp) = scene
            .sea_mask(key)?
            .context("expected verified coastline mask")?;
        let decoded_mask = Provider::decode_mask(&resolved_path, &stamp)?;
        let mut pixels = vec![[40, 80, 120, 255]; 64];
        for row in 5..8 {
            for col in 0..8 {
                pixels[row * 8 + col] = [0, 0, 0, 0];
            }
        }
        let coastal = Raster {
            size: 8,
            quadrants: ALL_QUADRANTS,
            pixels,
        };
        assert!(coastal.crop(0, 0, 8).is_err());
        let cropped = coastal.crop_masked(0, 0, 8, Some(&decoded_mask))?;
        assert_eq!(cropped[0], [40.0 / 255.0, 80.0 / 255.0, 120.0 / 255.0]);
        assert_eq!(cropped[7 * 8], [0.0, 0.0, 0.0]);
        let mut margin_breach = coastal.pixels.clone();
        margin_breach[3 * 8] = [0, 0, 0, 0];
        assert!(
            Raster {
                size: 8,
                quadrants: ALL_QUADRANTS,
                pixels: margin_breach
            }
            .crop_masked(0, 0, 8, Some(&decoded_mask))
            .is_err()
        );
        let empty = Raster {
            size: 8,
            quadrants: ALL_QUADRANTS,
            pixels: vec![[0, 0, 0, 0]; 64],
        };
        let zero_mask = Mask {
            size: 8,
            pixels: vec![0; 64],
        };
        assert!(empty.crop_masked(0, 0, 8, Some(&zero_mask)).is_err());
        fs::write(&mask_path, b"corrupted")?;
        assert!(scene.sea_mask(key).is_err());
        Ok(())
    }

    #[test]
    #[ignore = "requires live USGS service and local borrowed z_ao_na coastline masks"]
    fn actual_coastal_and_previously_failing_footprints_build() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path();
        fs::write(root.join("osm.sqlite"), [])?;
        let source_package = std::env::var("GROUNDEFFECT_TEST_SOURCE_PACKAGE")
            .context("set GROUNDEFFECT_TEST_SOURCE_PACKAGE to local z_ao_na mesh path")?;
        let config = StreamConfig {
            source_package,
            mount_point: root.join("reader").display().to_string(),
            osm_db: root.join("osm.sqlite").display().to_string(),
            viirs: None,
            source_revision: "live-coastal-qualification".into(),
            tile_source: crate::model::TileSource::default(),
            size: 1024,
            exposure: 0.35,
            budget_mib: 512,
        };
        let provider = Provider::new(config.clone(), Arc::new(Scene::new(config)?))?;
        for name in [
            "26096_18624_BI16.dds",
            "26048_18704_BI16.dds",
            "26032_18720_BI16.dds",
            "25552_18160_BI16.dds",
            "25776_18176_BI16.dds",
            "103248_72432_BI18.dds",
            "103664_73536_BI18.dds",
        ] {
            let footprint = Footprint::parse(name)?;
            let spec = Spec {
                key: digest(name.as_bytes()),
                length: dds::total_bytes(1024, 8) as u64,
                kind: Kind::Day(footprint),
            };
            let bytes = provider.build(&spec, root)?;
            provider.validate(&spec, &bytes, root)?;
            println!(
                "qualified {name}: bytes={} sha256={}",
                bytes.len(),
                digest(&bytes)
            );
        }
        Ok(())
    }

    #[test]
    fn ancestor_tile_upsampling_preserves_quadrant_selection() {
        let n = TILE_SIZE as usize;
        let half = n / 2;
        let mut parent = vec![[0_u8, 0_u8, 0_u8, 255_u8]; n * n];
        for y in 0..n {
            for x in 0..n {
                let q = (if y >= half { 2 } else { 0 }) + (if x >= half { 1 } else { 0 });
                let v = [20_u8, 90_u8, 160_u8, 230_u8][q];
                parent[y * n + x] = [v, v, v, 255];
            }
        }
        for (dr, dc, expected) in [(0, 0, 20_u8), (0, 1, 90_u8), (1, 0, 160_u8), (1, 1, 230_u8)] {
            let child = upsample_ancestor_tile(&parent, 1, 100 + dr, 200 + dc);
            assert_eq!(child.len(), n * n);
            assert_eq!(child[64 * n + 64], [expected, expected, expected, 255]);
        }
    }

    #[test]
    fn bing_quadkey_and_tile_url_match_web_mercator_coordinates() {
        assert_eq!(crate::model::TileSource::quadkey(3, 5, 3), "213");
        assert_eq!(
            crate::model::TileSource::quadkey(14, 6440, 4608),
            "03201200202000"
        );
        assert_eq!(
            crate::model::TileSource::Bing.tile_url(14, 6440, 4608),
            "https://t.ssl.ak.tiles.virtualearth.net/tiles/a03201200202000.jpeg?g=15312"
        );
    }
}
