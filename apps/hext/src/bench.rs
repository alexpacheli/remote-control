//! `bench`: how long video encoding takes on this machine, per encoder
//! configuration, with a real image of the screen scrolling.

use anyhow::Result;

use crate::BenchArgs;

#[cfg(target_os = "macos")]
pub fn run(args: &BenchArgs) -> Result<()> {
    use std::time::{Duration, Instant};

    use anyhow::{Context, bail};
    use hext_capture::PixelFormat;
    use hext_codec::{EncoderPreference, EncoderSettings};

    hext_capture::preflight()?;
    let mut capturer = hext_capture::open(args.display, false, args.fps)?;
    // The first frame arrives once the stream starts, even on a still screen.
    let deadline = Instant::now() + Duration::from_secs(3);
    while capturer.frame().is_none() && Instant::now() < deadline {
        capturer.next_frame(Duration::from_millis(100))?;
    }
    let frame = capturer
        .frame()
        .context("nenhuma imagem da tela em 3 s")?
        .clone();
    drop(capturer);
    if frame.height < 16 {
        bail!("imagem pequena demais: {}x{}", frame.width, frame.height);
    }

    let settings = EncoderSettings {
        bitrate_bps: args.bitrate_mbps * 1_000_000,
        max_fps: args.fps as f32,
        preference: EncoderPreference::Auto,
    };
    let (width, height) = (frame.width, frame.height);
    println!(
        "{width}x{height} ({:?}), {} quadros por configuração a {} fps, {} Mbps; \
         a imagem da tela rola 8 linhas por quadro.\n",
        frame.format, args.frames, args.fps, args.bitrate_mbps
    );

    // Frame t: the screenshot scrolled up by 8 rows per frame, wrapping
    // around (in NV12 each plane scrolls on its own: 8 Y rows, 4 UV rows).
    let (w, h) = (width as usize, height as usize);
    let (format, planes) = match frame.format {
        PixelFormat::Bgra => (
            hext_codec::PixelFormat::Bgra,
            vec![(0..w * h * 4, w * 4, 8)],
        ),
        PixelFormat::Nv12 => (
            hext_codec::PixelFormat::Nv12,
            vec![(0..w * h, w, 8), (w * h..w * h * 3 / 2, w, 4)],
        ),
    };
    let image_at = |t: u32, out: &mut Vec<u8>| {
        out.clear();
        for (range, row, step) in &planes {
            let plane = &frame.data[range.clone()];
            let offset = (t as usize * step * row) % plane.len();
            out.extend_from_slice(&plane[offset..]);
            out.extend_from_slice(&plane[..offset]);
        }
    };

    println!(
        "{:<44} {:>9} {:>9} {:>10}",
        "configuração", "média", "p95", "KB/quadro"
    );
    for (label, stats) in
        hext_codec::benchmark(settings, width, height, format, args.frames, image_at)
    {
        match stats {
            Ok(s) => println!(
                "{label:<44} {:>6.2} ms {:>6.2} ms {:>10.1}",
                s.avg_ms, s.p95_ms, s.avg_kb
            ),
            Err(e) => println!("{label:<44} falhou: {e}"),
        }
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn run(_args: &BenchArgs) -> Result<()> {
    anyhow::bail!("o benchmark de codificação ainda só existe no macOS")
}
