use crate::{
    geo::{Footprint, mercator},
    model::{FxBuildHasher, FxHashMap, FxHashSet},
};
use anyhow::{Context, Result, ensure};
use flate2::{Decompress, FlushDecompress, Status};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    os::fd::AsRawFd,
    path::Path,
    sync::{Arc, Mutex},
};

const SUBGRID: usize = 16;
const SUBCELLS: usize = SUBGRID * SUBGRID;
const MAX_POLYLINE_CHUNK_POINTS: usize = 64;

struct Mmap {
    ptr: *mut libc::c_void,
    len: usize,
}

// Safety: Mmap is mapped PROT_READ (read-only) and remains valid until munmap in Drop.
unsafe impl Send for Mmap {}
unsafe impl Sync for Mmap {}

impl Mmap {
    fn open(path: &Path) -> Result<Self> {
        let file =
            File::open(path).with_context(|| format!("opening mmap file {}", path.display()))?;
        let len = file.metadata()?.len() as usize;
        ensure!(len > 0, "cannot mmap empty file {}", path.display());
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("mmap failed for {}", path.display()));
        }
        unsafe {
            let _ = libc::madvise(ptr, len, libc::MADV_RANDOM);
        }
        Ok(Self { ptr, len })
    }

    #[inline]
    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
    }
}

impl Drop for Mmap {
    fn drop(&mut self) {
        if !self.ptr.is_null() && self.ptr != libc::MAP_FAILED && self.len > 0 {
            unsafe {
                let _ = libc::munmap(self.ptr, self.len);
            }
        }
    }
}

enum Backing {
    Mmap(Mmap),
    Owned(Vec<u8>),
}

impl Backing {
    #[inline]
    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Mmap(m) => m.as_slice(),
            Self::Owned(v) => v.as_slice(),
        }
    }
}

type ViirsCellKey = (i16, i16);
type ViirsLocalCache = Option<(ViirsCellKey, Option<Arc<[u16]>>)>;

struct Viirs {
    mmap: Mmap,
    size: usize,
    #[cfg(test)]
    scale: f32,
    max_ln1p: f32,
    index: FxHashMap<ViirsCellKey, (u64, u32)>,
    cached: Mutex<Vec<(ViirsCellKey, Arc<[u16]>)>>,
}

fn u32_at(b: &[u8], p: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        b.get(p..p + 4)
            .context("truncated binary data")?
            .try_into()?,
    ))
}

fn u64_at(b: &[u8], p: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(
        b.get(p..p + 8)
            .context("truncated binary data")?
            .try_into()?,
    ))
}

fn f32_at(b: &[u8], p: usize) -> Result<f32> {
    Ok(f32::from_bits(u32_at(b, p)?))
}

impl Viirs {
    fn open(path: &Path) -> Result<Self> {
        let mmap =
            Mmap::open(path).with_context(|| format!("opening VIIRS source {}", path.display()))?;
        let data = mmap.as_slice();
        let length = data.len() as u64;
        ensure!(data.len() >= 16, "truncated VIIRS header");
        let head = &data[..16];
        ensure!(&head[..4] == b"RDT1", "expected local RDT1 VIIRS data");
        let size = u32_at(head, 4)? as usize;
        let count = u32_at(head, 8)? as usize;
        let max = f32::from_bits(u32_at(head, 12)?);
        ensure!(
            (1..=4096).contains(&size) && count <= 64800,
            "VIIRS exceeds memory/index limits"
        );
        ensure!(max.is_finite() && max > 0.0, "invalid VIIRS radiance");
        let table_end = 16usize
            .checked_add(count.checked_mul(16).context("VIIRS index overflow")?)
            .context("VIIRS index overflow")?;
        let table = data.get(16..table_end).context("truncated VIIRS index")?;
        let mut index = FxHashMap::with_capacity_and_hasher(count, FxBuildHasher::default());
        for b in table.as_chunks::<16>().0 {
            let north = i16::from_le_bytes(b[..2].try_into()?);
            let west = i16::from_le_bytes(b[2..4].try_into()?);
            let len = u32_at(b, 4)?;
            let offset = u64::from_le_bytes(b[8..16].try_into()?);
            ensure!(
                (-89..=90).contains(&north) && (-180..180).contains(&west),
                "invalid VIIRS cell"
            );
            ensure!(
                len <= 32 * 1024 * 1024
                    && offset >= table_end as u64
                    && offset
                        .checked_add(len as u64)
                        .is_some_and(|end| end <= length),
                "VIIRS tile out of bounds"
            );
            ensure!(
                index.insert((north, west), (offset, len)).is_none(),
                "duplicate VIIRS tile"
            );
        }
        Ok(Self {
            mmap,
            size,
            #[cfg(test)]
            scale: max.ln_1p() / (65535.0 * 25_f32.ln_1p()),
            max_ln1p: max.ln_1p(),
            index,
            cached: Mutex::new(Vec::with_capacity(64)),
        })
    }

    fn cell_values(&self, key: (i16, i16)) -> Result<Option<Arc<[u16]>>> {
        if let Ok(mut guard) = self.cached.lock()
            && let Some(pos) = guard.iter().position(|(k, _)| *k == key)
        {
            let item = guard.remove(pos);
            let values = Arc::clone(&item.1);
            guard.push(item);
            return Ok(Some(values));
        }
        let Some(&(offset, len)) = self.index.get(&key) else {
            return Ok(None);
        };
        let compressed = self
            .mmap
            .as_slice()
            .get(offset as usize..(offset + len as u64) as usize)
            .context("VIIRS tile payload out of bounds")?;
        let expected = self.size * self.size * 2;
        let mut raw = vec![0; expected + 1];
        let mut decoder = Decompress::new(true);
        let status = decoder.decompress(compressed, &mut raw, FlushDecompress::Finish)?;
        ensure!(
            status == Status::StreamEnd
                && decoder.total_out() == expected as u64
                && decoder.total_in() == compressed.len() as u64,
            "invalid VIIRS compressed stream/decoded size"
        );
        raw.truncate(expected);
        let values: Arc<[u16]> = raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| u16::from_le_bytes([p[0], p[1]]))
            .collect::<Vec<_>>()
            .into();
        if let Ok(mut guard) = self.cached.lock() {
            if let Some(pos) = guard.iter().position(|(k, _)| *k == key) {
                guard.remove(pos);
            } else if guard.len() >= 64 {
                guard.remove(0);
            }
            guard.push((key, Arc::clone(&values)));
        }
        Ok(Some(values))
    }

    #[cfg(test)]
    fn factor(&self, lat: f64, lon: f64) -> Result<f32> {
        let key = (lat.floor() as i16 + 1, lon.floor() as i16);
        let size = self.size;
        let scale = self.scale;
        let Some(values) = self.cell_values(key)? else {
            return Ok(1.0);
        };
        let row = (((key.0 as f64 - lat) * size as f64) as usize).min(size - 1);
        let col = (((lon - key.1 as f64) * size as f64) as usize).min(size - 1);
        Ok((values[row * size + col] as f32 * scale).clamp(0.25, 1.0))
    }

    /// Returns `(intensity, urbanity)` using bilinear interpolation of decoded
    /// physical radiance (`nW/cm^2/sr`) across the 240x240 VIIRS grid.
    fn sample_with_local(
        &self,
        lat: f64,
        lon: f64,
        local: &mut ViirsLocalCache,
    ) -> Result<(f32, f32)> {
        let key = (lat.floor() as i16 + 1, lon.floor() as i16);
        let values = match local {
            Some((k, v)) if *k == key => v.as_deref(),
            _ => {
                let fetched = self.cell_values(key)?;
                *local = Some((key, fetched));
                local.as_ref().and_then(|(_, v)| v.as_deref())
            }
        };
        let Some(values) = values else {
            return Ok((0.65, 0.25));
        };
        let size = self.size;
        let max_ln1p = self.max_ln1p;
        let fy = ((key.0 as f64 - lat) * size as f64 - 0.5).clamp(0.0, (size - 1) as f64);
        let fx = ((lon - key.1 as f64) * size as f64 - 0.5).clamp(0.0, (size - 1) as f64);
        let r0 = fy.floor() as usize;
        let c0 = fx.floor() as usize;
        let r1 = (r0 + 1).min(size - 1);
        let c1 = (c0 + 1).min(size - 1);
        let ty = (fy - r0 as f64) as f32;
        let tx = (fx - c0 as f64) as f32;
        let decode = |code: u16| -> f32 {
            if code == 0 {
                0.0
            } else {
                ((code as f32 / 65535.0) * max_ln1p).exp_m1()
            }
        };
        let v00 = decode(values[r0 * size + c0]);
        let v01 = decode(values[r0 * size + c1]);
        let v10 = decode(values[r1 * size + c0]);
        let v11 = decode(values[r1 * size + c1]);
        let top = v00 + (v01 - v00) * tx;
        let bot = v10 + (v11 - v10) * tx;
        let radiance = (top + (bot - top) * ty).max(0.0);
        // Steep urban contrast so true city cores (60-200+ nW/cm^2/sr) stand out
        // over suburban (20-45 nW) and rural (<10 nW) areas along road networks
        let norm = (radiance.ln_1p() / 400_f32.ln_1p()).clamp(0.0, 1.35);
        let urbanity = ((radiance - 14.0) / 85.0).clamp(0.0, 1.0);
        let intensity = 0.04 + 0.85 * norm.powf(1.35) + 1.25 * urbanity.powf(1.40);
        Ok((intensity, urbanity))
    }
}

/// Memory-mapped `RDS1` spatial road archive indexed by `1°x1°` degree cell ->
/// `16x16` (`0.0625° x 0.0625°`) sub-cell directory -> contiguous packed road records.
struct Rds1 {
    backing: Backing,
    cells: FxHashMap<(i16, i16), usize>,
}

impl Rds1 {
    fn from_backing(backing: Backing) -> Result<Self> {
        let data = backing.as_slice();
        ensure!(data.len() >= 16, "truncated RDS1 road archive header");
        ensure!(&data[..4] == b"RDS1", "expected RDS1 binary road archive");
        let subgrid = u32_at(data, 4)? as usize;
        let count = u32_at(data, 8)? as usize;
        ensure!(
            subgrid == SUBGRID && count <= 64800,
            "unsupported RDS1 subgrid dimension or cell count"
        );
        let table_end = 16usize
            .checked_add(count.checked_mul(16).context("RDS1 cell table overflow")?)
            .context("RDS1 cell table overflow")?;
        let table = data
            .get(16..table_end)
            .context("truncated RDS1 cell directory")?;
        let mut cells = FxHashMap::with_capacity_and_hasher(count, FxBuildHasher::default());
        for entry in table.as_chunks::<16>().0 {
            let lat_floor = i16::from_le_bytes(entry[0..2].try_into()?);
            let lon_floor = i16::from_le_bytes(entry[2..4].try_into()?);
            let subgrid_offset = u64_at(entry, 8)? as usize;
            ensure!(
                (-90..=89).contains(&lat_floor) && (-180..=179).contains(&lon_floor),
                "invalid RDS1 cell coordinate"
            );
            let subgrid_end = subgrid_offset
                .checked_add(SUBCELLS * 12)
                .context("RDS1 subgrid offset overflow")?;
            ensure!(
                subgrid_offset >= table_end && subgrid_end <= data.len(),
                "RDS1 subgrid directory out of bounds"
            );
            ensure!(
                cells
                    .insert((lat_floor, lon_floor), subgrid_offset)
                    .is_none(),
                "duplicate RDS1 cell entry"
            );
        }
        Ok(Self { backing, cells })
    }

    #[inline]
    fn subcell_slice(&self, lat_floor: i16, lon_floor: i16, sub_idx: usize) -> Result<&[u8]> {
        let Some(&subgrid_offset) = self.cells.get(&(lat_floor, lon_floor)) else {
            return Ok(&[]);
        };
        let data = self.backing.as_slice();
        let entry_offset = subgrid_offset + sub_idx * 12;
        let payload_offset = u64_at(data, entry_offset)? as usize;
        let payload_len = u32_at(data, entry_offset + 8)? as usize;
        if payload_len == 0 {
            return Ok(&[]);
        }
        data.get(
            payload_offset
                ..payload_offset
                    .checked_add(payload_len)
                    .context("RDS1 payload overflow")?,
        )
        .context("RDS1 subcell payload out of bounds")
    }
}

#[inline]
fn grid_coord(deg: f64) -> i32 {
    (deg * SUBGRID as f64).floor() as i32
}

#[inline]
fn split_grid_coord(g: i32) -> (i16, usize) {
    (
        g.div_euclid(SUBGRID as i32) as i16,
        g.rem_euclid(SUBGRID as i32) as usize,
    )
}

fn encode_road_record(id: u64, tier: f32, pts: &[[f64; 2]], out: &mut Vec<u8>) -> Result<[f32; 4]> {
    ensure!(
        tier.is_finite() && (0.0..=1.0).contains(&tier),
        "invalid feature tier"
    );
    ensure!((2..=32768).contains(&pts.len()), "invalid road point count");
    let mut min_lat = f32::INFINITY;
    let mut max_lat = f32::NEG_INFINITY;
    let mut min_lon = f32::INFINITY;
    let mut max_lon = f32::NEG_INFINITY;
    for &[lat, lon] in pts {
        ensure!(
            lat.is_finite() && lon.is_finite() && lat.abs() < 85.06 && lon.abs() <= 180.0,
            "invalid source feature coordinate"
        );
        let lat_f = lat as f32;
        let lon_f = lon as f32;
        min_lat = min_lat.min(lat_f);
        max_lat = max_lat.max(lat_f);
        min_lon = min_lon.min(lon_f);
        max_lon = max_lon.max(lon_f);
    }
    out.reserve(32 + pts.len() * 8);
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&tier.to_bits().to_le_bytes());
    out.extend_from_slice(&min_lat.to_bits().to_le_bytes());
    out.extend_from_slice(&max_lat.to_bits().to_le_bytes());
    out.extend_from_slice(&min_lon.to_bits().to_le_bytes());
    out.extend_from_slice(&max_lon.to_bits().to_le_bytes());
    out.extend_from_slice(&(pts.len() as u32).to_le_bytes());
    for &[lat, lon] in pts {
        out.extend_from_slice(&(lat as f32).to_bits().to_le_bytes());
        out.extend_from_slice(&(lon as f32).to_bits().to_le_bytes());
    }
    Ok([min_lat, max_lat, min_lon, max_lon])
}

fn collect_road_subcells(pts: &[[f64; 2]], buckets: &mut Vec<(i16, i16, u8)>) {
    buckets.clear();
    for pair in pts.windows(2) {
        let min_lat = pair[0][0].min(pair[1][0]);
        let max_lat = pair[0][0].max(pair[1][0]);
        let min_lon = pair[0][1].min(pair[1][1]);
        let max_lon = pair[0][1].max(pair[1][1]);
        if (max_lat - min_lat) > 2.0 || (max_lon - min_lon) > 2.0 {
            for pt in pair {
                let (lat_f, sy) = split_grid_coord(grid_coord(pt[0]));
                let (lon_f, sx) = split_grid_coord(grid_coord(pt[1]));
                let key = (lat_f, lon_f, (sy * SUBGRID + sx) as u8);
                if !buckets.contains(&key) {
                    buckets.push(key);
                }
            }
            continue;
        }
        let gy_min = grid_coord(min_lat);
        let gy_max = grid_coord(max_lat);
        let gx_min = grid_coord(min_lon);
        let gx_max = grid_coord(max_lon);
        for gy in gy_min..=gy_max {
            let (lat_f, sy) = split_grid_coord(gy);
            for gx in gx_min..=gx_max {
                let (lon_f, sx) = split_grid_coord(gx);
                let key = (lat_f, lon_f, (sy * SUBGRID + sx) as u8);
                if !buckets.contains(&key) {
                    buckets.push(key);
                }
            }
        }
    }
}

pub fn pack_roads_rds1(roads: &[(u64, f32, Vec<[f64; 2]>)]) -> Result<Vec<u8>> {
    let mut cells: BTreeMap<(i16, i16), Vec<Vec<u8>>> = BTreeMap::new();
    let mut record = Vec::with_capacity(256);
    let mut buckets = Vec::with_capacity(8);
    for (id, tier, points) in roads {
        ensure!(
            (2..=32768).contains(&points.len()),
            "invalid road point count"
        );
        let mut start = 0;
        while start < points.len() {
            let end = (start + MAX_POLYLINE_CHUNK_POINTS).min(points.len());
            let slice = &points[start..end];
            if slice.len() >= 2 {
                record.clear();
                encode_road_record(*id, *tier, slice, &mut record)?;
                collect_road_subcells(slice, &mut buckets);
                for &(lat_f, lon_f, sub_idx) in &buckets {
                    let subcells = cells
                        .entry((lat_f, lon_f))
                        .or_insert_with(|| vec![Vec::new(); SUBCELLS]);
                    subcells[sub_idx as usize].extend_from_slice(&record);
                }
            }
            if end == points.len() {
                break;
            }
            start = end - 1;
        }
    }
    Ok(pack_rds1_cells(cells))
}

fn pack_rds1_cells(cells: BTreeMap<(i16, i16), Vec<Vec<u8>>>) -> Vec<u8> {
    let count = cells.len();
    let table_end = 16 + count * 16;
    let mut out = vec![0_u8; table_end];
    out[0..4].copy_from_slice(b"RDS1");
    out[4..8].copy_from_slice(&(SUBGRID as u32).to_le_bytes());
    out[8..12].copy_from_slice(&(count as u32).to_le_bytes());
    for (idx, ((lat_f, lon_f), subcells)) in cells.into_iter().enumerate() {
        let subgrid_offset = out.len() as u64;
        let entry_pos = 16 + idx * 16;
        out[entry_pos..entry_pos + 2].copy_from_slice(&lat_f.to_le_bytes());
        out[entry_pos..entry_pos + 4][2..4].copy_from_slice(&lon_f.to_le_bytes());
        out[entry_pos + 8..entry_pos + 16].copy_from_slice(&subgrid_offset.to_le_bytes());
        let dir_start = out.len();
        out.resize(dir_start + SUBCELLS * 12, 0);
        for (sub_idx, payload) in subcells.into_iter().enumerate() {
            let payload_offset = out.len() as u64;
            let payload_len = payload.len() as u32;
            let sub_pos = dir_start + sub_idx * 12;
            out[sub_pos..sub_pos + 8].copy_from_slice(&payload_offset.to_le_bytes());
            out[sub_pos + 8..sub_pos + 12].copy_from_slice(&payload_len.to_le_bytes());
            out.extend_from_slice(&payload);
        }
    }
    out
}

fn compile_dsf_cell_rds1_bytes(dsf_path: &Path, cell_lat: i16, cell_lon: i16) -> Result<Vec<u8>> {
    let raw_roads = crate::dsf_compiler::extract_laminar_dsf_roads(dsf_path, cell_lat, cell_lon)?;
    pack_roads_rds1(&raw_roads)
}

type CellRds1Cache = FxHashMap<(i16, i16), Option<Arc<Rds1>>>;
type CellRds1Local = Option<((i16, i16), Option<Arc<Rds1>>)>;

struct LaminarDsfStore {
    roots: Vec<std::path::PathBuf>,
    cache_dir: std::path::PathBuf,
    cells: Mutex<CellRds1Cache>,
}

impl LaminarDsfStore {
    fn new(roots: Vec<std::path::PathBuf>) -> Self {
        let base = if Path::new("/var/tmp").is_dir() {
            std::path::PathBuf::from("/var/tmp")
        } else {
            std::env::temp_dir()
        };
        let cache_dir = base.join("groundeffect-laminar-rds1-v1");
        let _ = fs::create_dir_all(&cache_dir);
        Self {
            roots,
            cache_dir,
            cells: Mutex::new(FxHashMap::default()),
        }
    }

    fn cell_rds1(&self, lat_floor: i16, lon_floor: i16) -> Result<Option<Arc<Rds1>>> {
        if let Ok(guard) = self.cells.lock()
            && let Some(cached) = guard.get(&(lat_floor, lon_floor))
        {
            return Ok(cached.clone());
        }

        let rds1_file = self
            .cache_dir
            .join(format!("{lat_floor:+03}{lon_floor:+04}.rds1"));
        if rds1_file.is_file()
            && let Ok(mmap) = Mmap::open(&rds1_file)
            && let Ok(rds1) = Rds1::from_backing(Backing::Mmap(mmap))
        {
            let arc = Arc::new(rds1);
            if let Ok(mut guard) = self.cells.lock() {
                guard.insert((lat_floor, lon_floor), Some(Arc::clone(&arc)));
            }
            return Ok(Some(arc));
        }

        let b_lat = (lat_floor as i32).div_euclid(10) * 10;
        let b_lon = (lon_floor as i32).div_euclid(10) * 10;
        let rel = std::path::PathBuf::from(format!(
            "Earth nav data/{b_lat:+03}{b_lon:+04}/{lat_floor:+03}{lon_floor:+04}.dsf"
        ));
        let Some(dsf_path) = self
            .roots
            .iter()
            .map(|r| r.join(&rel))
            .find(|p| p.is_file())
        else {
            if let Ok(mut guard) = self.cells.lock() {
                guard.insert((lat_floor, lon_floor), None);
            }
            return Ok(None);
        };

        let bytes = compile_dsf_cell_rds1_bytes(&dsf_path, lat_floor, lon_floor)?;
        let rds1 = if let Ok(mut staged) = tempfile::Builder::new()
            .prefix(".laminar-rds1-")
            .suffix(".tmp")
            .tempfile_in(&self.cache_dir)
            && staged.write_all(&bytes).is_ok()
            && staged.persist(&rds1_file).is_ok()
            && let Ok(mmap) = Mmap::open(&rds1_file)
        {
            Rds1::from_backing(Backing::Mmap(mmap))?
        } else {
            Rds1::from_backing(Backing::Owned(bytes))?
        };
        let arc = Arc::new(rds1);
        if let Ok(mut guard) = self.cells.lock() {
            guard.insert((lat_floor, lon_floor), Some(Arc::clone(&arc)));
        }
        Ok(Some(arc))
    }
}

enum RoadArchive {
    Static(Arc<Rds1>),
    LaminarDsf(Arc<LaminarDsfStore>),
}

impl Clone for RoadArchive {
    fn clone(&self) -> Self {
        match self {
            Self::Static(r) => Self::Static(Arc::clone(r)),
            Self::LaminarDsf(s) => Self::LaminarDsf(Arc::clone(s)),
        }
    }
}

#[derive(Clone)]
pub struct Lighting {
    roads: RoadArchive,
    viirs: Option<Arc<Viirs>>,
}

impl Lighting {
    pub fn open(db: &Path, viirs: Option<&Path>) -> Result<Self> {
        let is_laminar = db.as_os_str().is_empty()
            || db == Path::new("xplane")
            || db == Path::new("laminar")
            || db.is_dir();
        let roads = if is_laminar {
            let roots = crate::dsf_compiler::discover_laminar_scenery_roots(if db.is_dir() {
                Some(db)
            } else {
                None
            });
            ensure!(
                !roots.is_empty(),
                "no X-Plane 12 Global Scenery DSF roots found for road lighting ({})",
                db.display()
            );
            RoadArchive::LaminarDsf(Arc::new(LaminarDsfStore::new(roots)))
        } else {
            let mut magic = [0_u8; 4];
            let mut file =
                File::open(db).with_context(|| format!("opening road source {}", db.display()))?;
            let file_len = file.metadata()?.len();
            let is_rds1 = file.read_exact(&mut magic).is_ok() && &magic == b"RDS1";
            drop(file);

            let rds1 = if file_len == 0 {
                Rds1::from_backing(Backing::Owned(pack_rds1_cells(BTreeMap::new())))?
            } else if is_rds1 {
                Rds1::from_backing(Backing::Mmap(Mmap::open(db)?))?
            } else {
                let sibling = db.with_extension("rds1");
                ensure!(
                    sibling.is_file(),
                    "unsupported road archive {}; expected Laminar DSF directory or RDS1 binary archive",
                    db.display()
                );
                Rds1::from_backing(Backing::Mmap(Mmap::open(&sibling)?))?
            };
            RoadArchive::Static(Arc::new(rds1))
        };
        Ok(Self {
            roads,
            viirs: viirs.map(Viirs::open).transpose()?.map(Arc::new),
        })
    }

    pub fn render(
        &mut self,
        footprint: Footprint,
        size: u32,
        exposure: f32,
    ) -> Result<Vec<[f32; 3]>> {
        self.render_modulated(footprint, size, exposure, None)
    }

    pub fn render_modulated(
        &mut self,
        footprint: Footprint,
        size: u32,
        exposure: f32,
        day_rgb: Option<&[[f32; 3]]>,
    ) -> Result<Vec<[f32; 3]>> {
        let day_rgb = day_rgb.filter(|px| px.len() == (size * size) as usize);
        let b = footprint.geographic();
        let meters = footprint.meters();
        let step = (meters.east - meters.west) / size as f64;
        let ground_step = (step * ((b.north + b.south) / 2.0).to_radians().cos()).max(0.5);
        let px_per_meter = (1.0 / ground_step) as f32;
        let mut rgb = vec![[0_f32; 3]; (size * size) as usize];

        let q_east = (b.east + 0.001) as f32;
        let q_west = (b.west - 0.001) as f32;
        let q_north = (b.north + 0.001) as f32;
        let q_south = (b.south - 0.001) as f32;

        let gy_min = grid_coord(b.south - 0.001);
        let gy_max = grid_coord(b.north + 0.001);
        let gx_min = grid_coord(b.west - 0.001);
        let gx_max = grid_coord(b.east + 0.001);
        let multi_subcell = gy_min != gy_max || gx_min != gx_max;
        let mut seen_ids: FxHashSet<u64> = FxHashSet::default();
        let mut seen_junctions: FxHashSet<u64> = FxHashSet::default();
        let mut viirs_local: ViirsLocalCache = None;
        let mut count = 0usize;

        let project = |lat: f64, lon: f64| -> (f32, f32) {
            let (x, y) = mercator(lon, lat);
            (
                ((x - meters.west) / step - 0.5) as f32,
                ((meters.north - y) / step - 0.5) as f32,
            )
        };

        let mut cached_cell: CellRds1Local = None;
        for gy in gy_min..=gy_max {
            let (lat_floor, sy) = split_grid_coord(gy);
            for gx in gx_min..=gx_max {
                let (lon_floor, sx) = split_grid_coord(gx);
                let sub_idx = sy * SUBGRID + sx;
                let cell_rds1 = match &self.roads {
                    RoadArchive::Static(r) => Some(Arc::clone(r)),
                    RoadArchive::LaminarDsf(store) => {
                        if let Some((key, ref val)) = cached_cell
                            && key == (lat_floor, lon_floor)
                        {
                            val.clone()
                        } else {
                            let loaded = store.cell_rds1(lat_floor, lon_floor)?;
                            cached_cell = Some(((lat_floor, lon_floor), loaded.clone()));
                            loaded
                        }
                    }
                };
                let Some(rds1) = cell_rds1 else {
                    continue;
                };
                let slice = rds1.subcell_slice(lat_floor, lon_floor, sub_idx)?;
                let mut pos = 0usize;
                while pos < slice.len() {
                    ensure!(pos + 32 <= slice.len(), "truncated RDS1 road record header");
                    let id = u64_at(slice, pos)?;
                    let tier = f32_at(slice, pos + 8)?;
                    let min_lat = f32_at(slice, pos + 12)?;
                    let max_lat = f32_at(slice, pos + 16)?;
                    let min_lon = f32_at(slice, pos + 20)?;
                    let max_lon = f32_at(slice, pos + 24)?;
                    let pt_count = u32_at(slice, pos + 28)? as usize;
                    let pts_start = pos + 32;
                    let pts_end = pts_start
                        .checked_add(pt_count.checked_mul(8).context("RDS1 point overflow")?)
                        .context("RDS1 point overflow")?;
                    ensure!(
                        (2..=32768).contains(&pt_count) && pts_end <= slice.len(),
                        "invalid or truncated RDS1 road vertices"
                    );
                    pos = pts_end;

                    if max_lon < q_west
                        || min_lon > q_east
                        || max_lat < q_south
                        || min_lat > q_north
                    {
                        continue;
                    }
                    if multi_subcell && !seen_ids.insert(id) {
                        continue;
                    }
                    count += 1;
                    ensure!(
                        count <= 100000,
                        "OSM query exceeds per-footprint feature budget"
                    );

                    let h = mix64(id);
                    let h_u01 = ((h & 0xFFFF) as f32) / 65535.0;
                    let (core_r, halo_r, skirt_r, lamp_period_m) = if tier >= 0.85 {
                        (
                            (5.0 * px_per_meter).max(0.95),
                            (11.5 * px_per_meter).max(2.05),
                            if day_rgb.is_some() {
                                (20.0 * px_per_meter).max(3.50)
                            } else {
                                (15.0 * px_per_meter).max(2.50)
                            },
                            52.0,
                        )
                    } else if tier >= 0.58 {
                        (
                            (4.6 * px_per_meter).max(0.88),
                            (11.0 * px_per_meter).max(1.90),
                            if day_rgb.is_some() {
                                (19.5 * px_per_meter).max(3.30)
                            } else {
                                (14.5 * px_per_meter).max(2.30)
                            },
                            30.0,
                        )
                    } else if tier >= 0.35 {
                        (
                            (3.6 * px_per_meter).max(0.75),
                            (8.4 * px_per_meter).max(1.45),
                            if day_rgb.is_some() {
                                (14.5 * px_per_meter).max(2.45)
                            } else {
                                (10.8 * px_per_meter).max(1.75)
                            },
                            32.0,
                        )
                    } else {
                        (
                            (2.7 * px_per_meter).max(0.60),
                            (6.0 * px_per_meter).max(1.10),
                            if day_rgb.is_some() {
                                (10.0 * px_per_meter).max(1.80)
                            } else {
                                (7.8 * px_per_meter).max(1.35)
                            },
                            38.0,
                        )
                    };
                    let lamp_period_px = (lamp_period_m * px_per_meter).max(3.6);
                    let mut arc_px = h_u01 * lamp_period_px;
                    let exp_gain = exposure * 2.05;

                    let pts = slice[pts_start..pts_end].as_chunks::<8>().0;
                    let first_lat = f32::from_bits(u32::from_le_bytes([
                        pts[0][0], pts[0][1], pts[0][2], pts[0][3],
                    ])) as f64;
                    let first_lon = f32::from_bits(u32::from_le_bytes([
                        pts[0][4], pts[0][5], pts[0][6], pts[0][7],
                    ])) as f64;
                    let first_proj = project(first_lat, first_lon);
                    let mut prev_lat = first_lat;
                    let mut prev_lon = first_lon;
                    let mut prev_proj = first_proj;
                    let mut last_power = 0.0_f32;
                    let mut last_color = [1.00_f32, 0.72, 0.36];

                    // A single VIIRS pixel spans 1/240 deg (~0.00417 deg). Short road chunks
                    // (<0.002 deg) can share one VIIRS sample across their segments.
                    let uniform_viirs = if (max_lat - min_lat) <= 0.002
                        && (max_lon - min_lon) <= 0.002
                    {
                        let mid_lat = ((min_lat + max_lat) * 0.5) as f64;
                        let mid_lon = ((min_lon + max_lon) * 0.5) as f64;
                        Some(match self.viirs.as_deref() {
                            Some(v) => v.sample_with_local(mid_lat, mid_lon, &mut viirs_local)?,
                            None => (0.75, 0.35),
                        })
                    } else {
                        None
                    };

                    for (seg_idx, pt) in pts[1..].iter().enumerate() {
                        let lat1 =
                            f32::from_bits(u32::from_le_bytes([pt[0], pt[1], pt[2], pt[3]])) as f64;
                        let lon1 =
                            f32::from_bits(u32::from_le_bytes([pt[4], pt[5], pt[6], pt[7]])) as f64;
                        let seg_lat = (prev_lat + lat1) * 0.5;
                        let seg_lon = (prev_lon + lon1) * 0.5;
                        let dlat = lat1 - prev_lat;
                        let dlon = lon1 - prev_lon;
                        let a = prev_proj;
                        let z = project(lat1, lon1);
                        let seg_len = ((z.0 - a.0).powi(2) + (z.1 - a.1).powi(2)).sqrt();
                        let (intensity, urbanity) = match uniform_viirs {
                            Some(sample) => sample,
                            None => match self.viirs.as_deref() {
                                Some(v) => {
                                    v.sample_with_local(seg_lat, seg_lon, &mut viirs_local)?
                                }
                                None => (0.75, 0.35),
                            },
                        };
                        prev_lat = lat1;
                        prev_lon = lon1;
                        prev_proj = z;
                        let rural_gate = if tier < 0.55 && intensity < 0.25 {
                            (intensity / 0.25).powi(2).max(0.04)
                        } else {
                            1.0
                        };
                        let corridor = sample_corridor_lighting(
                            seg_lat, seg_lon, dlat, dlon, id, tier, intensity, urbanity,
                        );
                        let color = lamp_palette(corridor.kelvin, corridor.chem);
                        let tier_weight = if tier >= 0.85 {
                            0.62
                        } else if tier >= 0.58 {
                            0.82
                        } else if tier >= 0.35 {
                            0.52
                        } else {
                            0.26
                        };
                        let cbd_boost = if intensity > 0.68 && urbanity > 0.50 {
                            1.0 + 0.45 * ((intensity - 0.68) / 0.32).clamp(0.0, 1.0) * urbanity
                        } else {
                            1.0
                        };
                        let power = tier_weight
                            * corridor.gain
                            * cbd_boost
                            * exp_gain
                            * (0.16 + 0.84 * intensity)
                            * rural_gate
                            * (0.18 + 0.24 * urbanity);
                        if seg_idx == 0 && tier >= 0.42 {
                            let jkey = (((first_proj.0 * 0.25).round() as i32 as u32 as u64) << 32)
                                | ((first_proj.1 * 0.25).round() as i32 as u32 as u64);
                            if seen_junctions.insert(jkey) {
                                draw_hotspot_rgb(
                                    &mut rgb,
                                    day_rgb,
                                    size,
                                    first_proj,
                                    halo_r * 1.35,
                                    power * (0.48 + 0.28 * corridor.gain.min(1.6)),
                                    color,
                                );
                            }
                        }
                        last_power = power;
                        last_color = color;
                        let seg_seed = h ^ ((seg_idx as u64).wrapping_mul(0x9e3779b97f4a7c15));
                        draw_rgb(
                            &mut rgb,
                            day_rgb,
                            size,
                            a,
                            z,
                            core_r,
                            halo_r,
                            skirt_r,
                            power,
                            color,
                            arc_px / lamp_period_px,
                            1.0 / lamp_period_px,
                            tier,
                            seg_seed,
                        );
                        arc_px += seg_len;
                    }
                    if tier >= 0.42 && last_power > 0.0 {
                        let jkey = (((prev_proj.0 * 0.25).round() as i32 as u32 as u64) << 32)
                            | ((prev_proj.1 * 0.25).round() as i32 as u32 as u64);
                        if seen_junctions.insert(jkey) {
                            draw_hotspot_rgb(
                                &mut rgb,
                                day_rgb,
                                size,
                                prev_proj,
                                halo_r * 1.35,
                                last_power * 0.58,
                                last_color,
                            );
                        }
                    }
                }
            }
        }
        if let (Some(day), Some(viirs)) = (day_rgb, self.viirs.as_deref()) {
            let n = size as usize;
            let exp_gain = exposure * 2.05;
            let mut viirs_row = vec![
                (
                    0.0_f32,
                    0.0_f32,
                    DistrictLighting {
                        kelvin: 0.35,
                        chem: 0.0,
                        gain: 1.0,
                    }
                );
                n.div_ceil(8)
            ];
            for y in 0..n {
                if y % 8 == 0 {
                    let lat =
                        b.north - ((y as f64 + 4.0).min(n as f64) / n as f64) * (b.north - b.south);
                    for (bx, slot) in viirs_row.iter_mut().enumerate() {
                        let lon =
                            b.west + (((bx * 8 + 4).min(n) as f64) / n as f64) * (b.east - b.west);
                        let (intensity, urbanity) =
                            viirs.sample_with_local(lat, lon, &mut viirs_local)?;
                        let district = sample_district_lighting(lat, lon, intensity, urbanity);
                        *slot = (intensity, urbanity, district);
                    }
                }
                let row_offset = y * n;
                let gy = (footprint.row as f64) * (n as f64) + (y as f64);
                let gx_base = (footprint.col as f64) * (n as f64);
                for x in 0..n {
                    let (intensity, urbanity, district) = viirs_row[x / 8];
                    let urban_core = intensity * urbanity.sqrt();
                    // Restrict unanchored block-interior architectural glints to genuine high-radiance
                    // metropolitan cores (~55+ nW/cm^2/sr) so small cities, industrial parks, and
                    // bright prairie/warehouse ortho surfaces never glow outside road corridors.
                    if urban_core <= 0.45 {
                        continue;
                    }
                    let prof = ortho_surface_profile(day, n, x, y);
                    let has_canyon_edge = (prof.canyon > 0.18 && prof.grad > 0.045)
                        || (prof.canyon > 0.08 && prof.grad > 0.085);
                    if !has_canyon_edge {
                        continue;
                    }
                    let arch_struct = (prof.canyon * 0.85 + (prof.grad - 0.055).max(0.0) * 1.80)
                        .clamp(0.0, 1.0)
                        * (1.0 - (1.0 - prof.core_atten) * 1.2).clamp(0.0, 1.0);
                    if arch_struct <= 0.06 {
                        continue;
                    }
                    let gx = gx_base + (x as f64);
                    let raw_bldg = 0.58
                        * value_noise_2d(gx / 18.0, gy / 18.0, 0x517c_c1b7_2722_0a95)
                        + 0.42 * value_noise_2d(gx / 6.5, gy / 6.5, 0x3c6e_f372_fe94_f82a);
                    let u_bldg = (0.5 + (raw_bldg - 0.5) * 1.70).clamp(0.0, 1.0);
                    let cell_h =
                        mix64((((x as u64 / 2) << 32) | (y as u64 / 2)) ^ 0x9e3779b97f4a7c15);
                    let px_h = mix64(((x as u64) << 32) | (y as u64));
                    let u_cell = ((cell_h & 0xFFFF) as f32) / 65535.0;
                    let u_px = ((px_h & 0xFFFF) as f32) / 65535.0;
                    let core_t = ((urban_core - 0.45) / 0.45).clamp(0.0, 1.0);
                    let core_smooth = core_t * core_t * (3.0 - 2.0 * core_t);
                    let active_thresh =
                        (0.86 - 0.48 * core_smooth - 0.16 * core_smooth * arch_struct)
                            .clamp(0.22, 0.86);
                    if u_bldg < active_thresh || u_cell <= 0.54 {
                        continue;
                    }
                    let bldg_intensity =
                        ((u_bldg - active_thresh) / (1.0 - active_thresh).max(0.1)).clamp(0.0, 1.0);
                    let edge_boost =
                        ((prof.grad - 0.050).max(0.0) * 3.2 + prof.canyon * 1.05).clamp(0.0, 1.45);
                    let window_spark = (0.38 + 1.35 * u_px * u_px) * edge_boost;
                    let cbd_t = ((urban_core - 0.52) / 0.42).clamp(0.0, 1.0);
                    let core_gain = 0.45 + 1.45 * cbd_t * cbd_t * (0.70 + 0.30 * arch_struct);
                    let w = exp_gain
                        * urban_core
                        * core_gain
                        * bldg_intensity
                        * (0.006 * arch_struct + 0.034 * arch_struct * window_spark);
                    let local_kelvin =
                        (district.kelvin * 0.70 + (u_bldg - 0.45) * 0.38 + prof.chroma_cool * 0.18)
                            .clamp(0.08, 0.88);
                    let arch_color = lamp_palette(local_kelvin, district.chem * 0.5);
                    let dst = &mut rgb[row_offset + x];
                    dst[0] += w * arch_color[0] * prof.tint[0];
                    dst[1] += w * arch_color[1] * prof.tint[1];
                    dst[2] += w * arch_color[2] * prof.tint[2];
                }
            }
        }
        for px in &mut rgb {
            let lum = 0.299 * px[0] + 0.587 * px[1] + 0.114 * px[2];
            if lum > 0.0001 {
                let mapped = lum / (1.0 + 0.62 * lum);
                let scale = mapped / lum;
                let desat = ((lum - 0.78) / 1.55).clamp(0.0, 0.18);
                let cool_ratio = ((px[2] - px[0] * 0.82) / lum.max(0.05)).clamp(-1.0, 1.0);
                let white_r = mapped * (0.93 - 0.11 * cool_ratio);
                let white_g = mapped * (0.92 + 0.05 * cool_ratio);
                let white_b = mapped * (0.76 + 0.24 * cool_ratio);
                px[0] = (px[0] * scale * (1.0 - desat) + white_r * desat).clamp(0.0, 1.0);
                px[1] = (px[1] * scale * (1.0 - desat) + white_g * desat).clamp(0.0, 1.0);
                px[2] = (px[2] * scale * (1.0 - desat) + white_b * desat).clamp(0.0, 1.0);
            }
        }
        Ok(rgb)
    }
}

#[derive(Clone, Copy)]
struct DistrictLighting {
    kelvin: f32,
    chem: f32,
    gain: f32,
}

#[inline]
fn value_noise_2d(x: f64, y: f64, seed: u64) -> f32 {
    let xi = x.floor() as i64;
    let yi = y.floor() as i64;
    let tx = (x - xi as f64) as f32;
    let ty = (y - yi as f64) as f32;
    let ux = tx * tx * (3.0 - 2.0 * tx);
    let uy = ty * ty * (3.0 - 2.0 * ty);
    let cell = |gx: i64, gy: i64| -> f32 {
        let h = mix64(
            (gx as u64)
                .wrapping_mul(0x9e3779b97f4a7c15)
                .wrapping_add((gy as u64).wrapping_mul(0xbf58476d1ce4e5b9))
                ^ seed,
        );
        ((h & 0xFFFF_FFFF) as f32) * (1.0 / 4_294_967_295.0)
    };
    let c00 = cell(xi, yi);
    let c10 = cell(xi + 1, yi);
    let c01 = cell(xi, yi + 1);
    let c11 = cell(xi + 1, yi + 1);
    let a = c00 + (c10 - c00) * ux;
    let b = c01 + (c11 - c01) * ux;
    a + (b - a) * uy
}

#[allow(clippy::too_many_arguments)]
#[inline]
fn sample_corridor_lighting(
    lat: f64,
    lon: f64,
    dlat: f64,
    dlon: f64,
    road_id: u64,
    tier: f32,
    intensity: f32,
    urbanity: f32,
) -> DistrictLighting {
    let cos_lat = lat.to_radians().cos().abs().max(0.2);
    let y_km = lat * 111.32;
    let x_km = lon * 111.32 * cos_lat;
    let dx = (dlon * cos_lat) as f32;
    let dy = dlat as f32;
    let len = (dx * dx + dy * dy).sqrt();
    let (ux, uy) = if len > 1e-7 {
        let (mut tx, mut ty) = (dx / len, dy / len);
        if ty < 0.0 || (ty == 0.0 && tx < 0.0) {
            tx = -tx;
            ty = -ty;
        }
        (tx as f64, ty as f64)
    } else {
        (1.0, 0.0)
    };
    // Corridor-aligned coordinates:
    // d_perp changes every ~55-75m across parallel streets/avenues,
    // while s_along runs continuously for kilometers along the same street line.
    let d_perp = x_km * (-uy) + y_km * ux;
    let s_along = x_km * ux + y_km * uy;
    let angle_family = ((uy.atan2(ux) / std::f64::consts::PI) * 4.0).round() as u64 & 3;
    let tier_seed = if tier >= 0.85 {
        0x7140_5926_5358_9793_u64
    } else if tier >= 0.58 {
        0x2718_2818_2845_9045_u64 ^ angle_family.wrapping_mul(0x9e3779b97f4a7c15)
    } else if tier >= 0.35 {
        0x1618_0339_8874_9895_u64 ^ angle_family.wrapping_mul(0xbf58476d1ce4e5b9)
    } else {
        0x517c_c1b7_2722_0a95_u64 ^ angle_family.wrapping_mul(0x94d049bb133111eb)
    };

    let n_corridor = value_noise_2d(d_perp / 0.062, s_along / 1.75, tier_seed);
    let n_district = value_noise_2d(x_km / 1.45, y_km / 1.45, 0x3141_5926_5358_9793);
    let id_jitter = (((mix64(road_id) & 0xFFFF) as f32) / 65535.0) - 0.5;

    let raw_k = 0.66 * n_corridor + 0.24 * n_district + 0.10 * (0.5 + id_jitter * 0.5);
    let centered_k = (raw_k - 0.50) * 3.60;
    let s_k = centered_k / (1.0 + centered_k.abs() * 0.48);

    let kelvin = if tier >= 0.85 {
        // Highways: distinct high-pressure sodium amber corridors vs modern neutral-white LED corridors
        (0.46 + 0.56 * s_k).clamp(0.14, 0.90)
    } else if tier >= 0.58 {
        // Primary avenues: strong street-by-street separation between golden HPS, warm commercial white, and crisp LED
        let cbd = if intensity > 0.72 && urbanity > 0.52 {
            0.08
        } else {
            0.0
        };
        (0.48 + 0.62 * s_k + cbd).clamp(0.10, 0.94)
    } else if tier >= 0.35 {
        // Secondary collector streets: slightly warmer bias with distinct retrofit avenues
        (0.40 + 0.58 * s_k).clamp(0.08, 0.88)
    } else {
        // Residential streets: predominantly warm sodium/amber (2000K-2700K), with ~20% LED retrofit streets
        if s_k > 0.36 {
            (0.58 + 0.45 * (s_k - 0.36)).clamp(0.54, 0.82)
        } else {
            (0.22 + 0.32 * s_k + id_jitter * 0.08).clamp(0.06, 0.42)
        }
    };

    let n_chem =
        0.65 * value_noise_2d(
            d_perp / 0.068,
            s_along / 1.20,
            tier_seed ^ 0x6a09_e667_f3bc_c908,
        ) + 0.35 * value_noise_2d(x_km / 0.38, y_km / 0.38, 0xbb67_ae85_84ca_a73b);
    let chem = ((n_chem - 0.50) * 2.70).clamp(-1.0, 1.0);

    // Longitudinal commercial/retail frontage pockets and mid-corridor quiet stretches
    let n_frontage = value_noise_2d(
        d_perp / 0.070,
        s_along / 0.24,
        tier_seed ^ 0xa5a5_5a5a_f0f0_0f0f,
    );
    let n_pocket = value_noise_2d(
        d_perp / 0.052,
        s_along / 0.085,
        tier_seed ^ 0x1357_9bdf_2468_ace0,
    );
    let act = 0.60 * n_frontage + 0.40 * n_pocket;
    let gain = if tier >= 0.85 {
        // Highways are mostly dark between interchanges and urban junctions
        0.36 + 0.88 * act * act * act * (0.55 + 0.45 * urbanity)
    } else if tier >= 0.58 {
        // Primary commercial corridors surge brightly at retail strips and dip on quieter blocks
        0.42 + 1.85 * act * act * (0.65 + 0.35 * urbanity)
    } else if tier >= 0.35 {
        0.34 + 1.65 * act * act * (0.60 + 0.40 * urbanity)
    } else {
        // Residential side streets have dark tree-shaded stretches alternating with warm clusters
        0.22 + 1.20 * act * act * act
    };

    DistrictLighting { kelvin, chem, gain }
}

#[inline]
fn sample_district_lighting(lat: f64, lon: f64, intensity: f32, urbanity: f32) -> DistrictLighting {
    let y_km = lat * 111.32;
    let x_km = lon * 111.32 * lat.to_radians().cos().abs().max(0.2);
    let n_borough = value_noise_2d(x_km / 1.90, y_km / 1.90, 0x3141_5926_5358_9793);
    let n_hood = value_noise_2d(x_km / 0.48, y_km / 0.48, 0x2718_2818_2845_9045);
    let n_precinct = value_noise_2d(x_km / 0.14, y_km / 0.14, 0x1618_0339_8874_9895);
    let raw_spatial = 0.30 * n_borough + 0.48 * n_hood + 0.22 * n_precinct;
    let contrast_gain = 3.25 + 0.65 * (intensity * urbanity).clamp(0.0, 1.0);
    let centered = (raw_spatial - 0.50) * contrast_gain;
    let s_curve = centered / (1.0 + centered.abs() * 0.50);
    let cbd_shift = if intensity > 0.72 && urbanity > 0.55 && s_curve > 0.05 {
        0.10 * urbanity
    } else if intensity < 0.35 {
        -0.10 * (1.0 - intensity / 0.35)
    } else {
        -0.04
    };
    let kelvin = (0.44 + 0.62 * s_curve + cbd_shift).clamp(0.0, 1.0);
    let n_chem = 0.62 * value_noise_2d(x_km / 0.42, y_km / 0.42, 0x6a09_e667_f3bc_c908)
        + 0.38 * value_noise_2d(x_km / 0.14, y_km / 0.14, 0xbb67_ae85_84ca_a73b);
    let chem = ((n_chem - 0.50) * 2.80).clamp(-1.0, 1.0);
    DistrictLighting {
        kelvin,
        chem,
        gain: 1.0,
    }
}

#[inline]
fn lamp_palette(kelvin: f32, chem: f32) -> [f32; 3] {
    let k = kelvin.clamp(0.0, 1.0);
    let base = if k < 0.34 {
        let t = k / 0.34;
        [1.00, 0.46 + 0.20 * t, 0.08 + 0.16 * t]
    } else if k < 0.58 {
        let t = (k - 0.34) / 0.24;
        [1.00, 0.66 + 0.18 * t, 0.24 + 0.36 * t]
    } else if k < 0.78 {
        let t = (k - 0.58) / 0.20;
        [1.00 - 0.14 * t, 0.84 + 0.10 * t, 0.60 + 0.40 * t]
    } else {
        let t = (k - 0.78) / 0.22;
        [0.86 - 0.12 * t, 0.94 - 0.02 * t, 1.00]
    };
    if k > 0.46 && chem > 0.15 {
        let hg = ((k - 0.46) / 0.54).clamp(0.0, 1.0) * ((chem - 0.15) / 0.85).clamp(0.0, 1.0);
        [
            base[0] * (1.0 - 0.22 * hg),
            (base[1] + 0.08 * hg).min(1.0),
            base[2] * (1.0 - 0.14 * hg),
        ]
    } else {
        base
    }
}

#[inline]
fn mix64(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

struct SurfaceProfile {
    core_atten: f32,
    albedo: f32,
    urban_fill: f32,
    grad: f32,
    canyon: f32,
    chroma_cool: f32,
    tint: [f32; 3],
}

#[inline]
fn ortho_surface_profile(day: &[[f32; 3]], size: usize, x: usize, y: usize) -> SurfaceProfile {
    let idx = y * size + x;
    let [dr, dg, db] = day[idx];
    let luma = 0.299 * dr + 0.587 * dg + 0.114 * db;
    let xr = (x + 2).min(size - 1);
    let yd = (y + 2).min(size - 1);
    let [rr, rg, rb] = day[y * size + xr];
    let [br, bg, bb] = day[yd * size + x];
    let luma_r = 0.299 * rr + 0.587 * rg + 0.114 * rb;
    let luma_d = 0.299 * br + 0.587 * bg + 0.114 * bb;
    let grad = (luma - luma_r).abs() + (luma - luma_d).abs();

    // Sample a wider 6-pixel (~10m) neighborhood so we can distinguish open water bodies
    // (uniformly dark & smooth) from urban skyscraper shadow canyons (dark Rayleigh-tinted
    // shadow immediately adjacent to bright building roofs/walls/streets).
    let xl6 = x.saturating_sub(6);
    let xr6 = (x + 6).min(size - 1);
    let yu6 = y.saturating_sub(6);
    let yd6 = (y + 6).min(size - 1);
    let luma_at = |px: [f32; 3]| 0.299 * px[0] + 0.587 * px[1] + 0.114 * px[2];
    let l_l6 = luma_at(day[y * size + xl6]);
    let l_r6 = luma_at(day[y * size + xr6]);
    let l_u6 = luma_at(day[yu6 * size + x]);
    let l_d6 = luma_at(day[yd6 * size + x]);
    let max_near = luma
        .max(luma_r)
        .max(luma_d)
        .max(l_l6)
        .max(l_r6)
        .max(l_u6)
        .max(l_d6);
    let min_near = luma
        .min(luma_r)
        .min(luma_d)
        .min(l_l6)
        .min(l_r6)
        .min(l_u6)
        .min(l_d6);
    let span6 = max_near - min_near;

    let is_water =
        (db > dr * 1.06 && db >= dg * 0.95 && luma < 0.22 && max_near < 0.25 && span6 < 0.065)
            || (dg > dr * 1.09 && db > dr * 1.07 && luma < 0.37 && grad < 0.055 && span6 < 0.075);
    if is_water {
        return SurfaceProfile {
            core_atten: 0.18,
            albedo: 0.03,
            urban_fill: 0.0,
            grad,
            canyon: 0.0,
            chroma_cool: 0.0,
            tint: [0.85, 0.90, 1.0],
        };
    }
    let green_dom = (dg - dr.max(db * 0.94)).max(0.0);
    let green_rel = ((dg - dr.max(db * 0.94)) / dg.max(0.06)).max(0.0);
    let canopy_raw = (green_dom * 6.5)
        .max((green_rel - 0.055).max(0.0) * 7.5)
        .clamp(0.0, 1.0);
    let canopy = canopy_raw * (1.0 - (luma - 0.22).max(0.0) * 3.2).clamp(0.0, 1.0);
    let core_atten = 1.0 - 0.62 * canopy;
    let non_canopy = 1.0 - canopy;

    // Skyscraper / building shadow canyon detection:
    // Daytime cast shadows next to high-contrast building edges are not dark at night;
    // they mark tall building bases/canyons where street and lobby light bounce strongly.
    let canyon = if luma < 0.22 && canopy < 0.30 && span6 > 0.065 {
        (((0.22 - luma) * 4.5).clamp(0.0, 1.0) * ((span6 - 0.065) * 4.2).clamp(0.0, 1.0))
            * non_canopy
    } else {
        0.0
    };
    let eff_luma = luma.max(0.32 * canyon);
    let eff_grad = grad.max(0.22 * canyon);

    let albedo = ((0.32 + 1.45 * eff_luma + 0.72 * eff_grad.min(0.38)).clamp(0.26, 1.60))
        * (1.0 - 0.80 * canopy);
    let urban_fill = if (eff_luma > 0.18 || canyon > 0.12) && canopy < 0.42 {
        let structure =
            ((eff_luma - 0.19).max(0.0) * 1.15 + eff_grad.min(0.38) * 1.50 + canyon * 0.65)
                .clamp(0.0, 1.45);
        structure * non_canopy * non_canopy
    } else {
        0.0
    };
    let chroma_cool =
        ((db - dr * 0.94) / luma.max(0.08)).clamp(-0.65, 0.65) * (1.0 - 0.75 * canyon);
    let inv_luma = if luma > 0.03 {
        (0.24 / luma).min(1.6)
    } else {
        0.0
    };
    let raw_tint = [
        0.74 + (dr * inv_luma).clamp(0.0, 0.42),
        0.74 + (dg * inv_luma).clamp(0.0, 0.42),
        0.74 + (db * inv_luma).clamp(0.0, 0.42),
    ];
    // Neutralize daytime Rayleigh blue shadow cast inside urban shadow canyons
    let tint = [
        raw_tint[0] * (1.0 - 0.65 * canyon) + 0.98 * (0.65 * canyon),
        raw_tint[1] * (1.0 - 0.65 * canyon) + 0.95 * (0.65 * canyon),
        raw_tint[2] * (1.0 - 0.65 * canyon) + 0.90 * (0.65 * canyon),
    ];
    SurfaceProfile {
        core_atten,
        albedo,
        urban_fill,
        grad,
        canyon,
        chroma_cool,
        tint,
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_hotspot_rgb(
    image: &mut [[f32; 3]],
    day_rgb: Option<&[[f32; 3]]>,
    size: u32,
    center: (f32, f32),
    radius: f32,
    energy: f32,
    color: [f32; 3],
) {
    let min_x = (center.0 - radius - 1.0).floor().max(0.0) as u32;
    let max_x = (center.0 + radius + 1.0).ceil().max(0.0).min(size as f32) as u32;
    let min_y = (center.1 - radius - 1.0).floor().max(0.0) as u32;
    let max_y = (center.1 + radius + 1.0).ceil().max(0.0).min(size as f32) as u32;
    let max_dist = radius + 0.5;
    let max_dist2 = max_dist * max_dist;
    let inv_max = 1.0 / max_dist.max(1.0);
    let hot_color = [
        (color[0] * 0.88 + 0.12).min(1.0),
        (color[1] * 0.88 + 0.12).min(1.0),
        (color[2] * 0.88 + 0.12).min(1.0),
    ];
    let n = size as usize;
    for y in min_y..max_y {
        let dy = y as f32 - center.1;
        let row_offset = (y as usize) * n;
        for x in min_x..max_x {
            let dx = x as f32 - center.0;
            let dist2 = dx * dx + dy * dy;
            if dist2 >= max_dist2 {
                continue;
            }
            let norm = (1.0 - dist2.sqrt() * inv_max).clamp(0.0, 1.0);
            let falloff = norm * norm * norm;
            let idx = row_offset + x as usize;
            let (atten, tint) = match day_rgb {
                Some(day) => {
                    let prof = ortho_surface_profile(day, n, x as usize, y as usize);
                    (
                        (0.55 * prof.core_atten + 0.45 * prof.albedo) * falloff,
                        prof.tint,
                    )
                }
                None => (falloff * 0.85, [1.0, 1.0, 1.0]),
            };
            let w = energy * atten;
            let dst = &mut image[idx];
            dst[0] += w * hot_color[0] * tint[0];
            dst[1] += w * hot_color[1] * tint[1];
            dst[2] += w * hot_color[2] * tint[2];
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_rgb(
    image: &mut [[f32; 3]],
    day_rgb: Option<&[[f32; 3]]>,
    size: u32,
    a: (f32, f32),
    b: (f32, f32),
    core_r: f32,
    halo_r: f32,
    skirt_r: f32,
    energy: f32,
    color: [f32; 3],
    phase: f32,
    inv_lamp_period: f32,
    tier: f32,
    seg_seed: u64,
) {
    let min_x = (a.0.min(b.0) - skirt_r - 1.0).floor().max(0.0) as u32;
    let max_x = (a.0.max(b.0) + skirt_r + 1.0)
        .ceil()
        .max(0.0)
        .min(size as f32) as u32;
    let min_y = (a.1.min(b.1) - skirt_r - 1.0).floor().max(0.0) as u32;
    let max_y = (a.1.max(b.1) + skirt_r + 1.0)
        .ceil()
        .max(0.0)
        .min(size as f32) as u32;
    let vx = b.0 - a.0;
    let vy = b.1 - a.1;
    let length2 = vx * vx + vy * vy;
    let inv_length2 = if length2 > 0.0 { 1.0 / length2 } else { 0.0 };
    let length = length2.sqrt();
    let max_dist = skirt_r + 0.5;
    let max_dist2 = max_dist * max_dist;
    let inv_skirt_span = 1.0 / (max_dist - core_r).max(0.75);
    let inv_halo_span = 1.0 / (halo_r + 0.5).max(0.75);
    let inv_core_span = 1.0 / (core_r + 0.45).max(0.65);
    let t_freq = length * inv_lamp_period;
    let n = size as usize;
    for y in min_y..max_y {
        let dy_a = y as f32 - a.1;
        let row_offset = (y as usize) * n;
        for x in min_x..max_x {
            let dx_a = x as f32 - a.0;
            let t = if length2 > 0.0 {
                ((dx_a * vx + dy_a * vy) * inv_length2).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let px_dx = dx_a - t * vx;
            let px_dy = dy_a - t * vy;
            let dist2 = px_dx * px_dx + px_dy * px_dy;
            if dist2 >= max_dist2 {
                continue;
            }
            let distance = dist2.sqrt();
            let skirt_norm = ((max_dist - distance) * inv_skirt_span).clamp(0.0, 1.0);
            let skirt_sq = skirt_norm * skirt_norm;
            let skirt = skirt_sq * skirt_sq;
            if skirt <= 0.0 {
                continue;
            }
            let halo_norm = ((halo_r + 0.5 - distance) * inv_halo_span).clamp(0.0, 1.0);
            let halo = halo_norm * halo_norm * halo_norm;
            let core_norm = ((core_r + 0.45 - distance) * inv_core_span).clamp(0.0, 1.0);
            let core = core_norm * core_norm;

            // Organic per-lamp and per-block modulation along the street corridor:
            // Raised-cosine pool (0.5 - 0.5 * cos(frac * TAU)) is 0 with 0 derivative at integer
            // lamp boundaries, allowing each individual lamp pole to vary in amplitude and tint seamlessly.
            let (lamp_mod, lamp_warm_shift) = if length > 0.5 {
                let u = phase + t * t_freq;
                let lamp_idx = u.floor() as i64;
                let frac = u - lamp_idx as f32;
                let pool = 0.5 - 0.5 * (frac * std::f32::consts::TAU).cos();
                let h_lamp = mix64(seg_seed ^ (lamp_idx as u64).wrapping_mul(0x9e3779b97f4a7c15));
                let u_amp = ((h_lamp & 0xFFFF) as f32) * (1.0 / 65535.0);
                let u_tint = (((h_lamp >> 16) & 0xFFFF) as f32) * (1.0 / 65535.0) - 0.5;
                let mod_val = if tier < 0.35 {
                    let pole_gain = if u_amp < 0.18 {
                        0.08
                    } else {
                        0.35 + 1.55 * u_amp * u_amp
                    };
                    0.12 + pole_gain * pool * pool
                } else if tier < 0.58 {
                    let pole_gain = 0.35 + 1.45 * u_amp * u_amp;
                    0.26 + 0.90 * pole_gain * pool
                } else if tier < 0.85 {
                    let pole_gain = 0.44 + 1.25 * u_amp * u_amp;
                    0.38 + 0.78 * pole_gain * pool
                } else {
                    let pole_gain = 0.38 + 1.15 * u_amp * u_amp;
                    0.24 + 0.72 * pole_gain * pool * pool
                };
                (mod_val, u_tint * pool)
            } else {
                (1.0, 0.0)
            };
            let idx = row_offset + x as usize;
            let (w_core, w_outer, tint) = match day_rgb {
                Some(day) => {
                    let prof = ortho_surface_profile(day, n, x as usize, y as usize);
                    let wc = 1.18 * core * prof.core_atten * lamp_mod * energy;
                    let wo =
                        (0.54 * halo * (0.45 * prof.core_atten + 0.55 * prof.albedo) * lamp_mod
                            + 0.20
                                * skirt
                                * (0.35 * prof.albedo + 0.65 * prof.urban_fill)
                                * (0.45 + 0.55 * lamp_mod))
                            * energy;
                    (wc, wo, prof.tint)
                }
                None => {
                    let wc = 1.10 * core * lamp_mod * energy;
                    let wo = (0.48 * halo + 0.12 * skirt) * lamp_mod * energy;
                    (wc, wo, [1.0, 1.0, 1.0])
                }
            };
            let c_r = (color[0] + lamp_warm_shift * 0.14).clamp(0.0, 1.0);
            let c_g = (color[1] + lamp_warm_shift * 0.04).clamp(0.0, 1.0);
            let c_b = (color[2] - lamp_warm_shift * 0.18).clamp(0.0, 1.0);
            let coolness = ((c_b - c_r * 0.72) / 0.36).clamp(0.0, 1.0);
            let core_r = (c_r * 0.90 + 0.10).min(1.0);
            let core_g = (c_g * 0.88 + 0.12).min(1.0);
            let core_b = (c_b * 0.88 + 0.12).min(1.0);
            let outer_r = c_r * (1.0 - 0.18 * coolness);
            let outer_g = c_g * (0.82 + 0.16 * coolness);
            let outer_b = c_b * (0.56 + 0.44 * coolness);
            let dst = &mut image[idx];
            dst[0] += (w_core * core_r + w_outer * outer_r) * tint[0];
            dst[1] += (w_core * core_g + w_outer * outer_g) * tint[1];
            dst[2] += (w_core * core_b + w_outer * outer_b) * tint[2];
        }
    }
}

#[cfg(test)]
fn draw(image: &mut [f32], size: u32, a: (f32, f32), b: (f32, f32), radius: f32, energy: f32) {
    let min_x = (a.0.min(b.0) - radius - 1.0).floor().max(0.0) as u32;
    let max_x = (a.0.max(b.0) + radius + 1.0)
        .ceil()
        .max(0.0)
        .min(size as f32) as u32;
    let min_y = (a.1.min(b.1) - radius - 1.0).floor().max(0.0) as u32;
    let max_y = (a.1.max(b.1) + radius + 1.0)
        .ceil()
        .max(0.0)
        .min(size as f32) as u32;
    let vx = b.0 - a.0;
    let vy = b.1 - a.1;
    let length2 = vx * vx + vy * vy;
    for y in min_y..max_y {
        for x in min_x..max_x {
            let t = if length2 > 0.0 {
                (((x as f32 - a.0) * vx + (y as f32 - a.1) * vy) / length2).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let distance =
                ((x as f32 - a.0 - t * vx).powi(2) + (y as f32 - a.1 - t * vy).powi(2)).sqrt();
            let coverage = (radius + 0.5 - distance).clamp(0.0, 1.0).min(2.0 * radius);
            image[(y * size + x) as usize] += coverage * energy;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipped_and_subpixel_segments_keep_fractional_energy() {
        let mut image = vec![0.0; 64];
        draw(&mut image, 8, (-2.0, 3.0), (12.0, 3.0), 0.1, 0.1);
        let sum: f32 = image.iter().sum();
        assert!(sum > 0.0 && sum < 0.2);
        assert!(image.iter().all(|v| *v >= 0.0 && *v < 0.1));
    }

    #[test]
    fn rejects_truncated_and_expanding_radiance_payloads() -> Result<()> {
        use flate2::{Compression, write::ZlibEncoder};
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("viirs.tiles");
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        std::io::Write::write_all(&mut encoder, &[0; 100])?;
        let compressed = encoder.finish()?;
        let mut bytes = b"RDT1".to_vec();
        bytes.extend(1_u32.to_le_bytes());
        bytes.extend(1_u32.to_le_bytes());
        bytes.extend(25_f32.to_le_bytes());
        bytes.extend(36_i16.to_le_bytes());
        bytes.extend((-79_i16).to_le_bytes());
        bytes.extend((compressed.len() as u32).to_le_bytes());
        bytes.extend(32_u64.to_le_bytes());
        bytes.extend(compressed);
        std::fs::write(&path, &bytes)?;
        let viirs = Viirs::open(&path)?;
        assert!(viirs.factor(35.85, -78.7).is_err());
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        std::io::Write::write_all(&mut encoder, &65535_u16.to_le_bytes())?;
        let compressed = encoder.finish()?;
        let mut valid = bytes[..32].to_vec();
        valid[20..24].copy_from_slice(&(compressed.len() as u32).to_le_bytes());
        valid.extend(compressed);
        std::fs::write(&path, &valid)?;
        assert!((Viirs::open(&path)?.factor(35.85, -78.7)? - 1.0).abs() < 1e-5);
        valid.pop();
        let length = valid.len() as u32 - 32;
        valid[20..24].copy_from_slice(&length.to_le_bytes());
        std::fs::write(&path, &valid)?;
        assert!(Viirs::open(&path)?.factor(35.85, -78.7).is_err());
        std::fs::write(&path, &bytes[..20])?;
        assert!(Viirs::open(&path).is_err());
        Ok(())
    }

    #[test]
    fn packs_and_mmaps_rds1_and_renders_identically() -> Result<()> {
        let fixture = crate::fixtures::Fixture::new()?;
        let rds1_path = Path::new(&fixture.config.osm_db);
        let fp = Footprint::parse("25760_18432_BI16.dds")?;
        let mut from_rds1 = Lighting::open(rds1_path, None)?;
        let pixels = from_rds1.render(fp, 256, 0.35)?;
        assert!(pixels.iter().any(|p| p[0] > 0.0));
        Ok(())
    }

    #[test]
    fn extracts_and_renders_laminar_dsf_road_network() -> Result<()> {
        let roots = crate::dsf_compiler::discover_laminar_scenery_roots(None);
        if roots.is_empty() {
            return Ok(());
        }
        let dsf_rel = "Earth nav data/+40-130/+45-123.dsf";
        let Some(dsf_path) = roots.iter().map(|r| r.join(dsf_rel)).find(|p| p.is_file()) else {
            return Ok(());
        };
        let roads = crate::dsf_compiler::extract_laminar_dsf_roads(&dsf_path, 45, -123)?;
        assert!(
            roads.len() > 10_000,
            "expected >10k road chains in +45-123.dsf, got {}",
            roads.len()
        );
        let mut lighting = Lighting::open(&roots[0], None)?;
        let fp = Footprint::parse("23424_10432_BI16.dds")?;
        let pixels = lighting.render(fp, 256, 0.35)?;
        assert!(
            pixels.iter().any(|p| p[0] > 0.05),
            "expected non-zero road lighting in Portland footprint"
        );
        let concrete_day = vec![[0.65_f32, 0.65, 0.65]; 256 * 256];
        let canopy_day = vec![[0.10_f32, 0.26, 0.09]; 256 * 256];
        let water_day = vec![[0.04_f32, 0.07, 0.14]; 256 * 256];
        let concrete_lit = lighting.render_modulated(fp, 256, 0.35, Some(&concrete_day))?;
        let canopy_lit = lighting.render_modulated(fp, 256, 0.35, Some(&canopy_day))?;
        let water_lit = lighting.render_modulated(fp, 256, 0.35, Some(&water_day))?;
        let sum_energy = |img: &[[f32; 3]]| -> f32 { img.iter().map(|p| p[0] + p[1] + p[2]).sum() };
        assert!(sum_energy(&concrete_lit) > sum_energy(&canopy_lit) * 1.4);
        assert!(sum_energy(&canopy_lit) > sum_energy(&water_lit));
        Ok(())
    }

    #[test]
    fn district_lighting_produces_heterogeneous_warm_and_cool_neighborhoods() {
        let mut warm_hps = 0usize;
        let mut cool_led = 0usize;
        let mut mercury_green = 0usize;
        let mut high_gain = 0usize;
        let mut low_gain = 0usize;
        for iy in 0..24 {
            for ix in 0..24 {
                let lat = 40.70 + (iy as f64) * 0.004;
                let lon = -74.02 + (ix as f64) * 0.004;
                let d = sample_corridor_lighting(
                    lat,
                    lon,
                    0.001,
                    0.0005,
                    (iy * 24 + ix) as u64,
                    0.70,
                    0.85,
                    0.80,
                );
                let rgb = lamp_palette(d.kelvin, d.chem);
                if rgb[0] > 0.98 && rgb[2] < 0.35 {
                    warm_hps += 1;
                }
                if rgb[2] > rgb[0] && rgb[1] > rgb[0] {
                    cool_led += 1;
                }
                if rgb[1] > rgb[0] && rgb[1] > rgb[2] {
                    mercury_green += 1;
                }
                if d.gain > 1.25 {
                    high_gain += 1;
                }
                if d.gain < 0.70 {
                    low_gain += 1;
                }
            }
        }
        assert!(warm_hps > 20, "expected warm HPS corridors, got {warm_hps}");
        assert!(cool_led > 20, "expected cool LED corridors, got {cool_led}");
        assert!(
            mercury_green > 5,
            "expected mercury/metal-halide pockets, got {mercury_green}"
        );
        assert!(
            high_gain > 20 && low_gain > 20,
            "expected longitudinal corridor contrast, got high={high_gain} low={low_gain}"
        );

        // Verify skyscraper shadow canyons are not misclassified as open water, while flat
        // bright prairie/warehouse surfaces have zero canyon score and do not trigger interior fill.
        let mut canyon_day = vec![[0.05_f32, 0.08, 0.14]; 16 * 16];
        canyon_day[8 * 16 + 14] = [0.55, 0.54, 0.52];
        let prof_canyon = ortho_surface_profile(&canyon_day, 16, 8, 8);
        let water_day = vec![[0.05_f32, 0.08, 0.14]; 16 * 16];
        let prof_water = ortho_surface_profile(&water_day, 16, 8, 8);
        let mut prairie_day = vec![[0.58_f32, 0.52, 0.44]; 16 * 16];
        prairie_day[8 * 16 + 10] = [0.68, 0.62, 0.54];
        let prof_prairie = ortho_surface_profile(&prairie_day, 16, 8, 8);
        assert!(prof_canyon.canyon > 0.25);
        assert!(prof_canyon.albedo > prof_water.albedo * 10.0);
        assert!(prof_canyon.urban_fill > 0.20 && prof_water.urban_fill == 0.0);
        assert_eq!(prof_prairie.canyon, 0.0);
    }
}
