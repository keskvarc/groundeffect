use crate::{
    dds,
    geo::{Bounds, Footprint},
    lighting::Lighting,
    model::{Asset, Generation, Manifest, PROTOCOL_VERSION, PrepareConfig, now},
    raster::{Raster, bounded_read},
    scenery::{Dsf, MAX_DSF_BYTES, Terrain, parse_dsf, parse_terrain, relative_path, source_path},
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

struct Plan {
    root: PathBuf,
    dsf: Dsf,
    terrains: Vec<Terrain>,
}

#[derive(Serialize)]
pub struct PlanReport {
    pub terrains: usize,
    pub footprints: usize,
    pub estimated_output_bytes: u64,
    pub geographic: [f64; 4],
    pub mercator: [f64; 4],
}

fn plan(config: &PrepareConfig) -> Result<Plan> {
    ensure!(
        [256, 512, 1024].contains(&config.size),
        "alpha size must be 256, 512 or 1024"
    );
    ensure!(
        config.exposure.is_finite() && (0.0..=1.0).contains(&config.exposure),
        "exposure must be between 0 and 1"
    );
    ensure!(
        (32..=16384).contains(&config.budget_mib),
        "budget must be 32..16384 MiB"
    );
    ensure!(
        !config.source_label.trim().is_empty() && config.source_label.len() <= 200,
        "source label is required and bounded"
    );
    let root = Path::new(&config.source_package).canonicalize()?;
    let dsf_path = relative_path(&config.dsf)?;
    ensure!(
        dsf_path.extension().is_some_and(|s| s == "dsf"),
        "expected a DSF path"
    );
    let source = root.join(&dsf_path).canonicalize()?;
    ensure!(source.starts_with(&root), "DSF symlink escapes package");
    let dsf = parse_dsf(bounded_read(&source, MAX_DSF_BYTES)?)?;
    let mut terrains = Vec::new();
    let mut seen = BTreeSet::new();
    for path in dsf.package_terrains() {
        relative_path(path)?;
        if !seen.insert(path.to_owned()) {
            continue;
        }
        let file = root
            .join(path)
            .canonicalize()
            .with_context(|| format!("missing terrain {path}"))?;
        ensure!(file.starts_with(&root), "terrain symlink escapes package");
        let text = String::from_utf8(bounded_read(&file, 65536)?)?;
        terrains.push(parse_terrain(path.to_owned(), text)?);
    }
    ensure!(
        !terrains.is_empty(),
        "DSF has no supported orthophoto terrain"
    );
    Ok(Plan {
        root,
        dsf,
        terrains,
    })
}

type TextureIdentity = (Footprint, String, [u64; 3]);

fn texture_identity(terrain: &Terrain, source: String) -> TextureIdentity {
    (
        terrain.footprint,
        source,
        terrain.load_center.map(f64::to_bits),
    )
}

fn report(p: &Plan, config: &PrepareConfig) -> PlanReport {
    let textures = p
        .terrains
        .iter()
        .map(|t| {
            texture_identity(
                t,
                if config.imagery_raster.is_some() {
                    "raster".into()
                } else {
                    t.base.clone()
                },
            )
        })
        .collect::<BTreeSet<_>>()
        .len() as u64;
    let base_block = if config.imagery_raster.is_some() {
        8
    } else {
        16
    };
    let bounds = |geographic: bool| {
        let mut b = Bounds {
            west: f64::INFINITY,
            south: f64::INFINITY,
            east: f64::NEG_INFINITY,
            north: f64::NEG_INFINITY,
        };
        for t in &p.terrains {
            let x = if geographic {
                t.footprint.geographic()
            } else {
                t.footprint.meters()
            };
            b.west = b.west.min(x.west);
            b.east = b.east.max(x.east);
            b.south = b.south.min(x.south);
            b.north = b.north.max(x.north);
        }
        [b.west, b.south, b.east, b.north]
    };
    PlanReport {
        terrains: p.terrains.len(),
        footprints: p
            .terrains
            .iter()
            .map(|t| t.footprint)
            .collect::<BTreeSet<_>>()
            .len(),
        estimated_output_bytes: p.dsf.bytes.len() as u64
            + textures
                * (dds::total_bytes(config.size, base_block) + dds::total_bytes(config.size, 8))
                    as u64
            + p.terrains.len() as u64 * 65536,
        geographic: bounds(true),
        mercator: bounds(false),
    }
}

pub fn inspect(config: &PrepareConfig) -> Result<PlanReport> {
    Ok(report(&plan(config)?, config))
}

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn hash_file(path: &Path) -> Result<(u64, String)> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut count = 0;
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        count += n as u64;
    }
    Ok((count, format!("{:x}", hasher.finalize())))
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("missing parent directory")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut file = File::options()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

struct Output {
    root: PathBuf,
    assets: Vec<Asset>,
    logical_bytes: u64,
    budget: u64,
}

impl Output {
    fn add(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        relative_path(name)?;
        self.reserve(name, bytes.len() as u64)?;
        write_atomic(&self.root.join(name), bytes)?;
        self.assets.push(Asset {
            path: name.to_owned(),
            bytes: bytes.len() as u64,
            sha256: digest(bytes),
        });
        self.logical_bytes += bytes.len() as u64;
        Ok(())
    }

    fn reserve(&self, name: &str, bytes: u64) -> Result<()> {
        ensure!(
            self.logical_bytes + bytes <= self.budget,
            "output exceeds disk budget"
        );
        ensure!(
            !self.assets.iter().any(|a| a.path == name),
            "duplicate output asset: {name}"
        );
        Ok(())
    }
}

pub fn prepare(
    data: &Path,
    id: &str,
    config: &PrepareConfig,
    mut progress: impl FnMut(&str, usize, usize) -> Result<()>,
) -> Result<Generation> {
    let p = plan(config)?;
    let report = report(&p, config);
    let budget = config.budget_mib * 1024 * 1024;
    ensure!(
        report.estimated_output_bytes <= budget,
        "estimated output exceeds disk budget"
    );
    ensure!(
        fs2::available_space(data)? > budget + 64 * 1024 * 1024,
        "insufficient free space including update reserve"
    );
    let stage = data.join("staging").join(id);
    ensure!(!stage.exists(), "staging generation already exists");
    let scenery = stage.join("scenery");
    fs::create_dir_all(&scenery)?;
    let mut raster = config
        .imagery_raster
        .as_ref()
        .map(|path| Raster::open(Path::new(path)))
        .transpose()?;
    if let Some(raster) = raster.as_ref() {
        for t in &p.terrains {
            raster.check_coverage(t.footprint)?;
        }
    }
    let mut lights = Lighting::open(
        Path::new(&config.osm_db),
        config.viirs.as_deref().map(Path::new),
    )?;
    let mut output = Output {
        root: scenery.clone(),
        assets: Vec::new(),
        logical_bytes: 0,
        budget,
    };
    output.add(&config.dsf, &p.dsf.bytes)?;
    let mut built: BTreeMap<TextureIdentity, (String, String)> = BTreeMap::new();
    let mut copied_extras = BTreeSet::new();
    for (i, terrain) in p.terrains.iter().enumerate() {
        progress("compiling", i, p.terrains.len())?;
        let source_key = if raster.is_some() {
            "raster".into()
        } else {
            source_path(&p.root, &terrain.path, &terrain.base)?
                .display()
                .to_string()
        };
        let cache_key = texture_identity(terrain, source_key);
        let (base_name, night_name) = if let Some(names) = built.get(&cache_key) {
            names.clone()
        } else {
            let (base_name, night_name) = terrain.texture_names()?;
            let base = if let Some(raster) = raster.as_mut() {
                dds::encode_rgb(
                    config.size,
                    raster
                        .sample(terrain.footprint, config.size)
                        .with_context(|| format!("sampling imagery for {}", terrain.path))?,
                    false,
                )?
            } else {
                let path = source_path(&p.root, &terrain.path, &terrain.base).context(
                    "missing daytime data; supply a complete local raster or DDS package",
                )?;
                dds::reduce(&bounded_read(&path, 64 * 1024 * 1024)?, config.size)?
            };
            let night = dds::encode_rgb(
                config.size,
                lights
                    .render(terrain.footprint, config.size, config.exposure)
                    .with_context(|| format!("rendering lighting for {}", terrain.path))?,
                true,
            )?;
            output.add(&base_name, &base)?;
            output.add(&night_name, &night)?;
            built.insert(cache_key, (base_name.clone(), night_name.clone()));
            (base_name, night_name)
        };
        output.add(
            &terrain.path,
            terrain
                .compiled_text(config.size, &base_name, &night_name)
                .as_bytes(),
        )?;
        for extra in &terrain.extras {
            let source = source_path(&p.root, &terrain.path, extra)?;
            let relative = source
                .strip_prefix(&p.root)?
                .to_str()
                .context("non-UTF8 dependency path")?;
            if copied_extras.insert(relative.to_owned()) {
                output.add(relative, &bounded_read(&source, 64 * 1024 * 1024)?)?;
            }
        }
    }
    progress("validating", p.terrains.len(), p.terrains.len())?;
    let manifest = Manifest {
        version: PROTOCOL_VERSION, id: id.into(), config: config.clone(),
        source_dsf_sha256: digest(&p.dsf.bytes), assets: output.assets,
        notes: vec![
            "Source DSF geometry and UV bytes are unchanged.".into(),
            "Native files; no imagery/OSM/VIIRS processing is needed for simulator reads.".into(),
            "Normalized legacy OSM roads/building centroids are inferred lighting, not a lamp survey; landuse emission is not included.".into(),
            "External X-Plane defaults: terrain_Water and lib/g10/decals/maquify_2_green_key.dcl where referenced.".into(),
            "RDT1 has no observation-quality layer; absent cells remain unknown and do not force darkness.".into(),
            "Structural verification only: appearance and load time in X-Plane are unverified.".into(),
        ],
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    write_atomic(&stage.join("manifest.json"), &manifest_bytes)?;
    verify_directory(&stage, &digest(&manifest_bytes))?;
    let mut parents = BTreeSet::new();
    for asset in &manifest.assets {
        let path = scenery.join(&asset.path);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444))?;
        File::open(&path)?.sync_all()?;
        let mut parent = path.parent();
        while let Some(directory) = parent {
            if !directory.starts_with(&scenery) {
                break;
            }
            parents.insert(directory.to_path_buf());
            parent = directory.parent();
        }
    }
    for path in parents.iter().rev() {
        File::open(path)?.sync_all()?;
    }
    let final_path = data.join("generations").join(id);
    fs::create_dir_all(final_path.parent().context("generation parent")?)?;
    fs::rename(&stage, &final_path)?;
    File::open(data.join("staging"))?.sync_all()?;
    File::open(data.join("generations"))?.sync_all()?;
    Ok(Generation {
        id: id.into(),
        package: final_path.join("scenery").display().to_string(),
        dsf: config.dsf.clone(),
        assets: manifest.assets.len(),
        bytes: output.logical_bytes,
        size: config.size,
        source_label: config.source_label.clone(),
        manifest_sha256: digest(&manifest_bytes),
        created: now(),
        verified: true,
    })
}

pub fn verify_directory(root: &Path, expected_manifest_hash: &str) -> Result<()> {
    let bytes = bounded_read(&root.join("manifest.json"), 4 * 1024 * 1024)?;
    ensure!(
        digest(&bytes) == expected_manifest_hash,
        "generation manifest changed"
    );
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    ensure!(
        root.file_name().and_then(|s| s.to_str()) == Some(manifest.id.as_str()),
        "generation directory and manifest identity disagree"
    );
    ensure!(
        manifest.version == PROTOCOL_VERSION && manifest.assets.len() <= 20000,
        "unsupported manifest"
    );
    let scenery = root.join("scenery").canonicalize()?;
    let mut paths = BTreeSet::new();
    ensure!(
        manifest
            .assets
            .iter()
            .any(|a| a.path == manifest.config.dsf),
        "manifest omits the DSF"
    );
    for asset in &manifest.assets {
        relative_path(&asset.path)?;
        ensure!(paths.insert(&asset.path), "duplicate manifest asset");
        let path = scenery.join(&asset.path).canonicalize()?;
        ensure!(path.starts_with(&scenery), "asset escapes generation");
        let (len, hash) = hash_file(&path)?;
        ensure!(
            len == asset.bytes && hash == asset.sha256,
            "asset integrity failure: {}",
            asset.path
        );
        match path.extension().and_then(|s| s.to_str()) {
            Some("dds") => {
                dds::validate(&bounded_read(&path, 64 * 1024 * 1024)?)?;
            }
            Some("dsf") => {
                let dsf = parse_dsf(bounded_read(&path, MAX_DSF_BYTES)?)?;
                ensure!(
                    digest(&dsf.bytes) == manifest.source_dsf_sha256,
                    "DSF geometry changed"
                );
                for terrain in dsf.package_terrains() {
                    relative_path(terrain)?;
                    ensure!(
                        manifest.assets.iter().any(|a| a.path == terrain),
                        "missing DSF terrain dependency"
                    );
                }
            }
            Some("ter") => {
                let text = String::from_utf8(bounded_read(&path, 65536)?)?;
                let mut night_count = 0;
                let mut base_count = 0;
                for line in text.lines() {
                    let fields: Vec<_> = line.split_whitespace().collect();
                    if let Some(
                        "BASE_TEX" | "BASE_TEX_NOWRAP" | "LIT_TEX" | "LIT_TEX_NOWRAP"
                        | "BORDER_TEX" | "BORDER_TEX_WRAP" | "COMPOSITE_TEX" | "COMPOSITE_NOWRAP",
                    ) = fields.first().copied()
                    {
                        ensure!(fields.len() == 2, "invalid compiled texture reference");
                        if fields[0].starts_with("LIT_TEX") {
                            night_count += 1;
                        }
                        if fields[0].starts_with("BASE_TEX") {
                            base_count += 1;
                        }
                        let dependency = source_path(&scenery, &asset.path, fields[1])?;
                        let name = dependency
                            .strip_prefix(&scenery)?
                            .to_str()
                            .context("non-UTF8 dependency")?;
                        ensure!(
                            manifest.assets.iter().any(|a| a.path == name),
                            "texture dependency is not sealed in manifest"
                        );
                    }
                }
                ensure!(
                    base_count == 1 && night_count == 1,
                    "terrain must bind one base and one night texture"
                );
            }
            Some("png") => {
                let image = gdal::Dataset::open(&path)?;
                ensure!(
                    image.driver().short_name() == "PNG",
                    "border/composite file is not PNG"
                );
                let (width, height) = image.raster_size();
                ensure!(
                    width > 0 && height > 0 && width <= 4096 && height <= 4096,
                    "border/composite PNG exceeds alpha dimensions"
                );
                image.rasterband(1)?.read_as::<u8>(
                    (0, 0),
                    (width, height),
                    (width, height),
                    None,
                )?;
            }
            _ => anyhow::bail!("unsupported scenery asset type: {}", asset.path),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::Fixture;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn publishes_complete_immutable_assets_without_changing_mesh() -> Result<()> {
        let fixture = Fixture::new()?;
        let generation = prepare(&fixture.data, "good", &fixture.config, |_, _, _| Ok(()))?;
        let root = fixture.data.join("generations/good");
        let package = root.join("scenery");
        assert_eq!(
            fs::read(package.join(&fixture.config.dsf))?,
            fs::read(fixture.source.join(&fixture.config.dsf))?
        );
        assert!(!package.join("textures/wet_GE.dds").exists());
        let wet = fs::read_to_string(package.join("terrain/wet.ter"))?;
        assert!(wet.contains("BASE_TEX_NOWRAP ../textures/dry_GE.dds"));
        assert!(wet.contains("LIT_TEX_NOWRAP ../textures/dry_GE_LIT.dds"));
        assert_eq!(
            fs::metadata(package.join("textures/dry_GE.dds"))?.mode() & 0o222,
            0
        );
        let lit = fs::read(package.join("textures/dry_GE_LIT.dds"))?;
        let mut rgba = vec![0; 256 * 256 * 4];
        texpresso::Format::Bc1.decompress(
            &lit[128..128 + dds::level_bytes(256, 8)],
            256,
            256,
            &mut rgba,
        );
        assert!(rgba.as_chunks::<4>().0.iter().any(|p| p[0] > 0));
        verify_directory(&root, &generation.manifest_sha256)?;
        write_atomic(&package.join("textures/dry_GE_LIT.dds"), b"broken")?;
        assert!(verify_directory(&root, &generation.manifest_sha256).is_err());
        Ok(())
    }

    #[test]
    fn distinct_loading_centers_keep_distinct_texture_bindings() -> Result<()> {
        let fixture = Fixture::new()?;
        let wet = fixture.source.join("terrain/wet.ter");
        let text = fs::read_to_string(&wet)?;
        fs::write(&wet, text.replace("7930", "7931"))?;
        prepare(&fixture.data, "centers", &fixture.config, |_, _, _| Ok(()))?;
        let package = fixture.data.join("generations/centers/scenery");
        assert!(package.join("textures/dry_GE.dds").exists());
        assert!(package.join("textures/wet_GE.dds").exists());
        let text = fs::read_to_string(package.join("terrain/wet.ter"))?;
        assert!(text.contains("7931 256"));
        assert!(text.contains("BASE_TEX_NOWRAP ../textures/wet_GE.dds"));
        Ok(())
    }

    #[test]
    fn incomplete_update_never_replaces_a_published_generation() -> Result<()> {
        let fixture = Fixture::new()?;
        let original = prepare(&fixture.data, "original", &fixture.config, |_, _, _| Ok(()))?;
        fs::remove_file(fixture.source.join("textures/25760_18432_BI16.dds"))?;
        assert!(
            prepare(&fixture.data, "incomplete", &fixture.config, |_, _, _| Ok(
                ()
            ))
            .is_err()
        );
        assert!(!fixture.data.join("generations/incomplete").exists());
        verify_directory(
            &fixture.data.join("generations/original"),
            &original.manifest_sha256,
        )?;
        Ok(())
    }

    #[test]
    fn a_valid_manifest_cannot_hide_a_missing_terrain_dependency() -> Result<()> {
        let fixture = Fixture::new()?;
        prepare(&fixture.data, "closure", &fixture.config, |_, _, _| Ok(()))?;
        let root = fixture.data.join("generations/closure");
        let mut manifest: Manifest =
            serde_json::from_slice(&fs::read(root.join("manifest.json"))?)?;
        manifest.assets.retain(|a| a.path != "textures/dry_GE.dds");
        let bytes = serde_json::to_vec(&manifest)?;
        write_atomic(&root.join("manifest.json"), &bytes)?;
        assert!(verify_directory(&root, &digest(&bytes)).is_err());
        Ok(())
    }
}
