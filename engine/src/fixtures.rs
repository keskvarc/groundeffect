use crate::{dds, lighting, model::PrepareConfig};
use anyhow::Result;
use md5::{Digest, Md5};
use std::{fs, path::PathBuf};
use tempfile::TempDir;

pub struct Fixture {
    pub _temporary: TempDir,
    pub source: PathBuf,
    pub data: PathBuf,
    pub config: PrepareConfig,
}

fn atom(tag: &[u8; 4], bytes: &[u8]) -> Vec<u8> {
    let mut result = tag.to_vec();
    result.extend_from_slice(&((bytes.len() + 8) as u32).to_le_bytes());
    result.extend_from_slice(bytes);
    result
}

impl Fixture {
    pub fn new() -> Result<Self> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        let data = temporary.path().join("data");
        fs::create_dir_all(source.join("terrain"))?;
        fs::create_dir_all(source.join("textures"))?;
        fs::create_dir_all(source.join("Earth nav data/+30-080"))?;
        fs::create_dir_all(&data)?;
        let mut dsf = b"XPLNEDSF\x01\0\0\0".to_vec();
        let names = b"terrain_Water\0terrain/dry.ter\0terrain/wet.ter\0";
        dsf.extend(atom(b"NFED", &atom(b"TRET", names)));
        dsf.extend_from_slice(&Md5::digest(&dsf));
        fs::write(source.join("Earth nav data/+30-080/+35-079.dsf"), dsf)?;
        for name in ["dry", "wet"] {
            fs::write(
                source.join(format!("terrain/{name}.ter")),
                "A\n800\nTERRAIN\nLOAD_CENTER 35.85344 -78.70605 7930 4096\nBASE_TEX_NOWRAP ../textures/25760_18432_BI16.dds\nNO_ALPHA\n",
            )?;
        }
        fs::write(
            source.join("textures/25760_18432_BI16.dds"),
            dds::encode_rgb(256, vec![[0.2, 0.4, 0.1]; 256 * 256], false)?,
        )?;
        let osm = temporary.path().join("roads.rds1");
        fs::write(
            &osm,
            lighting::pack_roads_rds1(&[(1, 1.0, vec![[35.85, -78.74], [35.85, -78.68]])])?,
        )?;
        let config = PrepareConfig {
            source_package: source.display().to_string(),
            dsf: "Earth nav data/+30-080/+35-079.dsf".into(),
            imagery_raster: None,
            osm_db: osm.display().to_string(),
            viirs: None,
            source_label: "synthetic mapping fixture; not a simulator scenery tile".into(),
            size: 256,
            exposure: 0.35,
            budget_mib: 32,
        };
        Ok(Self {
            _temporary: temporary,
            source,
            data,
            config,
        })
    }
}
