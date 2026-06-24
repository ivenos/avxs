use crate::ffms2::{PixelSubsampling, VideoInfo};

pub fn calculate(info: &VideoInfo, stem: &str, threads_per_worker: usize) -> usize {
    let cpu_cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    let ram_gb = available_ram_gib();

    let megapixels = (info.width as f64 * info.height as f64) / 1_000_000.0;

    let pix_mult = match info.pixel_format.subsampling {
        PixelSubsampling::Yuv444 => 1.5,
        PixelSubsampling::Yuv422 => 1.25,
        PixelSubsampling::Yuv420 => 1.0,
    };

    const CM_RAM: f64 = 0.3;
    const ENC_RAM: f64 = 1.2;

    let by_cpu = cpu_cores / threads_per_worker;
    let ram_per_worker = megapixels * (ENC_RAM + CM_RAM) * pix_mult;
    let by_ram = if ram_per_worker > 0.0 {
        (ram_gb / ram_per_worker).round() as usize
    } else {
        usize::MAX
    };

    let workers = by_cpu.min(by_ram).max(1);

    tracing::info!(
        "[{stem}] workers: {workers} \
         (cpu={cpu_cores}/{threads_per_worker} threads allows {by_cpu}, \
         ram={ram_gb:.0}GB/{ram_per_worker:.1}GB allows {by_ram})"
    );

    workers
}

fn available_ram_gib() -> f64 {
    let Ok(content) = std::fs::read_to_string("/proc/meminfo") else { return 1.0 };
    content.lines()
        .find(|l| l.starts_with("MemAvailable:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u64>().ok())
        .map(|kb| kb as f64 / 1_048_576.0)
        .unwrap_or(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffms2::{PixelFormat, PixelSubsampling};

    fn info(w: u32, h: u32, sub: PixelSubsampling) -> VideoInfo {
        VideoInfo {
            width: w,
            height: h,
            fps_num: 24,
            fps_den: 1,
            sar_num: 1,
            sar_den: 1,
            num_frames: 100,
            pixel_format: PixelFormat {
                pix_fmt: 0,
                bit_depth: 10,
                subsampling: sub,
            },
        }
    }

    #[test]
    fn workers_at_least_one() {
        let i = info(1920, 1080, PixelSubsampling::Yuv420);
        assert!(calculate(&i, "test", 6) >= 1);
    }

    #[test]
    fn workers_4k_hdr_fewer_than_1080p() {
        let hd = info(1920, 1080, PixelSubsampling::Yuv420);
        let uhd = info(3840, 2160, PixelSubsampling::Yuv444);
        let w_hd = calculate(&hd, "test", 6);
        let w_uhd = calculate(&uhd, "test", 6);
        assert!(w_hd >= w_uhd, "4K/444 should use <= workers than 1080p/420");
    }
}
