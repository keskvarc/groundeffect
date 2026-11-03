use anyhow::{Result, ensure};
use std::sync::OnceLock;
use texpresso::{Algorithm, Format, Params};

struct SrgbTables {
    u8_to_linear: [f32; 256],
    linear_thresholds: [f32; 255],
    coarse_range: [(u8, u8); 4097],
}

fn srgb_to_linear_exact(v: f32) -> f32 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb_u8_exact(value: f32) -> u8 {
    let v = value.clamp(0.0, 1.0);
    let v = if v <= 0.0031308 {
        12.92 * v
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    };
    (v * 255.0).round() as u8
}

fn srgb_tables() -> &'static SrgbTables {
    static TABLES: OnceLock<SrgbTables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut u8_to_linear = [0.0_f32; 256];
        for (byte, slot) in u8_to_linear.iter_mut().enumerate() {
            *slot = srgb_to_linear_exact(byte as f32 / 255.0);
        }
        let mut linear_thresholds = [0.0_f32; 255];
        let max_bits = 1.0_f32.to_bits();
        for (k, slot) in linear_thresholds.iter_mut().enumerate() {
            let target = (k + 1) as u8;
            let mut low = 0_u32;
            let mut high = max_bits;
            while low < high {
                let mid = low + (high - low) / 2;
                if linear_to_srgb_u8_exact(f32::from_bits(mid)) >= target {
                    high = mid;
                } else {
                    low = mid + 1;
                }
            }
            *slot = f32::from_bits(low);
        }
        let mut coarse_range = [(0_u8, 255_u8); 4097];
        for (bucket, slot) in coarse_range.iter_mut().enumerate() {
            let v0 = bucket as f32 / 4096.0;
            let v1 = ((bucket + 1) as f32 / 4096.0).min(1.0);
            let lo = linear_thresholds.partition_point(|&t| t <= v0) as u8;
            let hi = linear_thresholds.partition_point(|&t| t <= v1) as u8;
            *slot = (lo, hi);
        }
        SrgbTables {
            u8_to_linear,
            linear_thresholds,
            coarse_range,
        }
    })
}

#[inline]
fn linear_to_srgb_u8_fast(tables: &SrgbTables, value: f32) -> u8 {
    let v = value.clamp(0.0, 1.0);
    let bucket = ((v * 4096.0) as usize).min(4096);
    let (lo, hi) = tables.coarse_range[bucket];
    if lo == hi {
        return lo;
    }
    let mut k = lo as usize;
    let end = hi as usize;
    while k < end && tables.linear_thresholds[k] <= v {
        k += 1;
    }
    k as u8
}

fn word(bytes: &[u8], offset: usize) -> Result<u32> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| anyhow::anyhow!("truncated DDS header"))?;
    Ok(u32::from_le_bytes(slice.try_into()?))
}

pub fn level_bytes(size: u32, block: usize) -> usize {
    size.div_ceil(4) as usize * size.div_ceil(4) as usize * block
}

pub fn total_bytes(size: u32, block: usize) -> usize {
    128 + (0..size.ilog2() + 1)
        .map(|m| level_bytes((size >> m).max(1), block))
        .sum::<usize>()
}

pub fn validate(bytes: &[u8]) -> Result<(u32, usize)> {
    ensure!(bytes.get(..4) == Some(b"DDS "), "not a DDS file");
    ensure!(
        word(bytes, 4)? == 124 && word(bytes, 76)? == 32,
        "invalid DDS header sizes"
    );
    let size = word(bytes, 12)?;
    ensure!(
        size == word(bytes, 16)? && size.is_power_of_two() && size <= 8192,
        "DDS must be square, power-of-two and <=8192"
    );
    ensure!(
        word(bytes, 28)? == size.ilog2() + 1,
        "incomplete DDS mip count"
    );
    ensure!(word(bytes, 112)? == 0, "cube/volume DDS is unsupported");
    let block = match bytes.get(84..88) {
        Some(b"DXT1") => 8,
        Some(b"DXT5") => 16,
        _ => anyhow::bail!("only BC1/BC3 DDS is supported"),
    };
    ensure!(
        bytes.len() == total_bytes(size, block),
        "truncated or trailing DDS mip data"
    );
    Ok((size, block))
}

pub fn header(size: u32, block: usize) -> Vec<u8> {
    let mut b = vec![0; 128];
    b[..4].copy_from_slice(b"DDS ");
    for (offset, value) in [
        (4, 124),
        (8, 0x000a1007),
        (12, size),
        (16, size),
        (20, level_bytes(size, block) as u32),
        (28, size.ilog2() + 1),
        (76, 32),
        (80, 4),
        (108, 0x00401008),
    ] {
        b[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    b[84..88].copy_from_slice(if block == 8 { b"DXT1" } else { b"DXT5" });
    b
}

pub fn reduce(bytes: &[u8], target: u32) -> Result<Vec<u8>> {
    let (size, block) = validate(bytes)?;
    ensure!(
        target.is_power_of_two() && target <= size,
        "source DDS is smaller than the selected profile"
    );
    let shift = size.ilog2() - target.ilog2();
    let offset = 128
        + (0..shift)
            .map(|m| level_bytes(size >> m, block))
            .sum::<usize>();
    let mut result = header(target, block);
    result.extend_from_slice(&bytes[offset..]);
    validate(&result)?;
    Ok(result)
}

pub fn encode_rgb(size: u32, rgb: Vec<[f32; 3]>, linear: bool) -> Result<Vec<u8>> {
    ensure!(
        size.is_power_of_two() && size <= 2048 && rgb.len() == (size * size) as usize,
        "invalid bounded image dimensions"
    );
    let tables = srgb_tables();
    let mut out = Vec::with_capacity(total_bytes(size, 8));
    out.extend_from_slice(&header(size, 8));
    let mut pixels: Vec<_> = if linear {
        rgb
    } else {
        rgb.into_iter()
            .map(|p| {
                p.map(|v| {
                    let byte = (v * 255.0).round() as usize;
                    if byte <= 255 && (byte as f32 / 255.0) == v {
                        tables.u8_to_linear[byte]
                    } else {
                        srgb_to_linear_exact(v)
                    }
                })
            })
            .collect()
    };
    let mut n = size as usize;
    let mut mip_level = 0usize;
    let mut rgba = Vec::with_capacity(n * n * 4);
    let mut next = Vec::with_capacity((n / 2).max(1) * (n / 2).max(1));
    loop {
        rgba.clear();
        for p in &pixels[..n * n] {
            for &value in p {
                rgba.push(linear_to_srgb_u8_fast(tables, value));
            }
            rgba.push(255);
        }
        let start = out.len();
        out.resize(start + level_bytes(n as u32, 8), 0);
        Format::Bc1.compress(
            &rgba,
            n,
            n,
            Params {
                algorithm: Algorithm::RangeFit,
                ..Params::default()
            },
            &mut out[start..],
        );
        if n == 1 {
            break;
        }
        mip_level += 1;
        // Progressive distance falloff across higher mip levels for _LIT textures:
        // removes the old peak-hold boost and gently attenuates each coarser mip level
        // so distant roads and cities fade naturally toward the horizon.
        let mip_atten = if linear {
            match mip_level {
                1 => 0.90,
                2 => 0.85,
                _ => 0.82,
            }
        } else {
            1.0
        };
        let half = n / 2;
        next.clear();
        for y in 0..half {
            let row0 = (2 * y) * n;
            let row1 = row0 + n;
            for x in 0..half {
                let col0 = 2 * x;
                let p00 = pixels[row0 + col0];
                let p01 = pixels[row0 + col0 + 1];
                let p10 = pixels[row1 + col0];
                let p11 = pixels[row1 + col0 + 1];
                let mut cell = [0.0_f32; 3];
                for c in 0..3 {
                    let sum = p00[c] + p01[c] + p10[c] + p11[c];
                    let mean = sum * 0.25;
                    cell[c] = mean * mip_atten;
                }
                next.push(cell);
            }
        }
        std::mem::swap(&mut pixels, &mut next);
        n = half;
    }
    validate(&out)?;
    Ok(out)
}

pub fn decode_rgb_level0(bytes: &[u8]) -> Result<Vec<[f32; 3]>> {
    let (size, block) = validate(bytes)?;
    let n = size as usize;
    let level0 = level_bytes(size, block);
    let compressed = bytes
        .get(128..128 + level0)
        .ok_or_else(|| anyhow::anyhow!("truncated DDS level 0"))?;
    let mut rgba = vec![0_u8; n * n * 4];
    let format = if block == 8 { Format::Bc1 } else { Format::Bc3 };
    format.decompress(compressed, n, n, &mut rgba);
    let mut out = Vec::with_capacity(n * n);
    for px in rgba.as_chunks::<4>().0 {
        out.push([
            px[0] as f32 / 255.0,
            px[1] as f32 / 255.0,
            px[2] as f32 / 255.0,
        ]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_complete_mips_and_can_extract_a_tail() -> Result<()> {
        let encoded = encode_rgb(16, vec![[0.15, 0.1, 0.03]; 256], true)?;
        assert_eq!(validate(&encoded)?, (16, 8));
        let decoded = decode_rgb_level0(&encoded)?;
        assert_eq!(decoded.len(), 256);
        assert!(decoded[0][0] > 0.35 && decoded[0][1] > 0.30);
        let reduced = reduce(&encoded, 4)?;
        assert_eq!(validate(&reduced)?, (4, 8));
        assert_eq!(&reduced[128..], &encoded[encoded.len() - 24..]);
        assert!(validate(&encoded[..encoded.len() - 1]).is_err());
        assert_eq!(total_bytes(1024, 8), 699192);
        let tables = srgb_tables();
        for b in 0..=255_u8 {
            let v = b as f32 / 255.0;
            assert_eq!(tables.u8_to_linear[b as usize], srgb_to_linear_exact(v));
        }
        for step in 0..=65536 {
            let v = step as f32 / 65536.0;
            assert_eq!(
                linear_to_srgb_u8_fast(tables, v),
                linear_to_srgb_u8_exact(v)
            );
        }
        for &threshold in &tables.linear_thresholds {
            let prev = f32::from_bits(threshold.to_bits().saturating_sub(1));
            assert_eq!(
                linear_to_srgb_u8_fast(tables, prev),
                linear_to_srgb_u8_exact(prev)
            );
            assert_eq!(
                linear_to_srgb_u8_fast(tables, threshold),
                linear_to_srgb_u8_exact(threshold)
            );
        }
        Ok(())
    }
}
