//! Reading the system image: `runo-<board>.img.xz` as released, the zip
//! GitHub Actions gives for a build, or a plain `.img`.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{Context, Result};

use crate::text::t;

const XZ_MAGIC: &[u8] = &[0xfd, b'7', b'z', b'X', b'Z', 0];
const ZIP_MAGIC: &[u8] = b"PK\x03\x04";

/// Writes the image in `path`, uncompressed, to `sink`. `progress` is told
/// how far into the file reading has got, and the file's length.
pub fn decode(path: &Path, sink: &mut dyn Write, progress: &dyn Fn(u64, u64)) -> Result<()> {
    let file = File::open(path)
        .with_context(|| format!("{}: {}", t("cannot open", "打不开"), path.display()))?;
    let total = file.metadata()?.len();
    let mut file = Counted {
        inner: file,
        pos: 0,
        total,
        progress,
    };
    let mut magic = [0u8; 6];
    let n = read_up_to(&mut file, &mut magic)?;
    file.seek(SeekFrom::Start(0))?;
    let magic = &magic[..n];
    if magic.starts_with(ZIP_MAGIC) {
        let mut archive =
            zip::ZipArchive::new(file).context(t("not a readable zip file", "zip 文件读不了"))?;
        let (index, name) = (0..archive.len())
            .filter_map(|i| Some((i, archive.name_for_index(i)?.ok()?.to_string())))
            .find(|(_, name)| name.ends_with(".img.xz") || name.ends_with(".img"))
            .context(t(
                "the zip file has no .img.xz or .img in it",
                "zip 文件里没有 .img.xz 或 .img",
            ))?;
        let entry = archive.by_index(index)?;
        let xz = name.ends_with(".xz");
        let mut entry = BufReader::with_capacity(1 << 20, entry);
        if xz {
            unxz(&mut entry, sink)
        } else {
            copy(&mut entry, sink)
        }
    } else if magic.starts_with(XZ_MAGIC) {
        unxz(&mut BufReader::with_capacity(1 << 20, file), sink)
    } else {
        copy(&mut file, sink)
    }
}

fn unxz(input: &mut dyn io::BufRead, sink: &mut dyn Write) -> Result<()> {
    let mut sink = sink;
    lzma_rs::xz_decompress(&mut BufReaderRef(input), &mut sink).map_err(|err| match err {
        lzma_rs::error::Error::IoError(err) => anyhow::Error::new(err),
        err => anyhow::anyhow!("{}: {err}", t("the image file is damaged", "镜像文件损坏")),
    })
}

fn copy(input: &mut dyn Read, sink: &mut dyn Write) -> Result<()> {
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        sink.write_all(&buf[..n])?;
    }
}

fn read_up_to(input: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match input.read(&mut buf[n..])? {
            0 => break,
            m => n += m,
        }
    }
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            t("the image file is empty", "镜像文件是空的"),
        ));
    }
    Ok(n)
}

/// lzma-rs takes a sized BufRead.
struct BufReaderRef<'a>(&'a mut dyn io::BufRead);

impl Read for BufReaderRef<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl io::BufRead for BufReaderRef<'_> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.0.fill_buf()
    }
    fn consume(&mut self, amt: usize) {
        self.0.consume(amt)
    }
}

/// A file that says where reading has got to.
struct Counted<'a> {
    inner: File,
    pos: u64,
    total: u64,
    progress: &'a dyn Fn(u64, u64),
}

impl Read for Counted<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.pos += n as u64;
        (self.progress)(self.pos, self.total);
        Ok(n)
    }
}

impl Seek for Counted<'_> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.pos = self.inner.seek(pos)?;
        Ok(self.pos)
    }
}

/// Whether `path` looks like a Runo image by its name.
pub fn is_image_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with("runo")
        && (name.ends_with(".img.xz") || name.ends_with(".img") || name.ends_with(".zip"))
        && !name.contains("backup")
}

/// The board a file name says it is for, if any.
pub fn board_in_name(name: &str) -> Option<&'static str> {
    let name = name.to_ascii_lowercase();
    if name.contains("rpi5") {
        Some("rpi5")
    } else if name.contains("rpi4") {
        Some("rpi4")
    } else {
        None
    }
}
