//! `bench`: how long video encoding takes on this machine, per encoder
//! configuration, with a real image of the screen scrolling.

use anyhow::Result;

use crate::BenchArgs;

#[cfg(target_os = "macos")]
pub fn run(args: &BenchArgs) -> Result<()> {
    use std::time::{Duration, Instant};

    use anyhow::{Context, bail};
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
        "{width}x{height}, {} quadros por configuração a {} fps, {} Mbps; \
         a imagem da tela rola 8 linhas por quadro.\n",
        args.frames, args.fps, args.bitrate_mbps
    );

    // Frame t: the screenshot scrolled up by 8 rows per frame, wrapping around.
    let row = width as usize * 4;
    let rows = height as usize;
    let image_at = |t: u32, out: &mut Vec<u8>| {
        let offset = (t as usize * 8 % rows) * row;
        out.clear();
        out.extend_from_slice(&frame.data[offset..]);
        out.extend_from_slice(&frame.data[..offset]);
    };

    println!(
        "{:<42} {:>9} {:>9} {:>10}",
        "configuração", "média", "p95", "KB/quadro"
    );
    for (label, stats) in hext_codec::benchmark(settings, width, height, args.frames, image_at) {
        match stats {
            Ok(s) => println!(
                "{label:<42} {:>6.2} ms {:>6.2} ms {:>10.1}",
                s.avg_ms, s.p95_ms, s.avg_kb
            ),
            Err(e) => println!("{label:<42} falhou: {e}"),
        }
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn run(_args: &BenchArgs) -> Result<()> {
    anyhow::bail!("o benchmark de codificação ainda só existe no macOS")
}
