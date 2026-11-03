use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    hash::{BuildHasherDefault, Hasher},
};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_MESSAGE: u64 = 256 * 1024;

pub const TILE_EMPTY: u8 = 0;
pub const TILE_COLD: u8 = 1;
pub const TILE_BUILDING: u8 = 2;
pub const TILE_CACHED: u8 = 3;
pub const TILE_LOADED: u8 = 4;
pub const TILE_TER_ONLY: u8 = 5;

/// Provider-independent cache identity tags for synthesized night-lighting textures.
/// Preserves the v1 wire prefix so existing cached `_GE_LIT.dds` assets remain valid.
pub const NIGHT_IDENTITY_TAG: &str = "groundeffect/usgs-imageryonly/tile-group2-v7";
pub const NIGHT_COASTAL_IDENTITY_TAG: &str = "groundeffect/usgs-imageryonly/coastal-tile-group2-v7";

#[derive(Default)]
pub struct FxHasher {
    hash: usize,
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, mut bytes: &[u8]) {
        const K: usize = 0x517cc1b727220a95;
        let mut hash = self.hash;
        while bytes.len() >= 8 {
            let word = usize::from_ne_bytes(bytes[..8].try_into().unwrap());
            hash = (hash.rotate_left(5) ^ word).wrapping_mul(K);
            bytes = &bytes[8..];
        }
        for &byte in bytes {
            hash = (hash.rotate_left(5) ^ usize::from(byte)).wrapping_mul(K);
        }
        self.hash = hash;
    }

    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.hash = (self.hash.rotate_left(5) ^ usize::from(i)).wrapping_mul(0x517cc1b727220a95);
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.hash = (self.hash.rotate_left(5) ^ (i as usize)).wrapping_mul(0x517cc1b727220a95);
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.hash = (self.hash.rotate_left(5) ^ (i as usize)).wrapping_mul(0x517cc1b727220a95);
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.hash = (self.hash.rotate_left(5) ^ i).wrapping_mul(0x517cc1b727220a95);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.hash as u64
    }
}

pub type FxBuildHasher = BuildHasherDefault<FxHasher>;
pub type FxHashMap<K, V> = HashMap<K, V, FxBuildHasher>;
pub type FxHashSet<T> = HashSet<T, FxBuildHasher>;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareConfig {
    pub source_package: String,
    pub dsf: String,
    pub imagery_raster: Option<String>,
    pub osm_db: String,
    pub viirs: Option<String>,
    pub source_label: String,
    pub size: u32,
    pub exposure: f32,
    pub budget_mib: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TileSource {
    #[default]
    #[serde(alias = "bi", alias = "virtualearth", alias = "bing_maps")]
    Bing,
    #[serde(
        alias = "arcgis",
        alias = "esri",
        alias = "arc",
        alias = "world_imagery"
    )]
    ArcgisWorldImagery,
    #[serde(alias = "clarity", alias = "arcgis-clarity", alias = "esri_clarity")]
    ArcgisClarity,
    #[serde(alias = "usgs", alias = "naip", alias = "usgs-imagery-only")]
    UsgsImageryOnly,
}

impl TileSource {
    pub fn quadkey(zoom: u32, row: u32, col: u32) -> String {
        let mut key = String::with_capacity(zoom as usize);
        for i in (1..=zoom).rev() {
            let mask = 1_u32 << (i - 1);
            let mut digit = 0_u8;
            if (col & mask) != 0 {
                digit += 1;
            }
            if (row & mask) != 0 {
                digit += 2;
            }
            key.push(char::from(b'0' + digit));
        }
        key
    }

    pub fn tile_url(self, zoom: u32, row: u32, col: u32) -> String {
        match self {
            Self::Bing => {
                let qk = Self::quadkey(zoom, row, col);
                format!("https://t.ssl.ak.tiles.virtualearth.net/tiles/a{qk}.jpeg?g=15312")
            }
            Self::ArcgisWorldImagery => {
                format!(
                    "https://services.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/tile/{zoom}/{row}/{col}"
                )
            }
            Self::ArcgisClarity => {
                format!(
                    "https://clarity.maptiles.arcgis.com/arcgis/rest/services/World_Imagery/MapServer/tile/{zoom}/{row}/{col}"
                )
            }
            Self::UsgsImageryOnly => {
                format!(
                    "https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryOnly/MapServer/tile/{zoom}/{row}/{col}"
                )
            }
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Bing => "Bing Maps / VirtualEarth Aerial (BI)",
            Self::ArcgisWorldImagery => "Esri / ArcGIS World Imagery (Maxar + USDA NAIP)",
            Self::ArcgisClarity => "Esri / ArcGIS World Imagery Clarity",
            Self::UsgsImageryOnly => "USGS Imagery Only / mixed acquisition years / preview",
        }
    }

    pub fn identity_tag(self) -> &'static str {
        match self {
            Self::Bing => "groundeffect/bing-virtualearth/tile-group2-v1",
            Self::ArcgisWorldImagery => "groundeffect/arcgis-world-imagery/tile-group2-v1",
            Self::ArcgisClarity => "groundeffect/arcgis-clarity/tile-group2-v1",
            Self::UsgsImageryOnly => "groundeffect/usgs-imageryonly/tile-group2-v1",
        }
    }

    pub fn coastal_identity_tag(self) -> &'static str {
        match self {
            Self::Bing => "groundeffect/bing-virtualearth/coastal-tile-group2-v1",
            Self::ArcgisWorldImagery => "groundeffect/arcgis-world-imagery/coastal-tile-group2-v1",
            Self::ArcgisClarity => "groundeffect/arcgis-clarity/coastal-tile-group2-v1",
            Self::UsgsImageryOnly => "groundeffect/usgs-imageryonly/coastal-tile-group2-v1",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StreamConfig {
    #[serde(default)]
    pub source_package: String,
    pub mount_point: String,
    pub osm_db: String,
    pub viirs: Option<String>,
    #[serde(default)]
    pub tile_source: TileSource,
    pub source_revision: String,
    pub size: u32,
    pub exposure: f32,
    pub budget_mib: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TelemetryState {
    #[default]
    Unavailable,
    Fresh,
    Stale,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FlightPhase {
    #[default]
    Unknown,
    Parked,
    Taxi,
    Climb,
    Cruise,
    Descent,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct StreamStatus {
    pub mounted: bool,
    pub mount_point: String,
    pub source_label: String,
    pub cache_bytes: u64,
    pub other_managed_bytes: u64,
    pub budget_bytes: u64,
    pub reserved_bytes: u64,
    pub borrowed_lighting_bytes: u64,
    pub cached_assets: usize,
    pub pinned_assets: usize,
    pub open_handles: usize,
    pub active_consumers: usize,
    pub pending: usize,
    pub working: usize,
    pub cache_hits: u64,
    pub cold_builds: u64,
    pub network_requests: u64,
    pub network_bytes: u64,
    pub coalesced: u64,
    pub evictions: u64,
    pub failures: u64,
    pub reads: u64,
    pub served_bytes: u64,
    pub dds_header_reads: u64,
    pub dds_payload_reads: u64,
    pub dds_read_samples: Vec<[u64; 2]>,
    pub mip_reads: Vec<u64>,
    #[serde(default)]
    pub telemetry_state: TelemetryState,
    #[serde(default)]
    pub flight_phase: FlightPhase,
    #[serde(default)]
    pub aircraft_lat: Option<f64>,
    #[serde(default)]
    pub aircraft_lon: Option<f64>,
    #[serde(default)]
    pub aircraft_agl_m: Option<f32>,
    #[serde(default)]
    pub ground_speed_mps: Option<f32>,
    #[serde(default)]
    pub track_deg: Option<f32>,
    #[serde(default)]
    pub lookahead_queued: usize,
    #[serde(default)]
    pub lookahead_completed: u64,
    #[serde(default)]
    pub lookahead_cancelled: u64,
    #[serde(default)]
    pub prepared_tiles: usize,
    #[serde(default)]
    pub cached_tiles: usize,
    #[serde(default)]
    pub building_tiles: usize,
    #[serde(default)]
    pub ter_loaded_tiles: usize,
    #[serde(default)]
    pub loaded_tiles: usize,
    #[serde(default)]
    pub tile_grid_cols: usize,
    #[serde(default)]
    pub tile_grid_rows: usize,
    #[serde(default)]
    pub tile_grid: Vec<u8>,
    pub last_reader: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Running,
    Succeeded,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Job {
    pub id: String,
    pub kind: String,
    pub generation: String,
    pub state: JobState,
    pub stage: String,
    pub done: usize,
    pub total: usize,
    pub error: Option<String>,
    pub started: u64,
    pub finished: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Generation {
    pub id: String,
    pub package: String,
    pub dsf: String,
    pub assets: usize,
    pub bytes: u64,
    pub size: u32,
    pub source_label: String,
    pub manifest_sha256: String,
    pub created: u64,
    pub verified: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct State {
    pub version: u32,
    pub config: Option<PrepareConfig>,
    pub job: Option<Job>,
    pub generations: Vec<Generation>,
    pub installed: Option<String>,
    #[serde(default)]
    pub stream_config: Option<StreamConfig>,
    #[serde(default)]
    pub stream_enabled: bool,
    #[serde(default)]
    pub stream_installed: Option<String>,
    #[serde(default, skip_deserializing)]
    pub stream: Option<StreamStatus>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: PROTOCOL_VERSION,
            config: None,
            job: None,
            generations: Vec::new(),
            installed: None,
            stream_config: None,
            stream_enabled: false,
            stream_installed: None,
            stream: None,
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Asset {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Deserialize, Serialize)]
pub struct Manifest {
    pub version: u32,
    pub id: String,
    pub config: PrepareConfig,
    pub source_dsf_sha256: String,
    pub assets: Vec<Asset>,
    pub notes: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Serialize)]
pub struct Response {
    pub version: u32,
    pub id: String,
    pub result: Option<State>,
    pub error: Option<String>,
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
