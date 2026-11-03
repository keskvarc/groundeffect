use anyhow::{Result, bail, ensure};
use std::f64::consts::PI;

pub const RADIUS: f64 = 6_378_137.0;
pub const HALF_WORLD: f64 = PI * RADIUS;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Footprint {
    pub row: u32,
    pub col: u32,
    pub zoom: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct Bounds {
    pub west: f64,
    pub south: f64,
    pub east: f64,
    pub north: f64,
}

pub fn mercator(lon: f64, lat: f64) -> (f64, f64) {
    (
        lon.to_radians() * RADIUS,
        lat.to_radians().tan().asinh() * RADIUS,
    )
}

pub fn latitude(y: f64) -> f64 {
    (y / RADIUS).sinh().atan().to_degrees()
}

impl Footprint {
    pub fn parse(name: &str) -> Result<Self> {
        let stem = name.strip_suffix(".dds").unwrap_or(name);
        let parts: Vec<_> = stem.split('_').collect();
        ensure!(parts.len() == 3, "unsupported texture identifier: {name}");
        let digits = parts[2].trim_start_matches(char::is_alphabetic);
        ensure!(digits.len() < parts[2].len(), "missing provider: {name}");
        let key = Self {
            row: parts[0].parse()?,
            col: parts[1].parse()?,
            zoom: digits.parse()?,
        };
        ensure!((4..=22).contains(&key.zoom), "unsupported texture zoom");
        let n = 1_u32 << key.zoom;
        if !key.row.is_multiple_of(16)
            || !key.col.is_multiple_of(16)
            || key.row > n - 16
            || key.col > n - 16
        {
            bail!("unaligned or out-of-world terrain footprint: {name}");
        }
        Ok(key)
    }

    pub fn parse_terrain_name(path: &str) -> Result<Self> {
        let file = std::path::Path::new(path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(path);
        let stem = file.strip_suffix(".ter").unwrap_or(file);
        let stem = stem
            .strip_suffix("_sea_overlay")
            .or_else(|| stem.strip_suffix("_water_overlay"))
            .or_else(|| stem.strip_suffix("_sea"))
            .unwrap_or(stem);
        Self::parse(stem)
    }

    pub fn load_center(self) -> [f64; 3] {
        let m = self.meters();
        let b = self.geographic();
        let center_lat = latitude((m.north + m.south) * 0.5);
        let center_lon = (b.west + b.east) * 0.5;
        let super_tiles = ((1_u32 << self.zoom) / 16).max(1) as f64;
        let extent = (2.0 * HALF_WORLD * center_lat.to_radians().cos() / super_tiles).round();
        [center_lat, center_lon, extent.max(1.0)]
    }

    pub fn meters(self) -> Bounds {
        let unit = 2.0 * HALF_WORLD / (1_u32 << self.zoom) as f64;
        Bounds {
            west: -HALF_WORLD + self.col as f64 * unit,
            east: -HALF_WORLD + (self.col + 16) as f64 * unit,
            north: HALF_WORLD - self.row as f64 * unit,
            south: HALF_WORLD - (self.row + 16) as f64 * unit,
        }
    }

    pub fn geographic(self) -> Bounds {
        let b = self.meters();
        Bounds {
            west: (b.west / RADIUS).to_degrees(),
            east: (b.east / RADIUS).to_degrees(),
            north: latitude(b.north),
            south: latitude(b.south),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footprint_is_sixteen_web_tiles_not_one_pixel_grid() -> Result<()> {
        let k = Footprint::parse("25760_18432_BI16.dds")?;
        let b = k.geographic();
        assert!((b.east - b.west - 16.0 * 360.0 / 65536.0).abs() < 1e-10);
        assert!(b.south < 35.85344 && b.north > 35.85344);
        let (x, y) = mercator(b.west, b.north);
        assert!((x - k.meters().west).abs() < 1e-7);
        assert!((y - k.meters().north).abs() < 1e-7);
        Ok(())
    }

    #[test]
    fn rejects_bad_geometry_instead_of_wrapping_it() {
        for name in [
            "25761_18432_BI16.dds",
            "0_0_BI3.dds",
            "65536_0_BI16.dds",
            "../bad.dds",
        ] {
            assert!(Footprint::parse(name).is_err(), "{name}");
        }
    }
}
