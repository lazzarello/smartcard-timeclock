//! Debug-only, opt-in activation timings; after-paint is not compositor presentation.
use super::*;
use std::{cell::RefCell, time::Instant};

pub(super) struct StatusTiming {
    started: Instant,
    marks: Option<Vec<(&'static str, Duration)>>,
    renderer: Option<&'static str>,
}

impl StatusTiming {
    pub(super) fn new(started: Instant) -> Self {
        Self {
            started,
            marks: (std::env::var_os("TIMECLOCK_TRACE_STATUS").as_deref()
                == Some(std::ffi::OsStr::new("1")))
            .then(Vec::new),
            renderer: None,
        }
    }

    pub(super) fn mark(&mut self, name: &'static str) {
        if let Some(marks) = &mut self.marks {
            marks.push((name, self.started.elapsed()));
        }
    }

    fn finish(mut self, name: &'static str) {
        self.mark(name);
        if let Some(marks) = self.marks {
            let fields: Vec<_> = marks
                .into_iter()
                .map(|(name, elapsed)| format!("{name}={:.3}ms", elapsed.as_secs_f64() * 1000.0))
                .collect();
            eprintln!(
                "status-timing renderer={} {}",
                self.renderer.unwrap_or("unknown"),
                fields.join(" ")
            );
        }
    }
}

pub(super) struct FrameTrace {
    clock: gtk4::gdk::FrameClock,
    window: gtk4::Window,
    handlers: Option<(glib::SignalHandlerId, glib::SignalHandlerId)>,
    pending: Rc<RefCell<Option<StatusTiming>>>,
}

impl FrameTrace {
    pub(super) fn new(window: &gtk4::Window, mut timing: StatusTiming) -> Option<Self> {
        timing.marks.as_ref()?;
        timing.renderer = window.renderer().map(|renderer| renderer.type_().name());
        if window.is_mapped() {
            timing.mark("mapped");
        }
        let Some(clock) = window.frame_clock() else {
            timing.finish("no_frame_clock");
            return None;
        };
        let pending = Rc::new(RefCell::new(Some(timing)));
        let painted = pending.clone();
        let after_paint = clock.connect_after_paint(move |_| {
            if let Some(timing) = painted.borrow_mut().take() {
                timing.finish("after_paint_proxy");
            }
        });
        let hidden = pending.clone();
        let unmap = window.connect_unmap(move |_| {
            if let Some(timing) = hidden.borrow_mut().take() {
                timing.finish("unmapped_before_frame");
            }
        });
        // A visible singleton may need no new frame when raised again.
        window.queue_draw();
        Some(Self {
            clock,
            window: window.clone(),
            handlers: Some((after_paint, unmap)),
            pending,
        })
    }
}

impl Drop for FrameTrace {
    fn drop(&mut self) {
        let (after_paint, unmap) = self.handlers.take().unwrap();
        self.clock.disconnect(after_paint);
        self.window.disconnect(unmap);
        if let Some(timing) = self.pending.borrow_mut().take() {
            timing.finish("cancelled_before_frame");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_are_cumulative_and_disabled_traces_do_not_collect() {
        for enabled in [false, true] {
            let mut timing = StatusTiming {
                started: Instant::now(),
                marks: enabled.then(Vec::new),
                renderer: None,
            };
            timing.mark("dispatch");
            timing.mark("present");
            if let Some(marks) = timing.marks {
                assert_eq!(marks.len(), 2);
                assert_eq!((marks[0].0, marks[1].0), ("dispatch", "present"));
                assert!(marks[0].1 <= marks[1].1);
            } else {
                assert!(!enabled);
            }
        }
    }
}
