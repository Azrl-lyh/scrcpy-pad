use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::backend::HdcInputBackend;
use crate::hdc::Hdc;
use crate::input_capture::{Capture, CaptureEvent};
use crate::keymap::Profile;
use crate::runtime::MappingRuntime;

const MOTION_FLUSH: Duration = Duration::from_millis(8);
const IDLE_TICK: Duration = Duration::from_millis(16);

fn flush_motion(runtime: &mut MappingRuntime, pending: &mut (f32, f32)) -> bool {
    if pending.0 == 0.0 && pending.1 == 0.0 {
        return false;
    }
    runtime.handle_motion(pending.0, pending.1);
    *pending = (0.0, 0.0);
    true
}

/// Owns the PC input hook and translates its events into the HDC mapping runtime.
pub struct MappingService {
    runtime: Arc<Mutex<MappingRuntime>>,
    mouse_grab: Arc<AtomicBool>,
    _capture: Capture,
}

impl MappingService {
    pub fn start(
        serial: impl Into<String>,
        viewport: (u32, u32),
        profile: Profile,
    ) -> Result<Self> {
        let serial = serial.into();
        let hdc = Hdc::locate()?;
        let backend = Arc::new(HdcInputBackend::new(hdc, serial));
        let runtime = Arc::new(Mutex::new(MappingRuntime::new(backend, profile, viewport)));

        let (tx, rx) = std::sync::mpsc::channel::<CaptureEvent>();
        let capture = Capture::start(tx)?;
        let mouse_grab = capture.mouse_grab.clone();

        let worker_runtime = runtime.clone();
        let worker_mouse_grab = mouse_grab.clone();
        std::thread::spawn(move || {
            let mut pending_motion = (0.0_f32, 0.0_f32);
            let mut last_motion_flush = Instant::now();
            loop {
                let wait = if pending_motion.0 == 0.0 && pending_motion.1 == 0.0 {
                    IDLE_TICK
                } else {
                    MOTION_FLUSH
                        .saturating_sub(last_motion_flush.elapsed())
                        .max(Duration::from_millis(1))
                };
                match rx.recv_timeout(wait) {
                    Ok(CaptureEvent::Button { code, pressed }) => {
                        let mut runtime = worker_runtime.lock().unwrap();
                        flush_motion(&mut runtime, &mut pending_motion);
                        last_motion_flush = Instant::now();
                        runtime.handle_key_event(code, pressed, &[]);
                        worker_mouse_grab
                            .store(runtime.pointer_should_be_hidden(), Ordering::Release);
                    }
                    Ok(CaptureEvent::Motion { dx, dy }) => {
                        pending_motion.0 += dx;
                        pending_motion.1 += dy;
                        if last_motion_flush.elapsed() >= MOTION_FLUSH {
                            let mut runtime = worker_runtime.lock().unwrap();
                            flush_motion(&mut runtime, &mut pending_motion);
                            last_motion_flush = Instant::now();
                            worker_mouse_grab
                                .store(runtime.pointer_should_be_hidden(), Ordering::Release);
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        let mut runtime = worker_runtime.lock().unwrap();
                        flush_motion(&mut runtime, &mut pending_motion);
                        last_motion_flush = Instant::now();
                        runtime.tick_aim();
                        worker_mouse_grab
                            .store(runtime.pointer_should_be_hidden(), Ordering::Release);
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            worker_mouse_grab.store(false, Ordering::Release);
        });

        Ok(Self {
            runtime,
            mouse_grab,
            _capture: capture,
        })
    }

    pub fn runtime(&self) -> Arc<Mutex<MappingRuntime>> {
        self.runtime.clone()
    }

    pub fn mouse_grab(&self) -> Arc<AtomicBool> {
        self.mouse_grab.clone()
    }

    /// Synchronises state changed directly through the GUI (profile edits or
    /// toolbar toggles) with the global pointer-hiding flag.
    pub fn sync_state(&self) {
        let runtime = self.runtime.lock().unwrap();
        self.mouse_grab
            .store(runtime.pointer_should_be_hidden(), Ordering::Release);
    }
}
