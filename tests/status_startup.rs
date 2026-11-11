//! Debug-only first-frame probe and a release instrumentation-exclusion regression.

#[cfg(not(debug_assertions))]
#[test]
fn release_excludes_status_timing_instrumentation() {
    let binary = std::fs::read(env!("CARGO_BIN_EXE_smartcard-timeclock")).unwrap();
    for marker in [
        "TIMECLOCK_TRACE_STATUS",
        "status-timing renderer=",
        "after_paint_proxy",
    ] {
        assert!(
            !binary
                .windows(marker.len())
                .any(|bytes| bytes == marker.as_bytes()),
            "Release binary contains instrumentation marker: {marker}"
        );
    }
}

// Fake desktop services must stay on a private bus.
#[cfg(debug_assertions)]
mod debug {
    use std::{
        io::{BufRead, BufReader},
        process::{Child, Command, Stdio},
        sync::mpsc::{self, Sender},
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    };

    struct Watcher(Sender<String>);
    #[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
    impl Watcher {
        fn register_status_notifier_item(&self, service: &str) {
            self.0.send(service.into()).unwrap();
        }
        #[zbus(property)]
        fn is_status_notifier_host_registered(&self) -> bool {
            true
        }
    }

    // Prevent private-bus autoactivation of real portals (and their desktop services).
    struct Appearance;
    #[zbus::interface(name = "org.freedesktop.portal.Settings")]
    impl Appearance {
        #[zbus(property, name = "version")]
        fn version(&self) -> u32 {
            1
        }

        fn read(&self, _namespace: &str, _key: &str) -> zbus::zvariant::OwnedValue {
            0u32.into()
        }
        fn read_all(
            &self,
            _namespaces: Vec<&str>,
        ) -> std::collections::HashMap<
            String,
            std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
        > {
            Default::default()
        }
    }

    struct App {
        child: Child,
        reader: Option<JoinHandle<()>>,
    }
    impl Drop for App {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = self.reader.take().unwrap().join();
        }
    }

    #[test]
    #[ignore = "requires TIMECLOCK_ISOLATED_BUS=1, a private DBus, and a graphical session"]
    fn isolated_status_startup_first_frame() {
        assert_eq!(std::env::var("TIMECLOCK_ISOLATED_BUS").as_deref(), Ok("1"));
        let (tx, registered) = mpsc::channel();
        let bus = zbus::blocking::connection::Builder::session()
            .unwrap()
            .name("org.kde.StatusNotifierWatcher")
            .unwrap()
            .name("org.freedesktop.portal.Desktop")
            .unwrap()
            .serve_at("/org/freedesktop/portal/desktop", Appearance)
            .unwrap()
            .serve_at("/StatusNotifierWatcher", Watcher(tx))
            .unwrap()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("unused.db");
        let launched = Instant::now();
        let mut child = Command::new(env!("CARGO_BIN_EXE_smartcard-timeclock"))
            .args(["--db", db.to_str().unwrap(), "tray"])
            .env("TIMECLOCK_TRACE_STATUS", "1")
            .env("GIO_USE_VFS", "local")
            .env("GTK_A11Y", "none")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let (tx, lines) = mpsc::channel();
        let app = App {
            child,
            reader: Some(thread::spawn(move || {
                for line in BufReader::new(stderr).lines() {
                    if tx.send(line.unwrap()).is_err() {
                        break;
                    }
                }
            })),
        };
        let name = registered.recv_timeout(Duration::from_secs(10)).unwrap();
        eprintln!(
            "status-startup tray_registered={:.3}ms",
            launched.elapsed().as_secs_f64() * 1000.0
        );
        let item = zbus::blocking::Proxy::new(
            &bus,
            name.as_str(),
            "/StatusNotifierItem",
            "org.kde.StatusNotifierItem",
        )
        .unwrap();
        // No earlier Status/Reports/Settings request may warm up GTK.
        assert!(!db.exists());
        let activated = Instant::now();
        let _: () = item.call("Activate", &(0i32, 0i32)).unwrap();
        let timing = loop {
            let line = lines
                .recv_timeout(Duration::from_secs(15).saturating_sub(activated.elapsed()))
                .unwrap();
            eprintln!("{line}");
            if line.starts_with("status-timing ") {
                break line;
            }
        };
        eprintln!(
            "status-startup dbus_to_trace={:.3}ms",
            activated.elapsed().as_secs_f64() * 1000.0
        );
        assert!(timing.contains("gtk_init="));
        assert!(timing.contains("after_paint_proxy="));
        if std::env::var_os("GSK_RENDERER").is_none() {
            assert!(timing.contains("renderer=GskCairoRenderer"));
        }
        drop(app);
        assert!(!db.exists());
    }
}
