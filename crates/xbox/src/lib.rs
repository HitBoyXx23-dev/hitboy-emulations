//! Original Xbox (2001). Disc images (XISO / full Redump ISO), XBE executables, and the
//! machine built on our Pentium III core (`cpui686`).

pub use pchw as chips;
pub mod machine;
pub mod nv2a;
pub mod pci;
pub mod smbus;
pub mod xbe;
pub mod xiso;

pub use machine::Xbox;

pub use cpui686 as cpu;

/// Whether a file is an Xbox disc image (checks for the XDVDFS volume, no full read).
pub fn is_xbox_disc(path: &std::path::Path) -> bool {
    std::fs::File::open(path).is_ok_and(|mut f| xiso::Disc::open(&mut f).is_ok())
}

/// Reads a disc image and returns the game's default.xbe parsed.
pub fn read_disc<I: xiso::Image + ?Sized>(img: &mut I) -> Result<(xiso::Disc, xbe::Xbe), String> {
    let disc = xiso::Disc::open(img)?;
    let data = disc.read_file(img, "default.xbe").ok_or("This disc has no default.xbe.")?;
    let x = xbe::parse(&data)?;
    Ok((disc, x))
}

#[cfg(test)]
mod tests {
    #[test]
    fn disc_to_xbe() {
        let mut img = crate::xiso::tests::build(&crate::xbe::tests::build("Halo Test", &[0xF4]));
        let (_, x) = crate::read_disc(&mut img).unwrap();
        assert_eq!(x.title, "Halo Test");
    }
}
