use crate::{
    model::{FxBuildHasher, FxHashMap},
    scenery,
};
use anyhow::{Context, Result, bail, ensure};
use md5::{Digest, Md5};
use std::{collections::BTreeMap, f64::consts::PI, path::Path, process::Command};

const SEVEN_Z_MAGIC: &[u8; 6] = b"7z\xbc\xaf\x27\x1c";
const DSF_HEADER: &[u8; 12] = b"XPLNEDSF\x01\0\0\0";

#[derive(Clone, Copy)]
struct LandVertex {
    lon: f64,
    lat: f64,
    z: f64,
    nx: f64,
    nz: f64,
    orig: Option<(u16, [u16; 5])>,
}

struct LandPool {
    planes: [Vec<u16>; 5],
    scal: [(f64, f64); 5],
}

struct OutLandPool {
    scal: [(f32, f32); 7],
    vertices: Vec<[u16; 7]>,
    dedup: FxHashMap<[u16; 7], u16>,
}

impl OutLandPool {
    fn new(scal5: [(f64, f64); 5]) -> Self {
        let mut scal = [(0.0_f32, 0.0_f32); 7];
        for i in 0..5 {
            scal[i] = (scal5[i].0 as f32, scal5[i].1 as f32);
        }
        scal[5] = (1.0, 0.0);
        scal[6] = (1.0, 0.0);
        Self {
            scal,
            vertices: Vec::with_capacity(8192),
            dedup: FxHashMap::with_capacity_and_hasher(8192, FxBuildHasher::default()),
        }
    }

    fn intern(&mut self, v: [u16; 7]) -> Option<u16> {
        if let Some(&idx) = self.dedup.get(&v) {
            return Some(idx);
        }
        if self.vertices.len() >= 65535 {
            return None;
        }
        let idx = self.vertices.len() as u16;
        self.vertices.push(v);
        self.dedup.insert(v, idx);
        Some(idx)
    }
}

struct DemGrid {
    west: f64,
    south: f64,
    width: usize,
    height: usize,
    scale: f64,
    offset: f64,
    samples: Vec<i16>,
}

impl DemGrid {
    fn sample(&self, lon: f64, lat: f64) -> f64 {
        if self.width < 2 || self.height < 2 || self.samples.len() < self.width * self.height {
            return 0.0;
        }
        let fx = ((lon - self.west).clamp(0.0, 1.0)) * (self.width - 1) as f64;
        let fy = ((lat - self.south).clamp(0.0, 1.0)) * (self.height - 1) as f64;
        let x0 = (fx.floor() as usize).min(self.width - 1);
        let y0 = (fy.floor() as usize).min(self.height - 1);
        let x1 = (x0 + 1).min(self.width - 1);
        let y1 = (y0 + 1).min(self.height - 1);
        let tx = fx - x0 as f64;
        let ty = fy - y0 as f64;
        let at = |x: usize, y: usize| -> f64 {
            let raw = self.samples[y * self.width + x];
            if raw == i16::MIN {
                0.0
            } else {
                raw as f64 * self.scale + self.offset
            }
        };
        let v00 = at(x0, y0);
        let v10 = at(x1, y0);
        let v01 = at(x0, y1);
        let v11 = at(x1, y1);
        let v0 = v00 + tx * (v10 - v00);
        let v1 = v01 + tx * (v11 - v01);
        v0 + ty * (v1 - v0)
    }
}

fn lon_to_tx12(lon: f64) -> f64 {
    (lon + 180.0) / 360.0 * 4096.0
}

fn tx12_to_lon(tx: f64) -> f64 {
    tx / 4096.0 * 360.0 - 180.0
}

fn lat_to_ty12(lat: f64) -> f64 {
    let r = lat.to_radians();
    (1.0 - (r.tan() + 1.0 / r.cos()).ln() / PI) * 0.5 * 4096.0
}

fn ty12_to_lat(ty: f64) -> f64 {
    let n = PI - 2.0 * PI * ty / 4096.0;
    (0.5 * (n.exp() - (-n).exp())).atan().to_degrees()
}

fn read_raw_dsf(path: &Path) -> Result<Vec<u8>> {
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if data.starts_with(SEVEN_Z_MAGIC) {
        let output = Command::new("7z")
            .args(["x", "-so"])
            .arg(path)
            .output()
            .context("spawning 7z to decompress Global Scenery DSF")?;
        ensure!(
            output.status.success(),
            "7z failed on {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(output.stdout)
    } else {
        Ok(data)
    }
}

fn decode_planar_u16(bytes: &[u8], pos: &mut usize, n_pts: usize) -> Result<Vec<u16>> {
    let enc = *bytes.get(*pos).context("truncated planar encoding byte")?;
    *pos += 1;
    let mut vals = Vec::with_capacity(n_pts);
    match enc {
        0 => {
            for _ in 0..n_pts {
                let b = bytes.get(*pos..*pos + 2).context("truncated raw plane")?;
                vals.push(u16::from_le_bytes([b[0], b[1]]));
                *pos += 2;
            }
        }
        1 => {
            while vals.len() < n_pts {
                let run = *bytes.get(*pos).context("truncated RLE plane")? as usize;
                *pos += 1;
                if (run & 128) != 0 {
                    let b = bytes.get(*pos..*pos + 2).context("truncated RLE repeat")?;
                    let v = u16::from_le_bytes([b[0], b[1]]);
                    *pos += 2;
                    let count = run & 127;
                    vals.extend(std::iter::repeat_n(v, count));
                } else {
                    for _ in 0..run {
                        let b = bytes.get(*pos..*pos + 2).context("truncated RLE literal")?;
                        vals.push(u16::from_le_bytes([b[0], b[1]]));
                        *pos += 2;
                    }
                }
            }
        }
        2 => {
            let mut prev = 0_u16;
            for _ in 0..n_pts {
                let b = bytes.get(*pos..*pos + 2).context("truncated diff plane")?;
                prev = prev.wrapping_add(u16::from_le_bytes([b[0], b[1]]));
                vals.push(prev);
                *pos += 2;
            }
        }
        3 => {
            let mut prev = 0_u16;
            while vals.len() < n_pts {
                let run = *bytes.get(*pos).context("truncated RLE-diff plane")? as usize;
                *pos += 1;
                if (run & 128) != 0 {
                    let b = bytes
                        .get(*pos..*pos + 2)
                        .context("truncated RLE-diff repeat")?;
                    let d = u16::from_le_bytes([b[0], b[1]]);
                    *pos += 2;
                    for _ in 0..(run & 127) {
                        prev = prev.wrapping_add(d);
                        vals.push(prev);
                    }
                } else {
                    for _ in 0..run {
                        let b = bytes
                            .get(*pos..*pos + 2)
                            .context("truncated RLE-diff literal")?;
                        let d = u16::from_le_bytes([b[0], b[1]]);
                        *pos += 2;
                        prev = prev.wrapping_add(d);
                        vals.push(prev);
                    }
                }
            }
        }
        other => bail!("unsupported DSF pool encoding {other}"),
    }
    ensure!(vals.len() == n_pts, "decoded plane length mismatch");
    Ok(vals)
}

fn parse_cell_origin(daeh_body: &[u8]) -> (f64, f64) {
    let mut west = -180.0;
    let mut south = -90.0;
    let mut pos = 0;
    while pos + 8 <= daeh_body.len() {
        let tag = &daeh_body[pos..pos + 4];
        let sz = u32::from_le_bytes(daeh_body[pos + 4..pos + 8].try_into().unwrap()) as usize;
        if sz < 8 || pos + sz > daeh_body.len() {
            break;
        }
        if tag == b"PORP" {
            let strings: Vec<_> = daeh_body[pos + 8..pos + sz]
                .split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .filter_map(|s| std::str::from_utf8(s).ok())
                .collect();
            for pair in strings.as_chunks::<2>().0 {
                if pair[0] == "sim/west" {
                    if let Ok(v) = pair[1].parse::<f64>() {
                        west = v;
                    }
                } else if pair[0] == "sim/south"
                    && let Ok(v) = pair[1].parse::<f64>()
                {
                    south = v;
                }
            }
        }
        pos += sz;
    }
    (west, south)
}

fn parse_first_dem(smed_body: &[u8], west: f64, south: f64) -> Option<DemGrid> {
    let mut pos = 0;
    let mut header: Option<(usize, usize, f64, f64)> = None;
    while pos + 8 <= smed_body.len() {
        let tag = &smed_body[pos..pos + 4];
        let sz = u32::from_le_bytes(smed_body[pos + 4..pos + 8].try_into().ok()?) as usize;
        if sz < 8 || pos + sz > smed_body.len() {
            break;
        }
        let body = &smed_body[pos + 8..pos + sz];
        if tag == b"IMED" && body.len() >= 20 {
            let bpp = body[1];
            let flags = u16::from_le_bytes(body[2..4].try_into().ok()?);
            let width = u32::from_le_bytes(body[4..8].try_into().ok()?) as usize;
            let height = u32::from_le_bytes(body[8..12].try_into().ok()?) as usize;
            let scale = f32::from_le_bytes(body[12..16].try_into().ok()?) as f64;
            let offset = f32::from_le_bytes(body[16..20].try_into().ok()?) as f64;
            if bpp == 2 && (flags & 3) == 1 {
                header = Some((width, height, scale, offset));
            }
        } else if tag == b"DMED"
            && let Some((width, height, scale, offset)) = header
            && body.len() == width * height * 2
        {
            let samples = body
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
            return Some(DemGrid {
                west,
                south,
                width,
                height,
                scale,
                offset,
                samples,
            });
        }
        pos += sz;
    }
    None
}

fn lerp_vertex(a: LandVertex, b: LandVertex, t: f64) -> LandVertex {
    let t = t.clamp(0.0, 1.0);
    let z = if (a.z <= -32760.0) && (b.z <= -32760.0) {
        -32768.0
    } else {
        a.z + t * (b.z - a.z)
    };
    LandVertex {
        lon: a.lon + t * (b.lon - a.lon),
        lat: a.lat + t * (b.lat - a.lat),
        z,
        nx: (a.nx + t * (b.nx - a.nx)).clamp(-1.0, 1.0),
        nz: (a.nz + t * (b.nz - a.nz)).clamp(-1.0, 1.0),
        orig: None,
    }
}

fn clip_half_plane<F>(poly: Vec<LandVertex>, dist: F, is_lon: bool, bound: f64) -> Vec<LandVertex>
where
    F: Fn(&LandVertex) -> f64,
{
    if poly.is_empty() {
        return poly;
    }
    let mut out = Vec::with_capacity(poly.len() + 2);
    let mut prev = *poly.last().unwrap();
    let mut prev_d = dist(&prev);
    for curr in poly {
        let curr_d = dist(&curr);
        let prev_in = prev_d >= -1e-11;
        let curr_in = curr_d >= -1e-11;
        if curr_in {
            if !prev_in {
                let denom = prev_d - curr_d;
                let t = if denom.abs() > 1e-18 {
                    prev_d / denom
                } else {
                    0.5
                };
                let mut mid = lerp_vertex(prev, curr, t);
                if is_lon {
                    mid.lon = bound;
                } else {
                    mid.lat = bound;
                }
                out.push(mid);
            }
            out.push(curr);
        } else if prev_in {
            let denom = prev_d - curr_d;
            let t = if denom.abs() > 1e-18 {
                prev_d / denom
            } else {
                0.5
            };
            let mut mid = lerp_vertex(prev, curr, t);
            if is_lon {
                mid.lon = bound;
            } else {
                mid.lat = bound;
            }
            out.push(mid);
        }
        prev = curr;
        prev_d = curr_d;
    }
    out
}

fn clip_triangle_to_box(
    tri: [LandVertex; 3],
    west: f64,
    east: f64,
    south: f64,
    north: f64,
) -> Vec<LandVertex> {
    let poly = vec![tri[0], tri[1], tri[2]];
    let poly = clip_half_plane(poly, |v| v.lon - west, true, west);
    let poly = clip_half_plane(poly, |v| east - v.lon, true, east);
    let poly = clip_half_plane(poly, |v| v.lat - south, false, south);
    clip_half_plane(poly, |v| north - v.lat, false, north)
}

fn sync_state(
    out: &mut Vec<u8>,
    out_pool: &mut Option<u16>,
    out_def: &mut Option<u32>,
    need_pool: Option<u16>,
    need_def: Option<u32>,
) {
    if let Some(pool) = need_pool
        && *out_pool != Some(pool)
    {
        out.push(1);
        out.extend_from_slice(&pool.to_le_bytes());
        *out_pool = Some(pool);
    }
    if let Some(def) = need_def
        && *out_def != Some(def)
    {
        if def <= u8::MAX as u32 {
            out.push(3);
            out.push(def as u8);
        } else if def <= u16::MAX as u32 {
            out.push(4);
            out.extend_from_slice(&(def as u16).to_le_bytes());
        } else {
            out.push(5);
            out.extend_from_slice(&def.to_le_bytes());
        }
        *out_def = Some(def);
    }
}

fn quantize_to_pool(v: LandVertex, scal: &[(f64, f64); 5]) -> [u16; 5] {
    let vals = [v.lon, v.lat, v.z, v.nx, v.nz];
    let mut out = [0_u16; 5];
    for i in 0..5 {
        let (scale, offset) = scal[i];
        let norm = if scale.abs() > 1e-12 {
            ((vals[i] - offset) / scale).clamp(0.0, 1.0)
        } else {
            0.0
        };
        out[i] = (norm * 65535.0).round() as u16;
    }
    out
}

fn intern_land_vertex(
    v: LandVertex,
    tx: u32,
    ty: u32,
    land_pools: &[LandPool],
    out_pools: &mut [OutLandPool],
) -> Result<(u16, u16)> {
    let u = (lon_to_tx12(v.lon) - tx as f64).clamp(0.0, 1.0);
    let v_tex = (1.0 - (lat_to_ty12(v.lat) - ty as f64)).clamp(0.0, 1.0);
    let q_u = (u * 65535.0).round() as u16;
    let q_v = (v_tex * 65535.0).round() as u16;

    if let Some((pool_idx, q5)) = v.orig {
        let p = pool_idx as usize;
        if let Some(pool) = out_pools.get_mut(p) {
            let q7 = [q5[0], q5[1], q5[2], q5[3], q5[4], q_u, q_v];
            if let Some(idx) = pool.intern(q7) {
                return Ok((pool_idx, idx));
            }
        }
    }

    let mut best_pool = 0_usize;
    let mut best_dist = f64::INFINITY;
    for (idx, lp) in land_pools.iter().enumerate() {
        if out_pools[idx].vertices.len() >= 65535 {
            continue;
        }
        let (slon, olon) = lp.scal[0];
        let (slat, olat) = lp.scal[1];
        let dx = if v.lon < olon {
            olon - v.lon
        } else if v.lon > olon + slon {
            v.lon - (olon + slon)
        } else {
            0.0
        };
        let dy = if v.lat < olat {
            olat - v.lat
        } else if v.lat > olat + slat {
            v.lat - (olat + slat)
        } else {
            0.0
        };
        let d = dx + dy;
        if d < best_dist {
            best_dist = d;
            best_pool = idx;
            if d == 0.0 {
                break;
            }
        }
    }

    let q5 = quantize_to_pool(v, &land_pools[best_pool].scal);
    let q7 = [q5[0], q5[1], q5[2], q5[3], q5[4], q_u, q_v];
    let idx = out_pools[best_pool]
        .intern(q7)
        .context("all 5-plane DSF pools exhausted 65535 vertices")?;
    Ok((best_pool as u16, idx))
}

#[derive(Default)]
struct TileTriangles {
    by_pool: BTreeMap<u16, Vec<[u16; 3]>>,
    xpool: Vec<[(u16, u16); 3]>,
}

impl TileTriangles {
    fn push(&mut self, tri: [(u16, u16); 3]) {
        if tri[0].0 == tri[1].0 && tri[1].0 == tri[2].0 {
            self.by_pool
                .entry(tri[0].0)
                .or_default()
                .push([tri[0].1, tri[1].1, tri[2].1]);
        } else {
            self.xpool.push(tri);
        }
    }
}

pub fn compile_global_dsf_to_ortho(input_path: &Path, output_path: &Path) -> Result<Vec<String>> {
    let raw = read_raw_dsf(input_path)?;
    ensure!(
        raw.len() >= 28 && &raw[..12] == DSF_HEADER,
        "invalid uncompressed DSF header in {}",
        input_path.display()
    );
    let end = raw.len() - 16;

    let mut daeh_atom: Option<&[u8]> = None;
    let mut nfed_other_subatoms: Vec<&[u8]> = Vec::new();
    let mut doeg_body: Option<&[u8]> = None;
    let mut sdmc_body: Option<&[u8]> = None;
    let mut smed_atom: Option<&[u8]> = None;
    let mut cell_west = -180.0;
    let mut cell_south = -90.0;
    let mut dem_grid: Option<DemGrid> = None;

    let mut pos = 12;
    while pos < end {
        ensure!(end - pos >= 8, "truncated top-level DSF atom");
        let tag = &raw[pos..pos + 4];
        let sz = u32::from_le_bytes(raw[pos + 4..pos + 8].try_into()?) as usize;
        ensure!(sz >= 8 && pos + sz <= end, "DSF atom out of bounds");
        let body = &raw[pos + 8..pos + sz];
        match tag {
            b"DAEH" => {
                daeh_atom = Some(&raw[pos..pos + sz]);
                (cell_west, cell_south) = parse_cell_origin(body);
            }
            b"NFED" => {
                let mut sub = 0;
                while sub < body.len() {
                    ensure!(body.len() - sub >= 8, "truncated NFED sub-atom");
                    let stag = &body[sub..sub + 4];
                    let ssz = u32::from_le_bytes(body[sub + 4..sub + 8].try_into()?) as usize;
                    ensure!(ssz >= 8 && sub + ssz <= body.len(), "NFED sub-atom OOB");
                    if stag != b"TRET" {
                        nfed_other_subatoms.push(&body[sub..sub + ssz]);
                    }
                    sub += ssz;
                }
            }
            b"DOEG" => doeg_body = Some(body),
            b"SDMC" => sdmc_body = Some(body),
            b"SMED" => {
                smed_atom = Some(&raw[pos..pos + sz]);
                dem_grid = parse_first_dem(body, cell_west, cell_south);
            }
            _ => {}
        }
        pos += sz;
    }

    let daeh_atom = daeh_atom.context("DSF missing DAEH atom")?;
    let doeg_body = doeg_body.context("DSF missing DOEG atom")?;
    let sdmc_body = sdmc_body.context("DSF missing SDMC atom")?;

    // Parse contiguous prefix of 5-plane land pools (LOOPs + LACSs) in DOEG.
    let mut raw_loops: Vec<[Vec<u16>; 5]> = Vec::new();
    let mut raw_scals: Vec<[(f64, f64); 5]> = Vec::new();
    let mut doeg_tail_start = 0;
    let mut sub = 0;
    while sub < doeg_body.len() {
        ensure!(doeg_body.len() - sub >= 8, "truncated DOEG sub-atom");
        let stag = &doeg_body[sub..sub + 4];
        let ssz = u32::from_le_bytes(doeg_body[sub + 4..sub + 8].try_into()?) as usize;
        ensure!(
            ssz >= 8 && sub + ssz <= doeg_body.len(),
            "DOEG sub-atom OOB"
        );
        if stag == b"LOOP" {
            if sub + 13 > doeg_body.len() || doeg_body[sub + 12] != 5 {
                break;
            }
            let n_pts = u32::from_le_bytes(doeg_body[sub + 8..sub + 12].try_into()?) as usize;
            let mut p = sub + 13;
            let p0 = decode_planar_u16(doeg_body, &mut p, n_pts)?;
            let p1 = decode_planar_u16(doeg_body, &mut p, n_pts)?;
            let p2 = decode_planar_u16(doeg_body, &mut p, n_pts)?;
            let p3 = decode_planar_u16(doeg_body, &mut p, n_pts)?;
            let p4 = decode_planar_u16(doeg_body, &mut p, n_pts)?;
            raw_loops.push([p0, p1, p2, p3, p4]);
            sub += ssz;
            doeg_tail_start = sub;
        } else if stag == b"LACS" && ssz == 8 + 5 * 8 && raw_scals.len() < raw_loops.len() {
            let mut scal = [(0.0_f64, 0.0_f64); 5];
            for (i, slot) in scal.iter_mut().enumerate() {
                let base = sub + 8 + i * 8;
                let s = f32::from_le_bytes(doeg_body[base..base + 4].try_into()?) as f64;
                let o = f32::from_le_bytes(doeg_body[base + 4..base + 8].try_into()?) as f64;
                *slot = (s, o);
            }
            raw_scals.push(scal);
            sub += ssz;
            doeg_tail_start = sub;
        } else {
            break;
        }
    }
    ensure!(
        !raw_loops.is_empty() && raw_loops.len() == raw_scals.len(),
        "DSF 5-plane land LOOP/LACS count mismatch ({} vs {})",
        raw_loops.len(),
        raw_scals.len()
    );
    let land_pools: Vec<LandPool> = raw_loops
        .into_iter()
        .zip(raw_scals)
        .map(|(planes, scal)| LandPool { planes, scal })
        .collect();
    let doeg_tail = &doeg_body[doeg_tail_start..];

    let mut out_pools: Vec<OutLandPool> = land_pools
        .iter()
        .map(|lp| OutLandPool::new(lp.scal))
        .collect();
    let mut ortho_tiles: BTreeMap<(u32, u32), TileTriangles> = BTreeMap::new();

    let get_land_vertex = |pool_idx: u16, vtx_idx: u16| -> Result<LandVertex> {
        let lp = land_pools
            .get(pool_idx as usize)
            .with_context(|| format!("land triangle references non-land pool {pool_idx}"))?;
        let idx = vtx_idx as usize;
        ensure!(
            idx < lp.planes[0].len(),
            "vertex index {idx} out of bounds in pool {pool_idx}"
        );
        let q = [
            lp.planes[0][idx],
            lp.planes[1][idx],
            lp.planes[2][idx],
            lp.planes[3][idx],
            lp.planes[4][idx],
        ];
        let deq = |plane: usize| -> f64 {
            (q[plane] as f64 / 65535.0) * lp.scal[plane].0 + lp.scal[plane].1
        };
        Ok(LandVertex {
            lon: deq(0),
            lat: deq(1),
            z: deq(2),
            nx: deq(3),
            nz: deq(4),
            orig: Some((pool_idx, q)),
        })
    };

    let mut out_cmds = Vec::with_capacity(sdmc_body.len());
    let mut cur_pool = 0_u16;
    let mut cur_def = 0_u32;
    let mut cur_flags = 1_u8;
    let mut cur_near = 0.0_f32;
    let mut cur_far = -1.0_f32;
    let mut out_pool: Option<u16> = None;
    let mut out_def: Option<u32> = None;

    let mut cpos = 0;
    while cpos < sdmc_body.len() {
        let op_start = cpos;
        let op = sdmc_body[cpos];
        cpos += 1;
        match op {
            1 => {
                cur_pool = u16::from_le_bytes(
                    sdmc_body
                        .get(cpos..cpos + 2)
                        .context("truncated op 1")?
                        .try_into()?,
                );
                cpos += 2;
            }
            2 => {
                let end_op = cpos + 4;
                ensure!(end_op <= sdmc_body.len(), "truncated op 2");
                out_cmds.extend_from_slice(&sdmc_body[op_start..end_op]);
                cpos = end_op;
            }
            3 => {
                cur_def = *sdmc_body.get(cpos).context("truncated op 3")? as u32;
                cpos += 1;
            }
            4 => {
                cur_def = u16::from_le_bytes(
                    sdmc_body
                        .get(cpos..cpos + 2)
                        .context("truncated op 4")?
                        .try_into()?,
                ) as u32;
                cpos += 2;
            }
            5 => {
                cur_def = u32::from_le_bytes(
                    sdmc_body
                        .get(cpos..cpos + 4)
                        .context("truncated op 5")?
                        .try_into()?,
                );
                cpos += 4;
            }
            6 => {
                ensure!(cpos < sdmc_body.len(), "truncated op 6");
                cpos += 1;
                sync_state(
                    &mut out_cmds,
                    &mut out_pool,
                    &mut out_def,
                    None,
                    Some(cur_def),
                );
                out_cmds.extend_from_slice(&sdmc_body[op_start..cpos]);
            }
            7 => {
                ensure!(cpos + 2 <= sdmc_body.len(), "truncated op 7");
                cpos += 2;
                sync_state(
                    &mut out_cmds,
                    &mut out_pool,
                    &mut out_def,
                    Some(cur_pool),
                    Some(cur_def),
                );
                out_cmds.extend_from_slice(&sdmc_body[op_start..cpos]);
            }
            8 => {
                ensure!(cpos + 4 <= sdmc_body.len(), "truncated op 8");
                cpos += 4;
                sync_state(
                    &mut out_cmds,
                    &mut out_pool,
                    &mut out_def,
                    Some(cur_pool),
                    Some(cur_def),
                );
                out_cmds.extend_from_slice(&sdmc_body[op_start..cpos]);
            }
            9 => {
                let cnt = *sdmc_body.get(cpos).context("truncated op 9")? as usize;
                cpos += 1 + cnt * 2;
                ensure!(cpos <= sdmc_body.len(), "truncated op 9 indices");
                sync_state(
                    &mut out_cmds,
                    &mut out_pool,
                    &mut out_def,
                    Some(cur_pool),
                    Some(cur_def),
                );
                out_cmds.extend_from_slice(&sdmc_body[op_start..cpos]);
            }
            10 => {
                ensure!(cpos + 4 <= sdmc_body.len(), "truncated op 10");
                cpos += 4;
                sync_state(
                    &mut out_cmds,
                    &mut out_pool,
                    &mut out_def,
                    Some(cur_pool),
                    Some(cur_def),
                );
                out_cmds.extend_from_slice(&sdmc_body[op_start..cpos]);
            }
            11 => {
                let cnt = *sdmc_body.get(cpos).context("truncated op 11")? as usize;
                cpos += 1 + cnt * 4;
                ensure!(cpos <= sdmc_body.len(), "truncated op 11 indices");
                sync_state(
                    &mut out_cmds,
                    &mut out_pool,
                    &mut out_def,
                    Some(cur_pool),
                    Some(cur_def),
                );
                out_cmds.extend_from_slice(&sdmc_body[op_start..cpos]);
            }
            12 => {
                let cnt = *sdmc_body.get(cpos + 2).context("truncated op 12")? as usize;
                cpos += 3 + cnt * 2;
                ensure!(cpos <= sdmc_body.len(), "truncated op 12 indices");
                sync_state(
                    &mut out_cmds,
                    &mut out_pool,
                    &mut out_def,
                    Some(cur_pool),
                    Some(cur_def),
                );
                out_cmds.extend_from_slice(&sdmc_body[op_start..cpos]);
            }
            13 => {
                ensure!(cpos + 6 <= sdmc_body.len(), "truncated op 13");
                cpos += 6;
                sync_state(
                    &mut out_cmds,
                    &mut out_pool,
                    &mut out_def,
                    Some(cur_pool),
                    Some(cur_def),
                );
                out_cmds.extend_from_slice(&sdmc_body[op_start..cpos]);
            }
            14 => {
                let windings = *sdmc_body.get(cpos + 2).context("truncated op 14")? as usize;
                cpos += 3;
                for _ in 0..windings {
                    let cnt = *sdmc_body.get(cpos).context("truncated op 14 winding")? as usize;
                    cpos += 1 + cnt * 2;
                }
                ensure!(cpos <= sdmc_body.len(), "truncated op 14 indices");
                sync_state(
                    &mut out_cmds,
                    &mut out_pool,
                    &mut out_def,
                    Some(cur_pool),
                    Some(cur_def),
                );
                out_cmds.extend_from_slice(&sdmc_body[op_start..cpos]);
            }
            15 => {
                let windings = *sdmc_body.get(cpos + 2).context("truncated op 15")? as usize;
                cpos += 3 + (windings + 1) * 2;
                ensure!(cpos <= sdmc_body.len(), "truncated op 15 indices");
                sync_state(
                    &mut out_cmds,
                    &mut out_pool,
                    &mut out_def,
                    Some(cur_pool),
                    Some(cur_def),
                );
                out_cmds.extend_from_slice(&sdmc_body[op_start..cpos]);
            }
            16..=18 => {
                if op == 17 {
                    cur_flags = *sdmc_body.get(cpos).context("truncated op 17")?;
                    cpos += 1;
                } else if op == 18 {
                    ensure!(cpos + 9 <= sdmc_body.len(), "truncated op 18");
                    cur_flags = sdmc_body[cpos];
                    cur_near = f32::from_le_bytes(sdmc_body[cpos + 1..cpos + 5].try_into()?);
                    cur_far = f32::from_le_bytes(sdmc_body[cpos + 5..cpos + 9].try_into()?);
                    cpos += 9;
                }
                if cur_def == 0 {
                    sync_state(
                        &mut out_cmds,
                        &mut out_pool,
                        &mut out_def,
                        Some(cur_pool),
                        Some(0),
                    );
                    out_cmds.push(18);
                    out_cmds.push(cur_flags);
                    out_cmds.extend_from_slice(&cur_near.to_le_bytes());
                    out_cmds.extend_from_slice(&cur_far.to_le_bytes());
                }
            }
            23..=31 => {
                let mut verts = Vec::new();
                let uses_cur_pool = matches!(op, 23 | 25 | 26 | 28 | 29 | 31);
                match op {
                    23 | 26 | 29 => {
                        let cnt = *sdmc_body.get(cpos).context("truncated patch op")? as usize;
                        cpos += 1;
                        ensure!(cpos + cnt * 2 <= sdmc_body.len(), "truncated patch indices");
                        for _ in 0..cnt {
                            let idx = u16::from_le_bytes([sdmc_body[cpos], sdmc_body[cpos + 1]]);
                            verts.push((cur_pool, idx));
                            cpos += 2;
                        }
                    }
                    24 | 27 | 30 => {
                        let cnt = *sdmc_body.get(cpos).context("truncated xpool op")? as usize;
                        cpos += 1;
                        ensure!(cpos + cnt * 4 <= sdmc_body.len(), "truncated xpool indices");
                        for _ in 0..cnt {
                            let p = u16::from_le_bytes([sdmc_body[cpos], sdmc_body[cpos + 1]]);
                            let idx =
                                u16::from_le_bytes([sdmc_body[cpos + 2], sdmc_body[cpos + 3]]);
                            verts.push((p, idx));
                            cpos += 4;
                        }
                    }
                    25 | 28 | 31 => {
                        ensure!(cpos + 4 <= sdmc_body.len(), "truncated range op");
                        let first = u16::from_le_bytes([sdmc_body[cpos], sdmc_body[cpos + 1]]);
                        let last = u16::from_le_bytes([sdmc_body[cpos + 2], sdmc_body[cpos + 3]]);
                        cpos += 4;
                        for idx in first..last {
                            verts.push((cur_pool, idx));
                        }
                    }
                    _ => unreachable!(),
                }

                if cur_def == 0 {
                    let need_pool = uses_cur_pool.then_some(cur_pool);
                    sync_state(
                        &mut out_cmds,
                        &mut out_pool,
                        &mut out_def,
                        need_pool,
                        Some(0),
                    );
                    out_cmds.extend_from_slice(&sdmc_body[op_start..cpos]);
                } else if (cur_flags & 1) != 0 {
                    let mut tris = Vec::new();
                    match op {
                        23..=25 => {
                            for tri in verts.as_chunks::<3>().0 {
                                tris.push([tri[0], tri[1], tri[2]]);
                            }
                        }
                        26..=28 => {
                            if verts.len() >= 3 {
                                for i in 0..verts.len() - 2 {
                                    if i % 2 == 0 {
                                        tris.push([verts[i], verts[i + 1], verts[i + 2]]);
                                    } else {
                                        tris.push([verts[i + 1], verts[i], verts[i + 2]]);
                                    }
                                }
                            }
                        }
                        29..=31 => {
                            if verts.len() >= 3 {
                                for i in 0..verts.len() - 2 {
                                    tris.push([verts[0], verts[i + 1], verts[i + 2]]);
                                }
                            }
                        }
                        _ => unreachable!(),
                    }

                    for [a_ref, b_ref, c_ref] in tris {
                        let mut v0 = get_land_vertex(a_ref.0, a_ref.1)?;
                        let mut v1 = get_land_vertex(b_ref.0, b_ref.1)?;
                        let mut v2 = get_land_vertex(c_ref.0, c_ref.1)?;

                        let lon_c = (v0.lon + v1.lon + v2.lon) / 3.0;
                        let lat_c = (v0.lat + v1.lat + v2.lat) / 3.0;
                        let tx_c = (lon_to_tx12(lon_c).floor() as i32).clamp(0, 4095) as u32;
                        let ty_c = (lat_to_ty12(lat_c).floor() as i32).clamp(0, 4095) as u32;
                        let west = tx12_to_lon(tx_c as f64);
                        let east = tx12_to_lon((tx_c + 1) as f64);
                        let north = ty12_to_lat(ty_c as f64);
                        let south = ty12_to_lat((ty_c + 1) as f64);

                        let inside_single = [v0, v1, v2].iter().all(|v| {
                            v.lon >= west - 1e-9
                                && v.lon <= east + 1e-9
                                && v.lat >= south - 1e-9
                                && v.lat <= north + 1e-9
                        });

                        if inside_single {
                            let p0 =
                                intern_land_vertex(v0, tx_c, ty_c, &land_pools, &mut out_pools)?;
                            let p1 =
                                intern_land_vertex(v1, tx_c, ty_c, &land_pools, &mut out_pools)?;
                            let p2 =
                                intern_land_vertex(v2, tx_c, ty_c, &land_pools, &mut out_pools)?;
                            ortho_tiles
                                .entry((ty_c * 16, tx_c * 16))
                                .or_default()
                                .push([p0, p1, p2]);
                        } else {
                            let has_dem_sentinel =
                                [v0.z, v1.z, v2.z].iter().any(|&z| z <= -32760.0);
                            let has_explicit_z = [v0.z, v1.z, v2.z].iter().any(|&z| z > -32760.0);
                            if has_dem_sentinel
                                && has_explicit_z
                                && let Some(ref dem) = dem_grid
                            {
                                for v in [&mut v0, &mut v1, &mut v2] {
                                    if v.z <= -32760.0 {
                                        v.z = dem.sample(v.lon, v.lat);
                                        v.orig = None;
                                    }
                                }
                            }

                            let min_lon = v0.lon.min(v1.lon).min(v2.lon);
                            let max_lon = v0.lon.max(v1.lon).max(v2.lon);
                            let min_lat = v0.lat.min(v1.lat).min(v2.lat);
                            let max_lat = v0.lat.max(v1.lat).max(v2.lat);

                            let tx0 =
                                (lon_to_tx12(min_lon + 1e-9).floor() as i32).clamp(0, 4095) as u32;
                            let tx1 =
                                (lon_to_tx12(max_lon - 1e-9).floor() as i32).clamp(0, 4095) as u32;
                            let ty0 =
                                (lat_to_ty12(max_lat - 1e-9).floor() as i32).clamp(0, 4095) as u32;
                            let ty1 =
                                (lat_to_ty12(min_lat + 1e-9).floor() as i32).clamp(0, 4095) as u32;

                            for ty in ty0..=ty1 {
                                let t_north = ty12_to_lat(ty as f64);
                                let t_south = ty12_to_lat((ty + 1) as f64);
                                for tx in tx0..=tx1 {
                                    let t_west = tx12_to_lon(tx as f64);
                                    let t_east = tx12_to_lon((tx + 1) as f64);
                                    let poly = clip_triangle_to_box(
                                        [v0, v1, v2],
                                        t_west,
                                        t_east,
                                        t_south,
                                        t_north,
                                    );
                                    if poly.len() < 3 {
                                        continue;
                                    }
                                    for k in 1..poly.len() - 1 {
                                        let a = poly[0];
                                        let b = poly[k];
                                        let c = poly[k + 1];
                                        let area2 = ((b.lon - a.lon) * (c.lat - a.lat)
                                            - (b.lat - a.lat) * (c.lon - a.lon))
                                            .abs();
                                        if area2 <= 1e-16 {
                                            continue;
                                        }
                                        let p0 = intern_land_vertex(
                                            a,
                                            tx,
                                            ty,
                                            &land_pools,
                                            &mut out_pools,
                                        )?;
                                        let p1 = intern_land_vertex(
                                            b,
                                            tx,
                                            ty,
                                            &land_pools,
                                            &mut out_pools,
                                        )?;
                                        let p2 = intern_land_vertex(
                                            c,
                                            tx,
                                            ty,
                                            &land_pools,
                                            &mut out_pools,
                                        )?;
                                        if p0 != p1 && p1 != p2 && p0 != p2 {
                                            ortho_tiles
                                                .entry((ty * 16, tx * 16))
                                                .or_default()
                                                .push([p0, p1, p2]);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            32 => {
                let len = *sdmc_body.get(cpos).context("truncated op 32")? as usize;
                cpos += 1 + len;
            }
            33 => {
                let len = u16::from_le_bytes(
                    sdmc_body
                        .get(cpos..cpos + 2)
                        .context("truncated op 33")?
                        .try_into()?,
                ) as usize;
                cpos += 2 + len;
            }
            34 => {
                let len = u32::from_le_bytes(
                    sdmc_body
                        .get(cpos..cpos + 4)
                        .context("truncated op 34")?
                        .try_into()?,
                ) as usize;
                cpos += 4 + len;
            }
            other => bail!("unknown DSF command opcode {other} at offset {op_start}"),
        }
    }

    // Build terrain table and emit Ortho physical land patches into out_cmds.
    let mut terrains = Vec::with_capacity(1 + ortho_tiles.len());
    terrains.push("terrain_Water".to_owned());
    for (idx, (&(row, col), tile)) in ortho_tiles.iter().enumerate() {
        let ter_def = (idx + 1) as u32;
        terrains.push(format!("terrain/{row}_{col}_BI16.ter"));
        let first_pool = tile
            .by_pool
            .keys()
            .copied()
            .next()
            .or_else(|| tile.xpool.first().map(|tri| tri[0].0))
            .unwrap_or(0);
        sync_state(
            &mut out_cmds,
            &mut out_pool,
            &mut out_def,
            Some(first_pool),
            Some(ter_def),
        );
        out_cmds.push(18);
        out_cmds.push(1);
        out_cmds.extend_from_slice(&0.0_f32.to_le_bytes());
        out_cmds.extend_from_slice(&(-1.0_f32).to_le_bytes());

        for (&pool, tris) in &tile.by_pool {
            sync_state(&mut out_cmds, &mut out_pool, &mut out_def, Some(pool), None);
            for chunk in tris.chunks(85) {
                out_cmds.push(23);
                out_cmds.push((chunk.len() * 3) as u8);
                for tri in chunk {
                    for &vtx in tri {
                        out_cmds.extend_from_slice(&vtx.to_le_bytes());
                    }
                }
            }
        }
        for chunk in tile.xpool.chunks(85) {
            out_cmds.push(24);
            out_cmds.push((chunk.len() * 3) as u8);
            for tri in chunk {
                for &(pool, vtx) in tri {
                    out_cmds.extend_from_slice(&pool.to_le_bytes());
                    out_cmds.extend_from_slice(&vtx.to_le_bytes());
                }
            }
        }
    }

    // Assemble NFED atom.
    let mut tret_body = Vec::new();
    for name in &terrains {
        tret_body.extend_from_slice(name.as_bytes());
        tret_body.push(0);
    }
    let mut nfed_body = Vec::with_capacity(8 + tret_body.len() + 65536);
    nfed_body.extend_from_slice(b"TRET");
    nfed_body.extend_from_slice(&((8 + tret_body.len()) as u32).to_le_bytes());
    nfed_body.extend_from_slice(&tret_body);
    for sub in &nfed_other_subatoms {
        nfed_body.extend_from_slice(sub);
    }

    // Assemble DOEG atom: 7-plane land LOOPs 0..N, then 7-plane LACSs 0..N, followed by verbatim doeg_tail.
    for pool in &mut out_pools {
        if pool.vertices.is_empty() {
            pool.vertices.push([0; 7]);
        }
    }
    let mut new_doeg_body = Vec::with_capacity(doeg_body.len() + 2 * 1024 * 1024);
    for pool in &out_pools {
        let n_pts = pool.vertices.len();
        let loop_sz = 8 + 4 + 1 + 7 * (1 + n_pts * 2);
        new_doeg_body.extend_from_slice(b"LOOP");
        new_doeg_body.extend_from_slice(&(loop_sz as u32).to_le_bytes());
        new_doeg_body.extend_from_slice(&(n_pts as u32).to_le_bytes());
        new_doeg_body.push(7);
        for plane in 0..7 {
            new_doeg_body.push(0); // raw planar encoding
            for v in &pool.vertices {
                new_doeg_body.extend_from_slice(&v[plane].to_le_bytes());
            }
        }
    }
    for pool in &out_pools {
        let lacs_sz = 8 + 7 * 8;
        new_doeg_body.extend_from_slice(b"LACS");
        new_doeg_body.extend_from_slice(&(lacs_sz as u32).to_le_bytes());
        for &(scale, offset) in &pool.scal {
            new_doeg_body.extend_from_slice(&scale.to_le_bytes());
            new_doeg_body.extend_from_slice(&offset.to_le_bytes());
        }
    }
    new_doeg_body.extend_from_slice(doeg_tail);

    // Assemble final uncompressed DSF with MD5 trailer.
    let mut out_dsf = Vec::with_capacity(
        12 + daeh_atom.len()
            + 8
            + nfed_body.len()
            + 8
            + new_doeg_body.len()
            + 8
            + out_cmds.len()
            + smed_atom.map_or(0, <[u8]>::len)
            + 16,
    );
    out_dsf.extend_from_slice(DSF_HEADER);
    out_dsf.extend_from_slice(daeh_atom);

    out_dsf.extend_from_slice(b"NFED");
    out_dsf.extend_from_slice(&((8 + nfed_body.len()) as u32).to_le_bytes());
    out_dsf.extend_from_slice(&nfed_body);

    out_dsf.extend_from_slice(b"DOEG");
    out_dsf.extend_from_slice(&((8 + new_doeg_body.len()) as u32).to_le_bytes());
    out_dsf.extend_from_slice(&new_doeg_body);

    out_dsf.extend_from_slice(b"SDMC");
    out_dsf.extend_from_slice(&((8 + out_cmds.len()) as u32).to_le_bytes());
    out_dsf.extend_from_slice(&out_cmds);

    if let Some(smed) = smed_atom {
        out_dsf.extend_from_slice(smed);
    }

    let digest = Md5::digest(&out_dsf);
    out_dsf.extend_from_slice(&digest);

    // Validate with GroundEffect's own DSF parser before writing.
    scenery::read_dsf_terrains(&mut std::io::Cursor::new(&out_dsf), out_dsf.len() as u64)?;

    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp_path = output_path.with_extension(format!("dsf.tmp.{}.{seq}", std::process::id()));
    std::fs::write(&tmp_path, &out_dsf)?;
    std::fs::rename(&tmp_path, output_path)?;

    Ok(terrains)
}

pub fn discover_laminar_scenery_roots(hint: Option<&Path>) -> Vec<std::path::PathBuf> {
    let mut xp_roots = Vec::new();
    let mut candidates = Vec::new();
    if let Some(h) = hint {
        candidates.push(h.to_path_buf());
        xp_roots.push(h.to_path_buf());
    }
    if let Some(env_root) = std::env::var_os("XPLANE_ROOT") {
        let p = std::path::PathBuf::from(env_root);
        candidates.push(p.clone());
        xp_roots.push(p);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = std::path::PathBuf::from(home);
        let install_txt = home.join(".x-plane/x-plane_install_12.txt");
        if let Ok(contents) = std::fs::read_to_string(&install_txt) {
            for line in contents.lines().map(str::trim).filter(|l| !l.is_empty()) {
                xp_roots.push(std::path::PathBuf::from(line));
            }
        }
        let mut steam_libs = vec![
            home.join(".local/share/Steam"),
            home.join(".steam/steam"),
            home.join(".steam/root"),
            home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
        ];
        for steam_dir in steam_libs.clone() {
            let vdf = steam_dir.join("steamapps/libraryfolders.vdf");
            if let Ok(text) = std::fs::read_to_string(&vdf) {
                for line in text.lines() {
                    let tokens: Vec<&str> = line.split('"').collect();
                    if tokens.len() >= 4 && tokens[1] == "path" {
                        let lib = std::path::PathBuf::from(tokens[3].replace("\\\\", "/"));
                        if !steam_libs.contains(&lib) {
                            steam_libs.push(lib);
                        }
                    }
                }
            }
        }
        for lib in steam_libs {
            xp_roots.push(lib.join("steamapps/common/X-Plane 12"));
        }
    }
    for xp_root in xp_roots {
        for sub in [
            "X-Plane 12 Demo Areas",
            "X-Plane 12 Global Scenery",
            "Global Scenery/X-Plane 12 Demo Areas",
            "Global Scenery/X-Plane 12 Global Scenery",
        ] {
            candidates.push(xp_root.join(sub));
        }
    }
    let mut roots = Vec::new();
    for candidate in candidates {
        if candidate.join("Earth nav data").is_dir()
            && let Ok(canon) = candidate.canonicalize()
            && !roots.contains(&canon)
        {
            roots.push(canon);
        }
    }
    roots
}

fn decode_planar_u32(buf: &[u8], pos: &mut usize, n_pts: usize) -> Result<Vec<u32>> {
    let mode = *buf.get(*pos).context("truncated 23OP plane mode")?;
    *pos += 1;
    let mut out = Vec::with_capacity(n_pts);
    if (mode & 1) == 0 {
        let bytes_needed = n_pts.checked_mul(4).context("23OP plane overflow")?;
        let slice = buf
            .get(*pos..*pos + bytes_needed)
            .context("truncated raw 23OP plane")?;
        for chunk in slice.as_chunks::<4>().0 {
            out.push(u32::from_le_bytes(*chunk));
        }
        *pos += bytes_needed;
    } else {
        while out.len() < n_pts {
            let run = *buf.get(*pos).context("truncated RLE 23OP plane")?;
            *pos += 1;
            if (run & 128) != 0 {
                let cnt = (run & 127) as usize;
                let val_bytes = buf
                    .get(*pos..*pos + 4)
                    .context("truncated RLE repeat value in 23OP")?;
                let val = u32::from_le_bytes(val_bytes.try_into()?);
                *pos += 4;
                out.resize(out.len() + cnt, val);
            } else {
                let cnt = run as usize;
                let bytes_needed = cnt.checked_mul(4).context("23OP RLE literal overflow")?;
                let slice = buf
                    .get(*pos..*pos + bytes_needed)
                    .context("truncated RLE literal in 23OP")?;
                for chunk in slice.as_chunks::<4>().0 {
                    out.push(u32::from_le_bytes(*chunk));
                }
                *pos += bytes_needed;
            }
        }
        ensure!(out.len() == n_pts, "23OP RLE count mismatch");
    }
    if (mode & 2) != 0 {
        let mut acc = 0_u32;
        for v in &mut out {
            acc = acc.wrapping_add(*v);
            *v = acc;
        }
    }
    Ok(out)
}

fn laminar_road_tier(subtype: u8) -> Option<f32> {
    match subtype {
        100 => Some(1.00),
        101..=109 | 110 | 130 => Some(0.92),
        111..=119 | 131..=139 => Some(0.85),
        11..=18 | 21..=28 => Some(0.82),
        120 => Some(0.76),
        10 | 19 | 20 | 29 => Some(0.70),
        121..=129 => Some(0.68),
        31..=38 | 41..=48 => Some(0.55),
        30 | 39 | 40 | 49 => Some(0.45),
        51..=58 | 61..=68 => Some(0.26),
        50 | 59 | 60 | 69 => Some(0.20),
        70..=79 => Some(0.15),
        _ => None,
    }
}

pub type RawRoadChain = (u64, f32, Vec<[f64; 2]>);

pub fn extract_laminar_dsf_roads(
    dsf_path: &Path,
    cell_lat: i16,
    cell_lon: i16,
) -> Result<Vec<RawRoadChain>> {
    let raw = read_raw_dsf(dsf_path)?;
    ensure!(
        raw.len() >= 28 && &raw[..12] == DSF_HEADER,
        "invalid uncompressed DSF header in {}",
        dsf_path.display()
    );
    let end = raw.len() - 16;
    let mut doeg_body: Option<&[u8]> = None;
    let mut sdmc_body: Option<&[u8]> = None;

    let mut pos = 12;
    while pos < end {
        ensure!(end - pos >= 8, "truncated top-level DSF atom");
        let tag = &raw[pos..pos + 4];
        let sz = u32::from_le_bytes(raw[pos + 4..pos + 8].try_into()?) as usize;
        ensure!(sz >= 8 && pos + sz <= end, "DSF atom out of bounds");
        let body = &raw[pos + 8..pos + sz];
        match tag {
            b"DOEG" => doeg_body = Some(body),
            b"SDMC" => sdmc_body = Some(body),
            _ => {}
        }
        pos += sz;
    }

    let (Some(doeg_body), Some(sdmc_body)) = (doeg_body, sdmc_body) else {
        return Ok(Vec::new());
    };

    let mut raw_po32s: Vec<Option<(Vec<u32>, Vec<u32>)>> = Vec::new();
    let mut raw_sc32s: Vec<Option<(f64, f64, f64, f64)>> = Vec::new();
    let mut sub = 0;
    while sub < doeg_body.len() {
        ensure!(doeg_body.len() - sub >= 8, "truncated DOEG sub-atom");
        let stag = &doeg_body[sub..sub + 4];
        let ssz = u32::from_le_bytes(doeg_body[sub + 4..sub + 8].try_into()?) as usize;
        ensure!(
            ssz >= 8 && sub + ssz <= doeg_body.len(),
            "DOEG sub-atom OOB"
        );
        let body = &doeg_body[sub + 8..sub + ssz];
        if stag == b"23OP" {
            if body.len() >= 5 {
                let n_pts = u32::from_le_bytes(body[0..4].try_into()?) as usize;
                let n_planes = body[4] as usize;
                if n_pts > 0 && n_planes >= 2 {
                    let mut p = 5;
                    let lons = decode_planar_u32(body, &mut p, n_pts)?;
                    let lats = decode_planar_u32(body, &mut p, n_pts)?;
                    raw_po32s.push(Some((lons, lats)));
                } else {
                    raw_po32s.push(None);
                }
            } else {
                raw_po32s.push(None);
            }
        } else if stag == b"23CS" {
            if body.len() >= 16 {
                let slon = f32::from_le_bytes(body[0..4].try_into()?) as f64;
                let olon = f32::from_le_bytes(body[4..8].try_into()?) as f64;
                let slat = f32::from_le_bytes(body[8..12].try_into()?) as f64;
                let olat = f32::from_le_bytes(body[12..16].try_into()?) as f64;
                raw_sc32s.push(Some((slon, olon, slat, olat)));
            } else {
                raw_sc32s.push(None);
            }
        }
        sub += ssz;
    }

    if raw_po32s.is_empty() {
        return Ok(Vec::new());
    }

    let min_lat = cell_lat as f64;
    let max_lat = (cell_lat as f64 + 0.999_999).min(85.05);
    let min_lon = cell_lon as f64;
    let max_lon = (cell_lon as f64 + 0.999_999).min(179.999_999);

    let mut pools32: Vec<Vec<[f64; 2]>> = Vec::with_capacity(raw_po32s.len());
    for (po, sc) in raw_po32s.into_iter().zip(raw_sc32s) {
        if let (Some((lons, lats)), Some((slon, olon, slat, olat))) = (po, sc) {
            let mut pts = Vec::with_capacity(lons.len());
            for (qlon, qlat) in lons.into_iter().zip(lats) {
                let lon = ((qlon as f64 / 4_294_967_295.0) * slon + olon).clamp(min_lon, max_lon);
                let lat = ((qlat as f64 / 4_294_967_295.0) * slat + olat).clamp(min_lat, max_lat);
                pts.push([lat, lon]);
            }
            pools32.push(pts);
        } else {
            pools32.push(Vec::new());
        }
    }

    let cell_tag =
        (((cell_lat as i32 + 90) as u64) << 40) | (((cell_lon as i32 + 180) as u64) << 28);
    let mut roads = Vec::with_capacity(65536);
    let mut seq = 1_u64;
    let mut cur_pool = 0_usize;
    let mut cur_junc_off = 0_usize;
    let mut cur_net_sub = 0_u8;

    let mut push_chain = |pool_idx: usize, subtype: u8, indices: &[usize]| {
        let Some(tier) = laminar_road_tier(subtype) else {
            return;
        };
        let Some(pool) = pools32.get(pool_idx) else {
            return;
        };
        if indices.len() < 2 {
            return;
        }
        let mut pts: Vec<[f64; 2]> = Vec::with_capacity(indices.len());
        for &idx in indices {
            if let Some(&pt) = pool.get(idx)
                && pts.last().is_none_or(|prev| {
                    (prev[0] - pt[0]).abs() > 1e-8 || (prev[1] - pt[1]).abs() > 1e-8
                })
            {
                pts.push(pt);
            }
        }
        if pts.len() >= 2 {
            let id = cell_tag | seq;
            seq += 1;
            roads.push((id, tier, pts));
        }
    };

    let mut cpos = 0;
    let mut idx_buf = Vec::with_capacity(256);
    while cpos < sdmc_body.len() {
        let op_start = cpos;
        let op = sdmc_body[cpos];
        cpos += 1;
        match op {
            1 => {
                cur_pool = u16::from_le_bytes(
                    sdmc_body
                        .get(cpos..cpos + 2)
                        .context("truncated op 1")?
                        .try_into()?,
                ) as usize;
                cpos += 2;
            }
            2 => {
                cur_junc_off = u32::from_le_bytes(
                    sdmc_body
                        .get(cpos..cpos + 4)
                        .context("truncated op 2")?
                        .try_into()?,
                ) as usize;
                cpos += 4;
            }
            3 => cpos += 1,
            4 => cpos += 2,
            5 => cpos += 4,
            6 => {
                cur_net_sub = *sdmc_body.get(cpos).context("truncated op 6")?;
                cpos += 1;
            }
            7 => cpos += 2,
            8 => cpos += 4,
            9 => {
                let cnt = *sdmc_body.get(cpos).context("truncated op 9")? as usize;
                cpos += 1;
                ensure!(cpos + cnt * 2 <= sdmc_body.len(), "truncated op 9 indices");
                idx_buf.clear();
                for _ in 0..cnt {
                    let off = u16::from_le_bytes([sdmc_body[cpos], sdmc_body[cpos + 1]]) as usize;
                    idx_buf.push(cur_junc_off + off);
                    cpos += 2;
                }
                push_chain(cur_pool, cur_net_sub, &idx_buf);
            }
            10 => {
                ensure!(cpos + 4 <= sdmc_body.len(), "truncated op 10");
                let a = u16::from_le_bytes([sdmc_body[cpos], sdmc_body[cpos + 1]]) as usize;
                let b = u16::from_le_bytes([sdmc_body[cpos + 2], sdmc_body[cpos + 3]]) as usize;
                cpos += 4;
                if b > a + 1 {
                    idx_buf.clear();
                    for idx in (cur_junc_off + a)..(cur_junc_off + b) {
                        idx_buf.push(idx);
                    }
                    push_chain(cur_pool, cur_net_sub, &idx_buf);
                }
            }
            11 => {
                let cnt = *sdmc_body.get(cpos).context("truncated op 11")? as usize;
                cpos += 1;
                ensure!(cpos + cnt * 4 <= sdmc_body.len(), "truncated op 11 indices");
                idx_buf.clear();
                for _ in 0..cnt {
                    let idx = u32::from_le_bytes(sdmc_body[cpos..cpos + 4].try_into()?) as usize;
                    idx_buf.push(idx);
                    cpos += 4;
                }
                push_chain(cur_pool, cur_net_sub, &idx_buf);
            }
            12 => {
                let cnt = *sdmc_body.get(cpos + 2).context("truncated op 12")? as usize;
                cpos += 3 + cnt * 2;
            }
            13 => cpos += 6,
            14 => {
                let windings = *sdmc_body.get(cpos + 2).context("truncated op 14")? as usize;
                cpos += 3;
                for _ in 0..windings {
                    let cnt = *sdmc_body.get(cpos).context("truncated op 14 winding")? as usize;
                    cpos += 1 + cnt * 2;
                }
            }
            15 => {
                let windings = *sdmc_body.get(cpos + 2).context("truncated op 15")? as usize;
                cpos += 3 + (windings + 1) * 2;
            }
            16 => {}
            17 => cpos += 1,
            18 => cpos += 9,
            23 | 26 | 29 => {
                let cnt = *sdmc_body.get(cpos).context("truncated patch op")? as usize;
                cpos += 1 + cnt * 2;
            }
            24 | 27 | 30 => {
                let cnt = *sdmc_body.get(cpos).context("truncated patch op")? as usize;
                cpos += 1 + cnt * 4;
            }
            25 | 28 | 31 => cpos += 4,
            32 => {
                let len = *sdmc_body.get(cpos).context("truncated op 32")? as usize;
                cpos += 1 + len;
            }
            33 => {
                let len = u16::from_le_bytes(
                    sdmc_body
                        .get(cpos..cpos + 2)
                        .context("truncated op 33")?
                        .try_into()?,
                ) as usize;
                cpos += 2 + len;
            }
            34 => {
                let len = u32::from_le_bytes(
                    sdmc_body
                        .get(cpos..cpos + 4)
                        .context("truncated op 34")?
                        .try_into()?,
                ) as usize;
                cpos += 4 + len;
            }
            other => bail!("unknown DSF command opcode {other} at offset {op_start}"),
        }
    }

    Ok(roads)
}
