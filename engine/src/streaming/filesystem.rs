use super::{
    MAX_METADATA,
    cache::{Cache, Cached},
    lock,
    source::{Kind, MAX_RAW, Spec, Stamp, digest, in_imagery_envelope, source_window},
};
use crate::{
    dds,
    geo::Footprint,
    model::{
        FxHashMap, FxHashSet, NIGHT_COASTAL_IDENTITY_TAG, NIGHT_IDENTITY_TAG, StreamConfig,
        StreamStatus, TILE_BUILDING, TILE_CACHED, TILE_COLD, TILE_EMPTY, TILE_LOADED,
        TILE_TER_ONLY,
    },
    raster::bounded_read,
    scenery,
};
use anyhow::{Context, Result, ensure};
use fuser::{
    FileAttr, FileType, Filesystem, KernelConfig, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyStatfs, Request,
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ffi::OsStr,
    fs::{self, File},
    io::BufReader,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, UNIX_EPOCH},
};

const MAX_NODES: usize = 262_144;
const MAX_HANDLES: usize = 512;
const TTL: Duration = Duration::from_secs(3600);
const DSF_TTL: Duration = Duration::from_secs(1);
pub const TILE_GRID_COLS: usize = 16;
pub const TILE_GRID_ROWS: usize = 16;

const BLANK_OBJ: &[u8] = b"A\n800\nOBJ\n";
const RES_SPILL_OBJ: &[u8] = b"A\n800\nOBJ\n\nTEXTURE \nPOINT_COUNTS 0 0 0 0\nATTR_LOD 0.0 3400.0\nLIGHT_PARAM spot_params_sp_pm 0.0 8.5 -2.4 1.00 0.52 0.14 0.92 5600cd 0.0 -0.95 -0.31 0.26\n";
const RES_AMBER_SPILL_OBJ: &[u8] = b"A\n800\nOBJ\n\nTEXTURE \nPOINT_COUNTS 0 0 0 0\nATTR_LOD 0.0 3400.0\nLIGHT_PARAM spot_params_sp_pm 0.0 8.5 -2.4 1.00 0.68 0.28 0.92 6800cd 0.0 -0.95 -0.31 0.27\n";
const RES_LED_SPILL_OBJ: &[u8] = b"A\n800\nOBJ\n\nTEXTURE \nPOINT_COUNTS 0 0 0 0\nATTR_LOD 0.0 3400.0\nLIGHT_PARAM spot_params_sp_pm 0.0 8.5 -2.4 0.92 0.96 1.00 0.92 6400cd 0.0 -0.95 -0.31 0.27\n";
const RES_DIM_SPILL_OBJ: &[u8] = b"A\n800\nOBJ\n\nTEXTURE \nPOINT_COUNTS 0 0 0 0\nATTR_LOD 0.0 2800.0\nLIGHT_PARAM spot_params_sp_pm 0.0 8.2 -2.2 1.00 0.56 0.18 0.85 2300cd 0.0 -0.96 -0.28 0.22\n";
const COM_SPILL_OBJ: &[u8] = b"A\n800\nOBJ\n\nTEXTURE \nPOINT_COUNTS 0 0 0 0\nATTR_LOD 0.0 4200.0\nLIGHT_PARAM spot_params_sp_pm 0.0 9.8 -2.6 0.92 0.96 1.00 0.94 11500cd 0.0 -0.94 -0.34 0.32\n";
const PRI_SPILL_OBJ: &[u8] = b"A\n800\nOBJ\n\nTEXTURE \nPOINT_COUNTS 0 0 0 0\nATTR_LOD 0.0 4400.0\nLIGHT_PARAM spot_params_sp_pm 0.0 10.3 -2.8 1.00 0.66 0.24 0.95 13500cd 0.0 -0.94 -0.34 0.31\n";
const PRI_LED_SPILL_OBJ: &[u8] = b"A\n800\nOBJ\n\nTEXTURE \nPOINT_COUNTS 0 0 0 0\nATTR_LOD 0.0 4400.0\nLIGHT_PARAM spot_params_sp_pm 0.0 10.3 -2.8 0.90 0.96 1.00 0.95 15000cd 0.0 -0.94 -0.34 0.32\n";
const PRI_WARM_SPILL_OBJ: &[u8] = b"A\n800\nOBJ\n\nTEXTURE \nPOINT_COUNTS 0 0 0 0\nATTR_LOD 0.0 4200.0\nLIGHT_PARAM spot_params_sp_pm 0.0 10.3 -2.8 1.00 0.82 0.52 0.94 11800cd 0.0 -0.94 -0.34 0.30\n";
const PRI_DIM_SPILL_OBJ: &[u8] = b"A\n800\nOBJ\n\nTEXTURE \nPOINT_COUNTS 0 0 0 0\nATTR_LOD 0.0 3800.0\nLIGHT_PARAM spot_params_sp_pm 0.0 10.0 -2.6 1.00 0.70 0.32 0.90 6200cd 0.0 -0.95 -0.31 0.26\n";
const HWY_SPILL_OBJ: &[u8] = b"A\n800\nOBJ\n\nTEXTURE \nPOINT_COUNTS 0 0 0 0\nATTR_LOD 0.0 5200.0\nLIGHT_PARAM spot_params_sp_pm 0.0 12.2 -3.6 1.00 0.70 0.28 0.96 22000cd 0.0 -0.95 -0.31 0.34\n";
const HWY_LED_SPILL_OBJ: &[u8] = b"A\n800\nOBJ\n\nTEXTURE \nPOINT_COUNTS 0 0 0 0\nATTR_LOD 0.0 5200.0\nLIGHT_PARAM spot_params_sp_pm 0.0 12.2 -3.6 0.90 0.96 1.00 0.96 24000cd 0.0 -0.95 -0.31 0.34\n";
const FLOOD_HIGHMAST_OBJ: &[u8] = b"A\n800\nOBJ\n\nTEXTURE \nPOINT_COUNTS 0 0 0 0\nATTR_LOD 0.0 5800.0\nLIGHT_PARAM spot_params_sp_pm 0.0 22.5 0.0 1.00 0.78 0.46 0.98 48000cd 0.0 -0.98 -0.20 0.45\n";
const FLOOD_LRG_OBJ: &[u8] = b"A\n800\nOBJ\n\nTEXTURE \nPOINT_COUNTS 0 0 0 0\nATTR_LOD 0.0 4800.0\nLIGHT_PARAM spot_params_sp_pm 0.0 12.4 -0.6 0.94 0.96 1.00 0.96 26000cd 0.0 -0.82 -0.57 0.42\n";
const FLOOD_MED_OBJ: &[u8] = b"A\n800\nOBJ\n\nTEXTURE \nPOINT_COUNTS 0 0 0 0\nATTR_LOD 0.0 4200.0\nLIGHT_PARAM spot_params_sp_pm 0.0 11.1 -0.5 1.00 0.86 0.64 0.95 18000cd 0.0 -0.86 -0.50 0.38\n";
const FLOOD_SML_OBJ: &[u8] = b"A\n800\nOBJ\n\nTEXTURE \nPOINT_COUNTS 0 0 0 0\nATTR_LOD 0.0 3600.0\nLIGHT_PARAM spot_params_sp_pm 0.0 8.5 -0.4 0.92 0.95 1.00 0.94 12500cd 0.0 -0.86 -0.50 0.36\n";

const ROOT_LIGHT_OBJS: &[(&str, &[u8])] = &[
    ("blank.obj", BLANK_OBJ),
    ("ge_res_spill.obj", RES_SPILL_OBJ),
    ("ge_res_amber_spill.obj", RES_AMBER_SPILL_OBJ),
    ("ge_res_led_spill.obj", RES_LED_SPILL_OBJ),
    ("ge_res_dim_spill.obj", RES_DIM_SPILL_OBJ),
    ("ge_com_spill.obj", COM_SPILL_OBJ),
    ("ge_pri_spill.obj", PRI_SPILL_OBJ),
    ("ge_pri_led_spill.obj", PRI_LED_SPILL_OBJ),
    ("ge_pri_warm_spill.obj", PRI_WARM_SPILL_OBJ),
    ("ge_pri_dim_spill.obj", PRI_DIM_SPILL_OBJ),
    ("ge_hwy_spill.obj", HWY_SPILL_OBJ),
    ("ge_hwy_led_spill.obj", HWY_LED_SPILL_OBJ),
    ("ge_flood_highmast.obj", FLOOD_HIGHMAST_OBJ),
    ("ge_flood_lrg.obj", FLOOD_LRG_OBJ),
    ("ge_flood_med.obj", FLOOD_MED_OBJ),
    ("ge_flood_sml.obj", FLOOD_SML_OBJ),
];

const LIBRARY_TXT: &[u8] = b"A\n800\nLIBRARY\n\n\
# Replace default 3D streetlights (which spawn d^0.55 distance-scaled BILLBOARD_HW\n\
# point sprites that pop at the ~10 NM LOD cutoff) with multi-variant billboard-free\n\
# photometric SPILL_HW_DIR cones so 3D buildings, highrise bases, plazas, trees,\n\
# and traffic receive heterogeneous bounce light while _GE_LIT.dds renders to the horizon.\n\
EXPORT_EXCLUDE lib/g10/streetlights/AreaFloodHighMast.obj ge_flood_highmast.obj\n\
EXPORT_EXCLUDE lib/g10/streetlights/AreaFloodLrg.obj ge_flood_lrg.obj\n\
EXPORT_EXTEND lib/g10/streetlights/AreaFloodLrg.obj ge_flood_med.obj\n\
EXPORT_EXCLUDE lib/g10/streetlights/AreaFloodMed.obj ge_flood_med.obj\n\
EXPORT_EXTEND lib/g10/streetlights/AreaFloodMed.obj ge_flood_lrg.obj\n\
EXPORT_EXCLUDE lib/g10/streetlights/AreaFloodShort.obj ge_flood_sml.obj\n\
EXPORT_EXCLUDE lib/g10/streetlights/AreaFloodSml.obj ge_flood_sml.obj\n\
EXPORT_EXTEND lib/g10/streetlights/AreaFloodSml.obj ge_flood_med.obj\n\
EXPORT_EXCLUDE lib/g10/streetlights/BridgeOverhead.obj ge_hwy_led_spill.obj\n\
EXPORT_EXCLUDE lib/g10/streetlights/HwyLt1BarMnt.obj ge_hwy_spill.obj\n\
EXPORT_EXTEND lib/g10/streetlights/HwyLt1BarMnt.obj ge_hwy_led_spill.obj\n\
EXPORT_EXCLUDE lib/g10/streetlights/HwyLt1GndMnt.obj ge_hwy_spill.obj\n\
EXPORT_EXTEND lib/g10/streetlights/HwyLt1GndMnt.obj ge_hwy_led_spill.obj\n\
EXPORT_EXCLUDE lib/g10/streetlights/HwyLt2BarMnt.obj ge_hwy_led_spill.obj\n\
EXPORT_EXTEND lib/g10/streetlights/HwyLt2BarMnt.obj ge_hwy_spill.obj\n\
EXPORT_EXCLUDE lib/g10/streetlights/HwyLt2GndMnt.obj ge_hwy_led_spill.obj\n\
EXPORT_EXTEND lib/g10/streetlights/HwyLt2GndMnt.obj ge_hwy_spill.obj\n\
EXPORT_EXCLUDE lib/g10/streetlights/HwyLt3BarMnt.obj ge_hwy_spill.obj\n\
EXPORT_EXCLUDE lib/g10/streetlights/ParkingLot.obj ge_com_spill.obj\n\
EXPORT_EXTEND lib/g10/streetlights/ParkingLot.obj ge_flood_sml.obj\n\
EXPORT_EXCLUDE lib/g10/streetlights/PrimaryLt1.obj ge_pri_spill.obj\n\
EXPORT_EXTEND lib/g10/streetlights/PrimaryLt1.obj ge_pri_led_spill.obj\n\
EXPORT_EXTEND lib/g10/streetlights/PrimaryLt1.obj ge_pri_warm_spill.obj\n\
EXPORT_EXTEND lib/g10/streetlights/PrimaryLt1.obj ge_pri_dim_spill.obj\n\
EXPORT_EXCLUDE lib/g10/streetlights/ResLt1.obj ge_res_spill.obj\n\
EXPORT_EXTEND lib/g10/streetlights/ResLt1.obj ge_res_amber_spill.obj\n\
EXPORT_EXTEND lib/g10/streetlights/ResLt1.obj ge_res_led_spill.obj\n\
EXPORT_EXTEND lib/g10/streetlights/ResLt1.obj ge_res_dim_spill.obj\n\
EXPORT_EXCLUDE lib/g10/streetlights/ResLt1V1.obj ge_res_spill.obj\n\
EXPORT_EXTEND lib/g10/streetlights/ResLt1V1.obj ge_res_amber_spill.obj\n\
EXPORT_EXTEND lib/g10/streetlights/ResLt1V1.obj ge_res_led_spill.obj\n\
EXPORT_EXTEND lib/g10/streetlights/ResLt1V1.obj ge_res_dim_spill.obj\n\
EXPORT_EXCLUDE lib/g10/streetlights/ResLt1V2.obj ge_res_amber_spill.obj\n\
EXPORT_EXTEND lib/g10/streetlights/ResLt1V2.obj ge_res_spill.obj\n\
EXPORT_EXTEND lib/g10/streetlights/ResLt1V2.obj ge_res_led_spill.obj\n\
EXPORT_EXTEND lib/g10/streetlights/ResLt1V2.obj ge_res_dim_spill.obj\n\
EXPORT_EXCLUDE lib/g10/streetlights/ResLt2.obj ge_res_led_spill.obj\n\
EXPORT_EXTEND lib/g10/streetlights/ResLt2.obj ge_res_amber_spill.obj\n\
EXPORT_EXCLUDE lib/g10/streetlights/ResLt2Dim.obj ge_res_dim_spill.obj\n\
EXPORT_EXCLUDE lib/g10/streetlights/ResLt3.obj ge_res_spill.obj\n\
EXPORT_EXTEND lib/g10/streetlights/ResLt3.obj ge_res_amber_spill.obj\n\
EXPORT_EXTEND lib/g10/streetlights/ResLt3.obj ge_res_dim_spill.obj\n\
EXPORT_EXCLUDE lib/g10/streetlights/ResShort.obj ge_res_amber_spill.obj\n\
EXPORT_EXTEND lib/g10/streetlights/ResShort.obj ge_res_dim_spill.obj\n\
EXPORT_EXCLUDE lib/g10/streetlights/RmpLt1BarMnt.obj ge_hwy_spill.obj\n\
EXPORT_EXCLUDE lib/g10/streetlights/RmpLt1GndMnt.obj ge_hwy_spill.obj\n\
EXPORT_EXCLUDE lib/g10/streetlights/RmpLt2BarMnt.obj ge_hwy_led_spill.obj\n\
EXPORT_EXCLUDE lib/g10/streetlights/RmpLt2GndMnt.obj ge_hwy_led_spill.obj\n\
EXPORT_EXCLUDE lib/street/streetlights/AreaFlood_HighMast.obj ge_flood_highmast.obj\n\
EXPORT_EXCLUDE lib/street/streetlights/AreaFlood_Large.obj ge_flood_lrg.obj\n\
EXPORT_EXCLUDE lib/street/streetlights/AreaFlood_Medium.obj ge_flood_med.obj\n\
EXPORT_EXCLUDE lib/street/streetlights/AreaFlood_Short.obj ge_flood_sml.obj\n\
EXPORT_EXCLUDE lib/street/streetlights/AreaFlood_Small.obj ge_flood_sml.obj\n\
EXPORT_EXCLUDE lib/street/streetlights/BridgeOverhead.obj ge_hwy_led_spill.obj\n\
EXPORT_EXCLUDE lib/street/streetlights/HighwayLight_01.obj ge_hwy_spill.obj\n\
EXPORT_EXTEND lib/street/streetlights/HighwayLight_01.obj ge_hwy_led_spill.obj\n\
EXPORT_EXCLUDE lib/street/streetlights/HighwayLight_02.obj ge_hwy_led_spill.obj\n\
EXPORT_EXTEND lib/street/streetlights/HighwayLight_02.obj ge_hwy_spill.obj\n\
EXPORT_EXCLUDE lib/street/streetlights/HighwayRampLight_01.obj ge_hwy_spill.obj\n\
EXPORT_EXCLUDE lib/street/streetlights/HighwayRampLight_02.obj ge_hwy_led_spill.obj\n\
EXPORT_EXCLUDE lib/street/streetlights/HwyLt1BarMnt.obj ge_hwy_spill.obj\n\
EXPORT_EXCLUDE lib/street/streetlights/HwyLt2BarMnt.obj ge_hwy_led_spill.obj\n\
EXPORT_EXCLUDE lib/street/streetlights/HwyLt3BarMnt.obj ge_hwy_spill.obj\n\
EXPORT_EXCLUDE lib/street/streetlights/ParkingLot_01.obj ge_com_spill.obj\n\
EXPORT_EXTEND lib/street/streetlights/ParkingLot_01.obj ge_flood_sml.obj\n\
EXPORT_EXCLUDE lib/street/streetlights/PrimaryLight_01.obj ge_pri_spill.obj\n\
EXPORT_EXTEND lib/street/streetlights/PrimaryLight_01.obj ge_pri_led_spill.obj\n\
EXPORT_EXTEND lib/street/streetlights/PrimaryLight_01.obj ge_pri_warm_spill.obj\n\
EXPORT_EXTEND lib/street/streetlights/PrimaryLight_01.obj ge_pri_dim_spill.obj\n\
EXPORT_EXCLUDE lib/street/streetlights/ResidentialLight_01V1.obj ge_res_spill.obj\n\
EXPORT_EXTEND lib/street/streetlights/ResidentialLight_01V1.obj ge_res_amber_spill.obj\n\
EXPORT_EXTEND lib/street/streetlights/ResidentialLight_01V1.obj ge_res_led_spill.obj\n\
EXPORT_EXTEND lib/street/streetlights/ResidentialLight_01V1.obj ge_res_dim_spill.obj\n\
EXPORT_EXCLUDE lib/street/streetlights/ResidentialLight_01V2.obj ge_res_amber_spill.obj\n\
EXPORT_EXTEND lib/street/streetlights/ResidentialLight_01V2.obj ge_res_spill.obj\n\
EXPORT_EXTEND lib/street/streetlights/ResidentialLight_01V2.obj ge_res_led_spill.obj\n\
EXPORT_EXTEND lib/street/streetlights/ResidentialLight_01V2.obj ge_res_dim_spill.obj\n\
EXPORT_EXCLUDE lib/street/streetlights/ResidentialLight_02.obj ge_res_led_spill.obj\n\
EXPORT_EXTEND lib/street/streetlights/ResidentialLight_02.obj ge_res_amber_spill.obj\n\
EXPORT_EXCLUDE lib/street/streetlights/ResidentialLight_02_dim.obj ge_res_dim_spill.obj\n\
EXPORT_EXCLUDE lib/street/streetlights/ResidentialLight_03.obj ge_res_spill.obj\n\
EXPORT_EXTEND lib/street/streetlights/ResidentialLight_03.obj ge_res_amber_spill.obj\n\
EXPORT_EXTEND lib/street/streetlights/ResidentialLight_03.obj ge_res_dim_spill.obj\n\
EXPORT_EXCLUDE lib/street/streetlights/ResidentialLight_04.obj ge_res_amber_spill.obj\n\
EXPORT_EXTEND lib/street/streetlights/ResidentialLight_04.obj ge_res_dim_spill.obj\n\
EXPORT_EXCLUDE lib/street/streetlights/RmpLt1BarMnt.obj ge_hwy_spill.obj\n\
EXPORT_EXCLUDE lib/street/streetlights/RmpLt2BarMnt.obj ge_hwy_spill.obj\n";

#[derive(Clone)]
enum Data {
    Directory,
    Inline(Arc<[u8]>),
    Asset(Spec),
}

struct Node {
    ino: u64,
    path: PathBuf,
    data: Data,
}

impl Node {
    fn is_generated_dds(&self) -> bool {
        matches!(
            &self.data,
            Data::Asset(Spec {
                kind: Kind::Day(_) | Kind::Night(_),
                ..
            })
        )
    }

    fn kind(&self) -> FileType {
        if matches!(self.data, Data::Directory) {
            FileType::Directory
        } else {
            FileType::RegularFile
        }
    }

    fn length(&self) -> u64 {
        match &self.data {
            Data::Directory => 0,
            Data::Inline(bytes) => bytes.len() as u64,
            Data::Asset(spec) => spec.length,
        }
    }

    fn attr(&self, uid: u32, gid: u32) -> FileAttr {
        FileAttr {
            ino: self.ino,
            size: self.length(),
            blocks: self.length().div_ceil(512),
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: self.kind(),
            perm: if self.kind() == FileType::Directory {
                0o555
            } else {
                0o444
            },
            nlink: if self.kind() == FileType::Directory {
                2
            } else {
                1
            },
            uid,
            gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }
}

struct Handle {
    node: Arc<Node>,
    cached: Mutex<Option<Arc<Cached>>>,
}

struct Consumer {
    started: String,
}

struct Tree {
    paths: BTreeMap<PathBuf, u64>,
    nodes: FxHashMap<u64, Arc<Node>>,
    sea_masks: FxHashMap<Footprint, (PathBuf, Stamp)>,
    day_groups: FxHashMap<Footprint, Vec<Spec>>,
    prepared_specs: FxHashMap<Footprint, (Spec, Spec)>,
    prepared_cells: FxHashSet<(i32, i32)>,
    active_dsf_cells: FxHashSet<(i32, i32)>,
    pending_dsf_cells: VecDeque<(i32, i32)>,
    cell_footprints: FxHashMap<(i32, i32), FxHashSet<Footprint>>,
    active_center: Option<(f64, f64)>,
    last_dsf_touch: Option<Instant>,
    last_demand_at: Option<Instant>,
    terrain_footprints: FxHashMap<u64, Footprint>,
    ter_loaded_footprints: FxHashSet<Footprint>,
    dds_loaded_footprints: FxHashSet<Footprint>,
    dsf_consumer_pid: Option<u32>,
    thread_groups: FxHashMap<u32, (u32, String)>,
    handles: FxHashMap<u64, Arc<Handle>>,
    consumers: FxHashMap<u32, Consumer>,
    next: u64,
    next_handle: u64,
    bytes: u64,
    reads: u64,
    served: u64,
    dds_header_reads: u64,
    dds_payload_reads: u64,
    dds_read_samples: Vec<[u64; 2]>,
    mips: Vec<u64>,
    last_reader: Option<String>,
}

fn in_conus_cell(lat: i32, lon: i32) -> bool {
    (-85..=84).contains(&lat) && (-180..=179).contains(&lon)
}

#[cfg(test)]
fn discover_global_scenery_roots() -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(not(test))]
fn discover_global_scenery_roots() -> Vec<PathBuf> {
    crate::dsf_compiler::discover_laminar_scenery_roots(None)
}

impl Tree {
    fn insert(&mut self, path: PathBuf, data: Data) -> Result<Arc<Node>> {
        if let Some(ino) = self.paths.get(&path) {
            let node = self.nodes.get(ino).context("missing reader inode")?;
            let same = match (&node.data, &data) {
                (Data::Directory, Data::Directory) => true,
                (Data::Inline(a), Data::Inline(b)) => a == b,
                (Data::Asset(a), Data::Asset(b)) => a.key == b.key && a.length == b.length,
                _ => false,
            };
            ensure!(
                same,
                "source metadata changed behind a published inode: {}",
                path.display()
            );
            return Ok(Arc::clone(node));
        }
        let bytes = 256
            + path.as_os_str().len() as u64 * 4
            + if let Data::Inline(b) = &data {
                b.len() as u64
            } else {
                0
            };
        ensure!(
            self.nodes.len() < MAX_NODES && self.bytes + bytes <= MAX_METADATA,
            "bounded reader metadata capacity reached"
        );
        let ino = self.next;
        self.next += 1;
        let node = Arc::new(Node {
            ino,
            path: path.clone(),
            data,
        });
        self.paths.insert(path, ino);
        self.nodes.insert(ino, Arc::clone(&node));
        self.bytes += bytes;
        Ok(node)
    }

    fn reap_consumers(&mut self) -> Result<()> {
        let mut expired = Vec::new();
        for (pid, consumer) in &self.consumers {
            match process_start(*pid) {
                Ok(started) if started == consumer.started => {}
                Ok(_) => expired.push(*pid),
                Err(e)
                    if e.downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
                {
                    expired.push(*pid)
                }
                Err(e) => return Err(e).context("checking observed reader process"),
            }
        }
        for pid in &expired {
            self.consumers.remove(pid);
        }
        if !expired.is_empty() {
            self.thread_groups
                .retain(|_, (group, _)| !expired.contains(group));
        }
        Ok(())
    }

    fn expand_active_dsf_box(&mut self) {
        if self.active_dsf_cells.is_empty() {
            return;
        }
        let min_lat = self.active_dsf_cells.iter().map(|c| c.0).min().unwrap_or(0);
        let max_lat = self.active_dsf_cells.iter().map(|c| c.0).max().unwrap_or(0);
        let min_lon = self.active_dsf_cells.iter().map(|c| c.1).min().unwrap_or(0);
        let max_lon = self.active_dsf_cells.iter().map(|c| c.1).max().unwrap_or(0);
        if max_lat - min_lat <= 2 && max_lon - min_lon <= 3 {
            for lat in min_lat..=max_lat {
                for lon in min_lon..=max_lon {
                    if in_conus_cell(lat, lon) {
                        self.active_dsf_cells.insert((lat, lon));
                        if !self.prepared_cells.contains(&(lat, lon))
                            && !self.pending_dsf_cells.contains(&(lat, lon))
                        {
                            self.pending_dsf_cells.push_back((lat, lon));
                        }
                    }
                }
            }
            for lat in (min_lat - 1)..=(max_lat + 1) {
                for lon in (min_lon - 1)..=(max_lon + 1) {
                    if in_conus_cell(lat, lon)
                        && !self.prepared_cells.contains(&(lat, lon))
                        && !self.pending_dsf_cells.contains(&(lat, lon))
                    {
                        self.pending_dsf_cells.push_back((lat, lon));
                    }
                }
            }
            if max_lat > min_lat || max_lon > min_lon {
                self.active_center = Some((
                    (min_lat as f64 + max_lat as f64 + 1.0) * 0.5,
                    (min_lon as f64 + max_lon as f64 + 1.0) * 0.5,
                ));
            }
        }
    }

    fn note_dsf_touch(&mut self, cell: (i32, i32), now: Instant) -> bool {
        if !in_conus_cell(cell.0, cell.1) {
            return false;
        }
        let center = (cell.0 as f64 + 0.5, cell.1 as f64 + 0.5);
        let far_from_active = self.active_center.is_some_and(|(clat, clon)| {
            (center.0 - clat).abs() > 2.5 || (center.1 - clon).abs() > 3.5
        });
        if far_from_active {
            self.active_dsf_cells.clear();
            self.pending_dsf_cells.clear();
            self.ter_loaded_footprints.clear();
            self.dds_loaded_footprints.clear();
            self.active_center = Some(center);
        }
        self.last_dsf_touch = Some(now);
        self.last_demand_at = Some(now);
        self.active_dsf_cells.insert(cell);
        if self.active_dsf_cells.len() > 12 {
            let ref_center = self.active_center.unwrap_or(center);
            self.active_dsf_cells.retain(|&(lat, lon)| {
                (lat, lon) == cell
                    || (((lat as f64 + 0.5) - ref_center.0).abs() <= 1.6
                        && ((lon as f64 + 0.5) - ref_center.1).abs() <= 2.1)
            });
        }
        if !self.prepared_cells.contains(&cell) && !self.pending_dsf_cells.contains(&cell) {
            self.pending_dsf_cells.push_back(cell);
        }
        self.expand_active_dsf_box();
        if self.active_center.is_none() {
            self.active_center = Some(center);
        }
        !self.pending_dsf_cells.is_empty()
    }

    fn seed_aircraft_window(&mut self, ac_lat: f64, ac_lon: f64, now: Instant) -> bool {
        let base_lat = ac_lat.floor() as i32;
        let base_lon = ac_lon.round() as i32;
        if !in_conus_cell(base_lat, base_lon) {
            return false;
        }
        let far = self
            .active_center
            .is_some_and(|(clat, clon)| (ac_lat - clat).abs() > 2.5 || (ac_lon - clon).abs() > 3.5);
        if far {
            self.active_dsf_cells.clear();
            self.pending_dsf_cells.clear();
            self.ter_loaded_footprints.clear();
            self.dds_loaded_footprints.clear();
        }
        self.active_center = Some((ac_lat, ac_lon));
        self.last_dsf_touch = Some(now);
        self.active_dsf_cells.retain(|&(lat, lon)| {
            (lat - base_lat).abs() <= 1 && (base_lon - 2..=base_lon + 1).contains(&lon)
        });
        for lat in (base_lat - 1)..=(base_lat + 1) {
            for lon in (base_lon - 2)..=(base_lon + 1) {
                if in_conus_cell(lat, lon) {
                    self.active_dsf_cells.insert((lat, lon));
                    if !self.prepared_cells.contains(&(lat, lon))
                        && !self.pending_dsf_cells.contains(&(lat, lon))
                    {
                        self.pending_dsf_cells.push_back((lat, lon));
                    }
                }
            }
        }
        !self.pending_dsf_cells.is_empty()
    }

    fn note_footprint_demand(&mut self, fp: Footprint, now: Instant) -> bool {
        let b = fp.geographic();
        let lat = (b.north + b.south) * 0.5;
        let lon = (b.east + b.west) * 0.5;
        self.last_demand_at = Some(now);
        let far = self
            .active_center
            .is_some_and(|(clat, clon)| (lat - clat).abs() > 2.5 || (lon - clon).abs() > 3.5);
        if far || self.active_dsf_cells.is_empty() {
            self.seed_aircraft_window(lat, lon, now)
        } else {
            false
        }
    }
}

fn process_start(pid: u32) -> Result<String> {
    let stat = String::from_utf8(bounded_read(
        &PathBuf::from(format!("/proc/{pid}/stat")),
        65536,
    )?)?;
    Ok(stat
        .rsplit_once(')')
        .context("invalid process stat")?
        .1
        .split_whitespace()
        .nth(19)
        .context("process start time is missing")?
        .to_owned())
}

pub struct Scene {
    config: StreamConfig,
    root: PathBuf,
    dsf_cache_dir: PathBuf,
    global_scenery_roots: Vec<PathBuf>,
    dsf_compile_locks: Mutex<FxHashMap<PathBuf, Arc<Mutex<()>>>>,
    tree: Mutex<Tree>,
    dsf_prep_cv: Condvar,
    filesystem_errors: AtomicU64,
    filesystem_error: Mutex<Option<(String, Instant)>>,
    pub mounted: AtomicBool,
    pub accepting: AtomicBool,
    uid: u32,
    gid: u32,
    night_source: Vec<u8>,
}

impl Scene {
    pub fn new(config: StreamConfig) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let own = fs::metadata("/proc/self")?;
        let osm_stamp = Stamp::read_source(Path::new(&config.osm_db))?;
        let viirs_stamp = config
            .viirs
            .as_deref()
            .map(Path::new)
            .map(Stamp::read)
            .transpose()?;
        let night_source = serde_json::to_vec(&(
            "lighting-v7",
            osm_stamp,
            viirs_stamp,
            config.exposure.to_bits(),
        ))?;
        let mut tree = Tree {
            paths: BTreeMap::new(),
            nodes: FxHashMap::default(),
            sea_masks: FxHashMap::default(),
            day_groups: FxHashMap::default(),
            prepared_specs: FxHashMap::default(),
            prepared_cells: FxHashSet::default(),
            active_dsf_cells: FxHashSet::default(),
            pending_dsf_cells: VecDeque::new(),
            cell_footprints: FxHashMap::default(),
            active_center: None,
            last_dsf_touch: None,
            last_demand_at: None,
            terrain_footprints: FxHashMap::default(),
            ter_loaded_footprints: FxHashSet::default(),
            dds_loaded_footprints: FxHashSet::default(),
            dsf_consumer_pid: None,
            thread_groups: FxHashMap::default(),
            handles: FxHashMap::default(),
            consumers: FxHashMap::default(),
            next: 1,
            next_handle: 1,
            bytes: 0,
            reads: 0,
            served: 0,
            dds_header_reads: 0,
            dds_payload_reads: 0,
            dds_read_samples: Vec::new(),
            mips: vec![0; config.size.ilog2() as usize + 1],
            last_reader: None,
        };
        for path in ["", "Earth nav data", "terrain", "textures"] {
            tree.insert(PathBuf::from(path), Data::Directory)?;
        }
        tree.insert(
            PathBuf::from("library.txt"),
            Data::Inline(Arc::from(LIBRARY_TXT)),
        )?;
        for &(name, bytes) in ROOT_LIGHT_OBJS {
            tree.insert(PathBuf::from(name), Data::Inline(Arc::from(bytes)))?;
        }
        let mount_parent = Path::new(&config.mount_point)
            .parent()
            .unwrap_or_else(|| Path::new("/tmp"));
        let dsf_cache_dir = mount_parent.join("reader-cache/dsf");
        let root = if config.source_package.trim().is_empty() {
            let standalone = mount_parent.join("reader-cache/standalone-source");
            for sub in ["Earth nav data", "terrain", "textures"] {
                fs::create_dir_all(standalone.join(sub))?;
            }
            standalone.canonicalize()?
        } else {
            Path::new(&config.source_package).canonicalize()?
        };
        Ok(Self {
            root,
            dsf_cache_dir,
            global_scenery_roots: discover_global_scenery_roots(),
            dsf_compile_locks: Mutex::new(FxHashMap::default()),
            config,
            tree: Mutex::new(tree),
            dsf_prep_cv: Condvar::new(),
            filesystem_errors: AtomicU64::new(0),
            filesystem_error: Mutex::new(None),
            mounted: AtomicBool::new(false),
            accepting: AtomicBool::new(true),
            uid: own.uid(),
            gid: own.gid(),
            night_source,
        })
    }

    pub fn dsf_cell(path: &Path) -> Option<(i32, i32)> {
        let stem = path.file_stem().and_then(OsStr::to_str)?;
        if path.extension().is_none_or(|e| e != "dsf") {
            return None;
        }
        let (north, west) = (stem.get(..3)?, stem.get(3..)?);
        let (Ok(n), Ok(w)) = (north.parse::<i32>(), west.parse::<i32>()) else {
            return None;
        };
        if in_conus_cell(n, w) {
            Some((n, w))
        } else {
            None
        }
    }

    pub fn next_pending_dsf_cell(&self) -> Result<Option<(i32, i32)>> {
        let mut tree = lock(&self.tree)?;
        loop {
            if !self.accepting.load(Ordering::Acquire) {
                return Ok(None);
            }
            if let Some(cell) = tree.pending_dsf_cells.pop_front() {
                return Ok(Some(cell));
            }
            tree = self
                .dsf_prep_cv
                .wait(tree)
                .map_err(|_| anyhow::anyhow!("reader lock poisoned"))?;
        }
    }

    pub fn source_label(&self) -> String {
        self.config.tile_source.label().to_owned()
    }

    fn physical(&self, relative: &Path) -> Result<Option<PathBuf>> {
        let path = self.root.join(relative);
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                let path = path.canonicalize()?;
                ensure!(
                    path.starts_with(&self.root),
                    "source resource escapes package"
                );
                Ok(Some(path))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("examining physical resource"),
        }
    }

    fn existing_dsf(&self, relative: &Path) -> Result<Option<PathBuf>> {
        if let Some(path) = self.physical(relative)? {
            return Ok(Some(path));
        }
        let cached = self.dsf_cache_dir.join(relative);
        if cached.is_file() {
            return Ok(Some(cached.canonicalize()?));
        }
        Ok(None)
    }

    fn has_resolvable_dsf(&self, relative: &Path) -> Result<bool> {
        if self.existing_dsf(relative)?.is_some() {
            return Ok(true);
        }
        Ok(self
            .global_scenery_roots
            .iter()
            .any(|g| g.join(relative).is_file()))
    }

    fn resolve_dsf(&self, relative: &Path) -> Result<Option<PathBuf>> {
        if let Some(path) = self.existing_dsf(relative)? {
            return Ok(Some(path));
        }
        let Some(global_dsf) = self
            .global_scenery_roots
            .iter()
            .map(|g| g.join(relative))
            .find(|p| p.is_file())
        else {
            return Ok(None);
        };
        let cell_lock = {
            let mut map = lock(&self.dsf_compile_locks)?;
            Arc::clone(
                map.entry(relative.to_path_buf())
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };
        let _guard = lock(&cell_lock)?;
        if let Some(path) = self.existing_dsf(relative)? {
            return Ok(Some(path));
        }
        let cached = self.dsf_cache_dir.join(relative);
        scenery::compile_global_dsf_to_ortho(&global_dsf, &cached)?;
        Ok(Some(cached.canonicalize()?))
    }

    fn supported_cell(path: &Path) -> bool {
        Self::dsf_cell(path).is_some()
    }

    fn raw(&self, relative: PathBuf, dsf: bool) -> Result<Arc<Node>> {
        let path = if dsf {
            self.resolve_dsf(&relative)?
        } else {
            self.physical(&relative)?
        }
        .context("missing mesh/material resource")?;
        let stamp = Stamp::read(&path)?;
        let limit = if dsf { scenery::MAX_DSF_BYTES } else { MAX_RAW };
        ensure!(
            stamp.length <= limit,
            "mesh/material exceeds {limit}-byte reader limit: {} ({} bytes)",
            path.display(),
            stamp.length
        );
        let key = digest(&serde_json::to_vec(&(
            "groundeffect/raw/v1",
            &path,
            &stamp,
        ))?);
        lock(&self.tree)?.insert(
            relative,
            Data::Asset(Spec {
                key,
                length: stamp.length,
                kind: Kind::Raw { path, stamp, dsf },
            }),
        )
    }

    pub fn sea_mask(&self, key: Footprint) -> Result<Option<(PathBuf, Stamp)>> {
        let overlay = PathBuf::from(format!(
            "terrain/{}_{}_BI{}_sea_overlay.ter",
            key.row, key.col, key.zoom
        ));
        let Some(overlay_path) = self.physical(&overlay)? else {
            ensure!(
                !lock(&self.tree)?.sea_masks.contains_key(&key),
                "coastal terrain disappeared behind an active footprint: {}",
                overlay.display()
            );
            return Ok(None);
        };
        let before = Stamp::read(&overlay_path)?;
        let text = String::from_utf8(bounded_read(&overlay_path, 65536)?)?;
        before.check(&overlay_path)?;
        let relative_text = overlay
            .to_str()
            .context("non-UTF8 terrain path")?
            .to_owned();
        let terrain = scenery::parse_terrain(relative_text, text)
            .with_context(|| format!("invalid coastal terrain resource {}", overlay.display()))?;
        ensure!(
            terrain.footprint == key,
            "coastal terrain footprint mismatch: {}",
            overlay.display()
        );
        let expected_border = format!("../textures/{}_{}_ZL{}.png", key.row, key.col, key.zoom);
        ensure!(
            terrain.extras.iter().any(|extra| extra == &expected_border),
            "coastal terrain lacks expected coastline mask {}: {}",
            expected_border,
            overlay.display()
        );
        let mask_relative = PathBuf::from(format!(
            "textures/{}_{}_ZL{}.png",
            key.row, key.col, key.zoom
        ));
        let mask_path = self
            .physical(&mask_relative)?
            .with_context(|| format!("missing coastline mask {}", mask_relative.display()))?;
        let stamp = Stamp::read(&mask_path)?;
        ensure!(
            stamp.length <= MAX_RAW,
            "coastline mask exceeds {MAX_RAW}-byte reader limit: {} ({} bytes)",
            mask_path.display(),
            stamp.length
        );
        let mut tree = lock(&self.tree)?;
        if let Some((recorded_path, recorded_stamp)) = tree.sea_masks.get(&key) {
            ensure!(
                recorded_path == &mask_path,
                "coastline mask path changed: {}",
                mask_path.display()
            );
            recorded_stamp.check(&mask_path)?;
        } else {
            tree.sea_masks
                .insert(key, (mask_path.clone(), stamp.clone()));
        }
        Ok(Some((mask_path, stamp)))
    }

    fn terrain(&self, relative: PathBuf) -> Result<Arc<Node>> {
        let relative_text = relative
            .to_str()
            .context("non-UTF8 terrain path")?
            .to_owned();
        let terrain = if let Some(source) = self.physical(&relative)? {
            let before = Stamp::read(&source)?;
            let text = String::from_utf8(bounded_read(&source, 65536)?)?;
            before.check(&source)?;
            scenery::parse_terrain(relative_text, text)
                .with_context(|| format!("invalid terrain resource {}", relative.display()))?
        } else {
            let fp = Footprint::parse_terrain_name(&relative_text)?;
            let mask_rel =
                PathBuf::from(format!("textures/{}_{}_ZL{}.png", fp.row, fp.col, fp.zoom));
            let has_border = self.physical(&mask_rel)?.is_some();
            scenery::synthesize_terrain(relative_text, has_border)
                .with_context(|| format!("synthesizing terrain resource {}", relative.display()))?
        };
        ensure!(
            in_imagery_envelope(terrain.footprint),
            "terrain is outside the imagery qualification envelope"
        );
        let (day_identity, mut night_identity) =
            if let Some((mask_path, mask_stamp)) = self.sea_mask(terrain.footprint)? {
                (
                    serde_json::to_vec(&(
                        self.config.tile_source.coastal_identity_tag(),
                        &self.config.source_revision,
                        self.config.size,
                        terrain.footprint.row,
                        terrain.footprint.col,
                        terrain.footprint.zoom,
                        terrain.load_center.map(f64::to_bits),
                        &mask_path,
                        &mask_stamp,
                    ))?,
                    serde_json::to_vec(&(
                        NIGHT_COASTAL_IDENTITY_TAG,
                        &self.config.source_revision,
                        self.config.size,
                        terrain.footprint.row,
                        terrain.footprint.col,
                        terrain.footprint.zoom,
                        terrain.load_center.map(f64::to_bits),
                        mask_path,
                        mask_stamp,
                    ))?,
                )
            } else {
                (
                    serde_json::to_vec(&(
                        self.config.tile_source.identity_tag(),
                        &self.config.source_revision,
                        self.config.size,
                        terrain.footprint.row,
                        terrain.footprint.col,
                        terrain.footprint.zoom,
                        terrain.load_center.map(f64::to_bits),
                    ))?,
                    serde_json::to_vec(&(
                        NIGHT_IDENTITY_TAG,
                        &self.config.source_revision,
                        self.config.size,
                        terrain.footprint.row,
                        terrain.footprint.col,
                        terrain.footprint.zoom,
                        terrain.load_center.map(f64::to_bits),
                    ))?,
                )
            };
        let day = digest(&day_identity);
        night_identity.extend_from_slice(&self.night_source);
        let night = digest(&night_identity);
        let base = format!("textures/{day}_GE.dds");
        let lit = format!("textures/{night}_GE_LIT.dds");
        let text = terrain.compiled_text(self.config.size, &base, &lit);
        for extra in &terrain.extras {
            let dependency = scenery::dependency_path(&terrain.path, extra)?;
            ensure!(
                dependency.parent() == Some(Path::new("textures")),
                "material requires flat textures resources"
            );
            if !lock(&self.tree)?.paths.contains_key(&dependency) {
                self.raw(dependency, false)?;
            }
        }
        let mut tree = lock(&self.tree)?;
        let length = dds::total_bytes(self.config.size, 8) as u64;
        let day_spec = Spec {
            key: day.clone(),
            length,
            kind: Kind::Day(terrain.footprint),
        };
        let night_spec = Spec {
            key: night.clone(),
            length,
            kind: Kind::Night(terrain.footprint),
        };
        let base_node = tree.insert(PathBuf::from(base), Data::Asset(day_spec.clone()))?;
        let _ = base_node;
        let _lit_node = tree.insert(PathBuf::from(lit), Data::Asset(night_spec.clone()))?;
        let parent = source_window(terrain.footprint, self.config.size).0;
        let group = tree.day_groups.entry(parent).or_default();
        if !group.iter().any(|existing| existing.key == day_spec.key) {
            group.push(day_spec.clone());
        }
        tree.prepared_specs
            .entry(terrain.footprint)
            .or_insert((day_spec, night_spec));
        let node = tree.insert(relative, Data::Inline(Arc::from(text.into_bytes())))?;
        tree.terrain_footprints.insert(node.ino, terrain.footprint);
        Ok(node)
    }

    pub fn sibling_day_specs(&self, key: Footprint) -> Result<Vec<Spec>> {
        let parent = source_window(key, self.config.size).0;
        let tree = lock(&self.tree)?;
        Ok(tree
            .day_groups
            .get(&parent)
            .into_iter()
            .flatten()
            .filter(|spec| !matches!(spec.kind, Kind::Day(footprint) if footprint == key))
            .cloned()
            .collect())
    }

    pub fn prepared_specs(&self) -> Result<Vec<(Footprint, Spec, Spec)>> {
        let tree = lock(&self.tree)?;
        Ok(tree
            .prepared_specs
            .iter()
            .map(|(&fp, (day, night))| (fp, day.clone(), night.clone()))
            .collect())
    }

    pub fn day_key(&self, key: Footprint) -> Result<Option<String>> {
        let tree = lock(&self.tree)?;
        Ok(tree
            .prepared_specs
            .get(&key)
            .map(|(day, _)| day.key.clone()))
    }

    pub fn has_prepared_terrains(&self) -> Result<bool> {
        Ok(!lock(&self.tree)?.prepared_specs.is_empty())
    }

    pub fn request_dsf_cell(&self, lat_floor: i32, lon_floor: i32) -> Result<bool> {
        let bucket_lat = lat_floor.div_euclid(10) * 10;
        let bucket_lon = lon_floor.div_euclid(10) * 10;
        let relative = PathBuf::from(format!(
            "Earth nav data/{bucket_lat:+03}{bucket_lon:+04}/{lat_floor:+03}{lon_floor:+04}.dsf"
        ));
        if !Self::supported_cell(&relative) {
            return Ok(false);
        }
        if self.existing_dsf(&relative)?.is_some() {
            return self.prepare_dsf_cell(lat_floor, lon_floor);
        }
        if self
            .global_scenery_roots
            .iter()
            .any(|g| g.join(&relative).is_file())
        {
            let mut tree = lock(&self.tree)?;
            if !tree.prepared_cells.contains(&(lat_floor, lon_floor))
                && !tree.pending_dsf_cells.contains(&(lat_floor, lon_floor))
            {
                tree.pending_dsf_cells.push_back((lat_floor, lon_floor));
                drop(tree);
                self.dsf_prep_cv.notify_all();
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn prepare_dsf_cell(&self, lat_floor: i32, lon_floor: i32) -> Result<bool> {
        let pending_snapshot: Vec<(i32, i32)> = {
            let tree = lock(&self.tree)?;
            if tree.prepared_cells.contains(&(lat_floor, lon_floor)) {
                return Ok(false);
            }
            std::iter::once((lat_floor, lon_floor))
                .chain(tree.pending_dsf_cells.iter().copied())
                .filter(|cell| !tree.cell_footprints.contains_key(cell))
                .collect()
        };
        for (clat, clon) in pending_snapshot {
            let b_lat = clat.div_euclid(10) * 10;
            let b_lon = clon.div_euclid(10) * 10;
            let rel = PathBuf::from(format!(
                "Earth nav data/{b_lat:+03}{b_lon:+04}/{clat:+03}{clon:+04}.dsf"
            ));
            if Self::supported_cell(&rel)
                && let Ok(Some(p)) = self.existing_dsf(&rel)
                && let Ok(s) = Stamp::read(&p)
                && let Ok(mut f) = File::open(&p)
                && let Ok(terrains) = scenery::read_dsf_terrain_table(
                    &mut BufReader::with_capacity(16384, &mut f),
                    s.length,
                )
            {
                let fast_fps: FxHashSet<Footprint> = scenery::package_terrains(&terrains)
                    .filter_map(|t| Footprint::parse_terrain_name(t).ok())
                    .collect();
                if !fast_fps.is_empty()
                    && let Ok(mut tree) = lock(&self.tree)
                {
                    tree.cell_footprints.insert((clat, clon), fast_fps);
                }
            }
        }
        let bucket_lat = lat_floor.div_euclid(10) * 10;
        let bucket_lon = lon_floor.div_euclid(10) * 10;
        let relative = PathBuf::from(format!(
            "Earth nav data/{bucket_lat:+03}{bucket_lon:+04}/{lat_floor:+03}{lon_floor:+04}.dsf"
        ));
        if !Self::supported_cell(&relative) {
            lock(&self.tree)?
                .prepared_cells
                .insert((lat_floor, lon_floor));
            return Ok(false);
        }
        let Some(path) = self.resolve_dsf(&relative)? else {
            lock(&self.tree)?
                .prepared_cells
                .insert((lat_floor, lon_floor));
            return Ok(false);
        };
        let stamp = Stamp::read(&path)?;
        let mut file = File::open(&path)?;
        let terrains = scenery::read_dsf_terrain_table(
            &mut BufReader::with_capacity(16384, &mut file),
            stamp.length,
        )?;
        stamp.check(&path)?;
        self.prepare_dsf_terrains(Some((lat_floor, lon_floor)), &terrains)?;
        Ok(true)
    }

    #[cfg(test)]
    pub fn prepare_terrains(&self, terrains: &[String]) -> Result<()> {
        self.prepare_dsf_terrains(None, terrains)
    }

    pub fn prepare_dsf_terrains(
        &self,
        cell: Option<(i32, i32)>,
        terrains: &[String],
    ) -> Result<()> {
        if let Some(cell) = cell {
            let fast_fps: Vec<Footprint> = scenery::package_terrains(terrains)
                .filter_map(|t| Footprint::parse_terrain_name(t).ok())
                .collect();
            if !fast_fps.is_empty() {
                let mut tree = lock(&self.tree)?;
                let entry = tree.cell_footprints.entry(cell).or_default();
                for fp in fast_fps {
                    entry.insert(fp);
                }
            }
        }
        let mut cell_fps = Vec::new();
        for terrain in scenery::package_terrains(terrains) {
            let relative = scenery::relative_path(terrain)?;
            let existing_fp = {
                let tree = lock(&self.tree)?;
                tree.paths
                    .get(&relative)
                    .and_then(|ino| tree.terrain_footprints.get(ino).copied())
            };
            if let Some(fp) = existing_fp {
                cell_fps.push(fp);
                continue;
            }
            let node = self.terrain(relative)?;
            if let Some(fp) = lock(&self.tree)?.terrain_footprints.get(&node.ino).copied() {
                cell_fps.push(fp);
            }
        }
        let mut tree = lock(&self.tree)?;
        if let Some(cell) = cell {
            tree.prepared_cells.insert(cell);
            let entry = tree.cell_footprints.entry(cell).or_default();
            for fp in cell_fps {
                entry.insert(fp);
            }
        } else {
            for fp in cell_fps {
                let b = fp.geographic();
                let fp_cell = (
                    ((b.north + b.south) * 0.5).floor() as i32,
                    ((b.east + b.west) * 0.5).floor() as i32,
                );
                tree.cell_footprints.entry(fp_cell).or_default().insert(fp);
            }
        }
        Ok(())
    }

    fn lookup(&self, parent: u64, name: &OsStr) -> Result<Option<Arc<Node>>> {
        let name = name.to_str().context("resource name is not UTF-8")?;
        ensure!(
            name.len() <= 255 && !name.contains('/') && name != "." && name != "..",
            "invalid resource name"
        );
        let path = {
            let tree = lock(&self.tree)?;
            let parent = tree.nodes.get(&parent).context("unknown parent inode")?;
            ensure!(
                parent.kind() == FileType::Directory,
                "parent is not a directory"
            );
            let path = parent.path.join(name);
            if let Some(ino) = tree.paths.get(&path) {
                return Ok(tree.nodes.get(ino).cloned());
            }
            path
        };
        if path.parent() == Some(Path::new("Earth nav data")) {
            let exists = self.physical(&path)?.is_some_and(|s| s.is_dir())
                || self.dsf_cache_dir.join(&path).is_dir()
                || self
                    .global_scenery_roots
                    .iter()
                    .any(|g| g.join(&path).is_dir());
            if exists {
                return Ok(Some(lock(&self.tree)?.insert(path, Data::Directory)?));
            }
        } else if path.starts_with("Earth nav data") && Self::supported_cell(&path) {
            if self.has_resolvable_dsf(&path)? {
                return self.raw(path, true).map(Some);
            }
        } else if path.parent() == Some(Path::new("terrain"))
            && path.extension().is_some_and(|s| s == "ter")
            && (self.physical(&path)?.is_some() || Footprint::parse_terrain_name(name).is_ok())
        {
            return self.terrain(path).map(Some);
        }
        Ok(None)
    }

    fn directory(&self, ino: u64) -> Result<Vec<(u64, FileType, String)>> {
        let (path, mut entries) = {
            let tree = lock(&self.tree)?;
            let node = tree.nodes.get(&ino).context("unknown directory inode")?;
            ensure!(
                node.kind() == FileType::Directory,
                "inode is not a directory"
            );
            let parent = node
                .path
                .parent()
                .and_then(|p| tree.paths.get(p))
                .copied()
                .unwrap_or(1);
            (
                node.path.clone(),
                vec![
                    (ino, FileType::Directory, ".".into()),
                    (parent, FileType::Directory, "..".into()),
                ],
            )
        };
        if path.starts_with("Earth nav data") {
            let mut children = BTreeSet::new();
            let search_roots: Vec<&Path> = if path == Path::new("Earth nav data") {
                std::iter::once(self.root.as_path())
                    .chain(std::iter::once(self.dsf_cache_dir.as_path()))
                    .chain(self.global_scenery_roots.iter().map(PathBuf::as_path))
                    .collect()
            } else {
                vec![self.root.as_path(), self.dsf_cache_dir.as_path()]
            };
            let mut found_any_dir = false;
            for base in search_roots {
                let dir = base.join(&path);
                let Ok(rd) = fs::read_dir(&dir) else {
                    continue;
                };
                found_any_dir = true;
                for item in rd {
                    let item = item?;
                    let name = item
                        .file_name()
                        .into_string()
                        .map_err(|_| anyhow::anyhow!("non-UTF8 source filename"))?;
                    let child = path.join(&name);
                    if (path == Path::new("Earth nav data") && item.file_type()?.is_dir())
                        || Self::supported_cell(&child)
                    {
                        ensure!(
                            children.len() < 65536,
                            "source directory exceeds metadata bound"
                        );
                        children.insert(name);
                    }
                }
            }
            if !found_any_dir && let Some(source) = self.physical(&path)? {
                let _ = fs::read_dir(source)?;
            }
            for name in children {
                if let Some(node) = self.lookup(ino, OsStr::new(&name))? {
                    entries.push((node.ino, node.kind(), name));
                }
            }
        } else if path.as_os_str().is_empty() {
            let tree = lock(&self.tree)?;
            for name in ["Earth nav data", "terrain", "textures", "library.txt"]
                .into_iter()
                .chain(ROOT_LIGHT_OBJS.iter().map(|&(n, _)| n))
            {
                if let Some(&child_ino) = tree.paths.get(Path::new(name))
                    && let Some(node) = tree.nodes.get(&child_ino)
                {
                    entries.push((child_ino, node.kind(), name.to_owned()));
                }
            }
        }
        Ok(entries)
    }

    fn open(&self, ino: u64, pid: u32) -> Result<u64> {
        let (node, cached_proc) = {
            let tree = lock(&self.tree)?;
            ensure!(self.accepting.load(Ordering::Acquire), "reader is stopping");
            ensure!(
                tree.handles.len() < MAX_HANDLES,
                "bounded open-handle limit reached"
            );
            let node = Arc::clone(tree.nodes.get(&ino).context("unknown file inode")?);
            ensure!(
                node.kind() == FileType::RegularFile,
                "cannot open directory as file"
            );
            let cached_proc = if pid != 0 {
                tree.thread_groups
                    .get(&pid)
                    .cloned()
                    .filter(|(group, _)| tree.consumers.contains_key(group))
            } else {
                None
            };
            (node, cached_proc)
        };
        let inspected_proc = if pid != 0 && cached_proc.is_none() {
            let status = String::from_utf8(bounded_read(
                &PathBuf::from(format!("/proc/{pid}/status")),
                65536,
            )?)?;
            let group: u32 = status
                .lines()
                .find_map(|line| line.strip_prefix("Tgid:"))
                .context("reader process lacks Tgid")?
                .trim()
                .parse()?;
            let executable = fs::read_link(format!("/proc/{group}/exe"))?;
            let exe_name = executable
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or("reader")
                .to_owned();
            let started = process_start(group).ok();
            Some((group, exe_name, started))
        } else {
            None
        };
        let mut tree = lock(&self.tree)?;
        ensure!(self.accepting.load(Ordering::Acquire), "reader is stopping");
        ensure!(
            tree.handles.len() < MAX_HANDLES,
            "bounded open-handle limit reached"
        );
        if pid != 0 {
            let is_dsf = matches!(
                &node.data,
                Data::Asset(Spec {
                    kind: Kind::Raw { dsf: true, .. },
                    ..
                })
            );
            let (group, exe_name) = if let Some(pair) = cached_proc {
                pair
            } else if let Some((group, exe_name, started)) = inspected_proc {
                if (exe_name.starts_with("X-Plane") || is_dsf)
                    && !tree.consumers.contains_key(&group)
                {
                    tree.reap_consumers()?;
                    ensure!(
                        tree.consumers.len() < 128 || tree.consumers.contains_key(&group),
                        "reader process bound reached"
                    );
                    let started = match started {
                        Some(s) => s,
                        None => process_start(group)?,
                    };
                    tree.consumers.insert(group, Consumer { started });
                }
                if tree.thread_groups.len() >= 4096 {
                    let active_groups: FxHashSet<u32> = tree.consumers.keys().copied().collect();
                    tree.thread_groups
                        .retain(|_, (g, _)| active_groups.contains(g));
                    if tree.thread_groups.len() >= 4096 {
                        tree.thread_groups.clear();
                    }
                }
                tree.thread_groups.insert(pid, (group, exe_name.clone()));
                (group, exe_name)
            } else {
                unreachable!()
            };
            if (exe_name.starts_with("X-Plane") || is_dsf) && tree.dsf_consumer_pid != Some(group) {
                tree.dsf_consumer_pid = Some(group);
                tree.active_dsf_cells.clear();
                tree.pending_dsf_cells.clear();
                tree.active_center = None;
                tree.ter_loaded_footprints.clear();
                tree.dds_loaded_footprints.clear();
            }
            tree.last_reader = Some(format!("{exe_name} PID {group} / {}", node.path.display()));
        }
        let now = Instant::now();
        let wake_prep = if let Some(cell) = Self::dsf_cell(&node.path) {
            tree.note_dsf_touch(cell, now)
        } else if let Some(&fp) = tree.terrain_footprints.get(&node.ino) {
            let wake = tree.note_footprint_demand(fp, now);
            tree.ter_loaded_footprints.insert(fp);
            wake
        } else if let Data::Asset(Spec {
            kind: Kind::Day(fp) | Kind::Night(fp),
            ..
        }) = &node.data
        {
            let fp = *fp;
            let wake = tree.note_footprint_demand(fp, now);
            tree.ter_loaded_footprints.insert(fp);
            wake
        } else {
            false
        };
        let handle = tree.next_handle;
        tree.next_handle += 1;
        tree.handles.insert(
            handle,
            Arc::new(Handle {
                node,
                cached: Mutex::new(None),
            }),
        );
        drop(tree);
        if wake_prep {
            self.dsf_prep_cv.notify_one();
        }
        Ok(handle)
    }

    fn header_range(&self, node: &Node, offset: u64, size: u32) -> Option<Vec<u8>> {
        if node.is_generated_dds() && offset < 128 && offset.saturating_add(size as u64) <= 128 {
            let header = dds::header(self.config.size, 8);
            Some(header[offset as usize..offset as usize + size as usize].to_vec())
        } else {
            None
        }
    }

    fn trace(&self, node: &Node, offset: u64, bytes: usize) -> Result<()> {
        let mut tree = lock(&self.tree)?;
        tree.reads += 1;
        tree.served += bytes as u64;
        let now = Instant::now();
        let mut wake_prep = false;
        if let Some(&fp) = tree.terrain_footprints.get(&node.ino) {
            wake_prep |= tree.note_footprint_demand(fp, now);
            tree.ter_loaded_footprints.insert(fp);
        }
        if node.is_generated_dds() {
            if let Data::Asset(Spec {
                kind: Kind::Day(fp) | Kind::Night(fp),
                ..
            }) = &node.data
            {
                let fp = *fp;
                wake_prep |= tree.note_footprint_demand(fp, now);
                tree.ter_loaded_footprints.insert(fp);
                tree.dds_loaded_footprints.insert(fp);
            }
            if tree.dds_read_samples.len() < 16 {
                tree.dds_read_samples.push([offset, bytes as u64]);
            }
            if offset.saturating_add(bytes as u64) <= 128 {
                tree.dds_header_reads += 1;
            } else {
                tree.dds_payload_reads += 1;
            }
            let mut start = 128_u64;
            for mip in 0..tree.mips.len() {
                let end = start + dds::level_bytes((self.config.size >> mip).max(1), 8) as u64;
                if offset < end && offset.saturating_add(bytes as u64) > start {
                    tree.mips[mip] += 1;
                }
                start = end;
            }
        }
        drop(tree);
        if wake_prep {
            self.dsf_prep_cv.notify_one();
        }
        Ok(())
    }

    pub fn status_with_tiles(
        &self,
        status: &mut StreamStatus,
        tile_states: &[(Footprint, bool, bool)],
    ) -> Result<()> {
        let mut tree = lock(&self.tree)?;
        tree.reap_consumers()?;
        status.open_handles = tree.handles.len();
        status.active_consumers = tree.consumers.len();
        status.reads = tree.reads;
        status.served_bytes = tree.served;
        status.dds_header_reads = tree.dds_header_reads;
        status.dds_payload_reads = tree.dds_payload_reads;
        status.dds_read_samples = tree.dds_read_samples.clone();
        status.mip_reads = tree.mips.clone();
        status.last_reader = tree.last_reader.clone();
        status.failures += self.filesystem_errors.load(Ordering::Relaxed);
        {
            let mut fs_err = lock(&self.filesystem_error)?;
            if fs_err
                .as_ref()
                .is_some_and(|(_, at)| at.elapsed() >= Duration::from_secs(20))
            {
                *fs_err = None;
            }
            if let Some((error, _)) = fs_err.as_ref() {
                status.last_error = Some(error.clone());
            }
        }
        let now = Instant::now();
        let mut wake_prep = false;
        let relocating = match (status.aircraft_lat, status.aircraft_lon, tree.active_center) {
            (Some(ac_lat), Some(ac_lon), Some((act_lat, act_lon))) => {
                let far = (ac_lat - act_lat).abs() > 2.5 || (ac_lon - act_lon).abs() > 3.5;
                let recent_demand = tree
                    .last_demand_at
                    .is_some_and(|t| now.saturating_duration_since(t) < Duration::from_secs(12));
                if far
                    && (status.telemetry_state != crate::model::TelemetryState::Fresh
                        || recent_demand)
                {
                    true
                } else if status.telemetry_state == crate::model::TelemetryState::Fresh {
                    wake_prep = tree.seed_aircraft_window(ac_lat, ac_lon, now);
                    false
                } else {
                    false
                }
            }
            (Some(ac_lat), Some(ac_lon), None)
                if status.telemetry_state == crate::model::TelemetryState::Fresh =>
            {
                wake_prep = tree.seed_aircraft_window(ac_lat, ac_lon, now);
                false
            }
            _ => false,
        };
        if relocating
            || (status.aircraft_lat.is_none() && tree.active_center.is_some())
            || (status.telemetry_state != crate::model::TelemetryState::Fresh
                && tree.active_center.is_some())
        {
            if let Some((act_lat, act_lon)) = tree.active_center {
                status.aircraft_lat = Some(act_lat);
                status.aircraft_lon = Some(act_lon);
            }
            if relocating {
                status.telemetry_state = crate::model::TelemetryState::Stale;
                status.ground_speed_mps = None;
                status.track_deg = None;
            }
        }
        let mut active_footprints = FxHashSet::default();
        for cell in &tree.active_dsf_cells {
            if let Some(fps) = tree.cell_footprints.get(cell) {
                active_footprints.extend(fps.iter().copied());
            }
        }
        let state_by_fp: FxHashMap<Footprint, (bool, bool)> = tile_states
            .iter()
            .map(|&(fp, is_cached, is_building)| (fp, (is_cached, is_building)))
            .collect();
        let active_tiles: Vec<(Footprint, bool, bool)> = if !active_footprints.is_empty() {
            active_footprints
                .iter()
                .map(|&fp| {
                    let (is_cached, is_building) =
                        state_by_fp.get(&fp).copied().unwrap_or((false, false));
                    (fp, is_cached, is_building)
                })
                .collect()
        } else if !tree.active_dsf_cells.is_empty() {
            tile_states
                .iter()
                .copied()
                .filter(|(fp, _, _)| {
                    let b = fp.geographic();
                    let cell = (
                        ((b.north + b.south) * 0.5).floor() as i32,
                        ((b.east + b.west) * 0.5).floor() as i32,
                    );
                    tree.active_dsf_cells.contains(&cell)
                })
                .collect()
        } else {
            tile_states.to_vec()
        };
        let sim_active = !tree.dds_loaded_footprints.is_empty()
            || !tree.consumers.is_empty()
            || status.telemetry_state == crate::model::TelemetryState::Fresh;
        let is_tile_dds_loaded = |fp: &Footprint, is_cached: bool, is_building: bool| -> bool {
            if is_building {
                return false;
            }
            if let (Some(ac_lat), Some(ac_lon)) = (status.aircraft_lat, status.aircraft_lon) {
                let b = fp.geographic();
                let lat = (b.north + b.south) * 0.5;
                let lon = (b.east + b.west) * 0.5;
                let dlat = lat - ac_lat;
                let dlon = (lon - ac_lon) * ac_lat.to_radians().cos();
                let dist_deg = (dlat * dlat + dlon * dlon).sqrt();
                if tree.dds_loaded_footprints.contains(fp) && dist_deg <= 0.85 {
                    return true;
                }
                if sim_active && is_cached && dist_deg <= 0.40 {
                    return true;
                }
                false
            } else {
                tree.dds_loaded_footprints.contains(fp)
            }
        };
        status.prepared_tiles = active_tiles.len();
        status.cached_tiles = active_tiles.iter().filter(|(_, c, _)| *c).count();
        status.building_tiles = active_tiles.iter().filter(|(_, _, b)| *b).count();
        status.loaded_tiles = active_tiles
            .iter()
            .filter(|(fp, c, b)| is_tile_dds_loaded(fp, *c, *b))
            .count();
        status.ter_loaded_tiles = active_tiles
            .iter()
            .filter(|(fp, c, b)| {
                tree.ter_loaded_footprints.contains(fp) || is_tile_dds_loaded(fp, *c, *b)
            })
            .count();
        status.tile_grid_cols = TILE_GRID_COLS;
        status.tile_grid_rows = TILE_GRID_ROWS;
        let mut grid = vec![0_u8; TILE_GRID_COLS * TILE_GRID_ROWS];
        let mut map_footprints = active_footprints;
        for fps in tree.cell_footprints.values() {
            map_footprints.extend(fps.iter().copied());
        }
        let map_tiles: Vec<(Footprint, bool, bool)> = if !map_footprints.is_empty() {
            map_footprints
                .into_iter()
                .map(|fp| {
                    let (is_cached, is_building) =
                        state_by_fp.get(&fp).copied().unwrap_or((false, false));
                    (fp, is_cached, is_building)
                })
                .collect()
        } else {
            active_tiles.clone()
        };
        if !map_tiles.is_empty() {
            let mut min_lat = f64::INFINITY;
            let mut max_lat = f64::NEG_INFINITY;
            let mut min_lon = f64::INFINITY;
            let mut max_lon = f64::NEG_INFINITY;
            let mut centers = Vec::with_capacity(map_tiles.len());
            for (fp, is_cached, is_building) in &map_tiles {
                let b = fp.geographic();
                let lat = (b.north + b.south) * 0.5;
                let lon = (b.east + b.west) * 0.5;
                min_lat = min_lat.min(lat);
                max_lat = max_lat.max(lat);
                min_lon = min_lon.min(lon);
                max_lon = max_lon.max(lon);
                let dds_loaded = is_tile_dds_loaded(fp, *is_cached, *is_building);
                let ter_only = tree.ter_loaded_footprints.contains(fp) && !dds_loaded;
                centers.push((lat, lon, *is_cached, *is_building, ter_only, dds_loaded));
            }
            let (min_lat, max_lat, min_lon, max_lon, moving_window) =
                if let (Some(ac_lat), Some(ac_lon)) = (status.aircraft_lat, status.aircraft_lon) {
                    let dlat = 1.0;
                    let cos_lat = ac_lat.to_radians().cos().abs().max(0.25);
                    let dlon = (dlat / cos_lat).clamp(1.0, 2.5);
                    (
                        ac_lat - dlat,
                        ac_lat + dlat,
                        ac_lon - dlon,
                        ac_lon + dlon,
                        true,
                    )
                } else if !tree.active_dsf_cells.is_empty() {
                    let dsf_min_lat =
                        tree.active_dsf_cells.iter().map(|c| c.0).min().unwrap_or(0) as f64;
                    let dsf_max_lat =
                        (tree.active_dsf_cells.iter().map(|c| c.0).max().unwrap_or(0) + 1) as f64;
                    let dsf_min_lon =
                        tree.active_dsf_cells.iter().map(|c| c.1).min().unwrap_or(0) as f64;
                    let dsf_max_lon =
                        (tree.active_dsf_cells.iter().map(|c| c.1).max().unwrap_or(0) + 1) as f64;
                    (
                        dsf_min_lat,
                        dsf_max_lat.max(dsf_min_lat + 1.0),
                        dsf_min_lon,
                        dsf_max_lon.max(dsf_min_lon + 1.0),
                        false,
                    )
                } else {
                    let min_lat = min_lat.floor();
                    let max_lat = max_lat.ceil().max(min_lat + 1.0);
                    let min_lon = min_lon.floor();
                    let max_lon = max_lon.ceil().max(min_lon + 1.0);
                    (min_lat, max_lat, min_lon, max_lon, false)
                };
            let mut counts = vec![[0_u32; 5]; TILE_GRID_COLS * TILE_GRID_ROWS];
            for (lat, lon, is_cached, is_building, ter_only, dds_loaded) in centers {
                if moving_window
                    && (lat < min_lat || lat > max_lat || lon < min_lon || lon > max_lon)
                {
                    continue;
                }
                let col = (((lon - min_lon) / (max_lon - min_lon)) * TILE_GRID_COLS as f64)
                    .floor()
                    .clamp(0.0, (TILE_GRID_COLS - 1) as f64) as usize;
                let row = (((max_lat - lat) / (max_lat - min_lat)) * TILE_GRID_ROWS as f64)
                    .floor()
                    .clamp(0.0, (TILE_GRID_ROWS - 1) as f64) as usize;
                let cell = &mut counts[row * TILE_GRID_COLS + col];
                cell[0] += 1;
                if is_cached {
                    cell[1] += 1;
                }
                if is_building {
                    cell[2] += 1;
                }
                if ter_only {
                    cell[3] += 1;
                }
                if dds_loaded {
                    cell[4] += 1;
                }
            }
            for (dst, &[total, cached, building, ter_only, dds_loaded]) in
                grid.iter_mut().zip(counts.iter())
            {
                *dst = if total == 0 {
                    TILE_EMPTY
                } else if building > 0 {
                    TILE_BUILDING
                } else if dds_loaded > 0 && dds_loaded >= ter_only {
                    TILE_LOADED
                } else if ter_only > 0 {
                    TILE_TER_ONLY
                } else if cached > 0 {
                    TILE_CACHED
                } else {
                    TILE_COLD
                };
            }
        }
        drop(tree);
        if wake_prep {
            self.dsf_prep_cv.notify_one();
        }
        status.tile_grid = grid;
        Ok(())
    }

    pub fn quiesce(&self) -> Result<()> {
        let mut tree = lock(&self.tree)?;
        tree.reap_consumers()?;
        ensure!(
            tree.handles.is_empty() && tree.consumers.is_empty(),
            "a reader opened scenery while stop was requested"
        );
        self.accepting.store(false, Ordering::Release);
        drop(tree);
        self.dsf_prep_cv.notify_all();
        Ok(())
    }
}

pub struct Reader {
    scene: Arc<Scene>,
    cache: Arc<Cache>,
}

impl Reader {
    pub fn new(scene: Arc<Scene>, cache: Arc<Cache>) -> Self {
        Self { scene, cache }
    }

    fn fail(scene: &Scene, error: anyhow::Error) -> i32 {
        let message: String = format!("{error:#}").chars().take(2048).collect();
        let count = scene.filesystem_errors.fetch_add(1, Ordering::Relaxed);
        if count < 64 {
            eprintln!("reader filesystem: {message}");
        }
        match lock(&scene.filesystem_error) {
            Ok(mut recorded) => *recorded = Some((message, Instant::now())),
            Err(error) if count < 64 => eprintln!("recording reader failure: {error:#}"),
            Err(_) => {}
        }
        libc::EIO
    }
}

impl Filesystem for Reader {
    fn init(&mut self, _: &Request<'_>, _: &mut KernelConfig) -> std::result::Result<(), i32> {
        self.scene.mounted.store(true, Ordering::Release);
        Ok(())
    }

    fn destroy(&mut self) {
        self.scene.accepting.store(false, Ordering::Release);
        self.scene.mounted.store(false, Ordering::Release);
        self.scene.dsf_prep_cv.notify_all();
        let handles = lock(&self.scene.tree).map(|mut tree| std::mem::take(&mut tree.handles));
        match handles {
            Ok(handles) => {
                for handle in handles.into_values() {
                    if let Data::Asset(spec) = &handle.node.data
                        && let Err(error) = self.cache.unpin(&spec.key)
                    {
                        Self::fail(&self.scene, error);
                    }
                }
            }
            Err(error) => {
                Self::fail(&self.scene, error);
            }
        }
    }

    fn lookup(&mut self, _: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        match self.scene.lookup(parent, name) {
            Ok(Some(node)) => {
                let ttl = if node.path.starts_with("Earth nav data") {
                    &DSF_TTL
                } else {
                    &TTL
                };
                reply.entry(ttl, &node.attr(self.scene.uid, self.scene.gid), 0);
            }
            Ok(None) => reply.error(libc::ENOENT),
            Err(e) => reply.error(Self::fail(&self.scene, e)),
        }
    }

    fn getattr(&mut self, _: &Request<'_>, ino: u64, _: Option<u64>, reply: ReplyAttr) {
        match lock(&self.scene.tree)
            .and_then(|tree| tree.nodes.get(&ino).cloned().context("unknown inode"))
        {
            Ok(node) => reply.attr(&TTL, &node.attr(self.scene.uid, self.scene.gid)),
            Err(e) => reply.error(Self::fail(&self.scene, e)),
        }
    }

    fn readdir(
        &mut self,
        _: &Request<'_>,
        ino: u64,
        _: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }
        match self.scene.directory(ino) {
            Ok(entries) => {
                for (index, (ino, kind, name)) in
                    entries.into_iter().enumerate().skip(offset as usize)
                {
                    if reply.add(ino, index as i64 + 1, kind, name) {
                        break;
                    }
                }
                reply.ok();
            }
            Err(e) => reply.error(Self::fail(&self.scene, e)),
        }
    }

    fn open(&mut self, req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        if flags & libc::O_ACCMODE != libc::O_RDONLY || flags & libc::O_TRUNC != 0 {
            reply.error(libc::EROFS);
            return;
        }
        let key = match lock(&self.scene.tree).and_then(|tree| {
            let node = tree.nodes.get(&ino).context("unknown open inode")?;
            let key = if let Data::Asset(spec) = &node.data {
                Some(spec.key.clone())
            } else {
                None
            };
            Ok(key)
        }) {
            Ok(value) => value,
            Err(e) => {
                reply.error(Self::fail(&self.scene, e));
                return;
            }
        };
        if let Some(key) = &key
            && let Err(e) = self.cache.pin(key)
        {
            reply.error(Self::fail(&self.scene, e));
            return;
        }
        match self.scene.open(ino, req.pid()) {
            Ok(handle) => reply.opened(handle, 0),
            Err(e) => {
                if let Some(key) = &key
                    && let Err(error) = self.cache.unpin(key)
                {
                    reply.error(Self::fail(&self.scene, error));
                    return;
                }
                reply.error(Self::fail(&self.scene, e));
            }
        }
    }

    fn read(
        &mut self,
        _: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _: i32,
        _: Option<u64>,
        reply: ReplyData,
    ) {
        if offset < 0 || size > 1024 * 1024 {
            reply.error(libc::EINVAL);
            return;
        }
        let handle = match lock(&self.scene.tree).and_then(|tree| {
            tree.handles
                .get(&fh)
                .cloned()
                .context("unknown file handle")
        }) {
            Ok(handle) if handle.node.ino == ino => handle,
            Ok(_) => {
                reply.error(libc::EBADF);
                return;
            }
            Err(e) => {
                reply.error(Self::fail(&self.scene, e));
                return;
            }
        };
        let offset = offset as u64;
        if offset >= handle.node.length() {
            reply.data(&[]);
            return;
        }
        if let Some(bytes) = self.scene.header_range(&handle.node, offset, size) {
            if let Err(error) = self.scene.trace(&handle.node, offset, bytes.len()) {
                reply.error(Self::fail(&self.scene, error));
            } else {
                reply.data(&bytes);
            }
            return;
        }
        let prepared = lock(&handle.cached).map(|file| file.clone());
        match prepared {
            Ok(Some(file)) => {
                let result = file.read(offset, size).and_then(|bytes| {
                    self.scene.trace(&handle.node, offset, bytes.len())?;
                    Ok(bytes)
                });
                match result {
                    Ok(bytes) => reply.data(&bytes),
                    Err(error) => reply.error(Self::fail(&self.scene, error)),
                }
                return;
            }
            Err(error) => {
                reply.error(Self::fail(&self.scene, error));
                return;
            }
            Ok(None) => {}
        }
        match &handle.node.data {
            Data::Inline(bytes) => {
                let end = (offset.saturating_add(size as u64)).min(bytes.len() as u64) as usize;
                let data = &bytes[offset as usize..end];
                if let Err(e) = self.scene.trace(&handle.node, offset, data.len()) {
                    reply.error(Self::fail(&self.scene, e));
                } else {
                    reply.data(data);
                }
            }
            Data::Asset(spec) => {
                let spec = spec.clone();
                let scene = Arc::clone(&self.scene);
                self.cache.request(
                    spec,
                    Box::new(move |result| {
                        let result = result.and_then(|file| {
                            *lock(&handle.cached)? = Some(Arc::clone(&file));
                            let bytes = file.read(offset, size)?;
                            scene.trace(&handle.node, offset, bytes.len())?;
                            Ok(bytes)
                        });
                        match result {
                            Ok(bytes) => reply.data(&bytes),
                            Err(e) => reply.error(Self::fail(&scene, e)),
                        }
                    }),
                );
            }
            Data::Directory => reply.error(libc::EISDIR),
        }
    }

    fn release(
        &mut self,
        _: &Request<'_>,
        _: u64,
        fh: u64,
        _: i32,
        _: Option<u64>,
        _: bool,
        reply: ReplyEmpty,
    ) {
        match lock(&self.scene.tree)
            .and_then(|mut tree| tree.handles.remove(&fh).context("unknown release handle"))
        {
            Ok(handle) => {
                let result = if let Data::Asset(spec) = &handle.node.data {
                    self.cache.unpin(&spec.key)
                } else {
                    Ok(())
                };
                match result {
                    Ok(()) => reply.ok(),
                    Err(e) => reply.error(Self::fail(&self.scene, e)),
                }
            }
            Err(e) => reply.error(Self::fail(&self.scene, e)),
        }
    }

    fn statfs(&mut self, _: &Request<'_>, _: u64, reply: ReplyStatfs) {
        match self.cache.snapshot() {
            Ok(s) => {
                let free = s.budget_bytes.saturating_sub(
                    s.cache_bytes + s.other_managed_bytes + s.reserved_bytes + MAX_METADATA,
                );
                reply.statfs(
                    s.budget_bytes / 4096,
                    free / 4096,
                    free / 4096,
                    MAX_NODES as u64,
                    MAX_NODES.saturating_sub(s.cached_assets) as u64,
                    4096,
                    255,
                    4096,
                );
            }
            Err(e) => reply.error(Self::fail(&self.scene, e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_is_lazy_canonical_and_cannot_change_behind_an_inode() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path();
        fs::create_dir(root.join("terrain"))?;
        fs::create_dir(root.join("textures"))?;
        fs::write(root.join("osm"), [])?;
        let text = "A\n800\nTERRAIN\nLOAD_CENTER 35.85344 -78.70605 7930 4096\nBASE_TEX_NOWRAP ../textures/25760_18432_BI16.dds\n";
        fs::write(root.join("terrain/a.ter"), text)?;
        fs::write(root.join("terrain/wet.ter"), format!("{text}WET\n"))?;
        let scene = Scene::new(StreamConfig {
            source_package: root.display().to_string(),
            mount_point: root.join("reader").display().to_string(),
            osm_db: root.join("osm").display().to_string(),
            viirs: None,
            source_revision: "test".into(),
            tile_source: crate::model::TileSource::default(),
            size: 1024,
            exposure: 0.35,
            budget_mib: 512,
        })?;
        let a = scene.lookup(3, OsStr::new("a.ter"))?.context("terrain")?;
        let wet = scene
            .lookup(3, OsStr::new("wet.ter"))?
            .context("wet terrain")?;
        let (Data::Inline(a), Data::Inline(wet)) = (&a.data, &wet.data) else {
            anyhow::bail!("not terrain");
        };
        let bindings = |data: &[u8]| -> Result<Vec<String>> {
            Ok(std::str::from_utf8(data)?
                .lines()
                .filter(|line| line.starts_with("BASE_TEX") || line.starts_with("LIT_TEX"))
                .map(str::to_owned)
                .collect())
        };
        assert_eq!(bindings(a)?, bindings(wet)?);
        let tree = lock(&scene.tree)?;
        assert_eq!(
            tree.nodes
                .values()
                .filter(|n| matches!(n.data, Data::Asset(_)))
                .count(),
            2
        );
        assert!(
            tree.nodes
                .values()
                .filter(|n| matches!(n.data, Data::Asset(_)))
                .all(|n| n.length() == 699192)
        );
        let texture = tree
            .nodes
            .values()
            .find(|n| n.is_generated_dds())
            .cloned()
            .context("DDS node")?;
        drop(tree);
        assert_eq!(
            scene.header_range(&texture, 0, 128),
            Some(dds::header(1024, 8))
        );
        assert_eq!(
            scene.header_range(&texture, 8, 4),
            Some(dds::header(1024, 8)[8..12].to_vec())
        );
        assert!(scene.header_range(&texture, 127, 2).is_none());
        assert!(scene.header_range(&texture, 128, 8).is_none());
        scene.trace(&texture, 0, 128)?;
        scene.trace(&texture, 699184, 8)?;
        scene.trace(&texture, 524412, 8)?;
        let mut status = StreamStatus::default();
        let tile_states: Vec<_> = scene
            .prepared_specs()?
            .into_iter()
            .map(|(fp, _, _)| (fp, true, false))
            .collect();
        scene.status_with_tiles(&mut status, &tile_states)?;
        assert_eq!((status.dds_header_reads, status.dds_payload_reads), (1, 2));
        assert_eq!((status.prepared_tiles, status.loaded_tiles), (1, 1));
        assert_eq!(status.mip_reads, [1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(
            status.dds_read_samples,
            [[0, 128], [699184, 8], [524412, 8]]
        );
        assert!(
            scene
                .lookup(4, OsStr::new("25760_18432_BI16.dds"))?
                .is_none()
        );
        let lib = scene
            .lookup(1, OsStr::new("library.txt"))?
            .context("library.txt")?;
        let blank = scene
            .lookup(1, OsStr::new("blank.obj"))?
            .context("blank.obj")?;
        let res_spill = scene
            .lookup(1, OsStr::new("ge_res_spill.obj"))?
            .context("ge_res_spill.obj")?;
        let pri_spill = scene
            .lookup(1, OsStr::new("ge_pri_spill.obj"))?
            .context("ge_pri_spill.obj")?;
        let hwy_spill = scene
            .lookup(1, OsStr::new("ge_hwy_spill.obj"))?
            .context("ge_hwy_spill.obj")?;
        assert!(matches!(&lib.data, Data::Inline(b) if b.starts_with(b"A\n800\nLIBRARY\n")));
        assert!(matches!(&blank.data, Data::Inline(b) if b.as_ref() == b"A\n800\nOBJ\n"));
        for spill in [&res_spill, &pri_spill, &hwy_spill] {
            let Data::Inline(bytes) = &spill.data else {
                anyhow::bail!("expected inline spill obj");
            };
            let text = std::str::from_utf8(bytes)?;
            assert!(text.contains("LIGHT_PARAM spot_params_sp_pm"));
            assert!(!text.contains("_bb"));
        }
        let root_names: Vec<_> = scene
            .directory(1)?
            .into_iter()
            .map(|(_, _, name)| name)
            .collect();
        assert!(root_names.iter().any(|n| n == "library.txt"));
        assert!(root_names.iter().any(|n| n == "blank.obj"));
        assert!(root_names.iter().any(|n| n == "ge_res_spill.obj"));
        assert!(root_names.iter().any(|n| n == "ge_pri_spill.obj"));
        assert!(root_names.iter().any(|n| n == "ge_hwy_spill.obj"));
        assert!(scene.lookup(3, OsStr::new("../escape.ter")).is_err());
        fs::write(root.join("terrain/a.ter"), text.replace("7930", "7900"))?;
        assert!(scene.terrain(PathBuf::from("terrain/a.ter")).is_err());
        Ok(())
    }

    #[test]
    fn open_handles_prevent_quiescing() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path();
        fs::write(root.join("osm"), [])?;
        let scene = Scene::new(StreamConfig {
            source_package: root.display().to_string(),
            mount_point: root.join("reader").display().to_string(),
            osm_db: root.join("osm").display().to_string(),
            viirs: None,
            source_revision: "test".into(),
            tile_source: crate::model::TileSource::default(),
            size: 256,
            exposure: 0.35,
            budget_mib: 512,
        })?;
        let node = lock(&scene.tree)?.insert(
            PathBuf::from("test"),
            Data::Inline(Arc::from(b"data".as_slice())),
        )?;
        let handle = scene.open(node.ino, 0)?;
        assert!(scene.quiesce().is_err());
        lock(&scene.tree)?.handles.remove(&handle);
        scene.quiesce()?;
        assert!(scene.open(node.ino, 0).is_err());
        Ok(())
    }

    #[test]
    fn relocation_resets_active_region_progress_and_recenters_grid() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path();
        fs::create_dir(root.join("terrain"))?;
        fs::create_dir(root.join("textures"))?;
        fs::write(root.join("osm"), [])?;
        let raleigh = "A\n800\nTERRAIN\nLOAD_CENTER 35.85344 -78.70605 7930 4096\nBASE_TEX_NOWRAP ../textures/25760_18432_BI16.dds\n";
        let vegas = "A\n800\nTERRAIN\nLOAD_CENTER 36.13787 -115.18066 7894 4096\nBASE_TEX_NOWRAP ../textures/25696_11792_BI16.dds\n";
        fs::write(root.join("terrain/raleigh.ter"), raleigh)?;
        fs::write(root.join("terrain/vegas.ter"), vegas)?;
        let scene = Scene::new(StreamConfig {
            source_package: root.display().to_string(),
            mount_point: root.join("reader").display().to_string(),
            osm_db: root.join("osm").display().to_string(),
            viirs: None,
            source_revision: "test".into(),
            tile_source: crate::model::TileSource::default(),
            size: 1024,
            exposure: 0.35,
            budget_mib: 512,
        })?;
        let rdu_ter = scene.lookup(3, OsStr::new("raleigh.ter"))?.context("rdu")?;
        let las_ter = scene.lookup(3, OsStr::new("vegas.ter"))?.context("las")?;
        scene.trace(&rdu_ter, 0, 64)?;
        let rdu_dds = {
            let tree = lock(&scene.tree)?;
            tree.nodes
                .values()
                .find(|n| {
                    matches!(
                        &n.data,
                        Data::Asset(Spec {
                            kind: Kind::Day(fp),
                            ..
                        }) if fp.col == 18432
                    )
                })
                .cloned()
                .context("rdu dds")?
        };
        scene.trace(&rdu_dds, 0, 128)?;
        let tile_states: Vec<_> = scene
            .prepared_specs()?
            .into_iter()
            .map(|(fp, _, _)| (fp, true, false))
            .collect();
        let mut status = StreamStatus {
            telemetry_state: crate::model::TelemetryState::Fresh,
            aircraft_lat: Some(35.87),
            aircraft_lon: Some(-78.78),
            ..StreamStatus::default()
        };
        scene.status_with_tiles(&mut status, &tile_states)?;
        assert_eq!(
            (
                status.prepared_tiles,
                status.ter_loaded_tiles,
                status.loaded_tiles
            ),
            (1, 1, 1)
        );
        // Simulate jumping to Las Vegas while telemetry still holds Raleigh's last RPOS sample
        scene.trace(&las_ter, 0, 64)?;
        let mut relocated = StreamStatus {
            telemetry_state: crate::model::TelemetryState::Fresh,
            aircraft_lat: Some(35.87),
            aircraft_lon: Some(-78.78),
            ground_speed_mps: Some(60.0),
            track_deg: Some(270.0),
            ..StreamStatus::default()
        };
        scene.status_with_tiles(&mut relocated, &tile_states)?;
        assert_eq!(relocated.prepared_tiles, 1);
        assert_eq!(relocated.ter_loaded_tiles, 1);
        assert_eq!(relocated.loaded_tiles, 0);
        assert_eq!(
            relocated.telemetry_state,
            crate::model::TelemetryState::Stale
        );
        assert!(relocated.aircraft_lon.unwrap_or(0.0) < -110.0);
        assert!(relocated.tile_grid.contains(&5));
        // Wave 2/3 DSF opens >15s later in the same region must not wipe loaded progress
        {
            let mut tree = lock(&scene.tree)?;
            let las_fp = Footprint {
                row: 25696,
                col: 11792,
                zoom: 16,
            };
            tree.dds_loaded_footprints.insert(las_fp);
            let now = Instant::now();
            tree.note_dsf_touch((35, -117), now);
            tree.note_dsf_touch((37, -114), now);
            assert_eq!(tree.active_dsf_cells.len(), 12);
            tree.note_dsf_touch((36, -115), now + Duration::from_secs(25));
            assert!(tree.dds_loaded_footprints.contains(&las_fp));
            assert_eq!(tree.active_dsf_cells.len(), 12);
            // Border footprint demand must not expand the 3x4 DSF box
            let border_fp = Footprint {
                row: 25152,
                col: 11792,
                zoom: 16,
            };
            tree.note_footprint_demand(border_fp, now + Duration::from_secs(26));
            assert_eq!(tree.active_dsf_cells.len(), 12);
        }
        Ok(())
    }
}
