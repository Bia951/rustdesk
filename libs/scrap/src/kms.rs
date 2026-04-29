use crate::{common::would_block_if_equal, Frame, PixelBuffer, Pixfmt, TraitCapturer};
use hbb_common::log;
use serde::Deserialize;
use std::{
    fs,
    fs::OpenOptions,
    io::{self, BufRead, BufReader, Read, Write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    time::Duration,
};

const HELPER_READY_TIMEOUT: Duration = Duration::from_secs(120);
const HELPER_FRAME_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct Display {
    card_path: PathBuf,
    connector_path: PathBuf,
    name: String,
    origin: (i32, i32),
    width: usize,
    height: usize,
    online: bool,
    primary: bool,
    accessible: bool,
}

impl Display {
    pub fn primary() -> io::Result<Display> {
        let mut all = Self::all()?;
        if all.is_empty() {
            return Err(io::ErrorKind::NotFound.into());
        }
        Ok(all.remove(0))
    }

    pub fn all() -> io::Result<Vec<Display>> {
        let mut entries = match query_helper_displays() {
            Ok(entries) => entries.into_iter().map(Self::from_helper_display).collect(),
            Err(err) => {
                log::debug!("kms helper probe unavailable, fallback to local sysfs probe: {err}");
                fs::read_dir("/sys/class/drm")?
                    .filter_map(Result::ok)
                    .filter_map(|entry| Self::from_connector_path(entry.path()).ok().flatten())
                    .collect::<Vec<_>>()
            }
        };
        entries.sort_by(|left, right| left.name.cmp(&right.name));

        let mut current_x = 0_i32;
        for (index, entry) in entries.iter_mut().enumerate() {
            entry.origin = (current_x, 0);
            entry.primary = index == 0;
            current_x = current_x.saturating_add(entry.width as i32);
        }

        Ok(entries)
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn origin(&self) -> (i32, i32) {
        self.origin
    }

    pub fn is_online(&self) -> bool {
        self.online
    }

    pub fn is_primary(&self) -> bool {
        self.primary
    }

    pub fn name(&self) -> String {
        self.name.clone()
    }

    pub fn card_path(&self) -> &Path {
        &self.card_path
    }

    pub fn connector_path(&self) -> &Path {
        &self.connector_path
    }

    fn from_helper_display(display: HelperDisplay) -> Self {
        Self {
            card_path: PathBuf::from(display.card_path),
            connector_path: PathBuf::from(display.connector_path),
            name: display.name,
            origin: (0, 0),
            width: display.width,
            height: display.height,
            online: display.online,
            primary: false,
            accessible: display.can_open,
        }
    }

    fn from_connector_path(path: PathBuf) -> io::Result<Option<Self>> {
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            return Ok(None);
        };
        if !name.starts_with("card") || !name.contains('-') {
            return Ok(None);
        }

        let status = read_trimmed(path.join("status"))?;
        if !status.eq_ignore_ascii_case("connected") {
            return Ok(None);
        }

        let mode = read_first_line(path.join("modes"))?;
        let Some((width, height)) = parse_mode(&mode) else {
            return Ok(None);
        };

        let name = name.to_owned();
        let card_name = name
            .split_once('-')
            .map(|(card, _)| card)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid drm connector"))?;
        let card_path = PathBuf::from("/dev/dri").join(card_name);
        let (accessible, _) = check_card_access(&card_path);

        Ok(Some(Self {
            card_path,
            connector_path: path,
            name,
            origin: (0, 0),
            width,
            height,
            online: true,
            primary: false,
            accessible,
        }))
    }
}

pub struct Capturer {
    display: Display,
    width: usize,
    height: usize,
    pixfmt: Pixfmt,
    stride: Vec<usize>,
    frame_data: Vec<u8>,
    helper: Option<HelperSession>,
    pending_frame: Option<HelperFrameOutput>,
    privileged_attempted: bool,
}

impl Capturer {
    pub fn new(display: Display) -> io::Result<Capturer> {
        if !display.card_path().exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("DRM device '{}' not found", display.card_path().display()),
            ));
        }
        let mut capturer = Capturer {
            width: display.width(),
            height: display.height(),
            display,
            pixfmt: Pixfmt::BGRA,
            stride: vec![0],
            frame_data: Vec::new(),
            helper: None,
            pending_frame: None,
            privileged_attempted: false,
        };
        capturer.prime_frame()?;
        Ok(capturer)
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }
}

impl TraitCapturer for Capturer {
    fn frame<'a>(&'a mut self, _timeout: Duration) -> io::Result<Frame<'a>> {
        let frame = self.next_frame()?;
        let same_layout = self.width == frame.width
            && self.height == frame.height
            && self.pixfmt == frame.pixfmt
            && self.stride == frame.stride;
        if same_layout {
            would_block_if_equal(&mut self.frame_data, &frame.data)?;
        } else {
            self.frame_data = frame.data;
        }
        self.width = frame.width;
        self.height = frame.height;
        self.pixfmt = frame.pixfmt;
        self.stride = frame.stride;
        Ok(Frame::PixelBuffer(PixelBuffer::new(
            &self.frame_data,
            self.pixfmt,
            self.width,
            self.height,
        )))
    }
}

impl Capturer {
    fn prime_frame(&mut self) -> io::Result<()> {
        let frame = self.read_helper_frame()?;
        if self.width != frame.width || self.height != frame.height {
            log::info!(
                "kms capture frame size differs from display mode: {}x{} -> {}x{}",
                self.width,
                self.height,
                frame.width,
                frame.height
            );
        }
        self.width = frame.width;
        self.height = frame.height;
        self.pixfmt = frame.pixfmt;
        self.stride = frame.stride.clone();
        self.pending_frame = Some(frame);
        Ok(())
    }

    fn next_frame(&mut self) -> io::Result<HelperFrameOutput> {
        if let Some(frame) = self.pending_frame.take() {
            return Ok(frame);
        }
        self.read_helper_frame()
    }

    fn read_helper_frame(&mut self) -> io::Result<HelperFrameOutput> {
        if self.helper.is_none() {
            let privileged = !self.display.accessible;
            if let Err(err) = self.spawn_helper(privileged) {
                return self.retry_privileged_or_return(err);
            }
        }

        let frame = match self.helper.as_mut() {
            Some(helper) => helper.frame(),
            None => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "kms helper session unavailable",
            )),
        };

        match frame {
            Ok(frame) => Ok(frame),
            Err(err) => self.retry_privileged_or_return(err),
        }
    }

    fn spawn_helper(&mut self, privileged: bool) -> io::Result<()> {
        if privileged {
            self.privileged_attempted = true;
        }
        let helper = HelperSession::spawn(&self.display.name(), privileged)?;
        self.helper = Some(helper);
        Ok(())
    }

    fn retry_privileged_or_return(&mut self, err: io::Error) -> io::Result<HelperFrameOutput> {
        self.helper = None;
        if self.privileged_attempted || !should_retry_privileged_message(&err.to_string()) {
            return Err(err);
        }

        self.spawn_helper(true)?;
        match self.helper.as_mut() {
            Some(helper) => helper.frame(),
            None => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "privileged kms helper session unavailable",
            )),
        }
    }
}

fn read_trimmed(path: PathBuf) -> io::Result<String> {
    Ok(fs::read_to_string(path)?.trim().to_owned())
}

fn read_first_line(path: PathBuf) -> io::Result<String> {
    fs::read_to_string(path)?
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing drm mode"))
}

fn parse_mode(mode: &str) -> Option<(usize, usize)> {
    let (width, height) = mode.split_once('x')?;
    let width = width.parse().ok()?;
    let height = height.parse().ok()?;
    Some((width, height))
}

fn check_card_access(card_path: &Path) -> (bool, Option<String>) {
    match OpenOptions::new().read(true).write(true).open(card_path) {
        Ok(_) => (true, None),
        Err(err) => (false, Some(err.to_string())),
    }
}

fn query_helper_displays() -> io::Result<Vec<HelperDisplay>> {
    let output = run_helper(["probe"])?;
    let response: HelperProbeOutput = serde_json::from_slice(&output.stdout)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
    Ok(response.displays)
}

fn run_helper<const N: usize>(args: [&str; N]) -> io::Result<std::process::Output> {
    let output = Command::new(std::env::current_exe()?)
        .arg("--kms-helper")
        .args(args)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let message = if stderr.is_empty() {
            "kms helper failed".to_owned()
        } else {
            stderr
        };
        let lower = message.to_ascii_lowercase();
        let kind =
            if lower.contains("permission denied") || lower.contains("operation not permitted") {
                io::ErrorKind::PermissionDenied
            } else if lower.contains("unsupported") {
                io::ErrorKind::Unsupported
            } else {
                io::ErrorKind::Other
            };
        return Err(io::Error::new(kind, message));
    }
    Ok(output)
}

fn should_retry_privileged_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("permission denied")
        || lower.contains("operation not permitted")
        || lower.contains("no accessible gem handle")
        || lower.contains("no active framebuffer")
}

fn parse_pixfmt(pixfmt: &str) -> io::Result<Pixfmt> {
    match pixfmt {
        "BGRA" => Ok(Pixfmt::BGRA),
        "RGBA" => Ok(Pixfmt::RGBA),
        "RGB565LE" => Ok(Pixfmt::RGB565LE),
        other => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("unsupported kms helper pixfmt '{other}'"),
        )),
    }
}

#[derive(Deserialize)]
struct HelperProbeOutput {
    displays: Vec<HelperDisplay>,
}

#[derive(Deserialize)]
struct HelperDisplay {
    card_path: String,
    connector_path: String,
    name: String,
    width: usize,
    height: usize,
    online: bool,
    can_open: bool,
}

#[derive(Deserialize)]
struct HelperFrameHeader {
    width: usize,
    height: usize,
    stride: usize,
    pixfmt: String,
    byte_len: usize,
}

struct HelperFrameOutput {
    width: usize,
    height: usize,
    stride: Vec<usize>,
    pixfmt: Pixfmt,
    data: Vec<u8>,
}

struct HelperSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl HelperSession {
    fn spawn(display_name: &str, privileged: bool) -> io::Result<Self> {
        let mut command = if privileged {
            let mut command = Command::new("pkexec");
            command.arg("--disable-internal-agent");
            command.arg(std::env::current_exe()?);
            command
        } else {
            Command::new(std::env::current_exe()?)
        };

        log::info!(
            "starting {} kms helper stream for {}",
            if privileged { "privileged" } else { "unprivileged" },
            display_name
        );

        let mut child = command
            .arg("--kms-helper")
            .arg(if privileged {
                "stream-privileged"
            } else {
                "stream"
            })
            .arg(display_name)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "kms helper stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "kms helper stdout unavailable"))?;

        let mut session = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        };
        let mut ready = String::new();
        session.wait_for_stdout(HELPER_READY_TIMEOUT, "kms helper ready")?;
        let read = session.stdout.read_line(&mut ready)?;
        if read == 0 {
            return Err(session.child_error("kms helper exited before ready"));
        }
        if ready.trim() != "ready" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected kms helper ready message: {}", ready.trim()),
            ));
        }
        log::info!(
            "{} kms helper stream is ready for {}",
            if privileged { "privileged" } else { "unprivileged" },
            display_name
        );
        Ok(session)
    }

    fn frame(&mut self) -> io::Result<HelperFrameOutput> {
        self.stdin.write_all(b"frame\n")?;
        self.stdin.flush()?;

        let mut header = Vec::new();
        self.wait_for_stdout(HELPER_FRAME_TIMEOUT, "kms helper frame header")?;
        let read = self.stdout.read_until(b'\n', &mut header)?;
        if read == 0 {
            return Err(self.child_error("kms helper exited before frame header"));
        }
        if header.last() == Some(&b'\n') {
            header.pop();
        }
        let header: HelperFrameHeader = serde_json::from_slice(&header)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
        let mut data = vec![0u8; header.byte_len];
        self.stdout.read_exact(&mut data)?;
        Ok(HelperFrameOutput {
            width: header.width,
            height: header.height,
            stride: vec![header.stride],
            pixfmt: parse_pixfmt(&header.pixfmt)?,
            data,
        })
    }

    fn wait_for_stdout(&self, timeout: Duration, context: &str) -> io::Result<()> {
        let timeout_ms = timeout
            .as_millis()
            .min(i32::MAX as u128)
            as i32;
        let mut pollfd = crate::libc::pollfd {
            fd: self.stdout.get_ref().as_raw_fd(),
            events: crate::libc::POLLIN | crate::libc::POLLHUP | crate::libc::POLLERR,
            revents: 0,
        };
        let ret = unsafe { crate::libc::poll(&mut pollfd, 1, timeout_ms) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        if ret == 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{context} timed out after {} ms", timeout.as_millis()),
            ));
        }
        if pollfd.revents & crate::libc::POLLERR != 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("{context} pipe error"),
            ));
        }
        Ok(())
    }

    fn child_error(&mut self, fallback: &str) -> io::Error {
        let mut stderr = String::new();
        if let Some(stderr_pipe) = self.child.stderr.as_mut() {
            let _ = stderr_pipe.read_to_string(&mut stderr);
        }
        let message = if stderr.trim().is_empty() {
            fallback.to_owned()
        } else {
            stderr.trim().to_owned()
        };
        let kind = if should_retry_privileged_message(&message) {
            io::ErrorKind::PermissionDenied
        } else {
            io::ErrorKind::Other
        };
        io::Error::new(kind, message)
    }
}

impl Drop for HelperSession {
    fn drop(&mut self) {
        let _ = self.stdin.write_all(b"quit\n");
        let _ = self.stdin.flush();
        for _ in 0..10 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
