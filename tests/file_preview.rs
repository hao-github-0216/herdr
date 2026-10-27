//! Exercise the actual server/JSON/Poppler/decoder pipeline with a service-like PATH.
#![cfg(unix)]

use base64::Engine;
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SERVICE_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

struct Server {
    child: Child,
    base: PathBuf,
    socket: PathBuf,
}

impl Server {
    fn spawn(path: &str, prepare: impl FnOnce(&Path)) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = PathBuf::from(format!("/tmp/hpdf-{}-{nanos}", std::process::id()));
        fs::create_dir(&base).unwrap();
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
        for directory in ["config", "state", "data", "cache", "runtime", "documents"] {
            fs::create_dir(base.join(directory)).unwrap();
        }
        prepare(&base);
        let socket = base.join("runtime/api.sock");
        let child = Command::new(env!("CARGO_BIN_EXE_herdr"))
            .arg("server")
            .env_clear()
            .env("PATH", path)
            .env("HOME", &base)
            .env("XDG_CONFIG_HOME", base.join("config"))
            .env("XDG_STATE_HOME", base.join("state"))
            .env("XDG_DATA_HOME", base.join("data"))
            .env("XDG_CACHE_HOME", base.join("cache"))
            .env("XDG_RUNTIME_DIR", base.join("runtime"))
            .env("HERDR_SOCKET_PATH", &socket)
            .env("SHELL", "/bin/sh")
            .env("TERM", "xterm-256color")
            .current_dir(base.join("documents"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut server = Self {
            child,
            base,
            socket,
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if UnixStream::connect(&server.socket).is_ok() {
                return server;
            }
            assert!(
                server.child.try_wait().unwrap().is_none(),
                "disposable server exited"
            );
            assert!(
                Instant::now() < deadline,
                "disposable server socket timed out"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn preview(&self, path: &str, page: u32) -> Value {
        let mut stream = UnixStream::connect(&self.socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        writeln!(
            stream,
            "{}",
            json!({
                "id": "synthetic-pdf", "method": "file.preview", "params": {
                    "root": self.base.join("documents"), "path": path,
                    "page": page, "width": 40, "height": 80
                }
            })
        )
        .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // This handle only owns the disposable test process, never an installed server.
        // Let its shutdown reap pane children before falling back to a bounded kill.
        if self.child.try_wait().ok().flatten().is_none() {
            unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM) };
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.child.try_wait().ok().flatten().is_none() {
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn synthetic_pdf(url: &str) -> Vec<u8> {
    let red = "1 0 0 rg 0 0 100 200 re f\n";
    let blue = "0 0 1 rg 0 0 100 200 re f\n";
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R /OpenAction 7 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 200] /Contents 5 0 R >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 200] /Contents 6 0 R >>".into(),
        format!("<< /Length {} >>\nstream\n{red}endstream", red.len()),
        format!("<< /Length {} >>\nstream\n{blue}endstream", blue.len()),
        format!("<< /S /JavaScript /JS (this.submitForm\\(\\{{cURL: '{url}'\\}}\\);) >>"),
    ];
    let mut bytes = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(bytes.len());
        bytes.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
    }
    let start = bytes.len();
    bytes.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    bytes.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{start}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    bytes
}

fn assert_page(response: &Value, page: u32, color: [u8; 3]) {
    assert_eq!(response["result"]["type"], "file_preview", "{response}");
    let content = &response["result"]["content"];
    assert_eq!(content["kind"], "pdf", "{response}");
    assert_eq!(content["page"], page);
    assert_eq!(content["pages"], 2);
    let image = &content["image"];
    assert_eq!(image["width"], 40);
    assert_eq!(image["height"], 80);
    let rgb = base64::engine::general_purpose::STANDARD
        .decode(image["rgb_base64"].as_str().unwrap())
        .unwrap();
    assert_eq!(rgb.len(), 40 * 80 * 3);
    assert_eq!(&rgb[(40 * 40 + 20) * 3..][..3], &color);
}

#[test]
fn file_preview_pdf_service_path_renders_both_pages_and_confines_source() {
    // Local validation sets HERDR_TEST_REQUIRE_POPPLER=1; CI can exercise typed absence.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let bytes = synthetic_pdf(&format!(
        "http://{}/synthetic-action",
        listener.local_addr().unwrap()
    ));
    let server = Server::spawn(SERVICE_PATH, |base| {
        let documents = base.join("documents");
        fs::write(documents.join("two pages;$(touch SHELL_RAN).pdf"), &bytes).unwrap();
        fs::write(base.join("outside.pdf"), &bytes).unwrap();
        symlink(base.join("outside.pdf"), documents.join("escape.pdf")).unwrap();
        // An untrusted cwd must never supply either executable, even with an empty PATH entry.
        for name in ["pdfinfo", "pdftoppm"] {
            let path = documents.join(name);
            fs::write(&path, b"#!/bin/sh\ntouch CWD_TOOL_RAN\nexit 1\n").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
    });
    let name = "two pages;$(touch SHELL_RAN).pdf";
    let first = server.preview(name, 1);
    let missing = first["result"]["content"]["reason"] == "tool_missing";
    if missing {
        assert_eq!(first["result"]["type"], "file_preview", "{first}");
        assert_eq!(first["result"]["content"]["kind"], "unavailable", "{first}");
        let message = first["result"]["content"]["message"].as_str().unwrap();
        assert!(
            message.contains("pdfinfo") || message.contains("pdftoppm"),
            "{first}"
        );
        assert!(message.contains("install Poppler"), "{first}");
        assert_ne!(
            std::env::var("HERDR_TEST_REQUIRE_POPPLER").as_deref(),
            Ok("1"),
            "required Poppler pipeline unavailable: {first}"
        );
        eprintln!("optional-branch: Poppler absent from service PATH/platform fallback; verified typed tool_missing: {message}");
    } else {
        assert_page(&first, 1, [255, 0, 0]);
        assert_page(&server.preview(name, 2), 2, [0, 0, 255]);
        let response = server.preview(name, 3);
        assert_eq!(
            response["result"]["content"]["kind"], "unavailable",
            "{response}"
        );
        assert_eq!(response["result"]["content"]["reason"], "invalid_page");
    }
    for path in ["../outside.pdf", "escape.pdf", "/etc/passwd"] {
        let response = server.preview(path, 1);
        assert!(
            response.get("error").is_some(),
            "source confinement: {response}"
        );
    }
    let response = server.preview(name, 0);
    assert_eq!(response["error"]["code"], "invalid_request");
    assert!(!server.base.join("documents/SHELL_RAN").exists());
    assert!(!server.base.join("documents/CWD_TOOL_RAN").exists());
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn file_preview_pdf_uses_absolute_path_tools_before_platform_fallback_without_shell() {
    // No shell can parse this PATH directory, and no fallback may bypass these tools.
    let server = Server::spawn(SERVICE_PATH, |base| {
        fs::write(
            base.join("documents/synthetic.pdf"),
            synthetic_pdf("http://127.0.0.1:1/unused"),
        )
        .unwrap();
    });
    let tools = server.base.join("tools;$(touch PATH_SHELL_RAN)");
    fs::create_dir(&tools).unwrap();
    let fake_info = tools.join("pdfinfo");
    fs::write(
        &fake_info,
        b"#!/bin/sh\n/bin/cat >/dev/null\nprintf 'Pages: 0\\nEncrypted: no\\n'\n",
    )
    .unwrap();
    fs::set_permissions(&fake_info, fs::Permissions::from_mode(0o700)).unwrap();
    // A second disposable server inherits only the explicit path override.
    let path = format!("{}:{SERVICE_PATH}", tools.display());
    let with_override = Server::spawn(&path, |base| {
        fs::write(
            base.join("documents/synthetic.pdf"),
            synthetic_pdf("http://127.0.0.1:1/unused"),
        )
        .unwrap();
    });
    let response = with_override.preview("synthetic.pdf", 1);
    assert_eq!(
        response["result"]["content"]["reason"], "too_large",
        "{response}"
    );
    assert!(!with_override.base.join("documents/PATH_SHELL_RAN").exists());
}
