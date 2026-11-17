//! Renders the previews printpit makes for a sliced G-code file, to look at them while
//! working on `slicer::preview`:
//!
//!     cargo run --release --example render_preview -- part.gcode out-dir/
//!
//! writes `plate.png`, `top.png`, `thumb-WxH.png` for each thumbnail size, and `with-thumbnails.gcode`
//! (the G-code as printpit uploads it).

fn main() -> std::io::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let (Some(src), Some(dir)) = (args.next(), args.next()) else {
        eprintln!("usage: render_preview <file.gcode> <out dir>");
        std::process::exit(2);
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir)?;
    let start = std::time::Instant::now();
    let Some(r) = printpit::slicer::preview::render(src.as_ref())? else {
        eprintln!("nothing to draw");
        std::process::exit(1);
    };
    eprintln!("rendered in {:.2?}", start.elapsed());
    for (w, h, png) in &r.thumbnails {
        std::fs::write(dir.join(format!("thumb-{w}x{h}.png")), png)?;
    }
    if let Some(p) = &r.plate {
        std::fs::write(dir.join("plate.png"), p)?;
    }
    if let Some(p) = &r.top {
        std::fs::write(dir.join("top.png"), p)?;
    }
    let block = printpit::slicer::preview::thumbnail_block(&r.thumbnails);
    let mut out = std::io::BufWriter::new(std::fs::File::create(dir.join("with-thumbnails.gcode"))?);
    if !printpit::slicer::preview::inject(src.as_ref(), &mut out, block.as_bytes())? {
        eprintln!("the G-code already has thumbnails");
    }
    Ok(())
}
