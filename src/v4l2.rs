//! V4L2 structures and ioctl numbers (mirroring `<linux/videodev2.h>` on
//! 64-bit little-endian Linux), plus the capture and loopback devices built
//! on them.

use std::mem::size_of;

pub const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
pub const BUF_TYPE_VIDEO_OUTPUT: u32 = 2;
pub const MEMORY_MMAP: u32 = 1;
pub const FIELD_NONE: u32 = 1;
/// Unpacked 10-bit raw Bayer ('BA10'): one 16-bit little-endian value per
/// pixel. The packed variant (SGRBG10P) triggers a CSI2 receiver bug on every
/// stream start after the first ("Frame sync error" in dmesg).
pub const PIX_FMT_SGRBG10: u32 = u32::from_le_bytes(*b"BA10");
pub const PIX_FMT_YUYV: u32 = u32::from_le_bytes(*b"YUYV");
/// v4l2loopback's private event: fires when a reader starts or stops
/// streaming. `u[0]` is 1 while a reader is streaming, else 0.
pub const EVENT_CLIENT_USAGE: u32 = 0x0800_0000 + 0x08E0_0000 + 1;
pub const EVENT_SUB_FL_SEND_INITIAL: u32 = 1;

#[repr(C)]
pub struct PixFormat {
    pub width: u32,
    pub height: u32,
    pub pixelformat: u32,
    pub field: u32,
    pub bytesperline: u32,
    pub sizeimage: u32,
    pub colorspace: u32,
    pub priv_: u32,
    pub flags: u32,
    pub ycbcr_enc: u32,
    pub quantization: u32,
    pub xfer_func: u32,
}

/// `struct v4l2_format`, with only the `pix` member of the union spelled out.
#[repr(C)]
pub struct Format {
    pub type_: u32,
    _pad: u32,
    pub pix: PixFormat,
    _rest: [u8; 152],
}

#[repr(C)]
pub struct RequestBuffers {
    pub count: u32,
    pub type_: u32,
    pub memory: u32,
    pub capabilities: u32,
    pub flags: u8,
    reserved: [u8; 3],
}

/// `struct v4l2_buffer`. `m` is the union; its low 32 bits are the mmap offset.
#[repr(C)]
pub struct Buffer {
    pub index: u32,
    pub type_: u32,
    pub bytesused: u32,
    pub flags: u32,
    pub field: u32,
    pub tv_sec: i64,
    pub tv_usec: i64,
    timecode: [u32; 4],
    pub sequence: u32,
    pub memory: u32,
    pub m: u64,
    pub length: u32,
    reserved2: u32,
    request_fd: i32,
}

#[repr(C)]
pub struct EventSubscription {
    pub type_: u32,
    pub id: u32,
    pub flags: u32,
    reserved: [u32; 5],
}

#[repr(C)]
pub struct Event {
    pub type_: u32,
    pub u: [u64; 8],
    pub pending: u32,
    pub sequence: u32,
    ts_sec: i64,
    ts_nsec: i64,
    pub id: u32,
    reserved: [u32; 8],
}

// The kernel ABI depends on these sizes; a mismatch must not compile.
const _: () = assert!(size_of::<Format>() == 208);
const _: () = assert!(size_of::<RequestBuffers>() == 20);
const _: () = assert!(size_of::<Buffer>() == 88);
const _: () = assert!(size_of::<EventSubscription>() == 32);
const _: () = assert!(size_of::<Event>() == 136);

const WRITE: u32 = 1;
const READ: u32 = 2;

/// `_IOC(dir, 'V', nr, size)`
const fn ioc(dir: u32, nr: u32, size: usize) -> u32 {
    (dir << 30) | ((size as u32) << 16) | ((b'V' as u32) << 8) | nr
}

pub const VIDIOC_S_FMT: u32 = ioc(READ | WRITE, 5, size_of::<Format>());
pub const VIDIOC_REQBUFS: u32 = ioc(READ | WRITE, 8, size_of::<RequestBuffers>());
pub const VIDIOC_QUERYBUF: u32 = ioc(READ | WRITE, 9, size_of::<Buffer>());
pub const VIDIOC_QBUF: u32 = ioc(READ | WRITE, 15, size_of::<Buffer>());
pub const VIDIOC_DQBUF: u32 = ioc(READ | WRITE, 17, size_of::<Buffer>());
pub const VIDIOC_STREAMON: u32 = ioc(WRITE, 18, size_of::<i32>());
pub const VIDIOC_STREAMOFF: u32 = ioc(WRITE, 19, size_of::<i32>());
pub const VIDIOC_DQEVENT: u32 = ioc(READ, 89, size_of::<Event>());
pub const VIDIOC_SUBSCRIBE_EVENT: u32 = ioc(WRITE, 90, size_of::<EventSubscription>());

#[cfg(target_os = "linux")]
pub use device::{Capture, Loopback};

#[cfg(target_os = "linux")]
mod device {
    use super::*;
    use crate::config::Config;
    use crate::media::Sensor;
    use std::fs::{File, OpenOptions};
    use std::io::{self, Write};
    use std::os::fd::{AsRawFd, RawFd};
    use std::os::unix::fs::OpenOptionsExt;

    /// All the structs above are plain integers, for which all-zero is valid.
    fn zeroed<T>() -> T {
        unsafe { std::mem::zeroed() }
    }

    fn context(e: io::Error, what: impl std::fmt::Display) -> io::Error {
        io::Error::new(e.kind(), format!("{what}: {e}"))
    }

    fn ioctl<T>(fd: RawFd, request: u32, arg: &mut T, name: &str) -> io::Result<()> {
        loop {
            // The request parameter is c_ulong on glibc and c_int on musl.
            if unsafe { libc::ioctl(fd, request as _, arg as *mut T) } == 0 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(context(e, name));
            }
        }
    }

    /// Wait for `events` on `fd`. False on timeout or when a signal arrived.
    fn wait(fd: RawFd, events: libc::c_short, timeout_ms: i32) -> io::Result<bool> {
        let mut pfd = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        match unsafe { libc::poll(&mut pfd, 1, timeout_ms) } {
            n if n > 0 => Ok(true),
            0 => Ok(false),
            _ => match io::Error::last_os_error() {
                e if e.kind() == io::ErrorKind::Interrupted => Ok(false),
                e => Err(context(e, "poll")),
            },
        }
    }

    fn open(path: &str) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .map_err(|e| context(e, format_args!("cannot open {path}")))
    }

    /// The IPU6 ISYS capture node, streaming raw Bayer frames via mmap.
    pub struct Capture {
        file: File,
        maps: Vec<(*mut libc::c_void, usize)>,
        /// Bytes per row of a raw frame, padding included.
        pub stride: usize,
    }

    impl Capture {
        /// Open the device, program the sensor and media pipeline, and start
        /// streaming. The pipeline is configured before buffers are
        /// requested; doing it afterwards breaks STREAMON on later runs.
        pub fn open(cfg: &Config, sensor: &Sensor, analogue_gain: i32) -> io::Result<Self> {
            let file = open(&cfg.capture_device)?;
            let fd = file.as_raw_fd();
            sensor.set_controls(&format!(
                "analogue_gain={analogue_gain},digital_gain={}",
                cfg.digital_gain
            ))?;
            sensor.setup_pipeline(cfg)?;

            let mut fmt: Format = zeroed();
            fmt.type_ = BUF_TYPE_VIDEO_CAPTURE;
            fmt.pix.width = cfg.sensor_width as u32;
            fmt.pix.height = cfg.sensor_height as u32;
            fmt.pix.pixelformat = PIX_FMT_SGRBG10;
            fmt.pix.field = FIELD_NONE;
            ioctl(fd, VIDIOC_S_FMT, &mut fmt, "VIDIOC_S_FMT")?;
            info!(
                "Capture format: {}x{} SGRBG10, stride {}, {} bytes/frame",
                fmt.pix.width, fmt.pix.height, fmt.pix.bytesperline, fmt.pix.sizeimage
            );
            if (fmt.pix.width, fmt.pix.height)
                != (cfg.sensor_width as u32, cfg.sensor_height as u32)
            {
                return Err(io::Error::other(format!(
                    "{} gave {}x{} instead of the requested {}x{}",
                    cfg.capture_device,
                    fmt.pix.width,
                    fmt.pix.height,
                    cfg.sensor_width,
                    cfg.sensor_height
                )));
            }

            // From here on Drop unmaps whatever was mapped if a step fails.
            let mut cam = Self {
                file,
                maps: Vec::new(),
                stride: fmt.pix.bytesperline as usize,
            };

            let mut req: RequestBuffers = zeroed();
            req.count = cfg.num_buffers;
            req.type_ = BUF_TYPE_VIDEO_CAPTURE;
            req.memory = MEMORY_MMAP;
            ioctl(fd, VIDIOC_REQBUFS, &mut req, "VIDIOC_REQBUFS")?;

            for index in 0..req.count {
                let mut buf: Buffer = zeroed();
                buf.index = index;
                buf.type_ = BUF_TYPE_VIDEO_CAPTURE;
                buf.memory = MEMORY_MMAP;
                ioctl(fd, VIDIOC_QUERYBUF, &mut buf, "VIDIOC_QUERYBUF")?;
                let len = buf.length as usize;
                let ptr = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        len,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_SHARED,
                        fd,
                        (buf.m as u32) as libc::off_t,
                    )
                };
                if ptr == libc::MAP_FAILED {
                    return Err(context(io::Error::last_os_error(), "mmap"));
                }
                cam.maps.push((ptr, len));
                ioctl(fd, VIDIOC_QBUF, &mut buf, "VIDIOC_QBUF")?;
            }

            let mut buf_type = BUF_TYPE_VIDEO_CAPTURE as i32;
            ioctl(fd, VIDIOC_STREAMON, &mut buf_type, "VIDIOC_STREAMON")?;
            info!("Streaming started on {}", cfg.capture_device);
            Ok(cam)
        }

        /// Wait up to 2 s for a raw frame and hand it to `f`. `None` on
        /// timeout. The buffer goes back to the kernel when `f` returns.
        pub fn frame<R>(&mut self, f: impl FnOnce(&[u8]) -> R) -> io::Result<Option<R>> {
            let fd = self.file.as_raw_fd();
            if !wait(fd, libc::POLLIN, 2000)? {
                return Ok(None);
            }
            let mut buf: Buffer = zeroed();
            buf.type_ = BUF_TYPE_VIDEO_CAPTURE;
            buf.memory = MEMORY_MMAP;
            if let Err(e) = ioctl(fd, VIDIOC_DQBUF, &mut buf, "VIDIOC_DQBUF") {
                return match e.kind() {
                    io::ErrorKind::WouldBlock => Ok(None),
                    _ => Err(e),
                };
            }
            let result = self.maps.get(buf.index as usize).map(|&(ptr, len)| {
                let used = (buf.bytesused as usize).min(len);
                f(unsafe { std::slice::from_raw_parts(ptr as *const u8, used) })
            });
            ioctl(fd, VIDIOC_QBUF, &mut buf, "VIDIOC_QBUF")?;
            Ok(result)
        }
    }

    impl Drop for Capture {
        fn drop(&mut self) {
            let mut buf_type = BUF_TYPE_VIDEO_CAPTURE as i32;
            // Fails harmlessly if streaming never started.
            let _ = ioctl(
                self.file.as_raw_fd(),
                VIDIOC_STREAMOFF,
                &mut buf_type,
                "VIDIOC_STREAMOFF",
            );
            for &(ptr, len) in &self.maps {
                unsafe { libc::munmap(ptr, len) };
            }
        }
    }

    /// The v4l2loopback device, fed YUYV frames with plain `write()`.
    pub struct Loopback {
        file: File,
    }

    impl Loopback {
        pub fn open(cfg: &Config) -> io::Result<Self> {
            let file = open(&cfg.loopback_device)?;
            let (w, h) = (cfg.output_width as u32, cfg.output_height as u32);
            let mut fmt: Format = zeroed();
            fmt.type_ = BUF_TYPE_VIDEO_OUTPUT;
            fmt.pix.width = w;
            fmt.pix.height = h;
            fmt.pix.pixelformat = PIX_FMT_YUYV;
            fmt.pix.field = FIELD_NONE;
            fmt.pix.bytesperline = w * 2;
            fmt.pix.sizeimage = w * h * 2;
            ioctl(file.as_raw_fd(), VIDIOC_S_FMT, &mut fmt, "VIDIOC_S_FMT")
                .map_err(|e| context(e, &cfg.loopback_device))?;
            // v4l2loopback answers with the existing format instead when
            // another process already has the device configured.
            if (fmt.pix.width, fmt.pix.height, fmt.pix.pixelformat) != (w, h, PIX_FMT_YUYV) {
                return Err(io::Error::other(format!(
                    "{} is locked to another format ({}x{}) by a process that has it open; \
                     close the apps using the camera and retry",
                    cfg.loopback_device, fmt.pix.width, fmt.pix.height
                )));
            }
            Ok(Self { file })
        }

        /// Write one frame. With `exclusive_caps=1` the first write is what
        /// makes the device show up as a camera in other applications.
        pub fn write(&self, frame: &[u8]) -> io::Result<()> {
            let n = (&self.file)
                .write(frame)
                .map_err(|e| context(e, "writing to loopback device"))?;
            if n != frame.len() {
                return Err(io::Error::other(format!(
                    "loopback device took {n} of {} bytes",
                    frame.len()
                )));
            }
            Ok(())
        }

        /// Ask to be told when readers start and stop streaming. False if
        /// this v4l2loopback build has no such event.
        pub fn subscribe(&self) -> bool {
            let mut sub: EventSubscription = zeroed();
            sub.type_ = EVENT_CLIENT_USAGE;
            sub.flags = EVENT_SUB_FL_SEND_INITIAL;
            let fd = self.file.as_raw_fd();
            ioctl(
                fd,
                VIDIOC_SUBSCRIBE_EVENT,
                &mut sub,
                "VIDIOC_SUBSCRIBE_EVENT",
            )
            .is_ok()
        }

        /// Wait up to `timeout_ms` for a reader to start or stop streaming.
        /// Returns the latest state (true = someone is reading) if it changed.
        pub fn reader_change(&self, timeout_ms: i32) -> io::Result<Option<bool>> {
            let fd = self.file.as_raw_fd();
            if !wait(fd, libc::POLLPRI, timeout_ms)? {
                return Ok(None);
            }
            let mut latest = None;
            let mut ev: Event = zeroed();
            // The fd is non-blocking, so this stops once the queue is empty.
            while ioctl(fd, VIDIOC_DQEVENT, &mut ev, "VIDIOC_DQEVENT").is_ok() {
                if ev.type_ == EVENT_CLIENT_USAGE {
                    latest = Some(ev.u[0] as u32 != 0);
                }
            }
            Ok(latest)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_kernel_values() {
        assert_eq!(VIDIOC_S_FMT, 0xC0D0_5605);
        assert_eq!(VIDIOC_REQBUFS, 0xC014_5608);
        assert_eq!(VIDIOC_QUERYBUF, 0xC058_5609);
        assert_eq!(VIDIOC_QBUF, 0xC058_560F);
        assert_eq!(VIDIOC_DQBUF, 0xC058_5611);
        assert_eq!(VIDIOC_STREAMON, 0x4004_5612);
        assert_eq!(VIDIOC_STREAMOFF, 0x4004_5613);
        assert_eq!(VIDIOC_SUBSCRIBE_EVENT, 0x4020_565A);
        assert_eq!(VIDIOC_DQEVENT, 0x8088_5659);
        assert_eq!(EVENT_CLIENT_USAGE, 0x10E0_0001);
        assert_eq!(PIX_FMT_SGRBG10, 0x3031_4142);
        assert_eq!(PIX_FMT_YUYV, 0x5659_5559);
    }
}
