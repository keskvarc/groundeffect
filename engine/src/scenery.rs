pub use crate::dsf_compiler::compile_global_dsf_to_ortho;
use crate::geo::Footprint;
use anyhow::{Context, Result, bail, ensure};
use md5::{Digest, Md5};
use std::{
    io::{Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
};

pub const MAX_DSF_BYTES: u64 = 256 * 1024 * 1024;
const MAX_DEFINITIONS: u64 = 4 * 1024 * 1024;

pub struct Dsf {
    pub bytes: Vec<u8>,
    pub terrains: Vec<String>,
}

impl Dsf {
    pub fn package_terrains(&self) -> impl Iterator<Item = &str> {
        package_terrains(&self.terrains)
    }
}

pub fn package_terrains(terrains: &[String]) -> impl Iterator<Item = &str> {
    terrains
        .iter()
        .map(String::as_str)
        .filter(|name| *name != "terrain_Water")
}

pub struct Terrain {
    pub path: String,
    pub text: String,
    pub footprint: Footprint,
    pub load_center: [f64; 3],
    pub base: String,
    pub extras: Vec<String>,
}

pub fn relative_path(value: &str) -> Result<PathBuf> {
    ensure!(
        !value.is_empty() && !value.contains('\\'),
        "invalid resource path"
    );
    let p = Path::new(value);
    ensure!(
        p.components().all(|c| matches!(c, Component::Normal(_))),
        "resource must be a safe relative path: {value}"
    );
    Ok(p.to_path_buf())
}

pub fn dependency_path(anchor: &str, value: &str) -> Result<PathBuf> {
    ensure!(
        !value.is_empty() && !value.contains('\\') && !Path::new(value).is_absolute(),
        "absolute resource reference"
    );
    let mut p = relative_path(anchor)?;
    p.pop();
    for c in Path::new(value).components() {
        match c {
            Component::Normal(name) => p.push(name),
            Component::CurDir => {}
            Component::ParentDir => ensure!(p.pop(), "resource escapes package"),
            _ => bail!("invalid resource reference"),
        }
    }
    Ok(p)
}

pub fn source_path(root: &Path, anchor: &str, value: &str) -> Result<PathBuf> {
    let resolved = root.join(dependency_path(anchor, value)?).canonicalize()?;
    ensure!(
        resolved.starts_with(root),
        "resource symlink escapes package"
    );
    Ok(resolved)
}

fn atoms(bytes: &[u8]) -> Result<Vec<(&[u8], &[u8])>> {
    let mut result = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        let head = bytes.get(pos..pos + 8).context("truncated DSF atom")?;
        let len = u32::from_le_bytes(head[4..8].try_into()?) as usize;
        ensure!(len >= 8, "invalid DSF atom length");
        let end = pos.checked_add(len).context("DSF atom overflow")?;
        result.push((
            &head[..4],
            bytes.get(pos + 8..end).context("DSF atom out of bounds")?,
        ));
        pos = end;
    }
    Ok(result)
}

fn definitions(body: &[u8], terrains: &mut Option<Vec<String>>) -> Result<()> {
    ensure!(
        body.len() as u64 <= MAX_DEFINITIONS,
        "DSF definition table exceeds reader limit"
    );
    for (kind, entries) in atoms(body)? {
        if kind != b"TRET" {
            continue;
        }
        let names: Vec<_> = entries
            .split(|c| *c == 0)
            .filter(|s| !s.is_empty())
            .take(4097)
            .map(|s| String::from_utf8(s.to_vec()))
            .collect::<std::result::Result<_, _>>()?;
        ensure!(names.len() <= 4096, "too many terrain definitions");
        ensure!(terrains.is_none(), "duplicate terrain definition table");
        *terrains = Some(names);
    }
    Ok(())
}

pub fn read_dsf_terrains(reader: &mut (impl Read + Seek), length: u64) -> Result<Vec<String>> {
    ensure!(
        (28..=MAX_DSF_BYTES).contains(&length),
        "DSF size outside alpha limits"
    );
    reader.seek(SeekFrom::Start(0))?;
    let mut header = [0_u8; 12];
    reader.read_exact(&mut header)?;
    ensure!(
        &header == b"XPLNEDSF\x01\0\0\0",
        "expected uncompressed version-1 DSF"
    );
    let end = length - 16;
    let mut checksum = Md5::new();
    checksum.update(header);
    let mut remaining = end - 12;
    let mut buffer = [0_u8; 65536];
    while remaining != 0 {
        let count = remaining.min(buffer.len() as u64) as usize;
        reader.read_exact(&mut buffer[..count])?;
        checksum.update(&buffer[..count]);
        remaining -= count as u64;
    }
    let mut expected = [0_u8; 16];
    reader.read_exact(&mut expected)?;
    ensure!(
        checksum.finalize().as_slice() == expected,
        "DSF checksum mismatch"
    );
    reader.seek(SeekFrom::Start(12))?;
    let mut position = 12;
    let mut terrains = None;
    while position < end {
        ensure!(end - position >= 8, "truncated DSF atom");
        let mut head = [0_u8; 8];
        reader.read_exact(&mut head)?;
        let size = u32::from_le_bytes(head[4..8].try_into()?) as u64;
        ensure!(size >= 8, "invalid DSF atom length");
        let next = position.checked_add(size).context("DSF atom overflow")?;
        ensure!(next <= end, "DSF atom out of bounds");
        if &head[..4] == b"NFED" {
            ensure!(
                size - 8 <= MAX_DEFINITIONS,
                "DSF definition table exceeds reader limit"
            );
            let mut body = vec![0; (size - 8) as usize];
            reader.read_exact(&mut body)?;
            definitions(&body, &mut terrains)?;
        } else {
            reader.seek(SeekFrom::Start(next))?;
        }
        position = next;
    }
    let terrains = terrains.context("DSF has no terrain table")?;
    ensure!(terrains.len() <= 4096, "too many terrain definitions");
    Ok(terrains)
}

pub fn read_dsf_terrain_table(reader: &mut (impl Read + Seek), length: u64) -> Result<Vec<String>> {
    ensure!(
        (28..=MAX_DSF_BYTES).contains(&length),
        "DSF size outside alpha limits"
    );
    reader.seek(SeekFrom::Start(0))?;
    let mut header = [0_u8; 12];
    reader.read_exact(&mut header)?;
    ensure!(
        &header == b"XPLNEDSF\x01\0\0\0",
        "expected uncompressed version-1 DSF"
    );
    let end = length - 16;
    let mut position = 12;
    let mut terrains = None;
    while position < end {
        ensure!(end - position >= 8, "truncated DSF atom");
        let mut head = [0_u8; 8];
        reader.read_exact(&mut head)?;
        let size = u32::from_le_bytes(head[4..8].try_into()?) as u64;
        ensure!(size >= 8, "invalid DSF atom length");
        let next = position.checked_add(size).context("DSF atom overflow")?;
        ensure!(next <= end, "DSF atom out of bounds");
        if &head[..4] == b"NFED" {
            ensure!(
                size - 8 <= MAX_DEFINITIONS,
                "DSF definition table exceeds reader limit"
            );
            let mut body = vec![0; (size - 8) as usize];
            reader.read_exact(&mut body)?;
            definitions(&body, &mut terrains)?;
            break;
        } else {
            reader.seek(SeekFrom::Start(next))?;
        }
        position = next;
    }
    let terrains = terrains.context("DSF has no terrain table")?;
    ensure!(terrains.len() <= 4096, "too many terrain definitions");
    Ok(terrains)
}

pub fn parse_dsf(bytes: Vec<u8>) -> Result<Dsf> {
    let terrains = read_dsf_terrains(&mut std::io::Cursor::new(&bytes), bytes.len() as u64)?;
    Ok(Dsf { bytes, terrains })
}

fn loading_center(fields: &[&str]) -> Result<[f64; 3]> {
    ensure!(fields.len() == 5, "invalid loading-center field count");
    let center = [
        fields[1].parse::<f64>()?,
        fields[2].parse::<f64>()?,
        fields[3].parse::<f64>()?,
    ];
    let pixels = fields[4].parse::<u32>()?;
    ensure!(
        center.iter().all(|v| v.is_finite())
            && center[0].abs() <= 90.0
            && center[1].abs() <= 180.0
            && center[2] > 0.0
            && pixels.is_power_of_two()
            && pixels <= 8192,
        "invalid loading-center coordinates, extent or resolution"
    );
    Ok(center)
}

pub fn parse_terrain(path: String, text: String) -> Result<Terrain> {
    relative_path(&path)?;
    ensure!(
        Path::new(&path).parent() == Some(Path::new("terrain")),
        "alpha requires flat terrain/*.ter resources"
    );
    ensure!(text.len() <= 65536, "oversized terrain file");
    let tokens: Vec<_> = text.split_whitespace().take(3).collect();
    ensure!(
        tokens == ["A", "800", "TERRAIN"],
        "unsupported terrain header"
    );
    let mut base = None;
    let mut extras = Vec::new();
    let mut center = None;
    let mut border_center = false;
    let mut border_reference = false;
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        match fields.first().copied() {
            Some("BASE_TEX" | "BASE_TEX_NOWRAP") => {
                ensure!(fields.len() == 2 && base.is_none(), "invalid base texture");
                base = Some(fields[1].to_owned());
            }
            Some("BORDER_TEX" | "BORDER_TEX_WRAP" | "COMPOSITE_TEX" | "COMPOSITE_NOWRAP") => {
                ensure!(fields.len() == 2, "invalid texture dependency");
                border_reference |= matches!(fields[0], "BORDER_TEX" | "BORDER_TEX_WRAP");
                extras.push(fields[1].to_owned());
            }
            Some("LOAD_CENTER") => {
                ensure!(center.is_none(), "duplicate LOAD_CENTER");
                center = Some(loading_center(&fields)?);
            }
            Some("LOAD_CENTER_BORDER") => {
                ensure!(!border_center, "duplicate LOAD_CENTER_BORDER");
                loading_center(&fields)?;
                border_center = true;
            }
            Some("DECAL_LIB") => {
                ensure!(
                    fields.len() == 2 && fields[1] == "lib/g10/decals/maquify_2_green_key.dcl",
                    "alpha only qualifies the standard maquify_2_green_key decal"
                );
            }
            Some("LIT_TEX" | "LIT_TEX_NOWRAP") => {
                ensure!(fields.len() == 2, "invalid source LIT reference")
            }
            Some("A" | "800" | "TERRAIN" | "WET" | "NO_ALPHA" | "NO_SHADOW") => {
                ensure!(fields.len() == 1, "invalid terrain directive");
            }
            None => {}
            Some(command) if command.starts_with('#') || command.starts_with("//") => {}
            Some(command) => bail!("terrain directive is not qualified by the alpha: {command}"),
        }
    }
    ensure!(
        !border_center || border_reference,
        "LOAD_CENTER_BORDER has no border texture"
    );
    let base = base.context("terrain is missing BASE_TEX")?;
    let name = Path::new(&base)
        .file_name()
        .and_then(|s| s.to_str())
        .context("invalid base texture filename")?;
    let footprint = Footprint::parse(name)?;
    let load_center = center.context("orthophoto terrain requires LOAD_CENTER")?;
    let [lat, lon, _] = load_center;
    let b = footprint.geographic();
    ensure!(
        lat.is_finite()
            && lon.is_finite()
            && (b.south..=b.north).contains(&lat)
            && (b.west..=b.east).contains(&lon),
        "LOAD_CENTER disagrees with texture footprint"
    );
    Ok(Terrain {
        path,
        text,
        footprint,
        load_center,
        base,
        extras,
    })
}

pub fn synthesize_terrain(path: String, has_border_mask: bool) -> Result<Terrain> {
    relative_path(&path)?;
    ensure!(
        Path::new(&path).parent() == Some(Path::new("terrain")) && path.ends_with(".ter"),
        "alpha requires flat terrain/*.ter resources"
    );
    let footprint = Footprint::parse_terrain_name(&path)?;
    let [lat, lon, extent] = footprint.load_center();
    let file_stem = Path::new(&path)
        .file_stem()
        .and_then(|s| s.to_str())
        .context("invalid terrain filename")?;
    let is_overlay = file_stem.ends_with("_sea_overlay")
        || file_stem.ends_with("_water_overlay")
        || file_stem.ends_with("_sea");
    let base_stem = file_stem
        .strip_suffix("_sea_overlay")
        .or_else(|| file_stem.strip_suffix("_water_overlay"))
        .or_else(|| file_stem.strip_suffix("_sea"))
        .unwrap_or(file_stem);
    let base = format!("../textures/{base_stem}.dds");
    let text = if is_overlay && has_border_mask {
        let border = format!(
            "../textures/{}_{}_ZL{}.png",
            footprint.row, footprint.col, footprint.zoom
        );
        format!(
            "A\n800\nTERRAIN\n\nLOAD_CENTER {lat:.5} {lon:.5} {extent:.0} 4096\nBASE_TEX_NOWRAP {base}\nBORDER_TEX {border}\nLOAD_CENTER_BORDER {lat:.5} {lon:.5} {extent:.0} 512\nWET\nNO_SHADOW\n"
        )
    } else if is_overlay {
        format!(
            "A\n800\nTERRAIN\n\nLOAD_CENTER {lat:.5} {lon:.5} {extent:.0} 4096\nBASE_TEX_NOWRAP {base}\nWET\nNO_SHADOW\n"
        )
    } else {
        format!(
            "A\n800\nTERRAIN\n\nLOAD_CENTER {lat:.5} {lon:.5} {extent:.0} 4096\nBASE_TEX_NOWRAP {base}\nDECAL_LIB lib/g10/decals/maquify_2_green_key.dcl\nNO_ALPHA\n"
        )
    };
    parse_terrain(path, text)
}

impl Terrain {
    pub fn texture_names(&self) -> Result<(String, String)> {
        let name = Path::new(&self.path)
            .file_stem()
            .and_then(|s| s.to_str())
            .context("invalid terrain name")?;
        Ok((
            format!("textures/{name}_GE.dds"),
            format!("textures/{name}_GE_LIT.dds"),
        ))
    }

    pub fn compiled_text(&self, size: u32, base: &str, night: &str) -> String {
        let mut result = String::new();
        for line in self.text.lines() {
            let mut fields: Vec<_> = line.split_whitespace().map(str::to_owned).collect();
            match fields.first().map(String::as_str) {
                Some("BASE_TEX" | "BASE_TEX_NOWRAP") => {
                    result.push_str(&format!(
                        "BASE_TEX_NOWRAP ../{base}\nLIT_TEX_NOWRAP ../{night}\n"
                    ));
                }
                Some("LIT_TEX" | "LIT_TEX_NOWRAP") => {}
                Some("LOAD_CENTER") => {
                    fields[4] = size.to_string();
                    result.push_str(&fields.join(" "));
                    result.push('\n');
                }
                _ => {
                    result.push_str(line);
                    result.push('\n');
                }
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_terrain_semantics_and_replaces_only_texture_bindings() -> Result<()> {
        let t = parse_terrain("terrain/example.ter".into(),
            "A\n800\nTERRAIN\nLOAD_CENTER 35.85344 -78.70605 7930 4096\nBASE_TEX_NOWRAP ../textures/25760_18432_BI16.dds\nLIT_TEX old.dds\nLOAD_CENTER_BORDER 35.85344 -78.70605 7930 512\nBORDER_TEX ../textures/mask.png\nNO_ALPHA\n".into())?;
        let text = t.compiled_text(1024, "textures/shared.dds", "textures/shared_LIT.dds");
        assert!(text.contains("35.85344 -78.70605 7930 1024"));
        assert!(text.contains("NO_ALPHA"));
        assert!(!text.contains("old.dds"));
        assert_eq!(text.matches("LIT_TEX").count(), 1);
        assert!(text.contains("BASE_TEX_NOWRAP ../textures/shared.dds"));
        assert!(text.contains("LOAD_CENTER_BORDER 35.85344 -78.70605 7930 512"));
        assert!(text.contains("BORDER_TEX ../textures/mask.png"));
        assert!(relative_path("../outside").is_err());
        assert!(parse_dsf(b"XPLNEDSF".to_vec()).is_err());
        Ok(())
    }

    #[test]
    fn rejects_corruption_traversal_and_out_of_bounds_atoms() -> Result<()> {
        let fixture = crate::fixtures::Fixture::new()?;
        let mut bytes = std::fs::read(fixture.source.join(&fixture.config.dsf))?;
        parse_dsf(bytes.clone())?;
        bytes[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        let end = bytes.len() - 16;
        let checksum = Md5::digest(&bytes[..end]);
        bytes[end..].copy_from_slice(&checksum);
        assert!(parse_dsf(bytes).is_err());
        assert!(source_path(&fixture.source, "terrain/dry.ter", "../../outside").is_err());
        std::os::unix::fs::symlink("/etc/passwd", fixture.source.join("textures/escape.dds"))?;
        assert!(source_path(&fixture.source, "terrain/dry.ter", "../textures/escape.dds").is_err());
        Ok(())
    }

    #[test]
    fn rejects_invalid_border_loading_centers() {
        let text = "A\n800\nTERRAIN\nLOAD_CENTER 35.85344 -78.70605 7930 4096\nBASE_TEX ../textures/25760_18432_BI16.dds\nLOAD_CENTER_BORDER 35.85344 -78.70605 7930 512\nBORDER_TEX ../textures/mask.png\n";
        for invalid in [
            text.replace("7930 512", "7930 768"),
            text.replace("LOAD_CENTER_BORDER 35.85344", "LOAD_CENTER_BORDER NaN"),
            text.replace("BORDER_TEX ../textures/mask.png\n", ""),
            format!("{text}LOAD_CENTER_BORDER 35.85344 -78.70605 7930 512\n"),
        ] {
            assert!(parse_terrain("terrain/example.ter".into(), invalid).is_err());
        }
    }

    #[test]
    fn synthesizes_terrain_and_compiles_global_scenery_dsf() -> Result<()> {
        let land = synthesize_terrain("terrain/23168_10432_BI16.ter".into(), false)?;
        assert_eq!(land.footprint.row, 23168);
        assert_eq!(land.footprint.col, 10432);
        assert_eq!(land.footprint.zoom, 16);
        assert!((land.load_center[0] - 46.52863).abs() < 1e-4);
        assert!((land.load_center[1] - -122.65137).abs() < 1e-4);
        assert!((land.load_center[2] - 6731.0).abs() <= 1.0);

        let roots = crate::dsf_compiler::discover_laminar_scenery_roots(None);
        if let Some(demo_dsf) = roots
            .iter()
            .map(|r| r.join("Earth nav data/+40-130/+45-123.dsf"))
            .find(|p| p.is_file())
        {
            let tmp = tempfile::tempdir()?;
            let out_dsf = tmp.path().join("+45-123.dsf");
            let terrains = compile_global_dsf_to_ortho(&demo_dsf, &out_dsf)?;
            assert_eq!(terrains.first().map(String::as_str), Some("terrain_Water"));
            assert!(terrains.len() > 200);
            let parsed = parse_dsf(std::fs::read(&out_dsf)?)?;
            assert_eq!(parsed.terrains, terrains);
            for t in parsed.package_terrains() {
                let synth = synthesize_terrain(t.to_owned(), false)?;
                assert_eq!(synth.footprint.zoom, 16);
            }
        }
        Ok(())
    }
}
