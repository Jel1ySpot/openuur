//! Screencopy capture for the native wlroots backend.
//!
//! Frames land in `wl_shm` buffers that are reused while the compositor has
//! released them. A frame is published only after `ready`; the buffer stays
//! busy until the compositor sends `release`.

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use wayland_client::protocol::{wl_buffer, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};

use super::protocol::{FrameEvent, FrameFlags, ZwlrScreencopyFrameV1};
use super::WlrootsState;
use crate::desktop::CapturedFrame;

/// Buffer release flag handed to the compositor.
pub(super) struct BufferData {
    busy: Arc<AtomicBool>,
}

/// Per-frame data passed to the screencopy frame dispatcher.
pub(super) struct FrameData {
    pub(super) output_id: u32,
}

/// A sealed memory file mapped into this process and shared with the
/// compositor through `wl_shm`.
struct ShmMap {
    fd: OwnedFd,
    ptr: *mut u8,
    len: usize,
}

impl ShmMap {
    fn new(len: usize) -> Result<Self, String> {
        let name = c"openuur-wlroots";
        let raw = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        if raw < 0 {
            return Err(format!(
                "cannot create the screencopy buffer: {}",
                std::io::Error::last_os_error()
            ));
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } < 0 {
            return Err(format!(
                "cannot size the screencopy buffer: {}",
                std::io::Error::last_os_error()
            ));
        }
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(format!(
                "cannot map the screencopy buffer: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self {
            fd,
            ptr: ptr.cast(),
            len,
        })
    }

    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for ShmMap {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr.cast(), self.len);
        }
    }
}

/// One reusable `wl_shm` buffer.
pub(super) struct Slot {
    buffer: wl_buffer::WlBuffer,
    pool: wl_shm_pool::WlShmPool,
    map: ShmMap,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) stride: u32,
    pub(super) format: wl_shm::Format,
    busy: Arc<AtomicBool>,
}

impl Slot {
    fn new(
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<WlrootsState>,
        width: u32,
        height: u32,
        stride: u32,
        format: wl_shm::Format,
    ) -> Result<Self, String> {
        let len = stride as usize * height as usize;
        let map = ShmMap::new(len)?;
        let pool = shm.create_pool(map.fd.as_fd(), len as i32, qh, ());
        let busy = Arc::new(AtomicBool::new(false));
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            format,
            qh,
            BufferData { busy: busy.clone() },
        );
        Ok(Self {
            buffer,
            pool,
            map,
            width,
            height,
            stride,
            format,
            busy,
        })
    }

    fn is_free(&self) -> bool {
        !self.busy.load(Ordering::Acquire)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}

impl WlrootsState {
    /// Pick a released slot with the requested geometry, or allocate one.
    pub(super) fn acquire_slot(
        &mut self,
        width: u32,
        height: u32,
        stride: u32,
        format: wl_shm::Format,
    ) -> Result<usize, String> {
        if let Some(index) = self.slots.iter().position(|slot| {
            slot.is_free()
                && slot.width == width
                && slot.height == height
                && slot.stride == stride
                && slot.format == format
        }) {
            self.slots[index].busy.store(true, Ordering::Release);
            return Ok(index);
        }
        if self.slots.len() >= 4 && !self.slots.iter().any(Slot::is_free) {
            return Err("all screencopy buffers are still held by the compositor".to_owned());
        }
        let slot = Slot::new(&self.shm, &self.qh, width, height, stride, format)?;
        self.slots.push(slot);
        let mut index = self.slots.len() - 1;
        self.slots[index].busy.store(true, Ordering::Release);
        // Keep the freelist small when a viewer changes resolution. Never
        // drop the slot that is about to receive the current frame.
        while self.slots.len() > 3 {
            let last = self.slots.len() - 1;
            let Some(free) = self.slots[..last].iter().position(Slot::is_free) else {
                break;
            };
            self.slots.remove(free);
            if free < index {
                index -= 1;
            }
        }
        Ok(index)
    }

    fn on_frame_buffer(
        &mut self,
        frame: &ZwlrScreencopyFrameV1,
        output_id: u32,
        format: WEnum<wl_shm::Format>,
        width: u32,
        height: u32,
        stride: u32,
    ) {
        let WEnum::Value(format) = format else {
            tracing::warn!("screencopy returned an unknown buffer format");
            frame.destroy();
            self.pending = None;
            return;
        };
        if !supported_format(format) {
            // Without this the same unusable buffer would be acquired and
            // discarded on every frame.
            tracing::debug!("screencopy returned an unsupported format: {format:?}");
            frame.destroy();
            self.pending = None;
            return;
        }
        let Some(pending) = self.pending.as_ref() else {
            frame.destroy();
            return;
        };
        if pending.frame.id() != frame.id() {
            // A late event from a frame that already timed out.
            frame.destroy();
            return;
        }
        if pending.output_id != output_id {
            frame.destroy();
            return;
        }
        // The buffer event is authoritative for the captured size, including
        // outputs with a 90/270 degree transform.
        if let Some(output) = self
            .outputs
            .iter_mut()
            .find(|output| output.id == output_id)
        {
            if output.width != width || output.height != height {
                output.width = width;
                output.height = height;
                self.dirty = true;
            }
        }
        match self.acquire_slot(width, height, stride, format) {
            Ok(index) => {
                let buffer = self.slots[index].buffer.clone();
                if let Some(pending) = self.pending.as_mut() {
                    pending.slot = Some(index);
                }
                frame.copy(&buffer);
            }
            Err(error) => {
                tracing::warn!("screencopy buffer unavailable: {error}");
                frame.destroy();
                self.pending = None;
            }
        }
    }

    /// Drop the outstanding frame and free its buffer. Used when the frame
    /// fails, times out, or its output disappears: the compositor will not
    /// send `release` for a frame that never completed, so waiting for it
    /// would leak the slot.
    pub(super) fn abandon_pending(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        pending.frame.destroy();
        if let Some(index) = pending.slot {
            if let Some(slot) = self.slots.get_mut(index) {
                slot.busy.store(false, Ordering::Release);
            }
        }
    }

    fn on_frame_ready(&mut self, frame: &ZwlrScreencopyFrameV1) {
        frame.destroy();
        if self
            .pending
            .as_ref()
            .is_none_or(|pending| pending.frame.id() != frame.id())
        {
            return;
        }
        let Some(pending) = self.pending.take() else {
            return;
        };
        let Some(index) = pending.slot else {
            return;
        };
        let Some(slot) = self.slots.get(index) else {
            return;
        };
        if self.locked || self.suspended || self.desktop.is_none() {
            return;
        }
        if self.desktop.as_ref().map(|state| state.active_id()) != Some(pending.output_id) {
            return;
        }
        let Some(data) = convert_frame(
            slot.map.bytes(),
            slot.width,
            slot.height,
            slot.stride,
            slot.format,
            pending.y_invert,
        ) else {
            tracing::warn!(
                "screencopy returned an unsupported format: {:?}",
                slot.format
            );
            return;
        };
        if let Some(desktop) = &self.desktop {
            desktop.publish_frame(CapturedFrame {
                width: slot.width,
                height: slot.height,
                data: Arc::new(data),
            });
        }
    }
}

/// Formats [`convert_frame`] understands.
pub(super) fn supported_format(format: wl_shm::Format) -> bool {
    matches!(
        format,
        wl_shm::Format::Xrgb8888
            | wl_shm::Format::Argb8888
            | wl_shm::Format::Xbgr8888
            | wl_shm::Format::Abgr8888
    )
}

/// Convert a shm screencopy buffer into tightly packed BGRA rows.
pub(super) fn convert_frame(
    src: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
    y_invert: bool,
) -> Option<Vec<u8>> {
    let width = width as usize;
    let height = height as usize;
    let stride = stride as usize;
    if width == 0 || height == 0 || stride < width * 4 {
        return None;
    }
    if src.len() < stride.checked_mul(height)? {
        return None;
    }
    let mut out = vec![0u8; width * height * 4];
    for row in 0..height {
        let source_row = if y_invert { height - 1 - row } else { row };
        let src_row = &src[source_row * stride..source_row * stride + width * 4];
        let dst_row = &mut out[row * width * 4..(row + 1) * width * 4];
        for (px, dst) in src_row.chunks_exact(4).zip(dst_row.chunks_exact_mut(4)) {
            match format {
                wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888 => {
                    dst[0] = px[0];
                    dst[1] = px[1];
                    dst[2] = px[2];
                    dst[3] = if format == wl_shm::Format::Xrgb8888 {
                        0xff
                    } else {
                        px[3]
                    };
                }
                wl_shm::Format::Xbgr8888 | wl_shm::Format::Abgr8888 => {
                    dst[0] = px[2];
                    dst[1] = px[1];
                    dst[2] = px[0];
                    dst[3] = if format == wl_shm::Format::Xbgr8888 {
                        0xff
                    } else {
                        px[3]
                    };
                }
                _ => return None,
            }
        }
    }
    Some(out)
}

impl Dispatch<wl_buffer::WlBuffer, BufferData> for WlrootsState {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        data: &BufferData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_buffer::Event::Release) {
            data.busy.store(false, Ordering::Release);
        }
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for WlrootsState {
    fn event(
        _: &mut Self,
        _: &wl_shm_pool::WlShmPool,
        _: <wl_shm_pool::WlShmPool as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, FrameData> for WlrootsState {
    fn event(
        state: &mut Self,
        frame: &ZwlrScreencopyFrameV1,
        event: FrameEvent,
        data: &FrameData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            FrameEvent::Buffer {
                format,
                width,
                height,
                stride,
            } => state.on_frame_buffer(frame, data.output_id, format, width, height, stride),
            FrameEvent::Flags { flags } => {
                let inverted = match flags {
                    WEnum::Value(flags) => flags.contains(FrameFlags::YInvert),
                    WEnum::Unknown(_) => false,
                };
                if inverted {
                    if let Some(pending) = state.pending.as_mut() {
                        if pending.frame.id() == frame.id() {
                            pending.y_invert = true;
                        }
                    }
                }
            }
            FrameEvent::Ready { .. } => state.on_frame_ready(frame),
            FrameEvent::Failed => {
                tracing::debug!("screencopy frame failed");
                if state
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.frame.id() == frame.id())
                {
                    state.abandon_pending();
                } else {
                    frame.destroy();
                }
            }
            FrameEvent::Damage { .. } | FrameEvent::LinuxDmabuf { .. } | FrameEvent::BufferDone => {
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xrgb_rows_are_reordered_to_bgra_and_opaque() {
        let src = [
            1, 2, 3, 0, 4, 5, 6, 0, // row 0
            7, 8, 9, 0, 10, 11, 12, 0, // row 1
        ];
        let out = convert_frame(&src, 2, 2, 8, wl_shm::Format::Xrgb8888, false).unwrap();
        assert_eq!(
            out,
            vec![1, 2, 3, 0xff, 4, 5, 6, 0xff, 7, 8, 9, 0xff, 10, 11, 12, 0xff]
        );
    }

    #[test]
    fn inverted_rows_are_flipped() {
        let src = [1, 2, 3, 0, 4, 5, 6, 0, 7, 8, 9, 0, 10, 11, 12, 0];
        let out = convert_frame(&src, 2, 2, 8, wl_shm::Format::Xrgb8888, true).unwrap();
        assert_eq!(&out[..4], &[7, 8, 9, 0xff]);
    }

    #[test]
    fn abgr_swaps_red_and_blue_and_keeps_alpha() {
        let src = [1, 2, 3, 4];
        let out = convert_frame(&src, 1, 1, 4, wl_shm::Format::Abgr8888, false).unwrap();
        assert_eq!(out, vec![3, 2, 1, 4]);
    }

    #[test]
    fn malformed_buffers_are_rejected() {
        assert!(convert_frame(&[0; 8], 2, 2, 8, wl_shm::Format::Xrgb8888, false).is_none());
        assert!(convert_frame(&[0; 16], 2, 2, 4, wl_shm::Format::Xrgb8888, false).is_none());
        assert!(convert_frame(&[0; 4], 1, 1, 4, wl_shm::Format::C8, false).is_none());
    }
}
