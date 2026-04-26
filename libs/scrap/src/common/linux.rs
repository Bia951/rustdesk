use crate::{
    common::{
        wayland,
        x11::{self},
        LinuxCaptureBackend, TraitCapturer,
    },
    kms, Frame,
};
use std::{io, time::Duration};

pub enum Capturer {
    X11(x11::Capturer),
    WAYLAND(wayland::Capturer),
    KMS(kms::Capturer),
}

impl Capturer {
    pub fn new(display: Display) -> io::Result<Capturer> {
        Ok(match display {
            Display::X11(d) => Capturer::X11(x11::Capturer::new(d)?),
            Display::WAYLAND(d) => Capturer::WAYLAND(wayland::Capturer::new(d)?),
            Display::KMS(d) => Capturer::KMS(kms::Capturer::new(d)?),
        })
    }

    pub fn width(&self) -> usize {
        match self {
            Capturer::X11(d) => d.width(),
            Capturer::WAYLAND(d) => d.width(),
            Capturer::KMS(d) => d.width(),
        }
    }

    pub fn height(&self) -> usize {
        match self {
            Capturer::X11(d) => d.height(),
            Capturer::WAYLAND(d) => d.height(),
            Capturer::KMS(d) => d.height(),
        }
    }
}

impl TraitCapturer for Capturer {
    fn frame<'a>(&'a mut self, timeout: Duration) -> io::Result<Frame<'a>> {
        match self {
            Capturer::X11(d) => d.frame(timeout),
            Capturer::WAYLAND(d) => d.frame(timeout),
            Capturer::KMS(d) => d.frame(timeout),
        }
    }
}

pub enum Display {
    X11(x11::Display),
    WAYLAND(wayland::Display),
    KMS(kms::Display),
}

impl Display {
    pub fn primary() -> io::Result<Display> {
        Ok(match super::linux_capture_backend() {
            LinuxCaptureBackend::X11 => Display::X11(x11::Display::primary()?),
            LinuxCaptureBackend::Wayland => Display::WAYLAND(wayland::Display::primary()?),
            LinuxCaptureBackend::Kms => Display::KMS(kms::Display::primary()?),
            LinuxCaptureBackend::Auto => unreachable!(),
        })
    }

    // Currently, wayland need to call wayland::clear() before call Display::all()
    pub fn all() -> io::Result<Vec<Display>> {
        Ok(match super::linux_capture_backend() {
            LinuxCaptureBackend::X11 => x11::Display::all()?
                .drain(..)
                .map(|x| Display::X11(x))
                .collect(),
            LinuxCaptureBackend::Wayland => wayland::Display::all()?
                .drain(..)
                .map(|x| Display::WAYLAND(x))
                .collect(),
            LinuxCaptureBackend::Kms => kms::Display::all()?
                .drain(..)
                .map(|x| Display::KMS(x))
                .collect(),
            LinuxCaptureBackend::Auto => unreachable!(),
        })
    }

    pub fn width(&self) -> usize {
        match self {
            Display::X11(d) => d.width(),
            Display::WAYLAND(d) => d.width(),
            Display::KMS(d) => d.width(),
        }
    }

    pub fn height(&self) -> usize {
        match self {
            Display::X11(d) => d.height(),
            Display::WAYLAND(d) => d.height(),
            Display::KMS(d) => d.height(),
        }
    }

    pub fn scale(&self) -> f64 {
        match self {
            Display::X11(_d) => 1.0,
            Display::WAYLAND(d) => d.scale(),
            Display::KMS(_d) => 1.0,
        }
    }

    pub fn logical_width(&self) -> usize {
        match self {
            Display::X11(d) => d.width(),
            Display::WAYLAND(d) => d.logical_width(),
            Display::KMS(d) => d.width(),
        }
    }

    pub fn logical_height(&self) -> usize {
        match self {
            Display::X11(d) => d.height(),
            Display::WAYLAND(d) => d.logical_height(),
            Display::KMS(d) => d.height(),
        }
    }

    pub fn origin(&self) -> (i32, i32) {
        match self {
            Display::X11(d) => d.origin(),
            Display::WAYLAND(d) => d.origin(),
            Display::KMS(d) => d.origin(),
        }
    }

    pub fn is_online(&self) -> bool {
        match self {
            Display::X11(d) => d.is_online(),
            Display::WAYLAND(d) => d.is_online(),
            Display::KMS(d) => d.is_online(),
        }
    }

    pub fn is_primary(&self) -> bool {
        match self {
            Display::X11(d) => d.is_primary(),
            Display::WAYLAND(d) => d.is_primary(),
            Display::KMS(d) => d.is_primary(),
        }
    }

    pub fn name(&self) -> String {
        match self {
            Display::X11(d) => d.name(),
            Display::WAYLAND(d) => d.name(),
            Display::KMS(d) => d.name(),
        }
    }
}
