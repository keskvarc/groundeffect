use crate::geo::Footprint;
use anyhow::{Context, Result, ensure};
use gdal::{
    Dataset,
    raster::{ColorInterpretation, GdalDataType, ResampleAlg},
};
use std::path::Path;

pub struct Raster {
    dataset: Dataset,
    transform: [f64; 6],
}

impl Raster {
    pub fn open(path: &Path) -> Result<Self> {
        let path = path
            .canonicalize()
            .context("imagery must be a prepared local GeoTIFF")?;
        let dataset = Dataset::open(path)?;
        ensure!(
            dataset.driver().short_name() == "GTiff",
            "imagery must be a local GeoTIFF"
        );
        ensure!(
            dataset.spatial_ref()?.auth_code()? == 3857,
            "imagery must be EPSG:3857"
        );
        ensure!(dataset.raster_count() >= 3, "imagery must have RGB bands");
        for (index, interpretation) in [
            ColorInterpretation::RedBand,
            ColorInterpretation::GreenBand,
            ColorInterpretation::BlueBand,
        ]
        .into_iter()
        .enumerate()
        {
            let band = dataset.rasterband(index + 1)?;
            ensure!(
                band.band_type() == GdalDataType::UInt8
                    && band.color_interpretation() == interpretation,
                "imagery bands must be explicitly tagged 8-bit RGB"
            );
        }
        let transform = dataset.geo_transform()?;
        ensure!(
            transform.iter().all(|v| v.is_finite())
                && transform[1] > 0.0
                && transform[5] < 0.0
                && transform[2] == 0.0
                && transform[4] == 0.0,
            "unsupported raster transform"
        );
        Ok(Self { dataset, transform })
    }

    fn window(&self, key: Footprint) -> Result<((isize, isize), (usize, usize))> {
        let b = key.meters();
        let t = self.transform;
        let raw = [
            (b.west - t[0]) / t[1],
            (b.north - t[3]) / t[5],
            (b.east - b.west) / t[1],
            (b.south - b.north) / t[5],
        ];
        ensure!(
            raw.iter().all(|v| (v.round() - v).abs() < 0.001),
            "raster is not aligned to the Web Mercator pixel grid; prepare with -tap"
        );
        let x = raw[0].round() as isize;
        let y = raw[1].round() as isize;
        let w = raw[2].round() as usize;
        let h = raw[3].round() as usize;
        let (width, height) = self.dataset.raster_size();
        ensure!(
            x >= 0
                && y >= 0
                && w > 0
                && h > 0
                && x as usize + w <= width
                && y as usize + h <= height,
            "imagery does not cover terrain footprint {key:?}"
        );
        ensure!(
            w <= 4096 && h <= 4096,
            "source window exceeds alpha limit; prepare a coarser aligned raster"
        );
        Ok(((x, y), (w, h)))
    }

    pub fn check_coverage(&self, key: Footprint) -> Result<()> {
        self.window(key).map(|_| ())
    }

    pub fn sample(&self, key: Footprint, size: u32) -> Result<Vec<[f32; 3]>> {
        let (origin, window) = self.window(key)?;
        let n = size as usize;
        let mut result = vec![[0.0; 3]; n * n];
        for channel in 0..3 {
            let band = self.dataset.rasterband(channel + 1)?;
            let data = band.read_as::<u8>(origin, window, (n, n), Some(ResampleAlg::Bilinear))?;
            let mask = band
                .open_mask_band()?
                .read_as::<u8>(origin, window, window, None)?;
            ensure!(
                mask.data().iter().all(|v| *v == 255),
                "imagery contains uncovered/nodata pixels"
            );
            ensure!(
                data.data().len() == result.len(),
                "raster read size mismatch"
            );
            for (p, value) in result.iter_mut().zip(data.data()) {
                p[channel] = *value as f32 / 255.0;
            }
        }
        ensure!(result.len() == n * n, "invalid raster output");
        Ok(result)
    }
}

pub fn bounded_read(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    use std::{fs::File, io::Read};
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    ensure!(
        file.metadata()?.len() <= maximum,
        "file exceeds input limit: {}",
        path.display()
    );
    let mut result = Vec::new();
    file.take(maximum + 1).read_to_end(&mut result)?;
    ensure!(
        result.len() as u64 <= maximum,
        "input grew beyond its limit"
    );
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gdal::{DriverManager, raster::Buffer, spatial_ref::SpatialRef};

    fn fixture(path: &Path, hole: bool, shift: f64) -> Result<Footprint> {
        let key = Footprint::parse("25760_18432_BI16.dds")?;
        let b = key.meters();
        let resolution = (b.east - b.west) / 64.0;
        let mut dataset = DriverManager::get_driver_by_name("GTiff")?
            .create_with_band_type::<u8, _>(path, 64, 64, 3)?;
        dataset.set_geo_transform(&[
            b.west + shift * resolution,
            resolution,
            0.0,
            b.north,
            0.0,
            -resolution,
        ])?;
        dataset.set_spatial_ref(&SpatialRef::from_epsg(3857)?)?;
        for (index, interpretation) in [
            ColorInterpretation::RedBand,
            ColorInterpretation::GreenBand,
            ColorInterpretation::BlueBand,
        ]
        .into_iter()
        .enumerate()
        {
            let mut pixels = vec![100_u8; 64 * 64];
            if hole {
                pixels[0] = 0;
            }
            let mut band = dataset.rasterband(index + 1)?;
            band.set_color_interpretation(interpretation)?;
            band.set_no_data_value(Some(0.0))?;
            band.write((0, 0), (64, 64), &mut Buffer::new((64, 64), pixels))?;
        }
        dataset.flush_cache()?;
        Ok(key)
    }

    #[test]
    fn preserves_footprint_and_rejects_subsampling_hidden_holes() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("day.tif");
        let key = fixture(&path, false, 0.0)?;
        let raster = Raster::open(&path)?;
        let rgb = raster.sample(key, 8)?;
        assert!(rgb.iter().all(|p| (p[0] - 100.0 / 255.0).abs() < 1e-5));
        drop(raster);
        fixture(&path, true, 0.0)?;
        assert!(Raster::open(&path)?.sample(key, 8).is_err());
        fixture(&path, false, 0.25)?;
        assert!(Raster::open(&path)?.check_coverage(key).is_err());
        Ok(())
    }
}
