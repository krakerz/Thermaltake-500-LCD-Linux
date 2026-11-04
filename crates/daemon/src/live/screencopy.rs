//! A wlr-screencopy client in safe Rust: captures one output into a memfd-backed shm buffer.
//!
//! The buffer is read back with `read_exact_at` instead of being mapped, which keeps the
//! crate free of `unsafe`.

use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::path::Path;

use image::RgbImage;
use rustix::fs::{MemfdFlags, memfd_create};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_shm::{self, Format, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::{
    self, Flags, ZwlrScreencopyFrameV1,
};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;

use super::capture::{CaptureError, Capturer};

/// First screencopy version that sends `buffer_done` after the buffer events.
const BUFFER_DONE_VERSION: u32 = 3;
/// `wl_output` version that has the `name` event.
const OUTPUT_NAME_VERSION: u32 = 4;
/// Bytes per pixel of the supported 32-bit shm formats.
const BYTES_PER_PIXEL: usize = 4;

/// The shm pixel layouts we can convert (little-endian byte order in memory).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PixelFormat {
    /// `Xrgb8888` / `Argb8888`: bytes B, G, R, X.
    Bgrx,
    /// `Xbgr8888` / `Abgr8888`: bytes R, G, B, X.
    Rgbx,
}

impl PixelFormat {
    fn from_shm(format: Format) -> Option<Self> {
        match format {
            Format::Xrgb8888 | Format::Argb8888 => Some(Self::Bgrx),
            Format::Xbgr8888 | Format::Abgr8888 => Some(Self::Rgbx),
            _ => None,
        }
    }
}

/// Converts a captured shm buffer to RGB, flipping vertically when `y_invert` is set.
///
/// # Panics
///
/// Panics when `bytes` is shorter than `stride * height` or `stride < width * 4`.
fn to_rgb(
    bytes: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    format: PixelFormat,
    y_invert: bool,
) -> RgbImage {
    let (w, h) = (width as usize, height as usize);
    assert!(
        stride >= w * BYTES_PER_PIXEL && bytes.len() >= stride * h,
        "buffer too small: {} bytes for {w}x{h} with stride {stride}",
        bytes.len()
    );
    let mut out = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        let src_row = if y_invert { h - 1 - y } else { y };
        let row = &bytes[src_row * stride..src_row * stride + w * BYTES_PER_PIXEL];
        for px in row.chunks_exact(BYTES_PER_PIXEL) {
            match format {
                PixelFormat::Bgrx => out.extend_from_slice(&[px[2], px[1], px[0]]),
                PixelFormat::Rgbx => out.extend_from_slice(&px[..3]),
            }
        }
    }
    RgbImage::from_raw(width, height, out).expect("length is width * height * 3")
}

/// What the compositor asked for the current frame.
#[derive(Debug, Clone, Copy)]
struct BufferSpec {
    format: Format,
    width: u32,
    height: u32,
    stride: u32,
}

#[derive(Debug, Default)]
struct FrameState {
    spec: Option<BufferSpec>,
    /// Set once the buffer parameters are complete and the client may call `copy`.
    copy_ready: bool,
    y_invert: bool,
    ready: bool,
    failed: bool,
}

#[derive(Debug)]
struct OutputInfo {
    output: WlOutput,
    name: Option<String>,
}

#[derive(Debug, Default)]
struct State {
    outputs: Vec<OutputInfo>,
    frame: FrameState,
}

/// The shm buffer and the file backing it; recreated when the format or size changes.
#[derive(Debug)]
struct ShmBuffer {
    spec: BufferSpec,
    file: File,
    buffer: WlBuffer,
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
    }
}

/// Captures one Wayland output through `zwlr_screencopy_manager_v1`.
#[derive(Debug)]
pub struct Screencopy {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    shm: WlShm,
    manager: ZwlrScreencopyManagerV1,
    output: WlOutput,
    buffer: Option<ShmBuffer>,
    pixels: Vec<u8>,
}

impl Screencopy {
    /// Connects to the compositor at `socket` and picks `output` by name, or the first one.
    ///
    /// # Errors
    ///
    /// [`CaptureError::Failed`] when the socket can't be reached, the compositor lacks
    /// `wl_shm` or wlr-screencopy, or the named output doesn't exist.
    pub fn connect(socket: &Path, output: Option<&str>) -> Result<Self, CaptureError> {
        let stream = UnixStream::connect(socket)
            .map_err(|e| CaptureError::Failed(format!("connect {}: {e}", socket.display())))?;
        let conn = Connection::from_socket(stream)
            .map_err(|e| CaptureError::Failed(format!("wayland handshake: {e}")))?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn)
            .map_err(|e| CaptureError::Failed(format!("wayland registry: {e}")))?;
        let qh = queue.handle();
        let shm: WlShm = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| CaptureError::Failed(format!("compositor has no wl_shm: {e}")))?;
        let manager: ZwlrScreencopyManagerV1 = globals
            .bind(&qh, 1..=3, ())
            .map_err(|e| CaptureError::Failed(format!("compositor lacks wlr-screencopy: {e}")))?;

        let mut state = State::default();
        let advertised: Vec<(u32, u32)> = globals.contents().with_list(|list| {
            list.iter()
                .filter(|g| g.interface == WlOutput::interface().name)
                .map(|g| (g.name, g.version))
                .collect()
        });
        for (name, version) in advertised {
            let version = version.min(OUTPUT_NAME_VERSION);
            let output = globals
                .registry()
                .bind::<WlOutput, _, _>(name, version, &qh, ());
            state.outputs.push(OutputInfo { output, name: None });
        }
        // One roundtrip delivers the `name` events.
        queue
            .roundtrip(&mut state)
            .map_err(|e| CaptureError::Failed(format!("wayland roundtrip: {e}")))?;

        let chosen = pick_output(&state.outputs, output)?;
        Ok(Self {
            conn,
            queue,
            shm,
            manager,
            output: chosen,
            state,
            buffer: None,
            pixels: Vec::new(),
        })
    }

    /// Reuses the shm buffer when it still matches the compositor's request, else rebuilds it.
    fn ensure_buffer(&mut self, spec: BufferSpec) -> Result<(), CaptureError> {
        if self.buffer.as_ref().is_some_and(|b| {
            b.spec.format == spec.format
                && b.spec.width == spec.width
                && b.spec.height == spec.height
                && b.spec.stride == spec.stride
        }) {
            return Ok(());
        }
        self.buffer = None;
        let len = u64::from(spec.stride) * u64::from(spec.height);
        let size = i32::try_from(len)
            .map_err(|_| CaptureError::Failed(format!("buffer too large: {len} bytes")))?;
        let fd = memfd_create("tt500-screencopy", MemfdFlags::CLOEXEC)
            .map_err(|e| CaptureError::Failed(format!("memfd_create: {e}")))?;
        let file = File::from(fd);
        file.set_len(len)
            .map_err(|e| CaptureError::Failed(format!("sizing shm file: {e}")))?;
        let qh = self.queue.handle();
        let pool = self.shm.create_pool(file.as_fd(), size, &qh, ());
        let buffer = pool.create_buffer(
            0,
            to_i32(spec.width)?,
            to_i32(spec.height)?,
            to_i32(spec.stride)?,
            spec.format,
            &qh,
            (),
        );
        pool.destroy();
        self.buffer = Some(ShmBuffer { spec, file, buffer });
        Ok(())
    }

    fn capture_frame(&mut self) -> Result<RgbImage, CaptureError> {
        self.state.frame = FrameState::default();
        let qh = self.queue.handle();
        let frame = self.manager.capture_output(0, &self.output, &qh, ());
        let outcome = self.drive_frame(&frame);
        frame.destroy();
        outcome?;

        let spec = self
            .state
            .frame
            .spec
            .ok_or_else(|| CaptureError::Failed("compositor sent no buffer spec".to_owned()))?;
        let format = PixelFormat::from_shm(spec.format).ok_or_else(|| {
            CaptureError::Failed(format!("unsupported pixel format {:?}", spec.format))
        })?;
        let buffer = self
            .buffer
            .as_ref()
            .ok_or_else(|| CaptureError::Failed("no shm buffer".to_owned()))?;
        let len = spec.stride as usize * spec.height as usize;
        self.pixels.resize(len, 0);
        buffer
            .file
            .read_exact_at(&mut self.pixels, 0)
            .map_err(|e| CaptureError::Failed(format!("reading shm buffer: {e}")))?;
        Ok(to_rgb(
            &self.pixels,
            spec.width,
            spec.height,
            spec.stride as usize,
            format,
            self.state.frame.y_invert,
        ))
    }

    /// Pumps events until the frame is `ready`, issuing `copy` once the buffer spec is known.
    fn drive_frame(&mut self, frame: &ZwlrScreencopyFrameV1) -> Result<(), CaptureError> {
        let mut copied = false;
        loop {
            if self.state.frame.failed {
                return Err(CaptureError::Gone("screencopy frame failed".to_owned()));
            }
            if self.state.frame.ready {
                return Ok(());
            }
            if self.state.frame.copy_ready && !copied {
                let spec = self.state.frame.spec.ok_or_else(|| {
                    CaptureError::Failed("compositor sent no buffer spec".to_owned())
                })?;
                if PixelFormat::from_shm(spec.format).is_none() {
                    return Err(CaptureError::Failed(format!(
                        "unsupported pixel format {:?}",
                        spec.format
                    )));
                }
                self.ensure_buffer(spec)?;
                let buffer = self.buffer.as_ref().map(|b| &b.buffer);
                if let Some(buffer) = buffer {
                    frame.copy(buffer);
                }
                self.conn
                    .flush()
                    .map_err(|e| CaptureError::Gone(format!("flush: {e}")))?;
                copied = true;
            }
            self.queue
                .blocking_dispatch(&mut self.state)
                .map_err(|e| CaptureError::Gone(format!("dispatch: {e}")))?;
        }
    }
}

impl Capturer for Screencopy {
    fn capture(&mut self) -> Result<RgbImage, CaptureError> {
        self.capture_frame()
    }
}

fn to_i32(value: u32) -> Result<i32, CaptureError> {
    i32::try_from(value).map_err(|_| CaptureError::Failed(format!("value too large: {value}")))
}

fn pick_output(outputs: &[OutputInfo], wanted: Option<&str>) -> Result<WlOutput, CaptureError> {
    let found = match wanted {
        Some(name) => outputs.iter().find(|o| o.name.as_deref() == Some(name)),
        None => outputs.first(),
    };
    found.map(|o| o.output.clone()).ok_or_else(|| {
        let known: Vec<&str> = outputs.iter().filter_map(|o| o.name.as_deref()).collect();
        match wanted {
            Some(name) => {
                CaptureError::Failed(format!("output {name} not found (have: {known:?})"))
            }
            None => CaptureError::Failed("compositor has no outputs".to_owned()),
        }
    })
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlShm, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlShm,
        _: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlShmPool, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlShmPool,
        _: <WlShmPool as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlBuffer, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlBuffer,
        _: <WlBuffer as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrScreencopyManagerV1,
        _: <ZwlrScreencopyManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event
            && let Some(info) = state.outputs.iter_mut().find(|o| &o.output == output)
        {
            info.name = Some(name);
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        frame: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let current = &mut state.frame;
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                // Unknown formats stay `None` and are reported once the frame is driven.
                current.spec = match format {
                    WEnum::Value(format) => Some(BufferSpec {
                        format,
                        width,
                        height,
                        stride,
                    }),
                    WEnum::Unknown(_) => current.spec,
                };
                if frame.version() < BUFFER_DONE_VERSION {
                    current.copy_ready = true;
                }
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => current.copy_ready = true,
            zwlr_screencopy_frame_v1::Event::Flags { flags } => {
                current.y_invert = matches!(flags, WEnum::Value(f) if f.contains(Flags::YInvert));
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => current.ready = true,
            zwlr_screencopy_frame_v1::Event::Failed => current.failed = true,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Two pixels per row, stride padded to 12 bytes.
    const BGRX: [u8; 24] = [
        1, 2, 3, 0, 4, 5, 6, 0, 99, 99, 99, 99, //
        7, 8, 9, 0, 10, 11, 12, 0, 99, 99, 99, 99,
    ];

    #[test]
    fn converts_bgrx_and_ignores_padding() {
        let img = to_rgb(&BGRX, 2, 2, 12, PixelFormat::Bgrx, false);
        assert_eq!(img.as_raw(), &[3, 2, 1, 6, 5, 4, 9, 8, 7, 12, 11, 10]);
    }

    #[test]
    fn converts_rgbx() {
        let img = to_rgb(&BGRX, 2, 2, 12, PixelFormat::Rgbx, false);
        assert_eq!(img.as_raw(), &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
    }

    #[test]
    fn y_invert_flips_rows() {
        let img = to_rgb(&BGRX, 2, 2, 12, PixelFormat::Rgbx, true);
        assert_eq!(img.as_raw(), &[7, 8, 9, 10, 11, 12, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    #[should_panic(expected = "buffer too small")]
    fn short_buffer_panics_with_context() {
        let _ = to_rgb(&BGRX[..10], 2, 2, 12, PixelFormat::Bgrx, false);
    }

    #[test]
    fn ten_bit_formats_are_unsupported() {
        assert_eq!(PixelFormat::from_shm(Format::Xrgb2101010), None);
        assert_eq!(
            PixelFormat::from_shm(Format::Argb8888),
            Some(PixelFormat::Bgrx)
        );
        assert_eq!(
            PixelFormat::from_shm(Format::Abgr8888),
            Some(PixelFormat::Rgbx)
        );
    }

    #[test]
    fn capturer_is_send() {
        const fn assert_send<T: Send>() {}
        assert_send::<Screencopy>();
    }
}
