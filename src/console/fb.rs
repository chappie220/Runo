//! The Linux framebuffer (`/dev/fb0`) and the virtual terminal's graphics
//! mode, which keeps the kernel's console from drawing over it.

use std::fs::{File, OpenOptions};
use std::ops::Range;
use std::os::fd::{AsRawFd, IntoRawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};

use anyhow::{Context, Result, bail};

use super::render::{Canvas, Format};

const FBIOGET_VSCREENINFO: u32 = 0x4600;
const FBIOGET_FSCREENINFO: u32 = 0x4602;
const KDGETMODE: u32 = 0x4b3b;
const KDSETMODE: u32 = 0x4b3a;
const KD_TEXT: libc::c_int = 0;
const KD_GRAPHICS: libc::c_int = 1;

#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Bitfield {
    offset: u32,
    length: u32,
    msb_right: u32,
}

/// `struct fb_var_screeninfo`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VarInfo {
    xres: u32,
    yres: u32,
    xres_virtual: u32,
    yres_virtual: u32,
    xoffset: u32,
    yoffset: u32,
    bits_per_pixel: u32,
    grayscale: u32,
    red: Bitfield,
    green: Bitfield,
    blue: Bitfield,
    transp: Bitfield,
    nonstd: u32,
    activate: u32,
    height: u32,
    width: u32,
    accel_flags: u32,
    pixclock: u32,
    left_margin: u32,
    right_margin: u32,
    upper_margin: u32,
    lower_margin: u32,
    hsync_len: u32,
    vsync_len: u32,
    sync: u32,
    vmode: u32,
    rotate: u32,
    colorspace: u32,
    reserved: [u32; 4],
}

/// `struct fb_fix_screeninfo`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FixInfo {
    id: [u8; 16],
    smem_start: libc::c_ulong,
    smem_len: u32,
    kind: u32,
    type_aux: u32,
    visual: u32,
    xpanstep: u16,
    ypanstep: u16,
    ywrapstep: u16,
    line_length: u32,
    mmio_start: libc::c_ulong,
    mmio_len: u32,
    accel: u32,
    capabilities: u16,
    reserved: [u16; 2],
}

/// What a mode change is noticed by.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Mode {
    width: u32,
    height: u32,
    bits: u32,
    stride: u32,
    offset: (u32, u32),
}

pub struct Framebuffer {
    path: PathBuf,
    file: File,
    map: *mut u8,
    len: usize,
    mode: Mode,
    format: Format,
    /// Where the visible screen starts in the mapping.
    start: usize,
}

fn info(file: &File) -> Result<(VarInfo, FixInfo)> {
    let mut var = VarInfo::default();
    let mut fix = FixInfo::default();
    // SAFETY: both ioctls fill the struct they are given.
    unsafe {
        if libc::ioctl(file.as_raw_fd(), FBIOGET_VSCREENINFO as _, &mut var) < 0
            || libc::ioctl(file.as_raw_fd(), FBIOGET_FSCREENINFO as _, &mut fix) < 0
        {
            return Err(std::io::Error::last_os_error()).context("reading the screen's mode");
        }
    }
    Ok((var, fix))
}

fn mode(var: &VarInfo, fix: &FixInfo) -> Mode {
    Mode {
        width: var.xres,
        height: var.yres,
        bits: var.bits_per_pixel,
        stride: fix.line_length,
        offset: (var.xoffset, var.yoffset),
    }
}

impl Framebuffer {
    pub fn open(path: &Path) -> Result<Framebuffer> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        let (var, fix) = info(&file)?;
        let field = |b: Bitfield| (b.offset, b.length);
        let format = match var.bits_per_pixel {
            32 | 24 | 16 if var.red.length > 0 => Format {
                bytes: var.bits_per_pixel as usize / 8,
                red: field(var.red),
                green: field(var.green),
                blue: field(var.blue),
                alpha: field(var.transp),
            },
            32 => Format::XRGB8888,
            16 => Format::RGB565,
            bits => bail!(
                "{} uses {bits} bits per pixel; 16 or 32 are drawn",
                path.display()
            ),
        };
        let mode = mode(&var, &fix);
        if mode.width == 0 || mode.height == 0 {
            bail!("{} reports no screen", path.display());
        }
        let start =
            mode.offset.1 as usize * mode.stride as usize + mode.offset.0 as usize * format.bytes;
        let needed = start + mode.height as usize * mode.stride as usize;
        let len = (fix.smem_len as usize).max(needed);
        // SAFETY: maps the device's memory; the length is what it reports.
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if map == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("mapping {}", path.display()));
        }
        Ok(Framebuffer {
            path: path.to_owned(),
            file,
            map: map.cast(),
            len,
            mode,
            format,
            start,
        })
    }

    pub fn width(&self) -> usize {
        self.mode.width as usize
    }

    pub fn height(&self) -> usize {
        self.mode.height as usize
    }

    /// Opens the device again if its mode changed (a different screen);
    /// true when it did.
    pub fn reopen_if_changed(&mut self) -> Result<bool> {
        let (var, fix) = info(&self.file)?;
        if mode(&var, &fix) == self.mode {
            return Ok(false);
        }
        *self = Framebuffer::open(&self.path)?;
        Ok(true)
    }

    /// Copies `rows` of `canvas` to the screen.
    pub fn present(&mut self, canvas: &Canvas, rows: &[Range<usize>]) {
        let width = canvas.width.min(self.width());
        let stride = self.mode.stride as usize;
        let bytes = width * self.format.bytes;
        for range in rows {
            for y in range.start..range.end.min(self.height()).min(canvas.height) {
                let at = self.start + y * stride;
                if at + bytes > self.len {
                    return;
                }
                // SAFETY: inside the mapping, checked above.
                let dst = unsafe { std::slice::from_raw_parts_mut(self.map.add(at), bytes) };
                let src = &canvas.pixels[y * canvas.width..y * canvas.width + width];
                self.format.write_row(src, dst);
            }
        }
    }
}

impl Drop for Framebuffer {
    fn drop(&mut self) {
        // SAFETY: unmaps what `open` mapped.
        unsafe { libc::munmap(self.map.cast(), self.len) };
    }
}

/// The terminal put in graphics mode, put back in text mode on restore.
static VT: AtomicI32 = AtomicI32::new(-1);

/// Puts the console's virtual terminal (standard input, else `/dev/tty`)
/// in graphics mode.
pub fn graphics_mode() -> Result<()> {
    let is_vt = |fd: libc::c_int| {
        let mut mode: libc::c_int = 0;
        // SAFETY: KDGETMODE writes an int.
        unsafe { libc::ioctl(fd, KDGETMODE as _, &mut mode) == 0 }
    };
    let fd = if is_vt(0) {
        0
    } else {
        let tty = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .context("opening the terminal")?;
        if !is_vt(tty.as_raw_fd()) {
            bail!("the terminal is not a virtual console");
        }
        // Kept open for restoring.
        tty.into_raw_fd()
    };
    // SAFETY: KDSETMODE takes the mode as its argument.
    if unsafe { libc::ioctl(fd, KDSETMODE as _, KD_GRAPHICS) } < 0 {
        return Err(std::io::Error::last_os_error()).context("setting graphics mode");
    }
    VT.store(fd, Ordering::SeqCst);
    Ok(())
}

/// Gives the screen back to the kernel's console. Safe to call from a
/// panic hook, and more than once.
pub fn text_mode() {
    let fd = VT.swap(-1, Ordering::SeqCst);
    if fd >= 0 {
        // SAFETY: KDSETMODE takes the mode as its argument.
        unsafe { libc::ioctl(fd, KDSETMODE as _, KD_TEXT) };
    }
}
