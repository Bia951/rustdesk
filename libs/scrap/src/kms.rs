use crate::{
    common::would_block_if_equal, is_linux_kms_dmabuf_capture_enabled_for, DmabufFrame,
    DmabufPlane, Frame, PixelBuffer, Pixfmt, TraitCapturer,
};
use hbb_common::log;
use serde::Deserialize;
use std::{
    fs,
    fs::OpenOptions,
    io::{self, BufRead, BufReader, Read, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    os::unix::net::UnixDatagram,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

const HELPER_READY_TIMEOUT: Duration = Duration::from_secs(15);
const HELPER_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
const DRM_FORMAT_MOD_LINEAR: u64 = 0;
static DMABUF_SOCKET_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub struct Display {
    card_path: PathBuf,
    render_node: Option<PathBuf>,
    connector_path: PathBuf,
    name: String,
    origin: (i32, i32),
    width: usize,
    height: usize,
    online: bool,
    primary: bool,
    accessible: bool,
    origin_known: bool,
    modifier_known: bool,
    // false when the active scanout buffer is tiled/non-linear, so the CPU
    // readback path can't decode it and the dmabuf path is required.
    is_linear: bool,
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

        let mut current_x = entries
            .iter()
            .filter(|entry| entry.origin_known)
            .map(|entry| {
                entry
                    .origin
                    .0
                    .saturating_add(entry.width.min(i32::MAX as usize) as i32)
            })
            .max()
            .unwrap_or(0);
        for (index, entry) in entries.iter_mut().enumerate() {
            if !entry.origin_known {
                entry.origin = (current_x, 0);
                current_x = current_x.saturating_add(entry.width.min(i32::MAX as usize) as i32);
            }
            entry.primary = index == 0;
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

    pub fn render_node(&self) -> Option<&Path> {
        self.render_node.as_deref()
    }

    pub fn connector_path(&self) -> &Path {
        &self.connector_path
    }

    pub fn is_linear(&self) -> bool {
        self.is_linear
    }

    pub fn cpu_readback_safe(&self) -> bool {
        cpu_readback_safe(self.modifier_known, self.is_linear)
    }

    fn from_helper_display(display: HelperDisplay) -> Self {
        let origin = display.x.zip(display.y);
        Self {
            card_path: PathBuf::from(display.card_path),
            render_node: display.render_node.map(PathBuf::from),
            connector_path: PathBuf::from(display.connector_path),
            name: display.name,
            origin: origin.unwrap_or((0, 0)),
            width: display.width,
            height: display.height,
            online: display.online,
            primary: false,
            accessible: display.can_open,
            origin_known: origin.is_some(),
            modifier_known: display.modifier_known,
            is_linear: display.is_linear,
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
        let render_node = find_render_node(&card_path);
        let (accessible, _) = check_card_access(&card_path);

        Ok(Some(Self {
            card_path,
            render_node,
            connector_path: path,
            name,
            origin: (0, 0),
            width,
            height,
            online: true,
            primary: false,
            accessible,
            origin_known: false,
            modifier_known: false,
            // The sysfs fallback can't read the framebuffer modifier. Keep a
            // placeholder value, but modifier_known=false prevents treating it
            // as a safe CPU fallback.
            is_linear: true,
        }))
    }
}

fn cpu_readback_safe(modifier_known: bool, is_linear: bool) -> bool {
    modifier_known && is_linear
}

pub struct Capturer {
    display: Display,
    width: usize,
    height: usize,
    pixfmt: Pixfmt,
    stride: Vec<usize>,
    frame_data: Vec<u8>,
    helper: Option<HelperSession>,
    dmabuf_helper: Option<HelperDmabufSession>,
    pending_frame: Option<CapturedFrame>,
    privileged_attempted: bool,
    use_dmabuf: bool,
}

impl Capturer {
    pub fn new(display: Display) -> io::Result<Capturer> {
        Self::new_with_dmabuf(display, None)
    }

    pub fn new_dmabuf(display: Display) -> io::Result<Capturer> {
        Self::new_with_dmabuf(display, Some(true))
    }

    fn new_with_dmabuf(display: Display, force_dmabuf: Option<bool>) -> io::Result<Capturer> {
        if !display.card_path().exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("DRM device '{}' not found", display.card_path().display()),
            ));
        }
        let auto_dmabuf = force_dmabuf.is_none();
        let use_dmabuf = force_dmabuf.unwrap_or_else(|| {
            is_linux_kms_dmabuf_capture_enabled_for(display.cpu_readback_safe())
        });
        let mut capturer = Capturer {
            width: display.width(),
            height: display.height(),
            display,
            pixfmt: Pixfmt::BGRA,
            stride: vec![0],
            frame_data: Vec::new(),
            helper: None,
            dmabuf_helper: None,
            pending_frame: None,
            privileged_attempted: false,
            use_dmabuf,
        };
        capturer.prime_frame()?;
        // A probe without DRM access reports an unknown modifier. The first
        // privileged dmabuf frame gives us the authoritative modifier; if the
        // path was disabled and that frame is linear, recreate the session on
        // the safe CPU readback path instead of retrying dmabuf indefinitely.
        if auto_dmabuf
            && capturer.use_dmabuf
            && !is_linux_kms_dmabuf_capture_enabled_for(capturer.display.cpu_readback_safe())
        {
            log::info!("KMS scanout is linear; retrying capture through CPU readback");
            capturer.pending_frame = None;
            capturer.dmabuf_helper = None;
            capturer.privileged_attempted = false;
            capturer.use_dmabuf = false;
            capturer.prime_frame()?;
        }
        Ok(capturer)
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn produces_dmabuf(&self) -> bool {
        self.use_dmabuf
    }

    pub fn dmabuf_device_path(&self) -> Option<String> {
        self.display
            .render_node()
            .map(|path| path.display().to_string())
    }

    pub fn dmabuf_cpu_fallback_safe(&self) -> bool {
        self.display.cpu_readback_safe()
    }
}

impl TraitCapturer for Capturer {
    fn frame<'a>(&'a mut self, timeout: Duration) -> io::Result<Frame<'a>> {
        match self.next_frame(timeout)? {
            CapturedFrame::Pixel(frame) => {
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
            CapturedFrame::Dmabuf(frame) => {
                self.width = frame.width;
                self.height = frame.height;
                Ok(Frame::Dmabuf(frame))
            }
        }
    }
}

impl Capturer {
    fn prime_frame(&mut self) -> io::Result<()> {
        let frame = self.read_frame(HELPER_FRAME_TIMEOUT)?;
        let (width, height) = frame.size();
        if self.width != width || self.height != height {
            log::info!(
                "kms capture frame size differs from display mode: {}x{} -> {}x{}",
                self.width,
                self.height,
                width,
                height
            );
        }
        self.width = width;
        self.height = height;
        if let CapturedFrame::Pixel(frame) = &frame {
            self.pixfmt = frame.pixfmt;
            self.stride = frame.stride.clone();
        }
        self.pending_frame = Some(frame);
        Ok(())
    }

    fn next_frame(&mut self, timeout: Duration) -> io::Result<CapturedFrame> {
        if let Some(frame) = self.pending_frame.take() {
            return Ok(frame);
        }
        self.read_frame(timeout)
    }

    fn read_frame(&mut self, timeout: Duration) -> io::Result<CapturedFrame> {
        if self.use_dmabuf {
            let frame = self.read_helper_dmabuf_frame(timeout)?;
            self.display.modifier_known = true;
            self.display.is_linear = frame.modifier == DRM_FORMAT_MOD_LINEAR;
            Ok(CapturedFrame::Dmabuf(frame))
        } else {
            if !self.display.is_linear() {
                // Tiled scanout buffer + CPU readback would yield garbage, and we
                // didn't enable dmabuf (VAAPI unavailable). Fail with a clear,
                // actionable error instead of spawning a doomed helper.
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!(
                        "kms display '{}' uses a tiled/non-linear framebuffer the CPU capture \
                         path cannot read; the VAAPI dmabuf path is required but unavailable \
                         (needs hwcodec built with libavfilter and a VAAPI H264/H265 encoder)",
                        self.display.name()
                    ),
                ));
            }
            self.read_helper_frame().map(CapturedFrame::Pixel)
        }
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

    fn read_helper_dmabuf_frame(&mut self, timeout: Duration) -> io::Result<DmabufFrame> {
        if self.dmabuf_helper.is_none() {
            let privileged = !self.display.accessible;
            if let Err(err) = self.spawn_dmabuf_helper(privileged) {
                return self.retry_privileged_dmabuf_or_return(err, timeout);
            }
        }

        let frame = match self.dmabuf_helper.as_mut() {
            Some(helper) => helper.frame(timeout),
            None => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "kms dmabuf helper session unavailable",
            )),
        };

        match frame {
            Ok(frame) => Ok(frame),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Err(err)
            }
            Err(err) => self.retry_privileged_dmabuf_or_return(err, timeout),
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

    fn spawn_dmabuf_helper(&mut self, privileged: bool) -> io::Result<()> {
        if privileged {
            self.privileged_attempted = true;
        }
        let helper = HelperDmabufSession::spawn(&self.display.name(), privileged)?;
        self.dmabuf_helper = Some(helper);
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

    fn retry_privileged_dmabuf_or_return(
        &mut self,
        err: io::Error,
        timeout: Duration,
    ) -> io::Result<DmabufFrame> {
        self.dmabuf_helper = None;
        if self.privileged_attempted || !should_retry_privileged_message(&err.to_string()) {
            return Err(err);
        }

        log::warn!("retrying kms dmabuf capture with privileged helper after: {err}");
        self.spawn_dmabuf_helper(true)?;
        match self.dmabuf_helper.as_mut() {
            Some(helper) => helper.frame(timeout),
            None => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "privileged kms dmabuf helper session unavailable",
            )),
        }
    }
}

enum CapturedFrame {
    Pixel(HelperFrameOutput),
    Dmabuf(DmabufFrame),
}

impl CapturedFrame {
    fn size(&self) -> (usize, usize) {
        match self {
            CapturedFrame::Pixel(frame) => (frame.width, frame.height),
            CapturedFrame::Dmabuf(frame) => (frame.width, frame.height),
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

fn find_render_node(card_path: &Path) -> Option<PathBuf> {
    let card_name = card_path.file_name()?.to_str()?;
    let drm_dir = PathBuf::from("/sys/class/drm")
        .join(card_name)
        .join("device/drm");
    let mut nodes = fs::read_dir(drm_dir)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            name.starts_with("renderD")
                .then(|| PathBuf::from("/dev/dri").join(name))
        })
        .collect::<Vec<_>>();
    nodes.sort();
    nodes.into_iter().next()
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
    #[serde(default)]
    render_node: Option<String>,
    connector_path: String,
    name: String,
    width: usize,
    height: usize,
    #[serde(default)]
    x: Option<i32>,
    #[serde(default)]
    y: Option<i32>,
    // Defaulted for forward compatibility; modifier_known decides whether this
    // placeholder can be used for CPU fallback.
    #[serde(default = "helper_display_default_is_linear")]
    is_linear: bool,
    #[serde(default)]
    modifier_known: bool,
    online: bool,
    can_open: bool,
}

fn helper_display_default_is_linear() -> bool {
    true
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

#[derive(Deserialize)]
struct HelperDmabufHeader {
    card_path: String,
    #[serde(default)]
    render_node: Option<String>,
    width: usize,
    height: usize,
    fourcc: u32,
    modifier: u64,
    planes: Vec<HelperDmabufPlane>,
}

#[derive(Deserialize)]
struct HelperDmabufPlane {
    stride: u32,
    offset: u32,
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

struct HelperDmabufSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    socket: UnixDatagram,
    socket_path: PathBuf,
    frame_pending_since: Option<Instant>,
}

impl HelperDmabufSession {
    fn spawn(display_name: &str, privileged: bool) -> io::Result<Self> {
        let socket_path = dmabuf_socket_path();
        let _ = fs::remove_file(&socket_path);
        let socket = UnixDatagram::bind(&socket_path)?;
        let mut command = if privileged {
            let mut command = Command::new("pkexec");
            command.arg("--disable-internal-agent");
            command.arg(std::env::current_exe()?);
            command
        } else {
            Command::new(std::env::current_exe()?)
        };

        log::info!(
            "starting {} kms dmabuf helper stream for {}",
            if privileged { "privileged" } else { "unprivileged" },
            display_name
        );

        let mut child = command
            .arg("--kms-helper")
            .arg(if privileged {
                "dmabuf-stream-privileged"
            } else {
                "dmabuf-stream"
            })
            .arg(display_name)
            .arg(&socket_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "kms dmabuf helper stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "kms dmabuf helper stdout unavailable"))?;

        let mut session = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            socket,
            socket_path,
            frame_pending_since: None,
        };
        let mut ready = String::new();
        session.wait_for_stdout(HELPER_READY_TIMEOUT, "kms dmabuf helper ready")?;
        let read = session.stdout.read_line(&mut ready)?;
        if read == 0 {
            return Err(session.child_error("kms dmabuf helper exited before ready"));
        }
        if ready.trim() != "ready" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected kms dmabuf helper ready message: {}", ready.trim()),
            ));
        }
        log::info!(
            "{} kms dmabuf helper stream is ready for {}",
            if privileged { "privileged" } else { "unprivileged" },
            display_name
        );
        Ok(session)
    }

    fn frame(&mut self, timeout: Duration) -> io::Result<DmabufFrame> {
        if self.frame_pending_since.is_none() {
            self.stdin.write_all(b"frame\n")?;
            self.stdin.flush()?;
            self.frame_pending_since = Some(Instant::now());
        }
        let timeout = timeout.max(Duration::from_millis(1));
        let watchdog_remaining = self
            .frame_pending_since
            .map(|started| HELPER_FRAME_TIMEOUT.saturating_sub(started.elapsed()))
            .unwrap_or(HELPER_FRAME_TIMEOUT)
            .max(Duration::from_millis(1));
        let timeout = timeout.min(watchdog_remaining);
        self.socket.set_read_timeout(Some(timeout))?;
        let (header, fds) = match recv_dmabuf_message(self.socket.as_raw_fd()) {
            Ok(frame) => {
                self.frame_pending_since = None;
                frame
            }
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                match self.child.try_wait() {
                    Ok(Some(_)) => {
                        self.frame_pending_since = None;
                        return Err(self.child_error("kms dmabuf helper exited before frame"));
                    }
                    Ok(None) => {
                        if self
                            .frame_pending_since
                            .map(|started| started.elapsed() >= HELPER_FRAME_TIMEOUT)
                            .unwrap_or(false)
                        {
                            self.frame_pending_since = None;
                            return Err(io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                format!(
                                    "kms dmabuf helper frame timed out after {} ms",
                                    HELPER_FRAME_TIMEOUT.as_millis()
                                ),
                            ));
                        }
                        return Err(err);
                    }
                    Err(wait_err) => {
                        self.frame_pending_since = None;
                        return Err(wait_err);
                    }
                }
            }
            Err(err) => {
                if self.wait_for_child_exit(Duration::from_millis(200)) {
                    return Err(self.child_error("kms dmabuf helper exited before frame"));
                }
                return Err(err);
            }
        };
        if header.planes.len() != fds.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "kms dmabuf plane/fd count mismatch: {} planes, {} fds",
                    header.planes.len(),
                    fds.len()
                ),
            ));
        }
        let planes = header
            .planes
            .into_iter()
            .zip(fds)
            .map(|(plane, fd)| DmabufPlane {
                fd,
                stride: plane.stride,
                offset: plane.offset,
            })
            .collect();
        Ok(DmabufFrame {
            card_path: header.card_path,
            render_node: header.render_node,
            width: header.width,
            height: header.height,
            fourcc: header.fourcc,
            modifier: header.modifier,
            planes,
        })
    }

    fn wait_for_stdout(&self, timeout: Duration, context: &str) -> io::Result<()> {
        let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
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

    fn wait_for_child_exit(&mut self, timeout: Duration) -> bool {
        let started = Instant::now();
        while started.elapsed() < timeout {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

impl Drop for HelperDmabufSession {
    fn drop(&mut self) {
        let _ = self.stdin.write_all(b"quit\n");
        let _ = self.stdin.flush();
        for _ in 0..10 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                let _ = fs::remove_file(&self.socket_path);
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.socket_path);
    }
}

fn dmabuf_socket_path() -> PathBuf {
    let id = DMABUF_SOCKET_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "rustdesk-kms-dmabuf-{}-{id}.sock",
        std::process::id()
    ))
}

fn recv_dmabuf_message(socket_fd: RawFd) -> io::Result<(HelperDmabufHeader, Vec<OwnedFd>)> {
    let mut data = vec![0_u8; 4096];
    let mut iov = crate::libc::iovec {
        iov_base: data.as_mut_ptr().cast::<crate::libc::c_void>(),
        iov_len: data.len(),
    };
    let mut control = vec![0_u8; 256];
    let mut msg = crate::libc::msghdr {
        msg_name: std::ptr::null_mut(),
        msg_namelen: 0,
        msg_iov: &mut iov,
        msg_iovlen: 1,
        msg_control: control.as_mut_ptr().cast::<crate::libc::c_void>(),
        msg_controllen: control.len() as _,
        msg_flags: 0,
    };
    let read = unsafe {
        crate::libc::recvmsg(socket_fd, &mut msg, crate::libc::MSG_CMSG_CLOEXEC)
    };
    if read < 0 {
        return Err(io::Error::last_os_error());
    }
    // Own received descriptors before validating the payload so every error
    // path closes them instead of leaking process FDs.
    let fds = recv_fds_from_cmsg(&msg)?;
    if read == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "kms dmabuf socket closed",
        ));
    }
    if msg.msg_flags & crate::libc::MSG_TRUNC != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "kms dmabuf datagram header was truncated",
        ));
    }
    if msg.msg_flags & crate::libc::MSG_CTRUNC != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "kms dmabuf datagram file descriptors were truncated",
        ));
    }
    data.truncate(read as usize);
    if data.last() == Some(&b'\n') {
        data.pop();
    }
    let header: HelperDmabufHeader = serde_json::from_slice(&data)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
    Ok((header, fds))
}

fn cmsg_align(len: usize) -> usize {
    let align = std::mem::size_of::<usize>();
    (len + align - 1) & !(align - 1)
}

fn recv_fds_from_cmsg(msg: &crate::libc::msghdr) -> io::Result<Vec<OwnedFd>> {
    let mut fds = Vec::new();
    if msg.msg_controllen < std::mem::size_of::<crate::libc::cmsghdr>() as _ {
        return Ok(fds);
    }
    let cmsg = msg.msg_control.cast::<crate::libc::cmsghdr>();
    unsafe {
        if (*cmsg).cmsg_level != crate::libc::SOL_SOCKET
            || (*cmsg).cmsg_type != crate::libc::SCM_RIGHTS
        {
            return Ok(fds);
        }
        let header_len = cmsg_align(std::mem::size_of::<crate::libc::cmsghdr>());
        let cmsg_len = (*cmsg).cmsg_len as usize;
        if cmsg_len < header_len || cmsg_len > msg.msg_controllen as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid kms dmabuf control message length",
            ));
        }
        let data_len = cmsg_len - header_len;
        if data_len % std::mem::size_of::<RawFd>() != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid kms dmabuf file descriptor payload length",
            ));
        }
        let fd_count = data_len / std::mem::size_of::<RawFd>();
        let data = msg.msg_control.cast::<u8>().add(header_len).cast::<RawFd>();
        for idx in 0..fd_count {
            let fd = *data.add(idx);
            fds.push(OwnedFd::from_raw_fd(fd));
        }
    }
    Ok(fds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs::File, os::fd::AsRawFd};

    #[test]
    fn cpu_readback_requires_a_known_linear_modifier() {
        assert!(!cpu_readback_safe(false, true));
        assert!(!cpu_readback_safe(true, false));
        assert!(cpu_readback_safe(true, true));
    }

    fn send_test_fd(socket: &UnixDatagram, bytes: &[u8], fd: RawFd) -> io::Result<()> {
        let mut iov = crate::libc::iovec {
            iov_base: bytes.as_ptr() as *mut crate::libc::c_void,
            iov_len: bytes.len(),
        };
        let header_len = cmsg_align(std::mem::size_of::<crate::libc::cmsghdr>());
        let mut control = vec![0_u8; header_len + cmsg_align(std::mem::size_of::<RawFd>())];
        let cmsg = control.as_mut_ptr().cast::<crate::libc::cmsghdr>();
        unsafe {
            (*cmsg).cmsg_level = crate::libc::SOL_SOCKET;
            (*cmsg).cmsg_type = crate::libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = (header_len + std::mem::size_of::<RawFd>()) as _;
            control
                .as_mut_ptr()
                .add(header_len)
                .cast::<RawFd>()
                .write(fd);
            let msg = crate::libc::msghdr {
                msg_name: std::ptr::null_mut(),
                msg_namelen: 0,
                msg_iov: &mut iov,
                msg_iovlen: 1,
                msg_control: control.as_mut_ptr().cast::<crate::libc::c_void>(),
                msg_controllen: control.len() as _,
                msg_flags: 0,
            };
            let written = crate::libc::sendmsg(socket.as_raw_fd(), &msg, 0);
            if written < 0 {
                return Err(io::Error::last_os_error());
            }
            if written as usize != bytes.len() {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "short datagram"));
            }
        }
        Ok(())
    }

    #[test]
    fn dmabuf_datagram_preserves_header_and_fd() {
        let (sender, receiver) = UnixDatagram::pair().unwrap();
        let file = File::open("/dev/null").unwrap();
        let header = br#"{"card_path":"/dev/dri/card1","render_node":"/dev/dri/renderD129","width":1920,"height":1080,"fourcc":875713112,"modifier":0,"planes":[{"stride":7680,"offset":0}]}
"#;
        send_test_fd(&sender, header, file.as_raw_fd()).unwrap();

        let (header, fds) = recv_dmabuf_message(receiver.as_raw_fd()).unwrap();
        assert_eq!(header.card_path, "/dev/dri/card1");
        assert_eq!(header.render_node.as_deref(), Some("/dev/dri/renderD129"));
        assert_eq!((header.width, header.height), (1920, 1080));
        assert_eq!(header.planes.len(), 1);
        assert_eq!(fds.len(), 1);
        let flags = unsafe { crate::libc::fcntl(fds[0].as_raw_fd(), crate::libc::F_GETFD) };
        assert!(flags >= 0);
        assert_ne!(flags & crate::libc::FD_CLOEXEC, 0);
    }

    #[test]
    fn dmabuf_datagram_rejects_truncated_header() {
        let (sender, receiver) = UnixDatagram::pair().unwrap();
        sender.send(&vec![b'x'; 5000]).unwrap();
        let err = match recv_dmabuf_message(receiver.as_raw_fd()) {
            Ok(_) => panic!("truncated datagram was accepted"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn dmabuf_datagram_read_respects_socket_timeout() {
        let (_sender, receiver) = UnixDatagram::pair().unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let started = Instant::now();
        let err = match recv_dmabuf_message(receiver.as_raw_fd()) {
            Ok(_) => panic!("empty datagram read unexpectedly succeeded"),
            Err(err) => err,
        };
        assert!(matches!(
            err.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn dmabuf_timeout_detects_exited_helper() {
        let (_sender, receiver) = UnixDatagram::pair().unwrap();
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        while child.try_wait().unwrap().is_none() {
            std::thread::yield_now();
        }
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut session = HelperDmabufSession {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            socket: receiver,
            socket_path: dmabuf_socket_path(),
            frame_pending_since: Some(Instant::now()),
        };

        let err = session.frame(Duration::from_millis(20)).unwrap_err();
        assert!(!matches!(
            err.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ));
        assert!(session.frame_pending_since.is_none());
    }

    #[test]
    fn dmabuf_watchdog_restarts_a_stuck_helper() {
        let (_sender, receiver) = UnixDatagram::pair().unwrap();
        let mut child = Command::new("sleep")
            .arg("5")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut session = HelperDmabufSession {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            socket: receiver,
            socket_path: dmabuf_socket_path(),
            frame_pending_since: Some(Instant::now() - HELPER_FRAME_TIMEOUT),
        };

        let err = session.frame(Duration::from_millis(1)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
        assert!(session.frame_pending_since.is_none());
    }
}
