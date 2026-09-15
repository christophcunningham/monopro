//! Assemble the PNG-backed chunks of a modern ICNS container.
//!
//! macOS 26.4's `iconutil` rejects even an iconset it just unpacked. The container
//! format needed here is small and stable, so packaging writes it directly instead of
//! making a release depend on that host-tool regression.

use std::{env, fs, io::Write, path::Path};

const CHUNKS: [(&[u8; 4], &str); 11] = [
    (b"icp4", "icon_16x16.png"),
    (b"icp5", "icon_32x32.png"),
    (b"icp6", "icon_32x32@2x.png"),
    (b"ic07", "icon_128x128.png"),
    (b"ic08", "icon_256x256.png"),
    (b"ic09", "icon_512x512.png"),
    (b"ic10", "icon_512x512@2x.png"),
    (b"ic11", "icon_16x16@2x.png"),
    (b"ic12", "icon_32x32@2x.png"),
    (b"ic13", "icon_128x128@2x.png"),
    (b"ic14", "icon_256x256@2x.png"),
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args_os().skip(1);
    let iconset = args.next().ok_or("usage: make_icns ICONSET OUTPUT")?;
    let output = args.next().ok_or("usage: make_icns ICONSET OUTPUT")?;
    if args.next().is_some() {
        return Err("usage: make_icns ICONSET OUTPUT".into());
    }

    let chunks: Vec<(&[u8; 4], Vec<u8>)> = CHUNKS
        .iter()
        .map(|(kind, name)| Ok((*kind, fs::read(Path::new(&iconset).join(name))?)))
        .collect::<Result<_, std::io::Error>>()?;
    let length = 8usize
        + chunks
            .iter()
            .map(|(_, contents)| 8 + contents.len())
            .sum::<usize>();
    let length = u32::try_from(length).map_err(|_| "ICNS container is too large")?;

    let mut file = fs::File::create(output)?;
    file.write_all(b"icns")?;
    file.write_all(&length.to_be_bytes())?;
    for (kind, contents) in chunks {
        let chunk_length =
            u32::try_from(8 + contents.len()).map_err(|_| "ICNS chunk is too large")?;
        file.write_all(kind)?;
        file.write_all(&chunk_length.to_be_bytes())?;
        file.write_all(&contents)?;
    }
    file.sync_all()?;
    Ok(())
}
