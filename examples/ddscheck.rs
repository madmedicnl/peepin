//! Decode one .dds and print what came out. `cargo run --release --example ddscheck <file>`

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: ddscheck <file.dds>");
        std::process::exit(2);
    };
    let file = std::fs::File::open(&path).expect("open");
    let mut reader = std::io::BufReader::new(file);
    let dds = ddsfile::Dds::read(&mut reader).expect("read header");
    println!(
        "header: {}x{}, mips {}",
        dds.get_width(),
        dds.get_height(),
        dds.get_num_mipmap_levels()
    );
    match image_dds::image_from_dds(&dds, 0) {
        Ok(image) => println!("rgba8: {}x{}", image.width(), image.height()),
        Err(err) => println!("decode failed: {err:?}"),
    }
    // Report the last mip too, since Fallout's UI textures often only differ there.
    let last = dds.get_num_mipmap_levels().saturating_sub(1);
    match image_dds::image_from_dds(&dds, last) {
        Ok(image) => println!("last mip {last}: {}x{}", image.width(), image.height()),
        Err(err) => println!("last mip {last} failed: {err:?}"),
    }
}
